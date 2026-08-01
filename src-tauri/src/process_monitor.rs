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
    /// 状态成因(hook 事件语义):"attention" = 需要用户确认(授权/输入请求),
    /// "stop" = 一轮回答正常结束;None = 无成因信息(monitor 降级路径)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
}

/// AI 输出活跃超时阈值
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
    /// pty → 上次发出的 (status, cause)
    prev: Arc<Mutex<HashMap<u32, (String, Option<String>)>>>,
}

impl StatusEmitter {
    pub fn new() -> Self {
        Self {
            prev: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 与上次发出的状态不同才 emit。
    /// cause 规则:
    /// - 状态变化 → 总是 emit(cause 取本次值,None 会清掉前端 attention 标注,
    ///   这正是「用户批准后下一个事件自然熄灭黄灯」的路径);
    /// - 状态相同 + cause=None → **跳过**:monitor 每 500ms 以无成因方式重发
    ///   hook 状态,若放行会在黄灯点亮后 500ms 内把 attention 抹掉
    ///   (黄灯闪一下就被蓝色顶掉的根因);
    /// - 状态相同 + cause=attention → **总是 emit**:黄灯的清除发生在前端
    ///   (用户键入即批准),后端去重表感知不到;若按相同 cause 去重,
    ///   同一轮内第二次授权请求会被吞掉,黄灯不再点亮;
    /// - 状态相同 + 其他 cause 变化 → emit(如 attention → stop)。
    pub fn emit_if_changed(&self, app: &AppHandle, pty_id: u32, status: &str, cause: Option<&str>) {
        let mut prev = self.prev.lock().unwrap();
        if let Some((prev_status, prev_cause)) = prev.get(&pty_id) {
            if prev_status == status {
                match cause {
                    None => return,
                    Some("attention") => {}
                    Some(c) if prev_cause.as_deref() == Some(c) => return,
                    _ => {}
                }
            }
        }
        prev.insert(pty_id, (status.to_string(), cause.map(|s| s.to_string())));
        let _ = app.emit(
            "pty-status-change",
            PtyStatusChangePayload {
                pty_id,
                status: status.to_string(),
                cause: cause.map(|s| s.to_string()),
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
/// Hook 一旦启用即为权威，唯一兜底：hook 停在 ai-working 但 AI 已连续
/// AI_ACTIVE_TIMEOUT 无输出，视为空闲——hook 的完成事件（Stop/Notification）
/// 可能丢失或延迟。
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
        let hook_status = hook_state
            .get_status(pty_id)
            .unwrap_or_else(|| "idle".to_string());
        if hook_status == "ai-working"
            && !pty_manager.has_recent_output(pty_id, AI_ACTIVE_TIMEOUT)
        {
            "ai-idle".to_string()
        } else {
            hook_status
        }
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

    /// 同上但打断后没有任何 hook 事件（Stop 在用户主动打断时不触发）：
    /// hook 停在 ai-working、无输出 → 按超时兜底降到 ai-idle，而不是 idle。
    #[test]
    fn double_ctrlc_interrupt_with_stuck_ai_working_degrades_to_ai_idle() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        mgr.track_input(1, "claude\r");
        hooks.update(1, "ai-working".to_string());
        mgr.track_input(1, "\x03");
        mgr.track_input(1, "\x03");
        // 无后续 hook 事件、无 PTY 输出（has_recent_output 为 false）

        assert_eq!(resolve_status(&hooks, &mgr, 1), "ai-idle");
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
