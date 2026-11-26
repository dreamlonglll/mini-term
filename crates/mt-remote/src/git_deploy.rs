//! Versioned, bounded helper deployment on the application's authenticated SSH pool.
//!
//! # 本机 helper 从哪来(按需扩展,阶段 A)
//!
//! helper 不再随安装包分发,查找走 [`mt_extensions::Extensions::locate`]:① exe 旁旧布局
//! `remote-git-helpers/<target>/mt-git-helper`(dev 的 stage 脚本 / 源码构建,与 exe 同信任
//! 等级)→ ② `MT_REMOTE_GIT_HELPERS_DIR` 开发根目录 → ③ 按需下载装好的扩展。在场却坏掉的
//! 那一层**不会**落到下一层去用别的二进制。三层都没有 = [`DeployError::HelperMissing`]
//! (类型化,不是人话):`git_service::connect_transport` 认出它后在**部署 deadline 之外**
//! 调宿主注入的 [`set_helper_provisioner`] 下载该平台的扩展,成功后自动重试一次部署;
//! 仍不行就以 [`REMOTE_GIT_HELPER_MISSING`] 报给界面,[`missing_helper`] 解出缺的平台。

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};
use std::time::Duration;

use mt_config::SshConnection;
use mt_extensions::{ComponentId, Extensions, GIT_HELPER_FILE};
use mt_git_helper::GitFailure;
use mt_ssh::{CachedSession, SftpHandle, SftpNodeKind, SshPool, run_bounded_exec_on_session};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::git::{quote_posix, validate_absolute_path};
use crate::{acquire_session, connection_fingerprint, lock, state};

/// 本机缺某个服务器平台的 helper 时,[`GitFailure::Service`] 用的 code。message 的形状是
/// `"<target>\n<原因>"`,由 [`missing_helper`] 解开 —— 调用方按 code 认,不匹配人话。
pub const REMOTE_GIT_HELPER_MISSING: &str = "remote_git_helper_missing";

/// [`missing_helper`] 解出来的东西。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MissingHelper<'a> {
    /// 服务器平台键:`linux-x86_64` / `linux-aarch64` / `macos-x86_64` / `macos-aarch64`。
    pub target: &'static str,
    /// 自动补齐为什么没成(宿主下载器给的原因;没注入下载器时为空)。
    pub detail: &'a str,
}

/// 这个失败是不是「本机缺 helper」,缺的是哪个服务器平台。
pub fn missing_helper(failure: &GitFailure) -> Option<MissingHelper<'_>> {
    let GitFailure::Service { code, message } = failure else {
        return None;
    };
    if code != REMOTE_GIT_HELPER_MISSING {
        return None;
    }
    let (target, detail) = message.split_once('\n').unwrap_or((message.as_str(), ""));
    let target = ComponentId::git_helper(target)?.helper_target()?;
    Some(MissingHelper { target, detail })
}

pub(super) fn missing_helper_failure(target: &'static str, detail: &str) -> GitFailure {
    GitFailure::service(REMOTE_GIT_HELPER_MISSING, format!("{target}\n{detail}"))
}

/// 宿主注入的 helper 补齐器:收服务器平台键,**阻塞**到该平台的扩展装好(`Ok`)或确定装不上
/// (`Err(原因)`)为止。在 Git 执行器线程上、部署 deadline 之外调用。
type Provisioner = dyn Fn(&'static str) -> Result<(), String> + Send + Sync;
static PROVISIONER: OnceLock<Box<Provisioner>> = OnceLock::new();

/// 注入 helper 补齐器(mt-app 启动时调一次,下载那一跳在它那边:扩展服务 + 共享 HTTP 客户端)。
/// 返回 `false` = 已经注入过(先到先得)。没注入时缺 helper 直接报 [`REMOTE_GIT_HELPER_MISSING`]。
pub fn set_helper_provisioner(
    provisioner: impl Fn(&'static str) -> Result<(), String> + Send + Sync + 'static,
) -> bool {
    PROVISIONER.set(Box::new(provisioner)).is_ok()
}

pub(super) fn provision_helper(target: &'static str) -> Result<(), String> {
    match PROVISIONER.get() {
        Some(provisioner) => provisioner(target),
        None => Err(String::new()),
    }
}

/// 部署失败的两类:缺本机 helper(可补齐后重试)与其它一切(原样报)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DeployError {
    HelperMissing { target: &'static str },
    Failed(String),
}

impl From<String> for DeployError {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

impl fmt::Display for DeployError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HelperMissing { target } => write!(
                f,
                "no local remote Git helper for {target}; download the extension in Settings → Extensions"
            ),
            Self::Failed(error) => f.write_str(error),
        }
    }
}

