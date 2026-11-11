//! Versioned, bounded helper deployment on the application's authenticated SSH pool.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use mt_config::SshConnection;
use mt_ssh::{CachedSession, SftpHandle, SftpNodeKind, SshPool, run_bounded_exec_on_session};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::git::{quote_posix, validate_absolute_path};
use crate::{acquire_session, connection_fingerprint, lock, state};

const DEPLOY_TIMEOUT: Duration = Duration::from_secs(120);
const FINISH_GRACE: Duration = Duration::from_secs(10);
const EXEC_TIMEOUT: Duration = Duration::from_secs(15);
const OUTPUT_CAP: usize = 8 * 1024;
const MAX_ASSET_BYTES: u64 = 128 * 1024 * 1024;
const ASSET_ROOT_ENV: &str = "MT_REMOTE_GIT_HELPERS_DIR";
static PARTIAL_SEQUENCE: AtomicU64 = AtomicU64::new(0);
type DeploymentLock = tokio::sync::Mutex<()>;
static DEPLOYMENTS: LazyLock<Mutex<HashMap<(String, u64), Weak<DeploymentLock>>>> =
    LazyLock::new(Default::default);

fn deployment_lock(connection: &SshConnection) -> Arc<DeploymentLock> {
    let key = (connection.id.clone(), connection_fingerprint(connection));
    let mut locks = lock(&DEPLOYMENTS);
    locks.retain(|_, deployment| deployment.strong_count() != 0);
    if let Some(existing) = locks.get(&key).and_then(Weak::upgrade) {
        return existing;
    }
    let deployment = Arc::new(DeploymentLock::new(()));
    locks.insert(key, Arc::downgrade(&deployment));
    deployment
}

pub(super) fn ensure_helper(connection: &SshConnection) -> Result<String, String> {
    let runtime = state().runtime()?;
    runtime.block_on(async {
        let deadline = Instant::now() + DEPLOY_TIMEOUT;
        tokio::time::timeout_at(deadline, async {
            let deployment = deployment_lock(connection);
            let _guard = deployment.lock().await;
            let pool = state().pool();
            let session = acquire_session(&pool, connection).await?;
            let result = deploy(connection, &pool, &session, deadline).await;
            if result.is_err() && session.is_unhealthy_now() {
                let _ = tokio::time::timeout(Duration::from_secs(2),
                    pool.evict_if_same(&connection.id, &session)).await;
            }
            result
        }).await.unwrap_or_else(|_| Err(format!(
            "remote Git helper deployment timed out ({}s); no Git request was sent",
            DEPLOY_TIMEOUT.as_secs(),
        )))
    })
}

