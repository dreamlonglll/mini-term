import { useState, useEffect, useCallback, useRef, memo } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import { GitHistoryContent } from './GitHistoryContent';
import { GitChanges } from './GitChanges';
import { GitWorktreeModal } from './GitWorktreeModal';
import { getGitHistoryCache, setGitHistoryCache } from '../utils/projectDataCache';
import { showContextMenu } from '../utils/contextMenu';
import { newTerminal } from '../utils/paneActions';
import { useT } from '../i18n';
import type { GitRepoInfo, BranchInfo } from '../types';

// 两个区块的折叠状态与上下分割比例(ratio = 更改区块占比)。抽屉一关本组件
// 就整个卸载,放模块级让它在会话内保留;有意不落盘——属于临时视图状态
const sectionUi = { changesOpen: true, historyOpen: true, ratio: 0.5 };

const clampRatio = (r: number) => Math.min(0.85, Math.max(0.15, r));

type SyncState = { status: 'loading' | 'success' | 'error'; error?: string } | null;

const GitActionButton = memo(function GitActionButton({
  action,
  state,
  disabled,
  onClick,
}: {
  action: 'pull' | 'push';
  state: SyncState;
  disabled: boolean;
  onClick: () => void;
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
      className={`w-5 h-5 flex items-center justify-center text-sm transition-colors rounded flex-shrink-0 ${colorClass} ${
        disabled ? 'opacity-50 cursor-not-allowed' : 'cursor-pointer'
      } ${loading ? 'animate-pulse' : ''}`}
      title={error ? state?.error : action === 'pull' ? 'Git Pull' : 'Git Push'}
      disabled={disabled}
      onClick={(e) => {
        e.stopPropagation();
        if (!disabled) onClick();
      }}
    >
      {display}
    </button>
  );
});

function SectionHeader({
  label,
  expanded,
  bordered,
  onToggle,
}: {
  label: string;
  expanded: boolean;
  /** 顶部分隔线:仅下方「提交历史」区块需要,用来和上方更改内容划界 */
  bordered?: boolean;
  onToggle: () => void;
}) {
  return (
    <div
      className={`flex items-center gap-1 px-2 h-[30px] flex-shrink-0 cursor-pointer text-[var(--text-primary)] hover:bg-[var(--border-subtle)] transition-colors duration-100 ${
        bordered ? 'border-t border-[var(--border-subtle)]' : ''
      }`}
      onClick={onToggle}
    >
      <span
        className="git-section-chevron text-base w-3 text-center text-[var(--text-muted)]"
        style={{
          transform: expanded ? 'rotate(0deg)' : 'rotate(-90deg)',
          display: 'inline-block',
        }}
      >
        &#9662;
      </span>
      <span className="text-sm font-medium">{label}</span>
    </div>
  );
}

