/**
 * 移动端 → 中转的 WebSocket 连接生命周期、握手、重连、多桌面切换、消息路由与深链调度。
 */
import {
  PROTOCOL_VERSION,
  type MobileHello,
  type MobileProject,
  type RelayToMobile,
} from '../protocol';
import {
  DEFAULT_DESKTOP_ID,
  DESKTOPS_STORAGE_KEY,
  backoffMs,
  clearHash,
  clearLegacyCredential,
  getActiveDesktopId,
  getLegacyCredential,
  getSavedDesktop,
  listSavedDesktops,
  readDesktopHash,
  readDesktopQuery,
  readPairingCode,
  readPaneLink,
  removeSavedDesktopEntry,
  setActiveDesktopId,
  syncDesktopUrl,
  updateSavedDesktopName,
  upsertSavedDesktop,
  wsUrl,
} from './auth';
import {
  failPendingAssistOffline,
  handleAssistMessage,
  resetAssistState,
} from './assist';
import {
  clearHistoryListTimer,
  clearHistoryPreviewTimer,
  failPendingHistoryOffline,
  handleHistoryMessage,
  resyncPendingHistory,
} from './history';
import {
  closeMirror,
  failPendingCommandDesktopOffline,
  failPendingCommandOnLinkLost,
  handleMirrorMessage,
  openMirror,
  resetCommandSlotAfterReconnect,
} from './mirror';
import { finishStart, handleStartSessionReceipt, onProjectsChanged } from './session';
import { findPane, resetDesktopSessionState, setPhase, useRelayStore } from './store';
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
/** 握手被拒/被吊销/手动停用后不再自动重连 */
let stopped = false;
/** 当前连接的中转是否支持多桌面端(helloAck 带 desktopId 即为 true) */
let relaySupportsMultiDesktop = false;

/**
 * 本标签页当前绑定的桌面端 id 与凭证(仅活在内存,不随其他标签页切机漂移)。
 * 多标签页同时分别打开 `?desktop=A` 和 `?desktop=B` 时,各管各的重连。
 */
let activeDesktopId: string | null = null;
let activeCredential: string | null = null;

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

/**
 * 这条连接没了(断线,或主动断开重连)。挂在它上面、等回执的请求不会从新连接回来:
 * - 在途指令置为「结果未知」(不自动重发,指令可能已执行);
 * - 在途的 @ / / 补全收敛,免得重连后浮层一直转圈、技能列表因「加载中」不再重拉。
 * 历史列表 / 预览不在这里收:收到新连接的快照后会用同一 requestId 补发(resyncPendingHistory)。
 */
function handleLinkLost() {
  failPendingCommandOnLinkLost();
  failPendingAssistOffline();
}

function teardownSocket() {
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  if (ws) {
    ws.onclose = null;
    ws.close();
    ws = null;
    handleLinkLost();
  }
}

function clearInFlightUiState() {
  clearHistoryListTimer();
  clearHistoryPreviewTimer();
  resetAssistState();
  finishStart(null);
  resetDesktopSessionState();
}

/** 打开深链指向的 pane;已在看它就不动,在看别的就先退订再进。 */
function openPaneLink(paneId: string) {
  const { projects, mirror } = useRelayStore.getState();
  const pane = findPane(projects, paneId);
  if (!pane) return;
  if (mirror?.paneId === pane.paneId && !mirror.closed) return;
  if (mirror) closeMirror();
  openMirror(pane.paneId, pane.title);
}

/** 查找本标签页当前应使用的重连凭证。 */
function currentReconnectAuth(): { desktopId: string | null; credential: string | null } {
  if (activeDesktopId) {
    const saved = getSavedDesktop(activeDesktopId);
    if (saved) return { desktopId: saved.desktopId, credential: saved.credential };
  }
  if (activeCredential) {
    return { desktopId: activeDesktopId, credential: activeCredential };
  }
  return { desktopId: activeDesktopId, credential: null };
}

/**
 * 过期配对码书签的回退目标(issue #96 + #106):
 * - 链接自带 `?desktop=<id>` 时,只回退到该桌面端的本地凭证(绝不误连另一台电脑);
 * - 旧版链接没有 `?desktop=` 时,仅当本地只有 1 台设备(或旧单键凭证)才回退。
 */
