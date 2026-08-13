//! 对话镜像:会话记录(JSONL)的增量解析 → 镜像消息序列。
//!
//! 数据源是 Claude/Codex 会话记录文件,不是终端原始输出(docs/adr/0001)。
//! 移动端按 pane 订阅;桌面端把 pane 绑定到其项目目录下**最新**的会话文件,
//! 轮询增量解析新行并推送。用轮询而非复用 fs.rs 的 notify 监听是有意取舍:
//! 镜像除了"文件长大"还要发现"更新的会话文件出现"(换绑),对单文件挂 notify
//! 覆盖不了后者;1s 轮询两种情况一并处理,订阅通常只有一个,代价可忽略。
//!
//! 绑定策略分两层:hook 上报过会话身份(pty→session_id)时精确绑定该会话的
//! 文件,同项目多个 AI pane 各绑各的会话;未启用 hook 时退回"项目最新文件 +
//! AI 启动时刻下限"启发式(此路径保留 v1 限制:多 pane 共同镜像最新会话)。
//! 两层都保证:本轮会话未落盘时(首条消息前)给空镜像,不错绑别的会话。
//! v1 限制:仅本机(Windows 宿主)来源的会话记录。

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use mt_relay_protocol::MirrorMessage;

use crate::ai_sessions;

/// 打开对话默认取最近 50 条,上拉分页每页同量。
pub const MIRROR_PAGE_SIZE: usize = 50;

/// 镜像绑定的会话记录格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorAgent {
    Claude,
    Codex,
    Grok,
}

/// 增量解析器:按字节块喂入,只消费完整行(以 `\n` 结尾),半行留待下一块拼接。
/// seq 在一次绑定内从 0 连续递增——`history_slice` 的下标分页依赖此不变量。
pub struct MirrorParser {
    agent: MirrorAgent,
    next_seq: u64,
    partial: Vec<u8>,
    /// grok 专用:消息被拆成任意多个 chunk 行,要攒到边界才成一条。
    /// 其余两家一行即一条,该状态机不参与。
    grok: Option<ai_sessions::GrokUpdateParser>,
}

impl MirrorParser {
    pub fn new(agent: MirrorAgent) -> Self {
        Self {
            agent,
            next_seq: 0,
            partial: Vec::new(),
            grok: (agent == MirrorAgent::Grok).then(ai_sessions::GrokUpdateParser::new),
        }
    }

    /// 喂入新到的字节,返回其中完整行解析出的镜像消息(噪音行静默跳过)。
    ///
    /// grok 的尾部消息会**滞留**到下一个边界行到达才产出:回合收尾时 grok 会
    /// 落一条 `turn_completed`,它就是边界,所以正常对话不会卡住最后一条;
    /// 真正流式写到一半的那条本就不完整,晚一秒出比碎成几十条强。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<MirrorMessage> {
        self.partial.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.partial.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.partial.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                continue;
            }
            if let Some(m) = self.parse_line(line) {
                out.push(m);
            }
        }
        out
    }

    fn parse_line(&mut self, line: &str) -> Option<MirrorMessage> {
        let raw = match self.agent {
            MirrorAgent::Claude => ai_sessions::claude_message_from_line(line)?,
            MirrorAgent::Codex => ai_sessions::codex_message_from_line(line)?,
            MirrorAgent::Grok => self.grok.as_mut()?.feed_line(line)?,
        };
        // 来源标注:user = 桌面输入,assistant = AI 回复;与最近移动端指令匹配的
        // user 消息由 mobile_relay::relabel_mobile_sources 改标为 "mobile"
        let source = if raw.role == "user" {
            "desktop"
        } else {
            "assistant"
        };
        let msg = MirrorMessage {
            seq: self.next_seq,
            source: source.into(),
            content: raw.content,
            timestamp: raw.timestamp,
        };
        self.next_seq += 1;
        Some(msg)
    }
}

/// 分页取数:`before_seq = None` 取最近 `limit` 条(打开对话的首屏),
/// `Some(s)` 取 seq 严格小于 s 的最近 `limit` 条(上拉加载更早)。
/// 返回 (切片, 是否还有更早)。依赖 `messages[i].seq == i`。
pub fn history_slice(
    messages: &[MirrorMessage],
    before_seq: Option<u64>,
    limit: usize,
) -> (Vec<MirrorMessage>, bool) {
    let end = match before_seq {
        None => messages.len(),
        Some(s) => (s as usize).min(messages.len()),
    };
    let start = end.saturating_sub(limit);
    (messages[start..end].to_vec(), start > 0)
}

