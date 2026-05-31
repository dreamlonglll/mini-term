import { useEffect, useState, useRef, useCallback } from 'react';
import { createPortal } from 'react-dom';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import { useT } from '../i18n';

interface Props {
  /** modal 是否打开;false 时 iframe 仍保留(visibility:hidden) */
  open: boolean;
  /** 关闭回调,Esc / 点遮罩 / 点 X 触发 */
  onClose: () => void;
  /**
   * iframe 深链路径,例如 "/projects/MyProj"。
   * 留空时落到 cc-connect 默认 dashboard("/projects" 列表页)。
   */
  deepLink?: string;
}

/**
 * cc-connect Web Dashboard 嵌入弹窗。
 *
 * - 通过 `?token=<token>&redirect=<deepLink>` 让 iframe 自动登录并跳到目标页
 * - keep-alive:open=false 时只切 visibility,不卸载 iframe,避免每次重 login
 * - 通过 createPortal 渲染到 body,绕开 fluent2 [data-panel] 的 backdrop-filter
 *   形成的 containing block(见 commit e7316e5 处理过的同类问题)
 * - cc-connect 未运行时不渲染 iframe,显示降级提示
 */
export function CcConnectDashboard({ open, onClose, deepLink }: Props) {
  const t = useT();
  const ccConfig = useAppStore((s) => s.config.ccConnect);
  const status = useAppStore((s) => s.ccConnectStatus);
  const [iframeUrl, setIframeUrl] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const currentDeepLinkRef = useRef<string | undefined>(deepLink);
  // 边缘检测:cc-connect running false→true 或 ownPid 变化时强制 rebuild iframe,
  // 避免 cc-connect restart 后 token 仍有效但 session 已失效导致页面空白/登录页。
  const lastSeenRunningRef = useRef<boolean>(status?.running ?? false);
  const lastSeenOwnPidRef = useRef<number | undefined>(status?.ownPid);

  const buildUrl = useCallback(async (): Promise<string | null> => {
    if (!status?.running) {
      setError(t('ccDashboard.notRunning'));
      return null;
    }
    try {
      const token = await invoke<string>('cc_connect_read_token', {
        configPath: ccConfig?.configPath || undefined,
      });
      const port = status.port;
      const params = new URLSearchParams({ token });
      if (deepLink) params.set('redirect', deepLink);
      setError(null);
      return `http://127.0.0.1:${port}/login?${params.toString()}`;
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      setError(t('ccDashboard.readTokenFailed', { msg }));
      return null;
    }
  }, [status?.running, status?.port, ccConfig?.configPath, deepLink, t]);

  // cc-connect 重启检测:running false→true 边缘 或 ownPid 变化 → 强制 rebuild
  // (不在依赖里依赖 iframeUrl,避免循环;只看 status 切换)
  useEffect(() => {
    const curRunning = status?.running ?? false;
    const curPid = status?.ownPid;
    const runningEdgeUp = !lastSeenRunningRef.current && curRunning;
    const pidChanged = lastSeenOwnPidRef.current !== curPid;
    lastSeenRunningRef.current = curRunning;
    lastSeenOwnPidRef.current = curPid;
    // running false→true 或 ownPid 变化 → 清空 iframeUrl,让下面 effect 在 open 时重建
    // (running true→false 时保留 iframeUrl,避免空白闪烁)
    if (runningEdgeUp || pidChanged) {
      setIframeUrl(null);
    }
  }, [status?.running, status?.ownPid]);

  // 首次 open / deepLink 切换 / cc-connect 重启清空后重建 URL
  useEffect(() => {
    if (!open) return;
    // deepLink 切换时强制 reload(同 URL 多次设置 react 不会触发 iframe 重载,这里通过加时间戳确保 navigate)
    const deepLinkChanged = currentDeepLinkRef.current !== deepLink;
    currentDeepLinkRef.current = deepLink;
    if (iframeUrl && !deepLinkChanged) return;
    void buildUrl().then((url) => {
      if (url) setIframeUrl(url);
    });
  }, [open, deepLink, iframeUrl, buildUrl]);

  // Esc 关闭
  useEffect(() => {
    if (!open) return;
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    document.addEventListener('keydown', handleKey);
    return () => document.removeEventListener('keydown', handleKey);
  }, [open, onClose]);

  // open=false 时不渲染任何东西(不挂在 DOM 里),避免初次未打开就占用资源
  // open 切回 true 时上面 effect 会重新 buildUrl;实测 cc-connect 即使已登录也能秒进
  if (!open && !iframeUrl) return null;

  return createPortal(
    <div
      className="fixed inset-0 z-50 flex items-center justify-center"
      style={{ display: open ? 'flex' : 'none' }}
      onClick={onClose}
    >
      <div className="absolute inset-0 bg-black/60 backdrop-blur-sm" />
      <div
        className="relative w-[92vw] h-[88vh] bg-[var(--bg-surface)] border border-[var(--border-strong)] rounded-[var(--radius-md)] shadow-[var(--shadow-overlay)] flex flex-col overflow-hidden animate-slide-in"
        onClick={(e) => e.stopPropagation()}
      >
        {/* 顶栏 */}
        <div className="flex items-center justify-between px-4 py-2.5 border-b border-[var(--border-subtle)] flex-shrink-0">
          <div className="flex items-center gap-2 min-w-0">
            <span className="text-base font-semibold text-[var(--text-primary)]">cc-connect Dashboard</span>
            {status?.running && (
              <span className="text-xs text-[var(--text-muted)] font-mono">
                127.0.0.1:{status.port}
                {deepLink ? ` · ${deepLink}` : ''}
              </span>
            )}
          </div>
          <div className="flex items-center gap-2 flex-shrink-0">
            <button
              className="text-sm text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors"
              onClick={() => {
                // 强制重载 iframe
                setIframeUrl(null);
                void buildUrl().then((url) => { if (url) setIframeUrl(url); });
              }}
              title={t('ccDashboard.reload')}
            >
              {t('ccDashboard.refresh')}
            </button>
            <button
              className="text-[var(--text-muted)] hover:text-[var(--text-primary)] transition-colors text-lg leading-none"
              onClick={onClose}
              title={t('ccDashboard.close')}
            >
              ✕
            </button>
          </div>
        </div>

        {/* 内容区:iframe 或降级提示 */}
        <div className="flex-1 relative bg-[var(--bg-base)]">
          {error ? (
            <div className="absolute inset-0 flex items-center justify-center p-6">
              <div className="text-center max-w-md">
                <div className="text-base text-[var(--color-error)] mb-2">{t('ccDashboard.loadFailed')}</div>
                <div className="text-sm text-[var(--text-muted)] whitespace-pre-wrap break-all">{error}</div>
              </div>
            </div>
          ) : iframeUrl ? (
            <iframe
              key={iframeUrl}
              src={iframeUrl}
              className="absolute inset-0 w-full h-full border-0"
              title="cc-connect Dashboard"
            />
          ) : (
            <div className="absolute inset-0 flex items-center justify-center">
              <div className="text-sm text-[var(--text-muted)]">{t('ccDashboard.loading')}</div>
            </div>
          )}
        </div>
      </div>
    </div>,
    document.body,
  );
}