function fallbackCredentialForStaleCode(
  targetDesktopId?: string,
): { desktopId?: string; credential: string; fromLegacy?: boolean } | null {
  if (targetDesktopId) {
    const saved = getSavedDesktop(targetDesktopId);
    return saved ? { desktopId: saved.desktopId, credential: saved.credential } : null;
  }
  const all = listSavedDesktops();
  if (all.length === 1) {
    return { desktopId: all[0].desktopId, credential: all[0].credential };
  }
  if (all.length === 0) {
    const legacy = getLegacyCredential();
    if (legacy) return { credential: legacy, fromLegacy: true };
  }
  return null;
}

/**
 * `staleCode`:这次是配对码被拒后退回凭证的重连(见 `helloReject` 分支),
 * 凭证也不认时按「配对码无效」提示——用户要的是新二维码,不是「凭证已失效」。
 */
function connect(auth: {
  pairingCode?: string;
  credential?: string;
  targetDesktopId?: string;
  staleCode?: boolean;
  fromLegacy?: boolean;
}) {
  teardownSocket();
  stopped = false;
  snapshotReady = false;
  if (auth.targetDesktopId) {
    activeDesktopId = auth.targetDesktopId;
    const saved = getSavedDesktop(auth.targetDesktopId);
    useRelayStore.setState({
      currentDesktopId: auth.targetDesktopId,
      currentDesktopName: saved?.desktopName ?? useRelayStore.getState().currentDesktopName,
    });
  }
  if (auth.credential) {
    activeCredential = auth.credential;
  }

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
    if (ws !== socket) return;
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
        const desktopId = msg.desktopId ?? auth.targetDesktopId ?? DEFAULT_DESKTOP_ID;
        const credential = msg.credential ?? auth.credential ?? activeCredential;
        activeDesktopId = desktopId;
        activeCredential = credential ?? null;

        let savedDesktops = listSavedDesktops();
        const authoritativeName = msg.desktopId !== undefined;
        relaySupportsMultiDesktop = authoritativeName;
        if (credential) {
          savedDesktops = upsertSavedDesktop({
            desktopId,
            desktopName: msg.desktopName,
            credential,
            authoritativeName,
          });
          if (auth.fromLegacy) clearLegacyCredential();
        }
        setActiveDesktopId(desktopId);
        syncDesktopUrl(desktopId);
        const saved = savedDesktops.find((d) => d.desktopId === desktopId);
        // 手上的视图数据属于另一台桌面端(理论上切机都会先清,这里兜底):别把两台混着显示
        const prevEstablished = useRelayStore.getState().establishedDesktopId;
        if (prevEstablished !== null && prevEstablished !== desktopId) {
          clearInFlightUiState();
        }
        // 新连接:旧连接上在途指令的回执不会再来,单槽复位(此前断线时已置为结果未知)
        resetCommandSlotAfterReconnect();
        useRelayStore.setState({
          currentDesktopId: desktopId,
          currentDesktopName: authoritativeName
            ? (msg.desktopName?.trim() || null)
            : (msg.desktopName ?? saved?.desktopName ?? null),
          savedDesktops,
          establishedDesktopId: desktopId,
        });
        setPhase('connected');
        // 断线前在看的镜像不在这里重订阅:桌面端在线时中转会请它回发结构快照,
        // 收到快照再订阅(见 sessionsSnapshot 分支);桌面端不在线时订阅本来就到不了它
        break;
      }
      case 'helloReject': {
        stopped = true;
        // 握手被拒(凭证失效 / 配对码作废 / 版本不符)走全屏状态页:旧视图数据不再保留,
        // 否则重连期间保活主视图的判定会认它、返回手势也会去关看不见的视图
        clearInFlightUiState();
        if (msg.reason === 'invalidCredential') {
          if (auth.fromLegacy) {
            clearLegacyCredential();
          }
          const targetId = auth.targetDesktopId ?? activeDesktopId;
          if (targetId) {
            const savedDesktops = removeSavedDesktopEntry(targetId);
            activeCredential = null;
            useRelayStore.setState({ savedDesktops });
          }
        }
        // 配对码兑换不了(书签里存的是带 #pair= 的配对链接、码过期或已兑换过):
        // 若能精确定位到同一台桌面端的已保存凭证,则退回该凭证重连,别拦在错误页上
        if (msg.reason === 'invalidPairingCode') {
          const fallback = fallbackCredentialForStaleCode(auth.targetDesktopId);
          if (fallback) {
            connect({
              credential: fallback.credential,
              targetDesktopId: fallback.desktopId,
              fromLegacy: fallback.fromLegacy,
              staleCode: true,
            });
            break;
          }
        }
        setPhase(
          'rejected',
          auth.staleCode && msg.reason === 'invalidCredential' ? 'invalidPairingCode' : msg.reason,
        );
        break;
      }
      case 'revoked': {
        stopped = true;
        activeCredential = null;
        if (activeDesktopId) {
          const savedDesktops = removeSavedDesktopEntry(activeDesktopId);
          useRelayStore.setState({ savedDesktops });
        }
        clearInFlightUiState();
        setPhase('revoked');
        break;
      }
      case 'presence': {
        const patch: { desktopOnline: boolean; currentDesktopName?: string | null; savedDesktops?: ReturnType<typeof listSavedDesktops> } = {
          desktopOnline: msg.desktopOnline,
        };
        if ((msg.desktopName !== undefined || relaySupportsMultiDesktop) && activeDesktopId) {
          const nextName = msg.desktopName?.trim() || null;
          patch.currentDesktopName = nextName;
          patch.savedDesktops = updateSavedDesktopName(activeDesktopId, nextName ?? undefined);
        }
        useRelayStore.setState(patch);
        // 桌面端在等待期间掉线:在途的发起 / 历史 / 补全再也不会有下文,立刻给出原因而不是等超时;
        // 在途指令可能已写入也可能没有,置为「结果未知」
        if (!msg.desktopOnline) {
          if (useRelayStore.getState().starting) finishStart('desktopOffline');
          failPendingHistoryOffline();
          failPendingAssistOffline();
          failPendingCommandDesktopOffline();
        }
        break;
      }
      case 'sessionsSnapshot': {
        const supportsHistory = msg.supportsHistory === true;
        const supportsTerminalView = msg.supportsTerminalView === true;
        const supportsInputAssist = msg.supportsInputAssist === true;
        useRelayStore.setState((prev) => ({
          projects: msg.projects,
          launchers: msg.launchers ?? [],
          supportsHistory,
          supportsTerminalView,
          supportsInputAssist,
          // 旧桌面端不支持历史:避免停在不可用的历史 tab
          homeTab: supportsHistory ? prev.homeTab : 'active',
          // 若重连到了不支持终端视图的旧桌面端，自动回退回对话视图
          mirror:
            prev.mirror && !supportsTerminalView && prev.mirror.viewMode === 'terminal'
              ? { ...prev.mirror, viewMode: 'chat' }
              : prev.mirror,
        }));
        snapshotReady = true;
        // 全量快照 = 桌面端刚连上中转(或应本连接的握手请求回发):它那边的镜像与终端订阅
        // 此时是空的(断线即撤),正在看的会话要重新订阅一次,桌面端会回发最新快照。
        // 要在 onProjectsChanged 之前做:那里可能为刚发起的会话 openMirror,那次自带订阅
        const { mirror } = useRelayStore.getState();
        if (mirror && !mirror.closed) {
          sendToRelay({ type: 'subscribePane', paneId: mirror.paneId });
          if (supportsTerminalView && mirror.viewMode === 'terminal') {
            sendToRelay({ type: 'subscribeTerminal', paneId: mirror.paneId });
          }
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
      case 'terminalScreen':
        handleMirrorMessage(msg);
        break;
      case 'projectFiles':
      case 'projectSkills':
        handleAssistMessage(msg);
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
    handleLinkLost();
    if (stopped) return;
    const { desktopId, credential } = currentReconnectAuth();
    if (!credential) {
      // 配对失败且没有凭证可回退
      if (!handshakeDone) {
        useRelayStore.setState({ establishedDesktopId: null });
        setPhase('idle');
      }
      return;
    }
    reconnectAttempt += 1;
    // 首次连上之后的重连:主视图保持挂载(App 按 establishedDesktopId 判),只在顶部挂横幅
    setPhase('reconnecting');
    reconnectTimer = setTimeout(
      () =>
        connect({
          credential,
          targetDesktopId: desktopId ?? undefined,
          fromLegacy: auth.fromLegacy,
        }),
      backoffMs(reconnectAttempt),
    );
  };
}

