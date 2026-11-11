//! Real SSH channels, isolated loopback listener and known_hosts; no user SSH state.
use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use mt_ssh::russh::{self, ChannelId, server};
use mt_ssh::{PoolConfig, SshClientEnv};
use tokio::net::TcpListener;
use tokio::sync::Notify;

static UNIQUE: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Default)]
enum Startup {
    #[default]
    Hello,
    Reject,
    Exit,
    WrongVersion,
    Silent,
}

#[derive(Clone, Copy, Default)]
enum Answer {
    #[default]
    Normal,
    Zero,
    Oversize,
    Truncated,
    InvalidJson,
    Trailing,
    WrongId,
    WrongVersion,
    WrongKind,
    Disconnect,
    ErrorOnce,
    StderrFlood,
    Silent,
    Large,
}

#[derive(Clone, Default)]
struct Behavior {
    startup: Startup,
    answer: Answer,
    fragmented: bool,
    finish_gate: Option<Arc<Notify>>,
    ignore_eof: bool,
}

#[derive(Default)]
struct Counts {
    sessions: AtomicUsize,
    channels: AtomicUsize,
    requests: AtomicUsize,
    eof: AtomicUsize,
    finished: AtomicUsize,
    closed: AtomicUsize,
}

struct Handler {
    behavior: Behavior,
    counts: Arc<Counts>,
    input: Vec<u8>,
}

fn hello(version: u32) -> Vec<u8> {
    mt_git_helper::encode_frame(&Hello {
        protocol_version: version,
        helper_version: "test-server".into(),
    })
    .unwrap()
}

fn send(
    session: &mut server::Session,
    channel: ChannelId,
    bytes: Vec<u8>,
    fragmented: bool,
) -> Result<(), russh::Error> {
    if fragmented {
        // Split both the four-byte header and JSON across SSH data messages.
        for chunk in bytes.chunks(3) {
            session.data(channel, chunk.to_vec())?;
        }
        Ok(())
    } else {
        session.data(channel, bytes)
    }
}

impl server::Handler for Handler {
    type Error = russh::Error;

    async fn auth_password(
        &mut self,
        _user: &str,
        _password: &str,
    ) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<server::Msg>,
        _session: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        self.counts.channels.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        match self.behavior.startup {
            Startup::Reject => session.channel_failure(channel)?,
            Startup::Exit => {
                session.channel_success(channel)?;
                session.extended_data(channel, 1, b"helper cannot start".to_vec())?;
                session.exit_status_request(channel, 23)?;
                session.eof(channel)?;
                session.close(channel)?;
            }
            Startup::Hello | Startup::WrongVersion => {
                session.channel_success(channel)?;
                let version = if matches!(self.behavior.startup, Startup::WrongVersion) {
                    PROTOCOL_VERSION + 1
                } else {
                    PROTOCOL_VERSION
                };
                send(session, channel, hello(version), self.behavior.fragmented)?;
            }
            Startup::Silent => {
                session.channel_success(channel)?;
            }
        }
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.input.extend_from_slice(data);
        if self.input.len() < 4 {
            return Ok(());
        }
        let size = u32::from_be_bytes(self.input[..4].try_into().unwrap()) as usize;
        if self.input.len() < size + 4 {
            return Ok(());
        }
        let request: Request = serde_json::from_slice(&self.input[4..size + 4]).unwrap();
        self.input.drain(..size + 4);
        let number = self.counts.requests.fetch_add(1, Ordering::SeqCst);
        let reply = match &request.operation {
            Operation::Discover => Reply::Repositories { repos: Vec::new() },
            Operation::Status { .. } => Reply::Status { files: Vec::new() },
            Operation::Commit { .. } => Reply::Committed {
                output: if matches!(self.behavior.answer, Answer::Large) {
                    "x".repeat(3 * 1024 * 1024)
                } else {
                    "committed".into()
                },
            },
            _ => Reply::Updated,
        };
        let mut response = Response {
            protocol_version: PROTOCOL_VERSION,
            id: request.id,
            result: Ok(reply),
        };
        match self.behavior.answer {
            Answer::WrongId => response.id += 1,
            Answer::WrongVersion => response.protocol_version += 1,
            Answer::WrongKind => response.result = Ok(Reply::Updated),
            Answer::ErrorOnce if number == 0 => {
                response.result = Err("repository rejected operation".into())
            }
            Answer::StderrFlood => {
                session.extended_data(channel, 1, vec![b'e'; 3 * 1024 * 1024])?;
                // Queue a malformed response after all stderr, rather than an exit-status
                // request, which is allowed to overtake window-blocked channel data.
                session.data(channel, vec![0, 0, 0, 1, b'!'])?;
                session.eof(channel)?;
                session.close(channel)?;
                return Ok(());
            }
            Answer::Disconnect => {
                session.close(channel)?;
                return Ok(());
            }
            Answer::Silent => return Ok(()),
            _ => {}
        }
        let bytes = match self.behavior.answer {
            Answer::Zero => vec![0; 4],
            Answer::Oversize => ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes().to_vec(),
            Answer::Truncated => vec![0, 0, 0, 9, b'{'],
            Answer::InvalidJson => vec![0, 0, 0, 1, b'!'],
            Answer::Trailing => {
                let mut bytes = mt_git_helper::encode_frame(&response).unwrap();
                bytes.push(b'!');
                bytes
            }
            _ => mt_git_helper::encode_frame(&response).unwrap(),
        };
        send(session, channel, bytes, self.behavior.fragmented)?;
        if matches!(self.behavior.answer, Answer::Truncated) {
            session.eof(channel)?;
            session.close(channel)?;
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.counts.eof.fetch_add(1, Ordering::SeqCst);
        if self.behavior.ignore_eof {
            return Ok(());
        }
        if let Some(gate) = self.behavior.finish_gate.clone() {
            let handle = session.handle();
            let counts = self.counts.clone();
            tokio::spawn(async move {
                gate.notified().await;
                counts.finished.fetch_add(1, Ordering::SeqCst);
                let _ = handle.exit_status_request(channel, 0).await;
                let _ = handle.eof(channel).await;
                let _ = handle.close(channel).await;
            });
        } else {
            self.counts.finished.fetch_add(1, Ordering::SeqCst);
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
        }
        Ok(())
    }

