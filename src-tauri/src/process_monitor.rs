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
    /// 状态成因：hook 直推时是（归一化后的）hook 事件名（`Stop` /
    /// `PermissionRequest` / `SessionEnd` …，见 hook_server::event_cause），
    /// monitor 轮询算出的变化为 None。多个事件都落到 ai-idle，但只有 `Stop`
    /// 是「任务做完了」，前端据此决定播报完成与托盘绿灯；黄灯认
    /// `PermissionRequest`/`Elicitation`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// 会话内 AI 命令名("claude"/"codex"/…),前端品牌图标兜底用
    /// (hook 未启用时 aiSession 不会上报,这是 agent 的唯一来源);
    /// None = 非 AI 状态或来源未知。不参与去重(同一会话内恒定)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// AI 输出活跃超时阈值。**仅**用于无 hook 的降级路径（`resolve_status` 的
/// `else if` 分支）；hook 已启用的 pane 不在 `resolve_status` 里看输出活跃度
/// （hook 启用后的输出静默走另一套「一次性落盘」的停摆兜底，见
/// `stall_settle_target`）。
const AI_ACTIVE_TIMEOUT: Duration = Duration::from_secs(3);

/// hook 已启用的 pane 上「AI 停摆」的判定窗口：hook 状态停在 ai-working、
/// 且状态与 PTY 输出双双静默这么久，就认为这一轮实际已经结束。
const AI_STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// 停摆兜底改写状态时带的成因。两者都**不是** `Stop`，前端 `isAiCompletion`
/// 认不出，不会播报完成；也都不在 `hook_server::is_attention_cause` 里，
/// 不点托盘黄灯。纯粹用来把徽章从卡死的 ai-working 上摘下来。
const STALL_CAUSE: &str = "Stall";
/// 停摆 + 此前已触发过退出 → 判定 AI 已经退出，回落 idle（见 `stall_settle_target`）
const STALL_EXIT_CAUSE: &str = "StallExit";

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
    /// - 状态变化 → 总是 emit(cause 取本次值,None/非 attention 类事件会清掉
    ///   前端 attention 标注,这正是「用户批准后下一个事件自然熄灭黄灯」的路径);
    /// - 状态相同 + cause=None → **跳过**:monitor 每 500ms 以无成因方式重发
    ///   hook 状态,若放行会在黄灯点亮后 500ms 内把 attention 抹掉
    ///   (黄灯闪一下就被蓝色顶掉的根因);
    /// - 状态相同 + cause 为 attention 类事件(见 `hook_server::is_attention_cause`)
    ///   → **总是 emit**:黄灯的清除发生在前端(用户键入即批准),后端去重表
    ///   感知不到;若按相同 cause 去重,同一轮内第二次授权请求会被吞掉;
    /// - 状态相同 + 其他 cause 与上次相同 → 跳过;变化 → emit(如
    ///   PermissionRequest → Stop)。
    pub fn emit_if_changed(
        &self,
        app: &AppHandle,
        pty_id: u32,
        status: &str,
        cause: Option<&str>,
        agent: Option<String>,
    ) {
        let mut prev = self.prev.lock().unwrap();
        if let Some((prev_status, prev_cause)) = prev.get(&pty_id) {
            if prev_status == status {
                match cause {
                    None => return,
                    Some(c) if crate::hook_server::is_attention_cause(c) => {}
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
                agent,
            },
        );
    }

    /// 上次发给前端的成因。停摆兜底用它避让「正等用户批准」的 pane：
    /// 那类 pane 的黄灯要一直亮到用户处理，兜底若插一脚会把 attention 抹掉。
    pub(crate) fn last_cause(&self, pty_id: u32) -> Option<String> {
        self.prev
            .lock()
            .unwrap()
            .get(&pty_id)
            .and_then(|(_, cause)| cause.clone())
    }

    /// 清掉已不存在的 pty 的去重记录
    pub fn retain(&self, alive: &[u32]) {
        self.prev.lock().unwrap().retain(|id, _| alive.contains(id));
    }
}

