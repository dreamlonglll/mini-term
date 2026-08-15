import { invoke } from '@tauri-apps/api/core';
import { useAppStore, saveLayoutToConfig, setPaneAiSessionByPty } from '../store';
import { collectPanes } from './layoutOps';
import { activatePane, newTerminal } from './paneActions';
import {
  branchCapsForAgent,
  buildSessionTree,
  findFamilyRoot,
  flattenSessionTree,
  mergeLineageEdges,
  type FlatSessionRow,
} from './sessionBranch';
import { writePtyInput } from './terminalCache';
import { t } from '../i18n';
import type { AiSession, LineageEdge, PaneState } from '../types';

/**
 * 点击分支树/浮层里的会话节点该发生什么（设计文档「节点点击行为」）：
 * 已有 pane 在跑这个会话 → 切项目 + 激活 + 聚焦；没有 → 当前项目新开
 * tab 写 resume 命令恢复。
 */

/** 按 sessionId 跨全部项目找「在跑」的 pane。三个条件缺一不可：会话身份匹配、
 *  PTY 活着、状态是 ai-working/ai-idle —— aiSession 在 AI 退出后为续接语义
 *  刻意保留（status 落回 idle），只看身份会把「claude 已退出的 shell」当成
 *  在跑，点击分支节点跳过去对着一个死会话，而不是按口径新终端恢复。 */
export function findLiveSessionPane(
  sessionId: string,
): { projectId: string; paneId: string; status: PaneState['status'] } | null {
  const { projectStates, exitedPtyIds } = useAppStore.getState();
  for (const [projectId, ps] of projectStates) {
    if (!ps.layout) continue;
    for (const pane of collectPanes(ps.layout)) {
      if (
        pane.aiSession?.sessionId === sessionId
        && pane.ptyId !== undefined
        && !exitedPtyIds.has(pane.ptyId)
        && (pane.status === 'ai-working' || pane.status === 'ai-idle')
      ) {
        return { projectId, paneId: pane.id, status: pane.status };
      }
    }
  }
  return null;
}

/** sessionId 所在家族的连线行（悬停面板 BranchFamilyPanel 的取数口径：
 *  磁盘扫描边 + 自记账边合并，磁盘优先）。找不到家族返回空数组。 */
export async function fetchFamilyRows(
  projectPath: string,
  sessionId: string,
): Promise<FlatSessionRow[]> {
  const [sessions, edges] = await Promise.all([
    invoke<AiSession[]>('get_ai_sessions', { projectPath }),
    // 自记账边传给后端补「分叉后第一问」标题(claude CLI fork 无磁盘指针)
    invoke<LineageEdge[]>('scan_session_lineage', {
      projectPath,
      bookkept: useAppStore.getState().config.sessionLineage ?? [],
    }).catch(() => [] as LineageEdge[]),
  ]);
  const merged = mergeLineageEdges(edges, useAppStore.getState().config.sessionLineage ?? []);
  const family = findFamilyRoot(buildSessionTree(sessions, merged), sessionId);
  return family ? flattenSessionTree([family]) : [];
}

export async function jumpToSession(projectId: string, session: AiSession): Promise<void> {
  const live = findLiveSessionPane(session.id);
  if (live) {
    useAppStore.getState().setActiveProject(live.projectId);
    // 项目切换后布局才挂到前台，DOM 聚焦要等这一帧过去（同 attentionJump）
    requestAnimationFrame(() => activatePane(live.projectId, live.paneId));
    return;
  }

  // WSL / SSH 远程来源的会话记录在别的机器/发行版里，本地 resume 恢复不了
  if (session.wslDistro || session.sshConnectionId) {
    const project = useAppStore.getState().config.projects.find((p) => p.id === projectId);
    useAppStore.getState().pushNotification({
      projectId,
      projectName: project?.name ?? '',
      kind: 'wsl-info',
      message: t('sessionList.branchTree.remoteResumeUnsupported'),
    });
    return;
  }

  const cmd = branchCapsForAgent(session.sessionType)?.resumeCommand(session.id);
  if (!cmd) return;

  // claude --resume 只认「启动目录」对应的会话桶：起子目录的会话在项目根恢复
  // 会报 No conversation found，先反查记录的 cwd（与 PaneGroup 续接同一坑）；
  // codex 不按目录分桶，无需反查。
  let cwd: string | undefined;
  if (session.sessionType === 'claude' && /^[A-Za-z0-9_-]+$/.test(session.id)) {
    cwd = (await invoke<string | null>('lookup_ai_session_cwd', { sessionId: session.id })
      .catch(() => null)) ?? undefined;
  }
  const pane = await newTerminal(projectId, undefined, { cwd });
  if (!pane || pane.ptyId === undefined) return;
  void writePtyInput(pane.ptyId, `${cmd}\r`);
  // 恢复出的会话身份当场写回 pane，不等 hook —— codex resume 不会重新上报
  // SessionStart（与 PaneGroup 续接链路同一口径），干等会让新 pane 永远拿不到
  // 身份，右键的分支入口随之消失；claude 会上报同 id 幂等覆盖。身份随布局
  // 持久化，重启自动续接顺带受益。
  setPaneAiSessionByPty(pane.ptyId, {
    agent: session.sessionType,
    sessionId: session.id,
    cwd,
  });
  saveLayoutToConfig(projectId);
}
