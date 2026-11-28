import type {
  MobileHistorySession,
  MobileStartSessionReceipt,
  StartSessionFailReason,
} from '../protocol';
import { clearHistoryPreviewTimer } from './history';
import { openMirror } from './mirror';
import { newRequestId } from './requestId';
import {
  type RelayStore,
  findLivePaneForHistory,
  findPane,
  useRelayStore,
} from './store';
import { sendToRelay } from './transport';

/**
 * 发起会话后等待 pane 出现在活跃列表里的上限。
 *
 * 回执只说"命令已写入",AI 真起来才会出现在快照里;命令不存在、未登录、卡在
 * 交互确认都会让它永远不出现。超时纯粹是手机侧的本地判断——**不触发任何桌面端
 * 动作**:分不清"起不来"和"起得慢"(首次运行、等你按 y 确认信任目录都会慢),
 * 杀掉的破坏性大于留着。
 */
const START_SESSION_TIMEOUT_MS = 15_000;

let startTimer: ReturnType<typeof setTimeout> | null = null;

/** 结束一次发起流程并复位 + 按钮;error=null 表示成功收尾。 */
export function finishStart(error: StartSessionFailReason | 'timeout' | null) {
  if (startTimer) {
    clearTimeout(startTimer);
    startTimer = null;
  }
  useRelayStore.setState({ starting: null, startError: error });
}

/** 开始超时计时;只有仍是同一次请求时才复位,避免误杀后来的请求。 */
function armStartTimeout(requestId: string) {
  if (startTimer) clearTimeout(startTimer);
  startTimer = setTimeout(() => {
    if (useRelayStore.getState().starting?.requestId === requestId) {
      finishStart('timeout');
    }
  }, START_SESSION_TIMEOUT_MS);
}

/**
 * 项目列表变化时检查:等待中的 pane 出现了没有。
 * 出现即视为"AI 真起来了",自动进入它的对话镜像。
 */
export function onProjectsChanged() {
  const { starting, projects, mirror } = useRelayStore.getState();

  // 镜像页的标题是打开时从列表带入的副本:列表里的名字变了(手机改名、桌面端改名)
  // 得跟着走,否则正在看的这个会话还挂着旧名字
  if (mirror) {
    const current = findPane(projects, mirror.paneId);
    if (current && current.title !== mirror.title) {
      useRelayStore.setState({ mirror: { ...mirror, title: current.title } });
    }
  }

  if (!starting?.paneId) return;
  const pane = findPane(projects, starting.paneId);
  if (!pane) return;
  const resumedId = starting.resumeSessionId;
  finishStart(null);
  // 恢复成功后关掉只读预览页,并把历史列表里该会话标为运行中(返回列表立刻看得出)
  const { historyPreview, historyList } = useRelayStore.getState();
  const patch: Partial<RelayStore> = {};
  if (historyPreview) {
    clearHistoryPreviewTimer();
    patch.historyPreview = null;
  }
  if (resumedId && historyList) {
    patch.historyList = {
      ...historyList,
      sessions: historyList.sessions.map((s) =>
        s.sessionId === resumedId ? { ...s, livePaneId: pane.paneId } : s,
      ),
    };
  }
  if (Object.keys(patch).length > 0) useRelayStore.setState(patch);
  openMirror(pane.paneId, pane.title);
}

export function handleStartSessionReceipt(msg: MobileStartSessionReceipt) {
  const { starting } = useRelayStore.getState();
  // 陈旧回执(上一次已超时复位/被取消):丢弃,别让它复活一个不存在的等待
  if (!starting || starting.requestId !== msg.requestId) return;
  if (!msg.ok) {
    finishStart(msg.reason ?? 'spawnFailed');
    return;
  }
  // 命令已写入:接着等这个 pane 出现在活跃列表里,那才是真的起来了。
  // 计时窗口不重置——从用户点下那一刻起总共 15 秒,回执正常是毫秒级返回的,
  // 重新计时只会在桌面端卡住时把等待翻倍。
  useRelayStore.setState({
    starting: { ...starting, paneId: msg.paneId ?? null },
  });
  onProjectsChanged();
}

/**
 * 发起一次新 AI 会话。返回 false = 当前不可发起(未连接 / 桌面离线 / 已有在途请求)。
 * 发起后进入"启动中",+ 按钮由 UI 按 `starting` 禁用,连点不会开出一串会话。
 */
export function startAiSession(
  projectId: string,
  projectName: string,
  launcherId: string,
): boolean {
  const { phase, desktopOnline, starting } = useRelayStore.getState();
  if (starting) return false;
  if (phase !== 'connected' || desktopOnline === false) return false;

  const requestId = newRequestId();
  useRelayStore.setState({
    starting: { requestId, mode: 'start', projectName, paneId: null },
    startError: null,
  });
  sendToRelay({ type: 'startAiSession', requestId, projectId, launcherId });
  // 回执迟迟不回(中转/桌面端半死)也要有出路,不能让 + 按钮永远禁用
  armStartTimeout(requestId);
  return true;
}

/**
 * 基于历史会话发起继续对话:
 * - 若该会话此刻正跑在某个活跃 AI pane 里,直接打开那个 pane 的实时镜像;
 * - 否则向桌面端发 `resumeAiSession`,复用 `starting` 进行态与 15 秒超时,
 *   pane 起来出现在快照里即自动进入实时对话镜像。
 */
export function resumeAiSession(
  projectId: string,
  projectName: string,
  session: MobileHistorySession,
): boolean {
  const { phase, desktopOnline, starting, projects } = useRelayStore.getState();
  const live = findLivePaneForHistory(projects, session.livePaneId);
  if (live) {
    clearHistoryPreviewTimer();
    useRelayStore.setState({ historyPreview: null });
    openMirror(live.paneId, live.title);
    return true;
  }
  if (!session.canResume) return false;
  if (starting) return false;
  if (phase !== 'connected' || desktopOnline === false) return false;

  const requestId = newRequestId();
  useRelayStore.setState({
    starting: {
      requestId,
      mode: 'resume',
      projectName,
      paneId: null,
      resumeSessionId: session.sessionId,
    },
    startError: null,
  });
  sendToRelay({
    type: 'resumeAiSession',
    requestId,
    projectId,
    agent: session.agent,
    sessionId: session.sessionId,
  });
  armStartTimeout(requestId);
  return true;
}
