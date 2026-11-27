import { type RefObject, useEffect, useRef, useState } from 'react';
import {
  clearStartError,
  openMirror,
  openProjectHistory,
  refreshSessions,
  selectLinkDown,
  setHomeTab,
  useRelayStore,
} from './relay';
import { useT } from './i18n';
import { StartSessionSheet } from './StartSessionSheet';
import { RenameSheet } from './RenameSheet';
import { HistoryProjects } from './HistoryView';
import type { MobilePane } from './protocol';
import {
  PULL_TRIGGER,
  type PullAxis,
  classifyPullGesture,
  isAtScrollTop,
  pullDistance,
} from './pullGesture';

/** 刷新指示器至少转这么久(握手 + 快照常在几百毫秒内完成,一闪而过像没刷) */
const REFRESH_MIN_MS = 600;
/** 迟迟连不上时指示器最多转这么久,之后交给顶部的重连横幅 */
const REFRESH_MAX_MS = 10_000;

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
  // 重连中改名发不出去(renamePane 会拒),入口一并置灰;进镜像不受影响,重连后自动订阅
  const linkDown = useRelayStore(selectLinkDown);
  // 黄灯(提问待答/等待授权)展示优先于常规状态:用户此刻该做的事比 AI 在不在跑重要
  const attention = pane.needsAttention === true;
  return (
    <>
      {/* 行是容器不是按钮:进镜像与改名是两个热区,按钮不能相互嵌套 */}
      <div className="pane-row">
        <button className="pane-open" onClick={() => openMirror(pane.paneId, pane.title)}>
          <span
            className={`status-dot ${attention ? 'dot-attention' : (STATUS_CLASS[pane.status] ?? 'dot-error')}`}
          />
          <span className="pane-title">{pane.title}</span>
          <span className={`pane-status${attention ? ' pane-status-attention' : ''}`}>
            {t(attention ? 'sessions.status.attention' : statusKey(pane.status))}
          </span>
          <span className="pane-chevron">›</span>
        </button>
        <button
          className="pane-rename"
          aria-label={t('sessions.rename.action')}
          title={t('sessions.rename.action')}
          disabled={linkDown}
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

/**
 * 活跃 / 历史会话首页:桌面端离线时置灰不可交互。
 * `scrollRef` 是真正的滚动容器(App 的 `.app-main.list-mode`):列表本身不滚动。
 */
export function SessionList({ scrollRef }: { scrollRef: RefObject<HTMLElement | null> }) {
  const t = useT();
  const projects = useRelayStore((s) => s.projects);
  const launchers = useRelayStore((s) => s.launchers);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const supportsHistory = useRelayStore((s) => s.supportsHistory);
  const homeTab = useRelayStore((s) => s.homeTab);
  const starting = useRelayStore((s) => s.starting);
  const startError = useRelayStore((s) => s.startError);
  const phase = useRelayStore((s) => s.phase);
  const linkDown = phase !== 'connected';
  const [sheetOpen, setSheetOpen] = useState(false);
  const offline = desktopOnline === false;

  // ── 下拉刷新 ──
  // 列表本身是实时同步的，但「怀疑它卡住了」时用户需要一个能自己动手的动作。
  // 刷新 = 同一台桌面端重连一次;重连期间本组件保持挂载(App 按 establishedDesktopId 判),
  // 指示器一直转到重新连上为止。
  const [pull, setPull] = useState(0);
  const [pulling, setPulling] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  /** 这次手势的起点;null = 没在跟踪(没在顶部起手 / 已判定放弃) */
  const gesture = useRef<{ x: number; y: number; axis: PullAxis; pull: number } | null>(null);
  const refreshStartedAt = useRef(0);

  useEffect(() => {
    if (!refreshing) return;
    if (phase === 'connected') {
      const wait = Math.max(0, REFRESH_MIN_MS - (Date.now() - refreshStartedAt.current));
      const timer = setTimeout(() => setRefreshing(false), wait);
      return () => clearTimeout(timer);
    }
    const cap = setTimeout(
      () => setRefreshing(false),
      Math.max(0, REFRESH_MAX_MS - (Date.now() - refreshStartedAt.current)),
    );
    return () => clearTimeout(cap);
  }, [refreshing, phase]);

  const resetPull = () => {
    gesture.current = null;
    setPulling(false);
    setPull(0);
  };

  /** 滚动容器此刻是否在顶部;拿不到容器时按不在顶部处理(宁可不刷也别误刷) */
  const scrollerAtTop = () => {
    const scroller = scrollRef.current;
    return scroller !== null && isAtScrollTop(scroller.scrollTop);
  };

  const onTouchStart = (e: React.TouchEvent) => {
    // 只在真正滚到顶时才接管手势，否则会和正常滚动打架
    if (refreshing || e.touches.length !== 1 || !scrollerAtTop()) {
      gesture.current = null;
      return;
    }
    const touch = e.touches[0];
    gesture.current = { x: touch.clientX, y: touch.clientY, axis: 'pending', pull: 0 };
  };

  const onTouchMove = (e: React.TouchEvent) => {
    const g = gesture.current;
    if (!g) return;
    // 第二根手指落下(捏合)或列表已经离开顶部(起手后内容被滚动):放弃这次下拉
    if (e.touches.length !== 1 || !scrollerAtTop()) {
      resetPull();
      return;
    }
    const touch = e.touches[0];
    const dx = touch.clientX - g.x;
    const dy = touch.clientY - g.y;
    if (g.axis === 'pending') {
      g.axis = classifyPullGesture(dx, dy);
      if (g.axis === 'pending') return;
      if (g.axis === 'abandon') {
        // 横滑 / 往上滚:这次手势不是下拉刷新,松手前都不再接管
        resetPull();
        return;
      }
      setPulling(true);
    }
    g.pull = pullDistance(dy);
    setPull(g.pull);
  };

  const onTouchEnd = () => {
    const g = gesture.current;
    if (!g) return;
    const shouldRefresh = g.axis === 'pull' && g.pull >= PULL_TRIGGER;
    resetPull();
    if (!shouldRefresh) return;
    if (!refreshSessions()) return;
    refreshStartedAt.current = Date.now();
    setRefreshing(true);
  };

  // 失败提示展示 6s 后自动消失(超时文案偏长,给足阅读时间)
  useEffect(() => {
    if (!startError) return;
    const timer = setTimeout(clearStartError, 6000);
    return () => clearTimeout(timer);
  }, [startError]);

  // 快照含全部项目(发起弹层与历史 tab 要用),活跃页仍只渲染有活跃会话的那些
  const active = projects.filter((p) => p.panes.length > 0);
  const activePaneCount = active.reduce((sum, p) => sum + p.panes.length, 0);

  // + 按钮不可用的原因,按优先级取第一条;null = 可用
  const disabledReason = offline
    ? 'offline'
    : linkDown
      ? 'reconnecting'
      : launchers.length === 0
        ? 'noLaunchers'
        : starting
          ? 'starting'
          : null;
  // 按钮旁的说明:重连中顶部已有横幅、启动中有进行态横幅,不再重复
  const showFabHint =
    disabledReason !== null && disabledReason !== 'starting' && disabledReason !== 'reconnecting';

  return (
    <div className="session-list">
      {offline && (
        <div className="offline-banner">
          <div className="offline-title">{t('sessions.offlineBanner')}</div>
          <div className="offline-hint">{t('sessions.offlineHint')}</div>
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
      {supportsHistory && (
        <div className="home-tabs" role="tablist">
          <button
            type="button"
            role="tab"
            aria-selected={homeTab === 'active'}
            className={`home-tab${homeTab === 'active' ? ' is-active' : ''}`}
            onClick={() => setHomeTab('active')}
          >
            {t('history.tabs.active')}
            {activePaneCount > 0 && <span className="home-tab-count">{activePaneCount}</span>}
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={homeTab === 'history'}
            className={`home-tab${homeTab === 'history' ? ' is-active' : ''}`}
            onClick={() => setHomeTab('history')}
          >
            {t('history.tabs.history')}
          </button>
        </div>
      )}
      <div
        className={pulling ? 'pull-indicator is-pulling' : 'pull-indicator'}
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
        {supportsHistory && homeTab === 'history' ? (
          <HistoryProjects />
        ) : active.length === 0 ? (
          <div className="sessions-empty">
            <div className="sessions-empty-title">{t('sessions.empty')}</div>
            <div className="sessions-empty-hint">{t('sessions.emptyHint')}</div>
          </div>
        ) : (
          active.map((project) => (
            <section key={project.projectId} className="project-card">
              <div className="project-card-head">
                <h2 className="project-name">{project.name}</h2>
                {supportsHistory && project.canStartSession && (
                  <button
                    type="button"
                    className="project-history-btn"
                    disabled={offline || linkDown}
                    onClick={() => openProjectHistory(project.projectId, project.name)}
                  >
                    {t('history.entry')} ›
                  </button>
                )}
              </div>
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
      {showFabHint && <div className="fab-hint">{t(`start.disabled.${disabledReason}`)}</div>}

      {sheetOpen && <StartSessionSheet onClose={() => setSheetOpen(false)} />}
    </div>
  );
}
