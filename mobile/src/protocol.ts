/**
 * 中转协议 v2 的 TypeScript 手写镜像。
 * 与 relay-server/protocol/src/lib.rs 对齐(serde tag="type" + camelCase 字段);
 * 两侧字段增删必须同步维护。
 */

export const PROTOCOL_VERSION = 2;

// ── 移动端 → 中转 ──

export interface MobileHello {
  type: 'hello';
  protocolVersion: number;
  /** 扫码首连:一次性配对码 */
  pairingCode?: string;
  /** 重连:长期凭证 */
  credential?: string;
}

/** 订阅某 pane 的对话镜像(进入镜像页) */
export interface MobileSubscribePane {
  type: 'subscribePane';
  paneId: string;
}

/** 退订(返回列表) */
export interface MobileUnsubscribePane {
  type: 'unsubscribePane';
  paneId: string;
}

/** 上拉加载更早的镜像历史 */
export interface MobileRequestMirrorHistory {
  type: 'requestMirrorHistory';
  paneId: string;
  beforeSeq: number;
}

/** 移动端指令:写穿目标 pane 的 PTY(等价桌面敲入并回车),不排队 */
export interface MobileCommandMsg {
  type: 'mobileCommand';
  paneId: string;
  commandId: string;
  text: string;
}

/**
 * 重命名会话:改桌面端那个 pane 的自定义标题。
 * 无回执——改没改成看结构增量把新 title 推没推回来。空 title = 清除自定义名。
 */
export interface MobileRenamePane {
  type: 'renamePane';
  paneId: string;
  title: string;
}

/** 发起新 AI 会话:按桌面端配置的具名启动器,在某项目新开一个 tab */
export interface MobileStartAiSession {
  type: 'startAiSession';
  requestId: string;
  projectId: string;
  launcherId: string;
}

export type MobileToRelay =
  | MobileHello
  | MobileSubscribePane
  | MobileUnsubscribePane
  | MobileRequestMirrorHistory
  | MobileCommandMsg
  | MobileRenamePane
  | MobileStartAiSession;

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
  /** 处于 AI 会话中的 pane;v2 起没有活跃会话的项目也进快照,此时为空数组 */
  panes: MobilePane[];
  /** 能否在此项目发起新会话(桌面端判定:SSH 远程 / WSL 根项目为 false) */
  canStartSession: boolean;
  /**
   * 该项目在桌面端项目树里的祖先分组名链(根→父),顶层项目为空/缺省。
   * 快照里的项目已按桌面端树序排列,顺序渲染 + 这条链即可还原分组层级。
   * 旧桌面端不发、旧中转会把它吃掉 —— 两种情况都缺省,列表退化为平铺。
   */
  groupPath?: string[];
}

/** 可用的 AI 启动器:只有 id 与展示名,命令与 shell 永远留在桌面端 */
export interface MobileLauncher {
  id: string;
  name: string;
}

/** 桌面端在线状态(握手成功后立即推一次,此后变化时推送) */
export interface MobilePresence {
  type: 'presence';
  desktopOnline: boolean;
}

export interface MobileSessionsSnapshot {
  type: 'sessionsSnapshot';
  projects: MobileProject[];
  launchers: MobileLauncher[];
}

export interface MobileSessionsDelta {
  type: 'sessionsDelta';
  upserts: MobileProject[];
  removedProjectIds: string[];
}

// ── 对话镜像 ──

/** 镜像中的一条消息。seq 在一次绑定内从 0 连续递增,分页以此为锚 */
export interface MirrorMessage {
  seq: number;
  /** "desktop"(桌面输入)| "assistant"(AI 回复)| "mobile"(移动端指令) */
  source: string;
  content: string;
  timestamp: string;
}

export interface MobileMirrorSnapshot {
  type: 'mirrorSnapshot';
  paneId: string;
  messages: MirrorMessage[];
  hasMore: boolean;
}

export interface MobileMirrorAppend {
  type: 'mirrorAppend';
  paneId: string;
  messages: MirrorMessage[];
}

export interface MobileMirrorHistory {
  type: 'mirrorHistory';
  paneId: string;
  messages: MirrorMessage[];
  hasMore: boolean;
}

/** 被订阅的 pane 已关闭/AI 会话结束 */
export interface MobilePaneClosed {
  type: 'paneClosed';
  paneId: string;
}

export type CommandFailReason = 'desktopOffline' | 'paneNotFound' | 'writeFailed';

/** 指令回执:ok = 已写入桌面终端(AI 真正接收以镜像回流为准) */
export interface MobileCommandReceipt {
  type: 'commandReceipt';
  paneId: string;
  commandId: string;
  ok: boolean;
  reason?: CommandFailReason;
}

export type StartSessionFailReason =
  | 'desktopOffline'
  | 'projectNotFound'
  | 'launcherNotFound'
  | 'notSupported'
  | 'spawnFailed';

/** 发起回执:ok = pane 已建、启动命令已写入(AI 是否真起来以会话出现在列表为准) */
export interface MobileStartSessionReceipt {
  type: 'startSessionReceipt';
  requestId: string;
  ok: boolean;
  paneId?: string;
  reason?: StartSessionFailReason;
}

export type RelayToMobile =
  | MobileHelloAck
  | MobileHelloReject
  | MobileRevoked
  | MobilePresence
  | MobileSessionsSnapshot
  | MobileSessionsDelta
  | MobileMirrorSnapshot
  | MobileMirrorAppend
  | MobileMirrorHistory
  | MobilePaneClosed
  | MobileCommandReceipt
  | MobileStartSessionReceipt;
