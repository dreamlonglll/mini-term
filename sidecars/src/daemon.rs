//! daemon —— mt-ssh-cli 的守护进程服务端(全机单例,持全局 SshPool)。
//!
//! 进程模型(spec §2):全局单例、不按 project 拆分。池按 `connection_id` 缓存
//! session 与 project 无关;project 范围过滤是**请求级**参数 —— 每个请求处理时
//! 重新读 config.json 并按该请求的 `projectToken` 解析,天然保证「主程序里改关联
//! 范围即时生效」的既有承诺。
//!
//! 生命周期:IPC 端点绑定天然互斥(抢输实例静默退出);空闲(无活跃请求且
//! `idle_exit` 内无新连接)→ drain 池 → 返回;收到 shutdown op → 回 ack →
//! drain 池 → 返回。**本模块只返回不 exit** —— `std::process::exit` 由 bin 调,
//! 进程内集成测试才能安全驱动完整生命周期。
//!
//! 每个连接一问一答:daemon 先发 hello(版本握手用),读一行请求,流式回帧至
//! 终帧。CLI 中途断开 → 触发状态型取消令牌 → service 显式发送 channel close，
//! 等关闭路径完成后结束请求；session 留池、daemon 不退出。

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::sync::Notify;

use crate::ipc::{self, Op, Request, ServerFrame};
use crate::ssh_service::{self, ConnectionScope, StreamKind, TransferDirection};
use mt_ssh::pool::SshPool;

/// 空闲自退窗口:无活跃请求且这么久没有新连接 → drain 池退出(spec §2)。
pub const DEFAULT_IDLE_EXIT: Duration = Duration::from_secs(10 * 60);

/// 等待客户端发来请求行的上限。防御连上不说话的客户端把 `active` 卡住,
/// 导致空闲自退永不触发。
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// 绑定失败后的活 daemon 探测上限。仅成功连接不够：同名普通 pipe/socket
/// 也能 accept，必须在有限时间内读到协议 hello 才能静默让位。
const DAEMON_HELLO_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// serve 的退出原因。
#[derive(Debug, PartialEq, Eq)]
pub enum ServeOutcome {
    /// 空闲窗口内无活动,已 drain 池。
    Idle,
    /// 收到 shutdown op(daemon-stop / 版本换代),已 drain 池。
    Shutdown,
}

/// serve 的失败分类:并发竞态的「让位」与运行期故障必须区分 ——
/// 前者是正常收敛(静默退出 0),后者是要暴露的错误。
#[derive(Debug)]
pub enum ServeError {
    /// 端点已被别的 daemon 持有(绑定失败),本实例应静默让位。
    AlreadyRunning(String),
    /// 非占用类绑定错误，或绑定成功后的运行期故障。
    Runtime(String),
}

impl ServeError {
    pub fn message(&self) -> &str {
        match self {
            ServeError::AlreadyRunning(m) | ServeError::Runtime(m) => m,
        }
    }
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

/// 验证已连接流的首帧确实是 daemon hello。
async fn stream_speaks_daemon_hello<S>(stream: S) -> bool
where
    S: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    matches!(
        tokio::time::timeout(DAEMON_HELLO_PROBE_TIMEOUT, reader.read_line(&mut line)).await,
        Ok(Ok(n))
            if n > 0
                && matches!(
                    ipc::decode_frame::<ServerFrame>(&line),
                    Ok(ServerFrame::Hello { .. })
                )
    )
}

