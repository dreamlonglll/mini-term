//! 输入/输出旁路出来的 AI 会话状态(无 hook 时的降级判定依据)。
//!
//! 原本是 `src-tauri/src/pty.rs` 里 `PtyManager` 的一半字段:AI 会话标记、
//! 行编辑状态机、Ctrl+C 双击窗口、Enter 后的输出扫描窗口、TUI 重绘冷却、
//! 最近输出时刻。PTY 只管字节进出,这些全是 AI 感知的私产,随迁移整块搬来。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

// 这批表被 GPUI 主线程(每次击键/每批 PTY 输出)、500ms 轮询线程与 hook HTTP 线程
// 共同持有:std::sync::Mutex 只要有一个持锁者 panic 就整把锁中毒,其余线程下一次
// lock 全部跟着 panic。parking_lot 没有中毒概念,与 mt-pty/mt-terminal 同款。
use parking_lot::Mutex;

use crate::detect::{
    line_ai_command_name, output_ai_command_name, AI_EXIT_COMMANDS,
};

/// 连续两次 Ctrl+C 退出的时间窗口
const DOUBLE_CTRLC_WINDOW: Duration = Duration::from_millis(1000);

/// 按下 Enter 后扫描输出以检测 AI 命令 echo 的时间窗口
const AI_ENTER_SCAN_WINDOW: Duration = Duration::from_millis(2000);

/// PTY resize 后的 TUI 重绘冷却窗口
///
/// 窗口内的 PTY 输出不刷新 last_output。用于屏蔽 Claude/Codex/OpenCode 等 TUI
/// 应用在收到 ConPTY resize 信号后重绘 Alternate Screen Buffer 产生的伪输出,
/// 避免状态判定把这些重绘误判为 AI 活跃,导致 ai-working 状态闪烁以及
/// 误触发 ai-working → ai-idle 的"任务完成"通知。
pub const RESIZE_COOLDOWN: Duration = Duration::from_millis(800);

/// 终端焦点切换后的 TUI 重绘冷却窗口
///
/// TUI 开启 DEC 私有模式 1004 (sendFocus) 后,终端会在获得/失去焦点时向 PTY
/// 写入 CSI I / CSI O。Claude/Codex/OpenCode 等应用收到这些焦点事件后会做局部
/// 重绘(光标/状态反馈),产生伪输出。若不加冷却,重绘数据会刷新 last_output,
/// 被状态判定误判为 AI 活跃,导致仅仅点击/切出终端就把 ai-idle 推成 ai-working。
///
/// 与 RESIZE_COOLDOWN 对齐为 800ms:AI 进程调度延迟在慢机器/WSL 下并不比 ConPTY
/// resize 响应更可控,保守对齐更稳妥。
pub const FOCUS_COOLDOWN: Duration = Duration::from_millis(800);

/// 终端焦点事件的 CSI 序列(终端在 sendFocus 模式下写入 PTY)
pub(crate) const FOCUS_IN_SEQ: &str = "\x1b[I";
pub(crate) const FOCUS_OUT_SEQ: &str = "\x1b[O";

/// AI 会话内用户提交的一行(供上层做用量统计/移动端回显)
#[derive(Clone, Debug, PartialEq)]
pub struct UserSubmit {
    pub line: String,
    pub ts: i64,
    /// 正文是**从屏幕上猜的**,不是从本地输入缓冲抓的。
    ///
    /// TUI 自己往输入框里回填内容时(Esc 撤回上一条重发、↑ 召回历史、命令菜单
    /// 选中),终端只收到一个裸 Enter,缓冲是空的,只能拿可见行当候选
    /// (见 [`crate::detect::snapshot_submit_text`])。
    ///
    /// ⚠️ **为真时这条不可当真**:权限审批框、`/model` 菜单里按 Enter 同样会走到
    /// 这条路上。上层必须先拿它去屏幕上验明正身才能展示给用户。
    pub from_snapshot: bool,
}

#[derive(Clone)]
enum EscapeState {
    None,
    Escape,
    Csi(String),
    Ss3,
}

impl Default for EscapeState {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Clone, Default)]
struct InputState {
    line: Vec<char>,
    cursor: usize,
    escape: EscapeState,
    bracketed_paste: bool,
    allow_line_snapshot: bool,
}

impl InputState {
    fn clear_line(&mut self) {
        self.line.clear();
        self.cursor = 0;
        self.escape = EscapeState::None;
        self.allow_line_snapshot = false;
    }

    fn insert_char(&mut self, ch: char) {
        self.line.insert(self.cursor, ch);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.line.remove(self.cursor);
    }

    fn delete(&mut self) {
        if self.cursor < self.line.len() {
            self.line.remove(self.cursor);
        }
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        if self.cursor < self.line.len() {
            self.cursor += 1;
        }
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.line.len();
    }

    fn take_line(&mut self) -> String {
        let line = self.line.iter().collect();
        self.clear_line();
        line
    }

    fn apply_csi(&mut self, sequence: &str) {
        match sequence {
            "200~" => self.bracketed_paste = true,
            "201~" => self.bracketed_paste = false,
            "C" => self.move_right(),
            "D" => self.move_left(),
            "H" | "1~" | "7~" => self.move_home(),
            "F" | "4~" | "8~" => self.move_end(),
            "3~" => self.delete(),
            // Up/Down and other editing shortcuts can replace the whole shell line.
            // We can't reconstruct those mutations reliably from input alone.
            "A" | "B" => {
                self.clear_line();
                self.allow_line_snapshot = true;
            }
            _ => self.clear_line(),
        }
    }

