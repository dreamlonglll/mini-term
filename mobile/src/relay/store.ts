import { create } from 'zustand';
import type { SavedDesktop } from './auth';
import type { CommandSlot } from './commandSlot';
import type {
  HistoryFailReason,
  MirrorMessage,
  MobileHistorySession,
  MobileLauncher,
  MobilePane,
  MobileProject,
  MobileRejectReason,
  StartSessionFailReason,
  TerminalLine,
} from '../protocol';

export type { CommandReceiptView, CommandUnknownCause } from './commandSlot';

export type MirrorViewMode = 'chat' | 'terminal';

/** 当前活跃 AI pane 的实时终端屏幕快照数据 */
export interface TerminalScreenData {
  cols: number;
  rows: number;
  cursorRow: number;
  cursorCol: number;
  cursorVisible: boolean;
  altScreen: boolean;
  lines: TerminalLine[];
}

export type Phase =
  | 'idle' // 无配对码也无凭证:提示去桌面端扫码
  | 'pairing'
  | 'connecting'
  | 'connected'
  | 'reconnecting'
  | 'revoked' // 配对被顶替/重置
  | 'rejected'; // 握手被拒(带 reason)

/**
 * 对话镜像视图状态(一次只镜像一个 pane)。
 *
 * 指令单槽(pendingCommandId / pendingAnswer / unsettled / receipt)的状态机见 `commandSlot.ts`。
 * answeredProgress(卡片 seq → 已答题数)在回执 ok 即推进:已作答标记要等下一轮镜像轮询才回流,
 * 这 ~1s 里按钮不该复活可再点。这是提交防抖不是乐观渲染——选中项的展示仍以回流的标记为准。
 */
export interface MirrorState extends CommandSlot {
  paneId: string;
  /** 该 pane 的展示名(从列表带入) */
  title: string;
  /** 已加载的消息,按 seq 升序(重订阅快照与已有消息的合并规则见 `mirrorMerge.ts`) */
  messages: MirrorMessage[];
  /** 是否还有更早历史可分页 */
  hasMore: boolean;
  /** 是否已收到首个快照(false = 加载中) */
  loaded: boolean;
  /** 加载更早历史的请求进行中 */
  loadingOlder: boolean;
  /** 目标 pane 已关闭/AI 会话已结束 */
  closed: boolean;
  /** 当前展示的子视图：对话镜像 / 终端画面 */
  viewMode: MirrorViewMode;
  /** 最近一帧终端屏幕快照；null = 尚未收到 */
  terminalScreen: TerminalScreenData | null;
  /** 是否已收到过至少一帧终端屏幕快照 */
  terminalLoaded: boolean;
}

/** 发起/恢复会话的进行态(一次只允许一个,防连点)。 */
export interface StartSessionState {
  requestId: string;
  /** 'start' = 启动器新开;'resume' = 基于历史会话继续对话 */
  mode: 'start' | 'resume';
  /** 目标项目名(提示文案用) */
  projectName: string;
  /** 桌面端回执里的 pane id;null = 回执还没回来 */
  paneId: string | null;
  /** mode === 'resume' 时记录目标历史会话 id,起来后顺手把历史列表里的运行态点亮 */
  resumeSessionId?: string;
}

export type HistoryError = HistoryFailReason | 'timeout';

/** 某项目的历史会话列表页状态 */
export interface HistoryListState {
  projectId: string;
  projectName: string;
  requestId: string | null;
  sessions: MobileHistorySession[];
  loading: boolean;
  error: HistoryError | null;
}

/** 单条历史会话的只读预览页状态 */
export interface HistoryPreviewState {
  projectId: string;
  projectName: string;
  session: MobileHistorySession;
  requestId: string | null;
  /** 本次在途请求是不是「上拉加载更早」 */
  loadingOlder: boolean;
  messages: MirrorMessage[];
  hasMore: boolean;
  loaded: boolean;
  error: HistoryError | null;
}

export type HomeTab = 'active' | 'history';

