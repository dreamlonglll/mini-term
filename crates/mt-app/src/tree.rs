//! SplitNode 布局树:纯数据结构 + 树操作。**不依赖 gpui**,因此可以直接单测。
//!
//! 语义对照 `src/store.ts` 与 `src/utils/layoutOps.ts`,逐条列在下面 ——
//! 这一层是整个壳的地基,行为对不上,上面画得再像也是另一个软件:
//!
//! | TS 侧 | 这里 |
//! |---|---|
//! | `STATUS_PRIORITY` / `getHighestStatus` | [`StatusLight`] 的声明序 / [`SplitNode::highest_light`](多出第五档 attention,见 [`StatusLight`]) |
//! | `collectPanes` / `collectPtyIds` | [`SplitNode::panes`] / [`SplitNode::pty_ids`] |
//! | `insertSplit` / `insertSplitAt` | [`SplitNode::insert_split`] / [`SplitNode::insert_split_at`] |
//! | `movePaneInLayout` | [`SplitNode::move_pane_in_layout`] |
//! | `movePaneToTabIndex` | [`SplitNode::move_pane_to_tab_index`] |
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
use std::time::{SystemTime, UNIX_EPOCH};

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 生成 id:`{prefix}-{毫秒时间戳}-{进程内计数}`,对应 store.ts 的
/// `genId = () => \`id-${Date.now()}-${++idCounter}\``。
///
/// ⚠️ **时间戳那一段不能省**。这里生成的 id 有一部分会**落盘**(项目 / 分组 /
/// SSH 连接 / AI 启动器),而计数器每次启动都从 0 数起 —— 只靠计数的话,新一次
/// 启动里第 N 个 id 与上一次启动里第 N 个 id 一模一样。曾经就是这样:
/// 「分组右键 → 添加项目」偶发地拿到一个与库里既有项目相同的 `proj-N`,
/// 新项目在列表上完全看不见(渲染按 id 去重、`store.project(id)` 找到的是旧那条),
/// 重启后又因为 `config.db` 的 `projects` 表以 id 为主键、后写覆盖先写,**旧项目
/// 的那一行被新项目悄悄顶掉**。单测锁的是「同前缀连续生成互不相同」;跨进程唯一
/// 靠时间戳,再由各落盘点的 [`gen_unique_id`] 兜底。
pub fn gen_id(prefix: &str) -> String {
    let n = ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{prefix}-{millis}-{n}")
}

/// [`gen_id`] 的「对既有数据去重」版:`is_taken` 说已经有了就再生成一个。
/// 所有**会落盘**的 id 都该走这一条 —— 时间戳挡的是正常情况,这一道挡的是
/// 时钟回拨 / 恢复旧库这类正常情况之外的事。
pub fn gen_unique_id(prefix: &str, is_taken: impl Fn(&str) -> bool) -> String {
    loop {
        let id = gen_id(prefix);
        if !is_taken(&id) {
            return id;
        }
    }
}

/// pane 的四态(后端与移动端协议的口径)。界面上的灯按 [`StatusLight`] 画 ——
/// 它在这四态之上再叠一层 `attention`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PaneStatus {
    #[default]
    Idle,
    AiIdle,
    AiWorking,
    Error,
}

impl PaneStatus {
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

    /// [`from_str`] 的反向。移动端快照里 pane 状态是**字符串**上报的
    /// (`SyncPane::status`,协议 v2 的 wire 口径),这里是唯一的产出口。
    ///
    /// [`from_str`]: Self::from_str
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::AiIdle => "ai-idle",
            Self::AiWorking => "ai-working",
            Self::Error => "error",
        }
    }
}

/// 状态灯的**显示档位**:[`PaneStatus`] 四态再叠上 `attention` 位,多出第五档
/// 「等你处理」(授权 / 表单 / 回合因 API 错误结束)。
///
/// 第五档不进 [`PaneStatus`]:那是后端与移动端协议的字符串口径(`as_str` / `from_str`),
/// 加值会让旧手机端认不出;attention 本来就与状态解耦(Codex 的授权请求是
/// ai-working + attention),在显示这一层叠出来即可。
///
/// **声明顺序即显示优先级**(`derive(Ord)`):error > attention > ai-working >
/// ai-idle > idle —— 与标题栏灯 `TitleBarLight` 同一个「error 在前、attention 次之」。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StatusLight {
    #[default]
    Idle,
    AiIdle,
    AiWorking,
    Attention,
    Error,
}

impl StatusLight {
    /// 一个 pane 的显示档位。attention 只在两个 AI 态上生效:落 error / idle 时
    /// `update_status_by_pty` 本来就把它清了,这里再兜一层,免得哪条写入口漏清时
    /// 一个死掉的终端还挂着「等你处理」。
    pub fn of(status: PaneStatus, attention: bool) -> Self {
        match status {
            PaneStatus::Error => Self::Error,
            PaneStatus::AiIdle | PaneStatus::AiWorking if attention => Self::Attention,
            PaneStatus::AiWorking => Self::AiWorking,
            PaneStatus::AiIdle => Self::AiIdle,
            PaneStatus::Idle => Self::Idle,
        }
    }

    /// 「一个 `exit 1` 的 shell 不该亮红点、更不该盖住别处在跑的 AI」那几处聚合用
    /// (边条全局徽标、终端列表竖条):error 压成 idle 再比。
    pub fn error_as_idle(self) -> Self {
        if self == Self::Error {
            Self::Idle
        } else {
            self
        }
    }
}

/// 手上只有四态的调用点(会话列表的「在跑」点等)直接转:不带 attention。
impl From<PaneStatus> for StatusLight {
    fn from(status: PaneStatus) -> Self {
        Self::of(status, false)
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
    /// 待续接标记:恢复布局时随 `ai_session` 置位,起 PTY 写完 resume 命令后清除
    /// (**只清标记不清身份**)。
    ///
    /// **运行时派生,不进磁盘格式** —— `SavedPane` 里没有这个字段,`persist.rs`
    /// 在 `restore_layout` 里按「落盘过 ai_session」置位。置位**不看**
    /// `aiAutoResume` 开关:标记的语义是「这个 pane 还没续过」,不是「这次启动没续」,
    /// 开关中途打开后点开尚未起 PTY 的 pane 仍应续上(见 `src/utils/aiResume.ts`)。
    pub resume_pending: bool,
    /// 后端识别到的会话内 AI 命令名(hook / 输入检测),品牌标识兜底用。
    pub detected_agent: Option<String>,
    /// AI 停下来等你处理(授权 / 表单 / 回合因 API 错误结束)。与 `status` 解耦 ——
    /// Codex 的授权请求是 ai-working + attention;界面上叠成第五档灯,见 [`StatusLight`]。
    pub attention: bool,
    /// shell 通过 OSC 0/2 报上来的窗口标题(已清洗 + 限长),页签副段的来源。
    ///
    /// **运行时派生,不进磁盘格式** —— `SavedPane` 里没有这个字段,`persist.rs`
    /// 一行不动(与 `resume_pending` 同一口径)。重启后由新起的 shell 自己重报,
    /// 落盘一个上次的目录名只会在 PTY 起来前显示一段陈旧信息。
    ///
    /// `None` = 还没报过 / 收到了 `ResetTitle`。写入口唯一:
    /// [`AppStore::set_pane_osc_title`](crate::store::AppStore::set_pane_osc_title)。
    pub osc_title: Option<String>,
    /// 这条 PTY 是**干净退出**的(退出码 0:用户自己 `exit` / Ctrl+D 收的工)。
    /// 只在 `status == Error` 时有意义。
    ///
    /// **只影响显示层**:[`PaneStatus`] 四态是后端与移动端的协议口径,不为它加值;error
    /// 照旧是这条 PTY 的终态 —— 迟到事件的闸、撤出轮询、清完成记录一样不少。界面上的灯
    /// 经 [`Self::display_status`] / [`Self::light`] 统一推导:干净退出按 idle 画灰灯,
    /// 非 0 退出与起不来仍是红灯。否则一个正常收工的终端挂着红点,与「出错了」分不开。
    ///
    /// **运行时派生,不进磁盘格式**(与 `osc_title` 同一口径)。`AppStore::on_pty_exit`
    /// 置位,重连 / 起不来时清掉。
    pub clean_exit: bool,
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
            resume_pending: false,
            detected_agent: None,
            attention: false,
            osc_title: None,
            clean_exit: false,
        }
    }

    /// tab 上显示的名字:自定义名 > shell 名。
    ///
    /// ⚠️ **远程项目不要用这个** —— 三级口径「自定义名 > 远程连接名 > shell 名」
    /// 要查连接表,只有 store 拿得到:走
    /// [`AppStore::pane_display_label`](crate::store::AppStore::pane_display_label)。
    /// 本函数留着是给「压根没有项目上下文」的调用点(纯数据层单测)。
    pub fn label(&self) -> &str {
        self.custom_title.as_deref().unwrap_or(&self.shell_name)
    }

    /// 这个 pane 该不该显示「AI 会话」身份(tab 上的品牌图标)。
    /// 逐条照抄 `src/utils/aiResume.ts::paneShowsAiSession`。
    ///
    /// 第三条是关键:**待续接**的 pane 只有在自动续接开着时才算「有会话」——
    /// 开关关着时那份会话身份只是留着备查(用户手动点才续),tab 上挂个品牌图标
    /// 会让人以为 AI 正跑着。
    pub fn shows_ai_session(&self, auto_resume_enabled: bool) -> bool {
        if matches!(self.status, PaneStatus::AiWorking | PaneStatus::AiIdle) {
            return true;
        }
        if self.ai_session.is_none() {
            return false;
        }
        !self.resume_pending || auto_resume_enabled
    }

    /// tab 上品牌图标该显示哪家。口径与原版 `inferVendor({ agent })` 一致:
    /// hook 上报的 agent 优先,退到输入检测认出来的 agent。
    ///
    /// **不是 `for_session` 的「模型优先」口径** —— tab 表达的是「跑的是哪个 CLI」,
    /// claude CLI 挂 GLM 中转时 tab 上仍该是 claude。
    pub fn ai_agent(&self) -> Option<&str> {
        self.ai_session
            .as_ref()
            .and_then(|s| s.agent.as_deref())
            .or(self.detected_agent.as_deref())
    }

    /// 显示层的四态:干净退出的 pane 按 idle 算(见 [`Self::clean_exit`])。
    ///
    /// 界面上「按状态画灯 / 聚合 / 挑待办」的地方都从这里(或 [`Self::light`])取;
    /// **协议上报与状态机判定不许用它** —— 那些认的是 `status` 本身(error 终态的闸、
    /// 移动端快照的状态串)。
    pub fn display_status(&self) -> PaneStatus {
        if self.status == PaneStatus::Error && self.clean_exit {
            PaneStatus::Idle
        } else {
            self.status
        }
    }

    /// 状态灯画哪一档(四态 + attention,见 [`StatusLight`])。
    ///
    /// 页签、折叠标题条、悬停预览、项目行、切换器、终端列表竖条、边条徽标全从这里取
    /// (聚合走 [`SplitNode::highest_light_by`],同样逐 pane 调它),干净退出在这一处
    /// 压成灰灯,各处同一口径。
    pub fn light(&self) -> StatusLight {
        StatusLight::of(self.display_status(), self.attention)
    }
}