    fn apply_ss3(&mut self, code: char) {
        match code {
            'C' => self.move_right(),
            'D' => self.move_left(),
            'H' => self.move_home(),
            'F' => self.move_end(),
            _ => self.clear_line(),
        }
    }

    fn consume_escape_char(&mut self, ch: char) -> bool {
        match &mut self.escape {
            EscapeState::None => false,
            EscapeState::Escape => {
                self.escape = match ch {
                    '[' => EscapeState::Csi(String::new()),
                    'O' => EscapeState::Ss3,
                    _ => {
                        self.clear_line();
                        EscapeState::None
                    }
                };
                true
            }
            EscapeState::Csi(sequence) => {
                sequence.push(ch);
                if ('@'..='~').contains(&ch) {
                    let completed = std::mem::take(sequence);
                    self.escape = EscapeState::None;
                    self.apply_csi(&completed);
                }
                true
            }
            EscapeState::Ss3 => {
                self.escape = EscapeState::None;
                self.apply_ss3(ch);
                true
            }
        }
    }
}

/// 每个 pane 的 AI 旁路状态。内部全是 `Arc<Mutex<…>>`,Clone 即共享同一份。
#[derive(Clone)]
pub struct SessionTracker {
    /// pane → 会话内 AI 命令名("claude"/"codex"/"opencode";hook 扶正时取 hook 的 agent)。
    /// 有键即视为处于 AI 会话,值供前端品牌图标兜底(无 hook 时的唯一 agent 来源)。
    ai_sessions: Arc<Mutex<HashMap<u32, String>>>,
    /// pane → 本轮 AI 会话的启动时刻(enter_ai 时记录)。对话镜像用它过滤
    /// 早于本轮会话的旧记录文件,避免新会话未落盘时错绑上一次会话。
    ai_started: Arc<Mutex<HashMap<u32, SystemTime>>>,
    input_states: Arc<Mutex<HashMap<u32, InputState>>>,
    last_ctrlc: Arc<Mutex<HashMap<u32, Instant>>>,
    last_enter: Arc<Mutex<HashMap<u32, Instant>>>,
    pending_submits: Arc<Mutex<HashMap<u32, Vec<UserSubmit>>>>,
    last_output: Arc<Mutex<HashMap<u32, Instant>>>,
    /// resize / 焦点冷却窗口结束时间:在此之前 PTY 输出不刷新 last_output
    tui_redraw_cooldown_until: Arc<Mutex<HashMap<u32, Instant>>>,
}

