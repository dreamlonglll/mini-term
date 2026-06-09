//! `mt-ssh-mcp` 的 SSH 持久会话池。
//!
//! 设计来源:`.trellis/tasks/05-22-refactor-ssh-mcp-persistent-session-pool/research/`。
//! 摘要:
//! - 库:`russh 0.61`(pure Rust + 原生 tokio async),加密后端 `ring`(避免 Windows
//!   MSVC 上对 aws-lc-sys NASM 的依赖)。
//! - 数据结构:`HashMap<ConnId, Arc<CachedSession>>` 包在 `tokio::sync::Mutex` 内;
//!   每个 `CachedSession` 自己再裹一层 `Mutex<russh::client::Handle>` 把同 session
//!   的 channel 操作串行化(YAGNI 多 channel 并发)。
//! - 默认 profile:idle 10min / lifetime 2h / keepalive 30s × 3 / cap 8 LRU /
//!   lazy 重连 + 单次 retry + 30s gatetime cooldown。来源见 research 文件。
//! - host-key 策略:`accept-new` 语义。首见接受并写入 `~/.ssh/known_hosts`,
//!   变更拒绝。仅支持 plaintext known_hosts 条目;hashed 条目被识别为"未知"
//!   并按首见处理(append 一条 plaintext,与已有 hashed 共存,无安全损失)。
//! - 认证顺序:identity_file 优先 → password 兜底,password 走 password 与
//!   keyboard-interactive 两种 method(某些服务器仅接后者)。
//!
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mt_core::SshConnection;
use russh::client::{self, Handle, Handler};
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

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
    /// session 上一次 auth 失败后,在此时长内直接返回错误,不再去打远端
    /// (autossh `AUTOSSH_GATETIME=30s` 风格)。
    pub gatetime_cooldown: Duration,
    /// 后台 reaper 扫描频率。默认 60s。
    pub reaper_tick: Duration,
    /// shutdown 时单 session disconnect 的上限,防止远端 hang 阻塞 sidecar 退出。
    pub shutdown_per_session_timeout: Duration,
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
        }
    }
}

/// 池内部状态。`SshPool` 用 `Mutex` 把它包起来,保证 acquire/evict 串行。
struct PoolInner {
    /// `connection.id` → 缓存的 session。
    sessions: HashMap<String, Arc<CachedSession>>,
}

/// 池里一项:russh handle + 时间戳 + 连接快照。
pub struct CachedSession {
    /// 串行化同 session 上的 channel 操作。russh Handle 自身 Clone 廉价,但允许
    /// 并发开 channel 会让审计日志顺序与"标记 unhealthy"的语义复杂化。
    handle: Mutex<Handle<MtClient>>,
    /// session 建立时刻;用于 `max_lifetime` 判定。
    opened_at: Instant,
    /// 最近一次使用(`ssh_exec` 触发)的 UNIX 毫秒。Atomic 是为了 reaper 不抢锁就能读。
    last_used: AtomicU64,
    /// auth 连失败后的冷却截止 UNIX 毫秒,0 表示无 cooldown。
    unhealthy_until: AtomicU64,
}

impl CachedSession {
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

/// 池的对外 facade。`Arc<SshPool>` 由 `SshMcp` 持有,跨工具调用共享。
pub struct SshPool {
    inner: Arc<Mutex<PoolInner>>,
    config: PoolConfig,
    /// known_hosts 文件路径。为了便于测试,允许在构造时显式覆盖默认 `~/.ssh/known_hosts`。
    known_hosts_path: PathBuf,
    /// 后台 reaper task 句柄,持有以便在 Drop 时 abort。
    ///
    /// 用 `std::sync::Mutex` 而非 `tokio::sync::Mutex`:Drop 是同步上下文,不能
    /// `.await` 拿锁;abort 本身又是同步 API。reaper task 内部不再用这个字段,
    /// 它只会被 Drop / 显式访问读到。
    reaper: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl SshPool {
    /// 用默认 config 与 `~/.ssh/known_hosts` 路径构造。
    ///
    /// 找不到 home 时回退到当前目录下 `.known_hosts`(极端环境的兜底,正常 Tauri
    /// 桌面端不会触发)。
    pub fn new() -> Self {
        Self::with_config(PoolConfig::default())
    }

    pub fn with_config(config: PoolConfig) -> Self {
        let known_hosts_path = dirs::home_dir()
            .map(|h| h.join(".ssh").join("known_hosts"))
            .unwrap_or_else(|| PathBuf::from(".known_hosts"));
        Self::with_paths(config, known_hosts_path)
    }

    /// 全显式构造,主要给单测用。会在 tokio runtime 中 spawn 后台 reaper task。
    ///
    /// **前置**:必须在 tokio runtime 内调用(直接或间接处于 `#[tokio::main]` /
    /// `#[tokio::test]` 上下文),否则 `tokio::spawn` 会 panic。
    pub fn with_paths(config: PoolConfig, known_hosts_path: PathBuf) -> Self {
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
            known_hosts_path,
            reaper: std::sync::Mutex::new(Some(reaper_handle)),
        }
    }

