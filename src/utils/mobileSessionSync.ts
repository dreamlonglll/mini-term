/**
 * 项目与活跃 AI 会话结构同步:前端 store → Rust 后端(mobile_relay_update_sessions)。
 *
 * 可见性规则(docs/specs/mobile-start-session-v1.md):
 * - **项目**:上报全集——手机的发起弹层要能选到没有活跃会话的项目。顺序是项目树的
 *   深度优先序(非 `config.projects` 存储序),每项带 `groupPath`(祖先分组名链),
 *   移动端顺序渲染即可还原桌面端侧栏的分组层级;桌面端的折叠态不下发。
 * - **pane**:只有处于 AI 会话中的 pane 进快照(ai-working / ai-idle,以及"曾是 AI
 *   会话且现处 error 态"的 pane),裸 shell 一律不出现。这条规则**只作用于 pane 列表**,
 *   不再决定项目是否进快照;手机首页仍只渲染有 pane 的项目,用户看不出差别。
 *
 * 后端拿全量状态自行组装增量推给中转,并据 path / sshConnectionId 判定能否远程发起。
 */
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import { getProjectsWithGroupPath } from './projectTree';
import { collectPanes } from './layoutOps';
import type { PaneState } from '../types';

/** 与后端 mobile_relay::SyncPane / SyncProject 对齐(camelCase)。 */
interface MobilePanePayload {
  paneId: string;
  title: string;
  status: string;
  /** 移动端指令写穿目标;终端尚未创建时缺省 */
  ptyId?: number;
}

interface MobileProjectPayload {
  projectId: string;
  name: string;
  /** 项目根路径:后端镜像订阅据此定位会话记录文件,不会转发给移动端 */
  path: string;
  /** SSH 远程项目的连接 id;后端据此判定 canStartSession */
  sshConnectionId?: string;
  /** 祖先分组名链(根→父),顶层项目为空;移动端据此还原桌面端的分组层级 */
  groupPath: string[];
  panes: MobilePanePayload[];
}

/** error 态保留规则:pane 上一轮属于 AI 会话,error 后仍算 AI pane(直到被关闭)。 */
let aiPaneIds = new Set<string>();
let lastSentJson = '';
let debounceTimer: ReturnType<typeof setTimeout> | undefined;
let started = false;

function computeSnapshot(): MobileProjectPayload[] {
  const { config, projectStates } = useAppStore.getState();
  const nextAiPaneIds = new Set<string>();
  const projects: MobileProjectPayload[] = [];

  // 按项目树的深度优先序上报(不是 config.projects 的存储序):移动端顺序渲染
  // 就能还原桌面端侧栏的排列,分组层级靠每项自带的 groupPath 还原
  for (const { project, groupPath } of getProjectsWithGroupPath(config)) {
    const panes: MobilePanePayload[] = [];
    const layout = projectStates.get(project.id)?.layout;
    const flat: PaneState[] = layout ? collectPanes(layout) : [];
    for (const pane of flat) {
      const isAi = pane.status === 'ai-working' || pane.status === 'ai-idle';
      const isAiError = pane.status === 'error' && aiPaneIds.has(pane.id);
      if (!isAi && !isAiError) continue;
      nextAiPaneIds.add(pane.id);
      panes.push({
        paneId: pane.id,
        title: pane.customTitle ?? pane.shellName,
        status: pane.status,
        ptyId: pane.ptyId,
      });
    }
    projects.push({
      projectId: project.id,
      name: project.name,
      path: project.path,
      sshConnectionId: project.sshConnectionId,
      groupPath,
      panes,
    });
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
