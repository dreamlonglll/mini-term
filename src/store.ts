import { create } from 'zustand';
import { invoke } from '@tauri-apps/api/core';
import type {
  AppConfig,
  ProjectConfig,
  ProjectGroup,
  ProjectState,
  TerminalTab,
  SplitNode,
  PaneStatus,
  SavedSplitNode,
  SavedTab,
  SavedProjectLayout,
} from './types';

// 生成唯一 ID
let idCounter = 0;
export const genId = () => `id-${Date.now()}-${++idCounter}`;

// 计算 Tab 聚合状态
const STATUS_PRIORITY: Record<PaneStatus, number> = {
  error: 3,
  'ai-working': 2,
  'ai-idle': 1,
  idle: 0,
};

function getHighestStatus(node: SplitNode): PaneStatus {
  if (node.type === 'leaf') return node.pane.status;
  return node.children.reduce<PaneStatus>((acc, child) => {
    const s = getHighestStatus(child);
    return STATUS_PRIORITY[s] > STATUS_PRIORITY[acc] ? s : acc;
  }, 'idle');
}

// 在 SplitNode 中更新指定 pane 的状态
function updatePaneStatus(node: SplitNode, ptyId: number, status: PaneStatus): SplitNode {
  if (node.type === 'leaf') {
    if (node.pane.ptyId === ptyId) {
      return { ...node, pane: { ...node.pane, status } };
    }
    return node;
  }
  return {
    ...node,
    children: node.children.map((c) => updatePaneStatus(c, ptyId, status)),
  };
}

// 收集所有 pane 的 ptyId
export function collectPtyIds(node: SplitNode): number[] {
  if (node.type === 'leaf') return [node.pane.ptyId];
  return node.children.flatMap(collectPtyIds);
}

// 序列化 SplitNode 树（剥离运行时数据）
function serializeSplitNode(node: SplitNode): SavedSplitNode {
  if (node.type === 'leaf') {
    return { type: 'leaf', pane: { shellName: node.pane.shellName } };
  }
  return {
    type: 'split',
    direction: node.direction,
    children: node.children.map(serializeSplitNode),
    sizes: [...node.sizes],
  };
}

export function serializeLayout(ps: ProjectState): SavedProjectLayout {
  const tabs: SavedTab[] = ps.tabs.map((tab) => ({
    customTitle: tab.customTitle,
    splitLayout: serializeSplitNode(tab.splitLayout),
  }));
  const activeTabIndex = ps.tabs.findIndex((t) => t.id === ps.activeTabId);
  return { tabs, activeTabIndex: activeTabIndex >= 0 ? activeTabIndex : 0 };
}

// 反序列化：重建 SplitNode 树并创建 PTY
async function restoreSplitNode(
  saved: SavedSplitNode,
  projectPath: string,
  config: AppConfig,
): Promise<SplitNode | null> {
  if (saved.type === 'leaf') {
    const shell =
      config.availableShells.find((s) => s.name === saved.pane.shellName)
      ?? config.availableShells.find((s) => s.name === config.defaultShell)
      ?? config.availableShells[0];
    if (!shell) return null;
    try {
      const ptyId = await invoke<number>('create_pty', {
        shell: shell.command,
        args: shell.args ?? [],
        cwd: projectPath,
      });
      return {
        type: 'leaf',
        pane: { id: genId(), shellName: shell.name, status: 'idle' as PaneStatus, ptyId },
      };
    } catch {
      return null;
    }
  }

  const children: SplitNode[] = [];
  for (const child of saved.children) {
    const restored = await restoreSplitNode(child, projectPath, config);
    if (restored) children.push(restored);
  }
  if (children.length === 0) return null;
  if (children.length === 1) return children[0];
  return {
    type: 'split',
    direction: saved.direction,
    children,
    sizes: children.map(() => 100 / children.length),
  };
}

