/**
 * 「对终端 pane 做一件事」的共享实现：新建 / 分屏 / 关闭 / 重命名 / 切换。
 *
 * 之前这些逻辑在 TerminalArea 和 PaneGroup 里各有一份（createProjectPty + 拼
 * PaneState + 写 store + saveLayoutToConfig），加上键盘入口就会变成三份。
 * 统一到这里后，组件只负责「点了什么」，键盘只负责「按了什么」。
 *
 * 这些函数直接读写 store（不依赖 React），因此 hook 与普通事件回调都能调用。
 */
import { invoke } from '@tauri-apps/api/core';
import { useAppStore, genId, saveLayoutToConfig, registerPendingFork } from '../store';
import { createProjectPty, isRemoteProject, remotePaneLabel } from './remoteProject';
import { disposeTerminal, getCachedTerminal, writePtyInput } from './terminalCache';
import { branchCapsForAgent } from './sessionBranch';
import { showAlert, showConfirm, showPrompt } from './prompt';
import { closeTerminalSearchFor } from './terminalSearch';
import {
  collectLeaves,
  findAdjacentPtyId,
  findLeafContainingPane,
  findPaneById,
  findPaneByPtyId,
  insertSplit,
  removePaneFromLayout,
  replaceNode,
  resolveActivePane,
  updateLeafOfPane,
  type Direction,
} from './layoutOps';
import { t } from '../i18n';
import type { PaneState, ProjectConfig, ShellConfig, SplitNode } from '../types';

/** 取项目 + 布局的当前快照。 */
function snapshot(projectId: string) {
  const state = useAppStore.getState();
  const project = state.config.projects.find((p) => p.id === projectId);
  const layout = state.projectStates.get(projectId)?.layout ?? null;
  return { state, project, layout };
}

/** 按项目类型解析要用的 shell（远程项目不需要）。 */
function resolveShell(config: { availableShells: ShellConfig[]; defaultShell: string }, preferred?: ShellConfig | string) {
  if (typeof preferred === 'object') return preferred;
  return (
    (preferred ? config.availableShells.find((s) => s.name === preferred) : undefined)
    ?? config.availableShells.find((s) => s.name === config.defaultShell)
    ?? config.availableShells[0]
  );
}

/**
 * 起一个新 PTY 并拼出 PaneState。失败时弹窗说明原因并返回 null
 * （远程断链 / 缺 ssh 客户端等 create_pty 的明确错误，不留半开状态）。
 */
async function spawnPane(
  project: ProjectConfig,
  shell: ShellConfig | undefined,
  customTitle?: string,
  cwd?: string,
): Promise<PaneState | null> {
  const remote = isRemoteProject(project);
  if (!remote && !shell) return null;
  let ptyId: number;
  try {
    ptyId = await createProjectPty(project, shell, cwd);
  } catch (e) {
    await showAlert(
      t('terminalArea.remoteConnectFailedTitle'),
      e instanceof Error ? e.message : String(e),
    );
    return null;
  }
  return {
    id: genId(),
    shellName: remote ? remotePaneLabel(project) : shell!.name,
    customTitle,
    status: 'idle',
    ptyId,
    cwd,
  };
}

/** 写布局 + 落盘（防抖）。 */
function commit(projectId: string, layout: SplitNode | null) {
  useAppStore.getState().setProjectLayout(projectId, layout);
  saveLayoutToConfig(projectId);
}

/**
 * 新建一个终端标签。
 * - 项目还没有布局：建根 leaf
 * - 已有布局：加进目标 leaf 的 tab 栏并激活——`opts.targetPaneId` 显式指定
 *   锚点 pane（分屏下点某组的 + 号），缺省回落当前活动 pane 所在 leaf
 * - `opts.cwd` / `opts.title`:worktree「在终端打开」用,在指定目录起 shell 并命名 tab
 */
