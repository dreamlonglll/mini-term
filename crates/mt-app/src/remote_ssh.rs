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

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use mt_ai::sessions::{
    AiSession, AiSessionMessage, CachedSessions, MAX_SESSIONS_PER_SOURCE, MAX_TOTAL_SESSIONS,
    claude_message_from_line, claude_session_info_from_lines, codex_message_from_line,
    codex_meta_from_line, codex_user_title_from_line, encode_project_path, is_encoded_variant,
    normalize_unix_path, session_cache, session_id_path_safe,
};
use mt_config::SshConnection;
use mt_project::fs::{
    ALWAYS_IGNORE, FileContentResult, FileEntry, MAX_FILE_VIEW_SIZE, TextGitignore, natural_cmp,
};
use mt_ssh::sftp::{SftpBoundedFileRead, SftpFileReplaceResult};
use mt_ssh::{CachedSession, SftpHandle, SftpNodeKind, SshPool, run_bounded_exec_on_session};

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
// POSIX 路径纯函数(单测覆盖)
// ---------------------------------------------------------------------------

/// POSIX 路径拼接。`dir` 为绝对路径;根目录 `/` 不产生双斜杠。
pub fn join_posix(dir: &str, name: &str) -> String {
    let d = dir.trim_end_matches('/');
    if d.is_empty() {
        format!("/{name}")
    } else {
        format!("{d}/{name}")
    }
}

/// POSIX 路径父目录；不使用宿主平台 `Path`，因此远端文件名里的反斜杠不会在
/// Windows 客户端上被误当成分隔符。
pub fn parent_posix(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return None;
    }
    let index = trimmed.rfind('/')?;
    Some(if index == 0 {
        "/".into()
    } else {
        trimmed[..index].to_string()
    })
}

/// 计算 `full` 相对 `root` 的 POSIX 相对路径。不在 root 下返回 None。
/// **匹配 gitignore 必须用相对路径**:Windows 的 `Path` 语义对 POSIX 绝对路径
/// 有歧义(`/a/b` 在 Windows 上不是绝对路径),相对路径两平台行为一致。
pub fn posix_relative(root: &str, full: &str) -> Option<String> {
    let root_t = root.trim_end_matches('/');
    let full_t = full.trim_end_matches('/');
    if root_t.is_empty() {
        // root 是 `/`
        return Some(full_t.trim_start_matches('/').to_string());
    }
    if full_t == root_t {
        return Some(String::new());
    }
    full_t
        .strip_prefix(root_t)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(str::to_string)
}

/// 上传/下载冲突的用户选择。一次批处理内对所有剩余冲突沿用同一策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileConflictStrategy {
    Skip,
    Overwrite,
    KeepBoth,
}

#[derive(Debug, Clone, Default)]
pub struct FileOperationSummary {
    pub completed: usize,
    pub skipped: usize,
    pub failed: usize,
    pub bytes: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RemoteDirectoryEntry {
    pub name: String,
    pub path: String,
    pub is_symlink: bool,
}

#[derive(Debug, Clone)]
pub struct RemoteDirectoryListing {
    pub canonical_path: String,
    pub directories: Vec<RemoteDirectoryEntry>,
}

/// Opaque optimistic-concurrency token returned only for editable UTF-8 files.
/// Callers retain it with the draft and must send it back to
/// [`save_file_content`]. Raw bytes stay private so UI code cannot fabricate a
/// baseline for a binary or oversized file.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteFileBaseline {
    connection_id: String,
    connection_fingerprint: u64,
    canonical_root: String,
    canonical_path: String,
    bytes: Arc<[u8]>,
}

impl std::fmt::Debug for RemoteFileBaseline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteFileBaseline")
            .field("connection_id", &self.connection_id)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}

impl RemoteFileBaseline {
    /// Number of raw remote bytes represented by this baseline.
    #[cfg(test)]
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }
}

/// Result of a bounded remote document read. `baseline` is present only when
/// `content` is editable UTF-8 text.
#[derive(Clone)]
pub struct RemoteFileReadResult {
    pub content: FileContentResult,
    pub baseline: Option<RemoteFileBaseline>,
}

impl std::fmt::Debug for RemoteFileReadResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteFileReadResult")
            .field("content_len", &self.content.content.len())
            .field("is_binary", &self.content.is_binary)
            .field("too_large", &self.content.too_large)
            .field("has_baseline", &self.baseline.is_some())
            .finish()
    }
}

/// A normal save either commits and returns the next baseline or reports the
/// current remote value without modifying it. The caller may reload `current`
/// or explicitly retry [`save_file_content`] with `force = true`.
#[derive(Debug, Clone)]
pub enum RemoteFileSaveResult {
    Saved {
        baseline: RemoteFileBaseline,
        warning: Option<String>,
    },
    ExternalChange {
        current: RemoteFileReadResult,
    },
}

fn valid_remote_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains(':')
        && !name.contains('\0')
}

fn split_posix_leaf(path: &str) -> Result<(&str, &str), String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return Err("远程根目录不能作为文件条目操作".into());
    }
    let index = trimmed
        .rfind('/')
        .ok_or_else(|| format!("远程路径必须是绝对路径: {path}"))?;
    let parent = if index == 0 { "/" } else { &trimmed[..index] };
    let name = &trimmed[index + 1..];
    if !valid_remote_name(name) {
        return Err(format!("远程文件名无效: {name}"));
    }
    Ok((parent, name))
}

fn normalize_absolute_posix(path: &str) -> Result<String, String> {
    if !path.starts_with('/') {
        return Err(format!("远程路径必须是绝对路径: {path}"));
    }
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => return Err(format!("远程路径不能包含 `..`: {path}")),
            value if value.contains('\0') => return Err("远程路径不能包含 NUL".into()),
            value => segments.push(value),
        }
    }
    if segments.is_empty() {
        Ok("/".into())
    } else {
        Ok(format!("/{}", segments.join("/")))
    }
}

async fn canonical_project_root(sftp: &SftpHandle, project_root: &str) -> Result<String, String> {
    let normalized = normalize_absolute_posix(project_root)?;
    sftp.canonicalize(&normalized)
        .await
        .map_err(|e| format!("远程项目根不可访问: {}", e.message()))
}

async fn validate_remote_dir_under_root(
    sftp: &SftpHandle,
    project_root: &str,
    dir: &str,
) -> Result<String, String> {
    let root = canonical_project_root(sftp, project_root).await?;
    let normalized = normalize_absolute_posix(dir)?;
    let canonical = sftp
        .canonicalize(&normalized)
        .await
        .map_err(|e| format!("远程目录不可访问: {}", e.message()))?;
    if posix_relative(&root, &canonical).is_none() {
        return Err(format!("远程目录超出项目范围: {canonical}"));
    }
    let is_dir = sftp
        .is_dir(&canonical)
        .await
        .map_err(|e| format!("远程目录不可访问: {}", e.message()))?;
    if !is_dir {
        return Err(format!("远程路径不是目录: {canonical}"));
    }
    Ok(canonical)
}

async fn validate_remote_leaf_under_root(
    sftp: &SftpHandle,
    project_root: &str,
    path: &str,
) -> Result<String, String> {
    let root = canonical_project_root(sftp, project_root).await?;
    validate_remote_leaf_against_root(sftp, &root, path).await
}

async fn validate_remote_leaf_against_root(
    sftp: &SftpHandle,
    canonical_root: &str,
    path: &str,
) -> Result<String, String> {
    let normalized = normalize_absolute_posix(path)?;
    if normalized == canonical_root {
        return Err("不能操作远程项目根目录".into());
    }
    let (parent, name) = split_posix_leaf(&normalized)?;
    let canonical_parent = sftp
        .canonicalize(parent)
        .await
        .map_err(|e| format!("远程父目录不可访问: {}", e.message()))?;
    if posix_relative(canonical_root, &canonical_parent).is_none() {
        return Err(format!("远程路径超出项目范围: {normalized}"));
    }
    Ok(join_posix(&canonical_parent, name))
}

async fn canonical_remote_document_root(
    sftp: &SftpHandle,
    project_root: &str,
) -> Result<String, String> {
    let canonical_root = canonical_project_root(sftp, project_root).await?;
    match sftp
        .node_kind(&canonical_root)
        .await
        .map_err(|error| format!("远程项目根不可访问: {}", error.message()))?
    {
        SftpNodeKind::Directory => Ok(canonical_root),
        _ => Err(format!("远程项目根不是目录: {canonical_root}")),
    }
}

async fn validate_remote_document_file_against_root(
    sftp: &SftpHandle,
    canonical_root: &str,
    path: &str,
) -> Result<String, String> {
    let target = validate_remote_leaf_against_root(sftp, canonical_root, path).await?;
    sftp.guard_file_replacement_state(&target)
        .await
        .map_err(|error| format!("远程文件存在未决的保存恢复状态: {}", error.message()))?;
    match sftp
        .node_kind(&target)
        .await
        .map_err(|error| format!("远程文件不可访问: {}", error.message()))?
    {
        SftpNodeKind::File => Ok(target),
        SftpNodeKind::Directory => Err(format!("远程路径不是文件: {target}")),
        SftpNodeKind::Symlink => Err(format!("远程文件不能是符号链接: {target}")),
        SftpNodeKind::Other => Err(format!("远程路径不是普通文件: {target}")),
    }
}

fn build_remote_file_read_result(
    conn: &SshConnection,
    canonical_root: String,
    canonical_path: String,
    read: SftpBoundedFileRead,
) -> RemoteFileReadResult {
    let fingerprint = connection_fingerprint(conn);
    match read {
        SftpBoundedFileRead::TooLarge => RemoteFileReadResult {
            content: FileContentResult {
                content: String::new(),
                is_binary: false,
                too_large: true,
            },
            baseline: None,
        },
        SftpBoundedFileRead::Complete(bytes) => {
            let decoded = std::str::from_utf8(&bytes).map(str::to_owned);
            match decoded {
                Ok(content) => {
                    let baseline = RemoteFileBaseline {
                        connection_id: conn.id.clone(),
                        connection_fingerprint: fingerprint,
                        canonical_root: canonical_root.clone(),
                        canonical_path: canonical_path.clone(),
                        bytes: Arc::from(bytes),
                    };
                    RemoteFileReadResult {
                        content: FileContentResult {
                            content,
                            is_binary: false,
                            too_large: false,
                        },
                        baseline: Some(baseline),
                    }
                }
                Err(_) => RemoteFileReadResult {
                    content: FileContentResult {
                        content: String::new(),
                        is_binary: true,
                        too_large: false,
                    },
                    baseline: None,
                },
            }
        }
    }
}

fn validate_remote_file_baseline_connection(
    conn: &SshConnection,
    baseline: &RemoteFileBaseline,
) -> Result<(), String> {
    if conn.id != baseline.connection_id {
        return Err("远程文件所属 SSH 连接已变化，请重新打开文件".into());
    }
    if connection_fingerprint(conn) != baseline.connection_fingerprint {
        return Err("SSH 连接配置已变化，请重新打开远程文件后再保存".into());
    }
    Ok(())
}

fn validate_remote_file_baseline_path(
    baseline: &RemoteFileBaseline,
    canonical_root: &str,
    canonical_path: &str,
) -> Result<(), String> {
    if canonical_root != baseline.canonical_root {
        return Err("远程项目根已变化，请重新打开文件".into());
    }
    if canonical_path != baseline.canonical_path {
        return Err("远程文件路径身份已变化，请重新打开文件".into());
    }
    Ok(())
}

fn should_block_remote_save(
    current: &SftpBoundedFileRead,
    expected: &RemoteFileBaseline,
    force: bool,
) -> bool {
    match current {
        // “仍然覆盖”只跳过内容相等比较，不得跳过目标文件大小上限。
        SftpBoundedFileRead::TooLarge => true,
        SftpBoundedFileRead::Complete(_) if force => false,
        SftpBoundedFileRead::Complete(_) => !current.matches_bytes(expected.bytes.as_ref()),
    }
}

async fn read_remote_file_with_sftp(
    conn: &SshConnection,
    sftp: &SftpHandle,
    project_root: &str,
    path: &str,
) -> Result<RemoteFileReadResult, String> {
    let canonical_root = canonical_remote_document_root(sftp, project_root).await?;
    let canonical_path =
        validate_remote_document_file_against_root(sftp, &canonical_root, path).await?;
    let read = sftp
        .read_file_bounded(&canonical_path, REMOTE_DOCUMENT_MAX_BYTES)
        .await
        .map_err(|error| format!("读取远程文件失败: {}", error.message()))?;

    let root_after_read = canonical_remote_document_root(sftp, project_root).await?;
    let path_after_read =
        validate_remote_document_file_against_root(sftp, &root_after_read, path).await?;
    if root_after_read != canonical_root || path_after_read != canonical_path {
        return Err("远程文件路径在读取期间发生变化，请重试".into());
    }

    Ok(build_remote_file_read_result(
        conn,
        canonical_root,
        canonical_path,
        read,
    ))
}

/// Read one remote editor document with the same 1 MiB text/binary/oversize
/// contract as `mt_project::fs::read_file_content`.
///
/// This is a synchronous service boundary and must run on GPUI's background
/// executor. It opens/reuses the pooled SSH session, canonicalizes the project
/// root and parent, rejects symlink/special leaves, and reads at most 1 MiB + 1
/// byte.
pub fn read_file_content(
    conn: &SshConnection,
    project_root: &str,
    path: &str,
) -> Result<RemoteFileReadResult, String> {
    let st = state();
    st.block_on(async {
        let sftp = open_sftp(st, conn).await?;
        read_remote_file_with_sftp(conn, &sftp, project_root, path).await
    })
}

/// Safely save one previously loaded remote UTF-8 document.
///
/// A normal save (`force = false`) re-reads the bounded remote contents and
/// returns [`RemoteFileSaveResult::ExternalChange`] instead of writing when the
/// byte baseline changed. `force = true` skips only that byte comparison; it
/// still repeats connection, canonical-root, canonical-leaf, regular-file, and
/// size validation before a same-directory staged backup-swap.
pub fn save_file_content(
    conn: &SshConnection,
    project_root: &str,
    path: &str,
    content: &str,
    expected: &RemoteFileBaseline,
    force: bool,
) -> Result<RemoteFileSaveResult, String> {
    if content.len() > REMOTE_DOCUMENT_MAX_BYTES {
        return Err("内容过大(>1MB)，拒绝写入远程文件".into());
    }
    if expected.bytes.len() > REMOTE_DOCUMENT_MAX_BYTES {
        return Err("远程文件基线无效，请重新打开文件".into());
    }
    validate_remote_file_baseline_connection(conn, expected)?;

    let st = state();
    st.block_on(async {
        let sftp = open_sftp(st, conn).await?;
        let canonical_root = canonical_remote_document_root(&sftp, project_root).await?;
        let canonical_path =
            validate_remote_document_file_against_root(&sftp, &canonical_root, path).await?;
        validate_remote_file_baseline_path(expected, &canonical_root, &canonical_path)?;

        let current = sftp
            .read_file_bounded(&canonical_path, REMOTE_DOCUMENT_MAX_BYTES)
            .await
            .map_err(|error| format!("保存前读取远程文件失败: {}", error.message()))?;

        let root_after_read = canonical_remote_document_root(&sftp, project_root).await?;
        let path_after_read =
            validate_remote_document_file_against_root(&sftp, &root_after_read, path).await?;
        validate_remote_file_baseline_path(expected, &root_after_read, &path_after_read)?;

        if force && matches!(&current, SftpBoundedFileRead::TooLarge) {
            return Err(REMOTE_DOCUMENT_TOO_LARGE_SAVE_ERROR.into());
        }
        if should_block_remote_save(&current, expected, force) {
            return Ok(RemoteFileSaveResult::ExternalChange {
                current: build_remote_file_read_result(
                    conn,
                    root_after_read,
                    path_after_read,
                    current,
                ),
            });
        }

        // The optimistic content comparison above is not a transaction. Repeat
        // identity/type validation immediately before constructing and
        // promoting the staging file so a changed parent or leaf never inherits
        // the earlier check.
        let commit_root = canonical_remote_document_root(&sftp, project_root).await?;
        let commit_path =
            validate_remote_document_file_against_root(&sftp, &commit_root, path).await?;
        validate_remote_file_baseline_path(expected, &commit_root, &commit_path)?;
        let expected_at_commit = (!force).then_some(expected.bytes.as_ref());
        let replace_result = sftp
            .replace_file_contents(
                &commit_path,
                content.as_bytes(),
                REMOTE_DOCUMENT_MAX_BYTES,
                expected_at_commit,
            )
            .await
            .map_err(|error| format!("保存远程文件失败: {}", error.message()))?;
        let cleanup_warning = match replace_result {
            SftpFileReplaceResult::ExternalChange(current) => {
                if force && matches!(&current, SftpBoundedFileRead::TooLarge) {
                    return Err(REMOTE_DOCUMENT_TOO_LARGE_SAVE_ERROR.into());
                }
                let root_after_staging =
                    canonical_remote_document_root(&sftp, project_root).await?;
                let path_after_staging =
                    validate_remote_document_file_against_root(&sftp, &root_after_staging, path)
                        .await?;
                validate_remote_file_baseline_path(
                    expected,
                    &root_after_staging,
                    &path_after_staging,
                )?;
                return Ok(RemoteFileSaveResult::ExternalChange {
                    current: build_remote_file_read_result(
                        conn,
                        root_after_staging,
                        path_after_staging,
                        current,
                    ),
                });
            }
            SftpFileReplaceResult::Replaced { cleanup_warning } => cleanup_warning,
        };

        Ok(RemoteFileSaveResult::Saved {
            baseline: RemoteFileBaseline {
                connection_id: conn.id.clone(),
                connection_fingerprint: connection_fingerprint(conn),
                canonical_root: commit_root,
                canonical_path: commit_path,
                bytes: Arc::from(content.as_bytes()),
            },
            warning: cleanup_warning,
        })
    })
}

