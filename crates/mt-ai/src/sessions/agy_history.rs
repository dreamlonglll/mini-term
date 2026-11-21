//! Antigravity CLI(`agy`)的桌面历史列表、会话正文与镜像定位。路径、索引库与逐步
//! 解析都在 `agy.rs`(存储层),这里只在它上面组装。
//!
//! # 列表口径
//!
//! 条目来自索引库 `conversation_summaries.db`,不扫 `brain/` 目录:transcript 里没有
//! cwd,会话 → 项目只能查索引。收录条件:
//!
//! - 属于本项目([`AgyConversation::belongs_to`]:工作区与项目根相同或在其之内);
//! - **不是子代理会话**:它们是父会话派出去的任务,没有用户发言,续接也没有意义;
//! - `step_count > 0`:开了 agy 没发消息会留一行空会话;
//! - transcript 在盘上:正文、模型、镜像都靠它,没有它的条目点开也是空的。
//!
//! 标题用 agy 生成的 `title`(`-p` 会话常空,回落 `preview`),都空再取 transcript 里
//! 第一条用户发言;时间戳是索引的最后修改时间(回落最后一次用户输入时间、再回落
//! transcript 的修改时间);模型取 transcript 尾部最近一次模型调用。
//!
//! # 正文口径(与别家同一套:只出对话文字)
//!
//! | 步骤 | 呈现 |
//! |---|---|
//! | `USER_INPUT` | user:只取 `<USER_REQUEST>` 块内正文,元信息块剥掉 |
//! | `PLANNER_RESPONSE` | assistant:`content` 正文;思考不出;只有工具调用、没有正文的一步不出 |
//! | `GENERIC` | 不出(工具结果;别家的 tool_result / toolResult 同样不进正文) |
//! | `SYSTEM_MESSAGE` | 不出(系统注入:子代理回信、任务派发) |
//! | `ERROR_MESSAGE` | 不出,见下 |
//!
//! `ERROR_MESSAGE` 是 agy **写给模型**的系统提示(Esc 打断后是「The stream was
//! interrupted. Please continue the task you were working on.」),不是用户或模型说的话;
//! 单看这一步也分不出是用户打断还是别的原因断流。别家里 Codex 的 `<turn_aborted>`、
//! pi / OMP 的出错空回复同样不进正文,这里同口径跳过。被打断那一步模型已经吐出的半截
//! 正文(`PLANNER_RESPONSE.content`)照常显示。
//!
//! # 删除
//!
//! 不提供(见 `delete.rs::session_delete_supported`):删一条 agy 会话要动它自己的
//! SQLite 索引、`conversations/<id>.db` 与 `brain/<id>/` 三处,都是 agy 进程私有、
//! 可能正在写的结构。

use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, SecondsFormat, Utc};

use super::agy::{
    AgyConversation, AgyStep, AgyStepKind, agy_app_data_dir, agy_summaries_db_path_in,
    agy_transcript_path_in, find_agy_transcript, parse_agy_step, read_agy_conversations,
    read_agy_conversations_via_snapshot,
};
use super::{AiSession, AiSessionMessage, MAX_SESSIONS_PER_SOURCE, wsl_candidate_homes};
use crate::agent::AgentKind;

/// WSL 9P 经 `\\wsl$` 逐个探测 transcript 文件的上限(先按索引时间排序再探测,
/// 避免有数百条已删 transcript 残留在索引里时产生过多 9P 往返)。
const MAX_WSL_AGY_SESSIONS_TO_PROBE: usize = 160;

const TITLE_MAX_CHARS: usize = 100;
const UNTITLED: &str = "Untitled";
/// 标题回落到首条用户发言时最多看 transcript 的前几行:首步就是 `USER_INPUT`,
/// 留点余量给以后可能出现在它前面的系统步。
const TITLE_HEAD_LINES: usize = 20;
/// 找最新模型时先读的尾窗;尾部那一步若是带大段参数的工具调用(`write_to_file` 的
/// 整份文件内容),64 KB 装不下一整行,再放宽一次。
const MODEL_TAIL_WINDOWS: [u64; 2] = [64 * 1024, 1024 * 1024];

/// 本机项目的 agy 会话(条目口径见模块注释)。没装 agy / 索引库打不开 → 空。
pub(super) fn get_agy_sessions(project_path: &str) -> Vec<AiSession> {
    let Some(dir) = agy_app_data_dir() else {
        return Vec::new();
    };
    let conversations = read_agy_conversations();
    if conversations.is_empty() {
        return Vec::new();
    }
    agy_sessions_in(&dir, &conversations, project_path)
}

