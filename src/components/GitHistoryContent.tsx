import { useState, useEffect, useCallback, useRef, memo, useMemo, useId } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { writeText } from '@tauri-apps/plugin-clipboard-manager';
import { useTauriEvent } from '../hooks/useTauriEvent';
import { useOverlayValue } from '../hooks/useOverlayMotion';
import { showContextMenu } from '../utils/contextMenu';
import { isAiPty } from '../utils/terminalCache';
import { formatRelativeTime } from '../utils/timeFormat';
import { CommitDiffModal } from './CommitDiffModal';
import {
  computeGitGraph,
  segmentPath,
  segmentGradient,
  needsGradient,
  laneX,
  GRAPH_ROW_HEIGHT,
  type GraphRow,
} from '../utils/gitGraph';
import { useT } from '../i18n';
import type { GitCommitInfo, CommitFileInfo, BranchInfo, PtyOutputPayload } from '../types';

/** 单行的拓扑图：先画连线，再把节点圆盖在上面 */
const CommitGraphCell = memo(function CommitGraphCell({
  row,
  width,
}: {
  row: GraphRow;
  width: number;
}) {
  const mid = GRAPH_ROW_HEIGHT / 2;
  const x = laneX(row.lane);
  // useId 带冒号，SVG 的 url(#…) 引用里去掉更稳妥
  const uid = useId().replace(/:/g, '');
  return (
    <svg
      width={width}
      height={GRAPH_ROW_HEIGHT}
      className="shrink-0 pointer-events-none"
      aria-hidden="true"
    >
      <defs>
        {row.segments.map((seg, i) => {
          if (!needsGradient(seg)) return null;
          return (
            <linearGradient
              key={i}
              id={`${uid}-${i}`}
              gradientUnits="userSpaceOnUse"
              {...segmentGradient(seg, row.lane)}
            >
              {/* 前半段保持分支自己的颜色，只在根部融入目标线 */}
              <stop offset="0%" stopColor={seg.color} />
              <stop offset="70%" stopColor={seg.color} />
              <stop offset="100%" stopColor={seg.endColor} />
            </linearGradient>
          );
        })}
      </defs>
      {row.segments.map((seg, i) => (
        <path
          key={i}
          d={segmentPath(seg, row.lane)}
          stroke={needsGradient(seg) ? `url(#${uid}-${i})` : seg.color}
          strokeWidth={1.5}
          fill="none"
        />
      ))}
      {row.isMerge ? (
        <>
          <circle cx={x} cy={mid} r={5.5} fill="none" stroke={row.color} strokeWidth={1.5} opacity={0.55} />
          <circle cx={x} cy={mid} r={3} fill={row.color} />
        </>
      ) : (
        <circle cx={x} cy={mid} r={4} fill={row.color} />
      )}
    </svg>
  );
});

