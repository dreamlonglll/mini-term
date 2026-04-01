# Projects 嵌套组功能 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 Projects 板块增加嵌套组支持，允许组内包含子组，最大 3 层深度。

**Architecture:** 用 `projectTree: (string | ProjectGroup)[]` 树形结构替代当前的 `projectGroups` + `projectOrdering` 扁平模型。纯函数树操作工具层处理所有增删移动，store actions 调用工具层，组件通过 `getOrderedTree()` 获取带 depth 的扁平渲染列表。Rust 后端用 `#[serde(untagged)]` enum 反序列化，load 时自动迁移旧配置。

**Tech Stack:** TypeScript, React 19, Zustand, Rust (Tauri v2, serde)

**Spec:** `docs/superpowers/specs/2026-04-01-nested-groups-design.md`

---

## 文件结构

| 操作 | 文件 | 职责 |
|---|---|---|
| 修改 | `src/types.ts` | 新增 `ProjectTreeItem`，改造 `ProjectGroup`，`AppConfig` 新增 `projectTree` |
| 新建 | `src/utils/projectTree.ts` | 树操作纯函数（getDepth, removeFromTree, insertIntoTree, getSubtreeMaxDepth, isDescendant, canDrop, canDropAt, getOrderedTree, migrateToTree, collectAllGroups, findGroupInTree, deepCloneTree） |
| 修改 | `src/store.ts` | 移除旧分组 actions，新增树操作 actions，导入 projectTree 工具函数 |
| 修改 | `src/components/ProjectList.tsx` | 嵌套渲染、深度缩进、拖拽校验、右键菜单增强 |
| 修改 | `src-tauri/src/config.rs` | `ProjectTreeItem` enum、`ProjectGroup` 改 children、迁移逻辑 |

---

### Task 1: Rust 后端 — 数据模型与迁移

**Files:**
- Modify: `src-tauri/src/config.rs:6-31`（ProjectGroup、AppConfig 结构）

- [ ] **Step 1: 改造 ProjectGroup 结构**

将 `ProjectGroup` 的 `project_ids: Vec<String>` 改为递归 `children`:

```rust
// 注意：variant 顺序不可调换！untagged 按声明顺序尝试匹配
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProjectTreeItem {
    ProjectId(String),
    Group(ProjectGroup),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectGroup {
    pub id: String,
    pub name: String,
    pub collapsed: bool,
    pub children: Vec<ProjectTreeItem>,
}
```

- [ ] **Step 2: 更新 AppConfig 结构**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub projects: Vec<ProjectConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_tree: Option<Vec<ProjectTreeItem>>,
    // 旧字段：仅用于迁移读取，不再序列化
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_groups: Option<Vec<OldProjectGroup>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_ordering: Option<Vec<String>>,
    pub default_shell: String,
    pub available_shells: Vec<ShellConfig>,
    #[serde(default = "default_ui_font_size")]
    pub ui_font_size: f64,
    #[serde(default = "default_terminal_font_size")]
    pub terminal_font_size: f64,
    #[serde(default)]
    pub layout_sizes: Option<Vec<f64>>,
}
```

需要保留旧 `ProjectGroup`（重命名为 `OldProjectGroup`）用于反序列化旧配置：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OldProjectGroup {
    pub id: String,
    pub name: String,
    pub collapsed: bool,
    pub project_ids: Vec<String>,
}
```

更新 `Default for AppConfig`：

```rust
impl Default for AppConfig {
    fn default() -> Self {
        Self {
            projects: vec![],
            project_tree: None,
            project_groups: None,
            project_ordering: None,
            default_shell: default_shell_name(),
            available_shells: default_shells(),
            ui_font_size: default_ui_font_size(),
            terminal_font_size: default_terminal_font_size(),
            layout_sizes: None,
        }
    }
}
```

- [ ] **Step 3: 实现 load_config 中的迁移逻辑**

在 `load_config` 返回前，如果 `project_tree` 为 None 但旧字段有值，执行迁移：

