//! SplitNode 布局树:纯数据结构 + 树操作。**不依赖 gpui**,因此可以直接单测。
//!
//! 语义对照 `src/store.ts` 与 `src/utils/layoutOps.ts`,逐条列在下面 ——
//! 这一层是整个壳的地基,行为对不上,上面画得再像也是另一个软件:
//!
//! | TS 侧 | 这里 |
//! |---|---|
//! | `STATUS_PRIORITY` / `getHighestStatus` | [`PaneStatus::priority`] / [`SplitNode::highest_status`] |
//! | `collectPanes` / `collectPtyIds` | [`SplitNode::panes`] / [`SplitNode::pty_ids`] |
//! | `insertSplit` | [`SplitNode::insert_split`] |
//! | `removePaneFromLayout` | [`SplitNode::remove_pane`] |
//! | `updatePaneStatus`(按 ptyId) | [`SplitNode::update_status_by_pty`] |
//! | `newTerminal` 里的「加进目标 leaf 的 tab 栏」 | [`SplitNode::append_pane`] |
//! | `activatePane` | [`SplitNode::activate_pane`] |
//!
//! # 与 TS 版的两点结构差异
//!
//! 1. **节点带 id**。TS 侧靠对象引用相等来定位节点(`replaceNode`),Rust 里没有
//!    这条路;而 gpui 的元素、`ResizableState` 也都需要跨帧稳定的 id。于是每个
//!    节点自带一个 id,`SavedSplitNode` 里不落这个字段(磁盘格式一个字不改)。
//! 2. **就地改而不是整棵重建**。TS 用不可变更新是为了让 zustand 的引用比较能
//!    短路重渲染;gpui 靠 `cx.notify()` 显式触发,没有这个约束。

use std::sync::atomic::{AtomicU64, Ordering};

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 进程内唯一 id(对应 store.ts 的 `genId`)。
pub fn gen_id(prefix: &str) -> String {
    let n = ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("{prefix}-{n}")
}

/// pane / 项目的四态。聚合优先级 `error > ai-working > ai-idle > idle`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PaneStatus {
    #[default]
    Idle,
    AiIdle,
    AiWorking,
    Error,
}

impl PaneStatus {
    pub fn priority(self) -> u8 {
        match self {
            Self::Error => 3,
            Self::AiWorking => 2,
            Self::AiIdle => 1,
            Self::Idle => 0,
        }
    }

    /// 与后端(`mt_ai::StatusChange::status`)之间的字符串口径,一字不改。
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "idle" => Some(Self::Idle),
            "ai-idle" => Some(Self::AiIdle),
            "ai-working" => Some(Self::AiWorking),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

}

/// hook 上报的 AI 会话身份(对应 `types.ts` 的 `AiSessionRef`)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiSessionRef {
    pub agent: Option<String>,
    pub session_id: String,
    /// 会话启动目录:`claude --resume` 只认这个目录对应的会话桶。
    pub cwd: Option<String>,
}

/// 一个终端 tab(对应 `types.ts` 的 `PaneState`)。
///
/// **不持有 PTY 也不持有终端视图** —— 那些活在 [`crate::store::AppStore`] 的
/// `terminals` 表里(按 `pty_id` 索引,等价于旧版的 `terminalCache`)。这里只有
/// 能落盘/能比较的纯数据。
#[derive(Clone, Debug, PartialEq)]
pub struct PaneState {
    pub id: String,
    pub shell_name: String,
    pub custom_title: Option<String>,
    pub status: PaneStatus,
    /// 后端 pane 编号。同时是 `MINITERM_PTY_ID`(hook 回报的定位键)与
    /// `mt_ai` 里的 `pane_id`。`None` = PTY 还没起来 / 起失败。
    pub pty_id: Option<u32>,
    /// 工作目录覆盖;`None` 时用项目根。
    pub cwd: Option<String>,
    pub ai_session: Option<AiSessionRef>,
    /// 后端识别到的会话内 AI 命令名(hook / 输入检测),品牌标识兜底用。
    pub detected_agent: Option<String>,
    /// 本次 ai-idle 的成因是「需要用户确认」。
    pub attention: bool,
}

impl PaneState {
    pub fn new(shell_name: impl Into<String>) -> Self {
        Self {
            id: gen_id("pane"),
            shell_name: shell_name.into(),
            custom_title: None,
            status: PaneStatus::Idle,
            pty_id: None,
            cwd: None,
            ai_session: None,
            detected_agent: None,
            attention: false,
        }
    }

