/**
 * 布局树（SplitNode）的查询与变换。
 *
 * 从 TerminalArea / PaneGroup 里抽出来，因为快捷键链路也要做同样的事——
 * 「当前是哪个 pane」「往哪棵 leaf 加 pane」这类逻辑散在组件里，键盘入口就只能
 * 复制一份，两份迟早走岔。
 */
import type { PaneState, SplitNode } from '../types';

export type LeafNode = Extract<SplitNode, { type: 'leaf' }>;

/** 深度优先收集所有 leaf（左到右，与屏幕上的排列同序）。 */
export function collectLeaves(node: SplitNode, out: LeafNode[] = []): LeafNode[] {
  if (node.type === 'leaf') {
    out.push(node);
    return out;
  }
  for (const child of node.children) collectLeaves(child, out);
  return out;
}

/** 深度优先收集所有 pane。 */
export function collectPanes(node: SplitNode, out: PaneState[] = []): PaneState[] {
  if (node.type === 'leaf') {
    out.push(...node.panes);
    return out;
  }
  for (const child of node.children) collectPanes(child, out);
  return out;
}

export function findPaneById(node: SplitNode, paneId: string): PaneState | null {
  for (const pane of collectPanes(node)) {
    if (pane.id === paneId) return pane;
  }
  return null;
}

/** 找到持有该 pane 的 leaf。 */
export function findLeafContainingPane(node: SplitNode, paneId: string): LeafNode | null {
  for (const leaf of collectLeaves(node)) {
    if (leaf.panes.some((p) => p.id === paneId)) return leaf;
  }
  return null;
}

export function findPaneByPtyId(node: SplitNode, ptyId: number): PaneState | null {
  for (const pane of collectPanes(node)) {
    if (pane.ptyId === ptyId) return pane;
  }
  return null;
}

/**
 * 用 DOM 焦点定位「用户此刻在操作哪个 pane」。
 *
 * 多分屏下这是最准确的信号：用户输入时 xterm 的 helper textarea 是
 * `document.activeElement`，其最近的 `[data-pty-id]` 祖先就是所在 pane。
 * 焦点在弹窗 / 侧栏 / body 时返回 null，由调用方回退。
 */
export function focusedPtyIdFromDom(): number | null {
  const active = document.activeElement;
  if (!active) return null;
  const el = (active as Element).closest('[data-pty-id]') as HTMLElement | null;
  if (!el?.dataset.ptyId) return null;
  const id = Number(el.dataset.ptyId);
  return Number.isFinite(id) ? id : null;
}

/**
 * 当前活动 pane：先看 DOM 焦点，落不到就用布局里第一个 leaf 的 activePaneId。
 * 返回 null 表示这个项目还没有终端。
 */
export function resolveActivePane(layout: SplitNode | null): PaneState | null {
  if (!layout) return null;
  const ptyId = focusedPtyIdFromDom();
  if (ptyId != null) {
    const pane = findPaneByPtyId(layout, ptyId);
    if (pane) return pane;
  }
  const first = collectLeaves(layout)[0];
  if (!first) return null;
  return first.panes.find((p) => p.id === first.activePaneId) ?? first.panes[0] ?? null;
}

/** 按 leaf 引用替换树中的节点（引用相等即命中）。 */
export function replaceNode(node: SplitNode, target: SplitNode, next: SplitNode): SplitNode {
  if (node === target) return next;
  if (node.type === 'leaf') return node;
  let changed = false;
  const children = node.children.map((child) => {
    const updated = replaceNode(child, target, next);
    if (updated !== child) changed = true;
    return updated;
  });
  return changed ? { ...node, children } : node;
}

/** 对持有该 pane 的 leaf 做变换，返回新树（未命中返回原树）。 */
export function updateLeafOfPane(
  root: SplitNode,
  paneId: string,
  updater: (leaf: LeafNode) => SplitNode,
): SplitNode {
  const leaf = findLeafContainingPane(root, paneId);
  if (!leaf) return root;
  return replaceNode(root, leaf, updater(leaf));
}