```rust
fn migrate_config(mut config: AppConfig) -> AppConfig {
    if config.project_tree.is_some() {
        // 新格式，清理旧字段
        config.project_groups = None;
        config.project_ordering = None;
        return config;
    }
    let groups = match config.project_groups.take() {
        Some(g) if !g.is_empty() => g,
        _ => return config,
    };
    let ordering = config.project_ordering.take().unwrap_or_default();
    let group_map: std::collections::HashMap<String, &OldProjectGroup> =
        groups.iter().map(|g| (g.id.clone(), g)).collect();

    let mut tree: Vec<ProjectTreeItem> = Vec::new();
    for item_id in &ordering {
        if let Some(old_group) = group_map.get(item_id) {
            tree.push(ProjectTreeItem::Group(ProjectGroup {
                id: old_group.id.clone(),
                name: old_group.name.clone(),
                collapsed: old_group.collapsed,
                children: old_group.project_ids.iter()
                    .map(|pid| ProjectTreeItem::ProjectId(pid.clone()))
                    .collect(),
            }));
        } else {
            tree.push(ProjectTreeItem::ProjectId(item_id.clone()));
        }
    }
    config.project_tree = Some(tree);
    config
}
```

在 `load_config` 中调用 `migrate_config`。

- [ ] **Step 4: 更新现有测试，添加迁移测试**

更新 `old_config_without_groups_deserializes` 测试。新增：

```rust
#[test]
fn migrate_old_groups_to_tree() {
    let json = r#"{
        "projects": [
            {"id": "p1", "name": "proj1", "path": "/tmp/1"},
            {"id": "p2", "name": "proj2", "path": "/tmp/2"}
        ],
        "projectGroups": [{"id": "g1", "name": "Group1", "collapsed": false, "projectIds": ["p1"]}],
        "projectOrdering": ["g1", "p2"],
        "defaultShell": "cmd",
        "availableShells": [{"name": "cmd", "command": "cmd"}],
        "uiFontSize": 13,
        "terminalFontSize": 14
    }"#;
    let config: AppConfig = serde_json::from_str(json).unwrap();
    let config = migrate_config(config);
    assert!(config.project_tree.is_some());
    assert!(config.project_groups.is_none());
    assert!(config.project_ordering.is_none());
    let tree = config.project_tree.unwrap();
    assert_eq!(tree.len(), 2); // group + project
}

#[test]
fn nested_tree_round_trip() {
    let tree = vec![
        ProjectTreeItem::ProjectId("p1".into()),
        ProjectTreeItem::Group(ProjectGroup {
            id: "g1".into(),
            name: "Group1".into(),
            collapsed: false,
            children: vec![
                ProjectTreeItem::ProjectId("p2".into()),
                ProjectTreeItem::Group(ProjectGroup {
                    id: "g2".into(),
                    name: "Sub".into(),
                    collapsed: true,
                    children: vec![ProjectTreeItem::ProjectId("p3".into())],
                }),
            ],
        }),
    ];
    let json = serde_json::to_string(&tree).unwrap();
    let parsed: Vec<ProjectTreeItem> = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.len(), 2);
}
```

- [ ] **Step 5: 运行 Rust 测试**

Run: `cd src-tauri && cargo test`
Expected: 所有测试通过

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/config.rs
git commit -m "feat: Rust 后端支持嵌套组数据模型与旧配置自动迁移"
```

---

### Task 2: 前端类型定义

**Files:**
- Modify: `src/types.ts:1-19`

- [ ] **Step 1: 更新 ProjectGroup 和 AppConfig 类型**

```typescript
// === 配置持久化 ===

export type ProjectTreeItem = string | ProjectGroup;

export interface ProjectGroup {
  id: string;
  name: string;
  collapsed: boolean;
  children: ProjectTreeItem[];
}

export interface AppConfig {
  projects: ProjectConfig[];
  projectTree?: ProjectTreeItem[];
  // 旧字段仅用于迁移兼容（Rust 端处理后不再出现）
  projectGroups?: { id: string; name: string; collapsed: boolean; projectIds: string[] }[];
  projectOrdering?: string[];
  defaultShell: string;
  availableShells: ShellConfig[];
  uiFontSize: number;
  terminalFontSize: number;
  layoutSizes?: number[];
}
```

- [ ] **Step 2: 确认编译通过**

Run: `npm run build 2>&1 | head -30`
Expected: 会有编译错误（store.ts 引用旧字段），这是预期的，Task 4 修复。

- [ ] **Step 3: 提交**

```bash
git add src/types.ts
git commit -m "feat: 前端类型定义支持嵌套组 ProjectTreeItem"
```

---

### Task 3: 树操作工具函数

**Files:**
- Create: `src/utils/projectTree.ts`

- [ ] **Step 1: 实现核心工具函数**

创建 `src/utils/projectTree.ts`，包含以下纯函数：

```typescript
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
  // 不能拖到自己里
  if (draggedId === targetGroupId) return false;
  // 循环检测：目标不能是被拖拽项的后代
  if (isGroup(draggedItem) && isDescendant(tree, draggedId, targetGroupId)) return false;
  // 深度检查
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
  if (!isGroup(draggedItem)) return true; // 项目放到任何位置都合法
  const parentId = findParentGroupId(tree, targetId);
  if (parentId === null) {
    // 放到顶层，检查深度
    return getSubtreeMaxDepth(draggedItem) <= MAX_DEPTH;
  }
  // 放到某个组的同级
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

