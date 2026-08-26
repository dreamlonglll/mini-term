//! 项目分组树的**纯函数层**。逐条对照 `src/utils/projectTree.ts`。
//!
//! # 为什么单独一个模块
//!
//! 那边 ~420 行、零外部依赖,是整个分组功能里最好测的部分:深度判定、环路防护、
//! 展平顺序全是纯计算。放进 `store.rs` 会被 `Context<AppStore>` 污染成只能在
//! gpui 环境里跑的代码,单测就写不动了。
//!
//! # 与 TS 侧的两处结构性差异
//!
//! 1. **不需要 `deepCloneTree`**。TS 那边所有树操作都就地改,而 zustand 的状态
//!    必须换新对象才触发订阅,于是每个 action 开头都得先深拷贝。Rust 侧直接
//!    `&mut Vec<ProjectTreeItem>` 改 config 本体,`cx.notify()` 单独通知 ——
//!    深拷贝纯属浪费,故意不移植。
//! 2. **`getDepth` 用 `Option<usize>` 而不是 `-1`**。语义一致,少一个哨兵值。
//!
//! 磁盘格式(`config.projectTree`)一个字都不改:这里只读写
//! [`mt_config::ProjectTreeItem`],它的 `#[serde(untagged)]` 布局与装机版互读互写。

use mt_config::{AppConfig, ProjectConfig, ProjectGroup, ProjectTreeItem};

/// 分组嵌套上限(含项目那一层)。与 `projectTree.ts:3` 同值。
pub const MAX_DEPTH: usize = 3;

// ─── 节点类型判断 ─────────────────────────────────────────────

pub fn item_id(item: &ProjectTreeItem) -> &str {
    match item {
        ProjectTreeItem::ProjectId(id) => id,
        ProjectTreeItem::Group(group) => &group.id,
    }
}

pub fn is_group(item: &ProjectTreeItem) -> bool {
    matches!(item, ProjectTreeItem::Group(_))
}

// ─── 树查询 ───────────────────────────────────────────────────

/// 节点在树中的深度(0 = 顶层)。找不到 = `None`(TS 侧的 `-1`)。
pub fn get_depth(tree: &[ProjectTreeItem], target_id: &str) -> Option<usize> {
    fn walk(items: &[ProjectTreeItem], target: &str, depth: usize) -> Option<usize> {
        for item in items {
            if item_id(item) == target {
                return Some(depth);
            }
            if let ProjectTreeItem::Group(group) = item
                && let Some(found) = walk(&group.children, target, depth + 1)
            {
                return Some(found);
            }
        }
        None
    }
    walk(tree, target_id, 0)
}

/// 子树占用的**额外**深度层数:项目 → 0、空组 → 0、含项目的组 → 1、含子组的组 → 2+。
pub fn get_subtree_max_depth(item: &ProjectTreeItem) -> usize {
    let ProjectTreeItem::Group(group) = item else {
        return 0;
    };
    if group.children.is_empty() {
        return 0;
    }
    group
        .children
        .iter()
        .map(get_subtree_max_depth)
        .max()
        .unwrap_or(0)
        + 1
}

/// `ancestor_id` 是不是 `target_id` 的祖先(拖组进自己的子孙 = 自环,必须挡)。
pub fn is_descendant(tree: &[ProjectTreeItem], ancestor_id: &str, target_id: &str) -> bool {
    let Some(ancestor) = find_group_in_tree(tree, ancestor_id) else {
        return false;
    };
    contains_id(&ancestor.children, target_id)
}

fn contains_id(items: &[ProjectTreeItem], id: &str) -> bool {
    items.iter().any(|item| {
        item_id(item) == id
            || match item {
                ProjectTreeItem::Group(group) => contains_id(&group.children, id),
                ProjectTreeItem::ProjectId(_) => false,
            }
    })
}

/// 落进组里(`inside`)合法吗:无自环 且 落地后深度不超 [`MAX_DEPTH`]。
pub fn can_drop(tree: &[ProjectTreeItem], target_group_id: &str, dragged: &ProjectTreeItem) -> bool {
    let dragged_id = item_id(dragged);
    if dragged_id == target_group_id {
        return false;
    }
    if is_group(dragged) && is_descendant(tree, dragged_id, target_group_id) {
        return false;
    }
    let Some(target_depth) = get_depth(tree, target_group_id) else {
        return false;
    };
    target_depth + 1 + get_subtree_max_depth(dragged) <= MAX_DEPTH
}

/// 落到目标**旁边**(`before` / `after`)合法吗 —— 项目怎么放都行;拖「组」要挡
/// 两件事:自环与超深。自环 = 目标行落在被拖分组自己的子树里(Inside 落点的
/// [`can_drop`] 有同一道检查):放行的话 [`move_item_in_tree`] 摘下分组后目标
/// 随子树一起消失,失败兜底救得回节点、救不回它「本该在哪」。
pub fn can_drop_at(tree: &[ProjectTreeItem], target_id: &str, dragged: &ProjectTreeItem) -> bool {
    if !is_group(dragged) {
        return true;
    }
    let dragged_id = item_id(dragged);
    if dragged_id == target_id || is_descendant(tree, dragged_id, target_id) {
        return false;
    }
    let Some(parent_id) = find_parent_group_id(tree, target_id) else {
        return get_subtree_max_depth(dragged) <= MAX_DEPTH;
    };
    let Some(parent_depth) = get_depth(tree, &parent_id) else {
        return true;
    };
    parent_depth + 1 + get_subtree_max_depth(dragged) <= MAX_DEPTH
}

