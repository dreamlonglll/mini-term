/**
 * 移动端 → 中转的 WebSocket 连接生命周期、握手、重连、消息路由与深链调度。
 */
import {
  PROTOCOL_VERSION,
  type MobileHello,
  type MobileProject,
  type RelayToMobile,
} from '../protocol';
import {
  backoffMs,
  clearCredential,
  clearHash,
  consumePairingCode,
  getCredential,
  readPaneLink,
  saveCredential,
  wsUrl,
} from './auth';
import {
  failPendingHistoryOffline,
  handleHistoryMessage,
  resyncPendingHistory,
} from './history';
import { closeMirror, handleMirrorMessage, openMirror } from './mirror';
import { finishStart, handleStartSessionReceipt, onProjectsChanged } from './session';
import { findPane, setPhase, useRelayStore } from './store';
import { registerTransportSender, sendToRelay } from './transport';

/**
 * 建连 + 握手(收到 helloAck)的上限。浏览器自己的 WebSocket 建连超时动辄几十秒到
 * 几分钟,期间界面一直是「连接中」;到点就关掉,交给断线重连的退避重试。
 */
const HANDSHAKE_TIMEOUT_MS = 10_000;

/**
 * 页面在后台待了这么久,回前台就直接重连。手机切后台后系统会冻结或掐掉连接,
 * 中间网络也会回收空闲连接;浏览器不一定马上报 close,界面还显示「已连接」,
 * 发出去的订阅 / 指令却石沉大海。协议里没有应用层心跳可以探活,重连是最直接的
 * 「重新对齐」:握手后桌面端会推完整快照,正在看的会话随之重订阅。
 */
const STALE_AFTER_HIDDEN_MS = 30_000;

let ws: WebSocket | null = null;
let reconnectAttempt = 0;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
/** 握手被拒/被吊销后不再自动重连 */
let stopped = false;

/**
 * 等着打开的深链 pane。只试一次:收到快照时找得到就进镜像,找不到说明会话已经结束,
 * 留在列表页(再等下去只会在用户已经在看别的东西时突然跳走)。
 */
let pendingPaneId: string | null = null;

/**
 * 这条连接收到过 sessionsSnapshot 没有(每次建连复位)。握手成功到首份快照之间
 * (桌面端离线时这段可以很长)项目列表是空的或上一条连接的陈旧数据,深链在这期间
 * 只能挂起等快照,不能当场按列表判「找不到」丢掉。
 */
let snapshotReady = false;

registerTransportSender((msg) => {
  if (ws && ws.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify(msg));
  }
});

/** 打开深链指向的 pane;已在看它就不动,在看别的就先退订再进。 */
function openPaneLink(paneId: string) {
  const { projects, mirror } = useRelayStore.getState();
  const pane = findPane(projects, paneId);
  if (!pane) return;
  if (mirror?.paneId === pane.paneId && !mirror.closed) return;
  if (mirror) closeMirror();
  openMirror(pane.paneId, pane.title);
}

/**
 * `staleCode`:这次是配对码被拒后退回凭证的重连(见 `helloReject` 分支),
 * 凭证也不认时按「配对码无效」提示——用户要的是新二维码,不是「凭证已失效」。
 */