const DEPLOY_TIMEOUT: Duration = Duration::from_secs(120);
const FINISH_GRACE: Duration = Duration::from_secs(10);
const EXEC_TIMEOUT: Duration = Duration::from_secs(15);
const OUTPUT_CAP: usize = 8 * 1024;
const MAX_ASSET_BYTES: u64 = 128 * 1024 * 1024;
const ASSET_ROOT_ENV: &str = "MT_REMOTE_GIT_HELPERS_DIR";
static PARTIAL_SEQUENCE: AtomicU64 = AtomicU64::new(0);
type DeploymentLock = tokio::sync::Mutex<()>;
type DeploymentRegistry = HashMap<(String, u64), Weak<DeploymentLock>>;
static DEPLOYMENTS: LazyLock<Mutex<DeploymentRegistry>> = LazyLock::new(Default::default);

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

pub(super) fn ensure_helper(
    connection: &SshConnection,
    check: &(dyn Fn() -> Result<(), String> + Sync),
) -> Result<String, DeployError> {
    check()?;
    let runtime = state().runtime()?;
    runtime.block_on(async {
        let deadline = Instant::now() + DEPLOY_TIMEOUT;
        tokio::time::timeout_at(deadline, async {
            let deployment = deployment_lock(connection);
            let _guard = deployment.lock().await;
            check()?;
            let pool = state().pool();
            let session = acquire_session(&pool, connection).await?;
            let result = deploy(connection, &pool, &session, deadline, check).await;
            if result.is_err() && session.is_unhealthy_now() {
                let _ = tokio::time::timeout(
                    Duration::from_secs(2),
                    pool.evict_if_same(&connection.id, &session),
                )
                .await;
            }
            result
        })
        .await
        .unwrap_or_else(|_| {
            Err(DeployError::Failed(format!(
                "remote Git helper deployment timed out ({}s); no Git request was sent",
                DEPLOY_TIMEOUT.as_secs(),
            )))
        })
    })
}

async fn deploy(
    connection: &SshConnection,
    pool: &Arc<SshPool>,
    session: &Arc<CachedSession>,
    deadline: Instant,
    check: &(dyn Fn() -> Result<(), String> + Sync),
) -> Result<String, DeployError> {
    check()?;
    let platform = exec_checked(
        connection,
        pool,
        session,
        "uname -s; uname -m",
        deadline,
        check,
    )
    .await?;
    let target = parse_target(&platform)?;
    let (asset, digest) = tokio::task::spawn_blocking(move || {
        let root = std::env::var_os(ASSET_ROOT_ENV).map(PathBuf::from);
        let selected = select_asset(mt_extensions::global(), root.as_deref(), target)?;
        let digest = hash_asset(&selected.path)?;
        // 扩展目录里的那份顺手对一遍清单(旧布局 / 开发根目录没有期望值,与 exe 同信任等级)
        if let Some(expected) = &selected.expected_sha256
            && *expected != digest
        {
            return Err(DeployError::Failed(format!(
                "installed remote Git helper extension for {target} does not match the extension manifest; \
                 delete it in Settings → Extensions and download it again"
            )));
        }
        Ok::<_, DeployError>((selected.path, digest))
    })
    .await
    .map_err(|e| DeployError::Failed(format!("reading remote Git helper asset: {e}")))??;
    let sftp = Arc::new(
        SftpHandle::open_on_session(session.clone(), EXEC_TIMEOUT)
            .await
            .map_err(|e| format!("opening helper deployment SFTP: {}", e.message()))?,
    );
    let mut partial: Option<PartialCleanup> = None;
    let expected = (digest.as_str(), target);
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
            exec_checked(connection, pool, session, &command, deadline, check).await?;
            require_kind(&sftp, &directory, SftpNodeKind::Directory).await?;
        }
        let helper = format!("{directory}/mt-git-helper");
        match sftp.try_node_kind(&helper).await.map_err(|e| e.message().to_string())? {
            Some(SftpNodeKind::File) => {
                verify_helper(connection, pool, session, &helper, expected, deadline, check).await?;
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
        check()?;
        mt_ssh::run_sftp_upload_until(session, local_path, &staging, deadline - FINISH_GRACE)
            .await.map_err(|e| format!("uploading remote Git helper: {}", e.message()))?;
        require_kind(&sftp, &staging, SftpNodeKind::File).await?;
        // chmod and hash only an lstat-verified regular file; every shell argument is quoted.
        let command = format!(
            "test ! -L {path} && test -f {path} && chmod 700 {path}",
            path = quote_posix(&staging),
        );
        exec_checked(connection, pool, session, &command, deadline, check).await?;
        verify_helper(connection, pool, session, &staging, expected, deadline, check).await?;
        // SFTP v3 rename fails if destination exists; never replace an executing helper.
        check()?;
        if let Err(error) = sftp.rename(&staging, &helper).await {
            // A different app process may have published the same SHA concurrently. Only
            // accept the immutable verified final file, never overwrite it or blindly retry.
            require_kind(&sftp, &helper, SftpNodeKind::File).await.map_err(|state|
                format!("publishing remote Git helper failed: {}; {state}", error.message()))?;
            verify_helper(connection, pool, session, &helper, expected, deadline, check).await?;
        }
        require_kind(&sftp, &helper, SftpNodeKind::File).await?;
        verify_helper(connection, pool, session, &helper, expected, deadline, check).await?;
        Ok(helper)
    }.await;
    // Cleanup also runs on every failure. Cancellation gets its own bounded background
    // cleanup through the guard, since an outer hard deadline can drop this future.
    if let Some(mut partial) = partial.take() {
        match tokio::time::timeout(Duration::from_secs(3), remove_partial(&sftp, &partial.path))
            .await
        {
            Ok(Ok(())) => partial.armed = false,
            _ => {
                return Err(DeployError::Failed(format!(
                    "{}; remote helper partial cleanup unconfirmed: '{}'",
                    result.err().unwrap_or_else(|| "helper deployed".into()),
                    partial.path
                )));
            }
        }
    }
    // Dropping the last SFTP reference closes its channel and releases its session lease.
    result.map_err(DeployError::Failed)
}

