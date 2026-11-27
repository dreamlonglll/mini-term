import { useRef, useState, useSyncExternalStore } from 'react';
import { retryConnectionNow, selectKeepMainView, useRelayStore } from './relay';
import { useI18nStore, useT } from './i18n';
import { DesktopSheet, DesktopTrigger } from './DesktopSwitcher';
import { SessionList } from './SessionList';
import { MirrorView } from './MirrorView';
import { HistoryListView, HistoryPreviewView } from './HistoryView';

function LanguageToggle() {
  const lang = useI18nStore((s) => s.lang);
  const setLang = useI18nStore((s) => s.setLang);
  return (
    <div className="lang-toggle">
      <button className={lang === 'zh' ? 'active' : ''} onClick={() => setLang('zh')}>中文</button>
      <button className={lang === 'en' ? 'active' : ''} onClick={() => setLang('en')}>EN</button>
    </div>
  );
}

function StatusScreen({
  icon,
  title,
  hint,
  spinning,
  actionLabel,
  onAction,
}: {
  icon: string;
  title: string;
  hint?: string;
  spinning?: boolean;
  actionLabel?: string;
  onAction?: () => void;
}) {
  return (
    <div className="status-screen">
      <div className={`status-icon ${spinning ? 'spinning' : ''}`}>{icon}</div>
      <div className="status-title">{title}</div>
      {hint && <div className="status-hint">{hint}</div>}
      {actionLabel && onAction && (
        <button type="button" className="status-action" onClick={onAction}>
          {actionLabel}
        </button>
      )}
    </div>
  );
}

function subscribeOnline(onChange: () => void) {
  window.addEventListener('online', onChange);
  window.addEventListener('offline', onChange);
  return () => {
    window.removeEventListener('online', onChange);
    window.removeEventListener('offline', onChange);
  };
}

/** navigator.onLine 的订阅版(只当提示用:true 不保证真连得上,false 基本可信)。 */
function useOnline(): boolean {
  return useSyncExternalStore(
    subscribeOnline,
    () => navigator.onLine,
    () => true,
  );
}

/**
 * 主视图顶部的链路横幅:同一台桌面端断线重连 / 刷新期间不卸载列表与镜像页(草稿、
 * 展开的工具组、阅读位置都还在),只在这里说明连接状态;断网时换成断网提示。
 */
function LinkBanner() {
  const t = useT();
  const phase = useRelayStore((s) => s.phase);
  const online = useOnline();
  const reconnecting = phase === 'reconnecting';
  const text = !online
    ? t('pair.link.offline')
    : reconnecting
      ? t('pair.link.reconnecting')
      : t('pair.link.syncing');
  return (
    <div className={`link-banner${online ? '' : ' is-offline'}`} role="status" aria-live="polite">
      {online && <span className="link-banner-spinner" aria-hidden="true" />}
      <span className="link-banner-text">{text}</span>
      {online && reconnecting && (
        <button type="button" className="link-banner-retry" onClick={() => retryConnectionNow()}>
          {t('pair.link.retry')}
        </button>
      )}
    </div>
  );
}

export function App() {
  const t = useT();
  const phase = useRelayStore((s) => s.phase);
  const rejectReason = useRelayStore((s) => s.rejectReason);
  const savedDesktops = useRelayStore((s) => s.savedDesktops);
  const currentDesktopId = useRelayStore((s) => s.currentDesktopId);
  const mirrorOpen = useRelayStore((s) => s.mirror !== null);
  const historyPreviewOpen = useRelayStore((s) => s.historyPreview !== null);
  const historyListOpen = useRelayStore((s) => s.historyList !== null);
  // 首次连上之后,同一台桌面端的重连 / 刷新期间保持主视图挂载(旧数据照常显示)
  const keepMainView = useRelayStore(selectKeepMainView);
  const online = useOnline();
  const [desktopSheetOpen, setDesktopSheetOpen] = useState(false);
  // 主视图下真正的滚动容器(.app-main.list-mode),下拉刷新据它判断是否在顶部
  const mainRef = useRef<HTMLElement>(null);

  const showMainView = phase === 'connected' || keepMainView;
  // 只要已保存过电脑或正连着某台电脑,顶栏就展示设备切换入口(含 revoked/rejected 态,防卡死)
  const showDesktopTrigger = savedDesktops.length > 0 || showMainView;
  const hasOtherDesktop = savedDesktops.some((d) => d.desktopId !== currentDesktopId);

  let body;
  if (showMainView) {
    body = mirrorOpen ? (
      <MirrorView />
    ) : historyPreviewOpen ? (
      <HistoryPreviewView />
    ) : historyListOpen ? (
      <HistoryListView />
    ) : (
      <SessionList scrollRef={mainRef} />
    );
  } else {
    switch (phase) {
      case 'idle':
        body = (
          <StatusScreen
            icon="▣"
            title={t('pair.appName')}
            hint={t('pair.scanHint')}
            actionLabel={savedDesktops.length > 0 ? t('pair.desktops.switchOther') : undefined}
            onAction={savedDesktops.length > 0 ? () => setDesktopSheetOpen(true) : undefined}
          />
        );
        break;
      case 'pairing':
        body = <StatusScreen icon="◌" title={t('pair.pairing')} spinning />;
        break;
      case 'connecting':
        body = <StatusScreen icon="◌" title={t('pair.connecting')} spinning />;
        break;
      case 'reconnecting':
        body = <StatusScreen icon="◌" title={t('pair.reconnecting')} spinning />;
        break;
      case 'revoked':
        body = (
          <StatusScreen
            icon="⊘"
            title={t('pair.revoked')}
            hint={t('pair.revokedHint')}
            actionLabel={hasOtherDesktop ? t('pair.desktops.switchOther') : undefined}
            onAction={hasOtherDesktop ? () => setDesktopSheetOpen(true) : undefined}
          />
        );
        break;
      case 'rejected':
        body = (
          <StatusScreen
            icon="⊘"
            title={t(`pair.rejected.${rejectReason ?? 'missingAuth'}`)}
            actionLabel={hasOtherDesktop ? t('pair.desktops.switchOther') : undefined}
            onAction={hasOtherDesktop ? () => setDesktopSheetOpen(true) : undefined}
          />
        );
        break;
    }
  }

  return (
    <div className="app">
      <header className="app-header">
        <div className="app-header-left">
          {showDesktopTrigger ? (
            <DesktopTrigger onOpen={() => setDesktopSheetOpen(true)} />
          ) : (
            <span className="app-name">{t('pair.appName')}</span>
          )}
        </div>
        <LanguageToggle />
      </header>
      {showMainView && (keepMainView || !online) && <LinkBanner />}
      <main ref={mainRef} className={`app-main ${showMainView ? 'list-mode' : ''}`}>
        {body}
      </main>
      {desktopSheetOpen && <DesktopSheet onClose={() => setDesktopSheetOpen(false)} />}
    </div>
  );
}