impl Default for SessionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionTracker {
    pub fn new() -> Self {
        Self {
            ai_sessions: Arc::new(Mutex::new(HashMap::new())),
            ai_started: Arc::new(Mutex::new(HashMap::new())),
            input_states: Arc::new(Mutex::new(HashMap::new())),
            last_ctrlc: Arc::new(Mutex::new(HashMap::new())),
            last_enter: Arc::new(Mutex::new(HashMap::new())),
            pending_submits: Arc::new(Mutex::new(HashMap::new())),
            last_output: Arc::new(Mutex::new(HashMap::new())),
            tui_redraw_cooldown_until: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 清除某个 pane 的全部 AI 旁路状态(不碰 hook 状态,那由 `HookState::purge`)。
    ///
    /// kill 与「PTY 自然退出」两条路径必须清同一批表:此前自然退出分支漏清,
    /// 用户敲 `exit` 后把 pane 开着不动,这些条目就一直留到手动关 pane 才被回收。
    /// 抽成一处后新增字段不会再漏掉其中一条路径。
    pub fn purge_pane(&self, pane_id: u32) {
        self.ai_sessions.lock().remove(&pane_id);
        self.ai_started.lock().remove(&pane_id);
        self.input_states.lock().remove(&pane_id);
        self.last_ctrlc.lock().remove(&pane_id);
        self.last_enter.lock().remove(&pane_id);
        self.pending_submits.lock().remove(&pane_id);
        self.last_output.lock().remove(&pane_id);
        self.tui_redraw_cooldown_until.lock().remove(&pane_id);
    }

    pub fn has_recent_output(&self, pane_id: u32, within: Duration) -> bool {
        let map = self.last_output.lock();
        map.get(&pane_id).is_some_and(|t| t.elapsed() < within)
    }

    /// 测试用:伪造一次 PTY 输出时间戳。生产路径只由 `note_output` 写入
    /// (且受 TUI 重绘冷却窗口约束),单测里没法真起 PTY。
    #[cfg(test)]
    pub fn note_output_for_test(&self, pane_id: u32) {
        self.last_output
            .lock()
            .insert(pane_id, Instant::now());
    }

    pub fn is_ai_session(&self, pane_id: u32) -> bool {
        self.ai_sessions.lock().contains_key(&pane_id)
    }

    /// 会话内 AI 命令名("claude"/"codex"/…);不在 AI 会话中返回 None。
    pub fn ai_session_agent(&self, pane_id: u32) -> Option<String> {
        self.ai_sessions.lock().get(&pane_id).cloned()
    }

    /// 本轮 AI 会话的启动时刻;不在 AI 会话中返回 None。
    pub fn ai_session_started_at(&self, pane_id: u32) -> Option<SystemTime> {
        self.ai_started.lock().get(&pane_id).copied()
    }

    /// hook 事件证明 AI 进程存活时把会话标记扶正：输入检测漏判启动
    /// （别名/包装脚本，命令行里没有 "claude" 字样）或误判退出（任务运行中
    /// 双击 Ctrl+C 只是打断并不退出）的自愈路径。已标记时幂等 no-op，
    /// 不重置 ai_started（对话镜像按它过滤旧记录，中途重置会错绑）。
    pub fn mark_ai_session(&self, pane_id: u32, agent: &str) {
        let mut sessions = self.ai_sessions.lock();
        if !sessions.contains_key(&pane_id) {
            sessions.insert(pane_id, agent.to_string());
            self.ai_started
                .lock()
                .insert(pane_id, SystemTime::now());
        }
    }

    /// 清除 pane 的 AI 会话标记及相关输入痕迹。
    ///
    /// 输入检测到退出(双击 Ctrl+C / Ctrl+D / 显式退出命令)与 SessionEnd hook
    /// 的权威退出信号都走这里。顺带清 last_enter 关掉 Enter 后的输出扫描窗口:
    /// 否则退出瞬间 ConPTY 重绘把 scrollback 里的 "PS ..> claude" 再吐出来,
    /// 会被扫描误判成命令 echo 又把会话标回去。
    pub fn clear_ai_session(&self, pane_id: u32) {
        self.ai_sessions.lock().remove(&pane_id);
        self.ai_started.lock().remove(&pane_id);
        self.last_ctrlc.lock().remove(&pane_id);
        self.last_enter.lock().remove(&pane_id);
    }

    pub fn drain_submits(&self, pane_id: u32) -> Vec<UserSubmit> {
        self.pending_submits
            .lock()
            .remove(&pane_id)
            .unwrap_or_default()
    }

    /// 延长 TUI 重绘冷却窗口。采用 max 语义,不会缩短已有的更长冷却。
    /// resize 与 focus 共用同一冷却字段(效果一致:抑制 TUI 重绘刷新 last_output)。
    pub fn bump_cooldown(&self, pane_id: u32, duration: Duration) {
        let mut map = self.tui_redraw_cooldown_until.lock();
        let new_until = Instant::now() + duration;
        let final_until = match map.get(&pane_id).copied() {
            Some(old) if old > new_until => old,
            _ => new_until,
        };
        map.insert(pane_id, final_until);
    }

    pub fn is_in_cooldown(&self, pane_id: u32) -> bool {
        self.tui_redraw_cooldown_until
            .lock()
            .get(&pane_id)
            .copied()
            .is_some_and(|until| Instant::now() < until)
    }

    /// 若 data 是终端焦点事件序列(CSI I / CSI O),打开焦点冷却窗口,
    /// 避免 TUI 应用对焦点事件的重绘响应被误判为 AI 活跃。
    pub fn note_focus_event(&self, pane_id: u32, data: &str) {
        if data == FOCUS_IN_SEQ || data == FOCUS_OUT_SEQ {
            self.bump_cooldown(pane_id, FOCUS_COOLDOWN);
        }
    }

    /// PTY 输出旁路:Enter 后 2 秒窗口内扫描命令 echo 补判 AI 会话,
    /// 并（冷却窗口外）刷新最近输出时刻。
    ///
    /// 两件事都只服务于 AI 感知:前者补偿上箭头历史调用 / PSReadLine 补全导致的
    /// 输入检测漏判,后者是无 hook 降级路径与停摆兜底唯一的「活体证据」。
    pub fn note_output(&self, pane_id: u32, data: &str) {
        // 基于输出扫描检测 AI 会话（补偿上箭头历史调用 / PSReadLine 补全）：
        // 若在 Enter 后 2 秒内收到包含 AI 命令 echo 的输出，自动标记为 AI 会话
        {
            let recently_entered = self
                .last_enter
                .lock()
                .get(&pane_id)
                .map(|t| t.elapsed() < AI_ENTER_SCAN_WINDOW)
                .unwrap_or(false);
            if recently_entered {
                let mut sessions = self.ai_sessions.lock();
                if !sessions.contains_key(&pane_id) {
                    if let Some(agent) = output_ai_command_name(data) {
                        sessions.insert(pane_id, agent.to_string());
                    }
                }
            }
        }

        // 冷却窗口内(resize 或 focus 事件后)的输出不刷新 last_output。
        // Claude/Codex 等 TUI 应用在收到 ConPTY resize / 焦点事件后会重绘
        // Alternate Screen Buffer,这些重绘数据不能被状态判定当作 AI 活跃信号,
        // 否则会触发 ai-working 状态闪烁和假完成通知。
        if !self.is_in_cooldown(pane_id) {
            self.last_output.lock().insert(pane_id, Instant::now());
        }
    }

    #[cfg(test)]
    pub fn track_input(&self, pane_id: u32, data: &str) {
        self.track_input_with_line_snapshot(pane_id, data, None);
    }

    pub fn track_input_with_line_snapshot(
        &self,
        pane_id: u32,
        data: &str,
        line_snapshot: Option<&str>,
    ) {
        let in_ai = self.is_ai_session(pane_id);
        let mut enter_ai: Option<&'static str> = None;
        let mut exit_ai = false;
        {
            let mut states = self.input_states.lock();
            let state = states.entry(pane_id).or_default();
            // 单独一个字节的裸 Esc 是**用户按了 Esc 键**,不是转义序列的开头
            // (判据与 [`crate::detect::is_interrupt_key`] 同一条:终端把方向键、
            // 功能键等 CSI 序列一次性交给输入回调,长度一律大于 1)。
            //
            // ⚠️ 当成序列开头的话,状态机会把**紧接着那个字符**当作序列的第二字节
            // 吞掉 —— 而「Esc 撤回上一条、再按 Enter 重发」正是最常见的组合,
            // 被吞掉的就是那个 Enter,整条提交连 Enter 分支都进不去。
            // Esc 在行编辑里的语义本就是清空当前行,当场清掉即可。
            if data == "\x1b" {
                state.clear_line();
                // 裸 Esc 不进/不出 AI 会话(单次 Esc 只是打断,退出由
                // `note_user_interrupt` 与 SessionEnd 那条路管),直接收工
                return;
            }
            for ch in data.chars() {
                if state.consume_escape_char(ch) {
                    continue;
                }
                if ch == '\x1b' {
                    state.escape = EscapeState::Escape;
                    continue;
                }
                if state.bracketed_paste {
                    match ch {
                        '\r' | '\n' => state.insert_char('\n'),
                        c if c >= ' ' => state.insert_char(c),
                        _ => {}
                    }
                    continue;
                }
                match ch {
                    '\x03' => {
                        state.clear_line();
                        if in_ai {
                            // Ctrl+C: 单次取消当前任务，连续两次退出 AI 会话
                            let mut last = self.last_ctrlc.lock();
                            let now = Instant::now();
                            if let Some(prev) = last.get(&pane_id) {
                                if now.duration_since(*prev) < DOUBLE_CTRLC_WINDOW {
                                    exit_ai = true;
                                    last.remove(&pane_id);
                                } else {
                                    last.insert(pane_id, now);
                                }
                            } else {
                                last.insert(pane_id, now);
                            }
                        }
                    }
                    '\x04' => {
                        state.clear_line();
                        if in_ai {
                            // Ctrl+D (EOF) → 退出 AI 会话
                            exit_ai = true;
                        }
                    }
                    '\r' | '\n' => {
                        let allow_line_snapshot = state.allow_line_snapshot;
                        let raw = state.take_line();
                        let trimmed = raw.trim();
                        let snapshot_agent = if allow_line_snapshot {
                            line_snapshot.and_then(line_ai_command_name)
                        } else {
                            None
                        };
                        // 记录 Enter 时间，供输出扫描用。空回车不打开扫描窗口，
                        // 避免 shell autosuggestion 出现在重绘输出中时被当成命令 echo。
                        if !trimmed.is_empty() || snapshot_agent.is_some() {
                            self.last_enter
                                .lock()
                                .insert(pane_id, Instant::now());
                        }
                        if self.is_ai_session(pane_id) {
                            // 缓冲是空的但 pane 在 AI 会话里:内容多半在 TUI 自己
                            // 手上(Esc 撤回重发 / ↑ 召回历史 / 菜单选中),终端这边
                            // 只看得见一个裸 Enter —— 拿可见行当候选,标成「猜的」
                            // 交给上层去屏幕上验明正身(见 UserSubmit::from_snapshot)
                            let submit = if trimmed.is_empty() {
                                line_snapshot
                                    .and_then(crate::detect::snapshot_submit_text)
                                    .map(|line| (line, true))
                            } else {
                                Some((trimmed.to_string(), false))
                            };
                            if let Some((line, from_snapshot)) = submit {
                                let ts = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis() as i64)
                                    .unwrap_or(0);
                                self.pending_submits
                                    .lock()
                                    .entry(pane_id)
                                    .or_default()
                                    .push(UserSubmit {
                                        line,
                                        ts,
                                        from_snapshot,
                                    });
                            }
                        }
                        let cmd = trimmed.to_lowercase();
                        if in_ai {
                            // AI 会话中：识别显式退出命令
                            if AI_EXIT_COMMANDS.iter().any(|&c| cmd == c) {
                                exit_ai = true;
                            }
                        } else {
                            // 非 AI 会话：检测 AI 命令启动。优先使用本地输入状态；
                            // 对上方向键历史、Tab 补全等 shell 改写行的场景，使用
                            // 前端在 Enter 前捕获的可见行快照补判。
                            if let Some(agent) =
                                crate::detect::interactive_ai_command_name(trimmed)
                                    .or(snapshot_agent)
                            {
                                enter_ai = Some(agent);
                            }
                        }
                    }
                    '\t' => {
                        if !state.line.is_empty() {
                            state.allow_line_snapshot = true;
                        }
                    }
                    '\x7f' | '\x08' => {
                        state.backspace();
                    }
                    c if c >= ' ' => state.insert_char(c),
                    _ => {}
                }
            }
        }
        if let Some(agent) = enter_ai {
            self.ai_sessions
                .lock()
                .insert(pane_id, agent.to_string());
            self.ai_started
                .lock()
                .insert(pane_id, SystemTime::now());
        } else if exit_ai {
            self.clear_ai_session(pane_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_claude_command() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn detect_codex_command() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "codex\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn non_ai_command_not_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "npm install\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn prompt_in_ai_session_stays() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        // 在 Claude 内输入提示词不应退出 AI 会话
        mgr.track_input(1, "fix the bug\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn single_ctrl_c_does_not_exit_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        // 单次 Ctrl+C 是取消当前任务，不退出
        mgr.track_input(1, "\x03");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn double_ctrl_c_exits_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        // 连续两次 Ctrl+C 退出 AI 会话
        mgr.track_input(1, "\x03");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "\x03");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn ctrl_d_exits_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "\x04");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn clear_ai_session_resets_state() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        assert!(mgr.ai_session_started_at(1).is_some());
        // SessionEnd 权威退出信号走这里:双击 Ctrl+C 漏检时自愈
        mgr.clear_ai_session(1);
        assert!(!mgr.is_ai_session(1));
        assert!(mgr.ai_session_started_at(1).is_none());
    }

    #[test]
    fn mark_ai_session_rearms_after_false_exit() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        // 任务运行中双击 Ctrl+C 打断:输入检测误判为退出
        mgr.track_input(1, "\x03");
        mgr.track_input(1, "\x03");
        assert!(!mgr.is_ai_session(1));
        // 后续 hook 事件证明 AI 还活着 → 扶正
        mgr.mark_ai_session(1, "claude");
        assert!(mgr.is_ai_session(1));
        assert!(mgr.ai_session_started_at(1).is_some());
    }

    #[test]
    fn mark_ai_session_idempotent_keeps_started_at() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        let started = mgr.ai_session_started_at(1).expect("进入会话应记录启动时刻");
        // 会话已标记时 mark 为 no-op,不得重置 ai_started(镜像按它过滤旧记录)
        mgr.mark_ai_session(1, "claude");
        assert_eq!(mgr.ai_session_started_at(1), Some(started));
    }

