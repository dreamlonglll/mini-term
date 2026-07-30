/**
 * SSH 远程项目辅助函数（task 07-05-ssh-remote-projects PR3）。
 *
 * 远程项目 = `ProjectConfig.sshConnectionId` 有值,`path` 存远程 POSIX 绝对路径。
 * 连接被删除（config.sshConnections 里找不到 id）= 「断链」错误态:
 * 项目仍可见、可删除,功能入口需给出明确错误提示。
 */

import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import { getProjectEnvs } from './projectEnv';
import type { ProjectConfig, ShellConfig, SshConnection, SshRemoteSpec } from '../types';

/** 是否为 SSH 远程项目 */
export function isRemoteProject(project: ProjectConfig | undefined | null): boolean {
  return !!project?.sshConnectionId;
}

/** 取远程项目引用的 SSH 连接;断链（连接被删除）时返回 undefined */
export function getRemoteConnection(
  project: ProjectConfig | undefined | null,
): SshConnection | undefined {
  const id = project?.sshConnectionId;
  if (!id) return undefined;
  return useAppStore.getState().config.sshConnections.find((c) => c.id === id);
}

/** 远程 pane 的显示名:连接名（断链时回退 'ssh'） */
export function remotePaneLabel(project: ProjectConfig): string {
  return getRemoteConnection(project)?.name ?? 'ssh';
}

/**
 * 为项目创建 PTY:
 * - 远程项目 → create_pty 带 `sshRemote`,后端直接 spawn ssh 启动器
 *   （shell/args/cwd 被后端忽略,envVars 不注入,密码 autofill 由后端 spawn 前预注册）;
 *   断链 / 本机缺 ssh 客户端时后端返回明确 Err,由调用方展示。
 * - 本地项目 → 按 shell 启动（行为与既有链路完全一致）。
 *   `cwd` 覆盖仅对本地项目生效(worktree 终端在 worktree 目录起 shell);
 *   远程项目的工作目录由 sshRemote.remotePath 决定,覆盖无意义。
 */
export function createProjectPty(
  project: ProjectConfig,
  shell: ShellConfig | undefined,
  cwd?: string,
): Promise<number> {
  if (project.sshConnectionId) {
    const sshRemote: SshRemoteSpec = {
      connectionId: project.sshConnectionId,
      remotePath: project.path,
    };
    return invoke<number>('create_pty', {
      shell: '',
      args: [],
      cwd: '',
      sshRemote,
    });
  }
  if (!shell) {
    return Promise.reject(new Error('no shell configured'));
  }
  return invoke<number>('create_pty', {
    shell: shell.command,
    args: shell.args ?? [],
    cwd: cwd ?? project.path,
    envs: getProjectEnvs(project.id),
  });
}