async fn deploy(
    connection: &SshConnection,
    pool: &Arc<SshPool>,
    session: &Arc<CachedSession>,
    deadline: Instant,
) -> Result<String, String> {
    let platform = exec(connection, pool, session, "uname -s; uname -m", deadline).await?;
    let target = parse_target(&platform)?;
    let (asset, digest) = tokio::task::spawn_blocking(move || {
        let executable = std::env::current_exe()
            .map_err(|e| format!("locating remote Git helper assets: {e}"))?;
        let root = std::env::var_os(ASSET_ROOT_ENV).map(PathBuf::from);
        let asset = select_asset(&executable, root.as_deref(), target)?;
        let digest = hash_asset(&asset)?;
        Ok::<_, String>((asset, digest))
    }).await.map_err(|e| format!("reading remote Git helper asset: {e}"))??;
    let sftp = Arc::new(SftpHandle::open_on_session(session.clone(), EXEC_TIMEOUT).await
        .map_err(|e| format!("opening helper deployment SFTP: {}", e.message()))?);
    let mut partial: Option<PartialCleanup> = None;
    let result = async {
        // Do not use the connection-ID-only home cache: configuration identity matters.
        let home = sftp.canonicalize(".").await
            .map_err(|e| format!("resolving remote Git helper home: {}", e.message()))?;
        validate_absolute_path(&home, "home")?;
        let home = home.trim_end_matches('/');
        let mut directory = home.to_string();
        for component in [".cache", "mini-term", "git-helper", digest.as_str()] {
            directory.push('/');
            directory.push_str(component);
            let command = secure_directory_command(&directory,
                component == "git-helper" || component == digest.as_str());
            exec(connection, pool, session, &command, deadline).await?;
            require_kind(&sftp, &directory, SftpNodeKind::Directory).await?;
        }
        let helper = format!("{directory}/mt-git-helper");
        match sftp.try_node_kind(&helper).await.map_err(|e| e.message().to_string())? {
            Some(SftpNodeKind::File) => {
                verify_helper(connection, pool, session, &helper, &digest, target, deadline).await?;
                return Ok(helper);
            }
            Some(kind) => return Err(format!(
                "unsafe remote Git helper cache entry '{helper}': expected regular file, got {kind:?}",
            )),
            None => {}
        }
        let staging = partial_path(&directory);
        if sftp.try_node_kind(&staging).await.map_err(|e| e.message().to_string())?.is_some() {
            return Err(format!("remote Git helper staging path already exists: '{staging}'"));
        }
        partial = Some(PartialCleanup::new(sftp.clone(), staging.clone()));
        let local_path = asset.to_str().ok_or_else(||
            format!("remote Git helper asset path is not UTF-8: {}", asset.display()))?;
        mt_ssh::run_sftp_upload_until(session, local_path, &staging, deadline - FINISH_GRACE)
            .await.map_err(|e| format!("uploading remote Git helper: {}", e.message()))?;
        require_kind(&sftp, &staging, SftpNodeKind::File).await?;
        // chmod and hash only an lstat-verified regular file; every shell argument is quoted.
        let command = format!(
            "test ! -L {path} && test -f {path} && chmod 700 {path}",
            path = quote_posix(&staging),
        );
        exec(connection, pool, session, &command, deadline).await?;
        verify_helper(connection, pool, session, &staging, &digest, target, deadline).await?;
        // SFTP v3 rename fails if destination exists; never replace an executing helper.
        if let Err(error) = sftp.rename(&staging, &helper).await {
            // A different app process may have published the same SHA concurrently. Only
            // accept the immutable verified final file, never overwrite it or blindly retry.
            require_kind(&sftp, &helper, SftpNodeKind::File).await.map_err(|state|
                format!("publishing remote Git helper failed: {}; {state}", error.message()))?;
            verify_helper(connection, pool, session, &helper, &digest, target, deadline).await?;
        }
        require_kind(&sftp, &helper, SftpNodeKind::File).await?;
        verify_helper(connection, pool, session, &helper, &digest, target, deadline).await?;
        Ok(helper)
    }.await;
    // Cleanup also runs on every failure. Cancellation gets its own bounded background
    // cleanup through the guard, since an outer hard deadline can drop this future.
    if let Some(mut partial) = partial.take() {
        match tokio::time::timeout(Duration::from_secs(3),
            remove_partial(&sftp, &partial.path)).await
        {
            Ok(Ok(())) => partial.armed = false,
            _ => return Err(format!("{}; remote helper partial cleanup unconfirmed: '{}'",
                result.err().unwrap_or_else(|| "helper deployed".into()), partial.path)),
        }
    }
    // Dropping the last SFTP reference closes its channel and releases its session lease.
    result
}

struct PartialCleanup {
    sftp: Arc<SftpHandle>,
    path: String,
    runtime: tokio::runtime::Handle,
    armed: bool,
}

impl PartialCleanup {
    fn new(sftp: Arc<SftpHandle>, path: String) -> Self {
        Self { sftp, path, runtime: tokio::runtime::Handle::current(), armed: true }
    }
}

impl Drop for PartialCleanup {
    fn drop(&mut self) {
        if self.armed {
            let sftp = self.sftp.clone();
            let path = self.path.clone();
            self.runtime.spawn(async move {
                // The upload primitive can finish its atomic promotion after cancellation.
                // Wait for its request grace before removing our uniquely named partial.
                tokio::time::sleep(FINISH_GRACE).await;
                let cleanup = tokio::time::timeout(Duration::from_secs(5),
                    remove_partial(&sftp, &path)).await;
                if !matches!(cleanup, Ok(Ok(()))) {
                    eprintln!("[remote-git] helper partial cleanup unconfirmed: '{path}'");
                }
            });
        }
    }
}

async fn remove_partial(sftp: &SftpHandle, path: &str) -> Result<(), String> {
    match sftp.try_node_kind(path).await.map_err(|e| e.message().to_string())? {
        None => Ok(()),
        Some(SftpNodeKind::File | SftpNodeKind::Symlink) =>
            sftp.remove_file(path).await.map_err(|e| e.message().to_string()),
        Some(_) => Err(format!("unsafe helper staging entry type at '{path}'")),
    }
}

async fn require_kind(sftp: &SftpHandle, path: &str, expected: SftpNodeKind) -> Result<(), String> {
    let actual = sftp.try_node_kind(path).await.map_err(|e| e.message().to_string())?;
    if actual != Some(expected) {
        return Err(format!("unsafe remote Git helper path '{path}': expected {expected:?}, got {actual:?}"));
    }
    Ok(())
}

fn secure_directory_command(directory: &str, private: bool) -> String {
    let path = quote_posix(directory);
    let permissions = if private { format!(" && chmod 700 {path}") } else { String::new() };
    format!(
        "if test -L {path}; then exit 73; fi; \
         if test ! -e {path}; then (umask 077; mkdir {path}) || test -d {path} || exit 73; fi; \
         test ! -L {path} && test -d {path} && test -O {path}{permissions}",
    )
}

