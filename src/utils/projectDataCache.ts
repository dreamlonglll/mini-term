import type { FileEntry, GitFileStatus, GitRepoInfo } from '../types';

interface FileTreeCache {
  rootEntries: FileEntry[];
  gitStatusMap: Map<string, GitFileStatus>;
}

interface GitHistoryCache {
  repos: GitRepoInfo[];
  selectedRepo: string;
}

const fileTreeCache = new Map<string, FileTreeCache>();
const gitHistoryCache = new Map<string, GitHistoryCache>();

/**
 * 项目级缓存 key:SSH 远程项目掺连接 id —— 不同服务器上同一 POSIX 路径
 * (如两台机器都有 `/root/app`)的远程项目不得互串缓存。
 * 本地项目 key 即 path,与既有行为完全一致。
 */
export function projectCacheKey(project: { path: string; sshConnectionId?: string }): string {
  return project.sshConnectionId ? `${project.sshConnectionId}|${project.path}` : project.path;
}

export function getFileTreeCache(projectPath: string): FileTreeCache | undefined {
  return fileTreeCache.get(projectPath);
}

export function setFileTreeCache(projectPath: string, data: FileTreeCache): void {
  fileTreeCache.set(projectPath, data);
}

export function getGitHistoryCache(projectPath: string): GitHistoryCache | undefined {
  return gitHistoryCache.get(projectPath);
}

export function setGitHistoryCache(projectPath: string, data: GitHistoryCache): void {
  gitHistoryCache.set(projectPath, data);
}

export function clearProjectCache(projectPath: string): void {
  fileTreeCache.delete(projectPath);
  gitHistoryCache.delete(projectPath);
}