/// 节点所在的父组 id。顶层 → `None`,找不到也 → `None`(与 TS 同口径)。
pub fn find_parent_group_id(tree: &[ProjectTreeItem], target_id: &str) -> Option<String> {
    fn walk(items: &[ProjectTreeItem], target: &str, parent: Option<&str>) -> Option<Option<String>> {
        for item in items {
            if item_id(item) == target {
                return Some(parent.map(str::to_string));
            }
            if let ProjectTreeItem::Group(group) = item
                && let Some(found) = walk(&group.children, target, Some(&group.id))
            {
                return Some(found);
            }
        }
        None
    }
    // 外层 `Option` = 找没找到,内层 = 父组是谁。两层一起压平正是 TS 的返回值。
    walk(tree, target_id, None).flatten()
}

pub fn find_group_in_tree<'a>(
    tree: &'a [ProjectTreeItem],
    group_id: &str,
) -> Option<&'a ProjectGroup> {
    for item in tree {
        if let ProjectTreeItem::Group(group) = item {
            if group.id == group_id {
                return Some(group);
            }
            if let Some(found) = find_group_in_tree(&group.children, group_id) {
                return Some(found);
            }
        }
    }
    None
}

/// [`find_group_in_tree`] 的可变版。TS 侧那个 `updateGroupInTree(tree, id, updater)`
/// 在 Rust 里退化成「拿到 `&mut` 自己改」,少一层闭包。
pub fn find_group_in_tree_mut<'a>(
    tree: &'a mut [ProjectTreeItem],
    group_id: &str,
) -> Option<&'a mut ProjectGroup> {
    for item in tree {
        if let ProjectTreeItem::Group(group) = item {
            if group.id == group_id {
                return Some(group);
            }
            if let Some(found) = find_group_in_tree_mut(&mut group.children, group_id) {
                return Some(found);
            }
        }
    }
    None
}

/// 组内总项目数,**含嵌套子组里的项目**(分组行尾那个 `(3)`)。
pub fn count_projects_in_group(group: &ProjectGroup) -> usize {
    group
        .children
        .iter()
        .map(|child| match child {
            ProjectTreeItem::Group(inner) => count_projects_in_group(inner),
            ProjectTreeItem::ProjectId(_) => 1,
        })
        .sum()
}

/// 节点在**它自己那一层**里的下标(before/after 落点要拿它算插入位)。
pub fn index_in_parent(
    tree: &[ProjectTreeItem],
    parent_group_id: Option<&str>,
    id: &str,
) -> Option<usize> {
    let siblings = match parent_group_id {
        None => tree,
        Some(gid) => &find_group_in_tree(tree, gid)?.children[..],
    };
    siblings.iter().position(|item| item_id(item) == id)
}

// ─── 树操作(就地改) ──────────────────────────────────────────

/// 摘掉一个节点并把它交出来。找不到 → `None`(调用方据此判「是不是子项目」)。
pub fn remove_from_tree(tree: &mut Vec<ProjectTreeItem>, id: &str) -> Option<ProjectTreeItem> {
    for i in 0..tree.len() {
        if item_id(&tree[i]) == id {
            return Some(tree.remove(i));
        }
        if let ProjectTreeItem::Group(group) = &mut tree[i]
            && let Some(found) = remove_from_tree(&mut group.children, id)
        {
            return Some(found);
        }
    }
    None
}

/// 插到指定组里(`None` = 根层)。`index` 缺省 = 追加到末尾,越界自动收到末尾。
/// 返回值 = 目标组找到没有。
pub fn insert_into_tree(
    tree: &mut Vec<ProjectTreeItem>,
    target_group_id: Option<&str>,
    item: ProjectTreeItem,
    index: Option<usize>,
) -> bool {
    let Some(group_id) = target_group_id else {
        let idx = index.unwrap_or(usize::MAX).min(tree.len());
        tree.insert(idx, item);
        return true;
    };
    match find_group_in_tree_mut(tree, group_id) {
        Some(group) => {
            let idx = index.unwrap_or(usize::MAX).min(group.children.len());
            group.children.insert(idx, item);
            true
        }
        None => false,
    }
}

/// 删组:**组员(含子组)原位晋升到父级**,一个都不删。
pub fn remove_group_and_promote_children(
    tree: &mut Vec<ProjectTreeItem>,
    group_id: &str,
) -> bool {
    for i in 0..tree.len() {
        if let ProjectTreeItem::Group(group) = &tree[i] {
            if group.id == group_id {
                let ProjectTreeItem::Group(group) = tree.remove(i) else {
                    unreachable!("刚判过是分组");
                };
                for (offset, child) in group.children.into_iter().enumerate() {
                    tree.insert(i + offset, child);
                }
                return true;
            }
        } else {
            continue;
        }
        let ProjectTreeItem::Group(group) = &mut tree[i] else {
            unreachable!("刚判过是分组");
        };
        if remove_group_and_promote_children(&mut group.children, group_id) {
            return true;
        }
    }
    false
}

