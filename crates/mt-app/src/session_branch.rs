//! 会话分支树的**纯逻辑**层。对应 `src/utils/sessionBranch.ts`。
//!
//! 平铺会话列表 + 分支边 → 森林 → 带连线前缀的行,外加 pane 右键菜单的**分支段**判据。
//! 不碰 gpui、不碰磁盘,全部可单测 —— 与 TS 侧 `node --test` 直测同一个取舍。
//!
//! 各 agent 的分支能力位(fork / resume 模板、会话 id 白名单、opencode 整表缺席)
//! 原先是这里的一张 `AgentBranchCaps` 表,与历史面板、启动续接各写一份、口径已分叉;
//! 现在统一住在 [`mt_ai::agent`],这里只按表出菜单。
//!
//! # 两道磁盘数据防御(不是异常处理,是常态)
//!
//! 会话文件会被清理、也会超出扫描窗口,于是:
//!
//! - **自指边**(`parent == child`)在建图时就丢弃 —— 留着会让后代的父链游走
//!   误判成环;
//! - **悬空父**(边指向的父不在列表里)与**环**(沿父链回到自身)一律按根处理,
//!   不该让子节点凭空消失。

use mt_ai::AgentKind;
use mt_ai::sessions::LineageEdge;

// ─── 菜单分支段 ───────────────────────────────────────────────

/// pane 右键菜单里**分支那一段**该出什么。
///
/// tab 右键(`PaneGroup.tsx:344-349`)与终端本体右键(`TerminalInstance.tsx:343-350`)
/// 两处口径**逐字相同** —— 用户在哪儿右键都该找得到同一个入口,所以判据收在这里
/// 一份,两处共用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchMenuSegment {
    /// 有 hook 上报的会话身份、该 agent 有 fork 能力位、且 id 拼得出命令 →
    /// 「分支会话到新分屏」+「查看会话分支」两项(带分隔线)。
    Fork {
        /// 已经拼好的 fork 命令(调用方直接写进 PTY)。
        command: String,
        /// 被 fork 的会话 id(悬停面板与自记账都要)。
        session_id: String,
        /// 归一化后的 agent(自记账登记按它比对)。
        agent: String,
    },
    /// 输入检测认出 AI 在跑、但**没拿到 hook 身份**(hook 未注册 / 身份还没到) →
    /// 一条置灰提示说明原因。不再静默消失让人以为功能坏了;锚定 fork 能力位
    /// ([`AgentKind::can_fork`]),所以仅有 resume 位的 grok / omp 不提示。
    NeedsIdentity,
    /// 什么都不出(没有 AI、或该 agent 没有 fork 能力 / 认不出)。
    None,
}

/// fork 命令由 [`mt_ai::agent::fork_command`] 按表生成(识别口径、agent 缺省按
/// Claude、id 白名单都在那边);opencode / pi / 认不出的 agent 没有 fork 能力位,
/// 菜单里连置灰提示都不出。
pub fn branch_menu_segment(
    session: Option<&crate::tree::AiSessionRef>,
    detected_agent: Option<&str>,
) -> BranchMenuSegment {
    if let Some(session) = session {
        if let Some(command) =
            mt_ai::agent::fork_command(session.agent.as_deref(), &session.session_id)
        {
            // 登记自记账用的 agent 串:会话身份原文小写(hook 上报的 `claude-code`
            // 原样保留)—— 与新身份到手时 `resolve_fork_edge` 的比对同口径
            let agent = session
                .agent
                .as_deref()
                .unwrap_or(AgentKind::Claude.key())
                .to_ascii_lowercase();
            return BranchMenuSegment::Fork {
                command,
                session_id: session.session_id.clone(),
                agent,
            };
        }
        // 有身份但没 fork 能力(grok)/ id 认不出:两项都不出,也**不**给置灰提示
        // ——「未获会话身份」的提示锚在「没有 session」那一支上
        return BranchMenuSegment::None;
    }
    match detected_agent.and_then(AgentKind::parse) {
        Some(kind) if kind.can_fork() => BranchMenuSegment::NeedsIdentity,
        _ => BranchMenuSegment::None,
    }
}

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

