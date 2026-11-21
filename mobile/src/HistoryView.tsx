import { Fragment, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import {
  clearStartError,
  closeHistoryPreview,
  closeProjectHistory,
  findLivePaneForHistory,
  loadOlderHistoryPreview,
  openHistoryPreview,
  openProjectHistory,
  refreshProjectHistory,
  resumeAiSession,
  useRelayStore,
} from './relay';
import { useT } from './i18n';
import type { MirrorMessage, MobileHistorySession } from './protocol';
import { buildProjectTree, countProjects, type ProjectTreeNode } from './projectTree';
import { buildMirrorRows, type MirrorRow } from './mirrorRows';
import {
  InterruptRow,
  MessageRow,
  MirrorSkeleton,
  QuestionCard,
  ToolGroupRow,
} from './MirrorContent';

/** ISO 8601 → 相对时间(与桌面端 `session_panel::format_time` 同口径)。 */
function formatRelativeTime(
  iso: string,
  t: (key: string, params?: Record<string, string | number>) => string,
): string {
  if (!iso) return '';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '';
  const minutes = Math.floor((Date.now() - d.getTime()) / 60_000);
  if (minutes < 1) return t('history.time.justNow');
  if (minutes < 60) return t('history.time.minutesAgo', { n: minutes });
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return t('history.time.hoursAgo', { n: hours });
  const days = Math.floor(hours / 24);
  if (days < 7) return t('history.time.daysAgo', { n: days });
  const now = new Date();
  if (d.getFullYear() === now.getFullYear()) {
    return t('history.time.monthDay', { m: d.getMonth() + 1, d: d.getDate() });
  }
  return `${d.getFullYear()}/${d.getMonth() + 1}/${d.getDate()}`;
}

/** 首页「历史」Tab:按桌面端分组树列出全部项目,点项目进入其历史会话列表。 */
export function HistoryProjects() {
  const t = useT();
  const projects = useRelayStore((s) => s.projects);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(() => new Set<string>());
  const offline = desktopOnline === false;

  const tree = useMemo(() => buildProjectTree(projects), [projects]);

  const toggleGroup = (key: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (!next.delete(key)) next.add(key);
      return next;
    });
  };

  if (projects.length === 0) {
    return (
      <div className="sessions-empty">
        <div className="sessions-empty-title">{t('history.projectsEmpty')}</div>
      </div>
    );
  }

  const renderNode = (node: ProjectTreeNode, depth: number) => {
    const indent = { paddingLeft: `${14 + depth * 16}px` };
    if (node.kind === 'project') {
      const { project } = node;
      const supported = project.canStartSession;
      return (
        <button
          key={project.projectId}
          type="button"
          className="history-proj-row"
          style={indent}
          disabled={!supported || offline}
          onClick={() => openProjectHistory(project.projectId, project.name)}
        >
          <span className="history-proj-name">{project.name}</span>
          {supported ? (
            <>
              {project.panes.length > 0 && (
                <span className="history-proj-active">{project.panes.length}</span>
              )}
              <span className="pane-chevron">›</span>
            </>
          ) : (
            <span className="sheet-row-note">{t('history.projectUnsupported')}</span>
          )}
        </button>
      );
    }

    const isCollapsed = collapsed.has(node.key);
    return (
      <Fragment key={node.key}>
        <button
          type="button"
          className="sheet-group"
          style={indent}
          aria-expanded={!isCollapsed}
          onClick={() => toggleGroup(node.key)}
        >
          <span className={`sheet-group-arrow ${isCollapsed ? 'is-collapsed' : ''}`}>▾</span>
          <span className="sheet-group-name">{node.name}</span>
          <span className="sheet-group-count">({countProjects(node)})</span>
        </button>
        {!isCollapsed && node.children.map((child) => renderNode(child, depth + 1))}
      </Fragment>
    );
  };

  return <section className="project-card">{tree.map((node) => renderNode(node, 0))}</section>;
}

/** 历史会话列表的一行:点行进入只读预览,右侧快捷按钮可直接「继续」或「进入实时对话」。 */
function HistorySessionRow({
  projectId,
  projectName,
  session,
}: {
  projectId: string;
  projectName: string;
  session: MobileHistorySession;
}) {
  const t = useT();
  const projects = useRelayStore((s) => s.projects);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const starting = useRelayStore((s) => s.starting);
  const offline = desktopOnline === false;
  const live = findLivePaneForHistory(projects, session.livePaneId);
  const timeText = formatRelativeTime(session.timestamp, t);
  const canAct = live !== undefined || (session.canResume && !offline && !starting);

  return (
    <div className="history-session-row">
      <button
        type="button"
        className="history-session-open"
        disabled={offline}
        onClick={() => openHistoryPreview(projectId, projectName, session)}
      >
        <div className="history-session-title">{session.title || session.agentLabel}</div>
        <div className="history-session-meta">
          <span className="history-agent-badge">{session.agentLabel}</span>
          {live && (
            <span className="history-live-badge">
              <span
                className={`status-dot ${
                  live.needsAttention
                    ? 'dot-attention'
                    : live.status === 'ai-working'
                      ? 'dot-working'
                      : 'dot-idle'
                }`}
              />
              {t('history.runningBadge')}
            </span>
          )}
          {timeText && <span className="history-session-time">{timeText}</span>}
        </div>
      </button>
      {(live || session.canResume) && (
        <button
          type="button"
          className={`history-quick-btn${live ? ' is-live' : ''}`}
          disabled={!canAct}
          onClick={() => resumeAiSession(projectId, projectName, session)}
        >
          {t(live ? 'history.openLiveAction' : 'history.continueAction')}
        </button>
      )}
    </div>
  );
}

