import { useT } from '../i18n';
import type { MobileRelayStatusPayload } from '../types';

const RELAY_STATUS_COLORS: Record<string, string> = {
  connected: 'var(--color-success)',
  connecting: 'var(--color-ai-working)',
  reconnecting: 'var(--color-ai-working)',
  disconnected: 'var(--text-muted)',
  // 三种"配置问题"终态:已停止重连,红点提示要人动手改配置
  versionMismatch: 'var(--color-error)',
  authFailed: 'var(--color-error)',
  keyNotConfigured: 'var(--color-error)',
};

/** 中转连接状态徽章:彩色圆点 + 状态文案(设置页与「移动端」面板共用)。 */
export function RelayStatusBadge({ relayStatus }: { relayStatus: MobileRelayStatusPayload | null }) {
  const t = useT();
  const status = relayStatus?.status ?? 'disconnected';
  const statusText = status === 'versionMismatch'
    ? t('mobileRelay.status.versionMismatch', {
        expected: relayStatus?.expectedVersion ?? '?',
        actual: relayStatus?.actualVersion ?? '?',
      })
    : t(`mobileRelay.status.${status}`);

  return (
    <span className="flex items-center gap-2 text-base text-[var(--text-secondary)] max-w-[70%] text-right">
      <span
        className={`w-2 h-2 rounded-full flex-shrink-0 ${
          status === 'connecting' || status === 'reconnecting' ? 'animate-blink' : ''
        }`}
        style={{ backgroundColor: RELAY_STATUS_COLORS[status] ?? 'var(--text-muted)' }}
      />
      {statusText}
    </span>
  );
}
