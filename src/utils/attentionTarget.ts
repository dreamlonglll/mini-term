/**
 * 「下一个轮到我处理的会话是哪个」—— 纯函数，不碰 store 也不碰 DOM。
 *
 * 标题栏状态灯和状态栏（托盘 / 菜单栏）图标点下去要落到同一个地方，逻辑就不能
 * 各写一份：跳转副作用留在 attentionJump.ts，挑目标这段单独放这里，好单测。
 */
import { collectPanes } from './layoutOps';
import type { ProjectState } from '../types';

export type AttentionTarget = { projectId: string; paneId: string };

/**
 * 挑出「下一件该我做的事」所在的 pane。
 *
 * 优先级：
 *   1. 待确认 / 异常 —— 它卡在等你拍板，不处理什么都推进不了
 *   2. 已完成 —— 最先完成的排最前
 *   3. 处理中 —— 还没有结果，最不需要你现在过去
 *
 * @param onlyProjectId 只在这个项目内挑（状态栏右键菜单点某个项目走这条）；
 *                      省略 = 全局挑（标题栏状态灯、状态栏左键点击）
 * @returns 没有任何活跃 pane 时返回 null（调用方不必有反应）
 */
export function pickAttentionTarget(
  projectStates: Map<string, ProjectState>,
  aiDoneOrder: Map<string, number>,
  onlyProjectId?: string,
): AttentionTarget | null {
  let attention: AttentionTarget | null = null;
  let done: (AttentionTarget & { seq: number }) | null = null;
  let working: AttentionTarget | null = null;

  for (const [projectId, ps] of projectStates) {
    if (onlyProjectId !== undefined && projectId !== onlyProjectId) continue;
    if (!ps.layout) continue;
    for (const pane of collectPanes(ps.layout)) {
      if (pane.status === 'error' || pane.attention) {
        attention ??= { projectId, paneId: pane.id };
        continue;
      }
      const seq = aiDoneOrder.get(pane.id);
      if (seq !== undefined) {
        if (done === null || seq < done.seq) done = { projectId, paneId: pane.id, seq };
      } else if (pane.status === 'ai-working') {
        working ??= { projectId, paneId: pane.id };
      }
    }
  }

  const target = attention ?? done ?? working;
  // done 分支带着排序用的 seq，重新拼一遍别把内部字段漏给调用方
  return target ? { projectId: target.projectId, paneId: target.paneId } : null;
}
