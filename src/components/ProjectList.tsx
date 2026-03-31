import { useCallback, useState, useRef } from 'react';
import { Allotment } from 'allotment';
import { open } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { revealItemInDir } from '@tauri-apps/plugin-opener';
import { useAppStore, genId, getOrderedItems } from '../store';
import { StatusDot } from './StatusDot';
import { SessionList } from './SessionList';
import { showContextMenu } from '../utils/contextMenu';
import { setDragPayload, getDragPayload } from '../utils/dragState';
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
}

export function ProjectList() {
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
  const moveProjectToGroup = useAppStore((s) => s.moveProjectToGroup);
  const moveProjectOutOfGroup = useAppStore((s) => s.moveProjectOutOfGroup);
  const reorderItems = useAppStore((s) => s.reorderItems);

  const [confirmTarget, setConfirmTarget] = useState<{ id: string; name: string } | null>(null);
  const [dropIndicator, setDropIndicator] = useState<DropIndicator | null>(null);
  const [editingGroupId, setEditingGroupId] = useState<string | null>(null);
  const [editingName, setEditingName] = useState('');
  const editInputRef = useRef<HTMLInputElement>(null);

  const orderedItems = getOrderedItems(config);
  const groups = config.projectGroups ?? [];

  const handleAddProject = useCallback(async () => {
    const selected = await open({ directory: true, multiple: false });
    if (!selected) return;
    const path = selected as string;
    const name = path.split(/[/\\]/).pop() || path;
    addProject({ id: genId(), name, path });
    saveConfig();
  }, [addProject]);

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
    removeProject(confirmTarget.id);
    saveConfig();
    setConfirmTarget(null);
  }, [confirmTarget, removeProject]);

  const getProjectStatus = (projectId: string): PaneStatus => {
    const ps = projectStates.get(projectId);
    if (!ps || ps.tabs.length === 0) return 'idle';
    const hasPaneWith = (node: SplitNode, target: PaneStatus): boolean => {
      if (node.type === 'leaf') return node.pane.status === target;
      return node.children.some((c) => hasPaneWith(c, target));
    };
    let hasAiWorking = false;
    for (const tab of ps.tabs) {
      if (hasPaneWith(tab.splitLayout, 'ai-idle')) return 'ai-idle';
      if (hasPaneWith(tab.splitLayout, 'ai-working')) hasAiWorking = true;
    }
    return hasAiWorking ? 'ai-working' : 'idle';
  };

  // 创建分组
  const handleCreateGroup = useCallback(() => {
    const name = window.prompt('输入分组名称');
    if (!name?.trim()) return;
    createGroup(name.trim());
    saveConfig();
  }, [createGroup]);

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

  // === 拖拽处理 ===

  const handleProjectDragStart = useCallback((e: React.DragEvent, projectId: string) => {
    e.dataTransfer.setData('application/project-id', projectId);
    e.dataTransfer.effectAllowed = 'move';
    setDragPayload({ type: 'project', projectId });
    // 添加拖拽时的半透明效果
    requestAnimationFrame(() => {
      (e.target as HTMLElement).style.opacity = '0.4';
    });
  }, []);

  const handleGroupDragStart = useCallback((e: React.DragEvent, groupId: string) => {
    e.dataTransfer.setData('application/group-id', groupId);
    e.dataTransfer.effectAllowed = 'move';
    setDragPayload({ type: 'group', groupId });
    requestAnimationFrame(() => {
      (e.target as HTMLElement).style.opacity = '0.4';
    });
  }, []);

  const handleDragEnd = useCallback((e: React.DragEvent) => {
    (e.target as HTMLElement).style.opacity = '';
    setDragPayload(null);
    setDropIndicator(null);
  }, []);

  const handleDragOver = useCallback((e: React.DragEvent, targetId: string, allowInside: boolean) => {
    const payload = getDragPayload();
    if (!payload || payload.type === 'tab') return;
    // 不能拖到自己身上
    if (
      (payload.type === 'project' && payload.projectId === targetId) ||
      (payload.type === 'group' && payload.groupId === targetId)
    ) {
      return;
    }
    e.preventDefault();
    e.dataTransfer.dropEffect = 'move';

    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const y = e.clientY - rect.top;
    const ratio = y / rect.height;

    let position: DropIndicator['position'];
    if (allowInside && payload.type === 'project' && ratio > 0.25 && ratio < 0.75) {
      position = 'inside';
    } else if (ratio < 0.5) {
      position = 'before';
    } else {
      position = 'after';
    }
    setDropIndicator({ id: targetId, position });
  }, []);

  const handleDragLeave = useCallback((e: React.DragEvent) => {
    const next = e.relatedTarget as Node | null;
    if (!next || !e.currentTarget.contains(next)) {
      setDropIndicator(null);
    }
  }, []);

  const handleDrop = useCallback((e: React.DragEvent, targetId: string, targetContext?: { groupId?: string }) => {
    e.preventDefault();
    const payload = getDragPayload();
    if (!payload || payload.type === 'tab') return;
    const indicator = dropIndicator;
    setDropIndicator(null);
    setDragPayload(null);
    (e.target as HTMLElement).style.opacity = '';

    if (!indicator) return;

    const ordering = useAppStore.getState().config.projectOrdering ?? [];

    if (payload.type === 'project') {
      const projectId = payload.projectId;

      if (indicator.position === 'inside') {
        // 拖到分组上 → 移入分组
        moveProjectToGroup(projectId, targetId);
      } else if (targetContext?.groupId) {
        // 拖到分组内的项目上 → 移入同分组并排序
        const group = (useAppStore.getState().config.projectGroups ?? []).find((g) => g.id === targetContext.groupId);
        if (group) {
          const targetIdx = group.projectIds.indexOf(targetId);
          const insertIdx = indicator.position === 'after' ? targetIdx + 1 : targetIdx;
          moveProjectToGroup(projectId, targetContext.groupId, insertIdx);
        }
      } else {
        // 拖到顶层项目/分组旁 → 重排序
        // 先从分组中移出
        const groups = useAppStore.getState().config.projectGroups ?? [];
        const inGroup = groups.some((g) => g.projectIds.includes(projectId));
        if (inGroup) {
          moveProjectOutOfGroup(projectId, indicator.position === 'after' ? targetId : undefined);
          // 需要再调整位置
          const newOrdering = [...(useAppStore.getState().config.projectOrdering ?? [])];
          const fromIdx = newOrdering.indexOf(projectId);
          const toIdx = newOrdering.indexOf(targetId);
          if (fromIdx >= 0 && toIdx >= 0) {
            newOrdering.splice(fromIdx, 1);
            const insertIdx = indicator.position === 'after' ? newOrdering.indexOf(targetId) + 1 : newOrdering.indexOf(targetId);
            newOrdering.splice(insertIdx, 0, projectId);
            reorderItems(newOrdering);
          }
        } else {
          // 已在顶层，直接重排序
          const newOrdering = ordering.filter((id) => id !== projectId);
          const toIdx = newOrdering.indexOf(targetId);
          const insertIdx = indicator.position === 'after' ? toIdx + 1 : toIdx;
          newOrdering.splice(insertIdx, 0, projectId);
          reorderItems(newOrdering);
        }
      }
    } else if (payload.type === 'group') {
      // 分组重排序
      const groupId = payload.groupId;
      const newOrdering = ordering.filter((id) => id !== groupId);
      const toIdx = newOrdering.indexOf(targetId);
      const insertIdx = indicator.position === 'after' ? toIdx + 1 : Math.max(toIdx, 0);
      newOrdering.splice(insertIdx, 0, groupId);
      reorderItems(newOrdering);
    }
    saveConfig();
  }, [dropIndicator, moveProjectToGroup, moveProjectOutOfGroup, reorderItems]);

  // === 渲染子组件 ===

  const renderDropLine = (id: string, position: 'before' | 'after') => {
    if (dropIndicator?.id !== id || dropIndicator.position !== position) return null;
    return (
      <div className="absolute left-1 right-1 h-0.5 bg-[var(--accent)] rounded-full z-10"
        style={position === 'before' ? { top: -1 } : { bottom: -1 }} />
    );
  };

  const renderProjectItem = (project: ProjectConfig, groupId?: string) => {
    const isActive = project.id === activeProjectId;
    const projectStatus = getProjectStatus(project.id);

    return (
      <div
        key={project.id}
        className={`relative flex items-center gap-2 px-2.5 py-1.5 rounded-[var(--radius-sm)] cursor-pointer text-base group transition-all duration-150 ${
          isActive
            ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
            : 'text-[var(--text-secondary)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
        } ${groupId ? 'pl-5' : ''}`}
        draggable
        onDragStart={(e) => handleProjectDragStart(e, project.id)}
        onDragEnd={handleDragEnd}
        onDragOver={(e) => handleDragOver(e, project.id, false)}
        onDragLeave={handleDragLeave}
        onDrop={(e) => handleDrop(e, project.id, { groupId })}
        onClick={() => setActiveProject(project.id)}
        onContextMenu={(e) => {
          e.preventDefault();
          e.stopPropagation();
          const menuItems: Parameters<typeof showContextMenu>[2] = [
            { label: '在文件夹中打开', onClick: () => revealItemInDir(project.path) },
            { label: '复制绝对路径', onClick: () => navigator.clipboard.writeText(project.path) },
          ];
          // 添加分组相关菜单
          if (groups.length > 0) {
            menuItems.push({ separator: true });
            if (groupId) {
              menuItems.push({
                label: '移出分组',
                onClick: () => { moveProjectOutOfGroup(project.id); saveConfig(); },
              });
            }
            for (const g of groups) {
              if (g.id === groupId) continue;
              menuItems.push({
                label: `移动到「${g.name}」`,
                onClick: () => { moveProjectToGroup(project.id, g.id); saveConfig(); },
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
        <span className="truncate flex-1">{project.name}</span>
        <StatusDot status={projectStatus} />
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

  const renderGroup = (group: ProjectGroup, projects: ProjectConfig[]) => {
    const isEditing = editingGroupId === group.id;
    const isInsideTarget = dropIndicator?.id === group.id && dropIndicator.position === 'inside';

    return (
      <div key={group.id} className="relative">
        {renderDropLine(group.id, 'before')}
        <div
          className={`flex items-center gap-1.5 px-2.5 py-1.5 rounded-[var(--radius-sm)] cursor-pointer text-sm transition-all duration-150 select-none ${
            isInsideTarget
              ? 'bg-[var(--accent-subtle)] border border-dashed border-[var(--accent)]'
              : 'text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
          }`}
          draggable={!isEditing}
          onDragStart={(e) => handleGroupDragStart(e, group.id)}
          onDragEnd={handleDragEnd}
          onDragOver={(e) => handleDragOver(e, group.id, true)}
          onDragLeave={handleDragLeave}
          onDrop={(e) => handleDrop(e, group.id)}
          onClick={() => { if (!isEditing) toggleGroupCollapse(group.id); saveConfig(); }}
          onContextMenu={(e) => {
            e.preventDefault();
            e.stopPropagation();
            showContextMenu(e.clientX, e.clientY, [
              { label: '重命名分组', onClick: () => startRenameGroup(group.id, group.name) },
              { label: '删除分组（保留项目）', danger: true, onClick: () => { removeGroup(group.id); saveConfig(); } },
            ]);
          }}
        >
          <span className="text-xs flex-shrink-0 w-3 text-center transition-transform duration-150"
            style={{ transform: group.collapsed ? 'rotate(-90deg)' : undefined }}>
            ▾
          </span>
          {isEditing ? (
            <input
              ref={editInputRef}
              className="flex-1 bg-transparent border-b border-[var(--accent)] outline-none text-sm text-[var(--text-primary)] px-0 py-0"
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
          <span className="text-xs text-[var(--text-muted)]">({projects.length})</span>
        </div>
        {!group.collapsed && (
          <div className="space-y-0.5 mt-0.5">
            {projects.map((p) => renderProjectItem(p, group.id))}
          </div>
        )}
        {renderDropLine(group.id, 'after')}
      </div>
    );
  };

  return (
    <div className="h-full bg-[var(--bg-surface)] flex flex-col">
      <Allotment vertical>
        {/* 上半部分：项目列表 */}
        <Allotment.Pane minSize={100}>
          <div className="h-full flex flex-col overflow-hidden">
            <div
              className="px-3 pt-3 pb-1.5 text-sm text-[var(--text-muted)] uppercase tracking-[0.12em] font-medium cursor-default"
              onContextMenu={(e) => {
                e.preventDefault();
                showContextMenu(e.clientX, e.clientY, [
                  { label: '新建分组', onClick: handleCreateGroup },
                ]);
              }}
            >
              Projects
            </div>

            <div className="flex-1 overflow-y-auto px-1.5 space-y-0.5">
              {orderedItems.map((item) =>
                item.type === 'project'
                  ? renderProjectItem(item.project)
                  : renderGroup(item.group, item.projects)
              )}
            </div>

            <div className="p-2 flex gap-1.5">
              <div
                className="flex-1 px-3 py-2 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-center text-sm text-[var(--text-muted)] cursor-pointer hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all duration-200"
                onClick={handleAddProject}
              >
                + 添加项目
              </div>
              <div
                className="px-3 py-2 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-center text-sm text-[var(--text-muted)] cursor-pointer hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all duration-200"
                onClick={handleCreateGroup}
                title="新建分组"
              >
                +
              </div>
            </div>
          </div>
        </Allotment.Pane>

        {/* 下半部分：会话列表 */}
        <Allotment.Pane minSize={80}>
          <SessionList />
        </Allotment.Pane>
      </Allotment>

      {/* 删除确认弹窗 */}
      {confirmTarget && (
        <div className="fixed inset-0 z-50 flex items-center justify-center" onClick={() => setConfirmTarget(null)}>
          <div className="absolute inset-0 bg-black/50 backdrop-blur-sm" />
          <div
            className="relative w-[320px] bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-2xl p-5 animate-slide-in"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="text-sm font-medium text-[var(--text-primary)] mb-2">移除项目</div>
            <div className="text-xs text-[var(--text-secondary)] mb-5">
              确定要移除项目「<span className="text-[var(--accent)]">{confirmTarget.name}</span>」吗？此操作仅从列表中移除，不会删除文件。
            </div>
            <div className="flex justify-end gap-2">
              <button
                className="px-3 py-1.5 text-xs rounded-[var(--radius-sm)] text-[var(--text-secondary)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)] transition-colors"
                onClick={() => setConfirmTarget(null)}
              >
                取消
              </button>
              <button
                className="px-3 py-1.5 text-xs rounded-[var(--radius-sm)] bg-[var(--color-error)] text-white hover:opacity-90 transition-opacity"
                onClick={doRemove}
              >
                移除
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
