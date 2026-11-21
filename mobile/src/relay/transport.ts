import type { MobileToRelay } from '../protocol';

type Sender = (msg: MobileToRelay) => void;

let activeSender: Sender | null = null;

/** 由 connection 层登记当前 WebSocket 发送实现,避免子模块循环依赖 connection。 */
export function registerTransportSender(sender: Sender) {
  activeSender = sender;
}

/** 向中转发送一条消息;未连接时静默丢弃(调用方 UI 已按连接态禁用入口)。 */
export function sendToRelay(msg: MobileToRelay) {
  activeSender?.(msg);
}
