import { useState, useRef, useEffect, useCallback } from 'react';
import { createPortal } from 'react-dom';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import { TerminalInstance } from './TerminalInstance';
import { StatusDot } from './StatusDot';
import { MarkerList } from './MarkerList';
import { showContextMenu } from '../utils/contextMenu';
import { disposeTerminal } from '../utils/terminalCache';
import { createProjectPty, isRemoteProject, remotePaneLabel } from '../utils/remoteProject';
import { findPaneById } from '../utils/layoutOps';
import {
  activatePane,
  closeLeaf,
  closePane,
  newTerminal,
  renamePane,
  splitPane,
} from '../utils/paneActions';
import { hotkeyLabel } from '../utils/hotkeys';
import { openTerminalSearch } from '../utils/terminalSearch';
import { MOD_LABEL } from '../utils/platform';
import { useT } from '../i18n';
import type { SplitNode, PaneState, ShellConfig, AiMarker } from '../types';

const EMPTY_MARKERS: AiMarker[] = [];
const hydratingPaneIds = new Set<string>();

// 分屏 / 关闭控件的图标：与 ActivityBar 同一套描边语言（16 viewBox、stroke currentColor），
// 取代原先的 ┃ ━ ✕ 字符 —— 字符既没有图形语义，字体不同还会跳。
const ICON_SPLIT_RIGHT = (
  <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinejoin="round">
    <rect x="2" y="3" width="12" height="10" rx="1.5" />
    <path d="M8 3v10" />
  </svg>
);
const ICON_SPLIT_DOWN = (
  <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinejoin="round">
    <rect x="2" y="3" width="12" height="10" rx="1.5" />
    <path d="M2 8h12" />
  </svg>
);
const ICON_CLOSE = (
  <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round">
    <path d="M4.5 4.5l7 7M11.5 4.5l-7 7" />
  </svg>
);
const ICON_SEARCH = (
  <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round">
    <circle cx="7" cy="7" r="4.2" />
    <path d="M10.2 10.2L14 14" />
  </svg>
);

interface Props {
  projectId: string;
  node: SplitNode & { type: 'leaf' };
  projectPath: string;
}