    /// 查池里有几条 session(主要给测试用)。
    pub async fn len(&self) -> usize {
        self.inner.lock().await.sessions.len()
    }

    /// 池里是否一条 session 都没有(配对 `len`,clippy 要求)。
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.sessions.is_empty()
    }

    /// 拿一条可用 session。lazy 建,池满时 LRU 淘汰。
    ///
    /// 返回 `Arc<CachedSession>`,调用方再 `session.lock().await` 开 channel。
    /// 若返回的 session `is_unhealthy_now() == true`,调用方应立即返错而不去开 channel,
    /// 实现 30s gatetime cooldown 的语义。
    pub async fn acquire(&self, conn: &SshConnection) -> Result<Arc<CachedSession>, String> {
        {
            let inner = self.inner.lock().await;
            if let Some(s) = inner.sessions.get(&conn.id) {
                // 复用前只检查 underlying handle 是否还活着。
                //
                // **不要在这里同时检查 is_unhealthy_now**:cooldown 的意图是「上一次失败
                // 后 30s 内立即返错、不再去打远端」,需要把带 unhealthy 标记的 session
                // 原样返给调用方,由 ssh_exec 那边的 is_unhealthy_now 分支返错。如果
                // 在这里把 unhealthy session 当作 miss 跳过去走重建,acquire 会真的重连
                // 远端、并返回一个 unhealthy_until=0 的新 session,调用方永远看不到
                // cooldown,gatetime 语义彻底失效。
                if !s.handle.lock().await.is_closed() {
                    s.touch();
                    return Ok(s.clone());
                }
            }
        }

        // 不在池里、或缓存的 session 已死 —— 重建。
        let cached = self.build_session(conn).await?;
        let arc = Arc::new(cached);

        let mut inner = self.inner.lock().await;
        // 重建场景:把旧的(可能已死的)条目踢掉。
        inner.sessions.remove(&conn.id);

        // 池满则 LRU 淘汰一条。
        if inner.sessions.len() >= self.config.max_sessions {
            if let Some(victim_id) = pick_lru_victim(&inner.sessions) {
                if let Some(victim) = inner.sessions.remove(&victim_id) {
                    // 后台 disconnect,不阻塞 acquire。
                    spawn_disconnect(victim, self.config.shutdown_per_session_timeout);
                }
            }
        }
        inner.sessions.insert(conn.id.clone(), arc.clone());
        Ok(arc)
    }

