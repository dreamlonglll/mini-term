/**
 * 移动端远程发起 AI 会话的桌面端落地(docs/specs/mobile-start-session-v1.md)。
 *
 * 后端 `mobile-start-session` 事件到达后:
 *   建 pane(按启动器绑定的 shell,未绑定则用默认 shell)
 *   → 新开一个 tab,**不**切当前项目、**不**切当前 tab
 *   → 把启动命令连同回车写入 PTY
 *
 * 最后一步是关键:AI 会话身份靠输入检测建立,只有"往 shell 里敲进启动命令并回车"
 * 这条路径能让 pane 进入 AI 会话状态,把 AI CLI 当 PTY 根程序 spawn 是行不通的。
 * 写入走 `write_pty` 全语义(输入跟踪 / AI marker),与移动端指令同一条通道。
 *
 * 回执语义是"pane 已建、命令已写入",不承诺 AI 已经起来——真正的成功信号是该 pane
 * 出现在活跃会话快照里(手机侧据此自动进入镜像,超时则提示回桌面查看)。
 */
import { invoke } from '@tauri-apps/api/core';
import { useAppStore, genId, saveLayoutToConfig } from '../store';
import { createProjectPty } from './remoteProject';
import { t } from '../i18n';
import type {
  MobileStartSessionPayload,
  StartSessionFailReason,
  TerminalTab,
} from '../types';

function reportResult(
  requestId: string,
  ok: boolean,
  paneId?: string,
  reason?: StartSessionFailReason,
): void {
  invoke('mobile_relay_start_session_result', { requestId, ok, paneId, reason }).catch(() => {
    // 中转已断开时回执无处可去;手机侧会走 15s 超时提示
  });
}

/** 处理一次移动端发起请求。异常一律转成失败回执,不让手机侧一直转圈。 */
export async function handleMobileStartSession(
  payload: MobileStartSessionPayload,
): Promise<void> {
  const { requestId, projectId, launcherName, shellName, command } = payload;
  const { config } = useAppStore.getState();
  const project = config.projects.find((p) => p.id === projectId);
  if (!project) {
    reportResult(requestId, false, undefined, 'projectNotFound');
    return;
  }

  // 启动器绑定的 shell 已被删掉时退回默认 shell:总比不开好,用户在桌面能看到实情
  const shell =
    (shellName ? config.availableShells.find((s) => s.name === shellName) : undefined) ??
    config.availableShells.find((s) => s.name === config.defaultShell) ??
    config.availableShells[0];
  // 一个 shell 都没配:开不出终端,也没有能写进布局的 shellName
  if (!shell) {
    reportResult(requestId, false, undefined, 'spawnFailed');
    return;
  }

  let ptyId: number;
  try {
    ptyId = await createProjectPty(project, shell);
  } catch {
    reportResult(requestId, false, undefined, 'spawnFailed');
    return;
  }

  const paneId = genId();
  const tab: TerminalTab = {
    id: genId(),
    // 用启动器名当 tab 标题:回到电脑前一眼看出这个 tab 是什么,手机列表里也不再
    // 显示成裸 shell 名。shellName 仍存实际 shell —— 布局恢复靠它查 availableShells。
    customTitle: launcherName,
    status: 'idle',
    splitLayout: {
      type: 'leaf',
      panes: [{ id: paneId, shellName: shell.name, status: 'idle', ptyId }],
      activePaneId: paneId,
    },
  };

  // activate=false:远程操作不改动用户桌面上正在看的现场
  useAppStore.getState().addTab(projectId, tab, false);
  saveLayoutToConfig(projectId);

  try {
    await invoke('write_pty', { ptyId, data: `${command}\r` });
  } catch {
    // pane 建出来了但命令没写进去:保留 pane(用户回桌面能看到它卡在哪),回失败
    reportResult(requestId, false, undefined, 'spawnFailed');
    return;
  }

  // 桌面端提示:凭证被盗时这是唯一的审计迹象,所以即便不切过去也要弹。
  // 项目名由 toast 的标题行展示,消息里只补启动器名。
  useAppStore.getState().pushNotification({
    projectId,
    projectName: project.name,
    kind: 'mobile-session',
    message: t('app.mobileStartSession', { launcher: launcherName }),
  });

  reportResult(requestId, true, paneId);
}
