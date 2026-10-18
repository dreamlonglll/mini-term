//! exec 编排对着进程内 russh 假服务器的端到端用例。
//!
//! 假服务器绑 127.0.0.1 的临时端口,只认用户 `tester` / 密码 `pw`(明文密码走
//! `mt_secret::reveal_global` 的原样放行分支,不碰任何密钥文件);主机密钥由固定种子生成。
//! 客户端池用 `SshPool::with_paths` + 临时 known_hosts —— 不读 `~/.ssh`、不连 ssh-agent
//! (B1 之后 `with_paths` 明确是隔离构造);编排核心 `exec_resolved` 不读配置、不写审计,
//! 所以整组用例不碰真实数据目录,也不连任何真实服务器。
//!
//! 假服务器认的「命令」(exec 请求原文):
//! - `echo:<文本>`:输出文本,exit 0;`exit:<n>`:exit n;
//! - `cat`:等到客户端的 EOF 才输出 `got-eof`、exit 0 —— 客户端不发 EOF 就挂到超时;
//! - `kill` / `segv` / `usr2`:以 KILL / SEGV(core dumped)/ 自定义名 `USR2` 信号终止;
//! - `reject`:回 Failure(拒绝 exec);`hang`:接受后永不结束;
//! - `rendezvous`:等同一台服务器上另一个 `rendezvous` 到达(最多 3s)再一起结束,输出
//!   `together`;等不到输出 `alone` —— 用来证明同一 session 上的两个 exec 真的并发。
//!
//! 服务器开关 `hang_channel_open_on_first`:前 N 条 TCP 连接上的 channel-open 永不确认,
//! 模拟「网络切换后 session 已死、开 channel 卡住」。

use super::*;

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use mt_ssh::pool::PoolConfig;
use mt_ssh::russh::{self, server, ChannelId};
use tokio::net::TcpListener;
use tokio::sync::Notify;

/// 假服务器的共享状态(跨连接)。
#[derive(Default)]
struct FakeServerState {
    /// 已接受的 TCP 连接数。
    connections: AtomicUsize,
    /// 前多少条连接上的 channel-open 永不确认。
    hang_channel_open_on_first: AtomicUsize,
    /// `rendezvous` 已到达的数量与唤醒器。
    rendezvous_arrived: AtomicUsize,
    rendezvous: Notify,
}

impl FakeServerState {
    /// 等同伴:第二个到达时唤醒第一个。返回是否等到了同伴(最多等 3s)。
    async fn rendezvous(&self) -> bool {
        let notified = self.rendezvous.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.rendezvous_arrived.fetch_add(1, Ordering::SeqCst) + 1 >= 2 {
            self.rendezvous.notify_waiters();
            return true;
        }
        tokio::time::timeout(Duration::from_secs(3), notified)
            .await
            .is_ok()
    }
}

struct FakeHandler {
    state: Arc<FakeServerState>,
    hang_channel_open: bool,
    /// `cat`:等这些 channel 的 EOF 再回话。
    waiting_for_eof: HashSet<ChannelId>,
}

