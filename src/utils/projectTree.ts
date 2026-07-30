import type { ProjectTreeItem, ProjectGroup, ProjectConfig, AppConfig } from '../types';

export const MAX_DEPTH = 3;

// === 节点类型判断 ===

export function isGroup(item: ProjectTreeItem): item is ProjectGroup {
  return typeof item !== 'string';
}

export function getItemId(item: ProjectTreeItem): string {
  return isGroup(item) ? item.id : item;
}

// === 树查询 ===

/** 计算节点在树中的深度（0 = 顶层），未找到返回 -1 */
export function getDepth(tree: ProjectTreeItem[], targetId: string, currentDepth = 0): number {
  for (const item of tree) {
    if (getItemId(item) === targetId) return currentDepth;
    if (isGroup(item)) {
      const found = getDepth(item.children, targetId, currentDepth + 1);
      if (found !== -1) return found;
    }
  }
  return -1;
}

/** 计算子树占用的额外深度层数。项目→0, 空组→0, 含项目的组→1, 含子组的组→2+ */
export function getSubtreeMaxDepth(item: ProjectTreeItem): number {
  if (!isGroup(item)) return 0;
  if (item.children.length === 0) return 0;
  let max = 0;
  for (const child of item.children) {
    max = Math.max(max, getSubtreeMaxDepth(child));
  }
  return max + 1;
}

/** 检查 ancestorId 是否是 targetId 的祖先 */
export function isDescendant(tree: ProjectTreeItem[], ancestorId: string, targetId: string): boolean {
  for (const item of tree) {
    if (isGroup(item) && item.id === ancestorId) {
      return findInTree(item.children, targetId);
    }
    if (isGroup(item)) {
      const result = isDescendant(item.children, ancestorId, targetId);
      if (result) return true;
    }
  }
  return false;
}

function findInTree(tree: ProjectTreeItem[], id: string): boolean {
  for (const item of tree) {
    if (getItemId(item) === id) return true;
    if (isGroup(item) && findInTree(item.children, id)) return true;
  }
  return false;
}

/** 拖拽合法性检查（放入组内 inside）：无循环 且 深度不超限 */
export function canDrop(
  tree: ProjectTreeItem[],
  targetGroupId: string,
  draggedItem: ProjectTreeItem,
): boolean {
  const draggedId = getItemId(draggedItem);
  if (draggedId === targetGroupId) return false;
  if (isGroup(draggedItem) && isDescendant(tree, draggedId, targetGroupId)) return false;
  const targetDepth = getDepth(tree, targetGroupId);
  if (targetDepth === -1) return false;
  return targetDepth + 1 + getSubtreeMaxDepth(draggedItem) <= MAX_DEPTH;
}

/** 拖拽合法性检查（放到旁边 before/after）：被拖项放到目标的同级位置 */
export function canDropAt(
  tree: ProjectTreeItem[],
  targetId: string,
  draggedItem: ProjectTreeItem,
): boolean {
  if (!isGroup(draggedItem)) return true;
  const parentId = findParentGroupId(tree, targetId);
  if (parentId === null) {
    return getSubtreeMaxDepth(draggedItem) <= MAX_DEPTH;
  }
  const parentDepth = getDepth(tree, parentId);
  if (parentDepth === -1) return true;
  return parentDepth + 1 + getSubtreeMaxDepth(draggedItem) <= MAX_DEPTH;
}

// === 深拷贝 ===

/** 深拷贝树（所有树操作函数就地修改，调用前必须先深拷贝） */
export function deepCloneTree(tree: ProjectTreeItem[]): ProjectTreeItem[] {
  return tree.map((item) => {
    if (!isGroup(item)) return item;
    return { ...item, children: deepCloneTree(item.children) };
  });
}

// === 树操作（就地修改，调用前请先 deepCloneTree） ===

/** 从树中移除节点，返回被移除的节点 */
export function removeFromTree(tree: ProjectTreeItem[], id: string): ProjectTreeItem | null {
  for (let i = 0; i < tree.length; i++) {
    if (getItemId(tree[i]) === id) {
      return tree.splice(i, 1)[0];
    }
    const item = tree[i];
    if (isGroup(item)) {
      const found = removeFromTree(item.children, id);
      if (found) return found;
    }
  }
  return null;
}

/** 插入节点到指定组内（targetGroupId 为 null 表示根级别），返回是否成功 */
export function insertIntoTree(
  tree: ProjectTreeItem[],
  targetGroupId: string | null,
  item: ProjectTreeItem,
  index?: number,
): boolean {
  if (targetGroupId === null) {
    const idx = index !== undefined ? Math.min(index, tree.length) : tree.length;
    tree.splice(idx, 0, item);
    return true;
  }
  for (const node of tree) {
    if (isGroup(node) && node.id === targetGroupId) {
      const idx = index !== undefined ? Math.min(index, node.children.length) : node.children.length;
      node.children.splice(idx, 0, item);
      return true;
    }
    if (isGroup(node)) {
      if (insertIntoTree(node.children, targetGroupId, item, index)) return true;
    }
  }
  return false;
}

