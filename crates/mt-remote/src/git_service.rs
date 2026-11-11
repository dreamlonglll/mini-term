//! App-owned remote Git workspaces: bounded serial actors, revocable permission and no replay.
//! Synchronous adapters are restricted to [`run_git`]; GPUI only awaits that executor.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use mt_config::SshConnection;
use mt_git_helper::{GitFailure, GitQuery, Operation, Reply};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot};

use super::git::validate_absolute_path;
use super::{RemoteGitClient, connection_fingerprint, lock, state};

#[path = "git_deploy.rs"]
mod deploy;

const QUEUE_CAPACITY: usize = 32;
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
static WORKSPACES: LazyLock<Mutex<Vec<Weak<WorkspaceInner>>>> = LazyLock::new(Default::default);
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
static REMOTE_GIT_ENABLED: AtomicBool = AtomicBool::new(false);
static PERMISSION_EPOCH: AtomicU64 = AtomicU64::new(0);
static EXECUTOR_SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(8)));
thread_local! { static IN_GIT_EXECUTOR: Cell<bool> = const { Cell::new(false) }; }

/// Run synchronous Git/core IO without occupying a GPUI executor thread. Admission waits
/// asynchronously; at most eight closures occupy our runtime's blocking executor at once.
/// Dropping the future does not cancel a closure that has started, especially a remote write.
pub async fn run_git<F, T>(op: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let permit = EXECUTOR_SLOTS
        .clone()
        .acquire_owned()
        .await
        .expect("Git executor remains open");
    let runtime = state().runtime().expect("remote_git_runtime_unavailable");
    let task = runtime.spawn_blocking(move || {
        let _permit = permit;
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                IN_GIT_EXECUTOR.set(false);
            }
        }
        IN_GIT_EXECUTOR.set(true);
        let _reset = Reset;
        op()
    });
    let result = task.await;
    // The closure owns admission even if its awaiting caller is cancelled.
    match result {
        Ok(value) => value,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(_) => panic!("remote_git_executor_stopped"),
    }
}

/// Process-wide desktop permission. Explicit-path [`RemoteGitClient`] is independent.
/// Revocation invalidates queued work and prevents further deployment/dispatch; it does
/// not claim to cancel or roll back an already dispatched operation.
pub fn set_remote_git_enabled(enabled: bool) {
    if REMOTE_GIT_ENABLED.swap(enabled, Ordering::AcqRel) != enabled {
        PERMISSION_EPOCH.fetch_add(1, Ordering::AcqRel);
        if !enabled {
            for inner in active_workspaces() {
                inner.schedule_close();
            }
        }
    }
}

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
    // Covers dispatched writes until a known response or explicit completion verification.
    outcome_unknown: AtomicBool,
    sender: Mutex<Option<mpsc::Sender<QueuedRequest>>>,
    changed: Arc<Notify>,
    factory: Arc<TransportFactory>,
    idle_timeout: Duration,
}

type TransportFactory =
    dyn Fn(&WorkspaceLease) -> Result<Box<dyn WorkspaceTransport>, GitFailure> + Send + Sync;

trait WorkspaceTransport: Send {
    fn request(&mut self, operation: Operation) -> Result<Reply, GitFailure>;
    fn confirm_pending_write(&mut self) -> Result<(), GitFailure>;
    fn mark_last_write_unknown(&mut self);
    fn is_connected(&self) -> bool;
}

impl WorkspaceTransport for RemoteGitClient {
    fn request(&mut self, operation: Operation) -> Result<Reply, GitFailure> {
        self.request_typed(operation)
    }
    fn confirm_pending_write(&mut self) -> Result<(), GitFailure> {
        RemoteGitClient::prove_pending_write_completed(self)
    }
    fn mark_last_write_unknown(&mut self) {
        RemoteGitClient::mark_last_write_unknown(self);
    }
    fn is_connected(&self) -> bool {
        RemoteGitClient::is_connected(self)
    }
}

#[derive(Clone)]
struct WorkspaceLease {
    inner: Arc<WorkspaceInner>,
    epoch: u64,
    permission: u64,
}

