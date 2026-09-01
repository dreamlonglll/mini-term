//! 对话镜像:会话记录(JSONL)的增量解析 → 镜像消息序列。
//!
//! 数据源是 Claude/Codex 会话记录文件,不是终端原始输出(docs/adr/0001)。
//! 移动端按 pane 订阅;桌面端把 pane 绑定到其项目目录下**最新**的会话文件,
//! 轮询增量解析新行并推送。用轮询而非复用 mt-project 的 notify 监听是有意取舍:
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

use mt_ai::sessions as ai_sessions;
use mt_relay_protocol::{MirrorMessage, MirrorQuestionItem, MirrorQuestionOption};

/// 该 agent 是否有本模块能解析的会话记录(Claude / Codex / Grok 三家)。
///
/// 判定与其测试随 `ai_sessions.rs` 一起迁进了 mt-ai(那边是会话记录格式的归属方),
/// 这里 re-export 保留原调用点名字。**红线**:输入检测能认出的 agent 比这宽
/// (pi / opencode 也在 `AI_COMMANDS` 里),它们**没有**可解析的记录文件,调用方
/// 必须据此跳过启发式绑定 —— `resolve_session_file` 只按项目找"最新的
/// claude/codex/grok 记录",对一个 pi pane 调它会把同项目里别家的对话贴到这个
/// pane 上(串台)。宁可空镜像。
pub use mt_ai::sessions::agent_has_session_log;

/// 打开对话默认取最近 50 条,上拉分页每页同量。
pub const MIRROR_PAGE_SIZE: usize = 50;

/// 镜像绑定的会话记录格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorAgent {
    Claude,
    Codex,
    Grok,
}

/// 挂起中的 agent 提问:卡片已下发、尚未见到作答或打断。
/// 仅 Claude 有此形态(AskUserQuestion);Codex/Grok 的审批走 hook 黄灯,不落记录。
struct PendingQuestion {
    /// 提问卡片消息的镜像 seq(移动端作答以此定位)
    seq: u64,
    question: ai_sessions::AiQuestion,
    /// 已点选作答的题数:TUI 逐题推进,只接受按顺序作答下一题
    answered_items: usize,
}

/// [`MirrorParser::answer_keys`] 的产物:注入 PTY 的按键序列 + 选中项文本。
pub struct AnswerKeys {
    /// ↓×option_index + 回车(TUI 高亮从首项起,方向键对单选题普适;
    /// 数字键在各家 TUI 语义不一,不用)
    pub keys: String,
    /// 选中选项的 label(登记为移动端指令原文,镜像回流改标来源用)
    pub label: String,
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
    /// 挂起中的提问(Claude 专用)。换绑即随解析器整体重建,天然清空。
    pending: Vec<PendingQuestion>,
}

impl MirrorParser {
    pub fn new(agent: MirrorAgent) -> Self {
        Self {
            agent,
            next_seq: 0,
            partial: Vec::new(),
            grok: (agent == MirrorAgent::Grok).then(ai_sessions::GrokUpdateParser::new),
            pending: Vec::new(),
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
            self.parse_line(line, &mut out);
        }
        out
    }

    fn parse_line(&mut self, line: &str, out: &mut Vec<MirrorMessage>) {
        match self.agent {
            // Claude/Codex 一行可产出多条(说明文字 + 提问卡片 / 作答标记 + 用户输入)
            MirrorAgent::Claude => self.parse_claude_line(line, out),
            MirrorAgent::Codex => self.parse_codex_line(line, out),
            MirrorAgent::Grok => self.parse_grok_line(line, out),
        }
    }