struct PartialCleanup {
    sftp: Arc<SftpHandle>,
    path: String,
    runtime: tokio::runtime::Handle,
    armed: bool,
}

impl PartialCleanup {
    fn new(sftp: Arc<SftpHandle>, path: String) -> Self {
        Self {
            sftp,
            path,
            runtime: tokio::runtime::Handle::current(),
            armed: true,
        }
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
                let cleanup =
                    tokio::time::timeout(Duration::from_secs(5), remove_partial(&sftp, &path))
                        .await;
                if !matches!(cleanup, Ok(Ok(()))) {
                    tracing::warn!("[remote-git] helper partial cleanup unconfirmed: '{path}'");
                }
            });
        }
    }
}

async fn remove_partial(sftp: &SftpHandle, path: &str) -> Result<(), String> {
    match sftp
        .try_node_kind(path)
        .await
        .map_err(|e| e.message().to_string())?
    {
        None => Ok(()),
        Some(SftpNodeKind::File | SftpNodeKind::Symlink) => sftp
            .remove_file(path)
            .await
            .map_err(|e| e.message().to_string()),
        Some(_) => Err(format!("unsafe helper staging entry type at '{path}'")),
    }
}

async fn require_kind(sftp: &SftpHandle, path: &str, expected: SftpNodeKind) -> Result<(), String> {
    let actual = sftp
        .try_node_kind(path)
        .await
        .map_err(|e| e.message().to_string())?;
    if actual != Some(expected) {
        return Err(format!(
            "unsafe remote Git helper path '{path}': expected {expected:?}, got {actual:?}"
        ));
    }
    Ok(())
}

fn secure_directory_command(directory: &str, private: bool) -> String {
    let path = quote_posix(directory);
    let permissions = if private {
        format!(" && chmod 700 {path}")
    } else {
        String::new()
    };
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
    expected: (&str, &str),
    deadline: Instant,
    check: &(dyn Fn() -> Result<(), String> + Sync),
) -> Result<(), String> {
    let (digest, target) = expected;
    let path_arg = quote_posix(path);
    let hash = if target.starts_with("linux-") {
        "sha256sum"
    } else {
        "shasum -a 256"
    };
    let command = format!(
        "test ! -L {path_arg} && test -f {path_arg} && test -O {path_arg} && test -x {path_arg} && {hash} {path_arg}",
    );
    let output = exec_checked(connection, pool, session, &command, deadline, check).await?;
    let actual = output.split_ascii_whitespace().next().unwrap_or_default();
    if actual != digest {
        return Err(format!(
            "remote Git helper SHA-256 mismatch at '{path}'; cached executable was not replaced"
        ));
    }
    Ok(())
}

