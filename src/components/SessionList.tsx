import { useEffect, useState, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import { showContextMenu } from '../utils/contextMenu';
import { SessionViewerModal } from './SessionViewerModal';
import { useT, t } from '../i18n';
import type { AiSession } from '../types';

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

export function SessionList() {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const activeProjectId = useAppStore((s) => s.activeProjectId);

  const [allSessions, setAllSessions] = useState<AiSession[]>([]);
  const [displayCount, setDisplayCount] = useState(PAGE_SIZE);
  const [loading, setLoading] = useState(false);
  const [viewingSession, setViewingSession] = useState<AiSession | null>(null);
  const scrollRef = useRef<HTMLDivElement>(null);

  const activeProject = config.projects.find((p) => p.id === activeProjectId);

  const fetchSessions = useCallback(async (projectPath: string) => {
    setLoading(true);
    try {
      const result = await invoke<AiSession[]>('get_ai_sessions', { projectPath });
      setAllSessions(result);
      setDisplayCount(PAGE_SIZE);
    } catch {
      setAllSessions([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (activeProject?.path) {
      fetchSessions(activeProject.path);
    } else {
      setAllSessions([]);
      setDisplayCount(PAGE_SIZE);
    }
  }, [activeProject?.path, fetchSessions]);

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
        <span>Sessions</span>
        {activeProject && (
          <span
            className="text-xs normal-case tracking-normal cursor-pointer hover:text-[var(--text-primary)] transition-colors"
            onClick={() => fetchSessions(activeProject.path)}
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

          return (
            <div
              key={`${session.sessionType}-${session.id}`}
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
