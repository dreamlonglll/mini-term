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

export type RelayToMobile = MobileHelloAck | MobileHelloReject | MobileRevoked;
