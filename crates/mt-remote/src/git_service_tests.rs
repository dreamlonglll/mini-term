use super::*;
use std::collections::VecDeque;
use std::sync::mpsc as sync_mpsc;
use std::time::Instant;

pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());
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
#[derive(Default)]
struct FakeState {
    requests: Mutex<Vec<String>>,
    results: Mutex<VecDeque<Result<Reply, GitFailure>>>,
    connects: AtomicU64,
    drops: AtomicU64,
    proofs: AtomicU64,
    decode_unknown: AtomicU64,
    proof_lost: AtomicBool,
    entered: Mutex<Option<sync_mpsc::Sender<()>>>,
    release: Mutex<Option<sync_mpsc::Receiver<()>>>,
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
        if let Some(result) = lock(&self.0.results).pop_front() {
            return result;
        }
        Ok(match operation {
            Operation::Query {
                query: GitQuery::RepoSignature,
                ..
            } => data(RepoSignature { refs: 1, index: 2 }),
            Operation::Query {
                query: GitQuery::RepoOperationState,
                ..
            } => data(RepoOpState::default()),
            Operation::Query {
                query: GitQuery::GetGitLog { .. },
                ..
            } => data(Vec::<mt_project::git::GitCommitInfo>::new()),
            Operation::Status { .. } => Reply::Status { files: Vec::new() },
            _ => Reply::Updated,
        })
    }
    fn confirm_pending_write(&mut self) -> Result<(), GitFailure> {
        self.0.proofs.fetch_add(1, Ordering::SeqCst);
        if self.0.proof_lost.load(Ordering::Acquire) {
            Err(unknown_failure("test_old_channel_lost"))
        } else {
            Ok(())
        }
    }
    fn mark_last_write_unknown(&mut self) {
        self.0.decode_unknown.fetch_add(1, Ordering::SeqCst);
    }
    fn is_connected(&self) -> bool {
        true
    }
}
fn data(value: impl serde::Serialize) -> Reply {
    Reply::Data {
        value: serde_json::value::to_raw_value(&value).unwrap(),
    }
}
fn fixture(idle_timeout: Duration) -> (RemoteGitWorkspace, Arc<FakeState>) {
    let transport = Arc::new(FakeState::default());
    let factory_state = transport.clone();
    let project_id = format!(
        "actor-project-{}",
        TEST_SEQUENCE.fetch_add(1, Ordering::SeqCst)
    );
    let connection = connection();
    let root = format!("/{project_id}");
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
            factory_state.connects.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeTransport(factory_state.clone())))
        }),
        idle_timeout,
    });
    lock(&WORKSPACES).push(Arc::downgrade(&inner));
    (RemoteGitWorkspace { inner }, transport)
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
fn lose_write(workspace: &RemoteGitWorkspace, transport: &FakeState) {
    if !lock(&transport.requests).is_empty() {
        lock(&transport.results).push_back(Ok(data(RepoSignature { refs: 1, index: 2 })));
    }
    lock(&transport.results).push_back(Err(unknown_failure("test_write_lost")));
    assert!(matches!(
        state()
            .runtime()
            .unwrap()
            .block_on(workspace.request_async(write())),
        Err(GitFailure::OutcomeUnknown { .. })
    ));
}
fn recover(workspace: &RemoteGitWorkspace) {
    let runtime = state().runtime().unwrap();
    let snapshot = runtime.block_on(workspace.recovery_snapshot()).unwrap();
    runtime
        .block_on(workspace.acknowledge_recovery(snapshot))
        .unwrap();
}

#[test]
fn repository_identity_shares_gate_but_not_project_permission() {
    let _serial = lock(&TEST_LOCK);
    let a = RemoteGitWorkspace::new("identity-a".into(), connection(), "/Project".into());
    let same = RemoteGitWorkspace::new("identity-a".into(), connection(), "/Project".into());
    let other = RemoteGitWorkspace::new("identity-b".into(), connection(), "/Project".into());
    assert!(Arc::ptr_eq(&a.inner, &same.inner));
    assert!(!Arc::ptr_eq(&a.inner, &other.inner));
    assert_eq!(a.repository_key("/repo"), other.repository_key("/repo"));
    assert_ne!(a.repository_key("/Repo"), a.repository_key("/repo"));
    let root = RemoteGitWorkspace::new("identity-a".into(), connection(), "/other".into());
    assert_eq!(a.repository_key("/repo"), root.repository_key("/repo"));
    let mut endpoint = connection();
    endpoint.host = "other.invalid".into();
    assert_ne!(
        a.repository_key("/repo"),
        RemoteGitWorkspace::new("identity-a".into(), endpoint, "/Project".into())
            .repository_key("/repo")
    );
}

