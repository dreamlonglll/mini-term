//! AI 感知的装配层:把 hook 状态、旁路会话状态、状态发射器三者绑在一起,
//! 对外只暴露两个观察入口。
//!
//! 上层(mt-app)负责把 PTY 两个方向的字节各旁路一份进来:
//!
//! ```text
//! 用户键入 ─┬─→ mt-pty 写进子进程
//!           └─→ AiPerception::observe_input   (AI 命令识别 / 退出识别 / 打断收敛)
//!
//! 子进程输出 ┬─→ mt-terminal 解析成 grid
//!            └─→ AiPerception::observe_output (命令 echo 回扫 / 输出活跃度)
//! ```
//!
//! 本 crate **不依赖 mt-pty、不依赖 gpui**:PTY 不该知道 AI 的存在,AI 也不该
//! 知道渲染的存在。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::control::ControlPlane;
use crate::detect::is_interrupt_key;
use crate::hook_server::{self, HookState};
use crate::monitor::{self, StatusEmitter, StatusSink};
use crate::tracker::{SessionTracker, UserSubmit, RESIZE_COOLDOWN};

/// AI 感知的全部运行时状态。内部字段都是 `Arc` 共享,Clone 即同一份。
#[derive(Clone)]
pub struct AiPerception {
    tracker: SessionTracker,
    hooks: HookState,
    emitter: StatusEmitter,
    /// 编排控制面。与 AI 感知没有逻辑耦合,住在这里只因为它与 hook 共用那个
    /// 本地 HTTP 服务 —— 装配层顺手把它一并传进 `start_hook_server`,
    /// 上层不必为编排再记一个句柄。
    control: ControlPlane,
}

impl AiPerception {
    /// `sink` 是状态变化的去处(GPUI 模型 / 移动端中转 / 测试收集器)。
    pub fn new(sink: Arc<dyn StatusSink>) -> Self {
        Self {
            tracker: SessionTracker::new(),
            hooks: HookState::new(),
            emitter: StatusEmitter::new(sink),
            control: ControlPlane::new(),
        }
    }

    pub fn tracker(&self) -> &SessionTracker {
        &self.tracker
    }

    pub fn hooks(&self) -> &HookState {
        &self.hooks
    }

    /// 编排控制面(令牌授予/撤销、宿主注入)。
    pub fn control(&self) -> &ControlPlane {
        &self.control
    }

    pub fn emitter(&self) -> &StatusEmitter {
        &self.emitter
    }

    /// 写向 PTY 的字节旁路一份进来。
    ///
    /// **要在把字节交给 PTY 之前调**:焦点冷却窗口必须早于 TUI 对焦点事件的
    /// 重绘响应抵达,否则那波重绘会被当成 AI 活跃。
    pub fn observe_input(&self, pane_id: u32, bytes: &[u8]) {
        self.observe_input_with_line_snapshot(pane_id, bytes, None);
    }

    /// 带可见行快照的版本:上箭头历史召回 / Tab 补全会让 shell 整行改写,
    /// 本地输入缓冲重建不出来,只能靠调用方在回车前抓一份当前可见行补判。
    pub fn observe_input_with_line_snapshot(
        &self,
        pane_id: u32,
        bytes: &[u8],
        line_snapshot: Option<&str>,
    ) {
        let data = String::from_utf8_lossy(bytes);

        // 打开焦点冷却窗口:AI 对焦点事件的重绘响应几乎立即抵达,
        // 必须早于那之前把冷却建立起来。
        self.tracker.note_focus_event(pane_id, &data);
        self.tracker
            .track_input_with_line_snapshot(pane_id, &data, line_snapshot);

        // 用户打断 AI：Claude 在中断时不发任何 hook 事件（官方文档：`Stop` hooks
        // "don't fire on user interrupts"），状态会一直停在 ai-working。这里补一刀
        // 让它收敛到 ai-idle —— 判定与副作用都在 note_user_interrupt 里，含「只动
        // hook 已启用且正在 ai-working 的 pane」与「cause=Interrupt 不算完成」两道闸。
        // 放在 track_input 之后：双击 Ctrl+C 真退出的场景，紧随其后的 SessionEnd
        // 会把状态进一步落到 idle，这一刀只是让中间那段不至于显示成「工作中」。
        if is_interrupt_key(&data) {
            hook_server::note_user_interrupt(
                &self.hooks,
                &self.emitter,
                pane_id,
                self.tracker.ai_session_agent(pane_id),
            );
        }
    }

    /// 从 PTY 读出的字节旁路一份进来(命令 echo 回扫 + 输出活跃度)。
    pub fn observe_output(&self, pane_id: u32, bytes: &[u8]) {
        let data = String::from_utf8_lossy(bytes);
        self.tracker.note_output(pane_id, &data);
    }

    /// PTY 尺寸变化:打开重绘冷却窗口,别把 TUI 的重绘当成 AI 活跃。
    pub fn note_resize(&self, pane_id: u32) {
        self.tracker.bump_cooldown(pane_id, RESIZE_COOLDOWN);
    }

    /// AI 会话内用户提交的行(上层用于用量统计 / 移动端回显),取走即清空。
    pub fn drain_submits(&self, pane_id: u32) -> Vec<UserSubmit> {
        self.tracker.drain_submits(pane_id)
    }

    /// pane 关闭:清掉它在本 crate 里的一切痕迹(旁路状态 + hook 状态 + 墓碑 +
    /// 编排令牌)。**令牌必须跟着 pane 一起没** —— pane 重开是新身份,
    /// 够不到前世的编排能力(ADR 0003:MVP 不做收养)。
    pub fn pane_closed(&self, pane_id: u32) {
        self.tracker.purge_pane(pane_id);
        self.hooks.purge(pane_id);
        self.control.revoke_pane(pane_id);
    }