/** 递归展平树为带 depth 和 parentGroupId 的有序列表 */
export function getOrderedTree(config: AppConfig): OrderedItem[] {
  const projectMap = new Map(config.projects.map((p) => [p.id, p]));
  const result: OrderedItem[] = [];

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
          result.push({ type: 'project', project, depth, parentGroupId });
        }
      }
    }
  }

  const tree = config.projectTree ?? [];
  walk(tree, 0, null);

  // 追加不在 tree 中的项目到顶层
  const inTree = new Set<string>();
  function collectIds(items: ProjectTreeItem[]) {
    for (const item of items) {
      if (isGroup(item)) {
        collectIds(item.children);
      } else {
        inTree.add(item);
      }
    }
  }
  collectIds(tree);
  for (const p of config.projects) {
    if (!inTree.has(p.id)) {
      result.push({ type: 'project', project: p, depth: 0, parentGroupId: null });
    }
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
    if (getItemId(item) === targetId) return null; // 在顶层
    if (isGroup(item)) {
      for (const child of item.children) {
        if (getItemId(child) === targetId) return item.id; // 直接子项
      }
      // 递归搜索子树，但注意：如果子树中找到了，递归会返回正确的父组 ID
      const found = findParentGroupIdInner(item.children, targetId);
      if (found !== null) return found;
    }
  }
  return null;
}

/** 内部递归：仅在嵌套层中搜索，不返回 null 表示"在顶层" */
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
  // 追加遗漏的项目
  for (const p of projects) {
    if (!seen.has(p.id)) tree.push(p.id);
  }
  return tree;
}
```

- [ ] **Step 2: 确认文件无语法错误**

Run: `npx tsc --noEmit 2>&1 | head -30`
Expected: 可能有 store.ts / ProjectList.tsx 的错误（尚未更新），但 projectTree.ts 本身应无类型错误。

- [ ] **Step 3: 提交**

```bash
git add src/utils/projectTree.ts
git commit -m "feat: 新增树操作工具函数库 projectTree.ts"
```

---

### Task 4: Store 重构

**Files:**
- Modify: `src/store.ts:1-562`

- [ ] **Step 1: 更新 import 和移除旧代码**

更新 import，从 types 中移除 `ProjectGroup` 导入（如果有独立导入），新增导入：

```typescript
import type {
  AppConfig,
  ProjectConfig,
  ProjectTreeItem,
  ProjectGroup,
  ProjectState,
  TerminalTab,
  SplitNode,
  PaneStatus,
  SavedSplitNode,
  SavedTab,
  SavedProjectLayout,
} from './types';
import {
  isGroup,
  getItemId,
  deepCloneTree,
  removeFromTree,
  insertIntoTree,
  updateGroupInTree,
  removeGroupAndPromoteChildren,
  removeProjectFromTree,
  migrateToTree,
} from './utils/projectTree';
```

删除旧的 `OrderedItem` 类型定义（第 243-246 行）、`getOrderedItems` 函数（第 249-286 行）和 `ensureOrdering` 函数（第 288-296 行）。

- [ ] **Step 2: 新增 ensureTree 辅助函数**

替代旧的 `ensureOrdering`：

```typescript
function ensureTree(config: AppConfig): AppConfig {
  if (config.projectTree && config.projectTree.length > 0) return config;
  // 尝试从旧格式迁移
  if (config.projectOrdering || config.projectGroups) {
    return { ...config, projectTree: migrateToTree(config), projectGroups: undefined, projectOrdering: undefined };
  }
  return { ...config, projectTree: config.projects.map((p) => p.id) };
}
```

- [ ] **Step 3: 更新 AppStore 接口**

替换分组相关 actions：

```typescript
interface AppStore {
  config: AppConfig;
  setConfig: (config: AppConfig) => void;