#[test]
fn unknown_survives_reads_close_disposal_and_reuses_one_reader() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    lose_write(&workspace, &transport);
    workspace.close();
    runtime
        .block_on(workspace.request_async(Operation::Discover))
        .unwrap();
    runtime
        .block_on(workspace.request_async(Operation::Discover))
        .unwrap();
    assert_eq!(transport.connects.load(Ordering::SeqCst), 2);
    assert_eq!(transport.proofs.load(Ordering::SeqCst), 0);
    assert!(workspace.write_outcome_unknown());
    assert!(matches!(
        runtime.block_on(workspace.request_async(write())),
        Err(GitFailure::OutcomeUnknown { .. })
    ));
    let inner = Arc::downgrade(&workspace.inner);
    drop(workspace);
    let workspace = RemoteGitWorkspace {
        inner: inner
            .upgrade()
            .expect("explicit unknown registry retains original identity"),
    };
    recover(&workspace);
    assert!(!workspace.write_outcome_unknown());
    drop(workspace);
    wait_until(|| inner.upgrade().is_none());
}

#[test]
fn running_is_not_unknown_and_known_results_remain_known() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    let (entered, release) = block_transport(&transport);
    let op_workspace = workspace.clone();
    let task = runtime.spawn(async move { op_workspace.request_async(write()).await });
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(workspace.write_running());
    assert!(!workspace.write_outcome_unknown());
    release.send(()).unwrap();
    runtime.block_on(task).unwrap().unwrap();
    assert!(!workspace.write_running());
    lock(&transport.results).extend([
        Ok(data(RepoSignature { refs: 1, index: 2 })),
        Err("known operation rejection".into()),
    ]);
    assert_code(
        runtime
            .block_on(workspace.request_async(write()))
            .unwrap_err(),
        "remote_git_request_failed",
    );
    assert!(!workspace.write_outcome_unknown());
}

#[test]
fn dispatched_decode_failure_is_unknown_and_never_replayed() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    lock(&transport.results).push_back(Ok(data("not a number")));
    let op_workspace = workspace.clone();
    let context = GitContext::remote(
        workspace.inner.project_id.clone(),
        connection(),
        workspace.inner.root.clone(),
    );
    assert!(matches!(
        runtime.block_on(run_git_on(&context, move || op_workspace.request_data::<u64>(write()))),
        Err(GitFailure::OutcomeUnknown { .. })
    ));
    assert_eq!(transport.decode_unknown.load(Ordering::SeqCst), 1);
    assert!(matches!(
        runtime.block_on(workspace.request_async(write())),
        Err(GitFailure::OutcomeUnknown { .. })
    ));
    assert_eq!(lock(&transport.requests).len(), 1);
    recover(&workspace);
}

#[test]
fn hard_lost_recovery_requires_original_repo_snapshot_and_explicit_ack() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    transport.proof_lost.store(true, Ordering::Release);
    lose_write(&workspace, &transport);
    let snapshot = runtime.block_on(workspace.recovery_snapshot()).unwrap();
    assert!(!snapshot.completion_proven);
    assert_eq!(snapshot.repo, "/repo");
    assert!(snapshot.operation.contains("StageAll"));
    assert!(workspace.write_outcome_unknown());
    assert!(
        lock(&transport.requests)
            .iter()
            .skip(1)
            .all(|request| request.contains("/repo"))
    );
    runtime
        .block_on(workspace.request_async(Operation::Status {
            repo: "/other-repo".into(),
        }))
        .unwrap();
    assert_eq!(workspace.unknown_write().unwrap().repo, "/repo");
    runtime
        .block_on(workspace.acknowledge_recovery(snapshot.clone()))
        .unwrap();
    assert!(!workspace.write_outcome_unknown());
    assert_eq!(transport.connects.load(Ordering::SeqCst), 2);
    assert_code(
        runtime
            .block_on(workspace.acknowledge_recovery(snapshot))
            .unwrap_err(),
        "remote_git_recovery_stale",
    );
    runtime.block_on(workspace.request_async(write())).unwrap();
    assert_eq!(
        lock(&transport.requests)
            .iter()
            .filter(|request| request.contains("StageAll"))
            .count(),
        2
    );
}

