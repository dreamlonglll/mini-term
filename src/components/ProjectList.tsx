import { useCallback, useState, useRef, useEffect } from 'react';
import { open } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import { revealItemInDir } from '@tauri-apps/plugin-opener';
import { useAppStore, genId } from '../store';
import { removeProjectWithCleanup } from '../utils/projectActions';
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
import type { PaneStatus, SplitNode, ProjectConfig, ProjectGroup, ProjectTreeItem, WslDistro } from '../types';

// 保存配置的快捷方法
function saveConfig() {
  const config = useAppStore.getState().config;
  invoke('save_config', { config });
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

  // 预取 WSL 发行版列表,保证右键菜单构建时缓存已就绪
  useEffect(() => {
    prefetchWslDistros();
  }, []);

  // worktree 徽章:批量探测哪些项目路径是 linked worktree,是则记下分支名。
  // 远程项目路径在远端、无从探测;UNC(WSL)路径由后端直接跳过。
  const [worktreeBranches, setWorktreeBranches] = useState<Map<string, string>>(new Map());
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
  }, [projectPathsKey]);

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
    invoke('save_config', { config: newConfig });
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

    return (
      <div
        key={project.id}
        className={`relative flex items-center gap-2 py-1.5 rounded-[var(--radius-sm)] cursor-pointer text-base group transition-all duration-150 ${
          isActive
            ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
            : 'text-[var(--text-secondary)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
        }`}
        style={{ paddingLeft: `${depth * 16 + 10}px`, paddingRight: '10px' }}
        role="option"
        aria-selected={isActive}
        tabIndex={0}
        onMouseDown={(e) => handleProjectMouseDown(e, project.id)}
        onMouseMove={isChild ? undefined : (e) => handleMouseMoveOver(e, project.id, false)}
        onMouseLeave={isChild ? undefined : handleMouseLeaveTarget}
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
          // 远程项目 gate:本地专属入口（资源管理器打开 / 关联 SSH MCP / 环境变量 /
          // WSL 会话）一律隐藏 —— agent 已在远程机、envVars 不注入远程 shell（二期）,
          // 路径也不是本机可打开的位置。保留:重命名 / 复制绝对路径（远程 POSIX）/ 分组操作。
          const menuItems: Parameters<typeof showContextMenu>[2] = [
            { label: t('projectList.menu.rename'), onClick: () => startRenameProject(project.id, project.name) },
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
        title={project.path}
      >
        {renderDropLine(project.id, 'before')}
        {isActive && (
          <span className="w-0.5 h-4 rounded-full bg-[var(--accent)] flex-shrink-0" />
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
          <span className="truncate flex-1">{project.name}</span>
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
    <div data-panel className="h-full bg-[var(--bg-surface)] flex flex-col select-none">
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
    </div>
  );
}