export async function newTerminal(
  projectId: string,
  shell?: ShellConfig,
  opts?: { cwd?: string; title?: string; targetPaneId?: string },
): Promise<PaneState | null> {
  const { state, project } = snapshot(projectId);
  if (!project) return null;
  const resolved = isRemoteProject(project) ? undefined : resolveShell(state.config, shell);
  if (!isRemoteProject(project) && !resolved) return null;

  const pane = await spawnPane(project, resolved, opts?.title, opts?.cwd);
  if (!pane) return null;

  // await 期间布局可能已变。注意这里**不能**回落到 await 之前的快照：
  // layout 为 null 表示「期间用户把最后一个 pane 关掉了」，用旧树兜底会把
  // 那个已 kill 的 pane 复活回布局里，点开是一片永远写不进去的空白。
  const current = useAppStore.getState().projectStates.get(projectId)?.layout ?? null;
  if (!current) {
    commit(projectId, { type: 'leaf', panes: [pane], activePaneId: pane.id });
    focusPane(pane.ptyId);
    return pane;
  }
  // 显式锚点优先:resolveActivePane 靠 DOM 焦点猜「用户在哪」,焦点不在任何
  // xterm 上时会回退到第一个 leaf——分屏下点下方组的 + 号会把新 tab 加到上方组
  const anchor = opts?.targetPaneId ? findPaneById(current, opts.targetPaneId) : null;
  const active = anchor ?? resolveActivePane(current);
  const targetLeaf = active ? findLeafContainingPane(current, active.id) : collectLeaves(current)[0];
  if (!targetLeaf) {
    commit(projectId, { type: 'leaf', panes: [pane], activePaneId: pane.id });
    focusPane(pane.ptyId);
    return pane;
  }
  commit(projectId, replaceNode(current, targetLeaf, {
    ...targetLeaf,
    panes: [...targetLeaf.panes, pane],
    activePaneId: pane.id,
  }));
  focusPane(pane.ptyId);
  return pane;
}

/** 在指定 pane 处分屏；不传 paneId 时对当前活动 pane 分。
 *  返回新 pane（未能分出返回 null）；`opts.cwd` 显式指定新 PTY 的启动目录
 *  （fork 会话需要落在会话记录的目录，见 forkPaneSession）。 */
export async function splitPane(
  projectId: string,
  direction: 'horizontal' | 'vertical',
  paneId?: string,
  opts?: { cwd?: string },
): Promise<PaneState | null> {
  const { state, project, layout } = snapshot(projectId);
  if (!project || !layout) return null;
  const target = paneId ?? resolveActivePane(layout)?.id;
  if (!target) return null;
  const resolved = isRemoteProject(project) ? undefined : resolveShell(state.config);
  if (!isRemoteProject(project) && !resolved) return null;

  // 分屏继承源 pane 的 cwd 覆盖:worktree 终端分出来的屏理应还在 worktree 里
  const sourceCwd = findPaneById(layout, target)?.cwd;
  const pane = await spawnPane(project, resolved, undefined, opts?.cwd ?? sourceCwd);
  if (!pane) return null;

  // spawn 期间布局可能已变：目标 pane 被关掉、或整个项目的终端都关光了。
  // 这两种情况下新 PTY 无处安放，必须显式回收 —— 否则后端留一个谁也看不见、
  // 谁也杀不掉的孤儿子进程（远程项目就是一条挂着的 ssh）。
  const current = useAppStore.getState().projectStates.get(projectId)?.layout ?? null;
  const stillThere = current ? !!findPaneById(current, target) : false;
  if (!current || !stillThere) {
    await disposePane(pane);
    return null;
  }
  const newLeaf: SplitNode = { type: 'leaf', panes: [pane], activePaneId: pane.id };
  commit(projectId, insertSplit(current, target, direction, newLeaf));
  focusPane(pane.ptyId);
  return pane;
}

/**
 * 把 pane 里跑着的 AI 会话分支到新分屏（设计: docs/plans/2026-08-14-session-branch-tree-design.md）。
 * 原 pane 原会话继续跑；右侧分出的新 pane 写入 fork 命令（Claude:
 * `--resume {id} --fork-session`，Codex: `codex fork {id}`），PTY 内核缓冲
 * stdin、shell 就绪前写入不丢（与重启自动续接同一时序）。
 *
 * 新 PTY 的启动目录与源会话保持一致，取值链：hook 上报的 session.cwd →
 * （claude 系且无记录时）lookup_ai_session_cwd 反查（服务端带 is_dir 校验）→
 * 源 pane 目录。claude --resume 只认「启动目录」对应的会话桶，起于子目录的
 * 会话在别的目录 fork 会报 No conversation found（与 PaneGroup 续接同一坑）；
 * codex 不按目录分桶，继承源 pane 目录即可（还避开它的 resume_cwd 选目录提问）。
 * 会话目录可能已删（worktree 移除）——spawn 失败回落源 pane 默认目录重试，
 * 不让分支动作失败了事。自记账：新会话身份由 hook 上报后落边，见 store.consumePendingFork。
 */
