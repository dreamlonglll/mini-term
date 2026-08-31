//! SSH 远程项目的服务层(audit #28 的后端半场,BB-a 批)。
//!
//! 自 `src-tauri/src/remote_ssh.rs`(1392 行)逐字等价移植。通过共享 crate
//! `mt-ssh` 的 russh 持久会话池 + SFTP 只读原语,为「远程项目」提供五个能力:
//!
//! | 原版 command | 本模块入口 |
//! |---|---|
//! | `ssh_remote_list_directory` | [`list_directory`] |
//! | `ssh_remote_validate_dir` | [`validate_dir`] |
//! | `ssh_remote_upload_paste` | [`upload_paste`] |
//! | `ssh_remote_ai_sessions` | [`ai_sessions`] |
//! | `ssh_remote_ai_session_content` | [`ai_session_content`] |
//!
//! # 线程口径(与 Tauri 版的唯一结构性差异)
//!
//! 原版是 `#[tauri::command(async)]`,跑在 Tauri 自带的全局 tokio runtime 上。
//! GPUI 没有 tokio,主线程也不能阻塞,于是:
//!
//! - **本模块自持一个小 tokio 运行时**(见 [`RemoteSshState`] 的 `runtime`),
//!   与 `mt_relay::MobileRelayManager` 的 `Owned` 分支同一路数 —— 懒建、2 个
//!   工作线程、进程内唯一;
//! - **公开入口全是同步阻塞函数**,内部 `block_on`。调用方(BB-b 的视图层)
//!   **必须**把它们丢进 `cx.background_executor().spawn(...)`,与 `mt_project::git`
//!   / `pricing::fetch_models_dev` 同一条纪律。主线程直接调 = 卡界面。
//!   为什么不做成 `async fn` 让 gpui 的执行器 await:那样整条链路要一个
//!   tokio-compat 的反应堆(russh 的 IO 依赖 tokio driver),不如把 tokio 的边界
//!   收在本模块内部一层。
//!
//! # 池 / 缓存的归属
//!
//! 池按 `connection.id` 全局复用,故 [`RemoteSshState`] 是**进程级单例**
//! ([`state()`]),不挂在 `AppStore` 上 —— 后台任务拿不到 `Entity<AppStore>`,
//! 而这些函数就是给后台跑的。会话列表缓存复用 `mt_ai::sessions::session_cache()`
//! 那张全局表(与原版共用同一份、key 掺 `ssh|<connId>|<path>`)。
//!
//! # 契约(对齐 spec/backend/wsl-unc-session-scanning.md,一字未改)
//!
//! - 缓存锁即取即放,**绝不跨 SFTP 慢 IO 持锁**;
//! - 会话扫描一切失败静默降级为空列表(不弹错、不 panic);
//!   文件树 / 目录验证 / 正文读取失败返回明确 `Err(String)`。
//!
//! # 连接从哪里来
//!
//! 原版每个 command 自己 `read_config(app)` 再按 id 找连接。GPUI 侧配置活在
//! 主线程的 `AppStore` 里,后台任务读不到,于是**调用方在主线程取好
//! [`SshConnection`] 再传进来** —— 断链(连接已删)判定前移到
//! [`find_connection`],它是纯函数、有单测。

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use mt_config::SshConnection;
use mt_project::fs::{MAX_FILE_VIEW_SIZE, TextGitignore};
use mt_ssh::{CachedSession, SftpHandle, SshPool};

mod delete;
mod dirs;
mod files;
mod paths;
mod sessions;
mod transfer;

pub use delete::*;
pub use dirs::*;
pub use files::*;
pub use paths::*;
pub use sessions::*;
pub use transfer::*;

/// SFTP 协议层每请求超时(readdir / stat / 单个 read 包)。
/// 默认仅 10s 且逐请求计时(见 spec/backend/russh-sftp-file-transfer.md 坑 1),
/// 这里放宽到 20s 覆盖慢链路;整体不设长窗口——只读操作单包粒度小。
const SFTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const REMOTE_DOCUMENT_MAX_BYTES: usize = MAX_FILE_VIEW_SIZE as usize;
const REMOTE_DOCUMENT_TOO_LARGE_SAVE_ERROR: &str =
    "远程文件已超过 1MB，请重新下载或使用外部工具处理";