/**
 * 切换到本机已保存的另一台桌面端。
 * 清空上一台电脑的会话与镜像状态,用目标桌面端凭证重建连接并同步 `?desktop=` URL。
 */
export function switchDesktop(desktopId: string): boolean {
  const entry = getSavedDesktop(desktopId);
  if (!entry) return false;
  const { phase } = useRelayStore.getState();
  if (
    activeDesktopId === desktopId &&
    (phase === 'connected' || phase === 'connecting' || phase === 'reconnecting')
  ) {
    return true;
  }
  reconnectAttempt = 0;
  clearInFlightUiState();
  activeDesktopId = entry.desktopId;
  activeCredential = entry.credential;
  setActiveDesktopId(entry.desktopId);
  syncDesktopUrl(entry.desktopId);
  useRelayStore.setState({
    currentDesktopId: entry.desktopId,
    currentDesktopName: entry.desktopName ?? null,
    savedDesktops: listSavedDesktops(),
  });
  connect({ credential: entry.credential, targetDesktopId: entry.desktopId });
  return true;
}

/**
 * 从本机列表移除一台已保存的桌面端(只清本机缓存,不向中转发起重置)。
 * 若移除的是当前正在查看的桌面端,自动切到剩余的第一台;删光后进入 idle。
 */
export function removeSavedDesktop(desktopId: string) {
  const remaining = removeSavedDesktopEntry(desktopId);
  useRelayStore.setState({ savedDesktops: remaining });
  if (activeDesktopId !== desktopId) return;

  teardownSocket();
  stopped = true;
  clearInFlightUiState();
  if (remaining.length > 0) {
    const next = remaining[0];
    activeDesktopId = next.desktopId;
    activeCredential = next.credential;
    setActiveDesktopId(next.desktopId);
    syncDesktopUrl(next.desktopId);
    useRelayStore.setState({
      currentDesktopId: next.desktopId,
      currentDesktopName: next.desktopName ?? null,
    });
    reconnectAttempt = 0;
    connect({ credential: next.credential, targetDesktopId: next.desktopId });
  } else {
    activeDesktopId = null;
    activeCredential = null;
    setActiveDesktopId(null);
    syncDesktopUrl(null);
    useRelayStore.setState({ currentDesktopId: null, currentDesktopName: null });
    setPhase('idle');
  }
}