export async function forkPaneSession(projectId: string, paneId: string): Promise<void> {
  const { layout } = snapshot(projectId);
  const source = layout ? findPaneById(layout, paneId) : null;
  const session = source?.aiSession;
  if (!session) return;
  const agent = (session.agent ?? 'claude').toLowerCase();
  const cmd = branchCapsForAgent(agent)?.forkCommand(session.sessionId);
  if (!cmd) return;

  let cwd = session.cwd;
  if (!cwd && agent !== 'codex' && /^[A-Za-z0-9_-]+$/.test(session.sessionId)) {
    cwd = (await invoke<string | null>('lookup_ai_session_cwd', { sessionId: session.sessionId })
      .catch(() => null)) ?? undefined;
  }
  let pane = await splitPane(projectId, 'horizontal', paneId, { cwd });
  if (!pane && cwd) {
    pane = await splitPane(projectId, 'horizontal', paneId);
  }
  if (!pane || pane.ptyId === undefined) return;
  registerPendingFork(pane.ptyId, agent, session.sessionId);
  void writePtyInput(pane.ptyId, `${cmd}\r`);
}

/** 回收一个 pane 的运行时资源（后端 PTY 子进程 + 前端 xterm 实例 + markers）。 */
async function disposePane(pane: PaneState): Promise<void> {
  if (pane.ptyId === undefined) return;
  // 查找条正指着这个终端的话先关掉：它的定位是每帧 rAF 轮询目标元素，
  // pane 没了之后既看不见也点不着，循环却会一直空转下去
  closeTerminalSearchFor(pane.ptyId);
  await invoke('kill_pty', { ptyId: pane.ptyId }).catch(() => {});
  disposeTerminal(pane.ptyId);
  useAppStore.getState().clearMarkersForPty(pane.ptyId);
}

/**
 * 关闭一个 pane。`confirm=false` 用于「关整组」这种已经确认过一次的场景，
 * 避免一次操作弹 N 个确认框。
 */
export async function closePane(
  projectId: string,
  paneId?: string,
  opts: { confirm?: boolean } = {},
): Promise<void> {
  const { layout } = snapshot(projectId);
  if (!layout) return;
  const target = paneId ?? resolveActivePane(layout)?.id;
  if (!target) return;
  const pane = findPaneById(layout, target);
  if (!pane) return;

  if (opts.confirm !== false) {
    const label = paneDisplayLabel(projectId, pane);
    const hasAi = pane.status === 'ai-working' || pane.status === 'ai-idle';
    const ok = await showConfirm(
      hasAi ? t('paneGroup.closeAiTitle') : t('paneGroup.closeTerminalTitle'),
      hasAi
        ? t('paneGroup.closeTabAiMessage', { label })
        : t('paneGroup.closeTabMessage', { label }),
    );
    if (!ok) return;
  }

  // 重新按 id 取一次：确认框开着的这段时间里，pane 可能刚 hydrate 完拿到 ptyId。
  // 用确认前的快照去 dispose 会因 ptyId 还是 undefined 而直接跳过，
  // 结果是 pane 从布局里没了、后端 PTY 和前端 xterm 实例却都还活着。
  const current = useAppStore.getState().projectStates.get(projectId)?.layout ?? null;
  if (!current) return;
  const fresh = findPaneById(current, target);
  await disposePane(fresh ?? pane);
  commit(projectId, removePaneFromLayout(current, target));
}

/** 关闭整个 pane group（一个 leaf 及其所有 tab）。 */
export async function closeLeaf(projectId: string, paneId: string): Promise<void> {
  const { layout } = snapshot(projectId);
  if (!layout) return;
  const leaf = findLeafContainingPane(layout, paneId);
  if (!leaf) return;

  const aiCount = leaf.panes.filter(
    (p) => p.status === 'ai-working' || p.status === 'ai-idle',
  ).length;
  const ok = await showConfirm(
    aiCount > 0 ? t('paneGroup.closeAiTitle') : t('paneGroup.closeTerminalTitle'),
    aiCount > 0
      ? t('paneGroup.closeGroupAiMessage', { count: aiCount })
      : t('paneGroup.closeGroupMessage'),
  );
  if (!ok) return;

  // 同 closePane：确认之后按 id 从最新布局重取，避免回收到陈旧的 ptyId 快照
  let current = useAppStore.getState().projectStates.get(projectId)?.layout ?? null;
  for (const stale of leaf.panes) {
    const fresh = current ? findPaneById(current, stale.id) : null;
    await disposePane(fresh ?? stale);
  }
  for (const pane of leaf.panes) {
    if (!current) break;
    current = removePaneFromLayout(current, pane.id);
  }
  commit(projectId, current);
}

