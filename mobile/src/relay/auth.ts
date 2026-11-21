const CRED_KEY = 'mt-mobile-credential';

export function getCredential(): string | null {
  try {
    return localStorage.getItem(CRED_KEY);
  } catch {
    return null;
  }
}

export function saveCredential(cred: string) {
  try {
    localStorage.setItem(CRED_KEY, cred);
  } catch {
    /* 私密浏览等场景存不下:本次会话仍可用,下次需重新扫码 */
  }
}

export function clearCredential() {
  try {
    localStorage.removeItem(CRED_KEY);
  } catch {
    /* ignore */
  }
}

/** 清掉 URL hash(配对码 / 深链读过即清,避免刷新重放)。 */
export function clearHash() {
  try {
    history.replaceState(null, '', window.location.pathname + window.location.search);
  } catch {
    /* ignore */
  }
}

/** 从 URL hash 取一次性配对码(#pair=CODE),读后立即清除避免刷新重放。 */
export function consumePairingCode(): string | null {
  const m = /[#&]pair=([A-Za-z0-9-]+)/.exec(window.location.hash);
  if (!m) return null;
  clearHash();
  return m[1];
}

/**
 * 通知深链(#pane=ID):桌面端远程通知里附的那条,点开直接进该会话的对话镜像。
 * 只读不清 —— 清 hash 由调用方统一做(与配对码同在一条 URL 里时不能先被抹掉)。
 */
export function readPaneLink(): string | null {
  const m = /[#&]pane=([A-Za-z0-9_-]+)/.exec(window.location.hash);
  return m ? m[1] : null;
}

export function wsUrl(): string {
  const override = import.meta.env.VITE_RELAY_WS as string | undefined;
  const base =
    override ?? `${window.location.protocol === 'https:' ? 'wss' : 'ws'}://${window.location.host}`;
  return `${base.replace(/\/+$/, '')}/ws/mobile`;
}

export function backoffMs(attempt: number): number {
  return Math.min(1000 * 2 ** Math.max(0, attempt - 1), 30_000);
}
