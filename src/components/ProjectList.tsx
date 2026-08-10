import { useCallback, useState, useRef, useEffect } from 'react';
import { createPortal } from 'react-dom';
import { open } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import { revealItemInDir } from '@tauri-apps/plugin-opener';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { useAppStore, genId, saveConfigToDisk } from '../store';
import { removeProjectWithCleanup } from '../utils/projectActions';
import { collectWorktreeProbePaths, findStaleWorktreeProjects } from '../utils/worktreeReconcile';
import { StatusDot } from './StatusDot';
import { DoneTag } from './DoneTag';
import { SshAssocModal } from './SshAssocModal';
import { ProjectEnvVarsModal } from './ProjectEnvVarsModal';
import { AddRemoteProjectModal } from './AddRemoteProjectModal';
import { GitWorktreeModal } from './GitWorktreeModal';
import { Modal } from './Modal';
import { connectionSummary } from './SshModal';
import { showContextMenu } from '../utils/contextMenu';
import type { MenuEntry } from '../utils/contextMenu';
import { showConfirm, showPrompt } from '../utils/prompt';
import { initProjectDrag, isProjectDragging, getProjectDragPayload, onProjectDragEnd } from '../utils/projectDragState';
import { isWslPath } from '../utils/wslPath';
import { useProjectKinds } from '../hooks/useProjectKinds';
import { PROJECT_KINDS, PROJECT_KIND_LABELS } from '../utils/projectKind';
import type { ProjectKind } from '../utils/projectKind';
import { TechIcon } from './TechIcon';
import { BrandIcon } from './BrandIcon';
import { ProjectPanePreview } from './ProjectPanePreview';
import { inferVendor } from '../utils/inferVendor';
import type { AiVendor } from '../utils/inferVendor';
import { paneShowsAiSession } from '../utils/aiResume';
import { Boxes, Package, Server } from './icons';
import { useT } from '../i18n';
import {
  getOrderedTree,
  countProjectsInGroup,
  canDrop,
  canDropAt,
  getDepth,
  isGroup,
  findParentGroupId,
  findGroupInTree,
  MAX_DEPTH,
} from '../utils/projectTree';
import type { PaneStatus, PaneState, SplitNode, ProjectConfig, ProjectGroup, ProjectTreeItem, WslDistro } from '../types';

// 保存配置的快捷方法
function saveConfig() {
  const config = useAppStore.getState().config;
  saveConfigToDisk(config);
}

// 模块级缓存:WSL 发行版列表只 invoke 一次。
// 右键菜单是同步构建的,组件挂载时预取,打开菜单时直接读缓存。
// 非 Windows / 未装 WSL 时列表为空,「WSL 会话」菜单项自然隐藏。
let wslDistrosCache: WslDistro[] | null = null;
let wslDistrosPromise: Promise<void> | null = null;
function prefetchWslDistros() {
  if (wslDistrosCache || wslDistrosPromise) return;
  wslDistrosPromise = invoke<WslDistro[]>('list_wsl_distros')
    .then((list) => { wslDistrosCache = list; })
    .catch(() => { wslDistrosCache = []; });
}

type TFunc = (key: string, params?: Record<string, string | number>) => string;

/**
 * 「移动到分组」的树形子菜单:按 projectTree 的层级逐级展开,而不是把所有分组拍平成
 * 一长串「移动到「X」」。
 *
 * 含子组的组既是落点又是入口 —— 子菜单父项本身不可点(contextMenu 的约定),
 * 所以把「移动到此处」放在它子菜单的第一项,后面才是子组。
 * 项目当前所在的组标 ✓ 并置灰(移到原地是空操作)。
 */
function buildMoveToGroupMenu(
  items: ProjectTreeItem[],
  depth: number,
  currentParentId: string | undefined,
  onPick: (groupId: string) => void,
  t: TFunc,
): MenuEntry[] {
  const entries: MenuEntry[] = [];
  for (const item of items) {
    if (!isGroup(item)) continue;
    const isCurrent = item.id === currentParentId;
    // 项目落进该组后就到了 depth+1 层,超限则该组不可选(其子组更深,同样不可选)
    const selectable = !isCurrent && depth + 1 <= MAX_DEPTH;
    // 前缀留一个全角空格,让有无 ✓ 的行文字左对齐(与「WSL 会话」子菜单同一写法)
    const label = `${isCurrent ? '✓ ' : '　'}${item.name}`;
    const children = buildMoveToGroupMenu(item.children, depth + 1, currentParentId, onPick, t);
    if (children.length > 0) {
      entries.push({
        label,
        submenu: [
          {
            label: t('projectList.menu.moveToThisGroup'),
            disabled: !selectable,
            onClick: () => onPick(item.id),
          },
          { separator: true },
          ...children,
        ],
      });
    } else {
      entries.push({ label, disabled: !selectable, onClick: () => onPick(item.id) });
    }
  }
  return entries;
}

// Drop 指示器位置
interface DropIndicator {
  id: string;
  position: 'before' | 'after' | 'inside';
  forbidden?: boolean;
}

/** 收集布局树里与 AI 会话相关的 pane;判定与 pane 标签页的品牌图标共用
 *  paneShowsAiSession(含重启后待续接的 pane —— 重启不该丢图标)。 */
function collectAiPanes(
  node: SplitNode | null | undefined,
  autoResumeEnabled: boolean,
): PaneState[] {
  if (!node) return [];
  if (node.type === 'leaf') {
    return node.panes.filter((p) => paneShowsAiSession(p, autoResumeEnabled));
  }
  return node.children.flatMap((c) => collectAiPanes(c, autoResumeEnabled));
}

/** 项目里有没有 AI 会话 pane —— pane 预览浮层的开闸条件。
 *
 *  与 collectAiPanes 同一把尺子(短路版,不构造数组):判定口径与项目行的 AI
 *  品牌图标一致,于是「行上亮着图标 → 悬停才有预览」,一眼可预期。没跑 AI 的
 *  项目悬停只出原生 title(绝对路径),不弹整张大卡打断视线。 */
function hasAiPane(node: SplitNode | null | undefined, autoResumeEnabled: boolean): boolean {
  if (!node) return false;
  if (node.type === 'leaf') return node.panes.some((p) => paneShowsAiSession(p, autoResumeEnabled));
  return node.children.some((c) => hasAiPane(c, autoResumeEnabled));
}

