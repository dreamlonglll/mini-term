import { useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore, genId, collectPtyIds, saveLayoutToConfig } from '../store';
import { TabBar } from './TabBar';
import { SplitLayout } from './SplitLayout';
import { showContextMenu } from '../utils/contextMenu';
import type { TerminalTab, PaneState, SplitNode, ShellConfig } from '../types';

interface Props {
  projectId: string;
  projectPath: string;
}

function removePane(node: SplitNode, targetPaneId: string): SplitNode | null {
  if (node.type === 'leaf') {
    return node.pane.id === targetPaneId ? null : node;
  }
  const remaining = node.children
    .map((c) => removePane(c, targetPaneId))
    .filter((c): c is SplitNode => c !== null);
  if (remaining.length === 0) return null;
  if (remaining.length === 1) return remaining[0];
  return { ...node, children: remaining, sizes: remaining.map(() => 100 / remaining.length) };
}

function insertSplit(
  node: SplitNode,
  targetPaneId: string,
  direction: 'horizontal' | 'vertical',
  newPane: PaneState
): SplitNode {
  if (node.type === 'leaf') {
    if (node.pane.id === targetPaneId) {
      return {
        type: 'split',
        direction,
        children: [node, { type: 'leaf', pane: newPane }],
        sizes: [50, 50],
      };
    }
    return node;
  }
  return {
    ...node,
    children: node.children.map((c) => insertSplit(c, targetPaneId, direction, newPane)),
  };
}

function insertSplitNode(
  node: SplitNode,
  targetPaneId: string,
  direction: 'horizontal' | 'vertical',
  newNode: SplitNode,
  position: 'before' | 'after'
): SplitNode {
  if (node.type === 'leaf') {
    if (node.pane.id === targetPaneId) {
      const children = position === 'before' ? [newNode, node] : [node, newNode];
      return { type: 'split', direction, children, sizes: [50, 50] };
    }
    return node;
  }
  return {
    ...node,
    children: node.children.map((c) => insertSplitNode(c, targetPaneId, direction, newNode, position)),
  };
}

