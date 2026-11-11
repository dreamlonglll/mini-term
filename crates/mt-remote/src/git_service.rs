//! App-owned remote Git workspaces. Construction and close never perform network IO.
//! Requests block and must run on the caller's background executor.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use mt_config::SshConnection;
use mt_git_helper::{Operation, Reply};

use super::git::validate_absolute_path;
use super::{RemoteGitClient, connection_fingerprint, lock, state};

#[path = "git_deploy.rs"]
mod deploy;

static WORKSPACES: LazyLock<Mutex<Vec<Weak<WorkspaceInner>>>> = LazyLock::new(Default::default);
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// A connection/configuration identity and a case-sensitive remote POSIX root.
/// Clones share a single lazy helper, request serialization, and uncertain-write state.
#[derive(Clone)]
pub struct RemoteGitWorkspace {
    inner: Arc<WorkspaceInner>,
}

struct WorkspaceInner {
    connection: SshConnection,
    fingerprint: u64,
    root: String,
    epoch: AtomicU64,
    invalidated: AtomicBool,
    client: Mutex<WorkspaceClient>,
}

#[derive(Default)]
struct WorkspaceClient {
    client: Option<RemoteGitClient>,
    epoch: u64,
    write_outcome_unknown: bool,
}

impl RemoteGitWorkspace {
    /// Pure construction: does not acquire SSH, initialize a runtime, or inspect assets.
    pub fn new(connection: SshConnection, root: String) -> Self {
        let inner = Arc::new(WorkspaceInner {
            fingerprint: connection_fingerprint(&connection),
            connection,
            root,
            epoch: AtomicU64::new(0),
            invalidated: AtomicBool::new(false),
            client: Mutex::new(WorkspaceClient::default()),
        });
        let mut registry = lock(&WORKSPACES);
        registry.retain(|workspace| workspace.strong_count() != 0);
        registry.push(Arc::downgrade(&inner));
        Self { inner }
    }

    /// Unambiguous UI/repository gate identity; never host-platform case-folds POSIX paths.
    pub fn repository_key(&self, repo: &str) -> String {
        // Length-prefix strings because connection IDs and POSIX paths may contain `|`.
        format!(
            "ssh|{}:{}|{:016x}|{}:{}|{}:{}",
            self.inner.connection.id.len(),
            self.inner.connection.id,
            self.inner.fingerprint,
            self.inner.root.len(),
            self.inner.root,
            repo.len(),
            repo,
        )
    }

    /// Send one operation, once. A failed write is never replayed, including on reconnect.
    /// After an uncertain write, an explicit successful read is required before another write.
    pub fn request(&self, operation: Operation) -> Result<Reply, String> {
        self.request_typed(operation)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn request_typed(
        &self,
        operation: Operation,
    ) -> Result<Reply, mt_git_helper::GitFailure> {
        self.inner.ensure_active()?;
        validate_absolute_path(&self.inner.root, "root")?;
        let epoch = self.inner.epoch.load(Ordering::Acquire);
        let mut slot = lock(&self.inner.client);
        self.inner.ensure_active()?;
        if epoch != self.inner.epoch.load(Ordering::Acquire) {
            return Err("remote Git workspace was closed while this request was waiting".into());
        }
        if slot.epoch != epoch {
            // Drop schedules bounded async cleanup, rather than blocking on helper exit.
            slot.client.take();
            slot.epoch = epoch;
        }
        let write = operation.is_write();
        require_verified_write_state(slot.write_outcome_unknown, write)?;
        if slot.client.is_none() {
            // Deploy has one async boundary; connecting starts only after it has returned.
            // Never call a blocking client API inside this runtime's block_on.
            let helper = deploy::ensure_helper(&self.inner.connection)?;
            self.inner.ensure_active()?;
            if epoch != self.inner.epoch.load(Ordering::Acquire) {
                return Err("remote Git workspace was closed during helper deployment".into());
            }
            slot.client = Some(RemoteGitClient::connect(
                &self.inner.connection,
                &helper,
                &self.inner.root,
            )?);
        }
        if let Err(error) = self.inner.ensure_active() {
            slot.client.take();
            return Err(error.into());
        }
        if epoch != self.inner.epoch.load(Ordering::Acquire) {
            slot.client.take();
            return Err("remote Git workspace was closed during helper connection".into());
        }
        let client = slot.client.as_mut().expect("initialized above");
        let result = client.request_typed(operation);
        if !client.is_connected() {
            slot.client.take();
            if write {
                slot.write_outcome_unknown = true;
            }
        }
        if result.is_ok() && !write {
            slot.write_outcome_unknown = false;
        }
        result
    }

    /// Cancel queued acquisition and schedule cleanup, without waiting for an in-flight request.
    /// A later explicit request may open a fresh helper, but cannot bypass unknown-write state.
    pub fn close(&self) {
        self.inner.schedule_close();
    }
}

impl WorkspaceInner {
    fn ensure_active(&self) -> Result<(), String> {
        if SHUTTING_DOWN.load(Ordering::Acquire) {
            return Err("remote Git service is shutting down".into());
        }
        if self.invalidated.load(Ordering::Acquire) {
            return Err(
                "SSH connection configuration changed; reopen this remote Git workspace".into(),
            );
        }
        Ok(())
    }

    fn schedule_close(self: &Arc<Self>) {
        let closed_epoch = self.epoch.fetch_add(1, Ordering::AcqRel);
        // A never-used workspace must not create a runtime merely to close it.
        let runtime = lock(&state().runtime).clone();
        if let Some(runtime) = runtime {
            let inner = self.clone();
            runtime.spawn_blocking(move || {
                let mut slot = lock(&inner.client);
                if slot.epoch <= closed_epoch {
                    slot.client.take();
                }
            });
        }
    }
}

fn require_verified_write_state(unknown: bool, write: bool) -> Result<(), String> {
    if unknown && write {
        return Err("previous remote Git write outcome is unknown; refresh or discover the repository successfully before another write; no request was replayed".into());
    }
    Ok(())
}

fn active_workspaces() -> Vec<Arc<WorkspaceInner>> {
    let mut registry = lock(&WORKSPACES);
    let active: Vec<_> = registry.iter().filter_map(Weak::upgrade).collect();
    registry.retain(|workspace| workspace.strong_count() != 0);
    active
}

pub(crate) fn invalidate_workspaces(connection_id: &str) {
    for inner in active_workspaces() {
        if inner.connection.id == connection_id {
            inner.invalidated.store(true, Ordering::Release);
            inner.schedule_close();
        }
    }
}

pub(crate) fn shutdown_workspaces() {
    SHUTTING_DOWN.store(true, Ordering::Release);
    for inner in active_workspaces() {
        inner.schedule_close();
    }
}

#[cfg(test)]
#[path = "git_service_tests.rs"]
mod tests;
