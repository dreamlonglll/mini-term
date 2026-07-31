//! daemon —— mt-ssh-cli 的守护进程服务端(全机单例,持全局 SshPool)。
//!
//! 进程模型(spec §2):全局单例、不按 project 拆分。池按 `connection_id` 缓存
//! session 与 project 无关;project 范围过滤是**请求级**参数 —— 每个请求处理时
//! 重新读 config.json 并按该请求的 `projectId` 过滤,天然保证「主程序里改关联
//! 范围即时生效」的既有承诺。
//!
//! 生命周期:IPC 端点绑定天然互斥(抢输实例静默退出);空闲(无活跃请求且
//! `idle_exit` 内无新连接)→ drain 池 → 返回;收到 shutdown op → 回 ack →
//! drain 池 → 返回。**本模块只返回不 exit** —— `std::process::exit` 由 bin 调,
//! 进程内集成测试才能安全驱动完整生命周期。
//!
//! 每个连接一问一答:daemon 先发 hello(版本握手用),读一行请求,流式回帧至
//! 终帧。CLI 中途断开 → 写帧失败 → 丢弃 exec future(russh channel 随之关闭),
//! session 留池、daemon 不退出。

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::sync::Notify;

use crate::ipc::{self, Op, Request, ServerFrame};
use crate::ssh_service::{self, StreamKind, TransferDirection};
use mt_ssh::pool::SshPool;

/// 空闲自退窗口:无活跃请求且这么久没有新连接 → drain 池退出(spec §2)。
pub const DEFAULT_IDLE_EXIT: Duration = Duration::from_secs(10 * 60);

/// 等待客户端发来请求行的上限。防御连上不说话的客户端把 `active` 卡住,
/// 导致空闲自退永不触发。
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// serve 的退出原因。
#[derive(Debug, PartialEq, Eq)]
pub enum ServeOutcome {
    /// 空闲窗口内无活动,已 drain 池。
    Idle,
    /// 收到 shutdown op(daemon-stop / 版本换代),已 drain 池。
    Shutdown,
}

/// daemon 运行期共享状态。
struct DaemonState {
    pool: Arc<SshPool>,
    /// 活跃连接数(连接即请求:一问一答)。
    active: AtomicUsize,
    /// 最近一次活动(新连接建立 / 请求处理完)的 UNIX 毫秒。
    last_activity_ms: AtomicU64,
    /// shutdown op 的触发信号。
    shutdown: Notify,
}

impl DaemonState {
    fn touch(&self) {
        self.last_activity_ms.store(now_millis(), Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        let last = self.last_activity_ms.load(Ordering::Relaxed);
        Duration::from_millis(now_millis().saturating_sub(last))
    }
}

fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 活跃连接计数的 RAII guard:drop 时递减并刷新活动时间。
struct ActiveGuard(Arc<DaemonState>);

impl ActiveGuard {
    fn new(state: Arc<DaemonState>) -> Self {
        state.active.fetch_add(1, Ordering::SeqCst);
        state.touch();
        Self(state)
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

/// 绑定端点并服务直至空闲/收到 shutdown。返回前已 drain 池。
///
/// 绑定失败(端点已被别的 daemon 持有)→ `Err`,caller 应静默退出 ——
/// 并发自拉起的竞态由端点绑定互斥收敛。
pub async fn serve(endpoint: &str, idle_exit: Duration) -> Result<ServeOutcome, String> {
    let state = Arc::new(DaemonState {
        pool: Arc::new(SshPool::new()),
        active: AtomicUsize::new(0),
        last_activity_ms: AtomicU64::new(now_millis()),
        shutdown: Notify::new(),
    });

    eprintln!(
        "[mt-ssh-cli daemon] v{} pid={} listening on {endpoint}",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );

    let outcome = serve_until_exit(endpoint, idle_exit, &state).await?;

    // drain 池:逐 session disconnect(ByApplication),远端不留 dangling。
    eprintln!("[mt-ssh-cli daemon] draining session pool ({outcome:?})");
    state.pool.shutdown().await;
    Ok(outcome)
}

/// 平台特定的 accept 循环 + 空闲计时 + shutdown 信号,三路 select。
#[cfg(windows)]
async fn serve_until_exit(
    endpoint: &str,
    idle_exit: Duration,
    state: &Arc<DaemonState>,
) -> Result<ServeOutcome, String> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let security = ipc::windows_security::PipeSecurity::current_user_only()?;

    // 首实例带 first_pipe_instance:同名 pipe 已被持有(另一个 daemon 赢了)
    // 会直接失败 —— 这是并发自拉起竞态的收敛点。
    // SAFETY: security 在本函数存活期间持有,attributes_ptr 指向的内存有效。
    let mut server = unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .create_with_security_attributes_raw(endpoint, security.attributes_ptr())
    }
    .map_err(|e| format!("endpoint already held or unavailable: {e}"))?;