    /// 把指定连接对应的 session 从池里踢出去并后台 disconnect。
    ///
    /// 用途:`ssh_exec` 在拿到 session 后开 channel / exec 失败(transport-level
    /// 死链 race),需要先把这条死 session 移出池再重新 acquire 触发重连。
    /// 不阻塞 caller —— 后台 spawn disconnect 带超时兜底。
    pub async fn evict(&self, id: &str) {
        let mut inner = self.inner.lock().await;
        if let Some(victim) = inner.sessions.remove(id) {
            spawn_disconnect(victim, self.config.shutdown_per_session_timeout);
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

        let timeout = self.config.shutdown_per_session_timeout;
        let futures = entries.into_iter().map(|s| {
            let t = timeout;
            async move {
                let _ = tokio::time::timeout(t, async {
                    let h = s.handle.lock().await;
                    let _ = h.disconnect(russh::Disconnect::ByApplication, "", "en").await;
                })
                .await;
            }
        });
        futures::future::join_all(futures).await;
    }

    /// 真正建一条 session。涵盖 connect + 主机密钥校验(在 Handler 内) + auth。
    async fn build_session(&self, conn: &SshConnection) -> Result<CachedSession, String> {
        // 用 `..Default::default()` 一次性赋值,避开 clippy::field_reassign_with_default。
        let cfg = Arc::new(client::Config {
            keepalive_interval: Some(self.config.keepalive_interval),
            keepalive_max: self.config.keepalive_max,
            ..Default::default()
        });

        let handler = MtClient {
            host: conn.host.clone(),
            port: conn.port,
            known_hosts_path: self.known_hosts_path.clone(),
        };

        let port = if conn.port == 0 { 22 } else { conn.port };
        let mut handle = client::connect(cfg, (conn.host.as_str(), port), handler)
            .await
            .map_err(|e| format!("ssh connect to {}:{} failed: {e}", conn.host, port))?;

        authenticate(&mut handle, conn).await?;

        Ok(CachedSession {
            handle: Mutex::new(handle),
            opened_at: Instant::now(),
            last_used: AtomicU64::new(now_millis()),
            unhealthy_until: AtomicU64::new(0),
        })
    }
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
        .min_by_key(|(_, s)| s.last_used.load(Ordering::Relaxed))
        .map(|(id, _)| id.clone())
}

/// 后台异步 disconnect 一条 session,带超时;失败静默(stderr 一行)。
fn spawn_disconnect(s: Arc<CachedSession>, timeout: Duration) {
    tokio::spawn(async move {
        let res = tokio::time::timeout(timeout, async {
            let h = s.handle.lock().await;
            h.disconnect(russh::Disconnect::ByApplication, "", "en").await
        })
        .await;
        if res.is_err() {
            eprintln!("[mt-ssh-mcp] session disconnect timed out, dropping");
        }
    });
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
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| {
            SftpTransferError::Transport(format!("request_subsystem(sftp) failed: {e}"))
        })?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| SftpTransferError::Transport(format!("sftp handshake failed: {e}")))?;
    // 放宽协议层每请求超时(默认 10s),避免慢链路下单个 chunk 包就把整段传输打断。
    sftp.set_timeout(sftp_request_timeout_secs(transfer_timeout));

    let mut remote = sftp
        .open_with_flags(
            remote_path,
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .map_err(|e| SftpTransferError::Sftp(format!("sftp open '{remote_path}' failed: {e}")))?;

    let mut buf = vec![0u8; SFTP_CHUNK_BYTES];
    let mut total: u64 = 0;
    loop {
        let n = local.read(&mut buf).await.map_err(|e| {
            SftpTransferError::Sftp(format!("read local file '{local_path}' failed: {e}"))
        })?;
        if n == 0 {
            break;
        }
        remote.write_all(&buf[..n]).await.map_err(|e| {
            SftpTransferError::Sftp(format!("sftp write to '{remote_path}' failed: {e}"))
        })?;
        total += n as u64;
    }
    remote
        .flush()
        .await
        .map_err(|e| SftpTransferError::Sftp(format!("sftp flush '{remote_path}' failed: {e}")))?;
    remote.shutdown().await.map_err(|e| {
        SftpTransferError::Sftp(format!("sftp close remote '{remote_path}' failed: {e}"))
    })?;
    let _ = sftp.close().await; // best-effort
    drop(handle_guard);

    Ok(total)
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
    let tmp_path = format!("{local_path}.mt-sftp-partial");

    let handle_guard = session.lock().await;
    let channel = handle_guard
        .channel_open_session()
        .await
        .map_err(|e| SftpTransferError::Transport(format!("channel_open_session failed: {e}")))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| {
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
    tokio::fs::rename(&tmp_path, local_path).await.map_err(|e| {
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

/// 按"先 publickey 后 password"顺序尝试认证;两路皆败抛错。
///
/// password 路径包含 `authenticate_password` 与 `authenticate_keyboard_interactive_*`
/// 两个 method —— 某些服务器禁用 password 而只接受 keyboard-interactive。
async fn authenticate(handle: &mut Handle<MtClient>, conn: &SshConnection) -> Result<(), String> {
    // 1) publickey
    if let Some(path) = conn.identity_file.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let key = load_private_key_compat(path)?;
        // RSA 公钥签名的 hash 选择 —— 关键:`PrivateKeyWithHashAlg::new(key, None)` 对
        // RSA key 会落到 **legacy ssh-rsa(SHA-1)**,而现代 OpenSSH(>=8.8,如 Ubuntu
        // 22.04/24.04)默认禁用 SHA-1 公钥认证,导致 "server rejected all methods"。
        // 按服务器通告的 server-sig-algs 选 rsa-sha2-512/256;服务器未发 EXT_INFO 时
        // best_supported_rsa_hash 返回 Ok(None),回退 SHA-512(现代服务器普遍接受,远胜
        // 默认 SHA-1)。仅 RSA key 需要查 —— `best_supported_rsa_hash` 会等最多 1s
        // EXT_INFO,对 ed25519/ecdsa 该 hash 反正被 new() 忽略,无谓多查只会平添连接延迟。
        let rsa_hash = if key.algorithm().is_rsa() {
            match handle.best_supported_rsa_hash().await {
                Ok(Some(alg)) => alg,
                Ok(None) | Err(_) => Some(russh::keys::HashAlg::Sha512),
            }
        } else {
            None
        };
        let with_hash = PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash);
        let auth = handle
            .authenticate_publickey(&conn.user, with_hash)
            .await
            .map_err(|e| format!("publickey auth error: {e}"))?;
        if auth.success() {
            return Ok(());
        }
    }

    // 2) password (含 keyboard-interactive fallback)
    if let Some(pw) = conn.password.as_deref().filter(|p| !p.is_empty()) {
        let auth = handle
            .authenticate_password(&conn.user, pw)
            .await
            .map_err(|e| format!("password auth error: {e}"))?;
        if auth.success() {
            return Ok(());
        }
        // keyboard-interactive fallback —— 给一个空 submethods 走默认。
        let auth_kbd = handle
            .authenticate_keyboard_interactive_start(&conn.user, None)
            .await
            .map_err(|e| format!("keyboard-interactive auth start error: {e}"))?;
        // 简化处理:遇到任何 prompt 就把密码 echo 进去。多数服务器只问一个 password。
        let success = drive_keyboard_interactive(handle, auth_kbd, pw).await?;
        if success {
            return Ok(());
        }
    }

    Err("authentication failed: server rejected all configured methods (publickey/password)".into())
}

/// 加载私钥,在 russh 原生 `load_secret_key` 之上对传统 PEM 格式做 fallback。
///
/// russh 底层 `ssh-key` 只解析 OpenSSH(`-----BEGIN OPENSSH PRIVATE KEY-----`)与
/// PKCS#8(`-----BEGIN PRIVATE KEY-----`)两种明文私钥;OpenSSH 早期版本及
/// `ssh-keygen -m PEM` / 各类云控制台(如 Oracle Cloud)下发的传统 **PKCS#1 明文
/// RSA**(`-----BEGIN RSA PRIVATE KEY-----`)会被它判为 `Unsupported key type RSA`,
/// 导致用该密钥的连接永远建不起来(系统 `ssh` 客户端却能正常登录)。
///
/// 这里先走 russh 原生解析覆盖现代密钥;失败再读原文件按 PEM 标签判定:命中
/// PKCS#1 标签就用纯 Rust 的 `rsa` crate 解析、转成 `ssh_key::PrivateKey`。其余
/// 格式回退到 russh 原始错误文本,并附 passphrase 不支持的指引。
///
/// 加密私钥(passphrase —— 传统 PEM 的 `Proc-Type: 4,ENCRYPTED` 或 OpenSSH 加密块)
/// 仍不支持,给出明确指引而非吐底层晦涩错误。
fn load_private_key_compat(path: &str) -> Result<russh::keys::PrivateKey, String> {
    // 1) russh 原生: OpenSSH / PKCS#8 明文,覆盖绝大多数现代密钥。
    //    失败时保留错误,供步骤 3 回退诊断复用——避免重复读盘+重复解析。
    let orig_err = match load_secret_key(path, None) {
        Ok(key) => return Ok(key),
        Err(e) => e,
    };

    // 2) fallback: 读原文件,按 PEM 标签判定传统格式。
    //    注意先于"加密指引"读文件,文件不可读直接给 IO 错误。
    let pem = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read private key '{path}': {e}"))?;

    if let Some(key) = try_parse_pkcs1_rsa(&pem)
        .map_err(|e| format!("failed to load private key '{path}': {e}"))?
    {
        return Ok(key);
    }

    // 3) 不是我们能补救的格式 —— 回退到 russh 原始错误 + passphrase 指引。
    Err(format!(
        "failed to load private key '{path}': {orig_err}. \
        If the key is encrypted with a passphrase, mt-ssh-mcp does not support \
        passphrase keys yet — use an unencrypted key (`ssh-keygen -p -N \"\" -f <key>`) or ssh-agent."
    ))
}

/// 尝试把传统 **PKCS#1 明文 RSA** PEM(`-----BEGIN RSA PRIVATE KEY-----`)解析成
/// `ssh_key::PrivateKey`。不是 PKCS#1 标签时返回 `Ok(None)`,让调用方回退到原始错误。
///
/// 抽成接受 `&str`、不碰文件系统的纯函数,便于单测直接喂 PEM 字符串。
fn try_parse_pkcs1_rsa(pem: &str) -> Result<Option<russh::keys::PrivateKey>, String> {
    use rsa::pkcs1::DecodeRsaPrivateKey;

    const BEGIN: &str = "-----BEGIN RSA PRIVATE KEY-----";
    const END: &str = "-----END RSA PRIVATE KEY-----";

    // 不是 PKCS#1 标签 —— 交回上层(可能是 SEC1 EC / 加密 OpenSSH 等,本函数不处理)。
    let Some(begin_at) = pem.find(BEGIN) else {
        return Ok(None);
    };
    // 加密的传统 PEM(`Proc-Type: 4,ENCRYPTED`)明确不支持,给可操作指引。
    if pem.contains("Proc-Type:") && pem.contains("ENCRYPTED") {
        return Err(
            "key is an encrypted PKCS#1 RSA private key (Proc-Type: 4,ENCRYPTED); \
            passphrase keys are not supported — decrypt it first \
            (`openssl rsa -in <key> -out <key.dec>` or re-export without a passphrase)"
                .into(),
        );
    }

    // 自剥 PEM -> DER:rsa 0.10 没有 `pem` feature(PEM 方法 gated 在 pkcs1/pem,
    // rsa 未传递),故不能用 from_pkcs1_pem;改取 BEGIN/END 之间的 base64 主体、
    // 去掉所有空白后解码成 DER,再走不依赖 pem feature 的 from_pkcs1_der。
    let body_start = begin_at + BEGIN.len();
    let body_end = pem
        .find(END)
        .filter(|&e| e >= body_start)
        .ok_or_else(|| "PKCS#1 RSA PEM missing END marker".to_string())?;
    let b64: String = pem[body_start..body_end]
        .split_whitespace()
        .collect();
    let der = {
        use base64_engine::Engine;
        base64_engine::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .map_err(|e| format!("invalid base64 in PKCS#1 RSA PEM: {e}"))?
    };

    let rsa_key = rsa::RsaPrivateKey::from_pkcs1_der(&der)
        .map_err(|e| format!("invalid PKCS#1 RSA private key: {e}"))?;
    let keypair = russh::keys::ssh_key::private::RsaKeypair::try_from(&rsa_key)
        .map_err(|e| format!("cannot convert RSA key into ssh-key form: {e}"))?;
    Ok(Some(russh::keys::PrivateKey::from(keypair)))
}

/// 把 password 灌进 keyboard-interactive 响应。多 round prompt 都重复 echo 同一密码,
/// 服务器若用奇怪 prompt(如 OTP)会自然失败 —— 这是设计意图,不要瞎猜。
async fn drive_keyboard_interactive(
    handle: &mut Handle<MtClient>,
    mut state: russh::client::KeyboardInteractiveAuthResponse,
    password: &str,
) -> Result<bool, String> {
    use russh::client::KeyboardInteractiveAuthResponse::*;
    loop {
        match state {
            Success => return Ok(true),
            Failure { .. } => return Ok(false),
            InfoRequest { prompts, .. } => {
                let answers: Vec<String> = prompts.iter().map(|_| password.to_string()).collect();
                state = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(|e| format!("keyboard-interactive respond error: {e}"))?;
            }
        }
    }
}

/// russh Handler 实现:每条 session 一个实例,负责主机密钥校验。
pub struct MtClient {
    host: String,
    port: u16,
    known_hosts_path: PathBuf,
}

impl Handler for MtClient {
    type Error = russh::Error;

    /// host-key 校验:accept-new 语义。
    /// - 在 known_hosts 找到一条匹配 host + 同 algo,key 字节完全一致 → 通过。
    /// - 找到匹配 host + 同 algo 但 key 不同 → 拒绝(返回 Ok(false))。
    /// - 没找到匹配 host → 把当前 server key 以 plaintext 追加到 known_hosts,通过。
    /// - I/O 出错(known_hosts 不可读 / 不可写) → 拒绝,避免悄默接受未知 host。
    async fn check_server_key(
        &mut self,
        server_pubkey: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let host_pattern = host_pattern(&self.host, self.port);
        let raw = std::fs::read_to_string(&self.known_hosts_path).unwrap_or_default();
        match match_known_host(&raw, &host_pattern, server_pubkey) {
            HostKeyMatch::Match => Ok(true),
            HostKeyMatch::Mismatch => {
                eprintln!(
                    "[mt-ssh-mcp] host key MISMATCH for {host_pattern}; refusing to connect. \
                    Remove the offending line from {} if the change is expected.",
                    self.known_hosts_path.display()
                );
                Ok(false)
            }
            HostKeyMatch::Unknown => {
                if let Err(e) = append_known_host(&self.known_hosts_path, &host_pattern, server_pubkey) {
                    eprintln!(
                        "[mt-ssh-mcp] failed to append to {}: {e}",
                        self.known_hosts_path.display()
                    );
                    return Ok(false);
                }
                Ok(true)
            }
        }
    }
}

/// 拼一条 known_hosts 的 host 字段。22 端口写 `host`,其它端口写 `[host]:port`,
/// 与 OpenSSH 客户端写入风格一致。
fn host_pattern(host: &str, port: u16) -> String {
    if port == 22 || port == 0 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// 主机密钥比对结果。
#[derive(Debug, PartialEq, Eq)]
enum HostKeyMatch {
    /// host 匹配且 key 字节相同。
    Match,
    /// host 匹配,**同 algo** 但 key 字节不同。MITM / 服务器换 key 都会落这条。
    Mismatch,
    /// 没找到任何 host 匹配条目。
    Unknown,
}

/// 在 known_hosts 文本里查 `host_pattern` 对应的条目并与 `server_pubkey` 比对。
///
/// 解析规则:
/// - 跳过空行与 `#` 起始的注释。
/// - 字段以空格 / TAB 分隔:`<hostspec> <algo> <base64key> [comment]`。
/// - hostspec 可以是逗号分隔多个 host;**仅支持 plaintext**,以 `|1|` 起始的
///   hashed 条目被识别为"不匹配本 host",转给 accept-new 路径(可能造成与
///   已有 hashed 条目共存,无安全损失)。
/// - 同 host + 同 algo 但 key 不同 → 立即返回 `Mismatch`,不再扫剩余行。
fn match_known_host(
    raw: &str,
    host_pattern: &str,
    server_pubkey: &russh::keys::ssh_key::PublicKey,
) -> HostKeyMatch {
    let want_algo = server_pubkey.algorithm().as_str().to_string();
    let want_bytes = match server_pubkey.to_bytes() {
        Ok(b) => b,
        Err(_) => return HostKeyMatch::Unknown,
    };
    let mut saw_same_host_same_algo_diff_key = false;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_ascii_whitespace();
        let hostspec = match fields.next() {
            Some(h) => h,
            None => continue,
        };
        if hostspec.starts_with("|") {
            // hashed,跳过 —— 见函数 doc comment 说明。
            continue;
        }
        if !hostspec.split(',').any(|h| h.eq_ignore_ascii_case(host_pattern)) {
            continue;
        }
        let algo = match fields.next() {
            Some(a) => a,
            None => continue,
        };
        if !algo.eq_ignore_ascii_case(&want_algo) {
            // 不同算法的同 host 条目,不算 mismatch —— 允许 host 同时存在多种 key 类型。
            continue;
        }
        let b64 = match fields.next() {
            Some(b) => b,
            None => continue,
        };
        let entry_bytes = match base64_decode(b64) {
            Some(b) => b,
            None => continue,
        };
        if entry_bytes == want_bytes {
            return HostKeyMatch::Match;
        }
        saw_same_host_same_algo_diff_key = true;
    }
    if saw_same_host_same_algo_diff_key {
        HostKeyMatch::Mismatch
    } else {
        HostKeyMatch::Unknown
    }
}

/// 标准 base64 解码,接受常见的等号填充。无 padding 用例也容忍。
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64_engine::Engine;
    base64_engine::engine::general_purpose::STANDARD.decode(s.trim()).ok()
}

// 我们已经间接通过 russh 拉了 base64 —— 但直接 use 路径不稳。改成自己引入。
// (实际依赖在 Cargo.toml 也已添加。)
use base64 as base64_engine;

/// 把一条新 host-key 写入 known_hosts,父目录不存在则创建。
fn append_known_host(
    path: &std::path::Path,
    host_pattern: &str,
    server_pubkey: &russh::keys::ssh_key::PublicKey,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // algorithm() 返回临时,先 bind 延长生命周期再借 as_str()。
    let algo_holder = server_pubkey.algorithm();
    let algo = algo_holder.as_str();
    let b64 = {
        use base64_engine::Engine;
        let bytes = server_pubkey
            .to_bytes()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        base64_engine::engine::general_purpose::STANDARD.encode(bytes)
    };
    let line = format!("{host_pattern} {algo} {b64}\n");
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())
}