impl server::Handler for FakeHandler {
    type Error = russh::Error;

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        Ok(if user == "tester" && password == "pw" {
            server::Auth::Accept
        } else {
            server::Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        _channel: russh::Channel<server::Msg>,
        _session: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        if self.hang_channel_open {
            // 卡住整条 session 的消息循环:客户端的 channel-open 永远等不到确认。
            std::future::pending::<()>().await;
        }
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        match command.as_str() {
            "reject" => session.channel_failure(channel)?,
            "cat" => {
                session.channel_success(channel)?;
                self.waiting_for_eof.insert(channel);
            }
            "hang" => session.channel_success(channel)?,
            _ => {
                session.channel_success(channel)?;
                // 放到独立任务里回话:不堵 session 的消息循环,同一连接上的
                // 其它 channel 才能并发推进。
                let handle = session.handle();
                let state = self.state.clone();
                tokio::spawn(async move { respond(handle, channel, &command, &state).await });
            }
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if self.waiting_for_eof.remove(&channel) {
            session.data(channel, b"got-eof".to_vec())?;
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
        }
        Ok(())
    }
}

/// 执行一条假命令并收尾(EOF + close)。
async fn respond(
    handle: server::Handle,
    channel: ChannelId,
    command: &str,
    state: &FakeServerState,
) {
    if let Some(text) = command.strip_prefix("echo:") {
        let _ = handle.data(channel, text.as_bytes().to_vec()).await;
        let _ = handle.exit_status_request(channel, 0).await;
    } else if let Some(code) = command.strip_prefix("exit:") {
        let _ = handle
            .exit_status_request(channel, code.parse().unwrap_or(1))
            .await;
    } else if command == "kill" {
        let _ = handle
            .exit_signal_request(
                channel,
                russh::Sig::KILL,
                false,
                String::new(),
                String::new(),
            )
            .await;
    } else if command == "segv" {
        let _ = handle
            .exit_signal_request(
                channel,
                russh::Sig::SEGV,
                true,
                "segmentation fault".into(),
                String::new(),
            )
            .await;
    } else if command == "usr2" {
        let _ = handle
            .exit_signal_request(
                channel,
                russh::Sig::Custom("USR2".into()),
                false,
                String::new(),
                String::new(),
            )
            .await;
    } else if command == "rendezvous" {
        let reply: &[u8] = if state.rendezvous().await {
            b"together"
        } else {
            b"alone"
        };
        let _ = handle.data(channel, reply.to_vec()).await;
        let _ = handle.exit_status_request(channel, 0).await;
    } else {
        let message = format!("unknown fake command: {command}");
        let _ = handle.extended_data(channel, 1, message.into_bytes()).await;
        let _ = handle.exit_status_request(channel, 127).await;
    }
    let _ = handle.eof(channel).await;
    let _ = handle.close(channel).await;
}

/// 固定种子的主机密钥(不需要随机数源)。
fn host_key() -> russh::keys::PrivateKey {
    let keypair = russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[7u8; 32]);
    russh::keys::PrivateKey::from(keypair)
}

/// 起一台假服务器,返回端口。
async fn start_fake_server(state: Arc<FakeServerState>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = Arc::new(server::Config {
        keys: vec![host_key()],
        auth_rejection_time: Duration::from_millis(10),
        auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: Some(Duration::from_secs(60)),
        ..Default::default()
    });
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let index = state.connections.fetch_add(1, Ordering::SeqCst);
            let handler = FakeHandler {
                state: state.clone(),
                hang_channel_open: index < state.hang_channel_open_on_first.load(Ordering::SeqCst),
                waiting_for_eof: HashSet::new(),
            };
            // run_stream 要先交换版本串才返回,放进独立任务,不堵 accept 循环;它自己
            // spawn 了 session 循环,返回的句柄丢掉不影响 session 继续跑。
            let config = config.clone();
            tokio::spawn(async move {
                let _ = server::run_stream(config, stream, handler).await;
            });
        }
    });
    port
}

/// 只收 TCP、一个字节都不说的「服务器」:客户端卡在等 SSH banner。返回端口。
async fn start_silent_listener() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    port
}

/// 每个用例一个临时目录(放 known_hosts)。
fn temp_dir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "mt-ssh-svc-fake-{label}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 隔离的客户端池:显式 known_hosts,不碰 `~/.ssh`。
fn fake_pool(dir: &Path) -> SshPool {
    SshPool::with_paths(PoolConfig::default(), dir.join("known_hosts"))
}

fn fake_conn(port: u16) -> mt_core::SshConnection {
    mt_core::SshConnection {
        id: format!("fake-{port}"),
        name: "fake".into(),
        host: "127.0.0.1".into(),
        port,
        user: "tester".into(),
        password: Some("pw".into()),
        identity_file: None,
        group: None,
        extra: Default::default(),
    }
}