/// VS Code 风格的同名副本名。目录与文件共用，文件保留最后一个扩展名。
pub fn keep_both_name(name: &str, ordinal: usize) -> String {
    let suffix = if ordinal <= 1 {
        " copy".to_string()
    } else {
        format!(" copy {ordinal}")
    };
    if name.starts_with('.') && !name[1..].contains('.') {
        return format!("{name}{suffix}");
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => {
            format!("{stem}{suffix}.{ext}")
        }
        _ => format!("{name}{suffix}"),
    }
}

async fn keep_both_remote_path(sftp: &SftpHandle, desired: &str) -> Result<String, String> {
    let (parent, name) = split_posix_leaf(desired)?;
    let existing: HashSet<String> = sftp
        .read_dir(parent)
        .await
        .map_err(|e| format!("读取远程目录失败: {}", e.message()))?
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    if !existing.contains(name) {
        return Ok(desired.to_string());
    }
    for ordinal in 1..=10_000 {
        let candidate = keep_both_name(name, ordinal);
        if !existing.contains(&candidate) {
            return Ok(join_posix(parent, &candidate));
        }
    }
    Err(format!("无法为远程条目生成可用副本名: {desired}"))
}

/// 把 `~` / `~/xxx` 展开为远程绝对路径(home 来自 SFTP canonicalize(".")).
/// 空输入视同 `~`;非 `~` 前缀原样返回(交给 SFTP canonicalize 处理相对路径)。
fn expand_tilde(path: &str, home: &str) -> String {
    let home_t = home.trim_end_matches('/');
    let home_norm = if home_t.is_empty() { "/" } else { home_t };
    let p = path.trim();
    if p.is_empty() || p == "~" {
        return home_norm.to_string();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        let rest = rest.trim_start_matches('/');
        if rest.is_empty() {
            return home_norm.to_string();
        }
        return join_posix(home_norm, rest);
    }
    p.to_string()
}

/// 把配置里的「远程粘贴落盘目录」解析成远端绝对路径。
///
/// 三种写法(对齐 `AppConfig::remote_paste_dir` 的文档):
/// - 相对路径 `.mini-term/pasted` → 相对**项目根**展开(默认形态,图片落项目内)
/// - `~` / `~/xxx` → 远程 home 展开
/// - 绝对路径 `/tmp/mini-term` → 原样
///
/// **保证返回的路径不含 `..` 段**。这条路径最终会拼进 SFTP **写**操作 ——
/// 逃出项目根 / home 的写入不是这个功能该有的能力,宁可报错。
/// 判定放在归一之后,`project_path`(调用方传入)带 `..` 的情形一并挡掉,
/// 而不只是校验用户填的 `dest_dir`。
fn resolve_paste_dir(project_path: &str, home: &str, dest_dir: &str) -> Result<String, String> {
    // 用户可能顺手填了反斜杠,统一成 POSIX 分隔符再判定。
    let raw = dest_dir.trim().replace('\\', "/");
    let raw = if raw.trim().is_empty() {
        DEFAULT_REMOTE_PASTE_DIR.to_string()
    } else {
        raw
    };

    let abs = if raw.starts_with('/') {
        raw.clone()
    } else if raw == "~" || raw.starts_with("~/") {
        expand_tilde(&raw, home)
    } else {
        // 相对项目根。项目根必须是绝对路径(添加远程项目时已 canonicalize)。
        if !project_path.starts_with('/') {
            return Err(format!("远程项目路径不是绝对路径: {project_path}"));
        }
        join_posix(project_path, raw.trim_start_matches('/'))
    };

    // 归一:丢掉空段与 `.` 段。`./x` 和 `x` 必须解析成同一条路径,否则
    // `/proj/.` 这种写法会绕过下游「目录是否严格位于项目内」的判定。
    // 注意 `.` / `..` 都是**整段**比较,`.mini-term` 这类点开头的目录名不受影响。
    let normalized: Vec<&str> = abs
        .split('/')
        .filter(|seg| !seg.is_empty() && *seg != ".")
        .collect();
    if normalized.is_empty() {
        return Err("远程粘贴目录解析为空".into());
    }
    // 归一后再查 `..`:此时 dest_dir 与 project_path 两部分都已合入 abs,
    // 一处判定覆盖两个来源。
    if normalized.contains(&"..") {
        return Err("远程粘贴目录不能包含 `..`".into());
    }
    Ok(format!("/{}", normalized.join("/")))
}

/// 从本地临时文件路径提取文件名。两种分隔符都切 —— 传进来的是 Windows 路径,
/// 不能让 `\` 残留在远端路径里。
fn paste_file_name(local_path: &str) -> Result<String, String> {
    let name = local_path.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("无法从本地路径提取文件名: {local_path}"));
    }
    Ok(name.to_string())
}

/// UNIX 秒 → ISO 8601 UTC 字符串(`YYYY-MM-DDTHH:MM:SSZ`)。
/// 会话缺失 timestamp 字段时用文件 mtime 兜底,保证时间混排仍可比较。
fn unix_secs_to_iso(secs: u64) -> String {
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    fn is_leap(year: u64) -> bool {
        (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
    }
    let mut year = 1970u64;
    let mut day_of_year = days;
    loop {
        let year_len = if is_leap(year) { 366 } else { 365 };
        if day_of_year < year_len {
            break;
        }
        day_of_year -= year_len;
        year += 1;
    }
    let leap = is_leap(year);
    let month_lens = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0usize;
    while month < 12 && day_of_year >= month_lens[month] {
        day_of_year -= month_lens[month];
        month += 1;
    }
    format!(
        "{year:04}-{:02}-{:02}T{hh:02}:{mm:02}:{ss:02}Z",
        month + 1,
        day_of_year + 1,
    )
}

/// 取字节缓冲中「完整行」前缀:截到最后一个 `\n`(含)。返回 (consumed, 完整行切片)。
/// 尾部无换行的半行不解析、不计入 consumed —— 会话文件可能正被写入,半行下次再读,
/// 保证增量读取不重复、不丢消息(JSONL 每行都以 `\n` 结束)。
fn split_complete_lines(bytes: &[u8]) -> (usize, &[u8]) {
    match bytes.iter().rposition(|&b| b == b'\n') {
        Some(i) => (i + 1, &bytes[..i + 1]),
        None => (0, &[]),
    }
}

/// codex rollout 文件名是否以该 session id 结尾(`rollout-<ts>-<id>.jsonl`)。
fn codex_filename_matches_session(path: &str, session_id: &str) -> bool {
    if session_id.is_empty() {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".jsonl")
        .map(|stem| stem.ends_with(session_id))
        .unwrap_or(false)
}

/// 解析 codex session_index.jsonl 内容 → { id: thread_name }。
fn parse_codex_thread_names(content: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in content.lines() {
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(line)
            && let (Some(id), Some(name)) = (
                obj.get("id").and_then(|v| v.as_str()),
                obj.get("thread_name").and_then(|v| v.as_str()),
            )
        {
            map.insert(id.to_string(), name.to_string());
        }
    }
    map
}

// ---------------------------------------------------------------------------
// 入口 1:远程文件树
// ---------------------------------------------------------------------------

/// SFTP readdir 远程目录,返回与本地 `mt_project::fs::list_directory` 同构的
/// [`FileEntry`] 列表。
///
/// 忽略过滤 = 项目根 `.gitignore`(读一次、按 connId+projectRoot 缓存)
/// + [`ALWAYS_IGNORE`] 固定黑名单(目录直接隐藏)。
///
/// `refresh_ignore=true` 强制重读 .gitignore(树顶手动刷新按钮用)。
///
/// **阻塞**,丢 `background_executor`。
pub fn list_directory(
    conn: &SshConnection,
    path: &str,
    project_root: &str,
    refresh_ignore: bool,
) -> Result<Vec<FileEntry>, String> {
    let st = state();
    let ignore_key = format!("{}|{}", conn.id, normalize_unix_path(project_root));
    if refresh_ignore {
        lock(&st.gitignore_cache).remove(&ignore_key);
    }
    // 锁即取即放;miss 时在 SFTP 打开后无锁读取,再短暂加锁写回。
    let cached_ignore = lock(&st.gitignore_cache).get(&ignore_key).cloned();

    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let gitignore = match cached_ignore {
                Some(g) => g,
                None => {
                    let gi_path = join_posix(project_root, ".gitignore");
                    // .gitignore 不存在 / 读失败 → 空规则,静默降级。
                    let content = match sftp.read_head(&gi_path, GITIGNORE_MAX_BYTES).await {
                        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                        Err(_) => String::new(),
                    };
                    let g = Arc::new(TextGitignore::from_text(&content));
                    lock(&st.gitignore_cache).insert(ignore_key.clone(), g.clone());
                    g
                }
            };

            let entries = sftp
                .read_dir(path)
                .await
                .map_err(|e| format!("读取远程目录失败: {}", e.message()))?;

            let mut out: Vec<FileEntry> = entries
                .into_iter()
                .filter_map(|e| {
                    // FileTree 目前用宿主 `PathBuf` 承载远程路径；反斜杠在 Windows
                    // 会被解释成分隔符，因此无法无损、安全地操作这类远程名称。
                    if !valid_remote_name(&e.name) {
                        return None;
                    }
                    // ALWAYS_IGNORE 目录完全隐藏(与本地树一致)
                    if e.is_dir && ALWAYS_IGNORE.contains(&e.name.as_str()) {
                        return None;
                    }
                    let full = join_posix(path, &e.name);
                    let ignored = posix_relative(project_root, &full)
                        .map(|rel| gitignore.is_ignored(&rel, e.is_dir))
                        .unwrap_or(false);
                    Some(FileEntry {
                        name: e.name,
                        // 远程路径是 POSIX 字符串,`PathBuf` 在这里只是容器 ——
                        // 拼接一律走 `join_posix`,绝不用 `Path::join`(会插 `\`)。
                        path: PathBuf::from(full),
                        is_dir: e.is_dir,
                        ignored,
                    })
                })
                .collect();
            out.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.ignored.cmp(&b.ignored))
                    .then_with(|| natural_cmp(&a.name, &b.name))
            });
            Ok(out)
        }
        .await;
        sftp.close().await;
        result
    })
}

/// 列一个目录的**分流开关**:远程项目走上面的 SFTP 那条,本地项目走
/// [`mt_project::fs::list_directory`]。两条路返回同一个 [`FileEntry`]。
///
/// 文件树只需问一次「这个项目有没有远程连接」
/// ([`AppStore::remote_connection_of`](crate::store::AppStore::remote_connection_of),
/// 断链时是 `None`)就能共用同一段加载代码 —— 分流判据只有这一处,不会出现
/// 「树顶刷新走了本地、展开子目录走了远程」这类半截状态。
///
/// 断链项目由 FileTree 在进入此分流函数前拦住，绝不会把远程 POSIX 路径当成本机
/// 路径读取。
///
/// **阻塞**,丢 `background_executor`。
pub fn list_directory_for(
    remote: Option<&SshConnection>,
    project_root: &std::path::Path,
    dir: &std::path::Path,
    refresh_ignore: bool,
) -> Result<Vec<FileEntry>, String> {
    match remote {
        Some(conn) => list_directory(
            conn,
            &dir.to_string_lossy(),
            &project_root.to_string_lossy(),
            refresh_ignore,
        ),
        None => mt_project::fs::list_directory(project_root, dir).map_err(|e| format!("{e:#}")),
    }
}

// ---------------------------------------------------------------------------
// 入口 2:远程目录验证(「添加远程项目」保存前)
// ---------------------------------------------------------------------------

/// 验证远程路径是一个存在的目录,返回展开后的绝对路径。
/// `~` / `~/xxx` 用 SFTP canonicalize 展开;不存在或不是目录返回 Err。
///
/// 兼作**连接测试**:走完整的「取 session → 认证 → 开 SFTP → canonicalize」,
/// 连不上时的错误面与真实使用一致(原版没有独立的 test 命令,同一条路)。
///
/// **阻塞**,丢 `background_executor`。
pub fn validate_dir(conn: &SshConnection, path: &str) -> Result<String, String> {
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let trimmed = path.trim();
            let expanded = if trimmed.is_empty() || trimmed == "~" || trimmed.starts_with("~/") {
                let home = remote_home(st, &sftp, &conn.id).await?;
                expand_tilde(trimmed, &home)
            } else {
                trimmed.to_string()
            };
            let canonical = sftp
                .canonicalize(&expanded)
                .await
                .map_err(|e| format!("远程路径无效: {}", e.message()))?;
            let is_dir = sftp
                .is_dir(&canonical)
                .await
                .map_err(|e| format!("远程路径不可访问: {}", e.message()))?;
            if !is_dir {
                return Err(format!("远程路径不是目录: {canonical}"));
            }
            Ok(canonical)
        }
        .await;
        sftp.close().await;
        result
    })
}

/// 为“新建远程项目”提供的轻量目录浏览；不应用项目 `.gitignore` 或固定隐藏目录。
/// **阻塞**,调用方必须放到 background executor。
pub fn browse_directory(
    conn: &SshConnection,
    requested_path: &str,
) -> Result<RemoteDirectoryListing, String> {
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let trimmed = requested_path.trim();
            let expanded = if trimmed.is_empty() || trimmed == "~" || trimmed.starts_with("~/") {
                let home = remote_home(st, &sftp, &conn.id).await?;
                expand_tilde(trimmed, &home)
            } else {
                trimmed.to_string()
            };
            let canonical = sftp
                .canonicalize(&expanded)
                .await
                .map_err(|e| format!("远程路径无效: {}", e.message()))?;
            if !sftp
                .is_dir(&canonical)
                .await
                .map_err(|e| format!("远程路径不可访问: {}", e.message()))?
            {
                return Err(format!("远程路径不是目录: {canonical}"));
            }
            let entries = sftp
                .read_dir(&canonical)
                .await
                .map_err(|e| format!("读取远程目录失败: {}", e.message()))?;
            let mut directories = Vec::new();
            for entry in entries {
                if !valid_sftp_child_name(&entry.name) {
                    continue;
                }
                let path = join_posix(&canonical, &entry.name);
                let browsable =
                    entry.is_dir || (entry.is_symlink && sftp.is_dir(&path).await.unwrap_or(false));
                if !browsable {
                    continue;
                }
                directories.push(RemoteDirectoryEntry {
                    path,
                    name: entry.name,
                    is_symlink: entry.is_symlink,
                });
            }
            directories.sort_by(|a, b| natural_cmp(&a.name, &b.name));
            Ok(RemoteDirectoryListing {
                canonical_path: canonical,
                directories,
            })
        }
        .await;
        sftp.close().await;
        result
    })
}

/// 在远程项目目录中新建文件或文件夹。
pub fn create_entry(
    conn: &SshConnection,
    project_root: &str,
    parent_dir: &str,
    name: &str,
    is_dir: bool,
) -> Result<String, String> {
    if !valid_remote_name(name) {
        return Err(format!("文件名无效: {name}"));
    }
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let parent = validate_remote_dir_under_root(&sftp, project_root, parent_dir).await?;
            let target = join_posix(&parent, name);
            if is_dir {
                sftp.create_dir(&target)
                    .await
                    .map_err(|e| format!("创建远程文件夹失败: {}", e.message()))?;
            } else {
                sftp.create_file(&target)
                    .await
                    .map_err(|e| format!("创建远程文件失败: {}", e.message()))?;
            }
            Ok(target)
        }
        .await;
        sftp.close().await;
        result
    })
}

/// 重命名远程条目；新名称只允许单个 POSIX basename。
pub fn rename_entry(
    conn: &SshConnection,
    project_root: &str,
    path: &str,
    new_name: &str,
) -> Result<String, String> {
    if !valid_remote_name(new_name) {
        return Err(format!("文件名无效: {new_name}"));
    }
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let source = validate_remote_leaf_under_root(&sftp, project_root, path).await?;
            let (parent, _) = split_posix_leaf(&source)?;
            let target = join_posix(parent, new_name);
            sftp.rename(&source, &target)
                .await
                .map_err(|e| format!("重命名远程条目失败: {}", e.message()))?;
            Ok(target)
        }
        .await;
        sftp.close().await;
        result
    })
}