impl WorkspaceLease {
    fn check(&self) -> Result<(), GitFailure> {
        self.inner.ensure_active()?;
        if self.epoch != self.inner.epoch.load(Ordering::Acquire) {
            return Err(service_error("remote_git_request_cancelled"));
        }
        if self.permission != PERMISSION_EPOCH.load(Ordering::Acquire) {
            return Err(service_error("remote_git_permission_changed"));
        }
        Ok(())
    }
}

type Decoder = Box<dyn FnOnce(Reply) -> Result<Reply, String> + Send>;
enum Command {
    Request(Operation, Option<Decoder>),
    VerifyUnknown(String),
}
struct QueuedRequest {
    lease: WorkspaceLease,
    command: Command,
    reply: oneshot::Sender<Result<Reply, GitFailure>>,
}

#[derive(Default)]
struct WorkspaceClient {
    client: Option<Box<dyn WorkspaceTransport>>,
    epoch: u64,
    write_outcome_unknown: bool,
    unknown_repo: Option<String>,
    // Preserve the uncertain identity even if every pane drops its workspace handle.
    unknown_owner: Option<Arc<WorkspaceInner>>,
    completion_proven: bool,
}

impl RemoteGitWorkspace {
    /// Pure construction: no runtime, assets or SSH. Equal identities share their actor
    /// and unknown-write latch even when independently constructed by separate panes.
    pub fn new(connection: SshConnection, root: String) -> Self {
        let fingerprint = connection_fingerprint(&connection);
        let mut registry = lock(&WORKSPACES);
        registry.retain(|workspace| workspace.strong_count() != 0);
        if let Some(inner) = registry.iter().filter_map(Weak::upgrade).find(|inner| {
            (!inner.invalidated.load(Ordering::Acquire)
                || inner.outcome_unknown.load(Ordering::Acquire))
                && inner.connection.id == connection.id
                && inner.fingerprint == fingerprint
                && inner.root == root
        }) {
            return Self { inner };
        }
        let inner = Arc::new(WorkspaceInner {
            fingerprint,
            connection,
            root,
            epoch: AtomicU64::new(0),
            invalidated: AtomicBool::new(false),
            sender: Mutex::new(None),
            outcome_unknown: AtomicBool::new(false),
            changed: Arc::new(Notify::new()),
            factory: Arc::new(connect_transport),
            idle_timeout: IDLE_TIMEOUT,
        });
        registry.push(Arc::downgrade(&inner));
        Self { inner }
    }

    pub fn repository_key(&self, repo: &str) -> String {
        format!(
            "ssh|{}:{}|{:016x}|{}:{}|{}:{}",
            self.inner.connection.id.len(),
            self.inner.connection.id,
            self.inner.fingerprint,
            self.inner.root.len(),
            self.inner.root,
            repo.len(),
            repo
        )
    }

    pub fn write_outcome_unknown(&self) -> bool {
        self.inner.outcome_unknown.load(Ordering::Acquire)
    }

    pub fn request(&self, operation: Operation) -> Result<Reply, String> {
        self.request_typed(operation)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn request_typed(&self, operation: Operation) -> Result<Reply, GitFailure> {
        require_executor()?;
        let runtime = state()
            .runtime()
            .map_err(|e| failure("remote_git_runtime_unavailable", e))?;
        runtime.block_on(self.request_async(operation))
    }

    /// A bounded, nonblocking enqueue. Full queues fail without dispatching or replaying.
    /// Dropping a queued future cancels it; dropping a dispatched write cannot cancel it.
    pub async fn request_async(&self, operation: Operation) -> Result<Reply, GitFailure> {
        self.enqueue(Command::Request(operation, None)).await
    }

    pub(crate) fn request_data<T>(&self, operation: Operation) -> Result<T, GitFailure>
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        require_executor()?;
        let runtime = state()
            .runtime()
            .map_err(|e| failure("remote_git_runtime_unavailable", e))?;
        runtime.block_on(async {
            let (value_tx, value_rx) = oneshot::channel();
            let decoder: Decoder = Box::new(move |reply| {
                let value = reply.decode::<T>()?;
                let _ = value_tx.send(value);
                Ok(Reply::Updated)
            });
            self.enqueue(Command::Request(operation, Some(decoder)))
                .await?;
            value_rx
                .await
                .map_err(|_| service_error("remote_git_reply_decode_failed"))
        })
    }

