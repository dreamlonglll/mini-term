//! daemon —— mt-ssh-cli 的守护进程服务端(全机单例,持全局 SshPool)。
//!
//! 进程模型(spec §2):全局单例、不按 project 拆分。池按 `connection_id` 缓存
//! session 与 project 无关;project 范围过滤是**请求级**参数 —— 每个请求处理时
//! 重新读 config.json 并按该请求的 `projectToken` 解析,天然保证「主程序里改关联
//! 范围即时生效」的既有承诺。
//!
//! 生命周期:IPC 端点绑定天然互斥(抢输实例静默退出);空闲(无活跃请求且
//! `idle_exit` 内无新连接)→ drain 池 → 返回。收到 shutdown op → 回 ack → **立刻
//! 停止接受新连接**(Windows 关掉监听实例、Unix 关 listener 并删 socket)→ 等在途
//! 请求做完(上限 [`SHUTDOWN_DRAIN_TIMEOUT`])→ drain 池 → 返回。**本模块只返回不
//! exit** —— `std::process::exit` 由 bin 调,进程内集成测试才能安全驱动完整生命周期。
//! ⚠️ 0.4.10 及更早的 daemon 收到 shutdown 仍是立即退出、硬切在途请求 —— 已发出去
//! 的旧二进制改不了,新 CLI 给旧 daemon 换代时那一刻的在途请求照旧会断。
//!
//! 每个连接一问一答:daemon 先发 hello(版本握手用),读一行请求,流式回帧至
//! 终帧。CLI 中途断开 → 触发状态型取消令牌 → service 显式发送 channel close，
//! 等关闭路径完成后结束请求(等待有上限,见 [`CANCEL_SETTLE_TIMEOUT`]);session
//! 留池、daemon 不退出。
//!
//! 生命周期事件(启动 / 版本 / 端点 / 空闲退出 / shutdown / 客户端断开 / 取消兜底超时)
//! 经 [`daemon_log::event`] 带 UTC 时间戳写 stderr;CLI 拉起的 daemon 的 stderr 接在
//! 数据目录的 `mt-ssh-cli-daemon.log` 上(见 [`crate::daemon_spawn`])。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::sync::Notify;

use crate::daemon_log;
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

/// shutdown 后等在途请求做完的上限。
///
/// 取 60s = exec 的默认超时([`ssh_service::DEFAULT_TIMEOUT_SECS`]):默认参数下的 exec
/// 在这个窗口里要么跑完、要么被它自己的超时收掉,换代 / `daemon-stop` 不会硬切断它。
/// 更长的请求(upload/download 默认 300s、显式加大 `--timeout` 的 exec)到点照样被切断,
/// 那条 CLI 收到 "connection to daemon lost mid-request" —— 换代不能被一个长传输拖住
/// 好几分钟,这是有意的取舍。
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(60);

/// shutdown 等在途请求时的轮询间隔。
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// 客户端断开、发出取消后,等 exec 编排收尾的上限。
///
/// 0.4.11 起取消令牌在编排的每一步都被响应(建连 / 认证、等会话锁、开 channel、收流),
/// 正常情况下几毫秒内就收尾;这道上限是纵深兜底:万一哪一步不看取消,`await` 无限挂住会
/// 连带 `ActiveGuard` 不释放、daemon 永远不会空闲自退。到点就放弃这次编排(丢弃 future)
/// 并记一笔日志。
const CANCEL_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

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

/// 绑定失败后对端点的探测结论。
#[derive(Debug)]
enum EndpointProbe {
    /// 当前用户的活 daemon(身份核验通过且发了 hello)—— 静默让位。
    LiveDaemon,
    /// 端点上的服务端不属于当前用户。绝不能让位:让了就等于默许冒充者接走所有请求。
    Untrusted(String),
    /// 连不上,或连上了但不讲 daemon 协议。
    NoDaemon,
}

impl EndpointProbe {
    /// 按探测结论给绑定失败归类;`no_daemon` 是「没有活 daemon」时的报错文本。
    fn into_serve_error(self, no_daemon: String) -> ServeError {
        match self {
            EndpointProbe::LiveDaemon => {
                ServeError::AlreadyRunning("endpoint already held by a live daemon".into())
            }
            EndpointProbe::Untrusted(reason) => untrusted_holder(&reason),
            EndpointProbe::NoDaemon => ServeError::Runtime(no_daemon),
        }
    }
}