/**
 * 下拉刷新:主动断开重连。
 *
 * 协议里没有「请求全量快照」这条消息,而握手成功后桌面端本来就会推一份完整
 * sessionsSnapshot —— 重连即是最直接的「重新对齐」,且复用了已经跑熟的那条路径。
 * 同一台桌面端的刷新期间主视图保持挂载(见 store 的 establishedDesktopId),
 * 列表的刷新指示器、镜像页的草稿与阅读位置都不会因为这次重连被卸载。
 *
 * 返回 false = 当前没有凭证,刷新无从谈起(未配对)。
 */
export function refreshSessions(): boolean {
  const { desktopId, credential } = currentReconnectAuth();
  if (!credential) return false;
  reconnectAttempt = 0;
  connect({ credential, targetDesktopId: desktopId ?? undefined });
  return true;
}

/**
 * 重连横幅上的「立即重试」:不等退避到点,马上再连一次(握手进行中则重来一次)。
 * 返回 false = 当前不在重连 / 已停用自动重连 / 没有凭证。
 */
export function retryConnectionNow(): boolean {
  if (stopped) return false;
  const { phase } = useRelayStore.getState();
  if (phase !== 'reconnecting' && phase !== 'connecting') return false;
  const { desktopId, credential } = currentReconnectAuth();
  if (!credential) return false;
  // 不复位 reconnectAttempt:这次再失败仍按退避走,界面也继续是「重新连接中」
  connect({ credential, targetDesktopId: desktopId ?? undefined });
  return true;
}

