import type {
  MobileHistorySession,
  MobileSessionContent,
  MobileSessionHistory,
} from '../protocol';
import { useRelayStore } from './store';
import { sendToRelay } from './transport';

const HISTORY_REQUEST_TIMEOUT_MS = 15_000;

let historyListTimer: ReturnType<typeof setTimeout> | null = null;
let historyPreviewTimer: ReturnType<typeof setTimeout> | null = null;

export function clearHistoryListTimer() {
  if (historyListTimer) {
    clearTimeout(historyListTimer);
    historyListTimer = null;
  }
}

export function clearHistoryPreviewTimer() {
  if (historyPreviewTimer) {
    clearTimeout(historyPreviewTimer);
    historyPreviewTimer = null;
  }
}

function armHistoryListTimeout(requestId: string) {
  historyListTimer = setTimeout(() => {
    const cur = useRelayStore.getState().historyList;
    if (cur && cur.requestId === requestId) {
      useRelayStore.setState({
        historyList: { ...cur, requestId: null, loading: false, error: 'timeout' },
      });
    }
  }, HISTORY_REQUEST_TIMEOUT_MS);
}

function armHistoryPreviewTimeout(requestId: string) {
  historyPreviewTimer = setTimeout(() => {
    const cur = useRelayStore.getState().historyPreview;
    if (cur && cur.requestId === requestId) {
      useRelayStore.setState({
        historyPreview: { ...cur, requestId: null, loadingOlder: false, error: 'timeout' },
      });
    }
  }, HISTORY_REQUEST_TIMEOUT_MS);
}

/** 桌面端掉线时立刻结束在途的历史列表 / 预览请求,不等满 15s 超时。 */
export function failPendingHistoryOffline() {
  const st = useRelayStore.getState();
  if (st.historyList?.loading) {
    clearHistoryListTimer();
    useRelayStore.setState({
      historyList: {
        ...st.historyList,
        requestId: null,
        loading: false,
        error: 'desktopOffline',
      },
    });
  }
  if (st.historyPreview && st.historyPreview.requestId !== null) {
    clearHistoryPreviewTimer();
    useRelayStore.setState({
      historyPreview: {
        ...st.historyPreview,
        requestId: null,
        loadingOlder: false,
        error: 'desktopOffline',
      },
    });
  }
}

/** 断线重连收到快照后,若历史列表 / 预览正卡在加载中,立刻补发一次。 */
export function resyncPendingHistory(supportsHistory: boolean) {
  if (!supportsHistory) return;
  const { historyList, historyPreview } = useRelayStore.getState();
  if (historyList?.loading && historyList.requestId) {
    sendToRelay({
      type: 'requestSessionHistory',
      requestId: historyList.requestId,
      projectId: historyList.projectId,
    });
  }
  if (historyPreview?.requestId) {
    sendToRelay({
      type: 'requestSessionContent',
      requestId: historyPreview.requestId,
      projectId: historyPreview.projectId,
      agent: historyPreview.session.agent,
      sessionId: historyPreview.session.sessionId,
      ...(historyPreview.loadingOlder && historyPreview.messages.length > 0
        ? { beforeSeq: historyPreview.messages[0].seq }
        : {}),
    });
  }
}

/** 进入某项目的历史会话列表并向桌面端拉取最新列表。 */
export function openProjectHistory(projectId: string, projectName: string) {
  const { phase, desktopOnline } = useRelayStore.getState();
  clearHistoryListTimer();
  clearHistoryPreviewTimer();
  if (phase !== 'connected' || desktopOnline === false) {
    useRelayStore.setState({
      historyPreview: null,
      historyList: {
        projectId,
        projectName,
        requestId: null,
        sessions: [],
        loading: false,
        error: 'desktopOffline',
      },
    });
    return;
  }
  const requestId = crypto.randomUUID();
  useRelayStore.setState({
    historyPreview: null,
    historyList: {
      projectId,
      projectName,
      requestId,
      sessions: [],
      loading: true,
      error: null,
    },
  });
  sendToRelay({ type: 'requestSessionHistory', requestId, projectId });
  armHistoryListTimeout(requestId);
}

