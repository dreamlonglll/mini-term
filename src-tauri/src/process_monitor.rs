use crate::hook_server::HookState;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyStatusChangePayload {
    pub pty_id: u32,
    pub status: String,
    /// 本次状态变化的成因：hook 直推时是 hook 事件名（`Stop` / `PermissionRequest`
    /// / `SessionEnd` …），monitor 轮询自己算出的变化为 `None`。
    ///
    /// 前端据此区分"任务真做完了"（`Stop`）与"只是又在等用户"（权限请求 /
    /// 通知 / 澄清），两者都落到 `ai-idle`，但只有前者该播报完成。
    pub cause: Option<String>,
}

/// AI 输出活跃超时阈值。**仅**用于无 hook 的降级路径（`resolve_status` 的
/// `else if` 分支）；hook 已启用的 pane 不看输出活跃度。
const AI_ACTIVE_TIMEOUT: Duration = Duration::from_secs(3);

/// `pty-status-change` 的统一发射器：monitor 轮询与 hook server 直推
/// 共用同一份"上次发给前端的状态"去重表。
///
/// 此前两个发射源各自为政（hook 直推不更新 monitor 的 prev_statuses）：
/// AI 退出后迟到的 Stop hook 把前端直推回 ai-idle，而 monitor 自己算出的
/// 纠正值 "idle" 与它的 prev 相同被去重吞掉，前端就永久停在 ai-idle。
/// 比较、记录、emit 收在同一把锁内，保证两个发射源的事件顺序一致。
#[derive(Clone)]
pub struct StatusEmitter {
    prev: Arc<Mutex<HashMap<u32, String>>>,
}