    /// 来源标注:user = 桌面输入,assistant = AI 回复;与最近移动端指令匹配的
    /// user 消息由 relay::MobileRelayManager::relabel_mobile_sources 改标为 "mobile"
    fn next_text_message(&mut self, raw: ai_sessions::AiSessionMessage) -> MirrorMessage {
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
            ..Default::default()
        };
        self.next_seq += 1;
        msg
    }

    /// Claude 行的完整解析:普通文本之外还产出提问卡片(assistant 行的
    /// AskUserQuestion tool_use)与已作答标记(user 行的 tool_result 对账)。
    fn parse_claude_line(&mut self, line: &str, out: &mut Vec<MirrorMessage>) {
        if let Some(answer) = ai_sessions::claude_question_answer_from_line(line) {
            self.resolve_answer(&answer, out);
        }

        if let Some(raw) = ai_sessions::claude_message_from_line(line) {
            // 真实用户文本(键入/打断标记)= 提问 TUI 必已了结:打断可能不落
            // tool_result,靠这里把挂起清掉,卡片按钮随之失效
            let is_user_text = raw.role == "user";
            let msg = self.next_text_message(raw);
            out.push(msg);
            if is_user_text {
                self.pending.clear();
            }
        }

        if let Some(question) = ai_sessions::claude_question_from_line(line) {
            self.push_question(question, out);
        }
    }

    /// Codex 行的完整解析:与 Claude 同构——提问是 `function_call`
    /// (name=request_user_input),回执是 `function_call_output` 按 call_id 对账。
    fn parse_codex_line(&mut self, line: &str, out: &mut Vec<MirrorMessage>) {
        if let Some(answer) = ai_sessions::codex_question_answer_from_line(line) {
            self.resolve_answer(&answer, out);
        }

        if let Some(raw) = ai_sessions::codex_message_from_line(line) {
            // Codex 把系统注入(`# AGENTS.md`/`<turn_aborted>` 等)写成普通 user
            // 消息行,不进镜像(判据与 codex_user_title_from_line 同源);但仍算
            // 「用户动作」——TUI 已了结,挂起提问照清
            let is_user_text = raw.role == "user";
            if !(is_user_text && is_codex_injected_text(&raw.content)) {
                let msg = self.next_text_message(raw);
                out.push(msg);
            }
            if is_user_text {
                self.pending.clear();
            }
        }

        if let Some(question) = ai_sessions::codex_question_from_line(line) {
            self.push_question(question, out);
        }
    }

    /// Grok 行的完整解析:分片攒消息之外,提问是 tool_call 行
    /// (title=ask_user_question),回执是 tool_call_update 终态行按 call_id 对账。
    fn parse_grok_line(&mut self, line: &str, out: &mut Vec<MirrorMessage>) {
        if let Some(answer) = ai_sessions::grok_question_answer_from_line(line) {
            self.resolve_answer(&answer, out);
        }

        if let Some(raw) = self.grok.as_mut().and_then(|g| g.feed_line(line)) {
            // 分片解析器在边界行才吐出上一条消息:user 消息成形 = TUI 已了结,
            // 挂起照清(与 Claude/Codex 的用户文本清挂起同语义,晚一行到账)
            let is_user_text = raw.role == "user";
            let msg = self.next_text_message(raw);
            out.push(msg);
            if is_user_text {
                self.pending.clear();
            }
        }

        if let Some(question) = ai_sessions::grok_question_from_line(line) {
            // 富化行可能重发同一提问:已挂起即跳过,不重复出卡
            if !self
                .pending
                .iter()
                .any(|p| p.question.tool_use_id == question.tool_use_id)
            {
                self.push_question(question, out);
            }
        }
    }

    /// 作答对账:回执命中挂起提问 → 产出标记并只移除**该**提问。
    /// 不能一刀切清空——提问可能与别的工具调用并行(同一条 assistant 消息里
    /// AskUserQuestion + Bash),别家工具的回执行不代表提问已了结
    fn resolve_answer(&mut self, answer: &ai_sessions::AiQuestionAnswer, out: &mut Vec<MirrorMessage>) {
        let (resolved, still): (Vec<_>, Vec<_>) = self
            .pending
            .drain(..)
            .partition(|p| answer.tool_use_ids.contains(&p.question.tool_use_id));
        self.pending = still;
        for pending in resolved {
            // is_error = Esc 打断/报错的回执,不是用户的选择:labels 留空,
            // 移动端据此显示中性的「已处理」而不是「已作答」
            let labels: Vec<String> = if answer.is_error {
                Vec::new()
            } else {
                pending
                    .question
                    .items
                    .iter()
                    .flat_map(|item| {
                        // Claude 的 answers 按题文对账;Codex 按题目 id
                        answer
                            .answers
                            .get(&item.question)
                            .or_else(|| answer.answers.get(&item.id))
                            .cloned()
                            .unwrap_or_default()
                    })
                    .collect()
            };
            let msg = MirrorMessage {
                seq: self.next_seq,
                // 作答是用户动作;移动端点选过的由 relabel_mobile_sources 改标
                source: "desktop".into(),
                // content 只是旧链路的纯文本兜底;结构化选中项在 labels
                content: labels.join(", "),
                timestamp: answer.timestamp.clone(),
                kind: Some("questionAnswered".into()),
                ref_seq: Some(pending.seq),
                labels,
                ..Default::default()
            };
            self.next_seq += 1;
            out.push(msg);
        }
    }

    /// 产出提问卡片并登记挂起。
    ///
    /// 三家 TUI 都会在选项末尾自动追加一个会话记录里没有的兜底项
    /// (Claude「Other」/Codex「None of the above」/Grok「z. Type your answer here」),
    /// 这里补进卡片与挂起,让移动端也点得到它——追加在尾部,已有选项的下标映射
    /// 不受影响。选中后 TUI 进入文本输入态(Codex 直接提交),移动端用下方
    /// 指令输入框接着打字即可。
    fn push_question(&mut self, mut question: ai_sessions::AiQuestion, out: &mut Vec<MirrorMessage>) {
        let extra_label = match self.agent {
            MirrorAgent::Claude => "Other",
            MirrorAgent::Codex => "None of the above",
            MirrorAgent::Grok => "Type your answer here",
        };
        for item in &mut question.items {
            // 多选题 v1 只展示不可点选,不补
            if !item.multi_select {
                item.options.push(ai_sessions::AiQuestionOption {
                    label: extra_label.to_string(),
                    description: String::new(),
                });
            }
        }
        let questions: Vec<MirrorQuestionItem> = question
            .items
            .iter()
            .map(|item| MirrorQuestionItem {
                question: item.question.clone(),
                header: item.header.clone(),
                options: item
                    .options
                    .iter()
                    .map(|o| MirrorQuestionOption {
                        label: o.label.clone(),
                        description: o.description.clone(),
                    })
                    .collect(),
                multi_select: item.multi_select,
            })
            .collect();
        let msg = MirrorMessage {
            seq: self.next_seq,
            source: "assistant".into(),
            // 纯文本兜底:旧移动端/被旧中转丢掉结构化字段时仍可读
            content: question_fallback_text(&question),
            timestamp: question.timestamp.clone(),
            kind: Some("question".into()),
            questions,
            question_id: Some(question.tool_use_id.clone()),
            ..Default::default()
        };
        self.pending.push(PendingQuestion {
            seq: msg.seq,
            question,
            answered_items: 0,
        });
        self.next_seq += 1;
        out.push(msg);
    }

    /// 移动端点选作答:校验 seq **与提问身份(tool_use id)**指向的提问仍挂起、
    /// 按题序作答、选项下标合法且非多选题,通过则返回注入 PTY 的按键序列。
    /// 身份对账挡住换绑后的 seq 碰撞——新会话恰好在同 seq 也有挂起提问时,
    /// 旧卡片的作答不能记到新提问头上。不推进进度——PTY 写入成功后由调用方
    /// 回调 [`Self::mark_answered`],写失败不脏化进度。
    /// 校验不过返回 None——提问已作答/被打断/镜像已换绑,调用方回执 QuestionNotPending。
    pub fn answer_keys(
        &self,
        seq: u64,
        question_id: &str,
        question_index: u32,
        option_index: u32,
    ) -> Option<AnswerKeys> {
        let pending = self
            .pending
            .iter()
            .find(|p| p.seq == seq && p.question.tool_use_id == question_id)?;
        if question_index as usize != pending.answered_items {
            return None;
        }
        let item = pending.question.items.get(question_index as usize)?;
        if item.multi_select {
            return None;
        }
        let option = item.options.get(option_index as usize)?;
        // grok 的自由输入项(TUI 里的 z 项)不在方向键导航序列里:↓×N 会停在
        // 最后一个编号项、回车把它交出去(踩过)。热键 z 直达输入态,后续
        // 文本由用户经指令输入框补上。该项永远是我们追加在尾部的那一个
        let is_grok_free_input = matches!(self.agent, MirrorAgent::Grok)
            && option_index as usize == item.options.len() - 1;
        let keys = if is_grok_free_input {
            "z".to_string()
        } else {
            let mut keys = "\x1b[B".repeat(option_index as usize);
            keys.push('\r');
            keys
        };
        Some(AnswerKeys {
            keys,
            label: option.label.clone(),
        })
    }

    /// 点选作答的按键已成功写入 PTY:推进该提问的作答进度(逐题推进)。
    pub fn mark_answered(&mut self, seq: u64) {
        if let Some(pending) = self.pending.iter_mut().find(|p| p.seq == seq) {
            pending.answered_items += 1;
        }
    }
}

