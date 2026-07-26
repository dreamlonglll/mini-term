import { useEffect, useRef, useState } from 'react';
import { clearStartError, openMirror, refreshSessions, useRelayStore } from './relay';
import { useT } from './i18n';
import { StartSessionSheet } from './StartSessionSheet';
import { RenameSheet } from './RenameSheet';
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
  const [renaming, setRenaming] = useState(false);
  return (
    <>
      {/* 行是容器不是按钮:进镜像与改名是两个热区,按钮不能相互嵌套 */}
      <div className="pane-row">
        <button className="pane-open" onClick={() => openMirror(pane.paneId, pane.title)}>
          <span className={`status-dot ${STATUS_CLASS[pane.status] ?? 'dot-error'}`} />
          <span className="pane-title">{pane.title}</span>
          <span className="pane-status">{t(statusKey(pane.status))}</span>
          <span className="pane-chevron">›</span>
        </button>
        <button
          className="pane-rename"
          aria-label={t('sessions.rename.action')}
          title={t('sessions.rename.action')}
          onClick={() => setRenaming(true)}
        >
          ✎
        </button>
      </div>
      {renaming && (
        <RenameSheet
          paneId={pane.paneId}
          current={pane.title}
          onClose={() => setRenaming(false)}
        />
      )}
    </>
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

  // ── 下拉刷新 ──
  // 列表本身是实时同步的，但「怀疑它卡住了」时用户需要一个能自己动手的动作。
  const [pull, setPull] = useState(0);
  const [refreshing, setRefreshing] = useState(false);
  const pullStart = useRef<number | null>(null);
  const PULL_TRIGGER = 64;

  const onTouchStart = (e: React.TouchEvent) => {
    // 只在真正滚到顶时才接管手势，否则会和正常滚动打架
    const scroller = e.currentTarget.parentElement;
    if (scroller && scroller.scrollTop > 0) return;
    if (refreshing) return;
    pullStart.current = e.touches[0].clientY;
  };

  const onTouchMove = (e: React.TouchEvent) => {
    if (pullStart.current === null) return;
    const delta = e.touches[0].clientY - pullStart.current;
    if (delta <= 0) {
      setPull(0);
      return;
    }
    // 阻尼：越拉越沉，避免手指一滑就整屏位移
    setPull(Math.min(delta * 0.5, PULL_TRIGGER + 20));
  };

  const onTouchEnd = () => {
    if (pullStart.current === null) return;
    const shouldRefresh = pull >= PULL_TRIGGER;
    pullStart.current = null;
    setPull(0);
    if (!shouldRefresh) return;
    if (!refreshSessions()) return;
    setRefreshing(true);
    // 重连握手 + 全量快照通常在 1s 内；这里只是指示器的收尾，不是真等待
    setTimeout(() => setRefreshing(false), 1200);
  };

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
      <div
        className="pull-indicator"
        style={{ height: refreshing ? 36 : pull }}
        aria-hidden={!refreshing && pull === 0}
      >
        {refreshing ? (
          <><span className="spinner" />{t('sessions.refreshing')}</>
        ) : pull >= PULL_TRIGGER ? (
          t('sessions.releaseToRefresh')
        ) : pull > 0 ? (
          t('sessions.pullToRefresh')
        ) : null}
      </div>

      <div
        className={`session-body ${offline ? 'inert' : ''}`}
        onTouchStart={onTouchStart}
        onTouchMove={onTouchMove}
        onTouchEnd={onTouchEnd}
        onTouchCancel={onTouchEnd}
      >
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