    /// 当前状态(不发射,只读)。
    pub fn status_of(&self, pane_id: u32) -> String {
        monitor::resolve_status(&self.hooks, &self.tracker, pane_id)
    }

    /// 当前状态的**成因**(不发射,只读):上一次发给 UI 的那个 hook 事件名
    /// (`Stop` / `PermissionRequest` / `Interrupt` / `Stall` …)。
    ///
    /// 成因只住在发射器的去重表里(`StatusChange::cause` 的来源),`HookState`
    /// 不存它 —— 它是「上一次状态变化是因为什么」,不是状态本身。
    ///
    /// 编排控制面的 `wait` 靠它分出两档终态(attention 的判据是成因而非状态,
    /// 见 `mt_ai::control::PaneLiveness::cause`);托盘黄灯与停摆兜底读的
    /// 也是同一份事实,**不另养一份会漂移的副本**。
    ///
    /// `None` = 无 hook 的降级路径(monitor 一律以无成因发射),或还没发过状态。
    pub fn cause_of(&self, pane_id: u32) -> Option<String> {
        self.emitter.last_cause(pane_id)
    }

    /// 启动 hook HTTP 服务器。`data_dir` 是端口文件的落地目录。
    pub fn start_hook_server(&self, data_dir: PathBuf) -> Result<(), String> {
        hook_server::start_hook_server(
            self.hooks.clone(),
            self.emitter.clone(),
            self.tracker.clone(),
            self.control.clone(),
            data_dir,
        )
    }

    pub fn stop_hook_server(&self, data_dir: &Path) {
        hook_server::stop_hook_server(&self.hooks, data_dir);
    }

    /// 运行时切换 hook server 开关。
    pub fn set_hook_server_enabled(&self, data_dir: &Path, enabled: bool) -> Result<(), String> {
        hook_server::set_hook_server_enabled(
            &self.hooks,
            &self.emitter,
            &self.tracker,
            &self.control,
            data_dir,
            enabled,
        )
    }

    /// 启动 500ms 状态轮询线程。`live_panes` 给出当前还活着的 pane id
    /// (本 crate 不认识 PTY,列表由上层提供)。
    pub fn start_monitor(&self, live_panes: monitor::PaneListFn) {
        monitor::start_monitor(
            self.tracker.clone(),
            self.hooks.clone(),
            self.emitter.clone(),
            live_panes,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::StatusChange;
    use std::sync::Mutex;

    fn perception() -> (AiPerception, Arc<Mutex<Vec<StatusChange>>>) {
        let seen: Arc<Mutex<Vec<StatusChange>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let p = AiPerception::new(Arc::new(move |c: StatusChange| {
            sink_seen.lock().unwrap().push(c);
        }));
        (p, seen)
    }

    /// 输入旁路走完整条链:命令识别 → 无 hook 的降级状态。
    #[test]
    fn observe_input_detects_ai_command() {
        let (p, _) = perception();
        assert_eq!(p.status_of(1), "idle");
        p.observe_input(1, b"claude\r");
        assert_eq!(p.status_of(1), "ai-idle");
    }

    /// 输出旁路:降级路径下近期有输出即 ai-working(3 秒窗口内)。
    #[test]
    fn observe_output_feeds_activity_fallback() {
        let (p, _) = perception();
        p.observe_input(1, b"claude\r");
        p.observe_output(1, b"thinking...");
        assert_eq!(p.status_of(1), "ai-working");
    }

    /// 打断收敛必须经由 observe_input 生效,且结论落盘(第二次不再重复发射)。
    #[test]
    fn observe_input_settles_interrupt_once() {
        let (p, seen) = perception();
        p.observe_input(1, b"claude\r");
        p.hooks().update(1, "ai-working".to_string());

        p.observe_input(1, b"\x1b"); // 裸 Esc
        p.observe_input(1, b"\x1b");

        assert_eq!(p.status_of(1), "ai-idle");
        let causes: Vec<Option<String>> = seen.lock().unwrap().iter().map(|c| c.cause.clone()).collect();
        assert_eq!(causes, vec![Some("Interrupt".to_string())]);
    }

    /// 方向键不是打断键:翻历史不该把「工作中」打灭。
    #[test]
    fn arrow_key_is_not_an_interrupt() {
        let (p, seen) = perception();
        p.observe_input(1, b"claude\r");
        p.hooks().update(1, "ai-working".to_string());

        p.observe_input(1, b"\x1b[A");

        assert_eq!(p.status_of(1), "ai-working");
        assert!(seen.lock().unwrap().is_empty());
    }

    /// resize 冷却窗口内的输出不算活体证据。
    #[test]
    fn resize_cooldown_suppresses_redraw_activity() {
        let (p, _) = perception();
        p.observe_input(1, b"claude\r");
        p.note_resize(1);
        p.observe_output(1, b"\x1b[2Jredraw");
        assert_eq!(p.status_of(1), "ai-idle", "重绘不该被当成 AI 活跃");
    }

    #[test]
    fn pane_closed_clears_everything() {
        let (p, _) = perception();
        p.observe_input(1, b"claude\r");
        p.hooks().update(1, "ai-working".to_string());
        p.pane_closed(1);
        assert_eq!(p.status_of(1), "idle");
        assert!(!p.hooks().is_hook_enabled(1));
    }

    /// 非 UTF-8 字节不得 panic(PTY 里什么都可能来)。
    #[test]
    fn invalid_utf8_bytes_are_lossy_not_fatal() {
        let (p, _) = perception();
        p.observe_input(1, &[0xff, 0xfe]);
        p.observe_output(1, &[0xff, 0xfe]);
    }
}
