/**
 * 移动端 → 中转的连接管理与状态入口。
 *
 * 拆分结构:
 * - `store.ts`: Zustand 状态树、导出类型与查询辅助
 * - `auth.ts`: localStorage 凭证、URL hash 配对码/深链解析与退避计算
 * - `transport.ts`: 发送通道注册(避免子模块循环依赖 connection)
 * - `mirror.ts`: 对话镜像订阅、指令写穿、提问作答与分页加载
 * - `history.ts`: 历史会话列表与只读正文预览(含超时与断线重连补发)
 * - `session.ts`: 发起新会话 / 恢复历史会话及自动跳转镜像
 * - `connection.ts`: WebSocket 握手、消息路由、重连、前后台与网络监听
 */
export { type SavedDesktop } from './auth';
export {
  type CommandReceiptView,
  type CommandUnknownCause,
  type HistoryError,
  type HistoryListState,
  type HistoryPreviewState,
  type HomeTab,
  type MirrorState,
  type MirrorViewMode,
  type Phase,
  type StartSessionState,
  type TerminalScreenData,
  clearStartError,
  findLivePaneForHistory,
  selectKeepMainView,
  selectLinkDown,
  setHomeTab,
  useRelayStore,
} from './store';
export { sendToRelay } from './transport';
export {
  answerQuestion,
  clearCommandReceipt,
  closeMirror,
  loadOlderMirror,
  openMirror,
  renamePane,
  sendMobileCommand,
  sendTerminalKey,
  setMirrorViewMode,
} from './mirror';
export {
  type FilesAssistState,
  type SkillsAssistState,
  requestProjectFiles,
  requestProjectSkills,
  resetAssistState,
  useAssistStore,
} from './assist';
export {
  closeHistoryPreview,
  closeProjectHistory,
  loadOlderHistoryPreview,
  openHistoryPreview,
  openProjectHistory,
  refreshProjectHistory,
} from './history';
export { resumeAiSession, startAiSession } from './session';
export {
  refreshSessions,
  removeSavedDesktop,
  retryConnectionNow,
  startRelay,
  switchDesktop,
} from './connection';
