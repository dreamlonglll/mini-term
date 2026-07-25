import { useEffect, useState } from 'react';
import { clearStartError, openMirror, useRelayStore } from './relay';
import { useT } from './i18n';
import { StartSessionSheet } from './StartSessionSheet';
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
  const launchers = useRelayStore((s) => s.launchers);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const starting = useRelayStore((s) => s.starting);
  const startError = useRelayStore((s) => s.startError);
  const [sheetOpen, setSheetOpen] = useState(false);
  const offline = desktopOnline === false;

  // 失败提示展示 6s 后自动消失(超时文案偏长,给足阅读时间)
  useEffect(() => {
    if (!startError) return;
    const timer = setTimeout(clearStartError, 6000);
    return () => clearTimeout(timer);
  }, [startError]);

  // 快照含全部项目(发起弹层要用),首页仍只渲染有活跃会话的那些
  const active = projects.filter((p) => p.panes.length > 0);

  // + 按钮不可用的原因,按优先级取第一条;null = 可用
  const disabledReason = offline
    ? 'offline'
    : launchers.length === 0
      ? 'noLaunchers'
      : starting
        ? 'starting'
        : null;

  return (
    <div className="session-list">
      {offline && (
        <div className="offline-banner">
          <div className="offline-title">{t('sessions.offlineBanner')}</div>
          <div className="offline-hint">{t('sessions.offlineHint')}</div>
        </div>
      )}
      {starting && (
        <div className="start-banner">{t('start.starting', { project: starting.projectName })}</div>
      )}
      {startError && (
        <div className="start-banner start-banner--error">{t(`start.error.${startError}`)}</div>
      )}
      <div className={`session-body ${offline ? 'inert' : ''}`}>
        {active.length === 0 ? (
          <div className="sessions-empty">
            <div className="sessions-empty-title">{t('sessions.empty')}</div>
            <div className="sessions-empty-hint">{t('sessions.emptyHint')}</div>
          </div>
        ) : (
          active.map((project) => (
            <section key={project.projectId} className="project-card">
              <h2 className="project-name">{project.name}</h2>
              {project.panes.map((pane) => (
                <PaneRow key={pane.paneId} pane={pane} />
              ))}
            </section>
          ))
        )}
      </div>

      <button
        className="fab"
        disabled={disabledReason !== null}
        title={disabledReason ? t(`start.disabled.${disabledReason}`) : t('start.fab')}
        aria-label={t('start.fab')}
        onClick={() => setSheetOpen(true)}
      >
        +
      </button>
      {disabledReason && disabledReason !== 'starting' && (
        <div className="fab-hint">{t(`start.disabled.${disabledReason}`)}</div>
      )}

      {sheetOpen && <StartSessionSheet onClose={() => setSheetOpen(false)} />}
    </div>
  );
}
