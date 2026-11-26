//! SSH 远程项目的服务层(audit #28 的后端半场,BB-a 批)。
//!
//! # 为什么是独立 crate
//!
//! 原先是 mt-app 的 `remote_ssh/` 模块,2026-09 整块下沉到这里,逻辑与单测逐字
//! 未动。它从头到尾不碰 gpui,留在 mt-app 里只会让这 40 多个单测陪着 GPUI 与
//! 三十多个 tree-sitter 语法一起链接;`cargo test -p mt-remote` 现在不编 gpui。
//! 对壳的唯一依赖(解开已存 SSH 密码交给 autofill)改为直连底层 `mt-secret`,
//! 与 mt-app 的 `secrets::reveal_password` 走的是同一个进程级凭据库,见
//! [`prepare_remote_launch`]。
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
//! 终端里的内置 SSH(远程项目 pane 的 shell、右键「SSH 连接」接管)不走会话池:每个终端独占
//! 一条连接,建连放在共享运行时上、提示通道包成不含 tokio 类型的样子交给 GPUI 壳,见
//! [`open_terminal_shell`]。系统 OpenSSH 回退那条路仍是 [`prepare_remote_launch`]。
//!
//! [`RemoteGitWorkspace`] manages lazy packaged-helper deployment and persistent Git channels.
//! Managed Git IO runs through [`run_git_on`] or the workspace's bounded async request queue;
//! GPUI executor threads only await and never wait on a blocking service mutex.
//! [`set_project_git_permission`] controls default-denied, project/endpoint/root scoped grants.
//! Explicit-path [`RemoteGitClient`] remains a synchronous transport independent of that gate.
//!
//! # 线程口径(与 Tauri 版的唯一结构性差异)
//!
//! 原版是 `#[tauri::command(async)]`,跑在 Tauri 自带的全局 tokio runtime 上。
//! GPUI 没有 tokio,主线程也不能阻塞,于是:
//!
//! - **本模块自持进程级共享的小 tokio 运行时**(见 [`RemoteSshState`] 的 `runtime`,
//!   由 [`shared_runtime_handle`] 暴露给同进程内其它异步子系统如 `mt_relay` 复用)——
//!   懒建、2 个工作线程、进程内唯一,避免远程 SSH 与移动端中转各起一套线程池;
//! - Existing synchronous SSH APIs retain their blocking boundary. Git UI callers use
//!   `run_git_on(&context, move || captured.some_git_method()).await`, which admits closures
//!   asynchronously onto the service-owned executor. Managed synchronous requests reject
//!   calls outside that executor; local core methods remain direct synchronous calls.
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
//!
//! Managed workspaces bind permission and UI identity to project/configuration/root. Repository
//! mutation gates share host/port/user/path identity independent of saved credentials. A
//! credential-only successor retains the originating project's unknown operation and routes
//! recovery through its original actor using the newly granted credentials. Running writes
//! are distinct from uncertain
//! outcomes; neither is replayed. Recovery snapshots retain the original repository, attempt the
//! old helper's barrier, and observe HEAD/index/operation/status through a reusable read channel.
//! Lost barriers require explicit risk acknowledgement; revoked projects may instead give a
//! single-use recovery-only consent (fixed observation/ack, never a normal grant). Offline
//! abandonment consumes an owner/generation/source-bound confirmation without waiting for
//! the actor or network, deploying, connecting, or issuing Git requests. Existing channels
//! still receive normal bounded asynchronous EOF/close cleanup; this is not cancellation
//! or proof that the original remote write has finished.
//! Shared repository write rejections preserve the actual unknown repository and all
//! owner project IDs in [`GitFailure::RepositoryBlocked`], not an unknown outcome for
//! the rejected caller. Workspace recovery metadata can list the configured root and
//! nested repositories without discovery or a grant; metadata never grants recovery.
//! Source configuration mutations hold [`remote_git_source_change_guard`] across their
//! pending check, mutation and grant synchronization; dispatch atomically publishes Running
//! under the same guard before releasing it for network IO.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use mt_config::SshConnection;
use mt_project::fs::{MAX_FILE_VIEW_SIZE, TextGitignore};
use mt_ssh::{CachedSession, SftpHandle, SshPool};

