import { create } from 'zustand';
import type {
  AppConfig,
  ProjectConfig,
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
      const newConfig = { ...state.config, projects: [...state.config.projects, project] };
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

}));
