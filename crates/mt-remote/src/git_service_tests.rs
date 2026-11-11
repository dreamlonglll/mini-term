use super::*;
use std::collections::VecDeque;
use std::sync::mpsc as sync_mpsc;
use std::time::Instant;

static TEST_LOCK: Mutex<()> = Mutex::new(());
static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn connection() -> SshConnection {
    SshConnection {
        id: "managed-git-unit-test".into(),
        name: "test".into(),
        host: "example.invalid".into(),
        port: 22,
        user: "tester".into(),
        password: None,
        identity_file: None,
        group: None,
        extra: Default::default(),
    }
}

struct FakeState {
    requests: Mutex<Vec<String>>,
    results: Mutex<VecDeque<Result<Reply, GitFailure>>>,
    connects: AtomicU64,
    drops: AtomicU64,
    proofs: AtomicU64,
    decode_unknown: AtomicU64,
    proof_ok: AtomicBool,
    entered: Mutex<Option<sync_mpsc::Sender<()>>>,
    release: Mutex<Option<sync_mpsc::Receiver<()>>>,
}

impl Default for FakeState {
    fn default() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            results: Mutex::new(VecDeque::new()),
            connects: AtomicU64::new(0),
            drops: AtomicU64::new(0),
            proofs: AtomicU64::new(0),
            decode_unknown: AtomicU64::new(0),
            proof_ok: AtomicBool::new(true),
            entered: Mutex::new(None),
            release: Mutex::new(None),
        }
    }
}

struct FakeTransport(Arc<FakeState>);
impl Drop for FakeTransport {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl WorkspaceTransport for FakeTransport {
    fn request(&mut self, operation: Operation) -> Result<Reply, GitFailure> {
        lock(&self.0.requests).push(format!("{operation:?}"));
        if let Some(entered) = lock(&self.0.entered).take() {
            entered.send(()).unwrap();
        }
        if let Some(release) = lock(&self.0.release).take() {
            release.recv().unwrap();
        }
        lock(&self.0.results)
            .pop_front()
            .unwrap_or(Ok(Reply::Updated))
    }
    fn confirm_pending_write(&mut self) -> Result<(), GitFailure> {
        self.0.proofs.fetch_add(1, Ordering::SeqCst);
        if self.0.proof_ok.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(unknown_failure("test_old_channel_lost"))
        }
    }
    fn mark_last_write_unknown(&mut self) {
        self.0.decode_unknown.fetch_add(1, Ordering::SeqCst);
    }
    fn is_connected(&self) -> bool {
        true
    }
}

fn fixture(idle_timeout: Duration) -> (RemoteGitWorkspace, Arc<FakeState>) {
    REMOTE_GIT_ENABLED.store(true, Ordering::Release);
    let state = Arc::new(FakeState::default());
    let factory_state = state.clone();
    let inner = Arc::new(WorkspaceInner {
        connection: connection(),
        fingerprint: 1,
        root: format!(
            "/actor-test-{}",
            TEST_SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ),
        epoch: AtomicU64::new(0),
        invalidated: AtomicBool::new(false),
        outcome_unknown: AtomicBool::new(false),
        sender: Mutex::new(None),
        changed: Arc::new(Notify::new()),
        factory: Arc::new(move |lease| {
            lease.check()?;
            factory_state.connects.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeTransport(factory_state.clone())))
        }),
        idle_timeout,
    });
    lock(&WORKSPACES).push(Arc::downgrade(&inner));
    (RemoteGitWorkspace { inner }, state)
}

