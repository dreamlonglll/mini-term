/**
 * 输入辅助（`@` 文件引用补全、`/` 技能补全）的独立状态管理。
 *
 * 刻意放在独立 store，避免高频输入补全刷新带动整个对话列表重渲染。
 */
import { create } from 'zustand';
import type {
  AssistFailReason,
  MobileProjectFile,
  MobileProjectFiles,
  MobileProjectSkills,
  MobileSkillItem,
} from '../protocol';
import { newRequestId } from './requestId';
import { useRelayStore } from './store';
import { sendToRelay } from './transport';

export interface FilesAssistState {
  paneId: string;
  query: string;
  requestId: string | null;
  loading: boolean;
  files: MobileProjectFile[];
  truncated: boolean;
  error: AssistFailReason | null;
}

export interface SkillsAssistState {
  paneId: string;
  requestId: string | null;
  loading: boolean;
  loaded: boolean;
  prefix: string;
  skills: MobileSkillItem[];
  error: AssistFailReason | null;
}

interface AssistStore {
  files: FilesAssistState | null;
  skills: SkillsAssistState | null;
}

export const useAssistStore = create<AssistStore>(() => ({
  files: null,
  skills: null,
}));

export type AssistIncomingMessage = MobileProjectFiles | MobileProjectSkills;

export function resetAssistState() {
  useAssistStore.setState({ files: null, skills: null });
}

/** 桌面端离线时立即收敛在途的补全请求，避免浮层永远转圈。 */
export function failPendingAssistOffline() {
  const { files, skills } = useAssistStore.getState();
  const patch: Partial<AssistStore> = {};
  if (files?.loading) {
    patch.files = {
      ...files,
      requestId: null,
      loading: false,
      error: 'desktopOffline',
    };
  }
  if (skills?.loading) {
    patch.skills = {
      ...skills,
      requestId: null,
      loading: false,
      error: 'desktopOffline',
    };
  }
  if (patch.files || patch.skills) {
    useAssistStore.setState(patch);
  }
}

/** 请求指定活跃 AI pane 所属项目的文件候选路径。 */
export function requestProjectFiles(paneId: string, query: string) {
  const { phase, desktopOnline, supportsInputAssist } = useRelayStore.getState();
  if (!supportsInputAssist) return;
  if (phase !== 'connected' || desktopOnline === false) {
    useAssistStore.setState({
      files: {
        paneId,
        query,
        requestId: null,
        loading: false,
        files: [],
        truncated: false,
        error: 'desktopOffline',
      },
    });
    return;
  }

  const prev = useAssistStore.getState().files;
  const requestId = newRequestId();
  useAssistStore.setState({
    files: {
      paneId,
      query,
      requestId,
      loading: true,
      // 同一 pane 连续打字时保留上一份列表，减少闪烁
      files: prev?.paneId === paneId ? prev.files : [],
      truncated: prev?.paneId === paneId ? prev.truncated : false,
      error: null,
    },
  });
  sendToRelay({ type: 'requestProjectFiles', requestId, paneId, query });
}

/** 请求指定活跃 AI pane 的可用技能列表（同 pane 缓存复用，force=true 时强制重拉）。 */
export function requestProjectSkills(paneId: string, force = false) {
  const { phase, desktopOnline, supportsInputAssist } = useRelayStore.getState();
  if (!supportsInputAssist) return;

  const prev = useAssistStore.getState().skills;
  if (!force && prev && prev.paneId === paneId && (prev.loaded || prev.loading)) {
    return;
  }

  if (phase !== 'connected' || desktopOnline === false) {
    useAssistStore.setState({
      skills: {
        paneId,
        requestId: null,
        loading: false,
        loaded: false,
        prefix: '/',
        skills: [],
        error: 'desktopOffline',
      },
    });
    return;
  }

  const requestId = newRequestId();
  useAssistStore.setState({
    skills: {
      paneId,
      requestId,
      loading: true,
      loaded: false,
      prefix: prev?.paneId === paneId ? prev.prefix : '/',
      skills: prev?.paneId === paneId ? prev.skills : [],
      error: null,
    },
  });
  sendToRelay({ type: 'requestProjectSkills', requestId, paneId });
}

/** 处理下行补全响应：按 requestId 对账，过期响应直接丢弃。 */
export function handleAssistMessage(msg: AssistIncomingMessage) {
  if (msg.type === 'projectFiles') {
    const cur = useAssistStore.getState().files;
    if (!cur || cur.requestId !== msg.requestId || cur.paneId !== msg.paneId) return;
    useAssistStore.setState({
      files: {
        ...cur,
        requestId: null,
        loading: false,
        files: msg.ok ? (msg.files ?? []) : [],
        truncated: msg.ok && msg.truncated === true,
        error: msg.ok ? null : (msg.reason ?? 'readFailed'),
      },
    });
    return;
  }

  const cur = useAssistStore.getState().skills;
  if (!cur || cur.requestId !== msg.requestId || cur.paneId !== msg.paneId) return;
  useAssistStore.setState({
    skills: {
      ...cur,
      requestId: null,
      loading: false,
      loaded: msg.ok,
      prefix: msg.prefix || '/',
      skills: msg.ok ? (msg.skills ?? []) : [],
      error: msg.ok ? null : (msg.reason ?? 'readFailed'),
    },
  });
}