/** 应用启动入口:优先兑换 URL 里的配对码,否则按 `?desktop=` 或上次活跃桌面重连。 */
export function startRelay() {
  const savedDesktops = listSavedDesktops();
  useRelayStore.setState({ savedDesktops });

  // 深链与配对码先全部读出来,再统一清 hash
  const urlDesktopId = readDesktopQuery() ?? readDesktopHash();
  pendingPaneId = readPaneLink();
  const code = readPairingCode();
  if (code || pendingPaneId) clearHash();

  if (code) {
    // 扫了新码就走新配对(中转只顶替那一台桌面端原绑定的手机,本机保留其他电脑凭证)
    connect({ pairingCode: code, targetDesktopId: urlDesktopId ?? undefined });
    return;
  }

  // 指定了 ?desktop=<id>(书签 / 远程通知深链 / 刷新):精准连那一台,不存在则不误连别台
  if (urlDesktopId) {
    const target = getSavedDesktop(urlDesktopId);
    if (target) {
      activeDesktopId = target.desktopId;
      activeCredential = target.credential;
      useRelayStore.setState({
        currentDesktopId: target.desktopId,
        currentDesktopName: target.desktopName ?? null,
      });
      connect({ credential: target.credential, targetDesktopId: target.desktopId });
      return;
    }
    useRelayStore.setState({ currentDesktopId: urlDesktopId, currentDesktopName: null });
    setPhase('idle');
    return;
  }

  // 无 ?desktop= 参数(如主屏幕 PWA 图标打开):回落到上次活跃的电脑或列表首台
  const preferredId = getActiveDesktopId();
  const target =
    (preferredId ? getSavedDesktop(preferredId) : null) ?? savedDesktops[0] ?? null;
  if (target) {
    activeDesktopId = target.desktopId;
    activeCredential = target.credential;
    syncDesktopUrl(target.desktopId);
    useRelayStore.setState({
      currentDesktopId: target.desktopId,
      currentDesktopName: target.desktopName ?? null,
    });
    connect({ credential: target.credential, targetDesktopId: target.desktopId });
    return;
  }

  // 兼容旧版单桌面 PWA 留下的单键凭证:重连一次拿到 desktopId 后自动迁入多机表
  const legacy = getLegacyCredential();
  if (legacy) {
    connect({ credential: legacy, fromLegacy: true });
  } else {
    setPhase('idle');
  }
}

// 页面已经开着时再点一条通知或配对链接:同一文档只换了 hash,不会重新走 startRelay。
window.addEventListener('hashchange', () => {
  const code = readPairingCode();
  const paneId = readPaneLink();
  const targetDesktopId = readDesktopQuery() ?? readDesktopHash();
  if (!code && !paneId) return;
  clearHash();

  if (code) {
    pendingPaneId = paneId;
    clearInFlightUiState();
    reconnectAttempt = 0;
    connect({ pairingCode: code, targetDesktopId: targetDesktopId ?? undefined });
    return;
  }

  if (paneId) {
    if (targetDesktopId && targetDesktopId !== activeDesktopId) {
      pendingPaneId = paneId;
      if (switchDesktop(targetDesktopId)) return;
    }
    if (useRelayStore.getState().phase === 'connected' && snapshotReady) {
      openPaneLink(paneId);
    } else {
      pendingPaneId = paneId;
    }
  }
});

// 同浏览器多标签页同步设备列表(例如在标签页 B 扫了新电脑,标签页 A 的切换器立刻看到)
window.addEventListener('storage', (ev) => {
  if (ev.key !== DESKTOPS_STORAGE_KEY) return;
  const savedDesktops = listSavedDesktops();
  const current = activeDesktopId
    ? savedDesktops.find((d) => d.desktopId === activeDesktopId)
    : undefined;
  useRelayStore.setState((prev) => ({
    savedDesktops,
    currentDesktopName: current?.desktopName ?? prev.currentDesktopName,
  }));
});

/** 页面最近一次转入后台的时刻;null = 在前台。 */
let hiddenAt: number | null = null;

/** 正处于断线重连的退避等待里:不等了,立即重连。 */
function reconnectNowIfWaiting() {
  const { phase } = useRelayStore.getState();
  if (phase === 'reconnecting' && !ws) {
    if (reconnectTimer) clearTimeout(reconnectTimer);
    reconnectTimer = null;
    const { desktopId, credential } = currentReconnectAuth();
    if (credential) {
      connect({ credential, targetDesktopId: desktopId ?? undefined });
    }
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

/** 已连接状态下经历过一次断网(浏览器未必因此报 close,连接可能已是死的)。 */
let wentOfflineWhileConnected = false;

window.addEventListener('offline', () => {
  if (useRelayStore.getState().phase === 'connected') wentOfflineWhileConnected = true;
});

// 网络恢复(切 Wi-Fi / 蜂窝、出电梯):在退避等待里就别等到点了;断网期间一直显示
// 「已连接」的,连接多半已死却没报 close(同回前台的口径),直接重连对齐一次
window.addEventListener('online', () => {
  const stale = wentOfflineWhileConnected;
  wentOfflineWhileConnected = false;
  if (stale && !stopped && useRelayStore.getState().phase === 'connected') {
    refreshSessions();
    return;
  }
  reconnectNowIfWaiting();
});