    /// Explicit user verification only: prove the previous helper finished the write,
    /// then inspect HEAD/refs, index and status on a fresh channel. A lost old channel
    /// cannot supply that proof, so the write gate remains locked (no unsafe override).
    pub async fn verify_unknown_write(&self, repo: String) -> Result<(), GitFailure> {
        self.enqueue(Command::VerifyUnknown(repo)).await.map(|_| ())
    }

    pub fn verify_unknown_write_blocking(&self, repo: &str) -> Result<(), GitFailure> {
        require_executor()?;
        let runtime = state()
            .runtime()
            .map_err(|e| failure("remote_git_runtime_unavailable", e))?;
        runtime.block_on(self.verify_unknown_write(repo.to_owned()))
    }

    async fn enqueue(&self, command: Command) -> Result<Reply, GitFailure> {
        self.inner.ensure_active()?;
        validate_absolute_path(&self.inner.root, "root")
            .map_err(|e| failure("remote_git_invalid_root", e))?;
        let lease = WorkspaceLease {
            inner: self.inner.clone(),
            epoch: self.inner.epoch.load(Ordering::Acquire),
            permission: PERMISSION_EPOCH.load(Ordering::Acquire),
        };
        lease.check()?;
        let (reply, receiver) = oneshot::channel();
        let sender = {
            let mut sender = lock(&self.inner.sender);
            if sender.is_none() {
                let runtime = state()
                    .runtime()
                    .map_err(|e| failure("remote_git_runtime_unavailable", e))?;
                let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
                *sender = Some(tx);
                runtime.spawn(actor(Arc::downgrade(&self.inner), rx));
            }
            sender.as_ref().expect("initialized above").clone()
        };
        sender
            .try_send(QueuedRequest {
                lease,
                command,
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => service_error("remote_git_queue_full"),
                mpsc::error::TrySendError::Closed(_) => service_error("remote_git_actor_stopped"),
            })?;
        receiver
            .await
            .map_err(|_| service_error("remote_git_actor_stopped"))?
    }

    /// Cancel queued identity immediately. In-flight work finishes before channel cleanup;
    /// unknown-write proof channels are preserved rather than pretending a close is proof.
    pub fn close(&self) {
        self.inner.schedule_close();
    }
}

fn require_executor() -> Result<(), GitFailure> {
    if IN_GIT_EXECUTOR.get() {
        Ok(())
    } else {
        Err(service_error("remote_git_executor_required"))
    }
}

fn failure(code: &str, diagnostic: impl std::fmt::Display) -> GitFailure {
    GitFailure::service(code, diagnostic)
}

fn service_error(code: &str) -> GitFailure {
    failure(code, "")
}

fn request_failure(error: GitFailure) -> GitFailure {
    match error {
        GitFailure::Message { message } => failure("remote_git_request_failed", message),
        other => other,
    }
}

fn connect_transport(lease: &WorkspaceLease) -> Result<Box<dyn WorkspaceTransport>, GitFailure> {
    lease.check()?;
    let helper = deploy::ensure_helper(&lease.inner.connection, &|| {
        lease.check().map_err(|e| e.to_string())
    })
    .map_err(|e| failure("remote_git_deployment_failed", e))?;
    lease.check()?;
    let client = RemoteGitClient::connect(&lease.inner.connection, &helper, &lease.inner.root)
        .map_err(|e| {
            let code = if e.starts_with("Git helper protocol mismatch:") {
                "remote_git_protocol_mismatch"
            } else {
                "remote_git_connection_failed"
            };
            failure(code, e)
        })?;
    lease.check()?;
    Ok(Box::new(client))
}