/** 重新拉取当前打开项目的历史会话列表。 */
export function refreshProjectHistory() {
  const cur = useRelayStore.getState().historyList;
  if (!cur) return;
  const { phase, desktopOnline } = useRelayStore.getState();
  clearHistoryListTimer();
  if (phase !== 'connected' || desktopOnline === false) {
    useRelayStore.setState({
      historyList: { ...cur, requestId: null, loading: false, error: 'desktopOffline' },
    });
    return;
  }
  const requestId = crypto.randomUUID();
  useRelayStore.setState({
    historyList: { ...cur, requestId, loading: true, error: null },
  });
  sendToRelay({ type: 'requestSessionHistory', requestId, projectId: cur.projectId });
  armHistoryListTimeout(requestId);
}

/** 退出项目历史列表,回到首页。 */
export function closeProjectHistory() {
  clearHistoryListTimer();
  clearHistoryPreviewTimer();
  useRelayStore.setState({ historyList: null, historyPreview: null });
}

/** 打开一条历史会话的只读预览页并请求最近一页正文。 */
export function openHistoryPreview(
  projectId: string,
  projectName: string,
  session: MobileHistorySession,
) {
  const { phase, desktopOnline } = useRelayStore.getState();
  clearHistoryPreviewTimer();
  if (phase !== 'connected' || desktopOnline === false) {
    useRelayStore.setState({
      historyPreview: {
        projectId,
        projectName,
        session,
        requestId: null,
        loadingOlder: false,
        messages: [],
        hasMore: false,
        loaded: false,
        error: 'desktopOffline',
      },
    });
    return;
  }
  const requestId = crypto.randomUUID();
  useRelayStore.setState({
    historyPreview: {
      projectId,
      projectName,
      session,
      requestId,
      loadingOlder: false,
      messages: [],
      hasMore: false,
      loaded: false,
      error: null,
    },
  });
  sendToRelay({
    type: 'requestSessionContent',
    requestId,
    projectId,
    agent: session.agent,
    sessionId: session.sessionId,
  });
  armHistoryPreviewTimeout(requestId);
}

/** 历史预览页上拉加载更早消息。 */
export function loadOlderHistoryPreview() {
  const { historyPreview, phase, desktopOnline } = useRelayStore.getState();
  if (
    !historyPreview ||
    !historyPreview.hasMore ||
    historyPreview.requestId !== null ||
    historyPreview.messages.length === 0 ||
    phase !== 'connected' ||
    desktopOnline === false
  ) {
    return;
  }
  const requestId = crypto.randomUUID();
  clearHistoryPreviewTimer();
  useRelayStore.setState({
    historyPreview: { ...historyPreview, requestId, loadingOlder: true, error: null },
  });
  sendToRelay({
    type: 'requestSessionContent',
    requestId,
    projectId: historyPreview.projectId,
    agent: historyPreview.session.agent,
    sessionId: historyPreview.session.sessionId,
    beforeSeq: historyPreview.messages[0].seq,
  });
  armHistoryPreviewTimeout(requestId);
}

/** 退出历史会话预览页,返回历史列表。 */
export function closeHistoryPreview() {
  clearHistoryPreviewTimer();
  useRelayStore.setState({ historyPreview: null });
}

/** 处理历史会话列表与正文响应。 */
export function handleHistoryMessage(msg: MobileSessionHistory | MobileSessionContent) {
  if (msg.type === 'sessionHistory') {
    const { historyList } = useRelayStore.getState();
    if (!historyList || historyList.requestId !== msg.requestId) return;
    clearHistoryListTimer();
    useRelayStore.setState({
      historyList: {
        ...historyList,
        requestId: null,
        loading: false,
        sessions: msg.ok ? msg.sessions : historyList.sessions,
        error: msg.ok ? null : (msg.reason ?? 'readFailed'),
      },
    });
    return;
  }

  const { historyPreview } = useRelayStore.getState();
  if (!historyPreview || historyPreview.requestId !== msg.requestId) return;
  clearHistoryPreviewTimer();
  if (!msg.ok) {
    useRelayStore.setState({
      historyPreview: {
        ...historyPreview,
        requestId: null,
        loadingOlder: false,
        error: msg.reason ?? 'readFailed',
      },
    });
    return;
  }
  const messages = historyPreview.loadingOlder
    ? [...msg.messages, ...historyPreview.messages]
    : msg.messages;
  useRelayStore.setState({
    historyPreview: {
      ...historyPreview,
      requestId: null,
      loadingOlder: false,
      loaded: true,
      messages,
      hasMore: msg.hasMore,
      error: null,
    },
  });
}