/// 端点被别的用户的进程占着:按运行期故障暴露,不静默让位。
fn untrusted_holder(reason: &str) -> ServeError {
    ServeError::Runtime(format!(
        "endpoint is held by a process that does not belong to the current user ({reason}); \
         refusing to yield to it"
    ))
}

/// 探测端点上是不是**当前用户**的活 daemon。`ipc::connect` 先核验服务端身份,
/// 核验通过后再看首帧是不是 hello。
async fn probe_endpoint(endpoint: &str) -> EndpointProbe {
    let deadline = tokio::time::Instant::now() + DAEMON_HELLO_PROBE_TIMEOUT;
    loop {
        match ipc::connect(endpoint).await {
            Ok(stream) => {
                return if stream_speaks_daemon_hello(stream).await {
                    EndpointProbe::LiveDaemon
                } else {
                    EndpointProbe::NoDaemon
                };
            }
            Err(ipc::ConnectError::Untrusted(reason)) => return EndpointProbe::Untrusted(reason),
            Err(ipc::ConnectError::Unreachable(_)) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(ipc::ConnectError::Unreachable(_)) => return EndpointProbe::NoDaemon,
        }
    }
}

/// 活跃连接计数的 RAII guard:drop 时递减并刷新活动时间。
///
/// 在 accept 循环里、spawn 处理协程**之前**就建好:shutdown 一旦让 accept 循环返回,
/// 计数里就已含所有接下的连接,不会出现「协程还没开始跑、计数还是 0」就 drain 退出的空档。
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
/// `Err(AlreadyRunning)` = 端点已被当前用户的另一个 daemon 持有,caller 应静默退出 ——
/// 并发自拉起的竞态由端点绑定互斥收敛;`Err(Runtime)` = 其它绑定错误(含端点被别的
/// 用户的进程占着)或绑定后的运行期故障。
pub async fn serve(endpoint: &str, idle_exit: Duration) -> Result<ServeOutcome, ServeError> {
    serve_with_drain_timeout(endpoint, idle_exit, SHUTDOWN_DRAIN_TIMEOUT).await
}

/// [`serve`] 的本体;shutdown 等在途请求的上限可注入(测试用短上限)。
async fn serve_with_drain_timeout(
    endpoint: &str,
    idle_exit: Duration,
    drain_timeout: Duration,
) -> Result<ServeOutcome, ServeError> {
    let state = Arc::new(DaemonState {
        pool: Arc::new(SshPool::new()),
        active: AtomicUsize::new(0),
        last_activity_ms: AtomicU64::new(now_millis()),
        shutdown: Notify::new(),
    });

    daemon_log::event(&format!(
        "starting v{} (protocol v{}) pid={} endpoint={endpoint}",
        env!("CARGO_PKG_VERSION"),
        ipc::PROTOCOL_VERSION,
        std::process::id()
    ));

    // 返回时监听实例 / socket 已经关掉:shutdown 之后新客户端连不上。
    let outcome = serve_until_exit(endpoint, idle_exit, &state).await?;
    match outcome {
        ServeOutcome::Idle => daemon_log::event(&format!(
            "idle for {idle_exit:?} with no active requests, exiting"
        )),
        ServeOutcome::Shutdown => wait_for_active_requests(&state, drain_timeout).await,
    }

    // drain 池:逐 session disconnect(ByApplication),远端不留 dangling。
    let sessions = state.pool.len().await;
    daemon_log::event(&format!("draining session pool ({sessions} session(s))"));
    state.pool.shutdown().await;
    Ok(outcome)
}

