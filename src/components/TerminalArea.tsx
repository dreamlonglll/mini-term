import { useCallback } from 'react';
import { useAppStore, saveLayoutToConfig } from '../store';
import { SplitLayout } from './SplitLayout';
import { showContextMenu } from '../utils/contextMenu';
import { isRemoteProject } from '../utils/remoteProject';
import { newTerminal } from '../utils/paneActions';
import { hotkeyLabel } from '../utils/hotkeys';
import { useT } from '../i18n';
import type { SplitNode } from '../types';

interface Props {
  projectId: string;
  projectPath: string;
}

// 收集 SplitNode 树中所有 pane ID
function collectPaneIds(node: SplitNode): string[] {
  if (node.type === 'leaf') return node.panes.map((p) => p.id);
  return node.children.flatMap(collectPaneIds);
}

export function TerminalArea({ projectId, projectPath }: Props) {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const projectStates = useAppStore((s) => s.projectStates);
  const setProjectLayout = useAppStore((s) => s.setProjectLayout);
  const layout = projectStates.get(projectId)?.layout ?? null;
  // SSH 远程项目:新开 pane 一律 spawn ssh 启动器(shell 选择无意义)
  const project = config.projects.find((p) => p.id === projectId);
  const remote = isRemoteProject(project);

  const handleNewTabClick = useCallback((e: React.MouseEvent) => {
    // 远程项目 / 只有一个 shell 时不弹菜单,直接开
    if (remote || config.availableShells.length <= 1) {
      void newTerminal(projectId);
      return;
    }
    showContextMenu(
      e.clientX,
      e.clientY,
      config.availableShells.map((shell) => ({
        label: shell.name,
        onClick: () => void newTerminal(projectId, shell),
      })),
    );
  }, [remote, config.availableShells, projectId]);

  // 分隔条拖动后的 sizes 回写。pane 的增删由 paneActions 直接写全局布局，
  // 不再经过这里，所以只需要防「过期 RAF 回调覆盖新结构」这一种情况。
  const handleLayoutChange = useCallback((updatedNode: SplitNode) => {
    const current = useAppStore.getState().projectStates.get(projectId)?.layout;
    if (!current) return;
    const currentIds = collectPaneIds(current).sort().join(',');
    const updatedIds = collectPaneIds(updatedNode).sort().join(',');
    if (currentIds !== updatedIds) return;

    setProjectLayout(projectId, updatedNode);
    saveLayoutToConfig(projectId);
  }, [projectId, setProjectLayout]);

  return (
    <div data-panel data-mt-part="terminal-area" className="flex flex-col h-full bg-[var(--bg-terminal)]">
      <div className="flex-1 overflow-hidden relative">
        {layout ? (
          <div className="absolute inset-0">
            <SplitLayout
              projectId={projectId}
              node={layout}
              projectPath={projectPath}
              onLayoutChange={handleLayoutChange}
            />
          </div>
        ) : (
          <div className="flex flex-col items-center justify-center h-full gap-4 px-6 text-center">
            <div className="text-sm text-[var(--text-secondary)]">
              {t('terminalArea.emptyTitle', { project: project?.name ?? '' })}
            </div>
            <button
              type="button"
              className="px-5 py-2.5 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-sm text-[var(--text-muted)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all duration-200"
              onClick={handleNewTabClick}
            >
              + {t('terminalArea.newTerminal')}
            </button>
            <div className="flex items-center gap-1.5 text-xs text-[var(--text-muted)]">
              <span>{t('terminalArea.emptyHint')}</span>
              <kbd className="kbd">{hotkeyLabel('newTerminal')}</kbd>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