    /// tab 上显示的名字:自定义名 > shell 名(远程连接名那一支等 mt-ssh 移入后再补)。
    pub fn label(&self) -> &str {
        self.custom_title.as_deref().unwrap_or(&self.shell_name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDirection {
    /// 左右并排(「向右分屏」)。
    Horizontal,
    /// 上下堆叠(「向下分屏」)。
    Vertical,
}

impl SplitDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
        }
    }

    pub fn from_str(s: &str) -> Self {
        // 磁盘上只有这两个值;非法值按 horizontal 处理(与 TS 侧同样宽容)
        if s == "vertical" {
            Self::Vertical
        } else {
            Self::Horizontal
        }
    }
}

/// 分屏树。叶子是一组共享同一格子的 pane(tab 栏),split 是可拖拽的分割。
#[derive(Clone, Debug, PartialEq)]
pub enum SplitNode {
    Leaf {
        id: String,
        panes: Vec<PaneState>,
        active_pane_id: String,
    },
    Split {
        id: String,
        direction: SplitDirection,
        children: Vec<SplitNode>,
        /// 百分比(合计 100)。与 `savedLayout` 的 `sizes` 同一口径。
        sizes: Vec<f64>,
    },
}

impl SplitNode {
    /// 单 pane 的叶子。
    pub fn leaf(pane: PaneState) -> Self {
        let active = pane.id.clone();
        Self::Leaf {
            id: gen_id("leaf"),
            panes: vec![pane],
            active_pane_id: active,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Leaf { id, .. } | Self::Split { id, .. } => id,
        }
    }

    /// 聚合状态(`getHighestStatus`)。
    pub fn highest_status(&self) -> PaneStatus {
        match self {
            Self::Leaf { panes, .. } => panes.iter().fold(PaneStatus::Idle, |acc, p| {
                if p.status.priority() > acc.priority() {
                    p.status
                } else {
                    acc
                }
            }),
            Self::Split { children, .. } => children.iter().fold(PaneStatus::Idle, |acc, c| {
                let s = c.highest_status();
                if s.priority() > acc.priority() { s } else { acc }
            }),
        }
    }

    /// 深度优先(左到右)收集所有 pane —— 与屏幕上的排列同序。
    pub fn panes(&self) -> Vec<&PaneState> {
        let mut out = Vec::new();
        self.collect_panes(&mut out);
        out
    }