async fn actor(inner: Weak<WorkspaceInner>, mut receiver: mpsc::Receiver<QueuedRequest>) {
    let mut slot = WorkspaceClient::default();
    loop {
        let Some(workspace) = inner.upgrade() else {
            break;
        };
        let timeout = workspace.idle_timeout;
        let changed = workspace.changed.clone();
        let hard_invalidated = workspace.invalidated.load(Ordering::Acquire)
            || SHUTTING_DOWN.load(Ordering::Acquire)
            || !REMOTE_GIT_ENABLED.load(Ordering::Acquire);
        drop(workspace);
        if hard_invalidated {
            slot.client.take();
        }
        let next = tokio::select! {
            request = receiver.recv() => request,
            _ = changed.notified() => {
                if !slot.write_outcome_unknown { slot.client.take(); }
                continue;
            },
            _ = tokio::time::sleep(timeout) => {
                // No idle timer runs during an in-flight request.
                if !slot.write_outcome_unknown { slot.client.take(); }
                continue;
            },
        };
        let Some(mut request) = next else { break };
        if request.reply.is_closed() {
            continue;
        }
        let runtime = match state().runtime() {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = request
                    .reply
                    .send(Err(failure("remote_git_runtime_unavailable", error)));
                continue;
            }
        };
        // One task per actor, never a mutex waiter per request. State moves exclusively.
        let task = runtime.spawn_blocking(move || {
            let mut slot = slot;
            let result = dispatch(&mut slot, &mut request);
            let _ = request.reply.send(result);
            slot
        });
        match task.await {
            Ok(next) => slot = next,
            Err(_) => break,
        }
    }
}

fn dispatch(slot: &mut WorkspaceClient, request: &mut QueuedRequest) -> Result<Reply, GitFailure> {
    if request.reply.is_closed() {
        return Err(service_error("remote_git_request_cancelled"));
    }
    request.lease.check()?;
    if slot.epoch != request.lease.epoch {
        if !slot.write_outcome_unknown {
            slot.client.take();
        }
        slot.epoch = request.lease.epoch;
    }
    let command = std::mem::replace(&mut request.command, Command::VerifyUnknown(String::new()));
    match command {
        Command::VerifyUnknown(repo) => recover(slot, &request.lease, &repo),
        Command::Request(operation, decoder) => {
            let write = operation.is_write();
            if slot.write_outcome_unknown {
                if write {
                    return Err(unknown_failure("remote_git_write_verification_required"));
                }
                // Automatic reads never consume the old channel's barrier proof.
                let mut reader = (request.lease.inner.factory)(&request.lease)?;
                request.lease.check()?;
                let reply = reader.request(operation).map_err(request_failure)?;
                return decode_reply(reply, decoder, false, reader.as_mut());
            }
            if slot.client.is_none() {
                slot.client = Some((request.lease.inner.factory)(&request.lease)?);
            }
            request.lease.check()?;
            let repo = if write {
                operation_repo(&operation).map(str::to_owned)
            } else {
                None
            };
            if write {
                request
                    .lease
                    .inner
                    .outcome_unknown
                    .store(true, Ordering::Release);
            }
            let client = slot.client.as_mut().expect("initialized above");
            let result = client
                .request(operation)
                .map_err(request_failure)
                .and_then(|reply| decode_reply(reply, decoder, write, client.as_mut()));
            if matches!(&result, Err(GitFailure::OutcomeUnknown { .. })) {
                slot.write_outcome_unknown = true;
                slot.unknown_repo = repo;
                slot.unknown_owner = Some(request.lease.inner.clone());
                slot.completion_proven = false;
            } else if write {
                request
                    .lease
                    .inner
                    .outcome_unknown
                    .store(false, Ordering::Release);
            }
            if !client.is_connected() && !slot.write_outcome_unknown {
                slot.client.take();
            }
            // Permission/config epochs reject queued work, not an operation already sent.
            // Return the captured repository's actual result; UI identity guards decide
            // whether to refresh the currently visible pane.
            result
        }
    }
}

