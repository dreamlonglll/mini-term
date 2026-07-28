/**
 * 粘贴落盘路径的目标解析（issue #36）。
 *
 * 背景：粘贴剪贴板图片 / 超长文本时，链路是「先在**本机** temp 落一个文件，
 * 再把路径粘进终端让 agent 去读」。这个前提在本地 shell 成立，换成远程 pane
 * 就断了 —— 粘进去的 `C:\...\clip-1.png` 远端根本不存在。
 *
 * 三类 pane 三种处理：
 * - 本地 → 原样粘 Windows 路径
 * - WSL  → 转 `/mnt/<盘符>/...`（文件本身经 automount 就能读到，只差路径形式）
 * - SSH 远程 → SFTP 上传到远端目录，粘远端绝对路径
 *
 * 判定口径刻意与后端 `create_pty` 的启动分支保持一致：
 * - SSH：`ProjectConfig.sshConnectionId` 有值（后端据此直接 spawn ssh）
 * - WSL：项目路径是 WSL UNC（后端 `decide_wsl_override` 的判定），
 *        外加 pane 自身 shell 就是 `wsl.exe` 的手工配置场景
 * 判定错的代价不对称：漏判只是回到今天的行为，误判会把文件传到不该去的机器上，
 * 所以两边都取「后端确实会那样启动」的强证据，不做模糊猜测。
 */

import { invoke } from '@tauri-apps/api/core';
import { useAppStore, findPaneContextByPty } from '../store';
import { isWslPath, windowsPathToWsl } from './wslPath';
import type { ProjectConfig } from '../types';

export const DEFAULT_REMOTE_PASTE_DIR = '.mini-term/pasted';

export type PasteTarget =
  | { kind: 'local' }
  | { kind: 'wsl' }
  | {
      kind: 'ssh';
      connectionId: string;
      projectPath: string;
      projectId: string;
      projectName: string;
    };

/** pane 用的 shell 是否就是 wsl.exe（本地项目里手工配了 WSL shell 的情况） */
function paneRunsWslShell(shellName: string | undefined): boolean {
  if (!shellName) return false;
  const cmd = useAppStore
    .getState()
    .config.availableShells.find((s) => s.name === shellName)?.command;
  if (!cmd) return false;
  // 取 basename 再比对，避免 `C:\Windows\System32\wsl.exe` 漏判、
  // 也避免 `wslconfig.exe` 这类前缀相同的命令误判。
  const base = cmd.trim().replace(/\\/g, '/').split('/').pop()?.toLowerCase() ?? '';
  return base === 'wsl' || base === 'wsl.exe';
}

/** 判断某个 pty 所在 pane 的粘贴目标类型 */
export function resolvePasteTarget(ptyId: number): PasteTarget {
  const ctx = findPaneContextByPty(ptyId);
  if (!ctx) return { kind: 'local' };

  const project: ProjectConfig | undefined = useAppStore
    .getState()
    .config.projects.find((p) => p.id === ctx.projectId);
  if (!project) return { kind: 'local' };

  if (project.sshConnectionId) {
    return {
      kind: 'ssh',
      connectionId: project.sshConnectionId,
      projectPath: project.path,
      projectId: project.id,
      projectName: project.name,
    };
  }
  if (isWslPath(project.path) || paneRunsWslShell(ctx.pane.shellName)) {
    return { kind: 'wsl' };
  }
  return { kind: 'local' };
}

/**
 * 把本机临时文件路径映射成「该终端里真正可读的路径」。
 *
 * SSH 分支会实际发起上传，**耗时与文件大小 / 链路相关**，调用方需 await 完
 * 再往 PTY 写路径（否则路径会插在用户后续输入中间）。
 * 失败一律抛错，由调用方决定提示与回退 —— 静默回退成本地路径只会让远端
 * agent 报「文件不存在」，比明确失败更难排查。
 */
export async function mapPastedFilePath(
  localPath: string,
  target: PasteTarget,
): Promise<string> {
  switch (target.kind) {
    case 'local':
      return localPath;
    case 'wsl':
      // 转不了（UNC 等非盘符路径）就原样返回，行为退回改动前
      return windowsPathToWsl(localPath) ?? localPath;
    case 'ssh': {
      const destDir =
        useAppStore.getState().config.remotePasteDir?.trim() || DEFAULT_REMOTE_PASTE_DIR;
      return await invoke<string>('ssh_remote_upload_paste', {
        connectionId: target.connectionId,
        projectPath: target.projectPath,
        localPath,
        destDir,
      });
    }
  }
}
