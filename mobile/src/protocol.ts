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

/** 请求某项目的本机历史会话列表 */
export interface MobileRequestSessionHistory {
  type: 'requestSessionHistory';
  requestId: string;
  projectId: string;
}

/** 请求某条历史会话的只读正文(beforeSeq 缺省取最近一页,带值取更早一页) */
export interface MobileRequestSessionContent {
  type: 'requestSessionContent';
  requestId: string;
  projectId: string;
  agent: string;
  sessionId: string;
  beforeSeq?: number;
}

/** 基于历史会话发起继续对话(回执复用 startSessionReceipt) */
export interface MobileResumeAiSession {
  type: 'resumeAiSession';
  requestId: string;
  projectId: string;
  agent: string;
  sessionId: string;
}

/**
 * 点选作答 agent 的提问:按镜像消息 seq + 提问身份(questionId)定位提问卡片,
 * 按题序+选项下标选择。桌面端校验该提问仍挂起后向 PTY 注入按键;回执复用
 * commandReceipt,提问已不挂起时 reason = questionNotPending。
 */
export interface MobileAnswerQuestion {
  type: 'answerQuestion';
  paneId: string;
  commandId: string;
  seq: number;
  /** 提问卡片的 questionId:seq 在镜像换绑后会重排,靠它对账 */
  questionId: string;
  questionIndex: number;
  optionIndex: number;
}

/** 移动端可在终端视图下发送的应急特殊按键白名单 */
export type TerminalKey =
  | 'escape'
  | 'ctrlC'
  | 'ctrlD'
  | 'ctrlU'
  | 'tab'
  | 'shiftTab'
  | 'enter'
  | 'backspace'
  | 'space'
  | 'up'
  | 'down'
  | 'left'
  | 'right'
  | 'home'
  | 'end'
  | 'pageUp'
  | 'pageDown';

/** 切入终端视图：按需订阅活跃 AI pane 的实时终端屏幕快照 */
export interface MobileSubscribeTerminal {
  type: 'subscribeTerminal';
  paneId: string;
}

/** 退出终端视图：退订终端屏幕快照流 */
export interface MobileUnsubscribeTerminal {
  type: 'unsubscribeTerminal';
  paneId: string;
}

/** 在终端视图下发送应急特殊按键（回执复用 commandReceipt） */
export interface MobileSendTerminalKey {
  type: 'sendTerminalKey';
  paneId: string;
  commandId: string;
  key: TerminalKey;
}

/** 输入框 `@` 文件引用补全请求 */
export interface MobileRequestProjectFiles {
  type: 'requestProjectFiles';
  requestId: string;
  paneId: string;
  query: string;
}

/** 输入框 `/` 技能补全请求 */
export interface MobileRequestProjectSkills {
  type: 'requestProjectSkills';
  requestId: string;
  paneId: string;
}

export type MobileToRelay =
  | MobileHello
  | MobileSubscribePane
  | MobileUnsubscribePane
  | MobileRequestMirrorHistory
  | MobileCommandMsg
  | MobileAnswerQuestion
  | MobileRenamePane
  | MobileStartAiSession
  | MobileRequestSessionHistory
  | MobileRequestSessionContent
  | MobileResumeAiSession
  | MobileSubscribeTerminal
  | MobileUnsubscribeTerminal
  | MobileSendTerminalKey
  | MobileRequestProjectFiles
  | MobileRequestProjectSkills;

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
  /** 当前连接绑定的桌面端标识(多桌面端中转总是下发;旧中转缺省) */
  desktopId?: string;
  /** 桌面端展示名(桌面端握手时上报;缺省时 UI 回落默认名称) */
  desktopName?: string;
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
  /**
   * 有事等用户处理(agent 提问待答/等待授权批准),桌面端黄灯的投影;
   * 与 status 正交。旧桌面端不发、旧中转会把它吃掉——缺省按 false 处理。
   */
  needsAttention?: boolean;
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

/** 桌面端在线状态(握手成功后立即推一次,此后变化或桌面端改名重连时推送) */
export interface MobilePresence {
  type: 'presence';
  desktopOnline: boolean;
  /** 桌面端最新展示名(改名重连时更新) */
  desktopName?: string;
}

export interface MobileSessionsSnapshot {
  type: 'sessionsSnapshot';
  projects: MobileProject[];
  launchers: MobileLauncher[];
  /** 桌面端是否支持「历史会话展示与继续对话」(旧桌面端缺省 false) */
  supportsHistory?: boolean;
  /** 桌面端是否支持「终端视图与应急按键」(旧桌面端 / 旧中转缺省 false) */
  supportsTerminalView?: boolean;
  /** 桌面端是否支持「@ 文件引用与 / 技能补全」(旧桌面端 / 旧中转缺省 false) */
  supportsInputAssist?: boolean;
}

export interface MobileSessionsDelta {
  type: 'sessionsDelta';
  upserts: MobileProject[];
  removedProjectIds: string[];
}

// ── 对话镜像 ──

/** agent 提问的一个选项 */
export interface MirrorQuestionOption {
  label: string;
  description?: string;
}