/** 项目历史会话列表页 */
export function HistoryListView() {
  const t = useT();
  const historyList = useRelayStore((s) => s.historyList);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const starting = useRelayStore((s) => s.starting);
  const startError = useRelayStore((s) => s.startError);
  const [query, setQuery] = useState('');
  const offline = desktopOnline === false;

  useEffect(() => {
    if (!startError) return;
    const timer = setTimeout(clearStartError, 6000);
    return () => clearTimeout(timer);
  }, [startError]);

  const needle = query.trim().toLowerCase();
  const sessions = historyList?.sessions ?? [];
  const filtered = useMemo(() => {
    if (!needle) return sessions;
    return sessions.filter(
      (s) =>
        s.title.toLowerCase().includes(needle) ||
        s.agent.toLowerCase().includes(needle) ||
        s.agentLabel.toLowerCase().includes(needle),
    );
  }, [sessions, needle]);

  if (!historyList) return null;

  return (
    <div className="history-view">
      <div className="mirror-header">
        <button type="button" className="mirror-back" onClick={closeProjectHistory}>
          ‹ {t('mirror.back')}
        </button>
        <div className="history-header-title">
          <span className="history-header-name">{historyList.projectName}</span>
          <span className="history-header-sub">{t('history.title')}</span>
        </div>
        <button
          type="button"
          className="history-refresh-btn"
          disabled={offline || historyList.loading}
          onClick={refreshProjectHistory}
        >
          {t('history.refresh')}
        </button>
      </div>

      {offline && (
        <div className="offline-banner">
          <div className="offline-title">{t('sessions.offlineBanner')}</div>
        </div>
      )}
      {starting && (
        <div className="start-banner">
          {t(starting.mode === 'resume' ? 'history.resuming' : 'start.starting', {
            project: starting.projectName,
          })}
        </div>
      )}
      {startError && (
        <div className="start-banner start-banner--error">{t(`start.error.${startError}`)}</div>
      )}

      {sessions.length > 0 && (
        <div className="history-search-bar">
          <input
            type="search"
            className="history-search-input"
            value={query}
            placeholder={t('history.searchPlaceholder')}
            onChange={(e) => setQuery(e.target.value)}
          />
        </div>
      )}

      <div className="history-scroll">
        {historyList.loading && sessions.length === 0 ? (
          <div className="history-state">{t('history.loading')}</div>
        ) : historyList.error && sessions.length === 0 ? (
          <div className="history-state">
            <div className="history-state-error">{t(`history.error.${historyList.error}`)}</div>
            <button
              type="button"
              className="history-retry-btn"
              disabled={offline}
              onClick={refreshProjectHistory}
            >
              {t('history.retry')}
            </button>
          </div>
        ) : filtered.length === 0 ? (
          <div className="history-state">
            {sessions.length === 0 ? t('history.empty') : t('history.emptyFiltered')}
          </div>
        ) : (
          <section className="project-card">
            {filtered.map((session) => (
              <HistorySessionRow
                key={`${session.agent}:${session.sessionId}`}
                projectId={historyList.projectId}
                projectName={historyList.projectName}
                session={session}
              />
            ))}
          </section>
        )}
      </div>
    </div>
  );
}

const EMPTY_MESSAGES: MirrorMessage[] = [];

