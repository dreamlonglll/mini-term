//! App-owned remote Git actors: scoped permission, no write replay and explicit recovery.
//! Synchronous adapters run only on the independently admitted Git executors.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, SystemTime};

use mt_config::SshConnection;
use mt_git_helper::{GitFailure, GitQuery, Operation, Reply};
use mt_project::git::{ChangeFileStatus, RepoOpState, RepoSignature};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot};

use super::git::validate_absolute_path;
use super::{GitContext, RemoteGitClient, connection_fingerprint, lock, state};

#[path = "git_deploy.rs"]
mod deploy;

const QUEUE_CAPACITY: usize = 32;
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
static WORKSPACES: LazyLock<Mutex<Vec<Weak<WorkspaceInner>>>> = LazyLock::new(Default::default);
// An explicit registry, not an actor -> sender -> actor ownership cycle. Entries are
// removed on acknowledgement; retaining one uncertain identity preserves its proof channel.
static UNKNOWN_WORKSPACES: LazyLock<Mutex<Vec<Arc<WorkspaceInner>>>> =
    LazyLock::new(Default::default);
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
static GRANTS: LazyLock<Mutex<HashMap<String, Grant>>> = LazyLock::new(Default::default);
static SOURCE_LIFECYCLE: Mutex<()> = Mutex::new(());
static SEQUENCE: AtomicU64 = AtomicU64::new(1);
static LOCAL_EXECUTOR_SLOTS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(8)));
static REMOTE_EXECUTOR_SLOTS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(8)));
thread_local! { static IN_GIT_EXECUTOR: Cell<bool> = const { Cell::new(false) }; }

/// Serialize source pending checks/config mutation/grant revocation with the final write
/// dispatch boundary. Callers hold this only for synchronous state changes, never IO.
/// Permission setters, invalidation and workspace construction do not acquire this lock.
pub fn remote_git_source_change_guard() -> std::sync::MutexGuard<'static, ()> {
    lock(&SOURCE_LIFECYCLE)
}

/// Local-only executor. Remote callers must select their independent quota with `run_git_on`.
pub async fn run_git<F, T>(op: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    execute(false, op).await
}

/// Admission is asynchronous, including when all remote slots are occupied. A cancelled
/// await does not release the permit of a closure that is already executing.
pub async fn run_git_on<F, T>(context: &GitContext, op: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    execute(context.is_remote(), op).await
}

async fn execute<F, T>(remote: bool, op: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let slots = if remote {
        &REMOTE_EXECUTOR_SLOTS
    } else {
        &LOCAL_EXECUTOR_SLOTS
    };
    let permit = Arc::clone(slots)
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
    match task.await {
        Ok(value) => value,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(_) => panic!("remote_git_executor_stopped"),
    }
}

#[derive(PartialEq, Eq)]
struct Grant {
    connection_id: String,
    fingerprint: u64,
    root: String,
    generation: u64,
}

/// Replace only this project's grant. False always revokes, independent of the supplied
/// endpoint. Identical grants are idempotent; revocation never claims to cancel a sent write.
pub fn set_project_git_permission(
    project_id: &str,
    connection: Option<&SshConnection>,
    root: &str,
    enabled: bool,
) {
    let mut grants = lock(&GRANTS);
    let identity = connection.filter(|_| enabled).map(|connection| Grant {
        connection_id: connection.id.clone(),
        fingerprint: connection_fingerprint(connection),
        root: root.to_owned(),
        generation: 0,
    });
    if let (Some(old), Some(new)) = (grants.get(project_id), identity.as_ref()) {
        if old.connection_id == new.connection_id
            && old.fingerprint == new.fingerprint
            && old.root == new.root
        {
            return;
        }
    } else if grants.get(project_id).is_none() && identity.is_none() {
        return;
    }
    match identity {
        Some(mut grant) => {
            grant.generation = SEQUENCE.fetch_add(1, Ordering::AcqRel);
            grants.insert(project_id.to_owned(), grant);
        }
        None => {
            grants.remove(project_id);
        }
    }
    drop(grants);
    for inner in active_workspaces() {
        if inner.project_id == project_id {
            // A new grant for the exact endpoint restores the original uncertain identity,
            // never redirects a pending write to a different connection.
            if inner.ensure_granted().is_ok() {
                inner.invalidated.store(false, Ordering::Release);
            }
            inner.schedule_close();
        }
    }
}