/** agent 提问的一道题(一次提问可含多题,TUI 逐题推进) */
export interface MirrorQuestionItem {
  question: string;
  /** 短标签(如「作答方式」),可为空 */
  header?: string;
  options: MirrorQuestionOption[];
  /** 多选题:v1 只展示不可点选 */
  multiSelect?: boolean;
}

/** 镜像中的一条消息。seq 在一次绑定内从 0 连续递增,分页以此为锚 */
export interface MirrorMessage {
  seq: number;
  /** "desktop"(桌面输入)| "assistant"(AI 回复)| "mobile"(移动端指令) */
  source: string;
  content: string;
  timestamp: string;
  /**
   * 消息种类:缺省 = 普通文本;"question" = agent 提问卡片(questions/questionId
   * 随行);"questionAnswered" = 已作答标记(refSeq 指向提问消息,labels 为逐题
   * 选中项,为空 = 打断/旧版记录给不出选中项;content 只是纯文本兜底);
   * "tool" = agent 的一次工具调用(source 为 "assistant",toolName/toolTarget
   * 随行,content 是兜底文本 "{toolName} {toolTarget}";提问类工具不在此列,
   * 仍走 question 卡片)。
   * 旧桌面端不发、旧中转会把这些字段吃掉——缺省一律按普通文本渲染 content。
   */
  kind?: string;
  questions?: MirrorQuestionItem[];
  /** kind=question 时该次提问的稳定身份,作答请求带回它对账 */
  questionId?: string;
  refSeq?: number;
  labels?: string[];
  /** kind="tool" 时的工具名原文(Bash / Edit / shell / apply_patch …) */
  toolName?: string;
  /** kind="tool" 时的一行目标摘要(文件路径 / 命令首行 / 搜索词…),可缺省 */
  toolTarget?: string;
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

export type CommandFailReason =
  | 'desktopOffline'
  | 'paneNotFound'
  | 'writeFailed'
  | 'questionNotPending';

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
  | 'spawnFailed'
  | 'sessionNotFound'
  | 'resumeUnsupported';

/** 发起/恢复回执:ok = pane 已建且命令已写入(AI 是否真起来以会话出现在列表为准) */
export interface MobileStartSessionReceipt {
  type: 'startSessionReceipt';
  requestId: string;
  ok: boolean;
  paneId?: string;
  reason?: StartSessionFailReason;
}

/** 移动端可见的一条历史会话摘要(仅本机宿主来源) */
export interface MobileHistorySession {
  sessionId: string;
  agent: string;
  agentLabel: string;
  title: string;
  timestamp: string;
  canResume: boolean;
  /** 若该会话此刻正跑在桌面端某个活跃 AI pane 里,携带其 paneId */
  livePaneId?: string;
}

export type HistoryFailReason =
  | 'desktopOffline'
  | 'projectNotFound'
  | 'notSupported'
  | 'sessionNotFound'
  | 'unsupportedAgent'
  | 'readFailed';

export interface MobileSessionHistory {
  type: 'sessionHistory';
  requestId: string;
  projectId: string;
  ok: boolean;
  sessions: MobileHistorySession[];
  reason?: HistoryFailReason;
}

export interface MobileSessionContent {
  type: 'sessionContent';
  requestId: string;
  projectId: string;
  sessionId: string;
  ok: boolean;
  messages: MirrorMessage[];
  hasMore: boolean;
  reason?: HistoryFailReason;
}

// ── 终端视图与输入辅助 ──

export type AssistFailReason =
  | 'desktopOffline'
  | 'paneNotFound'
  | 'notSupported'
  | 'readFailed';

export interface TerminalSpan {
  text: string;
  /** `"ansi:0"`..`"ansi:15"` 或 `"#rrggbb"`；省略表示默认前景色 */
  fg?: string;
  /** `"ansi:0"`..`"ansi:15"` 或 `"#rrggbb"`；省略表示默认背景色 */
  bg?: string;
  bold?: boolean;
  dim?: boolean;
  inverse?: boolean;
}

export interface TerminalLine {
  spans?: TerminalSpan[];
  /** 本行是否为上一行的软换行续行（文本重排模式据此合并自然段） */
  wrapped?: boolean;
}

export interface MobileTerminalScreen {
  type: 'terminalScreen';
  paneId: string;
  cols: number;
  rows: number;
  cursorRow: number;
  cursorCol: number;
  cursorVisible: boolean;
  altScreen: boolean;
  lines?: TerminalLine[];
}

export interface MobileProjectFile {
  path: string;
}

export interface MobileProjectFiles {
  type: 'projectFiles';
  requestId: string;
  paneId: string;
  ok: boolean;
  files?: MobileProjectFile[];
  truncated?: boolean;
  reason?: AssistFailReason;
}

export interface MobileSkillItem {
  name: string;
  description?: string;
  source?: string;
}

export interface MobileProjectSkills {
  type: 'projectSkills';
  requestId: string;
  paneId: string;
  ok: boolean;
  prefix?: string;
  skills?: MobileSkillItem[];
  reason?: AssistFailReason;
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
  | MobileStartSessionReceipt
  | MobileSessionHistory
  | MobileSessionContent
  | MobileTerminalScreen
  | MobileProjectFiles
  | MobileProjectSkills;
