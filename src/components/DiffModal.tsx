import { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import type { GitFileStatus, GitDiffResult, DiffLine } from '../types';

interface DiffModalProps {
  open: boolean;
  onClose: () => void;
  projectPath: string;
  status: GitFileStatus;
}

type ViewMode = 'side-by-side' | 'inline';

// ─── InlineView ───

export function InlineView({ hunks }: { hunks: GitDiffResult['hunks'] }) {
  return (
    <div className="font-mono text-sm leading-6">
      {hunks.map((hunk, hi) => (
        <div key={hi}>
          {hunk.lines.map((line, li) => (
            <div
              key={li}
              className={`flex ${
                line.kind === 'add'
                  ? 'bg-[var(--diff-add-bg)]'
                  : line.kind === 'delete'
                  ? 'bg-[var(--diff-del-bg)]'
                  : ''
              }`}
            >
              <span className="w-12 text-right pr-2 text-[var(--text-muted)] select-none flex-shrink-0 opacity-50">
                {line.kind === 'add' ? '+' : line.kind === 'delete' ? '-' : (line.oldLineno ?? '')}
              </span>
              <span
                className={`flex-1 whitespace-pre px-2 ${
                  line.kind === 'add'
                    ? 'text-[var(--diff-add-text)]'
                    : line.kind === 'delete'
                    ? 'text-[var(--diff-del-text)]'
                    : 'text-[var(--text-primary)]'
                }`}
              >
                {line.content}
              </span>
            </div>
          ))}
        </div>
      ))}
    </div>
  );
}

// ─── SideBySideView ───

export function SideBySideView({ hunks }: { hunks: GitDiffResult['hunks'] }) {
  const rows: { left?: DiffLine; right?: DiffLine }[] = [];

  for (const hunk of hunks) {
    let i = 0;
    while (i < hunk.lines.length) {
      const line = hunk.lines[i];
      if (line.kind === 'context') {
        rows.push({ left: line, right: line });
        i++;
      } else if (line.kind === 'delete') {
        const deletes: DiffLine[] = [];
        while (i < hunk.lines.length && hunk.lines[i].kind === 'delete') {
          deletes.push(hunk.lines[i]);
          i++;
        }
        const adds: DiffLine[] = [];
        while (i < hunk.lines.length && hunk.lines[i].kind === 'add') {
          adds.push(hunk.lines[i]);
          i++;
        }
        const maxLen = Math.max(deletes.length, adds.length);
        for (let j = 0; j < maxLen; j++) {
          rows.push({
            left: deletes[j] ?? undefined,
            right: adds[j] ?? undefined,
          });
        }
      } else if (line.kind === 'add') {
        rows.push({ left: undefined, right: line });
        i++;
      } else {
        i++;
      }
    }
  }

  const renderCell = (line: DiffLine | undefined, side: 'left' | 'right') => {
    if (!line) {
      return (
        <div className="flex h-full bg-[var(--bg-base)] opacity-30">
          <span className="w-12 flex-shrink-0" />
          <span className="flex-1" />
        </div>
      );
    }
    const isAdd = line.kind === 'add';
    const isDel = line.kind === 'delete';
    return (
      <div
        className={`flex ${
          isAdd ? 'bg-[var(--diff-add-bg)]' : isDel ? 'bg-[var(--diff-del-bg)]' : ''
        }`}
      >
        <span className="w-12 text-right pr-2 text-[var(--text-muted)] select-none flex-shrink-0 opacity-50">
          {side === 'left' ? (line.oldLineno ?? '') : (line.newLineno ?? '')}
        </span>
        <span
          className={`flex-1 whitespace-pre px-2 ${
            isAdd ? 'text-[var(--diff-add-text)]' : isDel ? 'text-[var(--diff-del-text)]' : 'text-[var(--text-primary)]'
          }`}
        >
          {line.content}
        </span>
      </div>
    );
  };

  return (
    <div className="flex font-mono text-sm leading-6 h-full">
      <div className="flex-1 overflow-auto border-r border-[var(--border-subtle)]">
        {rows.map((row, i) => (
          <div key={i}>{renderCell(row.left, 'left')}</div>
        ))}
      </div>
      <div className="flex-1 overflow-auto">
        {rows.map((row, i) => (
          <div key={i}>{renderCell(row.right, 'right')}</div>
        ))}
      </div>
    </div>
  );
}

// ─── DiffModal ───

export function DiffModal({ open, onClose, projectPath, status }: DiffModalProps) {
  const [viewMode, setViewMode] = useState<ViewMode>('side-by-side');
  const [diffResult, setDiffResult] = useState<GitDiffResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');

  useEffect(() => {
    if (!open) return;
    setLoading(true);
    setError('');
    setDiffResult(null);

    invoke<GitDiffResult>('get_git_diff', {
      projectPath,
      filePath: status.path,
    })
      .then(setDiffResult)
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }, [open, projectPath, status.path]);

  useEffect(() => {
    if (!open) return;
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', handleKeyDown);
    return () => window.removeEventListener('keydown', handleKeyDown);
  }, [open, onClose]);

  if (!open) return null;

  const fileName = status.path.split('/').pop() ?? status.path;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center" onClick={onClose}>
      <div className="absolute inset-0 bg-black/50 backdrop-blur-sm" />
      <div
        className="relative flex flex-col overflow-hidden bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-[var(--shadow-overlay)] animate-slide-in"
        style={{ width: '90vw', height: '80vh' }}
        onClick={(e) => e.stopPropagation()}
      >
        {/* 工具栏 */}
        <div className="flex items-center justify-between px-4 py-3 border-b border-[var(--border-subtle)] flex-shrink-0">
          <div className="flex items-center gap-2">
            <span className="text-base font-medium text-[var(--accent)]">{fileName}</span>
            <span className="text-sm text-[var(--text-muted)] truncate max-w-[300px]">
              {status.path}
            </span>
            <span className="px-2 py-0.5 text-xs rounded bg-[var(--bg-elevated)] text-[var(--text-muted)] border border-[var(--border-subtle)]">
              {status.statusLabel}
            </span>
          </div>
          <div className="flex items-center gap-2">
            <div className="flex rounded-[var(--radius-sm)] border border-[var(--border-default)] overflow-hidden">
              <button
                className={`px-3 py-1 text-sm transition-colors ${
                  viewMode === 'side-by-side'
                    ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
                    : 'text-[var(--text-muted)] hover:text-[var(--text-primary)]'
                }`}
                onClick={() => setViewMode('side-by-side')}
              >
                并排
              </button>
              <button
                className={`px-3 py-1 text-sm transition-colors ${
                  viewMode === 'inline'
                    ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
                    : 'text-[var(--text-muted)] hover:text-[var(--text-primary)]'
                }`}
                onClick={() => setViewMode('inline')}
              >
                内联
              </button>
            </div>
            <button
              className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-lg leading-none ml-2"
              onClick={onClose}
            >
              ✕
            </button>
          </div>
        </div>

        {/* 内容区 */}
        <div className="flex-1 overflow-auto bg-[var(--bg-base)]">
          {loading && (
            <div className="flex items-center justify-center h-full text-[var(--text-muted)]">
              加载中...
            </div>
          )}
          {error && (
            <div className="flex items-center justify-center h-full text-[var(--color-error)]">
              {error}
            </div>
          )}
          {diffResult && diffResult.isBinary && (
            <div className="flex items-center justify-center h-full text-[var(--text-muted)]">
              二进制文件，不支持 diff 预览
            </div>
          )}
          {diffResult && diffResult.tooLarge && (
            <div className="flex items-center justify-center h-full text-[var(--text-muted)]">
              文件过大（&gt;1MB），不支持 diff 预览
            </div>
          )}
          {diffResult && !diffResult.isBinary && !diffResult.tooLarge && (
            viewMode === 'side-by-side'
              ? <SideBySideView hunks={diffResult.hunks} />
              : <InlineView hunks={diffResult.hunks} />
          )}
        </div>
      </div>
    </div>
  );
}
