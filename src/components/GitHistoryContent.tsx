import { useState, useEffect, useCallback, useRef, memo, useMemo } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { writeText } from '@tauri-apps/plugin-clipboard-manager';
import { useTauriEvent } from '../hooks/useTauriEvent';
import { showContextMenu } from '../utils/contextMenu';
import { isAiPty } from '../utils/terminalCache';
import { formatRelativeTime } from '../utils/timeFormat';
import { CommitDiffModal } from './CommitDiffModal';
import { useT } from '../i18n';
import type { GitRepoInfo, GitCommitInfo, CommitFileInfo, BranchInfo, PtyOutputPayload } from '../types';

interface RepoState {
  commits: GitCommitInfo[];
  loading: boolean;
  hasMore: boolean;
}

// === 仓库树结构 ===

interface RepoTreeNode {
  name: string;
  key: string;          // 用于展开/折叠状态跟踪的稳定标识
  repo?: GitRepoInfo;   // 仅叶节点（实际仓库）有值
  children: RepoTreeNode[];
}

function buildRepoTree(repos: GitRepoInfo[], projectPath: string): RepoTreeNode[] {
  const normalize = (p: string) => p.replace(/[\\/]+/g, '/').replace(/\/$/, '');
  const root: RepoTreeNode[] = [];
  const normalizedProject = normalize(projectPath);

  for (const repo of repos) {
    const normalizedRepo = normalize(repo.path);
    let relative: string;
    if (normalizedRepo === normalizedProject) {
      relative = '.';
    } else if (normalizedRepo.startsWith(normalizedProject + '/')) {
      relative = normalizedRepo.slice(normalizedProject.length + 1);
    } else {
      relative = repo.name;
    }

    if (relative === '.' || !relative.includes('/')) {
      root.push({ name: repo.name, key: repo.path, repo, children: [] });
    } else {
      const parts = relative.split('/');
      let current = root;
      let pathSoFar = normalizedProject;
      for (let i = 0; i < parts.length - 1; i++) {
        pathSoFar += '/' + parts[i];
        let found = current.find((n) => n.name === parts[i] && !n.repo);
        if (!found) {
          found = { name: parts[i], key: 'dir:' + pathSoFar, children: [] };
          current.push(found);
        }
        current = found.children;
      }
      current.push({ name: parts[parts.length - 1], key: repo.path, repo, children: [] });
    }
  }

  return root;
}

const EMPTY_BRANCHES: BranchInfo[] = [];

const GitActionButton = memo(function GitActionButton({
  repoPath,
  action,
  state,
  disabled,
  onClick,
}: {
  repoPath: string;
  action: 'pull' | 'push';
  state?: { status: string; error?: string };
  disabled: boolean;
  onClick: (repoPath: string) => void;
}) {
  const loading = state?.status === 'loading';
  const success = state?.status === 'success';
  const error = state?.status === 'error';

  let display: string;
  let colorClass: string;
  if (loading) {
    display = '↻';
    colorClass = 'text-[var(--text-muted)]';
  } else if (success) {
    display = '✓';
    colorClass = 'text-[var(--color-success)]';
  } else if (error) {
    display = '✕';
    colorClass = 'text-[var(--color-error)]';
  } else {
    display = action === 'pull' ? '↓' : '↑';
    colorClass = 'text-[var(--text-muted)] hover:text-[var(--text-primary)]';
  }

  return (
    <button
      className={`w-5 h-5 flex items-center justify-center text-sm transition-colors rounded ${colorClass} ${
        disabled ? 'opacity-50 cursor-not-allowed' : 'cursor-pointer'
      } ${loading ? 'animate-pulse' : ''}`}
      title={error ? state?.error : action === 'pull' ? 'Git Pull' : 'Git Push'}
      disabled={disabled}
      onClick={(e) => {
        e.stopPropagation();
        if (!disabled) onClick(repoPath);
      }}
    >
      {display}
    </button>
  );
});

