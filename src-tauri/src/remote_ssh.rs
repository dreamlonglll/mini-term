//! SSH 远程项目的主程序后端能力(task 07-05-ssh-remote-projects PR2)。
//!
//! 通过共享 crate `mt-ssh` 的 russh 持久会话池 + SFTP 只读原语,为「远程项目」
//! 提供四个 Tauri command:
//! - `ssh_remote_list_directory`:远程文件树 readdir(根 .gitignore + ALWAYS_IGNORE 过滤)
//! - `ssh_remote_validate_dir`:「添加远程项目」保存前验证(`~` 展开 + stat 目录)
//! - `ssh_remote_ai_sessions`:扫描远程 `~/.claude/projects` / `~/.codex/sessions`
//! - `ssh_remote_ai_session_content`:SFTP 读会话正文(支持增量 offset)
//!
//! 契约(对齐 spec/backend/wsl-unc-session-scanning.md):
//! - 全部 command 是真 async fn,跑在 Tauri 的 tokio runtime 上,不阻塞主线程;
//! - 缓存锁即取即放,**绝不跨 SFTP 慢 IO 持锁**;
//! - 会话扫描一切失败静默降级为空列表(不弹错、不 panic);
//!   文件树 / 目录验证 / 正文读取失败返回明确 `Err(String)`。
//!
//! 池生命周期:懒初始化(首个远程 command 触发,必须在 tokio 上下文——async
//! command 天然满足);app 退出时 `lib.rs` 在 `RunEvent::Exit` 里调
//! [`RemoteSshState::shutdown_pool_blocking`] 优雅断开全部 session(对齐
//! mt-ssh-mcp sidecar 的 shutdown 钩子)。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use mt_core::SshConnection;
use mt_ssh::{CachedSession, SftpHandle, SshPool};
use serde::Serialize;
use tauri::AppHandle;

use crate::ai_sessions::{
    claude_message_from_line, claude_session_info_from_lines, codex_message_from_line,
    codex_meta_from_line, codex_user_title_from_line, encode_project_path, is_encoded_variant,
    normalize_unix_path, session_cache, AiSession, AiSessionMessage, CachedSessions,
    MAX_SESSIONS_PER_SOURCE, MAX_TOTAL_SESSIONS,
};
use crate::fs::{natural_cmp, FileEntry, ALWAYS_IGNORE};

/// SFTP 协议层每请求超时(readdir / stat / 单个 read 包)。
/// 默认仅 10s 且逐请求计时(见 spec/backend/russh-sftp-file-transfer.md 坑 1),
/// 这里放宽到 20s 覆盖慢链路;整体不设长窗口——只读操作单包粒度小。
const SFTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// 建立(或复用)SSH session 的外层超时:TCP 连接 + 握手 + 认证。
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);
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
/// 会话正文单次增量读取上限;更多内容由前端带 nextOffset 再次调用。
const CONTENT_CHUNK_MAX_BYTES: usize = 8 * 1024 * 1024;
/// 变体目录 cwd 精确校验:读任一 jsonl 头部的字节上限。
const CWD_PROBE_HEAD_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// managed state
// ---------------------------------------------------------------------------

