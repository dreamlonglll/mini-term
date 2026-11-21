import { create } from 'zustand';
import type {
  CommandFailReason,
  HistoryFailReason,
  MirrorMessage,
  MobileHistorySession,
  MobileLauncher,
  MobilePane,
  MobileProject,
  MobileRejectReason,
  StartSessionFailReason,
} from '../protocol';

export type Phase =
  | 'idle' // 无配对码也无凭证:提示去桌面端扫码
  | 'pairing'
  | 'connecting'
  | 'connected'
  | 'reconnecting'
  | 'revoked' // 配对被顶替/重置
  | 'rejected'; // 握手被拒(带 reason)

/** 对话镜像视图状态(一次只镜像一个 pane)。 */
export interface MirrorState {
  paneId: string;
  /** 该 pane 的展示名(从列表带入) */
  title: string;
  /** 已加载的消息,按 seq 升序 */
  messages: MirrorMessage[];
  /** 是否还有更早历史可分页 */
  hasMore: boolean;
  /** 是否已收到首个快照(false = 加载中) */
  loaded: boolean;
  /** 加载更早历史的请求进行中 */
  loadingOlder: boolean;
  /** 目标 pane 已关闭/AI 会话已结束 */
  closed: boolean;
  /** 等待回执中的指令 id;null = 没有在途指令 */
  pendingCommandId: string | null;
  /** 在途的点选作答(commandId 与 pendingCommandId 同值,额外记住卡片 seq) */
  pendingAnswer: { commandId: string; seq: number } | null;
  /**
   * 各提问卡片已成功注入的作答数(卡片 seq → 已答题数)。回执 ok 即推进:
   * 已作答标记要等下一轮镜像轮询才回流,这 ~1s 里按钮不该复活可再点。
   * 这是提交防抖不是乐观渲染——选中项的展示仍以回流的标记为准。
   */
  answeredProgress: Record<number, number>;
  /** 最近一次指令回执(短暂展示后由 UI 清除) */
  receipt: { ok: boolean; reason?: CommandFailReason } | null;
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
  /** 桌面端在线状态;null = 尚未收到 presence */
  desktopOnline: boolean | null;
  /** 桌面端是否支持历史会话展示与继续对话(旧桌面端为 false) */
  supportsHistory: boolean;
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
  desktopOnline: null,
  supportsHistory: false,
  homeTab: 'active',
  projects: [],
  launchers: [],
  mirror: null,
  historyList: null,
  historyPreview: null,
  starting: null,
  startError: null,
}));

export function setPhase(phase: Phase, rejectReason: MobileRejectReason | null = null) {
  useRelayStore.setState({ phase, rejectReason });
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
