/**
 * 移动端 → 中转的 WebSocket 连接管理。
 *
 * - 扫码首连:URL hash 携带一次性配对码(#pair=CODE),兑换长期凭证存 localStorage
 * - 重开/断线:凭凭证自动重连(指数退避,封顶 30s;页面回前台立即重试)
 * - 被吊销(新设备顶替/桌面端重置):清除凭证,提示重新扫码,不再重连
 */
import { create } from 'zustand';
import {
  PROTOCOL_VERSION,
  type MobileHello,
  type MobileRejectReason,
  type RelayToMobile,
} from './protocol';

const CRED_KEY = 'mt-mobile-credential';

export type Phase =
  | 'idle' // 无配对码也无凭证:提示去桌面端扫码
  | 'pairing'
  | 'connecting'
  | 'connected'
  | 'reconnecting'
  | 'revoked' // 配对被顶替/重置
  | 'rejected'; // 握手被拒(带 reason)

interface RelayStore {
  phase: Phase;
  rejectReason: MobileRejectReason | null;
  /** 本次会话中刚完成配对(用于展示"配对完成"提示) */
  justPaired: boolean;
}

export const useRelayStore = create<RelayStore>(() => ({
  phase: 'idle',
  rejectReason: null,
  justPaired: false,
}));

function setPhase(phase: Phase, rejectReason: MobileRejectReason | null = null) {
  useRelayStore.setState({ phase, rejectReason });
}

function getCredential(): string | null {
  try {
    return localStorage.getItem(CRED_KEY);
  } catch {
    return null;
  }
}

function saveCredential(cred: string) {
  try {
    localStorage.setItem(CRED_KEY, cred);
  } catch {
    /* 私密浏览等场景存不下:本次会话仍可用,下次需重新扫码 */
  }
}

function clearCredential() {
  try {
    localStorage.removeItem(CRED_KEY);
  } catch {
    /* ignore */
  }
}

/** 从 URL hash 取一次性配对码(#pair=CODE),读后立即清除避免刷新重放。 */
function consumePairingCode(): string | null {
  const m = /[#&]pair=([A-Za-z0-9-]+)/.exec(window.location.hash);
  if (!m) return null;
  try {
    history.replaceState(null, '', window.location.pathname + window.location.search);
  } catch {
    /* ignore */
  }
  return m[1];
}

function wsUrl(): string {
  const override = import.meta.env.VITE_RELAY_WS as string | undefined;
  const base =
    override ?? `${window.location.protocol === 'https:' ? 'wss' : 'ws'}://${window.location.host}`;
  return `${base.replace(/\/+$/, '')}/ws/mobile`;
}

let ws: WebSocket | null = null;
let reconnectAttempt = 0;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
/** 握手被拒/被吊销后不再自动重连 */
let stopped = false;

function backoffMs(attempt: number): number {
  return Math.min(1000 * 2 ** Math.max(0, attempt - 1), 30_000);
}

function connect(auth: { pairingCode?: string; credential?: string }) {
  stopped = false;
  if (auth.pairingCode) {
    setPhase('pairing');
  } else {
    setPhase(reconnectAttempt > 0 ? 'reconnecting' : 'connecting');
  }

  const socket = new WebSocket(wsUrl());
  ws = socket;
  let handshakeDone = false;

  socket.onopen = () => {
    const hello: MobileHello = {
      type: 'hello',
      protocolVersion: PROTOCOL_VERSION,
      ...(auth.pairingCode ? { pairingCode: auth.pairingCode } : {}),
      ...(auth.credential ? { credential: auth.credential } : {}),
    };
    socket.send(JSON.stringify(hello));
  };

  socket.onmessage = (ev) => {
    let msg: RelayToMobile;
    try {
      msg = JSON.parse(String(ev.data)) as RelayToMobile;
    } catch {
      return;
    }
    switch (msg.type) {
      case 'helloAck':
        handshakeDone = true;
        reconnectAttempt = 0;
        if (msg.credential) {
          saveCredential(msg.credential);
          useRelayStore.setState({ justPaired: true });
        }
        setPhase('connected');
        break;
      case 'helloReject':
        stopped = true;
        if (msg.reason === 'invalidCredential') clearCredential();
        setPhase('rejected', msg.reason);
        break;
      case 'revoked':
        stopped = true;
        clearCredential();
        setPhase('revoked');
        break;
    }
  };

  socket.onclose = () => {
    if (ws !== socket) return; // 已被新连接取代
    ws = null;
    if (stopped) return;
    const cred = getCredential();
    if (!cred) {
      // 配对失败且没有旧凭证可回退
      if (!handshakeDone) setPhase('idle');
      return;
    }
    reconnectAttempt += 1;
    setPhase('reconnecting');
    reconnectTimer = setTimeout(() => connect({ credential: cred }), backoffMs(reconnectAttempt));
  };
}

/** 应用启动入口:优先兑换 URL 里的配对码,否则凭本地凭证重连。 */
export function startRelay() {
  const code = consumePairingCode();
  if (code) {
    // 扫了新码就走新配对(顶替旧凭证),即使本地已有凭证
    connect({ pairingCode: code });
    return;
  }
  const cred = getCredential();
  if (cred) {
    connect({ credential: cred });
  } else {
    setPhase('idle');
  }
}

// 手机浏览器切后台常导致连接被杀:回前台且处于重连等待时立即重试
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState !== 'visible') return;
  const { phase } = useRelayStore.getState();
  if (phase === 'reconnecting' && !ws) {
    if (reconnectTimer) clearTimeout(reconnectTimer);
    const cred = getCredential();
    if (cred) connect({ credential: cred });
  }
});
