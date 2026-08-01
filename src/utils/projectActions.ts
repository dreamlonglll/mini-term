/**
 * 项目级操作的共享实现(不依赖 React,组件与弹窗都能调)。
 */
import { invoke } from '@tauri-apps/api/core';
import { useAppStore, collectPtyIds, saveConfigToDisk } from '../store';
import { disposeTerminal } from './terminalCache';

/** 路径归一化:worktree 路径与项目路径可能一个用 / 一个用 \,比对前统一。 */
export function normalizePath(p: string): string {
  return p.replace(/[\\/]+/g, '/').replace(/\/$/, '').toLowerCase();
}

/** 按路径找已存在的项目(本地项目;远程项目 path 是远端路径,不参与比对)。 */
export function findProjectByPath(path: string) {
  const { config } = useAppStore.getState();
  const target = normalizePath(path);
  return config.projects.find((p) => !p.sshConnectionId && normalizePath(p.path) === target);
}

/**
 * 回收某项目全部终端资源:杀后端 PTY 子进程 + dispose 前端 xterm 实例。
 * pane 的 ptyId 仍留在布局里,PaneGroup 不会自动重建(重建只发生在 ptyId 为空时)。
 * 删 worktree 前也单独调它:Windows 下 shell 占着目录会让 `git worktree remove` 失败。
 */
export function disposeProjectTerminals(id: string): void {
  const ps = useAppStore.getState().projectStates.get(id);
  if (!ps?.layout) return;
  for (const ptyId of new Set(collectPtyIds(ps.layout))) {
    invoke('kill_pty', { ptyId }).catch(() => {});
    disposeTerminal(ptyId);
  }
}

/**
 * 删除项目并回收其全部终端资源,否则会残留孤儿 shell/AI 进程与泄漏的
 * WebGL 上下文。markers 由 removeProject 内部清理。
 */
export function removeProjectWithCleanup(id: string): void {
  disposeProjectTerminals(id);
  useAppStore.getState().removeProject(id);
  saveConfigToDisk();
}
