import type {
  MobileCommandReceipt,
  MobileMirrorAppend,
  MobileMirrorHistory,
  MobileMirrorSnapshot,
  MobilePaneClosed,
} from '../protocol';
import { type MirrorState, useRelayStore } from './store';
import { sendToRelay } from './transport';

export type MirrorIncomingMessage =
  | MobileMirrorSnapshot
  | MobileMirrorAppend
  | MobileMirrorHistory
  | MobilePaneClosed
  | MobileCommandReceipt;

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
      pendingAnswer: null,
      answeredProgress: {},
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

/** 镜像页动作的公共前置:有镜像且未关闭、已连接、桌面端在线。不满足返回 null。 */
function mirrorReady(): MirrorState | null {
  const { mirror, desktopOnline, phase } = useRelayStore.getState();
  if (!mirror || mirror.closed) return null;
  if (phase !== 'connected' || desktopOnline === false) return null;
  return mirror;
}

/** 发送移动端指令(写穿,不排队)。返回 false = 当前不可发送。 */
export function sendMobileCommand(text: string): boolean {
  const mirror = mirrorReady();
  const trimmed = text.trim();
  if (!mirror || !trimmed) return false;
  const commandId = crypto.randomUUID();
  useRelayStore.setState({
    mirror: { ...mirror, pendingCommandId: commandId, receipt: null },
  });
  sendToRelay({ type: 'mobileCommand', paneId: mirror.paneId, commandId, text: trimmed });
  return true;
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
  const commandId = crypto.randomUUID();
  useRelayStore.setState({
    mirror: {
      ...mirror,
      pendingCommandId: commandId,
      pendingAnswer: { commandId, seq },
      receipt: null,
    },
  });
  sendToRelay({
    type: 'answerQuestion',
    paneId: mirror.paneId,
    commandId,
    seq,
    questionId,
    questionIndex,
    optionIndex,
  });
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

/** 处理对话镜像相关的下行消息。 */
export function handleMirrorMessage(msg: MirrorIncomingMessage) {
  const { mirror } = useRelayStore.getState();
  if (!mirror || mirror.paneId !== msg.paneId) return;

  switch (msg.type) {
    case 'mirrorSnapshot':
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
    case 'mirrorAppend':
      useRelayStore.setState({
        mirror: { ...mirror, messages: [...mirror.messages, ...msg.messages] },
      });
      break;
    case 'mirrorHistory':
      useRelayStore.setState({
        mirror: {
          ...mirror,
          messages: [...msg.messages, ...mirror.messages],
          hasMore: msg.hasMore,
          loadingOlder: false,
        },
      });
      break;
    case 'paneClosed':
      useRelayStore.setState({ mirror: { ...mirror, closed: true } });
      break;
    case 'commandReceipt': {
      if (mirror.pendingCommandId !== null && mirror.pendingCommandId !== msg.commandId) return;
      // 点选作答成功:本地推进该卡片的作答进度(见 answeredProgress 注释)
      const answeredProgress = { ...mirror.answeredProgress };
      if (msg.ok && mirror.pendingAnswer?.commandId === msg.commandId) {
        const seq = mirror.pendingAnswer.seq;
        answeredProgress[seq] = (answeredProgress[seq] ?? 0) + 1;
      }
      useRelayStore.setState({
        mirror: {
          ...mirror,
          pendingCommandId: null,
          pendingAnswer: null,
          answeredProgress,
          receipt: { ok: msg.ok, reason: msg.reason },
        },
      });
      break;
    }
  }
}