/// [`get_agy_sessions`] 的可测部分:索引行由调用方给,transcript 在 `app_data_dir` 下找。
pub(super) fn agy_sessions_in(
    app_data_dir: &Path,
    conversations: &[AgyConversation],
    project_path: &str,
) -> Vec<AiSession> {
    let mut rows: Vec<(&AgyConversation, PathBuf, String)> = conversations
        .iter()
        .filter(|c| !c.is_subagent() && c.step_count > 0 && c.belongs_to(project_path))
        .filter_map(|c| {
            let path = agy_transcript_path_in(app_data_dir, &c.id)?;
            if !path.is_file() {
                return None;
            }
            let timestamp = conversation_time(c, &path);
            Some((c, path, timestamp))
        })
        .collect();
    // 时间串同为 RFC 3339 毫秒 UTC,按字典序即时间序;一个时间都没有的(空串)沉到最后。
    // 先排序截断,再去读 transcript 取标题与模型,截掉的那些不白读
    rows.sort_by(|(_, _, a), (_, _, b)| b.cmp(a));
    rows.truncate(MAX_SESSIONS_PER_SOURCE);
    rows.into_iter()
        .map(|(c, path, timestamp)| AiSession {
            id: c.id.clone(),
            session_type: AgentKind::Agy.key().to_string(),
            title: session_title(c, &path),
            timestamp,
            model: latest_agy_model(&path),
            wsl_distro: None,
            ssh_connection_id: None,
        })
        .collect()
}

/// 扫描指定 WSL 发行版 home 下的 agy 会话。
///
/// 经本地临时快照读 `conversation_summaries.db`(+ `-wal`),不跨 9P 对 Linux 侧
/// WAL 数据库加锁;项目归属走 POSIX 语义([`AgyConversation::belongs_to_posix`])。
pub(super) fn get_wsl_agy_sessions_in(home: &Path, unix_cwd: &str, distro: &str) -> Vec<AiSession> {
    let app_data_dir = home.join(".gemini").join("antigravity-cli");
    let db = agy_summaries_db_path_in(&app_data_dir);
    if !db.is_file() {
        return Vec::new();
    }
    let conversations = match read_agy_conversations_via_snapshot(&db) {
        Ok(list) => list,
        Err(e) => {
            tracing::warn!("[agy-sessions] 读 WSL({distro}) 会话索引失败: {e}");
            return Vec::new();
        }
    };
    if conversations.is_empty() {
        return Vec::new();
    }
    wsl_agy_sessions_in(&app_data_dir, &conversations, unix_cwd, distro)
}

/// [`get_wsl_agy_sessions_in`] 的可测部分:先按索引时间倒序过滤,再探测 transcript 存在性,
/// 凑够 [`MAX_SESSIONS_PER_SOURCE`] 即停,控制 9P 往返次数。
pub(super) fn wsl_agy_sessions_in(
    app_data_dir: &Path,
    conversations: &[AgyConversation],
    unix_cwd: &str,
    distro: &str,
) -> Vec<AiSession> {
    let mut candidates: Vec<&AgyConversation> = conversations
        .iter()
        .filter(|c| !c.is_subagent() && c.step_count > 0 && c.belongs_to_posix(unix_cwd))
        .collect();
    // 先按索引时间倒序排好再逐条探测 transcript(9P 上每次 stat 都是一次跨 VM 往返)
    candidates.sort_by(|a, b| {
        let ta = a
            .updated_at
            .as_deref()
            .or(a.last_user_input_at.as_deref())
            .unwrap_or("");
        let tb = b
            .updated_at
            .as_deref()
            .or(b.last_user_input_at.as_deref())
            .unwrap_or("");
        tb.cmp(ta)
    });

    let mut rows: Vec<(&AgyConversation, PathBuf, String)> = Vec::new();
    for c in candidates.into_iter().take(MAX_WSL_AGY_SESSIONS_TO_PROBE) {
        if rows.len() >= MAX_SESSIONS_PER_SOURCE {
            break;
        }
        let Some(path) = agy_transcript_path_in(app_data_dir, &c.id) else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        let timestamp = conversation_time(c, &path);
        rows.push((c, path, timestamp));
    }
    rows.sort_by(|(_, _, a), (_, _, b)| b.cmp(a));
    rows.into_iter()
        .map(|(c, path, timestamp)| AiSession {
            id: c.id.clone(),
            session_type: AgentKind::Agy.key().to_string(),
            title: session_title(c, &path),
            timestamp,
            model: latest_agy_model(&path),
            wsl_distro: Some(distro.to_string()),
            ssh_connection_id: None,
        })
        .collect()
}