/** 在目标 pane 所在 leaf 处插入分屏，新 leaf 放在第二格。 */
export function insertSplit(
  node: SplitNode,
  targetPaneId: string,
  direction: 'horizontal' | 'vertical',
  newLeaf: SplitNode,
): SplitNode {
  if (node.type === 'leaf') {
    if (node.panes.some((p) => p.id === targetPaneId)) {
      return { type: 'split', direction, children: [node, newLeaf], sizes: [50, 50] };
    }
    return node;
  }
  return {
    ...node,
    children: node.children.map((c) => insertSplit(c, targetPaneId, direction, newLeaf)),
  };
}

/**
 * 从树里摘掉一个 pane。
 * - leaf 里还有别的 pane：只摘 pane，必要时改 activePaneId
 * - leaf 空了：把 leaf 从父 split 摘掉；父 split 只剩一个孩子则塌陷成那个孩子
 * - 整棵树空了：返回 null（调用方据此回到空态）
 */
export function removePaneFromLayout(root: SplitNode, paneId: string): SplitNode | null {
  function walk(node: SplitNode): SplitNode | null {
    if (node.type === 'leaf') {
      if (!node.panes.some((p) => p.id === paneId)) return node;
      const panes = node.panes.filter((p) => p.id !== paneId);
      if (panes.length === 0) return null;
      const activePaneId = node.activePaneId === paneId
        ? panes[panes.length - 1].id
        : node.activePaneId;
      return { ...node, panes, activePaneId };
    }
    const children = node.children
      .map(walk)
      .filter((c): c is SplitNode => c !== null);
    if (children.length === 0) return null;
    if (children.length === 1) return children[0];
    return {
      ...node,
      children,
      // 子节点数变化后旧 sizes 对不上，均分比按旧值截断更不容易出怪布局
      sizes: children.length === node.children.length
        ? node.sizes
        : children.map(() => 100 / children.length),
    };
  }
  return walk(root);
}

/** 几何方向 */
export type Direction = 'left' | 'right' | 'up' | 'down';

/**
 * 按屏幕几何找相邻 pane —— 用实际渲染位置而不是在树上推导方向：
 * 树形结构里「右边」可能跨好几层 split，几何最近邻既简单又与用户所见一致。
 *
 * 打分 = 主轴距离 + 交叉轴错位惩罚，取同方向上最近的一个。
 */
export function findAdjacentPtyId(fromPtyId: number, dir: Direction): number | null {
  const nodes = Array.from(document.querySelectorAll<HTMLElement>('[data-pty-id]'))
    // PaneGroup 根与 TerminalInstance 的 dropZone 都带 data-pty-id，会重复；
    // 只取实际渲染出终端的那层（有尺寸的最内层），按 ptyId 去重取最小面积者
    .filter((el) => el.offsetParent !== null);
  if (nodes.length === 0) return null;

  const rects = new Map<number, DOMRect>();
  for (const el of nodes) {
    const id = Number(el.dataset.ptyId);
    if (!Number.isFinite(id)) continue;
    const rect = el.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) continue;
    const existing = rects.get(id);
    if (!existing || rect.width * rect.height < existing.width * existing.height) {
      rects.set(id, rect);
    }
  }

  const from = rects.get(fromPtyId);
  if (!from) return null;
  const fromCx = from.left + from.width / 2;
  const fromCy = from.top + from.height / 2;

  let best: { id: number; score: number } | null = null;
  for (const [id, rect] of rects) {
    if (id === fromPtyId) continue;
    const cx = rect.left + rect.width / 2;
    const cy = rect.top + rect.height / 2;
    const dx = cx - fromCx;
    const dy = cy - fromCy;

    let main: number;
    let cross: number;
    switch (dir) {
      case 'left': main = -dx; cross = Math.abs(dy); break;
      case 'right': main = dx; cross = Math.abs(dy); break;
      case 'up': main = -dy; cross = Math.abs(dx); break;
      case 'down': main = dy; cross = Math.abs(dx); break;
    }
    // 必须确实在该方向上（留 1px 容差，避免等宽分屏的浮点边界）
    if (main <= 1) continue;
    const score = main + cross * 2;
    if (!best || score < best.score) best = { id, score };
  }
  return best?.id ?? null;
}