/** 历史会话只读预览页:复用对话镜像渲染 + 上拉加载更早 + 底部「继续对话」操作条。 */
export function HistoryPreviewView() {
  const t = useT();
  const preview = useRelayStore((s) => s.historyPreview);
  const projects = useRelayStore((s) => s.projects);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const starting = useRelayStore((s) => s.starting);
  const startError = useRelayStore((s) => s.startError);
  const scrollRef = useRef<HTMLDivElement>(null);
  const historyAnchor = useRef<{ head: MirrorMessage; height: number } | null>(null);
  const scrolledInitial = useRef(false);
  const rowsCache = useRef<MirrorRow[]>([]);
  const [expandedTools, setExpandedTools] = useState<ReadonlySet<number>>(() => new Set());
  const offline = desktopOnline === false;

  useEffect(() => {
    if (!startError) return;
    const timer = setTimeout(clearStartError, 6000);
    return () => clearTimeout(timer);
  }, [startError]);

  const messages = preview?.messages ?? EMPTY_MESSAGES;
  const loaded = preview?.loaded ?? false;
  const loadingOlder = preview?.loadingOlder ?? false;
  const rows = useMemo(() => buildMirrorRows(messages, rowsCache.current), [messages]);

  useLayoutEffect(() => {
    rowsCache.current = rows;
  }, [rows]);

  const toggleTools = useCallback((firstSeq: number) => {
    setExpandedTools((prev) => {
      const next = new Set(prev);
      if (!next.delete(firstSeq)) next.add(firstSeq);
      return next;
    });
  }, []);

  const requestOlder = useCallback(() => {
    const el = scrollRef.current;
    const p = useRelayStore.getState().historyPreview;
    if (!el || !p || !p.hasMore || p.requestId !== null || p.messages.length === 0) return;
    historyAnchor.current = { head: p.messages[0], height: el.scrollHeight };
    loadOlderHistoryPreview();
  }, []);

  // 首屏加载完成后滚到底部看最近对话;上拉加载更早消息后补偿保持阅读位置
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const anchor = historyAnchor.current;
    if (anchor) {
      if (messages[0] !== anchor.head) {
        historyAnchor.current = null;
        if (messages.includes(anchor.head)) {
          const height = el.scrollHeight;
          el.scrollTop += height - anchor.height;
          requestAnimationFrame(() => {
            const node = scrollRef.current;
            if (node && node.scrollHeight !== height) {
              node.scrollTop += node.scrollHeight - height;
            }
          });
          return;
        }
      } else if (!loadingOlder) {
        historyAnchor.current = null;
      } else {
        anchor.height = el.scrollHeight;
      }
    }
    if (loaded && !scrolledInitial.current) {
      scrolledInitial.current = true;
      el.scrollTop = el.scrollHeight;
    }
  }, [messages, loaded, loadingOlder]);

  if (!preview) return null;

  const live = findLivePaneForHistory(projects, preview.session.livePaneId);
  const canResume = preview.session.canResume;
  const actionDisabled = live ? false : !canResume || offline || starting !== null;

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    if (el.scrollTop < 30) requestOlder();
  };

  const renderRow = (row: MirrorRow) => {
    switch (row.kind) {
      case 'question':
        return (
          <QuestionCard
            key={row.key}
            msg={row.msg}
            active={false}
            marker={row.marker}
            readOnly
          />
        );
      case 'tools':
        return (
          <ToolGroupRow
            key={row.key}
            firstSeq={row.key}
            tools={row.tools}
            expanded={expandedTools.has(row.key)}
            onToggle={toggleTools}
          />
        );
      case 'interrupt':
        return <InterruptRow key={row.key} msg={row.msg} />;
      default:
        return <MessageRow key={row.key} msg={row.msg} />;
    }
  };

  return (
    <div className="mirror-view">
      <div className="mirror-header">
        <button type="button" className="mirror-back" onClick={closeHistoryPreview}>
          ‹ {t('mirror.back')}
        </button>
        <div className="history-header-title">
          <span className="history-header-name">
            {preview.session.title || preview.session.agentLabel}
          </span>
          <span className="history-header-sub">
            {preview.projectName} · {preview.session.agentLabel}
          </span>
        </div>
      </div>

      {offline && (
        <div className="offline-banner">
          <div className="offline-title">{t('sessions.offlineBanner')}</div>
        </div>
      )}
      {starting && (
        <div className="start-banner">
          {t(starting.mode === 'resume' ? 'history.resuming' : 'start.starting', {
            project: starting.projectName,
          })}
        </div>
      )}
      {startError && (
        <div className="start-banner start-banner--error">{t(`start.error.${startError}`)}</div>
      )}

      <div className="mirror-scroll" ref={scrollRef} onScroll={onScroll}>
        {preview.hasMore && (
          <button
            type="button"
            className="mirror-load-older"
            onClick={requestOlder}
            disabled={preview.loadingOlder}
          >
            {preview.loadingOlder ? t('mirror.loading') : t('mirror.loadOlder')}
          </button>
        )}
        {!preview.loaded && !preview.error ? (
          <MirrorSkeleton label={t('mirror.loading')} />
        ) : preview.error && rows.length === 0 ? (
          <div className="history-state">
            <div className="history-state-error">{t(`history.error.${preview.error}`)}</div>
            <button
              type="button"
              className="history-retry-btn"
              disabled={offline}
              onClick={() =>
                openHistoryPreview(preview.projectId, preview.projectName, preview.session)
              }
            >
              {t('history.retry')}
            </button>
          </div>
        ) : rows.length === 0 ? (
          <div className="mirror-empty">{t('history.previewEmpty')}</div>
        ) : (
          rows.map(renderRow)
        )}
      </div>

      <div className="history-preview-footer">
        <button
          type="button"
          className="history-resume-btn"
          disabled={actionDisabled}
          onClick={() => resumeAiSession(preview.projectId, preview.projectName, preview.session)}
        >
          {live
            ? t('history.openLiveConversation')
            : !canResume
              ? t('history.resumeUnsupported')
              : starting
                ? t('history.resuming', { project: preview.projectName })
                : t('history.continueConversation')}
        </button>
      </div>
    </div>
  );
}