fn write() -> Operation {
    Operation::StageAll {
        repo: "/repo".into(),
    }
}
fn assert_code(error: GitFailure, expected: &str) {
    assert!(matches!(error, GitFailure::Service { code, .. } if code == expected));
}
fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !predicate() {
        assert!(Instant::now() < deadline, "actor event did not occur");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn block_transport(state: &FakeState) -> (sync_mpsc::Receiver<()>, sync_mpsc::Sender<()>) {
    let (entered_tx, entered_rx) = sync_mpsc::channel();
    let (release_tx, release_rx) = sync_mpsc::channel();
    *lock(&state.entered) = Some(entered_tx);
    *lock(&state.release) = Some(release_rx);
    (entered_rx, release_tx)
}

#[test]
fn repository_identity_shares_actor_and_preserves_posix_case() {
    let _serial = lock(&TEST_LOCK);
    let base = RemoteGitWorkspace::new(connection(), "/Project".into());
    let same = RemoteGitWorkspace::new(connection(), "/Project".into());
    assert!(Arc::ptr_eq(&base.inner, &same.inner));
    assert_eq!(
        base.repository_key("/Project/Repo"),
        same.repository_key("/Project/Repo")
    );
    assert_ne!(
        base.repository_key("/Project/Repo"),
        base.repository_key("/Project/repo")
    );
    let root = RemoteGitWorkspace::new(connection(), "/project".into());
    assert_ne!(
        base.repository_key("/Project/Repo"),
        root.repository_key("/Project/Repo")
    );
    let mut changed = connection();
    changed.host = "other.invalid".into();
    let endpoint = RemoteGitWorkspace::new(changed, "/Project".into());
    assert_ne!(
        base.repository_key("/Project/Repo"),
        endpoint.repository_key("/Project/Repo")
    );
    let mut a = connection();
    a.id = "id|x".into();
    let mut b = connection();
    b.id = "id".into();
    assert_ne!(
        RemoteGitWorkspace::new(a, "/r".into()).repository_key("/r"),
        RemoteGitWorkspace::new(b, "x|/r".into()).repository_key("/r")
    );
}

#[test]
fn unknown_write_is_not_cleared_by_automatic_reads_close_or_new_handles() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_millis(10));
    lock(&transport.results).push_back(Err(unknown_failure("test_write_lost")));
    assert!(matches!(
        runtime.block_on(workspace.request_async(write())),
        Err(GitFailure::OutcomeUnknown { .. })
    ));
    std::thread::sleep(Duration::from_millis(35));
    assert_eq!(
        transport.drops.load(Ordering::SeqCst),
        0,
        "idle must preserve the old completion-proof channel"
    );
    workspace.close();
    assert!(
        runtime
            .block_on(workspace.request_async(Operation::Discover))
            .is_ok()
    );
    assert!(matches!(
        runtime.block_on(workspace.request_async(write())),
        Err(GitFailure::OutcomeUnknown { .. })
    ));
    let inner = Arc::downgrade(&workspace.inner);
    drop(workspace);
    assert!(
        inner.upgrade().is_some(),
        "uncertain workspace identity must survive pane disposal"
    );
    let workspace = RemoteGitWorkspace {
        inner: inner.upgrade().unwrap(),
    };
    assert!(matches!(
        runtime.block_on(workspace.request_async(write())),
        Err(GitFailure::OutcomeUnknown { .. })
    ));
    assert_eq!(
        transport.proofs.load(Ordering::SeqCst),
        0,
        "ordinary reads are not recovery"
    );
    workspace.inner.invalidated.store(true, Ordering::Release);
    workspace.close();
}

#[test]
fn dispatched_write_decode_failure_is_latched_inside_actor() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    lock(&transport.results).push_back(Ok(Reply::Data {
        value: serde_json::value::RawValue::from_string("\"not a number\"".into()).unwrap(),
    }));
    let op_workspace = workspace.clone();
    let result = runtime.block_on(run_git(move || op_workspace.request_data::<u64>(write())));
    assert!(matches!(result, Err(GitFailure::OutcomeUnknown { .. })));
    assert_eq!(transport.decode_unknown.load(Ordering::SeqCst), 1);
    assert!(matches!(
        runtime.block_on(workspace.request_async(write())),
        Err(GitFailure::OutcomeUnknown { .. })
    ));
    assert_eq!(
        lock(&transport.requests).len(),
        1,
        "write must never replay"
    );
    workspace.inner.invalidated.store(true, Ordering::Release);
    workspace.close();
}

