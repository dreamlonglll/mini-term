import { useT } from './i18n';
import { useBackLayer } from './navigation';
import {
  type SavedDesktop,
  removeSavedDesktop,
  switchDesktop,
  useRelayStore,
} from './relay';

function formatDesktopLabel(
  desktopId: string | null,
  desktopName: string | null | undefined,
  defaultLabel: string,
): string {
  const trimmed = desktopName?.trim();
  if (trimmed) return trimmed;
  if (!desktopId || desktopId === 'default') return defaultLabel;
  return desktopId.length > 8 ? desktopId.slice(0, 8) : desktopId;
}

/** 顶栏紧凑设备胶囊按钮:展示当前桌面端名与状态,点击展开设备切换弹层。 */
export function DesktopTrigger({ onOpen }: { onOpen: () => void }) {
  const t = useT();
  const phase = useRelayStore((s) => s.phase);
  const desktopOnline = useRelayStore((s) => s.desktopOnline);
  const currentDesktopId = useRelayStore((s) => s.currentDesktopId);
  const currentDesktopName = useRelayStore((s) => s.currentDesktopName);
  const savedDesktops = useRelayStore((s) => s.savedDesktops);

  const label = formatDesktopLabel(
    currentDesktopId,
    currentDesktopName,
    t('pair.desktops.defaultName'),
  );

  const dotClass =
    phase === 'connected' && desktopOnline === true
      ? 'dot-online'
      : phase === 'revoked' || phase === 'rejected'
        ? 'dot-error'
        : phase === 'connected' && desktopOnline === false
          ? 'dot-offline'
          : 'dot-pending';

  return (
    <button
      type="button"
      className="desktop-trigger"
      onClick={onOpen}
      aria-label={t('pair.desktops.switchAction')}
    >
      <span className={`desktop-dot ${dotClass}`} />
      <span className="desktop-trigger-name">{label}</span>
      {savedDesktops.length > 1 && (
        <span className="desktop-trigger-count">{savedDesktops.length}</span>
      )}
      <span className="desktop-trigger-caret" aria-hidden="true">▾</span>
    </button>
  );
}

/** 底部设备切换抽屉:列出本机已配对的全部桌面端,支持一键切换与移除本地记录。 */
export function DesktopSheet({ onClose }: { onClose: () => void }) {
  const t = useT();
  const currentDesktopId = useRelayStore((s) => s.currentDesktopId);
  const savedDesktops = useRelayStore((s) => s.savedDesktops);
  // 返回手势先关弹层
  useBackLayer(true, onClose);

  const handleSelect = (desktop: SavedDesktop) => {
    switchDesktop(desktop.desktopId);
    onClose();
  };

  const handleRemove = (desktopId: string) => {
    removeSavedDesktop(desktopId);
    if (useRelayStore.getState().savedDesktops.length === 0) {
      onClose();
    }
  };

  return (
    <div className="sheet-backdrop" onClick={onClose}>
      <div className="sheet" onClick={(e) => e.stopPropagation()}>
        <div className="sheet-header">
          <span className="sheet-title">{t('pair.desktops.title')}</span>
          <button type="button" className="sheet-close" onClick={onClose}>
            {t('pair.desktops.close')}
          </button>
        </div>
        <div className="sheet-body">
          {savedDesktops.length === 0 ? (
            <div className="sheet-empty">{t('pair.desktops.empty')}</div>
          ) : (
            savedDesktops.map((desktop) => {
              const active = desktop.desktopId === currentDesktopId;
              const label = formatDesktopLabel(
                desktop.desktopId,
                desktop.desktopName,
                t('pair.desktops.defaultName'),
              );
              const shortId =
                desktop.desktopId === 'default'
                  ? 'default'
                  : desktop.desktopId.slice(0, 8);
              return (
                <div
                  key={desktop.desktopId}
                  className={`desktop-sheet-row${active ? ' is-active' : ''}`}
                >
                  <button
                    type="button"
                    className="desktop-sheet-pick"
                    onClick={() => handleSelect(desktop)}
                  >
                    <span className="desktop-sheet-check" aria-hidden="true">
                      {active ? '✓' : ''}
                    </span>
                    <span className="desktop-sheet-info">
                      <span className="desktop-sheet-name">{label}</span>
                      <span className="desktop-sheet-id">{shortId}</span>
                    </span>
                  </button>
                  <button
                    type="button"
                    className="desktop-sheet-remove"
                    aria-label={t('pair.desktops.remove')}
                    title={t('pair.desktops.remove')}
                    onClick={() => handleRemove(desktop.desktopId)}
                  >
                    ✕
                  </button>
                </div>
              );
            })
          )}
          <div className="desktop-sheet-hint">{t('pair.desktops.hint')}</div>
        </div>
      </div>
    </div>
  );
}
