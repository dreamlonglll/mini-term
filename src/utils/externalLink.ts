import type { MouseEvent } from 'react';
import { openUrl } from '@tauri-apps/plugin-opener';
import { showConfirm } from './prompt';
import { t } from '../i18n';

export function isHttpUrl(href: string | null | undefined): href is string {
  return !!href && /^https?:\/\//i.test(href);
}

/** 弹确认后用系统浏览器打开 http(s) 外链 */
export async function openExternalUrl(href: string) {
  const ok = await showConfirm(t('externalLink.openConfirm'), href);
  if (!ok) return;
  openUrl(href).catch((err) => console.error('打开链接失败:', err));
}

/**
 * 拦截 <a> 链接点击。
 * - http(s) 外链：弹确认后调系统浏览器打开。
 * - 其它（相对路径、锚点、mailto 等）：同样阻止默认导航，避免 WebView
 *   离开 SPA 导致整个程序重载；不做进一步处理。
 *
 * 关键：任何情况下都先 preventDefault，绝不放行 <a> 触发顶层导航。
 */
export function handleExternalLinkClick(e: MouseEvent<HTMLAnchorElement>) {
  e.preventDefault();
  const href = e.currentTarget.getAttribute('href');
  if (isHttpUrl(href)) void openExternalUrl(href);
}
