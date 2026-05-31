import { useState, useRef, useEffect, useCallback } from 'react';
import { createPortal } from 'react-dom';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore, genId } from '../store';
import { TerminalInstance } from './TerminalInstance';
import { StatusDot } from './StatusDot';
import { MarkerList } from './MarkerList';
import { showContextMenu } from '../utils/contextMenu';
import { showConfirm, showPrompt } from '../utils/prompt';
import { disposeTerminal } from '../utils/terminalCache';
import { getProjectEnvs } from '../utils/projectEnv';
import { MOD_LABEL } from '../utils/platform';
import { useT } from '../i18n';
import type { SplitNode, PaneState, ShellConfig, AiMarker } from '../types';

const EMPTY_MARKERS: AiMarker[] = [];
const hydratingPaneIds = new Set<string>();

function findPaneById(node: SplitNode, paneId: string): PaneState | null {
  if (node.type === 'leaf') {
    return node.panes.find((pane) => pane.id === paneId) ?? null;
  }
  for (const child of node.children) {
    const found = findPaneById(child, paneId);
    if (found) return found;
  }
  return null;
}

interface Props {
  projectId: string;
  node: SplitNode & { type: 'leaf' };
  projectPath: string;
  onSplit: (paneId: string, direction: 'horizontal' | 'vertical') => void;
  onClosePane: () => void;
  onUpdateNode: (updated: SplitNode) => void;
}