const CommitItem = memo(function CommitItem({
  commit,
  allBranches,
  depth,
  repoPath,
  onContextMenu,
  onDoubleClick,
}: {
  commit: GitCommitInfo;
  allBranches: BranchInfo[];
  depth: number;
  repoPath: string;
  onContextMenu: (e: React.MouseEvent, repoPath: string, commit: GitCommitInfo) => void;
  onDoubleClick: (repoPath: string, commit: GitCommitInfo) => void;
}) {
  const t = useT();
  const commitBranches = allBranches.filter((b) => b.commitHash === commit.hash);
  return (
    <div
      className="py-1.5 cursor-pointer hover:bg-[var(--border-subtle)] rounded-[var(--radius-sm)] transition-colors duration-100"
      style={{ paddingLeft: `${(depth + 1) * 16 + 8}px`, paddingRight: '8px' }}
      title={commit.body ? `${commit.message}\n\n${commit.body}` : commit.message}
      onContextMenu={(e) => onContextMenu(e, repoPath, commit)}
      onDoubleClick={() => onDoubleClick(repoPath, commit)}
    >
      <div className="text-sm text-[var(--text-primary)] flex items-center gap-1 min-w-0">
        {commitBranches.map((b) => (
          <span
            key={b.name}
            className="inline-flex items-center shrink-0 text-[11px] leading-[18px] px-1.5 rounded font-medium"
            style={{
              backgroundColor: b.isHead
                ? 'var(--color-accent, #58a6ff)'
                : b.isRemote
                  ? 'var(--border-subtle, #3d3d3d)'
                  : 'rgba(63, 185, 80, 0.2)',
              color: b.isHead
                ? '#fff'
                : b.isRemote
                  ? 'var(--text-muted)'
                  : 'rgb(63, 185, 80)',
            }}
            title={b.isRemote ? t('gitHistoryContent.remoteBranch', { name: b.name }) : b.isHead ? t('gitHistoryContent.currentBranch', { name: b.name }) : t('gitHistoryContent.localBranch', { name: b.name })}
          >
            {b.name}
          </span>
        ))}
        <span className="truncate">{commit.message}</span>
      </div>
      <div className="text-xs text-[var(--text-muted)] flex items-center gap-1.5 mt-0.5">
        <span>{commit.author}</span>
        <span>&middot;</span>
        <span>{formatRelativeTime(commit.timestamp)}</span>
        <span>&middot;</span>
        <span className="font-mono">{commit.shortHash}</span>
      </div>
    </div>
  );
});

const GIT_REFRESH_PATTERNS = [
  /create mode/,
  /Switched to/,
  /Already up to date/,
  /insertions?\(\+\)/,
  /deletions?\(-\)/,
];

interface GitHistoryContentProps {
  projectPath: string;
  repos: GitRepoInfo[];
  refreshRepos: () => void;
}