export function TerminalArea({ projectId, projectPath }: Props) {
  const config = useAppStore((s) => s.config);
  const projectStates = useAppStore((s) => s.projectStates);
  const addTab = useAppStore((s) => s.addTab);
  const updateTabLayout = useAppStore((s) => s.updateTabLayout);
  const removeTab = useAppStore((s) => s.removeTab);
  const ps = projectStates.get(projectId);
  const activeTab = ps?.tabs.find((t) => t.id === ps.activeTabId);

  const handleCloseTab = useCallback(async (tabId: string) => {
    const tab = ps?.tabs.find(t => t.id === tabId);
    if (tab) {
      const ptyIds = collectPtyIds(tab.splitLayout);
      for (const id of ptyIds) {
        await invoke('kill_pty', { ptyId: id });
      }
    }
    removeTab(projectId, tabId);
    saveLayoutToConfig(projectId);
  }, [ps, projectId, removeTab]);

  const handleNewTab = useCallback(async (selectedShell?: ShellConfig) => {
    const shell = selectedShell
      ?? config.availableShells.find((s) => s.name === config.defaultShell)
      ?? config.availableShells[0];
    if (!shell) return;

    const ptyId = await invoke<number>('create_pty', {
      shell: shell.command,
      args: shell.args ?? [],
      cwd: projectPath,
    });

    const paneId = genId();
    const tabId = genId();

    const tab: TerminalTab = {
      id: tabId,
      status: 'idle',
      splitLayout: {
        type: 'leaf',
        pane: {
          id: paneId,
          shellName: shell.name,
          status: 'idle',
          ptyId,
        },
      },
    };

    addTab(projectId, tab);
    saveLayoutToConfig(projectId);
  }, [projectId, projectPath, config, addTab]);

  const handleNewTabClick = useCallback((e: React.MouseEvent) => {
    showContextMenu(
      e.clientX,
      e.clientY,
      config.availableShells.map((shell) => ({
        label: shell.name,
        onClick: () => handleNewTab(shell),
      })),
    );
  }, [config.availableShells, handleNewTab]);

  const handleSplitPane = useCallback(
    async (paneId: string, direction: 'horizontal' | 'vertical') => {
      if (!ps || !activeTab) return;
      const shell = config.availableShells.find((s) => s.name === config.defaultShell)
        ?? config.availableShells[0];
      if (!shell) return;

      const ptyId = await invoke<number>('create_pty', {
        shell: shell.command,
        args: shell.args ?? [],
        cwd: projectPath,
      });

      const newPane: PaneState = {
        id: genId(),
        shellName: shell.name,
        status: 'idle',
        ptyId,
      };

      const newLayout = insertSplit(activeTab.splitLayout, paneId, direction, newPane);
      updateTabLayout(projectId, activeTab.id, newLayout);
      saveLayoutToConfig(projectId);
    },
    [ps, activeTab, config, projectId, projectPath, updateTabLayout]
  );

  const handleTabDrop = useCallback(
    (sourceTabId: string, targetPaneId: string, direction: 'horizontal' | 'vertical', position: 'before' | 'after') => {
      if (!ps || !activeTab) return;
      if (sourceTabId === activeTab.id) return;
      const sourceTab = ps.tabs.find((t) => t.id === sourceTabId);
      if (!sourceTab) return;

      const newLayout = insertSplitNode(
        activeTab.splitLayout,
        targetPaneId,
        direction,
        sourceTab.splitLayout,
        position
      );
      updateTabLayout(projectId, activeTab.id, newLayout);
      removeTab(projectId, sourceTabId);
      saveLayoutToConfig(projectId);
    },
    [ps, activeTab, projectId, updateTabLayout, removeTab]
  );

  const handleClosePane = useCallback(async (paneId: string) => {
    // 从 store 读取最新状态，避免闭包过期
    const currentPs = useAppStore.getState().projectStates.get(projectId);
    const currentTab = currentPs?.tabs.find(t => t.id === currentPs.activeTabId);
    if (!currentTab) return;

    const findPty = (node: SplitNode): number | null => {
      if (node.type === 'leaf') return node.pane.id === paneId ? node.pane.ptyId : null;
      for (const c of node.children) {
        const found = findPty(c);
        if (found !== null) return found;
      }
      return null;
    };

    const ptyId = findPty(currentTab.splitLayout);
    if (ptyId !== null) {
      await invoke('kill_pty', { ptyId });
    }

    // await 之后重新读取最新状态，防止并发关闭时布局被覆盖
    const latestPs = useAppStore.getState().projectStates.get(projectId);
    const latestTab = latestPs?.tabs.find(t => t.id === latestPs.activeTabId);
    if (!latestTab) return;

    const newLayout = removePane(latestTab.splitLayout, paneId);
    if (newLayout) {
      updateTabLayout(projectId, latestTab.id, newLayout);
      saveLayoutToConfig(projectId);
    } else {
      handleCloseTab(latestTab.id);
    }
  }, [projectId, updateTabLayout, handleCloseTab]);

  const handleLayoutChange = useCallback((updatedNode: SplitNode) => {
    const currentPs = useAppStore.getState().projectStates.get(projectId);
    const currentActiveTab = currentPs?.tabs.find((t) => t.id === currentPs.activeTabId);
    if (!currentActiveTab) return;
    updateTabLayout(projectId, currentActiveTab.id, updatedNode);
    saveLayoutToConfig(projectId);
  }, [projectId, updateTabLayout]);

  return (
    <div className="flex flex-col h-full bg-[var(--bg-terminal)]">
      <TabBar projectId={projectId} onNewTab={handleNewTabClick} onCloseTab={handleCloseTab} />

      <div className="flex-1 overflow-hidden relative">
        {ps?.tabs.map((tab) => (
          <div
            key={tab.id}
            className="absolute inset-0"
            style={{ display: tab.id === ps.activeTabId ? 'block' : 'none' }}
          >
            <SplitLayout node={tab.splitLayout} onSplit={handleSplitPane} onClose={handleClosePane} onTabDrop={handleTabDrop} onLayoutChange={handleLayoutChange} />
          </div>
        ))}

        {(!ps || ps.tabs.length === 0) && (
          <div className="flex flex-col items-center justify-center h-full gap-3 text-[var(--text-muted)]">
            <div className="text-3xl opacity-20">⌘</div>
            <button
              className="px-5 py-2.5 border border-dashed border-[var(--border-default)] rounded-[var(--radius-md)] text-sm hover:border-[var(--accent)] hover:text-[var(--accent)] transition-all duration-200"
              onClick={handleNewTabClick}
            >
              + 新建终端
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