    async fn channel_close(
        &mut self,
        _channel: ChannelId,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.counts.closed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Fixture {
    runtime: Arc<tokio::runtime::Runtime>,
    pool: Arc<SshPool>,
    connection: SshConnection,
    counts: Arc<Counts>,
    listener: tokio::task::JoinHandle<()>,
    home: PathBuf,
}

impl Fixture {
    fn new(behavior: Behavior) -> Self {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap(),
        );
        let home = std::env::temp_dir().join(format!(
            "mt-remote-git-{}-{}",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&home).unwrap();
        let (pool, connection, counts, listener) = runtime.block_on(async {
            let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = socket.local_addr().unwrap().port();
            let key = russh::keys::PrivateKey::from(
                russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[79; 32]));
            let config = Arc::new(server::Config {
                keys: vec![key],
                auth_rejection_time: Duration::ZERO,
                auth_rejection_time_initial: Some(Duration::ZERO),
                ..Default::default()
            });
            let counts = Arc::new(Counts::default());
            let server_counts = counts.clone();
            let listener = tokio::spawn(async move {
                let mut sessions = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = socket.accept() => {
                            let Ok((stream, _)) = accepted else { break };
                            server_counts.sessions.fetch_add(1, Ordering::SeqCst);
                            let handler = Handler {
                                behavior: behavior.clone(), counts: server_counts.clone(), input: Vec::new(),
                            };
                            let config = config.clone();
                            sessions.spawn(async move {
                                if let Ok(session) = server::run_stream(config, stream, handler).await {
                                    let _ = session.await;
                                }
                            });
                        }
                        _ = sessions.join_next(), if !sessions.is_empty() => {}
                    }
                }
            });
            let pool = Arc::new(SshPool::with_env(PoolConfig::default(),
                SshClientEnv::isolated(home.join("known_hosts"))));
            let connection = SshConnection {
                id: "git-test".into(), name: "git-test".into(), host: "127.0.0.1".into(),
                port, user: "tester".into(), password: Some("test-only".into()),
                identity_file: None, group: None, extra: Default::default(),
            };
            (pool, connection, counts, listener)
        });
        Self {
            runtime,
            pool,
            connection,
            counts,
            listener,
            home,
        }
    }

    fn connect(&self) -> Result<RemoteGitClient, String> {
        self.connect_with(GitTimeouts {
            start: Duration::from_secs(3),
            request: Duration::from_secs(3),
            close: Duration::from_secs(1),
        })
    }

    fn connect_with(&self, timeouts: GitTimeouts) -> Result<RemoteGitClient, String> {
        RemoteGitClient::connect_with(
            &self.connection,
            "/test/helper",
            "/test/repository",
            self.runtime.clone(),
            self.pool.clone(),
            timeouts,
        )
    }

    fn wait_count(&self, count: &AtomicUsize, expected: usize) {
        self.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                while count.load(Ordering::SeqCst) < expected {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("server lifecycle event");
        });
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.runtime.block_on(self.pool.shutdown());
        self.listener.abort();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn status() -> Operation {
    Operation::Status {
        repo: "/test/repository".into(),
    }
}
fn commit() -> Operation {
    Operation::Commit {
        repo: "/test/repository".into(),
        message: "test".into(),
    }
}