async fn verify_helper(
    connection: &SshConnection,
    pool: &Arc<SshPool>,
    session: &Arc<CachedSession>,
    path: &str,
    digest: &str,
    target: &str,
    deadline: Instant,
) -> Result<(), String> {
    let path_arg = quote_posix(path);
    let hash = if target.starts_with("linux-") { "sha256sum" } else { "shasum -a 256" };
    let command = format!(
        "test ! -L {path_arg} && test -f {path_arg} && test -O {path_arg} && test -x {path_arg} && {hash} {path_arg}",
    );
    let output = exec(connection, pool, session, &command, deadline).await?;
    let actual = output.split_ascii_whitespace().next().unwrap_or_default();
    if actual != digest {
        return Err(format!("remote Git helper SHA-256 mismatch at '{path}'; cached executable was not replaced"));
    }
    Ok(())
}

async fn exec(
    connection: &SshConnection,
    pool: &Arc<SshPool>,
    session: &Arc<CachedSession>,
    command: &str,
    deadline: Instant,
) -> Result<String, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining <= FINISH_GRACE {
        return Err("remote Git helper deployment deadline reached".into());
    }
    let output = run_bounded_exec_on_session(session, command,
        EXEC_TIMEOUT.min(remaining - FINISH_GRACE), OUTPUT_CAP).await.map_err(|error| {
            session.mark_unhealthy(Duration::from_secs(30));
            format!("remote Git helper deployment SSH exec failed: {error}")
        })?;
    if output.requires_session_retirement() {
        session.mark_unhealthy(Duration::from_secs(30));
        let _ = tokio::time::timeout(Duration::from_secs(2),
            pool.evict_if_same(&connection.id, session)).await;
        return Err("remote Git helper deployment channel cleanup uncertain; SSH session retired".into());
    }
    if output.timed_out || output.exit_code != Some(0)
        || output.stdout_truncated || output.stderr_truncated
    {
        return Err(format!(
            "remote Git helper deployment command failed (exit {:?}, timeout={}): {}",
            output.exit_code, output.timed_out, String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| "remote Git helper deployment returned non-UTF-8 output".into())
}

fn parse_target(platform: &str) -> Result<&'static str, String> {
    let mut lines = platform.lines();
    let os = lines.next().unwrap_or_default().trim();
    let arch = lines.next().unwrap_or_default().trim();
    if lines.any(|line| !line.trim().is_empty()) {
        return Err("remote Git helper platform probe returned unexpected output".into());
    }
    match (os, arch) {
        ("Linux", "x86_64" | "amd64") => Ok("linux-x86_64"),
        ("Linux", "aarch64" | "arm64") => Ok("linux-aarch64"),
        ("Darwin", "arm64" | "aarch64") => Ok("macos-aarch64"),
        ("Darwin", "x86_64" | "amd64") => Ok("macos-x86_64"),
        _ => Err(format!("remote Git helper does not support server platform {os:?}/{arch:?}")),
    }
}

fn select_asset(executable: &Path, configured: Option<&Path>, target: &str) -> Result<PathBuf, String> {
    let parent = executable.parent().ok_or("application executable has no resource directory")?;
    let bundled = parent.join("remote-git-helpers").join(target).join("mt-git-helper");
    let asset = match std::fs::symlink_metadata(&bundled) {
        Ok(_) => bundled,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => configured
            .map(|root| root.join(target).join("mt-git-helper")).unwrap_or(bundled),
        Err(error) => return Err(format!("inspecting remote Git helper asset '{}': {error}", bundled.display())),
    };
    let metadata = std::fs::symlink_metadata(&asset).map_err(|error| format!(
        "remote Git helper asset for {target} is unavailable at '{}': {error}; install a build containing remote-git-helpers/{target}/mt-git-helper",
        asset.display(),
    ))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_ASSET_BYTES {
        return Err(format!("remote Git helper asset '{}' must be a nonempty regular file of at most {MAX_ASSET_BYTES} bytes", asset.display()));
    }
    Ok(asset)
}

fn hash_asset(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("opening helper asset '{}': {e}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let size = file.read(&mut buffer).map_err(|e| format!("reading helper asset '{}': {e}", path.display()))?;
        if size == 0 { break }
        total += size as u64;
        if total > MAX_ASSET_BYTES {
            return Err("remote Git helper asset exceeded size limit while hashing".into());
        }
        hash.update(&buffer[..size]);
    }
    if total == 0 { return Err("remote Git helper asset is empty".into()) }
    use std::fmt::Write as _;
    let mut digest = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(&mut digest, "{byte:02x}").expect("writing to String");
    }
    Ok(digest)
}

fn partial_path(directory: &str) -> String {
    let sequence = PARTIAL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_nanos();
    format!("{directory}/.mt-git-helper-{}-{time:x}-{sequence:x}.partial", std::process::id())
}

#[cfg(test)]
#[path = "git_deploy_tests.rs"]
mod tests;
