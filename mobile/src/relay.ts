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
  type MobileLauncher,
  type MobileProject,
  type MobileRejectReason,
  type MobileToRelay,
  type RelayToMobile,
  type StartSessionFailReason,
} from './protocol';

const CRED_KEY = 'mt-mobile-credential';

/**
 * 发起会话后等待 pane 出现在活跃列表里的上限。
 *
 * 回执只说"命令已写入",AI 真起来才会出现在快照里;命令不存在、未登录、卡在
 * 交互确认都会让它永远不出现。超时纯粹是手机侧的本地判断——**不触发任何桌面端
 * 动作**:分不清"起不来"和"起得慢"(首次运行、等你按 y 确认信任目录都会慢),
 * 杀掉的破坏性大于留着。
 */
const START_SESSION_TIMEOUT_MS = 15_000;

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

/** 发起新会话的进行态(一次只允许一个,防连点)。 */
export interface StartSessionState {
  requestId: string;
  /** 目标项目名(提示文案用) */
  projectName: string;
  /** 桌面端回执里的 pane id;null = 回执还没回来 */
  paneId: string | null;
}

interface RelayStore {
  phase: Phase;
  rejectReason: MobileRejectReason | null;
  /** 桌面端在线状态;null = 尚未收到 presence */
  desktopOnline: boolean | null;
  /** 项目列表(全部项目,含没有活跃会话的;来自桌面端快照/增量) */
  projects: MobileProject[];
  /** 可用的 AI 启动器(只有 id 与展示名) */
  launchers: MobileLauncher[];
  /** 当前打开的对话镜像;null = 在列表页 */
  mirror: MirrorState | null;
  /** 发起会话进行中;null = 空闲(+ 按钮可用) */
  starting: StartSessionState | null;
  /** 最近一次发起失败的原因;'timeout' = 命令写进去了但会话迟迟没出现 */
  startError: StartSessionFailReason | 'timeout' | null;
}

