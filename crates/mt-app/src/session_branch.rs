//! 会话分支树的**纯逻辑**层。对应 `src/utils/sessionBranch.ts`。
//!
//! 平铺会话列表 + 分支边 → 森林 → 带连线前缀的行。不碰 gpui、不碰磁盘,
//! 全部可单测 —— 与 TS 侧 `node --test` 直测同一个取舍。
//!
//! # 两道磁盘数据防御(不是异常处理,是常态)
//!
//! 会话文件会被清理、也会超出扫描窗口,于是:
//!
//! - **自指边**(`parent == child`)在建图时就丢弃 —— 留着会让后代的父链游走
//!   误判成环;
//! - **悬空父**(边指向的父不在列表里)与**环**(沿父链回到自身)一律按根处理,
//!   不该让子节点凭空消失。

use mt_ai::sessions::LineageEdge;

/// 森林里的一个节点。
///
/// 只存**下标**而不是 `&AiSession`:调用方那边会话列表是 `Vec<AiSession>`,
/// 借用出来会把整棵树的生命周期钉死在那一份列表上(而渲染路径要 clone 出行来)。
#[derive(Debug, Clone, PartialEq)]
pub struct TreeNode {
    /// 在输入 `sessions` 里的下标。
    pub index: usize,
    /// 到父会话的那条边的下标(`None` = 根)。
    pub edge: Option<usize>,
    pub children: Vec<TreeNode>,
}

/// 拍平之后的一行。
#[derive(Debug, Clone, PartialEq)]
pub struct FlatRow {
    pub index: usize,
    pub edge: Option<usize>,
    /// 行首的连线前缀(`│ ├ └`),根为空串。**等宽字体**下才对得齐。
    pub prefix: String,
}

/// 合并磁盘扫描边与自记账边:按 child id 去重,**磁盘优先** ——
/// 磁盘指针是 CLI 亲写的权威,自记账只兜文件未落盘的窗口期。
///
/// 实现顺序:先塞 bookkept 再塞 disk(后写覆盖)。
pub fn merge_lineage_edges(disk: Vec<LineageEdge>, bookkept: Vec<LineageEdge>) -> Vec<LineageEdge> {
    let mut by_child: std::collections::HashMap<String, LineageEdge> = Default::default();
    let mut order: Vec<String> = Vec::new();
    for e in bookkept.into_iter().chain(disk.into_iter()) {
        if !by_child.contains_key(&e.session_id) {
            order.push(e.session_id.clone());
        }
        by_child.insert(e.session_id.clone(), e);
    }
    order
        .into_iter()
        .filter_map(|id| by_child.remove(&id))
        .collect()
}

/// 平铺会话 + 边 → 森林。
///
/// `ids` 是会话列表的 id(顺序即调用方排好的时间降序)。**根保持输入顺序**,
/// **子按 `timestamps` 升序**(先岔的在上)。
pub fn build_session_tree(
    ids: &[String],
    timestamps: &[String],
    edges: &[LineageEdge],
) -> Vec<TreeNode> {
    use std::collections::{HashMap, HashSet};

    // child id → 边下标。自指边在建图时即丢弃
    let mut parent_of: HashMap<&str, usize> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        if e.parent_session_id != e.session_id {
            parent_of.insert(e.session_id.as_str(), i);
        }
    }
    let index_of: HashMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();

    // 有效父:在列表内、非自指、且沿父链走到根途中不重逢(环防御)
    let effective_parent = |id: &str| -> Option<usize> {
        let cur = *parent_of.get(id)?;
        let parent = edges[cur].parent_session_id.as_str();
        if parent == id || !index_of.contains_key(parent) {
            return None;
        }
        let mut seen: HashSet<&str> = HashSet::new();
        seen.insert(id);
        let mut hop = parent;
        loop {
            if !seen.insert(hop) {
                return None;
            }
            let Some(next) = parent_of.get(hop) else { break };
            let next_parent = edges[*next].parent_session_id.as_str();
            if !index_of.contains_key(next_parent) {
                break;
            }
            hop = next_parent;
        }
        index_of.get(parent).copied()
    };

    let mut children_of: Vec<Vec<usize>> = vec![Vec::new(); ids.len()];
    let mut roots: Vec<usize> = Vec::new();
    let mut edge_of: Vec<Option<usize>> = vec![None; ids.len()];
    for (i, id) in ids.iter().enumerate() {
        match effective_parent(id) {
            Some(parent) => {
                edge_of[i] = parent_of.get(id.as_str()).copied();
                children_of[parent].push(i);
            }
            None => roots.push(i),
        }
    }

    // 子按 timestamp 升序(先岔的在上);根保持输入顺序
    let ts = |i: usize| timestamps.get(i).map(String::as_str).unwrap_or("");
    for list in children_of.iter_mut() {
        list.sort_by(|a, b| ts(*a).cmp(ts(*b)));
    }

    fn build(i: usize, children_of: &[Vec<usize>], edge_of: &[Option<usize>]) -> TreeNode {
        TreeNode {
            index: i,
            edge: edge_of[i],
            children: children_of[i]
                .iter()
                .map(|c| build(*c, children_of, edge_of))
                .collect(),
        }
    }
    roots
        .into_iter()
        .map(|r| build(r, &children_of, &edge_of))
        .collect()
}

