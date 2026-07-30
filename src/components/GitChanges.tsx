import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { ask } from '@tauri-apps/plugin-dialog';
import { useT } from '../i18n';
import { useAppStore } from '../store';
import { useTauriEvent } from '../hooks/useTauriEvent';
import { showContextMenu } from '../utils/contextMenu';
import { isAiPty } from '../utils/terminalCache';
import { DiffModal } from './DiffModal';
import type { ChangeFileStatus, PtyOutputPayload } from '../types';

interface GitChangesProps {
  projectPath: string;
  repoPath: string;
  onCommitSuccess: () => void;
}

const GIT_REFRESH_PATTERNS = [
  /create mode/,
  /Switched to/,
  /Already up to date/,
  /insertions?\(\+\)/,
  /deletions?\(-\)/,
];

// --- Tree view helpers ---

interface FileTreeNode {
  name: string;
  fullPath: string;
  file?: ChangeFileStatus;
  children: FileTreeNode[];
}

function buildFileTree(files: ChangeFileStatus[]): FileTreeNode[] {
  const root: FileTreeNode[] = [];
  for (const file of files) {
    const parts = file.path.split('/');
    let current = root;
    let pathSoFar = '';
    for (let i = 0; i < parts.length; i++) {
      pathSoFar += (i > 0 ? '/' : '') + parts[i];
      const isLast = i === parts.length - 1;
      if (isLast) {
        current.push({ name: parts[i], fullPath: pathSoFar, file, children: [] });
      } else {
        let dir = current.find((n) => n.name === parts[i] && !n.file);
        if (!dir) {
          dir = { name: parts[i], fullPath: pathSoFar, children: [] };
          current.push(dir);
        }
        current = dir.children;
      }
    }
  }
  return root;
}

function statusLabelFor(status?: string): string {
  switch (status) {
    case 'modified': return 'M';
    case 'added': return 'A';
    case 'deleted': return 'D';
    case 'renamed': return 'R';
    case 'untracked': return '?';
    case 'conflicted': return 'C';
    default: return ' ';
  }
}

function statusColor(_file: ChangeFileStatus, area: string): string {
  const status = area === 'staged' ? _file.stagedStatus : _file.unstagedStatus;
  switch (status) {
    case 'modified': return 'text-[var(--color-warning,#e5c07b)]';
    case 'added': return 'text-[var(--color-success,#98c379)]';
    case 'deleted': return 'text-[var(--color-error,#e06c75)]';
    case 'renamed': return 'text-[var(--color-info,#61afef)]';
    case 'untracked': return 'text-[var(--color-success,#98c379)]';
    default: return 'text-[var(--text-muted)]';
  }
}

// --- Main component ---

