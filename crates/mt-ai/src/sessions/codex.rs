// ─── Codex Sessions ────────────────────────────────────────────

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use super::lineage::latest_model_from_file_tail;
use super::{
    AiSession, AiSessionMessage, MAX_CODEX_SESSION_FILES_TO_SCAN, MAX_SESSIONS_PER_SOURCE,
    PathStyle, extract_text_content, home_dir, sort_newest_session_paths, wsl_candidate_homes,
};

/// 扫描指定 home 下的 Codex 会话。`wsl_distro` 为来源标识,一并写进结果。
pub(super) fn get_codex_sessions_in(
    home: &Path,
    project_path: &str,
    style: PathStyle,
    max_files: usize,
    wsl_distro: Option<&str>,
) -> Vec<AiSession> {
    let codex_dir = home.join(".codex");
    let sessions_dir = codex_dir.join("sessions");

    if !sessions_dir.exists() {
        return vec![];
    }

    // 加载 session_index.jsonl 中的 thread_name 映射
    let thread_names = load_codex_thread_names(&codex_dir);

    let mut sessions = Vec::new();
    let normalized_project = style.normalize(project_path);

    let mut session_paths = Vec::new();
    collect_codex_session_paths(&sessions_dir, &mut session_paths);
    sort_newest_session_paths(&mut session_paths, max_files);

    for path in session_paths {
        if sessions.len() >= MAX_SESSIONS_PER_SOURCE {
            break;
        }
        if let Some(session) =
            try_read_codex_session(&path, &normalized_project, style, &thread_names, wsl_distro)
        {
            sessions.push(session);
        }
    }

    sessions
}

/// Windows 宿主视角的 Codex 会话扫描
pub(super) fn get_codex_sessions(project_path: &str) -> Vec<AiSession> {
    let home = match home_dir() {
        Some(h) => h,
        None => return vec![],
    };
    get_codex_sessions_in(
        &home,
        project_path,
        PathStyle::Windows,
        MAX_CODEX_SESSION_FILES_TO_SCAN,
        None,
    )
}

