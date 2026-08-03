import type { ProjectConfig } from '../types';

/** UNC 路径(\\wsl$ 等):存在性探测依赖 WSL/网络状态,误判风险高,不参与清理 */
function isUncPath(p: string): boolean {
  return p.startsWith('\\\\');
}

/** 可参与失效清理的 worktree 子项目:本地路径、父项目存在且也是本地路径 */
function reconcilableChildren(
  projects: ProjectConfig[],
): { child: ProjectConfig; parent: ProjectConfig }[] {
  const byId = new Map(projects.map((p) => [p.id, p]));
  const out: { child: ProjectConfig; parent: ProjectConfig }[] = [];
  for (const p of projects) {
    if (!p.parentProjectId || p.sshConnectionId || isUncPath(p.path)) continue;
    const parent = byId.get(p.parentProjectId);
    if (!parent || parent.sshConnectionId || isUncPath(parent.path)) continue;
    out.push({ child: p, parent });
  }
  return out;
}

/**
 * 待探测的路径集合(去重):worktree 子项目自身路径 + 其父项目路径。
 * 父项目路径也要探测:整棵目录树一起消失(盘符拔出等)时不能把子项目误判为
 * 「worktree 被外部删除」而清理掉。
 */
export function collectWorktreeProbePaths(projects: ProjectConfig[]): string[] {
  const paths = new Set<string>();
  for (const { child, parent } of reconcilableChildren(projects)) {
    paths.add(child.path);
    paths.add(parent.path);
  }
  return [...paths];
}

/**
 * 找出应清理的失效 worktree 子项目:自身目录已不存在,而父项目目录仍在。
 * `existingPaths` 是 collectWorktreeProbePaths 的结果经存在性过滤后的子集,
 * 路径字符串原样往返,无需归一化。
 */
export function findStaleWorktreeProjects(
  projects: ProjectConfig[],
  existingPaths: Iterable<string>,
): ProjectConfig[] {
  const alive = new Set(existingPaths);
  return reconcilableChildren(projects)
    .filter(({ child, parent }) => !alive.has(child.path) && alive.has(parent.path))
    .map(({ child }) => child);
}