/** AI 品牌图标尺寸(px);图标间与领位图标后均留 2px 小间距,并排不重叠。 */
const AI_ICON_SIZE = 14;

export function ProjectList() {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const activeProjectId = useAppStore((s) => s.activeProjectId);
  const projectStates = useAppStore((s) => s.projectStates);
  const setActiveProject = useAppStore((s) => s.setActiveProject);
  const addProject = useAppStore((s) => s.addProject);
  const createGroup = useAppStore((s) => s.createGroup);
  const removeGroup = useAppStore((s) => s.removeGroup);
  const renameGroup = useAppStore((s) => s.renameGroup);
  const toggleGroupCollapse = useAppStore((s) => s.toggleGroupCollapse);
  const moveItem = useAppStore((s) => s.moveItem);

  const [confirmTarget, setConfirmTarget] = useState<{ id: string; name: string } | null>(null);
  const [sshAssocTarget, setSshAssocTarget] = useState<ProjectConfig | null>(null);
  const [envVarsTarget, setEnvVarsTarget] = useState<ProjectConfig | null>(null);
  // null = 关闭；{ groupId } = 打开（groupId 为空表示加到根层）
  const [addRemoteTarget, setAddRemoteTarget] = useState<{ groupId?: string } | null>(null);
  // Worktree 管理弹窗:记录右键项目的路径与 id(「开终端」要落在该项目里)
  const [worktreeTarget, setWorktreeTarget] = useState<{ path: string; projectId: string } | null>(null);
  const [dropIndicator, setDropIndicator] = useState<DropIndicator | null>(null);
  const [editingGroupId, setEditingGroupId] = useState<string | null>(null);
  const [editingProjectId, setEditingProjectId] = useState<string | null>(null);
  const [editingName, setEditingName] = useState('');
  const [isFileDragOver, setIsFileDragOver] = useState(false);
  const [fileDragKind, setFileDragKind] = useState<'valid' | 'forbidden' | 'duplicate' | null>(null);
  const editInputRef = useRef<HTMLInputElement>(null);
  const editProjectInputRef = useRef<HTMLInputElement>(null);
  const projectListRef = useRef<HTMLDivElement>(null);

  const orderedItems = getOrderedTree(config);

  // 项目类型徽标:本地项目自动探测,远程项目由 FileTree 首次展开时喂入
  const projectKinds = useProjectKinds(config.projects);

  // 预取 WSL 发行版列表,保证右键菜单构建时缓存已就绪
  useEffect(() => {
    prefetchWslDistros();
  }, []);

  // worktree 徽章:批量探测哪些项目路径是 linked worktree,是则记下分支名。
  // 远程项目路径在远端、无从探测;UNC(WSL)路径由后端直接跳过。
  // probeTick:窗口重获焦点时 +1 强制重探测——分支切换/worktree 增删都发生在窗外。
  const [worktreeBranches, setWorktreeBranches] = useState<Map<string, string>>(new Map());
  const [probeTick, setProbeTick] = useState(0);
  const projectPathsKey = config.projects
    .filter((p) => !p.sshConnectionId)
    .map((p) => p.path)
    .join('\n');
  useEffect(() => {
    const paths = projectPathsKey ? projectPathsKey.split('\n') : [];
    if (paths.length === 0) {
      setWorktreeBranches(new Map());
      return;
    }
    let cancelled = false;
    invoke<(string | null)[]>('get_worktree_branches', { paths })
      .then((res) => {
        if (cancelled) return;
        const next = new Map<string, string>();
        paths.forEach((p, i) => {
          const branch = res[i];
          if (branch) next.set(p, branch);
        });
        setWorktreeBranches(next);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [projectPathsKey, probeTick]);

  // 失效 worktree 子项目自动清理:AI agent/外部在终端里 `git worktree remove` 后
  // 不会有任何事件通知,挂载与窗口重获焦点时探测一次目录存在性;目录已消失
  // (且父项目目录仍在,排除盘符级整树消失的误判)的子项目连终端资源一起移除。
  useEffect(() => {
    let disposed = false;
    const reconcile = async () => {
      const projects = useAppStore.getState().config.projects;
      const probe = collectWorktreeProbePaths(projects);
      if (probe.length === 0) return;
      try {
        const existing = await invoke<string[]>('filter_directories', { paths: probe });
        if (disposed) return;
        for (const p of findStaleWorktreeProjects(projects, existing)) {
          removeProjectWithCleanup(p.id);
        }
      } catch {
        // 探测失败不做清理,下次获得焦点再试
      }
    };
    reconcile();
    let unlisten: (() => void) | undefined;
    getCurrentWindow()
      .onFocusChanged(({ payload: focused }) => {
        if (!focused) return;
        reconcile();
        setProbeTick((t) => t + 1);
      })
      .then((fn) => {
        if (disposed) fn();
        else unlisten = fn;
      })
      .catch(() => {});
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  // 写入项目类型徽标覆盖(undefined = 自动探测,'none' = 不显示),并持久化
  const setProjectKindOverride = useCallback((projectId: string, kind: ProjectKind | 'none' | undefined) => {
    const cfg = useAppStore.getState().config;
    const newConfig = {
      ...cfg,
      projects: cfg.projects.map((p) =>
        p.id === projectId ? { ...p, kindOverride: kind } : p,
      ),
    };
    useAppStore.getState().setConfig(newConfig);
    saveConfigToDisk(newConfig);
  }, []);

  // 写入项目的 WSL 会话来源发行版(undefined = 不启用),并持久化
  const setWslSessionsDistro = useCallback((projectId: string, distro: string | undefined) => {
    const cfg = useAppStore.getState().config;
    const newConfig = {
      ...cfg,
      projects: cfg.projects.map((p) =>
        p.id === projectId ? { ...p, wslSessionsDistro: distro } : p,
      ),
    };
    useAppStore.getState().setConfig(newConfig);
    saveConfigToDisk(newConfig);
  }, []);

  // === 系统文件拖放（从资源管理器拖入文件夹添加项目） ===
  useEffect(() => {
    let unlisten: (() => void) | undefined;

    const isOverProjectList = (position: { x: number; y: number }): boolean => {
      if (!projectListRef.current) return false;
      const rect = projectListRef.current.getBoundingClientRect();
      const scale = window.devicePixelRatio || 1;
      const x = position.x / scale;
      const y = position.y / scale;
      return x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom;
    };

    const onDrop = async (paths: string[]) => {
      const dirPaths: string[] = await invoke('filter_directories', { paths });
      if (dirPaths.length === 0) return;

      const { config: cfg, addProject: add, setActiveProject: setActive } = useAppStore.getState();
      let addedAny = false;
      let existingId: string | undefined;

      for (const dirPath of dirPaths) {
        const existing = cfg.projects.find((p) => p.path === dirPath);
        if (existing) {
          existingId = existing.id;
          continue;
        }
        const name = dirPath.split(/[/\\]/).pop() || dirPath;
        add({ id: genId(), name, path: dirPath });
        addedAny = true;
      }

      if (addedAny) {
        saveConfig();
      } else if (existingId) {
        setActive(existingId);
      }
    };

    getCurrentWebview()
      .onDragDropEvent((event) => {
        const { payload } = event;
        if (payload.type === 'enter') {
          setIsFileDragOver(isOverProjectList(payload.position));
          // 异步检测：文件 vs 文件夹 vs 重复
          invoke<string[]>('filter_directories', { paths: payload.paths }).then((dirPaths) => {
            if (dirPaths.length === 0) {
              setFileDragKind('forbidden');
            } else {
              const { config: cfg } = useAppStore.getState();
              const allDuplicate = dirPaths.every((p) => cfg.projects.some((proj) => proj.path === p));
              setFileDragKind(allDuplicate ? 'duplicate' : 'valid');
            }
          });
        } else if (payload.type === 'over') {
          setIsFileDragOver(isOverProjectList(payload.position));
        } else if (payload.type === 'drop') {
          setIsFileDragOver(false);
          setFileDragKind(null);
          if (isOverProjectList(payload.position)) {
            onDrop(payload.paths);
          }
        } else {
          setIsFileDragOver(false);
          setFileDragKind(null);
        }
      })
      .then((fn) => {
        unlisten = fn;
      });

    return () => {
      unlisten?.();
    };
  }, []);

  const handleAddProject = useCallback(async (targetGroupId?: string) => {
    const selected = await open({ directory: true, multiple: false });
    if (!selected) return;
    const path = selected as string;
    const name = path.split(/[/\\]/).pop() || path;
    const id = genId();
    addProject({ id, name, path });
    if (targetGroupId) {
      moveItem(id, targetGroupId);
      // 目标分组若折叠则展开,确保新项目可见
      const grp = findGroupInTree(useAppStore.getState().config.projectTree ?? [], targetGroupId);
      if (grp?.collapsed) toggleGroupCollapse(targetGroupId);
    }
    saveConfig();
  }, [addProject, moveItem, toggleGroupCollapse]);

  const handleRemoveProject = useCallback(
    (e: React.MouseEvent, id: string) => {
      e.stopPropagation();
      const project = config.projects.find((p) => p.id === id);
      if (project) setConfirmTarget({ id, name: project.name });
    },
    [config.projects]
  );

  const doRemove = useCallback(() => {
    if (!confirmTarget) return;
    // 删项目走的是 removeProject 而非 PaneGroup 关闭路径,后者才负责销毁终端;
    // removeProjectWithCleanup 会先回收该项目全部终端资源再删项目并落盘。
    removeProjectWithCleanup(confirmTarget.id);
    setConfirmTarget(null);
  }, [confirmTarget]);

  // 项目列表只关心 AI 相关状态:error 由 pane 自己在终端里显示,列在侧栏里
  // 会把「某个 shell 退出了」渲染成整个项目出事,反而盖住真正在跑的 AI。
  const getProjectStatus = (projectId: string): PaneStatus => {
    const layout = projectStates.get(projectId)?.layout;
    if (!layout) return 'idle';
    const hasPaneWith = (node: SplitNode, target: PaneStatus): boolean => {
      if (node.type === 'leaf') return node.panes.some((p) => p.status === target);
      return node.children.some((c) => hasPaneWith(c, target));
    };
    if (hasPaneWith(layout, 'ai-working')) return 'ai-working';
    return hasPaneWith(layout, 'ai-idle') ? 'ai-idle' : 'idle';
  };

  // 创建分组
  const handleCreateGroup = useCallback(async () => {
    const name = await showPrompt(t('projectList.newGroup'), t('projectList.newGroupPlaceholder'));
    if (!name?.trim()) return;
    createGroup(name.trim());
    saveConfig();
  }, [createGroup, t]);

  const renameProject = useAppStore((s) => s.renameProject);

  // 开始重命名项目
  const startRenameProject = useCallback((projectId: string, currentName: string) => {
    setEditingProjectId(projectId);
    setEditingName(currentName);
    setTimeout(() => editProjectInputRef.current?.select(), 0);
  }, []);

  // 提交项目重命名
  const commitProjectRename = useCallback(() => {
    if (editingProjectId && editingName.trim()) {
      renameProject(editingProjectId, editingName.trim());
      saveConfig();
    }
    setEditingProjectId(null);
  }, [editingProjectId, editingName, renameProject]);

  // 开始重命名分组
  const startRenameGroup = useCallback((groupId: string, currentName: string) => {
    setEditingGroupId(groupId);
    setEditingName(currentName);
    setTimeout(() => editInputRef.current?.select(), 0);
  }, []);

  // 提交重命名
  const commitRename = useCallback(() => {
    if (editingGroupId && editingName.trim()) {
      renameGroup(editingGroupId, editingName.trim());
      saveConfig();
    }
    setEditingGroupId(null);
  }, [editingGroupId, editingName, renameGroup]);

  // === pane 预览浮层：悬停项目行 250ms 后弹出，移出/按下/滚动/拖拽即关 ===
  // 仅限有 AI 会话的项目（hasAiPane）：预览的价值是「AI 在别的项目里跑到哪了」，
  // 普通 shell 项目弹一张 520px 的卡只是打断视线。

  const [preview, setPreview] = useState<{ projectId: string; rect: { top: number; right: number } } | null>(null);
  const previewTimer = useRef<ReturnType<typeof setTimeout>>(undefined);

  const closePreview = useCallback(() => {
    clearTimeout(previewTimer.current);
    setPreview(null);
  }, []);

  const handlePreviewEnter = useCallback((e: React.MouseEvent, projectId: string) => {
    clearTimeout(previewTimer.current);
    if (isProjectDragging()) return;
    // currentTarget 只在事件分发期间有效,先留住 DOM 引用;rect 到点弹时再取,
    // 悬停期间列表若有增删位置仍准
    const el = e.currentTarget as HTMLElement;
    previewTimer.current = setTimeout(() => {
      if (isProjectDragging() || !el.isConnected) return;
      // AI 判定放在到点时而非进入时:这 250ms 里 AI 完全可能刚起来(hook 一上报
      // 状态就变),读 getState 拿最新一份,不吃闭包里进入那一刻的旧值
      const s = useAppStore.getState();
      if (!hasAiPane(s.projectStates.get(projectId)?.layout, s.config.aiAutoResume ?? true)) return;
      const r = el.getBoundingClientRect();
      setPreview({ projectId, rect: { top: r.top, right: r.right } });
    }, 250);
  }, []);

  useEffect(() => () => clearTimeout(previewTimer.current), []);

  // AI 退出后把 preview 状态本身也收掉,而不只是渲染时 return null:留着的话
  // 同一次悬停里 AI 再起来(比如用户在别处让它接着跑),浮层会拿**旧 rect** 复活,
  // 而列表这期间完全可能已经增删过项目,卡就贴到别的行上了
  useEffect(() => {
    if (!preview) return;
    if (!hasAiPane(projectStates.get(preview.projectId)?.layout, config.aiAutoResume ?? true)) {
      closePreview();
    }
  }, [preview, projectStates, config.aiAutoResume, closePreview]);

  // 列表滚动/滚轮时浮层锚点失效,直接关闭(capture 才收得到内部容器的 scroll)
  useEffect(() => {
    if (!preview) return;
    window.addEventListener('scroll', closePreview, true);
    window.addEventListener('wheel', closePreview, { passive: true });
    return () => {
      window.removeEventListener('scroll', closePreview, true);
      window.removeEventListener('wheel', closePreview);
    };
  }, [preview, closePreview]);

  // === 拖拽处理（自定义鼠标事件，替代 HTML5 DnD，规避 WebView2 dragDropEnabled 拦截） ===

  const handleProjectMouseDown = useCallback((e: React.MouseEvent, projectId: string) => {
    if (e.button !== 0 || (e.target as HTMLElement).closest('input')) return;
    initProjectDrag(
      { type: 'project', projectId },
      e.currentTarget as HTMLElement,
      e.clientX, e.clientY,
    );
    onProjectDragEnd(() => setDropIndicator(null));
  }, []);

  const handleGroupMouseDown = useCallback((e: React.MouseEvent, groupId: string) => {
    if (e.button !== 0 || (e.target as HTMLElement).closest('input')) return;
    initProjectDrag(
      { type: 'group', groupId },
      e.currentTarget as HTMLElement,
      e.clientX, e.clientY,
    );
    onProjectDragEnd(() => setDropIndicator(null));
  }, []);

  const handleMouseMoveOver = useCallback((e: React.MouseEvent, targetId: string, allowInside: boolean) => {
    const payload = getProjectDragPayload();
    if (!payload) return;
    if (
      (payload.type === 'project' && payload.projectId === targetId) ||
      (payload.type === 'group' && payload.groupId === targetId)
    ) return;

    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const y = e.clientY - rect.top;
    const ratio = y / rect.height;

    let position: DropIndicator['position'];
    if (allowInside && ratio > 0.25 && ratio < 0.75) {
      position = 'inside';
    } else if (ratio < 0.5) {
      position = 'before';
    } else {
      position = 'after';
    }

    let forbidden = false;
    const tree = useAppStore.getState().config.projectTree ?? [];

    if (position === 'inside') {
      if (payload.type === 'project') {
        forbidden = !canDrop(tree, targetId, payload.projectId);
      } else {
        const draggedGroup = findGroupInTree(tree, payload.groupId);
        if (draggedGroup) {
          forbidden = !canDrop(tree, targetId, draggedGroup);
        }
      }
    } else if (payload.type === 'group') {
      const draggedGroup = findGroupInTree(tree, payload.groupId);
      if (draggedGroup) {
        forbidden = !canDropAt(tree, targetId, draggedGroup);
      }
    }

    setDropIndicator({ id: targetId, position, forbidden });
  }, []);

  const handleMouseLeaveTarget = useCallback(() => {
    if (!isProjectDragging()) return;
    setDropIndicator(null);
  }, []);

  const handleMouseUpDrop = useCallback((_e: React.MouseEvent, targetId: string) => {
    const payload = getProjectDragPayload();
    if (!payload) return;
    const indicator = dropIndicator;
    setDropIndicator(null);

    if (!indicator || indicator.forbidden) return;

    const itemId = payload.type === 'project' ? payload.projectId : payload.groupId;

    if (indicator.position === 'inside') {
      moveItem(itemId, targetId);
    } else {
      const tree = useAppStore.getState().config.projectTree ?? [];
      const parentGroupId = findParentGroupId(tree, targetId);
      const parent = parentGroupId
        ? findGroupInTree(tree, parentGroupId)?.children ?? []
        : tree;
      const targetIdx = parent.findIndex((item) =>
        (typeof item === 'string' ? item : item.id) === targetId
      );
      let insertIdx = indicator.position === 'after' ? targetIdx + 1 : targetIdx;
      const draggedIdx = parent.findIndex((item) =>
        (typeof item === 'string' ? item : item.id) === itemId
      );
      if (draggedIdx >= 0 && draggedIdx < insertIdx) {
        insertIdx--;
      }
      moveItem(itemId, parentGroupId ?? null, insertIdx);
    }
    saveConfig();
  }, [dropIndicator, moveItem]);

  // === 渲染子组件 ===

  const renderDropLine = (id: string, position: 'before' | 'after') => {
    if (dropIndicator?.id !== id || dropIndicator.position !== position) return null;
    if (dropIndicator.forbidden) return null;
    return (
      <div className="absolute left-1 right-1 h-0.5 bg-[var(--accent)] rounded-full z-10"
        style={position === 'before' ? { top: -1 } : { bottom: -1 }} />
    );
  };

  const renderProjectItem = (project: ProjectConfig, depth: number, parentGroupId?: string) => {
    const isActive = project.id === activeProjectId;
    const projectStatus = getProjectStatus(project.id);
    const projectPs = projectStates.get(project.id);
    const showDoneTag = !!projectPs?.needsAttention && !isActive;
    // SSH 远程项目:显示连接名标识;连接被删除 = 断链态（可见、可删,标识转错误色）
    const isRemote = !!project.sshConnectionId;
    const remoteConn = isRemote
      ? config.sshConnections.find((c) => c.id === project.sshConnectionId)
      : undefined;
    const remoteBroken = isRemote && !remoteConn;
    // 子项目(worktree「设为项目」):渲染由 getOrderedTree 注入到父项目下,
    // 位置是派生的 → 不能作为拖放目标;自身可拖走(= 脱离父项目转普通节点)
    const isChild = !!project.parentProjectId;
    // 项目路径是某仓库的 linked worktree → 显示 ⎇ 分支徽章
    const wtBranch = isRemote ? undefined : worktreeBranches.get(project.path);
    // 技术栈徽标:手动覆盖 > 自动探测;'none' = 用户关闭
    const projectKind: ProjectKind | null =
      project.kindOverride === 'none'
        ? null
        : project.kindOverride ?? projectKinds.get(project.id) ?? null;
    // 打开 pane 里的 AI 会话:有则领位图标换成品牌图标堆叠(哪家 AI 在跑一眼可见)。
    // 按厂商去重 —— 同款 AI 开多个 pane 只显示一枚,重叠一摞并不好看;
    // 再按厂商名字母序排列,不随开 pane 顺序漂移(未知厂商固定排最后)
    const aiVendors: (AiVendor | null)[] = [];
    {
      const seen = new Set<string>();
      for (const p of collectAiPanes(projectPs?.layout, config.aiAutoResume ?? true)) {
        const v = inferVendor({ agent: p.aiSession?.agent ?? p.detectedAgent });
        const key = v ?? 'unknown';
        if (!seen.has(key)) {
          seen.add(key);
          aiVendors.push(v);
        }
      }
      aiVendors.sort((a, b) =>
        a === null ? 1 : b === null ? -1 : a.localeCompare(b),
      );
    }

    return (
      <div
        key={project.id}
        className={`relative flex items-center gap-2 py-1.5 rounded-[var(--radius-sm)] cursor-pointer text-base group transition-all duration-150 ${
          isActive
            ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
            : 'text-[var(--text-secondary)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
        }`}
        style={{
          // 组内项目对齐父级分组的倒三角区域((depth-1)*16 起,+16 不贴左缘);
          // 无分组链时(顶层项目及其 worktree 子项目)以 10px 为基准每层 +16,
          // 不能共用组内公式——那会把顶层 worktree 子项目的相对缩进压到 6px
          paddingLeft: `${parentGroupId ? (depth - 1) * 16 + 16 : 10 + depth * 16}px`,
          paddingRight: '10px',
        }}
        role="option"
        aria-selected={isActive}
        tabIndex={0}
        // 绝对路径的去处按有无预览分流:有 AI 会话时浮层的卡头显示路径(原生
        // tooltip 会盖住浮层,不能同时挂);没有则退回行 title —— 否则不弹浮层
        // 的项目路径就彻底不可见了
        title={aiVendors.length > 0 ? undefined : project.path}
        onMouseDown={(e) => {
          closePreview();
          handleProjectMouseDown(e, project.id);
        }}
        onMouseEnter={(e) => handlePreviewEnter(e, project.id)}
        onMouseMove={isChild ? undefined : (e) => handleMouseMoveOver(e, project.id, false)}
        onMouseLeave={() => {
          closePreview();
          if (!isChild) handleMouseLeaveTarget();
        }}
        onMouseUp={isChild ? undefined : (e) => handleMouseUpDrop(e, project.id)}
        onClick={() => setActiveProject(project.id)}
        onKeyDown={(e) => {
          if (editingProjectId === project.id) return; // 重命名输入框自己处理按键
          if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            setActiveProject(project.id);
          } else if (e.key === 'F2') {
            e.preventDefault();
            startRenameProject(project.id, project.name);
          } else if (e.key === 'Delete') {
            e.preventDefault();
            setConfirmTarget({ id: project.id, name: project.name });
          }
        }}
        onContextMenu={(e) => {
          e.preventDefault();
          e.stopPropagation();
          closePreview();
          // 远程项目 gate:本地专属入口（资源管理器打开 / 关联 SSH MCP / 环境变量 /
          // WSL 会话）一律隐藏 —— agent 已在远程机、envVars 不注入远程 shell（二期）,
          // 路径也不是本机可打开的位置。保留:重命名 / 复制绝对路径（远程 POSIX）/ 分组操作。
          const menuItems: Parameters<typeof showContextMenu>[2] = [
            { label: t('projectList.menu.rename'), onClick: () => startRenameProject(project.id, project.name) },
            {
              label: t('projectList.menu.editDescription'),
              onClick: async () => {
                const next = await showPrompt(
                  t('projectList.menu.editDescription'),
                  t('projectList.descriptionPlaceholder'),
                  project.description ?? '',
                );
                if (next === null) return;
                useAppStore.getState().setProjectDescription(project.id, next.trim());
                saveConfig();
              },
            },
            ...(isRemote ? [] : [
              { label: t('projectList.menu.openInFolder'), onClick: () => revealItemInDir(project.path) },
            ]),
            { label: t('projectList.menu.copyAbsolutePath'), onClick: () => navigator.clipboard.writeText(project.path) },
            ...(isRemote ? [] : [
              { separator: true as const },
              {
                label: t('projectList.menu.associateSsh'),
                onClick: () => setSshAssocTarget(project),
              },
              {
                label: t('projectList.menu.envVars'),
                onClick: () => setEnvVarsTarget(project),
              },
              {
                label: t('projectList.menu.worktrees'),
                onClick: () => setWorktreeTarget({ path: project.path, projectId: project.id }),
              },
            ]),
          ];
          // 「WSL 会话」子菜单:选择该项目的 WSL 会话来源发行版。
          // WSL 根项目不显示(distro 从路径自动推导);远程项目不显示(来源互斥);
          // 发行版枚举为空且未配置时也不显示(非 Windows / 未装 WSL 自然隐藏)。
          if (!isRemote && !isWslPath(project.path)) {
            const distros = wslDistrosCache ?? [];
            const current = project.wslSessionsDistro;
            if (distros.length > 0 || current) {
              const submenu: Parameters<typeof showContextMenu>[2] = [
                {
                  label: `${current ? '　' : '✓ '}${t('projectList.menu.wslSessionsDisable')}`,
                  onClick: () => setWslSessionsDistro(project.id, undefined),
                },
                { separator: true },
                ...distros.map((d) => ({
                  label: `${current === d.name ? '✓ ' : '　'}${d.name}`,
                  onClick: () => setWslSessionsDistro(project.id, d.name),
                })),
              ];
              // 已配置的发行版枚举不到(被卸载):显示「(未找到)」标记项
              if (current && !distros.some((d) => d.name === current)) {
                submenu.push({
                  label: `✓ ${t('projectList.menu.wslSessionsNotFound', { name: current })}`,
                  disabled: true,
                });
              }
              menuItems.push({ label: t('projectList.menu.wslSessions'), submenu });
            }
          }
          // 「项目类型」子菜单:探测只提供默认值,允许手动指定或关闭徽标;
          // 远程项目领位固定 SSH 图标,不显示该菜单
          if (!isRemote) {
            const currentKind = project.kindOverride;
            const detected = projectKinds.get(project.id);
            menuItems.push({
              label: t('projectList.menu.projectKind'),
              submenu: [
                {
                  label: `${currentKind === undefined ? '✓ ' : '　'}${t('projectList.menu.projectKindAuto')}${
                    detected ? `（${PROJECT_KIND_LABELS[detected]}）` : ''
                  }`,
                  onClick: () => setProjectKindOverride(project.id, undefined),
                },
                {
                  label: `${currentKind === 'none' ? '✓ ' : '　'}${t('projectList.menu.projectKindHidden')}`,
                  onClick: () => setProjectKindOverride(project.id, 'none'),
                },
                { separator: true },
                ...PROJECT_KINDS.map((k) => ({
                  label: `${currentKind === k ? '✓ ' : '　'}${PROJECT_KIND_LABELS[k]}`,
                  onClick: () => setProjectKindOverride(project.id, k),
                })),
              ],
            });
          }
          // 添加分组相关菜单
          // 子项目不在树里(位置由父项目派生),没有「当前所在组」可言 ——
          // 选任意组都是有效动作(顺带脱离父项目),所以不传 currentParentId
          const moveToEntries = buildMoveToGroupMenu(
            config.projectTree ?? [],
            0,
            isChild ? undefined : parentGroupId,
            (groupId) => { moveItem(project.id, groupId); saveConfig(); },
            t,
          );
          if (moveToEntries.length > 0 || isChild || parentGroupId) {
            menuItems.push({ separator: true });
            if (isChild) {
              // 脱离父项目 = 清 parentProjectId 并转为顶层树节点(moveItem 内处理)
              menuItems.push({
                label: t('projectList.menu.detachFromParent'),
                onClick: () => { moveItem(project.id, null); saveConfig(); },
              });
            } else if (parentGroupId) {
              menuItems.push({
                label: t('projectList.menu.moveOutOfGroup'),
                onClick: () => { moveItem(project.id, null); saveConfig(); },
              });
            }
            if (moveToEntries.length > 0) {
              menuItems.push({ label: t('projectList.menu.moveToGroup'), submenu: moveToEntries });
            }
          }
          // 移除项目:与 ✕ 按钮 / Delete 键同一条确认路径
          menuItems.push(
            { separator: true },
            {
              label: t('projectList.menu.remove'),
              danger: true,
              onClick: () => setConfirmTarget({ id: project.id, name: project.name }),
            },
          );
          showContextMenu(e.clientX, e.clientY, menuItems);
        }}
      >
        {renderDropLine(project.id, 'before')}
        {isActive && (
          <span className="w-0.5 h-4 rounded-full bg-[var(--accent)] flex-shrink-0" />
        )}
        {/* 领位:项目身份图标恒显(SSH > 技术栈 > 通用),AI 品牌堆叠只追加不覆盖 */}
        {isRemote ? (
          <Server
            size={14}
            strokeWidth={1.5}
            aria-hidden
            className={`flex-shrink-0 ${remoteBroken ? 'text-[var(--color-error)]' : 'text-[var(--color-info)]'}`}
          />
        ) : projectKind ? (
          <TechIcon kind={projectKind} size={14} className="flex-shrink-0" />
        ) : (
          // 识别不出 / 用户选「不显示」:回退通用项目图标(主题文件色),每行都有图标、缩进对齐
          <Package size={14} strokeWidth={1.5} aria-hidden className="flex-shrink-0 text-[var(--color-file)]" />
        )}
        {aiVendors.length > 0 && (
          // 固定 text-secondary 颜色上下文:单色品牌图标(OpenAI/Grok…)与 pane
          // 标签观感一致,不随选中行的 accent 变色。
          // 负边距抵掉行内 gap-2,与领位图标只留 2px 小间隙;图标间同样 2px 并排不重叠
          <span
            className="flex items-center flex-shrink-0 text-[var(--text-secondary)]"
            style={{ marginLeft: -6, gap: 2 }}
          >
            {aiVendors.map((v) => (
              <BrandIcon key={v ?? 'unknown'} vendor={v} size={AI_ICON_SIZE} />
            ))}
          </span>
        )}
        {editingProjectId === project.id ? (
          <input
            ref={editProjectInputRef}
            className="truncate flex-1 bg-transparent border-b border-[var(--accent)] outline-none text-base text-[var(--text-primary)] px-0 py-0 select-text"
            value={editingName}
            onChange={(e) => setEditingName(e.target.value)}
            onBlur={commitProjectRename}
            onKeyDown={(e) => {
              if (e.key === 'Enter') commitProjectRename();
              if (e.key === 'Escape') setEditingProjectId(null);
            }}
            onClick={(e) => e.stopPropagation()}
            autoFocus
          />
        ) : (
          <span className="truncate flex-1">
            {project.name}
            {project.description && (
              <span className="ml-1.5 text-xs text-[var(--text-muted)]" title={project.description}>
                {project.description}
              </span>
            )}
          </span>
        )}
        {wtBranch && (
          <span
            className="flex-shrink-0 max-w-[100px] truncate text-xs leading-[14px] px-1 rounded font-mono text-[var(--text-muted)] bg-[var(--border-subtle)]"
            title={t('projectList.worktreeBadgeTitle', { branch: wtBranch })}
          >
            ⎇ {wtBranch}
          </span>
        )}
        {isRemote && (
          <span
            className={`flex-shrink-0 max-w-[80px] truncate text-xs leading-[14px] px-1 rounded font-mono ${
              remoteBroken
                ? 'text-[var(--color-error)] bg-[var(--color-error)]/15'
                : 'text-[var(--text-muted)] bg-[var(--border-subtle)]'
            }`}
            title={remoteBroken
              ? t('projectList.remoteBrokenTitle')
              : t('projectList.remoteBadgeTitle', { summary: remoteConn ? connectionSummary(remoteConn) : '' })}
          >
            {remoteBroken ? t('projectList.remoteBrokenBadge') : (remoteConn?.name ?? 'SSH')}
          </span>
        )}
        {showDoneTag ? <DoneTag /> : projectStatus !== 'idle' ? <StatusDot status={projectStatus} /> : null}
        <button
          type="button"
          tabIndex={-1}
          className="text-[var(--text-muted)] hover:text-[var(--color-error)] hidden group-hover:inline transition-colors text-sm"
          title={t('projectList.menu.remove')}
          aria-label={t('projectList.menu.remove')}
          onClick={(e) => handleRemoveProject(e, project.id)}
        >
          ✕
        </button>
        {renderDropLine(project.id, 'after')}
      </div>
    );
  };

  const renderGroup = (group: ProjectGroup, depth: number) => {
    const isEditing = editingGroupId === group.id;
    const isInsideTarget = dropIndicator?.id === group.id && dropIndicator.position === 'inside';
    const isForbidden = isInsideTarget && dropIndicator?.forbidden;
    const groupDepth = getDepth(config.projectTree ?? [], group.id);

    return (
      <div key={group.id} className="relative">
        {renderDropLine(group.id, 'before')}
        <div
          className={`flex items-center gap-1.5 py-1.5 rounded-[var(--radius-sm)] cursor-pointer text-sm transition-all duration-150 select-none ${
            isForbidden
              ? 'border border-dashed border-[var(--color-error)] cursor-not-allowed'
              : isInsideTarget
                ? 'bg-[var(--accent-subtle)] border border-dashed border-[var(--accent)]'
                : 'text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
          }`}
          style={{ paddingLeft: `${depth * 16}px`, paddingRight: '10px' }}
          role="treeitem"
          aria-expanded={!group.collapsed}
          tabIndex={0}
          onMouseDown={(e) => handleGroupMouseDown(e, group.id)}
          onMouseMove={(e) => handleMouseMoveOver(e, group.id, true)}
          onMouseLeave={handleMouseLeaveTarget}
          onMouseUp={(e) => handleMouseUpDrop(e, group.id)}
          onClick={() => { if (!isEditing) toggleGroupCollapse(group.id); saveConfig(); }}
          onKeyDown={(e) => {
            if (isEditing) return;
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              toggleGroupCollapse(group.id);
              saveConfig();
            } else if (e.key === 'F2') {
              e.preventDefault();
              startRenameGroup(group.id, group.name);
            }
          }}
          onContextMenu={(e) => {
            e.preventDefault();
            e.stopPropagation();
            const menuItems: Parameters<typeof showContextMenu>[2] = [
              { label: t('projectList.menu.renameGroup'), onClick: () => startRenameGroup(group.id, group.name) },
              { label: t('projectList.menu.addProject'), onClick: () => handleAddProject(group.id) },
              { label: t('projectList.menu.addRemoteProject'), onClick: () => setAddRemoteTarget({ groupId: group.id }) },
            ];
            if (depth > 0) {
              menuItems.push({
                label: t('projectList.menu.moveOutOfGroup'),
                onClick: () => { moveItem(group.id, null); saveConfig(); },
              });
            }
            if (groupDepth < MAX_DEPTH - 1) {
              menuItems.push({
                label: t('projectList.menu.newSubgroup'),
                onClick: async () => {
                  const name = await showPrompt(t('projectList.newSubgroup'), t('projectList.newSubgroupPlaceholder'));
                  if (!name?.trim()) return;
                  createGroup(name.trim(), group.id);
                  saveConfig();
                },
              });
            }
            menuItems.push({
              label: t('projectList.menu.deleteGroup'),
              danger: true,
              onClick: async () => {
                // 删组不删项目,但组内项目会散回上一级 —— 组织结构没得撤销,先确认
                const ok = await showConfirm(
                  t('projectList.deleteGroupConfirm.title'),
                  t('projectList.deleteGroupConfirm.message', {
                    name: group.name,
                    count: countProjectsInGroup(group),
                  }),
                );
                if (!ok) return;
                removeGroup(group.id);
                saveConfig();
              },
            });
            showContextMenu(e.clientX, e.clientY, menuItems);
          }}
        >
          <span className="text-xs flex-shrink-0 w-3 text-center transition-transform duration-150"
            style={{ transform: group.collapsed ? 'rotate(-90deg)' : undefined }}>
            ▾
          </span>
          {/* 分组 = 空间:用 Boxes 图标,着主题文件夹色(容器语义,四套主题自适配) */}
          <Boxes size={13} strokeWidth={1.5} aria-hidden className="flex-shrink-0 text-[var(--color-folder)]" />
          {isEditing ? (
            <input
              ref={editInputRef}
              className="flex-1 bg-transparent border-b border-[var(--accent)] outline-none text-sm text-[var(--text-primary)] px-0 py-0 select-text"
              value={editingName}
              onChange={(e) => setEditingName(e.target.value)}
              onBlur={commitRename}
              onKeyDown={(e) => {
                if (e.key === 'Enter') commitRename();
                if (e.key === 'Escape') setEditingGroupId(null);
              }}
              onClick={(e) => e.stopPropagation()}
              autoFocus
            />
          ) : (
            <span className="truncate flex-1 font-medium">{group.name}</span>
          )}
          <span className="text-xs text-[var(--text-muted)]">({countProjectsInGroup(group)})</span>
        </div>
        {renderDropLine(group.id, 'after')}
      </div>
    );
  };

  return (
    <div data-panel data-mt-part="sidebar" className="h-full bg-[var(--bg-surface)] flex flex-col select-none">
      {/* 项目列表（Sessions 已移至右侧悬浮抽屉） */}
      <div ref={projectListRef} className="relative flex-1 min-h-0 flex flex-col overflow-hidden">
            {isFileDragOver && (
              <div className={`absolute inset-0 z-20 border-2 border-dashed rounded-[var(--radius-md)] flex items-center justify-center pointer-events-none ${
                fileDragKind === 'forbidden'
                  ? 'bg-[var(--color-error)]/10 border-[var(--color-error)]'
                  : fileDragKind === 'duplicate'
                    ? 'bg-[var(--color-warning,#f59e0b)]/10 border-[var(--color-warning,#f59e0b)]'
                    : 'bg-[var(--accent)]/10 border-[var(--accent)]'
              }`}>
                <span className={`text-sm font-medium ${
                  fileDragKind === 'forbidden'
                    ? 'text-[var(--color-error)]'
                    : fileDragKind === 'duplicate'
                      ? 'text-[var(--color-warning,#f59e0b)]'
                      : 'text-[var(--accent)]'
                }`}>
                  {fileDragKind === 'forbidden'
                    ? t('projectList.dragHint.forbidden')
                    : fileDragKind === 'duplicate'
                      ? t('projectList.dragHint.duplicate')
                      : t('projectList.dragHint.valid')}
                </span>
              </div>
            )}
            <div
              className="px-3 pt-3 pb-1.5 text-sm text-[var(--text-muted)] uppercase tracking-[0.12em] font-medium cursor-default"
              onContextMenu={(e) => {
                e.preventDefault();
                showContextMenu(e.clientX, e.clientY, [
                  { label: t('projectList.newGroup'), onClick: handleCreateGroup },
                ]);
              }}
            >
              {t('panels.projects')}
            </div>

            <div className="flex-1 overflow-y-auto px-1.5 space-y-0.5" role="listbox" aria-label={t('panels.projects')}>
              {orderedItems.map((item) =>
                item.type === 'project'
                  ? renderProjectItem(item.project, item.depth, item.parentGroupId ?? undefined)
                  : renderGroup(item.group, item.depth)
              )}
            </div>

            <div className="p-2 flex gap-1.5">
              <button
                type="button"
                className="flex-1 px-3 py-2 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-center text-sm text-[var(--text-muted)] cursor-pointer hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all duration-200"
                onClick={() => handleAddProject()}
              >
                {t('projectList.addProject')}
              </button>
              <button
                type="button"
                className="px-2 py-2 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-center text-sm text-[var(--text-muted)] cursor-pointer hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all duration-200 font-mono"
                onClick={() => setAddRemoteTarget({})}
                title={t('projectList.addRemoteProject')}
                aria-label={t('projectList.addRemoteProject')}
              >
                SSH
              </button>
              <button
                type="button"
                className="px-3 py-2 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-center text-sm text-[var(--text-muted)] cursor-pointer hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all duration-200"
                onClick={handleCreateGroup}
                title={t('projectList.newGroup')}
                aria-label={t('projectList.newGroup')}
              >
                +
              </button>
            </div>
      </div>

      {/* 关联 SSH 弹窗 */}
      <SshAssocModal project={sshAssocTarget} onClose={() => setSshAssocTarget(null)} />
      {/* 环境变量弹窗 */}
      <ProjectEnvVarsModal project={envVarsTarget} onClose={() => setEnvVarsTarget(null)} />
      {/* 添加远程项目弹窗 */}
      <AddRemoteProjectModal
        open={!!addRemoteTarget}
        targetGroupId={addRemoteTarget?.groupId}
        onClose={() => setAddRemoteTarget(null)}
      />
      {/* Worktree 管理弹窗(项目右键菜单进入)。项目根目录未必是仓库,
          交给弹窗按 Git 面板同一套逻辑向下发现仓库(多个时可勾选批量新建)。
          onChanged 留空:后端已在增删后失效仓库发现缓存,Git 抽屉下次加载即为新数据 */}
      <GitWorktreeModal
        repoPath={worktreeTarget?.path ?? null}
        discoverRepos
        projectId={worktreeTarget?.projectId}
        onClose={() => setWorktreeTarget(null)}
        onChanged={() => {}}
      />

      {/* 删除确认 —— Modal 内部 portal 到 body,避免 fluent2 [data-panel] 的
          backdrop-filter 形成 containing block 把 fixed 拽进面板 */}
      <Modal
        open={!!confirmTarget}
        onClose={() => setConfirmTarget(null)}
        align="center"
        ariaLabel={t('projectList.removeConfirm.title')}
        panelClassName="w-[320px]"
      >
        <div className="p-5">
          <div className="text-sm font-medium text-[var(--text-primary)] mb-2">{t('projectList.removeConfirm.title')}</div>
          <div className="text-xs text-[var(--text-secondary)] mb-5">
            {t('projectList.removeConfirm.messagePrefix')}<span className="text-[var(--accent)]">{confirmTarget?.name}</span>{t('projectList.removeConfirm.messageSuffix')}
          </div>
          <div className="flex justify-end gap-2">
            <button
              className="px-3 py-1.5 text-xs rounded-[var(--radius-sm)] text-[var(--text-secondary)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)] transition-colors"
              onClick={() => setConfirmTarget(null)}
            >
              {t('projectList.removeConfirm.cancel')}
            </button>
            <button
              className="px-3 py-1.5 text-xs rounded-[var(--radius-sm)] bg-[var(--color-error)] text-white hover:opacity-90 transition-opacity"
              onClick={doRemove}
              autoFocus
            >
              {t('projectList.removeConfirm.confirm')}
            </button>
          </div>
        </div>
      </Modal>

      {/* pane 预览浮层:portal 到 body 越过 Allotment 裁剪;纯展示不参与命中。
          项目已被删除时 find 落空,静默不渲染 */}
      {preview && (() => {
        const p = config.projects.find((x) => x.id === preview.projectId);
        // 开闸条件每次渲染重判:AI 跑完退出的那一帧就不画了。state 的收尾交给
        // 上面那个 effect(它在 paint 之后才跑,只靠它会先闪一帧过期的卡)
        if (!p || !hasAiPane(projectStates.get(preview.projectId)?.layout, config.aiAutoResume ?? true)) {
          return null;
        }
        return createPortal(
          <ProjectPanePreview project={p} anchorRect={preview.rect} />,
          document.body,
        );
      })()}
    </div>
  );
}