/** 在树中查找组并更新 */
export function updateGroupInTree(
  tree: ProjectTreeItem[],
  groupId: string,
  updater: (group: ProjectGroup) => ProjectGroup,
): boolean {
  for (let i = 0; i < tree.length; i++) {
    const item = tree[i];
    if (isGroup(item)) {
      if (item.id === groupId) {
        tree[i] = updater(item);
        return true;
      }
      if (updateGroupInTree(item.children, groupId, updater)) return true;
    }
  }
  return false;
}

/** 删除组，将其子项释放到父级原位置 */
export function removeGroupAndPromoteChildren(tree: ProjectTreeItem[], groupId: string): boolean {
  for (let i = 0; i < tree.length; i++) {
    const item = tree[i];
    if (isGroup(item) && item.id === groupId) {
      tree.splice(i, 1, ...item.children);
      return true;
    }
    if (isGroup(item)) {
      if (removeGroupAndPromoteChildren(item.children, groupId)) return true;
    }
  }
  return false;
}

/** 从树中移除项目 id,并在原位置插入替补 id 列表(父项目删除时子项目晋升原位)。
 *  未找到目标时返回 false,替补的去处由调用方兜底。 */
export function replaceProjectInTree(
  tree: ProjectTreeItem[],
  projectId: string,
  replacementIds: string[],
): boolean {
  for (let i = 0; i < tree.length; i++) {
    if (tree[i] === projectId) {
      tree.splice(i, 1, ...replacementIds);
      return true;
    }
    const item = tree[i];
    if (isGroup(item) && replaceProjectInTree(item.children, projectId, replacementIds)) {
      return true;
    }
  }
  return false;
}

/** 从树中递归移除指定项目 ID */
export function removeProjectFromTree(tree: ProjectTreeItem[], projectId: string): boolean {
  for (let i = 0; i < tree.length; i++) {
    if (tree[i] === projectId) {
      tree.splice(i, 1);
      return true;
    }
    const item = tree[i];
    if (isGroup(item)) {
      if (removeProjectFromTree(item.children, projectId)) return true;
    }
  }
  return false;
}

// === 渲染辅助 ===

export type OrderedItem =
  | { type: 'project'; project: ProjectConfig; depth: number; parentGroupId: string | null }
  | { type: 'group'; group: ProjectGroup; depth: number; parentGroupId: string | null };

/** 递归展平树为带 depth 和 parentGroupId 的有序列表。
 *  子项目(parentProjectId 有值,不在树中)紧随其父项目之后、深度 +1 注入。 */
export function getOrderedTree(config: AppConfig): OrderedItem[] {
  const projectMap = new Map(config.projects.map((p) => [p.id, p]));
  const result: OrderedItem[] = [];

  const childrenByParent = new Map<string, ProjectConfig[]>();
  for (const p of config.projects) {
    if (!p.parentProjectId) continue;
    const list = childrenByParent.get(p.parentProjectId) ?? [];
    list.push(p);
    childrenByParent.set(p.parentProjectId, list);
  }

  // pushed 兼做环路保护:异常配置里 parentProjectId 互指时不至于无限递归
  const pushed = new Set<string>();
  function pushProject(project: ProjectConfig, depth: number, parentGroupId: string | null) {
    if (pushed.has(project.id)) return;
    pushed.add(project.id);
    result.push({ type: 'project', project, depth, parentGroupId });
    for (const child of childrenByParent.get(project.id) ?? []) {
      pushProject(child, depth + 1, parentGroupId);
    }
  }

  function walk(items: ProjectTreeItem[], depth: number, parentGroupId: string | null) {
    for (const item of items) {
      if (isGroup(item)) {
        result.push({ type: 'group', group: item, depth, parentGroupId });
        if (!item.collapsed) {
          walk(item.children, depth + 1, item.id);
        }
      } else {
        const project = projectMap.get(item);
        if (project) {
          pushProject(project, depth, parentGroupId);
        }
      }
    }
  }

  const tree = config.projectTree ?? [];
  walk(tree, 0, null);

  // 追加既不在 tree 中、也没有(存活的)父项目的项目到顶层。
  // 判断必须基于完整树而非 pushed——折叠组的子项没被 walk 渲染,但它们不是孤儿;
  // 父项目丢失的孤儿子项目在这里兜底回到顶层,保证不凭空消失。
  const inTree = new Set<string>();
  (function collectIds(items: ProjectTreeItem[]) {
    for (const item of items) {
      if (isGroup(item)) collectIds(item.children);
      else inTree.add(item);
    }
  })(tree);

  for (const p of config.projects) {
    if (pushed.has(p.id)) continue;
    if (inTree.has(p.id)) continue; // 折叠组内的项目:在树中,只是视图上隐藏
    if (p.parentProjectId && projectMap.has(p.parentProjectId)) continue;
    pushProject(p, 0, null);
  }

  return result;
}