/// 列表时间:索引的最后修改时间 → 最后一次用户输入时间 → transcript 的修改时间。
fn conversation_time(c: &AgyConversation, transcript: &Path) -> String {
    c.updated_at
        .clone()
        .or_else(|| c.last_user_input_at.clone())
        .or_else(|| file_mtime_rfc3339(transcript))
        .unwrap_or_default()
}

fn file_mtime_rfc3339(path: &Path) -> Option<String> {
    let modified = path.metadata().and_then(|m| m.modified()).ok()?;
    Some(DateTime::<Utc>::from(modified).to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn truncate_title(text: &str) -> String {
    text.trim().chars().take(TITLE_MAX_CHARS).collect()
}

/// 从 transcript 头部文本提取首条用户发言(供本地与 SSH 远程 SFTP 读头部共用)。
pub fn agy_first_user_text_from_str(head: &str) -> Option<String> {
    head.lines()
        .take(TITLE_HEAD_LINES)
        .filter_map(parse_agy_step)
        .find_map(|step| step.user_text())
}

/// 索引标题 → transcript 头部首条用户发言 → `Untitled`(供本地与 SSH 远程共用)。
pub fn agy_session_title_from_head(c: &AgyConversation, transcript_head: Option<&str>) -> String {
    let indexed = c.display_title();
    if !indexed.is_empty() {
        return truncate_title(indexed);
    }
    transcript_head
        .and_then(agy_first_user_text_from_str)
        .map(|text| truncate_title(&text))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| UNTITLED.to_string())
}

/// 索引标题 → transcript 首条用户发言 → `Untitled`。
fn session_title(c: &AgyConversation, transcript: &Path) -> String {
    let indexed = c.display_title();
    if !indexed.is_empty() {
        return truncate_title(indexed);
    }
    first_user_text(transcript)
        .map(|text| truncate_title(&text))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| UNTITLED.to_string())
}

fn first_user_text(transcript: &Path) -> Option<String> {
    let file = fs::File::open(transcript).ok()?;
    BufReader::new(file)
        .split(b'\n')
        .map_while(Result::ok)
        .take(TITLE_HEAD_LINES)
        .filter_map(|bytes| parse_agy_step(&String::from_utf8_lossy(&bytes)))
        .find_map(|step| step.user_text())
}

/// transcript 尾部最近一次模型调用(`PLANNER_RESPONSE.model`)用的模型。只读尾窗,
/// 不整读(长会话的 transcript 可达数 MB)。
fn latest_agy_model(transcript: &Path) -> Option<String> {
    let mut file = fs::File::open(transcript).ok()?;
    let len = file.metadata().ok()?.len();
    for window in MODEL_TAIL_WINDOWS {
        let start = len.saturating_sub(window);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut bytes = Vec::with_capacity(len.min(window) as usize);
        file.by_ref().take(window).read_to_end(&mut bytes).ok()?;
        // 尾窗起点可能切进半行 / 多字节字符,那半行 JSON 解析失败自然跳过
        if let Some(model) = latest_model_in(&String::from_utf8_lossy(&bytes)) {
            return Some(model);
        }
        if start == 0 {
            break;
        }
    }
    None
}

fn latest_model_in(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .filter(|line| line.contains("PLANNER_RESPONSE"))
        .filter_map(parse_agy_step)
        .filter(|step| step.kind == AgyStepKind::PlannerResponse)
        .find_map(|step| step.model.filter(|m| !m.trim().is_empty()))
}

/// transcript 里只有 `USER_INPUT` / `PLANNER_RESPONSE` 两种步可能是对话正文(口径见
/// 模块注释)。先按子串预筛再整行解析:`GENERIC` 步装着工具输出,动辄几 MB,镜像换绑
/// 要把整份 transcript 从头过一遍,不该为它们付全量 JSON 解析。预筛只会放过、不会
/// 误杀(类型名必然逐字出现在行里),放过的由解析后的类型再判一次。
pub fn agy_dialogue_step_from_line(line: &str) -> Option<AgyStep> {
    if !(line.contains("USER_INPUT") || line.contains("PLANNER_RESPONSE")) {
        return None;
    }
    parse_agy_step(line).filter(|s| {
        matches!(
            s.kind,
            AgyStepKind::UserInput | AgyStepKind::PlannerResponse
        )
    })
}

