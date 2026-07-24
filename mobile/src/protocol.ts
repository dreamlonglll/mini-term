/**
 * 中转协议 v1 的 TypeScript 手写镜像。
 * 与 relay-server/protocol/src/lib.rs 对齐(serde tag="type" + camelCase 字段);
 * 两侧字段增删必须同步维护。
 */

export const PROTOCOL_VERSION = 1;

// ── 移动端 → 中转 ──

export interface MobileHello {
  type: 'hello';
  protocolVersion: number;
  /** 扫码首连:一次性配对码 */
  pairingCode?: string;
  /** 重连:长期凭证 */
  credential?: string;
}

export type MobileToRelay = MobileHello;

// ── 中转 → 移动端 ──

export type MobileRejectReason =
  | 'versionMismatch'
  | 'invalidPairingCode'
  | 'invalidCredential'
  | 'missingAuth';

export interface MobileHelloAck {
  type: 'helloAck';
  protocolVersion: number;
  /** 配对兑换成功时携带新签发的长期凭证;凭证重连时缺省 */
  credential?: string;
}

export interface MobileHelloReject {
  type: 'helloReject';
  reason: MobileRejectReason;
}

/** 已建立的连接被吊销(新设备顶替/桌面端重置),应清除本地凭证并提示重新扫码 */
export interface MobileRevoked {
  type: 'revoked';
}

// ── 活跃 AI 会话结构 ──

export interface MobilePane {
  paneId: string;
  title: string;
  /** 与桌面端 PaneStatus 一致:"ai-working" | "ai-idle" | "error" */
  status: string;
}

export interface MobileProject {
  projectId: string;
  name: string;
  panes: MobilePane[];
}

/** 桌面端在线状态(握手成功后立即推一次,此后变化时推送) */
export interface MobilePresence {
  type: 'presence';
  desktopOnline: boolean;
}

export interface MobileSessionsSnapshot {
  type: 'sessionsSnapshot';
  projects: MobileProject[];
}

export interface MobileSessionsDelta {
  type: 'sessionsDelta';
  upserts: MobileProject[];
  removedProjectIds: string[];
}

export type RelayToMobile =
  | MobileHelloAck
  | MobileHelloReject
  | MobileRevoked
  | MobilePresence
  | MobileSessionsSnapshot
  | MobileSessionsDelta;