/// `AppStore::move_item` 的树侧全部语义:把节点(项目或分组)移到
/// `target_group_id` 里的 `index` 位置。`None` 目标 = 根层;`index` 缺省 = 末尾。
/// 返回值 = 这次有没有真的动过树(或 `projects`)。
///
/// 三条边界:
/// - **自环先验**:目标组是被移动节点自己或其子孙时直接拒绝、原树不动。
///   必须在摘之前判 —— 摘下分组后目标随子树一起消失,只剩失败兜底可走,
///   而兜底只保得住节点、保不住位置。
/// - **树里找不到**(对照 `store.ts:1296-1313`):worktree 子项目(按设计不在
///   树里,位置由父项目派生)与树外孤儿项目([`get_ordered_tree`] 的收尾兜底
///   把它顶在根层显示的那种)都以裸 id 入树 —— 前者语义是「脱离父项目」,
///   后者是给已损坏的树一次自愈机会:孤儿看得见却移不动才是死路。
///   `projects` 里也没有这个 id → 什么都不做。
/// - **目标组找不到**(分组被并发删掉):退回根层末尾,且放回的必须是摘下来的
///   **原节点** —— 按 id 合成 `ProjectId` 的话,摘下来的是分组时组名与组员
///   就地蒸发,只留一个指向不存在项目的幽灵 id。
pub fn move_item_in_tree(
    tree: &mut Vec<ProjectTreeItem>,
    projects: &mut [ProjectConfig],
    item_id: &str,
    target_group_id: Option<&str>,
    index: Option<usize>,
) -> bool {
    if let Some(gid) = target_group_id
        && (gid == item_id || is_descendant(tree, item_id, gid))
    {
        return false;
    }
    let removed = match remove_from_tree(tree, item_id) {
        Some(item) => item,
        None => {
            let Some(project) = projects.iter_mut().find(|p| p.id == item_id) else {
                return false;
            };
            project.parent_project_id = None;
            ProjectTreeItem::ProjectId(item_id.to_string())
        }
    };
    let backup = removed.clone();
    if !insert_into_tree(tree, target_group_id, removed, index) {
        insert_into_tree(tree, None, backup, None);
    }
    true
}

// ─── 渲染展平 ─────────────────────────────────────────────────

/// 展平后的一行。TS 侧的 `OrderedItem` 直接携带 `ProjectConfig` / `ProjectGroup`
/// 引用,Rust 里那会把 `&AppConfig` 的生命周期钉进渲染闭包 —— 只带 id 与
/// 渲染必需的几个字段,项目本体由调用方按 id 取。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedItem {
    Project {
        id: String,
        depth: usize,
        parent_group_id: Option<String>,
        /// worktree 子项目(`parentProjectId` 有值),它不在树里、位置是派生的。
        is_child: bool,
    },
    Group {
        id: String,
        name: String,
        collapsed: bool,
        /// 递归含子组的项目数(行尾括号里那个数)。
        count: usize,
        depth: usize,
        parent_group_id: Option<String>,
    },
}

impl OrderedItem {
    /// 只给单测用:生产侧一律 `match` 出变体自己那几个字段。
    #[cfg(test)]
    fn id(&self) -> &str {
        match self {
            OrderedItem::Project { id, .. } | OrderedItem::Group { id, .. } => id,
        }
    }
}

