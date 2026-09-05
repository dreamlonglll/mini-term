use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::{
    AiQuestion, AiQuestionAnswer, AiQuestionItem, AiQuestionOption, AiSessionMessage,
    answer_labels, extract_text_content,
};

const OMP_OTHER_OPTION: &str = "Other (type your own)";

/// OMP session file header. The filename contains the id, but the header is
/// still checked so a stale or colliding path cannot bind the wrong session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmpSessionMeta {
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
}

pub fn omp_session_meta_from_line(line: &str) -> Option<OmpSessionMeta> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;
    if obj.get("type").and_then(|v| v.as_str()) != Some("session") {
        return None;
    }
    Some(OmpSessionMeta {
        id: obj.get("id").and_then(|v| v.as_str())?.to_string(),
        timestamp: obj
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        cwd: obj
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

fn session_meta(path: &Path) -> Option<OmpSessionMeta> {
    let file = fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(5).flatten() {
        if let Some(meta) = omp_session_meta_from_line(&line) {
            return Some(meta);
        }
    }
    None
}

fn clean_windows_verbatim(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

fn normalize_windows_path(path: &str) -> String {
    clean_windows_verbatim(path)
        .replace('/', "\\")
        .to_ascii_lowercase()
        .trim_end_matches('\\')
        .to_string()
}

/// OMP 的 legacy absolute 目录编码。只去掉一个前导分隔符，UNC 路径必须保留第二个。
fn encoded_absolute_dir(path: &str) -> String {
    let path = clean_windows_verbatim(path).trim_end_matches(['/', '\\']);
    let path = path
        .strip_prefix('/')
        .or_else(|| path.strip_prefix('\\'))
        .unwrap_or(path);
    format!("--{}--", path.replace(['/', '\\', ':'], "-"))
}

/// Return the original-cased suffix when `path` is below `base`.
fn relative_to(base: &str, path: &str) -> Option<String> {
    let base_original = clean_windows_verbatim(base)
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_string();
    let path_original = clean_windows_verbatim(path)
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_string();
    let base_normalized = normalize_windows_path(&base_original);
    let path_normalized = normalize_windows_path(&path_original);
    if path_normalized == base_normalized {
        return Some(String::new());
    }
    let rest = path_normalized.strip_prefix(&base_normalized)?;
    if !rest.starts_with('\\') {
        return None;
    }
    path_original
        .get(base_normalized.len() + 1..)
        .map(str::to_string)
}

fn encoded_relative_dir(prefix: &str, relative: &str) -> String {
    let encoded = relative.replace(['/', '\\', ':'], "-");
    if encoded.is_empty() {
        prefix.to_string()
    } else if prefix.ends_with('-') {
        format!("{prefix}{encoded}")
    } else {
        format!("{prefix}-{encoded}")
    }
}

/// Compute OMP's default directory name and retain the legacy absolute name
/// as a read-only discovery fallback for sessions written by older versions.
fn session_dir_names(home: &str, temp_root: &str, project_path: &str) -> Vec<String> {
    let project_path = clean_windows_verbatim(project_path);
    let default_name = if let Some(relative) = relative_to(home, project_path) {
        encoded_relative_dir("-", &relative)
    } else if let Some(relative) = relative_to(temp_root, project_path) {
        encoded_relative_dir("-tmp", &relative)
    } else {
        encoded_absolute_dir(project_path)
    };
    let legacy_name = encoded_absolute_dir(project_path);
    if default_name == legacy_name {
        vec![default_name]
    } else {
        vec![default_name, legacy_name]
    }
}

fn project_session_dirs_in(
    sessions_root: &Path,
    home: &Path,
    temp_root: &Path,
    project_path: &str,
) -> Vec<PathBuf> {
    if !sessions_root.is_dir() {
        return Vec::new();
    }

    let mut project_paths = vec![project_path.to_string()];
    if let Ok(canonical) = PathBuf::from(project_path).canonicalize()
        && canonical.to_string_lossy() != project_path
    {
        project_paths.push(canonical.to_string_lossy().into_owned());
    }

    let home = home.to_string_lossy();
    let temp_root = temp_root.to_string_lossy();
    let mut names = Vec::new();
    for project_path in project_paths {
        for name in session_dir_names(&home, &temp_root, &project_path) {
            if !names.iter().any(|current| current == &name) {
                names.push(name);
            }
        }
    }

    names
        .into_iter()
        .map(|name| sessions_root.join(name))
        .filter(|path| path.is_dir())
        .collect()
}

fn project_session_dirs(project_path: &str) -> Vec<PathBuf> {
    let Some(agent_dir) = crate::hook_registry::omp_agent_dir() else {
        return Vec::new();
    };
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    project_session_dirs_in(
        &agent_dir.join("sessions"),
        &home,
        &std::env::temp_dir(),
        project_path,
    )
}

fn meta_matches_project(meta: &OmpSessionMeta, project_path: &str) -> bool {
    normalize_windows_path(&meta.cwd) == normalize_windows_path(project_path)
}

fn find_omp_session_file_in(
    sessions_root: &Path,
    home: &Path,
    temp_root: &Path,
    project_path: &str,
    session_id: &str,
) -> Option<PathBuf> {
    let suffix = format!("_{session_id}.jsonl");
    for dir in project_session_dirs_in(sessions_root, home, temp_root, project_path) {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let matches_name = path
                .file_name()
                .and_then(|v| v.to_str())
                .is_some_and(|name| name.ends_with(&suffix));
            if !matches_name {
                continue;
            }
            let Some(meta) = session_meta(&path) else {
                continue;
            };
            if meta.id == session_id && meta_matches_project(&meta, project_path) {
                return Some(path);
            }
        }
    }
    None
}

/// 按 Hook 上报的 session id 精确定位 OMP JSONL。
pub fn find_omp_session_file(project_path: &str, session_id: &str) -> Option<PathBuf> {
    let agent_dir = crate::hook_registry::omp_agent_dir()?;
    let home = dirs::home_dir()?;
    find_omp_session_file_in(
        &agent_dir.join("sessions"),
        &home,
        &std::env::temp_dir(),
        project_path,
        session_id,
    )
}

/// 无 Hook 时的启发式绑定：先按 mtime 限量，再读取 header 校验 cwd。
pub fn newest_omp_session_file(project_path: &str) -> Option<(PathBuf, SystemTime)> {
    const MAX_SCAN: usize = 30;
    let mut newest: Option<(PathBuf, SystemTime)> = None;
    for dir in project_session_dirs(project_path) {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|v| v.to_str()) == Some("jsonl"))
            .collect();
        super::sort_newest_session_paths(&mut paths, MAX_SCAN);
        for path in paths {
            let Some(meta) = session_meta(&path) else {
                continue;
            };
            if !meta_matches_project(&meta, project_path) {
                continue;
            }
            let Ok(modified) = path.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if newest
                .as_ref()
                .is_none_or(|(_, current)| modified > *current)
            {
                newest = Some((path, modified));
            }
        }
    }
    newest
}