/// 单个 pty 的状态判定（monitor 每轮对每个 pty 调一次）。
///
/// **本函数**里 hook 一旦启用即为绝对权威：状态完全由 `last_hook_status` 决定，
/// PTY 输出活跃度不参与判定，因此同一份 hook 状态连续多轮必然算出同一个值。
///
/// 这里曾有一条兜底——"hook 停在 ai-working 但连续 AI_ACTIVE_TIMEOUT 无输出即
/// 视为 ai-idle"（后又给 API 重试加了 retry_hold 豁免）。它是**无记忆**的：每轮
/// 500ms 重算，降级结果不落盘，hook_status 本身仍是 ai-working。于是只要
/// hook_status 卡住（Stop 丢失，或 Stop 之后又收到同会话迟到的 PostToolUse），
/// AI 空闲期每一次零星伪输出（TUI 定时重绘等）都会把状态抬回 ai-working、
/// 3 秒后再落回 ai-idle，形成以伪输出间隔为周期的脉冲（实测 20~50s 一轮），
/// 每个下降沿被前端当成一次"任务完成"反复播报。v0.9.3 起整条兜底从本函数删除
/// （retry_hold 机制随之失去消费者一并移除）。
///
/// 输出静默的兜底在 v0.10.3 以另一种形态回来了，但**不在这里**：见
/// `stall_settle_target` / `settle_stalled_ai`——它一次性地把结论**写进**
/// `last_hook_status`，于是本函数后续每轮读到的都是同一个收敛值，不存在摆动，
/// 也就没有可供误判成"完成"的下降沿。判据（hook 状态是否停在 ai-working）
/// 本身也随之改变，触发一次就不再满足，这正是与旧兜底的分水岭。
///
/// 退出的唯一权威信号是 SessionEnd hook（hook_server 处理：清状态 + 直推 idle）。
/// 这里**不能**根据输入检测（is_ai_session）把 hook 状态拆掉降级 idle：
/// 输入检测会漏判启动（别名/包装脚本）、误判退出（任务运行中双击 Ctrl+C 只是
/// 打断并不退出），曾经的 "ai-idle && !is_ai_session → idle" 兜底会把这类
/// 误差放大成 pane 整个会话期永久显示 idle。
///
/// 判定**不看 hook server 是否在运行**：`hook_enabled` 默认关闭，且 WSL / SSH /
/// opencode / pi 这些 pane 即便 server 开着也从来没有 hook 上报，它们的徽章全
/// 依赖这里的降级轮询（CLAUDE.md 「只靠输入检测识别的 agent 拿得到状态徽章」）。
/// 曾短暂加过一条 `if !server_running { return "idle" }` 的「AI 感知总开关」，
/// 在默认配置下等于把全部 AI 徽章、完成通知、托盘灯静默关掉，且没解决它声称
/// 要解决的「降级轮询把等待授权谎报成完成」——那条 else-if 分支原样还在。
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

/// 「AI 停摆」兜底的纯判定部分（发射一侧需要 `AppHandle`，单测里构造不出来，
/// 因此把判据抽出来；`timeout` 也做成参数，测试可用 `Duration::ZERO` 表示
/// "窗口已经走完"、用一个极大值表示"还没走完"）。
///
/// 背景：hook 状态卡在 ai-working 有确定性的来源——`Stop` 在若干情形下压根不
/// 触发（用户打断已由 `note_user_interrupt` 覆盖；API 错误已由 `StopFailure`
/// 覆盖），但仍有覆盖不到的：AI 进程被 kill / PTY 里的 shell 被换掉 / sidecar
/// 上报失败 / 新版本又加了没注册的事件。这类 pane 会一直顶着 ai-working 徽章。
///
/// 五道闸，缺一不可：
/// 1. hook 已启用——没启用的 pane 走 `resolve_status` 的轮询降级路径，那条路本就
///    按输出活跃度给 ai-idle，插手只会互相打架；
/// 2. 当前 hook 状态正是 ai-working——其余状态没什么可收敛的；
/// 3. 上次发给前端的成因不是 attention 类——Codex 的 `PermissionRequest` 映射为
///    ai-working 并点着黄灯，它是**明知故犯**地在等用户，且审批框弹出后本就没有
///    输出。此时插手会连黄灯一起抹掉（前端按 cause 重算 attention），
///    把"等你批准"变成"没在跑"；
/// 4. 状态本身静置满 `timeout`——刚进 ai-working 就判停摆是误伤；
/// 5. PTY 输出也静默满 `timeout`——真在干活的 Claude/Codex TUI 一直在重绘
///    计时器与 spinner，10 秒完全无输出基本只有"已经不在跑了"一种解释。
///
/// 目标状态区分两种情形：
/// - `ai-idle`：AI 进程还在（输入检测的会话标记仍在，且 hook 事件也一直在把它
///   扶正），只是这一轮结束了/卡住了——降徽章即可；
/// - `idle`：此前已经**触发过退出**（双击 Ctrl+C / Ctrl+D / `/exit` 等，
///   `track_input` 据此清掉会话标记），且此后没有任何 hook 事件把标记扶回来。
///   单看输入检测不可信（双击 Ctrl+C 常常只是打断），但"触发过退出"叠加
///   "10 秒完全无输出"就足以确认它真的退出了：还活着的 AI 不会这么安静。
pub(crate) fn stall_settle_target(
    hook_state: &HookState,
    pty_manager: &crate::pty::PtyManager,
    last_cause: Option<&str>,
    pty_id: u32,
    timeout: Duration,
) -> Option<&'static str> {
    if !hook_state.is_hook_enabled(pty_id) {
        return None;
    }
    if hook_state.get_status(pty_id).as_deref() != Some("ai-working") {
        return None;
    }
    if last_cause.is_some_and(crate::hook_server::is_attention_cause) {
        return None;
    }
    if !hook_state
        .status_age(pty_id)
        .is_some_and(|age| age >= timeout)
    {
        return None;
    }
    if pty_manager.has_recent_output(pty_id, timeout) {
        return None;
    }
    Some(if pty_manager.is_ai_session(pty_id) {
        "ai-idle"
    } else {
        "idle"
    })
}