export function PaneGroup({ projectId, node, projectPath, onSplit, onClosePane, onUpdateNode }: Props) {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const setPanePty = useAppStore((s) => s.setPanePty);
  const updatePaneStatusByPaneId = useAppStore((s) => s.updatePaneStatusByPaneId);
  const [headerHover, setHeaderHover] = useState(false);

  const activePane = node.panes.find((p) => p.id === node.activePaneId) ?? node.panes[0];

  useEffect(() => {
    if (!activePane || activePane.ptyId !== undefined || activePane.status === 'error') return;
    if (hydratingPaneIds.has(activePane.id)) return;

    const shell = config.availableShells.find((s) => s.name === activePane.shellName)
      ?? config.availableShells.find((s) => s.name === config.defaultShell)
      ?? config.availableShells[0];
    if (!shell) {
      updatePaneStatusByPaneId(projectId, activePane.id, 'error');
      return;
    }

    hydratingPaneIds.add(activePane.id);
    invoke<number>('create_pty', {
      shell: shell.command,
      args: shell.args ?? [],
      cwd: projectPath,
      envs: getProjectEnvs(projectId),
    })
      .then((ptyId) => {
        const ps = useAppStore.getState().projectStates.get(projectId);
        const pane = ps?.tabs
          .map((tab) => findPaneById(tab.splitLayout, activePane.id))
          .find(Boolean);
        if (pane && pane.ptyId === undefined) {
          setPanePty(projectId, activePane.id, ptyId);
        } else {
          invoke('kill_pty', { ptyId }).catch(() => {});
        }
      })
      .catch(() => updatePaneStatusByPaneId(projectId, activePane.id, 'error'))
      .finally(() => {
        hydratingPaneIds.delete(activePane.id);
      });
  }, [
    activePane?.id,
    activePane?.ptyId,
    activePane?.shellName,
    activePane?.status,
    config.availableShells,
    config.defaultShell,
    projectId,
    projectPath,
    setPanePty,
    updatePaneStatusByPaneId,
  ]);

  const handleNewTab = useCallback(async (selectedShell?: ShellConfig) => {
    const shell = selectedShell
      ?? config.availableShells.find((s) => s.name === config.defaultShell)
      ?? config.availableShells[0];
    if (!shell) return;

    const ptyId = await invoke<number>('create_pty', {
      shell: shell.command,
      args: shell.args ?? [],
      cwd: projectPath,
      envs: getProjectEnvs(projectId),
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

    const label = pane.customTitle || pane.shellName;
    const hasAi = pane.status === 'ai-working' || pane.status === 'ai-idle';
    const title = hasAi ? t('paneGroup.closeAiTitle') : t('paneGroup.closeTerminalTitle');
    const message = hasAi
      ? t('paneGroup.closeTabAiMessage', { label })
      : t('paneGroup.closeTabMessage', { label });

    const confirmed = await showConfirm(title, message);
    if (!confirmed) return;

    if (pane.ptyId !== undefined) {
      await invoke('kill_pty', { ptyId: pane.ptyId });
      disposeTerminal(pane.ptyId);
      useAppStore.getState().clearMarkersForPty(pane.ptyId);
    }

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

  const handleRenameTab = useCallback(async (paneId: string) => {
    const pane = node.panes.find((p) => p.id === paneId);
    if (!pane) return;
    const newTitle = await showPrompt(t('paneGroup.renameTerminal'), pane.customTitle || pane.shellName);
    if (newTitle === null) return;
    onUpdateNode({
      ...node,
      panes: node.panes.map((p) =>
        p.id === paneId ? { ...p, customTitle: newTitle.trim() || undefined } : p
      ),
    });
  }, [node, onUpdateNode]);

  const handleSetActive = useCallback((paneId: string) => {
    if (paneId !== node.activePaneId) {
      onUpdateNode({ ...node, activePaneId: paneId });
    }
  }, [node, onUpdateNode]);

  const handleClosePaneGroup = useCallback(async () => {
    const aiCount = node.panes.filter(
      (p) => p.status === 'ai-working' || p.status === 'ai-idle'
    ).length;
    const title = aiCount > 0 ? t('paneGroup.closeAiTitle') : t('paneGroup.closeTerminalTitle');
    const message = aiCount > 0
      ? t('paneGroup.closeGroupAiMessage', { count: aiCount })
      : t('paneGroup.closeGroupMessage');

    const confirmed = await showConfirm(title, message);
    if (!confirmed) return;

    for (const pane of node.panes) {
      if (pane.ptyId !== undefined) {
        await invoke('kill_pty', { ptyId: pane.ptyId });
        disposeTerminal(pane.ptyId);
        useAppStore.getState().clearMarkersForPty(pane.ptyId);
      }
    }
    onClosePane();
  }, [node.panes, onClosePane]);

  const [markerOpen, setMarkerOpen] = useState(false);
  const [markerAnchor, setMarkerAnchor] = useState<{ top: number; right: number } | null>(null);
  const markers = useAppStore(
    (s) => (activePane?.ptyId !== undefined && s.markersByPty.get(activePane.ptyId)) || EMPTY_MARKERS,
  );
  const markerBtnRef = useRef<HTMLButtonElement>(null);
  const markerPopoverRef = useRef<HTMLDivElement>(null);

  const openMarkerPopover = useCallback(() => {
    const rect = markerBtnRef.current?.getBoundingClientRect();
    if (!rect) return;
    setMarkerAnchor({ top: rect.bottom + 4, right: window.innerWidth - rect.right });
    setMarkerOpen(true);
  }, []);

  useEffect(() => {
    if (!markerOpen) return;
    const onDoc = (e: MouseEvent) => {
      const target = e.target as Node;
      if (markerPopoverRef.current?.contains(target)) return;
      if (markerBtnRef.current?.contains(target)) return;
      setMarkerOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    return () => document.removeEventListener('mousedown', onDoc);
  }, [markerOpen]);

  useEffect(() => {
    setMarkerOpen(false);
  }, [activePane?.ptyId]);

  const handleRetryCreatePty = useCallback(() => {
    if (!activePane) return;
    updatePaneStatusByPaneId(projectId, activePane.id, 'idle');
  }, [activePane, projectId, updatePaneStatusByPaneId]);

  if (!activePane) return null;

  return (
    <div className="w-full h-full flex flex-col" data-pty-id={activePane.ptyId}>
      {/* Tab bar */}
      <div
        data-panel-header
        className="flex bg-[var(--bg-elevated)] border-b border-[var(--border-subtle)] text-[11px] overflow-x-auto select-none shrink-0"
        onMouseEnter={() => setHeaderHover(true)}
        onMouseLeave={() => setHeaderHover(false)}
      >
        {node.panes.map((pane) => {
          const isActive = pane.id === activePane.id;
          return (
            <div
              key={pane.id}
              data-pane-tab
              className={`flex items-center gap-1.5 px-3 py-[3px] cursor-pointer whitespace-nowrap transition-all duration-100 relative ${
                isActive
                  ? 'bg-[var(--bg-terminal)] text-[var(--text-primary)]'
                  : 'text-[var(--text-muted)] hover:text-[var(--text-secondary)] hover:bg-[var(--border-subtle)]'
              }`}
              onClick={() => handleSetActive(pane.id)}
              onContextMenu={(e) => {
                e.preventDefault();
                e.stopPropagation();
                showContextMenu(e.clientX, e.clientY, [
                  { label: t('paneGroup.rename'), onClick: () => handleRenameTab(pane.id) },
                ]);
              }}
            >
              {isActive && (
                <span className="absolute bottom-0 left-2 right-2 h-[2px] rounded-full bg-[var(--accent)]" />
              )}
              <StatusDot status={pane.status} />
              <span className="font-medium">{pane.customTitle || pane.shellName}</span>
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
          {activePane.ptyId !== undefined && markers.length > 0 && (
            <button
              ref={markerBtnRef}
              type="button"
              className="mr-1 px-1.5 py-0.5 text-[11px] rounded text-[var(--text-muted)] hover:text-[var(--accent)] hover:bg-[var(--border-subtle)] flex items-center gap-1 transition-colors"
              onClick={() => (markerOpen ? setMarkerOpen(false) : openMarkerPopover())}
              title={t('paneGroup.markerTooltip', { mod: MOD_LABEL })}
            >
              <span>⚑</span>
              <span className="tabular-nums">{markers.length}</span>
            </button>
          )}
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
        <div className="absolute inset-0">
          {activePane.ptyId !== undefined ? (
            <TerminalInstance
              ptyId={activePane.ptyId}
            />
          ) : activePane.status === 'error' ? (
            <div className="h-full flex flex-col items-center justify-center gap-2 text-[var(--text-muted)] text-sm">
              <div>{t('paneGroup.startFailed')}</div>
              <button
                type="button"
                className="px-3 py-1.5 rounded-[var(--radius-sm)] border border-[var(--border-default)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-colors"
                onClick={handleRetryCreatePty}
              >
                {t('paneGroup.retry')}
              </button>
            </div>
          ) : (
            <div className="h-full flex items-center justify-center text-[var(--text-muted)] text-sm">
              {t('paneGroup.starting')}
            </div>
          )}
        </div>
      </div>

      {activePane.ptyId !== undefined && markerOpen && markerAnchor && createPortal(
        <div
          ref={markerPopoverRef}
          className="fixed z-50 rounded-md border shadow-lg"
          style={{
            top: markerAnchor.top,
            right: markerAnchor.right,
            background: 'var(--bg-elevated)',
            borderColor: 'var(--border-subtle)',
          }}
        >
          <MarkerList
            ptyId={activePane.ptyId}
            markers={markers}
            onClose={() => setMarkerOpen(false)}
          />
        </div>,
        document.body,
      )}
    </div>
  );
}