const REMOTE_DELETE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const REMOTE_DELETE_EXEC_TIMEOUT: Duration = Duration::from_secs(70);
const REMOTE_DELETE_SERVER_TIMEOUT_SECS: u64 = 60;
const REMOTE_DELETE_OUTPUT_CAP: usize = 16 * 1024;
static LOCAL_TRANSFER_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// 建立(或复用)SSH session 的外层超时:TCP 连接 + 握手 + 认证。
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);
/// 粘贴上传的**单请求**超时(`run_sftp_upload_on_session` 把它转成
/// `SftpSession::set_timeout`,不是整段传输的上限)。慢链路下单个 chunk 包
/// 不该把整段打断,故比只读的 20s 宽。
const PASTE_UPLOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// 粘贴上传的**整体**墙钟上限。必须显式加 —— 上层在上传期间用 in-flight 去重
/// 挡住重复 Ctrl+V,如果这里没有硬上限,一次卡死的传输会让该 pane 的粘贴
/// 静默失效且永不恢复。用户此刻正盯着「按了 Ctrl+V 还没出路径」,宁可早报错。
const PASTE_UPLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(90);
/// 根 `.gitignore` 读取上限。超大 .gitignore 截断(极端场景,规则少截无妨)。
const GITIGNORE_MAX_BYTES: usize = 256 * 1024;
/// 远程会话列表缓存 TTL(对齐 WSL 会话的 10s;`force=true` 绕过)。
const REMOTE_SESSION_CACHE_TTL: Duration = Duration::from_secs(10);
/// 远程扫描上限:SFTP 逐文件网络往返,全量扫描不可接受(对齐 WSL 侧下调值)。
const REMOTE_CLAUDE_SCAN_LIMIT: usize = 100;
const REMOTE_CODEX_SCAN_LIMIT: usize = 200;
/// Claude 会话标题提取:读文件头部的字节上限(首条 user 消息几乎总在最前面,
/// 但个别文件首行是巨大的 file-history-snapshot,给足余量)。
const CLAUDE_TITLE_HEAD_BYTES: usize = 256 * 1024;
/// Codex 会话 meta + 标题提取:session_meta 在第 1 行,64KB 覆盖含长 instructions 的情况。
const CODEX_META_HEAD_BYTES: usize = 64 * 1024;
/// codex session_index.jsonl(thread_name 映射)读取上限。
const SESSION_INDEX_MAX_BYTES: usize = 1024 * 1024;
/// 会话正文单次增量读取上限;更多内容由调用方带 next_offset 再次调用。
const CONTENT_CHUNK_MAX_BYTES: usize = 8 * 1024 * 1024;
/// 变体目录 cwd 精确校验:读任一 jsonl 头部的字节上限。
const CWD_PROBE_HEAD_BYTES: usize = 64 * 1024;

/// 远程粘贴落盘目录的缺省值(与 `mt_config::default_remote_paste_dir` 同值)。
/// 单独一份常量是为了纯函数 [`resolve_paste_dir`] 不必依赖 mt-config。
const DEFAULT_REMOTE_PASTE_DIR: &str = ".mini-term/pasted";

// ---------------------------------------------------------------------------
// 进程级状态(池 + 缓存 + tokio 运行时)
// ---------------------------------------------------------------------------

/// 远程 SSH 的进程级状态。原版是 Tauri managed state,这里是 [`state()`] 后面的
/// 全局单例 —— 后台任务拿不到 `Entity<AppStore>`,而所有 SFTP 调用都在后台。
pub struct RemoteSshState {
    /// 懒初始化的 tokio 运行时。russh / russh-sftp 的 IO 依赖 tokio driver,
    /// gpui 的执行器喂不动它们,只能自持一个。
    ///
    /// 2 个工作线程:全部操作都是网络等待型,与 `mt_relay` 的 `Owned` 分支同值。
    /// **不主动 shutdown**(见 [`RemoteSshState::shutdown_pool_blocking`] 的注释)。
    runtime: Mutex<Option<Arc<tokio::runtime::Runtime>>>,
    /// 懒初始化的 russh 会话池。session 按 `connection.id` 全局复用。
    pool: Mutex<Option<Arc<SshPool>>>,
    /// 远程项目根 `.gitignore` 编译结果缓存,key = `<connId>|<projectRoot 小写>`。
    gitignore_cache: Mutex<HashMap<String, Arc<TextGitignore>>>,
    /// 远程 `$HOME` 缓存(SFTP canonicalize(".")),key = connection id。
    home_cache: Mutex<HashMap<String, String>>,
    /// 会话 id → 远程文件路径映射(列表扫描时填充,正文读取直接命中免再扫)。
    session_paths: Mutex<HashMap<String, String>>,
}

