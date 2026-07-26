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
    prev: Arc<Mutex<HashMap<u32, String>>>,
}

impl StatusEmitter {
    pub fn new() -> Self {
        Self {
            prev: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 与上次发出的状态不同才 emit
    pub fn emit_if_changed(&self, app: &AppHandle, pty_id: u32, status: &str) {
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
            },
        );
    }

    /// 清掉已不存在的 pty 的去重记录
    pub fn retain(&self, alive: &[u32]) {
        self.prev.lock().unwrap().retain(|id, _| alive.contains(id));
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

                emitter.emit_if_changed(&app, *pty_id, &status);
            }

            emitter.retain(&pty_ids);

            let sleep_ms = if pty_ids.is_empty() { 2000 } else { 500 };
            thread::sleep(Duration::from_millis(sleep_ms));
        }
    });
}
