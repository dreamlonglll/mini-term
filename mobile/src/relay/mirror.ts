import type {
  MobileCommandReceipt,
  MobileMirrorAppend,
  MobileMirrorHistory,
  MobileMirrorSnapshot,
  MobilePaneClosed,
  MobileTerminalScreen,
  TerminalKey,
} from '../protocol';
import { resetAssistState } from './assist';
import {
  EMPTY_COMMAND_SLOT,
  beginCommand,
  expireCommand,
  resetCommandSlotForNewLink,
  settleCommandReceipt,
} from './commandSlot';
import {
  appendMirrorMessages,
  mergeMirrorSnapshot,
  prependOlderMirrorMessages,
} from './mirrorMerge';
import { newRequestId } from './requestId';
import { type MirrorState, type MirrorViewMode, useRelayStore } from './store';
import { sendToRelay } from './transport';

export type MirrorIncomingMessage =
  | MobileMirrorSnapshot
  | MobileMirrorAppend
  | MobileMirrorHistory
  | MobilePaneClosed
  | MobileCommandReceipt
  | MobileTerminalScreen;

/**
 * 指令 / 点选作答等回执的上限,到点按「结果未知」收场(与 history.ts 的 15s 同口径)。
 * 中转在桌面端离线时会立刻回失败回执,正常写入也是毫秒级——等这么久还没有,多半是
 * 回执在路上丢了(手机恰好断线、桌面端写完就掉线),再等下去按钮只会一直停在「发送中」。
 */
const COMMAND_RECEIPT_TIMEOUT_MS = 15_000;

let commandTimer: ReturnType<typeof setTimeout> | null = null;

function clearCommandTimer() {
  if (commandTimer) {
    clearTimeout(commandTimer);
    commandTimer = null;
  }
}

function armCommandTimeout(commandId: string) {
  clearCommandTimer();
  commandTimer = setTimeout(() => {
    commandTimer = null;
    const { mirror } = useRelayStore.getState();
    if (!mirror) return;
    // 只过期自己这条:期间换了会话 / 已落定,这里什么也不做
    const next = expireCommand(mirror, 'timeout', commandId);
    if (next) useRelayStore.setState({ mirror: next });
  }, COMMAND_RECEIPT_TIMEOUT_MS);
}

/**
 * 追踪发出的终端应急按键 commandId：允许高频连点（不占单槽 pendingCommandId），
 * 成功时静默消费回执，仅失败时在输入区提示。
 */
const pendingTerminalKeyIds = new Set<string>();