export function GitChanges({ projectPath: _projectPath, repoPath, onCommitSuccess }: GitChangesProps) {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);

  const [changes, setChanges] = useState<ChangeFileStatus[]>([]);
  const [loading, setLoading] = useState(false);
  const viewMode = config.gitChangesViewMode ?? 'list';

  const [commitMsg, setCommitMsg] = useState('');
  const [committing, setCommitting] = useState(false);

  const [diffModal, setDiffModal] = useState<{
    open: boolean;
    filePath: string;
    staged: boolean;
    statusLabel: string;
  } | null>(null);

  const [collapsedDirs, setCollapsedDirs] = useState<Set<string>>(new Set());

  // Grouping
  const staged = changes.filter((c) => c.stagedStatus);
  const unstaged = changes.filter((c) => c.unstagedStatus && c.unstagedStatus !== 'untracked');
  const untracked = changes.filter((c) => c.unstagedStatus === 'untracked');

  // Load changes
  const loadChanges = useCallback(() => {
    if (!repoPath) return;
    setLoading(true);
    invoke<ChangeFileStatus[]>('get_changes_status', { repoPath })
      .then(setChanges)
      .catch(() => setChanges([]))
      .finally(() => setLoading(false));
  }, [repoPath]);

  useEffect(() => {
    loadChanges();
  }, [loadChanges]);

  // PTY output listener for auto-refresh
  const refreshTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const debouncedRefresh = useCallback(() => {
    if (refreshTimerRef.current) clearTimeout(refreshTimerRef.current);
    refreshTimerRef.current = setTimeout(loadChanges, 500);
  }, [loadChanges]);

  useTauriEvent<PtyOutputPayload>(
    'pty-output',
    useCallback(
      (payload: PtyOutputPayload) => {
        if (isAiPty(payload.ptyId)) return;
        if (GIT_REFRESH_PATTERNS.some((p) => p.test(payload.data))) {
          debouncedRefresh();
        }
      },
      [debouncedRefresh],
    ),
  );

  // --- Action handlers ---

  const handleStage = useCallback(async (files: string[]) => {
    try {
      await invoke('git_stage', { repoPath, files });
      loadChanges();
    } catch (e) {
      console.error('stage failed:', e);
    }
  }, [repoPath, loadChanges]);

  const handleUnstage = useCallback(async (files: string[]) => {
    try {
      await invoke('git_unstage', { repoPath, files });
      loadChanges();
    } catch (e) {
      console.error('unstage failed:', e);
    }
  }, [repoPath, loadChanges]);

  const handleStageAll = useCallback(async () => {
    try {
      await invoke('git_stage_all', { repoPath });
      loadChanges();
    } catch (e) {
      console.error('stage all failed:', e);
    }
  }, [repoPath, loadChanges]);

  const handleUnstageAll = useCallback(async () => {
    try {
      await invoke('git_unstage_all', { repoPath });
      loadChanges();
    } catch (e) {
      console.error('unstage all failed:', e);
    }
  }, [repoPath, loadChanges]);

  const handleCommit = useCallback(async () => {
    if (!commitMsg.trim() || staged.length === 0) return;
    setCommitting(true);
    try {
      await invoke('git_commit', { repoPath, message: commitMsg.trim() });
      setCommitMsg('');
      loadChanges();
      onCommitSuccess();
    } catch (e) {
      console.error('commit failed:', e);
    } finally {
      setCommitting(false);
    }
  }, [repoPath, commitMsg, staged.length, loadChanges, onCommitSuccess]);

  const handleDiscard = useCallback(async (files: string[]) => {
    const confirmed = await ask(
      t('gitChanges.discardConfirm', { count: files.length }),
      { title: t('gitChanges.discardTitle'), kind: 'warning', okLabel: t('gitChanges.discardOk'), cancelLabel: t('gitChanges.discardCancel') },
    );
    if (!confirmed) return;
    try {
      await invoke('git_discard_file', { repoPath, files });
      loadChanges();
    } catch (e) {
      console.error('discard failed:', e);
    }
  }, [repoPath, loadChanges]);

  const handleViewDiff = useCallback((filePath: string, isStaged: boolean, statusLabel: string) => {
    setDiffModal({ open: true, filePath, staged: isStaged, statusLabel });
  }, []);

  const toggleViewMode = useCallback(() => {
    const next = viewMode === 'list' ? 'tree' : 'list';
    setConfig({ ...config, gitChangesViewMode: next });
  }, [viewMode, config, setConfig]);

  // --- Render helpers ---

  const renderFileRow = (
    file: ChangeFileStatus,
    area: 'staged' | 'unstaged' | 'untracked',
    displayName: string,
    depth: number = 0,
  ) => {
    const isStaged = area === 'staged';
    const statusChar = isStaged
      ? statusLabelFor(file.stagedStatus)
      : statusLabelFor(file.unstagedStatus);

    return (
      <div
        key={`${area}-${file.path}`}
        className="group flex items-center justify-between py-1 px-2 hover:bg-[var(--border-subtle)] rounded-[var(--radius-sm)] cursor-pointer text-sm"
        style={{ paddingLeft: `${depth * 16 + 8}px` }}
        onClick={() => handleViewDiff(file.path, isStaged, statusChar)}
        onContextMenu={(e) => {
          e.preventDefault();
          const sep = { separator: true as const };
          const items: Parameters<typeof showContextMenu>[2] = [
            { label: t('gitChanges.contextViewDiff'), onClick: () => handleViewDiff(file.path, isStaged, statusChar) },
            sep,
            ...(isStaged
              ? [{ label: t('panels.unstage'), onClick: () => handleUnstage([file.path]) }]
              : [{ label: t('panels.stage'), onClick: () => handleStage([file.path]) }]),
            ...(area !== 'staged'
              ? [sep, { label: t('gitChanges.contextDiscard'), onClick: () => handleDiscard([file.path]) }]
              : []),
          ];
          showContextMenu(e.clientX, e.clientY, items);
        }}
      >
        <div className="flex items-center gap-1.5 min-w-0">
          <span className={`shrink-0 text-xs font-mono w-4 text-center ${statusColor(file, area)}`}>
            {statusChar}
          </span>
          <span className="truncate" title={file.path}>
            {displayName}
          </span>
        </div>
        <button
          className="shrink-0 w-5 h-5 flex items-center justify-center text-sm opacity-0 group-hover:opacity-100 text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-opacity"
          title={isStaged ? t('panels.unstage') : t('panels.stage')}
          aria-label={isStaged ? t('panels.unstage') : t('panels.stage')}
          onClick={(e) => {
            e.stopPropagation();
            isStaged ? handleUnstage([file.path]) : handleStage([file.path]);
          }}
        >
          {isStaged ? '−' : '+'}
        </button>
      </div>
    );
  };

  const renderTreeNode = (node: FileTreeNode, area: 'staged' | 'unstaged' | 'untracked', depth: number) => {
    if (node.file) {
      return renderFileRow(node.file, area, node.name, depth);
    }
    const isCollapsed = collapsedDirs.has(`${area}:${node.fullPath}`);
    return (
      <div key={`dir-${area}-${node.fullPath}`}>
        <div
          className="flex items-center gap-1 py-0.5 px-2 text-sm text-[var(--text-muted)] cursor-pointer hover:bg-[var(--border-subtle)] rounded-[var(--radius-sm)]"
          style={{ paddingLeft: `${depth * 16 + 8}px` }}
          onClick={() => {
            const key = `${area}:${node.fullPath}`;
            setCollapsedDirs((prev) => {
              const next = new Set(prev);
              if (next.has(key)) next.delete(key); else next.add(key);
              return next;
            });
          }}
        >
          <span className="text-sm w-3 text-center" style={{
            transform: isCollapsed ? 'rotate(-90deg)' : 'rotate(0deg)',
            display: 'inline-block',
            transition: 'transform 150ms',
          }}>▾</span>
          <span>{node.name}</span>
        </div>
        {!isCollapsed && node.children.map((child) => renderTreeNode(child, area, depth + 1))}
      </div>
    );
  };

  const renderFiles = (files: ChangeFileStatus[], area: 'staged' | 'unstaged' | 'untracked') => {
    if (viewMode === 'tree') {
      const tree = buildFileTree(files);
      return tree.map((node) => renderTreeNode(node, area, 0));
    }
    return files.map((f) => renderFileRow(f, area, f.path));
  };

  const renderGroup = (
    title: string,
    files: ChangeFileStatus[],
    area: 'staged' | 'unstaged' | 'untracked',
    action?: { label: string; onClick: () => void },
  ) => {
    if (files.length === 0) return null;
    return (
      <div className="mb-2">
        <div className="flex items-center justify-between px-2 py-1">
          <span className="text-xs text-[var(--text-muted)] uppercase tracking-wider font-medium">
            {title} ({files.length})
          </span>
          {action && (
            <button
              className="text-xs text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors"
              onClick={action.onClick}
            >
              {action.label}
            </button>
          )}
        </div>
        {renderFiles(files, area)}
      </div>
    );
  };

  // Build a GitFileStatus-compatible object for DiffModal
  const diffModalStatus = diffModal
    ? {
        path: diffModal.filePath,
        status: 'modified' as const,
        statusLabel: diffModal.statusLabel,
      }
    : null;

  // --- JSX ---

  return (
    <div className="h-full flex flex-col">
      {/* Toolbar */}
      <div className="flex items-center justify-between px-3 py-1.5 flex-shrink-0">
        <button
          className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-sm"
          onClick={loadChanges}
          title={t('gitChanges.refresh')}
        >
          ↻
        </button>
        <button
          className="text-xs text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors"
          onClick={toggleViewMode}
          title={viewMode === 'list' ? t('gitChanges.switchToTree') : t('gitChanges.switchToList')}
        >
          {viewMode === 'list' ? '⊞' : '≡'}
        </button>
      </div>

      {/* File list */}
      <div className="flex-1 overflow-y-auto px-1">
        {loading && changes.length === 0 && (
          <div className="text-center text-[var(--text-muted)] text-sm py-6">{t('gitChanges.loading')}</div>
        )}

        {!loading && changes.length === 0 && (
          <div className="text-center text-[var(--text-muted)] text-sm py-6">{t('gitChanges.empty')}</div>
        )}

        {renderGroup(t('panels.stagedChanges'), staged, 'staged', {
          label: t('gitChanges.unstageAll'),
          onClick: handleUnstageAll,
        })}
        {renderGroup(t('panels.unstagedChanges'), unstaged, 'unstaged', {
          label: t('gitChanges.stageAll'),
          onClick: handleStageAll,
        })}
        {renderGroup(t('panels.untrackedFiles'), untracked, 'untracked', {
          label: t('gitChanges.stageAll'),
          onClick: handleStageAll,
        })}
      </div>

      {/* Commit area */}
      <div className="flex-shrink-0 border-t border-[var(--border-subtle)] p-2">
        <textarea
          className="w-full text-sm bg-[var(--bg-base)] text-[var(--text-primary)] border border-[var(--border-default)] rounded px-2 py-1.5 resize-none placeholder:text-[var(--text-muted)] select-text"
          rows={3}
          placeholder={t('panels.commitPlaceholder')}
          value={commitMsg}
          onChange={(e) => setCommitMsg(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
              handleCommit();
            }
          }}
        />
        <button
          className={`w-full mt-1.5 py-1.5 text-sm rounded font-medium transition-colors ${
            commitMsg.trim() && staged.length > 0 && !committing
              ? 'bg-[var(--accent)] text-white hover:opacity-90 cursor-pointer'
              : 'bg-[var(--bg-elevated)] text-[var(--text-muted)] cursor-not-allowed'
          }`}
          disabled={!commitMsg.trim() || staged.length === 0 || committing}
          onClick={handleCommit}
        >
          {committing ? t('gitChanges.committing') : t('panels.commit', { count: staged.length })}
        </button>
      </div>

      {/* Diff Modal */}
      {diffModal && diffModalStatus && repoPath && (
        <DiffModal
          open={diffModal.open}
          onClose={() => setDiffModal(null)}
          projectPath={repoPath}
          status={diffModalStatus}
          staged={diffModal.staged}
        />
      )}
    </div>
  );
}
