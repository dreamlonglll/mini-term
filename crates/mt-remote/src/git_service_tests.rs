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
    factories: AtomicU64,
    deployments: AtomicU64,
    connections: Mutex<Vec<Arc<AtomicBool>>>,
    connects: AtomicU64,
    drops: AtomicU64,
    proofs: AtomicU64,
    decode_unknown: AtomicU64,
    proof_lost: AtomicBool,
    entered: Mutex<Option<sync_mpsc::Sender<()>>>,
    release: Mutex<Option<sync_mpsc::Receiver<()>>>,
}
struct FakeTransport(Arc<FakeState>, Arc<AtomicBool>);
impl Drop for FakeTransport {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl WorkspaceTransport for FakeTransport {
    fn request(&mut self, operation: Operation) -> Result<Reply, GitFailure> {
        if !self.1.load(Ordering::Acquire) {
            return Err(service_error("test_disconnected"));
        }
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
        if !self.1.load(Ordering::Acquire) || self.0.proof_lost.load(Ordering::Acquire) {
            Err(unknown_failure("test_old_channel_lost"))
        } else {
            Ok(())
        }
    }
    fn mark_last_write_unknown(&mut self) {
        self.0.decode_unknown.fetch_add(1, Ordering::SeqCst);
    }
    fn is_connected(&self) -> bool {
        self.1.load(Ordering::Acquire)
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
            factory_state.factories.fetch_add(1, Ordering::SeqCst);
            lease.check()?;
            factory_state.deployments.fetch_add(1, Ordering::SeqCst);
            lease.check()?;
            factory_state.connects.fetch_add(1, Ordering::SeqCst);
            let connected = Arc::new(AtomicBool::new(true));
            lock(&factory_state.connections).push(connected.clone());
            Ok(Box::new(FakeTransport(factory_state.clone(), connected)))
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

#[test]
fn write_metadata_preserves_repository_and_operation_identity() {
    for (operation, expected) in [
        (write(), RemoteGitOperationKind::StageAll),
        (
            Operation::CommitWith {
                repo: "/repo".into(),
                message: "message".into(),
                amend: false,
            },
            RemoteGitOperationKind::Commit,
        ),
        (
            Operation::CommitWith {
                repo: "/repo".into(),
                message: "message".into(),
                amend: true,
            },
            RemoteGitOperationKind::Amend,
        ),
        (
            Operation::ApplyStash {
                repo: "/repo".into(),
                index: 0,
                hash: "stash-hash".into(),
                pop: false,
                reinstate: false,
            },
            RemoteGitOperationKind::ApplyStash,
        ),
        (
            Operation::ApplyStash {
                repo: "/repo".into(),
                index: 0,
                hash: "stash-hash".into(),
                pop: true,
                reinstate: false,
            },
            RemoteGitOperationKind::PopStash,
        ),
        (
            Operation::Mutate {
                repo: "/repo".into(),
                mutation: mt_git_helper::GitMutation::StashClear,
            },
            RemoteGitOperationKind::ClearStashes,
        ),
    ] {
        assert!(operation.is_write());
        assert_eq!(operation_repo(&operation), Some("/repo"));
        assert_eq!(operation_kind(&operation), Some(expected));
    }
}

#[test]
fn read_operations_have_no_write_metadata() {
    for operation in [
        Operation::Discover,
        Operation::Barrier { completed_id: 1 },
        Operation::Status {
            repo: "/repo".into(),
        },
        Operation::Query {
            repo: "/repo".into(),
            query: GitQuery::RepoSignature,
        },
        Operation::ChangesSnapshot {
            repo: "/repo".into(),
        },
        Operation::ListStashes {
            repo: "/repo".into(),
        },
    ] {
        assert!(!operation.is_write());
        assert_eq!(operation_repo(&operation), None);
        assert_eq!(operation_kind(&operation), None);
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
        Err(GitFailure::RepositoryBlocked { .. })
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
        Err(GitFailure::RepositoryBlocked { .. })
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
    assert_eq!(snapshot.operation, RemoteGitOperationKind::StageAll);
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
    let close_started = Instant::now();
    workspace.close();
    assert!(close_started.elapsed() < Duration::from_millis(250));
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
    for same_root in [true, false] {
        let (origin, transport) = fixture(Duration::from_secs(60));
        let root = if same_root {
            origin.inner.root.clone()
        } else {
            "/other-grant-root".into()
        };
        let other = RemoteGitWorkspace::new(
            format!("other-granted-project-{same_root}"),
            origin.inner.connection.clone(),
            root,
        );
        set_project_git_permission(
            &other.inner.project_id,
            Some(&other.inner.connection),
            &other.inner.root,
            true,
        );
        lose_write(&origin, &transport);
        assert_eq!(
            runtime.block_on(other.request_async(write())).unwrap_err(),
            GitFailure::RepositoryBlocked {
                repo: "/repo".into(),
                owner_project_ids: vec![origin.inner.project_id.clone()],
            }
        );
        assert!(!other.write_outcome_unknown());
        assert_eq!(
            lock(&transport.requests).len(),
            1,
            "blocked write was not dispatched"
        );
        assert_code(
            runtime.block_on(other.recovery_snapshot()).unwrap_err(),
            "remote_git_no_unknown_write",
        );
        assert!(other.prepare_offline_abandon().is_err());
        assert!(other.recovery_consent().is_err());
        let snapshot = runtime.block_on(origin.recovery_snapshot()).unwrap();
        assert_code(
            runtime
                .block_on(other.acknowledge_recovery(snapshot))
                .unwrap_err(),
            "remote_git_recovery_stale",
        );
        let before = lock(&transport.requests).len();
        let confirmation = origin.prepare_offline_abandon().unwrap();
        assert_code(
            other.abandon_recovery(confirmation).unwrap_err(),
            "remote_git_recovery_stale",
        );
        assert_eq!(lock(&transport.requests).len(), before);
        assert!(origin.write_outcome_unknown());
        set_project_git_permission(&other.inner.project_id, None, "", false);
        assert!(origin.inner.ensure_active().is_ok());
        recover(&origin);
    }
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
        Err(GitFailure::RepositoryBlocked { .. })
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
        Err(GitFailure::RepositoryBlocked { .. })
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

#[test]
fn revoked_offline_abandon_never_waits_for_actor_or_restores_permission() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    lose_write(&workspace, &transport);
    set_project_git_permission(&workspace.inner.project_id, None, "", false);
    let confirmation = workspace.prepare_offline_abandon().unwrap();
    let (entered, release) = block_transport(&transport);
    let request = workspace.clone();
    let consent = workspace.recovery_consent().unwrap();
    let snapshot =
        runtime.spawn(async move { request.recovery_snapshot_with_consent(consent).await });
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    let requests = lock(&transport.requests).len();
    let connects = transport.connects.load(Ordering::SeqCst);
    let started = Instant::now();
    workspace.abandon_recovery(confirmation).unwrap();
    assert!(started.elapsed() < Duration::from_millis(250));
    assert!(!workspace.write_outcome_unknown());
    assert_eq!(transport.connects.load(Ordering::SeqCst), connects);
    assert_eq!(lock(&transport.requests).len(), requests);
    assert!(!lock(&GRANTS).contains_key(&workspace.inner.project_id));
    release.send(()).unwrap();
    assert!(runtime.block_on(snapshot).unwrap().is_err());
    assert_code(
        runtime
            .block_on(workspace.request_async(write()))
            .unwrap_err(),
        "remote_git_disabled",
    );
}

#[test]
fn offline_confirmation_rejects_running_epoch_successor_and_new_generation() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    let (entered, release) = block_transport(&transport);
    let request = workspace.clone();
    let running = runtime.spawn(async move { request.request_async(write()).await });
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(
        matches!(workspace.prepare_offline_abandon(), Err(GitFailure::Service { code, .. }) if code == "remote_git_write_running")
    );
    release.send(()).unwrap();
    runtime.block_on(running).unwrap().unwrap();
    lose_write(&workspace, &transport);
    let old = workspace.prepare_offline_abandon().unwrap();
    workspace.close();
    assert_code(
        workspace.abandon_recovery(old).unwrap_err(),
        "remote_git_recovery_stale",
    );
    let old = workspace.prepare_offline_abandon().unwrap();
    let mut credentials = workspace.inner.connection.clone();
    credentials.password = Some("replacement".into());
    let successor = credential_successor(&workspace, credentials.clone());
    set_project_git_permission(
        &workspace.inner.project_id,
        Some(&credentials),
        &workspace.inner.root,
        true,
    );
    assert_code(
        successor.abandon_recovery(old).unwrap_err(),
        "remote_git_recovery_stale",
    );
    let old = successor.prepare_offline_abandon().unwrap();
    recover(&successor);
    lock(&transport.results).push_back(Err(unknown_failure("test_new_generation")));
    assert!(matches!(
        runtime.block_on(successor.request_async(write())),
        Err(GitFailure::OutcomeUnknown { .. })
    ));
    let new_generation = successor.unknown_write().unwrap().generation;
    assert_code(
        successor.abandon_recovery(old).unwrap_err(),
        "remote_git_recovery_stale",
    );
    assert_eq!(
        successor.unknown_write().unwrap().generation,
        new_generation
    );
    successor
        .abandon_recovery(successor.prepare_offline_abandon().unwrap())
        .unwrap();
}

#[test]
fn recovery_only_consent_observes_and_acknowledges_without_git_grant() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    lose_write(&workspace, &transport);
    set_project_git_permission(&workspace.inner.project_id, None, "", false);
    let before = lock(&transport.requests).len();
    let consent = workspace.recovery_consent().unwrap();
    assert_eq!(lock(&transport.requests).len(), before);
    assert_code(
        runtime
            .block_on(workspace.request_async(Operation::Discover))
            .unwrap_err(),
        "remote_git_disabled",
    );
    let snapshot = runtime
        .block_on(workspace.recovery_snapshot_with_consent(consent))
        .unwrap();
    assert!(!lock(&GRANTS).contains_key(&workspace.inner.project_id));
    let requests = lock(&transport.requests);
    assert!(
        requests[before..]
            .iter()
            .all(|request| request.starts_with("Query") || request.starts_with("Status"))
    );
    drop(requests);
    let late_confirmation = workspace.prepare_offline_abandon().unwrap();
    runtime
        .block_on(workspace.acknowledge_recovery(snapshot.clone()))
        .unwrap();
    assert!(!workspace.write_outcome_unknown());
    assert_code(
        workspace.abandon_recovery(late_confirmation).unwrap_err(),
        "remote_git_recovery_stale",
    );
    assert!(
        runtime
            .block_on(workspace.acknowledge_recovery(snapshot))
            .is_err()
    );
    assert_code(
        runtime
            .block_on(workspace.request_async(write()))
            .unwrap_err(),
        "remote_git_disabled",
    );
    assert_eq!(
        lock(&transport.requests)
            .iter()
            .filter(|request| request.starts_with("StageAll"))
            .count(),
        1
    );
}

#[test]
fn business_read_errors_preserve_reader_and_snapshot_but_recovery_errors_do_not() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    lose_write(&workspace, &transport);
    let snapshot = runtime.block_on(workspace.recovery_snapshot()).unwrap();
    let connects = transport.connects.load(Ordering::SeqCst);
    lock(&transport.results).push_back(Err("ordinary missing ref".into()));
    assert!(
        runtime
            .block_on(workspace.request_async(Operation::Discover))
            .is_err()
    );
    runtime
        .block_on(workspace.acknowledge_recovery(snapshot))
        .unwrap();
    assert_eq!(transport.connects.load(Ordering::SeqCst), connects);
    lose_write(&workspace, &transport);
    let snapshot = runtime.block_on(workspace.recovery_snapshot()).unwrap();
    lock(&transport.results).push_back(Err("recovery business failure".into()));
    assert!(runtime.block_on(workspace.recovery_snapshot()).is_err());
    assert_code(
        runtime
            .block_on(workspace.acknowledge_recovery(snapshot))
            .unwrap_err(),
        "remote_git_recovery_stale",
    );
    recover(&workspace);
}

#[test]
fn actual_reader_disconnect_reconnects_only_its_own_connection() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    lose_write(&workspace, &transport);
    let snapshot = runtime.block_on(workspace.recovery_snapshot()).unwrap();
    let connections = lock(&transport.connections);
    connections[1].store(false, Ordering::Release);
    assert!(connections[0].load(Ordering::Acquire));
    drop(connections);
    assert!(
        runtime
            .block_on(workspace.request_async(Operation::Discover))
            .is_err()
    );
    assert_code(
        runtime
            .block_on(workspace.acknowledge_recovery(snapshot))
            .unwrap_err(),
        "remote_git_recovery_stale",
    );
    recover(&workspace);
    assert_eq!(transport.connects.load(Ordering::SeqCst), 3);
}

