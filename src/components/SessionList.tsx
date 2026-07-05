import { useEffect, useState, useCallback, useRef, useMemo } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import { showContextMenu } from '../utils/contextMenu';
import { isWslPath } from '../utils/wslPath';
import { SessionViewerModal } from './SessionViewerModal';
import { useT, t } from '../i18n';
import type { AiSession, ProjectConfig } from '../types';

const PAGE_SIZE = 20;

/** 将 ISO 时间戳转换为简短的相对/绝对时间 */
function formatTime(iso: string): string {
  if (!iso) return '';
  const date = new Date(iso);
  if (isNaN(date.getTime())) return '';

  const now = Date.now();
  const diff = now - date.getTime();
  const minutes = Math.floor(diff / 60000);

  if (minutes < 1) return t('sessionList.time.justNow');
  if (minutes < 60) return t('sessionList.time.minutesAgo', { n: minutes });

  const hours = Math.floor(minutes / 60);
  if (hours < 24) return t('sessionList.time.hoursAgo', { n: hours });

  const days = Math.floor(hours / 24);
  if (days < 7) return t('sessionList.time.daysAgo', { n: days });

  // 超过一周显示日期
  const m = date.getMonth() + 1;
  const d = date.getDate();
  const y = date.getFullYear();
  const currentYear = new Date().getFullYear();
  return y === currentYear ? t('sessionList.time.monthDay', { m, d }) : `${y}/${m}/${d}`;
}

const TYPE_BADGE: Record<string, { label: string; color: string }> = {
  claude: { label: 'C', color: 'var(--color-ai)' },
  codex: { label: 'X', color: 'var(--color-success)' },
};

/** 项目是否有 WSL 会话来源:WSL 根项目(UNC)自动启用,或显式配置了发行版 */
function hasWslSource(project: ProjectConfig): boolean {
  return isWslPath(project.path) || !!project.wslSessionsDistro;
}