    fn collect_panes<'a>(&'a self, out: &mut Vec<&'a PaneState>) {
        match self {
            Self::Leaf { panes, .. } => out.extend(panes.iter()),
            Self::Split { children, .. } => {
                for c in children {
                    c.collect_panes(out);
                }
            }
        }
    }

    pub fn pty_ids(&self) -> Vec<u32> {
        self.panes().iter().filter_map(|p| p.pty_id).collect()
    }

    pub fn pane(&self, pane_id: &str) -> Option<&PaneState> {
        self.panes().into_iter().find(|p| p.id == pane_id)
    }

    pub fn pane_mut(&mut self, pane_id: &str) -> Option<&mut PaneState> {
        match self {
            Self::Leaf { panes, .. } => panes.iter_mut().find(|p| p.id == pane_id),
            Self::Split { children, .. } => {
                children.iter_mut().find_map(|c| c.pane_mut(pane_id))
            }
        }
    }

    pub fn pane_by_pty(&self, pty_id: u32) -> Option<&PaneState> {
        self.panes().into_iter().find(|p| p.pty_id == Some(pty_id))
    }

    pub fn pane_by_pty_mut(&mut self, pty_id: u32) -> Option<&mut PaneState> {
        match self {
            Self::Leaf { panes, .. } => panes.iter_mut().find(|p| p.pty_id == Some(pty_id)),
            Self::Split { children, .. } => {
                children.iter_mut().find_map(|c| c.pane_by_pty_mut(pty_id))
            }
        }
    }

    /// 持有该 pane 的叶子(`findLeafContainingPane`)。
    pub fn leaf_of_pane(&self, pane_id: &str) -> Option<&SplitNode> {
        match self {
            Self::Leaf { panes, .. } => panes.iter().any(|p| p.id == pane_id).then_some(self),
            Self::Split { children, .. } => children.iter().find_map(|c| c.leaf_of_pane(pane_id)),
        }
    }

    fn leaf_of_pane_mut(&mut self, pane_id: &str) -> Option<&mut SplitNode> {
        match self {
            Self::Leaf { panes, .. } => {
                if panes.iter().any(|p| p.id == pane_id) {
                    Some(self)
                } else {
                    None
                }
            }
            Self::Split { children, .. } => children
                .iter_mut()
                .find_map(|c| c.leaf_of_pane_mut(pane_id)),
        }
    }

    /// 树里第一个叶子当前激活的 pane(没有焦点信息时的回落,同 `resolveActivePane`)。
    pub fn first_active_pane(&self) -> Option<&PaneState> {
        match self {
            Self::Leaf {
                panes,
                active_pane_id,
                ..
            } => panes
                .iter()
                .find(|p| &p.id == active_pane_id)
                .or_else(|| panes.first()),
            Self::Split { children, .. } => children.iter().find_map(|c| c.first_active_pane()),
        }
    }

    /// 在目标 pane 所在叶子处分屏,新叶子放第二格,50/50(`insertSplit`)。
    /// 返回是否命中目标(未命中时新叶子原样丢弃,由调用方负责回收 PTY)。
    pub fn insert_split(
        &mut self,
        target_pane_id: &str,
        direction: SplitDirection,
        new_leaf: SplitNode,
    ) -> bool {
        // 叶子在递归里要能「借出去又拿回来」,用 Option 表达最直白:命中的那一层
        // take 走,没命中的层原样留在里面。
        let mut slot = Some(new_leaf);
        self.insert_split_inner(target_pane_id, direction, &mut slot);
        slot.is_none()
    }

    fn insert_split_inner(
        &mut self,
        target_pane_id: &str,
        direction: SplitDirection,
        new_leaf: &mut Option<SplitNode>,
    ) {
        match self {
            Self::Leaf { panes, .. } => {
                if !panes.iter().any(|p| p.id == target_pane_id) {
                    return;
                }
                let Some(new_leaf) = new_leaf.take() else {
                    return;
                };
                // 把自己换成 split(自己成为 children[0])。
                let old = std::mem::replace(
                    self,
                    Self::Split {
                        id: gen_id("split"),
                        direction,
                        children: Vec::new(),
                        sizes: vec![50.0, 50.0],
                    },
                );
                if let Self::Split { children, .. } = self {
                    children.push(old);
                    children.push(new_leaf);
                }
            }
            Self::Split { children, .. } => {
                for c in children.iter_mut() {
                    if new_leaf.is_none() {
                        return;
                    }
                    c.insert_split_inner(target_pane_id, direction, new_leaf);
                }
            }
        }
    }

    /// 把 pane 追加到锚点所在叶子的 tab 栏末尾并激活(`newTerminal` 的主路径)。
    /// 锚点为 `None` 或找不到时落到第一个叶子。返回是否成功。
    pub fn append_pane(&mut self, anchor_pane_id: Option<&str>, pane: PaneState) -> bool {
        let target = anchor_pane_id
            .and_then(|id| self.leaf_of_pane(id).map(|l| l.id().to_string()))
            .or_else(|| self.first_leaf_id());
        let Some(leaf_id) = target else {
            return false;
        };
        let Some(SplitNode::Leaf {
            panes,
            active_pane_id,
            ..
        }) = self.node_mut(&leaf_id)
        else {
            return false;
        };
        *active_pane_id = pane.id.clone();
        panes.push(pane);
        true
    }

    pub fn first_leaf_id(&self) -> Option<String> {
        match self {
            Self::Leaf { id, .. } => Some(id.clone()),
            Self::Split { children, .. } => children.iter().find_map(|c| c.first_leaf_id()),
        }
    }

    /// 按节点 id 定位。
    pub fn node(&self, node_id: &str) -> Option<&SplitNode> {
        if self.id() == node_id {
            return Some(self);
        }
        match self {
            Self::Leaf { .. } => None,
            Self::Split { children, .. } => children.iter().find_map(|c| c.node(node_id)),
        }
    }

    /// 按节点 id 定位(可变)。
    pub fn node_mut(&mut self, node_id: &str) -> Option<&mut SplitNode> {
        if self.id() == node_id {
            return Some(self);
        }
        match self {
            Self::Leaf { .. } => None,
            Self::Split { children, .. } => children.iter_mut().find_map(|c| c.node_mut(node_id)),
        }
    }

    /// 激活叶子里的某个 pane(tab 切换,`activatePane`)。返回是否改变。
    pub fn activate_pane(&mut self, pane_id: &str) -> bool {
        let Some(SplitNode::Leaf { active_pane_id, .. }) = self.leaf_of_pane_mut(pane_id) else {
            return false;
        };
        if active_pane_id == pane_id {
            return false;
        }
        *active_pane_id = pane_id.to_string();
        true
    }

    /// 从树里摘掉一个 pane(`removePaneFromLayout`):
    /// - 叶子里还有别的 pane:只摘 pane,必要时把 activePaneId 移到最后一个;
    /// - 叶子空了:从父 split 摘掉;父 split 只剩一个孩子则塌陷成那个孩子;
    /// - 整棵树空了:返回 `None`(调用方据此回到空态,也就是「关最后一个 pane 关 tab」)。
    pub fn remove_pane(self, pane_id: &str) -> Option<SplitNode> {
        match self {
            Self::Leaf {
                id,
                mut panes,
                active_pane_id,
            } => {
                if !panes.iter().any(|p| p.id == pane_id) {
                    return Some(Self::Leaf {
                        id,
                        panes,
                        active_pane_id,
                    });
                }
                panes.retain(|p| p.id != pane_id);
                if panes.is_empty() {
                    return None;
                }
                let active_pane_id = if active_pane_id == pane_id {
                    panes[panes.len() - 1].id.clone()
                } else {
                    active_pane_id
                };
                Some(Self::Leaf {
                    id,
                    panes,
                    active_pane_id,
                })
            }
            Self::Split {
                id,
                direction,
                children,
                sizes,
            } => {
                let before = children.len();
                let children: Vec<SplitNode> = children
                    .into_iter()
                    .filter_map(|c| c.remove_pane(pane_id))
                    .collect();
                match children.len() {
                    0 => None,
                    1 => children.into_iter().next(),
                    n => Some(Self::Split {
                        id,
                        direction,
                        // 孩子数变了旧 sizes 就对不上,均分比按旧值截断更不容易出怪布局
                        sizes: if n == before {
                            sizes
                        } else {
                            vec![100.0 / n as f64; n]
                        },
                        children,
                    }),
                }
            }
        }
    }

    /// 按 ptyId 更新状态(`updatePaneStatus`)。
    ///
    /// 回到 idle/error = AI 会话不复存在,连带清掉会话身份与识别到的 agent ——
    /// 否则用户主动退出 claude 之后,下次启动又会被 resume 回来。
    pub fn update_status_by_pty(
        &mut self,
        pty_id: u32,
        status: PaneStatus,
        attention: bool,
        agent: Option<&str>,
    ) -> bool {
        let Some(pane) = self.pane_by_pty_mut(pty_id) else {
            return false;
        };
        pane.status = status;
        pane.attention = attention;
        match status {
            PaneStatus::Idle | PaneStatus::Error => {
                pane.ai_session = None;
                pane.detected_agent = None;
            }
            _ => {
                if let Some(agent) = agent {
                    pane.detected_agent = Some(agent.to_string());
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(name: &str, pty: u32) -> PaneState {
        let mut p = PaneState::new(name);
        p.pty_id = Some(pty);
        p
    }

    fn leaf(name: &str, pty: u32) -> SplitNode {
        SplitNode::leaf(pane(name, pty))
    }

    /// getHighestStatus:error > ai-working > ai-idle > idle,跨层聚合。
    #[test]
    fn 状态聚合按优先级取最高() {
        let mut root = leaf("a", 1);
        root.insert_split("", SplitDirection::Horizontal, leaf("b", 2)); // 不命中,不该变
        assert!(matches!(root, SplitNode::Leaf { .. }));

        let target = root.panes()[0].id.clone();
        assert!(root.insert_split(&target, SplitDirection::Horizontal, leaf("b", 2)));

        assert_eq!(root.highest_status(), PaneStatus::Idle);
        root.pane_by_pty_mut(2).unwrap().status = PaneStatus::AiIdle;
        assert_eq!(root.highest_status(), PaneStatus::AiIdle);
        root.pane_by_pty_mut(1).unwrap().status = PaneStatus::AiWorking;
        assert_eq!(root.highest_status(), PaneStatus::AiWorking);
        root.pane_by_pty_mut(2).unwrap().status = PaneStatus::Error;
        assert_eq!(root.highest_status(), PaneStatus::Error);
    }

    /// insertSplit:命中叶子变成 split,原叶子在第一格,新叶子在第二格,50/50。
    #[test]
    fn 分屏把命中叶子换成两格的_split() {
        let mut root = leaf("a", 1);
        let target = root.panes()[0].id.clone();
        assert!(root.insert_split(&target, SplitDirection::Vertical, leaf("b", 2)));

        let SplitNode::Split {
            direction,
            children,
            sizes,
            ..
        } = &root
        else {
            panic!("应该变成 split");
        };
        assert_eq!(*direction, SplitDirection::Vertical);
        assert_eq!(sizes, &vec![50.0, 50.0]);
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].panes()[0].pty_id, Some(1));
        assert_eq!(children[1].panes()[0].pty_id, Some(2));
    }

    /// 深层分屏:只有命中的那个叶子变形,兄弟节点原样保留。
    #[test]
    fn 深层分屏只动命中的叶子() {
        let mut root = leaf("a", 1);
        let a = root.panes()[0].id.clone();
        root.insert_split(&a, SplitDirection::Horizontal, leaf("b", 2));
        let b = root.pane_by_pty(2).unwrap().id.clone();
        assert!(root.insert_split(&b, SplitDirection::Vertical, leaf("c", 3)));

        let SplitNode::Split { children, .. } = &root else {
            panic!()
        };
        assert!(matches!(children[0], SplitNode::Leaf { .. }), "兄弟没被动过");
        let SplitNode::Split {
            direction,
            children: inner,
            ..
        } = &children[1]
        else {
            panic!("命中的叶子应变成 split")
        };
        assert_eq!(*direction, SplitDirection::Vertical);
        assert_eq!(inner[0].panes()[0].pty_id, Some(2));
        assert_eq!(inner[1].panes()[0].pty_id, Some(3));
    }

    /// 同一叶子里的多 tab:关掉激活的那个,activePaneId 落到剩下的最后一个。
    #[test]
    fn 关闭激活_tab_后激活末尾那个() {
        let mut root = leaf("a", 1);
        root.append_pane(None, pane("b", 2));
        root.append_pane(None, pane("c", 3));
        let SplitNode::Leaf {
            panes,
            active_pane_id,
            ..
        } = &root
        else {
            panic!()
        };
        assert_eq!(panes.len(), 3);
        assert_eq!(active_pane_id, &panes[2].id, "新建的 tab 自动激活");

        let c = panes[2].id.clone();
        let root = root.remove_pane(&c).expect("还有两个 tab");
        let SplitNode::Leaf {
            panes,
            active_pane_id,
            ..
        } = &root
        else {
            panic!()
        };
        assert_eq!(panes.len(), 2);
        assert_eq!(active_pane_id, &panes[1].id);
    }

    /// 关掉未激活的 tab 不改变激活项。
    #[test]
    fn 关闭非激活_tab_不动激活项() {
        let mut root = leaf("a", 1);
        root.append_pane(None, pane("b", 2));
        let active = match &root {
            SplitNode::Leaf { active_pane_id, .. } => active_pane_id.clone(),
            _ => panic!(),
        };
        let first = root.panes()[0].id.clone();
        let root = root.remove_pane(&first).unwrap();
        match &root {
            SplitNode::Leaf { active_pane_id, .. } => assert_eq!(active_pane_id, &active),
            _ => panic!(),
        }
    }

    /// split 只剩一个孩子时塌陷成那个孩子。
    #[test]
    fn 分屏关掉一格后塌陷() {
        let mut root = leaf("a", 1);
        let a = root.panes()[0].id.clone();
        root.insert_split(&a, SplitDirection::Horizontal, leaf("b", 2));

        let b = root.pane_by_pty(2).unwrap().id.clone();
        let root = root.remove_pane(&b).expect("还剩一格");
        assert!(matches!(root, SplitNode::Leaf { .. }), "应塌陷回叶子");
        assert_eq!(root.panes().len(), 1);
        assert_eq!(root.panes()[0].pty_id, Some(1));
    }

    /// 三格分屏关掉一格:剩下两格,sizes 均分(旧值对不上时不做截断)。
    #[test]
    fn 三格关掉一格后_sizes_均分() {
        let mut root = leaf("a", 1);
        let a = root.panes()[0].id.clone();
        root.insert_split(&a, SplitDirection::Horizontal, leaf("b", 2));
        // 手工塞第三格,模拟用户拖过分隔条的非均分状态
        if let SplitNode::Split {
            children, sizes, ..
        } = &mut root
        {
            children.push(leaf("c", 3));
            *sizes = vec![20.0, 30.0, 50.0];
        }

        let b = root.pane_by_pty(2).unwrap().id.clone();
        let root = root.remove_pane(&b).unwrap();
        let SplitNode::Split { sizes, children, .. } = &root else {
            panic!("还有两格,不该塌陷")
        };
        assert_eq!(children.len(), 2);
        assert_eq!(sizes, &vec![50.0, 50.0]);
    }

    /// 关掉最后一个 pane → 整棵树消失(调用方据此关掉 tab / 回空态)。
    #[test]
    fn 关掉最后一个_pane_返回_none() {
        let root = leaf("a", 1);
        let a = root.panes()[0].id.clone();
        assert!(root.remove_pane(&a).is_none());
    }

    /// updatePaneStatus:回到 idle/error 清掉会话身份与 agent;AI 态则记下 agent。
    #[test]
    fn 状态回到_idle_时清掉会话身份() {
        let mut root = leaf("a", 1);
        root.pane_by_pty_mut(1).unwrap().ai_session = Some(AiSessionRef {
            agent: Some("claude".into()),
            session_id: "s1".into(),
            cwd: None,
        });

        assert!(root.update_status_by_pty(1, PaneStatus::AiWorking, false, Some("claude")));
        let p = root.pane_by_pty(1).unwrap();
        assert_eq!(p.status, PaneStatus::AiWorking);
        assert_eq!(p.detected_agent.as_deref(), Some("claude"));
        assert!(p.ai_session.is_some());

        root.update_status_by_pty(1, PaneStatus::Idle, false, None);
        let p = root.pane_by_pty(1).unwrap();
        assert!(p.ai_session.is_none(), "退出 AI 会话必须清身份");
        assert!(p.detected_agent.is_none());
    }

    /// attention 与状态解耦:codex 的 PermissionRequest 状态是 ai-working 但要点黄灯。
    #[test]
    fn attention_与状态解耦() {
        let mut root = leaf("a", 1);
        root.update_status_by_pty(1, PaneStatus::AiWorking, true, None);
        assert!(root.pane_by_pty(1).unwrap().attention);
        root.update_status_by_pty(1, PaneStatus::AiWorking, false, None);
        assert!(!root.pane_by_pty(1).unwrap().attention);
    }

    #[test]
    fn 激活_pane_切换叶子内的_tab() {
        let mut root = leaf("a", 1);
        root.append_pane(None, pane("b", 2));
        let first = root.panes()[0].id.clone();
        assert!(root.activate_pane(&first));
        assert!(!root.activate_pane(&first), "已激活的再点不算变化");
        match &root {
            SplitNode::Leaf { active_pane_id, .. } => assert_eq!(active_pane_id, &first),
            _ => panic!(),
        }
    }

    /// 锚点决定新 tab 落在哪一格 —— 分屏下点下方那格的 + 号不该加到上方去。
    #[test]
    fn 新_tab_落在锚点所在的格子() {
        let mut root = leaf("a", 1);
        let a = root.panes()[0].id.clone();
        root.insert_split(&a, SplitDirection::Horizontal, leaf("b", 2));
        let b = root.pane_by_pty(2).unwrap().id.clone();

        assert!(root.append_pane(Some(&b), pane("c", 3)));
        let SplitNode::Split { children, .. } = &root else {
            panic!()
        };
        assert_eq!(children[0].panes().len(), 1, "锚点不在这格");
        assert_eq!(children[1].panes().len(), 2);
    }

    #[test]
    fn pty_id_收集覆盖整棵树() {
        let mut root = leaf("a", 1);
        let a = root.panes()[0].id.clone();
        root.insert_split(&a, SplitDirection::Horizontal, leaf("b", 2));
        let b = root.pane_by_pty(2).unwrap().id.clone();
        root.append_pane(Some(&b), pane("c", 3));
        let mut ids = root.pty_ids();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3]);
    }
}