/// std Mutex 取锁,poisoned 时取回内部数据继续(缓存均可容忍脏读,绝不 panic)。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl RemoteSshState {
    pub fn new() -> Self {
        Self {
            runtime: Mutex::new(None),
            pool: Mutex::new(None),
            gitignore_cache: Mutex::new(HashMap::new()),
            home_cache: Mutex::new(HashMap::new()),
            session_paths: Mutex::new(HashMap::new()),
        }
    }

    /// 拿(或懒建)tokio 运行时。建不起来时返回明确错误 —— 全部远程能力随之
    /// 报错,而不是 panic 掉整个应用。
    fn runtime(&self) -> Result<Arc<tokio::runtime::Runtime>, String> {
        let mut guard = lock(&self.runtime);
        if let Some(rt) = guard.as_ref() {
            return Ok(rt.clone());
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("mt-remote-ssh")
            .build()
            .map_err(|e| format!("SSH 运行时不可用: {e}"))?;
        let rt = Arc::new(rt);
        *guard = Some(rt.clone());
        Ok(rt)
    }

    /// 拿(或懒建)会话池。
    ///
    /// **前置**:必须在 tokio runtime 上下文中调用(`SshPool` 构造要 spawn 后台
    /// reaper task)——本模块只在 [`block_on`](Self::block_on) 内部调用,天然满足。
    fn pool(&self) -> Arc<SshPool> {
        let mut guard = lock(&self.pool);
        guard
            .get_or_insert_with(|| Arc::new(SshPool::new()))
            .clone()
    }

    /// 在自持运行时上跑一段 future 到完成(**阻塞当前线程**)。
    ///
    /// 这就是「同步入口 + 内部 tokio」那层胶水:调用方在
    /// `background_executor` 的线程上调它,阻塞的是那条后台线程。
    fn block_on<F, T>(&self, fut: F) -> Result<T, String>
    where
        F: std::future::Future<Output = Result<T, String>>,
    {
        let rt = self.runtime()?;
        rt.block_on(fut)
    }

    /// 一条 SSH 连接的配置被改动/删除时,把它在本进程里的**全部残留**作废:
    /// 池里那条 session + 两张按连接缓存(`home_cache` / `gitignore_cache`)+
    /// 会话路径映射。
    ///
    /// 为什么仍需主动失效:池的 map key 是稳定的 `connection.id`；虽然
    /// `CachedSession` 会保存并在每次 acquire 时核对完整 endpoint/credential
    /// 身份，主动淘汰仍能及时释放旧 session，并同步清掉 home/gitignore/path
    /// 这些同样按 connection id 建键的派生缓存。
    ///
    /// **边界**:只作废本进程的池。三个 sidecar 是独立进程、各自另一份池,
    /// 它们每次请求重读 `config.json` 拿连接信息,自己的 session 仍可能是旧的 ——
    /// 那条链路不在本函数职责内(sidecar 的池由其自身生命周期收敛)。
    ///
    /// 可在主线程调用:**不阻塞**。evict 是 async,丢给自持运行时后台跑;
    /// 池还没懒建起来时直接跳过(没有池就没有 session 可踢)。
    fn invalidate_connection(&self, conn_id: &str) {
        // 1) 按连接缓存:home 一条,gitignore 是 `<connId>|<projectRoot>` 前缀的一族,
        //    session_paths 同为 `<connId>|<sessionId>` 前缀族。
        let prefix = format!("{conn_id}|");
        lock(&self.home_cache).remove(conn_id);
        lock(&self.gitignore_cache).retain(|k, _| !k.starts_with(&prefix));
        lock(&self.session_paths).retain(|k, _| !k.starts_with(&prefix));

        // 2) 池里的 session。池未建 = 没连过任何远程,无事可做;池已建则运行时
        //    必然也已建(池只在 `block_on` 内部懒建),`runtime()` 不会新建一个。
        let pool = lock(&self.pool).clone();
        let Some(pool) = pool else { return };
        let Ok(rt) = self.runtime() else { return };
        let id = conn_id.to_string();
        rt.spawn(async move {
            pool.evict(&id).await;
        });
    }

    /// app 退出时优雅关池:abort reaper + 并发 disconnect 全部 session
    /// (单 session 2s 超时,不 hang 退出)。池未初始化则 no-op。
    ///
    /// 运行时**故意不 shutdown**:`Runtime::drop` 会等所有阻塞任务收尾,在退出
    /// 路径上是净风险(mt-relay 的同款决策见其 U 批记档)。池 drain 完进程就走了。
    pub fn shutdown_pool_blocking(&self) {
        let pool = lock(&self.pool).take();
        let Some(pool) = pool else { return };
        let Ok(rt) = self.runtime() else { return };
        eprintln!("[remote-ssh] draining ssh session pool on exit");
        rt.block_on(async move {
            pool.shutdown().await;
        });
    }
}