async fn exec_checked(
    connection: &SshConnection,
    pool: &Arc<SshPool>,
    session: &Arc<CachedSession>,
    command: &str,
    deadline: Instant,
    check: &(dyn Fn() -> Result<(), String> + Sync),
) -> Result<String, String> {
    check()?;
    exec(connection, pool, session, command, deadline).await
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
    let output = run_bounded_exec_on_session(
        session,
        command,
        EXEC_TIMEOUT.min(remaining - FINISH_GRACE),
        OUTPUT_CAP,
    )
    .await
    .map_err(|error| {
        session.mark_unhealthy(Duration::from_secs(30));
        format!("remote Git helper deployment SSH exec failed: {error}")
    })?;
    if output.requires_session_retirement() {
        session.mark_unhealthy(Duration::from_secs(30));
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            pool.evict_if_same(&connection.id, session),
        )
        .await;
        return Err(
            "remote Git helper deployment channel cleanup uncertain; SSH session retired".into(),
        );
    }
    if output.timed_out
        || output.exit_code != Some(0)
        || output.stdout_truncated
        || output.stderr_truncated
    {
        return Err(format!(
            "remote Git helper deployment command failed (exit {:?}, timeout={}): {}",
            output.exit_code,
            output.timed_out,
            String::from_utf8_lossy(&output.stderr).trim(),
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
        _ => Err(format!(
            "remote Git helper does not support server platform {os:?}/{arch:?}"
        )),
    }
}

/// 选中的本机 helper。`expected_sha256` 只在来自扩展目录时有(清单里登记的哈希)。
#[derive(Debug, PartialEq, Eq)]
struct SelectedAsset {
    path: PathBuf,
    expected_sha256: Option<String>,
}

/// 按 [`Extensions::locate`] 的顺序找 `target` 的 helper(见模块注释)。三层都没有 =
/// [`DeployError::HelperMissing`];找到的那个不合规(符号链接 / 空 / 超限)= 报错,
/// **不**落到下一层去用别的二进制。
fn select_asset(
    extensions: &Extensions,
    configured: Option<&Path>,
    target: &'static str,
) -> Result<SelectedAsset, DeployError> {
    let id = ComponentId::git_helper(target)
        .ok_or_else(|| format!("remote Git helper does not support server platform {target}"))?;
    let located = extensions
        .locate(id, GIT_HELPER_FILE, configured)
        .map_err(|error| format!("inspecting remote Git helper asset for {target}: {error}"))?
        .ok_or(DeployError::HelperMissing { target })?;
    let asset = located.path;
    let metadata = std::fs::symlink_metadata(&asset).map_err(|error| {
        format!(
            "remote Git helper asset for {target} is unavailable at '{}': {error}",
            asset.display(),
        )
    })?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_ASSET_BYTES {
        return Err(DeployError::Failed(format!(
            "remote Git helper asset '{}' must be a nonempty regular file of at most {MAX_ASSET_BYTES} bytes",
            asset.display()
        )));
    }
    Ok(SelectedAsset {
        path: asset,
        expected_sha256: located.expected.map(|(sha256, _)| sha256),
    })
}

fn hash_asset(path: &Path) -> Result<String, String> {
    let mut file =
        File::open(path).map_err(|e| format!("opening helper asset '{}': {e}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let size = file
            .read(&mut buffer)
            .map_err(|e| format!("reading helper asset '{}': {e}", path.display()))?;
        if size == 0 {
            break;
        }
        total += size as u64;
        if total > MAX_ASSET_BYTES {
            return Err("remote Git helper asset exceeded size limit while hashing".into());
        }
        hash.update(&buffer[..size]);
    }
    if total == 0 {
        return Err("remote Git helper asset is empty".into());
    }
    use std::fmt::Write as _;
    let mut digest = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(&mut digest, "{byte:02x}").expect("writing to String");
    }
    Ok(digest)
}

fn partial_path(directory: &str) -> String {
    let sequence = PARTIAL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{directory}/.mt-git-helper-{}-{time:x}-{sequence:x}.partial",
        std::process::id()
    )
}

#[cfg(test)]
#[path = "git_deploy_tests.rs"]
mod tests;
