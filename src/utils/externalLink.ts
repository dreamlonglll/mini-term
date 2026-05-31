import type { MouseEvent } from 'react';
import { openUrl } from '@tauri-apps/plugin-opener';
import { showConfirm } from './prompt';
import { t } from '../i18n';

export function isHttpUrl(href: string | null | undefined): href is string {
  return !!href && /^https?:\/\//i.test(href);
}

/**
 * 拦截 <a> 链接点击，对 http(s) 外部链接弹确认后调系统浏览器打开。
 * 不是 http(s) 协议（相对路径、锚点、mailto 等）则放行不处理。
 */
export async function handleExternalLinkClick(e: MouseEvent<HTMLAnchorElement>) {
  const href = e.currentTarget.getAttribute('href');
  if (!isHttpUrl(href)) return;
  e.preventDefault();
  const ok = await showConfirm(t('externalLink.openConfirm'), href);
  if (!ok) return;
  openUrl(href).catch((err) => console.error('打开链接失败:', err));
}