/// 一次编排的结果 + 收到的输出。
struct Captured {
    run: ExecRun,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn run_exec(
    pool: &SshPool,
    conn: &mt_core::SshConnection,
    command: &str,
    timeout: Duration,
    channel_open_timeout: Duration,
    cancellation: &ExecCancellation,
) -> Captured {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let run = {
        let mut on_output = |kind: StreamKind, data: &[u8]| match kind {
            StreamKind::Stdout => stdout.extend_from_slice(data),
            StreamKind::Stderr => stderr.extend_from_slice(data),
        };
        exec_resolved(
            pool,
            conn,
            command,
            tokio::time::Instant::now() + timeout,
            channel_open_timeout,
            cancellation,
            &mut on_output,
        )
        .await
    };
    Captured {
        run,
        stdout,
        stderr,
    }
}

/// 常用参数下跑一条命令:时限 10s、开 channel 短超时 10s、不取消。
async fn run_simple(pool: &SshPool, conn: &mt_core::SshConnection, command: &str) -> Captured {
    run_exec(
        pool,
        conn,
        command,
        Duration::from_secs(10),
        CHANNEL_OPEN_TIMEOUT,
        &ExecCancellation::new(),
    )
    .await
}

fn outcome_of(captured: &Captured) -> &ExecOutcome {
    match &captured.run.result {
        Ok(outcome) => outcome,
        Err(e) => panic!(
            "expected an outcome, got error: {e:?} (stderr: {})",
            String::from_utf8_lossy(&captured.stderr)
        ),
    }
}

#[tokio::test]
async fn exec_streams_output_and_passes_exit_status_through() {
    let dir = temp_dir("echo");
    let port = start_fake_server(Arc::default()).await;
    let pool = fake_pool(&dir);
    let conn = fake_conn(port);

    let echoed = run_simple(&pool, &conn, "echo:hello").await;
    assert_eq!(outcome_of(&echoed).exit_code, Some(0));
    assert_eq!(echoed.stdout, b"hello");
    assert_eq!(echoed.run.audit, Some(AuditExit::Code(0)));

    let failed = run_simple(&pool, &conn, "exit:7").await;
    assert_eq!(outcome_of(&failed).exit_code, Some(7));
    assert_eq!(failed.run.audit, Some(AuditExit::Code(7)));

    pool.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A2-3:exec 发出后立刻给远端 EOF,读 stdin 的命令马上结束,不挂到超时。
#[tokio::test]
async fn exec_sends_eof_so_stdin_readers_finish_at_once() {
    let dir = temp_dir("eof");
    let port = start_fake_server(Arc::default()).await;
    let pool = fake_pool(&dir);
    let conn = fake_conn(port);

    let started = std::time::Instant::now();
    let captured = run_simple(&pool, &conn, "cat").await;
    assert_eq!(outcome_of(&captured).exit_code, Some(0));
    assert!(!outcome_of(&captured).timed_out);
    assert_eq!(captured.stdout, b"got-eof");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );

    pool.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A2-4:exit-signal 映射成 128+n,信号信息带回给 transport 打说明行。
#[tokio::test]
async fn exit_signals_map_to_128_plus_signal_number() {
    let dir = temp_dir("signal");
    let port = start_fake_server(Arc::default()).await;
    let pool = fake_pool(&dir);
    let conn = fake_conn(port);

    let killed = run_simple(&pool, &conn, "kill").await;
    let outcome = outcome_of(&killed);
    assert_eq!(outcome.exit_code, Some(137));
    assert_eq!(
        outcome.signal.as_ref().map(|s| s.name.as_str()),
        Some("KILL")
    );
    assert_eq!(killed.run.audit, Some(AuditExit::Code(137)));
    assert_eq!(
        outcome.signal_note().as_deref(),
        Some("remote command terminated by signal SIGKILL; exit code 137")
    );

    let segv = run_simple(&pool, &conn, "segv").await;
    let outcome = outcome_of(&segv);
    assert_eq!(outcome.exit_code, Some(139));
    assert!(outcome.signal.as_ref().is_some_and(|s| s.core_dumped));
    assert!(
        outcome
            .signal_note()
            .unwrap()
            .contains("(core dumped): segmentation fault"),
        "{:?}",
        outcome.signal_note()
    );

    // russh 没有 USR2 的枚举值,走 Custom 名字也要认出来
    let usr2 = run_simple(&pool, &conn, "usr2").await;
    assert_eq!(outcome_of(&usr2).exit_code, Some(140));

    pool.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A2-4:服务端拒绝 exec → 立即报错,命令没跑,不记审计,session 照常留池。
#[tokio::test]
async fn rejected_exec_fails_fast_without_audit() {
    let dir = temp_dir("reject");
    let port = start_fake_server(Arc::default()).await;
    let pool = fake_pool(&dir);
    let conn = fake_conn(port);

    let started = std::time::Instant::now();
    let rejected = run_simple(&pool, &conn, "reject").await;
    match &rejected.run.result {
        Err(ServiceError::Internal(message)) => {
            assert!(message.contains("rejected"), "{message}");
            assert!(message.contains("not started"), "{message}");
        }
        other => panic!("expected a rejection error, got {other:?}"),
    }
    assert_eq!(rejected.run.audit, None);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(pool.len().await, 1, "拒绝 exec 不说明 session 坏了");

    pool.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A2-2:会话锁只在开 channel 时持有 —— 同一连接上的两个 exec 并发跑。
#[tokio::test]
async fn concurrent_execs_on_one_session_are_not_serialized() {
    let dir = temp_dir("concurrent");
    let state = Arc::new(FakeServerState::default());
    let port = start_fake_server(state.clone()).await;
    let pool = fake_pool(&dir);
    let conn = fake_conn(port);

    // 先建好 session,保证两个请求复用同一条
    assert_eq!(
        outcome_of(&run_simple(&pool, &conn, "echo:warm").await).exit_code,
        Some(0)
    );

    let (a, b) = tokio::join!(
        run_simple(&pool, &conn, "rendezvous"),
        run_simple(&pool, &conn, "rendezvous")
    );
    // 串行的话第一个会在服务端苦等 3s 后报 `alone`
    assert_eq!(a.stdout, b"together");
    assert_eq!(b.stdout, b"together");
    assert_eq!(pool.len().await, 1);
    assert_eq!(
        state.connections.load(Ordering::SeqCst),
        1,
        "全程只有一条连接"
    );

    pool.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A2-2:开 channel 短超时 → 精确淘汰这条 session,在剩余时限里换新连接重试成功。
#[tokio::test]
async fn channel_open_timeout_retires_the_session_and_retries() {
    let dir = temp_dir("open-timeout");
    let state = Arc::new(FakeServerState::default());
    state.hang_channel_open_on_first.store(1, Ordering::SeqCst);
    let port = start_fake_server(state.clone()).await;
    let pool = fake_pool(&dir);
    let conn = fake_conn(port);

    let started = std::time::Instant::now();
    let captured = run_exec(
        &pool,
        &conn,
        "echo:recovered",
        Duration::from_secs(10),
        Duration::from_millis(300),
        &ExecCancellation::new(),
    )
    .await;
    assert_eq!(outcome_of(&captured).exit_code, Some(0));
    assert_eq!(captured.stdout, b"recovered");
    assert_eq!(
        state.connections.load(Ordering::SeqCst),
        2,
        "换了一条新连接"
    );
    assert_eq!(pool.len().await, 1, "池里只剩新 session");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );

    pool.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 开 channel 连新连接也卡住:报错(exit 2 口径),命令没跑,不记审计。
#[tokio::test]
async fn channel_open_timeout_twice_is_an_error() {
    let dir = temp_dir("open-timeout-twice");
    let state = Arc::new(FakeServerState::default());
    state.hang_channel_open_on_first.store(2, Ordering::SeqCst);
    let port = start_fake_server(state.clone()).await;
    let pool = fake_pool(&dir);
    let conn = fake_conn(port);

    let captured = run_exec(
        &pool,
        &conn,
        "echo:never",
        Duration::from_secs(10),
        Duration::from_millis(300),
        &ExecCancellation::new(),
    )
    .await;
    match &captured.run.result {
        Err(ServiceError::Internal(message)) => {
            assert!(message.contains("did not open a channel"), "{message}");
        }
        other => panic!("expected an error, got {other:?}"),
    }
    assert_eq!(captured.run.audit, None);
    assert_eq!(state.connections.load(Ordering::SeqCst), 2);

    pool.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A2-1:服务器只收 TCP 不说话 —— 时限覆盖建连,到点报「连接阶段超时、命令没启动」。
#[tokio::test]
async fn deadline_during_connect_is_a_not_started_timeout() {
    let dir = temp_dir("connect-timeout");
    let port = start_silent_listener().await;
    let pool = fake_pool(&dir);
    let conn = fake_conn(port);

    let started = std::time::Instant::now();
    let captured = run_exec(
        &pool,
        &conn,
        "echo:never",
        Duration::from_millis(300),
        CHANNEL_OPEN_TIMEOUT,
        &ExecCancellation::new(),
    )
    .await;
    let outcome = outcome_of(&captured);
    assert!(outcome.timed_out);
    assert!(outcome.not_started);
    assert_eq!(outcome.exit_code, None);
    assert_eq!(captured.run.audit, None, "命令没跑,不记审计");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "took {:?}",
        started.elapsed()
    );

    pool.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A2-1:取消令牌立刻打断建连(daemon:客户端断开)。
#[tokio::test]
async fn cancellation_interrupts_connect() {
    let dir = temp_dir("connect-cancel");
    let port = start_silent_listener().await;
    let pool = fake_pool(&dir);
    let conn = fake_conn(port);

    let cancellation = ExecCancellation::new();
    let trigger = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let started = std::time::Instant::now();
    let captured = run_exec(
        &pool,
        &conn,
        "echo:never",
        Duration::from_secs(30),
        CHANNEL_OPEN_TIMEOUT,
        &cancellation,
    )
    .await;
    assert!(matches!(captured.run.result, Err(ServiceError::Cancelled)));
    assert_eq!(captured.run.audit, None);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "took {:?}",
        started.elapsed()
    );

    pool.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 命令已发出后跑到时限:普通超时(不是连接阶段),记审计,session 不淘汰。
#[tokio::test]
async fn deadline_while_running_is_a_started_timeout() {
    let dir = temp_dir("run-timeout");
    let port = start_fake_server(Arc::default()).await;
    let pool = fake_pool(&dir);
    let conn = fake_conn(port);

    // 先把 session 建好,时限只花在命令上
    assert_eq!(
        outcome_of(&run_simple(&pool, &conn, "echo:warm").await).exit_code,
        Some(0)
    );
    let captured = run_exec(
        &pool,
        &conn,
        "hang",
        Duration::from_millis(500),
        CHANNEL_OPEN_TIMEOUT,
        &ExecCancellation::new(),
    )
    .await;
    let outcome = outcome_of(&captured);
    assert!(outcome.timed_out);
    assert!(!outcome.not_started);
    assert_eq!(captured.run.audit, Some(AuditExit::Timeout));
    assert_eq!(pool.len().await, 1, "单个 channel 超时不代表 session 死了");

    // 同一条 session 还能接着用
    assert_eq!(
        outcome_of(&run_simple(&pool, &conn, "echo:still-alive").await).exit_code,
        Some(0)
    );

    pool.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// 命令跑到一半被取消:记一行 `cancelled` 审计(此前取消路径不记)。
#[tokio::test]
async fn cancellation_while_running_is_audited() {
    let dir = temp_dir("run-cancel");
    let port = start_fake_server(Arc::default()).await;
    let pool = fake_pool(&dir);
    let conn = fake_conn(port);

    // 先把 session 建好:取消一定落在命令运行期间,而不是建连期间
    assert_eq!(
        outcome_of(&run_simple(&pool, &conn, "echo:warm").await).exit_code,
        Some(0)
    );
    let cancellation = ExecCancellation::new();
    let trigger = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        trigger.cancel();
    });
    let captured = run_exec(
        &pool,
        &conn,
        "hang",
        Duration::from_secs(30),
        CHANNEL_OPEN_TIMEOUT,
        &cancellation,
    )
    .await;
    assert!(matches!(captured.run.result, Err(ServiceError::Cancelled)));
    assert_eq!(captured.run.audit, Some(AuditExit::Cancelled));

    pool.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}
