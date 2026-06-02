use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_CLAUDE_SESSION_FILES_TO_SCAN: usize = 300;
const MAX_CODEX_SESSION_FILES_TO_SCAN: usize = 500;
const MAX_SESSIONS_PER_SOURCE: usize = 80;
const MAX_TOTAL_SESSIONS: usize = 120;
const SESSION_CACHE_TTL: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct CachedSessions {
    loaded_at: Instant,
    sessions: Vec<AiSession>,
}

static SESSION_CACHE: OnceLock<Mutex<HashMap<String, CachedSessions>>> = OnceLock::new();

fn session_cache() -> &'static Mutex<HashMap<String, CachedSessions>> {
    SESSION_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSession {
    pub id: String,
    pub session_type: String, // "claude" | "codex"
    pub title: String,
    pub timestamp: String, // ISO 8601
}

/// 获取用户 home 目录
fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// 将项目路径编码为 Claude 项目目录名。
/// Claude Code 会把 cwd 中**所有非字母数字字符**（含 `:` `\` `/` `.` 空格及中文等）
/// 统一替换为 `-`，而非仅替换路径分隔符。
/// 例如 `D:\Git\bhyt-一体机` → `D--Git-bhyt----`。
fn encode_project_path(project_path: &str) -> String {
    project_path
        .trim_end_matches(['/', '\\'])
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// 查找项目路径对应的所有 Claude 项目目录（含尾部斜杠导致的变体）
fn find_claude_project_dirs(project_path: &str) -> Vec<PathBuf> {
    let home = match home_dir() {
        Some(h) => h,
        None => return vec![],
    };
    let projects_dir = home.join(".claude").join("projects");
    if !projects_dir.exists() {
        return vec![];
    }

    let encoded = encode_project_path(project_path);
    let normalized_project = normalize_path(project_path);

    let entries = match fs::read_dir(&projects_dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if dir_name == encoded {
            // 名称完全一致：直接采用
            dirs.push(path);
        } else if dir_name.starts_with(&encoded)
            && dir_name[encoded.len()..].chars().all(|c| c == '-')
        {
            // 仅多出尾部 `-`：可能是「带尾部斜杠的同一项目」，也可能是「前缀相同的不同项目」。
            // 编码有损（如 `D:\Git\bhyt` 会前缀匹配到 `D:\Git\bhyt-一体机` 的目录 `D--Git-bhyt----`），
            // 因此读取会话文件内的真实 cwd 做精确校验，避免把兄弟项目的会话也吃进来。
            if dir_matches_project(&path, &normalized_project) {
                dirs.push(path);
            }
        }
    }

    dirs
}

/// 读取 Claude 项目目录下任一 jsonl 的 `cwd` 字段，确认其是否就是目标项目。
/// 用于消除目录名编码有损导致的前缀误匹配。
fn dir_matches_project(dir: &Path, normalized_project: &str) -> bool {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let reader = BufReader::new(file);
        for line in reader.lines().take(5) {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(cwd) = obj.get("cwd").and_then(|v| v.as_str()) {
                    return normalize_path(cwd) == normalized_project;
                }
            }
        }
    }
    false
}

/// 路径统一化（小写 + 反斜杠，去尾部斜杠），用于 Windows 路径比较
fn normalize_path(path: &str) -> String {
    path.replace('/', "\\")
        .to_lowercase()
        .trim_end_matches('\\')
        .to_string()
}

// ─── Claude Sessions ───────────────────────────────────────────

fn get_claude_sessions(project_path: &str) -> Vec<AiSession> {
    let project_dirs = find_claude_project_dirs(project_path);
    if project_dirs.is_empty() {
        return vec![];
    }

    let mut paths: Vec<PathBuf> = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    for dir in &project_dirs {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                let id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                if seen_ids.insert(id) {
                    paths.push(path);
                }
            }
        }
    }

    sort_newest_session_paths(&mut paths, MAX_CLAUDE_SESSION_FILES_TO_SCAN);

    let mut sessions = Vec::new();
    for path in paths {
        if sessions.len() >= MAX_SESSIONS_PER_SOURCE {
            break;
        }

        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let (title, timestamp) = read_claude_session_info(&path);

        sessions.push(AiSession {
            id,
            session_type: "claude".to_string(),
            title,
            timestamp,
        });
    }

    sessions
}

