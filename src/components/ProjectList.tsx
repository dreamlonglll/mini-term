import { useCallback, useState, useRef, useEffect } from 'react';
import { createPortal } from 'react-dom';
import { Allotment } from 'allotment';
import { open } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import { revealItemInDir } from '@tauri-apps/plugin-opener';
import { useAppStore, genId, collectPtyIds } from '../store';
import { StatusDot } from './StatusDot';
import { DoneTag } from './DoneTag';
import { SessionList } from './SessionList';
import { SshAssocModal } from './SshAssocModal';
import { ProjectEnvVarsModal } from './ProjectEnvVarsModal';
import { showContextMenu } from '../utils/contextMenu';
import { showPrompt } from '../utils/prompt';
import { disposeTerminal } from '../utils/terminalCache';
import { initProjectDrag, isProjectDragging, getProjectDragPayload, onProjectDragEnd } from '../utils/projectDragState';
import { useT } from '../i18n';
import {
  getOrderedTree,
  collectAllGroups,
  countProjectsInGroup,
  canDrop,
  canDropAt,
  getDepth,
  findParentGroupId,
  findGroupInTree,
  MAX_DEPTH,
} from '../utils/projectTree';
import type { PaneStatus, SplitNode, ProjectConfig, ProjectGroup } from '../types';

