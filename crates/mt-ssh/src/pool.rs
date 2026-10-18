//! SSH 持久会话池(mini-term 共享,原 `mt-sidecars/src/pool.rs`)。
//!
//! task 07-05-ssh-remote-projects PR1 把本模块从 `mt-sidecars` 抽到共享 crate
//! `mt-ssh`:mt-ssh-mcp sidecar 与主程序各持一个池实例,行为不变。
//! (stderr 日志统一用 `[mt-ssh]` 前缀 —— PR2 起主程序与 sidecar 共用本 crate,
//! 前缀跟 crate 而非跟调用方走,便于在两边日志里定位到同一层。)
//!
//! 设计摘要:
//! - 库:`russh 0.61`(pure Rust + 原生 tokio async),加密后端 `ring`(避免 Windows
//!   MSVC 上对 aws-lc-sys NASM 的依赖)。
//! - 数据结构:`HashMap<ConnId, Arc<CachedSession>>` 包在 `tokio::sync::Mutex` 内;
//!   每个 `CachedSession` 自己再裹一层 `Mutex<russh::client::Handle>` 把同 session
//!   的 channel 操作串行化(YAGNI 多 channel 并发)。
//! - 默认 profile:idle 10min / lifetime 2h / keepalive 30s × 3 / cap 8 LRU /
//!   lazy 重连 + 单次 retry + 30s 认证失败冷却 + 20s 建连超时。来源见 research 文件。
//! - 建连对齐终端里的系统 ssh(`ssh [-p port] [-i key] user@host`):读 `~/.ssh/config`
//!   (HostName / Port / User / IdentityFile / IdentitiesOnly / HostKeyAlias /
//!   ProxyJump,见 `ssh_config.rs`);认证依次试显式私钥 / ssh-agent / 默认私钥 /
//!   密码(见 `auth.rs`);DNS + TCP + 握手 + 认证整段受 `connect_timeout` 约束,
//!   超时报错写明卡在哪个阶段(见 `transport.rs`)。
//! - host-key 策略:`accept-new` 语义(对齐 OpenSSH `StrictHostKeyChecking=accept-new`)。
//!   首见接受并以 plaintext 追加到 `~/.ssh/known_hosts`,变更 / 吊销拒绝;plaintext、
//!   通配与 `|1|` 哈希条目都认(见 `known_hosts.rs`)。拒绝原因会拼进连接错误。
//! - 认证失败冷却:同一连接身份认证失败后 `gatetime_cooldown` 内直接返错、不再去打
//!   远端,避免 agent 反复重试把本机 IP 送进 fail2ban(之后连终端也连不上)。
//!
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mt_core::SshConnection;
use russh::client::{self, Handle};
use russh::ChannelMsg;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

pub use crate::known_hosts::MtClient;

use crate::auth::{self, AgentSession, AuthError, AuthPlan, ClientNotes, PasswordInput};
use crate::known_hosts;
use crate::ssh_config::{self, ResolvedHost, ResolvedRoute, RouteEnv};
use crate::transport::{self, ConnectStage, GuardedStream, KillOnDrop, StageTracker, StreamKill};

/// 会话池可调参数。所有默认值都来自 research/session-pool-patterns.md TL;DR 表。
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// 空闲淘汰:session 距上次 `ssh_exec` 超过此时长即被 reaper 关掉。
    pub idle_timeout: Duration,
    /// 最长生命周期:无论是否活跃,达到此时长就强制回收(防 NAT 静默丢链)。
    pub max_lifetime: Duration,
    /// keepalive 间隔(协议层 SSH_MSG_GLOBAL_REQUEST `keepalive@openssh.com`)。
    pub keepalive_interval: Duration,
    /// 连续多少次 keepalive 无应答判定 session 已死。
    pub keepalive_max: usize,
    /// 池上限。到上限时按 `last_used` 最小者 LRU 淘汰。
    pub max_sessions: usize,
    /// 认证失败冷却:某连接身份认证失败后,在此时长内 `acquire` 直接返回错误,不再去打
    /// 远端(autossh `AUTOSSH_GATETIME=30s` 风格),免得反复重试触发 fail2ban。网络错误与
    /// 超时不计;连接配置(或 ssh config 解析结果)一改,身份就变了,可立即重试。
    /// 为 0 时不冷却。sidecar 另用它配合 [`CachedSession::mark_unhealthy`]。
    pub gatetime_cooldown: Duration,
    /// 后台 reaper 扫描频率。默认 60s。
    pub reaper_tick: Duration,
    /// shutdown 时单 session disconnect 的上限,防止远端 hang 阻塞 sidecar 退出。
    pub shutdown_per_session_timeout: Duration,
    /// 建连总预算:DNS 解析 + TCP 连接 + SSH 握手(含主机密钥校验)+ 认证整段,
    /// ProxyJump 时含每一跳。超时报错写明卡在哪个阶段。
    ///
    /// 默认 20s:系统 ssh 没有默认 ConnectTimeout(Windows TCP 栈自身约 21s 放弃);
    /// 正常链路握手 + 认证一两秒内完成,PAM 每次密码失败约 2s 延迟、硬件 key 等人触摸
    /// 也都在这个量级之内,再长对调用方只是干等。主程序远程项目外层另有 30s 兜底
    /// (`mt-remote` 的 `ACQUIRE_TIMEOUT`),池先到点才能给出分阶段的原因。
    ///
    /// 宜小于 `keepalive_interval`:russh 认证前的 keepalive 计时器到点后不会重新上弦、
    /// 会空转(见 `transport.rs` 模块注释),建连在它到点前结束就碰不上。
    pub connect_timeout: Duration,
    /// 一个主机名解析出多个地址(如 IPv6 + IPv4)时,除最后一个外每个地址的 TCP 连接上限,
    /// 免得一个黑洞地址吃光整段预算;最后一个地址可用掉剩余的全部预算。默认 5s:够
    /// Windows 丢一次 SYN 后的首次重传(约 3s)。
    pub connect_timeout_per_address: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(10 * 60),
            max_lifetime: Duration::from_secs(2 * 60 * 60),
            keepalive_interval: Duration::from_secs(30),
            keepalive_max: 3,
            max_sessions: 8,
            gatetime_cooldown: Duration::from_secs(30),
            reaper_tick: Duration::from_secs(60),
            shutdown_per_session_timeout: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(20),
            connect_timeout_per_address: Duration::from_secs(5),
        }
    }
}

/// 池建连依赖的本机环境。生产用 [`SshClientEnv::system`];测试从
/// [`SshClientEnv::isolated`] 起步再按需打开,**绝不碰**真实的 `~/.ssh` 与 ssh-agent。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshClientEnv {
    /// home 目录:`~` 展开、默认私钥(`~/.ssh/id_*`)与 ssh config `Include` 相对路径的
    /// 基准。`None` 时不展开 `~`、不试默认私钥。
    pub home_dir: Option<PathBuf>,
    /// OpenSSH 客户端配置文件(`~/.ssh/config`)。`None` = 不读任何 ssh config。
    pub ssh_config_path: Option<PathBuf>,
    /// known_hosts 文件(读、并按 accept-new 追加)。
    pub known_hosts_path: PathBuf,
    /// ssh-agent 来源。
    pub agent: AgentSource,
    /// 本机用户名:ssh config token `%u`,以及连接与 ssh config 都没给用户名时的
    /// 默认登录名(OpenSSH 同样如此)。
    pub local_user: Option<String>,
}

impl SshClientEnv {
    /// 真实环境:`~/.ssh/config`、`~/.ssh/known_hosts`、默认私钥、系统 ssh-agent。
    /// 只取路径与环境变量,不读任何文件(构造池不该有 IO)。
    ///
    /// 找不到 home 时 known_hosts 回退到当前目录下 `.known_hosts`(极端环境的兜底,
    /// 正常桌面端不会触发),也不读 ssh config。
    pub fn system() -> Self {
        let home_dir = dirs::home_dir();
        let ssh_dir = home_dir.as_ref().map(|home| home.join(".ssh"));
        Self {
            known_hosts_path: ssh_dir
                .as_ref()
                .map(|dir| dir.join("known_hosts"))
                .unwrap_or_else(|| PathBuf::from(".known_hosts")),
            ssh_config_path: ssh_dir.map(|dir| dir.join("config")),
            home_dir,
            agent: AgentSource::System,
            local_user: local_user_name(),
        }
    }

    /// 隔离环境:只用给定的 known_hosts;不读 ssh config、不展开 `~`、不试默认私钥、
    /// 不连 ssh-agent —— 认证只剩连接配置里的私钥与密码(本 crate 早先的全部能力)。
    pub fn isolated(known_hosts_path: PathBuf) -> Self {
        Self {
            home_dir: None,
            ssh_config_path: None,
            known_hosts_path,
            agent: AgentSource::Disabled,
            local_user: None,
        }
    }
}

fn local_user_name() -> Option<String> {
    let primary = if cfg!(windows) { "USERNAME" } else { "USER" };
    [primary, "LOGNAME"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.trim().is_empty())
}

/// ssh-agent 来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSource {
    /// 不使用 ssh-agent。
    Disabled,
    /// 系统默认:Windows 先连 OpenSSH 的 agent(`SSH_AUTH_SOCK`,未设置时为
    /// `\\.\pipe\openssh-ssh-agent`),连不上再试 Pageant;Unix 用 `SSH_AUTH_SOCK`。
    System,
    /// 指定端点:Windows 为命名管道路径,Unix 为 socket 路径(主要给测试)。
    Endpoint(PathBuf),
}

/// [`SshPool::try_acquire`] 失败的分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AcquireErrorKind {
    /// 连接 / ssh config 的配置问题(ProxyCommand 不支持、取值非法……),重试无用。
    Config,
    /// DNS、TCP、跳板隧道或 SSH 握手失败(含算法协商失败)。
    Network,
    /// `connect_timeout` 内没建好连接;消息写明卡在哪个阶段。
    Timeout,
    /// 主机密钥校验失败(不符 / 已吊销 / known_hosts 读写失败)。
    HostKey,
    /// 服务器拒绝了全部认证方法(或根本没有可用的凭据)。随后进入认证失败冷却。
    Auth,
    /// 处于认证失败冷却期,没有去连服务器。
    Cooldown,
}

/// [`SshPool::try_acquire`] 的错误。`message` 一律英文开头(agent 在 GBK 代码页的控制台
/// 里读也认得出类别),绝不含密码或私钥内容;主机密钥拒绝原因后面附中文细节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireError {
    pub kind: AcquireErrorKind,
    pub message: String,
}