#[test]
fn recovery_rejects_changed_status_signature_generation_and_permission() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    lose_write(&workspace, &transport);
    let old = runtime.block_on(workspace.recovery_snapshot()).unwrap();
    lock(&transport.results).extend([
        Ok(data(RepoSignature { refs: 1, index: 2 })),
        Ok(data(RepoOpState::default())),
        Ok(data(Vec::<mt_project::git::GitCommitInfo>::new())),
        Ok(Reply::Status {
            files: vec![ChangeFileStatus {
                path: "changed".into(),
                old_path: None,
                staged_status: None,
                unstaged_status: None,
                status_label: "M".into(),
            }],
        }),
    ]);
    assert_code(
        runtime
            .block_on(workspace.acknowledge_recovery(old.clone()))
            .unwrap_err(),
        "remote_git_recovery_changed",
    );
    assert!(workspace.write_outcome_unknown());
    assert_code(
        runtime
            .block_on(workspace.acknowledge_recovery(old))
            .unwrap_err(),
        "remote_git_recovery_stale",
    );
    let old = runtime.block_on(workspace.recovery_snapshot()).unwrap();
    lock(&transport.results).push_back(Ok(data(RepoSignature { refs: 9, index: 2 })));
    assert_code(
        runtime
            .block_on(workspace.acknowledge_recovery(old))
            .unwrap_err(),
        "remote_git_recovery_changed",
    );
    let before_revoke = runtime.block_on(workspace.recovery_snapshot()).unwrap();
    set_project_git_permission(&workspace.inner.project_id, None, "ignored", false);
    set_project_git_permission(
        &workspace.inner.project_id,
        Some(&workspace.inner.connection),
        &workspace.inner.root,
        true,
    );
    assert_code(
        runtime
            .block_on(workspace.acknowledge_recovery(before_revoke.clone()))
            .unwrap_err(),
        "remote_git_recovery_stale",
    );
    recover(&workspace);
    lose_write(&workspace, &transport);
    assert_ne!(
        workspace.unknown_write().unwrap().generation,
        before_revoke.generation
    );
    assert_code(
        runtime
            .block_on(workspace.acknowledge_recovery(before_revoke))
            .unwrap_err(),
        "remote_git_recovery_stale",
    );
    recover(&workspace);
}

#[test]
fn failed_probe_invalidates_reader_but_retains_barrier_and_unknown() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    lose_write(&workspace, &transport);
    lock(&transport.results).push_back(Ok(data(serde_json::Value::Null)));
    assert_code(
        runtime.block_on(workspace.recovery_snapshot()).unwrap_err(),
        "remote_git_recovery_decode_failed",
    );
    assert!(workspace.write_outcome_unknown());
    let snapshot = runtime.block_on(workspace.recovery_snapshot()).unwrap();
    assert!(snapshot.completion_proven);
    assert_eq!(transport.proofs.load(Ordering::SeqCst), 1);
    assert_eq!(transport.connects.load(Ordering::SeqCst), 3);
    runtime
        .block_on(workspace.acknowledge_recovery(snapshot))
        .unwrap();
    assert_eq!(transport.connects.load(Ordering::SeqCst), 3);
}

#[test]
fn failed_idle_read_probe_reconnects_before_sending_write_once() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    runtime
        .block_on(workspace.request_async(Operation::Discover))
        .unwrap();
    lock(&transport.results).push_back(Err("dead idle channel".into()));
    runtime.block_on(workspace.request_async(write())).unwrap();
    assert_eq!(transport.connects.load(Ordering::SeqCst), 2);
    let requests = lock(&transport.requests);
    assert!(requests[1].contains("RepoSignature"));
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.contains("StageAll"))
            .count(),
        1
    );
    assert!(!workspace.write_outcome_unknown());
}