export function GitHistoryContent({ projectPath, repos, refreshRepos }: GitHistoryContentProps) {
  const t = useT();
  const [expandedRepos, setExpandedRepos] = useState<Set<string>>(new Set());
  const [repoStates, setRepoStates] = useState<Map<string, RepoState>>(new Map());
  const [diffModal, setDiffModal] = useState<{
    open: boolean;
    repoPath: string;
    commitHash: string;
    commitMessage: string;
    files: CommitFileInfo[];
  } | null>(null);

  // branch name → commit hash 映射（每个 repo 独立）
  const [repoBranches, setRepoBranches] = useState<Map<string, BranchInfo[]>>(new Map());

  // 每个 repo 当前"查看"的分支(用于只改 git log 显示,不 checkout)。未设则用 HEAD。
  const [viewBranches, setViewBranches] = useState<Map<string, string>>(new Map());
  const viewBranchesRef = useRef(viewBranches);
  viewBranchesRef.current = viewBranches;

  // 当前打开的分支下拉所属的 repoPath(同一时刻最多一个)
  const [branchDropdownOpen, setBranchDropdownOpen] = useState<string | null>(null);
  const branchDropdownRef = useRef<HTMLDivElement | null>(null);

  const scrollRef = useRef<HTMLDivElement>(null);

  // pull/push 操作状态
  const [pullState, setPullState] = useState<Map<string, { status: string; error?: string }>>(new Map());
  const [pushState, setPushState] = useState<Map<string, { status: string; error?: string }>>(new Map());

  const repoStatesRef = useRef(repoStates);
  repoStatesRef.current = repoStates;
  const autoExpandedForRef = useRef<string | null>(null);

  const loadBranches = useCallback(async (repoPath: string) => {
    try {
      const branches = await invoke<BranchInfo[]>('get_repo_branches', { repoPath });
      setRepoBranches((prev) => {
        const next = new Map(prev);
        next.set(repoPath, branches);
        return next;
      });
    } catch {
      // ignore
    }
  }, []);

  const loadCommits = useCallback(
    async (repoPath: string, beforeCommit?: string, branchOverride?: string) => {
      const existing = repoStatesRef.current.get(repoPath);
      if (existing?.loading) return;

      // 分页时(beforeCommit 有值)继续当前链路,不需要 branch 参数;
      // 否则优先使用 override,再回退到 viewBranchesRef 里记住的值
      const branch = beforeCommit
        ? undefined
        : branchOverride ?? viewBranchesRef.current.get(repoPath);

      setRepoStates((prev) => {
        const next = new Map(prev);
        const cur = next.get(repoPath) ?? { commits: [], loading: false, hasMore: true };
        next.set(repoPath, { ...cur, loading: true });
        return next;
      });

      try {
        const commits = await invoke<GitCommitInfo[]>('get_git_log', {
          repoPath,
          beforeCommit: beforeCommit ?? null,
          limit: 30,
          branch: branch ?? null,
        });
        setRepoStates((prev) => {
          const next = new Map(prev);
          const cur = next.get(repoPath) ?? { commits: [], loading: false, hasMore: true };
          next.set(repoPath, {
            commits: beforeCommit ? [...cur.commits, ...commits] : commits,
            loading: false,
            hasMore: commits.length >= 30,
          });
          return next;
        });
      } catch {
        setRepoStates((prev) => {
          const next = new Map(prev);
          const cur = next.get(repoPath) ?? { commits: [], loading: false, hasMore: true };
          next.set(repoPath, { ...cur, loading: false });
          return next;
        });
      }
    },
    [],
  );

  const toggleRepo = useCallback(
    (repoPath: string) => {
      setExpandedRepos((prev) => {
        const next = new Set(prev);
        if (next.has(repoPath)) {
          next.delete(repoPath);
        } else {
          next.add(repoPath);
          if (!repoStatesRef.current.has(repoPath)) {
            loadCommits(repoPath);
          }
          loadBranches(repoPath);
        }
        return next;
      });
    },
    [loadCommits, loadBranches],
  );

  const expandedReposRef = useRef(expandedRepos);
  expandedReposRef.current = expandedRepos;

  const handleScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    if (el.scrollTop + el.clientHeight < el.scrollHeight - 50) return;

    for (const repoPath of expandedReposRef.current) {
      const state = repoStatesRef.current.get(repoPath);
      if (state && state.hasMore && !state.loading && state.commits.length > 0) {
        const lastHash = state.commits[state.commits.length - 1].hash;
        loadCommits(repoPath, lastHash);
        break;
      }
    }
  }, [loadCommits]);

  const handleViewDiff = useCallback(async (repoPath: string, commit: GitCommitInfo) => {
    try {
      const files = await invoke<CommitFileInfo[]>('get_commit_files', {
        repoPath,
        commitHash: commit.hash,
      });
      setDiffModal({
        open: true,
        repoPath,
        commitHash: commit.hash,
        commitMessage: commit.message,
        files,
      });
    } catch (e) {
      console.error('get_commit_files failed:', e);
    }
  }, []);

  const handleSwitchView = useCallback(
    (repoPath: string, branchName: string) => {
      setBranchDropdownOpen(null);
      setViewBranches((prev) => {
        const next = new Map(prev);
        next.set(repoPath, branchName);
        return next;
      });
      // 清空该 repo 的 commits,随后用 branchOverride 传入新分支重新加载
      // (setState 异步,不能依赖 viewBranchesRef 立即拿到新值)
      setRepoStates((prev) => {
        const next = new Map(prev);
        next.set(repoPath, { commits: [], loading: false, hasMore: true });
        return next;
      });
      loadCommits(repoPath, undefined, branchName);
    },
    [loadCommits],
  );

  // 点击下拉外部关闭
  useEffect(() => {
    if (!branchDropdownOpen) return;
    const handleClick = (e: MouseEvent) => {
      if (branchDropdownRef.current && !branchDropdownRef.current.contains(e.target as Node)) {
        setBranchDropdownOpen(null);
      }
    };
    document.addEventListener('mousedown', handleClick);
    return () => document.removeEventListener('mousedown', handleClick);
  }, [branchDropdownOpen]);

  const handleCommitContextMenu = useCallback(
    (e: React.MouseEvent, repoPath: string, commit: GitCommitInfo) => {
      e.preventDefault();
      showContextMenu(e.clientX, e.clientY, [
        {
          label: t('gitHistoryContent.copyCommitHash'),
          onClick: () => writeText(commit.hash),
        },
        { separator: true },
        {
          label: t('gitHistoryContent.viewChanges'),
          onClick: () => handleViewDiff(repoPath, commit),
        },
      ]);
    },
    [handleViewDiff, t],
  );

  const refreshTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const debouncedRefresh = useCallback(() => {
    if (refreshTimerRef.current) clearTimeout(refreshTimerRef.current);
    refreshTimerRef.current = setTimeout(() => {
      refreshRepos();
      for (const repoPath of expandedReposRef.current) {
        loadCommits(repoPath);
        loadBranches(repoPath);
      }
    }, 500);
  }, [refreshRepos, loadCommits, loadBranches]);

  const handlePull = useCallback(async (repoPath: string) => {
    setPullState((prev) => new Map(prev).set(repoPath, { status: 'loading' }));
    setPushState((prev) => { const n = new Map(prev); n.delete(repoPath); return n; });
    try {
      await invoke('git_pull', { repoPath });
      setPullState((prev) => new Map(prev).set(repoPath, { status: 'success' }));
      loadCommits(repoPath);
      loadBranches(repoPath);
    } catch (e) {
      setPullState((prev) => new Map(prev).set(repoPath, { status: 'error', error: String(e) }));
    }
    setTimeout(() => {
      setPullState((prev) => { const n = new Map(prev); n.delete(repoPath); return n; });
    }, 1500);
  }, [loadCommits, loadBranches]);

  const handlePush = useCallback(async (repoPath: string) => {
    setPushState((prev) => new Map(prev).set(repoPath, { status: 'loading' }));
    setPullState((prev) => { const n = new Map(prev); n.delete(repoPath); return n; });
    try {
      await invoke('git_push', { repoPath });
      setPushState((prev) => new Map(prev).set(repoPath, { status: 'success' }));
      loadBranches(repoPath);
    } catch (e) {
      setPushState((prev) => new Map(prev).set(repoPath, { status: 'error', error: String(e) }));
    }
    setTimeout(() => {
      setPushState((prev) => { const n = new Map(prev); n.delete(repoPath); return n; });
    }, 1500);
  }, [loadBranches]);

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

  // 仅一个仓库时自动展开
  useEffect(() => {
    if (repos.length !== 1) return;
    if (autoExpandedForRef.current === projectPath) return;
    autoExpandedForRef.current = projectPath;

    const repoPath = repos[0].path;
    const tree = buildRepoTree(repos, projectPath);
    const keys = new Set<string>();
    const collect = (nodes: RepoTreeNode[]) => {
      for (const n of nodes) { keys.add(n.key); collect(n.children); }
    };
    collect(tree);
    setExpandedRepos(keys);
    loadCommits(repoPath);
    loadBranches(repoPath);
  }, [repos, projectPath, loadCommits, loadBranches]);

  const repoTree = useMemo(() => buildRepoTree(repos, projectPath), [repos, projectPath]);

  // 递归渲染树节点
  const renderTreeNode = (node: RepoTreeNode, depth: number) => {
    // 仓库叶节点 —— 可展开显示 commits
    if (node.repo) {
      const repo = node.repo;
      const isExpanded = expandedRepos.has(repo.path);
      const state = repoStates.get(repo.path);
      const dropdownOpen = branchDropdownOpen === repo.path;
      return (
        <div key={repo.path}>
          <div
            className="sticky bg-[var(--bg-surface)] h-[30px] flex items-center"
            style={{ top: `${depth * 30}px`, zIndex: dropdownOpen ? 50 : 10 - depth }}
          >
            <div
              className="group flex items-center justify-between w-full py-[5px] cursor-pointer hover:bg-[var(--border-subtle)] rounded-[var(--radius-sm)] text-base transition-colors duration-100 text-[var(--color-folder)]"
              style={{ paddingLeft: `${depth * 16 + 8}px`, paddingRight: '8px' }}
              onClick={() => toggleRepo(repo.path)}
            >
              <div className="flex items-center gap-1 min-w-0">
                <span
                  className="text-[13px] w-3 text-center text-[var(--text-muted)] transition-transform duration-150"
                  style={{
                    transform: isExpanded ? 'rotate(0deg)' : 'rotate(-90deg)',
                    display: 'inline-block',
                  }}
                >
                  &#9662;
                </span>
                <span className="truncate font-medium">{node.name}</span>
                {repo.currentBranch && (() => {
                  const viewing = viewBranches.get(repo.path);
                  const displayBranch = viewing ?? repo.currentBranch;
                  const isViewingOther = viewing !== undefined && viewing !== repo.currentBranch;
                  const allBranches = repoBranches.get(repo.path) ?? [];
                  return (
                    <div
                      className="relative shrink-0"
                      ref={dropdownOpen ? branchDropdownRef : null}
                    >
                      <span
                        className={`inline-flex items-center gap-0.5 text-[11px] leading-[18px] px-1.5 rounded font-mono cursor-pointer transition-colors ${
                          isViewingOther
                            ? 'text-[var(--color-accent,#58a6ff)] bg-[rgba(88,166,255,0.15)] hover:bg-[rgba(88,166,255,0.25)]'
                            : 'text-[var(--text-muted)] bg-[var(--border-subtle)] hover:bg-[var(--color-accent,#58a6ff)] hover:text-white'
                        }`}
                        title={
                          isViewingOther
                            ? t('gitHistoryContent.viewingBranchHistory', { branch: displayBranch, head: repo.currentBranch })
                            : t('gitHistoryContent.switchBranchHint')
                        }
                        onClick={(e) => {
                          e.stopPropagation();
                          if (dropdownOpen) {
                            setBranchDropdownOpen(null);
                          } else {
                            setBranchDropdownOpen(repo.path);
                            if (!repoBranches.has(repo.path)) loadBranches(repo.path);
                          }
                        }}
                      >
                        <span className="truncate max-w-[200px]">{displayBranch}</span>
                        <span className="text-[9px] opacity-70">▾</span>
                      </span>
                      {dropdownOpen && (
                        <div
                          className="absolute top-full left-0 mt-0.5 bg-[var(--bg-elevated)] border border-[var(--border-default)] rounded-[var(--radius-sm)] shadow-[var(--shadow-overlay)] overflow-hidden min-w-[180px] max-h-[320px] overflow-y-auto"
                          style={{ zIndex: 100 }}
                          onClick={(e) => e.stopPropagation()}
                        >
                          {allBranches.length === 0 ? (
                            <div className="px-3 py-1.5 text-xs text-[var(--text-muted)]">{t('gitHistoryContent.loading')}</div>
                          ) : (
                            allBranches.map((b) => {
                              const active = displayBranch === b.name;
                              return (
                                <div
                                  key={b.name}
                                  className={`flex items-center gap-1.5 px-3 py-1.5 text-xs cursor-pointer transition-colors duration-100 ${
                                    active
                                      ? 'bg-[var(--accent-subtle,rgba(88,166,255,0.15))] text-[var(--color-accent,#58a6ff)]'
                                      : 'text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
                                  }`}
                                  onClick={() => handleSwitchView(repo.path, b.name)}
                                >
                                  <span
                                    className="w-1.5 h-1.5 rounded-full shrink-0"
                                    style={{
                                      backgroundColor: b.isRemote
                                        ? 'var(--text-muted)'
                                        : 'rgb(63, 185, 80)',
                                    }}
                                  />
                                  <span className="truncate font-mono flex-1">{b.name}</span>
                                  {b.name === repo.currentBranch && (
                                    <span className="shrink-0 text-[9px] px-1 rounded bg-[var(--color-accent,#58a6ff)] text-white font-medium">HEAD</span>
                                  )}
                                </div>
                              );
                            })
                          )}
                        </div>
                      )}
                    </div>
                  );
                })()}
              </div>
              <div className="flex items-center gap-1 opacity-0 group-hover:opacity-100 transition-opacity duration-150">
                <button
                  className="w-5 h-5 flex items-center justify-center text-sm transition-colors rounded text-[var(--text-muted)] hover:text-[var(--text-primary)] cursor-pointer"
                  title={t('gitHistoryContent.refresh')}
                  onClick={(e) => {
                    e.stopPropagation();
                    loadCommits(repo.path);
                    loadBranches(repo.path);
                  }}
                >
                  ↻
                </button>
                <GitActionButton
                  repoPath={repo.path}
                  action="pull"
                  state={pullState.get(repo.path)}
                  disabled={pullState.get(repo.path)?.status === 'loading' || pushState.get(repo.path)?.status === 'loading'}
                  onClick={handlePull}
                />
                <GitActionButton
                  repoPath={repo.path}
                  action="push"
                  state={pushState.get(repo.path)}
                  disabled={pullState.get(repo.path)?.status === 'loading' || pushState.get(repo.path)?.status === 'loading'}
                  onClick={handlePush}
                />
              </div>
            </div>
          </div>

          {isExpanded && (
            <div className="relative" style={{ zIndex: 0 }}>
              {state?.commits.map((commit) => (
                <CommitItem
                  key={commit.hash}
                  commit={commit}
                  allBranches={repoBranches.get(repo.path) ?? EMPTY_BRANCHES}
                  depth={depth}
                  repoPath={repo.path}
                  onContextMenu={handleCommitContextMenu}
                  onDoubleClick={handleViewDiff}
                />
              ))}

              {state?.loading && (
                <div className="text-center text-[var(--text-muted)] text-xs py-2">
                  {t('gitHistoryContent.loading')}
                </div>
              )}

              {state && !state.loading && state.commits.length === 0 && (
                <div className="text-center text-[var(--text-muted)] text-xs py-2">
                  {t('gitHistoryContent.noCommits')}
                </div>
              )}
            </div>
          )}
        </div>
      );
    }

    // 纯目录节点 —— 可折叠
    const isDirExpanded = expandedRepos.has(node.key);
    return (
      <div key={node.key}>
        <div
          className="sticky bg-[var(--bg-surface)] h-[30px] flex items-center"
          style={{ top: `${depth * 30}px`, zIndex: 10 - depth }}
        >
          <div
            className="flex items-center gap-1 w-full py-[3px] cursor-pointer hover:bg-[var(--border-subtle)] rounded-[var(--radius-sm)] text-base text-[var(--text-muted)] transition-colors duration-100"
            style={{ paddingLeft: `${depth * 16 + 8}px` }}
            onClick={() => {
              setExpandedRepos((prev) => {
                const next = new Set(prev);
                if (next.has(node.key)) next.delete(node.key);
                else next.add(node.key);
                return next;
              });
            }}
          >
            <span
              className="text-[13px] w-3 text-center transition-transform duration-150"
              style={{ transform: isDirExpanded ? 'rotate(0deg)' : 'rotate(-90deg)', display: 'inline-block' }}
            >
              ▾
            </span>
            <span className="truncate">{node.name}</span>
          </div>
        </div>
        {isDirExpanded && node.children.map((child) => renderTreeNode(child, depth + 1))}
      </div>
    );
  };

  return (
    <div className="h-full bg-[var(--bg-surface)] flex flex-col">
      <div className="flex-1 overflow-y-auto px-1" ref={scrollRef} onScroll={handleScroll}>
        {repos.length === 0 && (
          <div className="text-center text-[var(--text-muted)] text-sm py-6">
            {t('gitHistoryContent.noRepos')}
          </div>
        )}
        {repoTree.map((node) => renderTreeNode(node, 0))}
      </div>

      {diffModal && (
        <CommitDiffModal
          open={diffModal.open}
          onClose={() => setDiffModal(null)}
          repoPath={diffModal.repoPath}
          commitHash={diffModal.commitHash}
          commitMessage={diffModal.commitMessage}
          files={diffModal.files}
        />
      )}
    </div>
  );
}