#[test]
fn denied_permission_reaches_neither_factory_deployment_nor_connect() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    set_project_git_permission(&workspace.inner.project_id, None, "", false);
    assert_code(
        runtime
            .block_on(workspace.request_async(Operation::Discover))
            .unwrap_err(),
        "remote_git_disabled",
    );
    assert_eq!(transport.factories.load(Ordering::SeqCst), 0);
    assert_eq!(transport.deployments.load(Ordering::SeqCst), 0);
    assert_eq!(transport.connects.load(Ordering::SeqCst), 0);
    assert!(lock(&workspace.inner.sender).is_none());
}

#[test]
fn all_shared_repository_blockers_expose_owners_but_not_recovery_authority() {
    let _serial = lock(&TEST_LOCK);
    let (first, first_transport) = fixture(Duration::from_secs(60));
    let (second, _) = fixture(Duration::from_secs(60));
    lose_write(&first, &first_transport);
    // Represent another independently dispatched operation that became unknown before
    // either project's result was published; both identities must be visible.
    {
        let _source = remote_git_source_change_guard();
        let mut registry = lock(&UNKNOWN_WORKSPACES);
        *lock(&second.inner.unknown) = Some(RemoteGitUnknownWrite {
            generation: SEQUENCE.fetch_add(1, Ordering::AcqRel),
            repo: "/repo".into(),
            operation: RemoteGitOperationKind::Push,
            occurred: SystemTime::now(),
        });
        registry.push(second.inner.clone());
    }
    let observer = RemoteGitWorkspace::new(
        "metadata-only-observer".into(),
        first.inner.connection.clone(),
        first.inner.root.clone(),
    );
    let blockers = observer.recovery_blockers("/repo/");
    assert_eq!(blockers.len(), 2);
    assert!(
        blockers
            .iter()
            .any(|blocker| blocker.owner_project_id == first.inner.project_id)
    );
    assert!(
        blockers
            .iter()
            .any(|blocker| blocker.owner_project_id == second.inner.project_id)
    );
    set_project_git_permission(
        &observer.inner.project_id,
        Some(&observer.inner.connection),
        &observer.inner.root,
        true,
    );
    let mut expected_ids = vec![
        first.inner.project_id.clone(),
        second.inner.project_id.clone(),
    ];
    expected_ids.sort_unstable();
    assert_eq!(
        state()
            .runtime()
            .unwrap()
            .block_on(observer.request_async(write()))
            .unwrap_err(),
        GitFailure::RepositoryBlocked {
            repo: "/repo".into(),
            owner_project_ids: expected_ids,
        }
    );
    assert!(lock(&observer.inner.sender).is_some());
    assert!(!observer.write_outcome_unknown());
    assert_eq!(first_transport.connects.load(Ordering::SeqCst), 1);
    assert!(observer.prepare_offline_abandon().is_err());
    assert!(observer.recovery_consent().is_err());
    let confirmation = first.prepare_offline_abandon().unwrap();
    assert_code(
        observer.abandon_recovery(confirmation).unwrap_err(),
        "remote_git_recovery_stale",
    );
    first
        .abandon_recovery(first.prepare_offline_abandon().unwrap())
        .unwrap();
    second
        .abandon_recovery(second.prepare_offline_abandon().unwrap())
        .unwrap();
}