async fn remove_remote_tree(
    sftp: &SftpHandle,
    target: String,
    target_kind: SftpNodeKind,
) -> Result<usize, String> {
    sftp.remove_tree(&target, target_kind)
        .await
        .map_err(|e| format!("删除远程条目失败: {}", e.message()))
}

fn valid_sftp_child_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

fn split_sftp_leaf(path: &str) -> Result<(&str, &str), String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return Err("远程根目录不能作为文件条目操作".into());
    }
    let index = trimmed
        .rfind('/')
        .ok_or_else(|| format!("远程路径必须是绝对路径: {path}"))?;
    let parent = if index == 0 { "/" } else { &trimmed[..index] };
    let name = &trimmed[index + 1..];
    if !valid_sftp_child_name(name) {
        return Err(format!("服务器返回了无效目录项名: {name:?}"));
    }
    Ok((parent, name))
}

async fn remote_kind_if_present(
    sftp: &SftpHandle,
    path: &str,
) -> Result<Option<SftpNodeKind>, String> {
    split_sftp_leaf(path)?;
    sftp.try_node_kind(path)
        .await
        .map_err(|e| format!("读取远程条目类型失败: {}", e.message()))
}

async fn validate_remote_delete_leaf_against_root(
    sftp: &SftpHandle,
    canonical_root: &str,
    path: &str,
) -> Result<String, String> {
    let normalized = normalize_absolute_posix(path)?;
    if normalized == canonical_root {
        return Err("不能操作远程项目根目录".into());
    }
    let (parent, name) = split_sftp_leaf(&normalized)?;
    let canonical_parent = sftp
        .canonicalize(parent)
        .await
        .map_err(|e| format!("远程父目录不可访问: {}", e.message()))?;
    if canonical_parent != parent {
        return Err(format!(
            "远程父目录在删除期间被符号链接替换或重定向: {parent}"
        ));
    }
    if posix_relative(canonical_root, &canonical_parent).is_none() {
        return Err(format!("远程路径超出项目范围: {normalized}"));
    }
    Ok(join_posix(&canonical_parent, name))
}

async fn validate_remote_delete_directory_identity(
    sftp: &SftpHandle,
    canonical_root: &str,
    path: &str,
) -> Result<String, String> {
    let validated = validate_remote_delete_leaf_against_root(sftp, canonical_root, path).await?;
    let canonical = sftp
        .canonicalize(&validated)
        .await
        .map_err(|e| format!("远程目录不可访问: {}", e.message()))?;
    if canonical != validated || posix_relative(canonical_root, &canonical).is_none() {
        return Err(format!(
            "远程目录在删除期间被替换或移出项目范围: {validated}"
        ));
    }
    if remote_kind_if_present(sftp, &validated).await? != Some(SftpNodeKind::Directory) {
        return Err(format!("远程目录在删除期间发生变化: {validated}"));
    }
    Ok(validated)
}

fn shell_quote_posix(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn remote_delete_command(
    target: &str,
    proof_path: &str,
    proof_nonce: &str,
) -> Result<String, String> {
    let (parent, name) = split_sftp_leaf(target)?;
    let (proof_parent, proof_name) = split_sftp_leaf(proof_path)?;
    if proof_parent != parent {
        return Err("远程删除验证标记必须与目标位于同一目录".into());
    }
    let relative = format!("./{name}");
    let proof_relative = format!("./{proof_name}");
    Ok(format!(
        "cd -P {} 2>/dev/null && [ \"$(pwd -P)\" = {} ] && \
         [ -d {} ] && [ ! -L {} ] && [ -f {} ] && [ ! -L {} ] && \
         [ \"$(cat -- {})\" = {} ] && rm -f -- {} && \
         exec timeout {} rm -rf -- {}",
        shell_quote_posix(parent),
        shell_quote_posix(parent),
        shell_quote_posix(&relative),
        shell_quote_posix(&relative),
        shell_quote_posix(&proof_relative),
        shell_quote_posix(&proof_relative),
        shell_quote_posix(&proof_relative),
        shell_quote_posix(proof_nonce),
        shell_quote_posix(&proof_relative),
        REMOTE_DELETE_SERVER_TIMEOUT_SECS,
        shell_quote_posix(&relative),
    ))
}

async fn create_remote_delete_proof(
    sftp: &SftpHandle,
    target: &str,
) -> Result<(String, String), String> {
    for _ in 0..16 {
        let proof_path = sftp.temporary_sibling_path(target, "delete-proof");
        let sequence = LOCAL_TRANSFER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let nonce = format!(
            "mt-delete-proof-{}-{timestamp}-{sequence}",
            std::process::id()
        );
        match sftp.write_new_file(&proof_path, nonce.as_bytes()).await {
            Ok(()) => return Ok((proof_path, nonce)),
            Err(error) => match sftp.try_node_kind(&proof_path).await {
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => {
                    return Err(format!("创建远程删除验证标记失败: {}", error.message()));
                }
            },
        }
    }
    Err("无法分配唯一的远程删除验证标记".into())
}

async fn cleanup_remote_delete_proof(sftp: &SftpHandle, proof_path: &str) -> Result<(), String> {
    match sftp
        .try_node_kind(proof_path)
        .await
        .map_err(|error| format!("检查远程删除验证标记失败: {}", error.message()))?
    {
        None => Ok(()),
        Some(SftpNodeKind::Directory) => Err(format!(
            "远程删除验证标记被替换为目录，已拒绝清理: {proof_path}"
        )),
        Some(_) => sftp
            .remove_file(proof_path)
            .await
            .map_err(|error| format!("清理远程删除验证标记失败: {}", error.message())),
    }
}

fn remote_exec_failure_detail(output: &mt_ssh::BoundedExecOutput) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let mut detail = if output.timed_out {
        "服务端删除命令超时，远端状态仍需确认".to_string()
    } else {
        match output.exit_code {
            Some(code) => format!("服务端删除命令退出码: {code}"),
            None => "服务端删除命令未返回退出码".to_string(),
        }
    };
    if !stderr.is_empty() {
        detail.push_str("; stderr: ");
        detail.push_str(stderr);
    }
    detail
}

async fn remove_remote_tree_safely(
    sftp: &SftpHandle,
    canonical_root: &str,
    target: &str,
) -> Result<usize, String> {
    enum RemoveWork {
        Visit(String),
        RemoveDirectory(String),
    }

    let mut stack = vec![RemoveWork::Visit(target.to_string())];
    let mut removed = 0usize;
    while let Some(work) = stack.pop() {
        match work {
            RemoveWork::Visit(path) => {
                let path =
                    validate_remote_delete_leaf_against_root(sftp, canonical_root, &path).await?;
                let Some(kind) = remote_kind_if_present(sftp, &path).await? else {
                    continue;
                };
                if kind == SftpNodeKind::Directory {
                    let path =
                        validate_remote_delete_directory_identity(sftp, canonical_root, &path)
                            .await?;
                    let entries = sftp
                        .read_dir(&path)
                        .await
                        .map_err(|e| format!("读取远程目录失败: {}", e.message()))?;
                    stack.push(RemoveWork::RemoveDirectory(path.clone()));
                    for entry in entries.into_iter().rev() {
                        if !valid_sftp_child_name(&entry.name) {
                            return Err(format!("服务器返回了无效目录项名: {:?}", entry.name));
                        }
                        stack.push(RemoveWork::Visit(join_posix(&path, &entry.name)));
                    }
                } else {
                    sftp.remove_file(&path)
                        .await
                        .map_err(|e| format!("删除远程条目失败: {}", e.message()))?;
                    removed += 1;
                }
            }
            RemoveWork::RemoveDirectory(path) => {
                let path =
                    validate_remote_delete_leaf_against_root(sftp, canonical_root, &path).await?;
                let Some(kind) = remote_kind_if_present(sftp, &path).await? else {
                    continue;
                };
                if kind == SftpNodeKind::Directory {
                    validate_remote_delete_directory_identity(sftp, canonical_root, &path).await?;
                    sftp.remove_dir(&path)
                        .await
                        .map_err(|e| format!("删除远程目录失败: {}", e.message()))?;
                } else {
                    sftp.remove_file(&path)
                        .await
                        .map_err(|e| format!("删除远程条目失败: {}", e.message()))?;
                }
                removed += 1;
            }
        }
    }
    Ok(removed)
}

async fn restore_isolated_remote_entry(
    sftp: &SftpHandle,
    isolation: &str,
    target: &str,
) -> Result<(), String> {
    if remote_kind_if_present(sftp, isolation).await?.is_none() {
        return Ok(());
    }
    if remote_kind_if_present(sftp, target).await?.is_some() {
        return Err(format!(
            "原路径已被重新创建，未覆盖；剩余条目保留在: {isolation}"
        ));
    }
    sftp.rename(isolation, target).await.map_err(|error| {
        format!(
            "恢复远程条目失败: {}; 剩余条目保留在: {isolation}",
            error.message()
        )
    })
}

async fn remove_remote_leaf_via_isolation(
    sftp: &SftpHandle,
    canonical_root: &str,
    target: &str,
) -> Result<usize, String> {
    let target = validate_remote_delete_leaf_against_root(sftp, canonical_root, target).await?;
    let isolation = loop {
        let candidate = sftp.temporary_sibling_path(&target, "delete-isolation");
        if remote_kind_if_present(sftp, &candidate).await?.is_none() {
            break candidate;
        }
    };
    sftp.rename(&target, &isolation)
        .await
        .map_err(|error| format!("隔离远程待删除条目失败: {}", error.message()))?;

    let isolated =
        validate_remote_delete_leaf_against_root(sftp, canonical_root, &isolation).await?;
    match remote_kind_if_present(sftp, &isolated).await? {
        Some(SftpNodeKind::Directory) => {
            let restore = restore_isolated_remote_entry(sftp, &isolated, &target).await;
            match restore {
                Ok(()) => Err("远程条目在删除期间变成了目录，已恢复原路径".into()),
                Err(restore_error) => Err(format!("远程条目在删除期间变成了目录；{restore_error}")),
            }
        }
        Some(_) => {
            if let Err(error) = sftp.remove_file(&isolated).await {
                let restore = restore_isolated_remote_entry(sftp, &isolated, &target).await;
                return match restore {
                    Ok(()) => Err(format!("删除远程条目失败: {}", error.message())),
                    Err(restore_error) => Err(format!(
                        "删除远程条目失败: {}; {restore_error}",
                        error.message()
                    )),
                };
            }
            Ok(1)
        }
        None => Ok(1),
    }
}

async fn remove_remote_directory_via_isolation(
    sftp: &SftpHandle,
    canonical_root: &str,
    target: &str,
) -> Result<usize, String> {
    let target = validate_remote_delete_directory_identity(sftp, canonical_root, target).await?;
    let isolation = loop {
        let candidate = sftp.temporary_sibling_path(&target, "delete-isolation");
        if remote_kind_if_present(sftp, &candidate).await?.is_none() {
            break candidate;
        }
    };
    sftp.rename(&target, &isolation)
        .await
        .map_err(|error| format!("隔离远程待删除目录失败: {}", error.message()))?;

    if let Err(error) =
        validate_remote_delete_directory_identity(sftp, canonical_root, &isolation).await
    {
        let restore = restore_isolated_remote_entry(sftp, &isolation, &target).await;
        return match restore {
            Ok(()) => Err(format!("隔离后的远程目录校验失败: {error}")),
            Err(restore_error) => Err(format!(
                "隔离后的远程目录校验失败: {error}; {restore_error}"
            )),
        };
    }

    match remove_remote_tree_safely(sftp, canonical_root, &isolation).await {
        Ok(removed) => Ok(removed),
        Err(error) => {
            let restore = restore_isolated_remote_entry(sftp, &isolation, &target).await;
            match restore {
                Ok(()) => Err(error),
                Err(restore_error) => Err(format!("{error}; {restore_error}")),
            }
        }
    }
}

async fn remove_remote_directory_via_fresh_session(
    st: &RemoteSshState,
    conn: &SshConnection,
    project_root: &str,
    target: &str,
) -> Result<usize, String> {
    let fresh_sftp = open_sftp(st, conn).await?;
    let result = async {
        let canonical_root = canonical_project_root(&fresh_sftp, project_root).await?;
        remove_remote_directory_via_isolation(&fresh_sftp, &canonical_root, target).await
    }
    .await;
    fresh_sftp.close().await;
    result
}

async fn delete_remote_directory(
    st: &RemoteSshState,
    conn: &SshConnection,
    project_root: &str,
    session: &Arc<CachedSession>,
    sftp: &SftpHandle,
    canonical_root: &str,
    target: &str,
) -> Result<usize, String> {
    // 只绑定并验证删除根目录。服务端 `rm` 自己完成递归；若先用 SFTP 扫描整棵树，
    // 大目录仍会因网络传输和目录往返退化为线性预处理，抵消快速路径的意义。
    let target = validate_remote_delete_directory_identity(sftp, canonical_root, target).await?;
    let capability = run_bounded_exec_on_session(
        session,
        "command -v timeout >/dev/null 2>&1 && command -v rm >/dev/null 2>&1 && \
         command -v cat >/dev/null 2>&1",
        REMOTE_DELETE_PROBE_TIMEOUT,
        1024,
    )
    .await;
    match capability {
        Ok(output)
            if !output.requires_session_retirement()
                && !output.timed_out
                && output.exit_code == Some(0) => {}
        Ok(output) if output.requires_session_retirement() => {
            st.pool().evict_if_same(&conn.id, session).await;
            return remove_remote_directory_via_fresh_session(st, conn, project_root, &target)
                .await;
        }
        Ok(_) => {
            return remove_remote_directory_via_isolation(sftp, canonical_root, &target).await;
        }
        Err(_) => {
            st.pool().evict_if_same(&conn.id, session).await;
            return remove_remote_directory_via_fresh_session(st, conn, project_root, &target)
                .await;
        }
    }

    let (proof_path, proof_nonce) = match create_remote_delete_proof(sftp, &target).await {
        Ok(proof) => proof,
        Err(_) => {
            return remove_remote_directory_via_isolation(sftp, canonical_root, &target).await;
        }
    };
    let command = remote_delete_command(&target, &proof_path, &proof_nonce)?;
    let execution = run_bounded_exec_on_session(
        session,
        &command,
        REMOTE_DELETE_EXEC_TIMEOUT,
        REMOTE_DELETE_OUTPUT_CAP,
    )
    .await;
    match &execution {
        Ok(output) if output.requires_session_retirement() => {
            st.pool().evict_if_same(&conn.id, session).await;
        }
        Err(_) => {
            st.pool().evict_if_same(&conn.id, session).await;
        }
        _ => {}
    }
    let proof_cleanup = cleanup_remote_delete_proof(sftp, &proof_path).await;
    let post_target =
        validate_remote_delete_leaf_against_root(sftp, canonical_root, &target).await?;
    if remote_kind_if_present(sftp, &post_target).await?.is_none() {
        if let Err(cleanup_error) = proof_cleanup {
            return Err(format!("远程目录已删除，但{cleanup_error}"));
        }
        // 调用方只关心成功与否；快速路径不为统计条目重新扫描整棵树。
        return Ok(1);
    }

    match execution {
        Ok(output) if output.safe_to_fallback() => {
            proof_cleanup?;
            remove_remote_directory_via_isolation(sftp, canonical_root, &target).await
        }
        Ok(output) if output.requires_session_retirement() && !output.state.may_have_started() => {
            proof_cleanup?;
            remove_remote_directory_via_fresh_session(st, conn, project_root, &target).await
        }
        Ok(output) => {
            let cleanup = proof_cleanup
                .err()
                .map(|error| format!("；{error}"))
                .unwrap_or_default();
            Err(format!(
                "{}；为避免与仍可能运行的服务端删除并发，未启动 SFTP 回退{cleanup}",
                remote_exec_failure_detail(&output)
            ))
        }
        Err(error) => {
            proof_cleanup?;
            remove_remote_directory_via_fresh_session(st, conn, project_root, &target)
                .await
                .map_err(|fallback_error| {
                    format!("服务端删除通道失败: {error}; SFTP 回退也失败: {fallback_error}")
                })
        }
    }
}

async fn discard_remote_staged_entry(sftp: &SftpHandle, staging: &str) -> Result<(), String> {
    let kind = sftp
        .node_kind(staging)
        .await
        .map_err(|e| format!("远程暂存条目不可访问: {}", e.message()))?;
    remove_remote_tree(sftp, staging.to_string(), kind)
        .await
        .map(|_| ())
}

async fn commit_new_remote_staged_directory(
    sftp: &SftpHandle,
    staging: &str,
    target: &str,
) -> Result<(), String> {
    if let Err(error) = sftp.rename(staging, target).await {
        let cleanup = discard_remote_staged_entry(sftp, staging).await;
        return match cleanup {
            Ok(()) => Err(format!("提交远程目录失败: {}", error.message())),
            Err(cleanup_error) => Err(format!(
                "提交远程目录失败: {}; 清理暂存目录也失败: {cleanup_error}",
                error.message()
            )),
        };
    }
    Ok(())
}