    let mut idle_tick = tokio::time::interval(idle_check_tick(idle_exit));
    idle_tick.tick().await; // 首个 tick 立即返回,跳过

    loop {
        tokio::select! {
            connected = server.connect() => {
                connected.map_err(|e| format!("pipe accept failed: {e}"))?;
                // 先补一个新实例再处理当前连接,保证任何时刻都有实例在监听。
                // SAFETY: 同上。
                let next = unsafe {
                    ServerOptions::new()
                        .create_with_security_attributes_raw(endpoint, security.attributes_ptr())
                }
                .map_err(|e| format!("pipe re-create failed: {e}"))?;
                let client = std::mem::replace(&mut server, next);
                let st = state.clone();
                tokio::spawn(async move { handle_connection(client, st).await });
            }
            _ = idle_tick.tick() => {
                if state.active.load(Ordering::SeqCst) == 0 && state.idle_for() >= idle_exit {
                    return Ok(ServeOutcome::Idle);
                }
            }
            _ = state.shutdown.notified() => {
                return Ok(ServeOutcome::Shutdown);
            }
        }
    }
}

#[cfg(unix)]
async fn serve_until_exit(
    endpoint: &str,
    idle_exit: Duration,
    state: &Arc<DaemonState>,
) -> Result<ServeOutcome, String> {
    use tokio::net::{UnixListener, UnixStream};

    let path = std::path::Path::new(endpoint);
    // 端点文件已存在:能连上 → 另一个 daemon 在跑,让位;连不上 → 陈旧残留,清掉。
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            return Err("endpoint already held by a live daemon".into());
        }
        let _ = std::fs::remove_file(path);
    }
    let listener =
        UnixListener::bind(path).map_err(|e| format!("endpoint bind failed: {e}"))?;
    // 权限 0600:仅当前用户可连。
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }

    let mut idle_tick = tokio::time::interval(idle_check_tick(idle_exit));
    idle_tick.tick().await;

    let outcome = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|e| format!("socket accept failed: {e}"))?;
                let st = state.clone();
                tokio::spawn(async move { handle_connection(stream, st).await });
            }
            _ = idle_tick.tick() => {
                if state.active.load(Ordering::SeqCst) == 0 && state.idle_for() >= idle_exit {
                    break ServeOutcome::Idle;
                }
            }
            _ = state.shutdown.notified() => {
                break ServeOutcome::Shutdown;
            }
        }
    };
    let _ = std::fs::remove_file(path);
    Ok(outcome)
}

/// 空闲检查频率:跟随窗口收缩(测试用短窗口也能及时触发),上限 30s。
fn idle_check_tick(idle_exit: Duration) -> Duration {
    (idle_exit / 4).clamp(Duration::from_millis(20), Duration::from_secs(30))
}