#[test]
fn own_unknown_on_another_repository_reports_the_original_repository() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    lose_write(&workspace, &transport);
    let before = lock(&transport.requests).len();
    assert_eq!(
        runtime
            .block_on(workspace.request_async(Operation::StageAll {
                repo: "/another-repository".into(),
            }))
            .unwrap_err(),
        GitFailure::RepositoryBlocked {
            repo: "/repo".into(),
            owner_project_ids: vec![workspace.inner.project_id.clone()],
        }
    );
    assert_eq!(lock(&transport.requests).len(), before);
    assert_eq!(workspace.unknown_write().unwrap().repo, "/repo");
    workspace
        .abandon_recovery(workspace.prepare_offline_abandon().unwrap())
        .unwrap();
}

#[test]
fn revoked_workspace_scope_metadata_includes_nested_repos_not_siblings_or_other_endpoints() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (root_owner, _) = fixture(Duration::from_secs(60));
    let root = root_owner.inner.root.clone();
    let repos = [
        root.clone(),
        format!("{root}/nested/repository/"),
        format!("{root}-sibling/repository"),
        format!("{root}/case-sensitive"),
        format!("{root}/../sibling/repository"),
        format!("{}/nested", root.to_uppercase()),
    ];
    let mut owners = Vec::new();
    for (index, repo) in repos.iter().enumerate() {
        let (owner, transport) = fixture(Duration::from_secs(60));
        let owner = if index == 3 {
            let mut endpoint = owner.inner.connection.clone();
            endpoint.host = "another-server.invalid".into();
            credential_successor(&owner, endpoint)
        } else {
            owner
        };
        {
            let _source = remote_git_source_change_guard();
            let mut registry = lock(&UNKNOWN_WORKSPACES);
            *lock(&owner.inner.unknown) = Some(RemoteGitUnknownWrite {
                generation: SEQUENCE.fetch_add(1, Ordering::AcqRel),
                repo: repo.clone(),
                operation: RemoteGitOperationKind::StageAll,
                occurred: SystemTime::now(),
            });
            registry.push(owner.inner.clone());
        }
        owners.push((owner, transport));
    }
    let observer = RemoteGitWorkspace::new(
        "revoked-scope-observer".into(),
        root_owner.inner.connection.clone(),
        format!("{root}/"),
    );
    set_project_git_permission(
        &observer.inner.project_id,
        Some(&observer.inner.connection),
        &observer.inner.root,
        true,
    );
    set_project_git_permission(&observer.inner.project_id, None, "", false);
    let blockers = observer.recovery_blockers_in_scope();
    assert_eq!(blockers.len(), 2);
    assert!(
        blockers
            .iter()
            .any(|blocker| blocker.write.repo == repos[0])
    );
    assert!(
        blockers
            .iter()
            .any(|blocker| blocker.write.repo == repos[1])
    );
    assert_eq!(
        observer.recovery_blockers(&root).len(),
        1,
        "exact API stays exact"
    );
    assert!(observer.unknown_write().is_none());
    assert!(observer.prepare_offline_abandon().is_err());
    assert!(observer.recovery_consent().is_err());
    assert!(lock(&observer.inner.sender).is_none());
    assert_code(
        runtime
            .block_on(observer.request_async(Operation::Discover))
            .unwrap_err(),
        "remote_git_disabled",
    );
    for (owner, transport) in owners {
        assert_eq!(transport.factories.load(Ordering::SeqCst), 0);
        assert!(lock(&transport.requests).is_empty());
        owner
            .abandon_recovery(owner.prepare_offline_abandon().unwrap())
            .unwrap();
    }
}

