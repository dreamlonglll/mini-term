export const DESKTOPS_STORAGE_KEY = 'mt-mobile-desktops';
const ACTIVE_DESKTOP_KEY = 'mt-mobile-active-desktop';
const LEGACY_CRED_KEY = 'mt-mobile-credential';
export const DEFAULT_DESKTOP_ID = 'default';

const DESKTOP_ID_RE = /^[A-Za-z0-9_-]{1,64}$/;

/** 本机保存的一台已配对桌面端(同一中转可接入多台电脑,各自持有独立凭证)。 */
export interface SavedDesktop {
  desktopId: string;
  /** 桌面端展示名(来自中转 helloAck / presence;未设置时缺省) */
  desktopName?: string;
  credential: string;
  /** 最近一次配对或成功连接的时间戳(毫秒,列表排序用) */
  updatedAt: number;
}

export function isValidDesktopId(raw: string | null | undefined): raw is string {
  return typeof raw === 'string' && DESKTOP_ID_RE.test(raw);
}

function readDesktopMap(): Record<string, SavedDesktop> {
  try {
    const raw = localStorage.getItem(DESKTOPS_STORAGE_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw) as unknown;
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return {};
    const out: Record<string, SavedDesktop> = {};
    for (const [id, value] of Object.entries(parsed as Record<string, unknown>)) {
      if (!isValidDesktopId(id) || !value || typeof value !== 'object') continue;
      const item = value as Partial<SavedDesktop>;
      if (typeof item.credential !== 'string' || !item.credential) continue;
      out[id] = {
        desktopId: id,
        ...(typeof item.desktopName === 'string' && item.desktopName.trim()
          ? { desktopName: item.desktopName.trim() }
          : {}),
        credential: item.credential,
        updatedAt: typeof item.updatedAt === 'number' ? item.updatedAt : 0,
      };
    }
    return out;
  } catch {
    return {};
  }
}

function writeDesktopMap(map: Record<string, SavedDesktop>) {
  try {
    localStorage.setItem(DESKTOPS_STORAGE_KEY, JSON.stringify(map));
  } catch {
    /* 私密浏览等场景存不下:本次会话仍可用,下次需重新扫码 */
  }
}

function sortDesktops(map: Record<string, SavedDesktop>): SavedDesktop[] {
  return Object.values(map).sort((a, b) => {
    if (b.updatedAt !== a.updatedAt) return b.updatedAt - a.updatedAt;
    return a.desktopId.localeCompare(b.desktopId);
  });
}

export function listSavedDesktops(): SavedDesktop[] {
  return sortDesktops(readDesktopMap());
}

export function getSavedDesktop(desktopId: string): SavedDesktop | null {
  return readDesktopMap()[desktopId] ?? null;
}

/**
 * 写入或更新某台桌面端的凭证与展示名(每次现读最新 map,避免多标签页互相覆盖)。
 * `authoritativeName = true` 表示本次信息来自支持多桌面端的新中转,空名称应清空旧缓存名。
 */
export function upsertSavedDesktop(entry: {
  desktopId: string;
  desktopName?: string;
  credential: string;
  authoritativeName?: boolean;
}): SavedDesktop[] {
  const map = readDesktopMap();
  const prev = map[entry.desktopId];
  const name = entry.desktopName?.trim();
  const resolvedName = entry.authoritativeName ? name : (name || prev?.desktopName);
  map[entry.desktopId] = {
    desktopId: entry.desktopId,
    ...(resolvedName ? { desktopName: resolvedName } : {}),
    credential: entry.credential,
    updatedAt: Date.now(),
  };
  writeDesktopMap(map);
  return sortDesktops(map);
}

/** 更新已保存桌面端的展示名(桌面端改名重连时同步刷新)。 */
export function updateSavedDesktopName(
  desktopId: string,
  desktopName?: string,
): SavedDesktop[] {
  const map = readDesktopMap();
  const prev = map[desktopId];
  if (!prev) return sortDesktops(map);
  const name = desktopName?.trim();
  map[desktopId] = {
    desktopId: prev.desktopId,
    credential: prev.credential,
    updatedAt: prev.updatedAt,
    ...(name ? { desktopName: name } : {}),
  };
  writeDesktopMap(map);
  return sortDesktops(map);
}

/** 仅移除指定桌面端的本地凭证,不影响本机保存的其他桌面端。 */
export function removeSavedDesktopEntry(desktopId: string): SavedDesktop[] {
  const map = readDesktopMap();
  delete map[desktopId];
  writeDesktopMap(map);
  if (getActiveDesktopId() === desktopId) {
    const rest = sortDesktops(map);
    setActiveDesktopId(rest[0]?.desktopId ?? null);
  }
  return sortDesktops(map);
}

export function getActiveDesktopId(): string | null {
  try {
    const id = localStorage.getItem(ACTIVE_DESKTOP_KEY);
    return isValidDesktopId(id) ? id : null;
  } catch {
    return null;
  }
}

export function setActiveDesktopId(desktopId: string | null) {
  try {
    if (desktopId) {
      localStorage.setItem(ACTIVE_DESKTOP_KEY, desktopId);
    } else {
      localStorage.removeItem(ACTIVE_DESKTOP_KEY);
    }
  } catch {
    /* ignore */
  }
}

/** 读取旧版单桌面 PWA 留下的凭证键(首次升级无缝迁移用)。 */
export function getLegacyCredential(): string | null {
  try {
    return localStorage.getItem(LEGACY_CRED_KEY);
  } catch {
    return null;
  }
}

export function clearLegacyCredential() {
  try {
    localStorage.removeItem(LEGACY_CRED_KEY);
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

/**
 * 把当前查看的桌面端 id 同步到 URL 查询参数(`?desktop=<id>`),同时清掉 hash。
 *
 * 刷新、收藏书签、或同一浏览器开多个标签页分别看不同电脑都依赖这一条:
 * 标签页 A 停在 `?desktop=A`,标签页 B 停在 `?desktop=B`,各自刷新仍回到原电脑、互不串台。
 */
export function syncDesktopUrl(desktopId: string | null) {
  try {
    const params = new URLSearchParams(window.location.search);
    if (desktopId) {
      params.set('desktop', desktopId);
    } else {
      params.delete('desktop');
    }
    const qs = params.toString();
    const next = `${window.location.pathname}${qs ? `?${qs}` : ''}`;
    history.replaceState(null, '', next);
  } catch {
    /* ignore */
  }
}

/** 从 URL 查询参数取目标桌面端标识(`?desktop=ID`)。 */
export function readDesktopQuery(): string | null {
  try {
    const value = new URLSearchParams(window.location.search).get('desktop');
    return isValidDesktopId(value) ? value : null;
  } catch {
    return null;
  }
}

/** 兜底从 URL hash 取目标桌面端标识(`#desktop=ID&pane=...`)。 */
export function readDesktopHash(): string | null {
  const m = /[#&]desktop=([A-Za-z0-9_-]{1,64})(?:&|$)/.exec(window.location.hash);
  return m ? m[1] : null;
}

/** 从 URL hash 读一次性配对码(`#pair=CODE`),只读不清 —— 由调用方统一清 hash。 */
export function readPairingCode(): string | null {
  const m = /[#&]pair=([A-Za-z0-9-]+)/.exec(window.location.hash);
  return m ? m[1] : null;
}

/**
 * 通知深链(`#pane=ID`):桌面端远程通知里附的那条,点开直接进该会话的对话镜像。
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