/// shutdown 之后等在途请求做完,最多等 `limit`。监听已关,`active` 只减不增。
async fn wait_for_active_requests(state: &DaemonState, limit: Duration) {
    daemon_log::event(&format!(
        "shutdown requested: stopped accepting connections; {} connection(s) in flight, \
         waiting up to {limit:?}",
        state.active.load(Ordering::SeqCst)
    ));
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let active = state.active.load(Ordering::SeqCst);
        if active == 0 {
            daemon_log::event("shutdown: no requests in flight, exiting");
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            daemon_log::event(&format!(
                "shutdown: {active} connection(s) still in flight after {limit:?}, exiting anyway"
            ));
            return;
        }
        tokio::time::sleep(SHUTDOWN_POLL_INTERVAL).await;
    }
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
            // 只有确认是**当前用户**的活 daemon 才是正常让位;非法名称、权限等绑定错误,
            // 以及别的用户抢先占住端点,都必须暴露为 Runtime,不能静默退出 0。
            return Err(probe_endpoint(endpoint).await.into_serve_error(format!(
                "endpoint bind failed and no live daemon was reachable: {bind_error}"
            )));
        }
    };
    daemon_log::event(&format!("listening on {endpoint}"));

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
                let guard = ActiveGuard::new(state.clone());
                let st = state.clone();
                tokio::spawn(async move { handle_connection(client, st, guard).await });
            }
            _ = idle_tick.tick() => {
                if state.active.load(Ordering::SeqCst) == 0 && state.idle_for() >= idle_exit {
                    return Ok(ServeOutcome::Idle);
                }
            }
            _ = state.shutdown.notified() => {
                // 返回即丢弃正在等连接的那个监听实例:此后再没有空闲实例,新客户端连不上
                // (拿到 ERROR_PIPE_BUSY / 找不到管道);仍连着的实例由各自的处理协程持有,
                // 请求做完自然关闭。
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
                Err(probe_endpoint(&endpoint.to_string_lossy())
                    .await
                    .into_serve_error(
                        "endpoint lock is held but no live daemon was reachable".into(),
                    ))
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
                // 先核身份:别的用户放在这里的 socket 绝不能当成「已有 daemon」静默让位。
                if let Err(reason) = ipc::verify_socket_peer(&stream) {
                    return Err(untrusted_holder(&reason));
                }
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

    daemon_log::event(&format!("listening on {endpoint}"));

    let mut idle_tick = tokio::time::interval(idle_check_tick(idle_exit));
    idle_tick.tick().await;

    let outcome = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted
                    .map_err(|e| ServeError::Runtime(format!("socket accept failed: {e}")))?;
                let guard = ActiveGuard::new(state.clone());
                let st = state.clone();
                tokio::spawn(async move { handle_connection(stream, st, guard).await });
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
    // 先删 socket 再关 listener、放恢复锁(函数返回时依次 drop):shutdown 之后新客户端
    // 连不上;锁一放,新 daemon 就能在同一路径上起来,本进程继续把在途请求做完 ——
    // 它已不再碰这个路径,两者互不干扰。
    let _ = std::fs::remove_file(path);
    Ok(outcome)
}

/// 空闲检查频率:跟随窗口收缩(测试用短窗口也能及时触发),上限 30s。
fn idle_check_tick(idle_exit: Duration) -> Duration {
    (idle_exit / 4).clamp(Duration::from_millis(20), Duration::from_secs(30))
}