/** 项目 + 它在树中的祖先分组名链（根→父）；顶层项目为空数组 */
export interface ProjectWithGroupPath {
  project: ProjectConfig;
  groupPath: string[];
}

/**
 * 按树序（深度优先）列出全部项目，每个带上祖先分组名链。
 *
 * 与 `getOrderedTree` 的区别：**不**跳过折叠组的子项——折叠是桌面侧栏的视图状态，
 * 而这里的消费者（移动端快照）要的是完整清单，折叠与否由移动端自己决定。
 */
export function getProjectsWithGroupPath(config: AppConfig): ProjectWithGroupPath[] {
  const projectMap = new Map(config.projects.map((p) => [p.id, p]));
  const result: ProjectWithGroupPath[] = [];
  const seen = new Set<string>();

  function walk(items: ProjectTreeItem[], groupPath: string[]) {
    for (const item of items) {
      if (isGroup(item)) {
        walk(item.children, [...groupPath, item.name]);
        continue;
      }
      const project = projectMap.get(item);
      if (project && !seen.has(project.id)) {
        seen.add(project.id);
        result.push({ project, groupPath });
      }
    }
  }
  walk(config.projectTree ?? [], []);

  // 不在树中的项目（异常配置兜底）追加到顶层，与 getOrderedTree 口径一致
  for (const p of config.projects) {
    if (!seen.has(p.id)) result.push({ project: p, groupPath: [] });
  }
  return result;
}

/** 收集树中所有组（递归），返回 [group, depth] 对 */
export function collectAllGroups(tree: ProjectTreeItem[], depth = 0): Array<[ProjectGroup, number]> {
  const result: Array<[ProjectGroup, number]> = [];
  for (const item of tree) {
    if (isGroup(item)) {
      result.push([item, depth]);
      result.push(...collectAllGroups(item.children, depth + 1));
    }
  }
  return result;
}

/** 计算组内总项目数（含嵌套子组内的项目） */
export function countProjectsInGroup(group: ProjectGroup): number {
  let count = 0;
  for (const child of group.children) {
    if (isGroup(child)) {
      count += countProjectsInGroup(child);
    } else {
      count++;
    }
  }
  return count;
}

/** 查找节点所在的父组 ID。顶层返回 null，未找到也返回 null */
export function findParentGroupId(tree: ProjectTreeItem[], targetId: string): string | null {
  for (const item of tree) {
    if (getItemId(item) === targetId) return null;
    if (isGroup(item)) {
      for (const child of item.children) {
        if (getItemId(child) === targetId) return item.id;
      }
      const found = findParentGroupIdInner(item.children, targetId);
      if (found !== null) return found;
    }
  }
  return null;
}

function findParentGroupIdInner(tree: ProjectTreeItem[], targetId: string): string | null {
  for (const item of tree) {
    if (isGroup(item)) {
      for (const child of item.children) {
        if (getItemId(child) === targetId) return item.id;
      }
      const found = findParentGroupIdInner(item.children, targetId);
      if (found !== null) return found;
    }
  }
  return null;
}

/** 在树中查找组（递归） */
export function findGroupInTree(tree: ProjectTreeItem[], groupId: string): ProjectGroup | null {
  for (const item of tree) {
    if (isGroup(item)) {
      if (item.id === groupId) return item;
      const found = findGroupInTree(item.children, groupId);
      if (found) return found;
    }
  }
  return null;
}

// === 迁移辅助 ===

/** 从旧配置格式迁移到 projectTree（前端侧，作为 Rust 迁移的备份） */
export function migrateToTree(config: AppConfig): ProjectTreeItem[] {
  const { projectGroups, projectOrdering, projects } = config;
  if (!projectOrdering || projectOrdering.length === 0) {
    return projects.map((p) => p.id);
  }
  const groupMap = new Map((projectGroups ?? []).map((g) => [g.id, g]));
  const tree: ProjectTreeItem[] = [];
  const seen = new Set<string>();

  for (const itemId of projectOrdering) {
    const oldGroup = groupMap.get(itemId);
    if (oldGroup) {
      seen.add(itemId);
      const children: ProjectTreeItem[] = oldGroup.projectIds.map((pid) => {
        seen.add(pid);
        return pid;
      });
      tree.push({ id: oldGroup.id, name: oldGroup.name, collapsed: oldGroup.collapsed, children });
    } else {
      seen.add(itemId);
      tree.push(itemId);
    }
  }
  for (const p of projects) {
    if (!seen.has(p.id)) tree.push(p.id);
  }
  return tree;
}