impl Default for RemoteSshState {
    fn default() -> Self {
        Self::new()
    }
}

/// 进程级单例。首次取用时构造(不建运行时、不建池,那两步各自更懒)。
pub fn state() -> &'static RemoteSshState {
    static STATE: OnceLock<RemoteSshState> = OnceLock::new();
    STATE.get_or_init(RemoteSshState::new)
}

/// 退出钩子:优雅关池。对应原版 `lib.rs` 在 `RunEvent::Exit` 里的那一调。
pub fn shutdown_on_exit() {
    state().shutdown_pool_blocking();
}

/// 连接配置被改动 / 删除后的失效入口(见
/// [`RemoteSshState::invalidate_connection`])。**由 `AppStore` 的写入侧调用**,
/// 主线程直接调即可,不阻塞。
pub fn invalidate_connection(conn_id: &str) {
    state().invalidate_connection(conn_id);
}

// ---------------------------------------------------------------------------
// 连接查找 / session 编排
// ---------------------------------------------------------------------------

/// 按 id 从连接表找连接。找不到 = 「断链」(连接被删除),给明确错误。
///
/// 原版在每个 command 里 `read_config(app)` 后现找;GPUI 侧由主线程从
/// `AppStore::config().ssh_connections` 取好再调这里,判定与文案一字不变。
pub fn find_connection(
    connections: &[SshConnection],
    connection_id: &str,
) -> Result<SshConnection, String> {
    connections
        .iter()
        .find(|c| c.id == connection_id)
        .cloned()
        .ok_or_else(|| format!("SSH 连接不存在或已被删除 (id={connection_id})"))
}

/// Runtime identity for a saved SSH connection. A document baseline includes
/// this value so changing host, user, port, password, or identity file cannot
/// silently redirect an already-open editor tab to another server.
pub fn connection_fingerprint(connection: &SshConnection) -> u64 {
    // Runtime-only identity: the process-random keyed hasher prevents a
    // password-derived fingerprint from becoming a stable offline oracle if it
    // ever appears in diagnostics. Callers only compare values in this process.
    static HASHER: OnceLock<RandomState> = OnceLock::new();
    let mut hasher = HASHER.get_or_init(RandomState::new).build_hasher();
    connection.id.hash(&mut hasher);
    connection.host.hash(&mut hasher);
    connection.port.hash(&mut hasher);
    connection.user.hash(&mut hasher);
    connection.password.hash(&mut hasher);
    connection.identity_file.hash(&mut hasher);
    hasher.finish()
}

/// 从池里拿一条可用 session(带外层超时 + gatetime cooldown 检查)。
async fn acquire_session(
    pool: &SshPool,
    conn: &SshConnection,
) -> Result<Arc<CachedSession>, String> {
    let session = tokio::time::timeout(ACQUIRE_TIMEOUT, pool.acquire(conn))
        .await
        .map_err(|_| format!("连接 {} 超时({}s)", conn.host, ACQUIRE_TIMEOUT.as_secs()))??;
    if session.is_unhealthy_now() {
        return Err("SSH 会话处于冷却期(上次失败后短时间内不再重试),请稍后再试".into());
    }
    Ok(session)
}