export function SessionList() {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const activeProjectId = useAppStore((s) => s.activeProjectId);

  // Windows 宿主与 WSL 两个来源各自持有 state,渲染时合并排序(分段加载:宿主先出)
  const [hostSessions, setHostSessions] = useState<AiSession[]>([]);
  const [wslSessions, setWslSessions] = useState<AiSession[]>([]);
  const [displayCount, setDisplayCount] = useState(PAGE_SIZE);
  const [loading, setLoading] = useState(false);
  const [wslLoading, setWslLoading] = useState(false);
  const [viewingSession, setViewingSession] = useState<AiSession | null>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  // 请求序号:项目切换后旧请求(尤其是慢的 WSL 请求)返回时不得覆盖新项目的列表
  const requestIdRef = useRef(0);

  const activeProject = config.projects.find((p) => p.id === activeProjectId);

  // 有意不用 async/await:两个 invoke 并行发出,各自 then 落各自 state(分段加载)
  const fetchSessions = useCallback((project: ProjectConfig, force: boolean) => {
    const reqId = ++requestIdRef.current;

    // SSH 远程项目:仅远程来源(本地 get_ai_sessions 对远程 POSIX 路径无意义,
    // WSL 来源与远程互斥)。失败(含断链)后端静默降级返回空列表。
    if (project.sshConnectionId) {
      setWslSessions([]);
      setWslLoading(false);
      setLoading(true);
      invoke<AiSession[]>('ssh_remote_ai_sessions', {
        connectionId: project.sshConnectionId,
        projectPath: project.path,
        force,
      })
        .then((result) => {
          if (requestIdRef.current !== reqId) return;
          setHostSessions(result);
          setDisplayCount(PAGE_SIZE);
        })
        .catch(() => {
          if (requestIdRef.current !== reqId) return;
          setHostSessions([]);
        })
        .finally(() => {
          if (requestIdRef.current !== reqId) return;
          setLoading(false);
        });
      return;
    }

    // Windows 宿主来源:照旧秒出先显示
    setLoading(true);
    invoke<AiSession[]>('get_ai_sessions', { projectPath: project.path })
      .then((result) => {
        if (requestIdRef.current !== reqId) return;
        setHostSessions(result);
        setDisplayCount(PAGE_SIZE);
      })
      .catch(() => {
        if (requestIdRef.current !== reqId) return;
        setHostSessions([]);
      })
      .finally(() => {
        if (requestIdRef.current !== reqId) return;
        setLoading(false);
      });

    // WSL 来源:并行请求,到达后合并(9P + 可能的 VM 冷启动,秒级,不阻塞宿主显示)
    if (hasWslSource(project)) {
      setWslLoading(true);
      invoke<AiSession[]>('get_wsl_ai_sessions', {
        projectPath: project.path,
        distro: project.wslSessionsDistro,
        force,
      })
        .then((result) => {
          if (requestIdRef.current !== reqId) return;
          setWslSessions(result);
        })
        .catch(() => {
          if (requestIdRef.current !== reqId) return;
          setWslSessions([]);
        })
        .finally(() => {
          if (requestIdRef.current !== reqId) return;
          setWslLoading(false);
        });
    } else {
      setWslSessions([]);
      setWslLoading(false);
    }
  }, []);

  useEffect(() => {
    if (activeProject?.path) {
      // 项目切换 / 来源配置变化:先清掉上一来源的条目,避免与新项目混排
      setWslSessions([]);
      setHostSessions([]);
      fetchSessions(activeProject, false);
    } else {
      requestIdRef.current++;
      setHostSessions([]);
      setWslSessions([]);
      setLoading(false);
      setWslLoading(false);
      setDisplayCount(PAGE_SIZE);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeProject?.path, activeProject?.wslSessionsDistro, activeProject?.sshConnectionId, fetchSessions]);

  // 合并两个来源,按时间戳降序混排(与后端排序口径一致:ISO 8601 字符串比较)
  const allSessions = useMemo(() => {
    if (wslSessions.length === 0) return hostSessions;
    const merged = [...hostSessions, ...wslSessions];
    merged.sort((a, b) => b.timestamp.localeCompare(a.timestamp));
    return merged;
  }, [hostSessions, wslSessions]);

  const visibleSessions = allSessions.slice(0, displayCount);
  const hasMore = displayCount < allSessions.length;

  const handleScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el || !hasMore || loading) return;
    if (el.scrollTop + el.clientHeight >= el.scrollHeight - 40) {
      setDisplayCount((c) => Math.min(c + PAGE_SIZE, allSessions.length));
    }
  }, [hasMore, loading, allSessions.length]);

  return (
    <div className="h-full flex flex-col overflow-hidden bg-[var(--bg-surface)] select-none">
      <div className="px-3 pt-2.5 pb-1.5 text-sm text-[var(--text-muted)] uppercase tracking-[0.12em] font-medium flex items-center justify-between">
        <span className="flex items-center gap-1.5">
          Sessions
          {wslLoading && (
            <span
              className="inline-block w-3 h-3 border border-[var(--text-muted)] border-t-transparent rounded-full animate-spin"
              title={t('sessionList.wslLoading')}
            />
          )}
          {loading && !!activeProject?.sshConnectionId && (
            <span
              className="inline-block w-3 h-3 border border-[var(--text-muted)] border-t-transparent rounded-full animate-spin"
              title={t('sessionList.remoteLoading')}
            />
          )}
        </span>
        {activeProject && (
          <span
            className="text-xs normal-case tracking-normal cursor-pointer hover:text-[var(--text-primary)] transition-colors"
            onClick={() => fetchSessions(activeProject, true)}
            title={t('sessionList.refresh')}
          >
            ↻
          </span>
        )}
      </div>

      <div ref={scrollRef} className="flex-1 overflow-y-auto px-1.5" onScroll={handleScroll}>
        {loading && allSessions.length === 0 && (
          <div className="px-2.5 py-3 text-xs text-[var(--text-muted)] text-center">{t('sessionList.loading')}</div>
        )}

        {!loading && allSessions.length === 0 && (
          <div className="px-2.5 py-3 text-xs text-[var(--text-muted)] text-center">
            {activeProject ? t('sessionList.empty') : t('sessionList.selectProject')}
          </div>
        )}

        <SessionViewerModal
          open={!!viewingSession}
          onClose={() => setViewingSession(null)}
          session={viewingSession}
          projectPath={activeProject?.path ?? ''}
        />

        {visibleSessions.map((session) => {
          const badge = TYPE_BADGE[session.sessionType] ?? TYPE_BADGE.claude;
          // 远程会话标识:显示来源连接名(连接被删时回退 'SSH')
          const remoteConnName = session.sshConnectionId
            ? (config.sshConnections.find((c) => c.id === session.sshConnectionId)?.name ?? 'SSH')
            : undefined;

          return (
            <div
              key={`${session.sessionType}-${session.wslDistro ?? session.sshConnectionId ?? 'host'}-${session.id}`}
              className="flex items-start gap-2 px-2.5 py-1.5 rounded-[var(--radius-sm)] text-xs group hover:bg-[var(--border-subtle)] transition-colors cursor-default"
              title={session.title}
              onContextMenu={(e) => {
                e.preventDefault();
                e.stopPropagation();
                const cmd = session.sessionType === 'claude'
                  ? `claude --resume ${session.id}`
                  : `codex resume ${session.id}`;
                showContextMenu(e.clientX, e.clientY, [
                  {
                    label: t('sessionList.view'),
                    onClick: () => setViewingSession(session),
                  },
                  { separator: true },
                  {
                    label: t('sessionList.copyResumeCommand'),
                    onClick: () => navigator.clipboard.writeText(cmd),
                  },
                ]);
              }}
            >
              {/* 类型徽标 */}
              <span
                className="flex-shrink-0 w-4 h-4 rounded flex items-center justify-center text-[10px] font-bold mt-0.5"
                style={{ backgroundColor: badge.color + '22', color: badge.color }}
              >
                {badge.label}
              </span>

              {/* 标题 + 时间 */}
              <div className="flex-1 min-w-0">
                <div className="truncate text-[var(--text-secondary)] group-hover:text-[var(--text-primary)] transition-colors leading-snug">
                  {session.title}
                </div>
                <div className="text-[var(--text-muted)] text-[10px] mt-0.5 leading-none">
                  {formatTime(session.timestamp)}
                  {session.wslDistro && (
                    <span className="ml-1.5 opacity-70" title={session.wslDistro}>
                      {t('sessionList.wslBadge')}
                    </span>
                  )}
                  {remoteConnName && (
                    <span
                      className="ml-1.5 opacity-70"
                      title={t('sessionList.remoteBadgeTitle', { name: remoteConnName })}
                    >
                      {remoteConnName}
                    </span>
                  )}
                </div>
              </div>
            </div>
          );
        })}

        {hasMore && (
          <div className="px-2.5 py-2 text-[10px] text-[var(--text-muted)] text-center">
            {t('sessionList.more', { n: allSessions.length - displayCount })}
          </div>
        )}
      </div>
    </div>
  );
}
