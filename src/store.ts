import { create } from 'zustand';
import { invoke } from '@tauri-apps/api/core';
import { getCurrentWindow, UserAttentionType } from '@tauri-apps/api/window';
import type {
  AppConfig,
  ProjectConfig,
  ProjectGroup,
  ProjectState,
  TerminalTab,
  SplitNode,
  PaneState,
  PaneStatus,
  SavedSplitNode,
  SavedTab,
  SavedProjectLayout,
  AiCompletionNotification,
  AiMarker,
  AiUserSubmitPayload,
  CcConnectStatus,
} from './types';
import { restoreSavedProjectLayout } from './utils/layoutRestore';
import { playNotificationSound } from './utils/notificationSound';
import {
  deepCloneTree,
  removeFromTree,
  insertIntoTree,
  updateGroupInTree,
  removeGroupAndPromoteChildren,
  removeProjectFromTree,
  migrateToTree,
} from './utils/projectTree';
import { clearProjectCache } from './utils/projectDataCache';

// 生成唯一 ID
let idCounter = 0;
export const genId = () => `id-${Date.now()}-${++idCounter}`;

// 计算 Tab 聚合状态
export const STATUS_PRIORITY: Record<PaneStatus, number> = {
  error: 3,
  'ai-working': 2,
  'ai-idle': 1,
  idle: 0,
};

export function getHighestStatus(node: SplitNode): PaneStatus {
  if (node.type === 'leaf') {
    return node.panes.reduce<PaneStatus>((acc, p) => {
      return STATUS_PRIORITY[p.status] > STATUS_PRIORITY[acc] ? p.status : acc;
    }, 'idle');
  }
  return node.children.reduce<PaneStatus>((acc, child) => {
    const s = getHighestStatus(child);
    return STATUS_PRIORITY[s] > STATUS_PRIORITY[acc] ? s : acc;
  }, 'idle');
}