async fn endpoint_has_live_daemon(endpoint: &str) -> bool {
    let deadline = tokio::time::Instant::now() + DAEMON_HELLO_PROBE_TIMEOUT;
    loop {
        match ipc::connect(endpoint).await {
            Ok(stream) => return stream_speaks_daemon_hello(stream).await,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(_) => return false,
        }
    }
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
/// `Err(AlreadyRunning)` = 端点已被别的 daemon 持有,caller 应静默退出 ——
/// 并发自拉起的竞态由端点绑定互斥收敛;`Err(Runtime)` = 绑定后的运行期故障。
pub async fn serve(endpoint: &str, idle_exit: Duration) -> Result<ServeOutcome, ServeError> {
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
) -> Result<ServeOutcome, ServeError> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let security =
        ipc::windows_security::PipeSecurity::current_user_only().map_err(ServeError::Runtime)?;

    // 首实例带 first_pipe_instance:同名 pipe 已被持有(另一个 daemon 赢了)
    // 会直接失败 —— 这是并发自拉起竞态的收敛点。
    // SAFETY: security 在本函数存活期间持有,attributes_ptr 指向的内存有效。
    let first_server = unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .create_with_security_attributes_raw(endpoint, security.attributes_ptr())
    };
    let mut server = match first_server {
        Ok(server) => server,
        Err(bind_error) => {
            // 只有确实能连上活 daemon 才是正常让位；非法名称、权限等绑定错误
            // 必须暴露为 Runtime，不能静默退出 0。
            if endpoint_has_live_daemon(endpoint).await {
                return Err(ServeError::AlreadyRunning(
                    "endpoint already held by a live daemon".into(),
                ));
            }
            return Err(ServeError::Runtime(format!(
                "endpoint bind failed and no live daemon was reachable: {bind_error}"
            )));
        }
    };

    let mut idle_tick = tokio::time::interval(idle_check_tick(idle_exit));
    idle_tick.tick().await; // 首个 tick 立即返回,跳过

    loop {
        tokio::select! {
            connected = server.connect() => {
                connected.map_err(|e| ServeError::Runtime(format!("pipe accept failed: {e}")))?;
                // 先补一个新实例再处理当前连接,保证任何时刻都有实例在监听。
                // SAFETY: 同上。
                let next = unsafe {
                    ServerOptions::new()
                        .create_with_security_attributes_raw(endpoint, security.attributes_ptr())
                }
                .map_err(|e| ServeError::Runtime(format!("pipe re-create failed: {e}")))?;
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
struct UnixEndpointLock {
    _file: std::fs::File,
}

#[cfg(unix)]
impl UnixEndpointLock {
    /// 用 advisory flock 串行化 stale socket 恢复。锁文件永久保留、fd 随
    /// daemon 生命周期持有；进程崩溃时内核自动释放锁，不产生 stale lock。
    async fn acquire(endpoint: &std::path::Path) -> Result<Self, ServeError> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;

        let mut lock_name = endpoint.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = std::path::PathBuf::from(lock_name);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&lock_path)
            .map_err(|e| ServeError::Runtime(format!("endpoint lock open failed: {e}")))?;
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| ServeError::Runtime(format!("endpoint lock chmod 0600 failed: {e}")))?;

        const LOCK_EX: i32 = 2;
        const LOCK_NB: i32 = 4;
        extern "C" {
            fn flock(fd: i32, operation: i32) -> i32;
        }
        let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            return if error.kind() == std::io::ErrorKind::WouldBlock {
                if endpoint_has_live_daemon(&endpoint.to_string_lossy()).await {
                    Err(ServeError::AlreadyRunning(
                        "endpoint already held by a live daemon".into(),
                    ))
                } else {
                    Err(ServeError::Runtime(
                        "endpoint lock is held but no live daemon was reachable".into(),
                    ))
                }
            } else {
                Err(ServeError::Runtime(format!(
                    "endpoint lock acquisition failed: {error}"
                )))
            };
        }
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn unix_socket_identity(path: &std::path::Path) -> Result<Option<(u64, u64)>, ServeError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ServeError::Runtime(format!(
                "endpoint metadata failed: {error}"
            )));
        }
    };
    if !metadata.file_type().is_socket() {
        return Err(ServeError::Runtime(format!(
            "endpoint exists but is not a Unix socket: {}",
            path.display()
        )));
    }
    Ok(Some((metadata.dev(), metadata.ino())))
}

#[cfg(unix)]
async fn bind_unix_listener(
    path: &std::path::Path,
) -> Result<tokio::net::UnixListener, ServeError> {
    use tokio::net::{UnixListener, UnixStream};

    for _ in 0..3 {
        match UnixListener::bind(path) {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(error) => {
                return Err(ServeError::Runtime(format!(
                    "endpoint bind failed: {error}"
                )));
            }
        }

        // AddressInUse 才允许进入 stale 恢复。记录 inode，探活，再次核对同一
        // dev+ino 后才删除；恢复锁保证两个新版 daemon 不会交错 unlink/bind。
        let before = match unix_socket_identity(path)? {
            Some(identity) => identity,
            None => continue,
        };
        match UnixStream::connect(path).await {
            Ok(stream) => {
                if stream_speaks_daemon_hello(stream).await {
                    return Err(ServeError::AlreadyRunning(
                        "endpoint already held by a live daemon".into(),
                    ));
                }
                return Err(ServeError::Runtime(
                    "endpoint accepted a connection but did not speak the daemon protocol".into(),
                ));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) => {}
            Err(error) => {
                return Err(ServeError::Runtime(format!(
                    "endpoint liveness probe failed: {error}"
                )));
            }
        }
        let after = match unix_socket_identity(path)? {
            Some(identity) => identity,
            None => continue,
        };
        if before != after {
            continue;
        }
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(ServeError::Runtime(format!(
                    "stale endpoint removal failed: {error}"
                )));
            }
        }
    }

    Err(ServeError::Runtime(
        "endpoint changed repeatedly while recovering a stale socket".into(),
    ))
}

