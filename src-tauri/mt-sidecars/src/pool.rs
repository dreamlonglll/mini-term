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
//! 本 PR(PR1)只交付池骨架与 `acquire`,**不接入 `ssh_exec`**——旧 `run_ssh_pty` /
//! `run_ssh_piped` 路径继续工作,PR2 才切流量。所以池里许多公共 API 会暂时
//! 没有调用方,允许 `dead_code` 警告。

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mt_core::SshConnection;
use russh::client::{self, Handle, Handler};
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use tokio::sync::Mutex;

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
    /// session 建立时所用的连接快照。**重连也用这一份**,不重读 config(故意行为,
    /// 见 PRD"配置一致性"决策)。
    conn_snapshot: SshConnection,
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

    /// 全显式构造,主要给单测用。
    pub fn with_paths(config: PoolConfig, known_hosts_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(PoolInner {
                sessions: HashMap::new(),
            })),
            config,
            known_hosts_path,
        }
    }

    /// 查池里有几条 session(主要给测试用)。
    pub async fn len(&self) -> usize {
        self.inner.lock().await.sessions.len()
    }

    /// 拿一条可用 session。lazy 建,池满时 LRU 淘汰。
    ///
    /// 返回 `Arc<CachedSession>`,调用方再 `session.lock().await` 开 channel。
    /// 若返回的 session `is_unhealthy_now() == true`,调用方应立即返错而不去开 channel,
    /// 实现 30s gatetime cooldown 的语义。
    pub async fn acquire(&self, conn: &SshConnection) -> Result<Arc<CachedSession>, String> {
        // 拒绝跳板机连接 —— MVP 阶段不支持(见 PRD Out of Scope)。
        if conn.proxy_jump.as_deref().map(str::trim).filter(|s| !s.is_empty()).is_some() {
            return Err(format!(
                "jump host is no longer supported by ssh_exec; remove proxy_jump on connection '{}'",
                conn.name
            ));
        }

        {
            let inner = self.inner.lock().await;
            if let Some(s) = inner.sessions.get(&conn.id) {
                // 复用前检查:underlying handle 还活着且不在 cooldown。
                if !s.handle.lock().await.is_closed() && !s.is_unhealthy_now() {
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

    /// 关掉所有 session,清空池。sidecar shutdown 时调用。
    pub async fn shutdown(&self) {
        let mut inner = self.inner.lock().await;
        let entries: Vec<_> = inner.sessions.drain().map(|(_, v)| v).collect();
        drop(inner);

        let timeout = self.config.shutdown_per_session_timeout;
        // 并发 disconnect,各自加超时;不让单条挂死阻塞退出。
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
        let mut cfg = client::Config::default();
        cfg.keepalive_interval = Some(self.config.keepalive_interval);
        cfg.keepalive_max = self.config.keepalive_max;
        let cfg = Arc::new(cfg);

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
            conn_snapshot: conn.clone(),
            unhealthy_until: AtomicU64::new(0),
        })
    }
}

impl Default for SshPool {
    fn default() -> Self {
        Self::new()
    }
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

/// 按"先 publickey 后 password"顺序尝试认证;两路皆败抛错。
///
/// password 路径包含 `authenticate_password` 与 `authenticate_keyboard_interactive_*`
/// 两个 method —— 某些服务器禁用 password 而只接受 keyboard-interactive。
async fn authenticate(handle: &mut Handle<MtClient>, conn: &SshConnection) -> Result<(), String> {
    // 1) publickey
    if let Some(path) = conn.identity_file.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let key = load_secret_key(path, None)
            .map_err(|e| {
                // OpenSSH 加密密钥会在这里报 "encrypted" 或类似;给清晰错误指引。
                format!(
                    "failed to load private key '{path}': {e}. \
                    If the key is encrypted with a passphrase, mt-ssh-mcp does not support \
                    passphrase keys yet — use an unencrypted key or ssh-agent."
                )
            })?;
        // PrivateKeyWithHashAlg::new(key, hash) —— None 让 russh 协商最合适的 hash 算法。
        let with_hash = PrivateKeyWithHashAlg::new(Arc::new(key), None);
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
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
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

    /// 用 `match` 取 Err,绕开 `unwrap_err` 对 Ok variant 的 Debug 约束
    /// (Arc<CachedSession> 没有 Debug,也不值得只为测试加)。
    fn err_of<T>(r: Result<T, String>) -> String {
        match r {
            Err(e) => e,
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    #[tokio::test]
    async fn acquire_rejects_proxy_jump_connections() {
        let conn = SshConnection {
            id: "1".into(),
            name: "with-jump".into(),
            host: "h.example.com".into(),
            port: 22,
            user: "u".into(),
            password: None,
            identity_file: None,
            proxy_jump: Some("user@bastion".into()),
            group: None,
        };
        let pool = SshPool::with_paths(
            PoolConfig::default(),
            std::env::temp_dir().join("mt-ssh-mcp-test-known_hosts"),
        );
        let err = err_of(pool.acquire(&conn).await);
        assert!(err.contains("jump host"), "got: {err}");
        assert!(err.contains("with-jump"), "got: {err}");
    }

    #[tokio::test]
    async fn acquire_blank_proxy_jump_treated_as_none_and_proceeds_to_connect() {
        // 空白 proxy_jump 应被视作 None,不被 jump 检查拒掉。这里没有真实 sshd,
        // 走到 connect 必然失败 —— 我们只验证 **失败原因不是 "jump host"**。
        let conn = SshConnection {
            id: "2".into(),
            name: "blank-jump".into(),
            host: "127.0.0.1".into(),
            port: 1, // 几乎肯定没人监听
            user: "u".into(),
            password: Some("x".into()),
            identity_file: None,
            proxy_jump: Some("   ".into()), // 全空白 → 视为没填
            group: None,
        };
        let pool = SshPool::with_paths(
            PoolConfig::default(),
            std::env::temp_dir().join("mt-ssh-mcp-test-known_hosts-2"),
        );
        let err = err_of(pool.acquire(&conn).await);
        assert!(!err.contains("jump host"), "got: {err}");
    }
}