/// 主程序的远程 SSH 状态(Tauri managed state)。
pub struct RemoteSshState {
    /// 懒初始化的 russh 会话池。session 按 `connection.id` 全局复用。
    pool: Mutex<Option<Arc<SshPool>>>,
    /// 远程项目根 `.gitignore` 编译结果缓存,key = `<connId>|<projectRoot 小写>`。
    gitignore_cache: Mutex<HashMap<String, Arc<Gitignore>>>,
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
            pool: Mutex::new(None),
            gitignore_cache: Mutex::new(HashMap::new()),
            home_cache: Mutex::new(HashMap::new()),
            session_paths: Mutex::new(HashMap::new()),
        }
    }

    /// 拿(或懒建)会话池。
    ///
    /// **前置**:必须在 tokio runtime 上下文中调用(`SshPool` 构造要 spawn 后台
    /// reaper task)——本模块只在 async command 内调用,天然满足。
    fn pool(&self) -> Arc<SshPool> {
        let mut guard = lock(&self.pool);
        guard
            .get_or_insert_with(|| Arc::new(SshPool::new()))
            .clone()
    }

    /// app 退出时优雅关池:abort reaper + 并发 disconnect 全部 session
    /// (单 session 2s 超时,不 hang 退出)。池未初始化则 no-op。
    /// 同步上下文调用(`RunEvent::Exit`),内部用 Tauri 的 tokio runtime block_on。
    pub fn shutdown_pool_blocking(&self) {
        let pool = lock(&self.pool).take();
        if let Some(pool) = pool {
            eprintln!("[remote-ssh] draining ssh session pool on exit");
            tauri::async_runtime::block_on(async move {
                pool.shutdown().await;
            });
        }
    }
}

impl Default for RemoteSshState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 连接查找 / session 编排
// ---------------------------------------------------------------------------

/// 按 id 从 config.json 找连接。找不到 = 「断链」(连接被删除),给明确错误。
fn find_connection(app: &AppHandle, connection_id: &str) -> Result<SshConnection, String> {
    crate::config::read_config(app)
        .ssh_connections
        .into_iter()
        .find(|c| c.id == connection_id)
        .ok_or_else(|| format!("SSH 连接不存在或已被删除 (id={connection_id})"))
}

/// 从池里拿一条可用 session(带外层超时 + gatetime cooldown 检查)。
async fn acquire_session(
    pool: &SshPool,
    conn: &SshConnection,
) -> Result<Arc<CachedSession>, String> {
    let session = tokio::time::timeout(ACQUIRE_TIMEOUT, pool.acquire(conn))
        .await
        .map_err(|_| {
            format!(
                "连接 {} 超时({}s)",
                conn.host,
                ACQUIRE_TIMEOUT.as_secs()
            )
        })??;
    if session.is_unhealthy_now() {
        return Err("SSH 会话处于冷却期(上次失败后短时间内不再重试),请稍后再试".into());
    }
    Ok(session)
}

/// 开一个 SFTP 会话句柄。transport 级失败(死链 race)evict + 重连再试一次,
/// 与 mt-ssh-mcp 的 exec/transfer 编排同构。
async fn open_sftp(state: &RemoteSshState, conn: &SshConnection) -> Result<SftpHandle, String> {
    let pool = state.pool();
    let session = acquire_session(&pool, conn).await?;
    match SftpHandle::open_on_session(&session, SFTP_REQUEST_TIMEOUT).await {
        Ok(h) => {
            session.touch();
            Ok(h)
        }
        Err(e) if e.is_transport() => {
            eprintln!("[remote-ssh] sftp open failed (transport), retrying once: {e}");
            pool.evict(&conn.id).await;
            let session2 = acquire_session(&pool, conn).await?;
            let h = SftpHandle::open_on_session(&session2, SFTP_REQUEST_TIMEOUT)
                .await
                .map_err(|e| e.message().to_string())?;
            session2.touch();
            Ok(h)
        }
        Err(e) => Err(e.message().to_string()),
    }
}

/// 远程 `$HOME`(SFTP canonicalize(".")),按连接缓存。锁即取即放。
async fn remote_home(
    state: &RemoteSshState,
    sftp: &SftpHandle,
    conn_id: &str,
) -> Result<String, String> {
    if let Some(h) = lock(&state.home_cache).get(conn_id).cloned() {
        return Ok(h);
    }
    let home = sftp
        .canonicalize(".")
        .await
        .map_err(|e| format!("获取远程 home 目录失败: {}", e.message()))?;
    lock(&state.home_cache).insert(conn_id.to_string(), home.clone());
    Ok(home)
}