/// 处理一个客户端连接:hello → 读一行请求 → 按 op 分发 → 终帧。
async fn handle_connection<S>(stream: S, state: Arc<DaemonState>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let _guard = ActiveGuard::new(state.clone());
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    // 1. hello 帧:版本 + pid,供 CLI 做版本握手与 daemon-status。
    let hello = ServerFrame::Hello {
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
    };
    if write_frame(&mut writer, &hello).await.is_err() {
        return;
    }

    // 2. 读请求(单行,带超时防呆连接)。
    let mut line = String::new();
    let read = tokio::time::timeout(REQUEST_READ_TIMEOUT, reader.read_line(&mut line)).await;
    match read {
        Ok(Ok(n)) if n > 0 => {}
        // 客户端只探不问(如版本探测后直接断开)/ 超时 → 静默收尾。
        _ => return,
    }
    let req: Request = match ipc::decode_frame(&line) {
        Ok(r) => r,
        Err(e) => {
            let _ = write_frame(&mut writer, &ServerFrame::Error { message: e }).await;
            return;
        }
    };
    if req.v != ipc::PROTOCOL_VERSION {
        let msg = format!(
            "protocol version mismatch: daemon speaks v{}, request is v{}",
            ipc::PROTOCOL_VERSION,
            req.v
        );
        let _ = write_frame(&mut writer, &ServerFrame::Error { message: msg }).await;
        return;
    }

    // 3. 按 op 分发。每个请求都以 config.json 的当下内容为准(请求级过滤)。
    let Some(op) = req.op else {
        let _ = write_frame(
            &mut writer,
            &ServerFrame::Error {
                message: "missing op".into(),
            },
        )
        .await;
        return;
    };
    match op {
        Op::List => {
            let views = ssh_service::list_connections(req.project_id.as_deref());
            let _ = write_frame(
                &mut writer,
                &ServerFrame::Result {
                    exit_code: None,
                    timed_out: None,
                    bytes: None,
                    connections: Some(views),
                    sessions: None,
                },
            )
            .await;
        }
        Op::Status => {
            let sessions = state.pool.len().await;
            let _ = write_frame(
                &mut writer,
                &ServerFrame::Result {
                    exit_code: None,
                    timed_out: None,
                    bytes: None,
                    connections: None,
                    sessions: Some(sessions),
                },
            )
            .await;
        }
        Op::Shutdown => {
            // 先 ack 再触发退出:CLI 拿得到确认,drain 由 serve 统一做。
            let _ = write_frame(
                &mut writer,
                &ServerFrame::Result {
                    exit_code: None,
                    timed_out: None,
                    bytes: None,
                    connections: None,
                    sessions: None,
                },
            )
            .await;
            state.shutdown.notify_one();
        }
        Op::Exec => {
            handle_exec(&mut writer, req, &state).await;
        }
        Op::Upload | Op::Download => {
            let direction = if op == Op::Upload {
                TransferDirection::Upload
            } else {
                TransferDirection::Download
            };
            handle_transfer(&mut writer, req, direction, &state).await;
        }
    }
}

/// exec:service 编排的输出经 mpsc 转成 stdout/stderr 帧实时写回。
///
/// 写帧失败(CLI 断开)→ 丢弃 exec future:russh channel 随 drop 关闭,
/// session 留池;请求级超时在 service 层强制,不依赖 CLI 存活。
async fn handle_exec<W>(writer: &mut W, req: Request, state: &Arc<DaemonState>)
where
    W: AsyncWrite + Unpin,
{
    let (connection, command) = match (req.connection.as_deref(), req.command.as_deref()) {
        (Some(c), Some(cmd)) => (c, cmd),
        _ => {
            let _ = write_frame(
                writer,
                &ServerFrame::Error {
                    message: "exec requires `connection` and `command`".into(),
                },
            )
            .await;
            return;
        }
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<ServerFrame>();
    let exec_fut = ssh_service::exec(
        &state.pool,
        req.project_id.as_deref(),
        connection,
        command,
        req.cwd.as_deref(),
        req.timeout_secs,
        move |kind, data| {
            let frame = match kind {
                StreamKind::Stdout => ServerFrame::Stdout {
                    data_b64: ipc::b64_encode(data),
                },
                StreamKind::Stderr => ServerFrame::Stderr {
                    data_b64: ipc::b64_encode(data),
                },
            };
            // 接收端满/关闭都不阻塞 exec —— 写侧失败在下方统一处理。
            let _ = tx.send(frame);
        },
    );
    tokio::pin!(exec_fut);

    let result = loop {
        tokio::select! {
            res = &mut exec_fut => break res,
            maybe_frame = rx.recv() => {
                let Some(frame) = maybe_frame else { continue };
                if write_frame(writer, &frame).await.is_err() {
                    // CLI 断开:丢弃 exec future(russh channel 关闭),session 留池。
                    eprintln!("[mt-ssh-cli daemon] client disconnected mid-exec, closing channel");
                    return;
                }
            }
        }
    };

    // exec 完成:回调帧都已同步入队,先清空积压再发终帧。
    while let Ok(frame) = rx.try_recv() {
        if write_frame(writer, &frame).await.is_err() {
            return;
        }
    }
    let terminal = match result {
        Ok(outcome) => ServerFrame::Result {
            exit_code: outcome.exit_code,
            timed_out: Some(outcome.timed_out),
            bytes: None,
            connections: None,
            sessions: None,
        },
        Err(e) => ServerFrame::Error {
            message: e.message().to_string(),
        },
    };
    let _ = write_frame(writer, &terminal).await;
}

/// upload / download:service 编排 → bytes 终帧。
async fn handle_transfer<W>(
    writer: &mut W,
    req: Request,
    direction: TransferDirection,
    state: &Arc<DaemonState>,
) where
    W: AsyncWrite + Unpin,
{
    let (connection, local, remote) = match (
        req.connection.as_deref(),
        req.local_path.as_deref(),
        req.remote_path.as_deref(),
    ) {
        (Some(c), Some(l), Some(r)) => (c, l, r),
        _ => {
            let _ = write_frame(
                writer,
                &ServerFrame::Error {
                    message: "transfer requires `connection`, `localPath` and `remotePath`".into(),
                },
            )
            .await;
            return;
        }
    };

    let terminal = match ssh_service::transfer(
        &state.pool,
        direction,
        req.project_id.as_deref(),
        connection,
        local,
        remote,
        req.timeout_secs,
    )
    .await
    {
        Ok(bytes) => ServerFrame::Result {
            exit_code: None,
            timed_out: None,
            bytes: Some(bytes),
            connections: None,
            sessions: None,
        },
        Err(e) => ServerFrame::Error {
            message: e.message().to_string(),
        },
    };
    let _ = write_frame(writer, &terminal).await;
}

/// 写一帧(单行 JSON)并 flush。
async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &ServerFrame,
) -> Result<(), ()> {
    let line = ipc::encode_frame(frame).map_err(|_| ())?;
    writer.write_all(line.as_bytes()).await.map_err(|_| ())?;
    writer.flush().await.map_err(|_| ())
}