export function PaneGroup({ projectId, node, projectPath }: Props) {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const setPanePty = useAppStore((s) => s.setPanePty);
  const updatePaneStatusByPaneId = useAppStore((s) => s.updatePaneStatusByPaneId);
  // create_pty 失败时的错误详情(按 paneId 记录),远程断链 / 缺 ssh 客户端时展示明确原因
  const [spawnErrors, setSpawnErrors] = useState<Record<string, string>>({});

  const activePane = node.panes.find((p) => p.id === node.activePaneId) ?? node.panes[0];

  // SSH 远程项目:所有 pane 统一按远程方式启动(布局恢复亦然);
  // pane 显示名用连接名(恢复布局时 shellName 会被映射为本地 shell 名,不可信)。
  const project = config.projects.find((p) => p.id === projectId);
  const remote = isRemoteProject(project);
  const remoteLabel = project && remote ? remotePaneLabel(project) : undefined;
  const paneLabel = (pane: PaneState) => pane.customTitle || (remote ? remoteLabel! : pane.shellName);

  useEffect(() => {
    if (!activePane || activePane.ptyId !== undefined || activePane.status === 'error') return;
    if (hydratingPaneIds.has(activePane.id)) return;
    if (!project) return;

    let shell: ShellConfig | undefined;
    if (!remote) {
      shell = config.availableShells.find((s) => s.name === activePane.shellName)
        ?? config.availableShells.find((s) => s.name === config.defaultShell)
        ?? config.availableShells[0];
      if (!shell) {
        updatePaneStatusByPaneId(projectId, activePane.id, 'error');
        return;
      }
    }

    hydratingPaneIds.add(activePane.id);
    // 远程分支:create_pty 带 sshRemote,后端直接 spawn ssh 并预注册密码 autofill;
    // 本地分支:行为与既有链路一致(shell + cwd + envVars),pane 的 cwd 覆盖优先(worktree 终端)。
    createProjectPty(project, shell, activePane.cwd)
      .then((ptyId) => {
        const layout = useAppStore.getState().projectStates.get(projectId)?.layout;
        const pane = layout ? findPaneById(layout, activePane.id) : null;
        if (pane && pane.ptyId === undefined) {
          setPanePty(projectId, activePane.id, ptyId);
          setSpawnErrors((prev) => {
            if (!(activePane.id in prev)) return prev;
            const next = { ...prev };
            delete next[activePane.id];
            return next;
          });
        } else {
          invoke('kill_pty', { ptyId }).catch(() => {});
        }
      })
      .catch((e) => {
        setSpawnErrors((prev) => ({
          ...prev,
          [activePane.id]: e instanceof Error ? e.message : String(e),
        }));
        updatePaneStatusByPaneId(projectId, activePane.id, 'error');
      })
      .finally(() => {
        hydratingPaneIds.delete(activePane.id);
      });
  }, [
    activePane?.id,
    activePane?.ptyId,
    activePane?.shellName,
    activePane?.status,
    activePane?.cwd,
    config.availableShells,
    config.defaultShell,
    project,
    remote,
    projectId,
    projectPath,
    setPanePty,
    updatePaneStatusByPaneId,
  ]);

  const handleNewTabClick = useCallback((e: React.MouseEvent) => {
    // 远程项目不弹 shell 菜单:pane 固定为 ssh 启动器
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

  // 远程 pane 断线检测:ssh 进程退出(pty-exit,不区分用户 exit 与异常断线)后
  // pane 不自动关闭,叠加「连接已断开,点击重连」覆盖层。
  const exitedPtyIds = useAppStore((s) => s.exitedPtyIds);
  const showReconnect =
    remote && activePane?.ptyId !== undefined && exitedPtyIds.has(activePane.ptyId);

  // 重连:同一 pane 重新 create_pty(清屏方案 —— 销毁旧 xterm 实例,复用懒创建
  // effect 重建全新终端)。选清屏而非保留历史:新 PTY 的输出从头开始,旧 buffer 的
  // 光标/滚动状态与新会话无法衔接,保留反而会出现«半屏旧内容 + 新登录横幅»的错位;
  // 且 dispose 一并回收 markers/WebGL 资源,链路与关 tab 完全一致,无新状态机。
  const handleReconnect = useCallback(() => {
    if (!activePane || activePane.ptyId === undefined) return;
    const oldPtyId = activePane.ptyId;
    invoke('kill_pty', { ptyId: oldPtyId }).catch(() => {});
    disposeTerminal(oldPtyId);
    useAppStore.getState().clearMarkersForPty(oldPtyId);
    useAppStore.getState().clearPtyExited(oldPtyId);
    // 置 ptyId=undefined + status=idle → 懒创建 effect 走远程分支重新 spawn ssh
    useAppStore.getState().resetPaneForReconnect(projectId, activePane.id);
  }, [activePane, projectId]);

  const paneContextMenu = useCallback((e: React.MouseEvent, paneId: string) => {
    e.preventDefault();
    e.stopPropagation();
    showContextMenu(e.clientX, e.clientY, [
      { label: t('paneGroup.rename'), shortcut: hotkeyLabel('renamePane'), onClick: () => void renamePane(projectId, paneId) },
      { separator: true },
      { label: t('paneGroup.splitRight'), shortcut: hotkeyLabel('splitRight'), onClick: () => void splitPane(projectId, 'horizontal', paneId) },
      { label: t('paneGroup.splitDown'), shortcut: hotkeyLabel('splitDown'), onClick: () => void splitPane(projectId, 'vertical', paneId) },
      { separator: true },
      { label: t('paneGroup.closeTab'), onClick: () => void closePane(projectId, paneId) },
      { label: t('paneGroup.closePane'), shortcut: hotkeyLabel('closePane'), danger: true, onClick: () => void closeLeaf(projectId, paneId) },
    ]);
  }, [projectId, t]);

  if (!activePane) return null;

  const ctrlBtn =
    'w-6 h-6 flex items-center justify-center rounded-[var(--radius-sm)] text-[var(--text-muted)] opacity-60 hover:opacity-100 hover:text-[var(--accent)] hover:bg-[var(--border-subtle)] transition-all';

  return (
    <div className="w-full h-full flex flex-col" data-pty-id={activePane.ptyId}>
      {/* Tab bar */}
      <div
        data-panel-header
        className="flex items-stretch bg-[var(--bg-elevated)] border-b border-[var(--border-subtle)] text-xs overflow-x-auto select-none shrink-0"
        role="tablist"
        aria-label={t('paneGroup.tablistLabel')}
      >
        {node.panes.map((pane) => {
          const isActive = pane.id === activePane.id;
          return (
            <div
              key={pane.id}
              data-pane-tab
              role="tab"
              tabIndex={isActive ? 0 : -1}
              aria-selected={isActive}
              // 状态点 + 标题 + 关闭按钮作为一组在 tab 内居中：左右 padding 对称，
              // 再给一个最小宽度 —— 否则 tab 宽度完全由内容撑开，居中无从谈起，
              // 短标题（nushell）和长标题的 tab 也会宽窄不一显得毛糙。
              className={`flex items-center justify-center gap-1.5 px-3 py-[3px] min-w-[7.5rem] cursor-pointer whitespace-nowrap transition-all duration-100 relative ${
                isActive
                  ? 'bg-[var(--bg-terminal)] text-[var(--text-primary)]'
                  : 'text-[var(--text-muted)] hover:text-[var(--text-secondary)] hover:bg-[var(--border-subtle)]'
              }`}
              onClick={() => activatePane(projectId, pane.id)}
              onDoubleClick={() => void renamePane(projectId, pane.id)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' || e.key === ' ') {
                  e.preventDefault();
                  activatePane(projectId, pane.id);
                }
              }}
              onContextMenu={(e) => paneContextMenu(e, pane.id)}
            >
              {isActive && (
                <span className="absolute bottom-0 left-2 right-2 h-[2px] rounded-full bg-[var(--accent)]" />
              )}
              <StatusDot status={pane.status} />
              <span className="font-medium">{paneLabel(pane)}</span>
              <button
                type="button"
                className="ml-0.5 w-4 h-4 flex items-center justify-center rounded-[var(--radius-sm)] text-[var(--text-muted)] hover:text-[var(--color-error)] hover:bg-[var(--border-subtle)] transition-colors"
                title={t('paneGroup.closeTab')}
                aria-label={t('paneGroup.closeTabLabel', { label: paneLabel(pane) })}
                onClick={(e) => {
                  e.stopPropagation();
                  void closePane(projectId, pane.id);
                }}
              >
                <svg width="9" height="9" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round">
                  <path d="M4.5 4.5l7 7M11.5 4.5l-7 7" />
                </svg>
              </button>
            </div>
          );
        })}

        {/* 新建终端 */}
        <button
          type="button"
          className="px-2 text-[var(--text-muted)] hover:text-[var(--accent)] transition-colors"
          onClick={handleNewTabClick}
          title={`${t('terminalArea.newTerminal')} (${hotkeyLabel('newTerminal')})`}
          aria-label={t('terminalArea.newTerminal')}
        >
          <svg width="11" height="11" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round">
            <path d="M8 3.5v9M3.5 8h9" />
          </svg>
        </button>

        {/* 右侧控件：常驻低透明度（原先 opacity:0 全靠 hover，分屏功能基本无从发现） */}
        <div className="ml-auto flex items-center gap-0.5 px-1.5">
          {activePane.ptyId !== undefined && markers.length > 0 && (
            <button
              ref={markerBtnRef}
              type="button"
              className="mr-1 px-1.5 py-0.5 text-xs rounded-[var(--radius-sm)] text-[var(--text-muted)] hover:text-[var(--accent)] hover:bg-[var(--border-subtle)] flex items-center gap-1 transition-colors"
              onClick={() => (markerOpen ? setMarkerOpen(false) : openMarkerPopover())}
              title={t('paneGroup.markerTooltip', { mod: MOD_LABEL })}
              aria-expanded={markerOpen}
            >
              <span>⚑</span>
              <span className="tabular-nums">{markers.length}</span>
            </button>
          )}
          {activePane.ptyId !== undefined && (
            <button
              type="button"
              className={ctrlBtn}
              title={`${t('terminalSearch.title')} (${hotkeyLabel('terminalSearch')})`}
              aria-label={t('terminalSearch.title')}
              onClick={() => openTerminalSearch(activePane.ptyId!)}
            >
              {ICON_SEARCH}
            </button>
          )}
          <button
            type="button"
            className={ctrlBtn}
            title={`${t('paneGroup.splitRight')} (${hotkeyLabel('splitRight')})`}
            aria-label={t('paneGroup.splitRight')}
            onClick={() => void splitPane(projectId, 'horizontal', activePane.id)}
          >
            {ICON_SPLIT_RIGHT}
          </button>
          <button
            type="button"
            className={ctrlBtn}
            title={`${t('paneGroup.splitDown')} (${hotkeyLabel('splitDown')})`}
            aria-label={t('paneGroup.splitDown')}
            onClick={() => void splitPane(projectId, 'vertical', activePane.id)}
          >
            {ICON_SPLIT_DOWN}
          </button>
          <button
            type="button"
            className={`${ctrlBtn} hover:!text-[var(--color-error)]`}
            title={`${t('paneGroup.closePane')} (${hotkeyLabel('closePane')})`}
            aria-label={t('paneGroup.closePane')}
            onClick={() => void closeLeaf(projectId, activePane.id)}
          >
            {ICON_CLOSE}
          </button>
        </div>
      </div>

      {/* Active terminal */}
      <div className="flex-1 overflow-hidden relative">
        <div className="absolute inset-0">
          {activePane.ptyId !== undefined ? (
            <>
              <TerminalInstance
                ptyId={activePane.ptyId}
              />
              {/* 远程断线覆盖层:保留 pane,点击在同一 pane 重连 */}
              {showReconnect && (
                <div className="absolute inset-0 z-10 flex flex-col items-center justify-center gap-3 bg-black/55 backdrop-blur-[1px]">
                  <div className="text-sm text-[var(--text-secondary)]">
                    {t('paneGroup.remoteDisconnected')}
                  </div>
                  <button
                    type="button"
                    className="px-3 py-1.5 text-sm rounded-[var(--radius-sm)] border border-[var(--border-default)] text-[var(--text-secondary)] hover:border-[var(--accent)] hover:text-[var(--accent)] transition-colors"
                    onClick={handleReconnect}
                  >
                    {t('paneGroup.reconnect')}
                  </button>
                </div>
              )}
            </>
          ) : activePane.status === 'error' ? (
            <div className="h-full flex flex-col items-center justify-center gap-2 text-[var(--text-muted)] text-sm px-4">
              <div>{t('paneGroup.startFailed')}</div>
              {spawnErrors[activePane.id] && (
                <div className="text-xs text-[var(--color-error)] max-w-[80%] text-center break-all">
                  {spawnErrors[activePane.id]}
                </div>
              )}
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
