import { useState, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore, genId } from '../store';
import { TerminalInstance } from './TerminalInstance';
import { StatusDot } from './StatusDot';
import { showContextMenu } from '../utils/contextMenu';
import { disposeTerminal } from '../utils/terminalCache';
import type { SplitNode, PaneState, ShellConfig } from '../types';

interface Props {
  node: SplitNode & { type: 'leaf' };
  projectPath: string;
  onSplit: (paneId: string, direction: 'horizontal' | 'vertical') => void;
  onClosePane: () => void;
  onUpdateNode: (updated: SplitNode) => void;
}

export function PaneGroup({ node, projectPath, onSplit, onClosePane, onUpdateNode }: Props) {
  const config = useAppStore((s) => s.config);
  const [headerHover, setHeaderHover] = useState(false);

  const activePane = node.panes.find((p) => p.id === node.activePaneId) ?? node.panes[0];

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

    const newPane: PaneState = {
      id: genId(),
      shellName: shell.name,
      status: 'idle',
      ptyId,
    };

    onUpdateNode({
      ...node,
      panes: [...node.panes, newPane],
      activePaneId: newPane.id,
    });
  }, [config, projectPath, node, onUpdateNode]);

  const handleNewTabClick = useCallback((e: React.MouseEvent) => {
    if (config.availableShells.length <= 1) {
      handleNewTab();
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
  }, [config.availableShells, handleNewTab]);

  const handleCloseTab = useCallback(async (paneId: string) => {
    const pane = node.panes.find((p) => p.id === paneId);
    if (!pane) return;

    await invoke('kill_pty', { ptyId: pane.ptyId });
    disposeTerminal(pane.ptyId);

    const remaining = node.panes.filter((p) => p.id !== paneId);
    if (remaining.length === 0) {
      onClosePane();
      return;
    }

    const newActive = node.activePaneId === paneId
      ? (remaining[remaining.length - 1]?.id ?? remaining[0].id)
      : node.activePaneId;

    onUpdateNode({
      ...node,
      panes: remaining,
      activePaneId: newActive,
    });
  }, [node, onClosePane, onUpdateNode]);

  const handleSetActive = useCallback((paneId: string) => {
    if (paneId !== node.activePaneId) {
      onUpdateNode({ ...node, activePaneId: paneId });
    }
  }, [node, onUpdateNode]);

  const handleClosePaneGroup = useCallback(async () => {
    // Kill all PTYs in this leaf before removing it
    for (const pane of node.panes) {
      await invoke('kill_pty', { ptyId: pane.ptyId });
      disposeTerminal(pane.ptyId);
    }
    onClosePane();
  }, [node.panes, onClosePane]);

  if (!activePane) return null;

  return (
    <div className="w-full h-full flex flex-col">
      {/* Tab bar */}
      <div
        className="flex bg-[var(--bg-elevated)] border-b border-[var(--border-subtle)] text-[11px] overflow-x-auto select-none shrink-0"
        style={{ WebkitAppRegion: 'no-drag' } as React.CSSProperties}
        onMouseEnter={() => setHeaderHover(true)}
        onMouseLeave={() => setHeaderHover(false)}
      >
        {node.panes.map((pane) => {
          const isActive = pane.id === activePane.id;
          return (
            <div
              key={pane.id}
              className={`flex items-center gap-1.5 px-3 py-[3px] cursor-pointer whitespace-nowrap transition-all duration-100 relative ${
                isActive
                  ? 'bg-[var(--bg-terminal)] text-[var(--text-primary)]'
                  : 'text-[var(--text-muted)] hover:text-[var(--text-secondary)] hover:bg-[var(--border-subtle)]'
              }`}
              onClick={() => handleSetActive(pane.id)}
            >
              {isActive && (
                <span className="absolute bottom-0 left-2 right-2 h-[2px] rounded-full bg-[var(--accent)]" />
              )}
              <StatusDot status={pane.status} />
              <span className="font-medium">{pane.shellName}</span>
              <span
                className="ml-0.5 text-[var(--text-muted)] hover:text-[var(--color-error)] text-[12px] transition-colors"
                onClick={(e) => {
                  e.stopPropagation();
                  handleCloseTab(pane.id);
                }}
              >
                ✕
              </span>
            </div>
          );
        })}

        {/* "+" button */}
        <div
          className="px-2 py-[3px] text-[var(--text-muted)] cursor-pointer hover:text-[var(--accent)] transition-colors text-[12px]"
          onClick={handleNewTabClick}
        >
          +
        </div>

        {/* Right-aligned split/close controls (on hover) */}
        <div
          className="ml-auto flex items-center gap-0.5 px-2 text-[12px]"
        >
          <div
            className="flex items-center gap-0.5 transition-opacity duration-150"
            style={{ opacity: headerHover ? 1 : 0 }}
          >
            <span
              className="text-[var(--text-muted)] hover:text-[var(--accent)] cursor-pointer transition-colors px-0.5"
              title="Split right"
              onClick={() => onSplit(activePane.id, 'horizontal')}
            >
              ┃
            </span>
            <span
              className="text-[var(--text-muted)] hover:text-[var(--accent)] cursor-pointer transition-colors px-0.5"
              title="Split down"
              onClick={() => onSplit(activePane.id, 'vertical')}
            >
              ━
            </span>
            <span
              className="text-[var(--text-muted)] hover:text-[var(--color-error)] cursor-pointer transition-colors pl-0.5"
              title="Close pane"
              onClick={handleClosePaneGroup}
            >
              ✕
            </span>
          </div>
        </div>
      </div>

      {/* Active terminal */}
      <div className="flex-1 overflow-hidden relative">
        {node.panes.map((pane) => (
          <div
            key={pane.ptyId}
            className="absolute inset-0"
            style={{ display: pane.id === activePane.id ? 'block' : 'none' }}
          >
            <TerminalInstance
              ptyId={pane.ptyId}
            />
          </div>
        ))}
      </div>
    </div>
  );
}
