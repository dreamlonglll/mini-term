import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import { GitHistoryContent } from './GitHistoryContent';
import { GitChanges } from './GitChanges';
import { GitWorktreeModal } from './GitWorktreeModal';
import { getGitHistoryCache, setGitHistoryCache } from '../utils/projectDataCache';
import { useT } from '../i18n';
import type { GitRepoInfo } from '../types';

type GitTab = 'history' | 'changes';

export function GitHistory() {
  const t = useT();
  const activeProjectId = useAppStore((s) => s.activeProjectId);
  const config = useAppStore((s) => s.config);
  const project = config.projects.find((p) => p.id === activeProjectId);
  // SSH 远程项目:git 命令跑在本地,对远程路径无意义 → 整个面板显示占位(远程 Git 二期)
  const isRemote = !!project?.sshConnectionId;

  const [activeTab, setActiveTab] = useState<GitTab>('history');

  // 仓库选择器状态 — 提升到容器层，两个 tab 共享
  const [repos, setRepos] = useState<GitRepoInfo[]>(() => {
    return (project ? getGitHistoryCache(project.path) : undefined)?.repos ?? [];
  });
  const [selectedRepo, setSelectedRepo] = useState<string>(() => {
    return (project ? getGitHistoryCache(project.path) : undefined)?.selectedRepo ?? '';
  });
  const [repoDropdownOpen, setRepoDropdownOpen] = useState(false);
  const dropdownRef = useRef<HTMLDivElement>(null);

  const loadRepos = useCallback(() => {
    if (!project || isRemote) return;
    invoke<GitRepoInfo[]>('discover_git_repos', { projectPath: project.path })
      .then((r) => {
        setRepos(r);
        let nextRepo = '';
        setSelectedRepo((prev) => {
          nextRepo = (prev && r.some((repo) => repo.path === prev)) ? prev : (r[0]?.path ?? '');
          return nextRepo;
        });
        setGitHistoryCache(project.path, { repos: r, selectedRepo: nextRepo });
      })
      .catch(() => setRepos([]));
  }, [project?.path, isRemote]);

  useEffect(() => {
    if (!project) {
      setRepos([]);
      setSelectedRepo('');
      return;
    }
    const cached = getGitHistoryCache(project.path);
    if (cached) {
      setRepos(cached.repos);
      setSelectedRepo(cached.selectedRepo);
    } else {
      setRepos([]);
      setSelectedRepo('');
    }
    setRepoDropdownOpen(false);
    loadRepos();
  }, [project?.path]);

  // 点击外部关闭下拉
  useEffect(() => {
    if (!repoDropdownOpen) return;
    const handleClick = (e: MouseEvent) => {
      if (dropdownRef.current && !dropdownRef.current.contains(e.target as Node)) {
        setRepoDropdownOpen(false);
      }
    };
    document.addEventListener('mousedown', handleClick);
    return () => document.removeEventListener('mousedown', handleClick);
  }, [repoDropdownOpen]);

  // commit 成功后 Changes tab 通知容器刷新 History
  const [historyRefreshKey, setHistoryRefreshKey] = useState(0);
  const onCommitSuccess = useCallback(() => {
    setHistoryRefreshKey((k) => k + 1);
  }, []);

  // Worktree 管理弹窗(仓库行右键菜单进入);增删后刷新仓库列表
  const [worktreeRepo, setWorktreeRepo] = useState<string | null>(null);

  const selectedRepoInfo = repos.find((r) => r.path === selectedRepo);

  if (!project) {
    return (
      <div className="h-full bg-[var(--bg-surface)] flex items-center justify-center text-[var(--text-muted)] text-base">
        {t("gitHistory.selectProject")}
      </div>
    );
  }

  if (isRemote) {
    return (
      <div className="h-full bg-[var(--bg-surface)] flex items-center justify-center text-[var(--text-muted)] text-base border-t border-[var(--border-subtle)]">
        {t("gitHistory.remoteNotSupported")}
      </div>
    );
  }

  return (
    <div data-panel className="h-full bg-[var(--bg-surface)] flex flex-col border-t border-[var(--border-subtle)] select-none">
      {/* Tab 栏 */}
      <div className="flex items-center gap-0 px-3 pt-2 pb-0 flex-shrink-0">
        {(['history', 'changes'] as const).map((tab) => (
          <button
            key={tab}
            className={`px-3 py-1.5 text-sm font-medium transition-colors border-b-2 ${
              activeTab === tab
                ? 'text-[var(--accent)] border-[var(--accent)]'
                : 'text-[var(--text-muted)] border-transparent hover:text-[var(--text-primary)]'
            }`}
            onClick={() => setActiveTab(tab)}
          >
            {tab === 'history' ? t('panels.history') : t('panels.changes')}
          </button>
        ))}
      </div>

      {/* 仓库选择器（仅 Changes tab + 多仓库时显示） */}
      {activeTab === 'changes' && repos.length > 1 && (
        <div className="px-2 pt-2 pb-0 flex-shrink-0 relative" ref={dropdownRef}>
          <div
            className="flex items-center justify-between w-full py-[5px] px-2 cursor-pointer hover:bg-[var(--border-subtle)] rounded-[var(--radius-sm)] text-sm transition-colors duration-100 text-[var(--color-folder)]"
            onClick={() => setRepoDropdownOpen((v) => !v)}
          >
            <div className="flex items-center gap-1.5 min-w-0">
              <span
                className="text-base w-3 text-center text-[var(--text-muted)] transition-transform duration-150"
                style={{
                  transform: repoDropdownOpen ? 'rotate(0deg)' : 'rotate(-90deg)',
                  display: 'inline-block',
                }}
              >
                &#9662;
              </span>
              <span className="truncate font-medium">{selectedRepoInfo?.name ?? t("gitHistory.selectRepo")}</span>
              {selectedRepoInfo?.currentBranch && (
                <span className="shrink-0 text-sm leading-[18px] px-1.5 rounded font-mono text-[var(--text-muted)] bg-[var(--border-subtle)]">
                  {selectedRepoInfo.isWorktree ? '⎇ ' : ''}{selectedRepoInfo.currentBranch}
                </span>
              )}
            </div>
          </div>
          {repoDropdownOpen && (
            <div className="absolute left-2 right-2 z-20 mt-0.5 bg-[var(--bg-elevated)] border border-[var(--border-default)] rounded-[var(--radius-sm)] shadow-[var(--shadow-overlay)] overflow-hidden">
              {repos.map((r) => (
                <div
                  key={r.path}
                  className={`flex items-center gap-1.5 px-3 py-1.5 text-sm cursor-pointer transition-colors duration-100 ${
                    r.path === selectedRepo
                      ? 'bg-[var(--accent-subtle)] text-[var(--accent)]'
                      : 'text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
                  }`}
                  onClick={() => {
                    setSelectedRepo(r.path);
                    setRepoDropdownOpen(false);
                  }}
                >
                  <span className="truncate">{r.name}</span>
                  {r.currentBranch && (
                    <span className="shrink-0 text-sm leading-[18px] px-1.5 rounded font-mono text-[var(--text-muted)] bg-[var(--border-subtle)]">
                      {r.isWorktree ? '⎇ ' : ''}{r.currentBranch}
                    </span>
                  )}
                </div>
              ))}
            </div>
          )}
        </div>
      )}

      {/* Tab 内容 */}
      <div className="flex-1 overflow-hidden">
        {activeTab === 'history' ? (
          <GitHistoryContent
            key={historyRefreshKey}
            projectPath={project.path}
            repos={repos}
            refreshRepos={loadRepos}
            onOpenWorktrees={setWorktreeRepo}
          />
        ) : (
          <GitChanges
            projectPath={project.path}
            repoPath={selectedRepo}
            onCommitSuccess={onCommitSuccess}
          />
        )}
      </div>

      <GitWorktreeModal
        repoPath={worktreeRepo}
        onClose={() => setWorktreeRepo(null)}
        onChanged={loadRepos}
      />
    </div>
  );
}
