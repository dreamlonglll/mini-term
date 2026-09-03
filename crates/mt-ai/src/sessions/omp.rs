use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::{
    answer_labels, extract_text_content, normalize_path, AiQuestion, AiQuestionAnswer,
    AiQuestionItem, AiQuestionOption, AiSessionMessage,
};

const OMP_OTHER_OPTION: &str = "Other (type your own)";
#[cfg(test)]
use parking_lot::Mutex;
#[cfg(test)]
use std::sync::LazyLock;

#[cfg(test)]
static TEST_AGENT_DIR: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(|| Mutex::new(None));

fn omp_agent_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(path) = TEST_AGENT_DIR.lock().clone() {
        return Some(path);
    }
    crate::hook_registry::omp_agent_dir()
}

/// OMP 会话文件头。18.x 的文件名前缀是创建时间，精确身份必须读头部 `session.id`。
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

fn encoded_absolute_dir(path: &str) -> String {
    let path = clean_windows_verbatim(path).trim_end_matches(['/', '\\']);
    let path = path.trim_start_matches(['/', '\\']);
    format!("--{}--", path.replace(['/', '\\', ':'], "-"))
}

fn relative_to(base: &Path, path: &Path) -> Option<String> {
    let base = clean_windows_verbatim(base.to_str()?);
    let path = clean_windows_verbatim(path.to_str()?);
    let base_cmp = base.to_ascii_lowercase();
    let path_cmp = path.to_ascii_lowercase();
    let base_cmp = base_cmp.trim_end_matches(['/', '\\']);
    let path_cmp = path_cmp.trim_end_matches(['/', '\\']);
    if path_cmp == base_cmp {
        return Some(String::new());
    }
    let rest = path_cmp.strip_prefix(base_cmp)?;
    if !rest.starts_with(['/', '\\']) {
        return None;
    }
    let offset = base.len() + 1;
    path.get(offset..).map(str::to_string)
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

/// 复刻 OMP 18.x 的默认 session 目录命名，并保留旧绝对路径目录候选。
/// 候选最终仍会用文件头 cwd 校验，编码碰撞不会串项目。
fn project_session_dirs(project_path: &str) -> Vec<PathBuf> {
    let Some(agent_dir) = omp_agent_dir() else {
        return Vec::new();
    };
    let root = agent_dir.join("sessions");
    if !root.is_dir() {
        return Vec::new();
    }


    let original = PathBuf::from(project_path);
    let mut paths = vec![original.clone()];
    if let Ok(canonical) = original.canonicalize()
        && canonical != original
    {
        paths.push(canonical);
    }

    let home = dirs::home_dir();
    let temp = std::env::temp_dir();
    let mut names = Vec::new();
    for path in paths {
        let raw = clean_windows_verbatim(path.to_str().unwrap_or(project_path));
        names.push(encoded_absolute_dir(raw));
        if let Some(relative) = home.as_ref().and_then(|h| relative_to(h, &path)) {
            names.push(encoded_relative_dir("-", &relative));
        } else if let Some(relative) = relative_to(&temp, &path) {
            names.push(encoded_relative_dir("-tmp", &relative));
        }
    }
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| root.join(name))
        .filter(|path| path.is_dir())
        .collect()
}

fn meta_matches_project(meta: &OmpSessionMeta, project_path: &str) -> bool {
    normalize_path(&meta.cwd) == normalize_path(project_path)
}

/// 按 Hook 上报的 session id 精确定位 OMP JSONL。
pub fn find_omp_session_file(project_path: &str, session_id: &str) -> Option<PathBuf> {
    let suffix = format!("_{session_id}.jsonl");
    for dir in project_session_dirs(project_path) {
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

/// 无 Hook 时的启发式绑定：只在目标项目自己的 OMP session 目录中取最新文件。
pub fn newest_omp_session_file(project_path: &str) -> Option<(PathBuf, SystemTime)> {
    let mut newest: Option<(PathBuf, SystemTime)> = None;
    for dir in project_session_dirs(project_path) {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|v| v.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(meta) = session_meta(&path) else {
                continue;
            };
            if !meta_matches_project(&meta, project_path) {
                continue;
            }
            let Ok(modified) = path.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if newest.as_ref().is_none_or(|(_, current)| modified > *current) {
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
                multi_select: question.get("multi").and_then(|v| v.as_bool()).unwrap_or(false),
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
    fn parses_ask_question_and_result() {
        let question = omp_question_from_line(
            r#"{"type":"message","timestamp":"t1","message":{"role":"assistant","content":[{"type":"text","text":"choose"},{"type":"toolCall","id":"call-1","name":"ask","arguments":{"questions":[{"id":"q1","question":"Which?","header":"Plan","options":[{"label":"A","description":"safe"},{"label":"B"}],"multi":false,"recommended":1}]}}]}}"#,
        )
        .unwrap();
        assert_eq!(question.tool_use_id, "call-1");
        assert_eq!(question.items[0].id, "q1");
        assert_eq!(question.items[0].recommended_index, Some(1));

        let answer = omp_question_answer_from_line(
            r#"{"type":"message","timestamp":"t2","message":{"role":"toolResult","toolCallId":"call-1","toolName":"ask","content":[{"type":"text","text":"Selected B"}],"details":{"question":"Which?","options":["A","B"],"selectedOptions":["B"],"multi":false}}}"#,
        )
        .unwrap();
        assert_eq!(answer.tool_use_ids, ["call-1"]);
        assert_eq!(answer.answers["Which?"], ["B"]);
        assert!(!answer.is_error);
    }

    #[test]
    fn custom_input_maps_to_omp_other_option() {
        let answer = omp_question_answer_from_line(
            r#"{"type":"message","message":{"role":"toolResult","toolCallId":"call-1","toolName":"ask","details":{"question":"Which?","selectedOptions":[],"customInput":"custom"}}}"#,
        )
        .unwrap();
        assert_eq!(answer.answers["Which?"], [OMP_OTHER_OPTION]);
    }


    #[test]
    fn finds_session_file_by_project_and_id() {
        let root = std::env::temp_dir().join(format!(
            "mini-term-omp-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let agent = root.join("agent");
        let project = root.join("workspace");
        fs::create_dir_all(&project).unwrap();
        let encoded = encoded_absolute_dir(project.to_str().unwrap());
        let sessions = agent.join("sessions").join(encoded);
        fs::create_dir_all(&sessions).unwrap();
        let wanted = sessions.join("2026-09-03T00-00-00Z_session-1.jsonl");
        fs::write(
            &wanted,
            format!(
                "{{\"type\":\"title\",\"title\":\"x\"}}\n{{\"type\":\"session\",\"id\":\"session-1\",\"timestamp\":\"t\",\"cwd\":{}}}\n",
                serde_json::to_string(project.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();
        fs::write(
            sessions.join("2026-09-03T00-00-01Z_other.jsonl"),
            format!(
                "{{\"type\":\"session\",\"id\":\"other\",\"cwd\":{}}}\n",
                serde_json::to_string(project.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();

        *TEST_AGENT_DIR.lock() = Some(agent);
        let found = find_omp_session_file(project.to_str().unwrap(), "session-1").unwrap();
        assert_eq!(found, wanted);
        assert!(find_omp_session_file(project.to_str().unwrap(), "missing").is_none());
        *TEST_AGENT_DIR.lock() = None;
        fs::remove_dir_all(root).unwrap();
    }
}