export async function restoreLayout(
  projectId: string,
  savedLayout: SavedProjectLayout,
  projectPath: string,
  config: AppConfig,
): Promise<void> {
  const tabs: TerminalTab[] = [];
  for (const savedTab of savedLayout.tabs) {
    const layout = await restoreSplitNode(savedTab.splitLayout, projectPath, config);
    if (layout) {
      tabs.push({
        id: genId(),
        customTitle: savedTab.customTitle,
        splitLayout: layout,
        status: 'idle',
      });
    }
  }
  if (tabs.length === 0) return;
  const activeTabId = tabs[savedLayout.activeTabIndex]?.id ?? tabs[0]?.id ?? '';
  useAppStore.setState((state) => {
    const newStates = new Map(state.projectStates);
    newStates.set(projectId, { id: projectId, tabs, activeTabId });
    return { projectStates: newStates };
  });
}

// 每个项目独立的防抖 timer
const saveLayoutTimers = new Map<string, ReturnType<typeof setTimeout>>();

function doSaveLayout(projectId: string) {
  const { config, projectStates } = useAppStore.getState();
  const ps = projectStates.get(projectId);
  if (!ps) return;
  const savedLayout = serializeLayout(ps);
  const newConfig = {
    ...config,
    projects: config.projects.map((p) =>
      p.id === projectId ? { ...p, savedLayout } : p
    ),
  };
  useAppStore.getState().setConfig(newConfig);
  invoke('save_config', { config: newConfig });
}

export function saveLayoutToConfig(projectId: string) {
  const existing = saveLayoutTimers.get(projectId);
  if (existing) clearTimeout(existing);
  saveLayoutTimers.set(projectId, setTimeout(() => {
    saveLayoutTimers.delete(projectId);
    doSaveLayout(projectId);
  }, 500));
}

// 立即保存（不防抖，用于 beforeunload / 项目切换）
export function flushLayoutToConfig(projectId: string) {
  const existing = saveLayoutTimers.get(projectId);
  if (existing) {
    clearTimeout(existing);
    saveLayoutTimers.delete(projectId);
  }
  doSaveLayout(projectId);
}

// 渲染列表项类型
export type OrderedItem =
  | { type: 'project'; project: ProjectConfig }
  | { type: 'group'; group: ProjectGroup; projects: ProjectConfig[] };

// 从 config 计算侧栏渲染列表
export function getOrderedItems(config: AppConfig): OrderedItem[] {
  const { projects, projectGroups, projectOrdering } = config;
  if (!projectOrdering || projectOrdering.length === 0) {
    return projects.map((p) => ({ type: 'project', project: p }));
  }

  const projectMap = new Map(projects.map((p) => [p.id, p]));
  const groupMap = new Map((projectGroups ?? []).map((g) => [g.id, g]));
  const seen = new Set<string>();
  const result: OrderedItem[] = [];

  for (const itemId of projectOrdering) {
    const group = groupMap.get(itemId);
    if (group) {
      seen.add(itemId);
      const groupProjects = group.projectIds
        .map((pid) => projectMap.get(pid))
        .filter((p): p is ProjectConfig => !!p);
      groupProjects.forEach((p) => seen.add(p.id));
      result.push({ type: 'group', group, projects: groupProjects });
    } else {
      const project = projectMap.get(itemId);
      if (project) {
        seen.add(itemId);
        result.push({ type: 'project', project });
      }
    }
  }

  // 追加不在 ordering 中的项目（新添加的项目）
  for (const p of projects) {
    if (!seen.has(p.id)) {
      result.push({ type: 'project', project: p });
    }
  }

  return result;
}

// 确保 ordering 已初始化
function ensureOrdering(config: AppConfig): AppConfig {
  if (config.projectOrdering && config.projectOrdering.length > 0) return config;
  return {
    ...config,
    projectOrdering: config.projects.map((p) => p.id),
    projectGroups: config.projectGroups ?? [],
  };
}

interface AppStore {
  // 配置
  config: AppConfig;
  setConfig: (config: AppConfig) => void;

  // 项目
  activeProjectId: string | null;
  projectStates: Map<string, ProjectState>;
  setActiveProject: (id: string) => void;
  addProject: (project: ProjectConfig) => void;
  removeProject: (id: string) => void;

  // Tab
  addTab: (projectId: string, tab: TerminalTab) => void;
  removeTab: (projectId: string, tabId: string) => void;
  setActiveTab: (projectId: string, tabId: string) => void;
  updateTabLayout: (projectId: string, tabId: string, layout: SplitNode) => void;