/// 处理一个客户端连接:hello → 读一行请求 → 按 op 分发 → 终帧。
///
/// `_guard` 由 accept 循环建好传进来,随本函数返回释放(见 [`ActiveGuard`])。
async fn handle_connection<S>(stream: S, state: Arc<DaemonState>, _guard: ActiveGuard)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
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
            // 活跃数不含这条 status 连接自己。
            let others = state.active.load(Ordering::SeqCst).saturating_sub(1);
            let _ = write_frame(&mut writer, &ServerFrame::result_status(sessions, others)).await;
        }
        Op::Shutdown => {
            // 先 ack 再触发退出:CLI 拿得到确认;停止监听、等在途请求、drain 池由 serve
            // 统一做。
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
/// 任一触发即设置状态型取消令牌，并等待 service 显式发送 SSH channel close
/// (还在建连时直接放弃建连;等待有上限,见 [`cancel_after_disconnect`]);
/// 请求级超时(从请求进门算起,含建连)同样由 service 强制，不依赖 CLI 存活。
///
/// 远端以信号终止命令时,终帧之前补一个 stderr 帧说明信号(退出码已在 service 层映射成
/// 128+n)—— 走现有帧,不改协议;任何版本的 CLI 都会把它原样打到 stderr。
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
                    cancel_after_disconnect(&cancellation, exec_fut.as_mut()).await;
                    return;
                }
            }
            read = reader.read_line(&mut probe) => {
                match read {
                    Ok(0) | Err(_) => {
                        cancel_after_disconnect(&cancellation, exec_fut.as_mut()).await;
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
        Ok(outcome) => {
            if let Some(note) = outcome.signal_note() {
                let note = ServerFrame::Stderr {
                    data_b64: ipc::b64_encode(format!("mt-ssh-cli: {note}\n").as_bytes()),
                };
                if write_frame(writer, &note).await.is_err() {
                    return;
                }
            }
            ServerFrame::result_exec(outcome.exit_code, outcome.timed_out, outcome.not_started)
        }
        Err(e) => ServerFrame::Error {
            message: e.message().to_string(),
        },
    };
    let _ = write_frame(writer, &terminal).await;
}

/// 客户端中途断开:设取消令牌,等 exec 编排显式关掉 channel 收尾。
///
/// 等待以 [`CANCEL_SETTLE_TIMEOUT`] 为限:编排卡在建连 / 认证时不响应取消,无限等下去
/// 会让这条连接的 `ActiveGuard` 永不释放、daemon 永不空闲自退。到点放弃 —— future 随
/// 调用方返回被丢弃。
async fn cancel_after_disconnect<F: Future>(
    cancellation: &ssh_service::ExecCancellation,
    exec: Pin<&mut F>,
) {
    daemon_log::event("client disconnected mid-exec, cancelling the remote command");
    cancellation.cancel();
    if !settle_within(exec, CANCEL_SETTLE_TIMEOUT).await {
        daemon_log::event(&format!(
            "exec did not settle within {CANCEL_SETTLE_TIMEOUT:?} after cancellation \
             (likely stuck connecting); abandoning it"
        ));
    }
}

/// 在 `limit` 内等 `fut` 完成;超时返回 false(future 本身由调用方处置)。
async fn settle_within<F: Future>(fut: Pin<&mut F>, limit: Duration) -> bool {
    tokio::time::timeout(limit, fut).await.is_ok()
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
        spawn_daemon_with_drain(endpoint, idle_exit, SHUTDOWN_DRAIN_TIMEOUT).await
    }

    /// 同 [`spawn_daemon`],shutdown 等在途请求的上限可调。
    async fn spawn_daemon_with_drain(
        endpoint: String,
        idle_exit: Duration,
        drain_timeout: Duration,
    ) -> tokio::task::JoinHandle<Result<ServeOutcome, ServeError>> {
        let handle = tokio::spawn(async move {
            serve_with_drain_timeout(&endpoint, idle_exit, drain_timeout).await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle
    }

    fn status_request() -> Request {
        Request {
            v: ipc::PROTOCOL_VERSION,
            op: Some(Op::Status),
            ..Default::default()
        }
    }

    fn shutdown_request() -> Request {
        Request {
            v: ipc::PROTOCOL_VERSION,
            op: Some(Op::Shutdown),
            ..Default::default()
        }
    }

    /// 等到端点连不上为止(最多 2s)。期间万一连上(监听实例还没来得及关),立刻断开 ——
    /// 只发 hello 不发请求的连接会被 daemon 当作探测静默收尾。
    async fn wait_until_unreachable(endpoint: &str) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match ipc::connect(endpoint).await {
                Err(_) => return true,
                Ok(stream) => drop(stream),
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
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
            ServerFrame::Result {
                sessions,
                active_requests,
                ..
            } => {
                assert_eq!(sessions, Some(0));
                // 只有这条 status 自己,不算在内。
                assert_eq!(active_requests, Some(0));
            }
            other => panic!("expected result, got {other:?}"),
        }
        daemon.abort();
    }

    #[tokio::test]
    async fn status_counts_other_connections_in_flight() {
        let ep = test_endpoint("inflight");
        let daemon = spawn_daemon(ep.clone(), Duration::from_secs(60)).await;

        // A:连上、收了 hello、还没发请求 —— 对 daemon 来说就是一条在途连接。
        let (_a_reader, _a_writer, _) = connect_and_hello(&ep).await;

        let (mut reader, mut writer, _) = connect_and_hello(&ep).await;
        send_request(&mut writer, &status_request()).await;
        match read_frame(&mut reader).await {
            ServerFrame::Result {
                active_requests, ..
            } => assert_eq!(active_requests, Some(1)),
            other => panic!("expected result, got {other:?}"),
        }
        daemon.abort();
    }

    #[tokio::test]
    async fn shutdown_stops_accepting_but_finishes_requests_in_flight() {
        let ep = test_endpoint("graceful");
        let daemon = spawn_daemon(ep.clone(), Duration::from_secs(60)).await;

        // A:在途连接(已连上,请求稍后才发)。
        let (mut a_reader, mut a_writer, _) = connect_and_hello(&ep).await;

        // B:请求 shutdown,先拿到 ack。
        let (mut b_reader, mut b_writer, _) = connect_and_hello(&ep).await;
        send_request(&mut b_writer, &shutdown_request()).await;
        assert!(matches!(
            read_frame(&mut b_reader).await,
            ServerFrame::Result { .. }
        ));
        drop((b_reader, b_writer));

        // 立刻停止接受新连接……
        assert!(
            wait_until_unreachable(&ep).await,
            "endpoint must stop accepting new clients after shutdown"
        );
        // ……但还在等 A,没有退出。
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!daemon.is_finished(), "must wait for the request in flight");

        // A 的请求照常处理完。
        send_request(&mut a_writer, &status_request()).await;
        assert!(matches!(
            read_frame(&mut a_reader).await,
            ServerFrame::Result { .. }
        ));
        drop((a_reader, a_writer));

        let outcome = tokio::time::timeout(Duration::from_secs(5), daemon)
            .await
            .expect("serve should return once nothing is in flight")
            .unwrap()
            .unwrap();
        assert_eq!(outcome, ServeOutcome::Shutdown);
    }

    #[tokio::test]
    async fn shutdown_wait_for_requests_in_flight_is_bounded() {
        let ep = test_endpoint("drain-cap");
        let daemon = spawn_daemon_with_drain(
            ep.clone(),
            Duration::from_secs(60),
            Duration::from_millis(300),
        )
        .await;

        // 一条一直不发请求的在途连接,撑满等待上限。
        let (_stuck_reader, _stuck_writer, _) = connect_and_hello(&ep).await;

        let (mut reader, mut writer, _) = connect_and_hello(&ep).await;
        send_request(&mut writer, &shutdown_request()).await;
        assert!(matches!(
            read_frame(&mut reader).await,
            ServerFrame::Result { .. }
        ));

        let outcome = tokio::time::timeout(Duration::from_secs(5), daemon)
            .await
            .expect("drain wait must give up at its limit")
            .unwrap()
            .unwrap();
        assert_eq!(outcome, ServeOutcome::Shutdown);
    }

    #[tokio::test]
    async fn settle_gives_up_on_a_future_that_ignores_cancellation() {
        let stuck = std::future::pending::<()>();
        tokio::pin!(stuck);
        assert!(!settle_within(stuck.as_mut(), Duration::from_millis(50)).await);

        let done = async {};
        tokio::pin!(done);
        assert!(settle_within(done.as_mut(), Duration::from_millis(50)).await);
    }

    /// 缺令牌在读配置之前就被拒(这条与下一条都不碰真实的 config.json)。
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
            ServerFrame::Error { message } => {
                assert_eq!(message, mt_core::TokenScopeError::MissingToken.to_string());
                assert!(message.contains("--project-token"), "got: {message}");
            }
            other => panic!("expected error, got {other:?}"),
        }
        daemon.abort();
    }

    #[tokio::test]
    async fn daemon_exec_scope_error_yields_error_frame() {
        let ep = test_endpoint("exec-err");
        let daemon = spawn_daemon(ep.clone(), Duration::from_secs(60)).await;

        let (mut reader, mut writer, _) = connect_and_hello(&ep).await;
        send_request(
            &mut writer,
            &Request {
                v: ipc::PROTOCOL_VERSION,
                op: Some(Op::Exec),
                // 纯空白令牌:fail closed 且先于读盘 —— 测试不读真实数据目录。
                project_token: Some("   ".into()),
                connection: Some("mt-test-definitely-no-such-connection".into()),
                command: Some("true".into()),
                ..Default::default()
            },
        )
        .await;
        match read_frame(&mut reader).await {
            ServerFrame::Error { message } => {
                assert_eq!(message, mt_core::TokenScopeError::MissingToken.to_string());
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
