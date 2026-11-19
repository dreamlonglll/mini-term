//! pi 系(pi 与它的分支 oh-my-pi)的 JSONL 会话记录:定位 + 行解析。
//!
//! 两家都用 `type=session` 头与 `type=message` 行(OMP 另有首行标题槽)；agent 目录
//! 和「cwd → 目录名」的编码用 [`PiFlavor`] 区分。
//! 行解析函数沿用 `omp_*` 的名字；两家的桌面历史读取共用 `pi_history.rs`。

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
    omp_session_meta(&obj)
}

fn omp_session_meta(obj: &serde_json::Value) -> Option<OmpSessionMeta> {
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
    pub(super) const fn kind(self) -> crate::AgentKind {
        match self {
            Self::Pi => crate::AgentKind::Pi,
            Self::Omp => crate::AgentKind::Omp,
        }
    }

    pub(super) fn agent_dir(self) -> Option<PathBuf> {
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

/// 共享目录有明确的格式差别时才标注 agent；最老的无标记绝对路径记录无法区分。
fn recorded_flavor(record: &serde_json::Value) -> Option<PiFlavor> {
    match record.get("type").and_then(|value| value.as_str())? {
        "title" | "title_change" | "model_usage" => Some(PiFlavor::Omp),
        "session" if record.get("title").is_some_and(|title| title.is_string()) => {
            Some(PiFlavor::Omp)
        }
        "model_change" if record.get("model").is_some_and(|model| model.is_string()) => {
            Some(PiFlavor::Omp)
        }
        "model_change"
            if record.get("modelId").is_some_and(|id| id.is_string())
                && record
                    .get("provider")
                    .is_some_and(|provider| provider.is_string()) =>
        {
            Some(PiFlavor::Pi)
        }
        _ if record
            .pointer("/runtime/version")
            .is_some_and(|version| version.is_string()) =>
        {
            Some(PiFlavor::Omp)
        }
        _ => None,
    }
}

pub(super) fn shared_sessions_root(flavor: PiFlavor, agent_dir: &Path) -> bool {
    let other = match flavor {
        PiFlavor::Pi => PiFlavor::Omp,
        PiFlavor::Omp => PiFlavor::Pi,
    };
    other.agent_dir().is_some_and(|other_dir| {
        other_dir == agent_dir
            || mt_core::path::canonicalize(agent_dir.join("sessions"))
                .ok()
                .is_some_and(|root| {
                    mt_core::path::canonicalize(other_dir.join("sessions"))
                        .is_ok_and(|other| other == root)
                })
    })
}

pub(super) struct SessionHeader {
    pub(super) meta: OmpSessionMeta,
    pub(super) lines: Vec<String>,
    pub(super) header_idx: usize,
}

/// 列表、正文、镜像和 pi 删除共用 cwd 与共享目录归属检查。
pub(super) fn read_session_header(
    reader: &mut impl BufRead,
    path: &Path,
    flavor: PiFlavor,
    project: &SessionProject<'_>,
    shared: bool,
) -> Option<SessionHeader> {
    let mut lines = Vec::new();
    let mut header = None;
    let mut recorded = None;
    for line in reader.lines().take(8).map_while(Result::ok) {
        let record: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
        if header.is_none()
            && let Some(meta) = omp_session_meta(&record)
        {
            if !project.matches(flavor, &meta.cwd) {
                return None;
            }
            header = Some((lines.len(), meta));
        }
        if shared
            && recorded.is_none()
            && let Some(kind) = recorded_flavor(&record)
        {
            recorded = Some(kind);
        }
        lines.push(line);
        if header.is_some() && (!shared || recorded.is_some()) {
            break;
        }
    }
    if shared {
        let owner =
            recorded.or_else(|| (!project.is_absolute_bucket(path)).then_some(PiFlavor::Omp));
        if owner != Some(flavor) {
            return None;
        }
    }
    let (header_idx, meta) = header?;
    Some(SessionHeader {
        meta,
        lines,
        header_idx,
    })
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

/// 新版 OMP 优先识别 temp，仍只读查找旧 home 相对桶和绝对路径桶。
fn session_dir_names(home: &str, temp_root: &str, project_path: &str) -> Vec<String> {
    let mut names = Vec::with_capacity(3);
    if let Some(relative) = relative_to(temp_root, project_path) {
        names.push(encoded_relative_dir("-tmp", &relative));
    }
    if let Some(relative) = relative_to(home, project_path) {
        let name = encoded_relative_dir("-", &relative);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names.push(encoded_absolute_dir(project_path));
    names
}

pub(super) struct SessionProject<'a> {
    path: &'a str,
    canonical: Option<PathBuf>,
}

impl<'a> SessionProject<'a> {
    pub(super) fn new(path: &'a str) -> Self {
        Self {
            path,
            canonical: mt_core::path::canonicalize(path).ok(),
        }
    }

    fn paths(&self) -> impl Iterator<Item = &str> {
        self.canonical
            .as_deref()
            .and_then(Path::to_str)
            .filter(|path| *path != self.path)
            .into_iter()
            .chain(std::iter::once(self.path))
    }

    pub(super) fn matches(&self, flavor: PiFlavor, cwd: &str) -> bool {
        if cwd.is_empty() {
            return false;
        }
        let eq = |a: &str, b: &str| {
            if cfg!(windows) || flavor == PiFlavor::Pi {
                windows_eq_key(&strip_verbatim_prefix(a))
                    == windows_eq_key(&strip_verbatim_prefix(b))
            } else {
                mt_core::path_key::posix_trim_trailing(a)
                    == mt_core::path_key::posix_trim_trailing(b)
            }
        };
        self.paths().any(|path| eq(cwd, path))
            || self.canonical.as_ref().is_some_and(|project| {
                Path::new(cwd).is_absolute()
                    && mt_core::path::canonicalize(cwd).is_ok_and(|cwd| cwd == *project)
            })
    }

    pub(super) fn is_absolute_bucket(&self, file: &Path) -> bool {
        let bucket = file.parent().and_then(Path::file_name);
        self.paths()
            .any(|path| bucket == Some(std::ffi::OsStr::new(&encoded_absolute_dir(path))))
    }
}

pub(super) fn session_dirs(
    flavor: PiFlavor,
    sessions_root: &Path,
    home: &Path,
    temp_root: &Path,
    project: &SessionProject<'_>,
) -> Vec<PathBuf> {
    if !sessions_root.is_dir() {
        return Vec::new();
    }

    let canonical_home = (flavor == PiFlavor::Omp)
        .then(|| mt_core::path::canonicalize(home).ok())
        .flatten();
    let canonical_temp = (flavor == PiFlavor::Omp)
        .then(|| mt_core::path::canonicalize(temp_root).ok())
        .flatten();
    let fallback = (home.to_string_lossy(), temp_root.to_string_lossy());
    let preferred = (
        canonical_home.as_deref().unwrap_or(home).to_string_lossy(),
        canonical_temp
            .as_deref()
            .unwrap_or(temp_root)
            .to_string_lossy(),
    );
    let mut names = Vec::new();
    for project_path in project.paths() {
        let candidates = flavor.dir_names(&preferred.0, &preferred.1, project_path);
        let legacy = (preferred != fallback)
            .then(|| flavor.dir_names(&fallback.0, &fallback.1, project_path));
        for name in candidates.into_iter().chain(legacy.into_iter().flatten()) {
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

pub(super) fn project_session_dirs(
    flavor: PiFlavor,
    agent_dir: &Path,
    project: &SessionProject<'_>,
) -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    session_dirs(
        flavor,
        &agent_dir.join("sessions"),
        &home,
        &std::env::temp_dir(),
        project,
    )
}

pub(super) fn session_filename_matches(path: &Path, session_id: &str) -> bool {
    !session_id.is_empty()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.strip_suffix(".jsonl")
                    .and_then(|stem| stem.strip_suffix(session_id))
                    .is_some_and(|prefix| prefix.ends_with('_'))
            })
}

/// 先按修改时间、再按桶优先级、最后按路径选择；列表与按 ID 查找用同一个顺序。
/// 定位正文时只 stat 文件名已经命中的副本，不扫整个项目的记录头。
pub(super) fn ordered_session_files(
    dirs: &[PathBuf],
    session_id: Option<&str>,
    limit: usize,
) -> impl Iterator<Item = (PathBuf, Option<SystemTime>)> {
    let mut files = Vec::new();
    for (bucket, dir) in dirs.iter().enumerate() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl")
                || session_id.is_some_and(|id| !session_filename_matches(&path, id))
            {
                continue;
            }
            if let Ok(meta) = path.metadata()
                && meta.is_file()
            {
                files.push((meta.modified().ok(), bucket, path));
            }
        }
    }
    files.sort_unstable_by(|(a_time, a_bucket, a), (b_time, b_bucket, b)| {
        b_time
            .cmp(a_time)
            .then_with(|| a_bucket.cmp(b_bucket))
            .then_with(|| a.cmp(b))
    });
    files
        .into_iter()
        .take(limit)
        .map(|(time, _, path)| (path, time))
}

pub(super) fn find_session_file_in(
    flavor: PiFlavor,
    dirs: &[PathBuf],
    project: &SessionProject<'_>,
    session_id: &str,
    shared: bool,
) -> Option<PathBuf> {
    for (path, _) in ordered_session_files(dirs, Some(session_id), usize::MAX) {
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        let header = read_session_header(&mut BufReader::new(file), &path, flavor, project, shared);
        if header.is_some_and(|header| header.meta.id == session_id) {
            return Some(path);
        }
    }
    None
}

pub(super) fn find_session_file(
    flavor: PiFlavor,
    project_path: &str,
    session_id: &str,
) -> Option<PathBuf> {
    let agent_dir = flavor.agent_dir()?;
    let project = SessionProject::new(project_path);
    let dirs = project_session_dirs(flavor, &agent_dir, &project);
    find_session_file_in(
        flavor,
        &dirs,
        &project,
        session_id,
        shared_sessions_root(flavor, &agent_dir),
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
    let agent_dir = flavor.agent_dir()?;
    let project = SessionProject::new(project_path);
    let dirs = project_session_dirs(flavor, &agent_dir, &project);
    let shared = shared_sessions_root(flavor, &agent_dir);
    for (path, modified) in ordered_session_files(&dirs, None, MAX_SCAN) {
        let Some(modified) = modified else { continue };
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        if read_session_header(&mut BufReader::new(file), &path, flavor, &project, shared).is_some()
        {
            return Some((path, modified));
        }
    }
    None
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
            session_dir_names(r"C:\Users\u", r"C:\Users\u\Temp", r"C:\Users\u\Temp\x"),
            ["-tmp-x", "-Temp-x", "--C--Users-u-Temp-x--"]
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
        let project = SessionProject::new(project.to_str().unwrap());
        for flavor in [PiFlavor::Omp, PiFlavor::Pi] {
            let dirs = session_dirs(
                flavor,
                &agent.join("sessions"),
                &root,
                &root.join("Temp"),
                &project,
            );
            let found = find_session_file_in(flavor, &dirs, &project, "session-1", false);
            assert_eq!(found.as_ref(), Some(&wanted), "{flavor:?}");
            assert!(find_session_file_in(flavor, &dirs, &project, "missing", false).is_none());
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