impl AcquireError {
    fn new(kind: AcquireErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AcquireError {}

/// 池内部状态。`SshPool` 用 `Mutex` 把它包起来,保证 acquire/evict 串行。
struct PoolInner {
    /// `connection.id` → 缓存的 session。
    sessions: HashMap<String, Arc<CachedSession>>,
}

/// 只有 map 里仍是 caller 看到的那个 `Arc` 时才移除。
///
/// 抽成泛型纯函数，让 acquire / evict 共用同一份 TOCTOU 规则，
/// 也能用不含真 SSH handle 的 `Arc` fixture 做单测。
fn remove_arc_if_same<T>(
    entries: &mut HashMap<String, Arc<T>>,
    id: &str,
    expected: &Arc<T>,
) -> Option<Arc<T>> {
    if entries
        .get(id)
        .is_some_and(|current| Arc::ptr_eq(current, expected))
    {
        entries.remove(id)
    } else {
        None
    }
}

/// 池里一项:russh handle + 时间戳 + 连接快照。
pub struct CachedSession {
    /// 建立该 session 时真正参与认证/寻址的连接身份。池的 map key 仍是稳定的
    /// connection id，但复用前必须逐字段核对这一份，避免用户原地修改 host、
    /// port、user、凭据或 ssh config 后短暂命中旧服务器 session。
    connection_identity: CachedConnectionIdentity,
    /// 串行化同 session 上的 channel 操作。russh Handle 自身 Clone 廉价,但允许
    /// 并发开 channel 会让审计日志顺序与"标记 unhealthy"的语义复杂化。
    handle: Mutex<Handle<MtClient>>,
    /// 目标会话底层传输的掐断开关,随本结构 drop 即掐断:russh 会话任务与 socket
    /// (或跳板上的 direct-tcpip 通道)不会比本结构活得久,见 `transport.rs`。
    transport: KillOnDrop,
    /// ProxyJump 经过的跳板会话,第一跳在前。目标会话的传输跑在最后一跳的
    /// direct-tcpip 通道里,所以它们与目标会话同生共死:关闭 / 淘汰时先断目标、再由近
    /// 及远断跳板([`CachedSession::close`]),drop 时一并掐断。
    jumps: Vec<JumpLink>,
    /// session 建立时刻;用于 `max_lifetime` 判定。
    opened_at: Instant,
    /// 最近一次使用(`ssh_exec` 触发)的 UNIX 毫秒。Atomic 是为了 reaper 不抢锁就能读。
    last_used: AtomicU64,
    /// auth 连失败后的冷却截止 UNIX 毫秒,0 表示无 cooldown。
    unhealthy_until: AtomicU64,
    /// 活跃 SFTP 句柄数。reaper/LRU 不得断开仍在跑长操作的 session。
    active_sftp_leases: AtomicUsize,
}

/// 一跳跳板会话。
struct JumpLink {
    handle: Mutex<Handle<MtClient>>,
    transport: KillOnDrop,
}

/// 连接身份:连接配置里参与寻址 / 认证的字段,加上 ssh config 解析出的实际路线。
#[derive(Clone, PartialEq, Eq)]
struct CachedConnectionIdentity {
    id: String,
    host: String,
    port: u16,
    user: String,
    password: Option<String>,
    identity_file: Option<String>,
    /// ssh config 解析出的实际路线(HostName / Port / User / 私钥 / 跳板 /
    /// known_hosts 名)。`~/.ssh/config` 改了,连接配置一字未动也不能复用到旧目标上。
    route: ResolvedRoute,
}

impl CachedConnectionIdentity {
    fn new(connection: &SshConnection, route: ResolvedRoute) -> Self {
        Self {
            id: connection.id.clone(),
            host: connection.host.clone(),
            port: connection.port,
            user: connection.user.clone(),
            password: connection.password.clone(),
            identity_file: connection.identity_file.clone(),
            route,
        }
    }
}

/// 发 DISCONNECT 并等 russh 会话任务把它写出去、退出(任务退出前才关闭消息通道,
/// `is_closed` 由此变真)。调用方负责限时。
async fn disconnect_handle(handle: &Mutex<Handle<MtClient>>) {
    let handle = handle.lock().await;
    let _ = handle
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await;
    while !handle.is_closed() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

impl CachedSession {
    fn matches_identity(&self, identity: &CachedConnectionIdentity) -> bool {
        self.connection_identity == *identity
    }

    /// 优雅断开:先目标、再由近及远的跳板,整体限时 `timeout`;到点无论成败都掐断
    /// 全部传输,保证 russh 会话任务与 socket 一定释放。返回优雅断开是否按时完成。
    async fn close(&self, timeout: Duration) -> bool {
        let graceful = tokio::time::timeout(timeout, async {
            disconnect_handle(&self.handle).await;
            for jump in self.jumps.iter().rev() {
                disconnect_handle(&jump.handle).await;
            }
        })
        .await
        .is_ok();
        self.transport.kill();
        for jump in &self.jumps {
            jump.transport.kill();
        }
        graceful
    }

    /// 现在是否处于 gatetime cooldown 内。
    pub fn is_unhealthy_now(&self) -> bool {
        let until = self.unhealthy_until.load(Ordering::Relaxed);
        until != 0 && now_millis() < until
    }

    /// 拿底层 russh handle 用一次。返回的 guard 在 drop 时释放锁。
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, Handle<MtClient>> {
        self.handle.lock().await
    }

    /// 标记本会话因 auth fail 进入冷却,持续 `cooldown`。
    pub fn mark_unhealthy(&self, cooldown: Duration) {
        let until = now_millis() + cooldown.as_millis() as u64;
        self.unhealthy_until.store(until, Ordering::Relaxed);
    }

    /// 更新 last_used 为 now。
    pub fn touch(&self) {
        self.last_used.store(now_millis(), Ordering::Relaxed);
    }

    pub fn acquire_sftp_lease(&self) {
        self.active_sftp_leases.fetch_add(1, Ordering::AcqRel);
        self.touch();
    }

    pub fn release_sftp_lease(&self) {
        self.active_sftp_leases.fetch_sub(1, Ordering::AcqRel);
        self.touch();
    }

    fn has_active_sftp_lease(&self) -> bool {
        self.active_sftp_leases.load(Ordering::Acquire) != 0
    }

    /// 取 last_used 的 UNIX 毫秒(reaper 用)。
    pub fn last_used_millis(&self) -> u64 {
        self.last_used.load(Ordering::Relaxed)
    }

    /// 取 opened_at 与某基准 Instant 的差值(reaper 用)。
    /// 我们提供 `now: Instant` 入参便于测试与避免单次 tick 内多次取 `Instant::now()`。
    pub fn age_from(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.opened_at)
    }
}

/// 当前 UNIX 毫秒(`SystemTime::now()` 单调性不保证,但 last_used 容忍轻微回拨)。
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 一次认证失败的记录(冷却用)。
struct AuthFailure {
    /// 失败时的连接身份。身份一变(改了连接配置或 ssh config)记录即不再适用。
    identity: CachedConnectionIdentity,
    failed_at: Instant,
    until: Instant,
    /// 当时的认证失败报错(不含凭据),冷却期内原样附在错误后面。
    message: String,
}

/// 冷却判定(纯函数便于单测):同一连接身份、仍在冷却期内时返回剩余时长。
fn cooldown_remaining(
    record: &AuthFailure,
    identity: &CachedConnectionIdentity,
    now: Instant,
) -> Option<Duration> {
    if record.identity != *identity {
        return None;
    }
    let remaining = record.until.saturating_duration_since(now);
    (!remaining.is_zero()).then_some(remaining)
}

/// 冷却期内的报错。英文开头,写明剩余秒数与「这是为了避免触发封禁」。
fn cooldown_message(record: &AuthFailure, remaining: Duration, now: Instant) -> String {
    let ago = now.saturating_duration_since(record.failed_at).as_secs();
    let remaining_secs = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
    format!(
        "authentication cooldown: the last attempt for this connection failed authentication \
         {ago}s ago; not retrying for another {remaining_secs}s to avoid triggering server-side \
         bans (fail2ban / sshd MaxAuthTries). Fix the credentials (editing the connection allows \
         an immediate retry) or wait. Last error: {}",
        record.message
    )
}

/// 池的对外 facade。`Arc<SshPool>` 由 `SshMcp` 持有,跨工具调用共享。
pub struct SshPool {
    inner: Arc<Mutex<PoolInner>>,
    config: PoolConfig,
    /// 建连依赖的本机环境(known_hosts / ssh config / home / ssh-agent)。
    env: SshClientEnv,
    /// 认证失败冷却记录:connection id → 最近一次认证失败。
    auth_failures: std::sync::Mutex<HashMap<String, AuthFailure>>,
    /// 后台 reaper task 句柄,持有以便在 Drop 时 abort。
    ///
    /// 用 `std::sync::Mutex` 而非 `tokio::sync::Mutex`:Drop 是同步上下文,不能
    /// `.await` 拿锁;abort 本身又是同步 API。reaper task 内部不再用这个字段,
    /// 它只会被 Drop / 显式访问读到。
    reaper: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl SshPool {
    /// 用默认 config 与真实环境([`SshClientEnv::system`])构造。
    pub fn new() -> Self {
        Self::with_config(PoolConfig::default())
    }

    /// 指定 config,真实环境([`SshClientEnv::system`])。
    pub fn with_config(config: PoolConfig) -> Self {
        Self::with_env(config, SshClientEnv::system())
    }

    /// 隔离环境([`SshClientEnv::isolated`]):只用给定的 known_hosts,不读 ssh config、
    /// 不试默认私钥、不连 ssh-agent。主要给单测用。
    pub fn with_paths(config: PoolConfig, known_hosts_path: PathBuf) -> Self {
        Self::with_env(config, SshClientEnv::isolated(known_hosts_path))
    }

    /// 全显式构造。会在 tokio runtime 中 spawn 后台 reaper task。不做任何文件 IO。
    ///
    /// **前置**:必须在 tokio runtime 内调用(直接或间接处于 `#[tokio::main]` /
    /// `#[tokio::test]` 上下文),否则 `tokio::spawn` 会 panic。
    pub fn with_env(config: PoolConfig, env: SshClientEnv) -> Self {
        let inner = Arc::new(Mutex::new(PoolInner {
            sessions: HashMap::new(),
        }));
        // reaper 拿 Weak,避免它持有 Arc 让 pool 永远不被 drop —— 这是关键。
        let reaper_weak = Arc::downgrade(&inner);
        let reaper_handle = spawn_reaper(
            reaper_weak,
            config.reaper_tick,
            config.idle_timeout,
            config.max_lifetime,
            config.shutdown_per_session_timeout,
        );
        Self {
            inner,
            config,
            env,
            auth_failures: std::sync::Mutex::new(HashMap::new()),
            reaper: std::sync::Mutex::new(Some(reaper_handle)),
        }
    }

    fn route_env(&self) -> RouteEnv<'_> {
        RouteEnv {
            ssh_config_path: self.env.ssh_config_path.as_deref(),
            home: self.env.home_dir.as_deref(),
            local_user: self.env.local_user.as_deref(),
        }
    }

    fn auth_failures(&self) -> std::sync::MutexGuard<'_, HashMap<String, AuthFailure>> {
        self.auth_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 冷却期内直接返错;身份已变 / 已过期的旧记录顺手清掉。
    fn check_auth_cooldown(
        &self,
        conn: &SshConnection,
        identity: &CachedConnectionIdentity,
    ) -> Result<(), AcquireError> {
        let mut failures = self.auth_failures();
        let Some(record) = failures.get(&conn.id) else {
            return Ok(());
        };
        let now = Instant::now();
        match cooldown_remaining(record, identity, now) {
            Some(remaining) => Err(AcquireError::new(
                AcquireErrorKind::Cooldown,
                cooldown_message(record, remaining, now),
            )),
            None => {
                failures.remove(&conn.id);
                Ok(())
            }
        }
    }

    fn record_auth_failure(&self, identity: &CachedConnectionIdentity, message: &str) {
        let cooldown = self.config.gatetime_cooldown;
        if cooldown.is_zero() {
            return;
        }
        let now = Instant::now();
        let mut failures = self.auth_failures();
        failures.retain(|_, record| record.until > now);
        failures.insert(
            identity.id.clone(),
            AuthFailure {
                identity: identity.clone(),
                failed_at: now,
                until: now + cooldown,
                message: message.to_string(),
            },
        );
    }

    /// 查池里有几条 session(主要给测试用)。
    pub async fn len(&self) -> usize {
        self.inner.lock().await.sessions.len()
    }

    /// 池里是否一条 session 都没有(配对 `len`,clippy 要求)。
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.sessions.is_empty()
    }

    /// 拿一条可用 session。lazy 建,池满时 LRU 淘汰。错误是 [`AcquireError`] 的文本,
    /// 分类见 [`Self::try_acquire`]。
    ///
    /// 返回 `Arc<CachedSession>`,调用方再 `session.lock().await` 开 channel。
    /// 若返回的 session `is_unhealthy_now() == true`,调用方应立即返错而不去开 channel,
    /// 实现 30s gatetime cooldown 的语义。
    pub async fn acquire(&self, conn: &SshConnection) -> Result<Arc<CachedSession>, String> {
        self.try_acquire(conn).await.map_err(|e| e.message)
    }

    /// 同 [`Self::acquire`],错误带分类([`AcquireErrorKind`])。
    ///
    /// - 每次都按当前 ssh config 解析连接的实际路线;解析失败(ProxyCommand、取值非法……)
    ///   是 `Config`;
    /// - 池里有同 id 且身份(连接配置 + 解析出的路线)一致的存活 session 就复用;
    /// - 否则要新建:该身份仍在认证失败冷却期内时直接返 `Cooldown`,不碰网络;
    /// - 新建整段受 `PoolConfig::connect_timeout` 约束,超时是 `Timeout`(写明阶段);
    ///   服务器拒绝全部认证方法是 `Auth`,并开始冷却;主机密钥不符 / 吊销是 `HostKey`;
    ///   DNS / TCP / 隧道 / 握手失败是 `Network`。
    pub async fn try_acquire(
        &self,
        conn: &SshConnection,
    ) -> Result<Arc<CachedSession>, AcquireError> {
        let route = ssh_config::resolve_route(conn, &self.route_env())
            .map_err(|e| AcquireError::new(AcquireErrorKind::Config, e))?;
        let identity = CachedConnectionIdentity::new(conn, route);
        // **持池锁期间只克隆 Arc,立刻放锁**。下面的 `is_closed()` 要 await 单会话锁,
        // 而传输操作(`run_sftp_upload_on_session` / `run_sftp_download_on_session`)
        // 全程持有单会话锁 —— 若在池锁里等它,任何一条连接传大文件都会把**其它所有
        // 连接**的 acquire 一起堵死(全池 head-of-line 阻塞)。池锁只保护 map 本身。
        loop {
            let cached = {
                let inner = self.inner.lock().await;
                inner.sessions.get(&conn.id).cloned()
            };
            let Some(session) = cached else {
                break;
            };
            if !session.matches_identity(&identity) {
                // `invalidate_connection` 是 fire-and-forget；配置保存后紧接着发起的
                // 请求可能先于异步 evict 到达这里。复用边界本身必须再次核对完整
                // 身份，不能让 map 的纯 id key 把新配置导向旧服务器。
                let victim = {
                    let mut inner = self.inner.lock().await;
                    remove_arc_if_same(&mut inner.sessions, &conn.id, &session)
                };
                if let Some(victim) = victim {
                    retire_removed_session(victim, self.config.shutdown_per_session_timeout);
                }
                continue;
            }
            // 复用前只检查 underlying handle 是否还活着。
            //
            // **不要在这里同时检查 is_unhealthy_now**:cooldown 的意图是「上一次失败
            // 后 30s 内立即返错、不再去打远端」,需要把带 unhealthy 标记的 session
            // 原样返给调用方,由 ssh_exec 那边的 is_unhealthy_now 分支返错。如果
            // 在这里把 unhealthy session 当作 miss 跳过去走重建,acquire 会真的重连
            // 远端、并返回一个 unhealthy_until=0 的新 session,调用方永远看不到
            // cooldown,gatetime 语义彻底失效。
            if !session.handle.lock().await.is_closed() {
                let still_cached = {
                    let inner = self.inner.lock().await;
                    inner
                        .sessions
                        .get(&conn.id)
                        .is_some_and(|current| Arc::ptr_eq(current, &session))
                };
                if still_cached {
                    session.touch();
                    return Ok(session);
                }
                // 检查存活性期间这条 session 已被精确淘汰/替换，不得把
                // 退役对象再交给 caller。
                continue;
            }
            // 已死:重新取池锁把它剔掉(否则 build 失败时死条目会一直占着 max_sessions)。
            // **TOCTOU**:放锁的这段空档里别人可能已经重建过同 id 的 session,
            // 用 `Arc::ptr_eq` 确认 map 里还是当初那一条再删,别误删新的。
            let mut inner = self.inner.lock().await;
            let _ = remove_arc_if_same(&mut inner.sessions, &conn.id, &session);
        }

        // 不在池里、或缓存的 session 已死 —— 重建。认证失败冷却只拦「新建」:
        // 池里有健康的同身份 session 时照常复用。
        self.check_auth_cooldown(conn, &identity)?;
        let cached = match self.build_session(conn, &identity).await {
            Ok(cached) => cached,
            Err(error) => {
                if error.kind == AcquireErrorKind::Auth {
                    self.record_auth_failure(&identity, &error.message);
                }
                return Err(error);
            }
        };
        let candidate = Arc::new(cached);

        // connect/auth 不持池锁，因此同 id 的并发 miss 可能同时建好多条。
        // 插入时重新选 winner：已有存活 session 就复用，只关闭本次未入池的 loser。
        loop {
            let winner = {
                let mut inner = self.inner.lock().await;
                if let Some(winner) = inner.sessions.get(&conn.id).cloned() {
                    winner
                } else {
                    // 池满则 LRU 淘汰一条。只有没有外部强引用/活跃 lease 的
                    // 条目会被 pick，因此移除后可安全后台 disconnect。
                    if inner.sessions.len() >= self.config.max_sessions {
                        if let Some(victim_id) = pick_lru_victim(&inner.sessions) {
                            if let Some(victim) = inner.sessions.remove(&victim_id) {
                                spawn_disconnect(victim, self.config.shutdown_per_session_timeout);
                            }
                        }
                    }
                    inner.sessions.insert(conn.id.clone(), candidate.clone());
                    return Ok(candidate);
                }
            };

            if !winner.matches_identity(&identity) {
                let victim = {
                    let mut inner = self.inner.lock().await;
                    remove_arc_if_same(&mut inner.sessions, &conn.id, &winner)
                };
                if let Some(victim) = victim {
                    retire_removed_session(victim, self.config.shutdown_per_session_timeout);
                }
                continue;
            }

            if !winner.handle.lock().await.is_closed() {
                let still_winner = {
                    let inner = self.inner.lock().await;
                    inner
                        .sessions
                        .get(&conn.id)
                        .is_some_and(|current| Arc::ptr_eq(current, &winner))
                };
                if still_winner {
                    winner.touch();
                    // candidate 从未入池，也没有其他引用；带上限关闭，不泄漏竞争 loser。
                    spawn_disconnect(candidate, self.config.shutdown_per_session_timeout);
                    return Ok(winner);
                }
                continue;
            }

            // winner 已死。移除前再比对 Arc；如果已被第三个任务替换，
            // 下轮检查新 winner，本次 candidate 仍然保留。
            let mut inner = self.inner.lock().await;
            let _ = remove_arc_if_same(&mut inner.sessions, &conn.id, &winner);
        }
    }

    /// 把指定连接对应的 session 从池里踢出去。
    ///
    /// 用途:`ssh_exec` 在拿到 session 后开 channel / exec 失败(transport-level
    /// 死链 race),需要先把这条死 session 移出池再重新 acquire 触发重连。
    /// 没有外部引用/lease 时后台 disconnect；否则只移出缓存，待使用者自然释放。
    pub async fn evict(&self, id: &str) {
        let mut inner = self.inner.lock().await;
        let victim = inner.sessions.remove(id);
        drop(inner);
        if let Some(victim) = victim {
            retire_removed_session(victim, self.config.shutdown_per_session_timeout);
        }
    }

    /// 仅当 `id` 仍指向 caller 遇到错误的那条 session 时才淘汰。
    ///
    /// 返回是否真正从池中移除。这个 API 用于 transport 失败后的精确重连：
    /// 若同 id 已经由其它任务重建，不能把新 winner 误删。移除后如果仍有强引用
    ///（至少包括 `expected`）或活跃 SFTP lease，不会强制 disconnect；它会在最后
    /// 一个使用者释放时自然销毁。
    pub async fn evict_if_same(&self, id: &str, expected: &Arc<CachedSession>) -> bool {
        let mut inner = self.inner.lock().await;
        let victim = remove_arc_if_same(&mut inner.sessions, id, expected);
        drop(inner);
        if let Some(victim) = victim {
            retire_removed_session(victim, self.config.shutdown_per_session_timeout);
            true
        } else {
            false
        }
    }

    /// 关掉所有 session,清空池。sidecar shutdown 时调用。
    ///
    /// 同时 abort 后台 reaper task —— sidecar 主流程返回后 reaper 不再需要继续跑。
    /// 单条 session disconnect 各自加超时,**不让单条挂死阻塞退出**。
    pub async fn shutdown(&self) {
        // 1) abort reaper(Drop 也会兜底,但显式调用更清晰)
        if let Ok(mut guard) = self.reaper.lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }

        // 2) drain 并发 disconnect 所有 session
        let mut inner = self.inner.lock().await;
        let entries: Vec<_> = inner.sessions.drain().map(|(_, v)| v).collect();
        drop(inner);

        // 每条 session(连同它的跳板)各自限时优雅断开,到点即掐断传输。
        let timeout = self.config.shutdown_per_session_timeout;
        let futures = entries.into_iter().map(|s| async move {
            let _ = s.close(timeout).await;
        });
        futures::future::join_all(futures).await;
    }