/// 一个「项目级终端面板」:项目下的一个独立终端工作面,自带一整棵分屏树,
/// 面板之间互不影响(VS Code 终端面板右侧列表的那层语义)。
///
/// 对应磁盘格式的 `SavedTab` —— GPUI 迁移期这一层曾被收成单元素数组
/// (persist.rs 旧注释「项目级 tab 层早已删除」),现按原语义复活;
/// 磁盘格式本来就是 `tabs[]` + `activeTabIndex`,一个字都不用改。
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectPanel {
    /// 运行时 id(`panel-N`),与叶子/split 的 id 同理**不落盘**。
    pub id: String,
    /// 自定义名。`None` = 界面按序号显示。随布局落盘(`SavedTab.customTitle`)。
    pub custom_title: Option<String>,
    pub layout: SplitNode,
}

impl ProjectPanel {
    pub fn new(layout: SplitNode) -> Self {
        Self {
            id: gen_id("panel"),
            custom_title: None,
            layout,
        }
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

/// pane 拖拽的落点档位(`layoutOps.ts` 的 `DropZone`)。
///
/// `Center` = 并入目标叶子的 tab 栏;四边 = 在目标叶子的那个方向分出新格。
/// 几何判档(哪个坐标算哪一档)在 [`crate::dnd::pane_drop_zone`],这里只管语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropZone {
    Center,
    Left,
    Right,
    Top,
    Bottom,
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

    /// 聚合显示档位(`getHighestStatus` 再加第五档 attention):所有 pane 里最高的一档。
    pub fn highest_light(&self) -> StatusLight {
        self.highest_light_by(|light| light)
    }

    /// [`Self::highest_light`] 的「每个 pane 先过一道映射再比」版。边条全局徽标拿它
    /// 逐 pane 把 error 压成 idle([`StatusLight::error_as_idle`]):**先聚合再压**的话,
    /// 同一棵树里一个退出的 shell 与一个在跑的 AI 并存时聚合出 error、压完成了 idle,
    /// 那个 AI 就被藏掉了。
    pub fn highest_light_by(&self, map: fn(StatusLight) -> StatusLight) -> StatusLight {
        let highest = match self {
            Self::Leaf { panes, .. } => panes.iter().map(|p| map(p.light())).max(),
            Self::Split { children, .. } => children.iter().map(|c| c.highest_light_by(map)).max(),
        };
        highest.unwrap_or_default()
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

    /// 深度优先(左到右)收集所有叶子节点 —— 与 [`panes`] 同序,也就是与屏幕上
    /// 从左上到右下的排列同序。
    ///
    /// 唯一消费方是最大化时的折叠标题条(`terminal_area`):被铺满的那一格之外
    /// 的叶子按这个顺序码在底部,顺序稳定才不会因为一次重绘就跳位。
    ///
    /// [`panes`]: Self::panes
    pub fn leaves(&self) -> Vec<&SplitNode> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves<'a>(&'a self, out: &mut Vec<&'a SplitNode>) {
        match self {
            Self::Leaf { .. } => out.push(self),
            Self::Split { children, .. } => {
                for c in children {
                    c.collect_leaves(out);
                }
            }
        }
    }

    /// 收集「切到这棵树就能看见」的 pane —— 每个叶子的激活 tab(DFS 序)。
    /// 被压在后台的 tab 不在内;面板竖条的呼吸灯按这份算,后台 tab 不亮灯。
    pub fn visible_panes(&self) -> Vec<&PaneState> {
        let mut out = Vec::new();
        self.collect_visible_panes(&mut out);
        out
    }

    fn collect_visible_panes<'a>(&'a self, out: &mut Vec<&'a PaneState>) {
        match self {
            Self::Leaf {
                panes,
                active_pane_id,
                ..
            } => {
                if let Some(p) = panes
                    .iter()
                    .find(|p| &p.id == active_pane_id)
                    .or_else(|| panes.first())
                {
                    out.push(p);
                }
            }
            Self::Split { children, .. } => {
                for c in children {
                    c.collect_visible_panes(out);
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

    /// 树里**最后**一个叶子当前激活的 pane([`Self::first_active_pane`] 的镜像)。
    fn last_active_pane(&self) -> Option<&PaneState> {
        match self {
            Self::Leaf { .. } => self.first_active_pane(),
            Self::Split { children, .. } => {
                children.iter().rev().find_map(|c| c.last_active_pane())
            }
        }
    }

    /// 「当前该操作哪个 pane」:Ctrl+Shift+W / 新建 / 分屏 / 切换 / 最大化 / 关页签……
    /// 凡是靠 `target_pane` 取目标的动作都走这一条回落链,键盘焦点在不在终端上都一样:
    ///
    /// 1. 逻辑焦点 `focused`(上次聚焦的那个 pane)—— 在这棵树里才算;
    /// 2. 这个项目自己记着的上次聚焦 `remembered`:切项目回来时 `focused` 还指着别的
    ///    项目,没有这一档就会落到下一档,关掉的是最左那一组而不是眼前那一组;
    /// 3. 都不在(启动还原、从没聚焦过):第一个分组的激活页签。
    pub fn target_pane(&self, focused: Option<&str>, remembered: Option<&str>) -> Option<String> {
        [focused, remembered]
            .into_iter()
            .flatten()
            .find(|id| self.pane(id).is_some())
            .map(str::to_string)
            .or_else(|| self.first_active_pane().map(|p| p.id.clone()))
    }

    /// 关掉 `pane_id` 之后由谁接手(逻辑焦点 —— 也就是键盘焦点 —— 的去处)。在**摘之前**
    /// 的树上算:
    ///
    /// - 同组还有别的 tab:摘完之后这一组的激活 tab。规则与 [`Self::remove_pane`] 一致
    ///   (关的是激活 tab 就落到剩下的最后一个,否则激活 tab 不变),接手的正是眼前那一个;
    /// - 这一组随它一起没了:同一个父 split 里紧挨着的那一格接手 —— 优先前一格(左 / 上),
    ///   它本来就是第一格才取后一格;那一格自己又分了屏时取离它最近的叶子(前一格取最后
    ///   一片、后一格取第一片)的激活 tab。从前一律回落「第一个分组」,在右边关掉一组焦点
    ///   就跳回最左边;
    /// - 整棵树随它一起没了 / pane 不在树里:`None`。
    pub fn takeover_after_removal(&self, pane_id: &str) -> Option<String> {
        match self {
            Self::Leaf {
                panes,
                active_pane_id,
                ..
            } => {
                if !panes.iter().any(|p| p.id == pane_id) {
                    return None;
                }
                let rest: Vec<&PaneState> = panes.iter().filter(|p| p.id != pane_id).collect();
                let next = if active_pane_id == pane_id {
                    rest.last()
                } else {
                    // 激活 id 失配时照渲染的回落取第一个(`first_active_pane` 同一口径)
                    rest.iter()
                        .find(|p| &p.id == active_pane_id)
                        .or_else(|| rest.first())
                };
                next.map(|p| p.id.clone())
            }
            Self::Split { children, .. } => {
                let at = children.iter().position(|c| c.pane(pane_id).is_some())?;
                if let Some(next) = children[at].takeover_after_removal(pane_id) {
                    return Some(next);
                }
                let neighbour = match at.checked_sub(1) {
                    Some(prev) => children[prev].last_active_pane(),
                    None => children.get(at + 1)?.first_active_pane(),
                };
                neighbour.map(|p| p.id.clone())
            }
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
        self.insert_split_at(target_pane_id, direction, new_leaf, false)
    }

    /// [`insert_split`] 的带方位版(`insertSplitAt`):`before = true` 把新叶子放
    /// **第一格**,拖到目标格的左侧 / 上侧时用。
    ///
    /// `before = false`(默认分屏)刻意保持「原叶子仍在第一格」——
    /// 原版的 `getNodeKey` 稳定性承诺挂在这上面,GPUI 侧同样有价值:
    /// 叶子 id 不变,[`crate::terminal_area::TerminalArea`] 的进场动画表与
    /// `ResizableState` 就都不会因为一次分屏而重来(见那边 `wrap_pane_enter`)。
    ///
    /// [`insert_split`]: Self::insert_split
    pub fn insert_split_at(
        &mut self,
        target_pane_id: &str,
        direction: SplitDirection,
        new_leaf: SplitNode,
        before: bool,
    ) -> bool {
        // 叶子在递归里要能「借出去又拿回来」,用 Option 表达最直白:命中的那一层
        // take 走,没命中的层原样留在里面。
        let mut slot = Some(new_leaf);
        self.insert_split_inner(target_pane_id, direction, before, &mut slot);
        slot.is_none()
    }

    fn insert_split_inner(
        &mut self,
        target_pane_id: &str,
        direction: SplitDirection,
        before: bool,
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
                // 把自己换成 split(自己成为 children[0],`before` 时换成 children[1])。
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
                    if before {
                        children.push(new_leaf);
                        children.push(old);
                    } else {
                        children.push(old);
                        children.push(new_leaf);
                    }
                }
            }
            Self::Split { children, .. } => {
                for c in children.iter_mut() {
                    if new_leaf.is_none() {
                        return;
                    }
                    c.insert_split_inner(target_pane_id, direction, before, new_leaf);
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

    /// 叶内环形切 tab(`cyclePane`):`delta` 为 +1/-1。
    ///
    /// 只有一个 tab 时返回 `None`(不动),最后一个再往后回到第一个 ——
    /// Ctrl+Tab 的普遍预期。
    pub fn cycle_target(&self, from_pane_id: &str, delta: i32) -> Option<String> {
        let SplitNode::Leaf { panes, .. } = self.leaf_of_pane(from_pane_id)? else {
            return None;
        };
        if panes.len() < 2 {
            return None;
        }
        let idx = panes.iter().position(|p| p.id == from_pane_id)?;
        let len = panes.len() as i32;
        // delta 可能是任意整数,先取模再加一圈保证非负
        let next = ((idx as i32 + delta) % len + len) % len;
        Some(panes[next as usize].id.clone())
    }

    /// 叶内第 `index` 个 tab(**1-based**,`selectPaneByIndex`)。越界返回 `None`。
    pub fn pane_at_index(&self, from_pane_id: &str, index: usize) -> Option<String> {
        let SplitNode::Leaf { panes, .. } = self.leaf_of_pane(from_pane_id)? else {
            return None;
        };
        panes.get(index.checked_sub(1)?).map(|p| p.id.clone())
    }

    /// 相邻**分组**的激活 pane(Ctrl+Tab 在单页签组里的回落):按 [`Self::leaves`] 的
    /// DFS 序(与屏幕上左上到右下同序)走 `delta` 步,首尾相接。
    ///
    /// 只有一个分组 / pane 不在树里时返回 `None`。目标组取它的激活 tab(与点那一组的
    /// 终端主体同一个落点),不是第一个 tab。
    pub fn adjacent_leaf_pane(&self, from_pane_id: &str, delta: i32) -> Option<String> {
        let leaves = self.leaves();
        if leaves.len() < 2 {
            return None;
        }
        let from = leaves.iter().position(|leaf| match leaf {
            Self::Leaf { panes, .. } => panes.iter().any(|p| p.id == from_pane_id),
            Self::Split { .. } => false,
        })?;
        let len = leaves.len() as i32;
        let next = ((from as i32 + delta) % len + len) % len;
        match leaves[next as usize] {
            Self::Leaf {
                panes,
                active_pane_id,
                ..
            } => panes
                .iter()
                .find(|p| &p.id == active_pane_id)
                .or_else(|| panes.first())
                .map(|p| p.id.clone()),
            Self::Split { .. } => None,
        }
    }

    /// 把 pane 在自己那一组的 tab 栏里挪 `delta` 格(Ctrl+Shift+PageUp / PageDown)。
    /// **到头不循环**(挪不动返回 `false`、树不动);激活 tab 不变。
    pub fn move_pane_by(&mut self, pane_id: &str, delta: i32) -> bool {
        let Some(SplitNode::Leaf { panes, .. }) = self.leaf_of_pane_mut(pane_id) else {
            return false;
        };
        let Some(from) = panes.iter().position(|p| p.id == pane_id) else {
            return false;
        };
        let Some(to) = shifted_index(from, panes.len(), delta) else {
            return false;
        };
        let pane = panes.remove(from);
        panes.insert(to, pane);
        true
    }

    /// 键盘调分屏比例的落点:从 pane 往上找**最近**的一条、沿 `axis` 切、位于 pane
    /// `forward` 一侧(→ / ↓ 为 `true`)的分隔线,返回它所在 split 的 id 与分隔线下标
    /// (第 `i` 条在 `children[i]` 与 `children[i + 1]` 之间)。
    ///
    /// 「最近」= 嵌套最深的那层:`[a | [b | c]]` 里 b 往右是 b|c 那条,不是 a|… 那条。
    /// 那一侧已经贴着区域边缘(最右一格往右)返回 `None`,调用方据此放行按键。
    pub fn boundary_toward(
        &self,
        pane_id: &str,
        axis: SplitDirection,
        forward: bool,
    ) -> Option<(String, usize)> {
        let Self::Split {
            id,
            direction,
            children,
            ..
        } = self
        else {
            return None;
        };
        let at = children.iter().position(|c| c.pane(pane_id).is_some())?;
        if let Some(inner) = children[at].boundary_toward(pane_id, axis, forward) {
            return Some(inner);
        }
        if *direction != axis {
            return None;
        }
        if forward {
            (at + 1 < children.len()).then(|| (id.clone(), at))
        } else {
            at.checked_sub(1).map(|b| (id.clone(), b))
        }
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

    // ─── pane 拖拽移动 / 合并 / 重排(v0.14.0 / 原版 PR #49)─────────
    //
    // 两个入口都取 `&self` 返回新树,而不是 `self` 就地改 —— 与 `remove_pane`
    // 刻意不同。理由:它们都有「落回原位 = 什么也不做」的返回 `None` 语义,
    // 若按值消费,一次 no-op 就把调用方手上那棵树吃掉了(store 里 layout 是
    // `Option<SplitNode>`,take 出来再拿不回去)。多一次整树 clone 的代价可以忽略
    // —— 一个项目的布局树最多几十个节点,而这是**用户松手那一下**才跑一次的路径。

    /// 把已有 pane 移到目标 pane 所在的位置(拖拽移动 / 合并),对应
    /// `layoutOps.ts::movePaneInLayout`。
    ///
    /// 返回新树;`None` = 不需要变化(拖回原位),调用方据此**跳过写入**。
    ///
    /// # 锚点修正
    ///
    /// 目标锚 pane 恰好是被拖的 pane 自己时(拖到自己所在格的终端区上),换用
    /// 同格另一个 pane 做锚 —— 「先摘除再插入」两步里,锚 pane 必须在摘除之后
    /// 仍然存在。同格再没有别人(独占一格的 pane 拖回自己身上)就是纯 no-op:
    /// center 无事可做,四边等价于原位。
    pub fn move_pane_in_layout(
        &self,
        pane_id: &str,
        target_pane_id: &str,
        zone: DropZone,
    ) -> Option<SplitNode> {
        let pane = self.pane(pane_id)?.clone();
        let source_leaf_id = self.leaf_of_pane(pane_id)?.id().to_string();
        let target_leaf = self.leaf_of_pane(target_pane_id)?;
        let target_leaf_id = target_leaf.id().to_string();

        let anchor_id = if target_pane_id == pane_id {
            let Self::Leaf { panes, .. } = target_leaf else {
                return None;
            };
            // 独占一格 → 找不到别的锚 → no-op
            panes.iter().find(|p| p.id != pane_id)?.id.clone()
        } else {
            target_pane_id.to_string()
        };

        if zone == DropZone::Center {
            // 已经在同一格里了,并进去是空操作
            if source_leaf_id == target_leaf_id {
                return None;
            }
            let mut next = self.clone().remove_pane(pane_id)?;
            let leaf_id = next.leaf_of_pane(&anchor_id)?.id().to_string();
            let Some(Self::Leaf {
                panes,
                active_pane_id,
                ..
            }) = next.node_mut(&leaf_id)
            else {
                return None;
            };
            // 并入 tab 栏**末尾**并激活(原版 `[...leaf.panes, pane]`)
            *active_pane_id = pane.id.clone();
            panes.push(pane);
            return Some(next);
        }

        let mut next = self.clone().remove_pane(pane_id)?;
        // 摘除可能把锚所在的叶子一并收走(锚与被拖 pane 同格且只剩这一个时),
        // 那种情况下没有可插入的位置 —— 原版同一道 `findPaneById(removed, anchorId)` 闸
        if next.pane(&anchor_id).is_none() {
            return None;
        }
        let direction = match zone {
            DropZone::Left | DropZone::Right => SplitDirection::Horizontal,
            _ => SplitDirection::Vertical,
        };
        let before = matches!(zone, DropZone::Left | DropZone::Top);
        next.insert_split_at(&anchor_id, direction, SplitNode::leaf(pane), before);
        Some(next)
    }

    /// 把 pane 挪到锚点所在叶子 tab 栏的第 `index` 位(拖到 tab 栏的精确落位),
    /// 对应 `layoutOps.ts::movePaneToTabIndex`。
    ///
    /// - **同一叶子**:纯重排。先摘掉自己会让右侧的插入位左移一格,所以
    ///   `index > from` 时插入位要减一;换算后落回原位 → 返回 `None`
    ///   (原位与「紧邻自己右侧」这两个插入位都是 no-op)。
    /// - **跨叶子**:先从原处摘除,再按 `index` 插进去并激活;`index` 越界钳到末尾。
    ///
    /// `anchor_pane_id` 只用来定位目标叶子,**不能是被拖的 pane 自己**
    /// (调用方保证:tab 栏落点在本组只有这一个 tab 时压根不给指示线)。
    pub fn move_pane_to_tab_index(
        &self,
        pane_id: &str,
        anchor_pane_id: &str,
        index: usize,
    ) -> Option<SplitNode> {
        let pane = self.pane(pane_id)?.clone();
        let target_leaf = self.leaf_of_pane(anchor_pane_id)?;
        let target_leaf_id = target_leaf.id().to_string();
        let Self::Leaf { panes, .. } = target_leaf else {
            return None;
        };
        let same_leaf_from = panes.iter().position(|p| p.id == pane_id);

        if let Some(from) = same_leaf_from {
            let to = if index > from { index - 1 } else { index };
            if to == from {
                return None;
            }
            let mut next = self.clone();
            let Some(Self::Leaf {
                panes,
                active_pane_id,
                ..
            }) = next.node_mut(&target_leaf_id)
            else {
                return None;
            };
            panes.retain(|p| p.id != pane_id);
            let at = to.min(panes.len());
            panes.insert(at, pane);
            *active_pane_id = pane_id.to_string();
            return Some(next);
        }

        let mut next = self.clone().remove_pane(pane_id)?;
        let leaf_id = next.leaf_of_pane(anchor_pane_id)?.id().to_string();
        let Some(Self::Leaf {
            panes,
            active_pane_id,
            ..
        }) = next.node_mut(&leaf_id)
        else {
            return None;
        };
        let at = index.min(panes.len());
        panes.insert(at, pane);
        *active_pane_id = pane_id.to_string();
        Some(next)
    }

    /// 按 ptyId 更新状态(`updatePaneStatus`)。
    ///
    /// 回到 idle/error = AI 会话不复存在,连带清掉会话身份与识别到的 agent ——
    /// 否则用户主动退出 claude 之后,下次启动又会被 resume 回来。
    ///
    /// 从 AI 态落下来时 OSC 标题一并作废:那多半是 AI 自己写进去的(`✳ Claude Code`
    /// 之类),会话期间被副段的闸挡着看不见,会话一结束副段回来,不少 shell 又不会
    /// 自己重设标题 —— 留着就是一条过期的 AI 标题挂在页签上。**只在从 AI 态落下时清**:
    /// 新 pane 首轮轮询那次 idle → idle 不能把 shell 刚报上来的标题抹掉。
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
        let was_ai = matches!(pane.status, PaneStatus::AiIdle | PaneStatus::AiWorking);
        pane.status = status;
        pane.attention = attention;
        match status {
            PaneStatus::Idle | PaneStatus::Error => {
                pane.ai_session = None;
                pane.resume_pending = false;
                pane.detected_agent = None;
                if was_ai {
                    pane.osc_title = None;
                }
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

/// 列表里第 `from` 项挪 `delta` 格后的下标。**不循环**:挪出两端、`delta == 0`、`from`
/// 越界都返回 `None`(调用方据此什么都不做)。
pub fn shifted_index(from: usize, len: usize, delta: i32) -> Option<usize> {
    if from >= len || delta == 0 {
        return None;
    }
    let to = from as i64 + delta as i64;
    (0..len as i64).contains(&to).then_some(to as usize)
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

    /// 状态串的两个方向必须一一对应:`as_str` 是移动端快照的产出口、
    /// `from_str` 是 hook/monitor 的入口,任一侧改字面量而另一侧没跟上,
    /// 手机上的状态徽章就会静默停在 idle。
    #[test]
    fn 状态串两个方向可往返() {
        for status in [
            PaneStatus::Idle,
            PaneStatus::AiIdle,
            PaneStatus::AiWorking,
            PaneStatus::Error,
        ] {
            assert_eq!(PaneStatus::from_str(status.as_str()), Some(status));
        }
        // 反向:后端口径里的四个字面量都认得
        for s in ["idle", "ai-idle", "ai-working", "error"] {
            assert_eq!(PaneStatus::from_str(s).unwrap().as_str(), s);
        }
    }

    /// getHighestStatus 加第五档:error > attention > ai-working > ai-idle > idle,跨层聚合。
    #[test]
    fn 状态聚合按优先级取最高() {
        let mut root = leaf("a", 1);
        root.insert_split("", SplitDirection::Horizontal, leaf("b", 2)); // 不命中,不该变
        assert!(matches!(root, SplitNode::Leaf { .. }));

        let target = root.panes()[0].id.clone();
        assert!(root.insert_split(&target, SplitDirection::Horizontal, leaf("b", 2)));

        assert_eq!(root.highest_light(), StatusLight::Idle);
        root.pane_by_pty_mut(2).unwrap().status = PaneStatus::AiIdle;
        assert_eq!(root.highest_light(), StatusLight::AiIdle);
        root.pane_by_pty_mut(1).unwrap().status = PaneStatus::AiWorking;
        assert_eq!(root.highest_light(), StatusLight::AiWorking);
        // 另一格在等授权:「等你处理」压过「在跑」
        root.pane_by_pty_mut(2).unwrap().attention = true;
        assert_eq!(root.highest_light(), StatusLight::Attention);
        root.pane_by_pty_mut(2).unwrap().status = PaneStatus::Error;
        assert_eq!(root.highest_light(), StatusLight::Error);
    }

    /// 第五档怎么叠出来:attention 只在两个 AI 态上生效,error 永远最高。
    #[test]
    fn 显示档位由状态叠_attention_得出() {
        use PaneStatus::*;
        let cases = [
            (Idle, false, StatusLight::Idle),
            (AiIdle, false, StatusLight::AiIdle),
            (AiWorking, false, StatusLight::AiWorking),
            (Error, false, StatusLight::Error),
            // Claude 等授权 = ai-idle + attention;Codex 等授权 = ai-working + attention
            (AiIdle, true, StatusLight::Attention),
            (AiWorking, true, StatusLight::Attention),
            // 死掉的终端 / 裸 shell 不挂「等你处理」
            (Error, true, StatusLight::Error),
            (Idle, true, StatusLight::Idle),
        ];
        for (status, attention, expected) in cases {
            assert_eq!(
                StatusLight::of(status, attention),
                expected,
                "{status:?} + {attention}"
            );
        }
        assert_eq!(
            StatusLight::from(AiWorking),
            StatusLight::AiWorking,
            "四态直转不带 attention"
        );
        // 声明序即优先级
        assert!(StatusLight::Error > StatusLight::Attention);
        assert!(StatusLight::Attention > StatusLight::AiWorking);
        assert!(StatusLight::AiWorking > StatusLight::AiIdle);
        assert!(StatusLight::AiIdle > StatusLight::Idle);
        // 聚合时压红:只有 error 被压,其余原样
        assert_eq!(StatusLight::Error.error_as_idle(), StatusLight::Idle);
        assert_eq!(
            StatusLight::Attention.error_as_idle(),
            StatusLight::Attention
        );
    }

    /// 退出码 0 的终端按灰灯画:状态本身仍是 error(协议口径与终态闸不动),
    /// 只有显示层(单 pane 的灯与跨层聚合)压成 idle;非 0 退出 / 起不来照旧红灯。
    #[test]
    fn 干净退出的终端按灰灯画() {
        let mut root = leaf("a", 1);
        let target = root.panes()[0].id.clone();
        assert!(root.insert_split(&target, SplitDirection::Horizontal, leaf("b", 2)));

        let exited = root.pane_by_pty_mut(2).unwrap();
        exited.status = PaneStatus::Error;
        exited.clean_exit = true;
        assert_eq!(exited.status, PaneStatus::Error, "状态本身不改");
        assert_eq!(exited.display_status(), PaneStatus::Idle);
        assert_eq!(exited.light(), StatusLight::Idle);
        assert_eq!(root.highest_light(), StatusLight::Idle, "聚合同一口径");

        // 另一格在跑 AI:聚合照旧取它
        root.pane_by_pty_mut(1).unwrap().status = PaneStatus::AiWorking;
        assert_eq!(root.highest_light(), StatusLight::AiWorking);

        // 非 0 退出 / 起不来:红灯照旧压过一切
        root.pane_by_pty_mut(2).unwrap().clean_exit = false;
        assert_eq!(root.pane_by_pty_mut(2).unwrap().light(), StatusLight::Error);
        assert_eq!(root.highest_light(), StatusLight::Error);

        // `clean_exit` 只在 error 上生效:活着的 pane 残留这一位也不影响显示
        let mut alive = PaneState::new("pwsh");
        alive.status = PaneStatus::AiIdle;
        alive.clean_exit = true;
        assert_eq!(alive.display_status(), PaneStatus::AiIdle);
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

    /// tab 品牌图标的显示条件,逐条对照 `aiResume.ts::paneShowsAiSession`。
    #[test]
    fn 是否显示_ai_会话身份() {
        let mut p = PaneState::new("pwsh");
        assert!(!p.shows_ai_session(true), "光有 idle 状态不算");

        // AI 态一律算
        p.status = PaneStatus::AiWorking;
        assert!(p.shows_ai_session(false));
        p.status = PaneStatus::AiIdle;
        assert!(p.shows_ai_session(false));

        // 非 AI 态时看会话身份 + 待续接标记
        p.status = PaneStatus::Idle;
        p.ai_session = Some(AiSessionRef {
            agent: Some("codex".into()),
            session_id: "s1".into(),
            cwd: None,
        });
        assert!(p.shows_ai_session(false), "有身份且不待续接 → 算");
        p.resume_pending = true;
        assert!(
            !p.shows_ai_session(false),
            "待续接 + 自动续接关着 → 不算(挂图标会让人以为 AI 在跑)"
        );
        assert!(p.shows_ai_session(true), "待续接 + 自动续接开着 → 算");

        // agent 取值:hook 上报优先于输入检测
        p.detected_agent = Some("claude".into());
        assert_eq!(p.ai_agent(), Some("codex"));
        p.ai_session = None;
        assert_eq!(p.ai_agent(), Some("claude"));
    }

    /// AI 会话结束时它写进标题的那条随之作废;没进过 AI 的 idle → idle 不动标题。
    #[test]
    fn ai_会话结束清掉它写的标题() {
        let mut root = leaf("a", 1);
        root.pane_by_pty_mut(1).unwrap().osc_title = Some("~/repo".into());
        // 新 pane 首轮轮询的 idle → idle:shell 刚报的标题要留着
        root.update_status_by_pty(1, PaneStatus::Idle, false, None);
        assert_eq!(
            root.pane_by_pty(1).unwrap().osc_title.as_deref(),
            Some("~/repo")
        );

        root.update_status_by_pty(1, PaneStatus::AiWorking, false, Some("claude"));
        root.pane_by_pty_mut(1).unwrap().osc_title = Some("✳ Claude Code".into());
        root.update_status_by_pty(1, PaneStatus::Idle, false, None);
        assert_eq!(
            root.pane_by_pty(1).unwrap().osc_title,
            None,
            "过期的 AI 标题不留"
        );
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

    /// cyclePane:叶内环形前后切,最后一个再往后回到第一个。
    #[test]
    fn 叶内环形切_tab() {
        let mut root = leaf("a", 1);
        root.append_pane(None, pane("b", 2));
        root.append_pane(None, pane("c", 3));
        let ids: Vec<String> = root.panes().iter().map(|p| p.id.clone()).collect();

        assert_eq!(root.cycle_target(&ids[0], 1).as_ref(), Some(&ids[1]));
        assert_eq!(root.cycle_target(&ids[2], 1).as_ref(), Some(&ids[0]), "环形回头");
        assert_eq!(root.cycle_target(&ids[0], -1).as_ref(), Some(&ids[2]), "环形往前");
        assert_eq!(root.cycle_target(&ids[1], -1).as_ref(), Some(&ids[0]));
        assert_eq!(root.cycle_target("不存在的 pane", 1), None);
    }

    /// 只有一个 tab 时 Ctrl+Tab 什么也不做(不是切到自己)。
    #[test]
    fn 单_tab_不切() {
        let root = leaf("a", 1);
        let id = root.panes()[0].id.clone();
        assert_eq!(root.cycle_target(&id, 1), None);
        assert_eq!(root.cycle_target(&id, -1), None);
    }

    /// 分屏之后 cycle 只在**自己那一格**里绕,不会跳到隔壁格。
    #[test]
    fn 环形切_tab_不跨格() {
        let mut root = leaf("a", 1);
        let a = root.panes()[0].id.clone();
        root.insert_split(&a, SplitDirection::Horizontal, leaf("b", 2));
        let b = root.pane_by_pty(2).unwrap().id.clone();
        root.append_pane(Some(&b), pane("c", 3));

        // 左格只有一个 tab —— 不动
        assert_eq!(root.cycle_target(&a, 1), None);
        // 右格两个 tab —— 在这两个之间绕
        let c = root.pane_by_pty(3).unwrap().id.clone();
        assert_eq!(root.cycle_target(&b, 1).as_deref(), Some(c.as_str()));
        assert_eq!(root.cycle_target(&c, 1).as_deref(), Some(b.as_str()));
    }

    /// selectPaneByIndex:1-based,越界返回 None。
    #[test]
    fn 按序号选叶内_tab() {
        let mut root = leaf("a", 1);
        root.append_pane(None, pane("b", 2));
        let ids: Vec<String> = root.panes().iter().map(|p| p.id.clone()).collect();

        assert_eq!(root.pane_at_index(&ids[1], 1).as_ref(), Some(&ids[0]));
        assert_eq!(root.pane_at_index(&ids[0], 2).as_ref(), Some(&ids[1]));
        assert_eq!(root.pane_at_index(&ids[0], 3), None, "越界不动");
        assert_eq!(root.pane_at_index(&ids[0], 0), None, "0 不是合法序号");
    }

    /// 退出 AI 会话(idle/error)连带清掉待续接标记 —— 否则下次启动又被 resume 回来。
    #[test]
    fn 状态回到_idle_时清掉待续接标记() {
        let mut root = leaf("a", 1);
        {
            let p = root.pane_by_pty_mut(1).unwrap();
            p.ai_session = Some(AiSessionRef {
                agent: Some("claude".into()),
                session_id: "s1".into(),
                cwd: None,
            });
            p.resume_pending = true;
        }
        root.update_status_by_pty(1, PaneStatus::AiWorking, false, None);
        assert!(root.pane_by_pty(1).unwrap().resume_pending, "AI 态不清标记");

        root.update_status_by_pty(1, PaneStatus::Idle, false, None);
        assert!(!root.pane_by_pty(1).unwrap().resume_pending);
    }

    // ─── pane 拖拽移动 / 合并 / 重排 ───────────────────────────
    //
    // 以下用例逐条照抄原版 `tests/paneLayoutOps.test.cjs`(15 例)。
    // TS 侧的 pane 用字面量 id('a'/'b'/…),这里 id 是 `gen_id` 生成的,
    // 于是拿 `shell_name` 当**名牌**:`names()` 对应 TS 的 `paneIds()`,
    // `id_of()` 把名牌翻回真 id 喂给被测函数。

    /// 一组名牌 → 一个叶子(第一个是激活项,与 TS 的 `leaf(ids)` 同默认)。
    fn leaf_of(names: &[&str]) -> SplitNode {
        let panes: Vec<PaneState> = names.iter().map(|n| PaneState::new(*n)).collect();
        let active = panes[0].id.clone();
        SplitNode::Leaf {
            id: gen_id("leaf"),
            panes,
            active_pane_id: active,
        }
    }

    /// 手工拼一个 split(均分),对应 TS 的 `split(direction, children)`。
    fn split_of(direction: SplitDirection, children: Vec<SplitNode>) -> SplitNode {
        let n = children.len();
        SplitNode::Split {
            id: gen_id("split"),
            direction,
            sizes: vec![100.0 / n as f64; n],
            children,
        }
    }

    /// `visible_panes()`:每个叶子只出激活 tab,后台 tab 不在内;
    /// 分屏的各格都算「看得见」。
    #[test]
    fn 可见pane只含各叶子的激活tab() {
        // 叶子 a 有两个 tab,激活第二个;叶子 b 单 tab —— 可见 = [a2, b]
        let mut la = leaf("a1", 1);
        la.append_pane(None, pane("a2", 2));
        let a2 = la.panes()[1].id.clone();
        la.activate_pane(&a2);
        let root = split_of(SplitDirection::Horizontal, vec![la, leaf("b", 3)]);

        let visible: Vec<&str> = root
            .visible_panes()
            .iter()
            .map(|p| p.shell_name.as_str())
            .collect();
        assert_eq!(visible, ["a2", "b"], "后台 tab a1 不该出现");
    }

    /// `ProjectPanel::new`:运行时 id 逐个唯一、初始无自定义名。
    #[test]
    fn 面板构造带唯一id且无自定义名() {
        let a = ProjectPanel::new(leaf("a", 1));
        let b = ProjectPanel::new(leaf("b", 2));
        assert_ne!(a.id, b.id);
        assert!(a.id.starts_with("panel-"));
        assert_eq!(a.custom_title, None);
    }

    /// `gen_id` 的形状是 `{prefix}-{毫秒}-{计数}`:时间戳段是跨进程唯一的依据
    /// (见函数注释里那桩「新项目顶掉旧项目」的事故),不能退化回 `{prefix}-{计数}`。
    #[test]
    fn gen_id带毫秒时间戳段() {
        let id = gen_id("proj");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 3, "形状应为 proj-<millis>-<n>: {id}");
        assert_eq!(parts[0], "proj");
        let millis: u128 = parts[1].parse().expect("中段是毫秒时间戳");
        assert!(millis > 1_600_000_000_000, "毫秒时间戳量级不对: {millis}");
        parts[2].parse::<u64>().expect("末段是计数");
        assert_ne!(gen_id("proj"), id, "同前缀连续生成互不相同");
    }

    /// `gen_unique_id`:被占用的 id 会被跳过,直到拿到一个没人用的。
    #[test]
    fn gen_unique_id跳过已占用的id() {
        let mut seen: Vec<String> = Vec::new();
        for _ in 0..3 {
            let id = gen_unique_id("proj", |candidate| seen.iter().any(|s| s == candidate));
            assert!(!seen.contains(&id));
            seen.push(id);
        }
        // 把「除第一次外全部占用」翻过来:只放行带特定后缀的,验证确实会重试
        let taken = |candidate: &str| !candidate.ends_with('7');
        let id = gen_unique_id("x", taken);
        assert!(id.ends_with('7'), "应一直重试到计数末位为 7: {id}");
    }

    /// 深度优先的名牌序列(TS 的 `paneIds(node)`)。
    fn names(node: &SplitNode) -> Vec<String> {
        node.panes().iter().map(|p| p.shell_name.clone()).collect()
    }

    /// 名牌 → 真 id。
    fn id_of(node: &SplitNode, name: &str) -> String {
        node.panes()
            .into_iter()
            .find(|p| p.shell_name == name)
            .unwrap_or_else(|| panic!("树里没有名牌 {name}"))
            .id
            .clone()
    }

    /// 各子节点的名牌序列(TS 的 `next.children.map(paneIds)`)。
    fn child_names(node: &SplitNode) -> Vec<Vec<String>> {
        match node {
            SplitNode::Split { children, .. } => children.iter().map(names).collect(),
            _ => panic!("不是 split"),
        }
    }

    fn active_of(node: &SplitNode) -> String {
        match node {
            SplitNode::Leaf { active_pane_id, .. } => active_pane_id.clone(),
            _ => panic!("不是叶子"),
        }
    }

    // ===== leaves =====

    /// 折叠标题条按这个序码,所以顺序必须与 `panes()` 的深度优先序一致。
    #[test]
    fn 叶子收集与_panes_同序() {
        let root = split_of(
            SplitDirection::Horizontal,
            vec![
                leaf_of(&["a", "b"]),
                split_of(
                    SplitDirection::Vertical,
                    vec![leaf_of(&["c"]), leaf_of(&["d", "e"])],
                ),
            ],
        );
        let leaves = root.leaves();
        assert_eq!(leaves.len(), 3);
        assert_eq!(
            leaves.iter().map(|l| names(l)).collect::<Vec<_>>(),
            vec![vec!["a", "b"], vec!["c"], vec!["d", "e"]]
        );
        // 单格布局:自己就是唯一的叶子(最大化在这种树上不成立,但不许 panic)
        assert_eq!(leaf_of(&["a"]).leaves().len(), 1);
    }

    // ===== insert_split_at =====

    #[test]
    fn 分屏_before_把新叶子放第一格() {
        let mut root = leaf_of(&["a"]);
        let a = id_of(&root, "a");
        assert!(root.insert_split_at(&a, SplitDirection::Horizontal, leaf_of(&["b"]), true));
        assert!(matches!(root, SplitNode::Split { .. }));
        assert_eq!(child_names(&root), vec![vec!["b"], vec!["a"]]);
    }

    /// `after` 保持原叶子在第一格 —— 叶子 id 稳定性的前提(见 `insert_split_at` 注释)。
    #[test]
    fn 分屏_after_保持原叶子在第一格() {
        let mut root = leaf_of(&["a"]);
        let a = id_of(&root, "a");
        let leaf_id = root.id().to_string();
        assert!(root.insert_split_at(&a, SplitDirection::Vertical, leaf_of(&["b"]), false));
        assert_eq!(child_names(&root), vec![vec!["a"], vec!["b"]]);
        match &root {
            SplitNode::Split {
                direction, children, ..
            } => {
                assert_eq!(*direction, SplitDirection::Vertical);
                assert_eq!(children[0].id(), leaf_id, "原叶子 id 不变");
            }
            _ => panic!(),
        }
    }

    // ===== move_pane_in_layout:四边分屏落点 =====

    #[test]
    fn 拖到右侧在目标格右边分出新格并塌陷源格() {
        let root = split_of(
            SplitDirection::Horizontal,
            vec![leaf_of(&["a"]), leaf_of(&["b"])],
        );
        let (a, b) = (id_of(&root, "a"), id_of(&root, "b"));
        let next = root.move_pane_in_layout(&a, &b, DropZone::Right).unwrap();
        // a 那一格空了 → 塌陷;b 处分裂为 [b, a]
        assert_eq!(names(&next), vec!["b", "a"]);
        assert!(matches!(next, SplitNode::Split { .. }));
    }

    #[test]
    fn 拖到上侧新格在前且方向为纵向() {
        let root = split_of(
            SplitDirection::Horizontal,
            vec![leaf_of(&["a"]), leaf_of(&["b"])],
        );
        let (a, b) = (id_of(&root, "a"), id_of(&root, "b"));
        let next = root.move_pane_in_layout(&a, &b, DropZone::Top).unwrap();
        match &next {
            SplitNode::Split { direction, .. } => {
                assert_eq!(*direction, SplitDirection::Vertical)
            }
            _ => panic!("应是 split"),
        }
        assert_eq!(child_names(&next), vec![vec!["a"], vec!["b"]]);
    }

    #[test]
    fn center_并入目标格末尾并激活() {
        let root = split_of(
            SplitDirection::Horizontal,
            vec![leaf_of(&["a"]), leaf_of(&["b", "c"])],
        );
        let (a, b) = (id_of(&root, "a"), id_of(&root, "b"));
        let a_id = a.clone();
        let next = root.move_pane_in_layout(&a, &b, DropZone::Center).unwrap();
        // 源格塌陷后整棵树只剩一个叶子
        assert!(matches!(next, SplitNode::Leaf { .. }));
        assert_eq!(names(&next), vec!["b", "c", "a"]);
        assert_eq!(active_of(&next), a_id);
    }

    #[test]
    fn center_拖回自己所在组是空操作() {
        let root = split_of(
            SplitDirection::Horizontal,
            vec![leaf_of(&["a", "b"]), leaf_of(&["c"])],
        );
        let (a, b) = (id_of(&root, "a"), id_of(&root, "b"));
        assert!(root.move_pane_in_layout(&a, &b, DropZone::Center).is_none());
    }

    #[test]
    fn 独占一格的_pane_拖自己身上四边也是空操作() {
        let root = split_of(
            SplitDirection::Horizontal,
            vec![leaf_of(&["a"]), leaf_of(&["b"])],
        );
        let a = id_of(&root, "a");
        for zone in [
            DropZone::Left,
            DropZone::Right,
            DropZone::Top,
            DropZone::Bottom,
            DropZone::Center,
        ] {
            assert!(
                root.move_pane_in_layout(&a, &a, zone).is_none(),
                "zone={zone:?}"
            );
        }
    }

    /// 锚点是被拖 pane 自己 → 换锚:从多 tab 组里把自己拆出去。
    #[test]
    fn 锚点是自己时换锚从多_tab_组拆出去() {
        let root = leaf_of(&["a", "b"]);
        let a = id_of(&root, "a");
        let next = root.move_pane_in_layout(&a, &a, DropZone::Right).unwrap();
        assert!(matches!(next, SplitNode::Split { .. }));
        assert_eq!(child_names(&next), vec![vec!["b"], vec!["a"]]);
    }

    /// 任意落点前后 pane 集合一致 —— 移动路径上一个 pane 都不许丢。
    #[test]
    fn 移动不丢_pane() {
        let root = split_of(
            SplitDirection::Vertical,
            vec![
                split_of(
                    SplitDirection::Horizontal,
                    vec![leaf_of(&["a", "b"]), leaf_of(&["c"])],
                ),
                leaf_of(&["d"]),
            ],
        );
        let (a, d) = (id_of(&root, "a"), id_of(&root, "d"));
        for zone in [
            DropZone::Left,
            DropZone::Right,
            DropZone::Top,
            DropZone::Bottom,
            DropZone::Center,
        ] {
            let next = root.move_pane_in_layout(&a, &d, zone).unwrap();
            let mut got = names(&next);
            got.sort();
            assert_eq!(got, vec!["a", "b", "c", "d"], "zone={zone:?}");
        }
    }

    // ===== move_pane_to_tab_index:tab 栏按位落子 =====

    #[test]
    fn 同组重排拖到右侧插入位左移补位() {
        let root = leaf_of(&["a", "b", "c"]);
        let (a, b) = (id_of(&root, "a"), id_of(&root, "b"));
        let next = root.move_pane_to_tab_index(&a, &b, 2).unwrap();
        assert_eq!(names(&next), vec!["b", "a", "c"]);
        assert_eq!(active_of(&next), a);
    }

    #[test]
    fn 同组重排拖到最左与最右() {
        let root = leaf_of(&["a", "b", "c"]);
        let (a, b, c) = (id_of(&root, "a"), id_of(&root, "b"), id_of(&root, "c"));
        assert_eq!(
            names(&root.move_pane_to_tab_index(&c, &a, 0).unwrap()),
            vec!["c", "a", "b"]
        );
        assert_eq!(
            names(&root.move_pane_to_tab_index(&a, &b, 3).unwrap()),
            vec!["b", "c", "a"]
        );
    }

    /// 原位与「紧邻自己右侧」这两个插入位落下都没有动作。
    #[test]
    fn 同组重排落回原位与紧邻右侧均为空操作() {
        let root = leaf_of(&["a", "b", "c"]);
        let (a, b) = (id_of(&root, "a"), id_of(&root, "b"));
        assert!(root.move_pane_to_tab_index(&b, &a, 1).is_none());
        assert!(root.move_pane_to_tab_index(&b, &a, 2).is_none());
    }

    #[test]
    fn 跨组按位插入源格塌陷并激活() {
        let root = split_of(
            SplitDirection::Horizontal,
            vec![leaf_of(&["a"]), leaf_of(&["b", "c"])],
        );
        let (a, b) = (id_of(&root, "a"), id_of(&root, "b"));
        let next = root.move_pane_to_tab_index(&a, &b, 1).unwrap();
        assert!(matches!(next, SplitNode::Leaf { .. }));
        assert_eq!(names(&next), vec!["b", "a", "c"]);
        assert_eq!(active_of(&next), a);
    }

    #[test]
    fn 跨组插入下标越界钳到末尾() {
        let root = split_of(
            SplitDirection::Horizontal,
            vec![leaf_of(&["a"]), leaf_of(&["b"])],
        );
        let (a, b) = (id_of(&root, "a"), id_of(&root, "b"));
        let next = root.move_pane_to_tab_index(&a, &b, 99).unwrap();
        assert_eq!(names(&next), vec!["b", "a"]);
    }

    #[test]
    fn tab_栏落子不丢_pane() {
        let root = split_of(
            SplitDirection::Vertical,
            vec![leaf_of(&["a", "b"]), leaf_of(&["c", "d"])],
        );
        let (a, c) = (id_of(&root, "a"), id_of(&root, "c"));
        for i in 0..=2 {
            let next = root.move_pane_to_tab_index(&a, &c, i).unwrap();
            let mut got = names(&next);
            got.sort();
            assert_eq!(got, vec!["a", "b", "c", "d"], "index={i}");
        }
    }

    /// 移动/重排只换布局树的位置,**pane 自身(含 pty_id)原样搬过去** ——
    /// GPUI 侧终端实体按 `pty_id` 挂在 store 的 `terminals` 表里,pty_id 不变
    /// 就意味着 PTY 不断、终端内容不重建(原版 `getNodeKey` 修复的等价保证)。
    #[test]
    fn 移动保留_pty_id() {
        let mut root = split_of(
            SplitDirection::Horizontal,
            vec![leaf_of(&["a"]), leaf_of(&["b"])],
        );
        let (a, b) = (id_of(&root, "a"), id_of(&root, "b"));
        root.pane_mut(&a).unwrap().pty_id = Some(7);
        root.pane_mut(&b).unwrap().pty_id = Some(9);

        let moved = root.move_pane_in_layout(&a, &b, DropZone::Bottom).unwrap();
        assert_eq!(moved.pane(&a).unwrap().pty_id, Some(7));
        assert_eq!(moved.pane(&b).unwrap().pty_id, Some(9));

        let reordered = root.move_pane_to_tab_index(&a, &b, 0).unwrap();
        assert_eq!(reordered.pane(&a).unwrap().pty_id, Some(7));
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

    /// Ctrl+Tab 的分组回落:按 DFS 序(左上 → 右下)走,首尾相接;落到目标组的**激活**
    /// tab;只有一组 / pane 不在树里时不动。
    #[test]
    fn 相邻分组按遍历序循环且落在激活页签() {
        // [a | [b(b1,b2,激活 b2) / c]]
        let mut b = leaf_of(&["b1", "b2"]);
        if let SplitNode::Leaf {
            panes,
            active_pane_id,
            ..
        } = &mut b
        {
            *active_pane_id = panes[1].id.clone();
        }
        let root = split_of(
            SplitDirection::Horizontal,
            vec![
                leaf_of(&["a"]),
                split_of(SplitDirection::Vertical, vec![b, leaf_of(&["c"])]),
            ],
        );
        let (a, b2, c) = (id_of(&root, "a"), id_of(&root, "b2"), id_of(&root, "c"));

        assert_eq!(
            root.adjacent_leaf_pane(&a, 1),
            Some(b2.clone()),
            "落在激活的 b2"
        );
        assert_eq!(root.adjacent_leaf_pane(&b2, 1), Some(c.clone()));
        assert_eq!(
            root.adjacent_leaf_pane(&c, 1),
            Some(a.clone()),
            "最后一组回到第一组"
        );
        assert_eq!(
            root.adjacent_leaf_pane(&a, -1),
            Some(c.clone()),
            "第一组往前是最后一组"
        );
        assert_eq!(root.adjacent_leaf_pane(&c, -1), Some(b2));

        let single = leaf_of(&["x"]);
        let x = id_of(&single, "x");
        assert_eq!(single.adjacent_leaf_pane(&x, 1), None, "只有一组");
        assert_eq!(root.adjacent_leaf_pane("pane-不在树里", 1), None);
    }

    /// 下标挪动不循环:两端挪不出去、delta 为 0、越界起点都是 `None`。
    #[test]
    fn 页签挪动下标到头不循环() {
        assert_eq!(shifted_index(0, 3, 1), Some(1));
        assert_eq!(shifted_index(2, 3, -1), Some(1));
        assert_eq!(shifted_index(2, 3, 1), None, "最右再往右");
        assert_eq!(shifted_index(0, 3, -1), None, "最左再往左");
        assert_eq!(shifted_index(1, 3, 0), None);
        assert_eq!(shifted_index(3, 3, -1), None, "起点越界");
        assert_eq!(shifted_index(0, 1, 1), None, "只有一个");
    }

    /// 页签左右移:只在本组内换位,激活 tab 不变;到头时树不动。
    #[test]
    fn 页签在组内左右移() {
        let mut root = split_of(
            SplitDirection::Horizontal,
            vec![leaf_of(&["a", "b", "c"]), leaf_of(&["d"])],
        );
        let b = id_of(&root, "b");
        let active_before = root.leaf_of_pane(&b).map(|l| match l {
            SplitNode::Leaf { active_pane_id, .. } => active_pane_id.clone(),
            _ => unreachable!(),
        });

        assert!(root.move_pane_by(&b, 1));
        assert_eq!(names(&root), vec!["a", "c", "b", "d"]);
        assert!(!root.move_pane_by(&b, 1), "已在本组最右,不跨到 d 那一组");
        assert_eq!(names(&root), vec!["a", "c", "b", "d"]);
        assert!(root.move_pane_by(&b, -1));
        assert!(root.move_pane_by(&b, -1));
        assert_eq!(names(&root), vec!["b", "a", "c", "d"]);
        assert!(!root.move_pane_by(&b, -1), "最左不循环");

        let active_after = root.leaf_of_pane(&b).map(|l| match l {
            SplitNode::Leaf { active_pane_id, .. } => active_pane_id.clone(),
            _ => unreachable!(),
        });
        assert_eq!(active_before, active_after, "挪页签不换激活 tab");
        assert!(!root.move_pane_by("pane-不在树里", 1));
    }

    /// 调比例找的是**最近**一条位于箭头那一侧、方向对得上的分隔线;贴边时没有。
    #[test]
    fn 调比例找最近的分隔线() {
        // [a | [b / c] | d]:外层横切三格,中间那格再竖切
        let inner = split_of(
            SplitDirection::Vertical,
            vec![leaf_of(&["b"]), leaf_of(&["c"])],
        );
        let inner_id = inner.id().to_string();
        let root = split_of(
            SplitDirection::Horizontal,
            vec![leaf_of(&["a"]), inner, leaf_of(&["d"])],
        );
        let root_id = root.id().to_string();
        let (a, b, c, d) = (
            id_of(&root, "a"),
            id_of(&root, "b"),
            id_of(&root, "c"),
            id_of(&root, "d"),
        );
        let (h, v) = (SplitDirection::Horizontal, SplitDirection::Vertical);

        assert_eq!(
            root.boundary_toward(&a, h, true),
            Some((root_id.clone(), 0))
        );
        assert_eq!(root.boundary_toward(&a, h, false), None, "最左一格往左贴边");
        assert_eq!(
            root.boundary_toward(&b, h, false),
            Some((root_id.clone(), 0))
        );
        assert_eq!(
            root.boundary_toward(&b, h, true),
            Some((root_id.clone(), 1))
        );
        assert_eq!(
            root.boundary_toward(&d, h, false),
            Some((root_id.clone(), 1))
        );
        assert_eq!(root.boundary_toward(&d, h, true), None);
        // 竖向:b 往下是 b/c 那条(内层),往上贴边;a 根本不在竖切里
        assert_eq!(
            root.boundary_toward(&b, v, true),
            Some((inner_id.clone(), 0))
        );
        assert_eq!(root.boundary_toward(&c, v, false), Some((inner_id, 0)));
        assert_eq!(root.boundary_toward(&b, v, false), None);
        assert_eq!(root.boundary_toward(&a, v, true), None);

        // 嵌套同向:[x | [y | z]] 里 y 往右是 y|z(最近那条),不是外层的
        let nested = split_of(h, vec![leaf_of(&["y"]), leaf_of(&["z"])]);
        let nested_id = nested.id().to_string();
        let outer = split_of(h, vec![leaf_of(&["x"]), nested]);
        let y = id_of(&outer, "y");
        assert_eq!(outer.boundary_toward(&y, h, true), Some((nested_id, 0)));
        assert_eq!(
            outer.boundary_toward(&y, h, false),
            Some((outer.id().to_string(), 0)),
            "内层往左贴边,退到外层那条"
        );

        let single = leaf_of(&["s"]);
        assert_eq!(single.boundary_toward(&id_of(&single, "s"), h, true), None);
    }

    // ===== 关闭后的接手 / 动作目标 =====

    /// 同组还有 tab:关激活 tab 由剩下的最后一个接手(与 `remove_pane` 同一规则),
    /// 关后台 tab 激活项不动 —— 接手的永远是这一组眼前那个。
    #[test]
    fn 同组还有页签时由组内激活页签接手() {
        let root = split_of(
            SplitDirection::Horizontal,
            vec![leaf_of(&["a1", "a2", "a3"]), leaf_of(&["b"])],
        );
        let (a1, a2, a3) = (id_of(&root, "a1"), id_of(&root, "a2"), id_of(&root, "a3"));
        // a1 是激活 tab:剩下的最后一个 a3 接手
        assert_eq!(root.takeover_after_removal(&a1), Some(a3.clone()));
        // 关的是后台 a2:激活的 a1 不变
        assert_eq!(root.takeover_after_removal(&a2), Some(a1));
        let after = root.clone().remove_pane(&id_of(&root, "a1")).unwrap();
        assert_eq!(
            active_of(after.leaf_of_pane(&a3).unwrap()),
            a3,
            "与摘完后的激活项对得上"
        );
    }

    /// 整组没了:同一个 split 里紧挨着的那一格接手,优先前一格;那一格又分了屏时取离得
    /// 最近的叶子。不再一律跳回最左边那一组。
    #[test]
    fn 整组关掉由相邻分组接手() {
        // [a | b | c]
        let row = split_of(
            SplitDirection::Horizontal,
            vec![leaf_of(&["a"]), leaf_of(&["b"]), leaf_of(&["c"])],
        );
        let (a, b, c) = (id_of(&row, "a"), id_of(&row, "b"), id_of(&row, "c"));
        assert_eq!(
            row.takeover_after_removal(&b),
            Some(a.clone()),
            "优先前一格"
        );
        assert_eq!(row.takeover_after_removal(&c), Some(b.clone()));
        assert_eq!(row.takeover_after_removal(&a), Some(b), "第一格取后一格");

        // [[x / y] | z]:关 z,前一格是竖切,离它最近的是最后一片 y
        let left = split_of(
            SplitDirection::Vertical,
            vec![leaf_of(&["x"]), leaf_of(&["y"])],
        );
        let root = split_of(SplitDirection::Horizontal, vec![left, leaf_of(&["z"])]);
        let (x, y, z) = (id_of(&root, "x"), id_of(&root, "y"), id_of(&root, "z"));
        assert_eq!(root.takeover_after_removal(&z), Some(y.clone()));
        // 关 x:先在它自己那个竖切里找邻居 y,不跳到外层
        assert_eq!(root.takeover_after_removal(&x), Some(y));

        // [p | [q / r]]:关 p,后一格是竖切,取第一片 q
        let right = split_of(
            SplitDirection::Vertical,
            vec![leaf_of(&["q"]), leaf_of(&["r"])],
        );
        let root = split_of(SplitDirection::Horizontal, vec![leaf_of(&["p"]), right]);
        let (p, q) = (id_of(&root, "p"), id_of(&root, "q"));
        assert_eq!(root.takeover_after_removal(&p), Some(q));
    }

    /// 邻居那一组有多个 tab 时落在它的**激活** tab 上(与点那一组的终端主体同一个落点)。
    #[test]
    fn 接手的邻组落在它的激活页签() {
        let mut b = leaf_of(&["b1", "b2"]);
        let b2 = id_of(&b, "b2");
        b.activate_pane(&b2);
        let root = split_of(SplitDirection::Horizontal, vec![b, leaf_of(&["c"])]);
        assert_eq!(root.takeover_after_removal(&id_of(&root, "c")), Some(b2));
    }

    /// 接手的 pane 摘完之后一定还在树上、而且正是某一组眼前那个 tab —— 逐个 pane 对账,
    /// 免得规则与 `remove_pane` 的塌陷 / 激活规则走偏。
    #[test]
    fn 接手的总是摘完后看得见的那个() {
        let root = split_of(
            SplitDirection::Horizontal,
            vec![
                leaf_of(&["a1", "a2"]),
                split_of(
                    SplitDirection::Vertical,
                    vec![leaf_of(&["b"]), leaf_of(&["c1", "c2", "c3"])],
                ),
                leaf_of(&["d"]),
            ],
        );
        for name in ["a1", "a2", "b", "c1", "c2", "c3", "d"] {
            let id = id_of(&root, name);
            let next = root.takeover_after_removal(&id).expect("还有别的 pane");
            let after = root.clone().remove_pane(&id).unwrap();
            assert!(
                after.visible_panes().iter().any(|p| p.id == next),
                "关 {name} 后接手的不在眼前"
            );
        }
        // 最后一个 pane / 不在树里:没人接手
        let single = leaf_of(&["s"]);
        assert_eq!(single.takeover_after_removal(&id_of(&single, "s")), None);
        assert_eq!(root.takeover_after_removal("pane-不在树里"), None);
    }

    /// 动作目标的回落链:逻辑焦点 → 项目自己记着的上次聚焦 → 第一组的激活页签。
    /// 焦点在文件树 / 项目列表时按 Ctrl+Shift+W,关的是眼前那一组。
    #[test]
    fn 动作目标先看逻辑焦点再看项目记忆() {
        let root = split_of(
            SplitDirection::Horizontal,
            vec![leaf_of(&["a"]), leaf_of(&["b"])],
        );
        let (a, b) = (id_of(&root, "a"), id_of(&root, "b"));
        assert_eq!(root.target_pane(Some(&b), None), Some(b.clone()));
        // 逻辑焦点指着别的项目(切项目回来):用这个项目自己记着的
        assert_eq!(
            root.target_pane(Some("pane-别的项目"), Some(&b)),
            Some(b.clone())
        );
        // 两者都在时逻辑焦点优先
        assert_eq!(root.target_pane(Some(&a), Some(&b)), Some(a.clone()));
        // 都不在:第一组的激活页签
        assert_eq!(root.target_pane(None, Some("pane-已关")), Some(a.clone()));
        assert_eq!(root.target_pane(None, None), Some(a));
    }
}