#[test]
fn close_and_idle_preserve_inflight_transport_and_cancel_old_queue() {
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
    workspace.close();
    assert_eq!(transport.drops.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    runtime.block_on(task).unwrap().unwrap();
    assert_code(
        runtime.block_on(queued_task).unwrap().unwrap_err(),
        "remote_git_request_cancelled",
    );
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
fn scoped_permission_is_default_denied_idempotent_and_does_not_cancel_dispatched_write() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (a, transport) = fixture(Duration::from_secs(60));
    let b = RemoteGitWorkspace::new(
        "ungranted-project".into(),
        a.inner.connection.clone(),
        a.inner.root.clone(),
    );
    assert_code(
        runtime
            .block_on(b.request_async(Operation::Discover))
            .unwrap_err(),
        "remote_git_disabled",
    );
    let epoch = a.inner.epoch.load(Ordering::Acquire);
    set_project_git_permission(
        &a.inner.project_id,
        Some(&a.inner.connection),
        &a.inner.root,
        true,
    );
    assert_eq!(a.inner.epoch.load(Ordering::Acquire), epoch);
    let grant = a.inner.ensure_active().unwrap();
    set_project_git_permission("unrelated", Some(&a.inner.connection), &a.inner.root, true);
    set_project_git_permission("unrelated", None, "", false);
    assert_eq!(a.inner.ensure_active().unwrap(), grant);
    let (entered, release) = block_transport(&transport);
    let first = a.clone();
    let task = runtime.spawn(async move { first.request_async(write()).await });
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    set_project_git_permission(&a.inner.project_id, None, "arbitrary", false);
    assert_code(
        runtime
            .block_on(a.request_async(Operation::Discover))
            .unwrap_err(),
        "remote_git_disabled",
    );
    assert_eq!(transport.drops.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    runtime.block_on(task).unwrap().unwrap();
    assert_eq!(lock(&transport.requests).len(), 1);
    assert!(!a.write_outcome_unknown());
}

#[test]
fn stale_connection_and_executor_boundary_fail_before_transport() {
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
fn invalidation_rejects_queue_but_returns_dispatched_repository_result() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    let (entered, release) = block_transport(&transport);
    let first = workspace.clone();
    let task = runtime.spawn(async move { first.request_async(write()).await });
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    let queued = workspace.clone();
    let queued_task = runtime.spawn(async move { queued.request_async(write()).await });
    wait_until(|| lock(&workspace.inner.sender).as_ref().unwrap().capacity() < QUEUE_CAPACITY);
    workspace.inner.invalidated.store(true, Ordering::Release);
    workspace.close();
    release.send(()).unwrap();
    runtime.block_on(task).unwrap().unwrap();
    assert_code(
        runtime.block_on(queued_task).unwrap().unwrap_err(),
        "remote_git_connection_changed",
    );
    assert_eq!(lock(&transport.requests).len(), 1);
}

#[test]
fn all_remote_slots_cannot_starve_local_and_cancelled_closures_keep_admission() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let remote = GitContext::remote("quota".into(), connection(), "/quota".into());
    let mut tasks = Vec::new();
    let mut releases = Vec::new();
    for _ in 0..8 {
        let (entered_tx, entered_rx) = sync_mpsc::channel();
        let (release_tx, release_rx) = sync_mpsc::channel();
        let context = remote.clone();
        tasks.push(runtime.spawn(async move {
            run_git_on(&context, move || {
                assert!(require_executor().is_ok());
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .await
        }));
        releases.push(release_tx);
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    }
    assert_eq!(REMOTE_EXECUTOR_SLOTS.available_permits(), 0);
    let (local_tx, local_rx) = sync_mpsc::channel();
    let local = GitContext::local("/local");
    let local_task =
        runtime.spawn(async move { run_git_on(&local, move || local_tx.send(()).unwrap()).await });
    local_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    runtime.block_on(local_task).unwrap();
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        let _ = runtime.block_on(task);
    }
    assert_eq!(REMOTE_EXECUTOR_SLOTS.available_permits(), 0);
    for release in releases {
        release.send(()).unwrap();
    }
    wait_until(|| REMOTE_EXECUTOR_SLOTS.available_permits() == 8);
}

#[test]
fn unknown_blocks_other_granted_project_without_granting_recovery_authority() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (origin, transport) = fixture(Duration::from_secs(60));
    let other = RemoteGitWorkspace::new(
        "other-granted-project".into(),
        origin.inner.connection.clone(),
        origin.inner.root.clone(),
    );
    set_project_git_permission(
        &other.inner.project_id,
        Some(&other.inner.connection),
        &other.inner.root,
        true,
    );
    lose_write(&origin, &transport);
    assert!(matches!(
        runtime.block_on(other.request_async(write())),
        Err(GitFailure::OutcomeUnknown { .. })
    ));
    assert_code(
        runtime.block_on(other.recovery_snapshot()).unwrap_err(),
        "remote_git_no_unknown_write",
    );
    assert_eq!(lock(&transport.requests).len(), 1);
    set_project_git_permission(&other.inner.project_id, None, "", false);
    assert!(origin.inner.ensure_active().is_ok());
    recover(&origin);
}

#[test]
fn idle_keeps_old_proof_channel_and_ack_failure_invalidates_confirmation() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_millis(10));
    lose_write(&workspace, &transport);
    std::thread::sleep(Duration::from_millis(35));
    assert_eq!(transport.drops.load(Ordering::SeqCst), 0);
    let snapshot = runtime.block_on(workspace.recovery_snapshot()).unwrap();
    lock(&transport.results).push_back(Err("read channel failed".into()));
    assert!(
        runtime
            .block_on(workspace.acknowledge_recovery(snapshot.clone()))
            .is_err()
    );
    assert!(workspace.write_outcome_unknown());
    assert_code(
        runtime
            .block_on(workspace.acknowledge_recovery(snapshot))
            .unwrap_err(),
        "remote_git_recovery_stale",
    );
    recover(&workspace);
}