    /// 真正建一条 session:按路线逐跳建连(第一跳走 TCP,其后经上一跳的 direct-tcpip
    /// 隧道)→ 握手(主机密钥校验在 Handler 内)→ 认证。整段受 `connect_timeout` 约束;
    /// 超时丢弃建连 future 时,各跳的 [`KillOnDrop`] 随之 drop、掐断传输,russh 已 spawn
    /// 的会话任务随即退出,不泄漏任务与 socket。
    async fn build_session(
        &self,
        conn: &SshConnection,
        identity: &CachedConnectionIdentity,
    ) -> Result<CachedSession, AcquireError> {
        let route = &identity.route;
        let target = format!("{}@{}", route.target.user, route.target.endpoint());
        let budget = self.config.connect_timeout;
        let deadline = tokio::time::Instant::now() + budget;
        let tracker = StageTracker::default();
        let connecting = self.connect_route(conn, route, deadline, &tracker, &target);
        let (mut links, target_link) = match tokio::time::timeout_at(deadline, connecting).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(AcquireError::new(
                    AcquireErrorKind::Timeout,
                    tracker.describe_timeout(&target, budget),
                ));
            }
        };
        let jumps = links
            .drain(..)
            .map(|link| JumpLink {
                handle: Mutex::new(link.handle),
                transport: link.transport,
            })
            .collect();
        Ok(CachedSession {
            connection_identity: identity.clone(),
            handle: Mutex::new(target_link.handle),
            transport: target_link.transport,
            jumps,
            opened_at: Instant::now(),
            last_used: AtomicU64::new(now_millis()),
            unhealthy_until: AtomicU64::new(0),
            active_sftp_leases: AtomicUsize::new(0),
        })
    }

    /// 逐跳建连,返回 (跳板们, 目标)。任何一步失败,已建好的各跳随返回值 drop 而掐断。
    async fn connect_route(
        &self,
        conn: &SshConnection,
        route: &ResolvedRoute,
        deadline: tokio::time::Instant,
        tracker: &StageTracker,
        target: &str,
    ) -> Result<(Vec<Link>, Link), AcquireError> {
        // 一次建连里各跳共用一个 agent 连接(懒连接,用不到就不连)。
        let mut agent = AgentSession::new(self.env.agent.clone());
        let mut jumps: Vec<Link> = Vec::with_capacity(route.jumps.len());
        let hops = route
            .jumps
            .iter()
            .map(|hop| (hop, true))
            .chain(std::iter::once((&route.target, false)));
        for (hop, is_jump) in hops {
            let label = if is_jump {
                format!("jump host {}", hop.endpoint())
            } else {
                hop.endpoint()
            };
            let kill = StreamKill::new();
            // 先装好「drop 即掐断」的守卫再把流交给 russh:之后任何失败或超时丢弃本
            // future,它都会随之 drop。
            let transport = KillOnDrop::new(kill.clone());
            let notes = Arc::new(ClientNotes::default());
            let mut handle = match jumps.last() {
                None => {
                    let tcp = transport::connect_tcp(
                        &hop.host_name,
                        hop.port,
                        &label,
                        deadline,
                        self.config.connect_timeout_per_address,
                        tracker,
                    )
                    .await
                    .map_err(|e| match e {
                        transport::TcpConnectError::Resolve(message)
                        | transport::TcpConnectError::Failed(message) => AcquireError::new(
                            AcquireErrorKind::Network,
                            format!("ssh connect to {label} failed: {message}"),
                        ),
                        transport::TcpConnectError::DeadlineExceeded => AcquireError::new(
                            AcquireErrorKind::Timeout,
                            tracker.describe_timeout(target, self.config.connect_timeout),
                        ),
                    })?;
                    // 不设 TCP_NODELAY:与此前 `client::connect`(russh 默认 `nodelay =
                    // false`)的行为保持一致。
                    tracker.enter(&label, ConnectStage::Handshake);
                    tracker.watch_stream(&kill);
                    self.handshake(kill.wrap(tcp), hop, &label, &notes).await?
                }
                Some(previous) => {
                    tracker.enter(&label, ConnectStage::Tunnel);
                    let channel = previous
                        .handle
                        .channel_open_direct_tcpip(
                            hop.host_name.clone(),
                            u32::from(hop.port),
                            "127.0.0.1",
                            0,
                        )
                        .await
                        .map_err(|e| {
                            AcquireError::new(
                                AcquireErrorKind::Network,
                                format!(
                                    "ssh connect to {label} failed: the previous jump host \
                                     refused to open a tunnel to it ({e})"
                                ),
                            )
                        })?;
                    tracker.enter(&label, ConnectStage::Handshake);
                    tracker.watch_stream(&kill);
                    self.handshake(kill.wrap(channel.into_stream()), hop, &label, &notes)
                        .await?
                }
            };
            tracker.enter(&label, ConnectStage::Auth);
            let auth_label = if is_jump {
                format!("jump host {}@{}", hop.user, hop.endpoint())
            } else {
                format!("{}@{}", hop.user, hop.endpoint())
            };
            // 密码属于目标连接,跳板不用。
            let stored_password = if is_jump {
                None
            } else {
                conn.password.as_deref()
            };
            authenticate(
                &mut handle,
                hop,
                auth_label,
                stored_password,
                &self.env,
                &mut agent,
                &notes,
                tracker,
            )
            .await?;
            let link = Link { handle, transport };
            if !is_jump {
                return Ok((jumps, link));
            }
            jumps.push(link);
        }
        Err(AcquireError::new(
            AcquireErrorKind::Config,
            "ssh route has no target host",
        ))
    }

    /// 在已建立的传输上做 SSH 握手(标识串 + 密钥交换 + 主机密钥校验)。
    async fn handshake<S>(
        &self,
        stream: GuardedStream<S>,
        hop: &ResolvedHost,
        label: &str,
        notes: &Arc<ClientNotes>,
    ) -> Result<Handle<MtClient>, AcquireError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        // 首选 host key 算法按 known_hosts 调序(见 `preferred_host_key_algorithms`)。
        // 这里读失败就沿用默认顺序 —— `check_server_key` 会再读一次,失败即拒绝并说明原因。
        let host_keys = known_hosts::read_known_hosts(&self.env.known_hosts_path)
            .ok()
            .and_then(|raw| {
                known_hosts::preferred_host_key_algorithms(&known_hosts::known_host_key_types(
                    &raw,
                    &hop.known_hosts_name,
                ))
            });
        // 不设 `inactivity_timeout`(russh 默认 None):
        // - 认证前它兜不住 —— keepalive 计时器在认证前到点后空转、每轮都把 inactivity
        //   计时器重置(见 transport 模块注释)。建连期的收尾改由 connect_timeout +
        //   传输掐断保证,有测试证明服务器一侧会立刻看到连接关闭;
        // - 认证后,服务器不回 keepalive 时 `keepalive_max` 次后 russh 已会断开;服务器
        //   正常回 keepalive 时,每个回包都会重置 inactivity 计时,设了也不会误杀长时间
        //   没有输出的会话 —— 但也没有任何增益。
        // 用 `..Default::default()` 一次性赋值,避开 clippy::field_reassign_with_default。
        let cfg = Arc::new(client::Config {
            keepalive_interval: Some(self.config.keepalive_interval),
            keepalive_max: self.config.keepalive_max,
            preferred: transport::preferred_algorithms(host_keys),
            ..Default::default()
        });
        let handler = MtClient::new(
            hop.known_hosts_name.clone(),
            self.env.known_hosts_path.clone(),
            Arc::clone(notes),
        );
        client::connect_stream(cfg, stream, handler)
            .await
            .map_err(|e| match notes.take_rejection() {
                Some(reason) => AcquireError::new(
                    AcquireErrorKind::HostKey,
                    known_hosts::connect_error(label, &e, Some(reason)),
                ),
                None => AcquireError::new(
                    AcquireErrorKind::Network,
                    known_hosts::connect_error(label, &e, None),
                ),
            })
    }
}

