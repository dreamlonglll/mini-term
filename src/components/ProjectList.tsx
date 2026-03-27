import { useCallback } from 'react';
import { open } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { revealItemInDir } from '@tauri-apps/plugin-opener';
import { useAppStore, genId } from '../store';
import { StatusDot } from './StatusDot';
import { showContextMenu } from '../utils/contextMenu';
import type { PaneStatus } from '../types';

export function ProjectList() {
  const config = useAppStore((s) => s.config);
  const activeProjectId = useAppStore((s) => s.activeProjectId);
  const projectStates = useAppStore((s) => s.projectStates);
  const setActiveProject = useAppStore((s) => s.setActiveProject);
  const addProject = useAppStore((s) => s.addProject);
  const removeProject = useAppStore((s) => s.removeProject);

  const handleAddProject = useCallback(async () => {
    const selected = await open({ directory: true, multiple: false });
    if (!selected) return;

    const path = selected as string;
    const name = path.split(/[/\\]/).pop() || path;
    const id = genId();

    addProject({ id, name, path });

    // 持久化配置
    const latestConfig = useAppStore.getState().config;
    invoke('save_config', { config: latestConfig });
  }, [addProject]);

  const handleRemoveProject = useCallback(
    (e: React.MouseEvent, id: string) => {
      e.stopPropagation();
      removeProject(id);
      const latestConfig = useAppStore.getState().config;
      invoke('save_config', { config: latestConfig });
    },
    [removeProject]
  );

  // 获取项目的 AI 聚合状态
  const getProjectAiStatus = (projectId: string): PaneStatus | null => {
    const ps = projectStates.get(projectId);
    if (!ps) return null;
    for (const tab of ps.tabs) {
      if (tab.status === 'ai-working') return 'ai-working';
      if (tab.status === 'ai-idle') return 'ai-idle';
    }
    return null;
  };

  return (
    <div className="h-full bg-[var(--bg-surface)] flex flex-col overflow-y-auto">
      <div className="px-3 pt-3 pb-1.5 text-sm text-[var(--text-muted)] uppercase tracking-[0.12em] font-medium">
        Projects
      </div>

      <div className="flex-1 px-1.5 space-y-0.5">
        {config.projects.map((project) => {
          const isActive = project.id === activeProjectId;
          const aiStatus = getProjectAiStatus(project.id);

          return (
            <div
              key={project.id}
              className={`flex items-center gap-2 px-2.5 py-1.5 rounded-[var(--radius-sm)] cursor-pointer text-base group transition-all duration-150 ${
                isActive
                  ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
                  : 'text-[var(--text-secondary)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
              }`}
              onClick={() => setActiveProject(project.id)}
              onContextMenu={(e) => {
                e.preventDefault();
                e.stopPropagation();
                showContextMenu(e.clientX, e.clientY, [
                  {
                    label: '在文件夹中打开',
                    onClick: () => revealItemInDir(project.path),
                  },
                  {
                    label: '复制绝对路径',
                    onClick: () => navigator.clipboard.writeText(project.path),
                  },
                ]);
              }}
              title={project.path}
            >
              {isActive && (
                <span className="w-0.5 h-4 rounded-full bg-[var(--accent)] flex-shrink-0" />
              )}
              <span className="truncate flex-1">{project.name}</span>
              {aiStatus && <StatusDot status={aiStatus} />}
              <span
                className="text-[var(--text-muted)] hover:text-[var(--color-error)] hidden group-hover:inline transition-colors text-sm"
                onClick={(e) => handleRemoveProject(e, project.id)}
              >
                ✕
              </span>
            </div>
          );
        })}
      </div>

      <div className="p-2">
        <div
          className="px-3 py-2 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-center text-sm text-[var(--text-muted)] cursor-pointer hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all duration-200"
          onClick={handleAddProject}
        >
          + 添加项目
        </div>
      </div>
    </div>
  );
}