#[test]
fn offline_abandon_wins_over_inflight_ack_without_clearing_next_generation() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    lose_write(&workspace, &transport);
    let snapshot = runtime.block_on(workspace.recovery_snapshot()).unwrap();
    let confirmation = workspace.prepare_offline_abandon().unwrap();
    let (entered, release) = block_transport(&transport);
    let request = workspace.clone();
    let ack = runtime.spawn(async move { request.acknowledge_recovery(snapshot).await });
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    workspace.abandon_recovery(confirmation).unwrap();
    release.send(()).unwrap();
    assert!(runtime.block_on(ack).unwrap().is_err());
    assert!(!workspace.write_outcome_unknown());
    lock(&transport.results).push_back(Err(unknown_failure("next_write_lost")));
    assert!(matches!(
        runtime.block_on(workspace.request_async(write())),
        Err(GitFailure::OutcomeUnknown { .. })
    ));
    assert!(workspace.write_outcome_unknown());
    workspace
        .abandon_recovery(workspace.prepare_offline_abandon().unwrap())
        .unwrap();
}

#[test]
fn write_rechecks_shared_unknown_after_transport_setup_before_dispatch() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (origin, origin_transport) = fixture(Duration::from_secs(60));
    let (candidate, candidate_transport) = fixture(Duration::from_secs(60));
    let source = remote_git_source_change_guard();
    let request = candidate.clone();
    let task = runtime.spawn(async move { request.request_async(write()).await });
    wait_until(|| candidate_transport.connects.load(Ordering::SeqCst) == 1);
    // The operation passed admission before this independently completed write became
    // unknown, but has not crossed the final lifecycle-protected dispatch boundary.
    {
        let mut registry = lock(&UNKNOWN_WORKSPACES);
        *lock(&origin.inner.unknown) = Some(RemoteGitUnknownWrite {
            generation: SEQUENCE.fetch_add(1, Ordering::AcqRel),
            repo: "/repo".into(),
            operation: RemoteGitOperationKind::Push,
            occurred: SystemTime::now(),
        });
        registry.push(origin.inner.clone());
    }
    drop(source);
    assert_eq!(
        runtime.block_on(task).unwrap().unwrap_err(),
        GitFailure::RepositoryBlocked {
            repo: "/repo".into(),
            owner_project_ids: vec![origin.inner.project_id.clone()],
        }
    );
    assert!(lock(&candidate_transport.requests).is_empty());
    assert!(!candidate.write_running());
    assert!(!candidate.write_outcome_unknown());
    assert_eq!(origin_transport.connects.load(Ordering::SeqCst), 0);
    origin
        .abandon_recovery(origin.prepare_offline_abandon().unwrap())
        .unwrap();
}