/// 删除远程文件、符号链接或目录。普通目录先校验删除根并用 SFTP nonce 证明 shell
/// 与 SFTP 看见同一父目录，再优先使用带 `timeout` 的服务端 `rm`；能力不可用时先
/// 原子改名到随机隔离路径，再用一个复用 SFTP handle 后序删除。叶子 symlink 只删除
/// 链接自身，路径式 fallback 的每一步仍会重新校验 canonical parent。
pub fn delete_entry(conn: &SshConnection, project_root: &str, path: &str) -> Result<usize, String> {
    let st = state();
    st.block_on(async move {
        let (session, sftp) = open_sftp_with_session(st, conn).await?;
        let result = async {
            let canonical_root = canonical_project_root(&sftp, project_root).await?;
            let target = validate_remote_leaf_against_root(&sftp, &canonical_root, path).await?;
            let kind = remote_kind_if_present(&sftp, &target)
                .await?
                .ok_or_else(|| format!("远程条目不存在: {target}"))?;
            if kind == SftpNodeKind::Directory {
                delete_remote_directory(
                    st,
                    conn,
                    project_root,
                    &session,
                    &sftp,
                    &canonical_root,
                    &target,
                )
                .await
            } else {
                remove_remote_leaf_via_isolation(&sftp, &canonical_root, &target).await
            }
        }
        .await;
        sftp.close().await;
        result
    })
}

/// 在同一远程项目中复制文件或目录；同名时自动生成副本名。
pub fn copy_entry_keep_both(
    conn: &SshConnection,
    project_root: &str,
    source_path: &str,
    target_dir: &str,
) -> Result<(String, FileOperationSummary), String> {
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let source = validate_remote_leaf_under_root(&sftp, project_root, source_path).await?;
            let target_dir =
                validate_remote_dir_under_root(&sftp, project_root, target_dir).await?;
            let (_, source_name) = split_posix_leaf(&source)?;
            let desired = join_posix(&target_dir, source_name);
            let target = keep_both_remote_path(&sftp, &desired).await?;
            let source_kind = sftp
                .node_kind(&source)
                .await
                .map_err(|e| format!("远程源条目不可访问: {}", e.message()))?;
            if source_kind == SftpNodeKind::Directory && posix_relative(&source, &target).is_some()
            {
                return Err("不能把远程目录复制到自身或其子目录".into());
            }
            let mut summary = FileOperationSummary::default();
            match source_kind {
                SftpNodeKind::Symlink | SftpNodeKind::Other => {
                    return Err("暂不复制远程符号链接或特殊文件".into());
                }
                SftpNodeKind::File => {
                    summary.bytes = sftp
                        .copy_file(&source, &target, false)
                        .await
                        .map_err(|e| format!("复制远程文件失败: {}", e.message()))?;
                    summary.completed = 1;
                }
                SftpNodeKind::Directory => {
                    let staging = sftp.temporary_sibling_path(&target, "copy-directory");
                    sftp.create_dir(&staging)
                        .await
                        .map_err(|e| format!("创建远程副本目录失败: {}", e.message()))?;
                    let copy_result: Result<(), String> = async {
                        let mut stack = vec![(source, staging.clone())];
                        while let Some((source_dir, target_dir)) = stack.pop() {
                            let entries = sftp
                                .read_dir(&source_dir)
                                .await
                                .map_err(|e| format!("读取远程源目录失败: {}", e.message()))?;
                            for entry in entries {
                                if !valid_remote_name(&entry.name) {
                                    return Err(format!(
                                        "服务器返回了无效条目名: {:?}",
                                        entry.name
                                    ));
                                }
                                let source_child = join_posix(&source_dir, &entry.name);
                                let target_child = join_posix(&target_dir, &entry.name);
                                if entry.is_symlink {
                                    summary.skipped += 1;
                                    summary
                                        .warnings
                                        .push(format!("已跳过符号链接: {source_child}"));
                                } else if entry.is_dir {
                                    sftp.create_dir(&target_child).await.map_err(|e| {
                                        format!("创建远程副本目录失败: {}", e.message())
                                    })?;
                                    summary.completed += 1;
                                    stack.push((source_child, target_child));
                                } else if entry.is_file {
                                    summary.bytes += sftp
                                        .copy_file(&source_child, &target_child, false)
                                        .await
                                        .map_err(|e| {
                                            format!("复制远程文件失败: {}", e.message())
                                        })?;
                                    summary.completed += 1;
                                } else {
                                    summary.skipped += 1;
                                    summary
                                        .warnings
                                        .push(format!("已跳过特殊文件: {source_child}"));
                                }
                            }
                        }
                        Ok(())
                    }
                    .await;
                    if let Err(error) = copy_result {
                        let cleanup = discard_remote_staged_entry(&sftp, &staging).await;
                        return match cleanup {
                            Ok(()) => Err(error),
                            Err(cleanup_error) => {
                                Err(format!("{error}; 清理远程暂存目录失败: {cleanup_error}"))
                            }
                        };
                    }
                    commit_new_remote_staged_directory(&sftp, &staging, &target).await?;
                    summary.completed += 1;
                }
            }
            Ok((target, summary))
        }
        .await;
        sftp.close().await;
        result
    })
}

type RemoteDirectoryCache = HashMap<String, HashMap<String, SftpNodeKind>>;

async fn remote_kind_cached(
    sftp: &SftpHandle,
    path: &str,
    cache: &mut RemoteDirectoryCache,
) -> Result<Option<SftpNodeKind>, String> {
    let (parent, name) = split_posix_leaf(path)?;
    if !cache.contains_key(parent) {
        let entries = sftp
            .read_dir(parent)
            .await
            .map_err(|e| format!("读取远程目录失败: {}", e.message()))?
            .into_iter()
            .map(|entry| {
                let kind = if entry.is_symlink {
                    SftpNodeKind::Symlink
                } else if entry.is_dir {
                    SftpNodeKind::Directory
                } else if entry.is_file {
                    SftpNodeKind::File
                } else {
                    SftpNodeKind::Other
                };
                (entry.name, kind)
            })
            .collect();
        cache.insert(parent.to_string(), entries);
    }
    Ok(cache
        .get(parent)
        .and_then(|entries| entries.get(name))
        .copied())
}

fn set_remote_kind_cached(
    cache: &mut RemoteDirectoryCache,
    path: &str,
    kind: SftpNodeKind,
) -> Result<(), String> {
    let (parent, name) = split_posix_leaf(path)?;
    if let Some(entries) = cache.get_mut(parent) {
        entries.insert(name.to_string(), kind);
    }
    Ok(())
}

fn remove_remote_kind_cached(cache: &mut RemoteDirectoryCache, path: &str) -> Result<(), String> {
    let (parent, name) = split_posix_leaf(path)?;
    if let Some(entries) = cache.get_mut(parent) {
        entries.remove(name);
    }
    Ok(())
}

fn invalidate_remote_cache_subtree(cache: &mut RemoteDirectoryCache, path: &str) {
    let prefix = format!("{}/", path.trim_end_matches('/'));
    cache.retain(|dir, _| dir != path && !dir.starts_with(&prefix));
}

fn invalidate_remote_parent_cache(cache: &mut RemoteDirectoryCache, path: &str) {
    if let Ok((parent, _)) = split_posix_leaf(path) {
        cache.remove(parent);
    }
}

async fn keep_both_remote_path_cached(
    sftp: &SftpHandle,
    desired: &str,
    cache: &mut RemoteDirectoryCache,
) -> Result<String, String> {
    let (parent, name) = split_posix_leaf(desired)?;
    let _ = remote_kind_cached(sftp, desired, cache).await?;
    let existing = cache
        .get(parent)
        .ok_or_else(|| format!("远程目录缓存缺失: {parent}"))?;
    for ordinal in 1..=10_000 {
        let candidate = keep_both_name(name, ordinal);
        if !existing.contains_key(&candidate) {
            return Ok(join_posix(parent, &candidate));
        }
    }
    Err(format!("无法为远程条目生成可用副本名: {desired}"))
}

fn local_kind(path: &Path) -> Result<SftpNodeKind, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| format!("无法读取本地条目 {}: {e}", path.display()))?;
    let ty = metadata.file_type();
    Ok(if ty.is_symlink() {
        SftpNodeKind::Symlink
    } else if ty.is_dir() {
        SftpNodeKind::Directory
    } else if ty.is_file() {
        SftpNodeKind::File
    } else {
        SftpNodeKind::Other
    })
}

fn remove_local_entry(path: &Path) -> Result<(), String> {
    match local_kind(path)? {
        SftpNodeKind::Directory => std::fs::remove_dir_all(path)
            .map_err(|e| format!("删除本地目录 {} 失败: {e}", path.display())),
        _ => std::fs::remove_file(path)
            .map_err(|e| format!("删除本地文件 {} 失败: {e}", path.display())),
    }
}

fn create_local_operation_container(target: &Path, role: &str) -> Result<PathBuf, String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("无法获取本地目标父目录: {}", target.display()))?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("entry");
    for _ in 0..10_000 {
        let sequence = LOCAL_TRANSFER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{name}.mt-{role}-{}-{sequence}",
            std::process::id()
        ));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "创建本地操作目录 {} 失败: {error}",
                    candidate.display()
                ));
            }
        }
    }
    Err(format!("无法为本地目标分配暂存目录: {}", target.display()))
}

fn create_local_staging_directory(target: &Path) -> Result<(PathBuf, PathBuf), String> {
    let container = create_local_operation_container(target, "download")?;
    let staging = container.join("entry");
    if let Err(error) = std::fs::create_dir(&staging) {
        let _ = std::fs::remove_dir(&container);
        return Err(format!(
            "创建本地暂存目录 {} 失败: {error}",
            staging.display()
        ));
    }
    Ok((container, staging))
}

fn commit_new_local_staged_directory(
    staging: &Path,
    staging_container: &Path,
    target: &Path,
) -> Result<(), String> {
    match std::fs::symlink_metadata(target) {
        Ok(_) => {
            let _ = remove_local_entry(staging_container);
            return Err(format!(
                "提交本地下载目录时目标已存在: {}",
                target.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let _ = remove_local_entry(staging_container);
            return Err(format!(
                "提交前检查本地目标 {} 失败: {error}",
                target.display()
            ));
        }
    }
    if let Err(error) = std::fs::rename(staging, target) {
        let cleanup = remove_local_entry(staging_container);
        return match cleanup {
            Ok(()) => Err(format!(
                "提交本地下载目录 {} 失败: {error}",
                target.display()
            )),
            Err(cleanup_error) => Err(format!(
                "提交本地下载目录 {} 失败: {error}; 清理暂存目录也失败: {cleanup_error}",
                target.display()
            )),
        };
    }
    std::fs::remove_dir(staging_container).map_err(|error| {
        format!(
            "清理本地下载暂存目录 {} 失败: {error}",
            staging_container.display()
        )
    })
}

fn replace_local_staged_entry(
    staging: &Path,
    staging_container: &Path,
    target: &Path,
) -> Result<(), String> {
    let backup_container = create_local_operation_container(target, "backup")?;
    let backup = backup_container.join("entry");
    if let Err(error) = std::fs::rename(target, &backup) {
        let _ = remove_local_entry(staging_container);
        let _ = std::fs::remove_dir(&backup_container);
        return Err(format!("备份本地目标 {} 失败: {error}", target.display()));
    }
    if let Err(promote_error) = std::fs::rename(staging, target) {
        let rollback = std::fs::rename(&backup, target);
        let _ = remove_local_entry(staging_container);
        let _ = std::fs::remove_dir(&backup_container);
        return match rollback {
            Ok(()) => Err(format!(
                "提交本地下载 {} 失败: {promote_error}",
                target.display()
            )),
            Err(rollback_error) => Err(format!(
                "提交本地下载失败且恢复失败: {promote_error}; rollback: {rollback_error}; backup: {}",
                backup.display()
            )),
        };
    }
    std::fs::remove_dir(staging_container).map_err(|error| {
        format!(
            "清理本地下载暂存目录 {} 失败: {error}",
            staging_container.display()
        )
    })?;
    remove_local_entry(&backup)
        .map_err(|error| format!("下载完成但清理备份 {} 失败: {error}", backup.display()))?;
    std::fs::remove_dir(&backup_container).map_err(|error| {
        format!(
            "清理本地备份目录 {} 失败: {error}",
            backup_container.display()
        )
    })?;
    Ok(())
}

fn keep_both_local_path(desired: &Path) -> Result<PathBuf, String> {
    if std::fs::symlink_metadata(desired).is_err() {
        return Ok(desired.to_path_buf());
    }
    let name = desired
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("本地目标名称不是有效 UTF-8: {}", desired.display()))?;
    for ordinal in 1..=10_000 {
        let candidate = desired.with_file_name(keep_both_name(name, ordinal));
        if std::fs::symlink_metadata(&candidate).is_err() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "无法为本地条目生成可用副本名: {}",
        desired.display()
    ))
}

fn collect_upload_conflicts(existing: &HashSet<String>, local_paths: &[PathBuf]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut reported = HashSet::new();
    let mut conflicts = Vec::new();
    for path in local_paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let repeated_in_batch = !seen.insert(name.to_string());
        if (existing.contains(name) || repeated_in_batch) && reported.insert(name.to_string()) {
            conflicts.push(name.to_string());
        }
    }
    conflicts
}

/// 上传前扫描顶层冲突；返回发生冲突的本地条目名称。
pub fn upload_conflicts(
    conn: &SshConnection,
    project_root: &str,
    target_dir: &str,
    local_paths: &[PathBuf],
) -> Result<Vec<String>, String> {
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let target_dir =
                validate_remote_dir_under_root(&sftp, project_root, target_dir).await?;
            let existing: HashSet<String> = sftp
                .read_dir(&target_dir)
                .await
                .map_err(|e| format!("读取远程目录失败: {}", e.message()))?
                .into_iter()
                .map(|entry| entry.name)
                .collect();
            Ok(collect_upload_conflicts(&existing, local_paths))
        }
        .await;
        sftp.close().await;
        result
    })
}

