/**
 * 请求 / 指令回执配对用的 id(`requestId` / `commandId`),UUID v4 文本形状。
 *
 * 不能直接调 `crypto.randomUUID()`:它只在安全上下文(HTTPS / localhost)里存在。
 * 中转部署在内网、走 `http://` + `ws://` 时,手机浏览器里它是 `undefined`,一调就抛,
 * 发送、历史、补全等所有要回执的请求都发不出去(issue #109)。`getRandomValues` 不受
 * 安全上下文限制,缺 `randomUUID` 时用它拼同形状的 v4 UUID。
 *
 * 协议里这两个字段只是不透明字符串,中转与桌面端只做相等比较、不校验格式。
 */
type RequestIdCrypto = Pick<Crypto, 'getRandomValues'> & { randomUUID?: () => string };

export function newRequestId(c: RequestIdCrypto = globalThis.crypto): string {
  if (typeof c.randomUUID === 'function') return c.randomUUID();
  const b = c.getRandomValues(new Uint8Array(16));
  b[6] = (b[6] & 0x0f) | 0x40; // version 4
  b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
  const hex = Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
