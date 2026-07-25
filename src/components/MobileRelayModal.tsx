import { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { ask } from '@tauri-apps/plugin-dialog';
import QRCode from 'qrcode';
import { useAppStore } from '../store';
import { useTauriEvent } from '../hooks/useTauriEvent';
import { useT } from '../i18n';
import { RelayStatusBadge } from './RelayStatusBadge';
import type { MobileRelayStatusPayload } from '../types';

interface Props {
  open: boolean;
  onClose: () => void;
}

/** 中转地址(ws/wss)→ 移动端网页地址(http/https)。 */
function relayHttpBase(relayUrl: string): string {
  const trimmed = relayUrl.trim().replace(/\/+$/, '');
  if (trimmed.startsWith('wss://')) return `https://${trimmed.slice(6)}`;
  if (trimmed.startsWith('ws://')) return `http://${trimmed.slice(5)}`;
  if (trimmed.startsWith('https://') || trimmed.startsWith('http://')) return trimmed;
  return `https://${trimmed}`;
}

/** 顶栏「移动端」面板:中转地址配置、连接状态、配对二维码、重置配对(一站式)。 */
export function MobileRelayModal({ open, onClose }: Props) {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);
  const relayStatus = useAppStore((s) => s.mobileRelayStatus);
  const setMobileRelayStatus = useAppStore((s) => s.setMobileRelayStatus);

  const [qrDataUrl, setQrDataUrl] = useState<string | null>(null);
  const [qrRequested, setQrRequested] = useState(false);

  const relayUrl = config.mobileRelay?.relayUrl ?? '';
  const status = relayStatus?.status ?? 'disconnected';
  const connected = status === 'connected';

  const [url, setUrl] = useState(relayUrl);

  // 配置变化(含本面板保存后回写)同步到输入框
  useEffect(() => {
    setUrl(relayUrl);
  }, [relayUrl]);

  // 打开面板时取一次当前状态兜底;关闭时丢弃已展示的二维码(旧码可能已被后续操作作废)
  useEffect(() => {
    if (!open) {
      setQrDataUrl(null);
      setQrRequested(false);
      return;
    }
    invoke<MobileRelayStatusPayload>('mobile_relay_status')
      .then(setMobileRelayStatus)
      .catch(() => {});
  }, [open, setMobileRelayStatus]);

  // 保存中转地址并让后端重建连接;地址变了旧配对二维码即作废,一并清掉
  const applyUrl = useCallback(async (nextUrl: string) => {
    const trimmed = nextUrl.trim();
    setQrDataUrl(null);
    setQrRequested(false);
    const cfg = useAppStore.getState().config;
    const newConfig = {
      ...cfg,
      mobileRelay: trimmed ? { ...cfg.mobileRelay, relayUrl: trimmed } : undefined,
    };
    setConfig(newConfig);
    await invoke('save_config', { config: newConfig });
    await invoke('mobile_relay_apply', { relayUrl: trimmed });
  }, [setConfig]);

  // 中转签发配对码 → 组配对链接 → 渲染二维码
  useTauriEvent<{ code: string }>('mobile-relay-pairing-code', useCallback((payload) => {
    const pairUrl = `${relayHttpBase(relayUrl)}/#pair=${payload.code}`;
    QRCode.toDataURL(pairUrl, { width: 260, margin: 1 })
      .then(setQrDataUrl)
      .catch(() => setQrDataUrl(null));
  }, [relayUrl]));

  const requestQr = useCallback(() => {
    setQrRequested(true);
    setQrDataUrl(null);
    invoke('mobile_relay_request_pairing_code').catch(() => setQrRequested(false));
  }, []);

  const resetPairing = useCallback(async () => {
    const confirmed = await ask(t('mobileRelay.modal.resetConfirm'), {
      title: t('mobileRelay.modal.resetPairing'),
      kind: 'warning',
    });
    if (!confirmed) return;
    setQrDataUrl(null);
    setQrRequested(false);
    invoke('mobile_relay_reset_pairing').catch(() => {});
  }, [t]);

  if (!open) return null;

  const paired = relayStatus?.paired;

  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center pt-[12vh]" onClick={onClose}>
      <div className="absolute inset-0 bg-black/50 backdrop-blur-sm" />
      <div
        className="relative w-[440px] max-h-[76vh] bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-[var(--shadow-overlay)] flex flex-col overflow-hidden animate-slide-in"
        onClick={(e) => e.stopPropagation()}
      >
        {/* 顶栏 */}
        <div className="flex items-center justify-between px-5 py-4 border-b border-[var(--border-subtle)]">
          <h2 className="text-lg font-semibold text-[var(--text-primary)]">{t('mobileRelay.modal.title')}</h2>
          <button
            className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-lg leading-none"
            onClick={onClose}
          >
            ✕
          </button>
        </div>

        <div className="flex-1 overflow-y-auto px-5 py-4 space-y-4">
          <p className="text-sm text-[var(--text-muted)] leading-relaxed">{t('mobileRelay.intro')}</p>

          {/* 中转服务器地址 */}
          <div>
            <div className="text-base text-[var(--text-muted)] uppercase tracking-[0.1em] mb-2">
              {t('mobileRelay.urlLabel')}
            </div>
            <input
              type="text"
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              onKeyDown={(e) => { if (e.key === 'Enter') applyUrl(url); }}
              placeholder={t('mobileRelay.urlPlaceholder')}
              spellCheck={false}
              className="w-full px-3 py-2 rounded-[var(--radius-sm)] bg-[var(--bg-base)] border border-[var(--border-default)] text-base text-[var(--text-primary)] placeholder:text-[var(--text-muted)] focus:border-[var(--accent)] focus:outline-none"
            />
            <div className="flex gap-2 mt-2">
              <button
                className="px-4 py-1.5 rounded-[var(--radius-sm)] text-base bg-[var(--accent-muted)] text-[var(--accent)] border border-[var(--accent)] hover:opacity-90 transition-opacity disabled:opacity-40"
                disabled={!url.trim()}
                onClick={() => applyUrl(url)}
              >
                {t('mobileRelay.apply')}
              </button>
              <button
                className="px-4 py-1.5 rounded-[var(--radius-sm)] text-base bg-[var(--bg-base)] text-[var(--text-secondary)] border border-[var(--border-default)] hover:border-[var(--accent)] transition-colors disabled:opacity-40"
                disabled={!url.trim() && !config.mobileRelay}
                onClick={() => { setUrl(''); applyUrl(''); }}
              >
                {t('mobileRelay.clear')}
              </button>
            </div>
          </div>

          {/* 中转连接状态 */}
          <div className="flex items-center justify-between px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]">
            <span className="text-base text-[var(--text-primary)]">{t('mobileRelay.statusLabel')}</span>
            <RelayStatusBadge relayStatus={relayStatus} />
          </div>

          {!relayUrl ? (
            <p className="text-sm text-[var(--text-muted)] leading-relaxed">
              {t('mobileRelay.modal.notConfigured')}
            </p>
          ) : (
            <>
              {/* 配对状态 */}
              <div className="flex items-center justify-between px-3 py-2.5 rounded-[var(--radius-md)] bg-[var(--bg-base)] border border-[var(--border-subtle)]">
                <span className="text-base text-[var(--text-primary)]">{t('mobileRelay.modal.pairedLabel')}</span>
                <span className="text-base text-[var(--text-secondary)]">
                  {paired === true
                    ? t('mobileRelay.modal.paired')
                    : paired === false
                      ? t('mobileRelay.modal.notPaired')
                      : t('mobileRelay.modal.pairedUnknown')}
                </span>
              </div>

              {/* 二维码区域 */}
              {connected ? (
                <div className="space-y-3">
                  {qrDataUrl ? (
                    <div className="flex flex-col items-center gap-3">
                      <img
                        src={qrDataUrl}
                        alt="pairing qr"
                        className="rounded-[var(--radius-md)] border border-[var(--border-subtle)] bg-white p-1"
                        width={260}
                        height={260}
                      />
                      <p className="text-sm text-[var(--text-muted)] leading-relaxed">
                        {t('mobileRelay.modal.qrHint')}
                      </p>
                    </div>
                  ) : qrRequested ? (
                    <p className="text-sm text-[var(--text-muted)]">{t('mobileRelay.modal.qrWaiting')}</p>
                  ) : null}
                  <div className="flex gap-2">
                    <button
                      className="px-4 py-1.5 rounded-[var(--radius-sm)] text-base bg-[var(--accent-muted)] text-[var(--accent)] border border-[var(--accent)] hover:opacity-90 transition-opacity"
                      onClick={requestQr}
                    >
                      {qrDataUrl ? t('mobileRelay.modal.regenerateQr') : t('mobileRelay.modal.generateQr')}
                    </button>
                    {paired === true && (
                      <button
                        className="px-4 py-1.5 rounded-[var(--radius-sm)] text-base bg-[var(--bg-base)] text-[var(--color-error)] border border-[var(--border-default)] hover:border-[var(--color-error)] transition-colors"
                        onClick={resetPairing}
                      >
                        {t('mobileRelay.modal.resetPairing')}
                      </button>
                    )}
                  </div>
                </div>
              ) : (
                <p className="text-sm text-[var(--text-muted)]">{t('mobileRelay.modal.needConnected')}</p>
              )}
            </>
          )}
        </div>
      </div>
    </div>
  );
}