    #[test]
    fn exit_clears_enter_scan_window() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        // 会话内提交 prompt 会记录 last_enter(打开输出扫描窗口)
        mgr.track_input(1, "fix the bug\r");
        assert!(mgr.last_enter.lock().contains_key(&1));
        // 双击 Ctrl+C 退出后窗口必须关闭,防止退出重绘把会话标回去
        mgr.track_input(1, "\x03");
        mgr.track_input(1, "\x03");
        assert!(!mgr.is_ai_session(1));
        assert!(!mgr.last_enter.lock().contains_key(&1));
    }

    #[test]
    fn slash_exit_exits_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "/exit\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn exit_command_exits_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "exit\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn slash_quit_exits_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "/quit\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn quit_exits_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "codex\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "quit\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn colon_quit_exits_codex_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "codex\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, ":quit\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn slash_logout_exits_codex_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "codex\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "/logout\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn claude_with_interactive_args() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude --model opus\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn claude_version_not_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude -v\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn claude_long_version_not_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude --version\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn claude_help_not_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude -h\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn claude_print_not_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude -p \"hello\"\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn codex_version_not_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "codex --version\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn codex_help_not_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "codex --help\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn detect_pi_command() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "pi\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn detect_grok_command() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "grok\r");
        assert!(mgr.is_ai_session(1));
        assert_eq!(mgr.ai_session_agent(1).as_deref(), Some("grok"));
    }

    /// `--resume` / `--trust` 是交互式启动（前者恢复会话、后者授信项目 hook），
    /// 不能因为带参数就被当成非交互命令。
    #[test]
    fn grok_with_interactive_args() {
        for cmd in ["grok --resume\r", "grok --trust\r", "grok --resume sid-1\r"] {
            let mgr = SessionTracker::new();
            mgr.track_input(1, cmd);
            assert!(mgr.is_ai_session(1), "{cmd} 应进入 AI 会话");
        }
    }

    #[test]
    fn grok_non_interactive_flags_not_ai_session() {
        for cmd in ["grok -p \"hello\"\r", "grok --version\r", "grok -h\r"] {
            let mgr = SessionTracker::new();
            mgr.track_input(1, cmd);
            assert!(!mgr.is_ai_session(1), "{cmd} 不应进入 AI 会话");
        }
    }

    #[test]
    fn pi_with_interactive_args() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "pi --model claude-sonnet-5\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn pi_non_interactive_flags_not_ai_session() {
        for cmd in ["pi -p \"hello\"\r", "pi --print x\r", "pi -v\r", "pi --help\r"] {
            let mgr = SessionTracker::new();
            mgr.track_input(1, cmd);
            assert!(!mgr.is_ai_session(1), "{cmd} 不应进入 AI 会话");
        }
    }

    /// `pi` 只有两个字母,匹配必须是 basename 全等:任何以 pi 开头的常见命令
    /// (pip / ping / pixi)或同名脚本(pi.py)都不能把 pane 标成 AI 会话。
    #[test]
    fn pi_prefixed_commands_not_ai_session() {
        for cmd in [
            "pip install requests\r",
            "ping example.com\r",
            "pixi run build\r",
            "pi.py\r",
            "python pi.py\r",
        ] {
            let mgr = SessionTracker::new();
            mgr.track_input(1, cmd);
            assert!(!mgr.is_ai_session(1), "{cmd} 不应被认成 pi");
        }
    }

    /// ↑ 召回历史里的 `pi`:输入缓冲是空的,只能靠行快照判定。
    #[test]
    fn pi_from_line_snapshot() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "\x1b[A");
        mgr.track_input_with_line_snapshot(1, "\r", Some("PS D:\\Git\\mini-term> pi"));
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn pip_from_line_snapshot_not_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "\x1b[A");
        mgr.track_input_with_line_snapshot(1, "\r", Some("PS D:\\Git\\mini-term> pip install x"));
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn slash_quit_exits_pi_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "pi\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "/quit\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn backspace_corrects_input() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claue\x7fde\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn empty_enter_keeps_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.is_ai_session(1));
        mgr.track_input(1, "\r");
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn char_by_char_input() {
        let mgr = SessionTracker::new();
        for ch in "claude\r".chars() {
            mgr.track_input(1, &ch.to_string());
        }
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn left_right_arrows_preserve_inline_edit_for_claude() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "clade");
        mgr.track_input(1, "\x1b[D");
        mgr.track_input(1, "\x1b[D");
        mgr.track_input(1, "u");
        mgr.track_input(1, "\x1b[C");
        mgr.track_input(1, "\x1b[C");
        mgr.track_input(1, "\r");
        assert!(mgr.is_ai_session(1));
    }

    /// 转义序列被拆成两次写入时仍要正确处理。
    ///
    /// ⚠️ **拆点不能落在 `\x1b` 之后**:单独一个字节的 `\x1b` 现在被判为「用户按了
    /// Esc 键」(判据见 `track_input_with_line_snapshot` 开头),不再当作序列开头 ——
    /// 否则 Esc 之后紧跟的那个字符会被吞掉。终端本来也不会那样拆:CSI 序列一律
    /// 一次性交给输入回调。
    #[test]
    fn split_escape_sequence_still_moves_cursor() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "clade");
        mgr.track_input(1, "\x1b[");
        mgr.track_input(1, "D");
        mgr.track_input(1, "\x1b[");
        mgr.track_input(1, "D");
        mgr.track_input(1, "u\r");
        assert!(mgr.is_ai_session(1));
    }

    /// 裸 Esc **不吞掉紧接着的那个字符**。
    ///
    /// 回归:此前 `\x1b` 会把状态机推进 Escape 态,下一个字符被当成序列第二字节
    /// 消费掉 —— 「Esc 撤回、Enter 重发」这一组里被吞的正是那个 Enter,整条提交
    /// 连 Enter 分支都进不去,标记列表里什么都没有。
    #[test]
    fn bare_esc_does_not_swallow_next_key() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        mgr.track_input(1, "\x1b");
        // Esc 之后接着键入并提交:一个字符都不能少
        mgr.track_input(1, "fix the bug\r");
        let submits = mgr.drain_submits(1);
        assert_eq!(submits.len(), 1);
        assert_eq!(submits[0].line, "fix the bug", "首字符被吞的话这里会少一个 f");
    }

    /// Esc 清空当前行(行编辑里 Esc 的本义),半截输入不会粘到下一条上。
    #[test]
    fn bare_esc_clears_pending_line() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        mgr.track_input(1, "打了一半");
        mgr.track_input(1, "\x1b");
        mgr.track_input(1, "重新写过\r");
        let submits = mgr.drain_submits(1);
        assert_eq!(submits.len(), 1);
        assert_eq!(submits[0].line, "重新写过");
    }

    #[test]
    fn edited_non_interactive_flag_does_not_start_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude --versin");
        mgr.track_input(1, "\x1b[D");
        mgr.track_input(1, "o\r");
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn drain_submits_returns_empty_initially() {
        let mgr = SessionTracker::new();
        assert!(mgr.drain_submits(1).is_empty());
    }

    #[test]
    fn drain_submits_clears_after_call() {
        let mgr = SessionTracker::new();
        mgr.pending_submits
            .lock()
            .entry(1)
            .or_default()
            .push(UserSubmit {
                line: "test".into(),
                ts: 0,
                from_snapshot: false,
            });
        let first = mgr.drain_submits(1);
        assert_eq!(first.len(), 1);
        let second = mgr.drain_submits(1);
        assert!(second.is_empty());
    }

    #[test]
    fn track_input_does_not_submit_entering_command_itself() {
        // "claude\r" 本身是进入 AI 会话的命令,此时 is_ai_session 还是 false
        // 因为 ai_sessions.insert 发生在 Enter 分支的后续 enter_ai 处理中
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        assert!(mgr.drain_submits(1).is_empty());
        assert!(mgr.is_ai_session(1)); // 但会话状态已建立
    }

    #[test]
    fn ai_session_started_at_follows_session_lifecycle() {
        let mgr = SessionTracker::new();
        // 未进入 AI 会话:无启动时刻
        assert!(mgr.ai_session_started_at(1).is_none());

        let before = SystemTime::now();
        mgr.track_input(1, "claude\r");
        let started = mgr.ai_session_started_at(1).expect("进入会话应记录启动时刻");
        assert!(started >= before && started <= SystemTime::now());

        // Ctrl+D 退出:清除启动时刻(镜像不应再拿旧锚点)
        mgr.track_input(1, "\x04");
        assert!(!mgr.is_ai_session(1));
        assert!(mgr.ai_session_started_at(1).is_none());

        // 同 pane 再次进入:锚点刷新为新一轮的启动时刻
        mgr.track_input(1, "claude\r");
        let restarted = mgr.ai_session_started_at(1).expect("重启会话应重新记录");
        assert!(restarted >= started);
    }

    #[test]
    fn track_input_pushes_submit_in_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        mgr.track_input(1, "fix the bug\r");
        let submits = mgr.drain_submits(1);
        assert_eq!(submits.len(), 1);
        assert_eq!(submits[0].line, "fix the bug");
        assert!(submits[0].ts > 0);
    }

    #[test]
    fn track_input_no_submit_outside_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "npm install\r");
        assert!(mgr.drain_submits(1).is_empty());
    }

    #[test]
    fn track_input_no_submit_on_empty_enter() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        mgr.track_input(1, "\r"); // 空回车
        mgr.track_input(1, "   \r"); // 仅空白
        assert!(mgr.drain_submits(1).is_empty());
    }

    #[test]
    fn track_input_submits_multiple_in_working_window() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        mgr.track_input(1, "first question\r");
        mgr.track_input(1, "follow up\r"); // ai-working 中再次 Enter
        let submits = mgr.drain_submits(1);
        assert_eq!(submits.len(), 2);
        assert_eq!(submits[0].line, "first question");
        assert_eq!(submits[1].line, "follow up");
    }

    #[test]
    fn track_input_no_submit_for_bracketed_multiline_paste() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        mgr.track_input(1, "\x1b[200~first pasted line\nsecond pasted line\x1b[201~");
        assert!(mgr.drain_submits(1).is_empty());
    }

    #[test]
    fn track_input_submits_once_after_bracketed_multiline_paste_enter() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        mgr.track_input(1, "\x1b[200~first pasted line\rsecond pasted line\x1b[201~");
        mgr.track_input(1, "\r");

        let submits = mgr.drain_submits(1);
        assert_eq!(submits.len(), 1);
        assert_eq!(submits[0].line, "first pasted line\nsecond pasted line");
    }

    /// **Esc 撤回重发那条路**:内容被 TUI 退回输入框,用户再按 Enter —— 终端只
    /// 收到一个裸 Enter,行缓冲是空的,内容全程在 agent 进程手里。此时拿可见行
    /// 快照当候选,并标成 `from_snapshot`(上层要验明正身才敢示人)。
    #[test]
    fn bare_enter_in_ai_session_falls_back_to_line_snapshot() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        // 用户按 Esc 打断 → 内容回到输入框 → 直接按 Enter(没有键入任何字符)
        mgr.track_input(1, "\x1b");
        mgr.track_input_with_line_snapshot(1, "\r", Some("│ > 帮我看看这段代码 │"));

        let submits = mgr.drain_submits(1);
        assert_eq!(submits.len(), 1, "裸 Enter 也得记下这条提交");
        assert_eq!(submits[0].line, "帮我看看这段代码", "框线与提示符要剥掉");
        assert!(submits[0].from_snapshot, "必须标成「猜的」");
    }

    /// 键入的内容照旧从**本地缓冲**走,不去碰快照 —— 快照只是裸 Enter 的兜底。
    #[test]
    fn typed_line_is_not_marked_as_snapshot() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        mgr.track_input_with_line_snapshot(1, "fix the bug\r", Some("│ > 屏幕上的别的东西 │"));

        let submits = mgr.drain_submits(1);
        assert_eq!(submits.len(), 1);
        assert_eq!(submits[0].line, "fix the bug", "缓冲有内容就以缓冲为准");
        assert!(!submits[0].from_snapshot);
    }

    /// 空输入框上的裸 Enter 不记 —— 剥完装饰什么都不剩。
    #[test]
    fn bare_enter_with_empty_input_box_submits_nothing() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        for snapshot in [Some("│ > │"), Some("   "), Some(""), None] {
            mgr.track_input_with_line_snapshot(1, "\r", snapshot);
            assert!(
                mgr.drain_submits(1).is_empty(),
                "空输入框不该记提交: {snapshot:?}"
            );
        }
    }

    /// AI 会话**之外**的裸 Enter 一律不记:shell 里空敲回车是最常见的动作,
    /// 拿 prompt 那一行当提交会把标记列表塞满。
    #[test]
    fn bare_enter_outside_ai_session_submits_nothing() {
        let mgr = SessionTracker::new();
        mgr.track_input_with_line_snapshot(1, "\r", Some("PS D:\\Git\\mini-term>"));
        assert!(mgr.drain_submits(1).is_empty());
    }

    #[test]
    fn track_input_no_submit_on_arrow_keys() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "claude\r");
        mgr.track_input(1, "\x1b[A"); // 上方向键
        mgr.track_input(1, "\x1b[B"); // 下方向键
        assert!(mgr.drain_submits(1).is_empty());
    }

    #[test]
    fn line_snapshot_detects_ai_command_after_history_navigation() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "\x1b[A"); // shell/PSReadLine restores a previous command
        mgr.track_input_with_line_snapshot(1, "\r", Some("PS D:\\Git\\mini-term> claude"));
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn empty_enter_with_ai_autosuggestion_snapshot_does_not_enter_ai_session() {
        let mgr = SessionTracker::new();
        mgr.track_input_with_line_snapshot(1, "\r", Some("D:\\Git\\mini-term> claude"));
        assert!(!mgr.is_ai_session(1));
    }

    #[test]
    fn empty_enter_with_ai_autosuggestion_snapshot_does_not_open_output_scan_window() {
        let mgr = SessionTracker::new();
        mgr.track_input_with_line_snapshot(1, "\r", Some("D:\\Git\\mini-term> claude"));
        assert!(!mgr.last_enter.lock().contains_key(&1));
    }

    #[test]
    fn history_navigation_with_non_ai_snapshot_does_not_open_output_scan_window() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "\x1b[B");
        mgr.track_input_with_line_snapshot(1, "\r", Some("D:\\Git\\mini-term>"));
        assert!(!mgr.last_enter.lock().contains_key(&1));
    }

    #[test]
    fn line_snapshot_detects_ai_command_after_tab_completion() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "cla");
        mgr.track_input(1, "\t"); // shell completion changes visible line to "claude"
        mgr.track_input_with_line_snapshot(1, "\r", Some("PS D:\\Git\\mini-term> claude"));
        assert!(mgr.is_ai_session(1));
    }

    #[test]
    fn line_snapshot_respects_non_interactive_ai_flags() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "\x1b[A");
        mgr.track_input_with_line_snapshot(1, "\r", Some("PS D:\\Git\\mini-term> codex --help"));
        assert!(!mgr.is_ai_session(1));
    }

    // ---- 输出旁路(原 pty.rs flush 线程里的两件事) ----

    /// Tab 补全改写了可见行而调用方没给行快照时的最后一道兜底:Enter 后 2 秒内
    /// 看到命令 echo 就把 pane 标成 AI 会话。
    #[test]
    fn output_echo_marks_ai_session_within_enter_window() {
        let mgr = SessionTracker::new();
        mgr.track_input(1, "cla\t"); // 补全把可见行改成 claude,本地缓冲仍是 "cla"
        mgr.track_input(1, "\r"); // 无行快照
        assert!(!mgr.is_ai_session(1));
        // Enter 打开了扫描窗口(输入非空 → last_enter 已记),输出里出现 echo
        mgr.note_output(1, "PS D:\\Git\\mini-term> claude\r\n");
        assert!(mgr.is_ai_session(1));
    }

    /// 没按过 Enter 的 pane 不开扫描窗口:AI 名字随便出现在某次 `cat` 的输出里
    /// 不该把 pane 标成 AI 会话。
    #[test]
    fn output_echo_ignored_without_enter_window() {
        let mgr = SessionTracker::new();
        mgr.note_output(1, "PS D:\\Git\\mini-term> claude\r\n");
        assert!(!mgr.is_ai_session(1));
    }

    /// 冷却窗口内(resize / 焦点事件后)的输出是 TUI 重绘,不算活体证据。
    #[test]
    fn output_in_cooldown_does_not_refresh_activity() {
        let mgr = SessionTracker::new();
        mgr.bump_cooldown(1, RESIZE_COOLDOWN);
        mgr.note_output(1, "redraw");
        assert!(!mgr.has_recent_output(1, Duration::from_secs(3)));

        // 冷却外的输出正常刷新
        let other = SessionTracker::new();
        other.note_output(2, "real output");
        assert!(other.has_recent_output(2, Duration::from_secs(3)));
    }

    #[test]
    fn focus_in_sequence_opens_cooldown() {
        let mgr = SessionTracker::new();
        assert!(!mgr.is_in_cooldown(1));
        mgr.note_focus_event(1, "\x1b[I");
        assert!(mgr.is_in_cooldown(1));
    }

    #[test]
    fn focus_out_sequence_opens_cooldown() {
        let mgr = SessionTracker::new();
        mgr.note_focus_event(1, "\x1b[O");
        assert!(mgr.is_in_cooldown(1));
    }

    #[test]
    fn ordinary_input_does_not_open_cooldown() {
        let mgr = SessionTracker::new();
        mgr.note_focus_event(1, "a");
        mgr.note_focus_event(1, "\r");
        mgr.note_focus_event(1, "ls -la\r");
        assert!(!mgr.is_in_cooldown(1));
    }

    #[test]
    fn arrow_keys_do_not_open_cooldown() {
        let mgr = SessionTracker::new();
        mgr.note_focus_event(1, "\x1b[A");
        mgr.note_focus_event(1, "\x1b[B");
        mgr.note_focus_event(1, "\x1b[C");
        mgr.note_focus_event(1, "\x1b[D");
        assert!(!mgr.is_in_cooldown(1));
    }

    #[test]
    fn focus_event_embedded_in_longer_input_is_not_matched() {
        // 只有严格等于焦点序列才触发冷却,避免用户粘贴的文本意外命中。
        // 终端的 focus event 一定是一条独立的输入回调,不会和其他数据拼接。
        let mgr = SessionTracker::new();
        mgr.note_focus_event(1, "prefix\x1b[Isuffix");
        assert!(!mgr.is_in_cooldown(1));
    }

    #[test]
    fn bump_cooldown_uses_max_semantics() {
        let mgr = SessionTracker::new();
        // 先写一个长冷却,再写短冷却不应缩短它。
        mgr.bump_cooldown(1, Duration::from_secs(10));
        assert!(mgr.is_in_cooldown(1));
        let long_until = mgr
            .tui_redraw_cooldown_until
            .lock()
            .get(&1)
            .copied()
            .unwrap();

        mgr.bump_cooldown(1, Duration::from_millis(50));
        let after_short = mgr
            .tui_redraw_cooldown_until
            .lock()
            .get(&1)
            .copied()
            .unwrap();
        assert_eq!(long_until, after_short, "短冷却不应覆盖更长的已有冷却");
    }

    #[test]
    fn cooldown_is_per_pane() {
        let mgr = SessionTracker::new();
        mgr.note_focus_event(1, "\x1b[I");
        assert!(mgr.is_in_cooldown(1));
        assert!(!mgr.is_in_cooldown(2));
    }

    /// 回归测试(PTY 自然退出漏清旁路状态):purge 必须把每一张表都清空——
    /// 漏掉任何一张,这条断言就红。(SSH autofill 与流控两张表留在 mt-pty,
    /// 它们与 AI 感知无关。)
    #[test]
    fn purge_clears_every_side_table() {
        let mgr = SessionTracker::new();
        let id = 3;

        mgr.track_input(id, "claude\r"); // ai_sessions / ai_started / last_enter / input_states
        mgr.track_input(id, "abc"); // input_states 里留下半行
        mgr.track_input(id, "\x03"); // last_ctrlc
        mgr.note_output_for_test(id);
        mgr.bump_cooldown(id, RESIZE_COOLDOWN);
        mgr.pending_submits
            .lock()
            .entry(id)
            .or_default()
            .push(UserSubmit {
                line: "hi".into(),
                ts: 0,
                from_snapshot: false,
            });

        mgr.purge_pane(id);

        assert!(mgr.last_output.lock().is_empty(), "last_output 未清");
        assert!(mgr.ai_sessions.lock().is_empty(), "ai_sessions 未清");
        assert!(mgr.ai_started.lock().is_empty(), "ai_started 未清");
        assert!(mgr.input_states.lock().is_empty(), "input_states 未清");
        assert!(mgr.last_ctrlc.lock().is_empty(), "last_ctrlc 未清");
        assert!(mgr.last_enter.lock().is_empty(), "last_enter 未清");
        assert!(
            mgr.pending_submits.lock().is_empty(),
            "pending_submits 未清"
        );
        assert!(
            mgr.tui_redraw_cooldown_until.lock().is_empty(),
            "tui_redraw_cooldown_until 未清"
        );
    }
}
