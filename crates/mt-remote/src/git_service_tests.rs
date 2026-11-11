use super::*;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn connection() -> SshConnection {
    SshConnection {
        id: "managed-git-unit-test".into(), name: "test".into(), host: "example.invalid".into(),
        port: 22, user: "tester".into(), password: None, identity_file: None,
        group: None, extra: Default::default(),
    }
}

#[test]
fn repository_identity_includes_endpoint_root_and_case_sensitive_repo() {
    let base = RemoteGitWorkspace::new(connection(), "/Project".into());
    assert_eq!(base.repository_key("/Project/Repo"), base.clone().repository_key("/Project/Repo"));
    assert_ne!(base.repository_key("/Project/Repo"), base.repository_key("/Project/repo"));
    let root = RemoteGitWorkspace::new(connection(), "/project".into());
    assert_ne!(base.repository_key("/Project/Repo"), root.repository_key("/Project/Repo"));
    let mut changed = connection();
    changed.host = "other.invalid".into();
    let endpoint = RemoteGitWorkspace::new(changed, "/Project".into());
    assert_ne!(base.repository_key("/Project/Repo"), endpoint.repository_key("/Project/Repo"));
    // Separators inside strings cannot alias another identity.
    let mut a = connection();
    a.id = "id|x".into();
    let a = RemoteGitWorkspace::new(a, "/r".into());
    let mut b = connection();
    b.id = "id".into();
    let b = RemoteGitWorkspace::new(b, "x|/r".into());
    assert_ne!(a.repository_key("/r"), b.repository_key("/r"));
}

#[test]
fn unknown_write_state_allows_only_explicit_read_recovery() {
    assert!(require_verified_write_state(false, true).is_ok());
    assert!(require_verified_write_state(false, false).is_ok());
    assert!(require_verified_write_state(true, false).is_ok());
    let error = require_verified_write_state(true, true).unwrap_err();
    assert!(error.contains("outcome is unknown"));
    assert!(error.contains("no request was replayed"));
}

#[test]
fn closing_never_waits_for_the_workspace_request_mutex() {
    // No network or assets: only instantiate the existing service runtime.
    let _runtime = state().runtime().unwrap();
    let workspace = RemoteGitWorkspace::new(connection(), "/Project".into());
    let inner = workspace.inner.clone();
    let (locked_tx, locked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _guard = lock(&inner.client);
        locked_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    locked_rx.recv().unwrap();
    let start = Instant::now();
    workspace.close();
    let elapsed = start.elapsed();
    release_tx.send(()).unwrap();
    worker.join().unwrap();
    assert!(elapsed < Duration::from_millis(250), "close blocked for {elapsed:?}");
}

#[test]
fn invalidation_rejects_old_connection_before_acquiring_ssh() {
    let workspace = RemoteGitWorkspace::new(connection(), "/Project".into());
    workspace.inner.invalidated.store(true, Ordering::Release);
    let error = workspace.request(Operation::Discover).unwrap_err();
    assert!(error.contains("configuration changed"));
    assert!(lock(&workspace.inner.client).client.is_none());
}

#[test]
fn close_does_not_clear_an_unknown_write_outcome() {
    let workspace = RemoteGitWorkspace::new(connection(), "/Project".into());
    lock(&workspace.inner.client).write_outcome_unknown = true;
    workspace.close();
    assert!(lock(&workspace.inner.client).write_outcome_unknown);
}