#[test]
fn endpoint_replacement_revokes_only_own_project_and_old_ack_is_invalid() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    let (unrelated, _) = fixture(Duration::from_secs(60));
    let unrelated_grant = unrelated.inner.ensure_active().unwrap();
    lose_write(&workspace, &transport);
    let snapshot = runtime.block_on(workspace.recovery_snapshot()).unwrap();
    let mut endpoint = workspace.inner.connection.clone();
    endpoint.host = "changed.invalid".into();
    set_project_git_permission(
        &workspace.inner.project_id,
        Some(&endpoint),
        &workspace.inner.root,
        true,
    );
    assert_code(
        runtime
            .block_on(workspace.acknowledge_recovery(snapshot.clone()))
            .unwrap_err(),
        "remote_git_disabled",
    );
    assert_eq!(unrelated.inner.ensure_active().unwrap(), unrelated_grant);
    set_project_git_permission(
        &workspace.inner.project_id,
        Some(&workspace.inner.connection),
        &workspace.inner.root,
        true,
    );
    assert_code(
        runtime
            .block_on(workspace.acknowledge_recovery(snapshot))
            .unwrap_err(),
        "remote_git_recovery_stale",
    );
    recover(&workspace);
}

fn credential_successor(
    origin: &RemoteGitWorkspace,
    connection: SshConnection,
) -> RemoteGitWorkspace {
    let inner = Arc::new(WorkspaceInner {
        project_id: origin.inner.project_id.clone(),
        fingerprint: connection_fingerprint(&connection),
        connection,
        root: origin.inner.root.clone(),
        epoch: AtomicU64::new(0),
        invalidated: AtomicBool::new(false),
        running: AtomicBool::new(false),
        unknown: Mutex::new(None),
        sender: Mutex::new(None),
        changed: Arc::new(Notify::new()),
        factory: origin.inner.factory.clone(),
        idle_timeout: origin.inner.idle_timeout,
    });
    lock(&WORKSPACES).push(Arc::downgrade(&inner));
    RemoteGitWorkspace { inner }
}

