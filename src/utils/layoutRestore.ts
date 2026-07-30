import type {
  AppConfig,
  PaneState,
  ProjectState,
  SavedPane,
  SavedProjectLayout,
  SavedSplitNode,
  SplitNode,
} from '../types';

function resolveShellName(savedPane: SavedPane, config: Pick<AppConfig, 'availableShells' | 'defaultShell'>): string | null {
  const shell =
    config.availableShells.find((s) => s.name === savedPane.shellName)
    ?? config.availableShells.find((s) => s.name === config.defaultShell)
    ?? config.availableShells[0];
  return shell?.name ?? null;
}

export function restoreSavedSplitNode(
  saved: SavedSplitNode,
  config: Pick<AppConfig, 'availableShells' | 'defaultShell'>,
  createId: () => string,
): SplitNode | null {
  if (saved.type === 'leaf') {
    const legacyPane = (saved as unknown as { pane?: SavedPane }).pane;
    const savedPanes = saved.panes ?? [legacyPane].filter(Boolean) as SavedPane[];
    const panes: PaneState[] = [];

    for (const savedPane of savedPanes) {
      const shellName = resolveShellName(savedPane, config);
      if (!shellName) continue;
      panes.push({
        id: createId(),
        shellName,
        status: 'idle',
        cwd: savedPane.cwd,
      });
    }

    if (panes.length === 0) return null;
    return {
      type: 'leaf',
      panes,
      activePaneId: panes[0].id,
    };
  }

  const children: SplitNode[] = [];
  for (const child of saved.children) {
    const restored = restoreSavedSplitNode(child, config, createId);
    if (restored) children.push(restored);
  }

  if (children.length === 0) return null;
  if (children.length === 1) return children[0];
  return {
    type: 'split',
    direction: saved.direction,
    children,
    sizes: children.length === saved.sizes.length
      ? [...saved.sizes]
      : children.map(() => 100 / children.length),
  };
}

/** 收集一棵树里的全部 pane（深度优先，左到右）。 */
function collectPanes(node: SplitNode, out: PaneState[]): void {
  if (node.type === 'leaf') {
    out.push(...node.panes);
    return;
  }
  for (const child of node.children) collectPanes(child, out);
}

/** 把 pane 追加到树最左侧 leaf 的 tab 栏末尾，不动 activePaneId。 */
function appendPanesToFirstLeaf(node: SplitNode, panes: PaneState[]): SplitNode {
  if (panes.length === 0) return node;
  if (node.type === 'leaf') return { ...node, panes: [...node.panes, ...panes] };
  return {
    ...node,
    children: [appendPanesToFirstLeaf(node.children[0], panes), ...node.children.slice(1)],
  };
}

export function restoreSavedProjectLayout(
  projectId: string,
  savedLayout: SavedProjectLayout,
  config: Pick<AppConfig, 'availableShells' | 'defaultShell'>,
  createId: () => string,
): ProjectState | null {
  const trees: SplitNode[] = [];
  for (const savedTab of savedLayout.tabs) {
    const tree = restoreSavedSplitNode(savedTab.splitLayout, config, createId);
    if (tree) trees.push(tree);
  }
  if (trees.length === 0) return null;

  // 迁移旧配置的多 tab：项目级 tab 层已删除，直接丢掉多出来的树会静默吃掉用户的终端。
  // 把它们的 pane 平铺进保留那棵树最左侧 leaf 的 tab 栏 —— 布局塌成一个，但一个终端不少。
  // activeTabIndex 决定保留哪棵（用户上次看的那个留在原位）。
  const keepIdx = savedLayout.activeTabIndex >= 0 && savedLayout.activeTabIndex < trees.length
    ? savedLayout.activeTabIndex
    : 0;
  let layout = trees[keepIdx];
  if (trees.length > 1) {
    const extras: PaneState[] = [];
    trees.forEach((tree, i) => {
      if (i !== keepIdx) collectPanes(tree, extras);
    });
    layout = appendPanesToFirstLeaf(layout, extras);
  }

  return { id: projectId, layout, status: 'idle' };
}