/// 读取 Claude JSONL，提取第一条 user message 的内容和时间戳
fn read_claude_session_info(path: &Path) -> (String, String) {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return ("Untitled".into(), String::new()),
    };

    let reader = BufReader::new(file);

    for line in reader.lines().take(50) {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        let obj: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if obj.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }

        let content_val = obj.pointer("/message/content");

        let content = if let Some(s) = content_val.and_then(|c| c.as_str()) {
            s.to_string()
        } else if let Some(arr) = content_val.and_then(|c| c.as_array()) {
            // 多模态消息：取第一个 text block
            arr.iter()
                .filter_map(|item| {
                    if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                        item.get("text").and_then(|t| t.as_str()).map(String::from)
                    } else {
                        None
                    }
                })
                .next()
                .unwrap_or_else(|| "Untitled".into())
        } else {
            "Untitled".into()
        };

        // 跳过系统注入消息（如 /clear 等本地命令产生的 <local-command-caveat> 等）
        let trimmed = content.trim_start();
        if trimmed.starts_with('<') {
            continue;
        }

        // 截断到 100 字符
        let title: String = content.chars().take(100).collect();

        let timestamp = obj
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        return (title, timestamp);
    }

    ("Untitled".into(), String::new())
}

// ─── Codex Sessions ────────────────────────────────────────────

fn get_codex_sessions(project_path: &str) -> Vec<AiSession> {
    let home = match home_dir() {
        Some(h) => h,
        None => return vec![],
    };

    let codex_dir = home.join(".codex");
    let sessions_dir = codex_dir.join("sessions");

    if !sessions_dir.exists() {
        return vec![];
    }

    // 加载 session_index.jsonl 中的 thread_name 映射
    let thread_names = load_codex_thread_names(&codex_dir);

    let mut sessions = Vec::new();
    let normalized_project = normalize_path(project_path);

    let mut session_paths = Vec::new();
    collect_codex_session_paths(&sessions_dir, &mut session_paths);
    sort_newest_session_paths(&mut session_paths, MAX_CODEX_SESSION_FILES_TO_SCAN);

    for path in session_paths {
        if sessions.len() >= MAX_SESSIONS_PER_SOURCE {
            break;
        }
        if let Some(session) = try_read_codex_session(&path, &normalized_project, &thread_names) {
            sessions.push(session);
        }
    }

    sessions
}

/// 加载 Codex session_index.jsonl → { id: thread_name }
fn load_codex_thread_names(codex_dir: &Path) -> HashMap<String, String> {
    let index_path = codex_dir.join("session_index.jsonl");
    let mut map = HashMap::new();

    let file = match fs::File::open(&index_path) {
        Ok(f) => f,
        Err(_) => return map,
    };

    let reader = BufReader::new(file);
    for line in reader.lines().flatten() {
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) {
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

fn sort_newest_session_paths(paths: &mut Vec<PathBuf>, limit: usize) {
    paths.sort_by(|a, b| {
        let mt = |p: &PathBuf| p.metadata().and_then(|m| m.modified()).ok();
        match (mt(a), mt(b)) {
            (Some(ta), Some(tb)) => tb.cmp(&ta),
            _ => b.cmp(a),
        }
    });
    if paths.len() > limit {
        paths.truncate(limit);
    }
}

/// 递归遍历 sessions/<year>/<month>/<day>/ 目录，仅收集文件路径。
/// 真正读取 JSONL 前先按路径日期排序和限量，避免历史记录增长后每次刷新都读全量内容。
fn collect_codex_session_paths(dir: &Path, paths: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_codex_session_paths(&path, paths);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            paths.push(path);
        }
    }
}

