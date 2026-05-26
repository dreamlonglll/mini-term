use crate::hook_server::HookState;
use serde::Serialize;
use std::collections::HashMap;
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyStatusChangePayload {
    pub pty_id: u32,
    pub status: String,
}

/// AI 输出活跃超时阈值
const AI_ACTIVE_TIMEOUT: Duration = Duration::from_secs(3);

pub fn start_monitor(app: AppHandle, pty_manager: crate::pty::PtyManager, hook_state: HookState) {
    thread::spawn(move || {
        let mut prev_statuses: HashMap<u32, String> = HashMap::new();

        loop {
            let pty_ids = pty_manager.get_pty_ids();

            for pty_id in &pty_ids {
                // Hook 优先。两种兜底：
                // 1. hook 停在 ai-working 但 AI 已连续 AI_ACTIVE_TIMEOUT 无输出，
                //    视为空闲——hook 的完成事件（Stop/Notification）可能丢失或延迟。
                // 2. 状态为 ai-idle 且 AI 会话已退出（/exit、Ctrl+D 等），清除 hook 状态。
                let status = if hook_state.is_hook_enabled(*pty_id) {
                    let hook_status = hook_state
                        .get_status(*pty_id)
                        .unwrap_or_else(|| "idle".to_string());
                    let effective = if hook_status == "ai-working"
                        && !pty_manager.has_recent_output(*pty_id, AI_ACTIVE_TIMEOUT)
                    {
                        "ai-idle".to_string()
                    } else {
                        hook_status
                    };
                    if effective == "ai-idle" && !pty_manager.is_ai_session(*pty_id) {
                        // AI 已通过输入检测退出，清除 hook 状态后回退到 idle
                        hook_state.remove(*pty_id);
                        "idle".to_string()
                    } else {
                        effective
                    }
                } else if pty_manager.is_ai_session(*pty_id) {
                    // 未启用 hook 时降级到进程轮询逻辑
                    if pty_manager.has_recent_output(*pty_id, AI_ACTIVE_TIMEOUT) {
                        "ai-working".to_string()
                    } else {
                        "ai-idle".to_string()
                    }
                } else {
                    "idle".to_string()
                };

                let prev = prev_statuses.get(pty_id);
                if prev.map(|s| s.as_str()) != Some(status.as_str()) {
                    let _ = app.emit(
                        "pty-status-change",
                        PtyStatusChangePayload {
                            pty_id: *pty_id,
                            status: status.clone(),
                        },
                    );
                    prev_statuses.insert(*pty_id, status);
                }
            }

            prev_statuses.retain(|id, _| pty_ids.contains(id));

            let sleep_ms = if pty_ids.is_empty() { 2000 } else { 500 };
            thread::sleep(Duration::from_millis(sleep_ms));
        }
    });
}