export function GitHistory() {
  const t = useT();
  const activeProjectId = useAppStore((s) => s.activeProjectId);
  const config = useAppStore((s) => s.config);
  const project = config.projects.find((p) => p.id === activeProjectId);
  // SSH 远程项目:git 命令跑在本地,对远程路径无意义 → 整个面板显示占位(远程 Git 二期)
  const isRemote = !!project?.sshConnectionId;

  // 上下两个可折叠区块(更改 / 提交历史)——两块常驻挂载,折叠只把内容高度
  // 收到 0,已加载的 commits、提交草稿都不丢
  const [changesOpen, setChangesOpen] = useState(sectionUi.changesOpen);
  const [historyOpen, setHistoryOpen] = useState(sectionUi.historyOpen);
  const [ratio, setRatio] = useState(sectionUi.ratio);
  // 拖拽调比例期间关掉 flex-grow 补间,否则跟不上鼠标
  const [resizing, setResizing] = useState(false);
  const changesBodyRef = useRef<HTMLDivElement>(null);
  const historyBodyRef = useRef<HTMLDivElement>(null);

  const toggleChanges = useCallback(() => {
    setChangesOpen((v) => {
      sectionUi.changesOpen = !v;
      return !v;
    });
  }, []);
  const toggleHistory = useCallback(() => {
    setHistoryOpen((v) => {
      sectionUi.historyOpen = !v;
      return !v;
    });
  }, []);

  // 两块都展开时中缝可拖拽调比例(同 RightDrawer 左缘拖宽):
  // 按下时记两块内容实测高度,移动量换算成新 ratio
  const startSectionResize = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    const h1 = changesBodyRef.current?.offsetHeight ?? 0;
    const total = h1 + (historyBodyRef.current?.offsetHeight ?? 0);
    if (total <= 0) return;
    const startY = e.clientY;
    const prevUserSelect = document.body.style.userSelect;
    document.body.style.userSelect = 'none';
    setResizing(true);
    const onMove = (ev: MouseEvent) => {
      const next = clampRatio((h1 + (ev.clientY - startY)) / total);
      sectionUi.ratio = next;
      setRatio(next);
    };
    const onUp = () => {
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
      document.body.style.userSelect = prevUserSelect;
      setResizing(false);
    };
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
  }, []);

  // 仓库列表与选中仓库 — 顶部仓库栏统一选择,两个区块共享
  const [repos, setRepos] = useState<GitRepoInfo[]>(() => {
    return (project ? getGitHistoryCache(project.path) : undefined)?.repos ?? [];
  });
  const [selectedRepo, setSelectedRepo] = useState<string>(() => {
    return (project ? getGitHistoryCache(project.path) : undefined)?.selectedRepo ?? '';
  });
  const [repoDropdownOpen, setRepoDropdownOpen] = useState(false);
  const dropdownRef = useRef<HTMLDivElement>(null);
  const selectedRepoRef = useRef(selectedRepo);
  selectedRepoRef.current = selectedRepo;

  // 选中仓库的分支状态(分支徽章 + 下拉 + commit 行标注共用)
  const [branches, setBranches] = useState<BranchInfo[]>([]);
  // 正在查看(未 checkout)的分支;undefined = 跟随 HEAD。只改历史显示
  const [viewBranch, setViewBranch] = useState<string | undefined>(undefined);
  const [branchDropdownOpen, setBranchDropdownOpen] = useState(false);
  const branchDropdownRef = useRef<HTMLDivElement>(null);

  // pull/push 操作状态(作用于选中仓库)
  const [pullState, setPullState] = useState<SyncState>(null);
  const [pushState, setPushState] = useState<SyncState>(null);

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

  const loadBranches = useCallback(async (repoPath: string) => {
    try {
      const b = await invoke<BranchInfo[]>('get_repo_branches', { repoPath });
      // 加载期间可能已切走仓库,迟到的响应丢弃
      if (selectedRepoRef.current === repoPath) setBranches(b);
    } catch {
      // ignore
    }
  }, []);

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

  // 切换选中仓库 → 分支相关状态全部重置并重载
  useEffect(() => {
    setBranches([]);
    setViewBranch(undefined);
    setBranchDropdownOpen(false);
    setPullState(null);
    setPushState(null);
    if (selectedRepo) loadBranches(selectedRepo);
  }, [selectedRepo, loadBranches]);

  // 点击外部关闭两个下拉
  useEffect(() => {
    if (!repoDropdownOpen && !branchDropdownOpen) return;
    const handleClick = (e: MouseEvent) => {
      if (repoDropdownOpen && dropdownRef.current && !dropdownRef.current.contains(e.target as Node)) {
        setRepoDropdownOpen(false);
      }
      if (branchDropdownOpen && branchDropdownRef.current && !branchDropdownRef.current.contains(e.target as Node)) {
        setBranchDropdownOpen(false);
      }
    };
    document.addEventListener('mousedown', handleClick);
    return () => document.removeEventListener('mousedown', handleClick);
  }, [repoDropdownOpen, branchDropdownOpen]);

  // commit 成功 → 刷新提交历史与分支徽章(分支头已前移)
  const [historyRefreshKey, setHistoryRefreshKey] = useState(0);
  const onCommitSuccess = useCallback(() => {
    setHistoryRefreshKey((k) => k + 1);
    if (selectedRepoRef.current) loadBranches(selectedRepoRef.current);
  }, [loadBranches]);

  // 历史内容检测到 git 活动时回调:刷新仓库列表 + 选中仓库分支
  const refreshRepoMeta = useCallback(() => {
    loadRepos();
    if (selectedRepoRef.current) loadBranches(selectedRepoRef.current);
  }, [loadRepos, loadBranches]);

  const handlePull = useCallback(async () => {
    const repoPath = selectedRepoRef.current;
    if (!repoPath) return;
    setPullState({ status: 'loading' });
    setPushState(null);
    try {
      await invoke('git_pull', { repoPath });
      setPullState({ status: 'success' });
      loadBranches(repoPath);
      setHistoryRefreshKey((k) => k + 1);
    } catch (e) {
      setPullState({ status: 'error', error: String(e) });
    }
    setTimeout(() => setPullState(null), 1500);
  }, [loadBranches]);

  const handlePush = useCallback(async () => {
    const repoPath = selectedRepoRef.current;
    if (!repoPath) return;
    setPushState({ status: 'loading' });
    setPullState(null);
    try {
      await invoke('git_push', { repoPath });
      setPushState({ status: 'success' });
      loadBranches(repoPath);
    } catch (e) {
      setPushState({ status: 'error', error: String(e) });
    }
    setTimeout(() => setPushState(null), 1500);
  }, [loadBranches]);

  // Worktree 管理弹窗(仓库栏右键菜单进入);增删后刷新仓库列表
  const [worktreeRepo, setWorktreeRepo] = useState<string | null>(null);

  const selectedRepoInfo = repos.find((r) => r.path === selectedRepo);

  // 仓库栏右键:在终端打开 / Worktree 管理
  const handleRepoContextMenu = useCallback(
    (e: React.MouseEvent) => {
      const repo = repos.find((r) => r.path === selectedRepoRef.current);
      const projectPath = project?.path;
      if (!repo || !projectPath) return;
      e.preventDefault();
      e.stopPropagation();
      showContextMenu(e.clientX, e.clientY, [
        {
          label: t('gitHistoryContent.openInTerminal'),
          onClick: () => {
            const projectId = useAppStore.getState().activeProjectId;
            if (!projectId) return;
            // 项目根仓库不必带 cwd 覆盖(默认就是项目根);子仓库/worktree 才需要
            const isProjectRoot = repo.path.replace(/[\\/]+$/, '') === projectPath.replace(/[\\/]+$/, '');
            void newTerminal(projectId, undefined, isProjectRoot ? undefined : {
              cwd: repo.path,
              title: repo.isWorktree ? `⎇ ${repo.currentBranch ?? repo.name}` : repo.name,
            });
          },
        },
        { separator: true },
        {
          label: t('gitHistoryContent.manageWorktrees'),
          onClick: () => setWorktreeRepo(repo.path),
        },
      ]);
    },
    [repos, project?.path, t],
  );

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

  const displayBranch = viewBranch ?? selectedRepoInfo?.currentBranch;
  const isViewingOther = viewBranch !== undefined && viewBranch !== selectedRepoInfo?.currentBranch;

  return (
    <div data-panel className="h-full bg-[var(--bg-surface)] flex flex-col border-t border-[var(--border-subtle)] select-none">
      {/* ── 仓库栏:下拉选仓库 + 分支徽章 + 刷新/拉/推 ── */}
      {repos.length > 0 && (
        <div className="relative flex items-center gap-1 pl-1.5 pr-2 h-[34px] flex-shrink-0 border-b border-[var(--border-subtle)]">
          <div ref={dropdownRef} className="min-w-0">
            <div
              className="flex items-center gap-1 min-w-0 px-1 py-[3px] rounded-[var(--radius-sm)] cursor-pointer hover:bg-[var(--border-subtle)] transition-colors duration-100 text-[var(--color-folder)]"
              title={selectedRepoInfo?.path}
              onClick={() => setRepoDropdownOpen((v) => !v)}
              onContextMenu={handleRepoContextMenu}
            >
              <span
                className="text-base w-3 text-center text-[var(--text-muted)] transition-transform duration-150"
                style={{
                  transform: repoDropdownOpen ? 'rotate(0deg)' : 'rotate(-90deg)',
                  display: 'inline-block',
                }}
              >
                &#9662;
              </span>
              <span className="truncate font-medium text-sm">{selectedRepoInfo?.name ?? t("gitHistory.selectRepo")}</span>
              {selectedRepoInfo?.isWorktree && (
                <span className="shrink-0 text-sm text-[var(--text-muted)]" title={t('gitHistoryContent.worktreeBadgeTitle')}>
                  ⎇
                </span>
              )}
            </div>
            {repoDropdownOpen && (
              <div className="absolute left-1.5 right-2 top-full z-50 mt-0.5 bg-[var(--bg-elevated)] border border-[var(--border-default)] rounded-[var(--radius-sm)] shadow-[var(--shadow-overlay)] overflow-hidden">
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
                      setGitHistoryCache(project.path, { repos, selectedRepo: r.path });
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

          {/* 分支徽章:点击展开分支下拉,只切历史查看、不 checkout */}
          {displayBranch && (
            <div className="relative shrink-0" ref={branchDropdownRef}>
              <span
                className={`inline-flex items-center gap-0.5 text-sm leading-[18px] px-1.5 rounded font-mono cursor-pointer transition-colors ${
                  isViewingOther
                    ? 'text-[var(--color-accent,#58a6ff)] bg-[rgba(88,166,255,0.15)] hover:bg-[rgba(88,166,255,0.25)]'
                    : 'text-[var(--text-muted)] bg-[var(--border-subtle)] hover:bg-[var(--color-accent,#58a6ff)] hover:text-white'
                }`}
                title={
                  isViewingOther
                    ? t('gitHistoryContent.viewingBranchHistory', { branch: displayBranch, head: selectedRepoInfo?.currentBranch ?? '' })
                    : t('gitHistoryContent.switchBranchHint')
                }
                onClick={(e) => {
                  e.stopPropagation();
                  if (!branchDropdownOpen && branches.length === 0 && selectedRepo) loadBranches(selectedRepo);
                  setBranchDropdownOpen((v) => !v);
                }}
              >
                <span className="truncate max-w-[140px]">{displayBranch}</span>
                <span className="text-[0.7rem] opacity-70">▾</span>
              </span>
              {branchDropdownOpen && (
                <div
                  className="absolute top-full left-0 mt-0.5 bg-[var(--bg-elevated)] border border-[var(--border-default)] rounded-[var(--radius-sm)] shadow-[var(--shadow-overlay)] overflow-hidden min-w-[180px] max-h-[320px] overflow-y-auto"
                  style={{ zIndex: 100 }}
                  onClick={(e) => e.stopPropagation()}
                >
                  {branches.length === 0 ? (
                    <div className="px-3 py-1.5 text-xs text-[var(--text-muted)]">{t('gitHistoryContent.loading')}</div>
                  ) : (
                    branches.map((b) => {
                      const active = displayBranch === b.name;
                      return (
                        <div
                          key={b.name}
                          className={`flex items-center gap-1.5 px-3 py-1.5 text-xs cursor-pointer transition-colors duration-100 ${
                            active
                              ? 'bg-[var(--accent-subtle,rgba(88,166,255,0.15))] text-[var(--color-accent,#58a6ff)]'
                              : 'text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
                          }`}
                          onClick={() => {
                            setBranchDropdownOpen(false);
                            setViewBranch(b.name);
                          }}
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
                          {b.name === selectedRepoInfo?.currentBranch && (
                            <span className="shrink-0 text-[0.7rem] px-1 rounded bg-[var(--color-accent,#58a6ff)] text-white font-medium">HEAD</span>
                          )}
                        </div>
                      );
                    })
                  )}
                </div>
              )}
            </div>
          )}

          <div className="flex-1" />

          <button
            className="w-5 h-5 flex items-center justify-center text-sm transition-colors rounded text-[var(--text-muted)] hover:text-[var(--text-primary)] cursor-pointer flex-shrink-0"
            title={t('gitHistoryContent.refresh')}
            onClick={() => {
              refreshRepoMeta();
              setHistoryRefreshKey((k) => k + 1);
            }}
          >
            ↻
          </button>
          <GitActionButton
            action="pull"
            state={pullState}
            disabled={pullState?.status === 'loading' || pushState?.status === 'loading'}
            onClick={handlePull}
          />
          <GitActionButton
            action="push"
            state={pushState}
            disabled={pullState?.status === 'loading' || pushState?.status === 'loading'}
            onClick={handlePush}
          />
        </div>
      )}

      {/* ── 更改 ── */}
      <SectionHeader label={t('panels.changes')} expanded={changesOpen} onToggle={toggleChanges} />
      <div
        ref={changesBodyRef}
        className={`git-section-body min-h-0 overflow-hidden ${resizing ? 'is-resizing' : ''}`}
        style={{ flexGrow: changesOpen ? (historyOpen ? ratio : 1) : 0, flexBasis: 0 }}
      >
        <GitChanges
          projectPath={project.path}
          repoPath={selectedRepo}
          onCommitSuccess={onCommitSuccess}
        />
      </div>

      {/* 两块都展开时,中缝(历史头部上缘)是拖拽手柄;h-0 wrapper 不占布局 */}
      {changesOpen && historyOpen && (
        <div className="relative h-0 flex-shrink-0 z-30">
          <div
            className="absolute left-0 right-0 -top-[3px] h-1.5 cursor-row-resize hover:bg-[var(--accent)]/40 transition-colors"
            onMouseDown={startSectionResize}
          />
        </div>
      )}

      {/* ── 提交历史 ── */}
      <SectionHeader label={t('panels.history')} expanded={historyOpen} bordered onToggle={toggleHistory} />
      <div
        ref={historyBodyRef}
        className={`git-section-body min-h-0 overflow-hidden ${resizing ? 'is-resizing' : ''}`}
        style={{ flexGrow: historyOpen ? (changesOpen ? 1 - ratio : 1) : 0, flexBasis: 0 }}
      >
        <GitHistoryContent
          key={historyRefreshKey}
          repoPath={selectedRepo}
          branches={branches}
          viewBranch={viewBranch}
          refreshRepos={refreshRepoMeta}
        />
      </div>

      <GitWorktreeModal
        repoPath={worktreeRepo}
        onClose={() => setWorktreeRepo(null)}
        onChanged={loadRepos}
      />
    </div>
  );
}