/// 停摆兜底的发射侧：判定命中就把结论**落盘**到 hook 状态并通知前端。
///
/// 落盘是关键（与 `note_user_interrupt` 同一手法）：`last_hook_status` 被改写后，
/// 判据 2 不再成立，本轮之后不会重复触发；`resolve_status` 每轮读到的也都是这个
/// 收敛值，不会随空闲期的零星伪输出摆回 ai-working。没有摆动 → 没有下降沿 →
/// 不会重演 v0.9.3 修掉的"每 20~50s 播报一次假完成"。
///
/// 误判（AI 其实还在跑，只是安静）由下一个 hook 事件立刻纠正回 ai-working，
/// 那条路径还会顺带 `mark_ai_session` 把会话标记扶正。
fn settle_stalled_ai(
    app: &AppHandle,
    hook_state: &HookState,
    pty_manager: &crate::pty::PtyManager,
    emitter: &StatusEmitter,
    pty_id: u32,
) {
    let last_cause = emitter.last_cause(pty_id);
    let Some(target) = stall_settle_target(
        hook_state,
        pty_manager,
        last_cause.as_deref(),
        pty_id,
        AI_STALL_TIMEOUT,
    ) else {
        return;
    };
    hook_state.update(pty_id, target.to_string());
    let (cause, agent) = if target == "idle" {
        (STALL_EXIT_CAUSE, None)
    } else {
        (STALL_CAUSE, pty_manager.ai_session_agent(pty_id))
    };
    emitter.emit_if_changed(app, pty_id, target, Some(cause), agent);
    eprintln!(
        "[monitor] pty_id={} ai-working 静默 {}s -> {} (cause={})",
        pty_id,
        AI_STALL_TIMEOUT.as_secs(),
        target,
        cause
    );
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
                // 停摆收敛会改写 hook 状态，与 server 启停串行化；状态判定本身在
                // 锁外。server 没起来**不是**跳过收敛的理由：判据是 pane 自己的
                // hook 记录，AI 跑着时把 hook 开关关掉的 pane 照样得能被拉回来。
                // 顺序不能颠倒：收敛命中时下面这次 emit 值已相同，会被去重吞掉，
                // 前端只收到 settle 发出的那条带成因的。
                hook_state.with_server_lock(|| {
                    settle_stalled_ai(&app, &hook_state, &pty_manager, &emitter, *pty_id);
                });
                let status = resolve_status(&hook_state, &pty_manager, *pty_id);
                let agent = if status.starts_with("ai-") {
                    pty_manager.ai_session_agent(*pty_id)
                } else {
                    None
                };
                emitter.emit_if_changed(&app, *pty_id, &status, None, agent);
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

    /// 回归测试（PR #43 评审）：hook server 未运行**不得**让 AI 感知整体归零。
    /// `hook_enabled` 默认是 false，曾经的 `if !server_running { return "idle" }`
    /// 让全新安装与从没进过设置页的存量用户一次性失去全部 AI 徽章、完成通知与
    /// 托盘灯；WSL/SSH/opencode/pi 这些永远拿不到 hook 上报的 pane 更是彻底没
    /// 出路。判定只看 pane 自己有没有 hook（is_hook_enabled），与 server 无关。
    #[test]
    fn no_hook_server_still_falls_back_to_polling() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        // server 从未启动 → hook_state 里没有该 pty 的任何记录
        mgr.track_input(1, "claude\r"); // 只有输入检测标记了 AI 会话
        assert!(!hooks.is_hook_enabled(1));

        assert_eq!(resolve_status(&hooks, &mgr, 1), "ai-idle");
    }

    // ---- 停摆兜底（stall_settle_target）----
    //
    // 窗口用参数模拟：ZERO = 窗口已走完（`status_age >= 0` 恒真，
    // `has_recent_output(_, 0)` 恒假），大值 = 窗口远未走完。

    const ELAPSED: Duration = Duration::ZERO;
    const NOT_ELAPSED: Duration = Duration::from_secs(3600);

    /// 主场景：hook 卡在 ai-working、AI 进程还在（会话标记未被清），
    /// 输出静默满窗口 → 收敛到 ai-idle（只降徽章，不当作退出）。
    #[test]
    fn stalled_ai_working_settles_to_ai_idle() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        mgr.track_input(1, "claude\r");
        hooks.update(1, "ai-working".to_string());

        assert_eq!(
            stall_settle_target(&hooks, &mgr, None, 1, ELAPSED),
            Some("ai-idle")
        );
    }

    /// 另一半：此前已触发过退出（双击 Ctrl+C，输入检测清掉会话标记），
    /// 此后再无 hook 事件把标记扶正 + 静默满窗口 → 确认已退出，回落 idle。
    #[test]
    fn stalled_after_exit_trigger_settles_to_idle() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        mgr.track_input(1, "claude\r");
        hooks.update(1, "ai-working".to_string()); // 任务运行中
        mgr.track_input(1, "\x03"); // 双击 Ctrl+C
        mgr.track_input(1, "\x03"); // → clear_ai_session
        assert!(!mgr.is_ai_session(1));

        assert_eq!(
            stall_settle_target(&hooks, &mgr, None, 1, ELAPSED),
            Some("idle")
        );
    }

    /// Ctrl+D 与显式退出命令同样构成「触发过退出」。
    #[test]
    fn explicit_exit_paths_settle_to_idle() {
        for input in ["\x04", "/exit\r"] {
            let hooks = HookState::new();
            let mgr = PtyManager::new();

            mgr.track_input(1, "claude\r");
            hooks.update(1, "ai-working".to_string());
            mgr.track_input(1, input);

            assert_eq!(
                stall_settle_target(&hooks, &mgr, None, 1, ELAPSED),
                Some("idle"),
                "退出方式 {:?} 未被确认",
                input
            );
        }
    }

    /// hook 事件会把误清的会话标记扶正（`mark_ai_session`）：
    /// 打断后 AI 其实没退，标记回来了 → 只降 ai-idle，不再判定为退出。
    #[test]
    fn hook_event_after_exit_trigger_downgrades_target_to_ai_idle() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        mgr.track_input(1, "claude\r");
        hooks.update(1, "ai-working".to_string());
        mgr.track_input(1, "\x03");
        mgr.track_input(1, "\x03"); // 误判退出
        mgr.mark_ai_session(1, "claude"); // 后续 hook 事件证明它还活着
        hooks.update(1, "ai-working".to_string());

        assert_eq!(
            stall_settle_target(&hooks, &mgr, None, 1, ELAPSED),
            Some("ai-idle")
        );
    }

    /// 窗口未走完不动手：刚进 ai-working 的 pane 不算停摆。
    #[test]
    fn fresh_ai_working_is_not_settled() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        mgr.track_input(1, "claude\r");
        hooks.update(1, "ai-working".to_string());

        assert_eq!(stall_settle_target(&hooks, &mgr, None, 1, NOT_ELAPSED), None);
    }

    /// 有近期输出不动手：真在干活的 TUI 一直在重绘，这是最主要的活体证据。
    #[test]
    fn recent_output_keeps_ai_working() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        mgr.track_input(1, "claude\r");
        hooks.update(1, "ai-working".to_string());
        mgr.note_output_for_test(1);

        // 状态静置窗口已过，但输出窗口没过 → 不收敛
        assert_eq!(stall_settle_target(&hooks, &mgr, None, 1, NOT_ELAPSED), None);
    }

    /// Codex 的 PermissionRequest 映射为 ai-working 且点着黄灯：审批框弹出后
    /// 本就没有输出，兜底若插手会把 attention 一并抹掉（前端按 cause 重算），
    /// 把"等你批准"变成"没在跑"。attention 类成因一律避让。
    #[test]
    fn attention_pane_is_exempt_from_stall() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        mgr.track_input(1, "codex\r");
        hooks.update(1, "ai-working".to_string());

        for cause in ["PermissionRequest", "Elicitation", "StopFailure"] {
            assert_eq!(
                stall_settle_target(&hooks, &mgr, Some(cause), 1, ELAPSED),
                None,
                "cause={} 的 pane 不该被兜底改写",
                cause
            );
        }
        // 非 attention 成因（如工作中事件）不豁免
        assert_eq!(
            stall_settle_target(&hooks, &mgr, Some("PreToolUse"), 1, ELAPSED),
            Some("ai-idle")
        );
    }

    /// 只作用于 hook 已启用且正处于 ai-working 的 pane。
    #[test]
    fn stall_leaves_other_panes_alone() {
        let mgr = PtyManager::new();
        mgr.track_input(1, "claude\r");

        // hook 从未启用（WSL/SSH/hook 关闭）：走轮询降级路径，不插手
        let bare = HookState::new();
        assert_eq!(stall_settle_target(&bare, &mgr, None, 1, ELAPSED), None);

        // 已在等用户 / 已退出：没什么可收敛的
        for status in ["ai-idle", "idle"] {
            let hooks = HookState::new();
            hooks.update(1, status.to_string());
            assert_eq!(stall_settle_target(&hooks, &mgr, None, 1, ELAPSED), None);
        }
    }

    /// 与 v0.9.3 删掉的旧兜底的分水岭：结论必须**落盘**。
    /// 落盘后判据不再成立，不会每轮重复触发；`resolve_status` 每轮读到同一个
    /// 收敛值，不随零星伪输出摆回 ai-working —— 没有摆动就没有假完成沿。
    #[test]
    fn stall_settle_is_latched_and_not_repeated() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        mgr.track_input(1, "claude\r");
        hooks.update(1, "ai-working".to_string());
        let target = stall_settle_target(&hooks, &mgr, None, 1, ELAPSED).unwrap();

        hooks.update(1, target.to_string()); // settle_stalled_ai 的落盘动作
        assert_eq!(
            stall_settle_target(&hooks, &mgr, None, 1, ELAPSED),
            None,
            "已收敛的 pane 不该被反复改写"
        );

        // 落盘后即便伪输出继续零星抵达，多轮 resolve_status 也恒定
        mgr.note_output_for_test(1);
        let polls: Vec<String> = (0..5).map(|_| resolve_status(&hooks, &mgr, 1)).collect();
        assert!(
            polls.iter().all(|s| s == "ai-idle"),
            "收敛后状态应恒定，实测 {:?}",
            polls
        );
    }

    /// 收敛结果经 `resolve_status` 原样透出（hook 状态仍是唯一读取源）。
    #[test]
    fn settled_status_flows_through_resolve_status() {
        let hooks = HookState::new();
        let mgr = PtyManager::new();

        mgr.track_input(1, "claude\r");
        hooks.update(1, "ai-working".to_string());
        mgr.track_input(1, "\x04"); // Ctrl+D 触发退出
        let target = stall_settle_target(&hooks, &mgr, None, 1, ELAPSED).unwrap();
        hooks.update(1, target.to_string());

        assert_eq!(resolve_status(&hooks, &mgr, 1), "idle");
    }

    /// 兜底成因不得被前端当成「完成」或「待办」——与
    /// `utils/aiCompletion.ts` 的同名断言互为镜像。
    #[test]
    fn stall_causes_are_neither_completion_nor_attention() {
        for cause in [STALL_CAUSE, STALL_EXIT_CAUSE] {
            assert_ne!(cause, "Stop", "兜底不得伪装成完成事件");
            assert!(
                !crate::hook_server::is_attention_cause(cause),
                "{} 不该点黄灯",
                cause
            );
        }
    }
}
