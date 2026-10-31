//! pi 系(pi 与它的分支 oh-my-pi)的 JSONL 会话记录:定位 + 行解析。
//!
//! 两家的行格式相同(首行 `type=session` 头,正文 `type=message` 行,角色与正文在
//! `message.*`),差在 agent 目录与「cwd → 目录名」的编码,用 [`PiFlavor`] 区分。
//! 行解析函数沿用 `omp_*` 的名字,pi 直接复用;pi 独有的历史列表 / 正文读取在 `pi.rs`。

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use mt_core::path_key::{
    slash_form, strip_verbatim_prefix, trim_trailing_separators, windows_eq_key,
};

use super::{
    AiQuestion, AiQuestionAnswer, AiQuestionItem, AiQuestionOption, AiSessionMessage,
    answer_labels, extract_text_content,
};

const OMP_OTHER_OPTION: &str = "Other (type your own)";

/// OMP 会话文件头。文件名含 session id,仍需校验头部以防旧路径或碰撞绑定错误会话。
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

/// pi 系会话记录属于哪一家。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PiFlavor {
    Pi,
    Omp,
}

impl PiFlavor {
    fn agent_dir(self) -> Option<PathBuf> {
        match self {
            PiFlavor::Pi => crate::hook_registry::pi_agent_dir(),
            PiFlavor::Omp => crate::hook_registry::omp_agent_dir(),
        }
    }

    /// 项目对应的会话目录名候选(按优先级)。
    fn dir_names(self, home: &str, temp_root: &str, project_path: &str) -> Vec<String> {
        match self {
            // pi 只有绝对路径这一种编码(`session-manager.ts::getDefaultSessionDirPath`),
            // 正是 omp 改成 home 相对之前的那套
            PiFlavor::Pi => vec![encoded_absolute_dir(project_path)],
            PiFlavor::Omp => session_dir_names(home, temp_root, project_path),
        }
    }
}

pub(super) fn session_meta(path: &Path) -> Option<OmpSessionMeta> {
    let file = fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(5).flatten() {
        if let Some(meta) = omp_session_meta_from_line(&line) {
            return Some(meta);
        }
    }
    None
}

/// OMP 的 legacy absolute 目录编码。只去掉一个前导分隔符,UNC 路径必须保留第二个。
fn encoded_absolute_dir(path: &str) -> String {
    let path = strip_verbatim_prefix(path);
    let path = trim_trailing_separators(&path);
    let path = path
        .strip_prefix('/')
        .or_else(|| path.strip_prefix('\\'))
        .unwrap_or(path);
    format!("--{}--", path.replace(['/', '\\', ':'], "-"))
}