async fn upload_path_tree(
    sftp: &SftpHandle,
    local_root: PathBuf,
    remote_root: String,
    strategy: FileConflictStrategy,
    summary: &mut FileOperationSummary,
    remote_cache: &mut RemoteDirectoryCache,
) -> Result<(), String> {
    enum UploadWork {
        Visit {
            local: PathBuf,
            desired_remote: String,
            inside_staging: bool,
            staging_replaces_existing: bool,
        },
        CommitDirectory {
            staging: String,
            target: String,
            replace_existing: bool,
            summary_before: FileOperationSummary,
        },
    }

    let mut stack = vec![UploadWork::Visit {
        local: local_root,
        desired_remote: remote_root,
        inside_staging: false,
        staging_replaces_existing: false,
    }];
    let mut staged_directories = HashSet::new();
    let mut result: Result<(), String> = async {
        while let Some(work) = stack.pop() {
            let (local, desired_remote, inside_staging, staging_replaces_existing) = match work {
                UploadWork::Visit {
                    local,
                    desired_remote,
                    inside_staging,
                    staging_replaces_existing,
                } => (
                    local,
                    desired_remote,
                    inside_staging,
                    staging_replaces_existing,
                ),
                UploadWork::CommitDirectory {
                    staging,
                    target,
                    replace_existing,
                    summary_before,
                } => {
                    let commit_result = if replace_existing {
                        sftp.replace_staged_entry(&staging, &target)
                            .await
                            .map_err(|e| format!("替换远程目录失败: {}", e.message()))
                    } else {
                        commit_new_remote_staged_directory(sftp, &staging, &target).await
                    };
                    if let Err(error) = commit_result {
                        let rollback_summary = stack
                            .iter()
                            .find_map(|work| match work {
                                UploadWork::CommitDirectory { summary_before, .. } => {
                                    Some(summary_before.clone())
                                }
                                UploadWork::Visit { .. } => None,
                            })
                            .unwrap_or(summary_before);
                        *summary = rollback_summary;
                        invalidate_remote_parent_cache(remote_cache, &target);
                        invalidate_remote_parent_cache(remote_cache, &staging);
                        invalidate_remote_cache_subtree(remote_cache, &target);
                        invalidate_remote_cache_subtree(remote_cache, &staging);
                        return Err(error);
                    }
                    staged_directories.remove(&staging);
                    invalidate_remote_cache_subtree(remote_cache, &target);
                    invalidate_remote_cache_subtree(remote_cache, &staging);
                    remove_remote_kind_cached(remote_cache, &staging)?;
                    set_remote_kind_cached(remote_cache, &target, SftpNodeKind::Directory)?;
                    summary.completed += 1;
                    continue;
                }
            };
            let kind = match local_kind(&local) {
                Ok(kind) => kind,
                Err(error) if inside_staging => {
                    return Err(format!("远程暂存目录未完整构建: {error}"));
                }
                Err(error) => {
                    summary.failed += 1;
                    summary.warnings.push(error);
                    continue;
                }
            };
            if matches!(kind, SftpNodeKind::Symlink | SftpNodeKind::Other) {
                let warning = format!("已跳过本地符号链接或特殊文件: {}", local.display());
                if staging_replaces_existing {
                    return Err(format!("远程暂存目录未完整构建: {warning}"));
                }
                summary.skipped += 1;
                summary.warnings.push(warning);
                continue;
            }

            let existing = remote_kind_cached(sftp, &desired_remote, remote_cache).await?;
            if inside_staging && existing.is_some() {
                return Err(format!(
                    "远程暂存目录被意外修改，拒绝提交: {desired_remote}"
                ));
            }
            let (remote, existing) = match (existing, strategy) {
                (Some(_), FileConflictStrategy::Skip) => {
                    summary.skipped += 1;
                    continue;
                }
                (Some(_), FileConflictStrategy::KeepBoth) => (
                    keep_both_remote_path_cached(sftp, &desired_remote, remote_cache).await?,
                    None,
                ),
                (existing, _) => (desired_remote, existing),
            };

            match kind {
                SftpNodeKind::Directory => {
                    let mut completes_immediately = true;
                    let (child_remote_base, child_inside_staging, child_staging_replaces_existing) =
                        match existing {
                            None if inside_staging => {
                                sftp.create_dir(&remote)
                                    .await
                                    .map_err(|e| format!("创建远程目录失败: {}", e.message()))?;
                                set_remote_kind_cached(
                                    remote_cache,
                                    &remote,
                                    SftpNodeKind::Directory,
                                )?;
                                remote_cache.insert(remote.clone(), HashMap::new());
                                (remote.clone(), true, staging_replaces_existing)
                            }
                            Some(SftpNodeKind::Directory)
                                if strategy == FileConflictStrategy::Overwrite =>
                            {
                                (remote.clone(), inside_staging, staging_replaces_existing)
                            }
                            existing => {
                                let replace_existing = existing.is_some();
                                let staging = sftp.temporary_sibling_path(&remote, "directory");
                                sftp.create_dir(&staging).await.map_err(|e| {
                                    format!("创建远程暂存目录失败: {}", e.message())
                                })?;
                                staged_directories.insert(staging.clone());
                                set_remote_kind_cached(
                                    remote_cache,
                                    &staging,
                                    SftpNodeKind::Directory,
                                )?;
                                remote_cache.insert(staging.clone(), HashMap::new());
                                stack.push(UploadWork::CommitDirectory {
                                    staging: staging.clone(),
                                    target: remote.clone(),
                                    replace_existing,
                                    summary_before: summary.clone(),
                                });
                                completes_immediately = false;
                                (staging, true, staging_replaces_existing || replace_existing)
                            }
                        };
                    if completes_immediately {
                        summary.completed += 1;
                    }
                    let entries = std::fs::read_dir(&local)
                        .map_err(|e| format!("读取本地目录 {} 失败: {e}", local.display()))?;
                    let mut children = Vec::new();
                    for entry in entries {
                        let entry = entry
                            .map_err(|e| format!("读取本地目录项 {} 失败: {e}", local.display()))?;
                        let name = entry.file_name();
                        let Some(name) = name.to_str() else {
                            let warning = format!(
                                "已跳过名称不是有效 UTF-8 的本地条目: {}",
                                entry.path().display()
                            );
                            if child_staging_replaces_existing {
                                return Err(format!("远程暂存目录未完整构建: {warning}"));
                            }
                            summary.skipped += 1;
                            summary.warnings.push(warning);
                            continue;
                        };
                        if !valid_remote_name(name) {
                            let warning = format!("已跳过远程不支持的名称: {name}");
                            if child_staging_replaces_existing {
                                return Err(format!("远程暂存目录未完整构建: {warning}"));
                            }
                            summary.skipped += 1;
                            summary.warnings.push(warning);
                            continue;
                        }
                        children.push(UploadWork::Visit {
                            local: entry.path(),
                            desired_remote: join_posix(&child_remote_base, name),
                            inside_staging: child_inside_staging,
                            staging_replaces_existing: child_staging_replaces_existing,
                        });
                    }
                    children.reverse();
                    stack.extend(children);
                }
                SftpNodeKind::File => {
                    let overwrite =
                        existing.is_some() && strategy == FileConflictStrategy::Overwrite;
                    summary.bytes += sftp
                        .upload_file(&local, &remote, overwrite)
                        .await
                        .map_err(|e| format!("上传文件失败: {}", e.message()))?;
                    invalidate_remote_cache_subtree(remote_cache, &remote);
                    set_remote_kind_cached(remote_cache, &remote, SftpNodeKind::File)?;
                    summary.completed += 1;
                }
                SftpNodeKind::Symlink | SftpNodeKind::Other => unreachable!(),
            }
        }
        Ok(())
    }
    .await;

    if result.is_err() {
        if let Some(summary_before) = stack.iter().find_map(|work| match work {
            UploadWork::CommitDirectory { summary_before, .. } => Some(summary_before.clone()),
            UploadWork::Visit { .. } => None,
        }) {
            *summary = summary_before;
        }
        let mut cleanup_errors = Vec::new();
        for staging in staged_directories {
            if let Ok(kind) = sftp.node_kind(&staging).await
                && let Err(error) = sftp.remove_tree(&staging, kind).await
            {
                cleanup_errors.push(format!("{staging}: {}", error.message()));
            }
        }
        if !cleanup_errors.is_empty()
            && let Err(original) = &result
        {
            let original = original.clone();
            result = Err(format!(
                "{original}; 清理远程暂存目录失败: {}",
                cleanup_errors.join("; ")
            ));
        }
    }
    result
}

/// 上传一批本地文件/文件夹到远程目录。目录 Overwrite 为递归合并并保留目标独有项。
pub fn upload_paths(
    conn: &SshConnection,
    project_root: &str,
    target_dir: &str,
    local_paths: &[PathBuf],
    strategy: FileConflictStrategy,
) -> Result<FileOperationSummary, String> {
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let target_dir =
                validate_remote_dir_under_root(&sftp, project_root, target_dir).await?;
            let mut summary = FileOperationSummary::default();
            let mut remote_cache = RemoteDirectoryCache::new();
            for local in local_paths {
                let Some(name) = local.file_name().and_then(|name| name.to_str()) else {
                    summary.skipped += 1;
                    summary.warnings.push(format!(
                        "已跳过名称不是有效 UTF-8 的本地条目: {}",
                        local.display()
                    ));
                    continue;
                };
                if !valid_remote_name(name) {
                    summary.skipped += 1;
                    summary
                        .warnings
                        .push(format!("已跳过远程不支持的名称: {name}"));
                    continue;
                }
                if let Err(error) = upload_path_tree(
                    &sftp,
                    local.clone(),
                    join_posix(&target_dir, name),
                    strategy,
                    &mut summary,
                    &mut remote_cache,
                )
                .await
                {
                    remote_cache.clear();
                    summary.failed += 1;
                    summary.warnings.push(error);
                }
            }
            Ok(summary)
        }
        .await;
        sftp.close().await;
        result
    })
}

fn ensure_local_download_target(download_root: &Path, target: &Path) -> Result<(), String> {
    if !download_root.is_absolute() {
        return Err(format!(
            "下载根目录必须是绝对路径: {}",
            download_root.display()
        ));
    }
    if !target.starts_with(download_root) {
        return Err(format!(
            "本地下载目标逃出下载目录: {} (root: {})",
            target.display(),
            download_root.display()
        ));
    }
    Ok(())
}

fn checked_local_download_child(
    download_root: &Path,
    parent: &Path,
    name: &str,
) -> Result<PathBuf, String> {
    if !valid_remote_name(name) {
        return Err(format!("远程文件名不能安全落到本机: {name:?}"));
    }
    ensure_local_download_target(download_root, parent)?;
    let target = parent.join(name);
    ensure_local_download_target(download_root, &target)?;
    Ok(target)
}

/// 下载前检查顶层目标是否已存在。
pub fn download_conflicts(
    download_dir: &Path,
    remote_paths: &[PathBuf],
) -> Result<Vec<String>, String> {
    let mut conflicts = Vec::new();
    for path in remote_paths {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("远程下载目标名称无效: {}", path.display()))?;
        let target = checked_local_download_child(download_dir, download_dir, name)?;
        if std::fs::symlink_metadata(target).is_ok() {
            conflicts.push(name.to_string());
        }
    }
    Ok(conflicts)
}

async fn download_remote_tree(
    sftp: &SftpHandle,
    remote_root: String,
    download_root: &Path,
    local_root: PathBuf,
    strategy: FileConflictStrategy,
    summary: &mut FileOperationSummary,
) -> Result<(), String> {
    ensure_local_download_target(download_root, &local_root)?;
    enum DownloadWork {
        Visit {
            remote: String,
            desired_local: PathBuf,
            known_kind: Option<SftpNodeKind>,
            inside_staging: bool,
            staging_replaces_existing: bool,
        },
        CommitDirectory {
            staging: PathBuf,
            staging_container: PathBuf,
            target: PathBuf,
            replace_existing: bool,
            summary_before: FileOperationSummary,
        },
    }

    let mut stack = vec![DownloadWork::Visit {
        remote: remote_root,
        desired_local: local_root,
        known_kind: None,
        inside_staging: false,
        staging_replaces_existing: false,
    }];
    let mut staging_containers = HashSet::new();
    let mut result: Result<(), String> = async {
        while let Some(work) = stack.pop() {
            let (remote, desired_local, known_kind, inside_staging, staging_replaces_existing) =
                match work {
                    DownloadWork::Visit {
                        remote,
                        desired_local,
                        known_kind,
                        inside_staging,
                        staging_replaces_existing,
                    } => (
                        remote,
                        desired_local,
                        known_kind,
                        inside_staging,
                        staging_replaces_existing,
                    ),
                    DownloadWork::CommitDirectory {
                        staging,
                        staging_container,
                        target,
                        replace_existing,
                        summary_before,
                    } => {
                        ensure_local_download_target(download_root, &staging)?;
                        ensure_local_download_target(download_root, &staging_container)?;
                        ensure_local_download_target(download_root, &target)?;
                        let commit_result = if replace_existing {
                            replace_local_staged_entry(&staging, &staging_container, &target)
                        } else {
                            commit_new_local_staged_directory(&staging, &staging_container, &target)
                        };
                        if let Err(error) = commit_result {
                            let rollback_summary = stack
                                .iter()
                                .find_map(|work| match work {
                                    DownloadWork::CommitDirectory { summary_before, .. } => {
                                        Some(summary_before.clone())
                                    }
                                    DownloadWork::Visit { .. } => None,
                                })
                                .unwrap_or(summary_before);
                            *summary = rollback_summary;
                            return Err(error);
                        }
                        staging_containers.remove(&staging_container);
                        summary.completed += 1;
                        continue;
                    }
                };
            ensure_local_download_target(download_root, &desired_local)?;
            let kind = match known_kind {
                Some(kind) => kind,
                None => sftp
                    .node_kind(&remote)
                    .await
                    .map_err(|e| format!("远程条目不可访问: {}", e.message()))?,
            };
            if matches!(kind, SftpNodeKind::Symlink | SftpNodeKind::Other) {
                if staging_replaces_existing {
                    return Err(format!(
                        "本地下载暂存目录未完整构建: 远程条目不可传输: {remote}"
                    ));
                }
                summary.skipped += 1;
                summary
                    .warnings
                    .push(format!("已跳过远程符号链接或特殊文件: {remote}"));
                continue;
            }

            let existing = std::fs::symlink_metadata(&desired_local)
                .ok()
                .map(|metadata| {
                    let ty = metadata.file_type();
                    if ty.is_symlink() {
                        SftpNodeKind::Symlink
                    } else if ty.is_dir() {
                        SftpNodeKind::Directory
                    } else if ty.is_file() {
                        SftpNodeKind::File
                    } else {
                        SftpNodeKind::Other
                    }
                });
            if inside_staging && existing.is_some() {
                return Err(format!(
                    "本地下载暂存目录被意外修改，拒绝提交: {}",
                    desired_local.display()
                ));
            }
            let (local, existing) = match (existing, strategy) {
                (Some(_), FileConflictStrategy::Skip) => {
                    summary.skipped += 1;
                    continue;
                }
                (Some(_), FileConflictStrategy::KeepBoth) => {
                    (keep_both_local_path(&desired_local)?, None)
                }
                (existing, _) => (desired_local, existing),
            };
            ensure_local_download_target(download_root, &local)?;

            match kind {
                SftpNodeKind::Directory => {
                    let mut completes_immediately = true;
                    let (child_local_base, child_inside_staging, child_staging_replaces_existing) =
                        match existing {
                            None if inside_staging => {
                                std::fs::create_dir(&local).map_err(|e| {
                                    format!("创建本地下载目录 {} 失败: {e}", local.display())
                                })?;
                                (local.clone(), true, staging_replaces_existing)
                            }
                            Some(SftpNodeKind::Directory)
                                if strategy == FileConflictStrategy::Overwrite =>
                            {
                                (local.clone(), inside_staging, staging_replaces_existing)
                            }
                            existing => {
                                let replace_existing = existing.is_some();
                                let (staging_container, staging) =
                                    create_local_staging_directory(&local)?;
                                ensure_local_download_target(download_root, &staging_container)?;
                                ensure_local_download_target(download_root, &staging)?;
                                staging_containers.insert(staging_container.clone());
                                stack.push(DownloadWork::CommitDirectory {
                                    staging: staging.clone(),
                                    staging_container,
                                    target: local.clone(),
                                    replace_existing,
                                    summary_before: summary.clone(),
                                });
                                completes_immediately = false;
                                (staging, true, staging_replaces_existing || replace_existing)
                            }
                        };
                    if completes_immediately {
                        summary.completed += 1;
                    }
                    let entries = sftp
                        .read_dir(&remote)
                        .await
                        .map_err(|e| format!("读取远程目录失败: {}", e.message()))?;
                    for entry in entries.into_iter().rev() {
                        if !valid_remote_name(&entry.name) {
                            if child_staging_replaces_existing {
                                return Err(format!(
                                    "本地下载暂存目录未完整构建: 服务器返回了无效条目名: {:?}",
                                    entry.name
                                ));
                            }
                            summary.skipped += 1;
                            summary
                                .warnings
                                .push(format!("服务器返回了无效条目名: {:?}", entry.name));
                            continue;
                        }
                        let kind = if entry.is_symlink {
                            SftpNodeKind::Symlink
                        } else if entry.is_dir {
                            SftpNodeKind::Directory
                        } else if entry.is_file {
                            SftpNodeKind::File
                        } else {
                            SftpNodeKind::Other
                        };
                        stack.push(DownloadWork::Visit {
                            remote: join_posix(&remote, &entry.name),
                            desired_local: checked_local_download_child(
                                download_root,
                                &child_local_base,
                                &entry.name,
                            )?,
                            known_kind: Some(kind),
                            inside_staging: child_inside_staging,
                            staging_replaces_existing: child_staging_replaces_existing,
                        });
                    }
                }
                SftpNodeKind::File => {
                    let overwrite =
                        existing.is_some() && strategy == FileConflictStrategy::Overwrite;
                    summary.bytes += sftp
                        .download_file(&remote, &local, overwrite)
                        .await
                        .map_err(|e| format!("下载远程文件失败: {}", e.message()))?;
                    summary.completed += 1;
                }
                SftpNodeKind::Symlink | SftpNodeKind::Other => unreachable!(),
            }
        }
        Ok(())
    }
    .await;

    if result.is_err() {
        if let Some(summary_before) = stack.iter().find_map(|work| match work {
            DownloadWork::CommitDirectory { summary_before, .. } => Some(summary_before.clone()),
            DownloadWork::Visit { .. } => None,
        }) {
            *summary = summary_before;
        }
        let mut cleanup_errors = Vec::new();
        for staging_container in staging_containers {
            if std::fs::symlink_metadata(&staging_container).is_ok()
                && let Err(error) = remove_local_entry(&staging_container)
            {
                cleanup_errors.push(error);
            }
        }
        if !cleanup_errors.is_empty()
            && let Err(original) = &result
        {
            let original = original.clone();
            result = Err(format!(
                "{original}; 清理本地下载暂存目录失败: {}",
                cleanup_errors.join("; ")
            ));
        }
    }
    result
}