fn mtime(path: &Path) -> Option<SystemTime> {
    path.metadata().and_then(|m| m.modified()).ok()
}

/// 项目的最新 Claude 会话文件(Windows 宿主来源)。
fn newest_claude_file(project_path: &str) -> Option<(PathBuf, SystemTime)> {
    let mut newest: Option<(PathBuf, SystemTime)> = None;
    for dir in ai_sessions::find_claude_project_dirs(project_path) {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(t) = mtime(&path) else { continue };
            if newest.as_ref().is_none_or(|(_, cur)| t > *cur) {
                newest = Some((path, t));
            }
        }
    }
    newest
}

/// 项目的最新 Codex 会话文件:按 mtime 从新到旧检查头部 session_meta 的 cwd,
/// 命中即返回(限扫描量,避免历史膨胀拖慢轮询)。
fn newest_codex_file(project_path: &str) -> Option<(PathBuf, SystemTime)> {
    const MAX_SCAN: usize = 30;
    let home = dirs::home_dir()?;
    let sessions_dir = home.join(".codex").join("sessions");
    if !sessions_dir.exists() {
        return None;
    }
    let mut paths = Vec::new();
    ai_sessions::collect_codex_session_paths(&sessions_dir, &mut paths);
    ai_sessions::sort_newest_session_paths(&mut paths, MAX_SCAN);

    let normalized = ai_sessions::normalize_path(project_path);
    for path in paths {
        let Ok(content) = fs::File::open(&path) else {
            continue;
        };
        use std::io::BufRead;
        let reader = std::io::BufReader::new(content);
        for line in reader.lines().take(5) {
            let Ok(line) = line else { continue };
            if let Some(meta) = ai_sessions::codex_meta_from_line(&line) {
                if ai_sessions::normalize_path(&meta.cwd) == normalized {
                    if let Some(t) = mtime(&path) {
                        return Some((path, t));
                    }
                }
                break;
            }
        }
    }
    None
}

/// 丢弃早于 `min_mtime` 的候选:候选取的是项目内 mtime 最大者,它若早于锚点,
/// 项目里就不存在属于本轮会话的文件。锚点为 None(无法确定启动时刻)时不过滤。
fn fresh_since(
    candidate: Option<(PathBuf, SystemTime)>,
    min_mtime: Option<SystemTime>,
) -> Option<(PathBuf, SystemTime)> {
    match (candidate, min_mtime) {
        (Some((_, t)), Some(min)) if t < min => None,
        (candidate, _) => candidate,
    }
}

/// 项目的最新 grok 会话文件(`{组目录}/{session-id}/updates.jsonl`)。
/// grok 一个会话是一整个目录,这里只在项目命中的组目录里逐会话取 updates.jsonl。
fn newest_grok_file(project_path: &str) -> Option<(PathBuf, SystemTime)> {
    let mut newest: Option<(PathBuf, SystemTime)> = None;
    for group in ai_sessions::find_grok_cwd_dirs(project_path) {
        let Ok(entries) = fs::read_dir(&group) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let Some(path) = ai_sessions::grok_updates_path(&dir) else {
                continue;
            };
            let Some(t) = mtime(&path) else { continue };
            if newest.as_ref().is_none_or(|(_, cur)| t > *cur) {
                newest = Some((path, t));
            }
        }
    }
    newest
}

/// 该 agent 是否有本模块能解析的会话记录(Claude / Codex / Grok 三家)。
///
/// 输入检测能认出的 agent 比这宽(pi / opencode 也在 `AI_COMMANDS` 里),它们**没有**
/// 可解析的记录文件。调用方必须据此跳过启发式绑定:`resolve_session_file` 只按项目
/// 找"最新的 claude/codex/grok 记录",对一个 pi pane 调它,会把同项目里别家的对话
/// 贴到这个 pane 上(串台)。宁可空镜像。
///
/// 用 `contains` 而非全等:hook 上报的 agent 是 `claude-code`,输入检测是 `claude`。
pub fn agent_has_session_log(agent: &str) -> bool {
    let agent = agent.to_ascii_lowercase();
    agent.contains("claude") || agent.contains("codex") || agent.contains("grok")
}

