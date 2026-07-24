import { openMirror, useRelayStore } from './relay';
import { useT } from './i18n';
import type { MobilePane } from './protocol';

const STATUS_CLASS: Record<string, string> = {
  'ai-working': 'dot-working',
  'ai-idle': 'dot-idle',
  error: 'dot-error',
};

function statusKey(status: string): string {
  switch (status) {
    case 'ai-working':
      return 'sessions.status.aiWorking';
    case 'ai-idle':
      return 'sessions.status.aiIdle';
    default:
      return 'sessions.status.error';
  }
}

function PaneRow({ pane }: { pane: MobilePane }) {
  const t = useT();
  return (
    <button className="pane-row" onClick={() => openMirror(pane.paneId, pane.title)}>
      <span className={`status-dot ${STATUS_CLASS[pane.status] ?? 'dot-error'}`} />
      <span className="pane-title">{pane.title}</span>
      <span className="pane-status">{t(statusKey(pane.status))}</span>
      <span className="pane-chevron">›</span>
    </button>
  );
}

/** 活跃 AI 会话列表:按项目分组;桌面端离线时置灰不可交互。 */
export function SessionList() {
  const t = useT();
  const projects = useRelayStore((s) => s.projects);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const offline = desktopOnline === false;

  return (
    <div className="session-list">
      {offline && (
        <div className="offline-banner">
          <div className="offline-title">{t('sessions.offlineBanner')}</div>
          <div className="offline-hint">{t('sessions.offlineHint')}</div>
        </div>
      )}
      <div className={`session-body ${offline ? 'inert' : ''}`}>
        {projects.length === 0 ? (
          <div className="sessions-empty">
            <div className="sessions-empty-title">{t('sessions.empty')}</div>
            <div className="sessions-empty-hint">{t('sessions.emptyHint')}</div>
          </div>
        ) : (
          projects.map((project) => (
            <section key={project.projectId} className="project-card">
              <h2 className="project-name">{project.name}</h2>
              {project.panes.map((pane) => (
                <PaneRow key={pane.paneId} pane={pane} />
              ))}
            </section>
          ))
        )}
      </div>
    </div>
  );
}