/// 读取 Codex session 文件，匹配 cwd 后返回 AiSession
fn try_read_codex_session(
    path: &Path,
    normalized_project: &str,
    thread_names: &HashMap<String, String>,
) -> Option<AiSession> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut matched_id = None;
    let mut matched_timestamp = String::new();

    let mut lines_iter = reader.lines();

    // 第一遍：前 5 行找 session_meta，匹配 cwd
    for line in (&mut lines_iter).take(5) {
        let line = line.ok()?;
        let obj: serde_json::Value = serde_json::from_str(&line).ok()?;

        if obj.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
            continue;
        }

        let cwd = obj
            .pointer("/payload/cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if normalize_path(cwd) != normalized_project {
            return None;
        }

        matched_id = Some(
            obj.pointer("/payload/id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );

        matched_timestamp = obj
            .pointer("/payload/timestamp")
            .or_else(|| obj.get("timestamp"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        break;
    }

    let id = matched_id?;

    // 先查 session_index 中的 thread_name
    let mut title = thread_names.get(&id).cloned().unwrap_or_default();

    // 如果 thread_name 为空，从后续行中找第一条真实用户消息
    if title.is_empty() {
        for line in lines_iter.take(30) {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            let obj: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if obj.get("type").and_then(|t| t.as_str()) != Some("response_item") {
                continue;
            }
            if obj.pointer("/payload/role").and_then(|v| v.as_str()) != Some("user") {
                continue;
            }

            // 遍历 content blocks，找第一个非系统注入的 text
            if let Some(arr) = obj.pointer("/payload/content").and_then(|v| v.as_array()) {
                for item in arr {
                    if item.get("type").and_then(|t| t.as_str()) != Some("input_text") {
                        continue;
                    }
                    let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    let trimmed = text.trim_start();
                    if !trimmed.is_empty()
                        && !trimmed.starts_with('<')
                        && !trimmed.starts_with("# AGENTS.md")
                    {
                        title = trimmed.chars().take(100).collect();
                        break;
                    }
                }
            }
            if !title.is_empty() {
                break;
            }
        }

        if title.is_empty() {
            title = "Untitled".into();
        }
    }

    let timestamp = matched_timestamp;

    Some(AiSession {
        id,
        session_type: "codex".to_string(),
        title,
        timestamp,
    })
}

// ─── Session Content ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSessionMessage {
    pub role: String,
    pub content: String,
    pub timestamp: String,
}

fn extract_text_content(content_val: Option<&serde_json::Value>) -> String {
    match content_val {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let texts: Vec<String> = arr
                .iter()
                .filter_map(|item| {
                    let t = item.get("type").and_then(|t| t.as_str())?;
                    match t {
                        "text" | "output_text" | "input_text" => {
                            item.get("text").and_then(|t| t.as_str()).map(String::from)
                        }
                        _ => None,
                    }
                })
                .collect();
            texts.join("\n\n")
        }
        _ => String::new(),
    }
}

fn read_claude_session_content(
    session_id: &str,
    project_path: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    let project_dirs = find_claude_project_dirs(project_path);
    let filename = format!("{}.jsonl", session_id);

    let path = project_dirs
        .iter()
        .map(|dir| dir.join(&filename))
        .find(|p| p.exists())
        .ok_or_else(|| "会话文件不存在".to_string())?;

    let file = fs::File::open(&path).map_err(|e| format!("无法打开文件: {}", e))?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        let obj: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let role = match obj.get("type").and_then(|t| t.as_str()) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };

        let content = extract_text_content(obj.pointer("/message/content"));
        if content.is_empty() {
            continue;
        }

        let timestamp = obj
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        messages.push(AiSessionMessage {
            role: role.to_string(),
            content,
            timestamp,
        });
    }

    Ok(messages)
}