  activeProjectId: string | null;
  projectStates: Map<string, ProjectState>;
  setActiveProject: (id: string) => void;
  addProject: (project: ProjectConfig) => void;
  removeProject: (id: string) => void;

  addTab: (projectId: string, tab: TerminalTab) => void;
  removeTab: (projectId: string, tabId: string) => void;
  setActiveTab: (projectId: string, tabId: string) => void;
  updateTabLayout: (projectId: string, tabId: string, layout: SplitNode) => void;
  updatePaneStatusByPty: (ptyId: number, status: PaneStatus) => void;

  // 嵌套组操作
  createGroup: (name: string, parentGroupId?: string) => void;
  removeGroup: (groupId: string) => void;
  renameGroup: (groupId: string, name: string) => void;
  toggleGroupCollapse: (groupId: string) => void;
  moveItem: (itemId: string, targetGroupId: string | null, index?: number) => void;
}
```

- [ ] **Step 4: 重写 addProject action**

```typescript
addProject: (project) =>
  set((state) => {
    const config = ensureTree(state.config);
    const newTree = [...(config.projectTree ?? []), project.id];
    const newConfig = {
      ...config,
      projects: [...config.projects, project],
      projectTree: newTree,
    };
    const newStates = new Map(state.projectStates);
    newStates.set(project.id, { id: project.id, tabs: [], activeTabId: '' });
    return {
      config: newConfig,
      projectStates: newStates,
      activeProjectId: state.activeProjectId ?? project.id,
    };
  }),
```

- [ ] **Step 5: 重写 removeProject action**

```typescript
removeProject: (id) =>
  set((state) => {
    expandedDirsMap.delete(id);
    const timer = saveExpandedTimers.get(id);
    if (timer) { clearTimeout(timer); saveExpandedTimers.delete(id); }

    const newTree = deepCloneTree(state.config.projectTree ?? []);
    removeProjectFromTree(newTree, id);
    const newConfig = {
      ...state.config,
      projects: state.config.projects.filter((p) => p.id !== id),
      projectTree: newTree,
    };
    const newStates = new Map(state.projectStates);
    newStates.delete(id);
    const newActive =
      state.activeProjectId === id
        ? newConfig.projects[0]?.id ?? null
        : state.activeProjectId;
    return { config: newConfig, projectStates: newStates, activeProjectId: newActive };
  }),
```

- [ ] **Step 6: 重写分组 actions**

```typescript
createGroup: (name, parentGroupId) =>
  set((state) => {
    const config = ensureTree(state.config);
    const group: ProjectGroup = { id: genId(), name, collapsed: false, children: [] };
    const newTree = deepCloneTree(config.projectTree ?? []);
    insertIntoTree(newTree, parentGroupId ?? null, group);
    return { config: { ...config, projectTree: newTree } };
  }),

removeGroup: (groupId) =>
  set((state) => {
    const newTree = deepCloneTree(state.config.projectTree ?? []);
    removeGroupAndPromoteChildren(newTree, groupId);
    return { config: { ...state.config, projectTree: newTree } };
  }),

renameGroup: (groupId, name) =>
  set((state) => {
    const newTree = deepCloneTree(state.config.projectTree ?? []);
    updateGroupInTree(newTree, groupId, (g) => ({ ...g, name }));
    return { config: { ...state.config, projectTree: newTree } };
  }),

toggleGroupCollapse: (groupId) =>
  set((state) => {
    const newTree = deepCloneTree(state.config.projectTree ?? []);
    updateGroupInTree(newTree, groupId, (g) => ({ ...g, collapsed: !g.collapsed }));
    return { config: { ...state.config, projectTree: newTree } };
  }),

moveItem: (itemId, targetGroupId, index) =>
  set((state) => {
    const config = ensureTree(state.config);
    const newTree = deepCloneTree(config.projectTree ?? []);
    const removed = removeFromTree(newTree, itemId);
    if (!removed) return state;
    insertIntoTree(newTree, targetGroupId, removed, index);
    return { config: { ...config, projectTree: newTree } };
  }),