/// 取 `target`(输入列表里的下标)所在家族的**根**。找不到返回 `None`。
///
/// 悬停家族面板只画**单支**家族 —— 面板挂在某个 pane 的会话上,画整片森林
/// 等于把 AI 历史面板的树视图塞进一个 340px 的浮层里。对应
/// `sessionBranch.ts::findFamilyRoot`(那边按 session id 找,这里按下标 ——
/// 与 [`TreeNode`] 存下标的取舍同源)。
pub fn find_family_root(roots: &[TreeNode], target: usize) -> Option<&TreeNode> {
    fn contains(node: &TreeNode, target: usize) -> bool {
        node.index == target || node.children.iter().any(|c| contains(c, target))
    }
    roots.iter().find(|r| contains(r, target))
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

    // ---- 菜单分支段 ----
    //
    // fork / resume 模板、识别口径与 id 白名单的逐条用例在 `mt_ai::agent`;
    // 这里只钉「菜单出什么」。

    fn session(agent: Option<&str>, id: &str) -> crate::tree::AiSessionRef {
        crate::tree::AiSessionRef {
            agent: agent.map(str::to_string),
            session_id: id.to_string(),
            cwd: None,
        }
    }

    /// 有身份 + 有 fork 能力位 → 出两项,命令与 agent 都归一化好。
    #[test]
    fn 菜单段有身份时出分支两项() {
        let id = "0199a1b2-c3d4-7e8f-9012-3456789abcde";
        // hook 上报的是 `claude-code`,归一化后按 claude 拼命令
        let seg = branch_menu_segment(Some(&session(Some("claude-code"), id)), None);
        assert_eq!(
            seg,
            BranchMenuSegment::Fork {
                command: format!("claude --resume {id} --fork-session"),
                session_id: id.to_string(),
                agent: "claude-code".to_string(),
            }
        );

        let seg = branch_menu_segment(Some(&session(Some("codex"), id)), None);
        assert_eq!(
            seg,
            BranchMenuSegment::Fork {
                command: format!("codex fork {id}"),
                session_id: id.to_string(),
                agent: "codex".to_string(),
            }
        );
        // agent 缺省按 claude
        assert!(matches!(
            branch_menu_segment(Some(&session(None, id)), None),
            BranchMenuSegment::Fork { .. }
        ));
        // 大小写不敏感;登记用的 agent 串是身份原文的小写
        assert_eq!(
            branch_menu_segment(Some(&session(Some("CoDeX"), id)), None),
            BranchMenuSegment::Fork {
                command: format!("codex fork {id}"),
                session_id: id.to_string(),
                agent: "codex".to_string(),
            }
        );
    }

    /// 认不出的 agent 不再按 Claude 兜底:旧口径会给一个 gemini 会话敲出
    /// `claude --resume … --fork-session`。
    #[test]
    fn 菜单段认不出的_agent_不出分支() {
        let id = "0199a1b2-c3d4-7e8f-9012-3456789abcde";
        for agent in ["什么鬼", "gemini", "pi", "PI", "opencode"] {
            assert_eq!(
                branch_menu_segment(Some(&session(Some(agent), id)), None),
                BranchMenuSegment::None,
                "{agent}"
            );
        }
        // 参数注入形态的 id 同样不出
        assert_eq!(
            branch_menu_segment(Some(&session(Some("claude"), "--help")), None),
            BranchMenuSegment::None
        );
    }

    /// 有身份但无 fork 能力位(grok)/ id 认不出 → 两项都不出,
    /// **也不给置灰提示**(提示只锚在「没有身份」那一支)。
    #[test]
    fn 菜单段无_fork_能力时静默() {
        let id = "0199a1b2-c3d4-7e8f-9012-3456789abcde";
        assert_eq!(
            branch_menu_segment(Some(&session(Some("grok"), id)), None),
            BranchMenuSegment::None,
            "grok 只有 resume 位"
        );
        assert_eq!(
            branch_menu_segment(Some(&session(Some("omp"), id)), None),
            BranchMenuSegment::None,
            "omp 谱系未接,不开 fork"
        );
        assert_eq!(
            branch_menu_segment(Some(&session(Some("opencode"), id)), None),
            BranchMenuSegment::None
        );
        assert_eq!(
            branch_menu_segment(Some(&session(Some("claude"), "坏 id")), None),
            BranchMenuSegment::None
        );
        // 有身份时 detected_agent 一律不看(哪怕它有 fork 位)
        assert_eq!(
            branch_menu_segment(Some(&session(Some("grok"), id)), Some("claude")),
            BranchMenuSegment::None
        );
    }

    /// 输入检测认出 AI 在跑但没 hook 身份 → 置灰提示;锚在 fork 能力位上,
    /// 所以只有 resume 位的 grok 与整表缺席的 opencode/pi 都不提示。
    #[test]
    fn 菜单段未获身份时置灰提示() {
        assert_eq!(
            branch_menu_segment(None, Some("claude")),
            BranchMenuSegment::NeedsIdentity
        );
        assert_eq!(
            branch_menu_segment(None, Some("codex")),
            BranchMenuSegment::NeedsIdentity
        );
        assert_eq!(
            branch_menu_segment(None, Some("grok")),
            BranchMenuSegment::None,
            "grok 无 fork 位,不提示"
        );
        for agent in ["opencode", "pi", "omp"] {
            assert_eq!(
                branch_menu_segment(None, Some(agent)),
                BranchMenuSegment::None,
                "{agent}"
            );
        }
        // 什么 AI 都没有 → 整段消失
        assert_eq!(branch_menu_segment(None, None), BranchMenuSegment::None);
    }

    // ---- 单支家族过滤 ----

    /// 家族过滤:只留 target 所在那一支,别的根整棵不进结果。
    #[test]
    fn 家族过滤只留单支() {
        let ids = strs(&["r1", "c1", "c11", "r2", "c2"]);
        let ts = strs(&["1", "2", "3", "4", "5"]);
        let edges = vec![edge("c1", "r1"), edge("c11", "c1"), edge("c2", "r2")];
        let roots = build_session_tree(&ids, &ts, &edges);
        assert_eq!(roots.len(), 2, "两片家族");

        // 从孙节点出发也要找到它那一支的根
        let family = find_family_root(&roots, 2).expect("c11 在 r1 家族里");
        let rows = flatten_session_tree(std::slice::from_ref(family));
        assert_eq!(order(&rows, &ids), vec!["r1", "c1", "c11"]);
        assert_eq!(prefixes(&rows), vec!["", "└─ ", "   └─ "]);

        // 另一支
        let family = find_family_root(&roots, 4).expect("c2 在 r2 家族里");
        let rows = flatten_session_tree(std::slice::from_ref(family));
        assert_eq!(order(&rows, &ids), vec!["r2", "c2"]);

        // 根自己
        assert_eq!(find_family_root(&roots, 0).map(|n| n.index), Some(0));
        // 不在森林里的下标
        assert!(find_family_root(&roots, 99).is_none());
        assert!(find_family_root(&[], 0).is_none());
    }

    /// 孤零零一条会话(没有任何分支)照样是一支家族 —— 面板画一行,不是空。
    #[test]
    fn 无分支的会话自成家族() {
        let ids = strs(&["solo"]);
        let ts = strs(&["1"]);
        let roots = build_session_tree(&ids, &ts, &[]);
        let family = find_family_root(&roots, 0).expect("自成一支");
        assert_eq!(flatten_session_tree(std::slice::from_ref(family)).len(), 1);
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