// ============================================================================
// tests —— 进程内起真实端点驱动完整生命周期(spec §8 daemon 集成测试)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    /// 每个测试独立端点,避免并行互撞。
    fn test_endpoint(label: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        #[cfg(windows)]
        {
            format!(r"\\.\pipe\mt-ssh-cli-test-{label}-{}-{nanos}", std::process::id())
        }
        #[cfg(unix)]
        {
            std::env::temp_dir()
                .join(format!("mt-cli-test-{label}-{}-{nanos}.sock", std::process::id()))
                .to_string_lossy()
                .to_string()
        }
    }

    /// 连上端点,读掉 hello 帧,返回 (reader, writer, hello)。
    async fn connect_and_hello(
        endpoint: &str,
    ) -> (
        BufReader<tokio::io::ReadHalf<Box<dyn ipc::IpcStream>>>,
        tokio::io::WriteHalf<Box<dyn ipc::IpcStream>>,
        ServerFrame,
    ) {
        let stream = ipc::connect(endpoint).await.expect("connect");
        let (r, w) = tokio::io::split(stream);
        let mut reader = BufReader::new(r);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("hello");
        let hello: ServerFrame = ipc::decode_frame(&line).expect("hello frame");
        (reader, w, hello)
    }

    async fn send_request(
        writer: &mut tokio::io::WriteHalf<Box<dyn ipc::IpcStream>>,
        req: &Request,
    ) {
        let line = ipc::encode_frame(req).unwrap();
        writer.write_all(line.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();
    }

    async fn read_frame(
        reader: &mut BufReader<tokio::io::ReadHalf<Box<dyn ipc::IpcStream>>>,
    ) -> ServerFrame {
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("frame line");
        ipc::decode_frame(&line).expect("server frame")
    }

    /// 起 daemon 任务,等端点可连。
    async fn spawn_daemon(
        endpoint: String,
        idle_exit: Duration,
    ) -> tokio::task::JoinHandle<Result<ServeOutcome, String>> {
        let handle = tokio::spawn(async move { serve(&endpoint, idle_exit).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle
    }

    #[tokio::test]
    async fn daemon_sends_hello_and_answers_status() {
        let ep = test_endpoint("status");
        let daemon = spawn_daemon(ep.clone(), Duration::from_secs(60)).await;

        let (mut reader, mut writer, hello) = connect_and_hello(&ep).await;
        match hello {
            ServerFrame::Hello { version, pid } => {
                assert_eq!(version, env!("CARGO_PKG_VERSION"));
                assert_eq!(pid, std::process::id());
            }
            other => panic!("expected hello, got {other:?}"),
        }

        send_request(
            &mut writer,
            &Request {
                v: ipc::PROTOCOL_VERSION,
                op: Some(Op::Status),
                ..Default::default()
            },
        )
        .await;
        match read_frame(&mut reader).await {
            ServerFrame::Result { sessions, .. } => assert_eq!(sessions, Some(0)),
            other => panic!("expected result, got {other:?}"),
        }
        daemon.abort();
    }

    #[tokio::test]
    async fn daemon_list_answers_connections_field() {
        let ep = test_endpoint("list");
        let daemon = spawn_daemon(ep.clone(), Duration::from_secs(60)).await;

        let (mut reader, mut writer, _) = connect_and_hello(&ep).await;
        send_request(
            &mut writer,
            &Request {
                v: ipc::PROTOCOL_VERSION,
                op: Some(Op::List),
                // 不存在的项目 id → 默认全部可见(与 MCP 语义一致);此测试只
                // 断言字段形状,不断言条数(取决于本机 config.json)。
                ..Default::default()
            },
        )
        .await;
        match read_frame(&mut reader).await {
            ServerFrame::Result { connections, .. } => assert!(connections.is_some()),
            other => panic!("expected result, got {other:?}"),
        }
        daemon.abort();
    }

    #[tokio::test]
    async fn daemon_exec_unknown_connection_yields_error_frame() {
        let ep = test_endpoint("exec-err");
        let daemon = spawn_daemon(ep.clone(), Duration::from_secs(60)).await;

        let (mut reader, mut writer, _) = connect_and_hello(&ep).await;
        send_request(
            &mut writer,
            &Request {
                v: ipc::PROTOCOL_VERSION,
                op: Some(Op::Exec),
                // 用一个显式空 scope 的假项目..仍可能全部可见;换成绝无可能存在的连接名
                connection: Some("mt-test-definitely-no-such-connection".into()),
                command: Some("true".into()),
                ..Default::default()
            },
        )
        .await;
        match read_frame(&mut reader).await {
            ServerFrame::Error { message } => {
                assert!(message.contains("No SSH connection found"), "got: {message}");
                assert!(!message.to_lowercase().contains("password"));
            }
            other => panic!("expected error, got {other:?}"),
        }
        daemon.abort();
    }

    #[tokio::test]
    async fn daemon_rejects_protocol_version_mismatch() {
        let ep = test_endpoint("ver");
        let daemon = spawn_daemon(ep.clone(), Duration::from_secs(60)).await;

        let (mut reader, mut writer, _) = connect_and_hello(&ep).await;
        send_request(
            &mut writer,
            &Request {
                v: 999,
                op: Some(Op::Status),
                ..Default::default()
            },
        )
        .await;
        match read_frame(&mut reader).await {
            ServerFrame::Error { message } => {
                assert!(message.contains("protocol version mismatch"), "got: {message}")
            }
            other => panic!("expected error, got {other:?}"),
        }
        daemon.abort();
    }

    #[tokio::test]
    async fn daemon_malformed_request_yields_error_frame() {
        let ep = test_endpoint("malformed");
        let daemon = spawn_daemon(ep.clone(), Duration::from_secs(60)).await;

        let (mut reader, mut writer, _) = connect_and_hello(&ep).await;
        writer.write_all(b"this is not json\n").await.unwrap();
        writer.flush().await.unwrap();
        match read_frame(&mut reader).await {
            ServerFrame::Error { message } => {
                assert!(message.contains("decode frame failed"), "got: {message}")
            }
            other => panic!("expected error, got {other:?}"),
        }
        daemon.abort();
    }

    #[tokio::test]
    async fn daemon_endpoint_binding_is_mutually_exclusive() {
        let ep = test_endpoint("mutex");
        let daemon = spawn_daemon(ep.clone(), Duration::from_secs(60)).await;

        // 第二个 daemon 起在同一端点 → 必须立即失败(抢输方静默退出的依据)。
        let second = serve(&ep, Duration::from_secs(60)).await;
        assert!(second.is_err(), "second daemon must fail to bind");

        // 原 daemon 仍健在可服务。
        let (mut reader, mut writer, _) = connect_and_hello(&ep).await;
        send_request(
            &mut writer,
            &Request {
                v: ipc::PROTOCOL_VERSION,
                op: Some(Op::Status),
                ..Default::default()
            },
        )
        .await;
        assert!(matches!(
            read_frame(&mut reader).await,
            ServerFrame::Result { .. }
        ));
        daemon.abort();
    }

    #[tokio::test]
    async fn daemon_shutdown_op_acks_then_serve_returns() {
        let ep = test_endpoint("shutdown");
        let daemon = spawn_daemon(ep.clone(), Duration::from_secs(60)).await;

        let (mut reader, mut writer, _) = connect_and_hello(&ep).await;
        send_request(
            &mut writer,
            &Request {
                v: ipc::PROTOCOL_VERSION,
                op: Some(Op::Shutdown),
                ..Default::default()
            },
        )
        .await;
        // 先收到 ack,再看 serve 以 Shutdown 结束。
        assert!(matches!(
            read_frame(&mut reader).await,
            ServerFrame::Result { .. }
        ));
        let outcome = tokio::time::timeout(Duration::from_secs(5), daemon)
            .await
            .expect("serve should return after shutdown")
            .unwrap()
            .unwrap();
        assert_eq!(outcome, ServeOutcome::Shutdown);
    }

    #[tokio::test]
    async fn daemon_idle_exit_after_quiet_window() {
        let ep = test_endpoint("idle");
        // 极短空闲窗口:200ms 无活动即退出。
        let daemon = spawn_daemon(ep.clone(), Duration::from_millis(200)).await;

        let outcome = tokio::time::timeout(Duration::from_secs(5), daemon)
            .await
            .expect("serve should return after idle window")
            .unwrap()
            .unwrap();
        assert_eq!(outcome, ServeOutcome::Idle);

        // 端点已释放:新 daemon 可立即重新绑定(陈旧 socket/句柄不残留)。
        let rebind = spawn_daemon(ep.clone(), Duration::from_secs(60)).await;
        let (_, _, hello) = connect_and_hello(&ep).await;
        assert!(matches!(hello, ServerFrame::Hello { .. }));
        rebind.abort();
    }

    #[tokio::test]
    async fn daemon_concurrent_connections_do_not_cross_streams() {
        // 两个并发连接各自问 status,各自拿到自己的完整帧序列(hello + result),
        // 帧不会串到别人连接上(每连接独立 writer)。
        let ep = test_endpoint("concurrent");
        let daemon = spawn_daemon(ep.clone(), Duration::from_secs(60)).await;

        let mut tasks = Vec::new();
        for _ in 0..2 {
            let ep = ep.clone();
            tasks.push(tokio::spawn(async move {
                let (mut reader, mut writer, hello) = connect_and_hello(&ep).await;
                assert!(matches!(hello, ServerFrame::Hello { .. }));
                send_request(
                    &mut writer,
                    &Request {
                        v: ipc::PROTOCOL_VERSION,
                        op: Some(Op::Status),
                        ..Default::default()
                    },
                )
                .await;
                matches!(read_frame(&mut reader).await, ServerFrame::Result { .. })
            }));
        }
        for t in tasks {
            assert!(t.await.unwrap());
        }
        daemon.abort();
    }

    #[tokio::test]
    async fn daemon_survives_client_disconnect_without_request() {
        // 连上就断(版本探测场景):daemon 不退出、后续连接照常服务。
        let ep = test_endpoint("probe");
        let daemon = spawn_daemon(ep.clone(), Duration::from_secs(60)).await;

        {
            let _probe = ipc::connect(&ep).await.expect("probe connect");
            // drop 即断开
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        let (mut reader, mut writer, _) = connect_and_hello(&ep).await;
        send_request(
            &mut writer,
            &Request {
                v: ipc::PROTOCOL_VERSION,
                op: Some(Op::Status),
                ..Default::default()
            },
        )
        .await;
        assert!(matches!(
            read_frame(&mut reader).await,
            ServerFrame::Result { .. }
        ));
        daemon.abort();
    }
}