#[test]
fn persistent_fragmented_requests_and_close_confirm_helper_completion() {
    let fixture = Fixture::new(Behavior {
        fragmented: true,
        ..Default::default()
    });
    let mut client = fixture.connect().unwrap();
    assert!(matches!(
        client.request(Operation::Discover).unwrap(),
        Reply::Repositories { .. }
    ));
    assert!(matches!(
        client.request(status()).unwrap(),
        Reply::Status { .. }
    ));
    assert!(matches!(
        client.request(commit()).unwrap(),
        Reply::Committed { .. }
    ));
    client.close().unwrap();
    assert_eq!(fixture.counts.finished.load(Ordering::SeqCst), 1);
    fixture.wait_count(&fixture.counts.closed, 1);
    assert_eq!(fixture.counts.channels.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.counts.sessions.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.counts.requests.load(Ordering::SeqCst), 3);
    client.close().unwrap();
    assert!(client.request(status()).is_err());
}

#[test]
fn drop_is_nonblocking_but_waits_for_remote_completion_in_background() {
    let gate = Arc::new(Notify::new());
    let fixture = Fixture::new(Behavior {
        finish_gate: Some(gate.clone()),
        ..Default::default()
    });
    let client = fixture.connect().unwrap();
    drop(client); // A synchronous drain would hang here until the gate below opens.
    fixture.wait_count(&fixture.counts.eof, 1);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(fixture.counts.finished.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.counts.closed.load(Ordering::SeqCst), 0);
    gate.notify_one();
    fixture.wait_count(&fixture.counts.closed, 1);
    assert_eq!(fixture.counts.finished.load(Ordering::SeqCst), 1);
}