/// 建连途中的一跳:会话 handle + 它底层传输的掐断开关。
struct Link {
    handle: Handle<MtClient>,
    transport: KillOnDrop,
}

impl Default for SshPool {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SshPool {
    /// Drop 时兜底 abort reaper。`shutdown()` 显式调用过则这里是 no-op。
    ///
    /// Drop 不能 `.await`,所以这里**不能** disconnect 后端 session ——
    /// 那部分必须靠 `shutdown()` 显式驱动。SIGKILL / 异常退出场景下,远端
    /// 会通过 TCP RST 感知本地断开,与此前 spawn-ssh 路径行为一致。
    fn drop(&mut self) {
        if let Ok(mut guard) = self.reaper.lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
    }
}

/// 启动后台 reaper task,返回 JoinHandle 以便 Drop / shutdown 时 abort。
///
/// **生命周期关键**:reaper 持有 `Weak<Mutex<PoolInner>>`,每次 tick 内
/// `Weak::upgrade()` 拿 Arc。pool 被 drop 后 strong count 归零,upgrade 返回
/// None,reaper 退出循环、task 结束。**这一点保证 reaper 不会因为持有 Arc
/// 让 pool 永远不被 drop**。
fn spawn_reaper(
    inner: Weak<Mutex<PoolInner>>,
    tick: Duration,
    idle_timeout: Duration,
    max_lifetime: Duration,
    per_session_shutdown_timeout: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(tick);
        // 第一次 tick 立刻返回 —— 跳过,我们想等 `tick` 再开始扫,避免构造瞬间
        // 就把刚 acquire 完的 session 误判为 idle(opened_at 与 last_used 距 now
        // 都是 0ms,不会过 threshold,但 reaper 也无事可做)。
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let Some(arc) = inner.upgrade() else {
                // pool 已被 drop,退出 reaper task。
                return;
            };
            // 锁内只做"读快照 + 决定踢谁 + 从 map 移除",真正的 disconnect 异步外抛。
            let to_evict: Vec<Arc<CachedSession>> = {
                let mut guard = arc.lock().await;
                let now_instant = Instant::now();
                let now_ms = now_millis();
                let triples: Vec<(String, u64, Duration)> = guard
                    .sessions
                    .iter()
                    .filter(|(_, session)| {
                        !session.has_active_sftp_lease() && Arc::strong_count(session) == 1
                    })
                    .map(|(id, s)| (id.clone(), s.last_used_millis(), s.age_from(now_instant)))
                    .collect();
                let victims = select_expired(&triples, now_ms, idle_timeout, max_lifetime);
                let mut evicted = Vec::with_capacity(victims.len());
                for id in victims {
                    if let Some(v) = guard.sessions.remove(&id) {
                        evicted.push(v);
                    }
                }
                evicted
            };
            for v in to_evict {
                spawn_disconnect(v, per_session_shutdown_timeout);
            }
        }
    })
}

/// 从 (id, last_used_millis, opened_age) 三元组里挑出已过期(idle 或 lifetime)
/// 的 id。**抽成纯函数便于单测**——绕开"造真 CachedSession"的不可能任务。
///
/// - `triples`:池子里每条 session 的 `(id, last_used UNIX 毫秒, opened_at 距 now 的 age)`。
/// - `now_ms`:当前 UNIX 毫秒,用于判断 idle。
/// - `idle_timeout`:`now_ms - last_used >= idle_timeout` 即过期。
/// - `max_lifetime`:`age >= max_lifetime` 即过期(防 NAT 静默丢链)。
fn select_expired(
    triples: &[(String, u64, Duration)],
    now_ms: u64,
    idle_timeout: Duration,
    max_lifetime: Duration,
) -> Vec<String> {
    let idle_ms = idle_timeout.as_millis() as u64;
    triples
        .iter()
        .filter(|(_, last_used, age)| {
            let idle_too_long = now_ms.saturating_sub(*last_used) >= idle_ms;
            let lived_too_long = *age >= max_lifetime;
            idle_too_long || lived_too_long
        })
        .map(|(id, _, _)| id.clone())
        .collect()
}

/// 按 `last_used` 最小者挑一条 victim。
///
/// 抽成纯函数便于单测;入参拿不可变 ref 不破坏外部 lock 状态。
fn pick_lru_victim(sessions: &HashMap<String, Arc<CachedSession>>) -> Option<String> {
    sessions
        .iter()
        .filter(|(_, session)| !session.has_active_sftp_lease() && Arc::strong_count(session) == 1)
        .min_by_key(|(_, s)| s.last_used.load(Ordering::Relaxed))
        .map(|(id, _)| id.clone())
}

/// 从池中移除后，只有本地退役逻辑持有唯一强引用且没有活跃
/// SFTP lease 时，才能主动 disconnect。否则仅丢掉池引用，让在途使用者
/// 自然释放，避免淘汰一条 session 却中断其它 channel。
fn should_disconnect_removed_session(active_sftp_lease: bool, strong_count: usize) -> bool {
    !active_sftp_lease && strong_count == 1
}