function connect(auth: { pairingCode?: string; credential?: string; staleCode?: boolean }) {
  stopped = false;
  snapshotReady = false;
  if (auth.pairingCode) {
    setPhase('pairing');
  } else {
    setPhase(reconnectAttempt > 0 ? 'reconnecting' : 'connecting');
  }

  const socket = new WebSocket(wsUrl());
  ws = socket;
  let handshakeDone = false;
  // 到点还没握手成功:关掉,走 onclose 的退避重连(CONNECTING 态 close 也会触发 onclose)
  const handshakeTimer = setTimeout(() => {
    if (!handshakeDone && ws === socket) socket.close();
  }, HANDSHAKE_TIMEOUT_MS);

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
        clearTimeout(handshakeTimer);
        reconnectAttempt = 0;
        if (msg.credential) saveCredential(msg.credential);
        setPhase('connected');
        // 断线前在看的镜像不在这里重订阅:桌面端在线时中转会请它回发结构快照,
        // 收到快照再订阅(见 sessionsSnapshot 分支);桌面端不在线时订阅本来就到不了它
        break;
      }
      case 'helloReject': {
        stopped = true;
        if (msg.reason === 'invalidCredential') clearCredential();
        // 配对码兑换不了(书签里存的是带 #pair= 的配对链接、码过期或已兑换过),本地还留着
        // 上次配对的凭证:退回凭证重连,别把用户拦在「配对码无效」页上。一个中转只对一台
        // 桌面端,这把凭证连的就是同一台。凭证重连只会回 invalidCredential,不会再进这里
        if (msg.reason === 'invalidPairingCode') {
          const cred = getCredential();
          if (cred) {
            connect({ credential: cred, staleCode: true });
            break;
          }
        }
        setPhase(
          'rejected',
          auth.staleCode && msg.reason === 'invalidCredential' ? 'invalidPairingCode' : msg.reason,
        );
        break;
      }
      case 'revoked':
        stopped = true;
        clearCredential();
        setPhase('revoked');
        break;
      case 'presence': {
        useRelayStore.setState({ desktopOnline: msg.desktopOnline });
        // 桌面端在等待期间掉线:在途的发起 / 历史拉取再也不会有下文,立刻给出原因而不是等超时
        if (!msg.desktopOnline) {
          if (useRelayStore.getState().starting) finishStart('desktopOffline');
          failPendingHistoryOffline();
        }
        break;
      }
      case 'sessionsSnapshot': {
        const supportsHistory = msg.supportsHistory === true;
        useRelayStore.setState((prev) => ({
          projects: msg.projects,
          launchers: msg.launchers ?? [],
          supportsHistory,
          // 旧桌面端不支持历史:避免停在不可用的历史 tab
          homeTab: supportsHistory ? prev.homeTab : 'active',
        }));
        snapshotReady = true;
        // 全量快照 = 桌面端刚连上中转(或应本连接的握手请求回发):它那边的镜像订阅
        // 此时是空的(断线即撤),正在看的会话要重新订阅一次,桌面端会回发镜像快照。
        // 要在 onProjectsChanged 之前做:那里可能为刚发起的会话 openMirror,那次自带订阅
        const { mirror } = useRelayStore.getState();
        if (mirror && !mirror.closed) {
          sendToRelay({ type: 'subscribePane', paneId: mirror.paneId });
        }
        resyncPendingHistory(supportsHistory);
        onProjectsChanged();
        if (pendingPaneId) {
          const paneId = pendingPaneId;
          pendingPaneId = null;
          openPaneLink(paneId);
        }
        break;
      }
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
      case 'mirrorSnapshot':
      case 'mirrorAppend':
      case 'mirrorHistory':
      case 'paneClosed':
      case 'commandReceipt':
        handleMirrorMessage(msg);
        break;
      case 'startSessionReceipt':
        handleStartSessionReceipt(msg);
        break;
      case 'sessionHistory':
      case 'sessionContent':
        handleHistoryMessage(msg);
        break;
    }
  };

  socket.onclose = () => {
    clearTimeout(handshakeTimer);
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
  // 深链先读(配对码那一步会把整个 hash 清掉);连上后的首个快照里兑现
  pendingPaneId = readPaneLink();
  const code = consumePairingCode();
  if (pendingPaneId && !code) clearHash();
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

// 页面已经开着时再点一条通知:同一文档只换了 hash,不会重新走 startRelay。
// 这条连接已收到过快照就当场打开;还在连、或连上了但快照没到(桌面端离线),
// 就记下,等快照到了再兑现 —— 与冷启动走 startRelay 的那条路同一口径。
window.addEventListener('hashchange', () => {
  const paneId = readPaneLink();
  if (!paneId) return;
  clearHash();
  if (useRelayStore.getState().phase === 'connected' && snapshotReady) {
    openPaneLink(paneId);
  } else {
    pendingPaneId = paneId;
  }
});

/** 页面最近一次转入后台的时刻;null = 在前台。 */
let hiddenAt: number | null = null;

/** 正处于断线重连的退避等待里:不等了,立即重连。 */
function reconnectNowIfWaiting() {
  const { phase } = useRelayStore.getState();
  if (phase === 'reconnecting' && !ws) {
    if (reconnectTimer) clearTimeout(reconnectTimer);
    reconnectTimer = null;
    const cred = getCredential();
    if (cred) connect({ credential: cred });
  }
}

// 手机浏览器切后台常导致连接被杀:回前台时,待得久就直接重连(连接多半已死却未必
// 报 close,见 STALE_AFTER_HIDDEN_MS),否则只在处于重连等待时立即重试
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState !== 'visible') {
    hiddenAt = Date.now();
    return;
  }
  const hiddenFor = hiddenAt === null ? 0 : Date.now() - hiddenAt;
  hiddenAt = null;
  const { phase } = useRelayStore.getState();
  const live = phase === 'connected' || phase === 'connecting' || phase === 'reconnecting';
  if (hiddenFor >= STALE_AFTER_HIDDEN_MS && live && !stopped) {
    refreshSessions();
    return;
  }
  reconnectNowIfWaiting();
});

// 网络恢复(切 Wi-Fi / 蜂窝、出电梯):在退避等待里就别等到点了
window.addEventListener('online', reconnectNowIfWaiting);