/// 加载 Codex session_index.jsonl → { id: thread_name }
/// pub：使用统计(usage_stats → 将来的 mt-usage)全局扫描时复用同一标题映射。
pub fn load_codex_thread_names(codex_dir: &Path) -> HashMap<String, String> {
    let index_path = codex_dir.join("session_index.jsonl");
    let mut map = HashMap::new();

    let file = match fs::File::open(&index_path) {
        Ok(f) => f,
        Err(_) => return map,
    };

    let reader = BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
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

/// 递归遍历 sessions/<year>/<month>/<day>/ 目录,仅收集文件路径。
/// 真正读取 JSONL 前先按路径日期排序和限量,避免历史记录增长后每次刷新都读全量内容。
pub fn collect_codex_session_paths(dir: &Path, paths: &mut Vec<PathBuf>) {
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

/// Codex 会话文件头部 session_meta 行的关键字段。
pub struct CodexSessionMeta {
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
}

/// 解析一行,若是 session_meta 则取出 id/timestamp/cwd。行级纯函数,
/// SSH 远程扫描(remote_ssh.rs)用它对远程 rollout 文件做 cwd 匹配。
pub fn codex_meta_from_line(line: &str) -> Option<CodexSessionMeta> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;
    if obj.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    Some(CodexSessionMeta {
        id: obj
            .pointer("/payload/id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        timestamp: obj
            .pointer("/payload/timestamp")
            .or_else(|| obj.get("timestamp"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        cwd: obj
            .pointer("/payload/cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// 从一行 response_item 里提取第一条真实用户输入作为标题候选
/// (跳过 `<...>` 系统注入与 `# AGENTS.md` 前缀)。行级纯函数,本地与远程共用。
pub fn codex_user_title_from_line(line: &str) -> Option<String> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;
    if obj.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return None;
    }
    if obj.pointer("/payload/role").and_then(|v| v.as_str()) != Some("user") {
        return None;
    }
    let arr = obj.pointer("/payload/content").and_then(|v| v.as_array())?;
    for item in arr {
        if item.get("type").and_then(|t| t.as_str()) != Some("input_text") {
            continue;
        }
        let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
        let trimmed = text.trim_start();
        if !trimmed.is_empty() && !trimmed.starts_with('<') && !trimmed.starts_with("# AGENTS.md") {
            return Some(trimmed.chars().take(100).collect());
        }
    }
    None
}

/// 读取 Codex session 文件,匹配 cwd 后返回 AiSession
fn try_read_codex_session(
    path: &Path,
    normalized_project: &str,
    style: PathStyle,
    thread_names: &HashMap<String, String>,
    wsl_distro: Option<&str>,
) -> Option<AiSession> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut matched_id = None;
    let mut matched_timestamp = String::new();

    let mut lines_iter = reader.lines();

    // 第一遍:前 5 行找 session_meta,匹配 cwd
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

        if style.normalize(cwd) != normalized_project {
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

    // 如果 thread_name 为空,从后续行中找第一条真实用户消息
    if title.is_empty() {
        for line in lines_iter.take(30) {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            if let Some(t) = codex_user_title_from_line(&line) {
                title = t;
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
        model: latest_model_from_file_tail(path),
        wsl_distro: wsl_distro.map(String::from),
        ssh_connection_id: None,
    })
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

/// 在 sessions 目录下按 session_meta.payload.id 定位会话文件
pub(super) fn find_codex_session_file(sessions_dir: &Path, session_id: &str) -> Option<PathBuf> {
    if !sessions_dir.exists() {
        return None;
    }
    let mut paths = Vec::new();
    collect_codex_session_paths(sessions_dir, &mut paths);
    paths
        .into_iter()
        .find(|p| is_codex_session_match(p, session_id))
}

/// 解析 Codex JSONL 的一行为消息。非 response_item / 非 user/assistant / 空内容 → None。
/// 行级纯函数,本地与远程(SFTP)两条正文读取路径共用。
pub fn codex_message_from_line(line: &str) -> Option<AiSessionMessage> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;

    if obj.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return None;
    }

    let role = match obj.pointer("/payload/role").and_then(|v| v.as_str()) {
        Some("user") => "user",
        Some("assistant") => "assistant",
        _ => return None,
    };

    let content = extract_text_content(obj.pointer("/payload/content"));
    if content.is_empty() {
        return None;
    }

    let timestamp = obj
        .pointer("/payload/timestamp")
        .or_else(|| obj.get("timestamp"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();

    Some(AiSessionMessage {
        role: role.to_string(),
        content,
        timestamp,
    })
}

/// 从单个 Codex JSONL 会话文件读取全部消息
fn read_codex_messages_from_file(path: &Path) -> Result<Vec<AiSessionMessage>, String> {
    let file = fs::File::open(path).map_err(|e| format!("无法打开文件: {}", e))?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    // 显式循环而非 map_while(Result::ok):坏行只跳过,不截断后续消息(同 claude 侧)。
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if let Some(m) = codex_message_from_line(&line) {
            messages.push(m);
        }
    }
    Ok(messages)
}

pub(super) fn read_codex_session_content(
    session_id: &str,
    _project_path: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    let home = home_dir().ok_or_else(|| "无法获取 home 目录".to_string())?;
    let sessions_dir = home.join(".codex").join("sessions");

    if !sessions_dir.exists() {
        return Err("Codex sessions 目录不存在".to_string());
    }

    let session_file = find_codex_session_file(&sessions_dir, session_id)
        .ok_or_else(|| "未找到 Codex 会话文件".to_string())?;

    read_codex_messages_from_file(&session_file)
}

/// 读取 WSL 发行版内的 Codex 会话正文:逐 candidate home 按 id 定位
pub(super) fn read_wsl_codex_session_content(
    distro: &str,
    session_id: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    for home in wsl_candidate_homes(distro) {
        let sessions_dir = home.join(".codex").join("sessions");
        if let Some(path) = find_codex_session_file(&sessions_dir, session_id) {
            return read_codex_messages_from_file(&path);
        }
    }
    Err("未找到 Codex 会话文件".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_message_from_line_parses_response_items_only() {
        let user = r#"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"do it"}],"timestamp":"t1"}}"#;
        let m = codex_message_from_line(user).unwrap();
        assert_eq!(m.role, "user");
        assert_eq!(m.content, "do it");

        assert!(codex_message_from_line(r#"{"type":"session_meta","payload":{}}"#).is_none());
        assert!(codex_message_from_line("garbage").is_none());
    }

    #[test]
    fn codex_meta_from_line_extracts_fields() {
        let line = r#"{"type":"session_meta","payload":{"id":"abc","cwd":"/home/u/proj","timestamp":"2026-01-01T00:00:00Z"}}"#;
        let meta = codex_meta_from_line(line).unwrap();
        assert_eq!(meta.id, "abc");
        assert_eq!(meta.cwd, "/home/u/proj");
        assert_eq!(meta.timestamp, "2026-01-01T00:00:00Z");

        assert!(codex_meta_from_line(r#"{"type":"response_item"}"#).is_none());
        assert!(codex_meta_from_line("").is_none());
    }

    #[test]
    fn codex_user_title_from_line_skips_injected_text() {
        let injected = r#"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"<user_instructions>x</user_instructions>"}]}}"#;
        assert!(codex_user_title_from_line(injected).is_none());

        let agents = r##"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions"}]}}"##;
        assert!(codex_user_title_from_line(agents).is_none());

        let real = r#"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"refactor the pool"}]}}"#;
        assert_eq!(
            codex_user_title_from_line(real).as_deref(),
            Some("refactor the pool")
        );
    }
}