// 在 SplitNode 中更新指定 pane 的状态
function updatePaneStatus(node: SplitNode, ptyId: number, status: PaneStatus): SplitNode {
  if (node.type === 'leaf') {
    const idx = node.panes.findIndex((p) => p.ptyId === ptyId);
    if (idx >= 0) {
      const newPanes = [...node.panes];
      newPanes[idx] = { ...newPanes[idx], status };
      return { ...node, panes: newPanes };
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
  if (node.type === 'leaf') return node.panes.flatMap((p) => p.ptyId === undefined ? [] : [p.ptyId]);
  return node.children.flatMap(collectPtyIds);
}

// 查找 ptyId 所属的 pane（按 SplitNode 树深搜）
function findPaneByPty(node: SplitNode, ptyId: number): PaneState | null {
  if (node.type === 'leaf') {
    return node.panes.find((p) => p.ptyId === ptyId) ?? null;
  }
  for (const child of node.children) {
    const found = findPaneByPty(child, ptyId);
    if (found) return found;
  }
  return null;
}

function updatePaneById(
  node: SplitNode,
  paneId: string,
  updater: (pane: PaneState) => PaneState,
): SplitNode {
  if (node.type === 'leaf') {
    const idx = node.panes.findIndex((p) => p.id === paneId);
    if (idx < 0) return node;
    const updatedPane = updater(node.panes[idx]);
    if (updatedPane === node.panes[idx]) return node;
    const newPanes = [...node.panes];
    newPanes[idx] = updatedPane;
    return { ...node, panes: newPanes };
  }

  let changed = false;
  const children = node.children.map((child) => {
    const updated = updatePaneById(child, paneId, updater);
    if (updated !== child) changed = true;
    return updated;
  });
  return changed ? { ...node, children } : node;
}

// 序列化 SplitNode 树（剥离运行时数据）
function serializeSplitNode(node: SplitNode): SavedSplitNode {
  if (node.type === 'leaf') {
    return { type: 'leaf', panes: node.panes.map((p) => ({ shellName: p.shellName })) };
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

export function restoreLayout(
  projectId: string,
  savedLayout: SavedProjectLayout,
  config: AppConfig,
): void {
  const restored = restoreSavedProjectLayout(projectId, savedLayout, config, genId);
  if (!restored) return;
  useAppStore.setState((state) => {
    const newStates = new Map(state.projectStates);
    newStates.set(projectId, restored);
    return { projectStates: newStates };
  });
}

// 每个项目的展开目录集合（运行时状态）
const expandedDirsMap = new Map<string, Set<string>>();

export function initExpandedDirs(projectId: string, dirs: string[]) {
  expandedDirsMap.set(projectId, new Set(dirs));
}

export function isExpanded(projectId: string, path: string): boolean {
  return expandedDirsMap.get(projectId)?.has(path) ?? false;
}

export function toggleExpandedDir(projectId: string, path: string, expanded: boolean) {
  let set = expandedDirsMap.get(projectId);
  if (!set) {
    set = new Set();
    expandedDirsMap.set(projectId, set);
  }
  if (expanded) {
    set.add(path);
  } else {
    set.delete(path);
  }
  saveExpandedDirsToConfig(projectId);
}

// 保存展开目录到配置（防抖）
const saveExpandedTimers = new Map<string, ReturnType<typeof setTimeout>>();

function applyExpandedDirsToStore(projectId: string) {
  const { config } = useAppStore.getState();
  const dirs = Array.from(expandedDirsMap.get(projectId) ?? []);
  const newConfig = {
    ...config,
    projects: config.projects.map((p) =>
      p.id === projectId ? { ...p, expandedDirs: dirs } : p
    ),
  };
  useAppStore.getState().setConfig(newConfig);
}

function doSaveExpandedDirs(projectId: string) {
  applyExpandedDirsToStore(projectId);
  invoke('save_config', { config: useAppStore.getState().config });
}

function saveExpandedDirsToConfig(projectId: string) {
  const existing = saveExpandedTimers.get(projectId);
  if (existing) clearTimeout(existing);
  saveExpandedTimers.set(projectId, setTimeout(() => {
    saveExpandedTimers.delete(projectId);
    doSaveExpandedDirs(projectId);
  }, 500));
}

export function flushExpandedDirsToConfig(projectId: string) {
  const existing = saveExpandedTimers.get(projectId);
  if (existing) {
    clearTimeout(existing);
    saveExpandedTimers.delete(projectId);
  }
  applyExpandedDirsToStore(projectId);
}

// 每个项目独立的防抖 timer
const saveLayoutTimers = new Map<string, ReturnType<typeof setTimeout>>();

function applyLayoutToStore(projectId: string) {
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
}

function doSaveLayout(projectId: string) {
  applyLayoutToStore(projectId);
  invoke('save_config', { config: useAppStore.getState().config });
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
  applyLayoutToStore(projectId);
}

/** 合并 flush：一次 setConfig 同时保存布局 + 展开目录 + lastActiveProjectId */
export function flushProjectToConfig(projectId: string) {
  // 取消所有待执行的防抖 timer
  const layoutTimer = saveLayoutTimers.get(projectId);
  if (layoutTimer) { clearTimeout(layoutTimer); saveLayoutTimers.delete(projectId); }
  const expandTimer = saveExpandedTimers.get(projectId);
  if (expandTimer) { clearTimeout(expandTimer); saveExpandedTimers.delete(projectId); }

  const { config, projectStates, activeProjectId } = useAppStore.getState();
  const ps = projectStates.get(projectId);
  const savedLayout = ps ? serializeLayout(ps) : undefined;
  const expandedDirs = Array.from(expandedDirsMap.get(projectId) ?? []);

  const newConfig = {
    ...config,
    lastActiveProjectId: activeProjectId ?? config.lastActiveProjectId,
    projects: config.projects.map((p) => {
      if (p.id !== projectId) return p;
      const updated = { ...p, expandedDirs };
      if (savedLayout) updated.savedLayout = savedLayout;
      return updated;
    }),
  };
  useAppStore.getState().setConfig(newConfig);
}

/** 将当前 store 中的 config 写入磁盘（返回 Promise） */
export function persistConfig() {
  return invoke('save_config', { config: useAppStore.getState().config });
}

function ensureTree(config: AppConfig): AppConfig {
  if (config.projectTree && config.projectTree.length > 0) return config;
  if (config.projectOrdering || config.projectGroups) {
    return { ...config, projectTree: migrateToTree(config), projectGroups: undefined, projectOrdering: undefined };
  }
  return { ...config, projectTree: config.projects.map((p) => p.id) };
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
  renameProject: (id: string, name: string) => void;

  // Tab
  addTab: (projectId: string, tab: TerminalTab) => void;
  removeTab: (projectId: string, tabId: string) => void;
  setActiveTab: (projectId: string, tabId: string) => void;
  updateTabLayout: (projectId: string, tabId: string, layout: SplitNode) => void;

  // Pane 状态
  updatePaneStatusByPty: (ptyId: number, status: PaneStatus) => void;
  setPanePty: (projectId: string, paneId: string, ptyId: number) => void;
  updatePaneStatusByPaneId: (projectId: string, paneId: string, status: PaneStatus) => void;

  // AI 任务分段 marker
  markersByPty: Map<number, AiMarker[]>;
  addMarker: (payload: AiUserSubmitPayload, xtermMarkerId: number) => string;
  clearMarkersForPty: (ptyId: number) => void;
  pruneDisposed: (ptyId: number, isDisposed: (xtermMarkerId: number) => boolean) => void;
  getMarkersForPty: (ptyId: number) => AiMarker[];

  // Notifications
  notifications: AiCompletionNotification[];
  pushNotification: (n: Omit<AiCompletionNotification, 'id' | 'timestamp'>) => void;
  dismissNotification: (id: string) => void;

  // 面板显隐
  togglePanel: (panel: 'projects' | 'sessions' | 'files' | 'git') => void;

  // 分组
  createGroup: (name: string, parentGroupId?: string) => void;
  removeGroup: (groupId: string) => void;
  renameGroup: (groupId: string, name: string) => void;
  toggleGroupCollapse: (groupId: string) => void;
  moveItem: (itemId: string, targetGroupId: string | null, index?: number) => void;

  // 搜索弹窗
  searchModalOpen: boolean;
  setSearchModalOpen: (open: boolean) => void;

  // cc-connect 状态(顶部状态点 + 设置页共享,由 App.tsx 拉起 5s 轮询)
  ccConnectStatus: CcConnectStatus | null;
  setCcConnectStatus: (status: CcConnectStatus | null) => void;

  // cc-connect Dashboard modal(单例:App.tsx 唯一挂载,ProjectList 等通过 action 打开)
  ccDashboardOpen: boolean;
  ccDashboardDeepLink: string;
  openCcDashboard: (deepLink?: string) => void;
  closeCcDashboard: () => void;
}

export const useAppStore = create<AppStore>((set, get) => ({
  config: {
    projects: [],
    defaultShell: '',
    availableShells: [],
    uiFontSize: 13,
    terminalFontSize: 14,
    terminalLigatures: false,
    theme: 'auto',
    skin: 'none',
    terminalFollowTheme: true,
    aiCompletionPopup: true,
    aiCompletionTaskbarFlash: true,
    aiCompletionSound: true,
    editors: [],
    gitChangesViewMode: 'list',
    longPasteToFile: true,
    longPasteLineThreshold: 10,
    longPasteCharThreshold: 2000,
    projectsVisible: true,
    sessionsVisible: true,
    filesVisible: true,
    gitVisible: true,
    hookEnabled: false,
    smartCopyPaste: false,
    sshConnections: [],
  },
  setConfig: (config) => set({ config }),

  activeProjectId: null,
  projectStates: new Map(),
  notifications: [],
  markersByPty: new Map(),
  searchModalOpen: false,
  setSearchModalOpen: (open) => set({ searchModalOpen: open }),

  ccConnectStatus: null,
  setCcConnectStatus: (status) => set({ ccConnectStatus: status }),

  ccDashboardOpen: false,
  ccDashboardDeepLink: '',
  openCcDashboard: (deepLink) => set({ ccDashboardOpen: true, ccDashboardDeepLink: deepLink ?? '' }),
  closeCcDashboard: () => set({ ccDashboardOpen: false }),

  setActiveProject: (id) =>
    set((state) => {
      const newStates = new Map(state.projectStates);
      const ps = newStates.get(id);
      if (ps?.needsAttention) {
        newStates.set(id, { ...ps, needsAttention: false });
      }
      return { activeProjectId: id, projectStates: newStates };
    }),

  addProject: (project) =>
    set((state) => {
      const config = ensureTree(state.config);
      const newTree = [...(config.projectTree ?? []), project.id];
      const newConfig = {
        ...config,
        projects: [...config.projects, project],
        projectTree: newTree,
      };
      const newStates = new Map(state.projectStates);
      newStates.set(project.id, { id: project.id, tabs: [], activeTabId: '' });
      return {
        config: newConfig,
        projectStates: newStates,
        activeProjectId: state.activeProjectId ?? project.id,
      };
    }),

  removeProject: (id) => {
    set((state) => {
      // 非纯状态副作用:清理运行时 Map / timer(不参与 zustand 状态)
      const removingProject = state.config.projects.find((p) => p.id === id);
      if (removingProject) {
        clearProjectCache(removingProject.path);
      }
      expandedDirsMap.delete(id);
      const timer = saveExpandedTimers.get(id);
      if (timer) { clearTimeout(timer); saveExpandedTimers.delete(id); }

      // 合并清理该项目下所有 pane 的 AI markers,防止内存泄漏
      const removingPs = state.projectStates.get(id);
      let newMarkers = state.markersByPty;
      if (removingPs) {
        const ptyIds: number[] = [];
        for (const tab of removingPs.tabs) {
          ptyIds.push(...collectPtyIds(tab.splitLayout));
        }
        if (ptyIds.some((pid) => newMarkers.has(pid))) {
          newMarkers = new Map(newMarkers);
          for (const pid of ptyIds) newMarkers.delete(pid);
        }
      }

      const newTree = deepCloneTree(state.config.projectTree ?? []);
      removeProjectFromTree(newTree, id);
      const newConfig = {
        ...state.config,
        projects: state.config.projects.filter((p) => p.id !== id),
        projectTree: newTree,
      };
      const newStates = new Map(state.projectStates);
      newStates.delete(id);
      const newActive =
        state.activeProjectId === id
          ? newConfig.projects[0]?.id ?? null
          : state.activeProjectId;
      if (newConfig.lastActiveProjectId === id) {
        newConfig.lastActiveProjectId = newActive ?? undefined;
      }
      return {
        config: newConfig,
        projectStates: newStates,
        activeProjectId: newActive,
        notifications: state.notifications.filter((n) => n.projectId !== id),
        markersByPty: newMarkers,
      };
    });
  },

  renameProject: (id, name) =>
    set((state) => ({
      config: {
        ...state.config,
        projects: state.config.projects.map((p) =>
          p.id === id ? { ...p, name } : p
        ),
      },
    })),

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
      const ps = state.projectStates.get(projectId);
      if (!ps) return state;
      const closingTab = ps.tabs.find((t) => t.id === tabId);
      if (!closingTab) return state;

      // 合并清理该 tab 下所有 pane 的 AI markers,避免多次 set 触发的中间态
      const ptyIds = collectPtyIds(closingTab.splitLayout);
      let newMarkers = state.markersByPty;
      if (ptyIds.some((id) => newMarkers.has(id))) {
        newMarkers = new Map(newMarkers);
        for (const id of ptyIds) newMarkers.delete(id);
      }

      const newStates = new Map(state.projectStates);
      const newTabs = ps.tabs.filter((t) => t.id !== tabId);
      const newActive =
        ps.activeTabId === tabId ? (newTabs[newTabs.length - 1]?.id ?? '') : ps.activeTabId;
      newStates.set(projectId, { ...ps, tabs: newTabs, activeTabId: newActive });
      return { projectStates: newStates, markersByPty: newMarkers };
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
      // 1. 找到 pane 所属项目并捕获 oldStatus
      let oldStatus: PaneStatus | null = null;
      let owningProjectId: string | null = null;
      for (const [pid, ps] of state.projectStates) {
        for (const tab of ps.tabs) {
          const found = findPaneByPty(tab.splitLayout, ptyId);
          if (found) {
            oldStatus = found.status;
            owningProjectId = pid;
            break;
          }
        }
        if (owningProjectId) break;
      }
      if (!owningProjectId || oldStatus === null) return state;

      // 2. 更新各项目 tabs 中匹配 ptyId 的 pane status
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
      if (!changed) return state;

      // 3. 检测 transition：ai-working → ai-idle
      const isCompletion = oldStatus === 'ai-working' && status === 'ai-idle';
      if (isCompletion) {
        // 3a. 提示音 — 不区分激活项目
        if (state.config.aiCompletionSound) {
          queueMicrotask(() => {
            playNotificationSound(state.config.aiCompletionSoundPath);
          });
        }

        // 3b. 任务栏闪烁 — 不区分激活项目（Tauri API 自带 focus 检测）
        if (state.config.aiCompletionTaskbarFlash) {
          queueMicrotask(() => {
            getCurrentWindow()
              .requestUserAttention(UserAttentionType.Informational)
              .catch(() => {});
          });
        }

        // 3c. Tag + Toast — 仅非激活项目
        if (owningProjectId !== state.activeProjectId) {
          const ps = newStates.get(owningProjectId);
          if (ps && !ps.needsAttention) {
            // 设置 needsAttention（防重：已为 true 时不重复）
            newStates.set(owningProjectId, { ...ps, needsAttention: true });

            // 推 toast（同项目当前没有未消失的 toast 才推）
            if (state.config.aiCompletionPopup) {
              const project = state.config.projects.find((p) => p.id === owningProjectId);
              const hasExisting = state.notifications.some(
                (n) => n.projectId === owningProjectId
              );
              if (project && !hasExisting) {
                const projectName = project.name;
                const targetPid = owningProjectId;
                queueMicrotask(() =>
                  useAppStore.getState().pushNotification({
                    projectId: targetPid,
                    projectName,
                  })
                );
              }
            }
          }
        }
      }

      return { projectStates: newStates };
    }),

  setPanePty: (projectId, paneId, ptyId) =>
    set((state) => {
      const ps = state.projectStates.get(projectId);
      if (!ps) return state;

      let changed = false;
      const tabs = ps.tabs.map((tab) => {
        const splitLayout = updatePaneById(tab.splitLayout, paneId, (pane) => {
          if (pane.ptyId !== undefined) return pane;
          return { ...pane, ptyId, status: 'idle' };
        });
        if (splitLayout === tab.splitLayout) return tab;
        changed = true;
        return { ...tab, splitLayout, status: getHighestStatus(splitLayout) };
      });
      if (!changed) return state;

      const newStates = new Map(state.projectStates);
      newStates.set(projectId, { ...ps, tabs });
      return { projectStates: newStates };
    }),

  updatePaneStatusByPaneId: (projectId, paneId, status) =>
    set((state) => {
      const ps = state.projectStates.get(projectId);
      if (!ps) return state;

      let changed = false;
      const tabs = ps.tabs.map((tab) => {
        const splitLayout = updatePaneById(tab.splitLayout, paneId, (pane) => (
          pane.status === status ? pane : { ...pane, status }
        ));
        if (splitLayout === tab.splitLayout) return tab;
        changed = true;
        return { ...tab, splitLayout, status: getHighestStatus(splitLayout) };
      });
      if (!changed) return state;

      const newStates = new Map(state.projectStates);
      newStates.set(projectId, { ...ps, tabs });
      return { projectStates: newStates };
    }),

  addMarker: (payload, xtermMarkerId) => {
    const id = crypto.randomUUID();
    set((state) => {
      const next = new Map(state.markersByPty);
      const existing = next.get(payload.ptyId) ?? [];
      const updated = existing.map((m, idx) =>
        idx === existing.length - 1 ? { ...m, inProgress: false } : m
      );
      const marker: AiMarker = {
        id,
        seq: updated.length + 1,
        ptyId: payload.ptyId,
        line: payload.line,
        ts: payload.ts,
        xtermMarkerId,
        inProgress: true,
      };
      next.set(payload.ptyId, [...updated, marker]);
      return { markersByPty: next };
    });
    return id;
  },

  clearMarkersForPty: (ptyId) =>
    set((state) => {
      if (!state.markersByPty.has(ptyId)) return state;
      const next = new Map(state.markersByPty);
      next.delete(ptyId);
      return { markersByPty: next };
    }),

  pruneDisposed: (ptyId, isDisposed) =>
    set((state) => {
      const list = state.markersByPty.get(ptyId);
      if (!list || list.length === 0) return state;
      const filtered = list.filter((m) => !isDisposed(m.xtermMarkerId));
      if (filtered.length === list.length) return state;
      const next = new Map(state.markersByPty);
      if (filtered.length === 0) next.delete(ptyId);
      else next.set(ptyId, filtered);
      return { markersByPty: next };
    }),

  getMarkersForPty: (ptyId) => get().markersByPty.get(ptyId) ?? [],

  pushNotification: (n) => {
    const id = genId();
    set((state) => ({
      notifications: [
        ...state.notifications,
        { ...n, id, timestamp: Date.now() },
      ],
    }));
    // 5s 自动消失：在 store 内部管理定时器，避免组件 useEffect 重置问题
    setTimeout(() => {
      useAppStore.getState().dismissNotification(id);
    }, 5000);
  },

  dismissNotification: (id) =>
    set((state) => ({
      notifications: state.notifications.filter((x) => x.id !== id),
    })),

  togglePanel: (panel) =>
    set((state) => {
      const visibleKeys = {
        projects: 'projectsVisible',
        sessions: 'sessionsVisible',
        files: 'filesVisible',
        git: 'gitVisible',
      } as const;
      const key = visibleKeys[panel];
      const newConfig = { ...state.config, [key]: !state.config[key] };
      invoke('save_config', { config: newConfig }).catch(() => {});
      return { config: newConfig };
    }),

  createGroup: (name, parentGroupId) =>
    set((state) => {
      const config = ensureTree(state.config);
      const group: ProjectGroup = { id: genId(), name, collapsed: false, children: [] };
      const newTree = deepCloneTree(config.projectTree ?? []);
      insertIntoTree(newTree, parentGroupId ?? null, group);
      return { config: { ...config, projectTree: newTree } };
    }),

  removeGroup: (groupId) =>
    set((state) => {
      const newTree = deepCloneTree(state.config.projectTree ?? []);
      removeGroupAndPromoteChildren(newTree, groupId);
      return { config: { ...state.config, projectTree: newTree } };
    }),

  renameGroup: (groupId, name) =>
    set((state) => {
      const newTree = deepCloneTree(state.config.projectTree ?? []);
      updateGroupInTree(newTree, groupId, (g) => ({ ...g, name }));
      return { config: { ...state.config, projectTree: newTree } };
    }),

  toggleGroupCollapse: (groupId) =>
    set((state) => {
      const newTree = deepCloneTree(state.config.projectTree ?? []);
      updateGroupInTree(newTree, groupId, (g) => ({ ...g, collapsed: !g.collapsed }));
      return { config: { ...state.config, projectTree: newTree } };
    }),

  moveItem: (itemId, targetGroupId, index) =>
    set((state) => {
      const config = ensureTree(state.config);
      const newTree = deepCloneTree(config.projectTree ?? []);
      const removed = removeFromTree(newTree, itemId);
      if (!removed) return state;
      insertIntoTree(newTree, targetGroupId, removed, index);
      return { config: { ...config, projectTree: newTree } };
    }),

}));