/// 开一个 SFTP 会话句柄,**并把承载它的 session 一并返回**。
/// transport 级失败(死链 race)evict + 重连再试一次,与 mt-ssh-mcp 的
/// exec/transfer 编排同构。
///
/// `SftpHandle` 自己持有活动 lease，长操作期间 reaper/LRU 不会断开它；额外返回
/// session 只供仍需在同一认证连接上另开 channel 的旧调用点使用。
async fn open_sftp_with_session(
    st: &RemoteSshState,
    conn: &SshConnection,
) -> Result<(Arc<CachedSession>, SftpHandle), String> {
    let pool = st.pool();
    let session = acquire_session(&pool, conn).await?;
    match SftpHandle::open_on_session(session.clone(), SFTP_REQUEST_TIMEOUT).await {
        Ok(h) => {
            session.touch();
            Ok((session, h))
        }
        Err(e) if e.is_transport() => {
            eprintln!("[remote-ssh] sftp open failed (transport), retrying once: {e}");
            pool.evict_if_same(&conn.id, &session).await;
            let session2 = acquire_session(&pool, conn).await?;
            let h = SftpHandle::open_on_session(session2.clone(), SFTP_REQUEST_TIMEOUT)
                .await
                .map_err(|e| e.message().to_string())?;
            session2.touch();
            Ok((session2, h))
        }
        Err(e) => Err(e.message().to_string()),
    }
}

/// 开一个 SFTP 会话句柄；句柄内部持有 session lease。
async fn open_sftp(st: &RemoteSshState, conn: &SshConnection) -> Result<SftpHandle, String> {
    Ok(open_sftp_with_session(st, conn).await?.1)
}

/// 远程 `$HOME`(SFTP canonicalize(".")),按连接缓存。锁即取即放。
async fn remote_home(
    st: &RemoteSshState,
    sftp: &SftpHandle,
    conn_id: &str,
) -> Result<String, String> {
    if let Some(h) = lock(&st.home_cache).get(conn_id).cloned() {
        return Ok(h);
    }
    let home = sftp
        .canonicalize(".")
        .await
        .map_err(|e| format!("获取远程 home 目录失败: {}", e.message()))?;
    lock(&st.home_cache).insert(conn_id.to_string(), home.clone());
    Ok(home)
}

// ---------------------------------------------------------------------------
// 远程 pane 的启动器(原 `src-tauri/src/pty.rs::prepare_ssh_remote_launch`)
// ---------------------------------------------------------------------------

/// 远程启动器的最终形态:spawn 的程序、参数与(可选)用于 autofill 预注册的密码。
///
/// argv 拼装本身在 `mt_pty::ssh`(那一层只关心「用什么 argv 起子进程」);
/// 这里负责**查连接 → 探 ssh 客户端 → 私钥临时副本**这三件配置层的事,
/// 与原版 `prepare_ssh_remote_launch` 的分工一字不差。
#[derive(Debug, Clone)]
pub struct RemoteLaunch {
    pub program: String,
    pub args: Vec<String>,
    /// 明文登录密码(配置里没填则 `None`)。只交给 PTY 的 autofill 状态机,
    /// 不进 argv、不进环境变量、不写日志。
    pub password: Option<String>,
}

/// 把「连接 + 远程路径」解析成可 spawn 的远程启动器。
///
/// 失败面(两条,都给可直接展示的中文):
/// - 本机没有 OpenSSH 客户端;
/// - 私钥文件不存在 / 复制临时副本失败(`mt_core::prepare_ssh_key`)。
///
/// **断链**(连接被删)由更早的 [`find_connection`] 挡下,不在本函数里。
pub fn prepare_remote_launch(
    conn: &SshConnection,
    remote_path: &str,
) -> Result<RemoteLaunch, String> {
    let ssh_program = mt_pty::ssh::find_ssh_client().ok_or_else(|| {
        "未找到 ssh 客户端(OpenSSH)。Windows 10+ 可在「设置 → 系统 → 可选功能」中安装 \
        「OpenSSH 客户端」后重试"
            .to_string()
    })?;

    // 私钥复制为权限收紧的临时副本(绕过 OpenSSH 的 UNPROTECTED PRIVATE KEY 拒绝),
    // 复用既有 prepare_ssh_key;失败(源文件不存在等)直接报错。
    let identity = match conn
        .identity_file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(path) => Some(mt_core::prepare_ssh_key(path)?),
        None => None,
    };

    let args = mt_pty::ssh::build_ssh_launcher_args(
        &conn.host,
        conn.port,
        &conn.user,
        identity.as_deref(),
        remote_path,
    );

    Ok(RemoteLaunch {
        program: ssh_program.to_string_lossy().into_owned(),
        args,
        password: conn.password.clone().filter(|p| !p.is_empty()),
    })
}

// ---------------------------------------------------------------------------
// tests(全部自 `src-tauri/src/remote_ssh.rs` 的同名测试原样搬来,不触网)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
