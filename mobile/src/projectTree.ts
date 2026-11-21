import type { MobileProject } from './protocol';

export interface GroupNode {
  kind: 'group';
  /** 全路径 key:同名但不同父的组不会被并到一起 */
  key: string;
  name: string;
  children: ProjectTreeNode[];
}

export type ProjectTreeNode = GroupNode | { kind: 'project'; project: MobileProject };

/**
 * 把带 groupPath 的扁平项目清单重建成可折叠的分组树(发起弹层与历史项目选择共用)。
 *
 * 组按整条路径索引复用,不要求同组项目在输入里连续;没有任何项目的空组不会出现;
 * groupPath 缺省(旧桌面端 / 旧中转把字段吃掉)时整棵树退化成平铺列表。
 */
export function buildProjectTree(projects: MobileProject[]): ProjectTreeNode[] {
  const roots: ProjectTreeNode[] = [];
  const groups = new Map<string, GroupNode>();

  for (const project of projects) {
    let siblings = roots;
    const path: string[] = [];
    for (const name of project.groupPath ?? []) {
      path.push(name);
      const key = JSON.stringify(path);
      let group = groups.get(key);
      if (!group) {
        group = { kind: 'group', key, name, children: [] };
        groups.set(key, group);
        siblings.push(group);
      }
      siblings = group.children;
    }
    siblings.push({ kind: 'project', project });
  }
  return roots;
}

/** 组内项目总数(含子组),对齐桌面端组名后面的计数 */
export function countProjects(group: GroupNode): number {
  let total = 0;
  for (const child of group.children) {
    total += child.kind === 'group' ? countProjects(child) : 1;
  }
  return total;
}
