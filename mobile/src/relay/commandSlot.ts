/**
 * 移动端指令 / 点选作答共用的「单槽」状态机(纯函数,零运行时依赖,
 * `tests/mobilePureLogic.test.cjs` 直接引用;计时器与 store 写入在 `mirror.ts`)。
 *
 * 一次只允许一条在途指令(两者本就互斥:提问挂起时 agent 在等作答,不会同时有别的指令)。
 *
 *   idle ──begin──▶ sending ──回执 ok──────▶ idle(receipt=ok)
 *                     │      ──回执失败────▶ idle(receipt=failed)
 *                     │      ──超时 15s────▶ idle(receipt=unknown/timeout)
 *                     │      ──连接断开────▶ idle(receipt=unknown/disconnected)
 *                     └──── ──桌面端掉线──▶ idle(receipt=unknown/desktopOffline)
 *
 * 「结果未知」而不是「失败」:指令可能已经写进了桌面终端(写完回执还没到就断了),
 * 自动重发可能让 AI 执行两次,所以**绝不自动重发**,只提示用户去对话里确认。
 * 结果未知之后、同一连接上该指令的回执若迟到(多见于超时),按回执把结果升级为确定的
 * ok / failed;换了连接回执不可能再来(中转不缓存回执),重连后单槽直接复位。
 */
import type { CommandFailReason } from '../protocol';

export type CommandUnknownCause = 'timeout' | 'disconnected' | 'desktopOffline';

/** 指令回执在输入区的展示态(短暂展示后由 UI 清除)。 */
export type CommandReceiptView =
  | { commandId: string; status: 'ok' }
  | { commandId: string; status: 'failed'; reason?: CommandFailReason }
  | { commandId: string; status: 'unknown'; cause: CommandUnknownCause };

export interface CommandSlot {
  /** 等待回执中的指令 id;null = 没有在途指令 */
  pendingCommandId: string | null;
  /** 在途的点选作答(commandId 与 pendingCommandId 同值,额外记住卡片 seq) */
  pendingAnswer: { commandId: string; seq: number } | null;
  /**
   * 上一条「结果未知」的指令:同一连接上它的回执迟到时据此认领并升级结果。
   * 发下一条指令或重连时清掉。answerSeq = 它是点选作答时的卡片 seq。
   */
  unsettled: { commandId: string; answerSeq: number | null } | null;
  /** 各提问卡片已成功注入的作答数(卡片 seq → 已答题数),回执 ok 即推进 */
  answeredProgress: Record<number, number>;
  /** 最近一次指令回执的展示态 */
  receipt: CommandReceiptView | null;
}

export interface IncomingCommandReceipt {
  commandId: string;
  ok: boolean;
  reason?: CommandFailReason;
}

export const EMPTY_COMMAND_SLOT: CommandSlot = {
  pendingCommandId: null,
  pendingAnswer: null,
  unsettled: null,
  answeredProgress: {},
  receipt: null,
};

/** 占槽发出一条指令(answerSeq 非 null = 点选作答)。调用方先确认槽是空的。 */
export function beginCommand<S extends CommandSlot>(
  slot: S,
  commandId: string,
  answerSeq: number | null = null,
): S {
  return {
    ...slot,
    pendingCommandId: commandId,
    pendingAnswer: answerSeq === null ? null : { commandId, seq: answerSeq },
    unsettled: null,
    receipt: null,
  };
}

function advanceProgress(progress: Record<number, number>, seq: number | null, ok: boolean) {
  if (!ok || seq === null) return progress;
  return { ...progress, [seq]: (progress[seq] ?? 0) + 1 };
}

/**
 * 收到一条回执。返回 null = 与单槽无关(陈旧回执 / 别的指令),调用方丢弃。
 * - 正是在途那条:落定 ok / failed;
 * - 正是「结果未知」的那条(迟到):升级为确定结果;
 * - 其它:不认。
 */
export function settleCommandReceipt<S extends CommandSlot>(
  slot: S,
  msg: IncomingCommandReceipt,
): S | null {
  const view: CommandReceiptView = msg.ok
    ? { commandId: msg.commandId, status: 'ok' }
    : { commandId: msg.commandId, status: 'failed', reason: msg.reason };
  if (slot.pendingCommandId !== null && slot.pendingCommandId === msg.commandId) {
    const answerSeq =
      slot.pendingAnswer?.commandId === msg.commandId ? slot.pendingAnswer.seq : null;
    return {
      ...slot,
      pendingCommandId: null,
      pendingAnswer: null,
      unsettled: null,
      answeredProgress: advanceProgress(slot.answeredProgress, answerSeq, msg.ok),
      receipt: view,
    };
  }
  if (slot.unsettled !== null && slot.unsettled.commandId === msg.commandId) {
    return {
      ...slot,
      unsettled: null,
      answeredProgress: advanceProgress(slot.answeredProgress, slot.unsettled.answerSeq, msg.ok),
      receipt: view,
    };
  }
  return null;
}

/**
 * 在途指令等不到回执了:置为「结果未知」并腾出单槽。返回 null = 没有在途指令(或不是
 * `onlyCommandId` 指定的那条——超时计时器只该过期它自己那条),无事可做。
 */
export function expireCommand<S extends CommandSlot>(
  slot: S,
  cause: CommandUnknownCause,
  onlyCommandId?: string,
): S | null {
  const id = slot.pendingCommandId;
  if (id === null) return null;
  if (onlyCommandId !== undefined && onlyCommandId !== id) return null;
  const answerSeq = slot.pendingAnswer?.commandId === id ? slot.pendingAnswer.seq : null;
  return {
    ...slot,
    pendingCommandId: null,
    pendingAnswer: null,
    unsettled: { commandId: id, answerSeq },
    receipt: { commandId: id, status: 'unknown', cause },
  };
}

/**
 * 换了一条新连接:旧连接上的回执不可能再来。还在途的按「断线」过期,迟到认领也不再等。
 * 返回 null = 没有要变的。
 */
export function resetCommandSlotForNewLink<S extends CommandSlot>(slot: S): S | null {
  const expired = expireCommand(slot, 'disconnected');
  if (expired) return { ...expired, unsettled: null };
  if (slot.unsettled !== null) return { ...slot, unsettled: null };
  return null;
}

/**
 * 回执落定后输入框该显示什么(草稿只在「确定写入成功」时才清):
 * - ok 且输入框还是发出去的那段原文 → 清空;
 * - ok 但用户在等回执期间改过 → 保留用户的新内容;
 * - failed / unknown → 原样保留(草稿本就还在输入框里,用户可以检查后手动重发)。
 */
export function draftAfterReceipt(
  currentText: string,
  sentText: string,
  status: CommandReceiptView['status'],
): string {
  return status === 'ok' && currentText === sentText ? '' : currentText;
}
