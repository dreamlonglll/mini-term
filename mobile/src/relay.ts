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
  type CommandFailReason,
  type MirrorMessage,
  type MobileHello,
  type MobileProject,
  type MobileRejectReason,
  type MobileToRelay,
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

/** 对话镜像视图状态(一次只镜像一个 pane)。 */
export interface MirrorState {
  paneId: string;
  /** 该 pane 的展示名(从列表带入) */
  title: string;
  /** 已加载的消息,按 seq 升序 */
  messages: MirrorMessage[];
  /** 是否还有更早历史可分页 */
  hasMore: boolean;
  /** 是否已收到首个快照(false = 加载中) */
  loaded: boolean;
  /** 加载更早历史的请求进行中 */
  loadingOlder: boolean;
  /** 目标 pane 已关闭/AI 会话已结束 */
  closed: boolean;
  /** 等待回执中的指令 id;null = 没有在途指令 */
  pendingCommandId: string | null;
  /** 最近一次指令回执(短暂展示后由 UI 清除) */
  receipt: { ok: boolean; reason?: CommandFailReason } | null;
}

interface RelayStore {
  phase: Phase;
  rejectReason: MobileRejectReason | null;
  /** 桌面端在线状态;null = 尚未收到 presence */
  desktopOnline: boolean | null;
  /** 活跃 AI 会话列表(按项目分组,来自桌面端快照/增量) */
  projects: MobileProject[];
  /** 当前打开的对话镜像;null = 在列表页 */
  mirror: MirrorState | null;
}

export const useRelayStore = create<RelayStore>(() => ({
  phase: 'idle',
  rejectReason: null,
  desktopOnline: null,
  projects: [],
  mirror: null,
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
      case 'helloAck': {
        handshakeDone = true;
        reconnectAttempt = 0;
        if (msg.credential) saveCredential(msg.credential);
        setPhase('connected');
        // 断线前若在看某个镜像:重连后自动恢复订阅(桌面端会重发快照)
        const { mirror } = useRelayStore.getState();
        if (mirror && !mirror.closed) {
          sendToRelay({ type: 'subscribePane', paneId: mirror.paneId });
        }
        break;
      }
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
      case 'presence':
        useRelayStore.setState({ desktopOnline: msg.desktopOnline });
        break;
      case 'sessionsSnapshot':
        useRelayStore.setState({ projects: msg.projects });
        break;
      case 'sessionsDelta': {
        const { projects } = useRelayStore.getState();
        const removed = new Set(msg.removedProjectIds);
        const upsertMap = new Map(msg.upserts.map((p) => [p.projectId, p]));
        const next: MobileProject[] = [];
        for (const p of projects) {
          if (removed.has(p.projectId)) continue;
          const upserted = upsertMap.get(p.projectId);
          if (upserted) {
            next.push(upserted);
            upsertMap.delete(p.projectId);
          } else {
            next.push(p);
          }
        }
        next.push(...upsertMap.values()); // 新增项目追加在尾部
        useRelayStore.setState({ projects: next });
        break;
      }
      case 'mirrorSnapshot': {
        const { mirror } = useRelayStore.getState();
        if (!mirror || mirror.paneId !== msg.paneId) break;
        useRelayStore.setState({
          mirror: {
            ...mirror,
            messages: msg.messages,
            hasMore: msg.hasMore,
            loaded: true,
            loadingOlder: false,
          },
        });
        break;
      }
      case 'mirrorAppend': {
        const { mirror } = useRelayStore.getState();
        if (!mirror || mirror.paneId !== msg.paneId) break;
        useRelayStore.setState({
          mirror: { ...mirror, messages: [...mirror.messages, ...msg.messages] },
        });
        break;
      }
      case 'mirrorHistory': {
        const { mirror } = useRelayStore.getState();
        if (!mirror || mirror.paneId !== msg.paneId) break;
        useRelayStore.setState({
          mirror: {
            ...mirror,
            messages: [...msg.messages, ...mirror.messages],
            hasMore: msg.hasMore,
            loadingOlder: false,
          },
        });
        break;
      }
      case 'paneClosed': {
        const { mirror } = useRelayStore.getState();
        if (!mirror || mirror.paneId !== msg.paneId) break;
        useRelayStore.setState({ mirror: { ...mirror, closed: true } });
        break;
      }
      case 'commandReceipt': {
        const { mirror } = useRelayStore.getState();
        if (!mirror || mirror.paneId !== msg.paneId) break;
        if (mirror.pendingCommandId !== null && mirror.pendingCommandId !== msg.commandId) break;
        useRelayStore.setState({
          mirror: {
            ...mirror,
            pendingCommandId: null,
            receipt: { ok: msg.ok, reason: msg.reason },
          },
        });
        break;
      }
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

/** 向中转发送一条消息;未连接时静默丢弃(调用方 UI 已按连接态禁用入口)。 */
export function sendToRelay(msg: MobileToRelay) {
  if (ws && ws.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify(msg));
  }
}

/** 进入某 pane 的对话镜像:登记视图状态并向桌面端订阅。 */
export function openMirror(paneId: string, title: string) {
  useRelayStore.setState({
    mirror: {
      paneId,
      title,
      messages: [],
      hasMore: false,
      loaded: false,
      loadingOlder: false,
      closed: false,
      pendingCommandId: null,
      receipt: null,
    },
  });
  sendToRelay({ type: 'subscribePane', paneId });
}

/** 发送移动端指令(写穿,不排队)。返回 false = 当前不可发送。 */
export function sendMobileCommand(text: string): boolean {
  const { mirror, desktopOnline, phase } = useRelayStore.getState();
  const trimmed = text.trim();
  if (!mirror || mirror.closed || !trimmed) return false;
  if (phase !== 'connected' || desktopOnline === false) return false;
  const commandId = crypto.randomUUID();
  useRelayStore.setState({
    mirror: { ...mirror, pendingCommandId: commandId, receipt: null },
  });
  sendToRelay({ type: 'mobileCommand', paneId: mirror.paneId, commandId, text: trimmed });
  return true;
}

/** 清除回执提示(UI 展示几秒后调用)。 */
export function clearCommandReceipt() {
  const { mirror } = useRelayStore.getState();
  if (mirror?.receipt) {
    useRelayStore.setState({ mirror: { ...mirror, receipt: null } });
  }
}

/** 退出镜像返回列表:退订并清空视图状态。 */
export function closeMirror() {
  const { mirror } = useRelayStore.getState();
  if (mirror) sendToRelay({ type: 'unsubscribePane', paneId: mirror.paneId });
  useRelayStore.setState({ mirror: null });
}

/** 上拉加载更早历史(以当前最早消息的 seq 为锚)。 */
export function loadOlderMirror() {
  const { mirror } = useRelayStore.getState();
  if (!mirror || !mirror.hasMore || mirror.loadingOlder || mirror.messages.length === 0) return;
  useRelayStore.setState({ mirror: { ...mirror, loadingOlder: true } });
  sendToRelay({
    type: 'requestMirrorHistory',
    paneId: mirror.paneId,
    beforeSeq: mirror.messages[0].seq,
  });
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