// ============================================================================
// tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn host_pattern_uses_bracket_form_only_for_nonstandard_port() {
        assert_eq!(host_pattern("h.example.com", 22), "h.example.com");
        assert_eq!(host_pattern("h.example.com", 0), "h.example.com");
        assert_eq!(host_pattern("h.example.com", 2222), "[h.example.com]:2222");
    }

    #[test]
    fn match_known_host_ignores_blank_and_comment_lines() {
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let raw = "\n# comment line\n\n# another\n";
        assert_eq!(match_known_host(raw, "h.example.com", &pub_key), HostKeyMatch::Unknown);
    }

    /// pick_lru_victim 的算法纯函数等价物,用 u64 而非 Arc<CachedSession>,
    /// 避开"造真 Handle"的不可能任务 —— 而 pick_lru_victim 本身就是这套
    /// 算法在 HashMap<_, Arc<CachedSession>> 上的应用。
    #[test]
    fn pick_lru_victim_algorithm_chooses_smallest_last_used() {
        fn pick<T>(map: &HashMap<String, T>, key: impl Fn(&T) -> u64) -> Option<String> {
            map.iter().min_by_key(|(_, v)| key(v)).map(|(k, _)| k.clone())
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

    // --- match_known_host fixture helpers ----------------------------------
    //
    // 直接用 32 字节常量构造 ed25519 PublicKey,不经过 rng;ssh-key 不验证 ed25519
    // 公钥的密码学合法性(只解析 wire 格式),所以任意 32 字节都能 round-trip。

    fn test_pubkey_from_bytes(bytes: [u8; 32]) -> russh::keys::ssh_key::PublicKey {
        use russh::keys::ssh_key::public::{Ed25519PublicKey, KeyData, PublicKey};
        PublicKey::new(KeyData::Ed25519(Ed25519PublicKey(bytes)), "test")
    }

    fn pubkey_b64(pub_key: &russh::keys::ssh_key::PublicKey) -> String {
        use base64_engine::Engine;
        base64_engine::engine::general_purpose::STANDARD.encode(pub_key.to_bytes().unwrap())
    }

    fn pubkey_algo(pub_key: &russh::keys::ssh_key::PublicKey) -> String {
        pub_key.algorithm().as_str().to_string()
    }

    const KEY_BYTES_A: [u8; 32] = [0x11; 32];
    const KEY_BYTES_B: [u8; 32] = [0x22; 32];

    #[test]
    fn match_known_host_finds_exact_plaintext_entry() {
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let host = "h.example.com";
        let raw = format!(
            "# header\n{host} {} {}\nother-host ssh-rsa AAAA\n",
            pubkey_algo(&pub_key),
            pubkey_b64(&pub_key)
        );
        assert_eq!(match_known_host(&raw, host, &pub_key), HostKeyMatch::Match);
    }

    #[test]
    fn match_known_host_detects_same_host_same_algo_diff_key_as_mismatch() {
        let pub_a = test_pubkey_from_bytes(KEY_BYTES_A);
        let pub_b = test_pubkey_from_bytes(KEY_BYTES_B);
        let raw = format!(
            "h.example.com {} {}\n",
            pubkey_algo(&pub_a),
            pubkey_b64(&pub_b)
        );
        // 文件里登记的是 pub_b,但服务器报上来的是 pub_a → mismatch
        assert_eq!(match_known_host(&raw, "h.example.com", &pub_a), HostKeyMatch::Mismatch);
    }

    #[test]
    fn match_known_host_skips_hashed_entries_and_treats_as_unknown() {
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let raw = "|1|abcsalt|abchash ssh-ed25519 AAAA\n";
        assert_eq!(match_known_host(raw, "h.example.com", &pub_key), HostKeyMatch::Unknown);
    }

    #[test]
    fn match_known_host_comma_separated_hosts_match_any() {
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let raw = format!(
            "alias.example.com,h.example.com {} {}\n",
            pubkey_algo(&pub_key),
            pubkey_b64(&pub_key)
        );
        assert_eq!(match_known_host(&raw, "h.example.com", &pub_key), HostKeyMatch::Match);
    }

    #[test]
    fn match_known_host_different_algo_not_mismatch_but_unknown() {
        // 同 host 但 algo 不同 → 不算 mismatch,允许同 host 多算法共存。
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let raw = "h.example.com ssh-rsa AAAAB3NzaC1yc2EFakeFakeFake\n";
        assert_eq!(match_known_host(raw, "h.example.com", &pub_key), HostKeyMatch::Unknown);
    }

    #[test]
    fn host_pattern_case_insensitive_match() {
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        let raw = format!(
            "H.Example.COM {} {}\n",
            pubkey_algo(&pub_key),
            pubkey_b64(&pub_key)
        );
        assert_eq!(match_known_host(&raw, "h.example.com", &pub_key), HostKeyMatch::Match);
    }

    #[test]
    fn append_known_host_creates_parent_dir_and_writes_entry() {
        let dir = std::env::temp_dir().join(format!(
            "mt-ssh-mcp-test-append-{}",
            std::process::id()
        ));
        let path = dir.join("nested").join("known_hosts");
        let _ = std::fs::remove_dir_all(&dir);
        let pub_key = test_pubkey_from_bytes(KEY_BYTES_A);
        append_known_host(&path, "[h.example.com]:2222", &pub_key).expect("append");
        let content = std::fs::read_to_string(&path).expect("read back");
        assert!(content.starts_with("[h.example.com]:2222 ssh-ed25519 "));
        assert!(content.ends_with("\n"));
        // 再追加一条不同 host,文件应有两行。
        append_known_host(&path, "other.example.com", &pub_key).expect("append 2");
        let content2 = std::fs::read_to_string(&path).expect("read 2");
        assert_eq!(content2.lines().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- PR3: reaper / select_expired / Drop --------------------------------

    /// `select_expired` 在空池上稳健返回空。
    #[test]
    fn select_expired_empty_input_returns_empty() {
        let v = select_expired(&[], 1_000_000, Duration::from_secs(60), Duration::from_secs(3600));
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
        let v = select_expired(&triples, now_ms, Duration::from_secs(60), Duration::from_secs(3600));
        assert!(v.is_empty(), "expected no victims, got {v:?}");
    }

    /// 仅 idle 过期:`now - last_used >= idle_timeout`,但 age 仍小于 max_lifetime。
    #[test]
    fn select_expired_picks_idle_expired() {
        let now_ms: u64 = 1_000_000_000;
        let triples = vec![
            ("idle".to_string(), now_ms - 70_000, Duration::from_secs(120)), // 70s 没用 ≥ 60s
            ("fresh".to_string(), now_ms - 1_000, Duration::from_secs(120)),
        ];
        let v = select_expired(&triples, now_ms, Duration::from_secs(60), Duration::from_secs(3600));
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
            ("idle".to_string(), now_ms - 700_000, Duration::from_secs(10)),
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
        let triples = vec![
            ("future".to_string(), 1_000_000_000, Duration::from_secs(10)),
        ];
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
        let triples = vec![
            ("on_edge".to_string(), now_ms - 60_000, Duration::from_secs(1)),
        ];
        let v = select_expired(&triples, now_ms, Duration::from_secs(60), Duration::from_secs(3600));
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

    // --- PKCS#1 传统 RSA 私钥 fallback (task 06-06-ssh-mcp-pkcs1-rsa-key) -------
    //
    // 下面这把私钥是 `ssh-keygen -t rsa -b 2048 -m PEM` 一次性生成的**测试专用
    // 废弃密钥**,只用于验证 PKCS#1 PEM 能被 try_parse_pkcs1_rsa 解析,不对应任何
    // 真实主机/账号,无任何安全意义。

    const PKCS1_RSA_FIXTURE: &str = r#"-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAlsXj/txeglED7/6iFd1ic1U6NygRpptNLQ5AxqEBHFgRLOxa
v5oTe1JQCIz4AcUiBM1d8DCRhFgdrUCz8O/gc7R3d0CjlWmcrw9VTe/OsndWeexz
WQWduN7dqBR53jNmVw1+638mc6hXq2wNvDpohVJVK7WkpyGIfTJ6Pu/RE0JIpLjj
5UoYHGOtGApME3/xmznA77BLjACCDvOzBtnRJWdvMruzzFgvfZEOJJIEqbgXSrCd
XBxBFo+QEggaHtSzvCE2f5ADAnlZya6EZrxkMykxqWbHrRPrlISw7kOqgx4636QQ
trdwNr6qpWFTon9wMPMC3soJg0wNs3gVxXVuaQIDAQABAoIBAA2/wAR8DguY3bYN
+gk/u1GZzQmTQtYhmrkSxVTCVo/szAF/uuAhaci41ImNvrP9SRaNBRWj0tvxuSBq
d75EnFWQzcV3LzOvLNWd8qvSbtPsJAZRn0yClu5xNwoJIU/iT2EoNDxqHOnhHmrd
xfw14KrVEOU/1uL9di03OYScaUF13xFsvrRgdpjIKyJF7G/Tg7iSn0CrQ14mGmqJ
1y+xYHGEezZjNAQNqYVpcyGXgNqWyNjdg0VOyunnR77GuyGgLzZMTMJGPTm7psnz
o2ieLJOyZWO9H9EnjhwLkC+SfG6f1h6l65ZB/kS9lTwyZehQJZsN0aHUQeEYK45D
GalV1X0CgYEAzbGacQ2FR7Pc0RM52pvRRFc3pI1LAyAiJCnDI80DKUW0dlZurr2D
/jG3WY3FMlPImM5juzwosxgOwBvA3x6Enwz2ZFt9IXDkcykzSXKITp3OdK8Xjy+L
gn2wdvAWHhzp859wwoekXc0oRTAXWsiW9bhSysWoroaTy2HRgwwSf58CgYEAu6W5
FGY7oIHwOXP0uSDBs3HMkuXoUMVcUnk397GGk2rNzH0LkFzYqmsB56IRNgZG2IbZ
3SKPx+9qrm7/SOv7Hbr/HJ8rTfj6U9xGoIGX2ruX8lS2pRhlcgB38t2bvFsrb8RN
xIueELXerOOMnq4oBXp+EGPtEMuK1vf3tQRQNPcCgYAL8TzTRYKweAvhA6m/PH64
5gtv/VgWlV4GFXqj8Ho3gjmJCVmhwZURRBeuFmIVmvGxlYIK0+JVC5eHpdTb32y5
w0nm57zrHR/WY9T7da/eSKE8+xF2Gb+S0vNU5HmUQ/99SouEb9WmMIwfADzK44yI
Naxw42r4vw2DqGk+n4vPZwKBgBLEuaVTsGUWeguVEIYvw5AKMtcCjeD+TISnQTTS
Gc7G4PyyCSUQVE9/UnpzmFsZ954Sptnaah0qUjZOPdRyXfSUTo3zUaaD363hm2LU
c3baSpFfbcFHlmX3rAerqLcHO2n7bXfaKx4qwrHyNI9uhew+WzuScxS59xIXTTxa
yRbzAoGBALUQGCvKpui8rdcPsRAIjqxQ1/EPB2smWtvLwv23d0D9xjotpWdW5Ujk
vm3b+9rtE2PW5jCNTR/JgxaWkAqC6j+fCIAunMUxYFqHMC+Q2aExzL+dDSxBe1Jb
ipMBLNlhlJHNKVmgnpLBSiUoO5fDWn1KcwvQouOC3U3hSMAPnT+7
-----END RSA PRIVATE KEY-----
"#;

    #[test]
    fn try_parse_pkcs1_rsa_parses_plaintext_pkcs1() {
        let key = try_parse_pkcs1_rsa(PKCS1_RSA_FIXTURE)
            .expect("PKCS#1 RSA 应解析成功")
            .expect("PKCS#1 标签应命中 Some 分支");
        // 解析出的私钥算法应为 RSA(ssh-rsa)。
        assert!(
            key.algorithm().as_str().contains("rsa"),
            "expected an RSA key, got algorithm {}",
            key.algorithm().as_str()
        );
    }

    #[test]
    fn try_parse_pkcs1_rsa_returns_none_for_non_pkcs1_tag() {
        // OpenSSH 格式标签 —— 本函数不处理,返回 None 让上层回退到 russh 原生错误。
        let openssh = "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n";
        assert!(matches!(try_parse_pkcs1_rsa(openssh), Ok(None)));
        // PKCS#8 标签同理。
        let pkcs8 = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n";
        assert!(matches!(try_parse_pkcs1_rsa(pkcs8), Ok(None)));
    }

    #[test]
    fn try_parse_pkcs1_rsa_rejects_encrypted_pkcs1_with_guidance() {
        let encrypted = "-----BEGIN RSA PRIVATE KEY-----\n\
            Proc-Type: 4,ENCRYPTED\n\
            DEK-Info: AES-128-CBC,0123456789ABCDEF0123456789ABCDEF\n\n\
            QUJDRA==\n\
            -----END RSA PRIVATE KEY-----\n";
        let err = try_parse_pkcs1_rsa(encrypted).expect_err("加密 PKCS#1 应被拒绝");
        assert!(
            err.contains("passphrase"),
            "错误信息应给 passphrase 指引,实际: {err}"
        );
    }

    #[test]
    fn try_parse_pkcs1_rsa_errors_on_corrupt_base64() {
        // 命中 PKCS#1 标签但主体不是合法 base64/DER —— 返回 Err,不会 panic。
        let bad = "-----BEGIN RSA PRIVATE KEY-----\n@@@not-base64@@@\n-----END RSA PRIVATE KEY-----\n";
        assert!(try_parse_pkcs1_rsa(bad).is_err());
    }
}
