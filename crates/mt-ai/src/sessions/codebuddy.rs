//! CodeBuddy Code 会话记录。
//!
//! 记录目录：`~/.codebuddy/projects/<项目路径编码>/<session-id>.jsonl`，
//! 与 Claude 一样按启动目录分桶；子代理 transcript 在 `<id>/subagents/` 子目录，
//! 镜像只认主会话，不读子代理。
//!
//! 行形态与 Claude / Codex 都不同：一行一条事件，`type` 取
//! `session-meta` / `message` / `function_call` / `function_call_result` /
//! `reasoning` / `summary` 等；正文在 `message` 行的 `role` + `content`
//! （content 是 `{type: input_text|output_text, text}` 数组）。

use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use super::{
    AiSession, AiSessionMessage, encode_project_path, extract_text_content, home_dir,
    session_id_path_safe,
};

/// CodeBuddy 会话 id 形态是 UUID（含 `-`），与现有白名单一致。

/// 项目在 `~/.codebuddy/projects` 下的桶目录。
fn project_dirs(project_path: &str) -> Vec<PathBuf> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let root = home.join(".codebuddy").join("projects");
    let encoded = encode_project_path(project_path);
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == encoded || name.starts_with(&encoded) {
            // 编码有损时会有前缀相同的兄弟项目，读文件里的 cwd 校验
            if name == encoded || dir_matches_project(&path, project_path) {
                dirs.push(path);
            }
        }
    }
    dirs
}

fn dir_matches_project(dir: &Path, project_path: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let normalize = |p: &str| {
        p.trim_end_matches(['/', '\\'])
            .to_ascii_lowercase()
            .replace('\\', "/")
    };
    let target = normalize(project_path);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines().take(8) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
                    if normalize(cwd) == target {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// 会话目录下的主会话文件（排除 subagents/ 子目录）。
fn session_file_in(dir: &Path, session_id: &str) -> Option<PathBuf> {
    let path = dir.join(format!("{session_id}.jsonl"));
    path.is_file().then_some(path)
}

/// 按 hook 上报的 session id 精确定位 CodeBuddy JSONL。
pub fn find_codebuddy_session_file(project_path: &str, session_id: &str) -> Option<PathBuf> {
    if !session_id_path_safe(session_id) {
        return None;
    }
    project_dirs(project_path)
        .into_iter()
        .find_map(|dir| session_file_in(&dir, session_id))
}

/// 无 hook 时的启发式绑定：项目桶里最新的主会话文件。
pub fn newest_codebuddy_session_file(project_path: &str) -> Option<(PathBuf, SystemTime)> {
    let mut best: Option<(PathBuf, SystemTime)> = None;
    for dir in project_dirs(project_path) {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            if best.as_ref().is_none_or(|(_, cur)| modified > *cur) {
                best = Some((path, modified));
            }
        }
    }
    best
}

/// 按 id 在所有项目桶里反查启动目录（用于续接前的 cwd 校正）。
pub fn lookup_codebuddy_session_cwd(session_id: &str) -> Option<String> {
    if !session_id_path_safe(session_id) {
        return None;
    }
    let home = home_dir()?;
    let root = home.join(".codebuddy").join("projects");
    let target = format!("{session_id}.jsonl");
    for entry in fs::read_dir(&root).ok()?.flatten() {
        let path = entry.path().join(&target);
        if !path.is_file() {
            continue;
        }
        let content = fs::read_to_string(&path).ok()?;
        for line in content.lines().take(50) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
                    if !cwd.is_empty() && Path::new(cwd).is_dir() {
                        return Some(cwd.to_string());
                    }
                }
            }
        }
    }
    None
}

/// 系统注入噪声：与 Claude 的 `<system-reminder>` 同源，不进镜像。
fn is_injected_text(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("<system-reminder")
        || t.starts_with("<local-command-")
        || t.starts_with("<command-name>")
        || t.starts_with("Caveat: The messages below were generated by the user")
}

/// 一条 CodeBuddy `message` 行 → 镜像消息。非 message / 噪声 / 空正文一律 None。
pub fn codebuddy_message_from_line(line: &str) -> Option<AiSessionMessage> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;
    if obj.get("type").and_then(|v| v.as_str()) != Some("message") {
        return None;
    }
    let role = match obj.get("role").and_then(|v| v.as_str()) {
        Some("user") => "user",
        Some("assistant") => "assistant",
        _ => return None,
    };
    let content = extract_text_content(obj.get("content"));
    if content.is_empty() || is_injected_text(&content) {
        return None;
    }
    Some(AiSessionMessage {
        role: role.to_string(),
        content,
        timestamp: obj
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

/// 读取一条会话正文（历史预览 / 续接前检查用）。
pub fn read_codebuddy_session_content(
    session_id: &str,
    project_path: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    let path = find_codebuddy_session_file(project_path, session_id)
        .ok_or_else(|| "会话文件不存在".to_string())?;
    let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    Ok(content
        .lines()
        .filter_map(codebuddy_message_from_line)
        .collect())
}

/// 项目下的 CodeBuddy 会话列表（桌面 AI 历史面板）。
pub fn get_codebuddy_sessions(project_path: &str) -> Vec<AiSession> {
    let mut sessions = Vec::new();
    for dir in project_dirs(project_path) {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let mut title = String::new();
            let mut timestamp = String::new();
            let model = None;
            for line in content.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    match v.get("type").and_then(|t| t.as_str()) {
                        Some("ai-title") if title.is_empty() => {
                            title = v
                                .get("title")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();
                        }
                        Some("message") => {
                            if timestamp.is_empty() {
                                timestamp = v
                                    .get("timestamp")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("")
                                    .to_string();
                            }
                            if title.is_empty()
                                && v.get("role").and_then(|r| r.as_str()) == Some("user")
                            {
                                title = extract_text_content(v.get("content"));
                                if is_injected_text(&title) {
                                    title.clear();
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            if title.is_empty() {
                title = id.chars().take(8).collect();
            }
            sessions.push(AiSession {
                id,
                session_type: "codebuddy".to_string(),
                title: title.chars().take(80).collect(),
                timestamp,
                model,
                wsl_distro: None,
                ssh_connection_id: None,
            });
        }
    }
    sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    sessions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_message_line() {
        let line = r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}],"cwd":"G:\\Project\\stone","sessionId":"s1","timestamp":"2026-01-01T00:00:00Z"}"#;
        let msg = codebuddy_message_from_line(line).expect("应解析出正文");
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.timestamp, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn skips_injected_and_non_message_lines() {
        let injected = r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"<system-reminder data-role=\"command-caveat\">noise</system-reminder>"}]}"#;
        assert!(codebuddy_message_from_line(injected).is_none());
        assert!(codebuddy_message_from_line(r#"{"type":"function_call"}"#).is_none());
    }
}
