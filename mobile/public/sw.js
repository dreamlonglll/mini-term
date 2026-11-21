/*
 * mini-term 移动端 Service Worker:只做「外壳秒开」,不碰业务数据。
 *
 * - 页面导航:先给缓存里的 index.html(打开不等网络),同时后台向中转要最新的
 *   (cache: 'no-cache' 强制回源校验,中转没发缓存头,HTTP 启发式缓存会给旧的);
 *   拿到新版先把它引用的 /assets/* 备齐,再换 index —— 缓存里的 index 永远指向
 *   缓存里有的文件。新版下次打开生效。
 * - /assets/*:文件名带内容哈希、永不改写,缓存优先。
 * - 图标、manifest:缓存优先,后台更新。
 * - WebSocket(/ws/*)、/healthz、跨源请求、非 GET:一律不经手。
 *
 * 中转对不存在的路径也回 200 + index.html(SPA 兜底),所以存资源前要核对
 * Content-Type,不能把兜底页当 JS / CSS 存进缓存。
 *
 * 要整体作废(比如这份脚本本身出了问题):把 CACHE 改个名重新部署,activate 会删掉旧缓存。
 */
const CACHE = 'mt-shell-v1';
const INDEX_KEY = '/index.html';

self.addEventListener('install', (event) => {
  event.waitUntil(
    refreshShell()
      .catch(() => {})
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    (async () => {
      const keys = await caches.keys();
      await Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k)));
      await self.clients.claim();
    })(),
  );
});

self.addEventListener('fetch', (event) => {
  const req = event.request;
  if (req.method !== 'GET') return;
  const url = new URL(req.url);
  if (url.origin !== self.location.origin) return;
  if (url.pathname.startsWith('/ws/') || url.pathname === '/healthz') return;

  if (req.mode === 'navigate') {
    event.respondWith(serveShell(event));
  } else if (url.pathname.startsWith('/assets/')) {
    event.respondWith(cacheFirst(req));
  } else if (url.pathname.startsWith('/icons/') || url.pathname === '/manifest.webmanifest') {
    event.respondWith(staleWhileRevalidate(event));
  }
});

/** 响应是不是中转的 SPA 兜底页(HTML)。 */
function isHtml(res) {
  return (res.headers.get('content-type') || '').includes('text/html');
}

/** index.html 里引用的 /assets/* 路径(Vite 产物:一个 JS、一个 CSS)。 */
function assetsOf(html) {
  return [...new Set(html.match(/\/assets\/[^"'\s)<>]+/g) || [])];
}

let refreshing = null;

/**
 * 向中转要最新的 index.html,连同它引用的资源一起换进缓存,并清掉不再引用的旧哈希资源。
 * 同时只跑一份(安装与导航可能同时触发)。
 */
function refreshShell() {
  if (!refreshing) {
    refreshing = doRefreshShell().finally(() => {
      refreshing = null;
    });
  }
  return refreshing;
}

async function doRefreshShell() {
  const res = await fetch('/', { cache: 'no-cache' });
  if (!res.ok || !isHtml(res)) throw new Error(`index: ${res.status}`);
  const html = await res.clone().text();
  const assets = assetsOf(html);
  const cache = await caches.open(CACHE);
  await Promise.all(
    assets.map(async (path) => {
      if (await cache.match(path)) return;
      const r = await fetch(path);
      if (!r.ok || isHtml(r)) throw new Error(`asset ${path}: ${r.status}`);
      await cache.put(path, r);
    }),
  );
  await cache.put(INDEX_KEY, res);
  for (const key of await cache.keys()) {
    const path = new URL(key.url).pathname;
    if (path.startsWith('/assets/') && !assets.includes(path)) await cache.delete(key);
  }
}

async function serveShell(event) {
  const cache = await caches.open(CACHE);
  const cached = await cache.match(INDEX_KEY);
  if (cached) {
    event.waitUntil(refreshShell().catch(() => {}));
    return cached;
  }
  // 缓存里还没有(首次、或被系统清过):等这一轮刷新,失败就直接走网络
  try {
    await refreshShell();
    const fresh = await cache.match(INDEX_KEY);
    if (fresh) return fresh;
  } catch {
    /* 落到下面直接请求 */
  }
  return fetch(event.request);
}

async function cacheFirst(req) {
  const cache = await caches.open(CACHE);
  const hit = await cache.match(req);
  if (hit) return hit;
  const res = await fetch(req);
  if (res.ok && !isHtml(res)) await cache.put(req, res.clone());
  return res;
}

async function staleWhileRevalidate(event) {
  const cache = await caches.open(CACHE);
  const hit = await cache.match(event.request);
  const update = fetch(event.request)
    .then(async (res) => {
      if (res.ok && !isHtml(res)) await cache.put(event.request, res.clone());
      return res;
    })
    .catch(() => null);
  if (hit) {
    event.waitUntil(update);
    return hit;
  }
  return (await update) || fetch(event.request);
}