  // Pane 状态
  updatePaneStatusByPty: (ptyId: number, status: PaneStatus) => void;

  // 分组
  createGroup: (name: string) => void;
  removeGroup: (groupId: string) => void;
  renameGroup: (groupId: string, name: string) => void;
  toggleGroupCollapse: (groupId: string) => void;
  moveProjectToGroup: (projectId: string, groupId: string, index?: number) => void;
  moveProjectOutOfGroup: (projectId: string, insertAfter?: string) => void;
  reorderItems: (ordering: string[]) => void;
}

export const useAppStore = create<AppStore>((set) => ({
  config: {
    projects: [],
    defaultShell: '',
    availableShells: [],
    uiFontSize: 13,
    terminalFontSize: 14,
  },
  setConfig: (config) => set({ config }),

  activeProjectId: null,
  projectStates: new Map(),

  setActiveProject: (id) => set({ activeProjectId: id }),

  addProject: (project) =>
    set((state) => {
      const newConfig = {
        ...state.config,
        projects: [...state.config.projects, project],
        // 如果 ordering 已初始化，追加新项目
        projectOrdering: state.config.projectOrdering
          ? [...state.config.projectOrdering, project.id]
          : undefined,
      };
      const newStates = new Map(state.projectStates);
      newStates.set(project.id, { id: project.id, tabs: [], activeTabId: '' });
      return {
        config: newConfig,
        projectStates: newStates,
        activeProjectId: state.activeProjectId ?? project.id,
      };
    }),

  removeProject: (id) =>
    set((state) => {
      const newConfig = {
        ...state.config,
        projects: state.config.projects.filter((p) => p.id !== id),
        projectOrdering: state.config.projectOrdering?.filter((item) => item !== id),
        projectGroups: state.config.projectGroups?.map((g) => ({
          ...g,
          projectIds: g.projectIds.filter((pid) => pid !== id),
        })),
      };
      const newStates = new Map(state.projectStates);
      newStates.delete(id);
      const newActive =
        state.activeProjectId === id
          ? newConfig.projects[0]?.id ?? null
          : state.activeProjectId;
      return { config: newConfig, projectStates: newStates, activeProjectId: newActive };
    }),

  addTab: (projectId, tab) =>
    set((state) => {
      const newStates = new Map(state.projectStates);
      const ps = newStates.get(projectId);
      if (!ps) return state;
      newStates.set(projectId, {
        ...ps,
        tabs: [...ps.tabs, tab],
        activeTabId: tab.id,
      });
      return { projectStates: newStates };
    }),

  removeTab: (projectId, tabId) =>
    set((state) => {
      const newStates = new Map(state.projectStates);
      const ps = newStates.get(projectId);
      if (!ps) return state;
      const newTabs = ps.tabs.filter((t) => t.id !== tabId);
      const newActive =
        ps.activeTabId === tabId ? (newTabs[newTabs.length - 1]?.id ?? '') : ps.activeTabId;
      newStates.set(projectId, { ...ps, tabs: newTabs, activeTabId: newActive });
      return { projectStates: newStates };
    }),

  setActiveTab: (projectId, tabId) =>
    set((state) => {
      const newStates = new Map(state.projectStates);
      const ps = newStates.get(projectId);
      if (!ps) return state;
      newStates.set(projectId, { ...ps, activeTabId: tabId });
      return { projectStates: newStates };
    }),

  updateTabLayout: (projectId, tabId, layout) =>
    set((state) => {
      const newStates = new Map(state.projectStates);
      const ps = newStates.get(projectId);
      if (!ps) return state;
      newStates.set(projectId, {
        ...ps,
        tabs: ps.tabs.map((t) =>
          t.id === tabId ? { ...t, splitLayout: layout, status: getHighestStatus(layout) } : t
        ),
      });
      return { projectStates: newStates };
    }),

  updatePaneStatusByPty: (ptyId, status) =>
    set((state) => {
      // 快速检查：是否有任何 pane 包含此 ptyId
      let found = false;
      for (const ps of state.projectStates.values()) {
        if (found) break;
        for (const tab of ps.tabs) {
          if (collectPtyIds(tab.splitLayout).includes(ptyId)) { found = true; break; }
        }
      }
      if (!found) return state;

      const newStates = new Map(state.projectStates);
      let changed = false;
      for (const [pid, ps] of newStates) {
        let tabsChanged = false;
        const updatedTabs = ps.tabs.map((tab) => {
          const newLayout = updatePaneStatus(tab.splitLayout, ptyId, status);
          if (newLayout === tab.splitLayout) return tab;
          tabsChanged = true;
          return { ...tab, splitLayout: newLayout, status: getHighestStatus(newLayout) };
        });
        if (tabsChanged) {
          newStates.set(pid, { ...ps, tabs: updatedTabs });
          changed = true;
        }
      }
      return changed ? { projectStates: newStates } : state;
    }),

  createGroup: (name) =>
    set((state) => {
      const config = ensureOrdering(state.config);
      const group: ProjectGroup = { id: genId(), name, collapsed: false, projectIds: [] };
      return {
        config: {
          ...config,
          projectGroups: [...(config.projectGroups ?? []), group],
          projectOrdering: [...(config.projectOrdering ?? []), group.id],
        },
      };
    }),

  removeGroup: (groupId) =>
    set((state) => {
      const groups = state.config.projectGroups ?? [];
      const group = groups.find((g) => g.id === groupId);
      if (!group) return state;
      const ordering = state.config.projectOrdering ?? [];
      const idx = ordering.indexOf(groupId);
      // 将分组内的项目释放到 ordering 中原分组位置
      const newOrdering = [...ordering];
      if (idx >= 0) {
        newOrdering.splice(idx, 1, ...group.projectIds);
      }
      return {
        config: {
          ...state.config,
          projectGroups: groups.filter((g) => g.id !== groupId),
          projectOrdering: newOrdering,
        },
      };
    }),

  renameGroup: (groupId, name) =>
    set((state) => ({
      config: {
        ...state.config,
        projectGroups: (state.config.projectGroups ?? []).map((g) =>
          g.id === groupId ? { ...g, name } : g
        ),
      },
    })),

  toggleGroupCollapse: (groupId) =>
    set((state) => ({
      config: {
        ...state.config,
        projectGroups: (state.config.projectGroups ?? []).map((g) =>
          g.id === groupId ? { ...g, collapsed: !g.collapsed } : g
        ),
      },
    })),

  moveProjectToGroup: (projectId, groupId, index) =>
    set((state) => {
      const config = ensureOrdering(state.config);
      // 从 ordering 顶层移除（如果在顶层）
      const newOrdering = (config.projectOrdering ?? []).filter((id) => id !== projectId);
      // 从所有分组中移除
      const newGroups = (config.projectGroups ?? []).map((g) => {
        if (g.id === groupId) {
          const ids = g.projectIds.filter((pid) => pid !== projectId);
          const insertIdx = index !== undefined ? Math.min(index, ids.length) : ids.length;
          ids.splice(insertIdx, 0, projectId);
          return { ...g, projectIds: ids };
        }
        return { ...g, projectIds: g.projectIds.filter((pid) => pid !== projectId) };
      });
      return { config: { ...config, projectOrdering: newOrdering, projectGroups: newGroups } };
    }),

  moveProjectOutOfGroup: (projectId, insertAfter) =>
    set((state) => {
      const config = ensureOrdering(state.config);
      // 从所有分组中移除
      const newGroups = (config.projectGroups ?? []).map((g) => ({
        ...g,
        projectIds: g.projectIds.filter((pid) => pid !== projectId),
      }));
      // 插入到 ordering 中
      const newOrdering = (config.projectOrdering ?? []).filter((id) => id !== projectId);
      if (insertAfter) {
        const idx = newOrdering.indexOf(insertAfter);
        newOrdering.splice(idx >= 0 ? idx + 1 : newOrdering.length, 0, projectId);
      } else {
        newOrdering.push(projectId);
      }
      return { config: { ...config, projectOrdering: newOrdering, projectGroups: newGroups } };
    }),

  reorderItems: (ordering) =>
    set((state) => ({
      config: { ...state.config, projectOrdering: ordering },
    })),

}));