/// 森林 → 带连线前缀的平铺行(先根深度优先,与视觉树一致)。
///
/// ```text
/// depth == 0                → prefix = ""
/// depth >= 1:
///   for i in 0..depth-1:  prefix += ancestors_last[i] ? "   " : "│  "
///   prefix += ancestors_last[depth-1] ? "└─ " : "├─ "
/// ```
/// `ancestors_last[i]` = 第 i 层祖先是不是它父亲的最后一个孩子。
pub fn flatten_session_tree(roots: &[TreeNode]) -> Vec<FlatRow> {
    fn walk(node: &TreeNode, ancestors_last: &mut Vec<bool>, out: &mut Vec<FlatRow>) {
        let depth = ancestors_last.len();
        let mut prefix = String::new();
        if depth > 0 {
            for last in &ancestors_last[..depth - 1] {
                prefix.push_str(if *last { "   " } else { "│  " });
            }
            prefix.push_str(if ancestors_last[depth - 1] {
                "└─ "
            } else {
                "├─ "
            });
        }
        out.push(FlatRow {
            index: node.index,
            edge: node.edge,
            prefix,
        });
        let last_i = node.children.len().saturating_sub(1);
        for (i, child) in node.children.iter().enumerate() {
            ancestors_last.push(i == last_i);
            walk(child, ancestors_last, out);
            ancestors_last.pop();
        }
    }
    let mut out = Vec::new();
    let mut stack = Vec::new();
    for root in roots {
        walk(root, &mut stack, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(child: &str, parent: &str) -> LineageEdge {
        LineageEdge {
            agent: "claude".into(),
            session_id: child.into(),
            parent_session_id: parent.into(),
            fork_point_uuid: None,
            branch_title: None,
        }
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn prefixes(rows: &[FlatRow]) -> Vec<&str> {
        rows.iter().map(|r| r.prefix.as_str()).collect()
    }

    fn order<'a>(rows: &[FlatRow], ids: &'a [String]) -> Vec<&'a str> {
        rows.iter().map(|r| ids[r.index].as_str()).collect()
    }

    /// 磁盘边压过自记账边(同一个 child 两边都有时,磁盘那条留下)。
    #[test]
    fn 边合并磁盘优先() {
        let merged = merge_lineage_edges(vec![edge("c", "disk-parent")], vec![edge("c", "book-parent")]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].parent_session_id, "disk-parent");

        // 各自独有的都保留
        let merged = merge_lineage_edges(vec![edge("a", "p")], vec![edge("b", "q")]);
        assert_eq!(merged.len(), 2);
    }

    /// 基本形状:根保持输入顺序,子按 timestamp 升序(先岔的在上)。
    #[test]
    fn 建树根序与子序() {
        let ids = strs(&["r1", "c2", "c1", "r2"]);
        let ts = ids
            .iter()
            .map(|id| match id.as_str() {
                "r1" => "2026-01-04",
                "c2" => "2026-01-03",
                "c1" => "2026-01-02",
                _ => "2026-01-01",
            })
            .map(String::from)
            .collect::<Vec<_>>();
        let edges = vec![edge("c1", "r1"), edge("c2", "r1")];
        let rows = flatten_session_tree(&build_session_tree(&ids, &ts, &edges));
        assert_eq!(order(&rows, &ids), vec!["r1", "c1", "c2", "r2"], "子按时间升序");
        assert_eq!(prefixes(&rows), vec!["", "├─ ", "└─ ", ""]);
    }

    /// 连线前缀:第二层要按祖先「是不是最后一个孩子」决定画 `│  ` 还是三空格。
    #[test]
    fn 连线前缀按祖先末位() {
        let ids = strs(&["r", "a", "b", "a1", "b1"]);
        let ts = strs(&["1", "2", "3", "4", "5"]);
        let edges = vec![
            edge("a", "r"),
            edge("b", "r"),
            edge("a1", "a"),
            edge("b1", "b"),
        ];
        let rows = flatten_session_tree(&build_session_tree(&ids, &ts, &edges));
        assert_eq!(order(&rows, &ids), vec!["r", "a", "a1", "b", "b1"], "先根深度优先");
        assert_eq!(
            prefixes(&rows),
            vec![
                "",
                "├─ ",     // a 不是最后一个孩子
                "│  └─ ",  // a1 在 a 下,祖先 a 非末位 → 竖线延续
                "└─ ",     // b 是最后一个孩子
                "   └─ ",  // b1 的祖先 b 是末位 → 留白
            ]
        );
    }

    /// 悬空父(父不在列表里)按根处理 —— 列表有扫描窗口上限,
    /// 父被清理或挤出窗口不该让子消失。
    #[test]
    fn 悬空父按根处理() {
        let ids = strs(&["child"]);
        let ts = strs(&["1"]);
        let rows = flatten_session_tree(&build_session_tree(&ids, &ts, &[edge("child", "gone")]));
        assert_eq!(order(&rows, &ids), vec!["child"]);
        assert_eq!(prefixes(&rows), vec![""], "落成根,前缀为空");
        assert_eq!(rows[0].edge, None, "边不生效时不该挂到节点上");
    }

    /// 自指边直接丢弃(留着会让后代的父链游走误判成环)。
    #[test]
    fn 自指边丢弃() {
        let ids = strs(&["a", "b"]);
        let ts = strs(&["1", "2"]);
        let edges = vec![edge("a", "a"), edge("b", "a")];
        let rows = flatten_session_tree(&build_session_tree(&ids, &ts, &edges));
        assert_eq!(order(&rows, &ids), vec!["a", "b"]);
        assert_eq!(prefixes(&rows), vec!["", "└─ "], "b 照样挂在 a 下");
    }

    /// 环(a→b→a)防御:两个节点都退成根,一条都不许丢。
    #[test]
    fn 成环时全部落根() {
        let ids = strs(&["a", "b"]);
        let ts = strs(&["1", "2"]);
        let edges = vec![edge("a", "b"), edge("b", "a")];
        let rows = flatten_session_tree(&build_session_tree(&ids, &ts, &edges));
        assert_eq!(rows.len(), 2, "会话一条都不许丢");
        assert_eq!(prefixes(&rows), vec!["", ""]);

        // 三节点环同理
        let ids = strs(&["a", "b", "c"]);
        let ts = strs(&["1", "2", "3"]);
        let edges = vec![edge("a", "b"), edge("b", "c"), edge("c", "a")];
        let rows = flatten_session_tree(&build_session_tree(&ids, &ts, &edges));
        assert_eq!(rows.len(), 3);
    }

    /// 没有任何边时 = 原样平铺(树只是列表长出了结构)。
    #[test]
    fn 无边时与平铺同形() {
        let ids = strs(&["a", "b", "c"]);
        let ts = strs(&["3", "2", "1"]);
        let rows = flatten_session_tree(&build_session_tree(&ids, &ts, &[]));
        assert_eq!(order(&rows, &ids), vec!["a", "b", "c"]);
        assert_eq!(prefixes(&rows), vec!["", "", ""]);
    }
}