/** pane 的显示名：自定义名 > 远程连接名 > shell 名。 */
export function paneDisplayLabel(projectId: string, pane: PaneState): string {
  if (pane.customTitle) return pane.customTitle;
  const project = useAppStore.getState().config.projects.find((p) => p.id === projectId);
  if (project && isRemoteProject(project)) return remotePaneLabel(project);
  return pane.shellName;
}

/** 重命名 pane（空串 = 清掉自定义名，回落 shell 名）。 */
export async function renamePane(projectId: string, paneId?: string): Promise<void> {
  const { layout } = snapshot(projectId);
  if (!layout) return;
  const target = paneId ?? resolveActivePane(layout)?.id;
  if (!target) return;
  const pane = findPaneById(layout, target);
  if (!pane) return;

  // 第三个参数才是预填值：之前传成了 placeholder，弹出来是个空框，
  // 想微调名字得整个重打，「清空以回落默认名」也无从表达
  const current0 = paneDisplayLabel(projectId, pane);
  const next = await showPrompt(t('paneGroup.renameTerminal'), current0, current0);
  if (next === null) return; // 取消；空串是有效输入（清掉自定义名）

  const current = useAppStore.getState().projectStates.get(projectId)?.layout;
  if (!current) return;
  commit(projectId, updateLeafOfPane(current, target, (leaf) => ({
    ...leaf,
    panes: leaf.panes.map((p) =>
      p.id === target ? { ...p, customTitle: next.trim() || undefined } : p,
    ),
  })));
}

/** 激活 leaf 内的某个 pane（tab 切换）。 */
export function activatePane(projectId: string, paneId: string): void {
  const { layout } = snapshot(projectId);
  if (!layout) return;
  const leaf = findLeafContainingPane(layout, paneId);
  if (!leaf || leaf.activePaneId === paneId) {
    focusPaneById(layout, paneId);
    return;
  }
  commit(projectId, replaceNode(layout, leaf, { ...leaf, activePaneId: paneId }));
  focusPaneById(layout, paneId);
}

/** 在当前 leaf 的 tab 栏内前后切换。 */
export function cyclePane(projectId: string, delta: 1 | -1): void {
  const { layout } = snapshot(projectId);
  if (!layout) return;
  const active = resolveActivePane(layout);
  if (!active) return;
  const leaf = findLeafContainingPane(layout, active.id);
  if (!leaf || leaf.panes.length < 2) return;
  const idx = leaf.panes.findIndex((p) => p.id === active.id);
  // 环形：最后一个再往后回到第一个，符合 Ctrl+Tab 的普遍预期
  const next = leaf.panes[(idx + delta + leaf.panes.length) % leaf.panes.length];
  activatePane(projectId, next.id);
}

/** 选中当前 leaf 的第 n 个 tab（1-based）；越界时不动。 */
export function selectPaneByIndex(projectId: string, n: number): void {
  const { layout } = snapshot(projectId);
  if (!layout) return;
  const active = resolveActivePane(layout);
  if (!active) return;
  const leaf = findLeafContainingPane(layout, active.id);
  const pane = leaf?.panes[n - 1];
  if (pane) activatePane(projectId, pane.id);
}

/** 把键盘焦点移到相邻分屏。 */
export function focusAdjacentPane(projectId: string, dir: Direction): void {
  const { layout } = snapshot(projectId);
  if (!layout) return;
  const active = resolveActivePane(layout);
  if (!active || active.ptyId === undefined) return;
  const targetPtyId = findAdjacentPtyId(active.ptyId, dir);
  if (targetPtyId == null) return;
  const pane = findPaneByPtyId(layout, targetPtyId);
  if (pane) activatePane(projectId, pane.id);
}

/** 把键盘焦点交给某个终端（xterm 自己会聚焦其 helper textarea）。 */
export function focusPane(ptyId: number | undefined): void {
  if (ptyId === undefined) return;
  // 布局刚变，DOM 还没挂上；下一帧再聚焦
  requestAnimationFrame(() => getCachedTerminal(ptyId)?.term.focus());
}

function focusPaneById(layout: SplitNode, paneId: string): void {
  focusPane(findPaneById(layout, paneId)?.ptyId);
}