/// 解析 pane 所属项目当前应镜像的会话记录:三家里最新修改的那个。
/// `min_mtime` 是本轮 AI 会话的启动时刻:更早的文件属于以前的会话,一律不绑,
/// 宁可先给空镜像等新会话落盘(代价:`--resume` 恢复的旧记录在下一条消息前不显示)。
pub fn resolve_session_file(
    project_path: &str,
    min_mtime: Option<SystemTime>,
) -> Option<(PathBuf, MirrorAgent)> {
    let candidates = [
        (
            fresh_since(newest_claude_file(project_path), min_mtime),
            MirrorAgent::Claude,
        ),
        (
            fresh_since(newest_codex_file(project_path), min_mtime),
            MirrorAgent::Codex,
        ),
        (
            fresh_since(newest_grok_file(project_path), min_mtime),
            MirrorAgent::Grok,
        ),
    ];
    let mut best: Option<(PathBuf, SystemTime, MirrorAgent)> = None;
    for (candidate, agent) in candidates {
        let Some((path, t)) = candidate else { continue };
        // 严格大于:同刻并列时保留先出现的(Claude > Codex > Grok)
        if best.as_ref().is_none_or(|(_, cur, _)| t > *cur) {
            best = Some((path, t, agent));
        }
    }
    best.map(|(path, _, agent)| (path, agent))
}

/// session_id 应为 UUID 形态;拒绝任何可构成路径穿越的字符——hook 端口对
/// 本机所有进程开放,上报的 session_id 不可未经校验直接拼文件路径。
fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// 在给定目录列表中定位 `<session_id>.jsonl`(Claude 会话文件名即 session id)。
fn claude_session_file_in(dirs: &[PathBuf], session_id: &str) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join(format!("{session_id}.jsonl")))
        .find(|p| p.is_file())
}

/// 在 Codex sessions 目录中按头部 session_meta 的 id 定位会话文件
/// (文件名不含 session id,须读 meta;限扫描量同 newest_codex_file)。
fn codex_session_file_in(sessions_dir: &Path, session_id: &str) -> Option<PathBuf> {
    const MAX_SCAN: usize = 30;
    if !sessions_dir.exists() {
        return None;
    }
    let mut paths = Vec::new();
    ai_sessions::collect_codex_session_paths(sessions_dir, &mut paths);
    ai_sessions::sort_newest_session_paths(&mut paths, MAX_SCAN);
    for path in paths {
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        use std::io::BufRead;
        let reader = std::io::BufReader::new(file);
        for line in reader.lines().take(5) {
            let Ok(line) = line else { continue };
            if let Some(meta) = ai_sessions::codex_meta_from_line(&line) {
                if meta.id == session_id {
                    return Some(path);
                }
                break;
            }
        }
    }
    None
}

/// 按 hook 上报的会话身份精确定位记录文件——同项目多个 AI pane 各绑各的
/// 会话,不再共同镜像"项目最新"。文件尚未落盘(首条消息前)返回 None。
pub fn resolve_session_file_by_id(
    project_path: &str,
    agent: Option<&str>,
    session_id: &str,
) -> Option<(PathBuf, MirrorAgent)> {
    if !valid_session_id(session_id) {
        return None;
    }
    let agent_lower = agent.map(|a| a.to_ascii_lowercase()).unwrap_or_default();
    if agent_lower.contains("codex") {
        let sessions_dir = dirs::home_dir()?.join(".codex").join("sessions");
        codex_session_file_in(&sessions_dir, session_id).map(|p| (p, MirrorAgent::Codex))
    } else if agent_lower.contains("grok") {
        let dir = ai_sessions::find_grok_session_dir(project_path, session_id)?;
        ai_sessions::grok_updates_path(&dir).map(|p| (p, MirrorAgent::Grok))
    } else {
        let dirs = ai_sessions::find_claude_project_dirs(project_path);
        claude_session_file_in(&dirs, session_id).map(|p| (p, MirrorAgent::Claude))
    }
}

