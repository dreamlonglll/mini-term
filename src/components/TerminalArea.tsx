import { useCallback } from 'react';
import { useAppStore, genId, saveLayoutToConfig } from '../store';
import { SplitLayout } from './SplitLayout';
import { showContextMenu } from '../utils/contextMenu';
import { createProjectPty, isRemoteProject, remotePaneLabel } from '../utils/remoteProject';
import { showAlert } from '../utils/prompt';
import { useT } from '../i18n';
import type { TerminalTab, PaneState, SplitNode, ShellConfig } from '../types';

interface Props {
  projectId: string;
  projectPath: string;
}

// 收集 SplitNode 树中所有 pane ID
function collectPaneIds(node: SplitNode): string[] {
  if (node.type === 'leaf') return node.panes.map((p) => p.id);
  return node.children.flatMap(collectPaneIds);
}

function insertSplit(
  node: SplitNode,
  targetPaneId: string,
  direction: 'horizontal' | 'vertical',
  newLeaf: SplitNode
): SplitNode {
  if (node.type === 'leaf') {
    if (node.panes.some((p) => p.id === targetPaneId)) {
      return {
        type: 'split',
        direction,
        children: [node, newLeaf],
        sizes: [50, 50],
      };
    }
    return node;
  }
  return {
    ...node,
    children: node.children.map((c) => insertSplit(c, targetPaneId, direction, newLeaf)),
  };
}