/// 下载一个或多个远程条目到本地目录。
pub fn download_entries(
    conn: &SshConnection,
    project_root: &str,
    remote_paths: &[PathBuf],
    download_dir: &Path,
    strategy: FileConflictStrategy,
) -> Result<FileOperationSummary, String> {
    if !download_dir.is_absolute() {
        return Err(format!(
            "下载目录必须是绝对路径: {}",
            download_dir.display()
        ));
    }
    std::fs::create_dir_all(download_dir)
        .map_err(|e| format!("无法创建下载目录 {}: {e}", download_dir.display()))?;
    mt_config::AppConfig::validate_download_dir(download_dir)
        .map_err(|e| format!("下载目录不可用: {e:#}"))?;
    let download_root = std::fs::canonicalize(download_dir)
        .map_err(|e| format!("无法解析下载目录 {}: {e}", download_dir.display()))?;
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let mut summary = FileOperationSummary::default();
            for remote_path in remote_paths {
                let remote = validate_remote_leaf_under_root(
                    &sftp,
                    project_root,
                    &remote_path.to_string_lossy(),
                )
                .await?;
                let (_, name) = split_posix_leaf(&remote)?;
                let local_root =
                    checked_local_download_child(&download_root, &download_root, name)?;
                if let Err(error) = download_remote_tree(
                    &sftp,
                    remote,
                    &download_root,
                    local_root,
                    strategy,
                    &mut summary,
                )
                .await
                {
                    summary.failed += 1;
                    summary.warnings.push(error);
                }
            }
            Ok(summary)
        }
        .await;
        sftp.close().await;
        result
    })
}

/// 连接自检:只探到远程 `$HOME` 为止,返回它。
///
/// 原版没有独立的「测试连接」command(SshModal 只做 CRUD),这里把
/// [`validate_dir`] 的前半段单独暴露,BB-b 若要做「测试」按钮可以直接用 ——
/// 不新增任何行为,失败文案与真实使用同源。
/// ⚠️ **有意保留的无调用点代码**(窄作用域 `allow`,不是整模块的那种):
/// BB-b 逐字复刻 `SshModal.tsx` 时确认原版**没有**「测试连接」按钮,所以没有
/// 入口接它。删掉的话哪天要加这颗按钮又得把同一段重写一遍,而它与
/// [`validate_dir`] 共用同一条失败面 —— 留着零成本。
#[allow(dead_code)]
pub fn probe_connection(conn: &SshConnection) -> Result<String, String> {
    validate_dir(conn, "~")
}

// ---------------------------------------------------------------------------
// 入口 3:粘贴内容上传(issue #36)
// ---------------------------------------------------------------------------

/// 把本地临时文件(剪贴板图片 / 长文本转存)上传到远程项目,返回**远端绝对路径**。
///
/// 背景:远程项目的 pane 跑的是本地 `ssh` 客户端,粘贴走本地链路只会得到一个
/// Windows 路径 —— 远端 agent 读不到。这里另开一条 SFTP(池里同一条 session)
/// 把文件送过去,调用方再把返回的远端路径粘进终端。
///
/// 目标目录由 `dest_dir` 决定(见 [`resolve_paste_dir`]),不存在则逐级创建。
/// 同名覆盖:文件名由调用方生成(`paste-<ms>.txt`),带毫秒时间戳,实际不会撞。
///
/// **阻塞**,丢 `background_executor`。
pub fn upload_paste(
    conn: &SshConnection,
    project_path: &str,
    local_path: &str,
    dest_dir: &str,
) -> Result<String, String> {
    let st = state();
    let file_name = paste_file_name(local_path)?;
    st.block_on(async move {
        let (session, sftp) = open_sftp_with_session(st, conn).await?;

        // 整段(建目录 + 上传)套一层墙钟上限:见 PASTE_UPLOAD_TOTAL_TIMEOUT。
        let result = tokio::time::timeout(PASTE_UPLOAD_TOTAL_TIMEOUT, async {
            let home = remote_home(st, &sftp, &conn.id).await?;
            let dir = resolve_paste_dir(project_path, &home, dest_dir)?;
            sftp.create_dir_all(&dir)
                .await
                .map_err(|e| format!("创建远程粘贴目录失败: {}", e.message()))?;

            // 目录**严格位于**项目内(默认形态)时放一个自忽略的 .gitignore ——
            // 否则每次粘图都会把用户仓库的 `git status` 弄脏。
            // CREATE|EXCLUDE 语义天然幂等,已存在就失败,失败也无所谓:这只是体面,
            // 绝不能拖累粘贴本身。
            //
            // 空相对路径(dir 就是项目根)必须排除 —— 那会在仓库根写下一个内容为
            // `*` 的 .gitignore,把用户整个仓库忽略掉。
            if posix_relative(project_path, &dir).is_some_and(|rel| !rel.is_empty()) {
                let _ = sftp
                    .write_new_file(&join_posix(&dir, ".gitignore"), b"*\n")
                    .await;
            }

            let remote_path = join_posix(&dir, &file_name);
            mt_ssh::run_sftp_upload_on_session(
                &session,
                local_path,
                &remote_path,
                PASTE_UPLOAD_REQUEST_TIMEOUT,
            )
            .await
            .map_err(|e| format!("上传到远程失败: {}", e.message()))?;
            session.touch();
            Ok(remote_path)
        })
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "上传到远程超时({}s)",
                PASTE_UPLOAD_TOTAL_TIMEOUT.as_secs()
            ))
        });

        sftp.close().await;
        result
    })
}

// ---------------------------------------------------------------------------
// 入口 4:远程 AI 会话列表
// ---------------------------------------------------------------------------

/// 扫描远程机器上该项目的 claude/codex 历史会话。
/// - 会话带 `sshConnectionId` 来源标识(对齐 WSL 会话的 `wslDistro`);
/// - 结果缓存 10s(key 掺 connection id),`force=true` 绕过(手动刷新);
/// - 远程不可达 / 目录缺失等一切失败:静默降级返回空列表。
///
/// 返回类型保留 `Result` 只为与本模块其它入口同形 —— 它**永不返回 Err**
/// (原版同款:`ssh_remote_ai_sessions` 的 `Err` 分支已被内部吞掉)。
///
/// **阻塞**,丢 `background_executor`。
pub fn ai_sessions(
    conn: &SshConnection,
    project_path: &str,
    force: bool,
) -> Result<Vec<AiSession>, String> {
    let cache_key = format!("ssh|{}|{}", conn.id, normalize_unix_path(project_path));

    if !force {
        // 锁即取即放,扫描期间不持锁(SFTP IO 秒级)。
        let cached = lock(session_cache()).get(&cache_key).cloned();
        if let Some(c) = cached
            && c.loaded_at.elapsed() < REMOTE_SESSION_CACHE_TTL
        {
            return Ok(c.sessions);
        }
    }

    let sessions = match scan_remote_sessions(conn, project_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[remote-ssh] session scan failed (degrading to empty): {e}");
            Vec::new()
        }
    };

    lock(session_cache()).insert(
        cache_key,
        CachedSessions {
            loaded_at: Instant::now(),
            sessions: sessions.clone(),
        },
    );

    Ok(sessions)
}

fn scan_remote_sessions(
    conn: &SshConnection,
    project_path: &str,
) -> Result<Vec<AiSession>, String> {
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let home = remote_home(st, &sftp, &conn.id).await?;
            let mut sessions = Vec::new();
            sessions.extend(scan_remote_claude(st, &sftp, &home, &conn.id, project_path).await);
            sessions.extend(scan_remote_codex(st, &sftp, &home, &conn.id, project_path).await);
            sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
            sessions.truncate(MAX_TOTAL_SESSIONS);
            Ok(sessions)
        }
        .await;
        sftp.close().await;
        result
    })
}

/// 记录会话 id → 远程文件路径,正文读取时免再扫。
fn remember_session_path(st: &RemoteSshState, conn_id: &str, session_id: &str, path: &str) {
    lock(&st.session_paths).insert(format!("{conn_id}|{session_id}"), path.to_string());
}

/// 变体目录精确校验:读目录里任一 jsonl 头部的前几行,比对真实 cwd。
/// 与本地 `dir_matches_project` 语义一致(编码有损,防吃进兄弟项目)。
async fn remote_claude_dir_matches(sftp: &SftpHandle, dir: &str, normalized_project: &str) -> bool {
    let Ok(entries) = sftp.read_dir(dir).await else {
        return false;
    };
    for e in entries {
        if e.is_dir || !e.name.ends_with(".jsonl") {
            continue;
        }
        let path = join_posix(dir, &e.name);
        let Ok(head) = sftp.read_head(&path, CWD_PROBE_HEAD_BYTES).await else {
            continue;
        };
        let text = String::from_utf8_lossy(&head);
        for line in text.lines().take(5) {
            if let Ok(obj) = serde_json::from_str::<serde_json::Value>(line)
                && let Some(cwd) = obj.get("cwd").and_then(|v| v.as_str())
            {
                return normalize_unix_path(cwd) == normalized_project;
            }
        }
    }
    false
}

async fn scan_remote_claude(
    st: &RemoteSshState,
    sftp: &SftpHandle,
    home: &str,
    conn_id: &str,
    project_path: &str,
) -> Vec<AiSession> {
    let projects_dir = join_posix(&join_posix(home, ".claude"), "projects");
    let Ok(dir_entries) = sftp.read_dir(&projects_dir).await else {
        return vec![]; // 远程没装 claude / 目录不存在 → 静默空
    };

    let encoded = encode_project_path(project_path);
    let normalized_project = normalize_unix_path(project_path);

    let mut matched_dirs: Vec<String> = Vec::new();
    for entry in dir_entries {
        if !entry.is_dir {
            continue;
        }
        if entry.name == encoded {
            matched_dirs.push(join_posix(&projects_dir, &entry.name));
        } else if is_encoded_variant(&entry.name, &encoded) {
            let dir_path = join_posix(&projects_dir, &entry.name);
            if remote_claude_dir_matches(sftp, &dir_path, &normalized_project).await {
                matched_dirs.push(dir_path);
            }
        }
    }

    // 收集 (path, id, mtime),同 id 去重,按 mtime 降序限量。
    let mut files: Vec<(String, String, u64)> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    for dir in &matched_dirs {
        let Ok(entries) = sftp.read_dir(dir).await else {
            continue;
        };
        for e in entries {
            if e.is_dir {
                continue;
            }
            let Some(id) = e.name.strip_suffix(".jsonl") else {
                continue;
            };
            if seen_ids.insert(id.to_string()) {
                files.push((
                    join_posix(dir, &e.name),
                    id.to_string(),
                    e.mtime_secs.unwrap_or(0),
                ));
            }
        }
    }
    files.sort_by_key(|entry| std::cmp::Reverse(entry.2));
    files.truncate(REMOTE_CLAUDE_SCAN_LIMIT);

    let mut sessions = Vec::new();
    for (path, id, mtime) in files {
        if sessions.len() >= MAX_SESSIONS_PER_SOURCE {
            break;
        }
        let Ok(head) = sftp.read_head(&path, CLAUDE_TITLE_HEAD_BYTES).await else {
            continue;
        };
        let text = String::from_utf8_lossy(&head);
        let (title, mut timestamp) = claude_session_info_from_lines(text.lines().take(50));
        if timestamp.is_empty() && mtime > 0 {
            timestamp = unix_secs_to_iso(mtime);
        }
        remember_session_path(st, conn_id, &id, &path);
        sessions.push(AiSession {
            id,
            session_type: "claude".to_string(),
            title,
            timestamp,
            // 远程文件尾窗反扫要再走一趟 SFTP,不值当;识别不出回落 CLI 图标
            model: None,
            wsl_distro: None,
            ssh_connection_id: Some(conn_id.to_string()),
        });
    }
    sessions
}

/// 按 `sessions/<year>/<month>/<day>/` 目录名倒序(零填充,字典序即时间序)收集
/// 最新的 rollout 文件,凑够 `limit` 即停 —— 避免全量递归的 SFTP 往返爆炸。
async fn collect_remote_codex_files(
    sftp: &SftpHandle,
    sessions_dir: &str,
    limit: usize,
) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = Vec::new();
    let Ok(mut years) = sftp.read_dir(sessions_dir).await else {
        return out;
    };
    years.retain(|e| e.is_dir);
    years.sort_by(|a, b| b.name.cmp(&a.name));
    'outer: for y in years {
        let ydir = join_posix(sessions_dir, &y.name);
        let Ok(mut months) = sftp.read_dir(&ydir).await else {
            continue;
        };
        months.retain(|e| e.is_dir);
        months.sort_by(|a, b| b.name.cmp(&a.name));
        for m in months {
            let mdir = join_posix(&ydir, &m.name);
            let Ok(mut days) = sftp.read_dir(&mdir).await else {
                continue;
            };
            days.retain(|e| e.is_dir);
            days.sort_by(|a, b| b.name.cmp(&a.name));
            for d in days {
                let ddir = join_posix(&mdir, &d.name);
                let Ok(mut file_entries) = sftp.read_dir(&ddir).await else {
                    continue;
                };
                file_entries.retain(|e| !e.is_dir && e.name.ends_with(".jsonl"));
                // 同一天内按 mtime 倒序。
                file_entries.sort_by_key(|entry| std::cmp::Reverse(entry.mtime_secs.unwrap_or(0)));
                for f in file_entries {
                    out.push((join_posix(&ddir, &f.name), f.mtime_secs.unwrap_or(0)));
                    if out.len() >= limit {
                        break 'outer;
                    }
                }
            }
        }
    }
    out
}

async fn scan_remote_codex(
    st: &RemoteSshState,
    sftp: &SftpHandle,
    home: &str,
    conn_id: &str,
    project_path: &str,
) -> Vec<AiSession> {
    let codex_dir = join_posix(home, ".codex");
    let sessions_dir = join_posix(&codex_dir, "sessions");
    let files = collect_remote_codex_files(sftp, &sessions_dir, REMOTE_CODEX_SCAN_LIMIT).await;
    if files.is_empty() {
        return vec![];
    }

    let thread_names = {
        let index_path = join_posix(&codex_dir, "session_index.jsonl");
        match sftp.read_head(&index_path, SESSION_INDEX_MAX_BYTES).await {
            Ok(bytes) => parse_codex_thread_names(&String::from_utf8_lossy(&bytes)),
            Err(_) => HashMap::new(),
        }
    };

    let normalized_project = normalize_unix_path(project_path);
    let mut sessions = Vec::new();
    for (path, mtime) in files {
        if sessions.len() >= MAX_SESSIONS_PER_SOURCE {
            break;
        }
        let Ok(head) = sftp.read_head(&path, CODEX_META_HEAD_BYTES).await else {
            continue;
        };
        let text = String::from_utf8_lossy(&head);
        let mut lines = text.lines();

        // 前 5 行找 session_meta(实际几乎总在第 1 行),匹配 cwd。
        let mut meta = None;
        for line in (&mut lines).take(5) {
            if let Some(m) = codex_meta_from_line(line) {
                meta = Some(m);
                break;
            }
        }
        let Some(meta) = meta else { continue };
        if meta.id.is_empty() || normalize_unix_path(&meta.cwd) != normalized_project {
            continue;
        }

        let mut title = thread_names.get(&meta.id).cloned().unwrap_or_default();
        if title.is_empty() {
            for line in lines.take(30) {
                if let Some(t) = codex_user_title_from_line(line) {
                    title = t;
                    break;
                }
            }
        }
        if title.is_empty() {
            title = "Untitled".into();
        }

        let mut timestamp = meta.timestamp;
        if timestamp.is_empty() && mtime > 0 {
            timestamp = unix_secs_to_iso(mtime);
        }

        remember_session_path(st, conn_id, &meta.id, &path);
        sessions.push(AiSession {
            id: meta.id,
            session_type: "codex".to_string(),
            title,
            timestamp,
            model: None,
            wsl_distro: None,
            ssh_connection_id: Some(conn_id.to_string()),
        });
    }
    sessions
}

// ---------------------------------------------------------------------------
// 入口 5:远程会话正文(支持增量 offset)
// ---------------------------------------------------------------------------

/// 远程会话正文的增量读取结果。
#[derive(Debug, Clone, Default)]
pub struct RemoteSessionContent {
    /// 本次解析出的消息(与本地 `get_ai_session_content` 的元素同构)。
    pub messages: Vec<AiSessionMessage>,
    /// 已解析到的字节偏移(指向本段最后一个完整行之后),续读传它即可。
    /// 首次调用传 offset=0;之后传上次返回的 `next_offset` 拿下一段。
    ///
    /// 读者是 [`accumulate_session_content`] 的续读循环 —— 单次 SFTP 读封顶
    /// [`CONTENT_CHUNK_MAX_BYTES`],大会话必须靠它才能读全。
    /// **它没有前进(`<= 传入的 offset`)就等于「没得读了」**:要么到了 EOF,
    /// 要么整段找不到换行,两种情况都必须停,别指望下一轮会不一样。
    pub next_offset: u64,
}