mod conn_test;
mod delete;
mod dirs;
mod files;
mod git;
mod git_context;
mod git_service;
mod paths;
mod session_delete;
mod sessions;
mod terminal_shell;
mod transfer;

pub use conn_test::*;
pub use delete::*;
pub use dirs::*;
pub use files::*;
pub use git::RemoteGitClient;
pub use git_context::GitContext;
pub use git_service::{
    MissingHelper, REMOTE_GIT_HELPER_MISSING, RemoteGitOfflineAbandonConfirmation,
    RemoteGitOperationKind, RemoteGitRecoveryBlocker, RemoteGitRecoveryConsent,
    RemoteGitRecoverySnapshot, RemoteGitUnknownWrite, RemoteGitWorkspace, missing_helper,
    remote_git_source_change_guard, run_git, run_git_on, set_helper_provisioner,
    set_project_git_permission,
};
pub use mt_git_helper::GitFailure;
// 「测试连接」结果里的类型(mt-app 不直接依赖 mt-ssh,经这里取)
pub use mt_ssh::{AuthFailureReport, AuthMethodUsed, ProbeSuccess};
pub use paths::*;
pub use session_delete::*;
pub use sessions::*;
pub use terminal_shell::*;
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
/// 交给池的上传时限(`mt_ssh::run_sftp_upload_until`)比 [`PASTE_UPLOAD_TOTAL_TIMEOUT`]
/// 提前这么多到点:池自己收尾、报出写明远端文件状态的错误,外层的墙钟上限只作兜底 ——
/// 替换一旦开始要跑完,不能在那时被截断。协议层每请求超时随之取池时限前的剩余时间。
const PASTE_UPLOAD_FINISH_GRACE: Duration = Duration::from_secs(10);
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
const REMOTE_AGY_SCAN_LIMIT: usize = 100;
/// Claude 会话标题提取:读文件头部的字节上限(首条 user 消息几乎总在最前面,
/// 但个别文件首行是巨大的 file-history-snapshot,给足余量)。
const CLAUDE_TITLE_HEAD_BYTES: usize = 256 * 1024;
/// Codex 会话 meta + 标题提取:session_meta 在第 1 行,64KB 覆盖含长 instructions 的情况。
const CODEX_META_HEAD_BYTES: usize = 64 * 1024;
/// agy 会话标题兜底(仅当索引中 title/preview 均为空时):读 transcript 头部的字节上限。
const AGY_TITLE_HEAD_BYTES: usize = 64 * 1024;
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
    /// 懒初始化的进程级共享 tokio 运行时:承载 russh / russh-sftp / 托管远端 Git
    /// IO,并经 [`shared_runtime_handle`] 复用给 `mt-relay`(gpui 执行器喂不动 tokio
    /// 的 net/time 驱动,统一收敛到这一份小运行时,避免进程内并存两套线程池)。
    ///
    /// 2 个工作线程:全部操作都是网络等待型。**不主动 shutdown**(见
    /// [`RemoteSshState::shutdown_pool_blocking`] 的注释)。
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

    /// 拿(或懒建)进程级共享 tokio 运行时。建不起来时返回明确错误 —— 全部远程能力
    /// 随之报错,而不是 panic 掉整个应用。
    fn runtime(&self) -> Result<Arc<tokio::runtime::Runtime>, String> {
        let mut guard = lock(&self.runtime);
        if let Some(rt) = guard.as_ref() {
            return Ok(rt.clone());
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("mt-async-io")
            .build()
            .map_err(|e| format!("共享异步运行时不可用: {e}"))?;
        let rt = Arc::new(rt);
        *guard = Some(rt.clone());
        Ok(rt)
    }

    /// 取共享 tokio 运行时的轻量 `Handle`(只按需懒建运行时,不触发 `SshPool` 初始化)。
    pub(crate) fn runtime_handle(&self) -> Result<tokio::runtime::Handle, String> {
        self.runtime().map(|rt| rt.handle().clone())
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
        tracing::info!("[remote-ssh] draining ssh session pool on exit");
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

/// 取进程级共享 tokio 运行时的 `Handle`,供同进程内其它异步子系统(如 `mt-relay`)
/// 经 `with_runtime` 复用,避免再起第二套 tokio 线程池。
///
/// 底层由 [`state`] 懒建(2 个工作线程、`enable_all` 含 time + net 驱动,线程名
/// `mt-async-io`),不会顺带初始化 `SshPool`;退出时 [`shutdown_on_exit`] 刻意不
/// drop 该运行时,因此与 `mt-relay` 共享不存在退出顺序竞态。
pub fn shared_runtime_handle() -> Result<tokio::runtime::Handle, String> {
    state().runtime_handle()
}

struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T> std::future::Future for AbortOnDrop<T> {
    type Output = Result<T, tokio::task::JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(cx)
    }
}

/// 在进程级共享 tokio 运行时(`mt-async-io`)上执行一段异步 future 并等待其结果。
///
/// 供不直接依赖 `tokio` 的上层 crate(如 `mt-app` 的异步 `reqwest::Client` 调用)
/// 在 GPUI 异步任务中 `.await` 需要 tokio reactor / timer 的 future:既不阻塞 GPUI
/// 后台线程池,也不另起一套 tokio 运行时;调用方 future 被取消(drop)时同步 abort
/// 底层任务,立即释放慢网连接。仅懒建运行时,不触发 `SshPool` 初始化。
pub async fn run_on_shared_runtime<F, T>(fut: F) -> Result<T, String>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let handle = shared_runtime_handle()?;
    AbortOnDrop(handle.spawn(fut))
        .await
        .map_err(|err| format!("共享异步任务执行失败: {err}"))
}