#[test]
fn explicit_recovery_requires_old_proof_then_new_head_index_status() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    lock(&transport.results).push_back(Err(unknown_failure("test_write_lost")));
    let _ = runtime.block_on(workspace.request_async(write()));
    transport.proof_ok.store(false, Ordering::Release);
    assert!(
        runtime
            .block_on(workspace.verify_unknown_write("/repo".into()))
            .is_err()
    );
    assert_eq!(
        transport.connects.load(Ordering::SeqCst),
        1,
        "new reads cannot replace old completion proof"
    );
    transport.proof_ok.store(true, Ordering::Release);
    lock(&transport.results).extend([
        Ok(Reply::Data {
            value: serde_json::value::RawValue::from_string("{\"refs\":1,\"index\":2}".into())
                .unwrap(),
        }),
        Ok(Reply::Status { files: Vec::new() }),
    ]);
    runtime
        .block_on(workspace.verify_unknown_write("/repo".into()))
        .unwrap();
    assert_eq!(transport.connects.load(Ordering::SeqCst), 2);
    runtime.block_on(workspace.request_async(write())).unwrap();
    let requests = lock(&transport.requests);
    assert!(requests[1].contains("RepoSignature"));
    assert!(requests[2].contains("Status"));
}

#[test]
fn close_and_idle_never_drop_inflight_transport_and_cancel_old_queue() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_millis(15));
    let (entered, release) = block_transport(&transport);
    let first = workspace.clone();
    let task = runtime.spawn(async move { first.request_async(Operation::Discover).await });
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    let queued = workspace.clone();
    let queued_task = runtime.spawn(async move { queued.request_async(write()).await });
    wait_until(|| lock(&workspace.inner.sender).as_ref().unwrap().capacity() < QUEUE_CAPACITY);
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(transport.drops.load(Ordering::SeqCst), 0);
    let start = Instant::now();
    workspace.close();
    assert!(start.elapsed() < Duration::from_millis(100));
    assert_eq!(transport.drops.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    runtime.block_on(task).unwrap().unwrap();
    assert_code(
        runtime.block_on(queued_task).unwrap().unwrap_err(),
        "remote_git_request_cancelled",
    );
    assert_eq!(lock(&transport.requests).len(), 1);
    wait_until(|| transport.drops.load(Ordering::SeqCst) == 1);
    runtime
        .block_on(workspace.request_async(Operation::Discover))
        .unwrap();
    wait_until(|| transport.drops.load(Ordering::SeqCst) == 2);
}

#[test]
fn bounded_queue_and_cancelled_waiters_never_dispatch() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    let (entered, release) = block_transport(&transport);
    let first = workspace.clone();
    let task = runtime.spawn(async move { first.request_async(Operation::Discover).await });
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    let mut queued = Vec::new();
    for _ in 0..QUEUE_CAPACITY {
        let workspace = workspace.clone();
        queued.push(runtime.spawn(async move { workspace.request_async(write()).await }));
    }
    wait_until(|| lock(&workspace.inner.sender).as_ref().unwrap().capacity() == 0);
    assert_code(
        runtime
            .block_on(workspace.request_async(write()))
            .unwrap_err(),
        "remote_git_queue_full",
    );
    for task in &queued {
        task.abort();
    }
    for task in queued {
        let _ = runtime.block_on(task);
    }
    release.send(()).unwrap();
    runtime.block_on(task).unwrap().unwrap();
    wait_until(|| lock(&workspace.inner.sender).as_ref().unwrap().capacity() == QUEUE_CAPACITY);
    assert_eq!(lock(&transport.requests).len(), 1);
}

#[test]
fn dynamic_permission_revokes_queued_identity_without_claiming_write_cancellation() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    let (entered, release) = block_transport(&transport);
    let first = workspace.clone();
    let task = runtime.spawn(async move { first.request_async(write()).await });
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    set_remote_git_enabled(false);
    assert_code(
        runtime
            .block_on(workspace.request_async(Operation::Discover))
            .unwrap_err(),
        "remote_git_disabled",
    );
    assert_eq!(transport.drops.load(Ordering::SeqCst), 0);
    set_remote_git_enabled(true);
    release.send(()).unwrap();
    runtime.block_on(task).unwrap().unwrap();
    assert_eq!(
        lock(&transport.requests).len(),
        1,
        "already dispatched write finished once"
    );
}