/** 进入某 pane 的对话镜像:登记视图状态并向桌面端订阅。 */
export function openMirror(paneId: string, title: string) {
  pendingTerminalKeyIds.clear();
  clearCommandTimer();
  resetAssistState();
  useRelayStore.setState({
    mirror: {
      ...EMPTY_COMMAND_SLOT,
      paneId,
      title,
      messages: [],
      hasMore: false,
      loaded: false,
      loadingOlder: false,
      closed: false,
      viewMode: 'chat',
      terminalScreen: null,
      terminalLoaded: false,
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

/** 镜像页动作的公共前置:有镜像且未关闭、已连接、桌面端在线。不满足返回 null。 */
function mirrorReady(): MirrorState | null {
  const { mirror, desktopOnline, phase } = useRelayStore.getState();
  if (!mirror || mirror.closed) return null;
  if (phase !== 'connected' || desktopOnline === false) return null;
  return mirror;
}

/**
 * 发送移动端指令(写穿,不排队)。返回本次的 commandId;null = 当前不可发送
 * (未连接 / 重连中 / 桌面端离线 / 单槽里已有在途指令 / 空文本),什么也没发出去。
 * 结果看 `mirror.receipt`(commandId 对得上的那条):ok / failed / unknown。
 */
export function sendMobileCommand(text: string): string | null {
  const mirror = mirrorReady();
  const trimmed = text.trim();
  if (!mirror || !trimmed || mirror.pendingCommandId != null) return null;
  const commandId = newRequestId();
  useRelayStore.setState({ mirror: beginCommand(mirror, commandId) });
  sendToRelay({ type: 'mobileCommand', paneId: mirror.paneId, commandId, text: trimmed });
  armCommandTimeout(commandId);
  return commandId;
}

/**
 * 点选作答 agent 的提问。返回 false = 当前不可作答。
 * 与手输指令共用 pendingCommandId 单槽与回执展示——两者本就互斥
 * (提问挂起时 agent 在等作答,不会同时有别的在途指令)。
 */
export function answerQuestion(
  seq: number,
  questionId: string,
  questionIndex: number,
  optionIndex: number,
): boolean {
  const mirror = mirrorReady();
  if (!mirror || mirror.pendingCommandId != null) return false;
  const commandId = newRequestId();
  useRelayStore.setState({ mirror: beginCommand(mirror, commandId, seq) });
  sendToRelay({
    type: 'answerQuestion',
    paneId: mirror.paneId,
    commandId,
    seq,
    questionId,
    questionIndex,
    optionIndex,
  });
  armCommandTimeout(commandId);
  return true;
}

/** 清除回执提示(UI 展示几秒后调用)。 */
export function clearCommandReceipt() {
  const { mirror } = useRelayStore.getState();
  if (mirror?.receipt) {
    useRelayStore.setState({ mirror: { ...mirror, receipt: null } });
  }
}

/**
 * 与中转的连接断了(socket 关闭 / 主动重连):在途指令的回执不可能再从新连接回来,
 * 置为「结果未知」腾出单槽(绝不自动重发);终端按键的回执同理不再等。
 */
export function failPendingCommandOnLinkLost() {
  clearCommandTimer();
  pendingTerminalKeyIds.clear();
  const { mirror } = useRelayStore.getState();
  if (!mirror) return;
  const next = expireCommand(mirror, 'disconnected');
  if (next) useRelayStore.setState({ mirror: next });
}

/**
 * 桌面端在等回执期间掉线:指令可能已写入也可能没有,置为「结果未知」。
 * (若中转随后补来该指令的失败回执,`settleCommandReceipt` 会把结果升级为确定的失败。)
 */
export function failPendingCommandDesktopOffline() {
  const { mirror } = useRelayStore.getState();
  if (!mirror || mirror.pendingCommandId === null) return;
  clearCommandTimer();
  const next = expireCommand(mirror, 'desktopOffline');
  if (next) useRelayStore.setState({ mirror: next });
}

/** 新连接握手成功:复位单槽(旧连接上的回执与迟到认领都作废)。 */
export function resetCommandSlotAfterReconnect() {
  clearCommandTimer();
  pendingTerminalKeyIds.clear();
  const { mirror } = useRelayStore.getState();
  if (!mirror) return;
  const next = resetCommandSlotForNewLink(mirror);
  if (next) useRelayStore.setState({ mirror: next });
}

/** 在对话视图与终端视图之间切换，按需订阅 / 退订终端屏幕快照流。 */
export function setMirrorViewMode(mode: MirrorViewMode) {
  const { mirror, supportsTerminalView } = useRelayStore.getState();
  if (!mirror) return;
  if (mode === 'terminal' && !supportsTerminalView) return;
  if (mirror.viewMode === mode) return;

  if (mode === 'terminal') {
    useRelayStore.setState({
      mirror: {
        ...mirror,
        viewMode: 'terminal',
        terminalLoaded: mirror.terminalScreen !== null,
      },
    });
    if (!mirror.closed) {
      sendToRelay({ type: 'subscribeTerminal', paneId: mirror.paneId });
    }
  } else {
    sendToRelay({ type: 'unsubscribeTerminal', paneId: mirror.paneId });
    useRelayStore.setState({
      mirror: { ...mirror, viewMode: 'chat' },
    });
  }
}

/** 向当前活跃 AI pane 发送应急特殊按键（不阻塞连点，失败时经 receipt 提示）。 */
export function sendTerminalKey(key: TerminalKey): boolean {
  const mirror = mirrorReady();
  if (!mirror) return false;
  const commandId = newRequestId();
  if (pendingTerminalKeyIds.size > 64) {
    pendingTerminalKeyIds.clear();
  }
  pendingTerminalKeyIds.add(commandId);
  sendToRelay({ type: 'sendTerminalKey', paneId: mirror.paneId, commandId, key });
  return true;
}

/** 退出镜像返回列表:退订(含终端快照流)并清空视图与补全状态。 */
export function closeMirror() {
  const { mirror } = useRelayStore.getState();
  if (mirror) {
    if (mirror.viewMode === 'terminal') {
      sendToRelay({ type: 'unsubscribeTerminal', paneId: mirror.paneId });
    }
    sendToRelay({ type: 'unsubscribePane', paneId: mirror.paneId });
  }
  pendingTerminalKeyIds.clear();
  clearCommandTimer();
  resetAssistState();
  useRelayStore.setState({ mirror: null });
}

/** 上拉加载更早历史(以当前最早消息的 seq 为锚)。重连中不发:请求到不了,只会一直转圈。 */
export function loadOlderMirror() {
  const { mirror, phase } = useRelayStore.getState();
  if (phase !== 'connected') return;
  if (!mirror || !mirror.hasMore || mirror.loadingOlder || mirror.messages.length === 0) return;
  useRelayStore.setState({ mirror: { ...mirror, loadingOlder: true } });
  sendToRelay({
    type: 'requestMirrorHistory',
    paneId: mirror.paneId,
    beforeSeq: mirror.messages[0].seq,
  });
}

/** 处理对话镜像相关的下行消息。 */
export function handleMirrorMessage(msg: MirrorIncomingMessage) {
  const { mirror } = useRelayStore.getState();
  if (!mirror || mirror.paneId !== msg.paneId) return;

  switch (msg.type) {
    case 'mirrorSnapshot': {
      // 首个快照直接用;同一会话的后续快照(重连后重订阅 / 桌面端换绑重发)先按 seq 合并,
      // 保住已「加载更早」拿到的历史,判不准是同一条时间线时整体替换(规则见 mirrorMerge.ts)
      const next = mirror.loaded
        ? mergeMirrorSnapshot(mirror.messages, mirror.hasMore, msg.messages, msg.hasMore)
        : { messages: msg.messages, hasMore: msg.hasMore };
      useRelayStore.setState({
        mirror: {
          ...mirror,
          messages: next.messages,
          hasMore: next.hasMore,
          loaded: true,
          loadingOlder: false,
        },
      });
      break;
    }
    case 'mirrorAppend': {
      const messages = appendMirrorMessages(mirror.messages, msg.messages);
      if (messages !== mirror.messages) {
        useRelayStore.setState({ mirror: { ...mirror, messages } });
      }
      break;
    }
    case 'mirrorHistory':
      // 没有在途的分页请求(快照已把 loadingOlder 复位):这是换绑 / 重订阅之前那次请求的
      // 陈旧响应,拼进来可能混入另一段会话
      if (!mirror.loadingOlder) return;
      useRelayStore.setState({
        mirror: {
          ...mirror,
          messages: prependOlderMirrorMessages(mirror.messages, msg.messages),
          hasMore: msg.hasMore,
          loadingOlder: false,
        },
      });
      break;
    case 'paneClosed':
      useRelayStore.setState({ mirror: { ...mirror, closed: true } });
      break;
    case 'terminalScreen':
      useRelayStore.setState({
        mirror: {
          ...mirror,
          terminalLoaded: true,
          terminalScreen: {
            cols: msg.cols,
            rows: msg.rows,
            cursorRow: msg.cursorRow,
            cursorCol: msg.cursorCol,
            cursorVisible: msg.cursorVisible,
            altScreen: msg.altScreen,
            lines: msg.lines ?? [],
          },
        },
      });
      break;
    case 'commandReceipt': {
      // 终端应急按键回执：成功静默消费，仅失败时弹出提示，绝不干扰手输命令单槽
      if (pendingTerminalKeyIds.delete(msg.commandId)) {
        if (!msg.ok) {
          useRelayStore.setState({
            mirror: {
              ...mirror,
              receipt: { commandId: msg.commandId, status: 'failed', reason: msg.reason },
            },
          });
        }
        return;
      }
      const next = settleCommandReceipt(mirror, msg);
      if (!next) return;
      if (mirror.pendingCommandId === msg.commandId) clearCommandTimer();
      useRelayStore.setState({ mirror: next });
      break;
    }
  }
}