impl StatusEmitter {
    pub fn new() -> Self {
        Self {
            prev: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 与上次发出的状态不同才 emit。`cause` 见 [`PtyStatusChangePayload::cause`]，
    /// 只随事件透传，不参与去重。
    pub fn emit_if_changed(
        &self,
        app: &AppHandle,
        pty_id: u32,
        status: &str,
        cause: Option<&str>,
    ) {
        let mut prev = self.prev.lock().unwrap();
        if prev.get(&pty_id).map(|s| s.as_str()) == Some(status) {
            return;
        }
        prev.insert(pty_id, status.to_string());
        let _ = app.emit(
            "pty-status-change",
            PtyStatusChangePayload {
                pty_id,
                status: status.to_string(),
                cause: cause.map(str::to_string),
            },
        );
    }

    /// 清掉已不存在的 pty 的去重记录
    pub fn retain(&self, alive: &[u32]) {
        self.prev.lock().unwrap().retain(|id, _| alive.contains(id));
    }
}

/// 单个 pty 的状态判定（monitor 每轮对每个 pty 调一次）。
///
/// Hook 一旦启用即为**绝对**权威：状态完全由 hook 事件决定，PTY 输出活跃度
/// 不参与判定。
///
/// 这里曾有一条兜底——"hook 停在 ai-working 但连续 AI_ACTIVE_TIMEOUT 无输出即
/// 视为 ai-idle"，用来对冲 Stop 事件丢失或迟到。它是**无记忆**的：每轮 500ms
/// 重算，降级结果不落盘，hook_status 本身仍是 ai-working。于是只要 hook_status
/// 卡住（Stop 丢失，或 Stop 之后又收到同会话迟到的 PostToolUse——hook 脚本是
/// 独立进程，同会话内事件到达顺序无保证且无时序保护），AI 空闲期每一次零星
/// 伪输出（TUI 定时重绘等）都会把状态抬回 ai-working、3 秒后再落回 ai-idle，
/// 形成以伪输出间隔为周期的脉冲（实测 20~50s 一轮）。前端把每个
/// ai-working → ai-idle 下降沿当作"任务完成"，一次任务因此被反复播报。
///
/// 现在的取舍是宁可徽章残留也不制造假完成：Stop 丢失时 pane 停在 ai-working，
/// 直到下一个 hook 事件或 SessionEnd 才恢复——罕见、纯视觉、下一轮对话即自愈，
/// 与 SessionEnd 丢失时残留 ai-idle 是同一口径。
///
/// 退出的唯一权威信号是 SessionEnd hook（hook_server 处理：清状态 + 直推 idle）。
/// 这里**不能**根据输入检测（is_ai_session）把 hook 状态拆掉降级 idle：
/// 输入检测会漏判启动（别名/包装脚本）、误判退出（任务运行中双击 Ctrl+C 只是
/// 打断并不退出），曾经的 "ai-idle && !is_ai_session → idle" 兜底会把这类
/// 误差放大成 pane 整个会话期永久显示 idle。SessionEnd 丢失（claude 被外部
/// kill 等）时状态残留在 ai-idle 徽章，是有意接受的失败模式——罕见、纯视觉、
/// 该 pane 下次启动 AI 会话即自愈。
pub(crate) fn resolve_status(
    hook_state: &HookState,
    pty_manager: &crate::pty::PtyManager,
    pty_id: u32,
) -> String {
    if hook_state.is_hook_enabled(pty_id) {
        hook_state
            .get_status(pty_id)
            .unwrap_or_else(|| "idle".to_string())
    } else if pty_manager.is_ai_session(pty_id) {
        // 未启用 hook 时降级到进程轮询逻辑
        if pty_manager.has_recent_output(pty_id, AI_ACTIVE_TIMEOUT) {
            "ai-working".to_string()
        } else {
            "ai-idle".to_string()
        }
    } else {
        "idle".to_string()
    }
}

pub fn start_monitor(
    app: AppHandle,
    pty_manager: crate::pty::PtyManager,
    hook_state: HookState,
    emitter: StatusEmitter,
) {
    thread::spawn(move || {
        loop {
            let pty_ids = pty_manager.get_pty_ids();

            for pty_id in &pty_ids {
                let status = resolve_status(&hook_state, &pty_manager, *pty_id);
                // 轮询算出的变化没有 hook 事件作依据，cause 为 None：hook 已启用的
                // pane 上这里只会重复发出与 hook 一致的值（被去重吞掉），真正会
                // 产生变化的只有无 hook 的降级路径。
                emitter.emit_if_changed(&app, *pty_id, &status, None);
            }

            emitter.retain(&pty_ids);

            let sleep_ms = if pty_ids.is_empty() { 2000 } else { 500 };
            thread::sleep(Duration::from_millis(sleep_ms));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::PtyManager;

    /// 回归测试（2026-07-31 tab 显示 idle 而非 ai-* 的 bug）：claude 任务
    /// 运行中快速连按两次 Ctrl+C 打断（claude 只是中断当前任务回到提示符，
    /// 并未退出），输入检测按"双击 Ctrl+C 退出"误清 AI 会话标记；随后 hook
    /// 上报 ai-idle。修复前 monitor 因 !is_ai_session 把 hook 状态整体拆除
    /// 并降级为 idle——hook 状态必须保持权威。
    #[test]
    fn double_ctrlc_interrupt_keeps_hook_ai_idle() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        mgr.track_input(1, "claude\r"); // 用户启动 claude
        hooks.update(1, "ai-working".to_string()); // UserPromptSubmit：任务运行中
        mgr.track_input(1, "\x03"); // 双击 Ctrl+C 打断任务
        mgr.track_input(1, "\x03"); // （claude 未退出，仅回到提示符）
        hooks.update(1, "ai-idle".to_string()); // 打断后 claude 上报 ai-idle

        assert_eq!(resolve_status(&hooks, &mgr, 1), "ai-idle");
    }

    /// 回归测试（AI 完成通知每 20~50s 重复播报的 bug）：hook 卡在 ai-working
    /// 且无 PTY 输出时，**不再**按输出超时降级为 ai-idle。
    ///
    /// 旧行为在这里返回 ai-idle，而 hook_status 仍是 ai-working、降级结果不落盘；
    /// 于是 AI 空闲期的零星伪输出会把状态抬回 ai-working，3 秒后再落回 ai-idle，
    /// 每个下降沿被前端当成一次"任务完成"。现在 hook 是绝对权威，代价是 Stop
    /// 丢失时徽章残留 ai-working（详见 `resolve_status` 文档）。
    #[test]
    fn stuck_ai_working_is_not_degraded_by_output_timeout() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        mgr.track_input(1, "claude\r");
        hooks.update(1, "ai-working".to_string());
        mgr.track_input(1, "\x03");
        mgr.track_input(1, "\x03");
        // 无后续 hook 事件、无 PTY 输出（has_recent_output 为 false）

        assert_eq!(resolve_status(&hooks, &mgr, 1), "ai-working");
    }

    /// 同一 bug 的另一面：monitor 每 500ms 重算一次，只要没有新 hook 事件，
    /// 连续多轮必须给出同一个值——状态不再随输出活跃度上下摆动，也就没有
    /// 供前端误判为"完成"的下降沿。
    #[test]
    fn hook_status_is_stable_across_polls() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        mgr.track_input(1, "claude\r");
        hooks.update(1, "ai-working".to_string());

        let polls: Vec<String> = (0..5).map(|_| resolve_status(&hooks, &mgr, 1)).collect();
        assert!(
            polls.iter().all(|s| s == "ai-working"),
            "hook 未更新时状态应恒定，实测 {:?}",
            polls
        );
    }

    /// 启动方式漏检（别名/包装脚本，输入检测从未标记 is_ai_session）时，
    /// hook 状态照常生效，不因 !is_ai_session 被降级。
    #[test]
    fn alias_start_without_input_detection_keeps_hook_status() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        hooks.update(1, "ai-idle".to_string()); // hook 正常上报，但输入检测漏了启动

        assert_eq!(resolve_status(&hooks, &mgr, 1), "ai-idle");
    }

    /// 对照组：输入检测与 hook 一致（未误判退出）时，ai-idle 正常保持。
    #[test]
    fn hook_ai_idle_stays_when_input_detection_agrees() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        mgr.track_input(1, "claude\r");
        hooks.update(1, "ai-idle".to_string());

        assert_eq!(resolve_status(&hooks, &mgr, 1), "ai-idle");
    }

    /// 无 hook 的 pane（WSL/SSH/hook 关闭）维持轮询逻辑：
    /// 输入检测在会话中 + 无近期输出 → ai-idle；不在会话中 → idle。
    #[test]
    fn fallback_path_without_hook_unchanged() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        assert_eq!(resolve_status(&hooks, &mgr, 1), "idle");
        mgr.track_input(1, "claude\r");
        assert_eq!(resolve_status(&hooks, &mgr, 1), "ai-idle");
    }
}
