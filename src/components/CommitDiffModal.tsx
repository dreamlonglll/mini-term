import { useState, useEffect, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { InlineView, SideBySideView } from './DiffModal';
import { useAppStore } from '../store';
import { Modal } from './Modal';
import { useT } from '../i18n';
import type { CommitFileInfo, GitDiffResult } from '../types';

interface CommitDiffModalProps {
  open: boolean;
  onClose: () => void;
  repoPath: string;
  commitHash: string;
  commitMessage: string;
  files: CommitFileInfo[];
}

type ViewMode = 'side-by-side' | 'inline';

const STATUS_LABELS: Record<string, { text: string; color: string }> = {
  added: { text: 'A', color: 'text-[var(--color-success)]' },
  modified: { text: 'M', color: 'text-[var(--color-warning)]' },
  deleted: { text: 'D', color: 'text-[var(--color-error)]' },
  renamed: { text: 'R', color: 'text-[var(--color-info)]' },
};

export function CommitDiffModal({
  open,
  onClose,
  repoPath,
  commitHash,
  commitMessage,
  files,
}: CommitDiffModalProps) {
  const t = useT();
  const [viewMode, setViewMode] = useState<ViewMode>('side-by-side');
  const [selectedFile, setSelectedFile] = useState<string>(files[0]?.path ?? '');
  const terminalFontSize = useAppStore((s) => s.config.terminalFontSize) || 14;
  const [diffResult, setDiffResult] = useState<GitDiffResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');

  const loadDiff = useCallback(
    async (filePath: string) => {
      if (!filePath) return;
      setLoading(true);
      setError('');
      setDiffResult(null);
      const fileInfo = files.find((f) => f.path === filePath);
      try {
        const result = await invoke<GitDiffResult>('get_commit_file_diff', {
          repoPath,
          commitHash,
          filePath,
          oldFilePath: fileInfo?.oldPath ?? null,
        });
        setDiffResult(result);
      } catch (e) {
        setError(String(e));
      } finally {
        setLoading(false);
      }
    },
    [repoPath, commitHash, files],
  );

  useEffect(() => {
    if (open && selectedFile) {
      loadDiff(selectedFile);
    }
  }, [open, selectedFile, loadDiff]);

  useEffect(() => {
    if (open && files.length > 0 && !files.find((f) => f.path === selectedFile)) {
      setSelectedFile(files[0].path);
    }
  }, [open, files, selectedFile]);

  if (!open) return null;

  const shortHash = commitHash.slice(0, 7);

  return (
    <Modal open={open} onClose={onClose} align="center" ariaLabel={commitMessage}
      panelClassName="w-[92vw] h-[85vh] select-text flex-row">
        {/* 左侧文件列表 */}
        <div className="w-56 flex-shrink-0 border-r border-[var(--border-subtle)] flex flex-col bg-[var(--bg-elevated)]">
          <div className="px-3 py-3 border-b border-[var(--border-subtle)]">
            <div className="text-sm font-medium text-[var(--accent)] truncate">
              {commitMessage}
            </div>
            <div className="text-xs text-[var(--text-muted)] mt-1 font-mono">{shortHash}</div>
          </div>
          <div className="flex-1 overflow-y-auto">
            {files.map((file) => {
              const label = STATUS_LABELS[file.status] ?? { text: '?', color: 'text-[var(--text-muted)]' };
              const fileName = file.path.split('/').pop() ?? file.path;
              const isSelected = file.path === selectedFile;
              return (
                <div
                  key={file.path}
                  className={`flex items-center gap-2 px-3 py-1.5 cursor-pointer text-sm transition-colors ${
                    isSelected
                      ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
                      : 'text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
                  }`}
                  onClick={() => setSelectedFile(file.path)}
                  title={file.path}
                >
                  <span className={`text-xs font-bold flex-shrink-0 ${label.color}`}>
                    {label.text}
                  </span>
                  <span className="truncate">{fileName}</span>
                </div>
              );
            })}
          </div>
          <div className="px-3 py-2 border-t border-[var(--border-subtle)] text-xs text-[var(--text-muted)]">
            {t("commitDiff.fileCount", { count: files.length })}
          </div>
        </div>

        {/* 右侧 diff 区域 */}
        <div className="flex-1 flex flex-col">
          <div className="flex items-center justify-between px-4 py-3 border-b border-[var(--border-subtle)] flex-shrink-0">
            <div className="flex items-center gap-2">
              <span className="text-sm text-[var(--text-primary)] truncate max-w-[400px]">
                {selectedFile}
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
                  {t("commitDiff.sideBySide")}
                </button>
                <button
                  className={`px-3 py-1 text-sm transition-colors ${
                    viewMode === 'inline'
                      ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
                      : 'text-[var(--text-muted)] hover:text-[var(--text-primary)]'
                  }`}
                  onClick={() => setViewMode('inline')}
                >
                  {t("commitDiff.inline")}
                </button>
              </div>
              <button
                className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-lg leading-none ml-2"
                onClick={onClose}
              >
                &#10005;
              </button>
            </div>
          </div>

          <div className="flex-1 overflow-auto bg-[var(--bg-base)]">
            {loading && (
              <div className="flex items-center justify-center h-full text-[var(--text-muted)]">
                {t("commitDiff.loading")}
              </div>
            )}
            {error && (
              <div className="flex items-center justify-center h-full text-[var(--color-error)]">
                {error}
              </div>
            )}
            {diffResult && diffResult.isBinary && (
              <div className="flex items-center justify-center h-full text-[var(--text-muted)]">
                {t("commitDiff.binaryFile")}
              </div>
            )}
            {diffResult && diffResult.tooLarge && (
              <div className="flex items-center justify-center h-full text-[var(--text-muted)]">
                {t("commitDiff.tooLarge")}
              </div>
            )}
            {diffResult && !diffResult.isBinary && !diffResult.tooLarge && (
              viewMode === 'side-by-side'
                ? <SideBySideView hunks={diffResult.hunks} fontSize={terminalFontSize} />
                : <InlineView hunks={diffResult.hunks} fontSize={terminalFontSize} />
            )}
            {!loading && !error && !diffResult && files.length === 0 && (
              <div className="flex items-center justify-center h-full text-[var(--text-muted)]">
                {t("commitDiff.noChanges")}
              </div>
            )}
          </div>
        </div>
    </Modal>
  );
}