fn retire_removed_session(session: Arc<CachedSession>, timeout: Duration) {
    if should_disconnect_removed_session(
        session.has_active_sftp_lease(),
        Arc::strong_count(&session),
    ) {
        spawn_disconnect(session, timeout);
    }
}

/// 后台异步 disconnect 一条 session(连同它的跳板),带超时;到点即掐断传输,
/// 失败静默(stderr 一行)。
fn spawn_disconnect(s: Arc<CachedSession>, timeout: Duration) {
    tokio::spawn(async move {
        if !s.close(timeout).await {
            eprintln!("[mt-ssh] session disconnect timed out, dropping");
        }
    });
}

// ============================================================================
// 有界 SSH exec 原语
// ============================================================================

/// 一次 exec 在 SSH channel 协议中达到的最终状态。
///
/// `russh::Channel::exec()` 只把请求送入本地 session 事件队列；服务器是否
/// 接受由后续 [`ChannelMsg::Success`] / [`ChannelMsg::Failure`] 表示。因此超时不能
/// 用一个 bool 混成“没启动”：调用方可以用此状态决定是否能安全 fallback。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum BoundedExecState {
    /// 还没有发起 channel open（例如等 session 锁超时）。
    #[default]
    NotDispatched,
    /// channel-open 请求已经可能入队，但 future 在返回 channel 前超时。
    /// 本函数没有发 exec，但旧 session 存在孤儿 channel/传输状态不明。
    ChannelOpenUnknown,
    /// `exec()` 在向本地 session 队列入队时超时。Tokio mpsc send
    /// 具有取消安全保证，此时请求没有入队，不是等服务器确认超时。
    ExecEnqueueTimedOut,
    /// exec 请求已成功入队，但未收到服务器 Success / Failure。
    ExecReplyUnknown,
    /// 服务器明确拒绝 exec，且没有观察到任何执行证据。
    Rejected,
    /// 服务器已确认，或已收到输出/退出状态等执行证据。
    Started,
}

impl BoundedExecState {
    /// 是否可以立即对同一目标启动业务 fallback。
    pub fn safe_to_fallback(self) -> bool {
        matches!(
            self,
            Self::NotDispatched | Self::ExecEnqueueTimedOut | Self::Rejected
        )
    }

    /// 远端命令是否可能已经启动。注意 `ChannelOpenUnknown` 为 false，
    /// 但该状态的旧 session 仍应先精确淘汰，再考虑新操作。
    pub fn may_have_started(self) -> bool {
        matches!(self, Self::ExecReplyUnknown | Self::Started)
    }
}

/// 一次 SSH exec 的有界结果。stdout / stderr 各自最多保留调用方指定的字节数；
/// 超出的数据仍会从 channel 排空，避免远端因发送窗口耗尽而卡住，只是不再留在内存中。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoundedExecOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<u32>,
    pub state: BoundedExecState,
    /// 向后兼容的保守 guard。只有 [`BoundedExecOutput::safe_to_fallback`]
    /// 为 true 时才是 false；因此 `ChannelOpenUnknown` 也会返回 true，
    /// 避免旧 caller 把“session 状态不明”误当成“安全 fallback”。
    /// 新 caller 应匹配 `state`，不要从该字段反推精确协议阶段。
    pub command_started: bool,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// channel close 在独立 grace 窗口内未能确认入队。命令本身的
    /// `state` 仍然保留，但 caller 应先精确淘汰该 session 再执行后续操作。
    pub channel_cleanup_uncertain: bool,
}

impl BoundedExecOutput {
    /// 结合命令派发状态与 channel 清理结果判断能否立即 fallback。
    pub fn safe_to_fallback(&self) -> bool {
        self.state.safe_to_fallback() && !self.channel_cleanup_uncertain
    }

    /// 是否应使用 `evict_if_same` 退役本次操作所在的具体 session。
    pub fn requires_session_retirement(&self) -> bool {
        self.state == BoundedExecState::ChannelOpenUnknown || self.channel_cleanup_uncertain
    }
}

fn append_bounded_output(output: &mut Vec<u8>, chunk: &[u8], cap: usize) -> bool {
    let available = cap.saturating_sub(output.len());
    let retained = available.min(chunk.len());
    output.extend_from_slice(&chunk[..retained]);
    retained < chunk.len()
}

fn exec_output(state: BoundedExecState, timed_out: bool) -> BoundedExecOutput {
    BoundedExecOutput {
        state,
        command_started: !state.safe_to_fallback(),
        timed_out,
        ..BoundedExecOutput::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecStateEvent {
    Accepted,
    Rejected,
    ExecutionEvidence,
}

fn transition_exec_state(current: BoundedExecState, event: ExecStateEvent) -> BoundedExecState {
    match event {
        ExecStateEvent::Accepted | ExecStateEvent::ExecutionEvidence => BoundedExecState::Started,
        // 一旦已看到执行证据，迟到/异常的 Failure 不能把状态倒退成可重试。
        ExecStateEvent::Rejected if current == BoundedExecState::Started => current,
        ExecStateEvent::Rejected => BoundedExecState::Rejected,
    }
}

fn set_exec_state(output: &mut BoundedExecOutput, state: BoundedExecState) {
    output.state = state;
    output.command_started = !output.safe_to_fallback();
}

fn mark_exec_cleanup_uncertain(output: &mut BoundedExecOutput) {
    output.channel_cleanup_uncertain = true;
    output.command_started = true;
}

const EXEC_CHANNEL_CLOSE_GRACE: Duration = Duration::from_millis(500);

async fn await_exec_cleanup<F, T, E>(future: F) -> bool
where
    F: std::future::Future<Output = Result<T, E>>,
{
    matches!(
        tokio::time::timeout(EXEC_CHANNEL_CLOSE_GRACE, future).await,
        Ok(Ok(_))
    )
}

/// 在已认证的共享 session 上执行一条远端命令，并收集有限输出、退出码和整体超时。
///
/// 该函数只负责单次 channel 协议，不负责连接 acquire / evict / retry。开 channel 或发送
/// exec 请求失败返回 transport 错误；远端非零退出码仍是正常的 [`BoundedExecOutput`]，由
/// 调用方按业务语义处理。超时后会在独立的短 grace 窗口内显式关闭
/// channel，且 `timed_out = true`；整个 API 的硬上限是 `timeout + 500ms`。
///
/// session 锁只覆盖 `channel_open_session`。channel 建立后立即释放，长命令不会阻塞同一
/// SSH 连接上的其它 channel；调用方持有的 `Arc<CachedSession>` 会阻止池 reaper/LRU 在
/// 本操作期间淘汰 session。
pub async fn run_bounded_exec_on_session(
    session: &CachedSession,
    remote_command: &str,
    timeout: Duration,
    output_cap_bytes: usize,
) -> Result<BoundedExecOutput, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    session.touch();

    let handle_guard = match tokio::time::timeout_at(deadline, session.lock()).await {
        Ok(guard) => guard,
        Err(_) => {
            return Ok(exec_output(BoundedExecState::NotDispatched, true));
        }
    };
    let mut channel =
        match tokio::time::timeout_at(deadline, handle_guard.channel_open_session()).await {
            Ok(Ok(channel)) => channel,
            Ok(Err(error)) => return Err(format!("channel_open_session failed: {error}")),
            Err(_) => {
                // `Handle::channel_open_session()` 先向 session 队列发 open，再等服务器
                // confirmation。future 在中间被取消后没有 Channel 可 close，旧 session
                // 可能留有孤儿 channel；不能谎报成安全 fallback。
                session.touch();
                return Ok(exec_output(BoundedExecState::ChannelOpenUnknown, true));
            }
        };
    drop(handle_guard);

    match tokio::time::timeout_at(deadline, channel.exec(true, remote_command)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let cleanup_note = if await_exec_cleanup(channel.close()).await {
                ""
            } else {
                "; channel cleanup uncertain; retire this exact session"
            };
            session.touch();
            return Err(format!("channel exec failed: {error}{cleanup_note}"));
        }
        Err(_) => {
            let cleanup_succeeded = await_exec_cleanup(channel.close()).await;
            session.touch();
            // russh 的 `exec()` 只负责向本地 session 事件队列入队，不等服务器
            // 确认。Tokio 取消安全性保证请求没有入队；这是“本地入队超时”，
            // 而不是“等服务器确认超时”。
            let mut output = exec_output(BoundedExecState::ExecEnqueueTimedOut, true);
            if !cleanup_succeeded {
                mark_exec_cleanup_uncertain(&mut output);
            }
            return Ok(output);
        }
    }

    // 成功入队不等于服务器已启动命令；等 Success / Failure 完成判定。
    let mut output = exec_output(BoundedExecState::ExecReplyUnknown, false);
    const SSH_EXTENDED_DATA_STDERR: u32 = 1;

    loop {
        let message = match tokio::time::timeout_at(deadline, channel.wait()).await {
            Ok(message) => message,
            Err(_) => {
                let cleanup_succeeded = await_exec_cleanup(channel.close()).await;
                output.timed_out = true;
                output.exit_code = None;
                if !cleanup_succeeded {
                    mark_exec_cleanup_uncertain(&mut output);
                }
                session.touch();
                return Ok(output);
            }
        };
        let Some(message) = message else { break };
        match message {
            ChannelMsg::Success => {
                let state = transition_exec_state(output.state, ExecStateEvent::Accepted);
                set_exec_state(&mut output, state);
            }
            ChannelMsg::Failure => {
                let state = transition_exec_state(output.state, ExecStateEvent::Rejected);
                set_exec_state(&mut output, state);
                if state == BoundedExecState::Rejected {
                    // `want_reply = true` 的 exec 被服务器明确拒绝；没有观察到
                    // 执行证据时可以安全 fallback，不必等到整体超时。
                    if !await_exec_cleanup(channel.close()).await {
                        mark_exec_cleanup_uncertain(&mut output);
                    }
                    session.touch();
                    return Ok(output);
                }
            }
            ChannelMsg::Data { data } => {
                let state = transition_exec_state(output.state, ExecStateEvent::ExecutionEvidence);
                set_exec_state(&mut output, state);
                output.stdout_truncated |=
                    append_bounded_output(&mut output.stdout, &data, output_cap_bytes);
            }
            ChannelMsg::ExtendedData { data, ext } => {
                let state = transition_exec_state(output.state, ExecStateEvent::ExecutionEvidence);
                set_exec_state(&mut output, state);
                if ext == SSH_EXTENDED_DATA_STDERR {
                    output.stderr_truncated |=
                        append_bounded_output(&mut output.stderr, &data, output_cap_bytes);
                }
            }
            ChannelMsg::ExitStatus { exit_status } => {
                let state = transition_exec_state(output.state, ExecStateEvent::ExecutionEvidence);
                set_exec_state(&mut output, state);
                output.exit_code = Some(exit_status);
            }
            ChannelMsg::ExitSignal { .. } => {
                let state = transition_exec_state(output.state, ExecStateEvent::ExecutionEvidence);
                set_exec_state(&mut output, state);
            }
            _ => {}
        }
    }

    // `wait()` 返回 None 已证明远端通道收流结束；此处 close 只是幂等的
    // best-effort 收尾，本地 sender 已关闭时的 Err 不代表 session 状态不明。
    let _ = await_exec_cleanup(channel.close()).await;
    session.touch();
    Ok(output)
}

// ============================================================================
// SFTP 文件传输原语 (task 06-09-ssh-mcp-sftp-transfer)
// ============================================================================

