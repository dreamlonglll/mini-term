import type { PaneStatus } from '../types';

/**
 * hook 事件里唯一表示"这一轮任务真的做完了"的事件名。
 *
 * `Stop`、`StopFailure`、`PermissionRequest`、`Notification`、`Elicitation`、
 * `SessionStart`、`Interrupt`、`Stall` 全部落到 `ai-idle` —— 因为它们确实都是
 * "AI 不在干活、在等用户"。但只有 `Stop` 是完成；其余是"又要你来处理一下"
 * （批权限 / 看通知 / 补澄清）、"这轮因 API 错误没跑完"（`StopFailure`）、
 * "你自己把它打断了"（`Interrupt`）、"它十秒没动静了"（`Stall`，后端
 * `process_monitor.rs` 的停摆兜底），把它们播报成完成就是误报。
 */
const COMPLETION_CAUSE = 'Stop';

/**
 * 该成因是否表示"有事等你处理"——托盘黄灯的依据，与后端
 * `hook_server.rs` 的 `is_attention_cause` 必须保持同集。
 *
 * - `PermissionRequest`：权限审批（权限类 Notification 已在后端归一化成它）
 * - `Elicitation`：MCP 表单待填
 * - `StopFailure`：回合因 API 错误结束（限流 / 超载 / 鉴权失败…）。它不是完成，
 *   不该播报，但用户得知道要回来重发，否则只会看到一个安静躺着的 ai-idle。
 *
 * 黄灯的熄灭不在这里：用户对该 pane 键入即视为已在处理
 * (`clearPaneAttentionByPty`)，或状态转回 ai-working 时随 attention=false 清掉。
 */
export function isAttentionCause(cause?: string): boolean {
  return cause === 'PermissionRequest' || cause === 'Elicitation' || cause === 'StopFailure';
}

/**
 * 这次状态变化是否构成「AI 转入待确认」的**上升沿**——待确认通知（提示音 /
 * 任务栏闪烁 / toast）的唯一判据。
 *
 * 必须看上升沿而不能只看 `isAttentionCause`：后端 `StatusEmitter` 把 attention
 * 类事件显式排除在去重之外（`process_monitor.rs`，为的是同一轮里第二次授权请求
 * 不被吞掉），所以同一次待确认完全可能连着推来多条（PermissionRequest 后跟
 * Elicitation、或同一事件重发）。不看上升沿就是一次待确认响好几声。
 *
 * 黄灯已亮着时新来的 attention 事件不再响，正对应「用户还没处理，提醒过了」；
 * 用户对该 pane 键入会清掉 attention（`clearPaneAttentionByPty`），下一次授权
 * 请求于是重新构成上升沿、重新提醒。
 *
 * @param prevAttention 该 pane 变化前的 attention 标记（黄灯是否已亮）
 * @param cause 后端 `pty-status-change` 带的成因；无 hook 的降级路径没有事件名，
 *   压根产生不了 attention，这里自然返回 false
 */
export function isAttentionRise(prevAttention: boolean | undefined, cause?: string): boolean {
  return isAttentionCause(cause) && !prevAttention;
}

/**
 * 判断一次 pane 状态变化是否构成"AI 任务完成"，即该不该播提示音 / 闪任务栏 /
 * 推 toast。
 *
 * 曾经的判据只有 `ai-working → ai-idle` 这个下降沿，它有两个漏报方向的反面：
 *
 * 1. **假完成（重复播报）**：hook 卡在 ai-working 时，旧的 `resolve_status`
 *    会按"3 秒无输出"把状态降级成 ai-idle，而降级不落盘，于是 AI 空闲期的
 *    零星伪输出把状态反复抬回 ai-working，每落一次就播报一次。这一条已在后端
 *    根治：输出静默的兜底改为一次性把结论写进 hook 状态（`Stall` / `StallExit`
 *    成因），触发一次即收敛，不再摆动；且成因不是 `Stop`，本函数直接排除。
 * 2. **假完成（权限请求）**：`PermissionRequest` 同样落到 ai-idle，弹审批框的
 *    瞬间就会被当成"任务完成"。后端对 Codex 已特判为 ai-working
 *    （`hook_server.rs`），Claude 侧保留 ai-idle 是对的——徽章该显示"在等你"
 *    ——所以只能在这里按事件名把它排除。
 *
 * @param cause 后端 `pty-status-change` 带的成因（hook 事件名）。
 *   `undefined` 表示这次变化是后端 monitor 轮询算出来的，没有 hook 事件作依据：
 *   hook 已启用的 pane 上轮询只会重复 hook 的值并被去重吞掉，所以无 cause 的
 *   下降沿只可能来自**无 hook 的降级路径**（WSL / SSH / hook 关闭）。那条路径
 *   压根收不到事件名，下降沿是它唯一的完成信号，必须放行——否则这些 pane 会
 *   彻底收不到完成通知。
 */
export function isAiCompletion(
  oldStatus: PaneStatus,
  newStatus: PaneStatus,
  cause?: string,
): boolean {
  if (oldStatus !== 'ai-working' || newStatus !== 'ai-idle') return false;
  if (cause === undefined) return true;
  return cause === COMPLETION_CAUSE;
}