#[cfg(unix)]
async fn serve_until_exit(
    endpoint: &str,
    idle_exit: Duration,
    state: &Arc<DaemonState>,
) -> Result<ServeOutcome, ServeError> {
    use std::os::unix::fs::PermissionsExt;

    let path = std::path::Path::new(endpoint);
    let _endpoint_lock = UnixEndpointLock::acquire(path).await?;
    let listener = bind_unix_listener(path).await?;
    // 权限 0600:仅当前用户可连。设置失败必须 fail closed：先关 listener，
    // 再删掉不可确认权限的 socket，绝不能继续服务。
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        drop(listener);
        let cleanup = std::fs::remove_file(path)
            .err()
            .map(|e| format!("; endpoint cleanup also failed: {e}"))
            .unwrap_or_default();
        return Err(ServeError::Runtime(format!(
            "endpoint chmod 0600 failed: {error}{cleanup}"
        )));
    }

    let mut idle_tick = tokio::time::interval(idle_check_tick(idle_exit));
    idle_tick.tick().await;

    let outcome = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted
                    .map_err(|e| ServeError::Runtime(format!("socket accept failed: {e}")))?;
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

    // 1. hello 帧：包版本 + 协议版本 + pid，供 CLI 做换代握手与 daemon-status。
    let hello = ServerFrame::Hello {
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: Some(ipc::PROTOCOL_VERSION),
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
            let frame = match ssh_service::list_connections(ConnectionScope::Capability(
                req.project_token.as_deref().unwrap_or_default(),
            )) {
                Ok(views) => ServerFrame::result_connections(views),
                Err(e) => ServerFrame::Error {
                    message: e.message().to_string(),
                },
            };
            let _ = write_frame(&mut writer, &frame).await;
        }
        Op::Status => {
            let sessions = state.pool.len().await;
            let _ = write_frame(&mut writer, &ServerFrame::result_sessions(sessions)).await;
        }
        Op::Shutdown => {
            // 先 ack 再触发退出:CLI 拿得到确认,drain 由 serve 统一做。
            let _ = write_frame(&mut writer, &ServerFrame::result_empty()).await;
            state.shutdown.notify_one();
        }
        Op::Exec => {
            handle_exec(&mut reader, &mut writer, req, &state).await;
        }
        Op::Upload | Op::Download => {
            // transfer_direction 对这两个 op 必有值
            let direction = op.transfer_direction().expect("transfer op");
            handle_transfer(&mut writer, req, direction, &state).await;
        }
    }
}