/// 一次全量读取([`ai_session_content_all`])允许拼接的正文总量上限。
/// 护栏而非功能上限:正常 Claude/Codex 会话是几百 KB 到几 MB,64 MB 已经离谱;
/// 设它是为了不让某个病态(或被构造的)远程会话文件把桌面端内存吃光 ——
/// 触到上限就带着已解析内容收尾,不报错、不死循环。
const CONTENT_TOTAL_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// 循环续读的通用核:反复调 `fetch(offset)` 并按 `next_offset` 推进,直到读完。
/// 抽成泛型是为了能不触网单测 —— 真调用见 [`ai_session_content_all`]。
///
/// **前进保证**(死循环护栏,三条任一命中即收尾并保留已得内容):
/// - `next_offset <= cursor`:偏移没前进。既覆盖读到 EOF(本段无字节可读),
///   也覆盖「单行 ≥ [`CONTENT_CHUNK_MAX_BYTES`]、整段找不到换行」的病态会话
///   —— 后者再读一次只会拿回同一段字节;
/// - 累计偏移撞上 [`CONTENT_TOTAL_MAX_BYTES`];
/// - 读出错:**首段就失败才报错**,已经拿到内容的后续段失败按截断处理,
///   宁可少给几条也别让用户看见空白预览。
fn accumulate_session_content<F>(mut fetch: F) -> Result<Vec<AiSessionMessage>, String>
where
    F: FnMut(u64) -> Result<RemoteSessionContent, String>,
{
    let mut messages: Vec<AiSessionMessage> = Vec::new();
    let mut cursor: u64 = 0;
    loop {
        let chunk = match fetch(cursor) {
            Ok(c) => c,
            Err(e) if messages.is_empty() => return Err(e),
            Err(_) => break,
        };
        messages.extend(chunk.messages);
        if chunk.next_offset <= cursor {
            break;
        }
        cursor = chunk.next_offset;
        if cursor >= CONTENT_TOTAL_MAX_BYTES {
            break;
        }
    }
    Ok(messages)
}

/// 读整篇远程会话正文:从 0 起循环续读拼接,直到文件读完。
///
/// 单次 SFTP 读封顶 [`CONTENT_CHUNK_MAX_BYTES`](8 MB),此前调用方只读一段就
/// 返回,超过这个体量的会话余下正文被**静默丢弃**;现在按 `next_offset` 续读,
/// 只在撞上 [`CONTENT_TOTAL_MAX_BYTES`] 护栏时才截断。
///
/// **阻塞**(内部每段各一次 `block_on`),丢 `background_executor`。
pub fn ai_session_content_all(
    conn: &SshConnection,
    session_type: &str,
    session_id: &str,
    project_path: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    accumulate_session_content(|offset| {
        ai_session_content(conn, session_type, session_id, project_path, offset)
    })
}

/// SFTP 读远程会话正文的**一段**。`offset = 0` 从头读;返回 `next_offset` 供续读。
/// 整篇读取走 [`ai_session_content_all`],别直接拿这个的结果当全量。
///
/// **阻塞**,丢 `background_executor`。
pub fn ai_session_content(
    conn: &SshConnection,
    session_type: &str,
    session_id: &str,
    project_path: &str,
    offset: u64,
) -> Result<RemoteSessionContent, String> {
    // id 会拼进远程路径(`<id>.jsonl`)与缓存键,统一在入口挡穿越
    if !session_id_path_safe(session_id) {
        return Err("非法会话 id".to_string());
    }
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let path = locate_remote_session_file(
                st,
                &sftp,
                &conn.id,
                session_type,
                session_id,
                project_path,
            )
            .await?;
            let bytes = sftp
                .read_from_offset(&path, offset, CONTENT_CHUNK_MAX_BYTES)
                .await
                .map_err(|e| format!("读取会话文件失败: {}", e.message()))?;
            // 只取到最后一个换行为止:分段边界永远落在行边界上,多字节字符不会被
            // 拦腰截断,逐段 from_utf8_lossy 与一次性读全量等价
            let (consumed, complete) = split_complete_lines(&bytes);
            let text = String::from_utf8_lossy(complete);
            let messages: Vec<AiSessionMessage> = match session_type {
                "claude" => text.lines().filter_map(claude_message_from_line).collect(),
                "codex" => text.lines().filter_map(codex_message_from_line).collect(),
                other => return Err(format!("不支持的会话类型: {other}")),
            };
            Ok(RemoteSessionContent {
                messages,
                next_offset: offset + consumed as u64,
            })
        }
        .await;
        sftp.close().await;
        result
    })
}

/// 定位会话对应的远程文件:优先取列表扫描时记下的映射;miss(如 app 重启)
/// 再按类型回退定位(claude 走编码目录推导,codex 按 rollout 文件名后缀匹配)。
async fn locate_remote_session_file(
    st: &RemoteSshState,
    sftp: &SftpHandle,
    conn_id: &str,
    session_type: &str,
    session_id: &str,
    project_path: &str,
) -> Result<String, String> {
    let key = format!("{conn_id}|{session_id}");
    // 先绑定再 await:if-let 直接嵌 lock() 会让临时 MutexGuard 活过 await 点,
    // 破坏 future 的 Send 约束。
    let cached_path = lock(&st.session_paths).get(&key).cloned();
    if let Some(p) = cached_path
        && sftp.exists(&p).await
    {
        return Ok(p);
    }

    let home = remote_home(st, sftp, conn_id).await?;
    match session_type {
        "claude" => {
            let projects_dir = join_posix(&join_posix(&home, ".claude"), "projects");
            let encoded = encode_project_path(project_path);
            let normalized = normalize_unix_path(project_path);
            let filename = format!("{session_id}.jsonl");
            let entries = sftp
                .read_dir(&projects_dir)
                .await
                .map_err(|_| "会话文件不存在".to_string())?;
            for e in entries {
                if !e.is_dir {
                    continue;
                }
                let dir = join_posix(&projects_dir, &e.name);
                let matches = e.name == encoded
                    || (is_encoded_variant(&e.name, &encoded)
                        && remote_claude_dir_matches(sftp, &dir, &normalized).await);
                if matches {
                    let p = join_posix(&dir, &filename);
                    if sftp.exists(&p).await {
                        remember_session_path(st, conn_id, session_id, &p);
                        return Ok(p);
                    }
                }
            }
            Err("会话文件不存在".into())
        }
        "codex" => {
            let sessions_dir = join_posix(&join_posix(&home, ".codex"), "sessions");
            let files =
                collect_remote_codex_files(sftp, &sessions_dir, REMOTE_CODEX_SCAN_LIMIT).await;
            for (path, _) in files {
                if codex_filename_matches_session(&path, session_id) {
                    remember_session_path(st, conn_id, session_id, &path);
                    return Ok(path);
                }
            }
            Err("未找到 Codex 会话文件,请刷新会话列表后重试".into())
        }
        other => Err(format!("不支持的会话类型: {other}")),
    }
}