/// 一步 → 一条对话消息(口径见模块注释)。不是对话 / 正文为空 → `None`。
/// 桌面历史正文与移动端镜像共用。
pub fn agy_message_from_step(step: &AgyStep) -> Option<AiSessionMessage> {
    let (role, content) = match step.kind {
        AgyStepKind::UserInput => ("user", step.user_text()?),
        AgyStepKind::PlannerResponse => (
            "assistant",
            step.content
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())?
                .to_string(),
        ),
        _ => return None,
    };
    Some(AiSessionMessage {
        role: role.to_string(),
        content,
        timestamp: step.created_at.clone().unwrap_or_default(),
    })
}

pub(super) fn read_agy_session_content(session_id: &str) -> Result<Vec<AiSessionMessage>, String> {
    let path = find_agy_transcript(session_id).ok_or_else(|| "未找到 agy 会话文件".to_string())?;
    read_agy_messages(&path)
}

/// 读取 WSL 发行版内的 agy 会话正文:逐 candidate home 定位 `brain/<id>/…/transcript_full.jsonl`。
pub(super) fn read_wsl_agy_session_content(
    distro: &str,
    session_id: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    let homes = wsl_candidate_homes(distro);
    read_agy_session_content_in_homes(&homes, session_id)
}

/// [`read_wsl_agy_session_content`] 的可测核心。
pub(super) fn read_agy_session_content_in_homes(
    homes: &[PathBuf],
    session_id: &str,
) -> Result<Vec<AiSessionMessage>, String> {
    for home in homes {
        let app_data_dir = home.join(".gemini").join("antigravity-cli");
        if let Some(path) = agy_transcript_path_in(&app_data_dir, session_id)
            && path.is_file()
        {
            return read_agy_messages(&path);
        }
    }
    Err("未找到 agy 会话文件".to_string())
}

fn read_agy_messages(path: &Path) -> Result<Vec<AiSessionMessage>, String> {
    let file = fs::File::open(path).map_err(|e| format!("无法打开文件: {e}"))?;
    // 按字节切行再 lossy 解码:坏 UTF-8 只坏那一行,不中断迭代(同 pi 的读法)
    Ok(BufReader::new(file)
        .split(b'\n')
        .map_while(Result::ok)
        .filter_map(|bytes| agy_dialogue_step_from_line(&String::from_utf8_lossy(&bytes)))
        .filter_map(|step| agy_message_from_step(&step))
        .collect())
}

/// 镜像启发式(无 hook 身份)用:项目里最新修改的 agy 主会话 transcript。
/// 子代理会话与项目同工作区,跑着的时候它的 transcript 比主会话新,必须滤掉,
/// 否则 pane 会被绑到子代理的任务记录上。
pub fn newest_agy_transcript(project_path: &str) -> Option<(PathBuf, SystemTime)> {
    let dir = agy_app_data_dir()?;
    newest_agy_transcript_in(&dir, &read_agy_conversations(), project_path)
}

