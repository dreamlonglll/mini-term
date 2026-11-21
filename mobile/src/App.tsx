import { useState } from 'react';
import { useRelayStore } from './relay';
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

export function App() {
  const t = useT();
  const phase = useRelayStore((s) => s.phase);
  const rejectReason = useRelayStore((s) => s.rejectReason);
  const savedDesktops = useRelayStore((s) => s.savedDesktops);
  const currentDesktopId = useRelayStore((s) => s.currentDesktopId);
  const mirrorOpen = useRelayStore((s) => s.mirror !== null);
  const historyPreviewOpen = useRelayStore((s) => s.historyPreview !== null);
  const historyListOpen = useRelayStore((s) => s.historyList !== null);
  const [desktopSheetOpen, setDesktopSheetOpen] = useState(false);

  // 只要已保存过电脑或正连着某台电脑,顶栏就展示设备切换入口(含 revoked/rejected 态,防卡死)
  const showDesktopTrigger = savedDesktops.length > 0 || phase === 'connected';
  const hasOtherDesktop = savedDesktops.some((d) => d.desktopId !== currentDesktopId);

  let body;
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
    case 'connected':
      body = mirrorOpen ? (
        <MirrorView />
      ) : historyPreviewOpen ? (
        <HistoryPreviewView />
      ) : historyListOpen ? (
        <HistoryListView />
      ) : (
        <SessionList />
      );
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
      <main className={`app-main ${phase === 'connected' ? 'list-mode' : ''}`}>{body}</main>
      {desktopSheetOpen && <DesktopSheet onClose={() => setDesktopSheetOpen(false)} />}
    </div>
  );
}