/// 递归展平树为带 depth / parentGroupId 的有序列表。对照 `projectTree.ts:221-282`。
///
/// 三条不显然的规则,逐条抄自那边的注释:
/// - **折叠组不递归 children**(但它们仍算「在树里」,见下一条);
/// - worktree 子项目紧随父项目之后、depth + 1 注入,`pushed` 兼做环路保护;
/// - 收尾兜底判据必须是「在不在完整树里」而**不是**「有没有被 push 过」——
///   折叠组里的项目一个都没 push,拿 `pushed` 判会让它们统统跑到列表底部去。
pub fn get_ordered_tree(config: &AppConfig) -> Vec<OrderedItem> {
    let mut result: Vec<OrderedItem> = Vec::new();
    let mut pushed: Vec<String> = Vec::new();

    // parentProjectId → 子项目 id 列表(保持 config.projects 的原序)
    let children_by_parent: Vec<(String, Vec<String>)> = {
        let mut map: Vec<(String, Vec<String>)> = Vec::new();
        for p in &config.projects {
            let Some(parent) = p.parent_project_id.as_ref() else {
                continue;
            };
            match map.iter_mut().find(|(k, _)| k == parent) {
                Some((_, list)) => list.push(p.id.clone()),
                None => map.push((parent.clone(), vec![p.id.clone()])),
            }
        }
        map
    };

    fn push_project(
        id: &str,
        depth: usize,
        parent_group_id: Option<&str>,
        is_child: bool,
        children_by_parent: &[(String, Vec<String>)],
        pushed: &mut Vec<String>,
        result: &mut Vec<OrderedItem>,
    ) {
        if pushed.iter().any(|p| p == id) {
            return;
        }
        pushed.push(id.to_string());
        result.push(OrderedItem::Project {
            id: id.to_string(),
            depth,
            parent_group_id: parent_group_id.map(str::to_string),
            is_child,
        });
        let Some((_, children)) = children_by_parent.iter().find(|(k, _)| k == id) else {
            return;
        };
        for child in children {
            push_project(
                child,
                depth + 1,
                parent_group_id,
                true,
                children_by_parent,
                pushed,
                result,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        items: &[ProjectTreeItem],
        depth: usize,
        parent_group_id: Option<&str>,
        config: &AppConfig,
        children_by_parent: &[(String, Vec<String>)],
        pushed: &mut Vec<String>,
        result: &mut Vec<OrderedItem>,
    ) {
        for item in items {
            match item {
                ProjectTreeItem::Group(group) => {
                    result.push(OrderedItem::Group {
                        id: group.id.clone(),
                        name: group.name.clone(),
                        collapsed: group.collapsed,
                        count: count_projects_in_group(group),
                        depth,
                        parent_group_id: parent_group_id.map(str::to_string),
                    });
                    if !group.collapsed {
                        walk(
                            &group.children,
                            depth + 1,
                            Some(&group.id),
                            config,
                            children_by_parent,
                            pushed,
                            result,
                        );
                    }
                }
                ProjectTreeItem::ProjectId(id) => {
                    if config.projects.iter().any(|p| &p.id == id) {
                        push_project(
                            id,
                            depth,
                            parent_group_id,
                            false,
                            children_by_parent,
                            pushed,
                            result,
                        );
                    }
                }
            }
        }
    }

    let empty: Vec<ProjectTreeItem> = Vec::new();
    let tree = config.project_tree.as_ref().unwrap_or(&empty);
    walk(
        tree,
        0,
        None,
        config,
        &children_by_parent,
        &mut pushed,
        &mut result,
    );

    // 收尾兜底:既不在树里、父项目也不存在的项目追加到顶层,保证不凭空消失。
    let mut in_tree: Vec<String> = Vec::new();
    fn collect_ids(items: &[ProjectTreeItem], out: &mut Vec<String>) {
        for item in items {
            match item {
                ProjectTreeItem::Group(group) => collect_ids(&group.children, out),
                ProjectTreeItem::ProjectId(id) => out.push(id.clone()),
            }
        }
    }
    collect_ids(tree, &mut in_tree);

    for p in &config.projects {
        if pushed.iter().any(|id| id == &p.id) {
            continue;
        }
        if in_tree.iter().any(|id| id == &p.id) {
            continue; // 折叠组里的项目:在树中,只是视图上隐藏
        }
        if let Some(parent) = p.parent_project_id.as_ref()
            && config.projects.iter().any(|q| &q.id == parent)
        {
            continue;
        }
        push_project(
            &p.id,
            0,
            None,
            false,
            &children_by_parent,
            &mut pushed,
            &mut result,
        );
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use mt_config::ProjectConfig;

    fn proj(id: &str) -> ProjectTreeItem {
        ProjectTreeItem::ProjectId(id.to_string())
    }

    fn group(id: &str, children: Vec<ProjectTreeItem>) -> ProjectTreeItem {
        ProjectTreeItem::Group(ProjectGroup {
            id: id.to_string(),
            name: format!("组{id}"),
            collapsed: false,
            children,
        })
    }

    /// 把树画成一行文本,方便断言(`ProjectTreeItem` 没有 `PartialEq`,
    /// 而给 mt-config 加 derive 会碰磁盘格式那一层 —— 不动它)。
    fn dump(tree: &[ProjectTreeItem]) -> String {
        tree.iter()
            .map(|item| match item {
                ProjectTreeItem::ProjectId(id) => id.clone(),
                ProjectTreeItem::Group(g) => {
                    format!("{}[{}]", g.id, dump(&g.children))
                }
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    fn project_cfg(id: &str, parent: Option<&str>) -> ProjectConfig {
        ProjectConfig {
            id: id.to_string(),
            name: id.to_string(),
            path: format!("/tmp/{id}"),
            description: None,
            saved_layout: None,
            expanded_dirs: Vec::new(),
            ssh_mcp_enabled: false,
            ssh_cli_token: None,
            ssh_connection_ids: None,
            env_vars: Vec::new(),
            wsl_sessions_distro: None,
            ssh_connection_id: None,
            parent_project_id: parent.map(str::to_string),
            kind_override: None,
        }
    }

    fn config_with(projects: Vec<ProjectConfig>, tree: Option<Vec<ProjectTreeItem>>) -> AppConfig {
        AppConfig {
            projects,
            project_tree: tree,
            ..AppConfig::default()
        }
    }

    // ─── 深度与子树 ───────────────────────────────────────────

    #[test]
    fn 深度按层数从零起算() {
        let tree = vec![proj("a"), group("g1", vec![proj("b"), group("g2", vec![proj("c")])])];
        assert_eq!(get_depth(&tree, "a"), Some(0));
        assert_eq!(get_depth(&tree, "g1"), Some(0));
        assert_eq!(get_depth(&tree, "b"), Some(1));
        assert_eq!(get_depth(&tree, "g2"), Some(1));
        assert_eq!(get_depth(&tree, "c"), Some(2));
        assert_eq!(get_depth(&tree, "查无此项"), None);
    }

    /// 项目 0、空组 0、含项目的组 1、含子组的组 2 —— 与 TS 注释逐字一致。
    #[test]
    fn 子树额外深度四档() {
        assert_eq!(get_subtree_max_depth(&proj("a")), 0);
        assert_eq!(get_subtree_max_depth(&group("g", vec![])), 0);
        assert_eq!(get_subtree_max_depth(&group("g", vec![proj("a")])), 1);
        assert_eq!(
            get_subtree_max_depth(&group("g", vec![group("h", vec![proj("a")])])),
            2
        );
    }

    // ─── 环路与深度防护 ───────────────────────────────────────

    #[test]
    fn 组不能落进自己的后代() {
        let tree = vec![group("g1", vec![group("g2", vec![])])];
        let dragged = tree[0].clone();
        assert!(is_descendant(&tree, "g1", "g2"));
        assert!(!can_drop(&tree, "g2", &dragged), "拖 g1 进 g2 = 自环");
        assert!(!can_drop(&tree, "g1", &dragged), "拖进自己也不行");
    }

    #[test]
    fn 落进组内不得超过最大深度() {
        // g1[g2[g3]] 已经用满三层:再往 g3 里放任何东西都超限
        let tree = vec![group("g1", vec![group("g2", vec![group("g3", vec![])])])];
        assert_eq!(get_depth(&tree, "g3"), Some(2));
        // 项目落进 g3 → 深度 3,正好 = MAX_DEPTH,合法
        assert!(can_drop(&tree, "g3", &proj("p")));
        // 空组落进 g3 → 2+1+0 = 3,合法
        assert!(can_drop(&tree, "g3", &group("新", vec![])));
        // 含项目的组落进 g3 → 2+1+1 = 4,超限
        assert!(!can_drop(&tree, "g3", &group("新", vec![proj("p")])));
    }

    #[test]
    fn 落到旁边只对组做深度判定() {
        let tree = vec![group("g1", vec![group("g2", vec![proj("p")])])];
        // 项目怎么放都合法
        assert!(can_drop_at(&tree, "g2", &proj("x")));
        // 「含子组、子组里还有项目」= 额外 2 层
        let 两层 = group("新", vec![group("内", vec![proj("x")])]);
        assert_eq!(get_subtree_max_depth(&两层), 2);
        // 放到 g2 旁边 → 父级 g1 深度 0,0+1+2 = 3,合法
        assert!(can_drop_at(&tree, "g2", &两层));
        // 同样的组放到 p 旁边 → 父级 g2 深度 1,1+1+2 = 4,超限
        assert!(!can_drop_at(&tree, "p", &两层));
        // 顶层旁边:只看子树自身层数,2 <= 3
        assert!(can_drop_at(&tree, "g1", &两层));
    }

    #[test]
    fn 目标不在树里一律拒绝落进去() {
        let tree = vec![proj("a")];
        assert!(!can_drop(&tree, "查无此组", &proj("a")));
    }

    // ─── 增删移 ───────────────────────────────────────────────

    #[test]
    fn 摘节点递归进分组() {
        let mut tree = vec![proj("a"), group("g1", vec![proj("b"), group("g2", vec![proj("c")])])];
        let removed = remove_from_tree(&mut tree, "c").expect("c 在 g2 里");
        assert_eq!(item_id(&removed), "c");
        assert_eq!(dump(&tree), "a,g1[b,g2[]]");
        assert!(remove_from_tree(&mut tree, "c").is_none(), "摘过就没了");
    }

    #[test]
    fn 摘掉分组会把子树一起带走() {
        let mut tree = vec![group("g1", vec![proj("a"), proj("b")]), proj("c")];
        let removed = remove_from_tree(&mut tree, "g1").expect("g1 在顶层");
        assert_eq!(get_subtree_max_depth(&removed), 1);
        assert_eq!(dump(&tree), "c");
    }

    #[test]
    fn 插入下标越界收到末尾() {
        let mut tree = vec![proj("a"), proj("b")];
        assert!(insert_into_tree(&mut tree, None, proj("c"), Some(99)));
        assert_eq!(dump(&tree), "a,b,c");
        assert!(insert_into_tree(&mut tree, None, proj("d"), Some(0)));
        assert_eq!(dump(&tree), "d,a,b,c");
        assert!(insert_into_tree(&mut tree, None, proj("e"), None));
        assert_eq!(dump(&tree), "d,a,b,c,e");
    }

    #[test]
    fn 插入找不到目标组时返回假且不改树() {
        let mut tree = vec![proj("a")];
        assert!(!insert_into_tree(&mut tree, Some("查无此组"), proj("b"), None));
        assert_eq!(dump(&tree), "a");
    }

    #[test]
    fn 插入递归进嵌套子组() {
        let mut tree = vec![group("g1", vec![group("g2", vec![])])];
        assert!(insert_into_tree(&mut tree, Some("g2"), proj("x"), None));
        assert_eq!(dump(&tree), "g1[g2[x]]");
    }

    /// 删组不删项目:组员**原位**晋升到父级,顺序不变。
    #[test]
    fn 删组组员原位晋升() {
        let mut tree = vec![proj("头"), group("g1", vec![proj("a"), group("g2", vec![proj("b")])]), proj("尾")];
        assert!(remove_group_and_promote_children(&mut tree, "g1"));
        assert_eq!(dump(&tree), "头,a,g2[b],尾");
    }

    #[test]
    fn 删嵌套子组也走同一条路() {
        let mut tree = vec![group("g1", vec![proj("a"), group("g2", vec![proj("b"), proj("c")])])];
        assert!(remove_group_and_promote_children(&mut tree, "g2"));
        assert_eq!(dump(&tree), "g1[a,b,c]");
        assert!(!remove_group_and_promote_children(&mut tree, "查无此组"));
    }

    #[test]
    fn 改名与折叠走可变查找() {
        let mut tree = vec![group("g1", vec![group("g2", vec![])])];
        let g2 = find_group_in_tree_mut(&mut tree, "g2").expect("在");
        g2.name = "新名".into();
        g2.collapsed = true;
        let g2 = find_group_in_tree(&tree, "g2").expect("在");
        assert_eq!(g2.name, "新名");
        assert!(g2.collapsed);
        assert!(find_group_in_tree_mut(&mut tree, "查无此组").is_none());
    }

    // ─── 计数 / 定位 ──────────────────────────────────────────

    #[test]
    fn 组内计数递归含子组() {
        let tree = vec![group(
            "g1",
            vec![proj("a"), group("g2", vec![proj("b"), proj("c")])],
        )];
        let g1 = find_group_in_tree(&tree, "g1").expect("在");
        assert_eq!(count_projects_in_group(g1), 3);
        let g2 = find_group_in_tree(&tree, "g2").expect("在");
        assert_eq!(count_projects_in_group(g2), 2);
    }

    #[test]
    fn 父组定位顶层为空() {
        let tree = vec![proj("a"), group("g1", vec![proj("b"), group("g2", vec![proj("c")])])];
        assert_eq!(find_parent_group_id(&tree, "a"), None);
        assert_eq!(find_parent_group_id(&tree, "g1"), None);
        assert_eq!(find_parent_group_id(&tree, "b").as_deref(), Some("g1"));
        assert_eq!(find_parent_group_id(&tree, "g2").as_deref(), Some("g1"));
        assert_eq!(find_parent_group_id(&tree, "c").as_deref(), Some("g2"));
        assert_eq!(find_parent_group_id(&tree, "查无此项"), None);
    }

    #[test]
    fn 同级下标按父组取() {
        let tree = vec![proj("a"), group("g1", vec![proj("b"), proj("c")])];
        assert_eq!(index_in_parent(&tree, None, "a"), Some(0));
        assert_eq!(index_in_parent(&tree, None, "g1"), Some(1));
        assert_eq!(index_in_parent(&tree, Some("g1"), "c"), Some(1));
        assert_eq!(index_in_parent(&tree, Some("g1"), "a"), None);
        assert_eq!(index_in_parent(&tree, Some("查无此组"), "a"), None);
    }

    // ─── 展平 ─────────────────────────────────────────────────

    #[test]
    fn 展平带深度与父组() {
        let config = config_with(
            vec![project_cfg("p1", None), project_cfg("p2", None)],
            Some(vec![proj("p1"), group("g1", vec![proj("p2")])]),
        );
        let ordered = get_ordered_tree(&config);
        assert_eq!(
            ordered,
            vec![
                OrderedItem::Project {
                    id: "p1".into(),
                    depth: 0,
                    parent_group_id: None,
                    is_child: false,
                },
                OrderedItem::Group {
                    id: "g1".into(),
                    name: "组g1".into(),
                    collapsed: false,
                    count: 1,
                    depth: 0,
                    parent_group_id: None,
                },
                OrderedItem::Project {
                    id: "p2".into(),
                    depth: 1,
                    parent_group_id: Some("g1".into()),
                    is_child: false,
                },
            ]
        );
    }

    /// 折叠组不递归 children,但组里的项目**不能**被兜底追加到底部。
    #[test]
    fn 折叠组里的项目不跑到列表底部() {
        let mut tree = vec![group("g1", vec![proj("p1")]), proj("p2")];
        let ProjectTreeItem::Group(g) = &mut tree[0] else {
            unreachable!()
        };
        g.collapsed = true;
        let config = config_with(
            vec![project_cfg("p1", None), project_cfg("p2", None)],
            Some(tree),
        );
        let ordered = get_ordered_tree(&config);
        let ids: Vec<&str> = ordered.iter().map(|item| item.id()).collect();
        assert_eq!(ids, vec!["g1", "p2"], "p1 藏在折叠组里,不该冒到底部");
    }

    /// worktree 子项目紧随父项目、深度 +1,且**不进树**。
    #[test]
    fn 子项目紧随父项目注入() {
        let config = config_with(
            vec![
                project_cfg("p1", None),
                project_cfg("wt", Some("p1")),
                project_cfg("p2", None),
            ],
            Some(vec![proj("p1"), proj("p2")]),
        );
        let ordered = get_ordered_tree(&config);
        assert_eq!(
            ordered
                .iter()
                .map(|i| i.id())
                .collect::<Vec<_>>(),
            vec!["p1", "wt", "p2"]
        );
        assert_eq!(
            ordered[1],
            OrderedItem::Project {
                id: "wt".into(),
                depth: 1,
                parent_group_id: None,
                is_child: true,
            }
        );
    }

    /// 组里的父项目:子项目跟着进组,`parentGroupId` 继承父项目那一份。
    #[test]
    fn 组内父项目的子项目继承父组() {
        let config = config_with(
            vec![project_cfg("p1", None), project_cfg("wt", Some("p1"))],
            Some(vec![group("g1", vec![proj("p1")])]),
        );
        let ordered = get_ordered_tree(&config);
        assert_eq!(
            ordered[2],
            OrderedItem::Project {
                id: "wt".into(),
                depth: 2,
                parent_group_id: Some("g1".into()),
                is_child: true,
            }
        );
    }

    /// `parentProjectId` 互指的坏配置不能让展平无限递归。
    #[test]
    fn 父子互指不死循环() {
        let config = config_with(
            vec![project_cfg("a", Some("b")), project_cfg("b", Some("a"))],
            Some(Vec::new()),
        );
        let ordered = get_ordered_tree(&config);
        // 两个都有"存活的父项目",兜底一个都不追加 —— 关键是**跑得完**
        assert!(ordered.len() <= 2);
    }

    /// 父项目丢了的孤儿子项目回到顶层,不凭空消失。
    #[test]
    fn 孤儿子项目兜底回顶层() {
        let config = config_with(
            vec![project_cfg("wt", Some("已删除的父项目"))],
            Some(Vec::new()),
        );
        let ordered = get_ordered_tree(&config);
        assert_eq!(
            ordered,
            vec![OrderedItem::Project {
                id: "wt".into(),
                depth: 0,
                parent_group_id: None,
                is_child: false,
            }]
        );
    }

    /// 树里有、projects 里没有的僵尸 id 直接跳过(不渲染空行)。
    #[test]
    fn 树里的僵尸_id_跳过() {
        let config = config_with(
            vec![project_cfg("p1", None)],
            Some(vec![proj("已删除"), proj("p1")]),
        );
        let ordered = get_ordered_tree(&config);
        let ids: Vec<&str> = ordered.iter().map(|i| i.id()).collect();
        assert_eq!(ids, vec!["p1"]);
    }

    // ─── store 五个 action 的组合语义 ─────────────────────────
    //
    // `AppStore::{create_group,remove_group,rename_group,toggle_group_collapse,
    // move_item}` 是「本模块的函数 + save/notify」两步,拿不到 `Context<AppStore>`
    // 的地方就直接测那一步组合 —— 语义全在这儿,store 那层只是搬运。

    /// `create_group`:新组一律**追加到末尾**,父组找不到就静默丢弃(不改树)。
    #[test]
    fn 建组追加末尾且父组缺失时丢弃() {
        let mut tree = vec![proj("a")];
        assert!(insert_into_tree(&mut tree, None, group("g1", vec![]), None));
        assert_eq!(dump(&tree), "a,g1[]");
        assert!(insert_into_tree(&mut tree, Some("g1"), group("g2", vec![]), None));
        assert_eq!(dump(&tree), "a,g1[g2[]]");
        // 父组不存在 → 返回 false,树原样
        assert!(!insert_into_tree(&mut tree, Some("没这组"), group("g3", vec![]), None));
        assert_eq!(dump(&tree), "a,g1[g2[]]");
    }

    /// `move_item` 的项目入组:先摘后插,原位置不留残影。
    #[test]
    fn 项目入组() {
        let mut tree = vec![proj("p1"), proj("p2"), group("g1", vec![])];
        let item = remove_from_tree(&mut tree, "p1").expect("p1 在顶层");
        assert!(insert_into_tree(&mut tree, Some("g1"), item, None));
        assert_eq!(dump(&tree), "p2,g1[p1]");
    }

    /// 「移出分组」= `moveItem(id, null)`:插到**根层末尾**。
    #[test]
    fn 项目出组到根层末尾() {
        let mut tree = vec![group("g1", vec![proj("p1")]), proj("p2")];
        let item = remove_from_tree(&mut tree, "p1").expect("p1 在 g1 里");
        assert!(insert_into_tree(&mut tree, None, item, None));
        assert_eq!(dump(&tree), "g1[],p2,p1");
    }

    /// 同级重排:先删后插的位移补偿(`crate::dnd::insert_index`)必须真的补上,
    /// 否则「往下拖一格」会变成原地不动。
    #[test]
    fn 同级重排补偿位移() {
        let mut tree = vec![proj("a"), proj("b"), proj("c")];
        // 把 a 拖到 b 之后
        let target_idx = index_in_parent(&tree, None, "b").expect("b 在");
        let dragged_idx = index_in_parent(&tree, None, "a");
        let idx = crate::dnd::insert_index(target_idx, dragged_idx, true);
        let item = remove_from_tree(&mut tree, "a").expect("a 在");
        assert!(insert_into_tree(&mut tree, None, item, Some(idx)));
        assert_eq!(dump(&tree), "b,a,c");
    }

    /// 反向(往上拖)不补偿。
    #[test]
    fn 反向重排不补偿() {
        let mut tree = vec![proj("a"), proj("b"), proj("c")];
        // 把 c 拖到 a 之前
        let target_idx = index_in_parent(&tree, None, "a").expect("a 在");
        let dragged_idx = index_in_parent(&tree, None, "c");
        let idx = crate::dnd::insert_index(target_idx, dragged_idx, false);
        let item = remove_from_tree(&mut tree, "c").expect("c 在");
        assert!(insert_into_tree(&mut tree, None, item, Some(idx)));
        assert_eq!(dump(&tree), "c,a,b");
    }

    /// 拖一整个分组进另一个分组:子树跟着走,顺序不乱。
    #[test]
    fn 分组带子树整体入组() {
        let mut tree = vec![
            group("g1", vec![proj("p1"), proj("p2")]),
            group("g2", vec![]),
        ];
        assert!(can_drop(&tree, "g2", &tree[0].clone()));
        let item = remove_from_tree(&mut tree, "g1").expect("g1 在");
        assert!(insert_into_tree(&mut tree, Some("g2"), item, None));
        assert_eq!(dump(&tree), "g2[g1[p1,p2]]");
    }

    /// `can_drop_at` 的自环:组不能落到**自己子树里任何一行**的旁边。
    /// 这里放行过一次真实事故:摘下组后目标随子树消失,整组被兜底降格毁掉。
    #[test]
    fn 组不能落到自己子树成员旁边() {
        let tree = vec![
            group("g1", vec![proj("p1"), group("g2", vec![proj("p2")])]),
            proj("外"),
        ];
        let dragged = tree[0].clone();
        assert!(!can_drop_at(&tree, "p1", &dragged), "直接子项目旁");
        assert!(!can_drop_at(&tree, "g2", &dragged), "子组旁");
        assert!(!can_drop_at(&tree, "p2", &dragged), "孙辈旁");
        assert!(!can_drop_at(&tree, "g1", &dragged), "自己旁边");
        assert!(can_drop_at(&tree, "外", &dragged), "子树之外照常");
    }

    /// `move_item_in_tree` 的自环先验:目标是自己或子孙 → 拒绝且原树一动不动。
    #[test]
    fn 移动目标为自己或子孙时原树不动() {
        let mut tree = vec![group("g1", vec![proj("p1"), group("g2", vec![])])];
        let mut projects = vec![project_cfg("p1", None)];
        assert!(!move_item_in_tree(&mut tree, &mut projects, "g1", Some("g1"), None));
        assert!(!move_item_in_tree(&mut tree, &mut projects, "g1", Some("g2"), None));
        assert_eq!(dump(&tree), "g1[p1,g2[]]");
    }

    /// 目标组被并发删掉:整棵**原样**退回根层末尾,组名与组员一个不丢。
    #[test]
    fn 目标组消失时分组原样退回根层() {
        let mut tree = vec![group("g1", vec![proj("p1"), proj("p2")]), proj("外")];
        let mut projects: Vec<ProjectConfig> = Vec::new();
        assert!(move_item_in_tree(&mut tree, &mut projects, "g1", Some("查无此组"), None));
        assert_eq!(dump(&tree), "外,g1[p1,p2]");
    }

    /// 树外孤儿项目(渲染兜底顶在根层显示的那种)也能移进组 —— 坏数据一次拖动自愈;
    /// 项目表里也没有的 id(如毁组事故残留的幽灵 id)仍然什么都不做。
    #[test]
    fn 树外孤儿项目移动时收编入组() {
        let mut tree = vec![group("g1", vec![])];
        let mut projects = vec![project_cfg("孤儿", None)];
        assert!(move_item_in_tree(&mut tree, &mut projects, "孤儿", Some("g1"), None));
        assert_eq!(dump(&tree), "g1[孤儿]");
        assert!(!move_item_in_tree(&mut tree, &mut projects, "查无此项", None, None));
        assert_eq!(dump(&tree), "g1[孤儿]");
    }

    /// worktree 子项目移动 = 脱离父项目:清 `parentProjectId` 并以裸 id 入树。
    #[test]
    fn 子项目移动即脱离父项目() {
        let mut tree = vec![proj("父"), group("g1", vec![])];
        let mut projects = vec![project_cfg("父", None), project_cfg("子", Some("父"))];
        assert!(move_item_in_tree(&mut tree, &mut projects, "子", Some("g1"), None));
        assert_eq!(dump(&tree), "父,g1[子]");
        assert_eq!(projects[1].parent_project_id, None);
    }

    // ─── 磁盘格式 ─────────────────────────────────────────────

    /// **红线**:本模块造出来的树,序列化后必须与装机版(Tauri)的
    /// `config.projectTree` 逐字同形 —— 项目是裸字符串、分组是 camelCase 对象。
    /// `#[serde(untagged)]` 的 variant 顺序换一下这里就红。
    #[test]
    fn 树的磁盘形态与装机版一致() {
        let mut tree: Vec<ProjectTreeItem> = Vec::new();
        insert_into_tree(&mut tree, None, proj("proj-1"), None);
        insert_into_tree(
            &mut tree,
            None,
            ProjectTreeItem::Group(ProjectGroup {
                id: "group-1".into(),
                name: "工作".into(),
                collapsed: false,
                children: Vec::new(),
            }),
            None,
        );
        insert_into_tree(&mut tree, Some("group-1"), proj("proj-2"), None);
        let json = serde_json::to_string(&tree).expect("序列化");
        assert_eq!(
            json,
            r#"["proj-1",{"id":"group-1","name":"工作","collapsed":false,"children":["proj-2"]}]"#
        );
        // 反过来也读得回来(装机版写的那份进得来)
        let back: Vec<ProjectTreeItem> = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(dump(&back), "proj-1,group-1[proj-2]");
    }
}