/// 流式分块读写的 chunk 大小。8 KB 与 russh-sftp `File` 内部按 max_packet_len
/// 切包的行为相容(每次 read/write 内部会再按服务器协商的上限二次切分),仅用作
/// 上层 copy 缓冲区,内存占用恒定、不随文件大小线性增长。
const SFTP_CHUNK_BYTES: usize = 8 * 1024;

/// 把 russh-sftp 的**协议层每请求超时**(`SftpSession::set_timeout`,默认仅 10s)
/// 放宽到与外层 `tokio::time::timeout` 传输窗口一致。
///
/// 关键:russh-sftp 的 10s 超时是**逐个 SFTP 请求包**(每次 read/write 一个 chunk、
/// 每次 open)各自计时的,不是整段传输。慢链路 / 拥塞下单个 chunk 等待 >10s 就会以
/// 一个晦涩的协议错中断整段传输,而此时外层 300s 窗口远未到。故按外层超时窗口同步放宽,
/// 让真正的「整段超时」由外层 `tokio::time::timeout` 统一裁决(见 prd Technical Notes)。
fn sftp_request_timeout_secs(transfer_timeout: Duration) -> u64 {
    // 至少 1s;并以外层窗口为上限,避免协议层比外层还早超时。
    transfer_timeout.as_secs().max(1)
}

/// SFTP 传输的错误分类:供 caller 决定是否 evict + 重连。
///
/// - `Transport`:开 channel / `request_subsystem` / SFTP 握手失败 —— 可能是死链 race,
///   caller 应 evict 这条 session 再重连重试一次(与现有 exec 的 transport 错一致)。
/// - `Sftp`:SFTP 协议层 / 本地 IO 错(远端路径不存在/无权限/本地文件读写失败),
///   是业务错,**不应 evict** —— session 本身没坏,重连也救不了一个不存在的路径。
#[derive(Debug)]
pub enum SftpTransferError {
    /// transport-level 失败,caller 可 evict + 重连。
    Transport(String),
    /// SFTP 协议层 / 本地文件系统业务错,caller 不应 evict。
    Sftp(String),
}

impl SftpTransferError {
    /// 是否属于 transport-level(caller 据此决定 evict + 重连)。
    pub fn is_transport(&self) -> bool {
        matches!(self, SftpTransferError::Transport(_))
    }

    /// 取人类可读的错误信息。**绝不含密码**(只透传 russh / russh-sftp / IO 的错误文本,
    /// 这些库的错误不携带认证凭据)。
    pub fn message(&self) -> &str {
        match self {
            SftpTransferError::Transport(m) | SftpTransferError::Sftp(m) => m,
        }
    }
}

impl std::fmt::Display for SftpTransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

/// 在已 acquire 到的 session 上开 SFTP channel,把本地文件流式上传到远程路径。
///
/// 全程持有 `session.lock()`(沿用现有 channel 串行化语义),用 `open_with_flags`
/// 拿到流式 `File`,以固定大小缓冲分块 `read`→`write_all`,内存占用恒定。
///
/// 与下载侧同款:先写远端临时文件(`<remote>.mt-sftp-partial`)再改名到目标,
/// **中断绝不会把远端原文件截成半截**;失败时清理临时文件。
///
/// 返回写入的字节数。错误区分 transport(可 evict 重连)与 SFTP 业务错(不 evict)。
pub async fn run_sftp_upload_on_session(
    session: &CachedSession,
    local_path: &str,
    remote_path: &str,
    transfer_timeout: Duration,
) -> Result<u64, SftpTransferError> {
    use russh_sftp::client::SftpSession;
    use russh_sftp::protocol::OpenFlags;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // 本地文件先打开 —— 本地不存在/不可读属业务错,不必去碰远端。
    let mut local = tokio::fs::File::open(local_path).await.map_err(|e| {
        SftpTransferError::Sftp(format!("cannot open local file '{local_path}': {e}"))
    })?;

    let handle_guard = session.lock().await;
    let channel = handle_guard
        .channel_open_session()
        .await
        .map_err(|e| SftpTransferError::Transport(format!("channel_open_session failed: {e}")))?;
    channel.request_subsystem(true, "sftp").await.map_err(|e| {
        SftpTransferError::Transport(format!("request_subsystem(sftp) failed: {e}"))
    })?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| SftpTransferError::Transport(format!("sftp handshake failed: {e}")))?;
    // 放宽协议层每请求超时(默认 10s),避免慢链路下单个 chunk 包就把整段传输打断。
    sftp.set_timeout(sftp_request_timeout_secs(transfer_timeout));

    // 临时文件路径:目标旁边加后缀,与下载侧同一套命名(`sftp_partial_path`)。
    // 传输期间远端目标文件**一个字节都不会被动**,中断只会留下一个临时文件。
    let tmp_path = sftp_partial_path(remote_path);

    let mut remote = sftp
        .open_with_flags(
            tmp_path.as_str(),
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .map_err(|e| SftpTransferError::Sftp(format!("sftp open '{tmp_path}' failed: {e}")))?;

    let mut buf = vec![0u8; SFTP_CHUNK_BYTES];
    let mut total: u64 = 0;
    let copy_result: Result<(), SftpTransferError> = async {
        loop {
            let n = local.read(&mut buf).await.map_err(|e| {
                SftpTransferError::Sftp(format!("read local file '{local_path}' failed: {e}"))
            })?;
            if n == 0 {
                break;
            }
            remote.write_all(&buf[..n]).await.map_err(|e| {
                SftpTransferError::Sftp(format!("sftp write to '{tmp_path}' failed: {e}"))
            })?;
            total += n as u64;
        }
        remote
            .flush()
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp flush '{tmp_path}' failed: {e}")))?;
        remote.shutdown().await.map_err(|e| {
            SftpTransferError::Sftp(format!("sftp close remote '{tmp_path}' failed: {e}"))
        })?;

        // 收尾改名 —— 必须在本 sftp 会话关闭前做。
        //
        // ⚠️ SFTP 的 rename 语义:标准 `SSH_FXP_RENAME` 在目标已存在时会失败
        // (OpenSSH 行为),而 russh-sftp **没有** `posix-rename@openssh.com`
        // 扩展 —— 主工作区的 2.4 与 sidecars 工作区的 2.3 都只有
        // limits/hardlink/fsync/statvfs(+2.4 的 expand-path),两边一致。
        // 故先直接 rename:目标不存在时一次成功、**零空窗**;只有「目标已存在」
        // 才回退到 remove + rename,留一个极小的空窗 —— 仍远好于原先
        // CREATE|TRUNCATE 的「一中断就把远端原文件截成半截」。
        if let Err(first) = sftp.rename(tmp_path.as_str(), remote_path).await {
            // remove 对「目标本就不存在」容错:那说明 rename 是别的原因失败,
            // 下面这次重试会把真正的错误报出来。
            let _ = sftp.remove_file(remote_path).await;
            sftp.rename(tmp_path.as_str(), remote_path)
                .await
                .map_err(|e| {
                    SftpTransferError::Sftp(format!(
                        "failed to move uploaded file into place '{remote_path}': {e} \
                        (first rename attempt: {first})"
                    ))
                })?;
        }
        Ok(())
    }
    .await;

    // 失败:清掉远端半截临时文件(目标文件从头到尾没被动过),再把错误返出去。
    if let Err(e) = copy_result {
        let _ = sftp.remove_file(tmp_path.as_str()).await;
        let _ = sftp.close().await; // best-effort
        drop(handle_guard);
        return Err(e);
    }

    let _ = sftp.close().await; // best-effort
    drop(handle_guard);

    Ok(total)
}

/// 传输中途落地的临时文件名:目标旁边加 `.mt-sftp-partial` 后缀。
///
/// 上传(远端)与下载(本地)共用同一套命名 —— 同目录保证 rename 是同一文件系统
/// 内的原子改名而非跨盘拷贝,后缀也让用户一眼认出「这是没传完的残留」。
fn sftp_partial_path(target: &str) -> String {
    format!("{target}.mt-sftp-partial")
}

/// 在已 acquire 到的 session 上开 SFTP channel,把远程文件流式下载并**落盘**到本地路径。
///
/// 不把内容回传给 caller(避免二进制 base64 + 返回体封顶问题)。先写临时文件
/// (`<local>.mt-sftp-partial`)再原子 rename,避免下载中途失败留下半截文件;失败时清理临时文件。
///
/// 返回写入本地的字节数。错误区分 transport(可 evict 重连)与 SFTP 业务错(不 evict)。
pub async fn run_sftp_download_on_session(
    session: &CachedSession,
    remote_path: &str,
    local_path: &str,
    transfer_timeout: Duration,
) -> Result<u64, SftpTransferError> {
    use russh_sftp::client::SftpSession;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // 临时文件路径:目标旁边加后缀,与目标同盘保证 rename 是原子改名而非跨盘拷贝。
    let tmp_path = sftp_partial_path(local_path);

    let handle_guard = session.lock().await;
    let channel = handle_guard
        .channel_open_session()
        .await
        .map_err(|e| SftpTransferError::Transport(format!("channel_open_session failed: {e}")))?;
    channel.request_subsystem(true, "sftp").await.map_err(|e| {
        SftpTransferError::Transport(format!("request_subsystem(sftp) failed: {e}"))
    })?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| SftpTransferError::Transport(format!("sftp handshake failed: {e}")))?;
    // 放宽协议层每请求超时(默认 10s),避免慢链路下单个 chunk 包就把整段传输打断。
    sftp.set_timeout(sftp_request_timeout_secs(transfer_timeout));

    // 先打开远端文件 —— 远端不存在/无权限是业务错,失败时无需建临时文件。
    let mut remote = sftp
        .open(remote_path)
        .await
        .map_err(|e| SftpTransferError::Sftp(format!("sftp open '{remote_path}' failed: {e}")))?;

    let mut local = tokio::fs::File::create(&tmp_path).await.map_err(|e| {
        SftpTransferError::Sftp(format!("cannot create local file '{tmp_path}': {e}"))
    })?;

    let mut buf = vec![0u8; SFTP_CHUNK_BYTES];
    let mut total: u64 = 0;
    let copy_result: Result<(), SftpTransferError> = async {
        loop {
            let n = remote.read(&mut buf).await.map_err(|e| {
                SftpTransferError::Sftp(format!("sftp read '{remote_path}' failed: {e}"))
            })?;
            if n == 0 {
                break;
            }
            local.write_all(&buf[..n]).await.map_err(|e| {
                SftpTransferError::Sftp(format!("write local file '{tmp_path}' failed: {e}"))
            })?;
            total += n as u64;
        }
        local.flush().await.map_err(|e| {
            SftpTransferError::Sftp(format!("flush local file '{tmp_path}' failed: {e}"))
        })?;
        Ok(())
    }
    .await;

    let _ = sftp.close().await; // best-effort
    drop(handle_guard);

    // 失败:清理半截临时文件后把错误返出去。
    if let Err(e) = copy_result {
        drop(local);
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(e);
    }

    // 成功:确保数据落盘后原子 rename 到目标。
    drop(local);
    tokio::fs::rename(&tmp_path, local_path)
        .await
        .map_err(|e| {
            // rename 失败也要清掉临时文件,避免污染目录。
            let tmp = tmp_path.clone();
            tokio::spawn(async move {
                let _ = tokio::fs::remove_file(&tmp).await;
            });
            SftpTransferError::Sftp(format!(
                "failed to move downloaded file into place '{local_path}': {e}"
            ))
        })?;

    Ok(total)
}