/// 从 `offset` 读到文件尾。返回 (新字节, 新 offset);文件比 offset 短(被截断/重写)
/// 返回 None,调用方应重新绑定。
pub fn read_from_offset(path: &Path, offset: u64) -> Option<(Vec<u8>, u64)> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len < offset {
        return None;
    }
    if len == offset {
        return Some((Vec::new(), offset));
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::with_capacity((len - offset) as usize);
    file.read_to_end(&mut buf).ok()?;
    let new_offset = offset + buf.len() as u64;
    Some((buf, new_offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_line(role: &str, text: &str, ts: &str) -> String {
        format!(
            r#"{{"type":"{role}","message":{{"role":"{role}","content":[{{"type":"text","text":"{text}"}}]}},"timestamp":"{ts}"}}"#
        )
    }

    /// 只有 Claude/Codex/Grok 有可解析的记录;pi/opencode 必须落在白名单外,
    /// 否则镜像会退启发式绑到同项目别家的会话文件(串台)。
    #[test]
    fn only_claude_codex_and_grok_have_session_logs() {
        for agent in ["claude", "claude-code", "codex", "Codex", "grok", "Grok"] {
            assert!(agent_has_session_log(agent), "{agent} 应有会话记录");
        }
        for agent in ["pi", "opencode", "", "gemini"] {
            assert!(!agent_has_session_log(agent), "{agent} 不应被认为有会话记录");
        }
    }

    /// grok 把一条消息拆成任意多个 chunk 行:必须攒到边界才产出一条,
    /// 否则一句回答会在镜像里碎成几十条。
    #[test]
    fn parser_joins_grok_chunks_into_one_message() {
        let mut parser = MirrorParser::new(MirrorAgent::Grok);
        let chunk = |tag: &str, text: &str, ts: u64| {
            format!(
                r#"{{"timestamp":{ts},"method":"session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"{tag}","content":{{"type":"text","text":"{text}"}}}}}}}}"#
            )
        };
        let data = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n",
            chunk("user_message_chunk", "fix ", 1_800_000_000),
            chunk("user_message_chunk", "the bug", 1_800_000_001),
            // 工具调用是边界:用户消息在此收尾
            r#"{"timestamp":1800000002,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","title":"read_file"}}}"#,
            chunk("agent_message_chunk", "done", 1_800_000_003),
            chunk("agent_message_chunk", " ✅", 1_800_000_004),
            // 回合收尾(xAI 扩展轨)同样是边界:AI 回复在此收尾
            r#"{"timestamp":1800000005,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"turn_completed","stop_reason":"end_turn"}}}"#,
        );
        let msgs = parser.feed(data.as_bytes());
        assert_eq!(msgs.len(), 2, "chunk 未合并: {msgs:?}");
        assert_eq!(msgs[0].source, "desktop");
        assert_eq!(msgs[0].content, "fix the bug");
        assert_eq!(msgs[0].seq, 0);
        assert_eq!(msgs[1].source, "assistant");
        assert_eq!(msgs[1].content, "done ✅");
        assert_eq!(msgs[1].seq, 1);
        // 时间戳取该消息**第一个** chunk 的时刻
        assert!(msgs[0].timestamp.starts_with("2027-01-15T"), "{}", msgs[0].timestamp);
    }

    /// 宿主注入的回合(工具结果/系统提醒)与 `!bash` 直通命令的回显都不是
    /// 用户说的话,镜像里不该出现——与 grok 自身的提示词抽取口径一致。
    #[test]
    fn parser_skips_grok_injected_user_chunks() {
        let mut parser = MirrorParser::new(MirrorAgent::Grok);
        let data = concat!(
            r#"{"timestamp":1,"method":"session/update","params":{"update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"tool result"},"_meta":{"hostTurn":true}}}}"#,
            "\n",
            r#"{"timestamp":2,"method":"session/update","params":{"update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"ls -la","_meta":{"bash_command":"ls -la"}}}}}"#,
            "\n",
            r#"{"timestamp":3,"method":"session/update","params":{"update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"real prompt"}}}}"#,
            "\n",
            r#"{"timestamp":4,"method":"session/update","params":{"update":{"sessionUpdate":"tool_call"}}}"#,
            "\n",
        );
        let msgs = parser.feed(data.as_bytes());
        assert_eq!(msgs.len(), 1, "注入行未被跳过: {msgs:?}");
        assert_eq!(msgs[0].content, "real prompt");
    }

    /// 半行拼接对 grok 同样成立(轮询按字节读,行边界不保证)
    #[test]
    fn parser_handles_grok_partial_line_across_chunks() {
        let mut parser = MirrorParser::new(MirrorAgent::Grok);
        let line = r#"{"timestamp":9,"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"split me"}}}}"#;
        let bytes = format!("{line}\n");
        let (head, tail) = bytes.as_bytes().split_at(bytes.len() / 2);
        assert!(parser.feed(head).is_empty(), "半行不应产出消息");
        // 补齐后仍在缓冲(等边界),再喂一个边界行才收尾
        assert!(parser.feed(tail).is_empty());
        let out = parser.feed(
            b"{\"method\":\"session/update\",\"params\":{\"update\":{\"sessionUpdate\":\"tool_call\"}}}\n",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content, "split me");
        assert_eq!(out[0].seq, 0);
    }

    #[test]
    fn parser_labels_sources_and_skips_noise() {
        let mut parser = MirrorParser::new(MirrorAgent::Claude);
        let data = format!(
            "{}\n{}\n{}\n{}\n",
            claude_line("user", "fix the bug", "2026-07-24T10:00:00Z"),
            r#"{"type":"summary","summary":"noise line"}"#,
            claude_line("assistant", "done", "2026-07-24T10:01:00Z"),
            "not json at all",
        );
        let msgs = parser.feed(data.as_bytes());
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].source, "desktop");
        assert_eq!(msgs[0].content, "fix the bug");
        assert_eq!(msgs[0].seq, 0);
        assert_eq!(msgs[1].source, "assistant");
        assert_eq!(msgs[1].content, "done");
        assert_eq!(msgs[1].seq, 1);
    }

    #[test]
    fn parser_handles_partial_line_across_chunks() {
        let mut parser = MirrorParser::new(MirrorAgent::Claude);
        let line = claude_line("user", "incremental boundary", "2026-07-24T10:00:00Z");
        let bytes = format!("{line}\n");
        let (head, tail) = bytes.as_bytes().split_at(bytes.len() / 2);

        // 第一块只含半行:不产出消息,也不丢字节
        let first = parser.feed(head);
        assert!(first.is_empty(), "半行不应产出消息");

        // 第二块补齐:恰好产出一条,无重复无丢失
        let second = parser.feed(tail);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].content, "incremental boundary");
        assert_eq!(second[0].seq, 0);

        // 后续消息 seq 连续
        let third = parser.feed(format!("{}\n", claude_line("assistant", "ok", "")).as_bytes());
        assert_eq!(third[0].seq, 1);
    }

    #[test]
    fn parser_codex_lines() {
        let mut parser = MirrorParser::new(MirrorAgent::Codex);
        let data = concat!(
            r#"{"type":"session_meta","payload":{"id":"x","cwd":"D:\\proj"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"hello"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"role":"assistant","content":[{"type":"output_text","text":"world"}]}}"#,
            "\n",
        );
        let msgs = parser.feed(data.as_bytes());
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].source, "desktop");
        assert_eq!(msgs[1].source, "assistant");
        assert_eq!(msgs[1].content, "world");
    }

    fn make_messages(n: u64) -> Vec<MirrorMessage> {
        (0..n)
            .map(|i| MirrorMessage {
                seq: i,
                source: "desktop".into(),
                content: format!("m{i}"),
                timestamp: String::new(),
            })
            .collect()
    }

    #[test]
    fn history_slice_first_page_is_latest_50() {
        let msgs = make_messages(120);
        let (page, has_more) = history_slice(&msgs, None, MIRROR_PAGE_SIZE);
        assert_eq!(page.len(), 50);
        assert_eq!(page.first().unwrap().seq, 70);
        assert_eq!(page.last().unwrap().seq, 119);
        assert!(has_more);
    }

    #[test]
    fn history_slice_pages_backwards_until_exhausted() {
        let msgs = make_messages(120);
        let (page2, has_more2) = history_slice(&msgs, Some(70), MIRROR_PAGE_SIZE);
        assert_eq!(page2.first().unwrap().seq, 20);
        assert_eq!(page2.last().unwrap().seq, 69);
        assert!(has_more2);

        let (page3, has_more3) = history_slice(&msgs, Some(20), MIRROR_PAGE_SIZE);
        assert_eq!(page3.len(), 20);
        assert_eq!(page3.first().unwrap().seq, 0);
        assert!(!has_more3);
    }

    #[test]
    fn history_slice_short_history_has_no_more() {
        let msgs = make_messages(10);
        let (page, has_more) = history_slice(&msgs, None, MIRROR_PAGE_SIZE);
        assert_eq!(page.len(), 10);
        assert!(!has_more);

        let (empty, has_more) = history_slice(&msgs, Some(0), MIRROR_PAGE_SIZE);
        assert!(empty.is_empty());
        assert!(!has_more);
    }

    #[test]
    fn valid_session_id_rejects_path_traversal() {
        assert!(valid_session_id("0198c2f4-7e4a-7b3c-9d2e-1f0a2b3c4d5e"));
        assert!(valid_session_id("abc123"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("../../etc/passwd"));
        assert!(!valid_session_id("a/b"));
        assert!(!valid_session_id("a\\b"));
        assert!(!valid_session_id("a.b"));
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mt-mirror-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn claude_session_file_found_by_exact_name() {
        let d1 = temp_dir("claude-a");
        let d2 = temp_dir("claude-b");
        fs::write(d2.join("sid-42.jsonl"), b"{}\n").unwrap();

        let dirs = vec![d1.clone(), d2.clone()];
        // 命中:文件在第二个候选目录里
        assert_eq!(
            claude_session_file_in(&dirs, "sid-42"),
            Some(d2.join("sid-42.jsonl"))
        );
        // 未落盘:返回 None(镜像给空快照,不退回项目最新文件)
        assert!(claude_session_file_in(&dirs, "sid-other").is_none());

        fs::remove_dir_all(&d1).ok();
        fs::remove_dir_all(&d2).ok();
    }

    #[test]
    fn codex_session_file_found_by_meta_id() {
        let root = temp_dir("codex");
        let day = root.join("2026").join("07").join("25");
        fs::create_dir_all(&day).unwrap();
        let meta =
            |id: &str| format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"D:\\\\proj\"}}}}\n");
        fs::write(day.join("rollout-1.jsonl"), meta("sid-first")).unwrap();
        fs::write(day.join("rollout-2.jsonl"), meta("sid-second")).unwrap();

        assert_eq!(
            codex_session_file_in(&root, "sid-first"),
            Some(day.join("rollout-1.jsonl"))
        );
        assert!(codex_session_file_in(&root, "sid-missing").is_none());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn fresh_since_filters_files_older_than_session_start() {
        use std::time::{Duration, UNIX_EPOCH};
        let old = UNIX_EPOCH + Duration::from_secs(1_000);
        let start = UNIX_EPOCH + Duration::from_secs(2_000);
        let new = UNIX_EPOCH + Duration::from_secs(3_000);
        let file = || PathBuf::from("s.jsonl");

        // 早于会话启动的旧文件不绑定(新会话首条消息前应显示空镜像)
        assert!(fresh_since(Some((file(), old)), Some(start)).is_none());
        // 会话启动后落盘的文件正常绑定;恰好等于锚点时刻也算本轮
        assert!(fresh_since(Some((file(), new)), Some(start)).is_some());
        assert!(fresh_since(Some((file(), start)), Some(start)).is_some());
        // 无锚点(无法确定启动时刻)不过滤,退回原行为
        assert!(fresh_since(Some((file(), old)), None).is_some());
        assert!(fresh_since(None, Some(start)).is_none());
    }

    #[test]
    fn read_from_offset_detects_truncation() {
        let dir = std::env::temp_dir().join(format!(
            "mt-mirror-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.jsonl");

        fs::write(&file, b"hello\n").unwrap();
        let (bytes, offset) = read_from_offset(&file, 0).unwrap();
        assert_eq!(bytes, b"hello\n");
        assert_eq!(offset, 6);

        // 追加后从 offset 续读
        let mut f = fs::OpenOptions::new().append(true).open(&file).unwrap();
        use std::io::Write;
        f.write_all(b"world\n").unwrap();
        drop(f);
        let (bytes, offset) = read_from_offset(&file, offset).unwrap();
        assert_eq!(bytes, b"world\n");
        assert_eq!(offset, 12);

        // 文件被截断(重写):返回 None 提示重新绑定
        fs::write(&file, b"x\n").unwrap();
        assert!(read_from_offset(&file, offset).is_none());

        fs::remove_dir_all(&dir).ok();
    }
}