#[test]
fn rejected_exited_incompatible_and_silent_helpers_fail_handshake() {
    for (startup, expected) in [
        (Startup::Reject, "exec request rejected"),
        (Startup::Exit, "status 23"),
        (Startup::WrongVersion, "protocol mismatch"),
        (Startup::Silent, "handshake timed out"),
    ] {
        let fixture = Fixture::new(Behavior {
            startup,
            ..Default::default()
        });
        let error = fixture
            .connect_with(GitTimeouts {
                start: Duration::from_millis(150),
                request: Duration::from_secs(1),
                close: Duration::from_millis(150),
            })
            .err()
            .expect("handshake must fail");
        assert!(error.contains(expected), "{error}");
        if matches!(startup, Startup::Exit) {
            assert!(error.contains("helper cannot start"), "{error}");
        }
        fixture.wait_count(&fixture.counts.closed, 1);
        assert_eq!(fixture.counts.requests.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn malformed_responses_invalidate_reads_without_replay() {
    for (answer, expected) in [
        (Answer::Zero, "invalid Git frame length 0"),
        (Answer::Oversize, "invalid Git frame length"),
        (Answer::Truncated, "disconnected before a complete response"),
        (Answer::InvalidJson, "invalid Git protocol JSON"),
        (Answer::Trailing, "unexpected trailing stdout"),
        (Answer::WrongId, "response mismatch"),
        (Answer::WrongVersion, "response mismatch"),
        (Answer::WrongKind, "response kind mismatch"),
        (Answer::Silent, "request timed out"),
        (
            Answer::Disconnect,
            "disconnected before a complete response",
        ),
    ] {
        let fixture = Fixture::new(Behavior {
            answer,
            ..Default::default()
        });
        let mut client = fixture
            .connect_with(GitTimeouts {
                start: Duration::from_secs(3),
                request: Duration::from_millis(150),
                close: Duration::from_millis(150),
            })
            .unwrap();
        let error = client.request(status()).unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains("outcome unknown"), "{error}");
        assert!(
            client
                .request(status())
                .unwrap_err()
                .contains("client is closed")
        );
        assert_eq!(fixture.counts.requests.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.counts.channels.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn dispatched_writes_are_unknown_after_disconnect_timeout_or_wrong_reply() {
    for answer in [Answer::Disconnect, Answer::Silent, Answer::WrongKind] {
        let fixture = Fixture::new(Behavior {
            answer,
            ..Default::default()
        });
        let mut client = fixture
            .connect_with(GitTimeouts {
                start: Duration::from_secs(3),
                request: Duration::from_millis(150),
                close: Duration::from_millis(150),
            })
            .unwrap();
        let error = client.request(commit()).unwrap_err();
        assert!(error.contains("remote outcome unknown"), "{error}");
        assert!(error.contains("not retried"), "{error}");
        assert!(
            client
                .request(commit())
                .unwrap_err()
                .contains("client is closed")
        );
        assert_eq!(fixture.counts.requests.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.counts.sessions.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn helper_operation_error_keeps_stream_usable() {
    let fixture = Fixture::new(Behavior {
        answer: Answer::ErrorOnce,
        ..Default::default()
    });
    let mut client = fixture.connect().unwrap();
    let error = client.request(commit()).unwrap_err();
    assert_eq!(error, "repository rejected operation");
    assert!(matches!(
        client.request(status()).unwrap(),
        Reply::Status { .. }
    ));
    client.close().unwrap();
    assert_eq!(fixture.counts.requests.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.counts.channels.load(Ordering::SeqCst), 1);
}

#[test]
fn stderr_flood_is_drained_across_windows_but_diagnostics_are_bounded() {
    let fixture = Fixture::new(Behavior {
        answer: Answer::StderrFlood,
        ..Default::default()
    });
    let mut client = fixture.connect().unwrap();
    let error = client.request(status()).unwrap_err();
    assert!(error.contains("invalid Git protocol JSON"), "{error}");
    let stderr = error.split("; helper stderr: ").nth(1).unwrap();
    assert_eq!(stderr, format!("{} [truncated]", "e".repeat(STDERR_CAP)));
    assert!(client.request(status()).is_err());
}

#[test]
fn large_frames_adjust_both_channel_windows() {
    let fixture = Fixture::new(Behavior {
        answer: Answer::Large,
        ..Default::default()
    });
    let mut client = fixture.connect().unwrap();
    let operation = Operation::Commit {
        repo: "/test/repository".into(),
        message: "m".repeat(3 * 1024 * 1024),
    };
    let Reply::Committed { output } = client.request(operation).unwrap() else {
        panic!("commit reply expected");
    };
    assert_eq!(output.len(), 3 * 1024 * 1024);
    assert!(matches!(
        client.request(status()).unwrap(),
        Reply::Status { .. }
    ));
    client.close().unwrap();
}

#[test]
fn unconfirmed_close_is_bounded_and_retires_session() {
    let fixture = Fixture::new(Behavior {
        ignore_eof: true,
        ..Default::default()
    });
    let mut client = fixture
        .connect_with(GitTimeouts {
            start: Duration::from_secs(3),
            request: Duration::from_secs(1),
            close: Duration::from_millis(100),
        })
        .unwrap();
    let session = client.transport.as_ref().unwrap().session.clone();
    let before = Instant::now();
    let error = client.close().unwrap_err();
    assert!(before.elapsed() < Duration::from_secs(2));
    assert!(error.contains("shutdown timed out"), "{error}");
    assert!(error.contains("session retired"), "{error}");
    assert!(session.is_unhealthy_now());
    assert!(client.transport.is_none());
    fixture.wait_count(&fixture.counts.closed, 1);
}