/// 返回 path 相对于 base 的原始大小写后缀(分隔符统一成 `/`,编码时与 `\` 同样变 `-`)。
fn relative_to(base: &str, path: &str) -> Option<String> {
    let base_original = slash_form(&strip_verbatim_prefix(base));
    let path_original = slash_form(&strip_verbatim_prefix(path));
    let base_key = windows_eq_key(&base_original);
    let path_key = windows_eq_key(&path_original);
    if path_key == base_key {
        return Some(String::new());
    }
    let rest = path_key.strip_prefix(&base_key)?;
    // 键是正斜杠形态(见 `windows_eq_key`)
    if !rest.starts_with('/') {
        return None;
    }
    path_original.get(base_key.len() + 1..).map(str::to_string)
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

/// 计算 OMP 默认目录名,同时保留 legacy absolute 名称作为只读发现兜底。
fn session_dir_names(home: &str, temp_root: &str, project_path: &str) -> Vec<String> {
    let project_path = strip_verbatim_prefix(project_path);
    let project_path = project_path.as_ref();
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
    flavor: PiFlavor,
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
        for name in flavor.dir_names(&home, &temp_root, &project_path) {
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

pub(super) fn project_session_dirs(flavor: PiFlavor, project_path: &str) -> Vec<PathBuf> {
    let Some(agent_dir) = flavor.agent_dir() else {
        return Vec::new();
    };
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    project_session_dirs_in(
        flavor,
        &agent_dir.join("sessions"),
        &home,
        &std::env::temp_dir(),
        project_path,
    )
}

pub(super) fn meta_matches_project(meta: &OmpSessionMeta, project_path: &str) -> bool {
    // 两边都可能是 canonicalize 的 verbatim 形态:先剥 `\\?\` 再取 Windows 比较键
    let key = |path: &str| windows_eq_key(&strip_verbatim_prefix(path));
    key(&meta.cwd) == key(project_path)
}

fn find_session_file_in(
    flavor: PiFlavor,
    sessions_root: &Path,
    home: &Path,
    temp_root: &Path,
    project_path: &str,
    session_id: &str,
) -> Option<PathBuf> {
    let suffix = format!("_{session_id}.jsonl");
    for dir in project_session_dirs_in(flavor, sessions_root, home, temp_root, project_path) {
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

fn find_session_file(flavor: PiFlavor, project_path: &str, session_id: &str) -> Option<PathBuf> {
    let agent_dir = flavor.agent_dir()?;
    let home = dirs::home_dir()?;
    find_session_file_in(
        flavor,
        &agent_dir.join("sessions"),
        &home,
        &std::env::temp_dir(),
        project_path,
        session_id,
    )
}

/// 按 Hook 上报的 session id 精确定位 OMP JSONL。
pub fn find_omp_session_file(project_path: &str, session_id: &str) -> Option<PathBuf> {
    find_session_file(PiFlavor::Omp, project_path, session_id)
}

/// 按会话 id(hook 上报 / 历史面板)精确定位 pi JSONL。
pub fn find_pi_session_file(project_path: &str, session_id: &str) -> Option<PathBuf> {
    find_session_file(PiFlavor::Pi, project_path, session_id)
}

/// 无 Hook 时的启发式绑定(OMP)。
pub fn newest_omp_session_file(project_path: &str) -> Option<(PathBuf, SystemTime)> {
    newest_session_file(PiFlavor::Omp, project_path)
}

/// 无 Hook 时的启发式绑定(pi)。
pub fn newest_pi_session_file(project_path: &str) -> Option<(PathBuf, SystemTime)> {
    newest_session_file(PiFlavor::Pi, project_path)
}

/// 无 Hook 时的启发式绑定:先按 mtime 限量,再读取 header 校验 cwd。
fn newest_session_file(flavor: PiFlavor, project_path: &str) -> Option<(PathBuf, SystemTime)> {
    const MAX_SCAN: usize = 30;
    let mut newest: Option<(PathBuf, SystemTime)> = None;
    for dir in project_session_dirs(flavor, project_path) {
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
            // paths 已按 mtime 降序,同一目录中首个 cwd 命中就是最新文件。
            break;
        }
    }
    newest
}

/// OMP 18.x 的普通消息:外层 `type=message`,角色与正文位于 `message.*`。
/// pi 同形(`system` / `toolResult` / `custom` 角色与非 message 条目一律不算对话)。
pub fn omp_message_from_line(line: &str) -> Option<AiSessionMessage> {
    let obj: serde_json::Value = serde_json::from_str(line).ok()?;
    omp_message_from_value(&obj)
}

/// [`omp_message_from_line`] 的已解析版本(调用方要顺带读 `id` / `parentId` 时免得解析两遍)。
pub(super) fn omp_message_from_value(obj: &serde_json::Value) -> Option<AiSessionMessage> {
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

/// `toolResult(toolName=ask)` → 提问已处理标记,按 toolCallId 与挂起卡片对账。
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
        assert_eq!(question.items[0].id, "q1");
        assert_eq!(question.items[0].recommended_index, Some(1));

        let answer = omp_question_answer_from_line(
            r#"{"type":"message","timestamp":"t2","message":{"role":"toolResult","toolCallId":"call-1","toolName":"ask","details":{"results":[{"id":"q1","selectedOptions":["B"]}]}}}"#,
        )
        .unwrap();
        assert_eq!(answer.tool_use_ids, ["call-1"]);
        assert_eq!(answer.answers["q1"], ["B"]);
        assert!(!answer.is_error);
    }

    #[test]
    fn custom_input_maps_to_omp_other_option() {
        let details = serde_json::json!({
            "selectedOptions": ["A"],
            "customInput": "use a custom answer"
        });
        assert_eq!(
            labels_with_custom(&details),
            vec!["A".to_string(), OMP_OTHER_OPTION.to_string()]
        );
        assert!(labels_with_custom(&serde_json::json!({"customInput": ""})).is_empty());
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
        // canonicalize 产出的 verbatim 形态与原路径编出同一个目录名
        // (`\\?\UNC\` 以前被剥成 `UNC\…`,编出一个永远不存在的 `--UNC-…` 候选)
        assert_eq!(
            session_dir_names(
                r"C:\Users\u",
                r"C:\Users\u\Temp",
                r"\\?\C:\Users\u\Git\proj"
            )[0],
            "-Git-proj"
        );
        assert_eq!(
            encoded_absolute_dir(r"\\?\UNC\server\share\proj"),
            encoded_absolute_dir(r"\\server\share\proj")
        );
        // 大小写 / 分隔符 / 尾随分隔符差异不影响相对段的判定,相对段保留原大小写
        assert_eq!(
            session_dir_names(r"c:/users/U/", r"C:\Users\u\Temp", r"C:\Users\u\Git\Proj\")[0],
            "-Git-Proj"
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
        let other = sessions.join("other.jsonl");
        fs::write(
            &wanted,
            format!(
                "{{\"type\":\"session\",\"id\":\"session-1\",\"cwd\":{}}}\n",
                serde_json::to_string(project.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();
        fs::write(
            &other,
            format!(
                "{{\"type\":\"session\",\"id\":\"session-other\",\"cwd\":{}}}\n",
                serde_json::to_string(project.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();
        // 项目在 root(当 home)之下:omp 的默认名是 home 相对,这里落的是 legacy 绝对名,
        // 照样找得到;pi 只认绝对名,同一份目录也找得到
        for flavor in [PiFlavor::Omp, PiFlavor::Pi] {
            let found = find_session_file_in(
                flavor,
                &agent.join("sessions"),
                &root,
                &root.join("Temp"),
                project.to_str().unwrap(),
                "session-1",
            );
            assert_eq!(found.as_ref(), Some(&wanted), "{flavor:?}");
            assert!(
                find_session_file_in(
                    flavor,
                    &agent.join("sessions"),
                    &root,
                    &root.join("Temp"),
                    project.to_str().unwrap(),
                    "missing",
                )
                .is_none()
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    /// pi 的目录名只有绝对路径一种(pi `getDefaultSessionDirPath`):剥一个前导分隔符,
    /// `/ \ :` 换 `-`,两头包 `--`。home 之下也不改成相对名(那是 omp 的改动)。
    #[test]
    fn pi_dir_names_are_absolute_only() {
        let names =
            |project: &str| PiFlavor::Pi.dir_names(r"C:\Users\u", r"C:\Users\u\Temp", project);
        // 与本机真实目录名 `--C--Users-12197-.pi-agent--` 同形
        assert_eq!(
            names(r"C:\Users\12197\.pi\agent"),
            ["--C--Users-12197-.pi-agent--"]
        );
        assert_eq!(names(r"C:\Users\u\Git\proj"), ["--C--Users-u-Git-proj--"]);
        assert_eq!(names(r"D:\Projects\本地处理"), ["--D--Projects-本地处理--"]);
        assert_eq!(names("/home/u/proj"), ["--home-u-proj--"]);
        assert_eq!(names(r"\\?\D:\Git\proj\"), ["--D--Git-proj--"]);
    }
}