export function TerminalArea({ projectId, projectPath }: Props) {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const projectStates = useAppStore((s) => s.projectStates);
  const addTab = useAppStore((s) => s.addTab);
  const updateTabLayout = useAppStore((s) => s.updateTabLayout);
  const removeTab = useAppStore((s) => s.removeTab);
  const ps = projectStates.get(projectId);
  const activeTab = ps?.tabs.find((t) => t.id === ps.activeTabId);
  // SSH 远程项目:新开 tab/分屏 pane 一律 spawn ssh 启动器(shell 选择无意义);
  // 断链 / 缺 ssh 客户端时后端返回明确 Err,弹窗提示。
  const project = config.projects.find((p) => p.id === projectId);
  const remote = isRemoteProject(project);

  const handleNewTab = useCallback(async (selectedShell?: ShellConfig) => {
    const shell = selectedShell
      ?? config.availableShells.find((s) => s.name === config.defaultShell)
      ?? config.availableShells[0];
    if (!project) return;
    if (!remote && !shell) return;

    let ptyId: number;
    try {
      ptyId = await createProjectPty(project, shell);
    } catch (e) {
      await showAlert(t('terminalArea.remoteConnectFailedTitle'), e instanceof Error ? e.message : String(e));
      return;
    }

    const paneId = genId();
    const tabId = genId();

    const tab: TerminalTab = {
      id: tabId,
      status: 'idle',
      splitLayout: {
        type: 'leaf',
        panes: [{
          id: paneId,
          shellName: remote ? remotePaneLabel(project) : shell!.name,
          status: 'idle',
          ptyId,
        }],
        activePaneId: paneId,
      },
    };

    addTab(projectId, tab);
    saveLayoutToConfig(projectId);
  }, [projectId, project, remote, config, addTab, t]);

  const handleNewTabClick = useCallback((e: React.MouseEvent) => {
    // 远程项目不弹 shell 菜单:pane 固定为 ssh 启动器
    if (remote) {
      void handleNewTab();
      return;
    }
    showContextMenu(
      e.clientX,
      e.clientY,
      config.availableShells.map((shell) => ({
        label: shell.name,
        onClick: () => handleNewTab(shell),
      })),
    );
  }, [remote, config.availableShells, handleNewTab]);

  const handleSplitPane = useCallback(
    async (paneId: string, direction: 'horizontal' | 'vertical') => {
      if (!ps || !activeTab || !project) return;
      const shell = config.availableShells.find((s) => s.name === config.defaultShell)
        ?? config.availableShells[0];
      if (!remote && !shell) return;

      let ptyId: number;
      try {
        ptyId = await createProjectPty(project, shell);
      } catch (e) {
        await showAlert(t('terminalArea.remoteConnectFailedTitle'), e instanceof Error ? e.message : String(e));
        return;
      }

      const newPane: PaneState = {
        id: genId(),
        shellName: remote ? remotePaneLabel(project) : shell!.name,
        status: 'idle',
        ptyId,
      };

      const newLeaf: SplitNode = {
        type: 'leaf',
        panes: [newPane],
        activePaneId: newPane.id,
      };

      const newLayout = insertSplit(activeTab.splitLayout, paneId, direction, newLeaf);
      updateTabLayout(projectId, activeTab.id, newLayout);
      saveLayoutToConfig(projectId);
    },
    [ps, activeTab, config, project, remote, projectId, updateTabLayout, t]
  );

  // Called when an entire leaf (pane group) is closed.
  // PTYs are already killed by PaneGroup before this is called.
  // For the root leaf case, we close the whole tab.
  const handleCloseLeaf = useCallback((_leafNode: SplitNode) => {
    const currentPs = useAppStore.getState().projectStates.get(projectId);
    const currentTab = currentPs?.tabs.find(t => t.id === currentPs.activeTabId);
    if (!currentTab) return;

    // PTYs are already killed by PaneGroup before this is called.
    // Remove the entire layout tab.
    removeTab(projectId, currentTab.id);
    saveLayoutToConfig(projectId);
  }, [projectId, removeTab]);

  const handleLayoutChange = useCallback((updatedNode: SplitNode) => {
    const currentPs = useAppStore.getState().projectStates.get(projectId);
    const currentActiveTab = currentPs?.tabs.find((t) => t.id === currentPs.activeTabId);
    if (!currentActiveTab) return;

    // Validate layout structure: if pane ID sets differ, discard stale RAF callback
    const currentIds = collectPaneIds(currentActiveTab.splitLayout).sort().join(',');
    const updatedIds = collectPaneIds(updatedNode).sort().join(',');
    if (currentIds !== updatedIds) return;

    updateTabLayout(projectId, currentActiveTab.id, updatedNode);
    saveLayoutToConfig(projectId);
  }, [projectId, updateTabLayout]);

  // Handler for structural changes: tabs added/removed/switched within a leaf,
  // or children removed from a split. Bypasses pane-ID validation since the
  // set of pane IDs is expected to change.
  const handleUpdateNode = useCallback((updatedNode: SplitNode) => {
    const currentPs = useAppStore.getState().projectStates.get(projectId);
    const currentActiveTab = currentPs?.tabs.find((t) => t.id === currentPs.activeTabId);
    if (!currentActiveTab) return;
    updateTabLayout(projectId, currentActiveTab.id, updatedNode);
    saveLayoutToConfig(projectId);
  }, [projectId, updateTabLayout]);

  return (
    <div data-panel className="flex flex-col h-full bg-[var(--bg-terminal)]">
      <div className="flex-1 overflow-hidden relative">
        {activeTab && (
          <div
            key={activeTab.id}
            className="absolute inset-0"
          >
            <SplitLayout
              projectId={projectId}
              node={activeTab.splitLayout}
              projectPath={projectPath}
              onSplit={handleSplitPane}
              onCloseLeaf={handleCloseLeaf}
              onUpdateNode={handleUpdateNode}
              onLayoutChange={handleLayoutChange}
            />
          </div>
        )}

        {(!ps || ps.tabs.length === 0) && (
          <div className="flex flex-col items-center justify-center h-full gap-3 text-[var(--text-muted)]">
            <div className="text-3xl opacity-20">⌘</div>
            <button
              className="px-5 py-2.5 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-sm hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all duration-200"
              onClick={handleNewTabClick}
            >
              + {t("terminalArea.newTerminal")}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