fn operation_repo(operation: &Operation) -> Option<&str> {
    // Keep all legacy and generic mutation entry points bound to the affected repo.
    match operation {
        Operation::Stage { repo, .. }
        | Operation::Unstage { repo, .. }
        | Operation::StageAll { repo }
        | Operation::UnstageAll { repo }
        | Operation::Commit { repo, .. }
        | Operation::Mutate { repo, .. }
        | Operation::CommitWith { repo, .. }
        | Operation::Discard { repo, .. }
        | Operation::StageHunk { repo, .. }
        | Operation::UnstageHunk { repo, .. }
        | Operation::DiscardHunk { repo, .. }
        | Operation::Checkout { repo, .. }
        | Operation::CreateBranch { repo, .. }
        | Operation::RenameBranch { repo, .. }
        | Operation::DeleteBranch { repo, .. }
        | Operation::Fetch { repo }
        | Operation::Pull { repo, .. }
        | Operation::Push { repo, .. }
        | Operation::SaveStash { repo, .. }
        | Operation::ApplyStash { repo, .. }
        | Operation::DropStash { repo, .. }
        | Operation::OperationContinue { repo, .. }
        | Operation::OperationAbort { repo, .. }
        | Operation::ResolveConflict { repo, .. }
        | Operation::MarkResolved { repo, .. } => Some(repo),
        _ => None,
    }
}

fn decode_reply(
    reply: Reply,
    decoder: Option<Decoder>,
    write: bool,
    client: &mut dyn WorkspaceTransport,
) -> Result<Reply, GitFailure> {
    let Some(decoder) = decoder else {
        return Ok(reply);
    };
    decoder(reply).map_err(|error| {
        if write {
            client.mark_last_write_unknown();
            GitFailure::outcome_unknown(format_args!("remote_git_write_decode_failed: {error}"))
        } else {
            failure("remote_git_reply_decode_failed", error)
        }
    })
}

fn unknown_failure(code: &str) -> GitFailure {
    GitFailure::outcome_unknown(code)
}

fn recover(
    slot: &mut WorkspaceClient,
    lease: &WorkspaceLease,
    repo: &str,
) -> Result<Reply, GitFailure> {
    if !slot.write_outcome_unknown {
        return Ok(Reply::Updated);
    }
    if slot.unknown_repo.as_deref() != Some(repo) {
        return Err(unknown_failure("remote_git_recovery_repository_mismatch"));
    }
    let old = slot
        .client
        .as_mut()
        .ok_or_else(|| unknown_failure("remote_git_completion_proof_unavailable"))?;
    if !slot.completion_proven {
        old.confirm_pending_write().map_err(|error| {
            GitFailure::outcome_unknown(format_args!(
                "remote_git_completion_proof_unavailable: {error}"
            ))
        })?;
        slot.completion_proven = true;
    }
    lease.check()?;
    let mut fresh = (lease.inner.factory)(lease)?;
    let signature = fresh
        .request(Operation::Query {
            repo: repo.into(),
            query: GitQuery::RepoSignature,
        })
        .map_err(request_failure)?;
    let _: mt_project::git::RepoSignature = signature
        .decode()
        .map_err(|e| failure("remote_git_recovery_decode_failed", e))?;
    match fresh
        .request(Operation::Status { repo: repo.into() })
        .map_err(request_failure)?
    {
        Reply::Status { .. } => {}
        _ => return Err(service_error("remote_git_recovery_invalid_status")),
    }
    lease.check()?;
    slot.client = Some(fresh);
    slot.write_outcome_unknown = false;
    lease.inner.outcome_unknown.store(false, Ordering::Release);
    slot.unknown_repo = None;
    slot.unknown_owner = None;
    slot.completion_proven = false;
    Ok(Reply::Updated)
}

impl WorkspaceInner {
    fn ensure_active(&self) -> Result<(), GitFailure> {
        if SHUTTING_DOWN.load(Ordering::Acquire) {
            return Err(service_error("remote_git_shutting_down"));
        }
        if self.invalidated.load(Ordering::Acquire) {
            return Err(service_error("remote_git_connection_changed"));
        }
        if !REMOTE_GIT_ENABLED.load(Ordering::Acquire) {
            return Err(service_error("remote_git_disabled"));
        }
        Ok(())
    }
    fn schedule_close(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.changed.notify_one();
    }
}

fn active_workspaces() -> Vec<Arc<WorkspaceInner>> {
    let mut registry = lock(&WORKSPACES);
    let active = registry.iter().filter_map(Weak::upgrade).collect();
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