#[test]
fn credential_rotation_preserves_original_unknown_until_new_grant_snapshot_ack() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (origin, transport) = fixture(Duration::from_secs(60));
    transport.proof_lost.store(true, Ordering::Release);
    lose_write(&origin, &transport);
    let unknown = origin.unknown_write().unwrap();
    let old_snapshot = runtime.block_on(origin.recovery_snapshot()).unwrap();
    let mut connection = origin.inner.connection.clone();
    connection.password = Some("rotated-secret".into());
    connection.identity_file = Some("/rotated/key".into());
    let successor = credential_successor(&origin, connection.clone());
    assert_eq!(successor.unknown_write(), Some(unknown.clone()));
    assert_eq!(
        successor.repository_key("/repo"),
        origin.repository_key("/repo")
    );
    assert_code(
        runtime.block_on(successor.recovery_snapshot()).unwrap_err(),
        "remote_git_disabled",
    );
    set_project_git_permission(
        &origin.inner.project_id,
        Some(&connection),
        &origin.inner.root,
        true,
    );
    assert!(matches!(
        runtime.block_on(successor.request_async(write())),
        Err(GitFailure::OutcomeUnknown { .. })
    ));
    assert_code(
        runtime
            .block_on(successor.acknowledge_recovery(old_snapshot))
            .unwrap_err(),
        "remote_git_recovery_stale",
    );
    let snapshot = runtime.block_on(successor.recovery_snapshot()).unwrap();
    assert_eq!(snapshot.generation, unknown.generation);
    assert_eq!(snapshot.operation, unknown.operation);
    assert_eq!(snapshot.repo, unknown.repo);
    assert!(!snapshot.completion_proven);
    let connects = transport.connects.load(Ordering::SeqCst);
    let snapshot = runtime.block_on(successor.recovery_snapshot()).unwrap();
    assert_eq!(transport.connects.load(Ordering::SeqCst), connects);
    let other = RemoteGitWorkspace::new(
        "credential-other-project".into(),
        connection.clone(),
        origin.inner.root.clone(),
    );
    set_project_git_permission(
        &other.inner.project_id,
        Some(&connection),
        &origin.inner.root,
        true,
    );
    assert!(matches!(
        runtime.block_on(other.request_async(write())),
        Err(GitFailure::OutcomeUnknown { .. })
    ));
    assert_code(
        runtime
            .block_on(other.acknowledge_recovery(snapshot.clone()))
            .unwrap_err(),
        "remote_git_recovery_stale",
    );
    runtime
        .block_on(successor.acknowledge_recovery(snapshot))
        .unwrap();
    assert!(!origin.write_outcome_unknown());
    assert!(!successor.write_outcome_unknown());
    assert_eq!(
        lock(&transport.requests)
            .iter()
            .filter(|request| request.contains("StageAll"))
            .count(),
        1
    );
    runtime.block_on(successor.request_async(write())).unwrap();
    assert_eq!(
        lock(&transport.requests)
            .iter()
            .filter(|request| request.contains("StageAll"))
            .count(),
        2
    );
    set_project_git_permission(&other.inner.project_id, None, "", false);
}

#[test]
fn credential_successor_sees_running_without_misreporting_unknown() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (origin, transport) = fixture(Duration::from_secs(60));
    let (entered, release) = block_transport(&transport);
    let request = origin.clone();
    let task = runtime.spawn(async move { request.request_async(write()).await });
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    let mut connection = origin.inner.connection.clone();
    connection.password = Some("new-secret".into());
    let successor = RemoteGitWorkspace::new(
        origin.inner.project_id.clone(),
        connection,
        origin.inner.root.clone(),
    );
    assert!(successor.write_running());
    assert!(!successor.write_outcome_unknown());
    release.send(()).unwrap();
    runtime.block_on(task).unwrap().unwrap();
    assert!(!successor.write_running());
}

#[test]
fn source_revocation_before_running_prevents_dispatch_after_final_setup() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    let source = remote_git_source_change_guard();
    let request = workspace.clone();
    let task = runtime.spawn(async move { request.request_async(write()).await });
    // Connection setup is outside the source lock, but the final lease check and
    // Running publication must wait until the entire source mutation finishes.
    wait_until(|| transport.connects.load(Ordering::SeqCst) == 1);
    assert!(!workspace.write_running());
    set_project_git_permission(&workspace.inner.project_id, None, "", false);
    drop(source);
    assert!(runtime.block_on(task).unwrap().is_err());
    assert!(lock(&transport.requests).is_empty());
    assert!(!workspace.write_running());
    assert!(!workspace.write_outcome_unknown());
}

#[test]
fn dispatched_network_io_does_not_hold_source_lifecycle_lock() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    let (entered, release) = block_transport(&transport);
    let request = workspace.clone();
    let task = runtime.spawn(async move { request.request_async(write()).await });
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    let source = SOURCE_LIFECYCLE
        .try_lock()
        .expect("network IO must not hold source guard");
    assert!(workspace.write_running());
    drop(source);
    release.send(()).unwrap();
    runtime.block_on(task).unwrap().unwrap();
}
