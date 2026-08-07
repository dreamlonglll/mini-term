import { useAppStore } from '../store';
import { pickAttentionTarget } from './attentionTarget';
import { activatePane } from './paneActions';

/**
 * 点标题栏状态灯 / 状态栏（托盘、菜单栏）图标时该跳去哪 ——
 * 找出「下一个轮到我处理」的 pane 并激活它。挑目标的优先级见
 * {@link pickAttentionTarget}：待确认 / 异常 > 最先完成 > 处理中。
 *
 * 与状态栏右键菜单的排序（待确认 > 处理中 > 完成 > 空闲）有意不同：那份列表是在窗口外
 * 回答「哪些项目还活着」，这里是回答「下一件该我做的事是什么」——
 * 一个还在跑的会话不需要你，一个跑完的在等你。
 *
 * @param onlyProjectId 限定在某个项目内跳（状态栏右键菜单点项目走这条）
 * @returns 是否找到了可跳转的目标（false = 全都闲着，调用方不必有反应）
 */
export function focusAttentionTarget(onlyProjectId?: string): boolean {
  const { projectStates, aiDoneOrder, setActiveProject } = useAppStore.getState();

  const target = pickAttentionTarget(projectStates, aiDoneOrder, onlyProjectId);
  if (!target) return false;

  setActiveProject(target.projectId);
  // 项目切换后布局才挂到前台，activatePane 里的 DOM 聚焦要等这一帧过去
  requestAnimationFrame(() => activatePane(target.projectId, target.paneId));
  return true;
}