const CommitItem = memo(function CommitItem({
  commit,
  allBranches,
  repoPath,
  row,
  graphWidth,
  onContextMenu,
  onDoubleClick,
}: {
  commit: GitCommitInfo;
  allBranches: BranchInfo[];
  repoPath: string;
  row: GraphRow;
  graphWidth: number;
  onContextMenu: (e: React.MouseEvent, repoPath: string, commit: GitCommitInfo) => void;
  onDoubleClick: (repoPath: string, commit: GitCommitInfo) => void;
}) {
  const t = useT();
  const commitBranches = allBranches.filter((b) => b.commitHash === commit.hash);
  return (
    <div
      className="flex items-stretch cursor-pointer hover:bg-[var(--border-subtle)] rounded-[var(--radius-sm)] transition-colors duration-100 px-2"
      style={{ height: `${GRAPH_ROW_HEIGHT}px` }}
      title={commit.body ? `${commit.message}\n\n${commit.body}` : commit.message}
      onContextMenu={(e) => onContextMenu(e, repoPath, commit)}
      onDoubleClick={() => onDoubleClick(repoPath, commit)}
    >
      <CommitGraphCell row={row} width={graphWidth} />
      <div className="flex-1 min-w-0 flex flex-col justify-center pl-1">
        <div className="text-sm text-[var(--text-primary)] flex items-center gap-1 min-w-0">
          {commitBranches.map((b) => (
            <span
              key={b.name}
              className="inline-flex items-center shrink-0 text-sm leading-[18px] px-1.5 rounded font-medium"
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
          <span className="truncate max-w-[140px]">{commit.author}</span>
          <span>&middot;</span>
          <span className="shrink-0">{formatRelativeTime(commit.timestamp)}</span>
          <span>&middot;</span>
          <span className="font-mono shrink-0">{commit.shortHash}</span>
        </div>
      </div>
    </div>
  );
});

/** 选中仓库的 commit 列表——拓扑图布局按整份列表统一计算 */
const RepoCommitList = memo(function RepoCommitList({
  commits,
  allBranches,
  viewBranch,
  repoPath,
  onContextMenu,
  onDoubleClick,
}: {
  commits: GitCommitInfo[];
  allBranches: BranchInfo[];
  /** 正在查看(未 checkout)的分支名;undefined = 跟随 HEAD */
  viewBranch?: string;
  repoPath: string;
  onContextMenu: (e: React.MouseEvent, repoPath: string, commit: GitCommitInfo) => void;
  onDoubleClick: (repoPath: string, commit: GitCommitInfo) => void;
}) {
  const graph = useMemo(() => computeGitGraph(commits), [commits]);
  // 只标注本仓库/worktree 自己检出的分支(以及正在查看的分支)。
  // worktree 与主仓库共享 refs,标出全部分支会把其他工作区的分支、
  // 远程分支全挂到 commit 上,看起来像本工作区持有它们。
  const shownBranches = useMemo(
    () => allBranches.filter((b) => b.isHead || b.name === viewBranch),
    [allBranches, viewBranch],
  );
  return (
    <>
      {commits.map((commit, i) => (
        <CommitItem
          key={commit.hash}
          commit={commit}
          allBranches={shownBranches}
          repoPath={repoPath}
          row={graph.rows[i]}
          graphWidth={graph.width}
          onContextMenu={onContextMenu}
          onDoubleClick={onDoubleClick}
        />
      ))}
    </>
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
  /** 选中仓库路径;空串 = 项目里没发现仓库 */
  repoPath: string;
  /** 选中仓库的分支列表(容器加载;commit 行标注 HEAD/查看分支用) */
  branches: BranchInfo[];
  /** 正在查看(未 checkout)的分支;undefined = 跟随 HEAD */
  viewBranch?: string;
  /** git 活动时让容器同步刷新仓库列表与分支徽章 */
  refreshRepos: () => void;
}

/**
 * 选中仓库的提交历史(平铺列表)。仓库选择、分支切换、pull/push 都在容器
 * (GitHistory)顶部的仓库栏上,这里只管取数、分页与渲染。
 */
export function GitHistoryContent({ repoPath, branches, viewBranch, refreshRepos }: GitHistoryContentProps) {
  const t = useT();
  const [commits, setCommits] = useState<GitCommitInfo[]>([]);
  const [loading, setLoading] = useState(false);
  const [hasMore, setHasMore] = useState(true);
  const [diffModal, setDiffModal] = useState<{
    open: boolean;
    repoPath: string;
    commitHash: string;
    commitMessage: string;
    files: CommitFileInfo[];
  } | null>(null);
  const [heldDiff, diffOpen] = useOverlayValue(diffModal);

  const scrollRef = useRef<HTMLDivElement>(null);
  const commitsRef = useRef(commits);
  commitsRef.current = commits;
  const hasMoreRef = useRef(hasMore);
  hasMoreRef.current = hasMore;
  const loadingRef = useRef(false);
  // 换仓库/换分支时在途请求作废:令牌不匹配的迟到响应直接丢弃
  const reqIdRef = useRef(0);

  const load = useCallback(
    async (beforeCommit?: string) => {
      if (!repoPath || loadingRef.current) return;
      const id = reqIdRef.current;
      loadingRef.current = true;
      setLoading(true);
      try {
        const page = await invoke<GitCommitInfo[]>('get_git_log', {
          repoPath,
          beforeCommit: beforeCommit ?? null,
          limit: 30,
          // 分页从上一页末尾 commit 的 parent 续走,不需要 branch 参数
          branch: beforeCommit ? null : (viewBranch ?? null),
        });
        if (id !== reqIdRef.current) return;
        if (!beforeCommit) {
          setCommits(page);
          setHasMore(page.length >= 30);
        } else {
          // 分页是从上一页末尾 commit 的 parent 重新 revwalk 的，有分支时会带回
          // 已经加载过的 commit。重复 hash 会让拓扑图的连线算错，按 hash 去重；
          // 若整页都是重复的则停止分页，避免用同一个游标反复请求。
          const seen = new Set(commitsRef.current.map((c) => c.hash));
          const merged = [...commitsRef.current, ...page.filter((c) => !seen.has(c.hash))];
          setHasMore(page.length >= 30 && merged.length > commitsRef.current.length);
          setCommits(merged);
        }
      } catch {
        // 加载失败保持现有列表
      } finally {
        if (id === reqIdRef.current) {
          loadingRef.current = false;
          setLoading(false);
        }
      }
    },
    [repoPath, viewBranch],
  );

  // 仓库或查看分支变化 → 作废在途请求、清空并重载
  useEffect(() => {
    reqIdRef.current++;
    loadingRef.current = false;
    setCommits([]);
    setHasMore(true);
    setLoading(false);
    load();
  }, [load]);

  const handleScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    if (el.scrollTop + el.clientHeight < el.scrollHeight - 50) return;
    if (!hasMoreRef.current || loadingRef.current) return;
    const last = commitsRef.current[commitsRef.current.length - 1];
    if (last) load(last.hash);
  }, [load]);

  const handleViewDiff = useCallback(async (repo: string, commit: GitCommitInfo) => {
    try {
      const files = await invoke<CommitFileInfo[]>('get_commit_files', {
        repoPath: repo,
        commitHash: commit.hash,
      });
      setDiffModal({
        open: true,
        repoPath: repo,
        commitHash: commit.hash,
        commitMessage: commit.message,
        files,
      });
    } catch (e) {
      console.error('get_commit_files failed:', e);
    }
  }, []);

  const handleCommitContextMenu = useCallback(
    (e: React.MouseEvent, repo: string, commit: GitCommitInfo) => {
      e.preventDefault();
      showContextMenu(e.clientX, e.clientY, [
        {
          label: t('gitHistoryContent.copyCommitHash'),
          onClick: () => writeText(commit.hash),
        },
        { separator: true },
        {
          label: t('gitHistoryContent.viewChanges'),
          onClick: () => handleViewDiff(repo, commit),
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
      load();
    }, 500);
  }, [refreshRepos, load]);

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

  return (
    <div className="h-full bg-[var(--bg-surface)] flex flex-col">
      <div className="flex-1 overflow-y-auto px-1 py-1" ref={scrollRef} onScroll={handleScroll}>
        {!repoPath ? (
          <div className="text-center text-[var(--text-muted)] text-sm py-6">
            {t('gitHistoryContent.noRepos')}
          </div>
        ) : (
          <>
            {commits.length > 0 && (
              <RepoCommitList
                commits={commits}
                allBranches={branches}
                viewBranch={viewBranch}
                repoPath={repoPath}
                onContextMenu={handleCommitContextMenu}
                onDoubleClick={handleViewDiff}
              />
            )}

            {loading && (
              <div className="text-center text-[var(--text-muted)] text-xs py-2">
                {t('gitHistoryContent.loading')}
              </div>
            )}

            {!loading && commits.length === 0 && (
              <div className="text-center text-[var(--text-muted)] text-xs py-2">
                {t('gitHistoryContent.noCommits')}
              </div>
            )}
          </>
        )}
      </div>

      {/* 置空后再多留一会儿（useOverlayValue），退场动画才播得完 */}
      {heldDiff && (
        <CommitDiffModal
          open={diffOpen && heldDiff.open}
          onClose={() => setDiffModal(null)}
          repoPath={heldDiff.repoPath}
          commitHash={heldDiff.commitHash}
          commitMessage={heldDiff.commitMessage}
          files={heldDiff.files}
        />
      )}
    </div>
  );
}