// ---------------------------------------------------------------------------
// tests(全部自 `src-tauri/src/remote_ssh.rs` 的同名测试原样搬来,不触网)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: &str) -> SshConnection {
        SshConnection {
            id: id.to_string(),
            name: format!("conn-{id}"),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            password: None,
            identity_file: None,
            group: None,
        }
    }

    fn remote_baseline(conn: &SshConnection, bytes: &[u8]) -> RemoteFileBaseline {
        RemoteFileBaseline {
            connection_id: conn.id.clone(),
            connection_fingerprint: connection_fingerprint(conn),
            canonical_root: "/srv/project".into(),
            canonical_path: "/srv/project/src/main.rs".into(),
            bytes: Arc::from(bytes),
        }
    }

    // --- 断链查找 ---

    #[test]
    fn find_connection_hits_by_id() {
        let list = vec![conn("a"), conn("b")];
        assert_eq!(find_connection(&list, "b").unwrap().id, "b");
    }

    #[test]
    fn find_connection_reports_broken_link_with_id() {
        let err = find_connection(&[conn("a")], "gone").unwrap_err();
        assert!(err.contains("gone"), "错误文案要带 id 便于排查: {err}");
        assert!(err.contains("SSH 连接不存在或已被删除"));
        // 空表同样是断链而不是 panic
        assert!(find_connection(&[], "x").is_err());
    }

    #[test]
    fn remote_document_connection_fingerprint_tracks_endpoint_and_credentials() {
        let base = conn("a");
        let fingerprint = connection_fingerprint(&base);

        let mut changed = base.clone();
        changed.host = "other-host".into();
        assert_ne!(connection_fingerprint(&changed), fingerprint);
        changed = base.clone();
        changed.port = 2222;
        assert_ne!(connection_fingerprint(&changed), fingerprint);
        changed = base.clone();
        changed.user = "other-user".into();
        assert_ne!(connection_fingerprint(&changed), fingerprint);
        changed = base.clone();
        changed.password = Some("new-password".into());
        assert_ne!(connection_fingerprint(&changed), fingerprint);
        changed = base.clone();
        changed.identity_file = Some("/keys/new".into());
        assert_ne!(connection_fingerprint(&changed), fingerprint);

        // Display-only edits must not invalidate an otherwise identical remote
        // filesystem identity.
        changed = base.clone();
        changed.name = "renamed".into();
        changed.group = Some("other group".into());
        assert_eq!(connection_fingerprint(&changed), fingerprint);
    }

    #[test]
    fn remote_document_read_classifies_text_binary_and_oversize() {
        let connection = conn("a");
        let text = build_remote_file_read_result(
            &connection,
            "/srv/project".into(),
            "/srv/project/notes.md".into(),
            SftpBoundedFileRead::Complete(b"# title\n".to_vec()),
        );
        assert_eq!(text.content.content, "# title\n");
        assert!(!text.content.is_binary);
        assert!(!text.content.too_large);
        assert_eq!(
            text.baseline.as_ref().map(|value| value.byte_len()),
            Some(8)
        );

        let binary = build_remote_file_read_result(
            &connection,
            "/srv/project".into(),
            "/srv/project/image.bin".into(),
            SftpBoundedFileRead::Complete(vec![0xff, 0xfe]),
        );
        assert!(binary.content.is_binary);
        assert!(!binary.content.too_large);
        assert!(binary.baseline.is_none());

        let oversize = build_remote_file_read_result(
            &connection,
            "/srv/project".into(),
            "/srv/project/large.txt".into(),
            SftpBoundedFileRead::TooLarge,
        );
        assert!(oversize.content.too_large);
        assert!(!oversize.content.is_binary);
        assert!(oversize.baseline.is_none());
    }

    #[test]
    fn remote_document_save_conflict_requires_explicit_force() {
        let connection = conn("a");
        let baseline = remote_baseline(&connection, b"original");
        assert!(!should_block_remote_save(
            &SftpBoundedFileRead::Complete(b"original".to_vec()),
            &baseline,
            false
        ));
        assert!(should_block_remote_save(
            &SftpBoundedFileRead::Complete(b"changed".to_vec()),
            &baseline,
            false
        ));
        assert!(should_block_remote_save(
            &SftpBoundedFileRead::TooLarge,
            &baseline,
            false
        ));
        assert!(!should_block_remote_save(
            &SftpBoundedFileRead::Complete(b"changed".to_vec()),
            &baseline,
            true
        ));
        assert!(should_block_remote_save(
            &SftpBoundedFileRead::TooLarge,
            &baseline,
            true
        ));
    }

    #[test]
    fn remote_document_baseline_rejects_connection_and_path_changes() {
        let connection = conn("a");
        let baseline = remote_baseline(&connection, b"original");
        assert!(validate_remote_file_baseline_connection(&connection, &baseline).is_ok());
        assert!(
            validate_remote_file_baseline_path(
                &baseline,
                "/srv/project",
                "/srv/project/src/main.rs"
            )
            .is_ok()
        );

        let mut changed_connection = connection.clone();
        changed_connection.host = "new-host".into();
        assert!(validate_remote_file_baseline_connection(&changed_connection, &baseline).is_err());
        assert!(
            validate_remote_file_baseline_path(&baseline, "/srv/other", "/srv/other/src/main.rs")
                .is_err()
        );
        assert!(
            validate_remote_file_baseline_path(
                &baseline,
                "/srv/project",
                "/srv/project/src/other.rs"
            )
            .is_err()
        );
    }

    // --- POSIX 路径拼接 / 相对化 ---

    #[test]
    fn join_posix_handles_root_and_trailing_slash() {
        assert_eq!(join_posix("/", "home"), "/home");
        assert_eq!(join_posix("/home/u", "proj"), "/home/u/proj");
        assert_eq!(join_posix("/home/u/", "proj"), "/home/u/proj");
    }

    #[test]
    fn posix_relative_computes_relative_paths() {
        assert_eq!(
            posix_relative("/home/u/proj", "/home/u/proj/src/main.rs").as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            posix_relative("/home/u/proj", "/home/u/proj").as_deref(),
            Some("")
        );
        // 尾部斜杠不影响
        assert_eq!(
            posix_relative("/home/u/proj/", "/home/u/proj/a").as_deref(),
            Some("a")
        );
        // 根目录项目
        assert_eq!(
            posix_relative("/", "/etc/hosts").as_deref(),
            Some("etc/hosts")
        );
    }

    #[test]
    fn posix_relative_rejects_sibling_prefix() {
        // `/home/u/proj2` 不在 `/home/u/proj` 之下,不能误判
        assert!(posix_relative("/home/u/proj", "/home/u/proj2/file").is_none());
        assert!(posix_relative("/home/u/proj", "/other/place").is_none());
    }

    #[test]
    fn parent_posix_handles_root_and_trailing_slashes() {
        assert_eq!(parent_posix("/home/u/project"), Some("/home/u".into()));
        assert_eq!(parent_posix("/home/u/project/"), Some("/home/u".into()));
        assert_eq!(parent_posix("/home"), Some("/".into()));
        assert_eq!(parent_posix("/"), None);
        assert_eq!(parent_posix(""), None);
    }

    #[test]
    fn keep_both_names_preserve_extensions_and_dotfiles() {
        assert_eq!(keep_both_name("notes.txt", 1), "notes copy.txt");
        assert_eq!(keep_both_name("notes.txt", 2), "notes copy 2.txt");
        assert_eq!(keep_both_name("archive.tar.gz", 1), "archive.tar copy.gz");
        assert_eq!(keep_both_name("folder", 1), "folder copy");
        assert_eq!(keep_both_name(".env", 1), ".env copy");
    }

    #[test]
    fn remote_path_validation_rejects_escape_and_host_separator_names() {
        assert_eq!(
            normalize_absolute_posix("/work/src/./main").unwrap(),
            "/work/src/main"
        );
        assert!(normalize_absolute_posix("/work/../etc").is_err());
        assert!(!valid_remote_name("a/b"));
        assert!(!valid_remote_name("a\\b"));
        assert!(!valid_remote_name("C:evil.exe"));
        assert!(!valid_remote_name("file:stream"));
        assert!(!valid_remote_name(".."));
    }

    #[test]
    fn local_download_targets_stay_inside_root() {
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\downloads")
        } else {
            PathBuf::from("/downloads")
        };
        let outside = if cfg!(windows) {
            PathBuf::from(r"D:\outside")
        } else {
            PathBuf::from("/outside")
        };

        assert_eq!(
            checked_local_download_child(&root, &root, "safe.txt").unwrap(),
            root.join("safe.txt")
        );
        assert!(checked_local_download_child(&root, &root, "C:evil.exe").is_err());
        assert!(checked_local_download_child(&root, &root, "a\\b").is_err());
        assert!(checked_local_download_child(&root, &outside, "safe.txt").is_err());
        assert!(ensure_local_download_target(&root, &outside).is_err());
        assert!(download_conflicts(&root, &[PathBuf::from("/remote/C:evil.exe")]).is_err());
    }

    #[test]
    fn delete_child_validation_allows_remote_backslashes_but_rejects_separators() {
        assert!(valid_sftp_child_name("a\\b"));
        assert!(!valid_sftp_child_name("a/b"));
        assert!(!valid_sftp_child_name("."));
        assert!(!valid_sftp_child_name(".."));
        assert!(!valid_sftp_child_name("a\0b"));
    }

    #[test]
    fn delete_shell_command_quotes_parent_and_leaf() {
        assert_eq!(shell_quote_posix("a'b"), "'a'\\''b'");
        let command = remote_delete_command(
            "/srv/project/a'b",
            "/srv/project/.proof'file",
            "nonce'value",
        )
        .unwrap();
        assert!(command.contains("cd -P '/srv/project'"));
        assert!(command.contains("[ ! -L './a'\\''b' ]"));
        assert!(command.contains("[ \"$(cat -- './.proof'\\''file')\" = 'nonce'\\''value' ]"));
        assert!(command.contains("rm -f -- './.proof'\\''file'"));
        assert!(command.contains("rm -rf -- './a'\\''b'"));
        assert!(!command.contains("rm -rf -- '/srv/project"));
    }

    // --- ~ 展开 ---

    #[test]
    fn expand_tilde_expands_home_forms() {
        assert_eq!(expand_tilde("~", "/home/u"), "/home/u");
        assert_eq!(expand_tilde("", "/home/u"), "/home/u");
        assert_eq!(expand_tilde("  ~  ", "/home/u"), "/home/u");
        assert_eq!(expand_tilde("~/proj", "/home/u"), "/home/u/proj");
        assert_eq!(expand_tilde("~/a/b", "/home/u/"), "/home/u/a/b");
        assert_eq!(expand_tilde("~/", "/home/u"), "/home/u");
    }

    #[test]
    fn expand_tilde_leaves_absolute_and_other_paths_alone() {
        assert_eq!(expand_tilde("/var/www", "/home/u"), "/var/www");
        // `~user` 形式不支持展开,原样交给 canonicalize 报错
        assert_eq!(expand_tilde("~other/x", "/home/u"), "~other/x");
        assert_eq!(expand_tilde("relative/dir", "/home/u"), "relative/dir");
    }

    // --- 粘贴落盘目录解析(issue #36) ---

    #[test]
    fn resolve_paste_dir_defaults_to_project_relative() {
        // 默认形态:相对项目根,图片落在项目内
        assert_eq!(
            resolve_paste_dir("/home/u/proj", "/home/u", ".mini-term/pasted").unwrap(),
            "/home/u/proj/.mini-term/pasted"
        );
        // 空配置回落到默认值,而不是把文件丢到项目根
        assert_eq!(
            resolve_paste_dir("/home/u/proj", "/home/u", "   ").unwrap(),
            "/home/u/proj/.mini-term/pasted"
        );
        // 项目根带尾斜杠不产生双斜杠
        assert_eq!(
            resolve_paste_dir("/home/u/proj/", "/home/u", "assets").unwrap(),
            "/home/u/proj/assets"
        );
    }

    #[test]
    fn resolve_paste_dir_default_matches_config_default() {
        // 本模块的默认值常量与 mt-config 的那份必须同值,
        // 否则「设置里清空 → 落盘目录」两侧会漂。
        assert_eq!(
            DEFAULT_REMOTE_PASTE_DIR,
            mt_config::default_remote_paste_dir()
        );
    }

    #[test]
    fn resolve_paste_dir_supports_absolute_and_tilde() {
        assert_eq!(
            resolve_paste_dir("/home/u/proj", "/home/u", "/tmp/mini-term").unwrap(),
            "/tmp/mini-term"
        );
        assert_eq!(
            resolve_paste_dir("/home/u/proj", "/home/u", "~/uploads").unwrap(),
            "/home/u/uploads"
        );
        assert_eq!(
            resolve_paste_dir("/home/u/proj", "/home/u", "~").unwrap(),
            "/home/u"
        );
        // 尾斜杠被归一,避免拼出 `//file`
        assert_eq!(
            resolve_paste_dir("/home/u/proj", "/home/u", "/tmp/x/").unwrap(),
            "/tmp/x"
        );
    }

    #[test]
    fn resolve_paste_dir_rejects_parent_traversal() {
        // 这条路径会拼进 SFTP 写操作,`..` 逃逸必须挡在解析层
        assert!(resolve_paste_dir("/home/u/proj", "/home/u", "../outside").is_err());
        assert!(resolve_paste_dir("/home/u/proj", "/home/u", "a/../../b").is_err());
        assert!(resolve_paste_dir("/home/u/proj", "/home/u", "/tmp/../etc").is_err());
        assert!(resolve_paste_dir("/home/u/proj", "/home/u", "~/../root").is_err());
        // 反斜杠写法先归一再判,不能绕过
        assert!(resolve_paste_dir("/home/u/proj", "/home/u", r"..\outside").is_err());
    }

    #[test]
    fn resolve_paste_dir_rejects_traversal_from_project_path_too() {
        // `..` 也可能来自 project_path(调用方传入,非用户在设置页填的那半)。
        // 判定放在归一之后就是为了一处覆盖两个来源 —— 返回值恒不含 `..`。
        assert!(resolve_paste_dir("/home/u/../etc", "/home/u", "assets").is_err());
        assert!(resolve_paste_dir("/home/u/proj/..", "/home/u", ".mini-term").is_err());
        // home 带 `..` 的 `~` 展开同样挡住
        assert!(resolve_paste_dir("/home/u/proj", "/home/../root", "~/x").is_err());
    }

    #[test]
    fn resolve_paste_dir_normalizes_dot_segments_and_double_slash() {
        // `.` 段必须被吃掉:否则 `/proj/.` 会被下游当成「严格位于项目内」,
        // 而它其实就是项目根 —— 自忽略 .gitignore 会写到仓库根,忽略整个仓库。
        assert_eq!(
            resolve_paste_dir("/home/u/proj", "/home/u", ".").unwrap(),
            "/home/u/proj"
        );
        assert_eq!(
            resolve_paste_dir("/home/u/proj", "/home/u", "./assets").unwrap(),
            "/home/u/proj/assets"
        );
        assert_eq!(
            resolve_paste_dir("/home/u/proj", "/home/u", "a//b").unwrap(),
            "/home/u/proj/a/b"
        );
        // 点开头的目录名不是 `.` 段,不能被误删
        assert_eq!(
            resolve_paste_dir("/home/u/proj", "/home/u", ".mini-term").unwrap(),
            "/home/u/proj/.mini-term"
        );
    }

    #[test]
    fn paste_dir_at_project_root_is_not_strictly_inside() {
        // 自忽略 .gitignore 的守卫条件:rel 非空才写。
        // 解析成项目根本身时 rel 为空 —— 绝不能在仓库根写下内容为 `*` 的 .gitignore。
        let dir = resolve_paste_dir("/home/u/proj", "/home/u", ".").unwrap();
        assert_eq!(posix_relative("/home/u/proj", &dir).as_deref(), Some(""));

        // 默认形态才是「严格位于项目内」,应当写
        let nested = resolve_paste_dir("/home/u/proj", "/home/u", ".mini-term/pasted").unwrap();
        assert_eq!(
            posix_relative("/home/u/proj", &nested).as_deref(),
            Some(".mini-term/pasted")
        );

        // 项目外的绝对路径不参与 .gitignore 逻辑
        let outside = resolve_paste_dir("/home/u/proj", "/home/u", "/tmp/mini-term").unwrap();
        assert!(posix_relative("/home/u/proj", &outside).is_none());
    }

    #[test]
    fn resolve_paste_dir_normalizes_backslash_input() {
        // 用户顺手填了 Windows 风格分隔符,不该原样拼进远端路径
        assert_eq!(
            resolve_paste_dir("/home/u/proj", "/home/u", r".mini-term\pasted").unwrap(),
            "/home/u/proj/.mini-term/pasted"
        );
    }

    #[test]
    fn resolve_paste_dir_rejects_relative_project_root() {
        // 相对目录 + 非绝对项目根 = 拼不出合法远端路径,明确报错而不是拼个怪路径
        assert!(resolve_paste_dir("proj", "/home/u", "assets").is_err());
        // 但绝对 dest_dir 不依赖项目根,仍应通过
        assert!(resolve_paste_dir("proj", "/home/u", "/tmp/x").is_ok());
    }

    // --- 粘贴文件名提取 ---

    #[test]
    fn paste_file_name_strips_both_separators() {
        assert_eq!(
            paste_file_name(r"C:\Users\u\AppData\Local\Temp\clip-123.png").unwrap(),
            "clip-123.png"
        );
        assert_eq!(paste_file_name("/tmp/paste-9.txt").unwrap(), "paste-9.txt");
        // 混合分隔符:不能让 `\` 残留进远端路径
        assert_eq!(
            paste_file_name(r"C:/Temp\clip-1.png").unwrap(),
            "clip-1.png"
        );
    }

    #[test]
    fn paste_file_name_rejects_degenerate_input() {
        assert!(paste_file_name("").is_err());
        assert!(paste_file_name(r"C:\Temp\").is_err());
        assert!(paste_file_name("/tmp/.").is_err());
        assert!(paste_file_name("..").is_err());
    }

    // --- 时间戳兜底 ---

    #[test]
    fn unix_secs_to_iso_known_values() {
        assert_eq!(unix_secs_to_iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(unix_secs_to_iso(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(unix_secs_to_iso(86_400), "1970-01-02T00:00:00Z");
        // 2000-03-01(闰年 2 月 29 日之后)
        assert_eq!(unix_secs_to_iso(951_868_800), "2000-03-01T00:00:00Z");
        // 2026-07-05T12:34:56Z
        assert_eq!(unix_secs_to_iso(1_783_254_896), "2026-07-05T12:34:56Z");
    }

    // --- 增量读取的完整行切分 ---

    #[test]
    fn split_complete_lines_cuts_at_last_newline() {
        let bytes = b"{\"a\":1}\n{\"b\":2}\n{\"partial";
        let (consumed, complete) = split_complete_lines(bytes);
        assert_eq!(consumed, 16);
        assert_eq!(complete, b"{\"a\":1}\n{\"b\":2}\n");
    }

    #[test]
    fn split_complete_lines_no_newline_consumes_nothing() {
        let (consumed, complete) = split_complete_lines(b"half a line");
        assert_eq!(consumed, 0);
        assert!(complete.is_empty());
    }

    #[test]
    fn split_complete_lines_empty_input() {
        let (consumed, complete) = split_complete_lines(b"");
        assert_eq!(consumed, 0);
        assert!(complete.is_empty());
    }

    // --- codex 文件名匹配 ---

    #[test]
    fn codex_filename_matches_session_by_suffix() {
        let p = "/home/u/.codex/sessions/2026/07/05/rollout-2026-07-05T10-00-00-abc-123.jsonl";
        assert!(codex_filename_matches_session(p, "abc-123"));
        assert!(!codex_filename_matches_session(p, "def-456"));
        // 空 id 永不匹配(防 ends_with("") 恒真)
        assert!(!codex_filename_matches_session(p, ""));
        // 非 .jsonl 不匹配
        assert!(!codex_filename_matches_session(
            "/x/rollout-abc-123.txt",
            "abc-123"
        ));
    }

    // --- session_index 解析 ---

    #[test]
    fn parse_codex_thread_names_extracts_pairs() {
        let content = "\
{\"id\":\"s1\",\"thread_name\":\"重构池\"}\n\
not json\n\
{\"id\":\"s2\"}\n\
{\"id\":\"s3\",\"thread_name\":\"fix bug\"}\n";
        let map = parse_codex_thread_names(content);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("s1").map(String::as_str), Some("重构池"));
        assert_eq!(map.get("s3").map(String::as_str), Some("fix bug"));
    }

    // --- state 基本行为(不触网) ---

    #[test]
    fn remote_state_caches_are_isolated_per_key() {
        let st = RemoteSshState::new();
        remember_session_path(&st, "c1", "s1", "/p/a.jsonl");
        remember_session_path(&st, "c2", "s1", "/p/b.jsonl");
        assert_eq!(
            lock(&st.session_paths).get("c1|s1").map(String::as_str),
            Some("/p/a.jsonl")
        );
        assert_eq!(
            lock(&st.session_paths).get("c2|s1").map(String::as_str),
            Some("/p/b.jsonl")
        );
    }

    #[test]
    fn invalidate_connection_clears_only_that_connections_caches() {
        let st = RemoteSshState::new();
        remember_session_path(&st, "c1", "s1", "/p/a.jsonl");
        remember_session_path(&st, "c2", "s1", "/p/b.jsonl");
        lock(&st.home_cache).insert("c1".into(), "/home/u1".into());
        lock(&st.home_cache).insert("c2".into(), "/home/u2".into());
        lock(&st.gitignore_cache).insert(
            "c1|/home/u1/proj".into(),
            Arc::new(TextGitignore::from_text("target/\n")),
        );
        lock(&st.gitignore_cache).insert(
            "c2|/home/u2/proj".into(),
            Arc::new(TextGitignore::from_text("target/\n")),
        );

        st.invalidate_connection("c1");

        // c1 的三张缓存全清。
        assert!(lock(&st.session_paths).get("c1|s1").is_none());
        assert!(lock(&st.home_cache).get("c1").is_none());
        assert!(lock(&st.gitignore_cache).get("c1|/home/u1/proj").is_none());
        // c2 一条都不许被误伤(前缀匹配必须带上分隔符)。
        assert_eq!(
            lock(&st.session_paths).get("c2|s1").map(String::as_str),
            Some("/p/b.jsonl")
        );
        assert_eq!(
            lock(&st.home_cache).get("c2").map(String::as_str),
            Some("/home/u2")
        );
        assert!(lock(&st.gitignore_cache).get("c2|/home/u2/proj").is_some());
        // 池没建过 → 不该为了 evict 现建一个运行时。
        assert!(lock(&st.runtime).is_none(), "不该为了 evict 现建运行时");
    }

    #[test]
    fn shutdown_without_pool_is_noop() {
        // 从未用过远程能力时退出:池与运行时都没建,不该起运行时、更不该 panic。
        let st = RemoteSshState::new();
        st.shutdown_pool_blocking();
        assert!(lock(&st.runtime).is_none(), "不该为了关池现建运行时");
    }

    #[test]
    fn upload_conflicts_include_existing_and_duplicate_batch_names_once() {
        let existing = HashSet::from(["existing.txt".to_string()]);
        let paths = vec![
            PathBuf::from("first/existing.txt"),
            PathBuf::from("first/new.txt"),
            PathBuf::from("second/new.txt"),
            PathBuf::from("third/new.txt"),
            PathBuf::from("second/existing.txt"),
        ];

        assert_eq!(
            collect_upload_conflicts(&existing, &paths),
            vec!["existing.txt".to_string(), "new.txt".to_string()]
        );
    }

    #[test]
    fn session_id_guard_rejects_traversal_before_touching_network() {
        // 非法 id 必须在开 SFTP 之前就被挡下(否则 `../` 会拼进远端路径)。
        // 这条不触网:守卫在函数第一行。
        let c = conn("c1");
        let err = ai_session_content(&c, "claude", "../etc/passwd", "/p", 0).unwrap_err();
        assert_eq!(err, "非法会话 id");
        let err2 = ai_session_content(&c, "claude", "a/b", "/p", 0).unwrap_err();
        assert_eq!(err2, "非法会话 id");
        // 全量入口共用同一道守卫
        let err3 = ai_session_content_all(&c, "claude", "../etc/passwd", "/p").unwrap_err();
        assert_eq!(err3, "非法会话 id");
    }

    // --- 会话正文续读循环 ---

    fn msg(text: &str) -> AiSessionMessage {
        AiSessionMessage {
            role: "user".into(),
            content: text.into(),
            timestamp: String::new(),
        }
    }

    #[test]
    fn accumulate_session_content_concatenates_until_exhausted() {
        // 三段:每段推进偏移,最后一段偏移不再前进(EOF)→ 拼接全部消息
        let chunks = [
            (vec![msg("a"), msg("b")], 10u64),
            (vec![msg("c")], 20u64),
            (vec![], 20u64),
        ];
        let mut calls: Vec<u64> = Vec::new();
        let mut i = 0usize;
        let out = accumulate_session_content(|offset| {
            calls.push(offset);
            let (messages, next_offset) = chunks[i].clone();
            i += 1;
            Ok(RemoteSessionContent {
                messages,
                next_offset,
            })
        })
        .unwrap();

        assert_eq!(
            calls,
            vec![0, 10, 20],
            "每轮都应带上上次的 next_offset 续读"
        );
        let texts: Vec<&str> = out.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(texts, vec!["a", "b", "c"]);
    }

    #[test]
    fn accumulate_session_content_stops_when_offset_does_not_advance() {
        // consumed == 0(整段没有换行,单行 ≥ 8MB 的病态会话):next_offset 原地
        // 不动。必须**只调一次**就收尾,否则是死循环。
        let mut calls = 0usize;
        let out = accumulate_session_content(|offset| {
            calls += 1;
            assert!(calls < 5, "偏移不前进却仍在续读 —— 死循环");
            Ok(RemoteSessionContent {
                messages: vec![msg("partial")],
                next_offset: offset, // 一步没走
            })
        })
        .unwrap();

        assert_eq!(calls, 1, "偏移不前进应立即停");
        assert_eq!(out.len(), 1, "已解析到的内容要保留,不能连带丢掉");
    }

    #[test]
    fn accumulate_session_content_caps_total_bytes() {
        // 每轮都「读满」一整块:撞到总量护栏就停,不会无限吃内存
        let mut calls = 0usize;
        let out = accumulate_session_content(|offset| {
            calls += 1;
            assert!(calls < 1000, "总量护栏没生效");
            Ok(RemoteSessionContent {
                messages: vec![msg("chunk")],
                next_offset: offset + CONTENT_CHUNK_MAX_BYTES as u64,
            })
        })
        .unwrap();

        let expected = (CONTENT_TOTAL_MAX_BYTES / CONTENT_CHUNK_MAX_BYTES as u64) as usize;
        assert_eq!(calls, expected, "读满 64 MB 即止");
        assert_eq!(out.len(), expected);
    }

    #[test]
    fn accumulate_session_content_error_policy() {
        // 首段就失败 → 报错(用户看得到原因)
        let err = accumulate_session_content(|_| Err("boom".to_string())).unwrap_err();
        assert_eq!(err, "boom");

        // 后续段失败 → 按截断处理,保留已拿到的内容
        let mut calls = 0usize;
        let out = accumulate_session_content(|offset| {
            calls += 1;
            if calls == 1 {
                Ok(RemoteSessionContent {
                    messages: vec![msg("first")],
                    next_offset: offset + 8,
                })
            } else {
                Err("网络断了".to_string())
            }
        })
        .unwrap();
        assert_eq!(out.len(), 1);
    }
}
