import { useAppStore } from '../store';
import { collectPanes } from './layoutOps';
import { activatePane } from './paneActions';

type Target = { projectId: string; paneId: string };

/**
 * 点标题栏状态灯时该跳去哪 —— 找出「下一个轮到我处理」的 pane 并激活它。
 *
 * 优先级：
 *   1. 待确认 / 异常 —— 它卡在等你拍板，不处理什么都推进不了
 *   2. 已完成 —— 最先完成的排最前
 *   3. 处理中 —— 还没有结果，最不需要你现在过去
 *
 * 与托盘右键菜单的排序（待确认 > 处理中 > 完成）有意不同：托盘是在窗口外
 * 回答「哪些项目还活着」，这里是在窗口内回答「下一件该我做的事是什么」——
 * 一个还在跑的会话不需要你，一个跑完的在等你。
 *
 * @returns 是否找到了可跳转的目标（false = 全都闲着，调用方不必有反应）
 */
export function focusAttentionTarget(): boolean {
  const { projectStates, aiDoneOrder, setActiveProject } = useAppStore.getState();

  let attention: Target | null = null;
  let done: (Target & { seq: number }) | null = null;
  let working: Target | null = null;

  for (const [projectId, ps] of projectStates) {
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
  if (!target) return false;

  setActiveProject(target.projectId);
  // 项目切换后布局才挂到前台，activatePane 里的 DOM 聚焦要等这一帧过去
  requestAnimationFrame(() => activatePane(target.projectId, target.paneId));
  return true;
}