/// Codex 会话记录里的系统注入 user 行(`<user_instructions>`/`<turn_aborted>`/
/// `# AGENTS.md` 等),判据与 `codex_user_title_from_line` 的跳过规则同源。
fn is_codex_injected_text(content: &str) -> bool {
    let trimmed = content.trim_start();
    trimmed.starts_with('<') || trimmed.starts_with("# AGENTS.md")
}

/// 提问卡片的纯文本兜底:`[header] 题目` + 编号选项,语言中立。
fn question_fallback_text(q: &ai_sessions::AiQuestion) -> String {
    let mut s = String::new();
    for item in &q.items {
        if !s.is_empty() {
            s.push_str("\n\n");
        }
        if !item.header.is_empty() {
            s.push('[');
            s.push_str(&item.header);
            s.push_str("] ");
        }
        s.push_str(&item.question);
        for (i, opt) in item.options.iter().enumerate() {
            s.push_str(&format!("\n{}. {}", i + 1, opt.label));
            if !opt.description.is_empty() {
                s.push_str(" — ");
                s.push_str(&opt.description);
            }
        }
    }
    s
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
        assert!(
            msgs[0].timestamp.starts_with("2027-01-15T"),
            "{}",
            msgs[0].timestamp
        );
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

    // ── agent 提问(AskUserQuestion)镜像 ──

    /// assistant 行:说明文字 + 两题里取一题的 AskUserQuestion。
    fn question_line() -> String {
        r#"{"type":"assistant","timestamp":"2026-09-01T03:30:00Z","message":{"role":"assistant","content":[{"type":"text","text":"先确认方案。"},{"type":"tool_use","id":"toolu_q1","name":"AskUserQuestion","input":{"questions":[{"question":"选哪个?","header":"方案","options":[{"label":"方案A","description":"稳"},{"label":"方案B","description":"快"},{"label":"方案C","description":""}],"multiSelect":false}]}}]}}"#.to_string()
    }

    /// 作答行:tool_result + 顶层 answers 映射。
    fn answer_line() -> String {
        r#"{"type":"user","timestamp":"2026-09-01T03:31:00Z","message":{"role":"user","content":[{"type":"tool_result","content":"answered","tool_use_id":"toolu_q1"}]},"toolUseResult":{"answers":{"选哪个?":"方案B"}}}"#.to_string()
    }

    /// 同一 assistant 行产出说明文字 + 提问卡片两条;卡片带结构化题目与
    /// 纯文本兜底,且随即可点选作答。
    #[test]
    fn parser_emits_question_card_beside_text() {
        let mut parser = MirrorParser::new(MirrorAgent::Claude);
        let msgs = parser.feed(format!("{}\n", question_line()).as_bytes());
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert_eq!(msgs[0].content, "先确认方案。");
        assert_eq!(msgs[0].kind, None);
        let card = &msgs[1];
        assert_eq!(card.seq, 1);
        assert_eq!(card.source, "assistant");
        assert_eq!(card.kind.as_deref(), Some("question"));
        assert_eq!(card.question_id.as_deref(), Some("toolu_q1"));
        assert_eq!(card.questions.len(), 1);
        // 3 个结构化选项 + 追加的「Other」兜底项(TUI 有、记录里没有)
        assert_eq!(card.questions[0].options.len(), 4);
        assert_eq!(card.questions[0].options[3].label, "Other");
        assert!(!card.questions[0].multi_select);
        // 兜底文本:标签 + 题目 + 编号选项(有描述的带描述)
        assert_eq!(
            card.content,
            "[方案] 选哪个?\n1. 方案A — 稳\n2. 方案B — 快\n3. 方案C\n4. Other"
        );

        // 点选第 2 项(下标 1):↓×1 + 回车;写入成功后推进进度
        let keys = parser
            .answer_keys(1, "toolu_q1", 0, 1)
            .expect("提问应挂起可作答");
        assert_eq!(keys.keys, "\x1b[B\r");
        assert_eq!(keys.label, "方案B");
        // 未 mark_answered 前重复校验仍通过(写失败可重试)
        assert!(parser.answer_keys(1, "toolu_q1", 0, 1).is_some());
        parser.mark_answered(1);
        // 单题作答完毕:题序推进后不再接受
        assert!(parser.answer_keys(1, "toolu_q1", 0, 0).is_none());
    }

    /// tool_result 回流:产出已作答标记(指回卡片 seq,选中项在结构化 labels,
    /// content 只是兜底文本),挂起随之清除。
    #[test]
    fn parser_marks_question_answered_on_tool_result() {
        let mut parser = MirrorParser::new(MirrorAgent::Claude);
        parser.feed(format!("{}\n", question_line()).as_bytes());
        let msgs = parser.feed(format!("{}\n", answer_line()).as_bytes());
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        let marker = &msgs[0];
        assert_eq!(marker.kind.as_deref(), Some("questionAnswered"));
        assert_eq!(marker.ref_seq, Some(1));
        assert_eq!(marker.labels, vec!["方案B".to_string()]);
        assert_eq!(marker.content, "方案B");
        assert_eq!(marker.source, "desktop");
        assert!(
            parser.answer_keys(1, "toolu_q1", 0, 0).is_none(),
            "已作答不可再点选"
        );
    }

    /// 与提问并行的**别家工具**回执行不代表提问已了结:不能清挂起,
    /// 否则桌面还在等作答、移动端却只能收 QuestionNotPending。
    #[test]
    fn parser_keeps_pending_on_unrelated_tool_result() {
        let mut parser = MirrorParser::new(MirrorAgent::Claude);
        parser.feed(format!("{}\n", question_line()).as_bytes());
        let unrelated = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"file list...","tool_use_id":"toolu_other"}]}}"#;
        let msgs = parser.feed(format!("{unrelated}\n").as_bytes());
        assert!(msgs.is_empty(), "别家回执不产出消息也不发标记: {msgs:?}");
        assert!(
            parser.answer_keys(1, "toolu_q1", 0, 0).is_some(),
            "提问应仍挂起可作答"
        );
    }

    /// Esc 打断落下的 is_error 回执不是用户的选择:标记 labels 留空
    /// (移动端显示中性「已处理」),挂起清除。
    #[test]
    fn parser_marks_interrupted_question_as_handled() {
        let mut parser = MirrorParser::new(MirrorAgent::Claude);
        parser.feed(format!("{}\n", question_line()).as_bytes());
        let interrupted = r#"{"type":"user","timestamp":"2026-09-01T03:33:00Z","message":{"role":"user","content":[{"type":"tool_result","content":"The user doesn't want to proceed","is_error":true,"tool_use_id":"toolu_q1"}]}}"#;
        let msgs = parser.feed(format!("{interrupted}\n").as_bytes());
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind.as_deref(), Some("questionAnswered"));
        assert!(msgs[0].labels.is_empty(), "打断不该伪装成有选中项");
        assert_eq!(msgs[0].content, "");
        assert!(parser.answer_keys(1, "toolu_q1", 0, 0).is_none());
    }

    /// 打断也可能只落一条 user 文本(无 tool_result):真实用户文本一律清挂起,
    /// 否则卡片按钮会对着一个已消失的 TUI 注入按键。
    #[test]
    fn parser_clears_pending_on_user_interrupt() {
        let mut parser = MirrorParser::new(MirrorAgent::Claude);
        parser.feed(format!("{}\n", question_line()).as_bytes());
        assert!(parser.answer_keys(1, "toolu_q1", 0, 0).is_some());
        let msgs = parser.feed(
            format!("{}\n", claude_line("user", "算了,先不选", "2026-09-01T03:32:00Z")).as_bytes(),
        );
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind, None, "打断不产出已作答标记");
        assert!(parser.answer_keys(1, "toolu_q1", 0, 0).is_none());
    }

    /// Codex 提问行(request_user_input 的 function_call,arguments 是 JSON 字符串)。
    fn codex_question_line() -> String {
        r#"{"timestamp":"t1","type":"response_item","payload":{"type":"function_call","name":"request_user_input","arguments":"{\"questions\":[{\"header\":\"测试选择\",\"id\":\"test_choice\",\"question\":\"测哪个?\",\"options\":[{\"label\":\"方案A\",\"description\":\"稳\"},{\"label\":\"方案B\",\"description\":\"\"}]}]}","call_id":"call_q1"}}"#.to_string()
    }

    fn codex_user_line(text: &str) -> String {
        format!(
            r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":{}}}]}}}}"#,
            serde_json::to_string(text).unwrap()
        )
    }

    /// Codex 提问出卡 + 点选作答按键与 Claude 同一套(↓×n + 回车)。
    #[test]
    fn parser_codex_question_card_and_answer_keys() {
        let mut parser = MirrorParser::new(MirrorAgent::Codex);
        let msgs = parser.feed(format!("{}\n", codex_question_line()).as_bytes());
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        let card = &msgs[0];
        assert_eq!(card.kind.as_deref(), Some("question"));
        assert_eq!(card.question_id.as_deref(), Some("call_q1"));
        assert_eq!(card.questions.len(), 1);
        // 2 个结构化选项 + 追加的「None of the above」兜底项
        assert_eq!(card.questions[0].options.len(), 3);
        assert_eq!(card.questions[0].options[2].label, "None of the above");
        assert!(!card.questions[0].multi_select);

        let keys = parser
            .answer_keys(0, "call_q1", 0, 1)
            .expect("提问应挂起可作答");
        assert_eq!(keys.keys, "\x1b[B\r");
        assert_eq!(keys.label, "方案B");
        // 兜底项也可点选:↓×2 + 回车
        let none = parser.answer_keys(0, "call_q1", 0, 2).unwrap();
        assert_eq!(none.keys, "\x1b[B\x1b[B\r");
        assert_eq!(none.label, "None of the above");
    }

    /// Esc 打断的回执("aborted by user …")不是用户的选择:标记 labels 留空。
    #[test]
    fn parser_codex_abort_marks_handled() {
        let mut parser = MirrorParser::new(MirrorAgent::Codex);
        parser.feed(format!("{}\n", codex_question_line()).as_bytes());
        let abort = r#"{"timestamp":"t2","type":"response_item","payload":{"type":"function_call_output","call_id":"call_q1","output":"aborted by user after 32.9s"}}"#;
        let msgs = parser.feed(format!("{abort}\n").as_bytes());
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert_eq!(msgs[0].kind.as_deref(), Some("questionAnswered"));
        assert_eq!(msgs[0].ref_seq, Some(0));
        assert!(msgs[0].labels.is_empty(), "打断不该伪装成有选中项");
        assert!(parser.answer_keys(0, "call_q1", 0, 0).is_none());
    }

    /// 模式门禁回执(Default 模式下工具不可用):卡片立即失效,不可再点选。
    #[test]
    fn parser_codex_mode_gate_marks_handled() {
        let mut parser = MirrorParser::new(MirrorAgent::Codex);
        parser.feed(format!("{}\n", codex_question_line()).as_bytes());
        let gate = r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_q1","output":"request_user_input is unavailable in Default mode"}}"#;
        let msgs = parser.feed(format!("{gate}\n").as_bytes());
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].labels.is_empty());
        assert!(parser.answer_keys(0, "call_q1", 0, 0).is_none());
    }

    /// 别家工具(exec 等)的回执不清挂起,与 Claude 侧同语义。
    #[test]
    fn parser_codex_keeps_pending_on_unrelated_output() {
        let mut parser = MirrorParser::new(MirrorAgent::Codex);
        parser.feed(format!("{}\n", codex_question_line()).as_bytes());
        let unrelated = r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_other","output":"ls done"}}"#;
        let msgs = parser.feed(format!("{unrelated}\n").as_bytes());
        assert!(msgs.is_empty(), "{msgs:?}");
        assert!(parser.answer_keys(0, "call_q1", 0, 0).is_some());
    }

    /// 系统注入的 user 行(# AGENTS.md / <turn_aborted>)不进镜像,
    /// 但仍算用户动作——挂起提问照清;真实用户文本正常产出。
    #[test]
    fn parser_codex_filters_injected_noise() {
        let mut parser = MirrorParser::new(MirrorAgent::Codex);
        let noise = format!(
            "{}\n{}\n",
            codex_user_line("# AGENTS.md instructions for D:\\Git\\mini-term\n\n<INSTRUCTIONS>…"),
            codex_user_line("<turn_aborted>\nThe user interrupted…\n</turn_aborted>")
        );
        assert!(parser.feed(noise.as_bytes()).is_empty(), "注入噪声不进镜像");

        // 被滤掉的噪声不消耗 seq:卡片取 seq 0
        parser.feed(format!("{}\n", codex_question_line()).as_bytes());
        assert!(parser.answer_keys(0, "call_q1", 0, 0).is_some());
        let msgs = parser.feed(
            format!(
                "{}\n",
                codex_user_line("<turn_aborted>\n打断标记\n</turn_aborted>")
            )
            .as_bytes(),
        );
        assert!(msgs.is_empty(), "{msgs:?}");
        assert!(
            parser.answer_keys(0, "call_q1", 0, 0).is_none(),
            "注入的打断标记也该清挂起"
        );

        let real = parser.feed(format!("{}\n", codex_user_line("真实输入")).as_bytes());
        assert_eq!(real.len(), 1);
        assert_eq!(real[0].content, "真实输入");
    }

    /// Grok 提问富化行(tool_call_update + variant 判别;形状取自真机 updates.jsonl)。
    /// ⚠️ tool_call 首行是流式草稿,内容可能与 TUI 渲染的整个不同,不作出卡依据。
    fn grok_question_line() -> String {
        r#"{"timestamp":1788249733,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-g1","kind":"other","title":"Ask: 接下来做什么？","rawInput":{"variant":"AskUserQuestion","questions":[{"question":"接下来做什么？","options":[{"label":"继续镜像工作","description":"继续处理改动"},{"label":"其他","description":"请说明"}],"multiSelect":null}]}}}}"#.to_string()
    }

    /// Grok 提问出卡 + 点选作答:结构化选项走 ↓×n+回车,自由输入兜底项走热键 z;
    /// completed 回执从 UserAnswered.message 扫出选中项做高亮对账。
    #[test]
    fn parser_grok_question_card_and_answered_labels() {
        let mut parser = MirrorParser::new(MirrorAgent::Grok);
        let msgs = parser.feed(format!("{}\n", grok_question_line()).as_bytes());
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        let card = &msgs[0];
        assert_eq!(card.kind.as_deref(), Some("question"));
        assert_eq!(card.question_id.as_deref(), Some("call-g1"));
        // 2 个结构化选项 + 追加的「z. Type your answer here」自由输入兜底项
        assert_eq!(card.questions[0].options.len(), 3);
        assert_eq!(card.questions[0].options[2].label, "Type your answer here");

        let keys = parser.answer_keys(0, "call-g1", 0, 0).unwrap();
        assert_eq!(keys.keys, "\r");
        // 自由输入项不吃方向键(↓×N 会停在最后一个编号项误交):注入热键 z
        let free = parser.answer_keys(0, "call-g1", 0, 2).unwrap();
        assert_eq!(free.keys, "z");
        assert_eq!(free.label, "Type your answer here");

        let answered = r#"{"timestamp":1788249746,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-g1","status":"completed","rawOutput":{"type":"AskUserQuestion","UserAnswered":{"message":"User has answered your questions: \"接下来做什么？\"=\"继续镜像工作\". You can now continue."}}}}}"#;
        let marks = parser.feed(format!("{answered}\n").as_bytes());
        assert_eq!(marks.len(), 1, "{marks:?}");
        assert_eq!(marks[0].kind.as_deref(), Some("questionAnswered"));
        assert_eq!(marks[0].ref_seq, Some(0));
        assert_eq!(marks[0].labels, vec!["继续镜像工作".to_string()]);
        assert!(parser.answer_keys(0, "call-g1", 0, 0).is_none());
    }

    /// 首行即出卡(富化行要等提问了结才落盘,等它卡片永远晚到);
    /// 富化行到来时同 call_id 已挂起,不重复出卡;
    /// 无 UserAnswered 的 completed 回执按打断处理(labels 留空)。
    #[test]
    fn parser_grok_no_duplicate_card_and_declined_marks_handled() {
        let mut parser = MirrorParser::new(MirrorAgent::Grok);
        let first = r#"{"timestamp":1788249699,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"call-g1","title":"ask_user_question","rawInput":{"questions":[{"question":"接下来做什么？","options":[{"label":"继续镜像工作","description":"继续处理改动"},{"label":"其他","description":"请说明"}]}]}}}}"#;
        let msgs = parser.feed(format!("{first}\n").as_bytes());
        assert_eq!(msgs.len(), 1, "首行即出卡: {msgs:?}");
        assert_eq!(msgs[0].kind.as_deref(), Some("question"));
        assert!(
            parser
                .feed(format!("{}\n", grok_question_line()).as_bytes())
                .is_empty(),
            "富化行不重复出卡"
        );

        let declined = r#"{"timestamp":1788249750,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"call-g1","status":"completed","rawOutput":{"type":"AskUserQuestion"}}}}"#;
        let marks = parser.feed(format!("{declined}\n").as_bytes());
        assert_eq!(marks.len(), 1);
        assert!(marks[0].labels.is_empty(), "拒答/打断不该伪装成有选中项");
        assert!(parser.answer_keys(0, "call-g1", 0, 0).is_none());
    }

    /// Grok 用户消息(分片攒出)成形即清挂起;正文消息照常产出。
    #[test]
    fn parser_grok_user_message_clears_pending() {
        let mut parser = MirrorParser::new(MirrorAgent::Grok);
        parser.feed(format!("{}\n", grok_question_line()).as_bytes());
        assert!(parser.answer_keys(0, "call-g1", 0, 0).is_some());
        let chunk = r#"{"timestamp":1788249800,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"算了,先看代码"}}}}"#;
        let boundary = r#"{"timestamp":1788249801,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"ok"}}}}"#;
        let msgs = parser.feed(format!("{chunk}\n{boundary}\n").as_bytes());
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert_eq!(msgs[0].content, "算了,先看代码");
        assert!(
            parser.answer_keys(0, "call-g1", 0, 0).is_none(),
            "用户消息成形即清挂起"
        );
    }

    /// 作答校验的负路径:错 seq / 身份不符 / 越界选项 / 乱序题号 / 多选题一律拒绝。
    #[test]
    fn answer_keys_rejects_invalid_requests() {
        let mut parser = MirrorParser::new(MirrorAgent::Claude);
        parser.feed(format!("{}\n", question_line()).as_bytes());
        assert!(
            parser.answer_keys(0, "toolu_q1", 0, 0).is_none(),
            "seq 0 是文字消息不是提问"
        );
        assert!(parser.answer_keys(99, "toolu_q1", 0, 0).is_none(), "不存在的 seq");
        assert!(
            parser.answer_keys(1, "toolu_stale", 0, 0).is_none(),
            "提问身份不符(换绑后 seq 碰撞的防线)"
        );
        // 下标 3 是追加的「Other」兜底项,合法;4 才越界
        assert!(parser.answer_keys(1, "toolu_q1", 0, 3).is_some(), "Other 兜底项可点选");
        assert!(parser.answer_keys(1, "toolu_q1", 0, 4).is_none(), "选项越界");
        assert!(parser.answer_keys(1, "toolu_q1", 1, 0).is_none(), "题号越界/乱序");

        // 多选题只展示不可点选
        let mut parser = MirrorParser::new(MirrorAgent::Claude);
        let multi = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_m","name":"AskUserQuestion","input":{"questions":[{"question":"启用哪些?","header":"","options":[{"label":"甲","description":""},{"label":"乙","description":""}],"multiSelect":true}]}}]}}"#;
        let msgs = parser.feed(format!("{multi}\n").as_bytes());
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind.as_deref(), Some("question"));
        assert!(parser.answer_keys(0, "toolu_m", 0, 0).is_none(), "多选题不可点选");
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
                ..Default::default()
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