#[test]
fn old_recovery_consent_is_rejected_before_connect_after_source_epoch_change() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    lose_write(&workspace, &transport);
    set_project_git_permission(&workspace.inner.project_id, None, "", false);
    let consent = workspace.recovery_consent().unwrap();
    let connects = transport.connects.load(Ordering::SeqCst);
    workspace.close();
    assert!(
        matches!(runtime.block_on(workspace.recovery_snapshot_with_consent(consent)),
        Err(GitFailure::Service { code, .. }) if code == "remote_git_recovery_stale")
    );
    assert_eq!(transport.connects.load(Ordering::SeqCst), connects);
    assert!(workspace.write_outcome_unknown());
    workspace
        .abandon_recovery(workspace.prepare_offline_abandon().unwrap())
        .unwrap();
}

#[test]
fn unknown_metadata_retains_operation_kind_not_commit_message_payload() {
    let _serial = lock(&TEST_LOCK);
    let runtime = state().runtime().unwrap();
    let (workspace, transport) = fixture(Duration::from_secs(60));
    lock(&transport.results).push_back(Err(unknown_failure("commit_lost")));
    assert!(matches!(
        runtime.block_on(workspace.request_async(Operation::CommitWith {
            repo: "/repo".into(),
            message: "private commit payload".repeat(4096),
            amend: true,
        })),
        Err(GitFailure::OutcomeUnknown { .. })
    ));
    assert_eq!(
        workspace.unknown_write().unwrap().operation,
        RemoteGitOperationKind::Amend
    );
    workspace
        .abandon_recovery(workspace.prepare_offline_abandon().unwrap())
        .unwrap();
}