/// 给一段 future 加时限:到点返回 `None`(那段 future 随之被丢弃)。
///
/// 给不直接依赖 `tokio` 的上层(mt-app 的扩展下载:连接 15s、卡住 60s 无数据判失败)取
/// tokio 计时器用。⚠️ 只能在共享运行时上的任务里 await —— 例如 [`run_on_shared_runtime`]
/// 包着的那段 future 内部;计时器要 tokio 的 time 驱动,别的执行器上一 poll 就 panic。
pub async fn timeout_on_shared_runtime<F>(duration: Duration, fut: F) -> Option<F::Output>
where
    F: std::future::Future,
{
    tokio::time::timeout(duration, fut).await.ok()
}

/// 退出钩子:优雅关池。对应原版 `lib.rs` 在 `RunEvent::Exit` 里的那一调。
pub fn shutdown_on_exit() {
    git_service::shutdown_workspaces();
    state().shutdown_pool_blocking();
}

/// 连接配置被改动 / 删除后的失效入口(见
/// [`RemoteSshState::invalidate_connection`])。**由 `AppStore` 的写入侧调用**,
/// 主线程直接调即可,不阻塞。
pub fn invalidate_connection(conn_id: &str) {
    git_service::invalidate_workspaces(conn_id);
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
            tracing::warn!("[remote-ssh] sftp open failed (transport), retrying once: {e}");
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
/// - 私钥文件不存在 / 复制临时副本失败(`mt_pty::prepare_ssh_key`)。
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
        Some(path) => Some(mt_pty::prepare_ssh_key(path)?),
        None => None,
    };

    let args = mt_pty::ssh::build_ssh_launcher_args(
        &conn.host,
        conn.port,
        &conn.user,
        identity.as_deref(),
        remote_path,
    );

    // 已存密码是 `mt-secret` 信封,这里解开交给 autofill;解不开只记日志、不填 ——
    // 终端里会照常出现密码提示,用户手输即可,不该因此连 pane 都开不了(私钥登录的
    // 连接更与它无关)。日志不含密码。
    //
    // 直连 `mt_secret::reveal_global`:mt-app 的 `secrets::reveal_password` 就是它加
    // 一层 `to_string`,凭据库是 `ConfigStore::load` 登记的进程级单例,两边同一把钥匙。
    let password = match conn.password.as_deref().filter(|p| !p.is_empty()) {
        Some(stored) => match mt_secret::reveal_global(stored) {
            Ok(plain) => Some(plain),
            Err(err) => {
                tracing::warn!(
                    "[remote-ssh] 连接 {} 的已存密码无法解密,本次不自动填充: {err}",
                    conn.id
                );
                None
            }
        },
        None => None,
    };

    Ok(RemoteLaunch {
        program: ssh_program.to_string_lossy().into_owned(),
        args,
        password,
    })
}

// ---------------------------------------------------------------------------
// tests(全部自 `src-tauri/src/remote_ssh.rs` 的同名测试原样搬来,不触网)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
