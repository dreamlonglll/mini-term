/**
 * 活跃 AI 会话结构同步:前端 store → Rust 后端(mobile_relay_update_sessions)。
 *
 * 可见性规则(docs/specs/mobile-relay-v1.md):仅处于 AI 会话中的 pane 进入快照——
 * ai-working / ai-idle,以及"曾是 AI 会话且现处 error 态"的 pane;裸 shell 一律不出现。
 * 无可见 pane 的项目不出现。后端拿全量状态自行组装增量推给中转。
 */
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import type { SplitNode, PaneState } from '../types';

/** 与后端 mt-relay-protocol 的 MobilePane / MobileProject 对齐(camelCase)。 */
interface MobilePanePayload {
  paneId: string;
  title: string;
  status: string;
}

interface MobileProjectPayload {
  projectId: string;
  name: string;
  /** 项目根路径:后端镜像订阅据此定位会话记录文件,不会转发给移动端 */
  path: string;
  panes: MobilePanePayload[];
}

/** error 态保留规则:pane 上一轮属于 AI 会话,error 后仍算 AI pane(直到被关闭)。 */
let aiPaneIds = new Set<string>();
let lastSentJson = '';
let debounceTimer: ReturnType<typeof setTimeout> | undefined;
let started = false;

function collectPanes(node: SplitNode, out: PaneState[]): void {
  if (node.type === 'leaf') {
    out.push(...node.panes);
  } else {
    for (const child of node.children) collectPanes(child, out);
  }
}

function computeSnapshot(): MobileProjectPayload[] {
  const { config, projectStates } = useAppStore.getState();
  const nextAiPaneIds = new Set<string>();
  const projects: MobileProjectPayload[] = [];

  for (const project of config.projects) {
    const ps = projectStates.get(project.id);
    if (!ps) continue;
    const panes: MobilePanePayload[] = [];
    for (const tab of ps.tabs) {
      const flat: PaneState[] = [];
      collectPanes(tab.splitLayout, flat);
      for (const pane of flat) {
        const isAi = pane.status === 'ai-working' || pane.status === 'ai-idle';
        const isAiError = pane.status === 'error' && aiPaneIds.has(pane.id);
        if (!isAi && !isAiError) continue;
        nextAiPaneIds.add(pane.id);
        panes.push({
          paneId: pane.id,
          title: pane.customTitle ?? tab.customTitle ?? pane.shellName,
          status: pane.status,
        });
      }
    }
    if (panes.length > 0) {
      projects.push({ projectId: project.id, name: project.name, path: project.path, panes });
    }
  }

  aiPaneIds = nextAiPaneIds;
  return projects;
}

function syncNow(): void {
  const projects = computeSnapshot();
  const json = JSON.stringify(projects);
  if (json === lastSentJson) return;
  lastSentJson = json;
  invoke('mobile_relay_update_sessions', { projects }).catch(() => {
    // 后端不可用(纯前端 dev 模式)时静默;下次状态变化会重试
    lastSentJson = '';
  });
}

/** App 挂载时调用一次:订阅 store,状态变化去抖 150ms 后同步给后端。 */
export function initMobileSessionSync(): void {
  if (started) return;
  started = true;
  useAppStore.subscribe(() => {
    clearTimeout(debounceTimer);
    debounceTimer = setTimeout(syncNow, 150);
  });
  syncNow();
}