fn is_codex_session_match(path: &Path, session_id: &str) -> bool {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let reader = BufReader::new(file);
    for line in reader.lines().take(5) {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let obj: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if obj.get("type").and_then(|t| t.as_str()) == Some("session_meta") {
            if let Some(id) = obj.pointer("/payload/id").and_then(|v| v.as_str()) {
                return id == session_id;
            }
        }
    }
    false
}

fn read_codex_session_content(
    session_id: &str,
    _project_path: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    let home = home_dir().ok_or_else(|| "无法获取 home 目录".to_string())?;
    let sessions_dir = home.join(".codex").join("sessions");

    if !sessions_dir.exists() {
        return Err("Codex sessions 目录不存在".to_string());
    }

    let mut paths = Vec::new();
    collect_codex_session_paths(&sessions_dir, &mut paths);

    let session_file = paths
        .iter()
        .find(|p| is_codex_session_match(p, session_id))
        .ok_or_else(|| "未找到 Codex 会话文件".to_string())?
        .clone();

    let file = fs::File::open(&session_file).map_err(|e| format!("无法打开文件: {}", e))?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        let obj: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if obj.get("type").and_then(|t| t.as_str()) != Some("response_item") {
            continue;
        }

        let role = match obj.pointer("/payload/role").and_then(|v| v.as_str()) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };

        let content = extract_text_content(obj.pointer("/payload/content"));
        if content.is_empty() {
            continue;
        }

        let timestamp = obj
            .pointer("/payload/timestamp")
            .or_else(|| obj.get("timestamp"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        messages.push(AiSessionMessage {
            role: role.to_string(),
            content,
            timestamp,
        });
    }

    Ok(messages)
}

#[tauri::command]
pub fn get_ai_session_content(
    session_type: String,
    session_id: String,
    project_path: String,
) -> Result<Vec<AiSessionMessage>, String> {
    match session_type.as_str() {
        "claude" => read_claude_session_content(&session_id, &project_path),
        "codex" => read_codex_session_content(&session_id, &project_path),
        _ => Err(format!("不支持的会话类型: {}", session_type)),
    }
}

// ─── Tauri Command ─────────────────────────────────────────────

#[tauri::command]
pub fn get_ai_sessions(project_path: String) -> Result<Vec<AiSession>, String> {
    let cache_key = normalize_path(&project_path);
    let mut cache = session_cache()
        .lock()
        .map_err(|_| "session cache lock poisoned".to_string())?;

    if let Some(cached) = cache.get(&cache_key) {
        if cached.loaded_at.elapsed() < SESSION_CACHE_TTL {
            return Ok(cached.sessions.clone());
        }
    }

    let mut sessions = Vec::new();

    sessions.extend(get_claude_sessions(&project_path));
    sessions.extend(get_codex_sessions(&project_path));

    // 按时间戳降序（最新在前）
    sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    if sessions.len() > MAX_TOTAL_SESSIONS {
        sessions.truncate(MAX_TOTAL_SESSIONS);
    }

    cache.insert(
        cache_key,
        CachedSessions {
            loaded_at: Instant::now(),
            sessions: sessions.clone(),
        },
    );

    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_newest_session_paths_keeps_recent_files_first() {
        let mut paths = vec![
            PathBuf::from(
                r"C:\Users\test\.codex\sessions\2025\10\28\rollout-2025-10-28T10-47-08-old.jsonl",
            ),
            PathBuf::from(
                r"C:\Users\test\.codex\sessions\2026\04\24\rollout-2026-04-24T19-00-00-newest.jsonl",
            ),
            PathBuf::from(
                r"C:\Users\test\.codex\sessions\2026\01\02\rollout-2026-01-02T09-00-00-middle.jsonl",
            ),
        ];

        sort_newest_session_paths(&mut paths, 2);

        assert_eq!(paths.len(), 2);
        assert!(paths[0].to_string_lossy().contains("newest"));
        assert!(paths[1].to_string_lossy().contains("middle"));
    }
}