/// 认证一跳(细节见 `auth.rs`)。
///
/// 配置里的 password 是 `mt-secret` 的信封(或升级窗口期的遗留明文),**在这里**
/// 解开 —— 主程序与三个 sidecar 共用这个池,解封点唯一就不会有谁忘了。
/// 解不开(密钥换了 / 密文被改)只记进报错(`password: stored password unavailable`),
/// 照样去试别的方法,绝不把密文当密码送出去;错误文本不含密码。跳板不传密码。
#[allow(clippy::too_many_arguments)]
async fn authenticate(
    handle: &mut Handle<MtClient>,
    hop: &ResolvedHost,
    label: String,
    stored_password: Option<&str>,
    env: &SshClientEnv,
    agent: &mut AgentSession,
    notes: &ClientNotes,
    tracker: &StageTracker,
) -> Result<(), AcquireError> {
    let password = match stored_password.filter(|p| !p.is_empty()) {
        Some(stored) => match mt_secret::reveal_global(stored) {
            Ok(password) => PasswordInput::Available(password),
            Err(e) => PasswordInput::Unavailable(e.to_string()),
        },
        None => PasswordInput::NotConfigured,
    };
    let plan = AuthPlan {
        host: hop,
        label,
        password,
        default_key_dir: env.home_dir.as_ref().map(|home| home.join(".ssh")),
    };
    auth::authenticate_host(handle, &plan, agent, notes, tracker)
        .await
        .map_err(|e| match e {
            AuthError::Rejected(message) => AcquireError::new(AcquireErrorKind::Auth, message),
            AuthError::Transport(message) => AcquireError::new(AcquireErrorKind::Network, message),
        })
}