export const useRelayStore = create<RelayStore>(() => ({
  phase: 'idle',
  rejectReason: null,
  desktopOnline: null,
  projects: [],
  launchers: [],
  mirror: null,
  starting: null,
  startError: null,
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
        // 桌面端在等待期间掉线:这次发起再也不会有下文,立刻给出原因而不是等超时
        if (!msg.desktopOnline && useRelayStore.getState().starting) {
          finishStart('desktopOffline');
        }
        break;
      case 'sessionsSnapshot':
        useRelayStore.setState({
          projects: msg.projects,
          launchers: msg.launchers ?? [],
        });
        onProjectsChanged();
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
        onProjectsChanged();
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
      case 'startSessionReceipt': {
        const { starting } = useRelayStore.getState();
        // 陈旧回执(上一次已超时复位/被取消):丢弃,别让它复活一个不存在的等待
        if (!starting || starting.requestId !== msg.requestId) break;
        if (!msg.ok) {
          finishStart(msg.reason ?? 'spawnFailed');
          break;
        }
        // 命令已写入:接着等这个 pane 出现在活跃列表里,那才是真的起来了。
        // 计时窗口不重置——从用户点下那一刻起总共 15 秒,回执正常是毫秒级返回的,
        // 重新计时只会在桌面端卡住时把等待翻倍。
        useRelayStore.setState({
          starting: { ...starting, paneId: msg.paneId ?? null },
        });
        onProjectsChanged();
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

/**
 * 重命名会话。返回 false = 当前不可改(未连接 / 桌面端离线)。
 *
 * 不做乐观更新:改完的名字由桌面端随结构增量推回来,列表和镜像页标题跟着变。
 * 本地先改会在失败时留下一个假名字——桌面端没改成的话它什么也不会推,
 * 那个假名字要等下次全量快照才被纠正回去。
 * 传空串 = 清除自定义名(回落 shell 名),与桌面端右键重命名留空同义。
 */
export function renamePane(paneId: string, title: string): boolean {
  const { phase, desktopOnline } = useRelayStore.getState();
  if (phase !== 'connected' || desktopOnline === false) return false;
  sendToRelay({ type: 'renamePane', paneId, title: title.trim() });
  return true;
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

// ── 发起新 AI 会话 ──

let startTimer: ReturnType<typeof setTimeout> | null = null;

/** 结束一次发起流程并复位 + 按钮;error=null 表示成功收尾。 */
function finishStart(error: StartSessionFailReason | 'timeout' | null) {
  if (startTimer) {
    clearTimeout(startTimer);
    startTimer = null;
  }
  useRelayStore.setState({ starting: null, startError: error });
}

/** 开始超时计时;只有仍是同一次请求时才复位,避免误杀后来的请求。 */
function armStartTimeout(requestId: string) {
  if (startTimer) clearTimeout(startTimer);
  startTimer = setTimeout(() => {
    if (useRelayStore.getState().starting?.requestId === requestId) {
      finishStart('timeout');
    }
  }, START_SESSION_TIMEOUT_MS);
}

/** 在快照里找某个 pane 及其所属项目(用于自动进入镜像)。 */
function findPane(projects: MobileProject[], paneId: string) {
  for (const project of projects) {
    const pane = project.panes.find((p) => p.paneId === paneId);
    if (pane) return pane;
  }
  return undefined;
}

/**
 * 项目列表变化时检查:等待中的 pane 出现了没有。
 * 出现即视为"AI 真起来了",自动进入它的对话镜像。
 */
function onProjectsChanged() {
  const { starting, projects, mirror } = useRelayStore.getState();

  // 镜像页的标题是打开时从列表带入的副本:列表里的名字变了(手机改名、桌面端改名)
  // 得跟着走,否则正在看的这个会话还挂着旧名字
  if (mirror) {
    const current = findPane(projects, mirror.paneId);
    if (current && current.title !== mirror.title) {
      useRelayStore.setState({ mirror: { ...mirror, title: current.title } });
    }
  }

  if (!starting?.paneId) return;
  const pane = findPane(projects, starting.paneId);
  if (!pane) return;
  finishStart(null);
  openMirror(pane.paneId, pane.title);
}

/**
 * 发起一次新 AI 会话。返回 false = 当前不可发起(未连接 / 桌面离线 / 已有在途请求)。
 * 发起后进入"启动中",+ 按钮由 UI 按 `starting` 禁用,连点不会开出一串会话。
 */
export function startAiSession(
  projectId: string,
  projectName: string,
  launcherId: string,
): boolean {
  const { phase, desktopOnline, starting } = useRelayStore.getState();
  if (starting) return false;
  if (phase !== 'connected' || desktopOnline === false) return false;

  const requestId = crypto.randomUUID();
  useRelayStore.setState({
    starting: { requestId, projectName, paneId: null },
    startError: null,
  });
  sendToRelay({ type: 'startAiSession', requestId, projectId, launcherId });
  // 回执迟迟不回(中转/桌面端半死)也要有出路,不能让 + 按钮永远禁用
  armStartTimeout(requestId);
  return true;
}

/** 清除发起失败提示(UI 展示后调用)。 */
export function clearStartError() {
  if (useRelayStore.getState().startError) {
    useRelayStore.setState({ startError: null });
  }
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

/**
 * 下拉刷新：主动断开重连。
 *
 * 协议里没有「请求全量快照」这条消息（加一条要同时动 relay-server 的 protocol
 * crate 与桌面端），而握手成功后桌面端本来就会推一份完整 sessionsSnapshot ——
 * 重连即是最直接的「重新对齐」，且复用了已经跑熟的那条路径。
 *
 * 返回 false = 当前没有凭证，刷新无从谈起（未配对）。
 */
export function refreshSessions(): boolean {
  const cred = getCredential();
  if (!cred) return false;
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  reconnectAttempt = 0;
  // 先摘掉 onclose 再关，免得旧连接的关闭回调把我们刚发起的重连排到退避队列里
  if (ws) {
    ws.onclose = null;
    ws.close();
    ws = null;
  }
  connect({ credential: cred });
  return true;
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
