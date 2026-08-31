use std::collections::{HashMap, HashSet};
use std::time::Instant;

use mt_ai::sessions::{
    AiSession, AiSessionMessage, CachedSessions, MAX_SESSIONS_PER_SOURCE, MAX_TOTAL_SESSIONS,
    claude_message_from_line, claude_session_info_from_lines, codex_message_from_line,
    codex_meta_from_line, codex_user_title_from_line, encode_project_path, is_encoded_variant,
    normalize_unix_path, session_cache, session_id_path_safe,
};
use mt_config::SshConnection;
use mt_ssh::SftpHandle;

use super::{
    CLAUDE_TITLE_HEAD_BYTES, CODEX_META_HEAD_BYTES, CONTENT_CHUNK_MAX_BYTES, CWD_PROBE_HEAD_BYTES,
    REMOTE_CLAUDE_SCAN_LIMIT, REMOTE_CODEX_SCAN_LIMIT, REMOTE_SESSION_CACHE_TTL, RemoteSshState,
    SESSION_INDEX_MAX_BYTES, join_posix, lock, open_sftp, remote_home, state,
};

/// UNIX 秒 → ISO 8601 UTC 字符串(`YYYY-MM-DDTHH:MM:SSZ`)。
/// 会话缺失 timestamp 字段时用文件 mtime 兜底,保证时间混排仍可比较。
pub(super) fn unix_secs_to_iso(secs: u64) -> String {
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
pub(super) fn split_complete_lines(bytes: &[u8]) -> (usize, &[u8]) {
    match bytes.iter().rposition(|&b| b == b'\n') {
        Some(i) => (i + 1, &bytes[..i + 1]),
        None => (0, &[]),
    }
}

/// codex rollout 文件名是否以该 session id 结尾(`rollout-<ts>-<id>.jsonl`)。
pub(super) fn codex_filename_matches_session(path: &str, session_id: &str) -> bool {
    if session_id.is_empty() {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".jsonl")
        .map(|stem| stem.ends_with(session_id))
        .unwrap_or(false)
}

/// 解析 codex session_index.jsonl 内容 → { id: thread_name }。
pub(super) fn parse_codex_thread_names(content: &str) -> HashMap<String, String> {
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
pub(super) fn remember_session_path(
    st: &RemoteSshState,
    conn_id: &str,
    session_id: &str,
    path: &str,
) {
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
pub(super) const CONTENT_TOTAL_MAX_BYTES: u64 = 64 * 1024 * 1024;

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
pub(super) fn accumulate_session_content<F>(mut fetch: F) -> Result<Vec<AiSessionMessage>, String>
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