// ============================================================================
// tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(id: &str) -> SshConnection {
        SshConnection {
            id: id.into(),
            name: "server".into(),
            host: "example.com".into(),
            port: 22,
            user: "deploy".into(),
            password: Some("secret".into()),
            identity_file: None,
            group: None,
            extra: Default::default(),
        }
    }

    /// 不读 ssh config、没有 home 的路线解析(纯函数测试用)。
    fn plain_route(conn: &SshConnection) -> ResolvedRoute {
        let env = RouteEnv {
            ssh_config_path: None,
            home: None,
            local_user: Some("me"),
        };
        ssh_config::resolve_route(conn, &env).unwrap()
    }

    fn identity_of(conn: &SshConnection) -> CachedConnectionIdentity {
        CachedConnectionIdentity::new(conn, plain_route(conn))
    }

    #[test]
    fn cached_connection_identity_tracks_endpoint_credentials_and_route() {
        let base = connection("ssh");
        let identity = identity_of(&base);
        assert!(identity == identity_of(&base));

        for changed in [
            SshConnection {
                host: "other.example.com".into(),
                ..base.clone()
            },
            SshConnection {
                port: 2222,
                ..base.clone()
            },
            SshConnection {
                user: "root".into(),
                ..base.clone()
            },
            SshConnection {
                password: Some("other".into()),
                ..base.clone()
            },
            SshConnection {
                identity_file: Some("/keys/id_ed25519".into()),
                ..base.clone()
            },
        ] {
            assert!(identity != identity_of(&changed));
        }

        let display_only = SshConnection {
            name: "renamed".into(),
            group: Some("production".into()),
            ..base.clone()
        };
        assert!(identity == identity_of(&display_only));

        // 连接配置一字未动,但 ssh config 把它解析到了别处:不能复用到旧目标上。
        let mut rerouted = plain_route(&base);
        rerouted.target.host_name = "elsewhere.example.com".into();
        assert!(identity != CachedConnectionIdentity::new(&base, rerouted));
    }

    #[test]
    fn sftp_request_timeout_tracks_transfer_window() {
        // 协议层每请求超时随外层传输窗口放宽,而非停在默认 10s。
        assert_eq!(sftp_request_timeout_secs(Duration::from_secs(300)), 300);
        assert_eq!(sftp_request_timeout_secs(Duration::from_secs(60)), 60);
        // 下限保护:0 秒窗口至少返 1s,不会出现 0 秒立即超时。
        assert_eq!(sftp_request_timeout_secs(Duration::from_secs(0)), 1);
        // 亚秒窗口(被 caller .max(1) 前可能出现)也至少 1s。
        assert_eq!(sftp_request_timeout_secs(Duration::from_millis(500)), 1);
    }

    #[test]
    fn append_bounded_output_caps_without_losing_prefix() {
        let mut output = b"abc".to_vec();
        assert!(append_bounded_output(&mut output, b"defgh", 6));
        assert_eq!(output, b"abcdef");
        assert!(append_bounded_output(&mut output, b"more", 6));
        assert_eq!(output, b"abcdef");
    }

    #[test]
    fn append_bounded_output_reports_exact_fit_as_not_truncated() {
        let mut output = Vec::new();
        assert!(!append_bounded_output(&mut output, b"abcd", 4));
        assert_eq!(output, b"abcd");
        assert!(!append_bounded_output(&mut output, b"", 4));
    }

    #[test]
    fn append_bounded_output_zero_cap_discards_nonempty_chunks() {
        let mut output = Vec::new();
        assert!(append_bounded_output(&mut output, b"data", 0));
        assert!(output.is_empty());
    }

    #[test]
    fn exec_state_exposes_safe_fallback_matrix() {
        assert!(BoundedExecState::NotDispatched.safe_to_fallback());
        assert!(BoundedExecState::ExecEnqueueTimedOut.safe_to_fallback());
        assert!(BoundedExecState::Rejected.safe_to_fallback());
        assert!(!BoundedExecState::ChannelOpenUnknown.safe_to_fallback());
        assert!(!BoundedExecState::ExecReplyUnknown.safe_to_fallback());
        assert!(!BoundedExecState::Started.safe_to_fallback());

        assert!(!BoundedExecState::ChannelOpenUnknown.may_have_started());
        assert!(!BoundedExecState::ExecEnqueueTimedOut.may_have_started());
        assert!(BoundedExecState::ExecReplyUnknown.may_have_started());
        assert!(BoundedExecState::Started.may_have_started());
    }

    #[test]
    fn compatibility_command_started_guard_is_conservative() {
        let output = exec_output(BoundedExecState::NotDispatched, true);
        assert!(output.timed_out);
        assert!(!output.command_started);
        assert_eq!(output.state, BoundedExecState::NotDispatched);
        assert_eq!(output.exit_code, None);

        // channel-open 阶段还没发 exec，但旧 caller 只看 bool 时也不得
        // 立即 fallback；精确含义由 state 表达。
        let uncertain = exec_output(BoundedExecState::ChannelOpenUnknown, true);
        assert!(uncertain.command_started);
        assert!(!uncertain.state.may_have_started());

        let mut rejected = exec_output(BoundedExecState::Rejected, false);
        assert!(rejected.safe_to_fallback());
        mark_exec_cleanup_uncertain(&mut rejected);
        assert!(!rejected.safe_to_fallback());
        assert!(rejected.requires_session_retirement());
        assert!(rejected.command_started);
    }

    #[test]
    fn failure_cannot_downgrade_observed_execution() {
        assert_eq!(
            transition_exec_state(BoundedExecState::ExecReplyUnknown, ExecStateEvent::Rejected,),
            BoundedExecState::Rejected
        );
        assert_eq!(
            transition_exec_state(BoundedExecState::Started, ExecStateEvent::Rejected),
            BoundedExecState::Started
        );
        assert_eq!(
            transition_exec_state(BoundedExecState::ExecReplyUnknown, ExecStateEvent::Accepted,),
            BoundedExecState::Started
        );
        assert_eq!(
            transition_exec_state(
                BoundedExecState::ExecReplyUnknown,
                ExecStateEvent::ExecutionEvidence,
            ),
            BoundedExecState::Started
        );
    }

    #[test]
    fn remove_arc_if_same_preserves_replacement_winner() {
        let stale = Arc::new(1_u8);
        let winner = Arc::new(2_u8);
        let mut entries = HashMap::from([("connection".to_string(), winner.clone())]);

        assert!(remove_arc_if_same(&mut entries, "connection", &stale).is_none());
        assert!(Arc::ptr_eq(
            entries.get("connection").expect("winner remains cached"),
            &winner
        ));

        let removed = remove_arc_if_same(&mut entries, "connection", &winner)
            .expect("matching winner is removed");
        assert!(Arc::ptr_eq(&removed, &winner));
        assert!(!entries.contains_key("connection"));
    }

    #[test]
    fn removed_session_disconnect_requires_exclusive_ownership() {
        assert!(should_disconnect_removed_session(false, 1));
        assert!(!should_disconnect_removed_session(true, 1));
        assert!(!should_disconnect_removed_session(false, 2));
        assert!(!should_disconnect_removed_session(true, 2));
    }

    /// 临时文件名:上传(远端路径)与下载(本地路径)共用同一后缀,
    /// 且**永远落在目标同目录**(同文件系统 → rename 是原子改名)。
    #[test]
    fn sftp_partial_path_appends_suffix_next_to_target() {
        assert_eq!(
            sftp_partial_path("/home/u/.mini-term/pasted/a.png"),
            "/home/u/.mini-term/pasted/a.png.mt-sftp-partial"
        );
        // Windows 本地路径(下载侧)同理:只加后缀,目录部分一字不动。
        assert_eq!(
            sftp_partial_path(r"D:\dl\a.zip"),
            r"D:\dl\a.zip.mt-sftp-partial"
        );
        // 无扩展名 / 带空格的目标也只是加后缀。
        assert_eq!(
            sftp_partial_path("/tmp/my file"),
            "/tmp/my file.mt-sftp-partial"
        );
    }

    #[test]
    fn pool_config_default_matches_research_profile() {
        let c = PoolConfig::default();
        assert_eq!(c.idle_timeout, Duration::from_secs(600));
        assert_eq!(c.max_lifetime, Duration::from_secs(7200));
        assert_eq!(c.keepalive_interval, Duration::from_secs(30));
        assert_eq!(c.keepalive_max, 3);
        assert_eq!(c.max_sessions, 8);
        assert_eq!(c.gatetime_cooldown, Duration::from_secs(30));
        assert_eq!(c.reaper_tick, Duration::from_secs(60));
        assert_eq!(c.shutdown_per_session_timeout, Duration::from_secs(2));
        assert_eq!(c.connect_timeout, Duration::from_secs(20));
        assert_eq!(c.connect_timeout_per_address, Duration::from_secs(5));
        // 建连在认证前的 keepalive 计时器到点之前结束(见 connect_timeout 注释)。
        assert!(c.connect_timeout < c.keepalive_interval);
    }

    #[test]
    fn cooldown_applies_only_to_the_same_identity_within_the_window() {
        let base = connection("ssh");
        let identity = identity_of(&base);
        let now = Instant::now();
        let record = AuthFailure {
            identity: identity.clone(),
            failed_at: now,
            until: now + Duration::from_secs(30),
            message: "authentication failed for deploy@example.com:22 (server allows: \
                      password): password: rejected by server"
                .into(),
        };
        let later = now + Duration::from_millis(10_500);
        let remaining = cooldown_remaining(&record, &identity, later).expect("仍在冷却期");
        assert_eq!(remaining, Duration::from_millis(19_500));
        let message = cooldown_message(&record, remaining, later);
        assert!(
            message.starts_with("authentication cooldown: "),
            "{message}"
        );
        assert!(message.contains("10s ago"), "{message}");
        // 剩余 19.5s 向上取整为 20s。
        assert!(message.contains("another 20s"), "{message}");
        assert!(
            message.contains("to avoid triggering server-side bans"),
            "{message}"
        );
        assert!(
            message.ends_with("password: rejected by server"),
            "{message}"
        );

        // 冷却期一过即可重试。
        assert!(cooldown_remaining(&record, &identity, now + Duration::from_secs(30)).is_none());
        // 改了连接配置(这里是密码):身份变了,立即可重试。
        let edited = SshConnection {
            password: Some("new-secret".into()),
            ..base
        };
        assert!(cooldown_remaining(&record, &identity_of(&edited), later).is_none());
    }

    #[tokio::test]
    async fn pool_auth_cooldown_is_keyed_by_connection_identity() {
        let pool = SshPool::with_paths(
            PoolConfig::default(),
            std::env::temp_dir().join("mt-ssh-test-known_hosts-cooldown"),
        );
        let base = connection("ssh");
        let identity = identity_of(&base);
        assert!(pool.check_auth_cooldown(&base, &identity).is_ok());

        pool.record_auth_failure(&identity, "authentication failed: x");
        let err = pool.check_auth_cooldown(&base, &identity).unwrap_err();
        assert_eq!(err.kind, AcquireErrorKind::Cooldown);
        let message = &err.message;
        assert!(
            message.ends_with("Last error: authentication failed: x"),
            "{message}"
        );

        // 换了用户 = 新身份:放行,并清掉不再适用的旧记录。
        let edited = SshConnection {
            user: "root".into(),
            ..base.clone()
        };
        let edited_identity = identity_of(&edited);
        assert!(pool.check_auth_cooldown(&edited, &edited_identity).is_ok());
        assert!(pool.check_auth_cooldown(&base, &identity).is_ok());

        // gatetime_cooldown = 0:不冷却。
        let no_cooldown = SshPool::with_paths(
            PoolConfig {
                gatetime_cooldown: Duration::ZERO,
                ..PoolConfig::default()
            },
            std::env::temp_dir().join("mt-ssh-test-known_hosts-cooldown"),
        );
        no_cooldown.record_auth_failure(&identity, "authentication failed: x");
        assert!(no_cooldown.check_auth_cooldown(&base, &identity).is_ok());
    }

    #[test]
    fn isolated_env_uses_nothing_but_the_given_known_hosts() {
        let path = PathBuf::from("known_hosts_for_test");
        let env = SshClientEnv::isolated(path.clone());
        assert_eq!(env.known_hosts_path, path);
        assert_eq!(env.home_dir, None);
        assert_eq!(env.ssh_config_path, None);
        assert_eq!(env.agent, AgentSource::Disabled);
        assert_eq!(env.local_user, None);
    }

    #[test]
    fn system_env_points_at_the_users_ssh_directory() {
        // 只看路径的拼法,不读任何文件。
        let env = SshClientEnv::system();
        assert_eq!(env.agent, AgentSource::System);
        if let Some(home) = &env.home_dir {
            assert_eq!(env.known_hosts_path, home.join(".ssh").join("known_hosts"));
            assert_eq!(env.ssh_config_path, Some(home.join(".ssh").join("config")));
        }
    }

    #[test]
    fn acquire_error_displays_its_message() {
        let error = AcquireError::new(AcquireErrorKind::Timeout, "ssh connection to x timed out");
        assert_eq!(error.to_string(), "ssh connection to x timed out");
    }

    /// pick_lru_victim 的算法纯函数等价物,用 u64 而非 Arc<CachedSession>,
    /// 避开"造真 Handle"的不可能任务 —— 而 pick_lru_victim 本身就是这套
    /// 算法在 HashMap<_, Arc<CachedSession>> 上的应用。
    #[test]
    fn pick_lru_victim_algorithm_chooses_smallest_last_used() {
        fn pick<T>(map: &HashMap<String, T>, key: impl Fn(&T) -> u64) -> Option<String> {
            map.iter()
                .min_by_key(|(_, v)| key(v))
                .map(|(k, _)| k.clone())
        }
        let mut m: HashMap<String, u64> = HashMap::new();
        m.insert("a".into(), 100);
        m.insert("b".into(), 50);
        m.insert("c".into(), 200);
        assert_eq!(pick(&m, |&v| v).as_deref(), Some("b"));
    }

    #[test]
    fn pick_lru_victim_empty_map_returns_none() {
        let m: HashMap<String, Arc<CachedSession>> = HashMap::new();
        assert!(pick_lru_victim(&m).is_none());
    }

    // --- PR3: reaper / select_expired / Drop --------------------------------

    /// `select_expired` 在空池上稳健返回空。
    #[test]
    fn select_expired_empty_input_returns_empty() {
        let v = select_expired(
            &[],
            1_000_000,
            Duration::from_secs(60),
            Duration::from_secs(3600),
        );
        assert!(v.is_empty());
    }

    /// 全部在 idle / lifetime 阈值内 → 没有 victim。
    #[test]
    fn select_expired_all_fresh_no_victims() {
        // last_used 在 now 前 1s,age 5s 内 —— idle 阈值 60s、lifetime 3600s,均未过期。
        let now_ms: u64 = 1_000_000_000;
        let triples = vec![
            ("a".to_string(), now_ms - 1_000, Duration::from_secs(5)),
            ("b".to_string(), now_ms - 500, Duration::from_secs(1)),
        ];
        let v = select_expired(
            &triples,
            now_ms,
            Duration::from_secs(60),
            Duration::from_secs(3600),
        );
        assert!(v.is_empty(), "expected no victims, got {v:?}");
    }

    /// 仅 idle 过期:`now - last_used >= idle_timeout`,但 age 仍小于 max_lifetime。
    #[test]
    fn select_expired_picks_idle_expired() {
        let now_ms: u64 = 1_000_000_000;
        let triples = vec![
            (
                "idle".to_string(),
                now_ms - 70_000,
                Duration::from_secs(120),
            ), // 70s 没用 ≥ 60s
            (
                "fresh".to_string(),
                now_ms - 1_000,
                Duration::from_secs(120),
            ),
        ];
        let v = select_expired(
            &triples,
            now_ms,
            Duration::from_secs(60),
            Duration::from_secs(3600),
        );
        assert_eq!(v, vec!["idle".to_string()]);
    }

    /// 仅 lifetime 过期:age ≥ max_lifetime,即便 last_used 很近。
    #[test]
    fn select_expired_picks_lifetime_expired() {
        let now_ms: u64 = 1_000_000_000;
        let triples = vec![
            ("old".to_string(), now_ms - 100, Duration::from_secs(7200)), // 已活够 2h
            ("young".to_string(), now_ms - 100, Duration::from_secs(60)),
        ];
        let v = select_expired(
            &triples,
            now_ms,
            Duration::from_secs(600),
            Duration::from_secs(3600),
        );
        assert_eq!(v, vec!["old".to_string()]);
    }

    /// 同时 idle 与 lifetime 过期的多个条目都会被踢。
    #[test]
    fn select_expired_handles_mixed_idle_and_lifetime() {
        let now_ms: u64 = 1_000_000_000;
        let triples = vec![
            (
                "idle".to_string(),
                now_ms - 700_000,
                Duration::from_secs(10),
            ),
            ("aged".to_string(), now_ms - 1, Duration::from_secs(7_201)),
            ("ok".to_string(), now_ms - 1_000, Duration::from_secs(60)),
        ];
        let mut v = select_expired(
            &triples,
            now_ms,
            Duration::from_secs(600),
            Duration::from_secs(3600),
        );
        v.sort();
        assert_eq!(v, vec!["aged".to_string(), "idle".to_string()]);
    }

    /// `now_ms < last_used`(系统时钟回拨)时 `saturating_sub` 不溢出,该条目按 idle=0 处理 → 不踢。
    #[test]
    fn select_expired_tolerates_clock_skew() {
        let triples = vec![("future".to_string(), 1_000_000_000, Duration::from_secs(10))];
        // now < last_used —— saturating_sub 返 0,idle 判断不命中;lifetime 也未到。
        let v = select_expired(
            &triples,
            999_999_000,
            Duration::from_secs(60),
            Duration::from_secs(3600),
        );
        assert!(v.is_empty(), "expected clock-skew tolerance, got {v:?}");
    }

    /// 边界:刚好 idle_timeout —— 用 `>=` 判定,等于阈值也算过期。
    #[test]
    fn select_expired_idle_exact_boundary_is_expired() {
        let now_ms: u64 = 1_000_000_000;
        let triples = vec![(
            "on_edge".to_string(),
            now_ms - 60_000,
            Duration::from_secs(1),
        )];
        let v = select_expired(
            &triples,
            now_ms,
            Duration::from_secs(60),
            Duration::from_secs(3600),
        );
        assert_eq!(v, vec!["on_edge".to_string()]);
    }

    /// Drop SshPool 时,reaper task 应该被 abort,且不再 hold inner 的强引用。
    ///
    /// 验证手段:**在 drop 后用 `Weak::strong_count` 探测 inner Arc 的余生**——
    /// reaper 始终只持有 `Weak`,所以 pool 这一份 Arc 一旦 drop,strong_count 立即
    /// 归零。短时可能因 reaper 正巧在 `upgrade()` 后临时持 Arc 而不为 0,
    /// 但 Drop 会同步 abort 它,abort 后 tokio 调度器会在下一次轮转里清掉。
    /// 用一个短自旋等待避免偶发抖动失败。
    #[tokio::test]
    async fn pool_drop_aborts_reaper() {
        let pool = SshPool::with_paths(
            PoolConfig {
                reaper_tick: Duration::from_millis(20), // 让 reaper 频繁 tick,便于触发竞争
                ..PoolConfig::default()
            },
            std::env::temp_dir().join("mt-ssh-mcp-test-known_hosts-drop"),
        );
        let weak_inner: Weak<Mutex<PoolInner>> = Arc::downgrade(&pool.inner);
        assert_eq!(weak_inner.strong_count(), 1, "pool 持有 1 个 strong ref");

        drop(pool);
        // 自旋等待 reaper 释放(若它正巧 upgrade 中)。最长 500ms。
        let deadline = Instant::now() + Duration::from_millis(500);
        while weak_inner.strong_count() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            weak_inner.strong_count(),
            0,
            "expected pool inner released, reaper should not hold strong ref"
        );
    }

    /// reaper task 在 pool 被 drop 后,下一个 tick 必须能感知到并主动退出。
    ///
    /// 直接 spawn 一个 reaper 而不通过 SshPool —— 这样 reaper 没有"Drop abort"兜底,
    /// 必须靠 Weak::upgrade 返 None 自行退出,**真正测出了 Weak 生命周期的正确性**。
    #[tokio::test]
    async fn reaper_exits_when_pool_dropped() {
        let inner = Arc::new(Mutex::new(PoolInner {
            sessions: HashMap::new(),
        }));
        let weak = Arc::downgrade(&inner);
        let handle = spawn_reaper(
            weak,
            Duration::from_millis(20),
            Duration::from_secs(600),
            Duration::from_secs(7200),
            Duration::from_secs(2),
        );

        // drop 掉唯一的 strong Arc;reaper 持的是 Weak,下一次 upgrade 会返 None。
        drop(inner);

        // 自旋等待 reaper task 退出(最长 500ms)。
        let deadline = Instant::now() + Duration::from_millis(500);
        while !handle.is_finished() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            handle.is_finished(),
            "reaper should have exited after the pool's inner Arc was dropped"
        );
    }
}