/// OMP 18.x 的普通消息：外层 `type=message`，角色与正文位于 `message.*`。
pub fn omp_message_from_line(line: &str) -> Option<AiSessionMessage> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;
    if obj.get("type").and_then(|v| v.as_str()) != Some("message") {
        return None;
    }
    let role = match obj.pointer("/message/role").and_then(|v| v.as_str()) {
        Some("user") => "user",
        Some("assistant") => "assistant",
        _ => return None,
    };
    if role == "user"
        && obj
            .pointer("/message/synthetic")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    {
        return None;
    }
    let content = extract_text_content(obj.pointer("/message/content"));
    if content.is_empty() {
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

/// assistant 消息中的 `toolCall(name=ask)` → 移动端提问卡片。
pub fn omp_question_from_line(line: &str) -> Option<AiQuestion> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;
    if obj.get("type").and_then(|v| v.as_str()) != Some("message")
        || obj.pointer("/message/role").and_then(|v| v.as_str()) != Some("assistant")
    {
        return None;
    }
    let call = obj
        .pointer("/message/content")?
        .as_array()?
        .iter()
        .find(|item| {
            item.get("type").and_then(|v| v.as_str()) == Some("toolCall")
                && item.get("name").and_then(|v| v.as_str()) == Some("ask")
        })?;
    let items: Vec<AiQuestionItem> = call
        .pointer("/arguments/questions")?
        .as_array()?
        .iter()
        .filter_map(|question| {
            let options: Vec<AiQuestionOption> = question
                .get("options")?
                .as_array()?
                .iter()
                .filter_map(|option| {
                    Some(AiQuestionOption {
                        label: option.get("label")?.as_str()?.to_string(),
                        description: option
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    })
                })
                .collect();
            if options.is_empty() {
                return None;
            }
            Some(AiQuestionItem {
                question: question.get("question")?.as_str()?.to_string(),
                header: question
                    .get("header")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                id: question
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                options,
                multi_select: question
                    .get("multi")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                recommended_index: question
                    .get("recommended")
                    .and_then(|v| v.as_u64())
                    .and_then(|v| usize::try_from(v).ok()),
            })
        })
        .collect();
    if items.is_empty() {
        return None;
    }
    Some(AiQuestion {
        tool_use_id: call.get("id")?.as_str()?.to_string(),
        items,
        timestamp: obj
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

fn labels_with_custom(details: &serde_json::Value) -> Vec<String> {
    let mut labels = details
        .get("selectedOptions")
        .and_then(answer_labels)
        .unwrap_or_default();
    if details
        .get("customInput")
        .and_then(|v| v.as_str())
        .is_some_and(|v| !v.is_empty())
    {
        labels.push(OMP_OTHER_OPTION.to_string());
    }
    labels
}

/// `toolResult(toolName=ask)` → 提问已处理标记，按 toolCallId 与挂起卡片对账。
pub fn omp_question_answer_from_line(line: &str) -> Option<AiQuestionAnswer> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;
    if obj.get("type").and_then(|v| v.as_str()) != Some("message")
        || obj.pointer("/message/role").and_then(|v| v.as_str()) != Some("toolResult")
        || obj.pointer("/message/toolName").and_then(|v| v.as_str()) != Some("ask")
    {
        return None;
    }
    let message = obj.get("message")?;
    let mut answers = HashMap::new();
    if let Some(details) = message.get("details") {
        if let Some(results) = details.get("results").and_then(|v| v.as_array()) {
            for result in results {
                let key = result
                    .get("question")
                    .or_else(|| result.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let labels = labels_with_custom(result);
                if !key.is_empty() && !labels.is_empty() {
                    answers.insert(key.to_string(), labels);
                }
            }
        } else {
            let key = details
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let labels = labels_with_custom(details);
            if !key.is_empty() && !labels.is_empty() {
                answers.insert(key.to_string(), labels);
            }
        }
    }
    Some(AiQuestionAnswer {
        tool_use_ids: vec![message.get("toolCallId")?.as_str()?.to_string()],
        answers,
        is_error: message
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        timestamp: obj
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_meta_and_messages() {
        let meta = omp_session_meta_from_line(
            r#"{"type":"session","version":3,"id":"s-1","timestamp":"t0","cwd":"D:\\repo"}"#,
        )
        .unwrap();
        assert_eq!(meta.id, "s-1");
        assert_eq!(meta.cwd, r"D:\repo");

        let user = omp_message_from_line(
            r#"{"type":"message","timestamp":"t1","message":{"role":"user","content":[{"type":"text","text":"fix it"}]}}"#,
        )
        .unwrap();
        assert_eq!(user.role, "user");
        assert_eq!(user.content, "fix it");
    }

    #[test]
    fn synthetic_user_messages_are_not_mirror_messages() {
        let line = r#"{"type":"message","timestamp":"t","message":{"role":"user","synthetic":true,"content":[{"type":"text","text":"internal"}]}}"#;
        assert!(omp_message_from_line(line).is_none());
    }

    #[test]
    fn parses_ask_question_and_result() {
        let question = omp_question_from_line(
            r#"{"type":"message","timestamp":"t1","message":{"role":"assistant","content":[{"type":"toolCall","id":"call-1","name":"ask","arguments":{"questions":[{"id":"q1","question":"Which?","header":"Plan","options":[{"label":"A","description":"safe"},{"label":"B"}],"multi":false,"recommended":1}]}}]}}"#,
        )
        .unwrap();
        assert_eq!(question.tool_use_id, "call-1");
        assert_eq!(question.items[0].recommended_index, Some(1));

        let answer = omp_question_answer_from_line(
            r#"{"type":"message","timestamp":"t2","message":{"role":"toolResult","toolCallId":"call-1","toolName":"ask","details":{"question":"Which?","selectedOptions":["B"]}}}"#,
        )
        .unwrap();
        assert_eq!(answer.answers["Which?"], ["B"]);
    }

    #[test]
    fn omp_path_encodings_match_source_rules() {
        assert_eq!(
            session_dir_names(r"C:\Users\u", r"C:\Users\u\Temp", r"C:\Users\u\Git\proj")[0],
            "-Git-proj"
        );
        assert_eq!(
            session_dir_names(r"C:\Users\u", r"C:\Users\u\Temp", r"C:\Users\u")[0],
            "-"
        );
        assert_eq!(
            session_dir_names(r"C:\Users\u", r"C:\Windows\Temp", r"C:\Windows\Temp\x",)[0],
            "-tmp-x"
        );
        assert_eq!(
            session_dir_names(r"C:\Users\u", r"C:\Users\u\Temp", r"D:\Git\proj")[0],
            "--D--Git-proj--"
        );
        assert_eq!(
            encoded_absolute_dir(r"\\server\share\proj"),
            "---server-share-proj--"
        );
    }

    #[test]
    fn finds_session_file_by_project_and_id() {
        let root = std::env::temp_dir().join(format!("mini-term-omp-test-{}", std::process::id()));
        let agent = root.join("agent");
        let project = root.join("workspace");
        fs::create_dir_all(&project).unwrap();
        let sessions = agent
            .join("sessions")
            .join(encoded_absolute_dir(project.to_str().unwrap()));
        fs::create_dir_all(&sessions).unwrap();
        let wanted = sessions.join("2026-09-03T00-00-00Z_session-1.jsonl");
        fs::write(
            &wanted,
            format!(
                "{{\"type\":\"session\",\"id\":\"session-1\",\"cwd\":{}}}\n",
                serde_json::to_string(project.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();
        let found = find_omp_session_file_in(
            &agent.join("sessions"),
            &root,
            &root.join("Temp"),
            project.to_str().unwrap(),
            "session-1",
        );
        assert_eq!(found, Some(wanted));
        fs::remove_dir_all(root).unwrap();
    }
}