fn same_endpoint(a: &SshConnection, b: &SshConnection) -> bool {
    a.host == b.host && a.port == b.port && a.user == b.user
}

pub(crate) fn repository_identity(connection: &SshConnection, repo: &str) -> String {
    let repo = mt_core::path_key::posix_trim_trailing(repo);
    format!(
        "ssh|{}:{}|{}|{}:{}|{}:{}",
        connection.host.len(),
        connection.host,
        connection.port,
        connection.user.len(),
        connection.user,
        repo.len(),
        repo
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteGitUnknownWrite {
    pub generation: u64,
    pub repo: String,
    pub operation: String,
    pub occurred: SystemTime,
}

/// Observations, not proof of a lost helper's completion. The private token binds explicit
/// acknowledgement to the exact workspace, grant/config epoch and last successful probe.
#[derive(Clone, Debug)]
pub struct RemoteGitRecoverySnapshot {
    pub generation: u64,
    pub repo: String,
    pub operation: String,
    pub occurred: SystemTime,
    pub head: Option<String>,
    pub signature: RepoSignature,
    pub op: RepoOpState,
    pub files: Vec<ChangeFileStatus>,
    pub observed: SystemTime,
    pub completion_proven: bool,
    token: u64,
    epoch: u64,
    permission: u64,
}

#[derive(Clone)]
pub struct RemoteGitWorkspace {
    inner: Arc<WorkspaceInner>,
}

struct WorkspaceInner {
    project_id: String,
    connection: SshConnection,
    fingerprint: u64,
    root: String,
    epoch: AtomicU64,
    invalidated: AtomicBool,
    running: AtomicBool,
    unknown: Mutex<Option<RemoteGitUnknownWrite>>,
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
    fn probe_idle(&mut self, repo: &str) -> Result<(), GitFailure> {
        self.request(Operation::Query {
            repo: repo.to_owned(),
            query: GitQuery::RepoSignature,
        })?
        .decode::<RepoSignature>()
        .map(|_| ())
        .map_err(|e| failure("remote_git_probe_failed", e))
    }
}
impl WorkspaceTransport for RemoteGitClient {
    fn request(&mut self, operation: Operation) -> Result<Reply, GitFailure> {
        self.request_typed(operation)
    }
    fn confirm_pending_write(&mut self) -> Result<(), GitFailure> {
        self.prove_pending_write_completed()
    }
    fn mark_last_write_unknown(&mut self) {
        RemoteGitClient::mark_last_write_unknown(self);
    }
    fn is_connected(&self) -> bool {
        RemoteGitClient::is_connected(self)
    }
    fn probe_idle(&mut self, repo: &str) -> Result<(), GitFailure> {
        RemoteGitClient::probe_idle(self, repo)
    }
}

#[derive(Clone)]
struct WorkspaceLease {
    // Current granted credentials perform new reads; the owner retains the original
    // operation/proof identity through credential rotations.
    inner: Arc<WorkspaceInner>,
    owner: Arc<WorkspaceInner>,
    epoch: u64,
    authority_epoch: u64,
    permission: u64,
}
impl WorkspaceLease {
    fn check(&self) -> Result<(), GitFailure> {
        let permission = self.inner.ensure_active()?;
        if self.epoch != self.owner.epoch.load(Ordering::Acquire)
            || self.authority_epoch != self.inner.epoch.load(Ordering::Acquire)
        {
            return Err(service_error("remote_git_request_cancelled"));
        }
        if self.owner.project_id != self.inner.project_id
            || self.owner.root != self.inner.root
            || !same_endpoint(&self.owner.connection, &self.inner.connection)
        {
            return Err(service_error("remote_git_recovery_repository_mismatch"));
        }
        if self.permission != permission {
            return Err(service_error("remote_git_permission_changed"));
        }
        Ok(())
    }
}

type Decoder = Box<dyn FnOnce(Reply) -> Result<Reply, String> + Send>;
enum Command {
    Request(Operation, Option<Decoder>),
    Snapshot(oneshot::Sender<RemoteGitRecoverySnapshot>),
    Acknowledge(RemoteGitRecoverySnapshot),
}
struct QueuedRequest {
    lease: WorkspaceLease,
    command: Command,
    reply: oneshot::Sender<Result<Reply, GitFailure>>,
}
#[derive(Default)]
struct WorkspaceClient {
    client: Option<Box<dyn WorkspaceTransport>>,
    reader: Option<Box<dyn WorkspaceTransport>>,
    epoch: u64,
    completion_proven: bool,
    snapshot: Option<RemoteGitRecoverySnapshot>,
    authority: Weak<WorkspaceInner>,
    permission: u64,
}

impl RemoteGitWorkspace {
    /// Pure construction. Actor and permission identity include the originating project.
    pub fn new(project_id: String, connection: SshConnection, root: String) -> Self {
        let fingerprint = connection_fingerprint(&connection);
        let mut registry = lock(&WORKSPACES);
        registry.retain(|workspace| workspace.strong_count() != 0);
        if let Some(inner) = registry.iter().filter_map(Weak::upgrade).find(|inner| {
            (!inner.invalidated.load(Ordering::Acquire) || lock(&inner.unknown).is_some())
                && inner.project_id == project_id
                && inner.connection.id == connection.id
                && inner.fingerprint == fingerprint
                && inner.root == root
        }) {
            return Self { inner };
        }
        let inner = Arc::new(WorkspaceInner {
            project_id,
            fingerprint,
            connection,
            root,
            epoch: AtomicU64::new(0),
            invalidated: AtomicBool::new(false),
            running: AtomicBool::new(false),
            unknown: Mutex::new(None),
            sender: Mutex::new(None),
            changed: Arc::new(Notify::new()),
            factory: Arc::new(connect_transport),
            idle_timeout: IDLE_TIMEOUT,
        });
        registry.push(Arc::downgrade(&inner));
        Self { inner }
    }

    /// Repository mutation gates deliberately exclude project/grant identity.
    pub fn repository_key(&self, repo: &str) -> String {
        repository_identity(&self.inner.connection, repo)
    }
    pub fn write_running(&self) -> bool {
        self.inner.running.load(Ordering::Acquire)
            || active_workspaces().iter().any(|other| {
                other.project_id == self.inner.project_id
                    && other.root == self.inner.root
                    && same_endpoint(&other.connection, &self.inner.connection)
                    && other.running.load(Ordering::Acquire)
            })
    }
    pub fn write_outcome_unknown(&self) -> bool {
        self.unknown_write().is_some()
    }
    pub fn unknown_write(&self) -> Option<RemoteGitUnknownWrite> {
        self.unknown_owner()
            .and_then(|owner| lock(&owner.unknown).clone())
    }
    fn unknown_owner(&self) -> Option<Arc<WorkspaceInner>> {
        lock(&UNKNOWN_WORKSPACES)
            .iter()
            .find(|owner| {
                owner.project_id == self.inner.project_id
                    && owner.root == self.inner.root
                    && same_endpoint(&owner.connection, &self.inner.connection)
                    && lock(&owner.unknown).is_some()
            })
            .cloned()
    }
    pub fn request(&self, operation: Operation) -> Result<Reply, String> {
        self.request_typed(operation)
            .map_err(|error| error.to_string())
    }
    pub(crate) fn request_typed(&self, operation: Operation) -> Result<Reply, GitFailure> {
        require_executor()?;
        runtime()?.block_on(self.request_async(operation))
    }
    pub async fn request_async(&self, operation: Operation) -> Result<Reply, GitFailure> {
        self.enqueue(Command::Request(operation, None)).await
    }
    pub(crate) fn request_data<T>(&self, operation: Operation) -> Result<T, GitFailure>
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        require_executor()?;
        runtime()?.block_on(async {
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
    pub async fn recovery_snapshot(&self) -> Result<RemoteGitRecoverySnapshot, GitFailure> {
        let (sender, receiver) = oneshot::channel();
        self.enqueue(Command::Snapshot(sender)).await?;
        receiver
            .await
            .map_err(|_| service_error("remote_git_actor_stopped"))
    }
    pub fn recovery_snapshot_blocking(&self) -> Result<RemoteGitRecoverySnapshot, GitFailure> {
        require_executor()?;
        runtime()?.block_on(self.recovery_snapshot())
    }
    pub async fn acknowledge_recovery(
        &self,
        snapshot: RemoteGitRecoverySnapshot,
    ) -> Result<(), GitFailure> {
        self.enqueue(Command::Acknowledge(snapshot))
            .await
            .map(|_| ())
    }
    pub fn acknowledge_recovery_blocking(
        &self,
        snapshot: &RemoteGitRecoverySnapshot,
    ) -> Result<(), GitFailure> {
        require_executor()?;
        runtime()?.block_on(self.acknowledge_recovery(snapshot.clone()))
    }
    async fn enqueue(&self, command: Command) -> Result<Reply, GitFailure> {
        let permission = self.inner.ensure_active()?;
        validate_absolute_path(&self.inner.root, "root")
            .map_err(|e| failure("remote_git_invalid_root", e))?;
        let recovery = matches!(&command, Command::Snapshot(_) | Command::Acknowledge(_));
        let owner = if recovery {
            self.unknown_owner().unwrap_or_else(|| self.inner.clone())
        } else {
            self.inner.clone()
        };
        let lease = WorkspaceLease {
            inner: self.inner.clone(),
            epoch: owner.epoch.load(Ordering::Acquire),
            owner: owner.clone(),
            authority_epoch: self.inner.epoch.load(Ordering::Acquire),
            permission,
        };
        lease.check()?;
        let (reply, receiver) = oneshot::channel();
        let sender = {
            let mut sender = lock(&owner.sender);
            if sender.is_none() {
                let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
                *sender = Some(tx);
                runtime()?.spawn(actor(Arc::downgrade(&owner), rx));
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
    pub fn close(&self) {
        self.inner.schedule_close();
    }
}

fn runtime() -> Result<Arc<tokio::runtime::Runtime>, GitFailure> {
    state()
        .runtime()
        .map_err(|e| failure("remote_git_runtime_unavailable", e))
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
fn unknown_failure(code: &str) -> GitFailure {
    GitFailure::outcome_unknown(code)
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
        let active = slot.authority.upgrade().map_or_else(
            || workspace.ensure_active().is_ok(),
            |authority| {
                authority
                    .ensure_active()
                    .is_ok_and(|permission| permission == slot.permission)
            },
        );
        if !active {
            if lock(&workspace.unknown).is_none() {
                slot.client.take();
            }
            slot.reader.take();
            slot.snapshot.take();
        }
        drop(workspace);
        let next = tokio::select! {
            request = receiver.recv() => request,
            _ = changed.notified() => {
                if let Some(workspace) = inner.upgrade() {
                    let active = slot.authority.upgrade().map_or_else(|| workspace.ensure_active().is_ok(),
                        |authority| authority.ensure_active().is_ok_and(|permission| permission == slot.permission));
                    if slot.epoch != workspace.epoch.load(Ordering::Acquire) || !active {
                        slot.reader.take(); slot.snapshot.take();
                        if lock(&workspace.unknown).is_none() { slot.client.take(); }
                    }
                }
                continue;
            },
            _ = tokio::time::sleep(timeout) => {
                slot.reader.take();
                if inner.upgrade().is_none_or(|inner| lock(&inner.unknown).is_none()) { slot.client.take(); }
                continue;
            },
        };
        let Some(mut request) = next else {
            break;
        };
        if request.reply.is_closed() {
            continue;
        }
        let Ok(runtime) = runtime() else {
            let _ = request
                .reply
                .send(Err(service_error("remote_git_runtime_unavailable")));
            continue;
        };
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
    let lease = &request.lease;
    if slot.epoch != lease.epoch || slot.permission != lease.permission {
        slot.reader.take();
        slot.snapshot.take();
        if lock(&lease.owner.unknown).is_none() {
            slot.client.take();
        }
        slot.epoch = lease.epoch;
    }
    slot.authority = Arc::downgrade(&lease.inner);
    slot.permission = lease.permission;
    match std::mem::replace(
        &mut request.command,
        Command::Request(Operation::Discover, None),
    ) {
        Command::Snapshot(sender) => {
            let snapshot = snapshot(slot, lease)?;
            let _ = sender.send(snapshot);
            Ok(Reply::Updated)
        }
        Command::Acknowledge(snapshot) => acknowledge(slot, lease, &snapshot),
        Command::Request(operation, decoder) => {
            let write = operation.is_write();
            let repo = write.then(|| operation_repo(&operation)).flatten();
            if write && repo.is_none() {
                return Err(service_error("remote_git_missing_repository"));
            }
            if write
                && lock(&UNKNOWN_WORKSPACES).iter().any(|other| {
                    same_endpoint(&other.connection, &lease.inner.connection)
                        && lock(&other.unknown).as_ref().is_some_and(|unknown| {
                            repo.is_some_and(|repo| {
                                mt_core::path_key::posix_trim_trailing(&unknown.repo)
                                    == mt_core::path_key::posix_trim_trailing(repo)
                            })
                        })
                })
            {
                return Err(unknown_failure("remote_git_write_verification_required"));
            }
            if lock(&lease.inner.unknown).is_some() {
                if write {
                    return Err(unknown_failure("remote_git_write_verification_required"));
                }
                return read(slot, lease, operation, decoder);
            }
            // A read-only liveness round trip on a reused channel precedes a write. Only
            // this probe can be retried; once write data starts, failures are never replayed.
            if write && slot.client.is_some() {
                let repo = operation_repo(&operation)
                    .ok_or_else(|| service_error("remote_git_missing_repository"))?;
                let probe = slot.client.as_mut().expect("present").probe_idle(repo);
                if probe.is_err() {
                    slot.client.take();
                }
            }
            if slot.client.is_none() {
                slot.client = Some((lease.inner.factory)(lease)?);
            }
            // Prepare metadata outside the source lifecycle critical section.
            let identity = write.then(|| RemoteGitUnknownWrite {
                generation: SEQUENCE.fetch_add(1, Ordering::AcqRel),
                repo: repo.expect("write repository checked above").to_owned(),
                operation: format!("{operation:?}"),
                occurred: SystemTime::now(),
            });
            {
                let _source = remote_git_source_change_guard();
                lease.check()?;
                if write {
                    lease.inner.running.store(true, Ordering::Release);
                }
            }
            let client = slot.client.as_mut().expect("initialized above");
            let result = client
                .request(operation)
                .map_err(request_failure)
                .and_then(|reply| decode_reply(reply, decoder, write, client.as_mut()));
            // Publish the uncertain identity before leaving Running, never the reverse.
            if write && matches!(&result, Err(GitFailure::OutcomeUnknown { .. })) {
                let mut identity = identity;
                if let Some(identity) = identity.as_mut() {
                    identity.occurred = SystemTime::now();
                }
                *lock(&lease.inner.unknown) = identity;
                lock(&UNKNOWN_WORKSPACES).push(lease.inner.clone());
                slot.completion_proven = false;
                slot.snapshot = None;
            } else if !client.is_connected() {
                slot.client.take();
            }
            if write {
                lease.inner.running.store(false, Ordering::Release);
            }
            result
        }
    }
}

fn operation_repo(operation: &Operation) -> Option<&str> {
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
fn read(
    slot: &mut WorkspaceClient,
    lease: &WorkspaceLease,
    operation: Operation,
    decoder: Option<Decoder>,
) -> Result<Reply, GitFailure> {
    if slot.reader.is_none() {
        slot.reader = Some((lease.inner.factory)(lease)?);
    }
    lease.check()?;
    let reader = slot.reader.as_mut().expect("initialized above");
    let result = reader
        .request(operation)
        .map_err(request_failure)
        .and_then(|reply| decode_reply(reply, decoder, false, reader.as_mut()));
    // Even a malformed typed payload invalidates a recovery probe channel. Only reads
    // reconnect on the next request; an acknowledgement never reuses failed evidence.
    if result.is_err() || !reader.is_connected() {
        slot.reader.take();
        slot.snapshot.take();
    }
    result
}
fn prove_completion(slot: &mut WorkspaceClient) -> Result<(), GitFailure> {
    if slot.completion_proven {
        return Ok(());
    }
    let old = slot
        .client
        .as_mut()
        .ok_or_else(|| unknown_failure("remote_git_completion_proof_unavailable"))?;
    let result = old.confirm_pending_write();
    if result.is_ok() {
        slot.completion_proven = true;
    }
    if !old.is_connected() {
        slot.client.take();
    }
    result.map_err(|error| {
        GitFailure::outcome_unknown(format_args!(
            "remote_git_completion_proof_unavailable: {error}"
        ))
    })
}
fn query<T: serde::de::DeserializeOwned>(
    slot: &mut WorkspaceClient,
    lease: &WorkspaceLease,
    repo: &str,
    query: GitQuery,
) -> Result<T, GitFailure> {
    let reply = read(
        slot,
        lease,
        Operation::Query {
            repo: repo.to_owned(),
            query,
        },
        None,
    )?;
    reply.decode().map_err(|e| {
        slot.reader.take();
        slot.snapshot.take();
        failure("remote_git_recovery_decode_failed", e)
    })
}
fn snapshot(
    slot: &mut WorkspaceClient,
    lease: &WorkspaceLease,
) -> Result<RemoteGitRecoverySnapshot, GitFailure> {
    let unknown = lock(&lease.owner.unknown)
        .clone()
        .ok_or_else(|| service_error("remote_git_no_unknown_write"))?;
    // A failed proof must not prohibit read-only observation after a hard-lost channel.
    let _ = prove_completion(slot);
    let signature = query(slot, lease, &unknown.repo, GitQuery::RepoSignature)?;
    let op = query(slot, lease, &unknown.repo, GitQuery::RepoOperationState)?;
    let commits: Vec<mt_project::git::GitCommitInfo> = query(
        slot,
        lease,
        &unknown.repo,
        GitQuery::GetGitLog {
            after: None,
            limit: Some(1),
            branch: None,
        },
    )?;
    let head = commits.into_iter().next().map(|commit| commit.hash);
    let files = match read(
        slot,
        lease,
        Operation::Status {
            repo: unknown.repo.clone(),
        },
        None,
    )? {
        Reply::Status { files } => files,
        _ => {
            slot.reader.take();
            slot.snapshot.take();
            return Err(service_error("remote_git_recovery_invalid_status"));
        }
    };
    lease.check()?;
    let snapshot = RemoteGitRecoverySnapshot {
        generation: unknown.generation,
        repo: unknown.repo,
        operation: unknown.operation,
        head,
        signature,
        op,
        files,
        observed: SystemTime::now(),
        completion_proven: slot.completion_proven,
        token: SEQUENCE.fetch_add(1, Ordering::AcqRel),
        epoch: lease.epoch,
        permission: lease.permission,
        occurred: unknown.occurred,
    };
    slot.snapshot = Some(snapshot.clone());
    Ok(snapshot)
}
fn acknowledge(
    slot: &mut WorkspaceClient,
    lease: &WorkspaceLease,
    supplied: &RemoteGitRecoverySnapshot,
) -> Result<Reply, GitFailure> {
    lease.check()?;
    let unknown = lock(&lease.owner.unknown)
        .clone()
        .ok_or_else(|| service_error("remote_git_recovery_stale"))?;
    let current = slot
        .snapshot
        .as_ref()
        .ok_or_else(|| service_error("remote_git_recovery_stale"))?;
    if supplied.token != current.token
        || supplied.generation != unknown.generation
        || supplied.repo != unknown.repo
        || supplied.epoch != lease.epoch
        || supplied.permission != lease.permission
    {
        return Err(service_error("remote_git_recovery_stale"));
    }
    // Compare the internally retained evidence, not caller-editable public fields.
    let current = current.clone();
    let fresh = snapshot(slot, lease)?;
    if current.head != fresh.head
        || current.signature != fresh.signature
        || current.op != fresh.op
        || current.files.len() != fresh.files.len()
        || current.files.iter().zip(&fresh.files).any(|(old, new)| {
            old.path != new.path
                || old.old_path != new.old_path
                || old.staged_status != new.staged_status
                || old.unstaged_status != new.unstaged_status
                || old.status_label != new.status_label
        })
    {
        return Err(service_error("remote_git_recovery_changed"));
    }
    lease.check()?;
    // Never reuse the uncertain writer, even when the user accepts the risk that it
    // could still finish later. No operation is replayed as part of acknowledgement.
    slot.client = slot.reader.take();
    slot.snapshot = None;
    slot.completion_proven = false;
    *lock(&lease.owner.unknown) = None;
    lock(&UNKNOWN_WORKSPACES).retain(|inner| !Arc::ptr_eq(inner, &lease.owner));
    Ok(Reply::Updated)
}

impl WorkspaceInner {
    fn ensure_active(&self) -> Result<u64, GitFailure> {
        if SHUTTING_DOWN.load(Ordering::Acquire) {
            return Err(service_error("remote_git_shutting_down"));
        }
        if self.invalidated.load(Ordering::Acquire) {
            return Err(service_error("remote_git_connection_changed"));
        }
        self.ensure_granted()
    }
    fn ensure_granted(&self) -> Result<u64, GitFailure> {
        lock(&GRANTS)
            .get(&self.project_id)
            .filter(|grant| {
                grant.connection_id == self.connection.id
                    && grant.fingerprint == self.fingerprint
                    && grant.root == self.root
            })
            .map(|grant| grant.generation)
            .ok_or_else(|| service_error("remote_git_disabled"))
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
    lock(&UNKNOWN_WORKSPACES).clear();
}

#[cfg(test)]
#[path = "git_service_tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
pub(crate) fn testing_workspace(
    project_id: String,
    connection: SshConnection,
    root: String,
    factory: Arc<dyn Fn() -> Result<RemoteGitClient, GitFailure> + Send + Sync>,
) -> RemoteGitWorkspace {
    set_project_git_permission(&project_id, Some(&connection), &root, true);
    let inner = Arc::new(WorkspaceInner {
        project_id,
        fingerprint: connection_fingerprint(&connection),
        connection,
        root,
        epoch: AtomicU64::new(0),
        invalidated: AtomicBool::new(false),
        running: AtomicBool::new(false),
        unknown: Mutex::new(None),
        sender: Mutex::new(None),
        changed: Arc::new(Notify::new()),
        factory: Arc::new(move |lease| {
            lease.check()?;
            factory().map(|client| Box::new(client) as Box<dyn WorkspaceTransport>)
        }),
        idle_timeout: IDLE_TIMEOUT,
    });
    lock(&WORKSPACES).push(Arc::downgrade(&inner));
    RemoteGitWorkspace { inner }
}