```

注意：`removeFromTree` 和 `insertIntoTree` 是就地修改。所有 store action 在修改前先通过 `deepCloneTree` 深拷贝整棵树，确保不会修改 Zustand 中的现有 state 引用。对于项目树这种小数据量，深拷贝性能完全可以忽略。

- [ ] **Step 7: 确认编译**

Run: `npm run build 2>&1 | head -30`
Expected: 可能在 ProjectList.tsx 报错（引用了旧 API），这是预期的，Task 5 修复。store.ts 本身应无错误。

- [ ] **Step 8: 提交**

```bash
git add src/store.ts
git commit -m "refactor: store 从扁平分组迁移到树操作模型"
```

---

### Task 5: ProjectList 组件重写

**Files:**
- Modify: `src/components/ProjectList.tsx`

这是最大的改动。分步骤进行。

- [ ] **Step 1: 更新 import 和 store 引用**

```typescript
import { useAppStore, genId } from '../store';
import {
  getOrderedTree,
  collectAllGroups,
  countProjectsInGroup,
  canDrop,
  canDropAt,
  getSubtreeMaxDepth,
  getDepth,
  isGroup,
  findParentGroupId,
  findGroupInTree,
  MAX_DEPTH,
} from '../utils/projectTree';
import type { OrderedItem } from '../utils/projectTree';
import type { ProjectTreeItem } from '../types';
```

移除 `getOrderedItems` 的导入。更新 store selectors：

```typescript
const createGroup = useAppStore((s) => s.createGroup);
const removeGroup = useAppStore((s) => s.removeGroup);
const renameGroup = useAppStore((s) => s.renameGroup);
const toggleGroupCollapse = useAppStore((s) => s.toggleGroupCollapse);
const moveItem = useAppStore((s) => s.moveItem);
```

移除 `moveProjectToGroup`、`moveProjectOutOfGroup`、`reorderItems` 的引用。

更新 `renderDropLine` 在 `forbidden` 时不显示：

```typescript
const renderDropLine = (id: string, position: 'before' | 'after') => {
  if (dropIndicator?.id !== id || dropIndicator.position !== position) return null;
  if (dropIndicator.forbidden) return null;
  return (
    <div className="absolute left-1 right-1 h-0.5 bg-[var(--accent)] rounded-full z-10"
      style={position === 'before' ? { top: -1 } : { bottom: -1 }} />
  );
};
```

更新 `orderedItems`：
```typescript
const orderedItems = getOrderedTree(config);
```

移除 `const groups = config.projectGroups ?? [];` 这一行，改为从树中收集：
```typescript
const allGroups = collectAllGroups(config.projectTree ?? []);
```

- [ ] **Step 2: 重写拖拽处理**

**handleGroupDragStart** — 缓存子树深度：

```typescript
const handleGroupDragStart = useCallback((e: React.DragEvent, groupId: string) => {
  e.dataTransfer.setData('application/group-id', groupId);
  e.dataTransfer.effectAllowed = 'move';
  setDragPayload({ type: 'group', groupId });
  requestAnimationFrame(() => {
    (e.target as HTMLElement).style.opacity = '0.4';
  });
}, []);
```

**handleDragOver** — 增加深度校验和禁止样式：

重写 `handleDragOver`，当 `allowInside` 为 true 且位置是 `inside` 时，检查 `canDrop`。如果不合法，设置 `dropIndicator` 带一个新的 `forbidden` 标记。

更新 `DropIndicator` 类型：

```typescript
interface DropIndicator {
  id: string;
  position: 'before' | 'after' | 'inside';
  forbidden?: boolean;
}
```

在 `handleDragOver` 中：

```typescript
const handleDragOver = useCallback((e: React.DragEvent, targetId: string, allowInside: boolean) => {
  const payload = getDragPayload();
  if (!payload || payload.type === 'tab') return;
  if (
    (payload.type === 'project' && payload.projectId === targetId) ||
    (payload.type === 'group' && payload.groupId === targetId)
  ) return;
  e.preventDefault();

  const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
  const y = e.clientY - rect.top;
  const ratio = y / rect.height;

  let position: DropIndicator['position'];
  if (allowInside && ratio > 0.25 && ratio < 0.75) {
    position = 'inside';
  } else if (ratio < 0.5) {
    position = 'before';
  } else {
    position = 'after';
  }

  let forbidden = false;
  const tree = useAppStore.getState().config.projectTree ?? [];

  if (position === 'inside') {
    // 放入组内校验
    if (payload.type === 'project') {
      forbidden = !canDrop(tree, targetId, payload.projectId);
    } else {
      const draggedGroup = findGroupInTree(tree, payload.groupId);
      if (draggedGroup) {
        forbidden = !canDrop(tree, targetId, draggedGroup);
      }
    }
  } else if (payload.type === 'group') {
    // 放到旁边（before/after）也需要校验组的深度
    const draggedGroup = findGroupInTree(tree, payload.groupId);
    if (draggedGroup) {
      forbidden = !canDropAt(tree, targetId, draggedGroup);
    }
  }

  e.dataTransfer.dropEffect = forbidden ? 'none' : 'move';
  setDropIndicator({ id: targetId, position, forbidden });
}, []);
```

`findGroupInTree` 已在 `projectTree.ts` 中导出，直接使用。

**handleDrop** — 简化为统一的 `moveItem` 调用：

```typescript
const handleDrop = useCallback((e: React.DragEvent, targetId: string, targetContext?: { groupId?: string; indexInParent?: number }) => {
  e.preventDefault();
  const payload = getDragPayload();
  if (!payload || payload.type === 'tab') return;
  const indicator = dropIndicator;
  setDropIndicator(null);
  setDragPayload(null);
  (e.target as HTMLElement).style.opacity = '';

  if (!indicator || indicator.forbidden) return;

  const itemId = payload.type === 'project' ? payload.projectId : payload.groupId;

  if (indicator.position === 'inside') {
    // 拖入组内
    moveItem(itemId, targetId);
  } else {
    // 拖到目标旁边
    const tree = useAppStore.getState().config.projectTree ?? [];
    const parentGroupId = findParentGroupId(tree, targetId);
    // 计算目标在父级中的索引
    const parent = parentGroupId
      ? findGroupInTree(tree, parentGroupId)?.children ?? []
      : tree;
    const targetIdx = parent.findIndex((item) =>
      (typeof item === 'string' ? item : item.id) === targetId
    );
    let insertIdx = indicator.position === 'after' ? targetIdx + 1 : targetIdx;
    // 修正 off-by-one：如果被拖拽项在同一父级且在目标之前，
    // moveItem 内部 removeFromTree 后索引会下移 1
    const draggedIdx = parent.findIndex((item) =>
      (typeof item === 'string' ? item : item.id) === itemId
    );
    if (draggedIdx >= 0 && draggedIdx < insertIdx) {
      insertIdx--;
    }
    moveItem(itemId, parentGroupId ?? null, insertIdx);
  }
  saveConfig();
}, [dropIndicator, moveItem]);
```

- [ ] **Step 3: 重写渲染逻辑**

**renderProjectItem** — 增加 `depth` 参数：

```typescript
const renderProjectItem = (project: ProjectConfig, depth: number, parentGroupId?: string) => {
  // ...  
  // 缩进：paddingLeft 根据 depth
  // className 中移除旧的 `groupId ? 'pl-5' : ''`
  // 改为 style={{ paddingLeft: `${depth * 16 + 10}px` }}
```

右键菜单中"移动到分组"需要从 `allGroups` 中过滤，排除自身所在的组、以及放入后会超深度的组：

```typescript
// 右键菜单分组操作
const tree = config.projectTree ?? [];
if (allGroups.length > 0) {
  menuItems.push({ separator: true });
  if (parentGroupId) {
    menuItems.push({
      label: '移出分组',
      onClick: () => { moveItem(project.id, null); saveConfig(); },
    });
  }
  for (const [g, gDepth] of allGroups) {
    if (g.id === parentGroupId) continue;
    // 项目放入组内：gDepth + 1 + 0 <= MAX_DEPTH
    if (gDepth + 1 > MAX_DEPTH) continue;
    menuItems.push({
      label: `移动到「${g.name}」`,
      onClick: () => { moveItem(project.id, g.id); saveConfig(); },
    });
  }
}
```

**renderGroup** — 增加 `depth` 参数，显示项目计数：

```typescript
const renderGroup = (group: ProjectGroup, depth: number) => {
  const isEditing = editingGroupId === group.id;
  const isInsideTarget = dropIndicator?.id === group.id && dropIndicator.position === 'inside';
  const isForbidden = isInsideTarget && dropIndicator?.forbidden;
  const projectCount = countProjectsInGroup(group);

  return (
    <div key={group.id} className="relative" style={{ paddingLeft: `${depth * 16}px` }}>
      {renderDropLine(group.id, 'before')}
      <div
        className={`flex items-center gap-1.5 px-2.5 py-1.5 rounded-[var(--radius-sm)] cursor-pointer text-sm transition-all duration-150 select-none ${
          isForbidden
            ? 'border border-dashed border-[var(--color-error)] cursor-not-allowed'
            : isInsideTarget
              ? 'bg-[var(--accent-subtle)] border border-dashed border-[var(--accent)]'
              : 'text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
        }`}
        draggable={!isEditing}
        onDragStart={(e) => handleGroupDragStart(e, group.id)}
        onDragEnd={handleDragEnd}
        onDragOver={(e) => handleDragOver(e, group.id, true)}
        onDragLeave={handleDragLeave}
        onDrop={(e) => handleDrop(e, group.id)}
        onClick={() => { if (!isEditing) toggleGroupCollapse(group.id); saveConfig(); }}
        onContextMenu={(e) => {
          e.preventDefault();
          e.stopPropagation();
          const tree = config.projectTree ?? [];
          const groupDepth = getDepth(tree, group.id);
          const items: Parameters<typeof showContextMenu>[2] = [
            { label: '重命名分组', onClick: () => startRenameGroup(group.id, group.name) },
          ];
          // 仅当深度 < MAX_DEPTH 时显示"新建子组"
          if (groupDepth >= 0 && groupDepth < MAX_DEPTH - 1) {
            items.push({
              label: '新建子组',
              onClick: async () => {
                const name = await showPrompt('新建子组', '请输入子组名称');
                if (!name?.trim()) return;
                createGroup(name.trim(), group.id);
                saveConfig();
              },
            });
          }
          items.push({ label: '删除分组（保留项目）', danger: true, onClick: () => { removeGroup(group.id); saveConfig(); } });
          showContextMenu(e.clientX, e.clientY, items);
        }}
      >
        {/* ... 折叠箭头、名称、计数 */}
        <span className="text-xs text-[var(--text-muted)]">({projectCount})</span>
      </div>
      {renderDropLine(group.id, 'after')}
    </div>
  );
};
```

**主渲染循环** — 使用 `getOrderedTree` 的扁平列表：

```typescript
<div className="flex-1 overflow-y-auto px-1.5 space-y-0.5">
  {orderedItems.map((item) =>
    item.type === 'project'
      ? renderProjectItem(item.project, item.depth, item.parentGroupId ?? undefined)
      : renderGroup(item.group, item.depth)
  )}
</div>
```

注意：
- `getOrderedTree` 返回的是扁平列表，每项带 `depth` 和 `parentGroupId`，组的子项已经展开在后续条目中（带更高的 depth），所以 `renderGroup` 不再需要内部递归渲染子项
- `renderProjectItem` 接收 `parentGroupId` 用于右键菜单"移出分组"功能
- `handleDragOver` 中 `allowInside` 条件不再限制 `payload.type === 'project'`（旧代码有此限制），因为嵌套组功能需要组也能拖入其他组

- [ ] **Step 4: 确认编译通过**

Run: `npm run build 2>&1 | head -30`
Expected: PASS（无错误）

- [ ] **Step 5: 提交**

```bash
git add src/components/ProjectList.tsx
git commit -m "feat: ProjectList 支持嵌套组渲染、深度校验和拖拽"
```

---

### Task 6: 集成测试与验收

**Files:**
- 无新文件

- [ ] **Step 1: 运行 Rust 测试**

Run: `cd src-tauri && cargo test`
Expected: 所有测试通过

- [ ] **Step 2: 运行前端构建**

Run: `npm run build`
Expected: 构建成功

- [ ] **Step 3: 手动验收检查清单**

启动 `npm run tauri dev`，验证以下场景：

1. **旧配置迁移**：已有分组的配置启动后，分组正常显示
2. **创建顶层组**：点击 "+" 按钮创建组
3. **创建子组**：右键组 → "新建子组"
4. **拖拽项目到组内**：项目拖入组中间区域
5. **拖拽项目到子组内**：项目拖入嵌套组
6. **拖拽组到组内**：组拖入另一个组
7. **深度限制**：尝试拖组到第 3 层组内，应显示红色禁止
8. **删除组**：子项释放到父级
9. **折叠/展开**：嵌套组的折叠状态正确
10. **缩进**：各层级缩进视觉正确
11. **右键菜单**：移动到分组列表正确过滤
12. **重启后状态保持**：关闭重开，嵌套结构和折叠状态不丢失

- [ ] **Step 4: 提交最终修复（如有）**

如果验收中发现问题，修复后统一提交。