// ---------------------------------------------------------------------------
// POSIX 路径纯函数(单测覆盖)
// ---------------------------------------------------------------------------

/// POSIX 路径拼接。`dir` 为绝对路径;根目录 `/` 不产生双斜杠。
fn join_posix(dir: &str, name: &str) -> String {
    let d = dir.trim_end_matches('/');
    if d.is_empty() {
        format!("/{name}")
    } else {
        format!("{d}/{name}")
    }
}

/// 计算 `full` 相对 `root` 的 POSIX 相对路径。不在 root 下返回 None。
/// **匹配 gitignore 必须用相对路径**:Windows 的 `Path` 语义对 POSIX 绝对路径
/// 有歧义(`/a/b` 在 Windows 上不是绝对路径),相对路径两平台行为一致。
fn posix_relative(root: &str, full: &str) -> Option<String> {
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

/// 用根 `.gitignore` 的文本内容构建匹配器。逐行喂 `add_line`(SFTP 读来的内容
/// 不落本地盘,不能用 `Gitignore::new(path)`);单行非法规则忽略,不影响其余行。
fn build_remote_gitignore(content: &str) -> Gitignore {
    let mut builder = GitignoreBuilder::new("");
    for line in content.lines() {
        // 单行解析失败(非法 glob)忽略该行,与 git 行为一致。
        let _ = builder.add_line(None, line);
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// 单个条目是否被根 .gitignore 忽略。`rel_path` 为相对项目根的 POSIX 路径。
/// 白名单(`!pattern`)由 Gitignore 内部按 gitignore 语义处理(后规则覆盖前规则)。
fn is_remote_entry_ignored(gitignore: &Gitignore, rel_path: &str, is_dir: bool) -> bool {
    if rel_path.is_empty() {
        return false;
    }
    gitignore.matched(rel_path, is_dir).is_ignore()
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
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) {
            if let (Some(id), Some(name)) = (
                obj.get("id").and_then(|v| v.as_str()),
                obj.get("thread_name").and_then(|v| v.as_str()),
            ) {
                map.insert(id.to_string(), name.to_string());
            }
        }
    }
    map
}

// ---------------------------------------------------------------------------
// command: 远程文件树
// ---------------------------------------------------------------------------

/// SFTP readdir 远程目录,返回与本地 `list_directory` 同构的 `FileEntry` 列表。
///
/// 忽略过滤 = 项目根 `.gitignore`(读一次、按 connId+projectRoot 缓存)
/// + `ALWAYS_IGNORE` 固定黑名单(目录直接隐藏)。
/// `refresh_ignore=true` 强制重读 .gitignore(树顶手动刷新按钮用)。
#[tauri::command]
pub async fn ssh_remote_list_directory(
    app: AppHandle,
    state: tauri::State<'_, RemoteSshState>,
    connection_id: String,
    path: String,
    project_root: String,
    refresh_ignore: Option<bool>,
) -> Result<Vec<FileEntry>, String> {
    let conn = find_connection(&app, &connection_id)?;
    let ignore_key = format!("{}|{}", connection_id, normalize_unix_path(&project_root));
    if refresh_ignore.unwrap_or(false) {
        lock(&state.gitignore_cache).remove(&ignore_key);
    }
    // 锁即取即放;miss 时在 SFTP 打开后无锁读取,再短暂加锁写回。
    let cached_ignore = lock(&state.gitignore_cache).get(&ignore_key).cloned();

    let sftp = open_sftp(&state, &conn).await?;
    let result = async {
        let gitignore = match cached_ignore {
            Some(g) => g,
            None => {
                let gi_path = join_posix(&project_root, ".gitignore");
                // .gitignore 不存在 / 读失败 → 空规则,静默降级。
                let content = match sftp.read_head(&gi_path, GITIGNORE_MAX_BYTES).await {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Err(_) => String::new(),
                };
                let g = Arc::new(build_remote_gitignore(&content));
                lock(&state.gitignore_cache).insert(ignore_key.clone(), g.clone());
                g
            }
        };

        let entries = sftp
            .read_dir(&path)
            .await
            .map_err(|e| format!("读取远程目录失败: {}", e.message()))?;

        let mut out: Vec<FileEntry> = entries
            .into_iter()
            .filter_map(|e| {
                // ALWAYS_IGNORE 目录完全隐藏(与本地树一致)
                if e.is_dir && ALWAYS_IGNORE.contains(&e.name.as_str()) {
                    return None;
                }
                let full = join_posix(&path, &e.name);
                let ignored = posix_relative(&project_root, &full)
                    .map(|rel| is_remote_entry_ignored(&gitignore, &rel, e.is_dir))
                    .unwrap_or(false);
                Some(FileEntry {
                    name: e.name,
                    path: full,
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
}

// ---------------------------------------------------------------------------
// command: 远程目录验证(「添加远程项目」保存前)
// ---------------------------------------------------------------------------

/// 验证远程路径是一个存在的目录,返回展开后的绝对路径。
/// `~` / `~/xxx` 用 SFTP canonicalize 展开;不存在或不是目录返回 Err。
#[tauri::command]
pub async fn ssh_remote_validate_dir(
    app: AppHandle,
    state: tauri::State<'_, RemoteSshState>,
    connection_id: String,
    path: String,
) -> Result<String, String> {
    let conn = find_connection(&app, &connection_id)?;
    let sftp = open_sftp(&state, &conn).await?;
    let result = async {
        let trimmed = path.trim();
        let expanded = if trimmed.is_empty() || trimmed == "~" || trimmed.starts_with("~/") {
            let home = remote_home(&state, &sftp, &connection_id).await?;
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
}

// ---------------------------------------------------------------------------
// command: 远程 AI 会话列表
// ---------------------------------------------------------------------------

/// 扫描远程机器上该项目的 claude/codex 历史会话。
/// - 会话带 `sshConnectionId` 来源标识(对齐 WSL 会话的 `wslDistro`);
/// - 结果缓存 10s(key 掺 connection id),`force=true` 绕过(手动刷新);
/// - 远程不可达 / 目录缺失等一切失败:静默降级返回空列表。
#[tauri::command]
pub async fn ssh_remote_ai_sessions(
    app: AppHandle,
    state: tauri::State<'_, RemoteSshState>,
    connection_id: String,
    project_path: String,
    force: Option<bool>,
) -> Result<Vec<AiSession>, String> {
    let cache_key = format!(
        "ssh|{}|{}",
        connection_id,
        normalize_unix_path(&project_path)
    );

    if !force.unwrap_or(false) {
        // 锁即取即放,扫描期间不持锁(SFTP IO 秒级)。
        let cached = lock(session_cache()).get(&cache_key).cloned();
        if let Some(c) = cached {
            if c.loaded_at.elapsed() < REMOTE_SESSION_CACHE_TTL {
                return Ok(c.sessions);
            }
        }
    }

    let sessions =
        match scan_remote_sessions(&app, &state, &connection_id, &project_path).await {
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

async fn scan_remote_sessions(
    app: &AppHandle,
    state: &RemoteSshState,
    connection_id: &str,
    project_path: &str,
) -> Result<Vec<AiSession>, String> {
    let conn = find_connection(app, connection_id)?;
    let sftp = open_sftp(state, &conn).await?;
    let result = async {
        let home = remote_home(state, &sftp, connection_id).await?;
        let mut sessions = Vec::new();
        sessions.extend(scan_remote_claude(state, &sftp, &home, connection_id, project_path).await);
        sessions.extend(scan_remote_codex(state, &sftp, &home, connection_id, project_path).await);
        sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        sessions.truncate(MAX_TOTAL_SESSIONS);
        Ok(sessions)
    }
    .await;
    sftp.close().await;
    result
}

/// 记录会话 id → 远程文件路径,正文读取时免再扫。
fn remember_session_path(state: &RemoteSshState, conn_id: &str, session_id: &str, path: &str) {
    lock(&state.session_paths).insert(format!("{conn_id}|{session_id}"), path.to_string());
}

/// 变体目录精确校验:读目录里任一 jsonl 头部的前几行,比对真实 cwd。
/// 与本地 `dir_matches_project` 语义一致(编码有损,防吃进兄弟项目)。
async fn remote_claude_dir_matches(
    sftp: &SftpHandle,
    dir: &str,
    normalized_project: &str,
) -> bool {
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
            if let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(cwd) = obj.get("cwd").and_then(|v| v.as_str()) {
                    return normalize_unix_path(cwd) == normalized_project;
                }
            }
        }
    }
    false
}

async fn scan_remote_claude(
    state: &RemoteSshState,
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
    files.sort_by(|a, b| b.2.cmp(&a.2));
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
        remember_session_path(state, conn_id, &id, &path);
        sessions.push(AiSession {
            id,
            session_type: "claude".to_string(),
            title,
            timestamp,
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
                file_entries.sort_by(|a, b| {
                    b.mtime_secs.unwrap_or(0).cmp(&a.mtime_secs.unwrap_or(0))
                });
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
    state: &RemoteSshState,
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

        remember_session_path(state, conn_id, &meta.id, &path);
        sessions.push(AiSession {
            id: meta.id,
            session_type: "codex".to_string(),
            title,
            timestamp,
            wsl_distro: None,
            ssh_connection_id: Some(conn_id.to_string()),
        });
    }
    sessions
}

// ---------------------------------------------------------------------------
// command: 远程会话正文(支持增量 offset)
// ---------------------------------------------------------------------------

/// 远程会话正文的增量读取结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSessionContent {
    /// 本次解析出的消息(与本地 `get_ai_session_content` 的元素同构)。
    pub messages: Vec<AiSessionMessage>,
    /// 下次增量读取应传入的字节偏移(指向已解析的最后一个完整行之后)。
    /// 首次调用传 offset=0(或省略)拿全量;之后传上次返回的 nextOffset 拿增量。
    pub next_offset: u64,
}

/// SFTP 读远程会话正文。`offset` 省略/0 = 从头读;返回 `nextOffset` 供增量刷新。
#[tauri::command]
pub async fn ssh_remote_ai_session_content(
    app: AppHandle,
    state: tauri::State<'_, RemoteSshState>,
    connection_id: String,
    session_type: String,
    session_id: String,
    project_path: String,
    offset: Option<u64>,
) -> Result<RemoteSessionContent, String> {
    let conn = find_connection(&app, &connection_id)?;
    let sftp = open_sftp(&state, &conn).await?;
    let result = async {
        let path = locate_remote_session_file(
            &state,
            &sftp,
            &connection_id,
            &session_type,
            &session_id,
            &project_path,
        )
        .await?;
        let start = offset.unwrap_or(0);
        let bytes = sftp
            .read_from_offset(&path, start, CONTENT_CHUNK_MAX_BYTES)
            .await
            .map_err(|e| format!("读取会话文件失败: {}", e.message()))?;
        let (consumed, complete) = split_complete_lines(&bytes);
        let text = String::from_utf8_lossy(complete);
        let messages: Vec<AiSessionMessage> = match session_type.as_str() {
            "claude" => text.lines().filter_map(claude_message_from_line).collect(),
            "codex" => text.lines().filter_map(codex_message_from_line).collect(),
            other => return Err(format!("不支持的会话类型: {other}")),
        };
        Ok(RemoteSessionContent {
            messages,
            next_offset: start + consumed as u64,
        })
    }
    .await;
    sftp.close().await;
    result
}

/// 定位会话对应的远程文件:优先取列表扫描时记下的映射;miss(如 app 重启)
/// 再按类型回退定位(claude 走编码目录推导,codex 按 rollout 文件名后缀匹配)。
async fn locate_remote_session_file(
    state: &RemoteSshState,
    sftp: &SftpHandle,
    conn_id: &str,
    session_type: &str,
    session_id: &str,
    project_path: &str,
) -> Result<String, String> {
    let key = format!("{conn_id}|{session_id}");
    // 先绑定再 await:if-let 直接嵌 lock() 会让临时 MutexGuard 活过 await 点,
    // 破坏 command future 的 Send 约束。
    let cached_path = lock(&state.session_paths).get(&key).cloned();
    if let Some(p) = cached_path {
        if sftp.exists(&p).await {
            return Ok(p);
        }
    }

    let home = remote_home(state, sftp, conn_id).await?;
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
                        remember_session_path(state, conn_id, session_id, &p);
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
                    remember_session_path(state, conn_id, session_id, &path);
                    return Ok(path);
                }
            }
            Err("未找到 Codex 会话文件,请刷新会话列表后重试".into())
        }
        other => Err(format!("不支持的会话类型: {other}")),
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(posix_relative("/home/u/proj", "/home/u/proj").as_deref(), Some(""));
        // 尾部斜杠不影响
        assert_eq!(
            posix_relative("/home/u/proj/", "/home/u/proj/a").as_deref(),
            Some("a")
        );
        // 根目录项目
        assert_eq!(posix_relative("/", "/etc/hosts").as_deref(), Some("etc/hosts"));
    }

    #[test]
    fn posix_relative_rejects_sibling_prefix() {
        // `/home/u/proj2` 不在 `/home/u/proj` 之下,不能误判
        assert!(posix_relative("/home/u/proj", "/home/u/proj2/file").is_none());
        assert!(posix_relative("/home/u/proj", "/other/place").is_none());
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

    // --- 根 .gitignore 相对路径匹配 ---

    #[test]
    fn remote_gitignore_matches_relative_paths() {
        let gi = build_remote_gitignore("node_modules/\n*.log\nbuild/\n");
        assert!(is_remote_entry_ignored(&gi, "node_modules", true));
        assert!(is_remote_entry_ignored(&gi, "app.log", false));
        assert!(is_remote_entry_ignored(&gi, "src/deep/trace.log", false));
        assert!(is_remote_entry_ignored(&gi, "src/build", true));
        assert!(!is_remote_entry_ignored(&gi, "src/main.rs", false));
        // 目录规则(尾 `/`)不忽略同名文件
        assert!(!is_remote_entry_ignored(&gi, "build", false));
    }

    #[test]
    fn remote_gitignore_supports_whitelist_override() {
        let gi = build_remote_gitignore("*.log\n!keep.log\n");
        assert!(is_remote_entry_ignored(&gi, "a.log", false));
        assert!(!is_remote_entry_ignored(&gi, "keep.log", false));
    }

    #[test]
    fn remote_gitignore_empty_and_invalid_lines_are_safe() {
        let gi = build_remote_gitignore("");
        assert!(!is_remote_entry_ignored(&gi, "anything", false));
        // 空相对路径(项目根本身)永不忽略
        let gi2 = build_remote_gitignore("*\n");
        assert!(!is_remote_entry_ignored(&gi2, "", true));
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
        assert!(!codex_filename_matches_session("/x/rollout-abc-123.txt", "abc-123"));
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
        let state = RemoteSshState::new();
        remember_session_path(&state, "c1", "s1", "/p/a.jsonl");
        remember_session_path(&state, "c2", "s1", "/p/b.jsonl");
        assert_eq!(
            lock(&state.session_paths).get("c1|s1").map(String::as_str),
            Some("/p/a.jsonl")
        );
        assert_eq!(
            lock(&state.session_paths).get("c2|s1").map(String::as_str),
            Some("/p/b.jsonl")
        );
    }
}