#[test]
fn stale_connection_and_outside_executor_fail_before_transport() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    assert_code(
        workspace.request_typed(Operation::Discover).unwrap_err(),
        "remote_git_executor_required",
    );
    workspace.inner.invalidated.store(true, Ordering::Release);
    assert_code(
        runtime
            .block_on(workspace.request_async(Operation::Discover))
            .unwrap_err(),
        "remote_git_connection_changed",
    );
    assert_eq!(transport.connects.load(Ordering::SeqCst), 0);
}

#[test]
fn invalidation_blocks_old_queue_but_preserves_dispatched_repository_result() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    let (entered, release) = block_transport(&transport);
    let first = workspace.clone();
    let first_task = runtime.spawn(async move { first.request_async(write()).await });
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    let queued = workspace.clone();
    let queued_task = runtime.spawn(async move { queued.request_async(write()).await });
    wait_until(|| lock(&workspace.inner.sender).as_ref().unwrap().capacity() < QUEUE_CAPACITY);
    workspace.inner.invalidated.store(true, Ordering::Release);
    workspace.close();
    release.send(()).unwrap();
    runtime.block_on(first_task).unwrap().unwrap();
    assert_code(
        runtime.block_on(queued_task).unwrap().unwrap_err(),
        "remote_git_connection_changed",
    );
    assert_eq!(lock(&transport.requests).len(), 1);
    assert!(!workspace.inner.outcome_unknown.load(Ordering::Acquire));
}

#[test]
fn cancelling_run_git_future_keeps_blocking_executor_admission_until_completion() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let before = EXECUTOR_SLOTS.available_permits();
    let (entered_tx, entered_rx) = sync_mpsc::channel();
    let (release_tx, release_rx) = sync_mpsc::channel();
    let task = runtime.spawn(run_git(move || {
        assert!(require_executor().is_ok());
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    }));
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    task.abort();
    let _ = runtime.block_on(task);
    assert_eq!(EXECUTOR_SLOTS.available_permits(), before - 1);
    release_tx.send(()).unwrap();
    wait_until(|| EXECUTOR_SLOTS.available_permits() == before);
}

#[test]
fn unchanged_permission_is_idempotent_and_disabled_requests_do_not_deploy() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    set_remote_git_enabled(false);
    let epoch = PERMISSION_EPOCH.load(Ordering::Acquire);
    let workspace_epoch = workspace.inner.epoch.load(Ordering::Acquire);
    set_remote_git_enabled(false);
    assert_eq!(PERMISSION_EPOCH.load(Ordering::Acquire), epoch);
    assert_eq!(
        workspace.inner.epoch.load(Ordering::Acquire),
        workspace_epoch
    );
    assert_code(
        runtime
            .block_on(workspace.request_async(write()))
            .unwrap_err(),
        "remote_git_disabled",
    );
    assert_eq!(transport.connects.load(Ordering::SeqCst), 0);
    set_remote_git_enabled(true);
    let epoch = PERMISSION_EPOCH.load(Ordering::Acquire);
    set_remote_git_enabled(true);
    assert_eq!(PERMISSION_EPOCH.load(Ordering::Acquire), epoch);
}

#[test]
fn failed_new_channel_verification_keeps_unknown_and_reuses_only_proven_completion() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    lock(&transport.results).extend([
        Err(unknown_failure("test_write_lost")),
        Ok(Reply::Data {
            value: serde_json::value::RawValue::from_string("null".into()).unwrap(),
        }),
    ]);
    let _ = runtime.block_on(workspace.request_async(write()));
    assert_code(
        runtime
            .block_on(workspace.verify_unknown_write("/repo".into()))
            .unwrap_err(),
        "remote_git_recovery_decode_failed",
    );
    assert!(matches!(
        runtime.block_on(workspace.request_async(write())),
        Err(GitFailure::OutcomeUnknown { .. })
    ));
    lock(&transport.results).extend([
        Ok(Reply::Data {
            value: serde_json::value::RawValue::from_string("{\"refs\":1,\"index\":2}".into())
                .unwrap(),
        }),
        Ok(Reply::Status { files: Vec::new() }),
    ]);
    runtime
        .block_on(workspace.verify_unknown_write("/repo".into()))
        .unwrap();
    assert_eq!(
        transport.proofs.load(Ordering::SeqCst),
        1,
        "completed receipt remains valid across failed reads"
    );
    runtime.block_on(workspace.request_async(write())).unwrap();
}