pub(super) fn newest_agy_transcript_in(
    app_data_dir: &Path,
    conversations: &[AgyConversation],
    project_path: &str,
) -> Option<(PathBuf, SystemTime)> {
    conversations
        .iter()
        .filter(|c| !c.is_subagent() && c.belongs_to(project_path))
        .filter_map(|c| {
            let path = agy_transcript_path_in(app_data_dir, &c.id)?;
            let modified = path.metadata().and_then(|m| m.modified()).ok()?;
            Some((path, modified))
        })
        .max_by_key(|(_, modified)| *modified)
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-19T13:54:00Z","content":"<USER_REQUEST>\nReply with exactly one word: hello\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-11-19T21:54:00+08:00.\n</ADDITIONAL_METADATA>"}"#;
    const REPLY: &str = r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-19T13:54:01Z","model":"gemini-3.8-flash-high","input_tokens":9986,"cache_read_tokens":0,"output_tokens":79,"content":"hello"}"#;
    const TOOLS_ONLY: &str = r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-19T13:54:02Z","model":"gemini-3.8-pro","thinking":"…","tool_calls":[{"name":"run_command","args":{"CommandLine":"echo hi"}}]}"#;
    const RESULT: &str = r#"{"step_index":3,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-11-19T13:54:03Z","content":"Created At: x\nCompleted At: x\nCommand output:\nhi USER_INPUT PLANNER_RESPONSE"}"#;
    const SYSTEM: &str = r#"{"step_index":4,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-11-19T13:54:04Z","content":"<SYSTEM_MESSAGE>sender=x content=y</SYSTEM_MESSAGE>"}"#;
    const PARTIAL: &str = r#"{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-19T13:54:17Z","model":"gemini-3.8-flash-high","content":"1\n2\n3"}"#;
    const ABORT: &str = r#"{"step_index":6,"source":"SYSTEM","type":"ERROR_MESSAGE","status":"DONE","error":"The stream was interrupted. Please continue the task you were working on.","created_at":"2026-11-19T13:54:20Z","content":"Error: The stream was interrupted. Please continue the task you were working on."}"#;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "mt-agy-hist-{tag}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        /// 在 `brain/<id>/…/transcript_full.jsonl` 写一份 transcript,返回其路径。
        fn transcript(&self, id: &str, lines: &[&str]) -> PathBuf {
            let path = agy_transcript_path_in(&self.0, id).unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, lines.join("\n") + "\n").unwrap();
            path
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn conv(id: &str, workspace: &str) -> AgyConversation {
        AgyConversation {
            id: id.into(),
            title: String::new(),
            preview: String::new(),
            workspaces: vec![workspace.into()],
            updated_at: None,
            last_user_input_at: None,
            step_count: 3,
            parent_id: None,
            nesting_depth: 0,
        }
    }

    fn texts(messages: &[AiSessionMessage]) -> Vec<(&str, &str)> {
        messages
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect()
    }

    /// 正文只出对话:用户请求块、模型正文(含被打断的半截);纯工具步、工具结果、
    /// 系统注入、打断提示都不出。
    #[test]
    fn 正文只出用户请求与模型正文() {
        let root = TempRoot::new("content");
        let path = root.transcript(
            "c1",
            &[
                USER, REPLY, TOOLS_ONLY, RESULT, SYSTEM, PARTIAL, ABORT, "not json", "",
            ],
        );
        let messages = read_agy_messages(&path).unwrap();
        assert_eq!(
            texts(&messages),
            [
                ("user", "Reply with exactly one word: hello"),
                ("assistant", "hello"),
                ("assistant", "1\n2\n3"),
            ]
        );
        assert_eq!(messages[0].timestamp, "2026-11-19T13:54:00Z");
        assert_eq!(messages[1].timestamp, "2026-11-19T13:54:01Z");
        assert!(read_agy_messages(&root.0.join("missing.jsonl")).is_err());
    }

    /// 预筛只放过两种对话步;GENERIC 正文里出现类型名也会被解析后的类型挡住。
    #[test]
    fn 对话步预筛() {
        assert!(agy_dialogue_step_from_line(USER).is_some());
        assert!(agy_dialogue_step_from_line(TOOLS_ONLY).is_some());
        for line in [RESULT, SYSTEM, ABORT, "garbage USER_INPUT", ""] {
            assert!(agy_dialogue_step_from_line(line).is_none(), "{line}");
        }
        // 纯工具步是对话步,但没有正文 → 不成消息
        let step = agy_dialogue_step_from_line(TOOLS_ONLY).unwrap();
        assert!(agy_message_from_step(&step).is_none());
        // 空白正文同样不出
        let blank = parse_agy_step(r#"{"type":"PLANNER_RESPONSE","content":"  \n "}"#).unwrap();
        assert!(agy_message_from_step(&blank).is_none());
    }

    /// 列表:只收本项目、非子代理、有步骤、transcript 在盘的会话;按时间倒序;
    /// 标题 / 时间 / 模型各自的回落链。
    #[test]
    fn 列表口径() {
        let root = TempRoot::new("list");
        let project = r"D:\Git\proj";

        // 索引有标题;时间取 updated_at;模型取尾部最近一次调用(纯工具步也算)
        let mut titled = conv("titled", "D:/Git/proj");
        titled.title = "One Word Hello Reply".into();
        titled.updated_at = Some("2020-11-19T13:54:20.694Z".into());
        root.transcript("titled", &[USER, REPLY, TOOLS_ONLY, RESULT]);

        // `-p` 会话:标题空、preview 有 → 用 preview;时间回落 last_user_input_at;
        // 在项目子目录里起的也算
        let mut preview = conv("preview", "d:/git/proj/crates/x");
        preview.preview = "Read hello.txt".into();
        preview.last_user_input_at = Some("2020-11-19T13:00:00.000Z".into());
        root.transcript("preview", &[USER, REPLY]);

        // 标题与预览都空 → 首条用户发言;时间全无 → transcript 修改时间;没有模型
        let untitled = conv("untitled", "D:/Git/proj");
        root.transcript("untitled", &[USER]);

        // 都不该出现的几种
        let mut sub = conv("sub", "D:/Git/proj");
        sub.parent_id = Some("titled".into());
        root.transcript("sub", &[SYSTEM, REPLY]);
        let mut empty = conv("empty", "D:/Git/proj");
        empty.step_count = 0;
        root.transcript("empty", &[USER]);
        let no_file = conv("nofile", "D:/Git/proj");
        let other = conv("other", "D:/Git/proj-2");
        root.transcript("other", &[USER]);
        let evil = conv("../evil", "D:/Git/proj");

        let all = [titled, preview, untitled, sub, empty, no_file, other, evil];
        let sessions = agy_sessions_in(&root.0, &all, project);
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        // 修改时间是「现在」,比两条固定时间都新,排最前
        assert_eq!(ids, ["untitled", "titled", "preview"]);

        let by_id = |id: &str| sessions.iter().find(|s| s.id == id).unwrap();
        let s = by_id("titled");
        assert_eq!(s.session_type, "agy");
        assert_eq!(s.title, "One Word Hello Reply");
        assert_eq!(s.timestamp, "2020-11-19T13:54:20.694Z");
        assert_eq!(s.model.as_deref(), Some("gemini-3.8-pro"));
        assert!(s.wsl_distro.is_none() && s.ssh_connection_id.is_none());

        let s = by_id("preview");
        assert_eq!(s.title, "Read hello.txt");
        assert_eq!(s.timestamp, "2020-11-19T13:00:00.000Z");
        assert_eq!(s.model.as_deref(), Some("gemini-3.8-flash-high"));

        let s = by_id("untitled");
        assert_eq!(s.title, "Reply with exactly one word: hello");
        assert!(
            DateTime::parse_from_rfc3339(&s.timestamp).is_ok(),
            "{}",
            s.timestamp
        );
        assert_eq!(s.model, None);
    }

    /// 标题截断到 100 字符;transcript 里也没有用户发言 → Untitled。
    #[test]
    fn 标题截断与兜底() {
        let root = TempRoot::new("title");
        let mut long = conv("long", "D:/p");
        long.title = format!("  {}  ", "长".repeat(TITLE_MAX_CHARS + 30));
        let long_path = root.transcript("long", &[REPLY]);
        assert_eq!(
            session_title(&long, &long_path).chars().count(),
            TITLE_MAX_CHARS
        );
        let bare = conv("bare", "D:/p");
        let bare_path = root.transcript("bare", &[SYSTEM, REPLY]);
        assert_eq!(session_title(&bare, &bare_path), UNTITLED);
    }

    /// 尾部那一步大过首个尾窗时放宽一次再找;尾窗切进半行不影响。
    #[test]
    fn 模型取尾部最近一次调用() {
        let root = TempRoot::new("model");
        let big = format!(
            r#"{{"type":"PLANNER_RESPONSE","model":"gemini-big","tool_calls":[{{"name":"write_to_file","args":{{"TargetFile":"a","CodeContent":"{}"}}}}]}}"#,
            "x".repeat(100 * 1024)
        );
        let path = root.transcript("big", &[REPLY, &big, RESULT]);
        assert_eq!(latest_agy_model(&path).as_deref(), Some("gemini-big"));
        let none = root.transcript("none", &[USER, RESULT]);
        assert_eq!(latest_agy_model(&none), None);
        assert_eq!(latest_agy_model(&root.0.join("missing")), None);
    }

    /// 镜像启发式:项目里最新修改的主会话;子代理与别的项目不算。
    #[test]
    fn 镜像启发式取最新主会话() {
        let root = TempRoot::new("newest");
        let old = root.transcript("old", &[USER]);
        let new = root.transcript("new", &[USER]);
        let sub_path = root.transcript("sub", &[SYSTEM]);
        let other_path = root.transcript("other", &[USER]);
        let set_mtime = |path: &Path, secs: u64| {
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime(&old, 1_000);
        set_mtime(&new, 2_000);
        set_mtime(&sub_path, 3_000);
        set_mtime(&other_path, 4_000);
        let mut sub = conv("sub", "D:/p");
        sub.nesting_depth = 1;
        let all = [
            conv("old", "D:/p"),
            conv("new", "D:/p"),
            sub,
            conv("other", "D:/q"),
            conv("ghost", "D:/p"),
        ];
        assert_eq!(
            newest_agy_transcript_in(&root.0, &all, r"D:\p").map(|(p, _)| p),
            Some(new)
        );
        assert_eq!(newest_agy_transcript_in(&root.0, &all, r"D:\none"), None);
    }

    /// WSL 列表:走 POSIX 归属(含子目录与 `/mnt/*` 大小写)、滤子代理与空会话、
    /// 标上 `wsl_distro`、标题与模型照常提取。
    #[test]
    fn wsl_列表按_posix_匹配并标出来源() {
        let root = TempRoot::new("wsl-list");
        let mut exact = conv("wsl-1", "/mnt/d/Git/proj");
        exact.title = "WSL Exact".into();
        exact.updated_at = Some("2026-11-19T14:00:00.000Z".into());
        root.transcript("wsl-1", &[USER, REPLY]);

        // 项目子目录里起的 agy,且索引无标题 → 走 transcript 首条用户发言
        let mut sub_dir = conv("wsl-2", "/mnt/d/git/proj/crates/mt-ai");
        sub_dir.updated_at = Some("2026-11-19T15:00:00.000Z".into());
        root.transcript("wsl-2", &[USER, TOOLS_ONLY]);

        let mut subagent = conv("wsl-sub", "/mnt/d/git/proj");
        subagent.parent_id = Some("wsl-1".into());
        root.transcript("wsl-sub", &[SYSTEM, REPLY]);

        let mut other = conv("wsl-other", "/mnt/d/git/proj-2");
        other.updated_at = Some("2026-11-19T16:00:00.000Z".into());
        root.transcript("wsl-other", &[USER, REPLY]);

        let all = [exact, sub_dir, subagent, other];
        let sessions = wsl_agy_sessions_in(&root.0, &all, "/mnt/d/git/proj", "Ubuntu");
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["wsl-2", "wsl-1"]);
        assert_eq!(sessions[0].wsl_distro.as_deref(), Some("Ubuntu"));
        assert_eq!(sessions[0].ssh_connection_id, None);
        assert_eq!(sessions[0].title, "Reply with exactly one word: hello");
        assert_eq!(sessions[0].model.as_deref(), Some("gemini-3.8-pro"));
        assert_eq!(sessions[1].title, "WSL Exact");
        assert_eq!(sessions[1].model.as_deref(), Some("gemini-3.8-flash-high"));
    }

    /// WSL 多用户 home 正文定位:命中第二个 home 的 transcript;非法 id 拒绝;缺失报错。
    #[test]
    fn wsl_多_home_正文读取与_head_标题兜底() {
        let root = TempRoot::new("wsl-content");
        let home_a = root.0.join("home-a");
        let home_b = root.0.join("home-b");
        fs::create_dir_all(&home_a).unwrap();
        let app_b = home_b.join(".gemini").join("antigravity-cli");
        let path = agy_transcript_path_in(&app_b, "conv-wsl").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("{USER}\n{REPLY}\n{RESULT}\n")).unwrap();

        let homes = vec![home_a, home_b];
        let msgs = read_agy_session_content_in_homes(&homes, "conv-wsl").unwrap();
        assert_eq!(
            texts(&msgs),
            [
                ("user", "Reply with exactly one word: hello"),
                ("assistant", "hello"),
            ]
        );
        assert!(read_agy_session_content_in_homes(&homes, "missing").is_err());
        assert!(read_agy_session_content_in_homes(&homes, "../evil").is_err());

        // 供远程复用的 head 标题提取
        let bare = conv("bare", "/home/u/p");
        assert_eq!(
            agy_session_title_from_head(&bare, Some(&format!("{USER}\n{REPLY}\n"))),
            "Reply with exactly one word: hello"
        );
        assert_eq!(agy_session_title_from_head(&bare, None), UNTITLED);
    }
}