// 保存配置的快捷方法
function saveConfig() {
  const config = useAppStore.getState().config;
  invoke('save_config', { config });
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
  const removeProject = useAppStore((s) => s.removeProject);
  const createGroup = useAppStore((s) => s.createGroup);
  const removeGroup = useAppStore((s) => s.removeGroup);
  const renameGroup = useAppStore((s) => s.renameGroup);
  const toggleGroupCollapse = useAppStore((s) => s.toggleGroupCollapse);
  const moveItem = useAppStore((s) => s.moveItem);

  const [confirmTarget, setConfirmTarget] = useState<{ id: string; name: string } | null>(null);
  const [sshAssocTarget, setSshAssocTarget] = useState<ProjectConfig | null>(null);
  const [envVarsTarget, setEnvVarsTarget] = useState<ProjectConfig | null>(null);
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
  const allGroups = collectAllGroups(config.projectTree ?? []);

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
    const id = confirmTarget.id;
    // 删项目走的是 removeProject 而非 PaneGroup 关闭路径,后者才负责销毁终端。
    // 这里先回收该项目所有 tab/分屏下的终端:杀后端 PTY 子进程 + dispose 前端 xterm 实例,
    // 否则会残留孤儿 shell/AI 进程(继续占用 CPU/内存、AI 可能还在烧 token)与泄漏的
    // WebGL 上下文。markers 由 removeProject 内部清理。
    const ps = useAppStore.getState().projectStates.get(id);
    if (ps) {
      const ptyIds = new Set<number>();
      for (const tab of ps.tabs) {
        for (const pid of collectPtyIds(tab.splitLayout)) ptyIds.add(pid);
      }
      for (const ptyId of ptyIds) {
        invoke('kill_pty', { ptyId }).catch(() => {});
        disposeTerminal(ptyId);
      }
    }
    removeProject(id);
    saveConfig();
    setConfirmTarget(null);
  }, [confirmTarget, removeProject, saveConfig]);

  const getProjectStatus = (projectId: string): PaneStatus => {
    const ps = projectStates.get(projectId);
    if (!ps || ps.tabs.length === 0) return 'idle';
    const hasPaneWith = (node: SplitNode, target: PaneStatus): boolean => {
      if (node.type === 'leaf') return node.panes.some((p) => p.status === target);
      return node.children.some((c) => hasPaneWith(c, target));
    };
    let hasAiIdle = false;
    for (const tab of ps.tabs) {
      if (hasPaneWith(tab.splitLayout, 'ai-working')) return 'ai-working';
      if (hasPaneWith(tab.splitLayout, 'ai-idle')) hasAiIdle = true;
    }
    return hasAiIdle ? 'ai-idle' : 'idle';
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

    return (
      <div
        key={project.id}
        className={`relative flex items-center gap-2 py-1.5 rounded-[var(--radius-sm)] cursor-pointer text-base group transition-all duration-150 ${
          isActive
            ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
            : 'text-[var(--text-secondary)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
        }`}
        style={{ paddingLeft: `${depth * 16 + 10}px`, paddingRight: '10px' }}
        onMouseDown={(e) => handleProjectMouseDown(e, project.id)}
        onMouseMove={(e) => handleMouseMoveOver(e, project.id, false)}
        onMouseLeave={handleMouseLeaveTarget}
        onMouseUp={(e) => handleMouseUpDrop(e, project.id)}
        onClick={() => setActiveProject(project.id)}
        onContextMenu={(e) => {
          e.preventDefault();
          e.stopPropagation();
          const menuItems: Parameters<typeof showContextMenu>[2] = [
            { label: t('projectList.menu.rename'), onClick: () => startRenameProject(project.id, project.name) },
            { label: t('projectList.menu.openInFolder'), onClick: () => revealItemInDir(project.path) },
            { label: t('projectList.menu.copyAbsolutePath'), onClick: () => navigator.clipboard.writeText(project.path) },
            { separator: true },
            {
              label: t('projectList.menu.associateSsh'),
              onClick: () => setSshAssocTarget(project),
            },
            {
              label: t('projectList.menu.envVars'),
              onClick: () => setEnvVarsTarget(project),
            },
          ];
          // 添加分组相关菜单
          if (allGroups.length > 0) {
            menuItems.push({ separator: true });
            if (parentGroupId) {
              menuItems.push({
                label: t('projectList.menu.moveOutOfGroup'),
                onClick: () => { moveItem(project.id, null); saveConfig(); },
              });
            }
            for (const [g, gDepth] of allGroups) {
              if (g.id === parentGroupId) continue;
              if (gDepth + 1 > MAX_DEPTH) continue;
              menuItems.push({
                label: t('projectList.menu.moveTo', { name: g.name }),
                onClick: () => { moveItem(project.id, g.id); saveConfig(); },
              });
            }
          }
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
        {showDoneTag ? <DoneTag /> : projectStatus !== 'idle' ? <StatusDot status={projectStatus} /> : null}
        <span
          className="text-[var(--text-muted)] hover:text-[var(--color-error)] hidden group-hover:inline transition-colors text-sm"
          onClick={(e) => handleRemoveProject(e, project.id)}
        >
          ✕
        </span>
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
          onMouseDown={(e) => handleGroupMouseDown(e, group.id)}
          onMouseMove={(e) => handleMouseMoveOver(e, group.id, true)}
          onMouseLeave={handleMouseLeaveTarget}
          onMouseUp={(e) => handleMouseUpDrop(e, group.id)}
          onClick={() => { if (!isEditing) toggleGroupCollapse(group.id); saveConfig(); }}
          onContextMenu={(e) => {
            e.preventDefault();
            e.stopPropagation();
            const menuItems: Parameters<typeof showContextMenu>[2] = [
              { label: t('projectList.menu.renameGroup'), onClick: () => startRenameGroup(group.id, group.name) },
              { label: t('projectList.menu.addProject'), onClick: () => handleAddProject(group.id) },
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
            menuItems.push(
              { label: t('projectList.menu.deleteGroup'), danger: true, onClick: () => { removeGroup(group.id); saveConfig(); } },
            );
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

  const projectsVisible = config.projectsVisible;
  const sessionsVisible = config.sessionsVisible;

  return (
    <div data-panel className="h-full bg-[var(--bg-surface)] flex flex-col select-none">
      <Allotment vertical>
        {/* 上半部分：项目列表 */}
        <Allotment.Pane minSize={100} visible={projectsVisible}>
          <div ref={projectListRef} className="relative h-full flex flex-col overflow-hidden">
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
              Projects
            </div>

            <div className="flex-1 overflow-y-auto px-1.5 space-y-0.5">
              {orderedItems.map((item) =>
                item.type === 'project'
                  ? renderProjectItem(item.project, item.depth, item.parentGroupId ?? undefined)
                  : renderGroup(item.group, item.depth)
              )}
            </div>

            <div className="p-2 flex gap-1.5">
              <div
                className="flex-1 px-3 py-2 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-center text-sm text-[var(--text-muted)] cursor-pointer hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all duration-200"
                onClick={() => handleAddProject()}
              >
                {t('projectList.addProject')}
              </div>
              <div
                className="px-3 py-2 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-center text-sm text-[var(--text-muted)] cursor-pointer hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all duration-200"
                onClick={handleCreateGroup}
                title={t('projectList.newGroup')}
              >
                +
              </div>
            </div>
          </div>
        </Allotment.Pane>

        {/* 下半部分：会话列表 */}
        <Allotment.Pane minSize={80} visible={sessionsVisible}>
          <SessionList />
        </Allotment.Pane>
      </Allotment>

      {/* 关联 SSH 弹窗 */}
      <SshAssocModal project={sshAssocTarget} onClose={() => setSshAssocTarget(null)} />
      {/* 环境变量弹窗 */}
      <ProjectEnvVarsModal project={envVarsTarget} onClose={() => setEnvVarsTarget(null)} />

      {/* 删除确认弹窗 — portal 到 body,避免 fluent2 [data-panel] 的 backdrop-filter 形成 containing block 把 fixed 拽进面板 */}
      {confirmTarget && createPortal(
        <div className="fixed inset-0 z-50 flex items-center justify-center" onClick={() => setConfirmTarget(null)}>
          <div className="absolute inset-0 bg-black/50 backdrop-blur-sm" />
          <div
            className="relative w-[320px] bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-[var(--shadow-overlay)] p-5 animate-slide-in"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="text-sm font-medium text-[var(--text-primary)] mb-2">{t('projectList.removeConfirm.title')}</div>
            <div className="text-xs text-[var(--text-secondary)] mb-5">
              {t('projectList.removeConfirm.messagePrefix')}<span className="text-[var(--accent)]">{confirmTarget.name}</span>{t('projectList.removeConfirm.messageSuffix')}
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
              >
                {t('projectList.removeConfirm.confirm')}
              </button>
            </div>
          </div>
        </div>,
        document.body,
      )}
    </div>
  );
}