export interface RelayStore {
  phase: Phase;
  rejectReason: MobileRejectReason | null;
  /** 当前标签页正在连接/查看的桌面端 id;null = 未选中任何桌面端 */
  currentDesktopId: string | null;
  /** 当前桌面端展示名(来自中转或本地缓存;null = 未命名) */
  currentDesktopName: string | null;
  /**
   * 本页面已与哪台桌面端握手成功过、手上的列表 / 镜像等视图数据属于它;null = 还没有。
   * 同一台桌面端断线重连 / 下拉刷新 / 回前台重连期间据此保持主视图挂载(只在顶部挂
   * 重连横幅),不整屏换成「连接中」;首次连接、配对、切换桌面端、凭证失效 / 被吊销时清空,
   * 那些场景照旧走全屏状态页。
   */
  establishedDesktopId: string | null;
  /** 本机已配对保存的全部桌面端(供顶栏多设备切换器展示) */
  savedDesktops: SavedDesktop[];
  /** 桌面端在线状态;null = 尚未收到 presence */
  desktopOnline: boolean | null;
  /** 桌面端是否支持历史会话展示与继续对话(旧桌面端为 false) */
  supportsHistory: boolean;
  /** 桌面端是否支持终端视图与应急按键(旧桌面端 / 旧中转为 false) */
  supportsTerminalView: boolean;
  /** 桌面端是否支持 @ 文件引用与 / 技能补全(旧桌面端 / 旧中转为 false) */
  supportsInputAssist: boolean;
  /** 首页分段标签:活跃会话 / 历史会话 */
  homeTab: HomeTab;
  /** 项目列表(全部项目,含没有活跃会话的;来自桌面端快照/增量) */
  projects: MobileProject[];
  /** 可用的 AI 启动器(只有 id 与展示名) */
  launchers: MobileLauncher[];
  /** 当前打开的对话镜像;null = 不在镜像页 */
  mirror: MirrorState | null;
  /** 当前打开的项目历史列表;null = 不在历史列表页 */
  historyList: HistoryListState | null;
  /** 当前打开的历史会话只读预览;null = 不在预览页 */
  historyPreview: HistoryPreviewState | null;
  /** 发起/恢复会话进行中;null = 空闲 */
  starting: StartSessionState | null;
  /** 最近一次发起/恢复失败的原因;'timeout' = 命令写进去了但会话迟迟没出现 */
  startError: StartSessionFailReason | 'timeout' | null;
}

export const useRelayStore = create<RelayStore>(() => ({
  phase: 'idle',
  rejectReason: null,
  currentDesktopId: null,
  currentDesktopName: null,
  establishedDesktopId: null,
  savedDesktops: [],
  desktopOnline: null,
  supportsHistory: false,
  supportsTerminalView: false,
  supportsInputAssist: false,
  homeTab: 'active',
  projects: [],
  launchers: [],
  mirror: null,
  historyList: null,
  historyPreview: null,
  starting: null,
  startError: null,
}));

/** 切换桌面端或解除绑定时清空上一台电脑的运行态视图数据。 */
export function resetDesktopSessionState() {
  useRelayStore.setState({
    establishedDesktopId: null,
    desktopOnline: null,
    supportsHistory: false,
    supportsTerminalView: false,
    supportsInputAssist: false,
    homeTab: 'active',
    projects: [],
    launchers: [],
    mirror: null,
    historyList: null,
    historyPreview: null,
    starting: null,
    startError: null,
  });
}

export function setPhase(phase: Phase, rejectReason: MobileRejectReason | null = null) {
  useRelayStore.setState({ phase, rejectReason });
}

/**
 * 与中转的链路此刻不通(重连 / 刷新握手中)。主视图在这期间可能仍挂着旧数据
 * (见 `establishedDesktopId`),要发消息的按钮据此置灰:发不出去就别让人点,
 * 更不能让人以为发出去了。
 */
export function selectLinkDown(s: RelayStore): boolean {
  return s.phase !== 'connected';
}

/** 同一台桌面端断线重连 / 刷新期间保留主视图:握手进行中且手上的数据属于当前这台。 */
export function selectKeepMainView(s: RelayStore): boolean {
  return (
    (s.phase === 'connecting' || s.phase === 'reconnecting') &&
    s.establishedDesktopId !== null &&
    s.establishedDesktopId === s.currentDesktopId
  );
}

export function setHomeTab(tab: HomeTab) {
  useRelayStore.setState({ homeTab: tab });
}

/** 清除发起失败提示(UI 展示后调用)。 */
export function clearStartError() {
  if (useRelayStore.getState().startError) {
    useRelayStore.setState({ startError: null });
  }
}

/** 在快照里找某个 pane(用于自动进入镜像 / 深链 / 标题同步)。 */
export function findPane(projects: MobileProject[], paneId: string): MobilePane | undefined {
  for (const project of projects) {
    const pane = project.panes.find((p) => p.paneId === paneId);
    if (pane) return pane;
  }
  return undefined;
}

/** 检查历史条目里记的 livePaneId 此刻是否仍在活跃快照里。 */
export function findLivePaneForHistory(
  projects: MobileProject[],
  livePaneId?: string,
) {
  if (!livePaneId) return undefined;
  return findPane(projects, livePaneId);
}