/// exec:service 编排的输出经 mpsc 转成 stdout/stderr 帧实时写回。
///
/// CLI 断开的两路感知(spec §2「daemon 检测到连接断开即关闭对应 exec channel」):
/// 写帧失败(有输出时),或读端到 EOF(无输出的长命令也能立即感知)——
/// 任一触发即设置状态型取消令牌，并等待 service 显式发送 SSH channel close；
/// 请求级超时同样在 service 的 channel 状态机内强制，不依赖 CLI 存活。
async fn handle_exec<R, W>(reader: &mut R, writer: &mut W, req: Request, state: &Arc<DaemonState>)
where
    R: tokio::io::AsyncBufRead + Unpin,
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
    let cancellation = ssh_service::ExecCancellation::new();
    let exec_fut = ssh_service::exec(
        &state.pool,
        ssh_service::ExecRequest {
            scope: ConnectionScope::Capability(req.project_token.as_deref().unwrap_or_default()),
            connection,
            command,
            cwd: req.cwd.as_deref(),
            timeout_secs: req.timeout_secs,
        },
        &cancellation,
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

    // 断连探测缓冲:协议里 CLI 发完请求行就不再说话,这里读到 EOF/错误即断连;
    // 读到额外行属协议外噪声,忽略续读。
    let mut probe = String::new();
    let result = loop {
        tokio::select! {
            res = &mut exec_fut => break res,
            maybe_frame = rx.recv() => {
                let Some(frame) = maybe_frame else { continue };
                if write_frame(writer, &frame).await.is_err() {
                    // CLI 断开:通知 service 显式关闭 SSH channel，并等 close 路径完成。
                    eprintln!("[mt-ssh-cli daemon] client disconnected mid-exec, closing channel");
                    cancellation.cancel();
                    let _ = exec_fut.as_mut().await;
                    return;
                }
            }
            read = reader.read_line(&mut probe) => {
                match read {
                    Ok(0) | Err(_) => {
                        eprintln!("[mt-ssh-cli daemon] client disconnected mid-exec, closing channel");
                        cancellation.cancel();
                        let _ = exec_fut.as_mut().await;
                        return;
                    }
                    Ok(_) => probe.clear(),
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
        Ok(outcome) => ServerFrame::result_exec(outcome.exit_code, outcome.timed_out),
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
        ConnectionScope::Capability(req.project_token.as_deref().unwrap_or_default()),
        connection,
        local,
        remote,
        req.timeout_secs,
    )
    .await
    {
        Ok(bytes) => ServerFrame::result_bytes(bytes),
        Err(e) => ServerFrame::Error {
            message: e.message().to_string(),
        },
    };
    let _ = write_frame(writer, &terminal).await;
}

/// 写一帧(单行 JSON)并 flush。
async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &ServerFrame) -> Result<(), ()> {
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
            format!(
                r"\\.\pipe\mt-ssh-cli-test-{label}-{}-{nanos}",
                std::process::id()
            )
        }
        #[cfg(unix)]
        {
            std::env::temp_dir()
                .join(format!(
                    "mt-cli-test-{label}-{}-{nanos}.sock",
                    std::process::id()
                ))
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
    ) -> tokio::task::JoinHandle<Result<ServeOutcome, ServeError>> {
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
            ServerFrame::Hello {
                version,
                protocol_version,
                pid,
            } => {
                assert_eq!(version, env!("CARGO_PKG_VERSION"));
                assert_eq!(protocol_version, Some(ipc::PROTOCOL_VERSION));
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
    async fn daemon_list_without_token_is_rejected() {
        let ep = test_endpoint("list");
        let daemon = spawn_daemon(ep.clone(), Duration::from_secs(60)).await;

        let (mut reader, mut writer, _) = connect_and_hello(&ep).await;
        send_request(
            &mut writer,
            &Request {
                v: ipc::PROTOCOL_VERSION,
                op: Some(Op::List),
                ..Default::default()
            },
        )
        .await;
        match read_frame(&mut reader).await {
            ServerFrame::Error { message } => assert_eq!(message, "invalid project token"),
            other => panic!("expected error, got {other:?}"),
        }
        daemon.abort();
    }

    #[tokio::test]
    async fn daemon_exec_unknown_token_yields_error_frame() {
        let ep = test_endpoint("exec-err");
        let daemon = spawn_daemon(ep.clone(), Duration::from_secs(60)).await;

        let (mut reader, mut writer, _) = connect_and_hello(&ep).await;
        send_request(
            &mut writer,
            &Request {
                v: ipc::PROTOCOL_VERSION,
                op: Some(Op::Exec),
                project_token: Some("definitely-not-a-real-token".into()),
                connection: Some("mt-test-definitely-no-such-connection".into()),
                command: Some("true".into()),
                ..Default::default()
            },
        )
        .await;
        match read_frame(&mut reader).await {
            ServerFrame::Error { message } => {
                assert_eq!(message, "invalid project token");
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
                assert!(
                    message.contains("protocol version mismatch"),
                    "got: {message}"
                )
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

    #[cfg(windows)]
    #[tokio::test]
    async fn invalid_or_non_daemon_pipe_endpoint_is_runtime_error() {
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            serve("not-a-valid-windows-pipe-endpoint", Duration::from_secs(60)),
        )
        .await
        .expect("非法 endpoint 不应进入 accept 循环");

        assert!(
            matches!(result, Err(ServeError::Runtime(_))),
            "got: {result:?}"
        );

        // 仅能 connect 不能证明对端是 daemon：普通同名 pipe 不发 hello 时也必须
        // 暴露 Runtime，不能被误判为并发 daemon 而静默 exit 0。
        use tokio::net::windows::named_pipe::ServerOptions;
        let ep = test_endpoint("non-daemon-holder");
        let holder = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&ep)
            .unwrap();
        let holder_task = tokio::spawn(async move {
            holder.connect().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        let result =
            tokio::time::timeout(Duration::from_secs(2), serve(&ep, Duration::from_secs(60)))
                .await
                .expect("非 daemon pipe 不应进入 accept 循环");
        assert!(
            matches!(result, Err(ServeError::Runtime(_))),
            "got: {result:?}"
        );
        holder_task.await.unwrap();
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
