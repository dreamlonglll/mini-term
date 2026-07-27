import { create } from 'zustand';
import { invoke } from '@tauri-apps/api/core';
import { getCurrentWindow, UserAttentionType } from '@tauri-apps/api/window';
import type {
  AppConfig,
  ProjectConfig,
  ProjectGroup,
  ProjectState,
  SplitNode,
  PaneState,
  PaneStatus,
  SavedSplitNode,
  SavedProjectLayout,
  AiCompletionNotification,
  AiMarker,
  AiUserSubmitPayload,
  MobileRelayStatusPayload,
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
  replaceProjectInTree,
  migrateToTree,
} from './utils/projectTree';
import { clearProjectCache, projectCacheKey } from './utils/projectDataCache';

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

/**
 * 查 ptyId 归属的项目 id 与 pane（跨全部项目布局深搜）；找不到返回 null。
 *
 * 粘贴链路要按「pane 属于本地 / WSL / SSH 远程项目」分流，而 xterm 的 key
 * handler 手上只有 ptyId —— 这里补上那一跳。
 */
export function findPaneContextByPty(
  ptyId: number,
): { projectId: string; pane: PaneState } | null {
  for (const [projectId, ps] of useAppStore.getState().projectStates) {
    if (!ps.layout) continue;
    const pane = findPaneByPty(ps.layout, ptyId);
    if (pane) return { projectId, pane };
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

/**
 * 对某个项目布局里的单个 pane 做原地更新，并同步项目级聚合状态。
 * updater 返回同一引用即视为无变化，整个 set 短路（不触发订阅）。
 */
function updateProjectPane(
  state: { projectStates: Map<string, ProjectState> },
  projectId: string,
  paneId: string,
  updater: (pane: PaneState) => PaneState,
): Partial<{ projectStates: Map<string, ProjectState> }> {
  const ps = state.projectStates.get(projectId);
  if (!ps?.layout) return state;
  const layout = updatePaneById(ps.layout, paneId, updater);
  if (layout === ps.layout) return state;
  const newStates = new Map(state.projectStates);
  newStates.set(projectId, { ...ps, layout, status: getHighestStatus(layout) });
  return { projectStates: newStates };
}

// 序列化 SplitNode 树（剥离运行时数据）
function serializeSplitNode(node: SplitNode): SavedSplitNode {
  if (node.type === 'leaf') {
    return { type: 'leaf', panes: node.panes.map((p) => ({ shellName: p.shellName, cwd: p.cwd })) };
  }
  return {
    type: 'split',
    direction: node.direction,
    children: node.children.map(serializeSplitNode),
    sizes: [...node.sizes],
  };
}

/**
 * 运行时布局 → 磁盘格式。`tabs` 恒为 0 或 1 个元素:项目级 tab 层已删除,
 * 数组只是为了兼容 Rust 端 SavedProjectLayout 与旧 config.json（见 types.ts）。
 */
export function serializeLayout(ps: ProjectState): SavedProjectLayout {
  if (!ps.layout) return { tabs: [], activeTabIndex: 0 };
  return {
    tabs: [{ splitLayout: serializeSplitNode(ps.layout) }],
    activeTabIndex: 0,
  };
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

// Toast 自动消失定时器（按 id）。悬停暂停 = 清掉定时器，移开 = 重新计时满 5s。
const NOTIFICATION_TTL_MS = 5000;
const notificationTimers = new Map<string, ReturnType<typeof setTimeout>>();

function clearNotificationTimer(id: string) {
  const timer = notificationTimers.get(id);
  if (timer) {
    clearTimeout(timer);
    notificationTimers.delete(id);
  }
}

function armNotificationTimer(id: string) {
  clearNotificationTimer(id);
  notificationTimers.set(id, setTimeout(() => {
    notificationTimers.delete(id);
    useAppStore.getState().dismissNotification(id);
  }, NOTIFICATION_TTL_MS));
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
  /** 传 parentProjectId = 作为其子项目挂载(worktree「设为项目」),不进 projectTree */
  addProject: (project: ProjectConfig, parentProjectId?: string) => void;
  removeProject: (id: string) => void;
  renameProject: (id: string, name: string) => void;

  // 终端布局
  /** 写入项目的终端布局树；`null` = 清空（最后一个 pane 被关掉）。
   *  布局里消失的 pane，其 AI markers 一并回收。 */
  setProjectLayout: (projectId: string, layout: SplitNode | null) => void;

  // Pane 状态
  updatePaneStatusByPty: (ptyId: number, status: PaneStatus) => void;
  setPanePty: (projectId: string, paneId: string, ptyId: number) => void;
  updatePaneStatusByPaneId: (projectId: string, paneId: string, status: PaneStatus) => void;
  /** 移动端改会话名:按 paneId 全局定位;空串 = 清除自定义名,回落 shell 名 */
  renamePaneById: (paneId: string, title: string) => void;

  // 已退出的 PTY 集合（pty-exit 事件登记）。远程 pane 据此显示「连接已断开,点击重连」
  // 覆盖层（远程 ssh 进程退出后 pane 不自动关闭,用户主动 exit 与异常断线不做区分）。
  exitedPtyIds: Set<number>;
  markPtyExited: (ptyId: number) => void;
  clearPtyExited: (ptyId: number) => void;
  /** 重连:清掉 pane 的 ptyId 并复位状态,PaneGroup 的懒创建 effect 会重新 create_pty */
  resetPaneForReconnect: (projectId: string, paneId: string) => void;

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
  /** 鼠标悬停在 toast 上时暂停自动消失，移开后重新计时 */
  pauseNotification: (id: string, paused: boolean) => void;

  // 面板显隐
  /** 折叠/展开中间栏（Projects + Files），持久化到 config */
  toggleMiddleColumn: () => void;

  // 右侧悬浮抽屉（Sessions / Git）——运行时态,互斥单抽屉,不持久化开合(每次启动收起)
  rightDrawer: 'sessions' | 'git' | null;
  toggleRightDrawer: (panel: 'sessions' | 'git') => void;
  /** 直接切到某个面板（抽屉内的 segmented 切换用，不做「再点一次关闭」） */
  openRightDrawer: (panel: 'sessions' | 'git') => void;
  closeRightDrawer: () => void;

  // 分组
  createGroup: (name: string, parentGroupId?: string) => void;
  removeGroup: (groupId: string) => void;
  renameGroup: (groupId: string, name: string) => void;
  toggleGroupCollapse: (groupId: string) => void;
  moveItem: (itemId: string, targetGroupId: string | null, index?: number) => void;

  // 搜索弹窗
  searchModalOpen: boolean;
  setSearchModalOpen: (open: boolean) => void;

  // 移动端中转连接状态(后端 mobile-relay-status 事件驱动,设置页「移动端」区域展示)
  mobileRelayStatus: MobileRelayStatusPayload | null;
  setMobileRelayStatus: (status: MobileRelayStatusPayload | null) => void;
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
    remotePasteDir: '.mini-term/pasted',
    middleColumnVisible: true,
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

  rightDrawer: null,

  mobileRelayStatus: null,
  setMobileRelayStatus: (status) => set({ mobileRelayStatus: status }),

  setActiveProject: (id) =>
    set((state) => {
      const newStates = new Map(state.projectStates);
      const ps = newStates.get(id);
      if (ps?.needsAttention) {
        newStates.set(id, { ...ps, needsAttention: false });
      }
      return { activeProjectId: id, projectStates: newStates };
    }),

  addProject: (project, parentProjectId) =>
    set((state) => {
      const config = ensureTree(state.config);
      // 父项目必须真实存在,否则回落为普通顶层项目(防止产生渲染不出来的孤儿)
      const parentOk = !!parentProjectId
        && config.projects.some((p) => p.id === parentProjectId);
      const newProject = parentOk ? { ...project, parentProjectId } : project;
      const newTree = parentOk
        ? (config.projectTree ?? [])
        : [...(config.projectTree ?? []), project.id];
      const newConfig = {
        ...config,
        projects: [...config.projects, newProject],
        projectTree: newTree,
      };
      const newStates = new Map(state.projectStates);
      newStates.set(project.id, { id: project.id, layout: null, status: 'idle' });
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
        // key 口径与 FileTree 一致:远程项目掺连接 id(见 projectCacheKey)
        clearProjectCache(projectCacheKey(removingProject));
      }
      expandedDirsMap.delete(id);
      const timer = saveExpandedTimers.get(id);
      if (timer) { clearTimeout(timer); saveExpandedTimers.delete(id); }

      // 合并清理该项目下所有 pane 的 AI markers,防止内存泄漏
      const removingPs = state.projectStates.get(id);
      let newMarkers = state.markersByPty;
      if (removingPs?.layout) {
        const ptyIds = collectPtyIds(removingPs.layout);
        if (ptyIds.some((pid) => newMarkers.has(pid))) {
          newMarkers = new Map(newMarkers);
          for (const pid of ptyIds) newMarkers.delete(pid);
        }
      }

      // 子项目晋升:被删项目若有 worktree 子项目,子项顶替它的树位置成为顶层节点;
      // 被删项目自己也是子项目时,子项改挂到它的父项目上(不进树)。
      const childIds = state.config.projects
        .filter((p) => p.parentProjectId === id)
        .map((p) => p.id);
      const inheritedParent = removingProject?.parentProjectId;

      const newTree = deepCloneTree(state.config.projectTree ?? []);
      if (inheritedParent) {
        removeProjectFromTree(newTree, id); // 子项目本不在树里,通常为 no-op
      } else if (!replaceProjectInTree(newTree, id, childIds)) {
        newTree.push(...childIds); // 被删项目不在树中(异常配置),子项兜底进根层
      }
      const newConfig = {
        ...state.config,
        projects: state.config.projects
          .filter((p) => p.id !== id)
          .map((p) =>
            p.parentProjectId === id ? { ...p, parentProjectId: inheritedParent } : p,
          ),
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

  setProjectLayout: (projectId, layout) =>
    set((state) => {
      const ps = state.projectStates.get(projectId);
      if (!ps) return state;
      if (ps.layout === layout) return state;

      // 从布局里消失的 pane 一并回收其 AI markers。这条路径同时覆盖「关一个 pane」
      // 与「关掉整个项目的终端」,不必再让各调用方自己记得清。
      let newMarkers = state.markersByPty;
      const before = ps.layout ? collectPtyIds(ps.layout) : [];
      if (before.length > 0) {
        const after = new Set(layout ? collectPtyIds(layout) : []);
        const gone = before.filter((id) => !after.has(id) && newMarkers.has(id));
        if (gone.length > 0) {
          newMarkers = new Map(newMarkers);
          for (const id of gone) newMarkers.delete(id);
        }
      }

      const newStates = new Map(state.projectStates);
      newStates.set(projectId, {
        ...ps,
        layout,
        status: layout ? getHighestStatus(layout) : 'idle',
      });
      return { projectStates: newStates, markersByPty: newMarkers };
    }),

  updatePaneStatusByPty: (ptyId, status) =>
    set((state) => {
      // 1. 找到 pane 所属项目并捕获 oldStatus
      let oldStatus: PaneStatus | null = null;
      let owningProjectId: string | null = null;
      for (const [pid, ps] of state.projectStates) {
        if (!ps.layout) continue;
        const found = findPaneByPty(ps.layout, ptyId);
        if (found) {
          oldStatus = found.status;
          owningProjectId = pid;
          break;
        }
      }
      if (!owningProjectId || oldStatus === null) return state;

      // 2. 更新各项目布局中匹配 ptyId 的 pane status
      const newStates = new Map(state.projectStates);
      let changed = false;
      for (const [pid, ps] of newStates) {
        if (!ps.layout) continue;
        const newLayout = updatePaneStatus(ps.layout, ptyId, status);
        if (newLayout === ps.layout) continue;
        newStates.set(pid, { ...ps, layout: newLayout, status: getHighestStatus(newLayout) });
        changed = true;
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
    set((state) => updateProjectPane(state, projectId, paneId, (pane) => (
      pane.ptyId !== undefined ? pane : { ...pane, ptyId, status: 'idle' }
    ))),

  exitedPtyIds: new Set<number>(),

  markPtyExited: (ptyId) =>
    set((state) => {
      // 只登记仍被某个 pane 持有的 ptyId,并顺手清掉已不属于任何 pane 的旧登记:
      // - pane 关闭后才到达的 pty-exit(kill_pty 触发)不登记,防 Set 无界增长;
      // - 重连后旧 pty 的迟到 pty-exit 也因 pane 已换新 ptyId 而被拒,消除竞态残留。
      const live = new Set<number>();
      state.projectStates.forEach((ps) => {
        if (!ps.layout) return;
        for (const id of collectPtyIds(ps.layout)) live.add(id);
      });
      const next = new Set<number>();
      state.exitedPtyIds.forEach((id) => {
        if (live.has(id)) next.add(id);
      });
      if (live.has(ptyId)) next.add(ptyId);
      // 集合内容未变则不触发订阅更新
      if (next.size === state.exitedPtyIds.size) {
        let same = true;
        next.forEach((id) => {
          if (!state.exitedPtyIds.has(id)) same = false;
        });
        if (same) return state;
      }
      return { exitedPtyIds: next };
    }),

  clearPtyExited: (ptyId) =>
    set((state) => {
      if (!state.exitedPtyIds.has(ptyId)) return state;
      const next = new Set(state.exitedPtyIds);
      next.delete(ptyId);
      return { exitedPtyIds: next };
    }),

  resetPaneForReconnect: (projectId, paneId) =>
    set((state) => updateProjectPane(state, projectId, paneId, (pane) => (
      pane.ptyId === undefined && pane.status === 'idle'
        ? pane
        : { ...pane, ptyId: undefined, status: 'idle' }
    ))),

  updatePaneStatusByPaneId: (projectId, paneId, status) =>
    set((state) => updateProjectPane(state, projectId, paneId, (pane) => (
      pane.status === status ? pane : { ...pane, status }
    ))),

  // 移动端改会话名:按 paneId 全局找（移动端只认得 pane，不知道它挂在哪个项目下）。
  // pane 级 customTitle 不进 savedLayout，所以不落配置——AI 会话本来就活不过重启。
  renamePaneById: (paneId, title) =>
    set((state) => {
      const nextTitle = title || undefined; // 空串 = 清掉自定义名，回落 shell 名
      const newStates = new Map(state.projectStates);
      for (const [pid, ps] of newStates) {
        if (!ps.layout) continue;
        const layout = updatePaneById(ps.layout, paneId, (pane) =>
          pane.customTitle === nextTitle ? pane : { ...pane, customTitle: nextTitle }
        );
        if (layout === ps.layout) continue;
        newStates.set(pid, { ...ps, layout });
        return { projectStates: newStates }; // paneId 全局唯一，命中即收工
      }
      return state;
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
    armNotificationTimer(id);
  },

  dismissNotification: (id) => {
    clearNotificationTimer(id);
    set((state) => ({
      notifications: state.notifications.filter((x) => x.id !== id),
    }));
  },

  // 悬停暂停：鼠标压在 toast 上时它不该在指针底下消失（点「跳转到项目」经常来不及）
  pauseNotification: (id, paused) => {
    if (paused) clearNotificationTimer(id);
    else if (get().notifications.some((n) => n.id === id)) armNotificationTimer(id);
  },

  toggleMiddleColumn: () =>
    set((state) => {
      const newConfig = { ...state.config, middleColumnVisible: !state.config.middleColumnVisible };
      invoke('save_config', { config: newConfig }).catch(() => {});
      return { config: newConfig };
    }),

  toggleRightDrawer: (panel) =>
    set((state) => ({ rightDrawer: state.rightDrawer === panel ? null : panel })),

  openRightDrawer: (panel) => set({ rightDrawer: panel }),

  closeRightDrawer: () => set({ rightDrawer: null }),

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
      let removed = removeFromTree(newTree, itemId);
      let newProjects = config.projects;
      if (!removed) {
        // 子项目(worktree)不在树里:移动即脱离父项目,清掉 parentProjectId 转普通树节点
        const child = config.projects.find((p) => p.id === itemId && p.parentProjectId);
        if (!child) return state;
        removed = itemId;
        newProjects = config.projects.map((p) =>
          p.id === itemId ? { ...p, parentProjectId: undefined } : p,
        );
      }
      insertIntoTree(newTree, targetGroupId, removed, index);
      return { config: { ...config, projects: newProjects, projectTree: newTree } };
    }),

}));
