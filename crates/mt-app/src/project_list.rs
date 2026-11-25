//! 左栏:项目列表。对应 `src/components/ProjectList.tsx` 的主干。
//!
//! # 领位图标
//!
//! 原版是「SSH > 技术栈 > 通用」三选一,**恒显**(每行都有图标,缩进才对得齐)。
//! BB-b 补上第一档:SSH 远程项目固定画 [`SERVER`] 服务器图标(断链时转 error 色),
//! 其后才轮到技术栈徽标([`mt_ui::icons::TechIcon`])与通用目录图标。
//!
//! 技术栈取值走 [`resolve_project_kind`]:手动 `kindOverride` 优先,没设过就用
//! [`mt_project::project_kind`] 的目录探测缓存(结果住在 store 的 `dir_kinds`,
//! 探测本身丢后台)。
//!
//! # 键盘与悬停(清尾批)
//!
//! - 每行 `track_focus` + `tab_index`(原版的 `tabIndex={0}`),Enter/Space 打开、
//!   Delete 移除(带确认)、F2 重命名;↑ / ↓ / Home / End 按可见行序(分组行与项目行
//!   混排)挪焦点并滚进可视区,到头停住。**F2 不在行上绑** —— 它与全局键位表
//!   (`renamePane`)同一条绑定,由 `main.rs` 按焦点分流,见
//!   [`ProjectList::rename_focused_row`];
//! - 悬停 250ms 弹 pane 缩略图([`crate::pane_preview`]),开闸条件是「这个项目
//!   里有 AI 会话 pane」,移出 / 按下 / 右键 / 滚动即关。
//!
//! # 分组与拖放(X 批)
//!
//! 渲染不再是 `store.projects()` 平铺,而是
//! [`project_tree::get_ordered_tree`](crate::project_tree::get_ordered_tree) 展平出来的
//! 「分组行 + 项目行 + worktree 子项目」有序表 —— 折叠、缩进、父组归属全从那里来。
//!
//! 拖放全部走 gpui 原生 drag(见 [`crate::dnd`] 的模块注释),这里只负责三件事:
//!
//! 1. `on_drag` 起拖:记下 `dragging`(源行变淡)并交出拖影实体;
//! 2. `on_drag_move` 判档:`bounds` + 鼠标 y 算出 before/inside/after 与合法性,
//!    存进 [`DropIndicator`] —— **`on_drop` 不带位置,这是唯一的传递通道**;
//! 3. `on_drop` 落地:读 indicator → `moveItem`。
//!
//! 外部资源管理器拖文件夹进来那一路(`gpui::ExternalPaths`)挂在整个面板的容器上,
//! 三态提示框与原版同构;目录判定(`filter_directories`)是阻塞 stat,一次性丢后台
//! 算完存进 `external`,`on_drag_move` 只读缓存 —— 逐帧 `is_dir()` 在网络盘上会卡死主线程。
//!
//! # 揭示(从「项目」块之外切过来的项目)
//!
//! 标题栏胶囊、Ctrl+Shift+P 切换器、toast、托盘、会话跳转、添加项目……这些入口切完
//! 活动项目,列表里的高亮跟着走,但那一行可能在可视区外、或藏在折叠的分组里。
//! 它们切完都调一次 [`reveal`]:展开挡路的折叠分组(走分组折叠同一条落盘口径,
//! 见 [`AppStore::expand_groups_to_project`])+ 登记一次滚动,下一帧渲染时由
//! [`ScrollHandle::scroll_to_item`] 把那一行滚进可视区 —— gpui 这个策略就是「最小
//! 滚动让它完整可见」:已在视口内不动,在上方贴上沿、在下方贴下沿。行高不用自己
//! 算,滚动在 prepaint 里拿这一帧的真实布局(再校准一帧,理由见 `render` 里兑现那段)。
//!
//! **列表里自己点 / 按 Enter 切的不揭示**:那一行本来就在眼前,贴边那一行还会被
//! 「补齐可见」顶一下,看着就是跳动。中栏收起时列表不在元素树里,登记的滚动留到
//! 下次展开中栏时兑现(届时还是活动项目才滚)。

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    AnyElement, App, AppContext, Bounds, ClickEvent, ClipboardItem, Context, DragMoveEvent, Entity,
    ExternalPaths, FocusHandle, Focusable, FontWeight, HighlightStyle, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Pixels, Render,
    ScrollEvent, SharedString, StatefulInteractiveElement, Styled, StyledText, Subscription, Task,
    Window, anchored, canvas, deferred, div, prelude::FluentBuilder, px,
};
use gpui::{Global, ScrollHandle};
use gpui_component::input::{Input, InputEvent, InputState, SelectAll};
use mt_config::{ProjectConfig, ProjectTreeItem};
use mt_project::project_kind::{ALL_PROJECT_KINDS, ALL_TECH_CATEGORIES, ProjectKind};
use mt_project::search::find_substring_char_ranges;
use mt_ui::icons::vector::VectorIcon;
use mt_ui::icons::{AiVendor, BrandIcon, FileIcon, StatusDot, TechIcon};
use mt_ui::tooltip::TooltipExt as _;

use crate::dnd::{
    self, DragProjectItem, DropPosition, ExternalDropKind, PreviewIcon,
};
use crate::focus_nav::{self, RowNav};
use crate::fs_ops;
use crate::git::ui::{ICON_BRANCH, ICON_WORKTREE};
use crate::i18n::{t, tr};
use crate::menu::{self, MenuEntry, MenuItem};
use crate::modal;
use crate::pane_preview::{self, MiniLayout};
use crate::project_tree::{self, MAX_DEPTH, OrderedItem, ProjectClusterRole};
use crate::store::{AppStore, StoreEvent};
use crate::terminal_area::{SwitchClick, SwitchClickGuard, click_count, click_position};
use crate::tree::StatusLight;
use crate::ui;

/// 右侧 AI 活跃胶囊里的品牌图标尺寸。
const AI_ICON_SIZE: f32 = 12.5;

/// worktree 子项行上的 AI 品牌图标尺寸。
const CHILD_AI_ICON_SIZE: f32 = 11.5;

/// 独立（已脱离父项目）worktree 徽章最多显示多宽。
const WORKTREE_BADGE_MAX_W: f32 = 104.0;

/// 远程连接名徽章最多多宽。
const REMOTE_BADGE_MAX_W: f32 = 76.0;

/// 统一水平呼吸留白（独立行 / 分组行 / worktree 微容器共用，保证整栏左右缘垂直对齐）。
const ROW_MARGIN_X: f32 = 6.0;

/// 分组嵌套的逐层水平步进。
const INDENT_STEP: f32 = 16.0;

/// 第 0 层分组折叠箭头的水平中心：`ROW_MARGIN_X(6) + pl(6) + half_chevron(6)`。
/// 组内每层引导竖线按 `GROUP_GUIDE_BASE_X + level * INDENT_STEP` 精确对齐箭头中心。
const GROUP_GUIDE_BASE_X: f32 = 18.0;

/// 常规行内左侧 padding（为绝对定位 2.5px 状态微轨留出呼吸感）。
const ROW_INNER_PL: f32 = 10.0;

/// worktree 簇微容器的外边距、描边宽与圆角。
const CLUSTER_BORDER_W: f32 = 1.0;
const CLUSTER_RADIUS: f32 = 7.0;
const CLUSTER_INNER_RADIUS: f32 = 6.0;

/// worktree 子项树形连线水平弯臂宽、弯臂与子项胶囊的留白、多层嵌套时的单层步进。
const CHILD_ELBOW_W: f32 = 10.0;
const CHILD_ELBOW_GAP: f32 = 2.0;
const CHILD_NEST_STEP: f32 = 14.0;

/// 拖放悬停在折叠分组上自动展开的停留时长（ms）。
const SPRING_OPEN_DELAY_MS: u64 = 550;

/// 单位方框（16×16 viewBox）换算。
const fn u16_norm(v: f32) -> f32 {
    v / 16.0
}
const ICON_STROKE_16: f32 = 1.35 / 16.0;

/// 放大镜（行内筛选开关与搜索框前缀）。
const ICON_SEARCH: &[mt_ui::icons::vector::Shape] = &[
    mt_ui::icons::vector::Shape::line(
        mt_ui::icons::vector::Ink::Current,
        ICON_STROKE_16,
        mt_ui::icons::vector::Geom::Circle {
            c: (u16_norm(7.0), u16_norm(7.0)),
            r: u16_norm(4.2),
        },
    ),
    mt_ui::icons::vector::Shape::line(
        mt_ui::icons::vector::Ink::Current,
        ICON_STROKE_16,
        mt_ui::icons::vector::Geom::Polyline(&[
            (u16_norm(10.2), u16_norm(10.2)),
            (u16_norm(13.8), u16_norm(13.8)),
        ]),
    ),
];

/// 加号（栏头新建菜单 & 分组行快捷添加）。
const ICON_PLUS: &[mt_ui::icons::vector::Shape] = &[
    mt_ui::icons::vector::Shape::line(
        mt_ui::icons::vector::Ink::Current,
        ICON_STROKE_16,
        mt_ui::icons::vector::Geom::Polyline(&[
            (u16_norm(8.0), u16_norm(3.0)),
            (u16_norm(8.0), u16_norm(13.0)),
        ]),
    ),
    mt_ui::icons::vector::Shape::line(
        mt_ui::icons::vector::Ink::Current,
        ICON_STROKE_16,
        mt_ui::icons::vector::Geom::Polyline(&[
            (u16_norm(3.0), u16_norm(8.0)),
            (u16_norm(13.0), u16_norm(8.0)),
        ]),
    ),
];

/// 水平三点 `···`（行尾悬浮更多操作按钮）。
const ICON_MORE: &[mt_ui::icons::vector::Shape] = &[
    mt_ui::icons::vector::Shape::fill(
        mt_ui::icons::vector::Ink::Current,
        mt_ui::icons::vector::Geom::Circle {
            c: (u16_norm(3.5), u16_norm(8.0)),
            r: u16_norm(1.2),
        },
    ),
    mt_ui::icons::vector::Shape::fill(
        mt_ui::icons::vector::Ink::Current,
        mt_ui::icons::vector::Geom::Circle {
            c: (u16_norm(8.0), u16_norm(8.0)),
            r: u16_norm(1.2),
        },
    ),
    mt_ui::icons::vector::Shape::fill(
        mt_ui::icons::vector::Ink::Current,
        mt_ui::icons::vector::Geom::Circle {
            c: (u16_norm(12.5), u16_norm(8.0)),
            r: u16_norm(1.2),
        },
    ),
];

/// 全部折叠（叠框减号，与 FileTree 头部同家族）。
const ICON_COLLAPSE_ALL: &[mt_ui::icons::vector::Shape] = &[
    mt_ui::icons::vector::Shape::line(
        mt_ui::icons::vector::Ink::Current,
        ICON_STROKE_16,
        mt_ui::icons::vector::Geom::Rect {
            x: u16_norm(1.8),
            y: u16_norm(4.5),
            w: u16_norm(9.7),
            h: u16_norm(9.7),
            round: u16_norm(1.2),
        },
    ),
    mt_ui::icons::vector::Shape::line(
        mt_ui::icons::vector::Ink::Current,
        ICON_STROKE_16,
        mt_ui::icons::vector::Geom::Polyline(&[
            (u16_norm(4.5), u16_norm(4.5)),
            (u16_norm(4.5), u16_norm(1.8)),
            (u16_norm(14.2), u16_norm(1.8)),
            (u16_norm(14.2), u16_norm(11.5)),
            (u16_norm(11.5), u16_norm(11.5)),
        ]),
    ),
    mt_ui::icons::vector::Shape::line(
        mt_ui::icons::vector::Ink::Current,
        ICON_STROKE_16,
        mt_ui::icons::vector::Geom::Polyline(&[
            (u16_norm(4.2), u16_norm(9.35)),
            (u16_norm(9.1), u16_norm(9.35)),
        ]),
    ),
];

/// 全部展开（叠框加号）。
const ICON_EXPAND_ALL: &[mt_ui::icons::vector::Shape] = &[
    mt_ui::icons::vector::Shape::line(
        mt_ui::icons::vector::Ink::Current,
        ICON_STROKE_16,
        mt_ui::icons::vector::Geom::Rect {
            x: u16_norm(1.8),
            y: u16_norm(4.5),
            w: u16_norm(9.7),
            h: u16_norm(9.7),
            round: u16_norm(1.2),
        },
    ),
    mt_ui::icons::vector::Shape::line(
        mt_ui::icons::vector::Ink::Current,
        ICON_STROKE_16,
        mt_ui::icons::vector::Geom::Polyline(&[
            (u16_norm(4.5), u16_norm(4.5)),
            (u16_norm(4.5), u16_norm(1.8)),
            (u16_norm(14.2), u16_norm(1.8)),
            (u16_norm(14.2), u16_norm(11.5)),
            (u16_norm(11.5), u16_norm(11.5)),
        ]),
    ),
    mt_ui::icons::vector::Shape::line(
        mt_ui::icons::vector::Ink::Current,
        ICON_STROKE_16,
        mt_ui::icons::vector::Geom::Polyline(&[
            (u16_norm(4.2), u16_norm(9.35)),
            (u16_norm(9.1), u16_norm(9.35)),
        ]),
    ),
    mt_ui::icons::vector::Shape::line(
        mt_ui::icons::vector::Ink::Current,
        ICON_STROKE_16,
        mt_ui::icons::vector::Geom::Polyline(&[
            (u16_norm(6.65), u16_norm(6.9)),
            (u16_norm(6.65), u16_norm(11.8)),
        ]),
    ),
];

/// 小叉（筛选框清空按钮）。
const ICON_CLOSE_SM: &[mt_ui::icons::vector::Shape] = &[
    mt_ui::icons::vector::Shape::line(
        mt_ui::icons::vector::Ink::Current,
        ICON_STROKE_16,
        mt_ui::icons::vector::Geom::Polyline(&[
            (u16_norm(4.0), u16_norm(4.0)),
            (u16_norm(12.0), u16_norm(12.0)),
        ]),
    ),
    mt_ui::icons::vector::Shape::line(
        mt_ui::icons::vector::Ink::Current,
        ICON_STROKE_16,
        mt_ui::icons::vector::Geom::Polyline(&[
            (u16_norm(12.0), u16_norm(4.0)),
            (u16_norm(4.0), u16_norm(12.0)),
        ]),
    ),
];

/// SSH 远程项目的领位图标 —— lucide 的 `Server`(原版 `<Server size={14}/>`)。
///
/// mt-ui 的图标表里没有对应项,用同一套形状 DSL 在宿主侧拼一份(与
/// [`crate::dnd::BOXES_SHAPES`] 同一条口径,**不动 mt-ui 的公开 API**)。
/// 原始 viewBox 是 24,顶点除以 24 归一;`stroke-width=1.5`(lucide 默认 2,
/// 原版显式传了 1.5)同样除以 24。两颗指示灯用小实心圆代替 lucide 的
/// 「零长度圆头线段」—— 形状 DSL 没有 linecap 语义。
const SERVER: &[mt_ui::icons::vector::Shape] = &[
    mt_ui::icons::vector::Shape::line(
        mt_ui::icons::vector::Ink::Current,
        1.5 / 24.0,
        mt_ui::icons::vector::Geom::Rect {
            x: 2.0 / 24.0,
            y: 2.0 / 24.0,
            w: 20.0 / 24.0,
            h: 8.0 / 24.0,
            round: 2.0 / 24.0,
        },
    ),
    mt_ui::icons::vector::Shape::line(
        mt_ui::icons::vector::Ink::Current,
        1.5 / 24.0,
        mt_ui::icons::vector::Geom::Rect {
            x: 2.0 / 24.0,
            y: 14.0 / 24.0,
            w: 20.0 / 24.0,
            h: 8.0 / 24.0,
            round: 2.0 / 24.0,
        },
    ),
    mt_ui::icons::vector::Shape::fill(
        mt_ui::icons::vector::Ink::Current,
        mt_ui::icons::vector::Geom::Circle {
            c: (6.0 / 24.0, 6.0 / 24.0),
            r: 1.1 / 24.0,
        },
    ),
    mt_ui::icons::vector::Shape::fill(
        mt_ui::icons::vector::Ink::Current,
        mt_ui::icons::vector::Geom::Circle {
            c: (6.0 / 24.0, 18.0 / 24.0),
            r: 1.1 / 24.0,
        },
    ),
];

/// 项目行的领位图标。**远程项目优先**(原版 `isRemote ? <Server/> : ...`):
/// 断链(连接被删)时转 error 色,否则 info 色;本地项目 `kind` 认得出就是
/// 技术栈徽标,否则退通用目录图标(对应原版认不出时的 `Package` 兜底,
/// 同样取 `--color-file`)。
fn project_icon(kind: Option<ProjectKind>, remote: Option<RemoteBadge>) -> AnyElement {
    if let Some(remote) = remote {
        return VectorIcon::new(SERVER, px(14.0))
            .ink(if remote.broken {
                ui::color_error()
            } else {
                ui::color_info()
            })
            .into_any_element();
    }
    match kind {
        Some(kind) => TechIcon::new(kind.as_str())
            .size(px(14.0))
            .into_any_element(),
        None => FileIcon::folder(false)
            .size(px(14.0))
            .color(ui::color_file())
            .into_any_element(),
    }
}

/// 远程项目行尾那枚徽章要画的东西。`None`(整个 `Option`)= 不是远程项目。
#[derive(Clone, Debug, PartialEq, Eq)]
struct RemoteBadge {
    /// 连接名;断链时是空串(徽章改画「断链」两字)。
    name: String,
    /// `user@host:port`,挂 tooltip 用;断链时是空串。
    summary: String,
    /// 引用的连接已被删除。
    broken: bool,
}

/// 项目 + 连接表 → 徽章数据。**不是远程项目返回 `None`**。
///
/// 判定与标签都走 [`crate::ssh_conn`] 的谓词,与文件树 / 会话面板 / 断线遮罩
/// 共用同一把尺子 —— 三处各判一次是 v0.6.x 那批 SSH bug 的来源。
fn remote_badge(project: &ProjectConfig, connections: &[mt_config::SshConnection]) -> Option<RemoteBadge> {
    if !crate::ssh_conn::is_remote_project(project) {
        return None;
    }
    Some(match crate::ssh_conn::remote_connection(project, connections) {
        Some(conn) => RemoteBadge {
            name: conn.name.clone(),
            summary: crate::ssh_conn::connection_summary(conn),
            broken: false,
        },
        None => RemoteBadge {
            name: String::new(),
            summary: String::new(),
            broken: true,
        },
    })
}

/// 完成标该不该出现(原版 `ProjectList.tsx:912` 的 `showDoneTag`)。
///
/// 判据有**两个**读者(行渲染 + `render` 里那张进场表),抽出来免得两边漂移。
fn shows_done_tag(needs_attention: bool, is_active: bool) -> bool {
    needs_attention && !is_active
}

/// 完成标退场:收回去再卸。与浮层退场同一条时长与缓动。
const DONE_TAG_OUT: mt_ui::motion::TransitionSpec = mt_ui::motion::OVERLAY_OUT;

/// 一枚完成标这一帧的形态:`open` = 药丸露出来的宽度比例(0..1),`scale` = 水平内边距的
/// 缩放(进场的过冲落在它上面)。
#[derive(Clone, Copy, Debug, PartialEq)]
struct DoneTagFrame {
    open: f32,
    scale: f32,
}

/// 进场按 `tagFadeIn` 的关键帧(见 [`mt_ui::motion::tag_fade_in`]):前 60% 药丸从 0 宽
/// 横向展开到全宽、同时内边距过冲到 1.15,之后内边距回落到 1 —— 原版的「淡入 + 放大过冲」
/// 换成宽度表达,不做透明度。退场从开始收的那一刻露着的宽度 `from` 线性收回 0。
fn done_tag_frame(leaving: Option<f32>, progress: f32) -> DoneTagFrame {
    let p = progress.clamp(0.0, 1.0);
    match leaving {
        Some(from) => DoneTagFrame {
            open: from.clamp(0.0, 1.0) * (1.0 - p),
            scale: 1.0,
        },
        None => {
            let (open, scale) = mt_ui::motion::tag_fade_in(p);
            DoneTagFrame { open, scale }
        }
    }
}

/// 退场途中掉头重新进场这一帧的形态:从掉头那一刻露着的宽度 `from` 接着展开到全宽,
/// 内边距停在 1(过冲只属于头一回出现;从 1 跳回关键帧起点的 0.6 会让药丸宽度当场缩一截)。
/// **纯函数**,终点精确到全宽。
fn resumed_tag_frame(from: f32, progress: f32) -> DoneTagFrame {
    let p = progress.clamp(0.0, 1.0);
    let from = from.clamp(0.0, 1.0);
    DoneTagFrame {
        open: if p >= 1.0 {
            1.0
        } else {
            from + (1.0 - from) * p
        },
        scale: 1.0,
    }
}

/// 一枚完成标的进出场状态(见 [`ProjectList::done_tags`])。
struct DoneTagAnim {
    transition: mt_ui::motion::Transition,
    /// `Some(起收时露着的宽度比例)` = 正在退场。
    leaving: Option<f32>,
    /// `Some(掉头时露着的宽度比例)` = 退场途中又该出现了,正从那里接着展开(见
    /// [`resumed_tag_frame`]);`None` + 不在退场 = 头一回进场(关键帧)。
    resumed: Option<f32>,
    /// 「完成」两个字的自然宽(canvas 量的;0 = 还没量过)。
    text_w: Rc<Cell<f32>>,
}

impl DoneTagAnim {
    fn enter() -> Self {
        Self {
            transition: mt_ui::motion::Transition::new(mt_ui::motion::TAG_FADE_IN),
            leaving: None,
            resumed: None,
            text_w: Rc::new(Cell::new(0.0)),
        }
    }

    /// 建一枚**已经到位**的(首次出现不播:见 [`ProjectList::sync_done_tags`] 的「隔了一阵
    /// 才再画」那一支)。
    fn settled() -> Self {
        Self {
            transition: mt_ui::motion::Transition::settled(mt_ui::motion::TAG_FADE_IN),
            ..Self::enter()
        }
    }

    fn frame(&self) -> DoneTagFrame {
        let p = self.transition.progress();
        match (self.leaving, self.resumed) {
            (None, Some(from)) => resumed_tag_frame(from, p),
            (leaving, _) => done_tag_frame(leaving, p),
        }
    }

    /// 开始退场:从眼下露着的宽度收回去。
    fn leave(&mut self) {
        let from = self.frame().open;
        self.leaving = Some(from);
        self.resumed = None;
        self.transition = mt_ui::motion::Transition::new(DONE_TAG_OUT);
    }

    /// 退场途中又该出现了:从眼下露着的宽度掉头接着展开(半路掉头不跳 —— 此前从 0 宽
    /// 重播,收到一半的药丸会当场缩没再长出来)。
    fn reenter(&mut self) {
        let from = self.frame().open;
        self.leaving = None;
        self.resumed = Some(from);
        self.transition = mt_ui::motion::Transition::new(mt_ui::motion::TAG_FADE_IN);
    }

    /// 退场播完了(可以从表里摘掉)。
    fn gone(&self) -> bool {
        self.leaving.is_some() && !self.transition.running()
    }
}

/// 把完成标表对齐到这一帧该挂标的那些行(`showing`):新出现的建一条进场,不该挂了的转
/// 退场,退场途中又该挂的掉头,收完的摘掉。
///
/// `stale` = 隔了一阵才再画(中栏收着时本视图不渲染,那期间切过项目 / AI 跑完过):那些
/// 变化用户没看见,再展开时不补播 —— 不该挂的直接摘、该挂的直接到位(否则中栏滑入的
/// 同时,一枚早该没了的标还要当着面收一次)。
fn sync_tag_table(tags: &mut HashMap<String, DoneTagAnim>, showing: HashSet<String>, stale: bool) {
    if stale {
        tags.retain(|id, _| showing.contains(id));
        for id in showing {
            tags.entry(id)
                .and_modify(|tag| {
                    if tag.leaving.is_some() {
                        *tag = DoneTagAnim::settled();
                    }
                })
                .or_insert_with(DoneTagAnim::settled);
        }
        return;
    }
    for (id, tag) in tags.iter_mut() {
        if !showing.contains(id) && tag.leaving.is_none() {
            tag.leave();
        }
    }
    tags.retain(|_, tag| !tag.gone());
    for id in showing {
        tags.entry(id)
            .and_modify(|tag| {
                if tag.leaving.is_some() {
                    tag.reenter();
                }
            })
            .or_insert_with(DoneTagAnim::enter);
    }
}

/// 完成标(原版的 `<DoneTag/>`,样式在 `styles.css:509-524`)。
///
/// 实心 success 底 + **底色字**(不是白字:浅色主题下白字配浅绿看不见,
/// 与 StatusDot 的 `contrast` 同一条理由)、圆角 10px、字号 `0.77rem`≈10px、粗体。
/// 原版还有一层 success 色的外发光,gpui 的 div 没有 box-shadow,省掉。
///
/// # 进出场(`tagFadeIn` 0.3s / 退场 [`DONE_TAG_OUT`])
///
/// 状态在 [`ProjectList::done_tags`] 里。药丸本身就是裁剪窗:底色与圆角画在外壳上、外壳
/// 宽度 = 自然宽 × [`DoneTagFrame::open`],字居中排在里面按自然宽度不动 —— 看上去是一枚
/// 从中间横向长开的药丸,任何一帧两端都是圆的。过冲落在水平内边距上(`scale`),字号与
/// 行高一动不动(动它们会让整行在这 300ms 里抖高度)。**豁免**系统「减少动画」(2026-11-21
/// 用户拍板:此前它过闸,用户机器上系统动画关着,这条从来没播过),服从「启用动画」总开关。
/// 静止时外壳不设宽度,与原来的药丸一模一样;探针一直量着字宽,留给下一次开合当起点。
fn done_tag(frame: DoneTagFrame, settled: bool, text_w: &Rc<Cell<f32>>) -> AnyElement {
    let measure = text_w.clone();
    let probe = canvas(
        move |bounds, _window, _cx| measure.set(f32::from(bounds.size.width)),
        |_, _, _, _| {},
    )
    // 钉住 inset(不写会按静态位置落到字的下方,见 `segment_slider::IndicatorSlider::probe`)
    .absolute()
    .top_0()
    .left_0()
    .size_full();
    let pad = 8.0 * frame.scale;
    let label = div()
        .relative()
        .flex_none()
        .whitespace_nowrap()
        .child(t("panels", "done"))
        .child(probe);
    let pill = div()
        .flex_shrink_0()
        .flex()
        .justify_center()
        .py(px(2.0))
        .rounded(px(10.0))
        .bg(ui::color_success())
        .text_size(ui::font_px(10.0))
        .font_weight(FontWeight::BOLD)
        .text_color(ui::bg_base());
    if settled {
        return pill.px(px(8.0)).child(label).into_any_element();
    }
    // 动画中:外壳定宽、裁掉多出的部分;还没量过字宽的那一帧按 0 算(那时 open≈0)
    let full = text_w.get() + pad * 2.0;
    pill.w(px(full * frame.open))
        .overflow_hidden()
        .child(label)
        .into_any_element()
}

/// 侧栏行内状态筛选档位。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ProjectFilterMode {
    #[default]
    All,
    /// 存在运行中 / 待审批 / 已完成未读的 AI 活跃项目。
    AiActive,
    /// SSH 远程项目。
    Remote,
}

/// 项目是否属于「AI 活跃」筛选集合（工作 / 等你处理 / 未读完成 / 出错）。
fn is_ai_active_project(light: StatusLight, needs_attention: bool, is_active: bool) -> bool {
    shows_done_tag(needs_attention, is_active)
        || matches!(
            light,
            StatusLight::AiWorking | StatusLight::Attention | StatusLight::Error
        )
}

/// 带关键词高亮与省略号截断的单行文本：空查询直接返回纯文本，有查询走 `StyledText`。
fn highlighted_text(text: String, query_lower: &str) -> AnyElement {
    if query_lower.is_empty() {
        return div().min_w(px(0.0)).truncate().child(text).into_any_element();
    }
    let ranges = find_substring_char_ranges(&text, query_lower);
    if ranges.is_empty() {
        return div().min_w(px(0.0)).truncate().child(text).into_any_element();
    }
    let style = HighlightStyle {
        color: Some(ui::accent()),
        font_weight: Some(FontWeight::SEMIBOLD),
        ..Default::default()
    };
    let highlights: Vec<_> = ranges
        .iter()
        .filter_map(|&(start, end)| crate::git::branches::char_range_to_bytes(&text, start, end))
        .map(|range| (range, style))
        .collect();
    div()
        .min_w(px(0.0))
        .truncate()
        .child(StyledText::new(text).with_highlights(highlights))
        .into_any_element()
}

/// 右侧整合式 AI 活跃胶囊：将 AI 品牌图标组与状态灯合成一枚精致微胶囊，
/// 让左侧项目名在整栏严格垂直对齐，同时一眼看清「哪家 AI + 当前进度」。
fn ai_activity_pill(
    vendors: &[Option<AiVendor>],
    icon_size: f32,
    light: StatusLight,
    include_light: bool,
) -> gpui::Div {
    let pill_bg = match light {
        StatusLight::Error => ui::with_alpha(ui::color_error(), 0.12),
        StatusLight::Attention => ui::with_alpha(ui::color_attention(), 0.14),
        StatusLight::AiWorking => ui::with_alpha(ui::color_ai_working(), 0.12),
        _ => ui::with_alpha(ui::bg_elevated(), 0.85),
    };
    let pill_border = match light {
        StatusLight::Error => ui::with_alpha(ui::color_error(), 0.28),
        StatusLight::Attention => ui::with_alpha(ui::color_attention(), 0.30),
        StatusLight::AiWorking => ui::with_alpha(ui::color_ai_working(), 0.25),
        _ => ui::border_subtle(),
    };
    let mut pill = div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(3.0))
        .px(px(5.0))
        .py(px(1.5))
        .rounded_full()
        .bg(pill_bg)
        .border_1()
        .border_color(pill_border);
    for vendor in vendors {
        pill = pill.child(
            BrandIcon::new(*vendor)
                .size(px(icon_size))
                .color(ui::text_secondary()),
        );
    }
    if include_light && light != StatusLight::Idle {
        pill = pill.child(ui::status_dot(light));
    }
    pill
}

/// 独立（已脱离父项目）worktree 行的行尾分支徽章:使用矢量 `ICON_BRANCH` 取代文本字符 `⎇`。
fn worktree_badge_chip(id: &str, branch: String) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(format!("worktree-{id}")))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(3.0))
        .min_w(px(0.0))
        .max_w(px(WORKTREE_BADGE_MAX_W))
        .px(px(5.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .text_size(ui::font_px(10.0))
        .text_color(ui::text_muted())
        .bg(ui::border_subtle())
        .tip(tr!(
            "projectList",
            "worktreeBadgeTitle",
            branch = branch.clone()
        ))
        .child(VectorIcon::new(ICON_BRANCH, px(10.0)).ink(ui::text_muted()))
        .child(div().min_w(px(0.0)).truncate().child(branch))
}

/// 折叠态容器（worktree 父项目胶囊 / 折叠分组行）的聚合状态：
/// 确保收起子项后不漏掉 AI 完成 / 待处理 / 报错 / SSH / 活动项目。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CollapsedSummary {
    /// 任意折叠子项有未读完成标（且非当前活动项目）。
    has_done: bool,
    /// 折叠子项里的最高状态灯档位。
    light: StatusLight,
    /// 折叠子项里是否存在右键 SSH 已连接终端。
    has_ssh: bool,
    /// 当前活动项目是否正藏在这批折叠子项里。
    contains_active: bool,
}

/// 折叠胶囊 / 折叠分组行上附带的小状态指示器（仅在有需要提醒的状态时出现）。
fn collapsed_status_indicator(summary: &CollapsedSummary) -> Option<AnyElement> {
    if summary.light >= StatusLight::Attention {
        return Some(
            StatusDot::new(ui::status_kind(summary.light))
                .size(px(8.5))
                .color(ui::status_color(summary.light))
                .contrast(ui::bg_elevated())
                .into_any_element(),
        );
    }
    if summary.has_done {
        return Some(
            div()
                .w(px(6.0))
                .h(px(6.0))
                .flex_shrink_0()
                .rounded_full()
                .bg(ui::color_success())
                .into_any_element(),
        );
    }
    if summary.light != StatusLight::Idle {
        return Some(
            StatusDot::new(ui::status_kind(summary.light))
                .size(px(8.5))
                .color(ui::status_color(summary.light))
                .contrast(ui::bg_elevated())
                .into_any_element(),
        );
    }
    if summary.has_ssh {
        return Some(
            div()
                .w(px(6.0))
                .h(px(6.0))
                .flex_shrink_0()
                .rounded_full()
                .bg(ui::color_info())
                .into_any_element(),
        );
    }
    None
}

/// 列表行（分组行 / 独立项目 / Worktree 微容器）的左外边距：
/// 顶层 `ROW_MARGIN_X(6)`，每进一层分组步进 `INDENT_STEP(16)`。
fn row_left_margin(group_depth: usize) -> f32 {
    ROW_MARGIN_X + group_depth as f32 * INDENT_STEP
}

/// 组内第 `level` 层（0 起）纵向分组引导线的 x 坐标（对齐该层分组折叠箭头中心）。
fn group_guide_x(level: usize) -> f32 {
    GROUP_GUIDE_BASE_X + level as f32 * INDENT_STEP
}

/// 在行外层 `relative()` 左侧绘制 `0..group_depth` 条极细分组引导竖线。
fn group_guide_lines(group_depth: usize) -> Vec<AnyElement> {
    if group_depth == 0 {
        return Vec::new();
    }
    let color = ui::with_alpha(ui::border_strong(), 0.55);
    (0..group_depth)
        .map(|level| {
            div()
                .absolute()
                .left(px(group_guide_x(level)))
                .top_0()
                .bottom_0()
                .w(px(1.0))
                .bg(color)
                .into_any_element()
        })
        .collect()
}

/// B1 分支优先子项的展示文案拆分。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChildDisplayLabel {
    /// 主文字（分支名 / 用户自定义名 / 去前缀目录名）。
    primary: String,
    /// 次要副文（用户手动改名后附带的分支名，或需求描述）。
    secondary: Option<String>,
    /// 主文字是否直接就是 Git 分支名。
    primary_is_branch: bool,
}

/// 取路径末段目录名（兼容 Windows `\` 与 POSIX `/`）。
fn path_basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rfind(['/', '\\']) {
        Some(idx) => &trimmed[idx + 1..],
        None => trimmed,
    }
}

/// 剥掉子项目目录名里与父项目重复的前缀（如 `mini-term` 下的 `mini-term-plugin` → `plugin`）。
/// 匹配不区分 ASCII 大小写、支持 `-` / `_` / `.` / 空格分隔符；剩余为空或无匹配则原样返回。
fn strip_parent_prefix(name: &str, parent_name: Option<&str>) -> String {
    let Some(parent) = parent_name.filter(|p| !p.is_empty()) else {
        return name.to_string();
    };
    let mut name_chars = name.chars();
    let mut matched = true;
    for pc in parent.chars() {
        match name_chars.next() {
            Some(nc) if nc.eq_ignore_ascii_case(&pc) => {}
            _ => {
                matched = false;
                break;
            }
        }
    }
    if matched
        && let Some(sep) = name_chars.next()
        && matches!(sep, '-' | '_' | '.' | ' ')
    {
        let rest = name_chars.as_str().trim();
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
    name.to_string()
}

/// B1 分支优先口径：
/// - 用户手动重命名过（`name != 目录末段`）→ 尊重自定义名为主文字，分支名退作次要副文；
/// - 未重命名且探测到 Git 分支 → 直接以分支名为主文字（告别 100px 截断胶囊）；
/// - 未重命名且无分支（游离 HEAD / 远程 / 探测中）→ 剥掉重复父项目前缀后显示。
fn resolve_child_display_label(
    name: &str,
    path: &str,
    parent_name: Option<&str>,
    worktree_branch: Option<&str>,
    description: Option<&str>,
) -> ChildDisplayLabel {
    let folder = path_basename(path);
    let is_custom_name = !folder.is_empty() && name != folder;
    if is_custom_name {
        let secondary = match (worktree_branch, description) {
            (Some(branch), Some(desc)) => Some(format!("{branch} · {desc}")),
            (Some(branch), None) => Some(branch.to_string()),
            (None, Some(desc)) => Some(desc.to_string()),
            (None, None) => None,
        };
        ChildDisplayLabel {
            primary: name.to_string(),
            secondary,
            primary_is_branch: false,
        }
    } else if let Some(branch) = worktree_branch {
        ChildDisplayLabel {
            primary: branch.to_string(),
            secondary: description.map(str::to_string),
            primary_is_branch: true,
        }
    } else {
        ChildDisplayLabel {
            primary: strip_parent_prefix(name, parent_name),
            secondary: description.map(str::to_string),
            primary_is_branch: false,
        }
    }
}

/// worktree 子项行的悬停提示：同时交代分支 / 目录名与完整路径。
fn child_tooltip_text(
    name: &str,
    path: &str,
    worktree_branch: Option<&str>,
    display: &ChildDisplayLabel,
) -> String {
    match worktree_branch {
        Some(branch) if name != branch => format!("{name} · {branch}\n{path}"),
        Some(branch) => format!("{branch}\n{path}"),
        None if display.primary != name => format!("{name}\n{path}"),
        None => path.to_string(),
    }
}

/// 微容器内第 `level` 层（0 = 直接子项）树形竖轨相对微容器内左缘的 x 坐标。
/// 微容器本身已按分组深度完成 `row_left_margin` 缩进，内部父行内距 `(ROW_INNER_PL - CLUSTER_BORDER_W) = 9`，
/// 故父项目 14px 领位图标中心恒落在 `9 + 7 = 16`。
fn cluster_rail_x(level: usize) -> f32 {
    (ROW_INNER_PL - CLUSTER_BORDER_W) + 7.0 + level as f32 * CHILD_NEST_STEP
}

/// 微容器子项左侧的树形连线层（`├─` / `╰─` + 多层祖先贯通竖线）。
/// 挂在子项外层 `relative()` 上，竖线从 `top_0()` 贯通到 `bottom_0()`，相邻子项无缝衔接。
fn child_tree_connectors(
    rel_depth: usize,
    is_last_sibling: bool,
    ancestor_continues: &[bool],
    is_active: bool,
) -> Vec<AnyElement> {
    let trunk_color = ui::with_alpha(ui::text_muted(), 0.42);
    let arm_color = if is_active {
        ui::with_alpha(ui::accent(), 0.85)
    } else {
        trunk_color
    };
    let mut parts: Vec<AnyElement> = Vec::with_capacity(ancestor_continues.len() + 2);
    for (level, &continues) in ancestor_continues.iter().enumerate() {
        if continues {
            let x = cluster_rail_x(level);
            parts.push(
                div()
                    .absolute()
                    .left(px(x))
                    .top_0()
                    .bottom_0()
                    .w(px(1.0))
                    .bg(trunk_color)
                    .into_any_element(),
            );
        }
    }
    let rail_x = cluster_rail_x(rel_depth.saturating_sub(1));
    if is_last_sibling {
        // 末位兄弟：圆角 L 弯折 `╰─`，从顶沿垂到行中线再优雅向右折出
        parts.push(
            div()
                .absolute()
                .left(px(rail_x))
                .top_0()
                .h(gpui::relative(0.5))
                .w(px(CHILD_ELBOW_W))
                .border_l_1()
                .border_b_1()
                .rounded_bl(px(5.0))
                .border_color(arm_color)
                .into_any_element(),
        );
    } else {
        // 非末位兄弟：全高竖干 + 中线水平支臂（互不重叠，半透明色下无叠加发亮）
        parts.push(
            div()
                .absolute()
                .left(px(rail_x))
                .top_0()
                .bottom_0()
                .w(px(1.0))
                .bg(trunk_color)
                .into_any_element(),
        );
        parts.push(
            div()
                .absolute()
                .left(px(rail_x + 1.0))
                .top(gpui::relative(0.5))
                .w(px(CHILD_ELBOW_W - 1.0))
                .h(px(1.0))
                .bg(arm_color)
                .into_any_element(),
        );
    }
    parts
}

/// 远程徽章:连接名(正常态柔润 info 配色，断链时「断链」两字 + error 配色)。
fn remote_badge_chip(id: &str, remote: RemoteBadge) -> gpui::Stateful<gpui::Div> {
    let (fg, bg) = if remote.broken {
        (ui::color_error(), ui::with_alpha(ui::color_error(), 0.14))
    } else {
        (
            ui::with_alpha(ui::color_info(), 0.92),
            ui::with_alpha(ui::color_info(), 0.11),
        )
    };
    let tip: SharedString = if remote.broken {
        t("projectList", "remoteBrokenTitle").into()
    } else {
        tr!(
            "projectList",
            "remoteBadgeTitle",
            summary = remote.summary.clone()
        )
        .into()
    };
    div()
        .id(SharedString::from(format!("remote-badge-{id}")))
        .flex_shrink_0()
        .min_w(px(0.0))
        .max_w(px(REMOTE_BADGE_MAX_W))
        .px(px(5.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .text_size(ui::font_px(9.75))
        .font_weight(FontWeight::MEDIUM)
        .text_color(fg)
        .bg(bg)
        .tip(tip)
        .truncate()
        .child(if remote.broken {
            SharedString::from(t("projectList", "remoteBrokenBadge"))
        } else {
            SharedString::from(remote.name.clone())
        })
}

/// 行尾右侧状态区（AI 活跃胶囊 / 完成标 / 裸状态灯 / 右键 SSH 已连接灯）。
///
/// 优先级严格保持 **AI 状态优先**:完成标 > AI/错误/待处理灯(`StatusLight != Idle`) >
/// 右键「SSH 连接」已连接灯(`◉`)。
/// 有 AI 品牌图标时将其与状态灯合进精致的「AI 活跃胶囊」；完成标退场期间优先保标。
fn row_right_status(
    id: &str,
    ai_vendors: &[Option<AiVendor>],
    icon_size: f32,
    show_done_tag: bool,
    tag: Option<&DoneTagAnim>,
    light: StatusLight,
    ssh_connected: Option<&str>,
) -> Vec<AnyElement> {
    let mut parts: Vec<AnyElement> = Vec::with_capacity(2);
    let has_done = tag.is_some() || show_done_tag;
    if !ai_vendors.is_empty() {
        parts.push(ai_activity_pill(ai_vendors, icon_size, light, !has_done).into_any_element());
    }
    if let Some(tag) = tag {
        let settled = tag.leaving.is_none() && !tag.transition.running();
        parts.push(done_tag(tag.frame(), settled, &tag.text_w));
    } else if show_done_tag {
        let frame = DoneTagFrame {
            open: 1.0,
            scale: 1.0,
        };
        parts.push(done_tag(frame, true, &Rc::new(Cell::new(0.0))));
    } else if ai_vendors.is_empty() {
        if light != StatusLight::Idle {
            parts.push(ui::status_dot(light).into_any_element());
        } else if let Some(conn_name) = ssh_connected {
            parts.push(
                div()
                    .id(SharedString::from(format!("ssh-dot-{id}")))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .tip(tr!(
                        "panels",
                        "statusDot.sshConnectedWith",
                        name = conn_name.to_string()
                    ))
                    .child(ui::ssh_connected_dot())
                    .into_any_element(),
            );
        }
    }
    parts
}

/// 外部拖拽的三态提示框:盖住整栏。
fn external_drop_hint(kind: Option<ExternalDropKind>) -> gpui::Div {
    let (border, bg) = match kind {
        // 还在后台判目录:先按"可以放"画,免得闪一下红框
        None | Some(ExternalDropKind::Valid) => {
            (ui::accent(), ui::with_alpha(ui::accent(), 0.1))
        }
        Some(ExternalDropKind::Forbidden) => {
            (ui::color_error(), ui::with_alpha(ui::color_error(), 0.1))
        }
        Some(ExternalDropKind::Duplicate) => {
            (ui::color_warning(), ui::with_alpha(ui::color_warning(), 0.1))
        }
    };
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .border_2()
        .border_dashed()
        .border_color(border)
        .bg(bg)
        .child(
            div()
                .text_size(ui::font_px(11.0))
                .text_color(border)
                .child(t(
                    "projectList",
                    kind.unwrap_or(ExternalDropKind::Valid).hint_key(),
                )),
        )
}

/// 领位徽标最终显示哪种技术栈(`ProjectList.tsx:629-632`)。
///
/// 原式:`kindOverride === 'none' ? null : kindOverride ?? detected ?? null`。
/// 两条不显然的地方:
/// - `'none'` 是「用户明确关掉徽标」,**不回退**到探测结果;
/// - 覆盖值一旦存在就压过探测(认不出的覆盖值也一样落到通用图标 —— `??` 链
///   只看 `kindOverride` 有没有值,不看它认不认得出)。
fn resolve_project_kind(
    kind_override: Option<&str>,
    detected: Option<ProjectKind>,
) -> Option<ProjectKind> {
    match kind_override {
        Some("none") => None,
        Some(other) => ProjectKind::from_str(other),
        None => detected,
    }
}

/// 项目行上的 AI 品牌堆叠(`ProjectList.tsx:636-650`)。
///
/// 入参是「这个项目里**显示 AI 会话**的那些 pane 的 agent 名」,判定口径与
/// tab 上的品牌图标共用(`PaneState::shows_ai_session` / `ai_agent`)。
///
/// 三条规则逐条照抄:
/// - **按厂商去重**(同款 AI 开多个 pane 只显示一枚,认不出厂商的算作同一个 `unknown`);
/// - **字母序**排列,不随开 pane 的顺序漂移;
/// - **未知厂商固定排最后**。
///
/// 数量**无上限** —— 原版就没有,去重之后厂商总共 11 家,天然收敛。
fn ai_vendor_stack<'a>(agents: impl IntoIterator<Item = Option<&'a str>>) -> Vec<Option<AiVendor>> {
    let mut seen: HashSet<&'static str> = HashSet::new();
    let mut out: Vec<Option<AiVendor>> = Vec::new();
    for agent in agents {
        // 与 tab 同一条:CLI 名直取,其余走词匹配(只认三家会漏掉 gemini 之类)
        let vendor = agent.and_then(|agent| {
            AiVendor::from_session_type(agent).or_else(|| AiVendor::infer(Some(agent), None))
        });
        let key = vendor.map(|v| v.as_str()).unwrap_or("unknown");
        if seen.insert(key) {
            out.push(vendor);
        }
    }
    out.sort_by(|a, b| match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, _) => std::cmp::Ordering::Greater,
        (_, None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => a.as_str().cmp(b.as_str()),
    });
    out
}

// ─── 失效 worktree 清理(`src/utils/worktreeReconcile.ts` 逐字移植) ───

/// UNC 路径(`\\wsl$` 等):存在性探测依赖 WSL / 网络状态,误判风险高,不参与清理。
fn is_unc_path(path: &str) -> bool {
    path.starts_with(r"\\")
}

/// 可参与失效清理的 worktree 子项目:本地路径、父项目存在且也是本地路径。
fn reconcilable_children(projects: &[ProjectConfig]) -> Vec<(&ProjectConfig, &ProjectConfig)> {
    let mut out = Vec::new();
    for p in projects {
        let Some(parent_id) = p.parent_project_id.as_deref() else {
            continue;
        };
        if p.ssh_connection_id.is_some() || is_unc_path(&p.path) {
            continue;
        }
        let Some(parent) = projects.iter().find(|q| q.id == parent_id) else {
            continue;
        };
        if parent.ssh_connection_id.is_some() || is_unc_path(&parent.path) {
            continue;
        }
        out.push((p, parent));
    }
    out
}

/// 待探测的路径集合(去重):worktree 子项目自身路径 + 其父项目路径。
///
/// 父项目路径也要探测:整棵目录树一起消失(盘符拔出等)时不能把子项目误判成
/// 「worktree 被外部删除」而清掉。
fn collect_worktree_probe_paths(projects: &[ProjectConfig]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (child, parent) in reconcilable_children(projects) {
        for path in [&child.path, &parent.path] {
            if seen.insert(path.clone()) {
                out.push(path.clone());
            }
        }
    }
    out
}

/// 应清理的失效 worktree 子项目(返回项目 id):自身目录已不存在、而父项目目录仍在。
fn find_stale_worktree_projects(
    projects: &[ProjectConfig],
    alive: &HashSet<String>,
) -> Vec<String> {
    reconcilable_children(projects)
        .into_iter()
        .filter(|(child, parent)| !alive.contains(&child.path) && alive.contains(&parent.path))
        .map(|(child, _)| child.id.clone())
        .collect()
}

/// `.git` / `.git/worktrees` 监听事件过滤：只放行 `worktrees` 目录自身或直接子项的结构变化
/// （创建 / 删除 / 改名），过滤掉 `index` / `HEAD` 改写与 Windows 非递归监听里子目录内容修改带来的噪声。
fn is_worktree_watch_event(change: &mt_project::watch::FsChange) -> bool {
    if !change.structural {
        return false;
    }
    let name = change.path.file_name().and_then(|n| n.to_str());
    let parent_name = change
        .path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());
    name == Some("worktrees") || parent_name == Some("worktrees")
}

/// 根据后台扫出的本地主仓库 worktree 清单，算出本轮要自动新增的 `(parent_project_id, wt_path)`：
/// - 父项目必须仍存活于当前 `projects`；
/// - 已作为项目存在（含独立或已脱离父项目的）不重复添加；
/// - 用户主动从项目列表移除过的（在 `dismissed` 中）跳过；
/// - 同路径只排一条（按 `windows_eq_key` 去重）。
fn plan_auto_added_worktrees(
    projects: &[ProjectConfig],
    dismissed: &[String],
    scanned: &[(String, mt_project::git::LocalRepoWorktrees)],
) -> Vec<(String, String)> {
    let alive_parents: HashSet<&str> = projects.iter().map(|p| p.id.as_str()).collect();
    let mut existing: HashSet<String> = projects
        .iter()
        .filter(|p| p.ssh_connection_id.is_none())
        .map(|p| mt_core::path_key::windows_eq_key(&p.path))
        .collect();
    let dismissed_set: HashSet<&str> = dismissed.iter().map(String::as_str).collect();

    let mut out = Vec::new();
    for (parent_id, info) in scanned {
        if !alive_parents.contains(parent_id.as_str()) {
            continue;
        }
        for wt_path in &info.linked_paths {
            let eq_key = mt_core::path_key::windows_eq_key(wt_path);
            let dismiss_key = crate::store::worktree_dismiss_key(None, wt_path);
            if dismissed_set.contains(dismiss_key.as_str()) || !existing.insert(eq_key) {
                continue;
            }
            out.push((parent_id.clone(), wt_path.clone()));
        }
    }
    out
}

/// 一行要画的东西。渲染前先从 store 抠出来 —— `store.read(cx)` 的借用
/// 活不过 `cx.listener`,一行 6 个字段用元组已经读不清了。
#[derive(Clone)]
struct Row {
    id: String,
    name: String,
    path: String,
    /// 项目聚合的状态灯档位(含第五档 attention)。
    light: StatusLight,
    /// 项目内是否存在鼠标右键「SSH 连接」已连上的终端(连接名,AI 状态优先于它)。
    ssh_connected: Option<String>,
    /// 非激活项目里有 AI 任务完成(行尾那颗绿点)。
    needs_attention: bool,
    /// 领位图标的技术栈;`None` = 走通用目录图标。
    /// 口径 `kindOverride === 'none' ? null : kindOverride ?? detected`。
    kind: Option<ProjectKind>,
    /// 探测结果本身(与 `kind` 分开存):「项目类型 → 自动识别」那一项要把它
    /// 写进括号里,而 `kind` 在用户手动指定后就不是探测值了。
    detected_kind: Option<ProjectKind>,
    /// 需求描述(右键「编辑描述」的默认值)。
    description: Option<String>,
    /// `kindOverride` 原文:`None` = 自动,`Some("none")` = 不显示,
    /// 其余是技术栈 key。子菜单的勾要按它打,不能用解析后的 `kind`
    /// (那一路把 "none" 和「认不出」压成了同一个 `None`)。
    kind_override: Option<String>,
    /// 渲染缩进层级。
    depth: usize,
    /// 所在分组;`None` = 顶层。
    parent_group_id: Option<String>,
    /// worktree 子项目:位置由父项目派生,**不作为落点**(自身仍可拖走 = 脱离父项目)。
    is_child: bool,
    /// 父项目名（仅 worktree 子项目有值，用于游离 HEAD 等无分支名时剥除重复前缀）。
    parent_name: Option<String>,
    /// 本行在 worktree 视觉簇里的角色（独立 / 折叠父 / 展开父 / 簇内子项）。
    cluster_role: ProjectClusterRole,
    /// 所属簇内是否存在当前活动项目（给展开微容器外框叠一层微弱 accent 提示）。
    cluster_has_active: bool,
    /// 折叠态父项目的子项聚合状态（仅 `CollapsedParent` 有意义）。
    collapsed_summary: CollapsedSummary,
    /// 所属祖先分组层数（0 = 顶层；非零 = 左侧绘制的分组引导竖线条数与外框缩进层数）。
    group_depth: usize,
    /// 行上的 AI 品牌堆叠(去重 + 字母序,见 [`ai_vendor_stack`])。
    ai_vendors: Vec<Option<AiVendor>>,
    /// 项目路径是某仓库的 linked worktree → `⎇ 分支名` 徽章。
    worktree_branch: Option<String>,
    /// SSH 远程项目的领位图标 + 行尾徽章;`None` = 本地项目。
    /// **同时是右键菜单的远程 gate 判据**(见 [`project_menu_actions`])。
    remote: Option<RemoteBadge>,
}

/// 分组行要画的东西。
#[derive(Clone)]
struct GroupRow {
    id: String,
    name: String,
    collapsed: bool,
    /// 递归含子组的项目数(行尾胶囊里那个数)。
    count: usize,
    depth: usize,
    /// 折叠态时组内全部后代项目的聚合状态（展开态为空默认值）。
    collapsed_summary: CollapsedSummary,
}

/// 落点指示。对应原版那个 `useState<DropIndicator>` —— 由 `on_drag_move` 写、
/// 渲染读、`on_drop` 消费。
#[derive(Clone, Debug, PartialEq, Eq)]
struct DropIndicator {
    id: String,
    position: DropPosition,
    /// 深度超限 / 自环。**非法时指示线不画**,分组行改画红色虚线框。
    forbidden: bool,
}

/// 外部文件正拖在列表上方。`kind` 为 `None` = 目录判定还在后台跑。
struct ExternalDrag {
    paths: Vec<PathBuf>,
    kind: Option<ExternalDropKind>,
}

// ─── 右键菜单 ─────────────────────────────────────────────────

/// 项目行右键菜单的**项序**。`None` = 分隔线。
///
/// 逐条对照 `ProjectList.tsx:699-833`。唯一没搬的是「WSL 会话」子菜单
/// (那块功能 GPUI 侧还没有,占位一个点不动的菜单项比没有更糟)。
///
/// # 远程项目 gate(原版 `isRemote ? [] : [...]`)
///
/// 本地专属入口一律隐藏:**资源管理器打开 / 关联 SSH / 环境变量 / 项目类型**。
/// 远程 Worktree 管理与本地共用入口，由授权确认拦住尚未启用的 helper；
/// 路径始终按所属 backend 解释，不拿远端路径打开本机位置。
/// 保留的是重命名 / 编辑描述 / 复制绝对路径(远程 POSIX)/ 分组操作 / 移除。
///
/// **分组那一段不在这张表里**:它是条件段(有没有分组、是不是子项目各不相同),
/// 由 [`group_section`] 在 [`ProjectMenuAction::GroupSection`] 这个位标处动态
/// 插入(自带前置分隔线,无内容时整段消失)—— 位置与原版一致:项目类型子菜单
/// 之后、「移除项目」那条分隔线之前;远程项目没有项目类型那一项,它就紧跟在
/// 「复制绝对路径」后面(同样是原版的位置)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectMenuAction {
    Rename,
    EditDescription,
    OpenInFolder,
    CopyAbsolutePath,
    /// 「关联 SSH」弹窗([`crate::ssh_assoc`])。
    AssociateSsh,
    /// 项目环境变量弹窗([`crate::env_vars`])。
    EnvVars,
    /// Worktree 管理弹窗(V 批建好的 `git_worktree::open`)。
    Worktrees,
    ToggleRemoteGit,
    /// 「项目类型」子菜单。
    ProjectKind,
    /// 分组段的位标,见类型注释。
    GroupSection,
    Remove,
}

fn project_menu_actions(is_remote: bool) -> Vec<Option<ProjectMenuAction>> {
    use ProjectMenuAction::*;
    let mut out = vec![Some(Rename), Some(EditDescription)];
    if !is_remote {
        out.push(Some(OpenInFolder));
    }
    out.push(Some(CopyAbsolutePath));
    if !is_remote {
        out.push(None);
        out.push(Some(AssociateSsh));
        out.push(Some(EnvVars));
        out.push(Some(Worktrees));
        out.push(Some(ProjectKind));
    }
    if is_remote {
        out.push(None);
        out.push(Some(ToggleRemoteGit));
        out.push(Some(Worktrees));
    }
    out.push(Some(GroupSection));
    out.push(None);
    out.push(Some(Remove));
    out
}

/// 子菜单里「当前选中」的标记。原版是文本方案(不是图标):选中 `✓ `,
/// 未选中一个**全角**空格 —— 两者宽度相同,菜单项文字才不会左右跳。
fn check_prefix(selected: bool) -> &'static str {
    if selected { "✓ " } else { "　" }
}

/// 「项目类型」子菜单。`current` 是 `kindOverride` 原文,`detected` 是探测结果
/// (`ProjectList.tsx:773`)。
fn kind_submenu(
    store: &Entity<AppStore>,
    project_id: &str,
    current: Option<&str>,
    detected: Option<ProjectKind>,
) -> Vec<MenuEntry> {
    let mut entries: Vec<MenuEntry> = Vec::new();

    let set = |kind: Option<&'static str>| {
        let store = store.clone();
        let project_id = project_id.to_string();
        move |_window: &mut Window, cx: &mut App| {
            store.update(cx, |store, cx| {
                store.set_project_kind_override(&project_id, kind, cx)
            });
        }
    };

    entries.push(
        MenuItem::new(format!(
            "{}{}{}",
            check_prefix(current.is_none()),
            t("projectList", "menu.projectKindAuto"),
            // 探测到什么写进**全角**括号里(原版 `（Rust）`);还没探完就不带括号
            detected.map(|k| format!("（{}）", k.label())).unwrap_or_default()
        ))
        .on_click(set(None))
        .into(),
    );
    entries.push(
        MenuItem::new(format!(
            "{}{}",
            check_prefix(current == Some("none")),
            t("projectList", "menu.projectKindHidden")
        ))
        .on_click(set(Some("none")))
        .into(),
    );
    entries.push(menu::separator());
    // 五十多种平铺成一条长龙没法用,按 TechCategory 分二级子菜单
    // (与「移动到分组」同一套嵌套菜单机制)。`ALL_PROJECT_KINDS` 已按分组聚拢,
    // 顺序扫一遍即可分段,不必先排序 —— 这条由 mt-ui 侧的单测钉着。
    for category in ALL_TECH_CATEGORIES {
        let kinds: Vec<&ProjectKind> = ALL_PROJECT_KINDS
            .iter()
            .filter(|k| k.category() == *category)
            .collect();
        if kinds.is_empty() {
            continue;
        }
        // 选中项藏在某个子菜单里时,父项也要标 ✓ —— 否则「现在选的是哪个」
        // 得逐个分组展开才找得到
        let selected_here = kinds.iter().any(|k| current == Some(k.as_str()));
        let children: Vec<MenuEntry> = kinds
            .iter()
            .map(|k| {
                let key = k.as_str();
                MenuItem::new(format!("{}{}", check_prefix(current == Some(key)), k.label()))
                    .on_click(set(Some(key)))
                    .into()
            })
            .collect();
        entries.push(
            MenuItem::new(format!(
                "{}{}",
                check_prefix(selected_here),
                t("projectList", category.i18n_key())
            ))
            .submenu(children)
            .into(),
        );
    }
    entries
}

/// 「移动到分组」树形子菜单(`ProjectList.tsx:76-110`)。
///
/// 三条不显然的规则,逐条抄自那边的注释:
/// - **按层级逐级展开,不拍平**;
/// - 含子组的组「既是落点又是入口」:带 submenu 的父项本身点不动,所以把
///   「移动到此分组」放进它子菜单的第一项,分隔线之后才是子组;
/// - 当前所在组标 `✓ ` 并置灰(移到原地是空操作),其余前缀一个全角空格对齐。
///
/// `current_parent_id` 对 worktree 子项目传 `None`:它不在树里,没有「当前组」
/// 可言 —— 选任意组都是有效动作(顺带脱离父项目)。
fn move_to_group_menu(
    items: &[ProjectTreeItem],
    depth: usize,
    current_parent_id: Option<&str>,
    store: &Entity<AppStore>,
    project_id: &str,
) -> Vec<MenuEntry> {
    let mut entries: Vec<MenuEntry> = Vec::new();
    for item in items {
        let ProjectTreeItem::Group(group) = item else {
            continue;
        };
        let is_current = Some(group.id.as_str()) == current_parent_id;
        // 项目落进该组后就到了 depth+1 层,超限则该组不可选(其子组更深,同样不可选)。
        // 原式是 `depth + 1 <= MAX_DEPTH`,与下面这个等价(clippy::int_plus_one)。
        let selectable = !is_current && depth < MAX_DEPTH;
        let label = format!("{}{}", check_prefix(is_current), group.name);
        let pick = {
            let store = store.clone();
            let project_id = project_id.to_string();
            let group_id = group.id.clone();
            move |_window: &mut Window, cx: &mut App| {
                store.update(cx, |store, cx| {
                    store.move_item(&project_id, Some(&group_id), None, cx);
                });
            }
        };
        let children = move_to_group_menu(&group.children, depth + 1, current_parent_id, store, project_id);
        if children.is_empty() {
            entries.push(
                MenuItem::new(label)
                    .disabled(!selectable)
                    .on_click(pick)
                    .into(),
            );
            continue;
        }
        let mut submenu = vec![
            MenuItem::new(t("projectList", "menu.moveToThisGroup"))
                .disabled(!selectable)
                .on_click(pick)
                .into(),
            menu::separator(),
        ];
        submenu.extend(children);
        entries.push(MenuItem::new(label).submenu(submenu).into());
    }
    entries
}

/// 项目行菜单里的分组段(`ProjectList.tsx:795-822`)。
///
/// 整段的出现条件:`有可移入的组 || 是子项目 || 已经在某个组里`。
/// 出现时前置一条分隔线。
fn group_section(store: &Entity<AppStore>, row: &Row, tree: &[ProjectTreeItem]) -> Vec<MenuEntry> {
    let move_to = move_to_group_menu(
        tree,
        0,
        if row.is_child {
            None
        } else {
            row.parent_group_id.as_deref()
        },
        store,
        &row.id,
    );
    if move_to.is_empty() && !row.is_child && row.parent_group_id.is_none() {
        return Vec::new();
    }
    let mut entries = vec![menu::separator()];
    let detach = {
        let store = store.clone();
        let id = row.id.clone();
        move |_window: &mut Window, cx: &mut App| {
            store.update(cx, |store, cx| {
                store.move_item(&id, None, None, cx);
            });
        }
    };
    if row.is_child {
        // 脱离父项目 = 清 parentProjectId 并转为顶层树节点(move_item 内处理)
        entries.push(menu::item(t("projectList", "menu.detachFromParent"), detach));
    } else if row.parent_group_id.is_some() {
        entries.push(menu::item(t("projectList", "menu.moveOutOfGroup"), detach));
    }
    if !move_to.is_empty() {
        entries.push(
            MenuItem::new(t("projectList", "menu.moveToGroup"))
                .submenu(move_to)
                .into(),
        );
    }
    entries
}

/// 组装一行的右键菜单。`view` 是列表本体 —— 「重命名」是**行内编辑**,
/// 得回到视图里置编辑态(原版右键菜单与 F2 调的是同一个 `startRenameProject`)。
fn project_menu(
    view: &Entity<ProjectList>,
    store: &Entity<AppStore>,
    row: &Row,
    tree: &[ProjectTreeItem],
    cx: &App,
) -> Vec<MenuEntry> {
    let mut entries = Vec::new();
    for action in project_menu_actions(row.remote.is_some()) {
        let Some(action) = action else {
            entries.push(menu::separator());
            continue;
        };
        entries.push(match action {
            ProjectMenuAction::Rename => {
                let view = view.clone();
                let id = row.id.clone();
                let name = row.name.clone();
                menu::item(t("projectList", "menu.rename"), move |window, cx| {
                    let (id, name) = (id.clone(), name.clone());
                    view.update(cx, |this: &mut ProjectList, cx| {
                        this.start_rename(id, false, &name, window, cx);
                    });
                })
            }
            ProjectMenuAction::Worktrees => {
                let id = row.id.clone();
                let source_store = store.clone();
                menu::item(t("projectList", "menu.worktrees"), move |window, cx| {
                    if source_store.read(cx).is_remote_project(&id)
                        && !crate::git::backend::remote_git_enabled(source_store.read(cx), &id)
                    {
                        crate::git::backend::request_enable(
                            source_store.clone(),
                            id.clone(),
                            window,
                            cx,
                        );
                        return;
                    }
                    crate::git::worktree::open(
                        crate::git::backend::project_context(source_store.read(cx), &id),
                        // 项目里可能挂着多个仓库,与原版的项目级入口同口径
                        true,
                        Some(id.clone()),
                        // 原版这个入口的 onChanged 是空的(项目列表自己有
                        // 焦点重探那条路把新 worktree 的徽章补上)
                        |_cx| {},
                        window,
                        cx,
                    );
                })
            }
            ProjectMenuAction::ToggleRemoteGit => {
                let id = row.id.clone();
                let source_store = store.clone();
                let enabled = crate::git::backend::remote_git_enabled(store.read(cx), &id);
                let broken = store.read(cx).remote_connection_of(&id).is_none();
                MenuItem::new(t(
                    "projectList",
                    if enabled {
                        "menu.disableRemoteGit"
                    } else {
                        "menu.enableRemoteGit"
                    },
                ))
                .disabled(broken)
                .detail(if broken {
                    t("gitHistory", "connectionRemoved")
                } else {
                    ""
                })
                .on_click(move |window, cx| {
                    if enabled {
                        source_store.update(cx, |store, cx| {
                            store.set_project_remote_git_enabled(&id, false, cx)
                        });
                    } else {
                        crate::git::backend::request_enable(
                            source_store.clone(),
                            id.clone(),
                            window,
                            cx,
                        );
                    }
                })
                .into()
            }
            ProjectMenuAction::EditDescription => {
                let store = store.clone();
                let id = row.id.clone();
                let current = row.description.clone().unwrap_or_default();
                menu::item(t("projectList", "menu.editDescription"), move |window, cx| {
                    let store = store.clone();
                    let id = id.clone();
                    crate::prompt::show_prompt(
                        t("projectList", "menu.editDescription"),
                        t("projectList", "descriptionPlaceholder"),
                        current.clone(),
                        move |value, _window, cx| {
                            // 空串 = 清除(原版 `setProjectDescription(id, next.trim())`)
                            store.update(cx, |store, cx| {
                                store.set_project_description(&id, &value, cx)
                            });
                        },
                        window,
                        cx,
                    );
                })
            }
            ProjectMenuAction::OpenInFolder => {
                let path = PathBuf::from(&row.path);
                menu::item(t("projectList", "menu.openInFolder"), move |_window, cx| {
                    // spawn 外部进程会卡(网络盘 / 杀软),`open_external` 丢后台
                    fs_ops::open_external(fs_ops::ExternalOpen::Reveal, path.clone(), cx);
                })
            }
            ProjectMenuAction::CopyAbsolutePath => {
                let path = row.path.clone();
                menu::item(
                    t("projectList", "menu.copyAbsolutePath"),
                    move |_window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                    },
                )
            }
            ProjectMenuAction::AssociateSsh => {
                let store = store.clone();
                let id = row.id.clone();
                menu::item(t("projectList", "menu.associateSsh"), move |window, cx| {
                    crate::ssh_assoc::open(store.clone(), &id, window, cx);
                })
            }
            ProjectMenuAction::EnvVars => {
                let store = store.clone();
                let id = row.id.clone();
                menu::item(t("projectList", "menu.envVars"), move |window, cx| {
                    crate::env_vars::open(store.clone(), &id, window, cx);
                })
            }
            ProjectMenuAction::ProjectKind => {
                MenuItem::new(t("projectList", "menu.projectKind"))
                    .submenu(kind_submenu(
                        store,
                        &row.id,
                        row.kind_override.as_deref(),
                        row.detected_kind,
                    ))
                    .into()
            }
            ProjectMenuAction::GroupSection => {
                // 位标本身不产生菜单项:整段(含前置分隔线)由 group_section 给,
                // 没内容时一条都不加
                entries.extend(group_section(store, row, tree));
                continue;
            }
            ProjectMenuAction::Remove => {
                let store = store.clone();
                let (id, name, path) = (row.id.clone(), row.name.clone(), row.path.clone());
                MenuItem::new(t("projectList", "menu.remove"))
                    .danger()
                    .on_click(move |window, cx| {
                        // 与 × 按钮同一条确认路径(原版也是同一个 confirmTarget)
                        modal::open_confirm_remove_project(
                            store.clone(),
                            id.clone(),
                            name.clone(),
                            path.clone(),
                            window,
                            cx,
                        );
                    })
                    .into()
            }
        });
    }
    entries
}

/// 分组行右键菜单(`ProjectList.tsx:965-1007`)。六项连排、**无分隔线**。
fn group_menu(
    view: &Entity<ProjectList>,
    store: &Entity<AppStore>,
    group: &GroupRow,
) -> Vec<MenuEntry> {
    let mut entries: Vec<MenuEntry> = Vec::new();

    entries.push({
        let view = view.clone();
        let id = group.id.clone();
        let name = group.name.clone();
        menu::item(t("projectList", "menu.renameGroup"), move |window, cx| {
            let (id, name) = (id.clone(), name.clone());
            // 与项目行同一条:行内编辑(原版 `startRenameGroup`)
            view.update(cx, |this: &mut ProjectList, cx| {
                this.start_rename(id, true, &name, window, cx);
            });
        })
    });

    entries.push({
        let store = store.clone();
        let id = group.id.clone();
        menu::item(t("projectList", "menu.addProject"), move |window, cx| {
            modal::open_add_project_into(store.clone(), Some(id.clone()), window, cx);
        })
    });

    // 「添加远程项目」紧跟「添加项目」(原版 `ProjectList.tsx:971`):
    // 新项目直接落进该分组,分组折叠则展开
    entries.push({
        let store = store.clone();
        let id = group.id.clone();
        menu::item(
            t("projectList", "menu.addRemoteProject"),
            move |window, cx| {
                crate::remote_project::open(store.clone(), Some(id.clone()), window, cx);
            },
        )
    });

    if group.depth > 0 {
        entries.push({
            let store = store.clone();
            let id = group.id.clone();
            menu::item(t("projectList", "menu.moveOutOfGroup"), move |_window, cx| {
                let id = id.clone();
                store.update(cx, |store, cx| {
                    store.move_item(&id, None, None, cx);
                });
            })
        });
    }

    // 「新建子组」的显隐条件与原版同式:groupDepth < MAX_DEPTH - 1
    if group.depth + 1 < MAX_DEPTH {
        entries.push({
            let store = store.clone();
            let id = group.id.clone();
            menu::item(t("projectList", "menu.newSubgroup"), move |window, cx| {
                let store = store.clone();
                let id = id.clone();
                crate::prompt::show_prompt(
                    t("projectList", "newSubgroup"),
                    t("projectList", "newSubgroupPlaceholder"),
                    "",
                    move |value, _window, cx| {
                        store.update(cx, |store, cx| store.create_group(&value, Some(&id), cx));
                    },
                    window,
                    cx,
                );
            })
        });
    }

    entries.push({
        let store = store.clone();
        let id = group.id.clone();
        let name = group.name.clone();
        let count = group.count;
        MenuItem::new(t("projectList", "menu.deleteGroup"))
            .danger()
            .on_click(move |window, cx| {
                // 删组不删项目,但组内项目会散回上一级 —— 组织结构没得撤销,先确认
                let store = store.clone();
                let id = id.clone();
                crate::prompt::Confirm::new(
                    t("projectList", "deleteGroupConfirm.title"),
                    tr!(
                        "projectList",
                        "deleteGroupConfirm.message",
                        name = name.clone(),
                        count = count
                    ),
                )
                .open(
                    move |_window, cx| {
                        let id = id.clone();
                        store.update(cx, |store, cx| store.remove_group(&id, cx));
                    },
                    window,
                    cx,
                );
            })
            .into()
    });

    entries
}

/// 「添加本地项目」领位图标（与原底栏按钮同款目录图标 + `--color-file`）。
fn icon_add_local(_window: &mut Window, _cx: &mut App) -> AnyElement {
    FileIcon::folder(false)
        .size(px(14.0))
        .color(ui::color_file())
        .into_any_element()
}

/// 「添加远程项目」领位图标（与原底栏按钮同款服务器图标 + `--color-info`）。
fn icon_add_remote(_window: &mut Window, _cx: &mut App) -> AnyElement {
    VectorIcon::new(SERVER, px(14.0))
        .ink(ui::color_info())
        .into_any_element()
}

/// 「新建分组 / 新建子组」领位图标（与原底栏按钮同款容器图标 + `--color-folder`）。
fn icon_new_group(_window: &mut Window, _cx: &mut App) -> AnyElement {
    VectorIcon::new(dnd::BOXES_SHAPES, px(13.0))
        .ink(ui::color_folder())
        .into_any_element()
}

/// 栏头 `+` / 空白右键的统一新建菜单：添加本地项目 / 添加远程项目 / 新建分组。
/// 每项都写明动作并保留与原底栏按钮一致的领位图标（规避 issue #83 裸「SSH」歧义）。
fn add_menu(store: &Entity<AppStore>) -> Vec<MenuEntry> {
    let store_local = store.clone();
    let store_remote = store.clone();
    let store_group = store.clone();
    vec![
        MenuItem::new(t("projectList", "addProject"))
            .icon(icon_add_local)
            .on_click(move |window, cx| {
                modal::open_add_project(store_local.clone(), window, cx);
            })
            .into(),
        MenuItem::new(t("projectList", "addRemoteProject"))
            .icon(icon_add_remote)
            .on_click(move |window, cx| {
                crate::remote_project::open(store_remote.clone(), None, window, cx);
            })
            .into(),
        menu::separator(),
        MenuItem::new(t("projectList", "newGroup"))
            .icon(icon_new_group)
            .on_click(move |window, cx| {
                let store = store_group.clone();
                crate::prompt::show_prompt(
                    t("projectList", "newGroup"),
                    t("projectList", "newGroupPlaceholder"),
                    "",
                    move |value, _window, cx| {
                        store.update(cx, |store, cx| store.create_group(&value, None, cx));
                    },
                    window,
                    cx,
                );
            })
            .into(),
    ]
}

/// 分组行 hover `+` 快捷菜单：直接向该分组添加本地/远程项目或新建子组。
fn group_add_menu(store: &Entity<AppStore>, group_id: &str, depth: usize) -> Vec<MenuEntry> {
    let mut entries: Vec<MenuEntry> = vec![
        {
            let store = store.clone();
            let id = group_id.to_string();
            MenuItem::new(t("projectList", "addProject"))
                .icon(icon_add_local)
                .on_click(move |window, cx| {
                    modal::open_add_project_into(store.clone(), Some(id.clone()), window, cx);
                })
                .into()
        },
        {
            let store = store.clone();
            let id = group_id.to_string();
            MenuItem::new(t("projectList", "addRemoteProject"))
                .icon(icon_add_remote)
                .on_click(move |window, cx| {
                    crate::remote_project::open(store.clone(), Some(id.clone()), window, cx);
                })
                .into()
        },
    ];
    if depth + 1 < MAX_DEPTH {
        entries.push(menu::separator());
        let store = store.clone();
        let id = group_id.to_string();
        entries.push(
            MenuItem::new(t("projectList", "menu.newSubgroup"))
                .icon(icon_new_group)
                .on_click(move |window, cx| {
                    let store = store.clone();
                    let id = id.clone();
                    crate::prompt::show_prompt(
                        t("projectList", "newSubgroup"),
                        t("projectList", "newSubgroupPlaceholder"),
                        "",
                        move |value, _window, cx| {
                            store.update(cx, |store, cx| store.create_group(&value, Some(&id), cx));
                        },
                        window,
                        cx,
                    );
                })
                .into(),
        );
    }
    entries
}

// ─── 揭示(见模块注释) ────────────────────────────────────────

struct GlobalProjectList(Entity<ProjectList>);
impl Global for GlobalProjectList {}

/// 登记全局的列表实体,[`reveal`] 经它找到列表。`Workspace` 建完列表调一次。
pub fn install(list: Entity<ProjectList>, cx: &mut App) {
    cx.set_global(GlobalProjectList(list));
}

/// 在「项目」块里揭示项目:展开挡路的折叠分组,下一帧把那一行滚进可视区。
/// 给**列表之外**的切项目入口用,调用方先把它切成活动项目再调这里。
///
/// 整段排到本轮 effect 之后(`cx.defer`):调用方常常正拿着 store 的租约
/// (`open_added_project`)或列表自己的租约(外部拖放),当场 `update` 就是嵌套
/// 租约,gpui 直接 panic。列表还没装上(单测里的 store)时什么都不做。
pub fn reveal(project_id: &str, cx: &mut App) {
    let project_id = project_id.to_string();
    cx.defer(move |cx| {
        let Some(list) = cx.try_global::<GlobalProjectList>().map(|g| g.0.clone()) else {
            return;
        };
        list.update(cx, |list, cx| list.reveal(project_id, cx));
    });
}

/// 揭示目标是列表滚动容器的第几个子元素(给 [`ScrollHandle::scroll_to_item`])。
///
/// `render_rows` 把展平表**逐条一一**铺成子元素(分组行、项目行各占一个;展平表只收
/// `config.projects` 里真有的项目,行渲染那道 `continue` 不会跳号),所以下标就是它
/// 在表里的位置。只认项目行 —— 不在表里(所在分组还折叠着 / 已被删)→ `None`。
fn reveal_row_index(ordered: &[OrderedItem], project_id: &str) -> Option<usize> {
    ordered
        .iter()
        .position(|item| matches!(item, OrderedItem::Project { id, .. } if id == project_id))
}

/// 展平表里一行的 id(分组行与项目行各在各的名字空间,不会撞)。
fn ordered_item_id(item: &OrderedItem) -> &str {
    match item {
        OrderedItem::Group { id, .. } | OrderedItem::Project { id, .. } => id,
    }
}

// ─── 视图 ─────────────────────────────────────────────────────

/// 行内重命名的编辑态(项目行与分组行共用一份 —— 同时只可能编辑一行)。
struct Editing {
    id: String,
    is_group: bool,
    input: Entity<InputState>,
    /// 提交路径之一:输入框失焦。**取消时必须连它一起丢掉**,
    /// 否则丢焦点那一下又把值提交回去了(Esc 等于没按)。
    _sub: Subscription,
}

pub struct ProjectList {
    store: Entity<AppStore>,
    /// 正在被拖的节点 id(拖影起来那一刻记下),源行据此变淡。
    /// 渲染时与 `cx.has_active_drag()` 与门 —— 拖拽中断(Esc / 松手在窗外)不会留脏。
    dragging: Option<String>,
    /// 落点指示,见 [`DropIndicator`]。
    drop_indicator: Option<DropIndicator>,
    /// 外部文件拖到列表上方时的三态提示。
    external: Option<ExternalDrag>,
    /// 正在行内重命名的那一行。
    editing: Option<Editing>,
    /// 鼠标停在哪一行 —— 行尾悬浮操作胶囊与预览卡的锚点靠它。
    hovered: Option<String>,
    /// 鼠标当前是否停在行尾悬浮操作浮层 / 折叠胶囊上：停在按钮上时不弹 520px 大预览卡。
    hover_on_action: bool,
    /// 单击切换、双击已激活行重命名的连点守卫（防切过来紧接一下误触重命名）。
    row_click: SwitchClickGuard,
    /// 拖放到列表底部弹性空白区的落点高亮态。
    drop_to_root_end: bool,
    /// 拖拽悬停在折叠分组上准备 550ms 自动展开的目标分组 id。
    spring_open_group: Option<String>,
    /// 折叠分组自动展开计时器（离开或丢弃即取消）。
    _spring_open_task: Option<Task<()>>,
    /// 行内筛选条是否展开。
    filter_open: bool,
    /// 行内筛选条的高度裁剪展开 / 收起动画。
    filter_reveal: crate::height_reveal::HeightReveal,
    /// 行内状态筛选档位（全部 / AI 活跃 / 远程）。
    filter_mode: ProjectFilterMode,
    /// 行内筛选输入框状态（首次展开时懒建）。
    filter_input: Option<Entity<InputState>>,
    /// 行内筛选输入框事件订阅。
    _filter_sub: Option<Subscription>,
    /// 打开筛选条后在下一帧聚焦输入框。
    pending_filter_focus: bool,
    /// 项目路径 → worktree 分支名。批量探测的结果,见 [`Self::probe_worktrees`]。
    worktree_branches: HashMap<String, String>,
    /// 上次探测用的路径清单(拼成一条),变了才重探。
    probe_key: String,
    /// 上次喂给技术栈探测的**待探路径**清单(拼成一条),变了才再喂一次。
    /// 见 [`Self::ensure_project_kinds`]。
    kinds_key: String,
    /// 上次看到的窗口聚焦态。`false → true` 是原版 `onFocusChanged` 那条重探时机。
    was_focused: bool,
    /// 每行一个焦点句柄(原版每行 `tabIndex={0}`)。行拿到焦点之后
    /// Enter/Space、Delete、F2 才有落点,见 [`Self::on_project_key`]。
    row_focus: HashMap<String, FocusHandle>,
    /// 悬停缩略图开在哪一行(key = 项目 id)+ 弹出那一刻行的屏幕矩形 + 开合 / 换卡
    /// 动画(见 [`pane_preview::HoverCard`])。矩形与原版「到点时才
    /// `getBoundingClientRect()`」同一时机(悬停这 250ms 里列表可能增删过行,进入那一刻
    /// 的矩形已经不作数)。
    preview: pane_preview::HoverCard<String>,
    /// 250ms 计时 + 开着之后的 500ms 续活节拍。**丢掉句柄就等于 `clearTimeout`**。
    _preview_task: Option<Task<()>>,
    /// 正被悬停的那一行的屏幕矩形(只给被悬停的那一行挂 `canvas` 量)。
    hover_rect: Option<Bounds<Pixels>>,
    /// 刚进编辑态、还等着「默认全选」的那个输入框。见 [`Self::start_rename`]。
    pending_select_all: Option<FocusHandle>,
    /// 各行完成标的进出场(`tagFadeIn` / 收回),按项目 id 索引。
    ///
    /// **标出现时建(从头播进场)、该消失时转退场、收完才丢** —— 丢掉之后再出现按
    /// 「新挂载」处理,从头播一遍。表在 `render` 的前置段维护(`render_project` 拿的是
    /// `&self`,建不了)。
    done_tags: HashMap<String, DoneTagAnim>,
    /// 中栏收着期间 store 变过(本视图那时不渲染,完成标表没跟上)。下一次 `render`
    /// 按「隔了一阵才再画」对齐完成标表,不补播进出场(见 [`Self::sync_done_tags`])。
    done_tags_stale: bool,
    /// 分组行与 worktree 簇的折叠箭头:只让刚点(或回车 / 空格翻)的那一行转(见 `fold_motion`)。
    chevrons: crate::fold_motion::FlipTweens,
    /// 分组(`grp:{id}`)与 worktree 簇(`wt:{id}`)的高度裁剪展开 / 收起动画(仅在飞期间存活)。
    folds: crate::height_reveal::FoldReveals,
    /// 已折叠收起 worktree 子项的父项目 id 集合（默认展开）。
    collapsed_worktrees: HashSet<String>,
    /// 行列表那个滚动容器的句柄(滚动偏移也住在这里)。揭示靠它把目标行滚进可视区。
    rows_scroll: ScrollHandle,
    /// 等着滚进可视区的项目,由 [`Self::reveal`] 登记、接下来的 `render` 兑现。
    pending_reveal: Option<PendingReveal>,
    /// 监听本地主仓库的 `.git` 与 `.git/worktrees` 目录，外部/终端增删 worktree 时实时同步。
    worktree_watcher: Arc<mt_project::watch::FsWatcher>,
    /// 当前挂着监听的目录集合（增量增删，避免重复注册）。
    watched_worktree_dirs: HashSet<PathBuf>,
    /// 上一次自动同步 worktree 用的本地非子项目路径清单。
    worktree_sync_key: String,
    /// 本轮同步代次号（丢弃过期后台扫描结果）。
    worktree_sync_gen: u64,
    /// `.git/worktrees` 变更防抖 / 慢 checkout 重试任务（替换即取消旧计时）。
    _worktree_sync_debounce: Option<Task<()>>,
    /// 监听事件唤醒任务。
    _worktree_watch_task: Task<()>,
}

/// 一次揭示的兑现进度(见 `render` 里兑现那一段)。
struct PendingReveal {
    project_id: String,
    /// 兑现之后还要不要再校准一帧。
    recheck: bool,
}

impl ProjectList {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        // 本视图套了 view 级缓存(`main.rs::cached_panel`),按 `touches_project_list`
        // 细粒度订阅:OSC 标题(4Hz 热路径)、焦点 pane、AI 标记与文件树展开态不再唤醒本视图。
        // 中栏收着时本视图不渲染(这条照样跑):记一笔,再展开时完成标表直接对齐、不补播
        // (见 `sync_done_tags`)。
        cx.subscribe(&store, |this: &mut Self, store, event: &StoreEvent, cx| {
            if !event.touches_project_list() {
                return;
            }
            if !store.read(cx).config().middle_column_visible {
                this.done_tags_stale = true;
            }
            cx.notify();
        })
        .detach();
        // 三道后台探测的闸只在项目表 / 技术栈缓存 / 窗口聚焦变了时过
        // (`StoreEvent::touches_project_probes`),不再被 OSC 标题 / AI 状态叫醒
        cx.subscribe(&store, |this: &mut Self, _, event: &StoreEvent, cx| {
            if !event.touches_project_probes() {
                return;
            }
            // 项目路径集合变了(增删项目 / worktree 变项目)→ 重探徽章与本地主仓库 worktree
            this.probe_worktrees(false, cx);
            this.sync_worktrees(false, 0, cx);
            // 技术栈探测(原版 `useProjectKinds` 那个 effect):列表变了就补探,
            // 缓存失效(`removeDirKind`)后同样由这条重跑补上
            this.ensure_project_kinds(cx);
            // 窗口重新聚焦:分支切换与外部 `git worktree add/remove` 都可能发生在窗外,
            // 回来时重探徽章、同步新增 worktree 并清理已失效子项目
            let focused = this.store.read(cx).window_focused();
            if focused && !this.was_focused {
                this.probe_worktrees(true, cx);
                this.sync_worktrees(true, 0, cx);
                this.reconcile_worktrees(cx);
            }
            this.was_focused = focused;
        })
        .detach();
        let (wt_tx, mut wt_rx) = mpsc::unbounded::<()>();
        let worktree_watcher = Arc::new(mt_project::watch::FsWatcher::with_label(
            "worktrees",
            move |change| {
                if is_worktree_watch_event(&change) {
                    let _ = wt_tx.unbounded_send(());
                }
            },
        ));
        let worktree_watch_task = cx.spawn(async move |this, cx| {
            while wt_rx.next().await.is_some() {
                while wt_rx.try_recv().is_ok() {}
                if this
                    .update(cx, |list: &mut ProjectList, cx| {
                        list.schedule_worktree_sync(Duration::from_millis(400), 0, cx);
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        let mut this = Self {
            store,
            dragging: None,
            drop_indicator: None,
            external: None,
            editing: None,
            hovered: None,
            hover_on_action: false,
            row_click: SwitchClickGuard::default(),
            drop_to_root_end: false,
            spring_open_group: None,
            _spring_open_task: None,
            filter_open: false,
            filter_reveal: crate::height_reveal::HeightReveal::new(false),
            filter_mode: ProjectFilterMode::All,
            filter_input: None,
            _filter_sub: None,
            pending_filter_focus: false,
            worktree_branches: HashMap::new(),
            probe_key: String::new(),
            kinds_key: String::new(),
            was_focused: true,
            row_focus: HashMap::new(),
            preview: pane_preview::HoverCard::default(),
            _preview_task: None,
            hover_rect: None,
            pending_select_all: None,
            done_tags: HashMap::new(),
            done_tags_stale: false,
            chevrons: crate::fold_motion::FlipTweens::new(mt_ui::motion::CHEVRON_ROTATE),
            folds: crate::height_reveal::FoldReveals::new(mt_ui::motion::SECTION_TOGGLE)
                .with_max_height(640.0),
            collapsed_worktrees: HashSet::new(),
            rows_scroll: ScrollHandle::new(),
            pending_reveal: None,
            worktree_watcher,
            watched_worktree_dirs: HashSet::new(),
            worktree_sync_key: String::new(),
            worktree_sync_gen: 0,
            _worktree_sync_debounce: None,
            _worktree_watch_task: worktree_watch_task,
        };
        // 挂载时先探一次(原版两个 effect 都在挂载时跑一遍)
        this.probe_worktrees(true, cx);
        this.sync_worktrees(true, 0, cx);
        this.reconcile_worktrees(cx);
        this.ensure_project_kinds(cx);
        this
    }

    /// 补探项目根目录的技术栈(原版 `useProjectKinds` 的 effect)。
    ///
    /// **远程项目不探**:领位固定 SSH 图标,路径也不是本机能列的位置
    /// (GPUI 侧还没有远程项目,判据照写,mt-ssh 接上自动生效)。
    /// 「探过就不再探」的判据在 store 那边(`dir_kinds`),这里只负责**不去白喂**
    /// ——见下面那道与 [`Self::probe_worktrees`] 同款的去重闸。
    fn ensure_project_kinds(&mut self, cx: &mut Context<Self>) {
        // 去重闸,与 [`Self::probe_worktrees`] 同款:这个方法挂在 store 事件上,
        // 项目表 / 技术栈缓存 / 窗口聚焦任一变化都会走一遍(每探完一个目录就有
        // 一次),此前每次都要把全部项目路径克隆成一个 `Vec<String>` 再喂给一个
        // 只会全部命中缓存的去重表。先只拼一条比较用的键,确定有新东西要探了
        // 才真去收集路径。
        //
        // ⚠️ **键只统计「还没探过」的路径**,不是全部路径。缓存被
        // `remove_dir_kind` 失效(项目根的标记文件变动)之后,那条路径会重新
        // 落进这个集合、键随之变化,重探照旧发生 —— 拿全量路径当键会把
        // 「失效后由这条重跑补上」那条通路闸死。
        let mut key = String::new();
        {
            let store = self.store.read(cx);
            for p in store
                .projects()
                .iter()
                .filter(|p| p.ssh_connection_id.is_none())
            {
                if store.dir_kind(&p.path).is_none() {
                    key.push_str(&p.path);
                    key.push('\n');
                }
            }
        }
        if key == self.kinds_key {
            return;
        }
        self.kinds_key = key;
        let paths: Vec<String> = {
            let store = self.store.read(cx);
            store
                .projects()
                .iter()
                .filter(|p| p.ssh_connection_id.is_none() && store.dir_kind(&p.path).is_none())
                .map(|p| p.path.clone())
                .collect()
        };
        if paths.is_empty() {
            return;
        }
        self.store
            .update(cx, |store, cx| store.ensure_dir_kinds(paths, cx));
    }

    /// 当前行内筛选框的规范化（trim + lowercase）查询词。
    fn filter_query_lower(&self, cx: &App) -> String {
        self.filter_input
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_lowercase())
            .unwrap_or_default()
    }

    /// 行内筛选是否正在生效（有状态芯片过滤或非空关键词）。
    fn is_filtering(&self, cx: &App) -> bool {
        self.filter_mode != ProjectFilterMode::All || !self.filter_query_lower(cx).is_empty()
    }

    /// 懒建行内筛选输入框并订阅变更与回车事件。
    fn ensure_filter_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(input) = self.filter_input.as_ref() {
            return input.clone();
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t("projectList", "filter.placeholder"))
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, _, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => {
                    this.folds.clear();
                    this.close_preview(cx);
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    this.activate_first_filtered_project(cx);
                }
                _ => {}
            },
        );
        self.filter_input = Some(input.clone());
        self._filter_sub = Some(sub);
        input
    }

    /// 打开并聚焦行内筛选条。
    fn open_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_filter_input(window, cx);
        self.filter_open = true;
        self.filter_reveal.set_open(true);
        self.pending_filter_focus = true;
        self.close_preview(cx);
        cx.notify();
    }

    /// 清空筛选条件（保持筛选条开合状态不变）。
    fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_mode = ProjectFilterMode::All;
        if let Some(input) = self.filter_input.clone() {
            input.update(cx, |state, cx| state.set_value(String::new(), window, cx));
        }
        self.close_preview(cx);
        cx.notify();
    }

    /// 收起行内筛选条；`clear = true` 时一并清空关键词与状态筛选。
    fn close_filter(&mut self, clear: bool, window: &mut Window, cx: &mut Context<Self>) {
        if clear {
            self.filter_mode = ProjectFilterMode::All;
            if let Some(input) = self.filter_input.clone() {
                input.update(cx, |state, cx| state.set_value(String::new(), window, cx));
            }
        }
        self.filter_open = false;
        self.filter_reveal.set_open(false);
        self.pending_filter_focus = false;
        self.close_preview(cx);
        cx.notify();
    }

    /// 栏头放大镜点击：开着就收起并清条件，关着就展开并聚焦。
    fn toggle_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_open {
            self.close_filter(true, window, cx);
        } else {
            self.open_filter(window, cx);
        }
    }

    /// 筛选框按回车：激活当前过滤结果里的第一个项目。
    fn activate_first_filtered_project(&mut self, cx: &mut Context<Self>) {
        let first_id = {
            let ordered = self.visible_ordered(self.store.read(cx), cx);
            ordered.into_iter().find_map(|item| match item {
                OrderedItem::Project { id, .. } => Some(id),
                OrderedItem::Group { .. } => None,
            })
        };
        if let Some(id) = first_id {
            self.store
                .update(cx, |store, cx| store.set_active_project(&id, cx));
        }
    }

    /// 筛选框按 `↓`：把焦点移入列表首行并滚进可视区。
    fn focus_first_visible_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let first_id = {
            let ordered = self.visible_ordered(self.store.read(cx), cx);
            ordered.first().map(|item| ordered_item_id(item).to_string())
        };
        if let Some(id) = first_id
            && let Some(focus) = self.row_focus.get(&id).cloned()
        {
            self.folds.clear();
            self.close_preview(cx);
            window.focus(&focus, cx);
            self.rows_scroll.scroll_to_item(0);
            cx.notify();
        }
    }

    /// 当前侧栏可见的展平列表：
    /// - 常规态：在 `get_ordered_tree` 基础上裁掉折叠着的 worktree 子项；
    /// - 筛选态：以临时全展开视角按状态芯片与关键词剪枝，绝不改写原树折叠态。
    ///
    /// 渲染、揭示行号、焦点同步、完成标同步与键盘导航共用同一份结果。
    fn visible_ordered(&self, store: &AppStore, cx: &App) -> Vec<OrderedItem> {
        let query_lower = self.filter_query_lower(cx);
        let text_active = !query_lower.is_empty();
        if self.filter_mode == ProjectFilterMode::All && !text_active {
            let ordered = project_tree::get_ordered_tree(store.config());
            return project_tree::filter_collapsed_worktrees(
                ordered,
                store.projects(),
                &self.collapsed_worktrees,
            );
        }
        let active_id = store.active_project_id.as_deref();
        let mode = self.filter_mode;
        let status_ok = |pid: &str| -> bool {
            let Some(p) = store.project(pid) else {
                return false;
            };
            match mode {
                ProjectFilterMode::All => true,
                ProjectFilterMode::Remote => crate::ssh_conn::is_remote_project(p),
                ProjectFilterMode::AiActive => {
                    let st = store.project_state(pid);
                    let light = st.map(|s| s.highest_light()).unwrap_or_default();
                    let needs = st.map(|s| s.needs_attention).unwrap_or(false);
                    is_ai_active_project(light, needs, active_id == Some(pid))
                }
            }
        };
        let project_text_match = |pid: &str| -> bool {
            let Some(p) = store.project(pid) else {
                return false;
            };
            if p.name.to_lowercase().contains(&query_lower) {
                return true;
            }
            if p.description
                .as_deref()
                .is_some_and(|d| d.to_lowercase().contains(&query_lower))
            {
                return true;
            }
            if p.ssh_connection_id.is_none()
                && self
                    .worktree_branches
                    .get(&p.path)
                    .is_some_and(|b| b.to_lowercase().contains(&query_lower))
            {
                return true;
            }
            if let Some(conn) = crate::ssh_conn::remote_connection(p, store.ssh_connections())
                && conn.name.to_lowercase().contains(&query_lower)
            {
                return true;
            }
            false
        };
        let tree = store.config().project_tree.as_deref().unwrap_or(&[]);
        let group_text_match = |gid: &str| -> bool {
            project_tree::find_group_in_tree(tree, gid)
                .is_some_and(|g| g.name.to_lowercase().contains(&query_lower))
        };
        project_tree::filter_ordered_tree(
            store.config(),
            &status_ok,
            text_active,
            &project_text_match,
            &group_text_match,
        )
    }

    /// 栏头「全部折叠 / 全部展开」的当前方向：`true` = 存在任意展开分组或展开 worktree 簇
    /// （点一下全部折叠），`false` = 已全折（点一下全部展开）。
    fn any_foldable_expanded(&self, store: &AppStore) -> (bool, bool) {
        let tree = store.config().project_tree.as_deref().unwrap_or(&[]);
        let has_group = project_tree::has_any_group(tree);
        let wt_parents: Vec<&str> = store
            .projects()
            .iter()
            .filter(|p| {
                p.parent_project_id.is_none()
                    && !project_tree::worktree_descendants(store.projects(), &p.id).is_empty()
            })
            .map(|p| p.id.as_str())
            .collect();
        let has_any = has_group || !wt_parents.is_empty();
        if !has_any {
            return (false, false);
        }
        let any_expanded = project_tree::has_any_expanded_group(tree)
            || wt_parents
                .iter()
                .any(|pid| !self.collapsed_worktrees.contains(*pid));
        (true, any_expanded)
    }

    /// 一键全部折叠或全部展开（同步驱动全部分组与 worktree 簇的箭头与抽屉动画）。
    fn toggle_all_folds(&mut self, cx: &mut Context<Self>) {
        let (has_any, any_expanded) = self.any_foldable_expanded(self.store.read(cx));
        if !has_any {
            return;
        }
        let next_collapsed = any_expanded;
        let next_open = !next_collapsed;

        // 1. 全部 worktree 父项目
        let wt_parents: Vec<String> = {
            let store = self.store.read(cx);
            store
                .projects()
                .iter()
                .filter(|p| {
                    p.parent_project_id.is_none()
                        && !project_tree::worktree_descendants(store.projects(), &p.id).is_empty()
                })
                .map(|p| p.id.clone())
                .collect()
        };
        for pid in wt_parents {
            let changed = if next_collapsed {
                self.collapsed_worktrees.insert(pid.clone())
            } else {
                self.collapsed_worktrees.remove(&pid)
            };
            if changed {
                let key = format!("wt:{pid}");
                self.chevrons
                    .flip_open(key.clone(), next_open, crate::fold_motion::OPEN_TURNS);
                self.folds.flip_open(key, next_open);
            }
        }

        // 2. 全部分组
        let changed_groups = self
            .store
            .update(cx, |store, cx| store.set_all_groups_collapsed(next_collapsed, cx));
        for gid in changed_groups {
            self.chevrons
                .flip_open(gid.clone(), next_open, crate::fold_motion::OPEN_TURNS);
            self.folds.flip_open(format!("grp:{gid}"), next_open);
        }
        self.close_preview(cx);
        cx.notify();
    }

    /// 折叠 / 展开分组，同步触发箭头旋转与高度裁剪动画。
    fn toggle_group_with_anim(&mut self, id: &str, collapsed: bool, cx: &mut Context<Self>) {
        // 翻之前折着 = 翻完展开
        let next_open = collapsed;
        self.chevrons
            .flip_open(id, next_open, crate::fold_motion::OPEN_TURNS);
        self.folds.flip_open(format!("grp:{id}"), next_open);
        self.close_preview(cx);
        self.store
            .update(cx, |store, cx| store.toggle_group_collapse(id, cx));
    }

    /// 折叠 / 展开某个父项目下挂的 worktree 子项，同步触发箭头旋转与微容器高度抽屉动画。
    fn toggle_worktree_collapse(&mut self, parent_id: &str, cx: &mut Context<Self>) {
        let was_collapsed = self.collapsed_worktrees.contains(parent_id);
        if was_collapsed {
            self.collapsed_worktrees.remove(parent_id);
        } else {
            self.collapsed_worktrees.insert(parent_id.to_string());
        }
        let key = format!("wt:{parent_id}");
        self.chevrons
            .flip_open(key.clone(), was_collapsed, crate::fold_motion::OPEN_TURNS);
        self.folds.flip_open(key, was_collapsed);
        self.close_preview(cx);
        cx.notify();
    }

    /// 揭示的视图半边(入口是自由函数 [`reveal`]):先把挡路的折叠分组与折叠 worktree 簇展开,
    /// 再登记滚动。滚动不能当场算 —— 刚展开的行还没铺出来,要等下一次 `render`
    /// 拿新的展平表定行号(见 `render` 里兑现那一段)。
    /// 若当前行内筛选把目标项目滤掉了，自动清空并收起筛选条以确保目标可见。
    fn reveal(&mut self, project_id: String, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.expand_groups_to_project(&project_id, cx)
        });
        let ancestors =
            project_tree::worktree_ancestors(self.store.read(cx).projects(), &project_id);
        for ancestor in ancestors {
            self.collapsed_worktrees.remove(&ancestor);
        }
        if self.is_filtering(cx) {
            let visible = self.visible_ordered(self.store.read(cx), cx);
            if reveal_row_index(&visible, &project_id).is_none() {
                self.filter_mode = ProjectFilterMode::All;
                // 不拿 window 也能在 render 前清掉：直接放弃筛选输入框实例，下次打开再懒建
                self.filter_input = None;
                self._filter_sub = None;
                self.filter_open = false;
                self.filter_reveal.jump(false);
                self.pending_filter_focus = false;
            }
        }
        // 揭示需要立即扁平定行号滚动，清掉在飞折叠补间直接到位
        self.folds.clear();
        // 列表要滚了,悬停缩略图的锚点随之失效(与滚轮那条同一个理由)
        self.close_preview(cx);
        self.pending_reveal = Some(PendingReveal {
            project_id,
            recheck: true,
        });
        // 本视图套着 view 级缓存:不 notify 的话这一帧整块沿用旧画面,
        // prepaint 不跑,滚动请求也就没人兑现
        cx.notify();
    }

    // ─── 行内重命名(`ProjectList.tsx:415-444`) ───────────────

    /// 进入编辑态。右键菜单的「重命名」与 F2 走同一个入口。
    ///
    /// # 「默认全选」怎么绕过 `pub(super)`(Y 批记档的复查结论)
    ///
    /// `InputState::select_all` 确实是 `pub(super)`,组件库没有任何公开的
    /// setter 能改选区(`set_cursor_position` 只动光标)。但它是一个
    /// **action handler** —— `input::SelectAll` 是 `actions!` 导出的公开类型,
    /// `Input` 元素上挂着 `on_action(InputState::select_all)`。于是把动作
    /// **派发到输入框的焦点节点**就等价于用户按了 Ctrl+A。
    ///
    /// 时机是唯一的坑:`FocusHandle::dispatch_action` 查的是
    /// `rendered_frame` 的 dispatch tree,而输入框这一刻还没被画出来。
    /// 所以这里只登记,真正派发在 `render` 里挂 `window.on_next_frame`
    /// (那时 `rendered_frame` 正是刚画完、含输入框的那一帧)。
    fn start_rename(
        &mut self,
        id: String,
        is_group: bool,
        current: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |state, cx| {
            state.set_value(current.to_string(), window, cx);
            state.focus(window, cx);
        });
        self.pending_select_all = Some(input.read(cx).focus_handle(cx));
        // 回车 = 提交,失焦 = 提交(原版 onKeyDown Enter / onBlur 两条都提交)
        let sub = cx.subscribe(&input, |this: &mut Self, _input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.commit_rename(cx);
            }
        });
        self.editing = Some(Editing {
            id,
            is_group,
            input,
            _sub: sub,
        });
        cx.notify();
    }

    /// 提交:`trim` 之后非空才改名,**无论如何退出编辑态**(原版同一条)。
    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        let value = editing.input.read(cx).value().trim().to_string();
        if !value.is_empty() {
            self.store.update(cx, |store, cx| {
                if editing.is_group {
                    store.rename_group(&editing.id, &value, cx);
                } else {
                    store.rename_project(&editing.id, &value, cx);
                }
            });
        }
        cx.notify();
    }

    /// Esc 放弃。**先把编辑态连订阅一起丢掉**,随之而来的失焦才不会变成提交。
    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            cx.notify();
        }
    }

    /// 编辑中的输入框。原版是「只有一条 accent 下划线、无边框无背景」。
    fn rename_input(&self, input: &Entity<InputState>, size: f32) -> AnyElement {
        div()
            .flex_1()
            .border_b_1()
            .border_color(ui::accent())
            .text_size(ui::font_px(size))
            .text_color(ui::text_primary())
            // 点输入框不该顺带切项目 / 折叠分组(原版那句 stopPropagation)
            .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                cx.stop_propagation();
            })
            .child(Input::new(input).appearance(false))
            .into_any_element()
    }

    // ─── 键盘导航(`ProjectList.tsx:686-698` / `954-964`) ───────

    /// 行的焦点句柄(按需建、跨帧稳定)。
    fn row_focus(&mut self, id: &str, cx: &mut Context<Self>) -> FocusHandle {
        self.row_focus
            .entry(id.to_string())
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }

    /// 项目行按键。**F2 不在这里** —— 它是全局 action(`RenamePane`),
    /// 由 `main.rs` 的处理器按「有没有行拿着焦点」分流,见
    /// [`Self::rename_focused_row`];两处各绑一次就会出现两套 F2 语义。
    fn on_project_key(
        &mut self,
        event: &KeyDownEvent,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 编辑态里这一行的按键整体让给输入框(原版那句 `if (editing) return`)
        if self.editing.is_some() {
            return;
        }
        match event.keystroke.key.as_str() {
            "enter" | "space" => {
                cx.stop_propagation();
                self.store
                    .update(cx, |store, cx| store.set_active_project(id, cx));
            }
            "delete" => {
                cx.stop_propagation();
                let Some((name, path)) = self
                    .store
                    .read(cx)
                    .project(id)
                    .map(|p| (p.name.clone(), p.path.clone()))
                else {
                    return;
                };
                modal::open_confirm_remove_project(
                    self.store.clone(),
                    id.to_string(),
                    name,
                    path,
                    window,
                    cx,
                );
            }
            "/" if !event.keystroke.modifiers.modified() => {
                cx.stop_propagation();
                self.open_filter(window, cx);
            }
            "escape" if self.filter_open || self.is_filtering(cx) => {
                cx.stop_propagation();
                self.close_filter(true, window, cx);
            }
            "left" if !event.keystroke.modifiers.modified() => {
                let has_children =
                    !project_tree::worktree_descendants(self.store.read(cx).projects(), id)
                        .is_empty();
                if has_children && !self.collapsed_worktrees.contains(id) && !self.is_filtering(cx) {
                    cx.stop_propagation();
                    self.toggle_worktree_collapse(id, cx);
                } else {
                    cx.stop_propagation();
                    self.move_row_focus(id, RowNav::Parent, window, cx);
                }
            }
            "right" if !event.keystroke.modifiers.modified() => {
                let has_children =
                    !project_tree::worktree_descendants(self.store.read(cx).projects(), id)
                        .is_empty();
                if has_children && self.collapsed_worktrees.contains(id) && !self.is_filtering(cx) {
                    cx.stop_propagation();
                    self.toggle_worktree_collapse(id, cx);
                } else if has_children {
                    cx.stop_propagation();
                    self.move_row_focus(id, RowNav::FirstChild, window, cx);
                }
            }
            key => self.on_row_nav_key(event, key, id, window, cx),
        }
    }

    /// 分组行按键(Enter/Space 折叠 + ←/→ 折展或跳父/首子行 + / 开筛选 + F2 改名,**没有 Delete**——
    /// 删组是不可逆的结构变更,只留在右键菜单里并带确认)。
    ///
    /// `collapsed` 是这一行眼下的折叠态(翻过去之后箭头转到哪靠它)。
    fn on_group_key(
        &mut self,
        event: &KeyDownEvent,
        id: &str,
        collapsed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing.is_some() {
            return;
        }
        let plain = !event.keystroke.modifiers.modified();
        match event.keystroke.key.as_str() {
            "enter" | "space" => {
                cx.stop_propagation();
                if !self.is_filtering(cx) {
                    self.toggle_group_with_anim(id, collapsed, cx);
                }
            }
            "/" if plain => {
                cx.stop_propagation();
                self.open_filter(window, cx);
            }
            "escape" if self.filter_open || self.is_filtering(cx) => {
                cx.stop_propagation();
                self.close_filter(true, window, cx);
            }
            "left" if plain => {
                cx.stop_propagation();
                if !collapsed && !self.is_filtering(cx) {
                    self.toggle_group_with_anim(id, false, cx);
                } else {
                    self.move_row_focus(id, RowNav::Parent, window, cx);
                }
            }
            "right" if plain => {
                cx.stop_propagation();
                if collapsed && !self.is_filtering(cx) {
                    self.toggle_group_with_anim(id, true, cx);
                } else {
                    self.move_row_focus(id, RowNav::FirstChild, window, cx);
                }
            }
            key => self.on_row_nav_key(event, key, id, window, cx),
        }
    }

    /// 项目行 / 分组行共用的 ↑ / ↓ / Home / End(原版没有,只能 Tab 逐个走)。只认不带
    /// 修饰键的按法,其余键原样放行。
    fn on_row_nav_key(
        &mut self,
        event: &KeyDownEvent,
        key: &str,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        if let Some(nav) = focus_nav::row_nav_for_key(key) {
            cx.stop_propagation();
            self.move_row_focus(id, nav, window, cx);
        }
    }

    /// 按**可见行序**(分组行与项目行混排,折叠分组里的不算 —— 与 `render_rows` 铺开的是
    /// 同一张展平表)把焦点挪到目标行,并把它滚进可视区。到头停住、不循环。
    ///
    /// 行号就是滚动容器的子元素下标(理由见 [`reveal_row_index`])。悬停缩略图的锚点随滚动
    /// 失效,顺手收掉(与滚轮那条同一个理由)。
    fn move_row_focus(
        &mut self,
        from_id: &str,
        nav: RowNav,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ordered = self.visible_ordered(self.store.read(cx), cx);
        let ids: Vec<&str> = ordered.iter().map(ordered_item_id).collect();
        let depths: Vec<usize> = ordered
            .iter()
            .map(|item| match item {
                OrderedItem::Group { depth, .. } | OrderedItem::Project { depth, .. } => *depth,
            })
            .collect();
        let Some(current) = ids.iter().position(|id| *id == from_id) else {
            return;
        };
        let Some(target) = focus_nav::row_nav_target(&depths, current, nav) else {
            return;
        };
        let Some(focus) = self.row_focus.get(ids[target]).cloned() else {
            return;
        };
        self.folds.clear();
        self.close_preview(cx);
        window.focus(&focus, cx);
        self.rows_scroll.scroll_to_item(target);
        // 本视图套着 view 级缓存:不 notify 的话 prepaint 不跑,滚动请求没人兑现
        cx.notify();
    }

    /// 全局 F2 落到项目列表时的分流:**有行拿着焦点就改那一行的名字**,
    /// 返回 `true` 表示这次 F2 由本视图消费掉了(否则由 `main.rs` 去改终端 tab)。
    ///
    /// 这是「同源判定」的落点:F2 只有一条 KeyBinding(`hotkeys.rs` 的
    /// `renamePane`)、只有一个处理器,分流靠焦点而不是靠第二条绑定。
    pub fn rename_focused_row(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // 同一时刻最多只有一个句柄是聚焦的,HashMap 的遍历序不影响结果
        let Some(id) = self
            .row_focus
            .iter()
            .find(|(_, f)| f.is_focused(window))
            .map(|(id, _)| id.clone())
        else {
            return false;
        };
        // 行 id 与分组 id 不同名空间,查一次就知道是哪种
        let store = self.store.read(cx);
        if let Some(project) = store.project(&id) {
            let name = project.name.clone();
            self.start_rename(id, false, &name, window, cx);
            return true;
        }
        let name = project_tree::find_group_in_tree(
            store.config().project_tree.as_deref().unwrap_or(&[]),
            &id,
        )
        .map(|g| g.name.clone());
        match name {
            Some(name) => {
                self.start_rename(id, true, &name, window, cx);
                true
            }
            None => false,
        }
    }

    // ─── 悬停缩略图(`ProjectList.tsx:446-496`) ────────────────

    /// 换卡时等新的一行量出矩形:每隔多久看一次、最多看几次(约四帧)。
    const WARM_RECT_POLL: std::time::Duration = std::time::Duration::from_millis(8);
    const WARM_RECT_POLLS: usize = 8;

    /// 收起浮层(播收起动画,播完才卸)并取消在飞的计时(原版 `closePreview`)。
    fn close_preview(&mut self, cx: &mut Context<Self>) {
        self._preview_task = None;
        if self.preview.close(cx) {
            cx.notify();
        }
    }

    /// 进入某一行:排一次 250ms 的计时。
    ///
    /// **AI 判定放在到点时而非进入时**(原版专门记了这条):这 250ms 里 AI 完全
    /// 可能刚起来,读当下的 store 才拿得到最新一份。矩形同理到点再取。
    ///
    /// 卡还在屏幕上(开着,或刚离开上一行、收起动画还没播完)时**不等**:当场按同样
    /// 三道闸换到这一行,卡身从上一行旁边滑过来,不重播开合(见
    /// [`pane_preview::HoverCard`]);这一行没有 AI 会话就让卡照常收起。
    fn schedule_preview(&mut self, project_id: String, cx: &mut Context<Self>) {
        self._preview_task = None;
        if let Some(anchor) = self.preview.anchor_of(&project_id) {
            // 收起途中又悬停回卡正显示的那一行:当场从当前进度往回长。锚点沿用卡上记着的
            // 那一个 —— `hover_rect` 只有一格,中间悬停过的那一行(哪怕没弹卡)已经把它
            // 量成了自己的矩形,这一行要等下一帧画过才量得回来
            if self.fire_preview_at(&project_id, Some(anchor), cx) {
                self._preview_task = Some(cx.spawn(async move |this, cx| {
                    Self::keep_preview_alive(this, cx).await;
                }));
            }
            return;
        }
        if self.preview.is_warm() {
            // 矩形的量尺只挂在被悬停的那一行上(见 render_project),新的这一行要等下一帧
            // 画过才量得到:手上的 `hover_rect` 还是上一行的。等它变了再换卡,最多等
            // 几帧 —— 这是交互时序,不是动画
            let stale = self.hover_rect;
            self._preview_task = Some(cx.spawn(async move |this, cx| {
                for _ in 0..Self::WARM_RECT_POLLS {
                    cx.background_executor().timer(Self::WARM_RECT_POLL).await;
                    let measured = this
                        .update(cx, |this: &mut ProjectList, _| this.hover_rect != stale)
                        .unwrap_or(true);
                    if measured {
                        break;
                    }
                }
                let opened = this
                    .update(cx, |this: &mut ProjectList, cx| {
                        let opened = this.fire_preview(&project_id, cx);
                        if !opened && this.preview.close(cx) {
                            // 这一行没有 AI 会话:卡照常收起
                            cx.notify();
                        }
                        opened
                    })
                    .unwrap_or(false);
                if opened {
                    Self::keep_preview_alive(this, cx).await;
                }
            }));
            return;
        }
        self._preview_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(pane_preview::HOVER_DELAY_MS))
                .await;
            if this
                .update(cx, |this: &mut ProjectList, cx| this.fire_preview(&project_id, cx))
                .unwrap_or(false)
            {
                Self::keep_preview_alive(this, cx).await;
            }
        }));
    }

    /// 开着期间按原版的 500ms 节拍重画 —— 缩略图是活的。MiniTerminalElement 自己也按
    /// 这个节拍取数,更密的重画只命中缓存。卡下了屏幕就停。
    async fn keep_preview_alive(this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp) {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(mt_ui::MINI_REFRESH_MS))
                .await;
            let alive = this
                .update(cx, |this: &mut ProjectList, cx| {
                    let warm = this.preview.is_warm();
                    if warm {
                        cx.notify();
                    }
                    warm
                })
                .unwrap_or(false);
            if !alive {
                return;
            }
        }
    }

    /// 计时到点:三道闸(还悬着同一行 / 量到过矩形 / 项目里真有 AI 会话)。
    fn fire_preview(&mut self, project_id: &str, cx: &mut Context<Self>) -> bool {
        self.fire_preview_at(project_id, self.hover_rect, cx)
    }

    /// 同 [`Self::fire_preview`],锚点矩形由调用方给(`None` = 还没量到,不开)。
    fn fire_preview_at(
        &mut self,
        project_id: &str,
        anchor: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.hover_on_action || self.hovered.as_deref() != Some(project_id) {
            return false;
        }
        let Some(anchor) = anchor else {
            return false;
        };
        let store = self.store.read(cx);
        let auto_resume = store.config().ai_auto_resume.unwrap_or(true);
        let layout = store.project_state(project_id).and_then(|s| s.active_layout());
        if !pane_preview::has_ai_pane(layout, auto_resume) {
            return false;
        }
        self.preview.open(
            project_id.to_string(),
            anchor,
            pane_preview::project_anchor,
            cx,
        );
        cx.notify();
        true
    }

    /// 组装浮层。渲染处**每帧重判开闸**(原版的双闸模式):AI 退出后不光不画,
    /// 状态本身也收掉(直接卸,不播动画)—— 留着的话同一次悬停里 AI 再起来会拿
    /// **旧锚点**复活。
    ///
    /// 开合走 [`crate::popover_reveal::reveal_frame`]:卡在行的右边弹出,框体从紧贴
    /// 项目行的左上角长出来(原点钳在卡的左缘),内容按最终尺寸排、只裁剪 ——
    /// 不淡入,拼图里的缩略图也不逐帧反解字号。
    fn render_preview(&mut self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let frame = self.preview.frame(window)?;
        let project_id = frame.key.clone();
        let (alive, name, path) = {
            let store = self.store.read(cx);
            let auto_resume = store.config().ai_auto_resume.unwrap_or(true);
            match store.project(&project_id) {
                Some(project) => {
                    let layout = store
                        .project_state(&project_id)
                        .and_then(|s| s.active_layout());
                    (
                        pane_preview::has_ai_pane(layout, auto_resume),
                        project.name.clone(),
                        project.path.clone(),
                    )
                }
                None => (false, String::new(), String::new()),
            }
        };
        if !alive {
            self.preview.close_now();
            return None;
        }
        let store = self.store.read(cx);
        let auto_resume = store.config().ai_auto_resume.unwrap_or(true);
        let layout = store
            .project_state(&project_id)
            .and_then(|s| s.active_layout());
        let mini: Option<MiniLayout> = layout
            .and_then(|l| pane_preview::snapshot_layout(l, &project_id, store, auto_resume, cx));
        let style = pane_preview::preview_style(store);
        let at = pane_preview::project_anchor(frame.anchor) + frame.offset;
        let mut state = frame.reveal;
        // 原点取行的右缘(卡紧挨着它弹出):钳到卡的左缘,框体从左上角往右下长
        state.origin_x = Some(frame.anchor.right());
        // 卡是纯展示(不拦鼠标):收起途中也别让 reveal_frame 盖拦截层
        state.open = true;
        let card = crate::popover_reveal::reveal_frame(
            pane_preview::project_preview_card(&name, &path, mini.as_ref(), &style),
            state,
            Some(pane_preview::card_chrome()),
        );
        Some(
            deferred(
                anchored()
                    .position(at)
                    // 原版那两句 `Math.min/max` 的贴边收拢在 gpui 里是白拿的
                    .snap_to_window_with_margin(px(8.0))
                    .child(card),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    // ─── worktree 徽章与失效清理(`ProjectList.tsx:186-254`) ───

    /// 批量探测哪些项目路径是 linked worktree。`force` = 路径没变也重探
    /// (窗口重获焦点那条:分支切换发生在窗外,路径清单不会变)。
    ///
    /// `get_worktree_branches` 逐个 `Repository::open`,**阻塞**,必须丢后台。
    fn probe_worktrees(&mut self, force: bool, cx: &mut Context<Self>) {
        // 这个方法挂在 store 事件上、项目表 / 技术栈缓存 / 聚焦变化都会走一遍,
        // 所以先只拼一条比较用的键,确定要探了才真去收集路径
        let mut key = String::new();
        for p in self
            .store
            .read(cx)
            .projects()
            .iter()
            .filter(|p| p.ssh_connection_id.is_none())
        {
            key.push_str(&p.path);
            key.push('\n');
        }
        if !force && key == self.probe_key {
            return;
        }
        let paths: Vec<String> = self
            .store
            .read(cx)
            .projects()
            .iter()
            .filter(|p| p.ssh_connection_id.is_none())
            .map(|p| p.path.clone())
            .collect();
        self.probe_key = key;
        if paths.is_empty() {
            self.worktree_branches.clear();
            return;
        }
        cx.spawn(async move |this, cx| {
            let probe: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
            let branches = cx
                .background_executor()
                .spawn(async move { mt_project::git::get_worktree_branches(&probe) })
                .await;
            let _ = this.update(cx, |this: &mut ProjectList, cx| {
                this.worktree_branches = paths
                    .into_iter()
                    .zip(branches)
                    .filter_map(|(path, branch)| branch.map(|b| (path, b)))
                    .collect();
                cx.notify();
            });
        })
        .detach();
    }

    /// 失效 worktree 子项目自动清理:目录已消失(**且父项目目录仍在**,排除盘符级
    /// 整树消失的误判)的子项目连终端资源一起移除。
    ///
    /// 外部 / AI agent 在终端里 `git worktree remove` 之后没有任何事件通知,
    /// 只能在挂载与窗口重获焦点时探一次。
    fn reconcile_worktrees(&mut self, cx: &mut Context<Self>) {
        let projects: Vec<ProjectConfig> = self.store.read(cx).projects().to_vec();
        let probe = collect_worktree_probe_paths(&projects);
        if probe.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let paths: Vec<PathBuf> = probe.iter().map(PathBuf::from).collect();
            let alive = cx
                .background_executor()
                .spawn(async move { mt_project::fs::filter_directories(paths) })
                .await;
            let alive: HashSet<String> = alive
                .into_iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            let _ = this.update(cx, |this: &mut ProjectList, cx| {
                // 探测回来时项目表可能已经变了 —— 按**当下**的表重算一遍
                let projects: Vec<ProjectConfig> = this.store.read(cx).projects().to_vec();
                let stale = find_stale_worktree_projects(&projects, &alive);
                if stale.is_empty() {
                    return;
                }
                this.store.update(cx, |store, cx| {
                    for id in stale {
                        store.remove_project(&id, cx);
                    }
                });
            });
        })
        .detach();
    }

    /// 安排一次去抖后的 worktree 自动同步（`.git/worktrees` 监听唤醒或慢 checkout 重试用）。
    /// 替换句柄自动取消前一次计时，连发事件合并为一趟；到期后同时触发新增发现、失效清理与分支徽章重探。
    fn schedule_worktree_sync(&mut self, delay: Duration, retry_count: u8, cx: &mut Context<Self>) {
        self._worktree_sync_debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this: &mut ProjectList, cx| {
                this._worktree_sync_debounce = None;
                this.sync_worktrees(true, retry_count, cx);
                this.reconcile_worktrees(cx);
                this.probe_worktrees(true, cx);
            });
        }));
    }

    /// 自动识别本地主仓库下的 linked worktree 并添加为子项目，同时维护 `.git` / `.git/worktrees`
    /// 的非递归监听集合（只盯本地非子项目，不扫远程与 UNC，也不把兄弟 worktree 误挂到 worktree 之下）。
    fn sync_worktrees(&mut self, force: bool, retry_count: u8, cx: &mut Context<Self>) {
        let candidates: Vec<(String, String)> = self
            .store
            .read(cx)
            .projects()
            .iter()
            .filter(|p| {
                p.ssh_connection_id.is_none()
                    && p.parent_project_id.is_none()
                    && !is_unc_path(&p.path)
            })
            .map(|p| (p.id.clone(), p.path.clone()))
            .collect();
        let mut key = String::new();
        for (id, path) in &candidates {
            key.push_str(id);
            key.push('\t');
            key.push_str(path);
            key.push('\n');
        }
        if !force && key == self.worktree_sync_key {
            return;
        }
        self.worktree_sync_key = key;
        self.worktree_sync_gen = self.worktree_sync_gen.wrapping_add(1);
        let sync_gen = self.worktree_sync_gen;
        if candidates.is_empty() {
            for dir in self.watched_worktree_dirs.drain() {
                self.worktree_watcher.unwatch(&dir);
            }
            return;
        }
        cx.spawn(async move |this, cx| {
            let paths: Vec<PathBuf> = candidates.iter().map(|(_, p)| PathBuf::from(p)).collect();
            let (all_reachable, results) = cx
                .background_executor()
                .spawn(async move {
                    let all_reachable = paths.iter().all(|p| p.is_dir());
                    let results = mt_project::git::discover_local_repo_worktrees(&paths);
                    (all_reachable, results)
                })
                .await;
            let scanned: Vec<(String, mt_project::git::LocalRepoWorktrees)> = candidates
                .into_iter()
                .zip(results)
                .filter_map(|((id, _), info)| info.map(|info| (id, info)))
                .collect();
            let _ = this.update(cx, |this: &mut ProjectList, cx| {
                if this.worktree_sync_gen != sync_gen {
                    return;
                }
                // 增量同步 `.git` 与 `.git/worktrees` 监听目录：只有主仓库挂监听，非 git 项目零开销
                let mut desired: HashMap<PathBuf, String> = HashMap::new();
                let mut any_pending_invalid = false;
                let mut live_local_keys: HashSet<String> = HashSet::new();
                for (_, info) in &scanned {
                    if info.has_pending_invalid {
                        any_pending_invalid = true;
                    }
                    let root = info.git_dir.to_string_lossy().to_string();
                    desired
                        .entry(info.git_dir.clone())
                        .or_insert_with(|| root.clone());
                    if info.worktrees_dir_exists {
                        desired
                            .entry(info.git_dir.join("worktrees"))
                            .or_insert_with(|| root);
                    }
                    for wt_path in &info.linked_paths {
                        live_local_keys.insert(crate::store::worktree_dismiss_key(None, wt_path));
                    }
                }
                let to_remove: Vec<PathBuf> = this
                    .watched_worktree_dirs
                    .iter()
                    .filter(|dir| !desired.contains_key(*dir))
                    .cloned()
                    .collect();
                for dir in to_remove {
                    this.watched_worktree_dirs.remove(&dir);
                    this.worktree_watcher.unwatch(&dir);
                }
                for (dir, root) in desired {
                    if this.watched_worktree_dirs.insert(dir.clone()) {
                        this.worktree_watcher.watch(&dir, &root);
                    }
                }

                this.store.update(cx, |store, cx| {
                    if all_reachable {
                        store.prune_local_dismissed_worktrees(&live_local_keys, cx);
                    }
                    let to_add = plan_auto_added_worktrees(
                        store.projects(),
                        &store.config().dismissed_worktrees,
                        &scanned,
                    );
                    for (parent_id, wt_path) in to_add {
                        store.add_project_at(Path::new(&wt_path), Some(&parent_id), cx);
                    }
                });

                // 遇到刚起跑还在 `initializing` 中的大仓库 checkout，补最多 2 趟短重试
                if any_pending_invalid && retry_count < 2 {
                    this.schedule_worktree_sync(Duration::from_millis(1500), retry_count + 1, cx);
                }
            });
        })
        .detach();
    }

    /// 取消在飞的「折叠分组悬停自动展开」计时。
    fn cancel_spring_open(&mut self) {
        self.spring_open_group = None;
        self._spring_open_task = None;
    }

    /// 拖放悬停在折叠分组 `Inside` 区域 550ms 后自动展开（spring-loaded group），方便精准放入指定顺序。
    fn schedule_spring_open(&mut self, group_id: String, cx: &mut Context<Self>) {
        if self.spring_open_group.as_deref() == Some(group_id.as_str()) {
            return;
        }
        self.spring_open_group = Some(group_id.clone());
        self._spring_open_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SPRING_OPEN_DELAY_MS))
                .await;
            let _ = this.update(cx, |this: &mut ProjectList, cx| {
                if !cx.has_active_drag() {
                    this.cancel_spring_open();
                    return;
                }
                let still_inside = this.drop_indicator.as_ref().is_some_and(|ind| {
                    ind.id == group_id && ind.position == DropPosition::Inside && !ind.forbidden
                });
                this.cancel_spring_open();
                if still_inside {
                    let changed = this
                        .store
                        .update(cx, |store, cx| store.set_group_collapsed(&group_id, false, cx));
                    if changed {
                        this.chevrons
                            .flip_open(&group_id, true, crate::fold_motion::OPEN_TURNS);
                        this.folds.flip_open(format!("grp:{group_id}"), true);
                        cx.notify();
                    }
                }
            });
        }));
    }

    /// 拖放到空分组占位提示区：强制以 `DropPosition::Inside` 命中该分组。
    fn on_empty_group_drag_move(
        &mut self,
        event: &DragMoveEvent<DragProjectItem>,
        group_id: &str,
        cx: &mut Context<Self>,
    ) {
        let dragged = event.drag(cx).clone();
        if !event.bounds.contains(&event.event.position) {
            if self
                .drop_indicator
                .as_ref()
                .is_some_and(|d| d.id == group_id && d.position == DropPosition::Inside)
            {
                self.drop_indicator = None;
                cx.notify();
            }
            return;
        }
        if dragged.id == group_id {
            return;
        }
        let forbidden = {
            let store = self.store.read(cx);
            let empty: Vec<ProjectTreeItem> = Vec::new();
            let tree = store.config().project_tree.as_ref().unwrap_or(&empty);
            let dragged_item = if dragged.is_group {
                project_tree::find_group_in_tree(tree, &dragged.id)
                    .map(|g| ProjectTreeItem::Group(g.clone()))
            } else {
                Some(ProjectTreeItem::ProjectId(dragged.id.clone()))
            };
            dragged_item.is_some_and(|item| !project_tree::can_drop(tree, group_id, &item))
        };
        let next = DropIndicator {
            id: group_id.to_string(),
            position: DropPosition::Inside,
            forbidden,
        };
        if self.drop_indicator.as_ref() != Some(&next) {
            self.drop_indicator = Some(next);
            self.drop_to_root_end = false;
            cx.notify();
        }
    }

    /// 拖放到列表底部弹性空白区：悬停高亮，松手直接移至顶层末尾。
    fn on_bottom_zone_drag_move(
        &mut self,
        event: &DragMoveEvent<DragProjectItem>,
        cx: &mut Context<Self>,
    ) {
        let hit = event.bounds.contains(&event.event.position);
        if hit {
            self.cancel_spring_open();
            let changed = !self.drop_to_root_end || self.drop_indicator.is_some();
            self.drop_to_root_end = true;
            self.drop_indicator = None;
            if changed {
                cx.notify();
            }
        } else if self.drop_to_root_end {
            self.drop_to_root_end = false;
            cx.notify();
        }
    }

    /// 底部空白区落地：将项目 / 分组 / worktree 子项目移到顶层末尾。
    fn on_bottom_zone_drop(&mut self, dragged: &DragProjectItem, cx: &mut Context<Self>) {
        self.drop_to_root_end = false;
        self.drop_indicator = None;
        self.dragging = None;
        self.cancel_spring_open();
        cx.notify();
        self.store.update(cx, |store, cx| {
            store.move_item(&dragged.id, None, None, cx);
        });
    }

    /// `on_drag_move` 的落点判定。`allow_inside` 只有分组行为真。
    ///
    /// 见 [`crate::dnd`] 模块注释第 2 条:这个回调会打给**每一个**注册者,
    /// 命中判定(`hit_ratio` 返回 `None`)必须自己做,否则整列会一起亮。
    fn on_row_drag_move(
        &mut self,
        event: &DragMoveEvent<DragProjectItem>,
        row_id: &str,
        allow_inside: bool,
        cx: &mut Context<Self>,
    ) {
        let dragged = event.drag(cx).clone();
        let Some(ratio) = dnd::hit_ratio(event.bounds, event.event.position) else {
            // 鼠标不在这一行上:只收自己那一份指示,别人的留给别人清
            if self.drop_indicator.as_ref().is_some_and(|d| d.id == row_id) {
                self.drop_indicator = None;
                if self.spring_open_group.as_deref() == Some(row_id) {
                    self.cancel_spring_open();
                }
                cx.notify();
            }
            return;
        };
        if dragged.id == row_id {
            // 拖到自己身上不给任何指示(原版 `handleMouseMoveOver` 开头那道 return)
            if self.drop_indicator.as_ref().is_some_and(|d| d.id == row_id) {
                self.drop_indicator = None;
                self.cancel_spring_open();
                cx.notify();
            }
            return;
        }

        let position = dnd::drop_position(ratio, allow_inside);
        let (forbidden, target_collapsed_group) = {
            let store = self.store.read(cx);
            let empty: Vec<ProjectTreeItem> = Vec::new();
            let tree = store.config().project_tree.as_ref().unwrap_or(&empty);
            // 被拖的那个节点本体:分组要连子树一起量深度,项目恒为 0 层
            let dragged_item = if dragged.is_group {
                project_tree::find_group_in_tree(tree, &dragged.id)
                    .map(|g| ProjectTreeItem::Group(g.clone()))
            } else {
                Some(ProjectTreeItem::ProjectId(dragged.id.clone()))
            };
            let forbidden = match dragged_item {
                None => false,
                Some(item) => match position {
                    DropPosition::Inside => !project_tree::can_drop(tree, row_id, &item),
                    // before/after 只有拖「组」才可能超深(项目恒 0 层)
                    _ if dragged.is_group => !project_tree::can_drop_at(tree, row_id, &item),
                    _ => false,
                },
            };
            let collapsed_group = allow_inside
                && position == DropPosition::Inside
                && !forbidden
                && project_tree::find_group_in_tree(tree, row_id).is_some_and(|g| g.collapsed);
            (forbidden, collapsed_group)
        };

        if target_collapsed_group {
            self.schedule_spring_open(row_id.to_string(), cx);
        } else if self.spring_open_group.as_deref() == Some(row_id) {
            self.cancel_spring_open();
        }

        let next = DropIndicator {
            id: row_id.to_string(),
            position,
            forbidden,
        };
        if self.drop_indicator.as_ref() != Some(&next) || self.drop_to_root_end {
            self.drop_to_root_end = false;
            self.drop_indicator = Some(next);
            cx.notify();
        }
    }

    /// `on_drop` 落地。位置来自上一次 `on_drag_move` 存下的 indicator ——
    /// gpui 的 `on_drop` 不带坐标,这是硬约束。
    fn on_row_drop(&mut self, dragged: &DragProjectItem, row_id: &str, cx: &mut Context<Self>) {
        let indicator = self.drop_indicator.take();
        self.drop_to_root_end = false;
        self.dragging = None;
        self.cancel_spring_open();
        cx.notify();
        let Some(indicator) = indicator else {
            return;
        };
        if indicator.forbidden || indicator.id != row_id || dragged.id == row_id {
            return;
        }

        if indicator.position == DropPosition::Inside {
            self.store.update(cx, |store, cx| {
                store.move_item(&dragged.id, Some(row_id), None, cx);
            });
            return;
        }

        // before/after:落到目标的**同级**,下标按目标位置算,同父级还要补偿位移
        let plan = {
            let store = self.store.read(cx);
            let empty: Vec<ProjectTreeItem> = Vec::new();
            let tree = store.config().project_tree.as_ref().unwrap_or(&empty);
            let parent = project_tree::find_parent_group_id(tree, row_id);
            let target_idx = project_tree::index_in_parent(tree, parent.as_deref(), row_id);
            let dragged_idx =
                project_tree::index_in_parent(tree, parent.as_deref(), &dragged.id);
            target_idx.map(|target_idx| {
                (
                    parent,
                    dnd::insert_index(
                        target_idx,
                        dragged_idx,
                        indicator.position == DropPosition::After,
                    ),
                )
            })
        };
        let Some((parent, index)) = plan else {
            return;
        };
        self.store.update(cx, |store, cx| {
            store.move_item(&dragged.id, parent.as_deref(), Some(index), cx);
        });
    }

    /// 外部文件悬停:命中就记下这批路径,并**只在路径变了的时候**丢一次后台判定。
    fn on_external_move(&mut self, event: &DragMoveEvent<ExternalPaths>, cx: &mut Context<Self>) {
        if !event.bounds.contains(&event.event.position) {
            if self.external.is_some() {
                self.external = None;
                cx.notify();
            }
            return;
        }
        let paths: Vec<PathBuf> = event.drag(cx).paths().to_vec();
        if self.external.as_ref().is_some_and(|e| e.paths == paths) {
            return;
        }
        self.external = Some(ExternalDrag {
            paths: paths.clone(),
            kind: None,
        });
        cx.notify();

        // `Path::is_dir()` 是同步 stat:网络盘上一次就能卡住主线程,必须丢后台
        cx.spawn(async move |this, cx| {
            let probe = paths.clone();
            let dirs = cx
                .background_executor()
                .spawn(async move { mt_project::fs::filter_directories(probe) })
                .await;
            let _ = this.update(cx, |this: &mut ProjectList, cx| {
                // 判定回来时用户可能已经拖到别处了 —— 只认还对得上号的那一批
                if !this.external.as_ref().is_some_and(|e| e.paths == paths) {
                    return;
                }
                let existing: Vec<String> = this
                    .store
                    .read(cx)
                    .projects()
                    .iter()
                    .map(|p| p.path.clone())
                    .collect();
                let kind = dnd::classify_external(&dirs, &existing);
                if let Some(state) = this.external.as_mut() {
                    state.kind = Some(kind);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 外部文件落地。逐个加;**新增过就打开第一个新增的**(切过去 + 开首个终端,
    /// 与「添加项目」弹窗同口径),一个没新增但撞上已有项目 → 只切过去(并揭示)。
    ///
    /// 原版 `ProjectList.tsx:295-319` 是「新增过任何一个就只落盘不切换」,
    /// 「添加完还得自己找过去」这一步被用户点名要掉,这里跟着弹窗一起改。
    fn on_external_drop(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.external = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let dirs = cx
                .background_executor()
                .spawn(async move { mt_project::fs::filter_directories(paths) })
                .await;
            if dirs.is_empty() {
                return;
            }
            let _ = this.update_in(cx, |this: &mut ProjectList, window, cx| {
                this.store.update(cx, |store, cx| {
                    let mut first_added: Option<String> = None;
                    let mut existing_id: Option<String> = None;
                    for dir in &dirs {
                        let path_str = dir.to_string_lossy().to_string();
                        if let Some(existing) = store.find_project_by_path(&path_str) {
                            existing_id = Some(existing.id.clone());
                            continue;
                        }
                        let id = store.add_project_at(dir, None, cx);
                        first_added.get_or_insert(id);
                    }
                    match (first_added, existing_id) {
                        // 新增的那个由 `open_added_project` 自己揭示
                        (Some(id), _) => store.open_added_project(&id, window, cx),
                        // 撞上的既有项目可能藏在折叠组里 / 可视区外 —— 拖进来的是
                        // 文件夹,不是点了那一行,同样要揭示
                        (None, Some(id)) => {
                            store.set_active_project(&id, cx);
                            reveal(&id, cx);
                        }
                        (None, None) => {}
                    }
                });
            });
        })
        .detach();
    }

    /// 落点指示线：左侧 6px 实心圆点 + 2px accent 横线，左缘按目标 `group_depth` 对齐，
    /// 一眼看清松手后落进组内还是组外。**非法落点不画线**。
    fn drop_line(
        &self,
        id: &str,
        position: DropPosition,
        group_depth: usize,
        active: bool,
    ) -> Option<AnyElement> {
        let indicator = self.drop_indicator.as_ref()?;
        if !active || indicator.id != id || indicator.position != position || indicator.forbidden {
            return None;
        }
        let left_x = row_left_margin(group_depth);
        Some(
            div()
                .absolute()
                .left(px(left_x))
                .right(px(ROW_MARGIN_X))
                .h(px(6.0))
                .flex()
                .items_center()
                .map(|el| match position {
                    DropPosition::Before => el.top(px(-3.0)),
                    _ => el.bottom(px(-3.0)),
                })
                .child(
                    div()
                        .w(px(6.0))
                        .h(px(6.0))
                        .flex_none()
                        .rounded_full()
                        .bg(ui::accent()),
                )
                .child(
                    div()
                        .flex_1()
                        .h(px(2.0))
                        .rounded_r_full()
                        .bg(ui::accent()),
                )
                .into_any_element(),
        )
    }
}

impl Render for ProjectList {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 行内重命名的「默认全选」:输入框在**上一帧**才被画进 dispatch tree,
        // 所以派发挂在 `on_next_frame`(那时 rendered_frame 正是这一帧)。
        // 见 `start_rename` 的注释。
        if let Some(focus) = self.pending_select_all.take() {
            window.on_next_frame(move |window, cx| {
                focus.dispatch_action(&SelectAll, window, cx);
            });
        }
        // 打开行内筛选条后在下一帧把焦点交到输入框。
        if self.pending_filter_focus
            && let Some(input) = self.filter_input.clone()
        {
            self.pending_filter_focus = false;
            window.on_next_frame(move |window, cx| {
                input.update(cx, |state, cx| state.focus(window, cx));
            });
        }
        // 拖拽中断(松手在窗外 / 被别人吃掉)后 gpui 会清 active_drag 并重画:
        // 借这一帧把残留的 view state 一并清掉(**不 notify** —— 正在渲染,
        // 再触发一次重画就是死循环)。高亮另外还与 `drag_active` 与门,
        // 保证即使这次没轮到重画也不会画出过期的指示。
        let drag_active = cx.has_active_drag();
        if !drag_active {
            self.dragging = None;
            self.drop_indicator = None;
            self.drop_to_root_end = false;
            self.cancel_spring_open();
            // 这一份不清的话,下次拖同一批路径会命中缓存、沿用过期的三态结论
            self.external = None;
        }
        let dragging = self.dragging.clone();
        let external = self.external.as_ref().map(|e| e.kind);
        let filtering = self.is_filtering(cx);
        let query_lower = self.filter_query_lower(cx);

        let (ordered, total_projects, has_any_tree_item) = {
            let store = self.store.read(cx);
            // 父项目名下的 worktree 全部删空后清掉旧折叠记录，避免日后再建 worktree 时莫名保持折叠
            if !self.collapsed_worktrees.is_empty() {
                let projects = store.projects();
                self.collapsed_worktrees
                    .retain(|pid| !project_tree::worktree_descendants(projects, pid).is_empty());
            }
            // 同步分组与 worktree 簇的在飞折叠动画：播完或被外部改了状态即摘除恢复扁平
            let tree = store.config().project_tree.as_deref().unwrap_or(&[]);
            let projects = store.projects();
            let collapsed_wt = &self.collapsed_worktrees;
            self.folds.sync(|key| {
                if let Some(gid) = key.strip_prefix("grp:") {
                    project_tree::find_group_in_tree(tree, gid).map(|g| !g.collapsed)
                } else if let Some(pid) = key.strip_prefix("wt:") {
                    (!project_tree::worktree_descendants(projects, pid).is_empty())
                        .then(|| !collapsed_wt.contains(pid))
                } else {
                    None
                }
            });
            let total = projects.len();
            let has_tree_item = total > 0 || project_tree::has_any_group(tree);
            (self.visible_ordered(store, cx), total, has_tree_item)
        };
        if total_projects == 0 && (self.filter_open || self.filter_mode != ProjectFilterMode::All) {
            self.filter_open = false;
            self.filter_mode = ProjectFilterMode::All;
            self.filter_reveal.jump(false);
            self.pending_filter_focus = false;
        }
        let matched_projects = ordered
            .iter()
            .filter(|item| matches!(item, OrderedItem::Project { .. }))
            .count();

        // 揭示在这里兑现:此刻的展平表已含 `reveal` 刚展开的分组,行号与这一帧铺出来
        // 的子元素一一对应。目标已不是活动项目(中栏收着时登记、展开前又被别处切走)
        // 就作罢 —— 揭示的语义是「让你看见当前项目」。
        if let Some(reveal) = self.pending_reveal.take() {
            let still_active = self.store.read(cx).active_project_id.as_deref()
                == Some(reveal.project_id.as_str());
            if still_active && let Some(ix) = reveal_row_index(&ordered, &reveal.project_id) {
                self.rows_scroll.scroll_to_item(ix);
                if reveal.recheck {
                    self.pending_reveal = Some(PendingReveal {
                        recheck: false,
                        ..reveal
                    });
                    window.request_animation_frame();
                }
            }
        }
        self.sync_row_focus(&ordered, cx);
        self.sync_done_tags(&ordered, window, cx);
        self.chevrons.drive(window);
        let preview = self.render_preview(window, cx);
        let header = self.render_list_header(total_projects, matched_projects, filtering, cx);
        let filter_bar = self
            .filter_reveal
            .is_mounted()
            .then(|| self.filter_reveal.render(self.render_filter_bar(cx), window));

        let body: AnyElement = if !has_any_tree_item {
            self.render_empty_state(cx).into_any_element()
        } else if filtering && ordered.is_empty() {
            self.render_filter_empty_state(cx).into_any_element()
        } else {
            self.render_rows(
                ordered,
                dragging.as_deref(),
                drag_active,
                filtering,
                &query_lower,
                window,
                cx,
            )
            .into_any_element()
        };

        div()
            .id("project-list")
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(ui::bg_surface())
            // 外部资源管理器拖文件夹进来 —— gpui 把平台的 FileDrop 翻译成
            // `ExternalPaths` 内部 drag,所以与内部拖拽是同一套 API
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<ExternalPaths>, _window, cx| {
                    this.on_external_move(event, cx);
                },
            ))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.on_external_drop(paths.paths().to_vec(), window, cx);
            }))
            .child(header)
            .children(filter_bar)
            .child(body)
            // 悬停缩略图。`deferred(priority 1)` 画在所有常规内容之上,
            // 卡本身不带 `.id()` → 无 hitbox → 等价原版的 pointer-events-none
            .children(preview)
            // 三态提示框:盖住整栏,`pointer-events` 不用管 —— gpui 的 drop 分发
            // 按 hitbox 命中走,这层没有 `.id()` 也就没有 hitbox
            .children(external.map(external_drop_hint))
    }
}

impl ProjectList {
    /// 行尾悬浮操作按钮的单颗小壳（20×20，供项目行 `···` 与分组行 `+`/`···` 共用）。
    fn floating_icon_button(
        &self,
        id: impl Into<SharedString>,
        icon: &'static [mt_ui::icons::vector::Shape],
        tip: &'static str,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id.into())
            .w(px(20.0))
            .h(px(20.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.0))
            .cursor_pointer()
            .text_color(ui::text_muted())
            .hover(|el| el.bg(ui::border_strong()).text_color(ui::text_primary()))
            .tip(tip)
            .on_hover(cx.listener(|this, entered: &bool, _window, cx| {
                this.hover_on_action = *entered;
                if *entered {
                    this.close_preview(cx);
                }
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .child(VectorIcon::new(icon, px(12.0)).ink(ui::text_secondary()))
    }

    /// 零抖动悬浮动作胶囊外壳（绝对定位贴行右缘，不推挤主内容）。
    fn floating_action_bar(&self) -> gpui::Div {
        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .p(px(1.5))
            .rounded(px(5.0))
            .bg(ui::bg_elevated())
            .border_1()
            .border_color(ui::border_default())
            .shadow_sm()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_group(
        &self,
        row: GroupRow,
        is_first_row: bool,
        dragging: Option<&str>,
        drag_active: bool,
        filtering: bool,
        query_lower: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let GroupRow {
            id,
            name,
            collapsed,
            count,
            depth,
            ref collapsed_summary,
        } = row.clone();
        let inside_target = drag_active
            && self
                .drop_indicator
                .as_ref()
                .is_some_and(|d| d.id == id && d.position == DropPosition::Inside);
        let forbidden =
            inside_target && self.drop_indicator.as_ref().is_some_and(|d| d.forbidden);
        let is_source = dragging == Some(id.as_str());
        let hovered = self.hovered.as_deref() == Some(id.as_str());

        let id_toggle = id.clone();
        let id_move = id.clone();
        let id_drop = id.clone();
        let id_drag = id.clone();
        let id_key = id.clone();
        let id_focus = id.clone();
        let id_hover = id.clone();
        let id_empty_move = id.clone();
        let id_empty_drop = id.clone();
        let id_add_btn = id.clone();
        let name_drag = name.clone();
        let row_menu = row.clone();
        let row_more_menu = row.clone();
        let this = cx.entity();
        let focus = self.row_focus.get(id.as_str()).cloned();
        let is_focused = focus.as_ref().is_some_and(|f| f.is_focused(window));
        // 编辑中的那一行:名字换成内联输入框,行本身的点击/按键让给它
        let editing = self
            .editing
            .as_ref()
            .filter(|e| e.is_group && e.id == id)
            .map(|e| e.input.clone());

        let collapsed_active = collapsed && !filtering && collapsed_summary.contains_active;
        let collapsed_ind = (collapsed && !filtering)
            .then(|| collapsed_status_indicator(collapsed_summary))
            .flatten();
        let guides = group_guide_lines(depth);
        let ml = row_left_margin(depth);

        let border_color = if forbidden {
            ui::color_error()
        } else if inside_target {
            ui::accent()
        } else if is_focused && editing.is_none() {
            ui::with_alpha(ui::accent(), 0.58)
        } else if collapsed_active {
            ui::with_alpha(ui::accent(), 0.28)
        } else {
            ui::with_alpha(ui::border_default(), 0.0)
        };

        let show_actions = hovered && editing.is_none() && !drag_active;
        let show_empty_hint = !collapsed && count == 0 && !filtering;

        let group_bar = div()
            .id(SharedString::from(format!("group-{id}")))
            .relative()
            // 分组行同样可 Tab 可聚焦(原版 `tabIndex={0}` + `role=treeitem`)
            .when_some(focus, |el, focus| el.track_focus(&focus).tab_index(0))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                this.on_group_key(event, &id_key, collapsed, window, cx);
            }))
            .on_hover(cx.listener(move |this, entered: &bool, _window, cx| {
                let mine = this.hovered.as_deref() == Some(id_hover.as_str());
                if *entered {
                    if mine {
                        return;
                    }
                    this.hovered = Some(id_hover.clone());
                    this.close_preview(cx);
                } else {
                    if !mine {
                        return;
                    }
                    this.hovered = None;
                    this.hover_on_action = false;
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event: &MouseDownEvent, window, cx| {
                    this.close_preview(cx);
                    if this.editing.is_none()
                        && let Some(focus) = this.row_focus.get(id_focus.as_str())
                    {
                        window.focus(focus, cx);
                    }
                }),
            )
            .flex()
            .items_center()
            .gap(px(5.0))
            .ml(px(ml))
            .mr(px(ROW_MARGIN_X))
            .when(!is_first_row, |el| el.mt(px(4.0)))
            .my(px(1.0))
            .pl(px(6.0))
            .pr(px(8.0))
            .py(px(5.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(border_color)
            .when(forbidden || inside_target, |el| el.border_dashed())
            .cursor_pointer()
            .text_size(ui::font_px(11.5))
            .text_color(if collapsed_active {
                ui::text_primary()
            } else {
                ui::text_secondary()
            })
            .when(is_source, |el| el.opacity(0.4))
            .when(inside_target && !forbidden, |el| el.bg(ui::accent_subtle()))
            .when(!inside_target && collapsed_active, |el| {
                el.bg(ui::with_alpha(ui::accent(), 0.06))
            })
            .when(!inside_target, |el| {
                el.hover(|el| {
                    el.bg(ui::with_alpha(ui::text_primary(), 0.045))
                        .text_color(ui::text_primary())
                })
            })
            .when(!filtering, |el| {
                let id_drag_payload = id_drag.clone();
                let id_move = id_move.clone();
                let id_drop = id_drop.clone();
                el.on_drag(
                    DragProjectItem {
                        id: id_drag_payload,
                        is_group: true,
                    },
                    move |item, _offset, _window, cx| {
                        let id = item.id.clone();
                        this.update(cx, |this: &mut ProjectList, _cx| {
                            this.dragging = Some(id);
                        });
                        dnd::preview(name_drag.clone(), PreviewIcon::Group, cx)
                    },
                )
                .on_drag_move(cx.listener(
                    move |this, event: &DragMoveEvent<DragProjectItem>, _window, cx| {
                        // 分组行是容器:allow_inside = true
                        this.on_row_drag_move(event, &id_move, true, cx);
                    },
                ))
                .on_drop(cx.listener(move |this, item: &DragProjectItem, _window, cx| {
                    this.on_row_drop(item, &id_drop, cx);
                }))
            })
            .on_click(cx.listener(move |this, _event, _window, cx| {
                // 编辑态与过滤态里点分组不切换真实折叠状态
                if this.editing.is_some() || this.is_filtering(cx) {
                    return;
                }
                this.toggle_group_with_anim(&id_toggle, collapsed, cx);
            }))
            // Esc 放弃重命名。输入框自己不吃 Escape(`clean_on_escape` 没开,
            // 它 `cx.propagate()`),所以在承载它的这一层收
            .when(editing.is_some(), |el| {
                el.on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.cancel_rename(cx);
                    }
                }))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let entries = group_menu(&cx.entity(), &this.store, &row_menu);
                    menu::show(event.position, entries, window, cx);
                }),
            )
            // 折叠箭头：矢量实心三角，平滑旋转
            .child(
                div()
                    .w(px(12.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(ui::font_px(9.5))
                    .text_color(ui::text_muted())
                    .child(crate::fold_motion::fold_glyph(self.chevrons.value_open(
                        &id,
                        !collapsed || filtering,
                        crate::fold_motion::OPEN_TURNS,
                    ))),
            )
            // 「分组 = 空间」:容器图标,着主题文件夹色
            .child(VectorIcon::new(dnd::BOXES_SHAPES, px(13.0)).ink(ui::color_folder()))
            .map(|el| match &editing {
                Some(input) => el.child(self.rename_input(input, 11.5)),
                None => el.child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .font_weight(FontWeight::MEDIUM)
                        .child(highlighted_text(name, query_lower)),
                ),
            })
            // 折叠态聚合指示器（不漏掉折叠组里的活动项目 / AI 待办 / 完工 / 报错）
            .when(collapsed_active, |el| {
                el.child(
                    div()
                        .id(SharedString::from(format!("group-active-dot-{id}")))
                        .w(px(5.0))
                        .h(px(5.0))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(ui::accent())
                        .tip(t("projectList", "group.collapsedActiveTip")),
                )
            })
            .children(collapsed_ind)
            // 右侧计数胶囊（替代老式 `(3)` 括号文本）
            .child(
                div()
                    .flex_shrink_0()
                    .min_w(px(18.0))
                    .h(px(16.0))
                    .px(px(5.0))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(if collapsed_active {
                        ui::accent_subtle()
                    } else {
                        ui::border_subtle()
                    })
                    .text_size(ui::font_px(10.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if collapsed_active {
                        ui::accent()
                    } else if count == 0 {
                        ui::with_alpha(ui::text_muted(), 0.7)
                    } else {
                        ui::text_muted()
                    })
                    .child(count.to_string()),
            )
            // 零抖动悬浮动作胶囊：`[+]` 添加到此分组 + `[···]` 分组菜单
            .when(show_actions, |el| {
                let row_more = row_more_menu.clone();
                el.child(
                    div()
                        .absolute()
                        .right(px(4.0))
                        .top_0()
                        .bottom_0()
                        .flex()
                        .items_center()
                        .child(
                            self.floating_action_bar()
                                .child(
                                    self.floating_icon_button(
                                        format!("group-add-{id}"),
                                        ICON_PLUS,
                                        t("projectList", "group.addTip"),
                                        cx,
                                    )
                                    .on_click(cx.listener(
                                        move |this, event: &ClickEvent, window, cx| {
                                            cx.stop_propagation();
                                            let entries =
                                                group_add_menu(&this.store, &id_add_btn, depth);
                                            menu::show(
                                                click_position(event, window),
                                                entries,
                                                window,
                                                cx,
                                            );
                                        },
                                    )),
                                )
                                .child(
                                    self.floating_icon_button(
                                        format!("group-more-{id}"),
                                        ICON_MORE,
                                        t("projectList", "group.moreTip"),
                                        cx,
                                    )
                                    .on_click(cx.listener(
                                        move |this, event: &ClickEvent, window, cx| {
                                            cx.stop_propagation();
                                            let entries = group_menu(
                                                &cx.entity(),
                                                &this.store,
                                                &row_more,
                                            );
                                            menu::show(
                                                click_position(event, window),
                                                entries,
                                                window,
                                                cx,
                                            );
                                        },
                                    )),
                                ),
                        ),
                )
            });

        // 空分组展开提示条：挂在同一外层 div 内，保持滚动列表直接子元素 1:1 下标，不破坏 scroll_to_item
        let empty_hint = show_empty_hint.then(|| {
            let child_ml = row_left_margin(depth + 1);
            let child_guides = group_guide_lines(depth + 1);
            div()
                .id(SharedString::from(format!("group-empty-{id}")))
                .relative()
                .py(px(2.0))
                .children(child_guides)
                .on_drag_move(cx.listener(
                    move |this, event: &DragMoveEvent<DragProjectItem>, _window, cx| {
                        this.on_empty_group_drag_move(event, &id_empty_move, cx);
                    },
                ))
                .on_drop(cx.listener(move |this, item: &DragProjectItem, _window, cx| {
                    this.on_row_drop(item, &id_empty_drop, cx);
                }))
                .child(
                    div()
                        .ml(px(child_ml))
                        .mr(px(ROW_MARGIN_X))
                        .px(px(8.0))
                        .py(px(4.0))
                        .rounded(px(5.0))
                        .border_1()
                        .border_dashed()
                        .border_color(if inside_target && !forbidden {
                            ui::accent()
                        } else {
                            ui::border_subtle()
                        })
                        .when(inside_target && !forbidden, |el| el.bg(ui::accent_subtle()))
                        .text_size(ui::font_px(10.5))
                        .text_color(if inside_target && !forbidden {
                            ui::accent()
                        } else {
                            ui::with_alpha(ui::text_muted(), 0.78)
                        })
                        .child(t("projectList", "group.emptyHint")),
                )
        });

        div()
            .relative()
            .flex()
            .flex_col()
            .children(guides)
            .child(group_bar)
            .children(empty_hint)
            .children(self.drop_line(&id_drag, DropPosition::Before, depth, drag_active))
            .children(self.drop_line(&id_drag, DropPosition::After, depth, drag_active))
            .into_any_element()
    }

    /// 行焦点句柄按当前行集合补齐并回收(`render_project` / `render_group` 拿的是
    /// `&self`,不能在那里现建)。句柄要**跨帧稳定**,不能每帧新建 ——
    /// 那样 Tab 过去的焦点每帧都会丢。
    fn sync_row_focus(&mut self, ordered: &[OrderedItem], cx: &mut Context<Self>) {
        let ids: HashSet<&str> = ordered
            .iter()
            .map(|item| match item {
                OrderedItem::Group { id, .. } | OrderedItem::Project { id, .. } => id.as_str(),
            })
            .collect();
        self.row_focus.retain(|id, _| ids.contains(id.as_str()));
        let missing: Vec<String> = ids
            .into_iter()
            .filter(|id| !self.row_focus.contains_key(*id))
            .map(|id| id.to_string())
            .collect();
        for id in missing {
            self.row_focus(&id, cx);
        }
    }

    /// 完成标的进出场表:这一帧哪些行该挂标 —— 新出现的建一条进场,不该挂了的转退场,
    /// 收完的才摘掉(等价于 DOM 的挂载 / 退场驻留 / 卸载)。进度在 `render_project` 里读,
    /// **请求下一帧只能在这儿**(那边没有 window)。
    fn sync_done_tags(
        &mut self,
        ordered: &[OrderedItem],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.read(cx);
        let active_id = store.active_project_id.clone();
        let showing: HashSet<String> = ordered
            .iter()
            .filter_map(|item| match item {
                OrderedItem::Project { id, .. } => {
                    let needs = store
                        .project_state(id)
                        .map(|s| s.needs_attention)
                        .unwrap_or(false);
                    shows_done_tag(needs, active_id.as_deref() == Some(id.as_str()))
                        .then(|| id.clone())
                }
                OrderedItem::Group { .. } => None,
            })
            .collect();
        let stale = std::mem::take(&mut self.done_tags_stale);
        sync_tag_table(&mut self.done_tags, showing, stale);
        if self.done_tags.values().any(|tag| tag.transition.running()) {
            window.request_animation_frame();
        }
    }

    /// 汇总一批后代项目 id 的折叠聚合状态（供折叠分组行与折叠 worktree 父项目共用）。
    fn summarize_projects(
        store: &AppStore,
        active: Option<&str>,
        ids: &[String],
    ) -> CollapsedSummary {
        let mut summary = CollapsedSummary::default();
        for desc_id in ids {
            let is_desc_active = active == Some(desc_id.as_str());
            if is_desc_active {
                summary.contains_active = true;
            }
            if let Some(st) = store.project_state(desc_id) {
                summary.light = summary.light.max(st.highest_light());
                if shows_done_tag(st.needs_attention, is_desc_active) {
                    summary.has_done = true;
                }
                if st.ssh_connected().is_some() {
                    summary.has_ssh = true;
                }
            }
        }
        summary
    }

    /// 列表本体:`get_ordered_tree` 展平出来的分组行 / 项目行逐条铺开。
    /// 静止时 1:1 扁平输出（不破坏 `scroll_to_item` 行号）；仅刚翻态仍在飞的分组与
    /// worktree 簇临时包进 `HeightReveal` 裁剪窗；尾部附带弹性空白区（右键 / 顶层末尾拖放）。
    #[allow(clippy::too_many_arguments)]
    fn render_rows(
        &self,
        ordered: Vec<OrderedItem>,
        dragging: Option<&str>,
        drag_active: bool,
        filtering: bool,
        query_lower: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let store_ref = self.store.read(cx);
        let active = store_ref.active_project_id.clone();
        // 缺省开启,与 store 里那处取值同口径
        let auto_resume = store_ref.config().ai_auto_resume.unwrap_or(true);
        let tree_snapshot: Vec<ProjectTreeItem> = store_ref
            .config()
            .project_tree
            .clone()
            .unwrap_or_default();

        // 若有分组或 worktree 簇正在播「收起最后一程」，把它临时留在渲染表里供 HeightReveal 裁到 0
        let (render_ordered, effective_collapsed_wt) = if !filtering && self.folds.is_animating() {
            let mut closing_groups = HashSet::new();
            let mut closing_wt = HashSet::new();
            for key in self.folds.closing_keys() {
                if let Some(gid) = key.strip_prefix("grp:") {
                    closing_groups.insert(gid.to_string());
                } else if let Some(pid) = key.strip_prefix("wt:") {
                    closing_wt.insert(pid.to_string());
                }
            }
            let mut effective_wt = self.collapsed_worktrees.clone();
            for pid in &closing_wt {
                effective_wt.remove(pid);
            }
            let base = if closing_groups.is_empty() {
                project_tree::get_ordered_tree(store_ref.config())
            } else {
                project_tree::get_ordered_tree_with_open_groups(store_ref.config(), &closing_groups)
            };
            let filtered =
                project_tree::filter_collapsed_worktrees(base, store_ref.projects(), &effective_wt);
            (filtered, effective_wt)
        } else if filtering {
            (ordered, HashSet::new())
        } else {
            (ordered, self.collapsed_worktrees.clone())
        };

        let roles = project_tree::compute_cluster_roles(
            &render_ordered,
            store_ref.projects(),
            &effective_collapsed_wt,
        );
        // 活动项目落在某个展开微容器内（父项目或其任意可见子项）→ 给该微容器外框叠微弱 accent
        let active_clusters: HashSet<String> = active
            .as_deref()
            .and_then(|active_id| {
                render_ordered
                    .iter()
                    .zip(roles.iter())
                    .find_map(|(item, role)| {
                        let OrderedItem::Project { id, .. } = item else {
                            return None;
                        };
                        if id != active_id {
                            return None;
                        }
                        match role.as_ref()? {
                            ProjectClusterRole::ExpandedParent { .. } => Some(id.clone()),
                            ProjectClusterRole::Child { root_parent_id, .. } => {
                                Some(root_parent_id.clone())
                            }
                            _ => None,
                        }
                    })
            })
            .into_iter()
            .collect();

        enum PreparedRow {
            Group(GroupRow),
            Project { row: Box<Row>, is_active: bool },
        }
        impl PreparedRow {
            fn depth(&self) -> usize {
                match self {
                    Self::Group(g) => g.depth,
                    Self::Project { row, .. } => row.depth,
                }
            }
        }

        let mut prepared: Vec<PreparedRow> = Vec::with_capacity(render_ordered.len());
        for (item, role) in render_ordered.into_iter().zip(roles) {
            match item {
                OrderedItem::Group {
                    id,
                    name,
                    collapsed,
                    count,
                    depth,
                    ..
                } => {
                    let collapsed_summary = if collapsed && !filtering {
                        let desc_ids = project_tree::group_descendant_project_ids(
                            &tree_snapshot,
                            store_ref.projects(),
                            &id,
                        );
                        Self::summarize_projects(store_ref, active.as_deref(), &desc_ids)
                    } else {
                        CollapsedSummary::default()
                    };
                    prepared.push(PreparedRow::Group(GroupRow {
                        id,
                        name,
                        collapsed,
                        count,
                        depth,
                        collapsed_summary,
                    }));
                }
                OrderedItem::Project {
                    id,
                    depth,
                    parent_group_id,
                    is_child,
                } => {
                    let store = self.store.read(cx);
                    let Some(p) = store.project(&id) else {
                        continue;
                    };
                    let state = store.project_state(&id);
                    // 行上的 AI 品牌堆叠:递归收布局树里「显示 AI 会话」的 pane,
                    // 判定与 tab 上的品牌图标共用同一把尺子
                    let ai_vendors = ai_vendor_stack(
                        state
                            .map(|s| s.all_panes())
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|pane| pane.shows_ai_session(auto_resume))
                            .map(|pane| pane.ai_agent()),
                    );
                    // 探测缓存:`None` = 还没探完 / 已探但认不出,两种都走通用图标
                    let detected_kind = store.dir_kind(&p.path).flatten();
                    let remote = remote_badge(p, store.ssh_connections());
                    let parent_name = p
                        .parent_project_id
                        .as_deref()
                        .and_then(|pid| store.project(pid))
                        .map(|parent| parent.name.clone());
                    let cluster_role = role.unwrap_or(ProjectClusterRole::Standalone);
                    let cluster_has_active = match &cluster_role {
                        ProjectClusterRole::ExpandedParent { .. } => active_clusters.contains(&id),
                        ProjectClusterRole::Child { root_parent_id, .. } => {
                            active_clusters.contains(root_parent_id)
                        }
                        _ => false,
                    };
                    let group_depth = if parent_group_id.is_some() {
                        match &cluster_role {
                            ProjectClusterRole::Child { root_depth, .. } => *root_depth,
                            _ => depth,
                        }
                    } else {
                        0
                    };
                    let collapsed_summary = if !filtering && self.collapsed_worktrees.contains(&id)
                    {
                        let desc_ids = project_tree::worktree_descendants(store.projects(), &id);
                        Self::summarize_projects(store, active.as_deref(), &desc_ids)
                    } else {
                        CollapsedSummary::default()
                    };
                    let row = Row {
                        id: p.id.clone(),
                        name: p.name.clone(),
                        path: p.path.clone(),
                        light: state.map(|s| s.highest_light()).unwrap_or_default(),
                        ssh_connected: state.and_then(|s| s.ssh_connected()).map(str::to_string),
                        needs_attention: state.map(|s| s.needs_attention).unwrap_or(false),
                        kind: resolve_project_kind(p.kind_override.as_deref(), detected_kind),
                        detected_kind,
                        description: p.description.clone(),
                        kind_override: p.kind_override.clone(),
                        depth,
                        parent_group_id,
                        is_child,
                        parent_name,
                        cluster_role,
                        cluster_has_active,
                        collapsed_summary,
                        group_depth,
                        ai_vendors,
                        // 远程项目的路径是远端 POSIX 路径,本机 worktree 探测与它无关
                        worktree_branch: if remote.is_some() {
                            None
                        } else {
                            self.worktree_branches.get(&p.path).cloned()
                        },
                        remote,
                    };
                    let is_active = active.as_deref() == Some(row.id.as_str());
                    prepared.push(PreparedRow::Project {
                        row: Box::new(row),
                        is_active,
                    });
                }
            }
        }

        #[allow(clippy::too_many_arguments)]
        fn build_slice(
            this: &ProjectList,
            items: &[PreparedRow],
            slice_offset: usize,
            dragging: Option<&str>,
            drag_active: bool,
            filtering: bool,
            query_lower: &str,
            tree_snapshot: &[ProjectTreeItem],
            in_animated_tray: bool,
            window: &Window,
            cx: &mut Context<ProjectList>,
        ) -> Vec<AnyElement> {
            let mut out = Vec::new();
            let mut i = 0;
            while i < items.len() {
                let global_index = slice_offset + i;
                match &items[i] {
                    PreparedRow::Group(group) => {
                        out.push(this.render_group(
                            group.clone(),
                            global_index == 0,
                            dragging,
                            drag_active,
                            filtering,
                            query_lower,
                            window,
                            cx,
                        ));
                        let key = format!("grp:{}", group.id);
                        if !filtering
                            && let Some(fold) = this.folds.get(&key)
                        {
                            let depth = group.depth;
                            let mut j = i + 1;
                            while j < items.len() && items[j].depth() > depth {
                                j += 1;
                            }
                            if j > i + 1 {
                                let children = build_slice(
                                    this,
                                    &items[i + 1..j],
                                    slice_offset + i + 1,
                                    dragging,
                                    drag_active,
                                    filtering,
                                    query_lower,
                                    tree_snapshot,
                                    false,
                                    window,
                                    cx,
                                );
                                let col = div().flex().flex_col().children(children);
                                out.push(fold.render(col, window));
                            }
                            i = j;
                            continue;
                        }
                        i += 1;
                    }
                    PreparedRow::Project { row, is_active } => {
                        let wt_key = format!("wt:{}", row.id);
                        if !filtering
                            && !row.is_child
                            && let Some(fold) = this.folds.get(&wt_key)
                        {
                            let mut j = i + 1;
                            while j < items.len()
                                && matches!(
                                    &items[j],
                                    PreparedRow::Project { row: child, .. } if child.is_child
                                )
                            {
                                j += 1;
                            }
                            out.push(this.render_project(
                                (**row).clone(),
                                *is_active,
                                false,
                                dragging,
                                drag_active,
                                filtering,
                                query_lower,
                                tree_snapshot,
                                window,
                                cx,
                            ));
                            if j > i + 1 {
                                let child_elems = build_slice(
                                    this,
                                    &items[i + 1..j],
                                    slice_offset + i + 1,
                                    dragging,
                                    drag_active,
                                    filtering,
                                    query_lower,
                                    tree_snapshot,
                                    true,
                                    window,
                                    cx,
                                );
                                let cluster_bg = ui::with_alpha(ui::border_subtle(), 0.78);
                                let cluster_border = if row.cluster_has_active {
                                    ui::with_alpha(ui::accent(), 0.28)
                                } else {
                                    ui::border_default()
                                };
                                let ml = row_left_margin(row.group_depth);
                                let guides = group_guide_lines(row.group_depth);
                                // 微容器底部外框包在 HeightReveal 之外：收起 / 展开时圆角底边
                                // 像物理抽屉一样平滑上下滑动，不会被裁剪窗削平
                                let tray_inner =
                                    div().flex().flex_col().pb(px(3.0)).children(child_elems);
                                out.push(
                                    div()
                                        .relative()
                                        .children(guides)
                                        .child(
                                            div()
                                                .ml(px(ml))
                                                .mr(px(ROW_MARGIN_X))
                                                .mb(px(3.0))
                                                .rounded_b(px(CLUSTER_RADIUS))
                                                .border_l_1()
                                                .border_r_1()
                                                .border_b_1()
                                                .border_color(cluster_border)
                                                .bg(cluster_bg)
                                                .overflow_hidden()
                                                .child(fold.render(tray_inner, window)),
                                        )
                                        .into_any_element(),
                                );
                            }
                            i = j;
                            continue;
                        }
                        out.push(this.render_project(
                            (**row).clone(),
                            *is_active,
                            in_animated_tray,
                            dragging,
                            drag_active,
                            filtering,
                            query_lower,
                            tree_snapshot,
                            window,
                            cx,
                        ));
                        i += 1;
                    }
                }
            }
            out
        }

        let elements = build_slice(
            self,
            &prepared,
            0,
            dragging,
            drag_active,
            filtering,
            query_lower,
            &tree_snapshot,
            false,
            window,
            cx,
        );

        let bottom_zone = (!filtering).then(|| self.render_bottom_drop_zone(drag_active, cx));

        div()
            .id("project-list-rows")
            .flex()
            .flex_col()
            .flex_1()
            .py(px(4.0))
            .overflow_y_scroll()
            // 滚动偏移交给句柄持有,揭示才滚得动它(见模块注释「揭示」)
            .track_scroll(&self.rows_scroll)
            // 列表一滚锚点就失效 —— 原版挂 window 的 scroll/wheel 直接关掉
            .on_scroll(cx.listener(|this, _event: &ScrollEvent, _window, cx| {
                this.close_preview(cx);
            }))
            .children(elements)
            .children(bottom_zone)
    }

    /// 列表尾部弹性空白区：支持空白处右键新建，也作为把深层分组或 worktree 子项
    /// 拖回顶层末尾的兜底落点（排在最后一位，不改变前面任何行的 `scroll_to_item` 下标）。
    fn render_bottom_drop_zone(
        &self,
        drag_active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let active_target = drag_active && self.drop_to_root_end;
        div()
            .id("project-list-bottom-zone")
            .flex_1()
            .min_h(px(44.0))
            .px(px(ROW_MARGIN_X))
            .py(px(6.0))
            .flex()
            .flex_col()
            .justify_start()
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.close_preview(cx);
                    let entries = add_menu(&this.store);
                    menu::show(event.position, entries, window, cx);
                }),
            )
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<DragProjectItem>, _window, cx| {
                    this.on_bottom_zone_drag_move(event, cx);
                },
            ))
            .on_drop(cx.listener(|this, item: &DragProjectItem, _window, cx| {
                this.on_bottom_zone_drop(item, cx);
            }))
            .when(active_target, |el| {
                el.child(
                    div()
                        .h(px(28.0))
                        .rounded(px(6.0))
                        .border_1()
                        .border_dashed()
                        .border_color(ui::accent())
                        .bg(ui::accent_subtle())
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(ui::font_px(10.5))
                        .text_color(ui::accent())
                        .child(t("projectList", "dropToRootEnd")),
                )
            })
    }

    /// 顶部栏头（严格保持 `ui::PANEL_HEADER_H = 30px`，与右侧工作台页签条底边线齐平）：
    /// 左侧标题 + 轻量计数胶囊，右侧精炼工具按钮组（筛选 / 全部折叠展开 / `+` 添加菜单）。
    fn render_list_header(
        &self,
        total_projects: usize,
        matched_projects: usize,
        filtering: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (has_foldable, any_expanded) = self.any_foldable_expanded(self.store.read(cx));
        let filter_active = self.filter_open || filtering;

        let header_btn = |id: &'static str, active: bool| {
            div()
                .id(id)
                .w(px(22.0))
                .h(px(22.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .cursor_pointer()
                .when(active, |el| el.bg(ui::accent_subtle()))
                .hover(|el| el.bg(ui::border_subtle()))
        };

        let count_text = if filtering {
            format!("{matched_projects}/{total_projects}")
        } else {
            total_projects.to_string()
        };

        div()
            .id("project-list-header")
            .flex()
            .items_center()
            .justify_between()
            .gap(px(6.0))
            .px(px(10.0))
            // 高度与右侧工作台页签条对齐(见 `ui::PANEL_HEADER_H`),不靠内衬凑
            .h(px(ui::PANEL_HEADER_H))
            .flex_none()
            .border_b_1()
            .border_color(ui::border_subtle())
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let entries = add_menu(&this.store);
                    menu::show(event.position, entries, window, cx);
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .min_w(px(0.0))
                    .child(
                        div()
                            .text_size(ui::font_px(11.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(ui::text_muted())
                            .child(t("panels", "projects")),
                    )
                    .when(total_projects > 0, |el| {
                        el.child(
                            div()
                                .px(px(5.0))
                                .py(px(0.5))
                                .rounded_full()
                                .bg(if filtering {
                                    ui::accent_subtle()
                                } else {
                                    ui::border_subtle()
                                })
                                .text_size(ui::font_px(9.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(if filtering {
                                    ui::accent()
                                } else {
                                    ui::text_muted()
                                })
                                .child(count_text),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_none()
                    .gap(px(2.0))
                    .when(total_projects > 0, |el| {
                        el.child(
                            header_btn("project-header-filter", filter_active)
                                .tip(t("projectList", "header.filterTip"))
                                .on_click(cx.listener(|this, _event, window, cx| {
                                    this.toggle_filter(window, cx);
                                }))
                                .child(
                                    VectorIcon::new(ICON_SEARCH, px(12.5)).ink(if filter_active {
                                        ui::accent()
                                    } else {
                                        ui::text_muted()
                                    }),
                                ),
                        )
                    })
                    .when(has_foldable && !filtering, |el| {
                        let (icon, tip) = if any_expanded {
                            (ICON_COLLAPSE_ALL, t("projectList", "header.collapseAll"))
                        } else {
                            (ICON_EXPAND_ALL, t("projectList", "header.expandAll"))
                        };
                        el.child(
                            header_btn("project-header-fold-all", false)
                                .tip(tip)
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.toggle_all_folds(cx);
                                }))
                                .child(VectorIcon::new(icon, px(12.5)).ink(ui::text_muted())),
                        )
                    })
                    .child(
                        header_btn("project-header-add", false)
                            .tip(t("projectList", "header.addTip"))
                            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.close_preview(cx);
                                let entries = add_menu(&this.store);
                                menu::show(click_position(event, window), entries, window, cx);
                            }))
                            .child(VectorIcon::new(ICON_PLUS, px(13.0)).ink(ui::text_muted())),
                    ),
            )
    }

    /// 原地行内筛选条（经 `HeightReveal` 从栏头下方平滑展开）：
    /// 紧凑搜索框（支持 `Esc` 收起、`↓` 进入首行、`Enter` 切首项目）+ 状态快捷芯片。
    fn render_filter_bar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let (ai_active_count, remote_count) = {
            let store = self.store.read(cx);
            let active_id = store.active_project_id.as_deref();
            let mut ai_cnt = 0usize;
            let mut rem_cnt = 0usize;
            for p in store.projects() {
                if crate::ssh_conn::is_remote_project(p) {
                    rem_cnt += 1;
                }
                let st = store.project_state(&p.id);
                let light = st.map(|s| s.highest_light()).unwrap_or_default();
                let needs = st.map(|s| s.needs_attention).unwrap_or(false);
                if is_ai_active_project(light, needs, active_id == Some(p.id.as_str())) {
                    ai_cnt += 1;
                }
            }
            (ai_cnt, rem_cnt)
        };
        let has_query = !self.filter_query_lower(cx).is_empty();
        let mode = self.filter_mode;

        let chip = |id: &'static str, label: String, selected: bool| {
            div()
                .id(id)
                .px(px(6.0))
                .py(px(2.0))
                .rounded_full()
                .border_1()
                .border_color(if selected {
                    ui::with_alpha(ui::accent(), 0.35)
                } else {
                    ui::with_alpha(ui::border_default(), 0.0)
                })
                .bg(if selected {
                    ui::accent_subtle()
                } else {
                    ui::border_subtle()
                })
                .cursor_pointer()
                .text_size(ui::font_px(10.0))
                .font_weight(if selected {
                    FontWeight::MEDIUM
                } else {
                    FontWeight::NORMAL
                })
                .text_color(if selected {
                    ui::accent()
                } else {
                    ui::text_muted()
                })
                .hover(|el| {
                    el.bg(if selected {
                        ui::accent_subtle()
                    } else {
                        ui::border_default()
                    })
                    .text_color(if selected {
                        ui::accent()
                    } else {
                        ui::text_primary()
                    })
                })
                .child(label)
        };

        let ai_label = if ai_active_count > 0 {
            format!("{} · {ai_active_count}", t("projectList", "filter.aiActive"))
        } else {
            t("projectList", "filter.aiActive").to_string()
        };
        let remote_label = format!("{} · {remote_count}", t("projectList", "filter.remote"));

        div()
            .flex()
            .flex_col()
            .gap(px(5.0))
            .px(px(8.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .bg(ui::with_alpha(ui::bg_base(), 0.35))
            // 捕获相先于 Input 处理 Esc（清空并收起）与 ↓（焦点跳到列表首行）
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let plain = !event.keystroke.modifiers.modified();
                match event.keystroke.key.as_str() {
                    "escape" => {
                        cx.stop_propagation();
                        this.close_filter(true, window, cx);
                    }
                    "down" if plain => {
                        cx.stop_propagation();
                        this.focus_first_visible_row(window, cx);
                    }
                    _ => {}
                }
            }))
            .child(
                div()
                    .h(px(24.0))
                    .px(px(6.0))
                    .rounded(px(5.0))
                    .bg(ui::bg_base())
                    .border_1()
                    .border_color(ui::border_default())
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .child(VectorIcon::new(ICON_SEARCH, px(11.5)).ink(ui::text_muted()))
                    .when_some(self.filter_input.as_ref(), |el, input| {
                        el.child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .text_size(ui::font_px(11.5))
                                .text_color(ui::text_primary())
                                .child(Input::new(input).appearance(false)),
                        )
                    })
                    .when(has_query, |el| {
                        el.child(
                            div()
                                .id("project-filter-clear-query")
                                .w(px(16.0))
                                .h(px(16.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(3.0))
                                .cursor_pointer()
                                .hover(|el| el.bg(ui::border_subtle()))
                                .tip(t("projectList", "filter.clear"))
                                .on_click(cx.listener(|this, _event, window, cx| {
                                    if let Some(input) = this.filter_input.clone() {
                                        input.update(cx, |state, cx| {
                                            state.set_value(String::new(), window, cx);
                                            state.focus(window, cx);
                                        });
                                    }
                                    cx.notify();
                                }))
                                .child(
                                    VectorIcon::new(ICON_CLOSE_SM, px(10.0)).ink(ui::text_muted()),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap(px(4.0))
                    .child(
                        chip(
                            "project-filter-all",
                            t("projectList", "filter.all").to_string(),
                            mode == ProjectFilterMode::All,
                        )
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.filter_mode = ProjectFilterMode::All;
                            this.folds.clear();
                            cx.notify();
                        })),
                    )
                    .child(
                        chip(
                            "project-filter-ai",
                            ai_label,
                            mode == ProjectFilterMode::AiActive,
                        )
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.filter_mode = if this.filter_mode == ProjectFilterMode::AiActive {
                                ProjectFilterMode::All
                            } else {
                                ProjectFilterMode::AiActive
                            };
                            this.folds.clear();
                            cx.notify();
                        })),
                    )
                    .when(remote_count > 0, |el| {
                        el.child(
                            chip(
                                "project-filter-remote",
                                remote_label,
                                mode == ProjectFilterMode::Remote,
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.filter_mode = if this.filter_mode == ProjectFilterMode::Remote
                                {
                                    ProjectFilterMode::All
                                } else {
                                    ProjectFilterMode::Remote
                                };
                                this.folds.clear();
                                cx.notify();
                            })),
                        )
                    }),
            )
    }

    /// 零项目时的空态引导卡片：清晰交代拖放文件夹与添加本地 / 远程项目入口。
    fn render_empty_state(&self, cx: &mut Context<Self>) -> gpui::Div {
        let store_local = self.store.clone();
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .p(px(12.0))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let entries = add_menu(&this.store);
                    menu::show(event.position, entries, window, cx);
                }),
            )
            .child(
                div()
                    .w_full()
                    .max_w(px(260.0))
                    .p(px(16.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_dashed()
                    .border_color(ui::border_default())
                    .bg(ui::with_alpha(ui::bg_elevated(), 0.42))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .w(px(34.0))
                            .h(px(34.0))
                            .rounded(px(8.0))
                            .bg(ui::accent_subtle())
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(FileIcon::folder(false).size(px(18.0)).color(ui::accent())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .text_size(ui::font_px(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(ui::text_primary())
                                    .child(t("projectList", "empty.title")),
                            )
                            .child(
                                div()
                                    .text_size(ui::font_px(11.0))
                                    .text_color(ui::text_muted())
                                    .text_center()
                                    .child(t("projectList", "empty.hint")),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .pt(px(2.0))
                            .child(
                                div()
                                    .id("empty-add-local")
                                    .w_full()
                                    .py(px(6.0))
                                    .px(px(10.0))
                                    .rounded(px(6.0))
                                    .bg(ui::accent())
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .gap(px(6.0))
                                    .text_size(ui::font_px(11.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(ui::bg_base())
                                    .hover(|el| el.opacity(0.9))
                                    .tip(t("projectList", "addProjectTip"))
                                    .on_click(move |_event, window, cx| {
                                        modal::open_add_project(store_local.clone(), window, cx);
                                    })
                                    .child(
                                        FileIcon::folder(false)
                                            .size(px(13.0))
                                            .color(ui::bg_base()),
                                    )
                                    .child(t("projectList", "addProject")),
                            )
                            .child(
                                div()
                                    .id("empty-add-remote")
                                    .w_full()
                                    .py(px(5.5))
                                    .px(px(10.0))
                                    .rounded(px(6.0))
                                    .border_1()
                                    .border_color(ui::border_default())
                                    .bg(ui::bg_surface())
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .gap(px(6.0))
                                    .text_size(ui::font_px(11.5))
                                    .text_color(ui::text_secondary())
                                    .hover(|el| {
                                        el.border_color(ui::accent()).text_color(ui::accent())
                                    })
                                    .tip(t("projectList", "addRemoteProjectTip"))
                                    .on_click(cx.listener(|this, _event, window, cx| {
                                        crate::remote_project::open(
                                            this.store.clone(),
                                            None,
                                            window,
                                            cx,
                                        );
                                    }))
                                    .child(VectorIcon::new(SERVER, px(13.0)).ink(ui::color_info()))
                                    .child(t("projectList", "addRemoteProject")),
                            ),
                    ),
            )
    }

    /// 筛选无匹配时的轻空态 + 一键清除筛选按钮。
    fn render_filter_empty_state(&self, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .p(px(16.0))
            .child(
                div()
                    .text_size(ui::font_px(11.5))
                    .text_color(ui::text_muted())
                    .child(t("projectList", "filter.empty")),
            )
            .child(
                div()
                    .id("project-filter-empty-clear")
                    .px(px(10.0))
                    .py(px(4.0))
                    .rounded(px(5.0))
                    .border_1()
                    .border_color(ui::border_default())
                    .cursor_pointer()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_secondary())
                    .hover(|el| el.border_color(ui::accent()).text_color(ui::accent()))
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.clear_filter(window, cx);
                    }))
                    .child(t("projectList", "filter.clear")),
            )
    }

    /// 项目行的名字位:编辑态换成内联输入框；否则主行（支持关键词高亮与激活加粗），
    /// 若设置了需求描述则自然折为紧凑副行，不与项目名抢水平空间。
    fn project_row_label(
        &self,
        editing: Option<&Entity<InputState>>,
        name: String,
        description: Option<String>,
        is_active: bool,
        query_lower: &str,
    ) -> AnyElement {
        match editing {
            Some(input) => self.rename_input(input, 13.0),
            None => div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .flex()
                .flex_col()
                .gap(px(1.0))
                .child(
                    div()
                        .min_w(px(0.0))
                        .when(is_active, |el| el.font_weight(FontWeight::MEDIUM))
                        .child(highlighted_text(name, query_lower)),
                )
                .when_some(description, |el, desc| {
                    el.child(
                        div()
                            .min_w(px(0.0))
                            .text_size(ui::font_px(10.5))
                            .text_color(ui::text_muted())
                            .child(highlighted_text(desc, query_lower)),
                    )
                })
                .into_any_element(),
        }
    }

    /// 项目行与 worktree 子项行共用的零抖动悬浮「···」更多操作按钮。
    /// 绝对定位贴在行右缘，不推挤状态灯 / 胶囊 / 标题；点击直接弹出完整右键菜单。
    fn project_more_overlay(
        &self,
        row: &Row,
        tree: &[ProjectTreeItem],
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let row_for_menu = row.clone();
        let tree_for_menu = tree.to_vec();
        div()
            .absolute()
            .right(px(4.0))
            .top_0()
            .bottom_0()
            .flex()
            .items_center()
            .child(
                self.floating_action_bar().child(
                    self.floating_icon_button(
                        format!("project-more-{}", row.id),
                        ICON_MORE,
                        t("projectList", "row.moreTip"),
                        cx,
                    )
                    .on_click(cx.listener(
                        move |this, event: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            this.close_preview(cx);
                            let entries = project_menu(
                                &cx.entity(),
                                &this.store,
                                &row_for_menu,
                                &tree_for_menu,
                                cx,
                            );
                            menu::show(click_position(event, window), entries, window, cx);
                        },
                    )),
                ),
            )
    }

    /// worktree 子项行的名字位（B1 分支优先）：编辑态换成内联输入框，否则主文 + 可选浅色副文，
    /// 均支持行内搜索关键词高亮。
    fn child_row_label(
        &self,
        editing: Option<&Entity<InputState>>,
        display: ChildDisplayLabel,
        is_active: bool,
        query_lower: &str,
    ) -> AnyElement {
        match editing {
            Some(input) => self.rename_input(input, 12.0),
            None => div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .flex()
                .items_center()
                .gap(px(5.0))
                .child(
                    div()
                        .min_w(px(0.0))
                        .when(is_active, |el| el.font_weight(FontWeight::MEDIUM))
                        .child(highlighted_text(display.primary, query_lower)),
                )
                .when_some(display.secondary, |el, sec| {
                    el.child(
                        div()
                            .min_w(px(0.0))
                            .text_size(ui::font_px(10.0))
                            .text_color(ui::text_muted())
                            .child(highlighted_text(sec, query_lower)),
                    )
                })
                .into_any_element(),
        }
    }

    /// 父项目行上的 worktree 折叠切换胶囊：`[⑂ N (聚合状态点) ▾/▸]`。
    /// 点击只切折叠态，不切换活动项目、不起拖拽；悬停时抑制 520px 大预览卡。
    fn worktree_toggle_chip(
        &self,
        parent_id: &str,
        count: usize,
        collapsed: bool,
        summary: &CollapsedSummary,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id_toggle = parent_id.to_string();
        let chevron_key = format!("wt:{parent_id}");
        let turns =
            self.chevrons
                .value_open(&chevron_key, !collapsed, crate::fold_motion::OPEN_TURNS);
        let highlight = collapsed && summary.contains_active;
        let fg = if highlight {
            ui::accent()
        } else if collapsed {
            ui::text_secondary()
        } else {
            ui::text_muted()
        };
        let bg = if highlight {
            ui::accent_subtle()
        } else if collapsed {
            ui::border_default()
        } else {
            ui::border_subtle()
        };
        let tip = if collapsed {
            tr!("projectList", "worktreeToggleExpand", count = count)
        } else {
            tr!("projectList", "worktreeToggleCollapse", count = count)
        };
        let indicator = collapsed
            .then(|| collapsed_status_indicator(summary))
            .flatten();

        div()
            .id(SharedString::from(format!("wt-toggle-{parent_id}")))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(3.0))
            .h(px(18.0))
            .px(px(5.0))
            .rounded(px(4.0))
            .bg(bg)
            .when(highlight, |el| {
                el.border_1()
                    .border_color(ui::with_alpha(ui::accent(), 0.38))
            })
            .cursor_pointer()
            .text_size(ui::font_px(10.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(fg)
            .hover(|el| el.bg(ui::border_strong()).text_color(ui::text_primary()))
            .tip(tip)
            .on_hover(cx.listener(|this, entered: &bool, _window, cx| {
                this.hover_on_action = *entered;
                if *entered {
                    this.close_preview(cx);
                }
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_worktree_collapse(&id_toggle, cx);
            }))
            .child(VectorIcon::new(ICON_WORKTREE, px(10.0)).ink(fg))
            .child(count.to_string())
            .children(indicator)
            .child(
                div()
                    .w(px(8.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(ui::font_px(8.5))
                    .child(crate::fold_motion::fold_glyph(turns)),
            )
    }

    /// 项目行共用的交互挂载（焦点 / 键盘 / 悬停 / 拖放 / 单击切换+双击改名 / 右键菜单 / 路径 tooltip），
    /// 样式壳由调用方（常规行 vs worktree 子项行）各自定制。
    #[allow(clippy::too_many_arguments)]
    fn project_row_interactions(
        &self,
        row: &Row,
        is_active: bool,
        tooltip: String,
        dragging: Option<&str>,
        filtering: bool,
        tree: &[ProjectTreeItem],
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id = row.id.clone();
        let kind = row.kind;
        let is_child = row.is_child;
        let is_source = dragging == Some(id.as_str());
        let id_click = id.clone();
        let name_click = row.name.clone();
        let id_move = id.clone();
        let id_drop = id.clone();
        let id_key = id.clone();
        let id_focus = id.clone();
        let id_hover = id.clone();
        let name_drag = row.name.clone();
        let row_for_menu = row.clone();
        let tree_for_menu: Vec<ProjectTreeItem> = tree.to_vec();
        let this = cx.entity();
        let focus = self.row_focus.get(id.as_str()).cloned();
        // 编辑中的那一行:行本身的点击/按键让给输入框
        let is_editing = self
            .editing
            .as_ref()
            .is_some_and(|e| !e.is_group && e.id == id);

        div()
            .id(SharedString::from(format!("project-{id}")))
            .group(SharedString::from(format!("project-row-{id}")))
            // 行级焦点 + tab 停靠点(原版每行 `tabIndex={0}`)。
            // 整栏是一个 tab group,组内序号从 0 起,不与别处的 tab 序打架
            .when_some(focus, |el, focus| el.track_focus(&focus).tab_index(0))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                this.on_project_key(event, &id_key, window, cx);
            }))
            .cursor_pointer()
            .when(is_source, |el| el.opacity(0.4))
            // 绝对路径挂 tooltip：有 AI 会话时路径改由缩略图卡头显示,原生 tooltip 会盖住那张卡
            .when(row.ai_vendors.is_empty(), |el| el.tip(tooltip))
            // 悬停记到 view state 上 —— 悬浮动作胶囊显隐与缩略图计时都要它。
            // ⚠️ 离开分支必须先核对「离开的正是我们记着的那一行」
            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                let mine = this.hovered.as_deref() == Some(id_hover.as_str());
                if *hovered {
                    if mine {
                        return;
                    }
                    this.hovered = Some(id_hover.clone());
                    this.hover_on_action = false;
                    this.schedule_preview(id_hover.clone(), cx);
                } else {
                    if !mine {
                        return;
                    }
                    this.hovered = None;
                    this.hover_on_action = false;
                    // 移出即关(原版 onMouseLeave 的 closePreview)
                    this.close_preview(cx);
                }
                cx.notify();
            }))
            // 筛选激活时禁拖放，防止在残缺视图里拖乱真实树序
            .when(!filtering, |el| {
                let id_drag = id.clone();
                let id_move = id_move.clone();
                let id_drop = id_drop.clone();
                el.on_drag(
                    DragProjectItem {
                        id: id_drag,
                        is_group: false,
                    },
                    move |item, _offset, _window, cx| {
                        let id = item.id.clone();
                        this.update(cx, |this: &mut ProjectList, _cx| {
                            this.dragging = Some(id);
                        });
                        dnd::preview(name_drag.clone(), PreviewIcon::Project(kind), cx)
                    },
                )
                // worktree 子项目**不作为落点**(位置是从父项目派生的),
                // 但自身可以被拖走 = 脱离父项目 —— 所以只摘 drop 那半边
                .when(!is_child, |el| {
                    el.on_drag_move(cx.listener(
                        move |this, event: &DragMoveEvent<DragProjectItem>, _window, cx| {
                            // 项目不是容器:allow_inside = false,只有 before/after
                            this.on_row_drag_move(event, &id_move, false, cx);
                        },
                    ))
                    .on_drop(cx.listener(move |this, item: &DragProjectItem, _window, cx| {
                        this.on_row_drop(item, &id_drop, cx);
                    }))
                })
            })
            // 按下即收缩略图并把行焦点收过来
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event: &MouseDownEvent, window, cx| {
                    this.close_preview(cx);
                    if this.editing.is_none()
                        && let Some(focus) = this.row_focus.get(id_focus.as_str())
                    {
                        window.focus(focus, cx);
                    }
                }),
            )
            // 单击切换，双击已激活项目智能进入行内重命名（切过来的紧邻第二下不误触）
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if this.editing.is_some() {
                    return;
                }
                let count = click_count(event);
                match this.row_click.click(&id_click, is_active, count) {
                    SwitchClick::Switch => {
                        this.store
                            .update(cx, |store, cx| store.set_active_project(&id_click, cx));
                    }
                    SwitchClick::Rename => {
                        this.start_rename(id_click.clone(), false, &name_click, window, cx);
                    }
                    SwitchClick::Ignore => {}
                }
            }))
            // Esc 放弃重命名(见分组行上的同一条注释)
            .when(is_editing, |el| {
                el.on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.cancel_rename(cx);
                    }
                }))
            })
            // 右键菜单,开菜单前先收掉悬停缩略图
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.close_preview(cx);
                    let entries =
                        project_menu(&cx.entity(), &this.store, &row_for_menu, &tree_for_menu, cx);
                    menu::show(event.position, entries, window, cx);
                }),
            )
    }

    /// 常规项目行（独立项目、折叠父项目、展开微容器头部）的样式壳。
    /// 恒保留 `border_1()`（透明/柔边/聚焦环），确保状态切换零 1px 抖动。
    #[allow(clippy::too_many_arguments)]
    fn project_row_shell(
        &self,
        row: &Row,
        is_active: bool,
        is_focused: bool,
        in_expanded_cluster: bool,
        show_done_tag: bool,
        dragging: Option<&str>,
        filtering: bool,
        tree: &[ProjectTreeItem],
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (pl, pr) = if in_expanded_cluster {
            (ROW_INNER_PL - CLUSTER_BORDER_W, 8.0 - CLUSTER_BORDER_W)
        } else {
            (ROW_INNER_PL, 8.0)
        };
        let row_tint = if is_active {
            Some(ui::accent_subtle())
        } else if row.light == StatusLight::Attention {
            Some(ui::with_alpha(ui::color_attention(), 0.07))
        } else if show_done_tag {
            Some(ui::with_alpha(ui::color_success(), 0.055))
        } else {
            None
        };
        let border_color = if is_focused {
            ui::with_alpha(ui::accent(), if is_active { 0.48 } else { 0.60 })
        } else if is_active && !in_expanded_cluster {
            ui::with_alpha(ui::accent(), 0.22)
        } else {
            ui::with_alpha(ui::accent(), 0.0)
        };
        let text_color = if is_active || row.light >= StatusLight::Attention || show_done_tag {
            ui::text_primary()
        } else {
            ui::text_secondary()
        };

        self.project_row_interactions(row, is_active, row.path.clone(), dragging, filtering, tree, cx)
            .relative()
            .flex()
            .items_center()
            .gap(px(7.0))
            .pl(px(pl))
            .pr(px(pr))
            .py(px(5.5))
            .when(in_expanded_cluster, |el| {
                el.rounded_t(px(CLUSTER_INNER_RADIUS))
                    .border_b_1()
                    .border_color(if is_focused {
                        ui::with_alpha(ui::accent(), 0.55)
                    } else {
                        ui::border_subtle()
                    })
            })
            .when(!in_expanded_cluster, |el| {
                el.rounded(px(6.0))
                    .border_1()
                    .border_color(border_color)
            })
            .text_size(ui::font_px(13.0))
            .text_color(text_color)
            .when_some(row_tint, |el, bg| el.bg(bg))
            .when(!is_active, |el| {
                el.hover(|el| {
                    el.bg(ui::with_alpha(ui::text_primary(), 0.045))
                        .text_color(ui::text_primary())
                })
            })
    }

    #[allow(clippy::too_many_arguments)]
    fn render_project(
        &self,
        row: Row,
        is_active: bool,
        in_animated_tray: bool,
        dragging: Option<&str>,
        drag_active: bool,
        filtering: bool,
        query_lower: &str,
        tree: &[ProjectTreeItem],
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cluster_bg = ui::with_alpha(ui::border_subtle(), 0.78);
        let cluster_border = if row.cluster_has_active {
            ui::with_alpha(ui::accent(), 0.28)
        } else {
            ui::border_default()
        };
        let is_focused = self
            .row_focus
            .get(row.id.as_str())
            .is_some_and(|f| f.is_focused(window));

        // ── 展开微容器内的 worktree 子项行（方案 B1 + C 精修） ─────────
        if let ProjectClusterRole::Child {
            rel_depth,
            is_last_sibling,
            is_last_in_cluster,
            ref ancestor_continues,
            ..
        } = row.cluster_role
        {
            let hovered = self.hovered.as_deref() == Some(row.id.as_str());
            let show_done_tag = shows_done_tag(row.needs_attention, is_active);
            let done_tag = self.done_tags.get(row.id.as_str());
            let editing = self
                .editing
                .as_ref()
                .filter(|e| !e.is_group && e.id == row.id)
                .map(|e| e.input.clone());
            let display = resolve_child_display_label(
                &row.name,
                &row.path,
                row.parent_name.as_deref(),
                row.worktree_branch.as_deref(),
                row.description.as_deref(),
            );
            let tooltip = child_tooltip_text(
                &row.name,
                &row.path,
                row.worktree_branch.as_deref(),
                &display,
            );
            let own_rail_x = cluster_rail_x(rel_depth.saturating_sub(1));
            let gutter_w = own_rail_x + CHILD_ELBOW_W + CHILD_ELBOW_GAP;
            let connectors = child_tree_connectors(
                rel_depth,
                is_last_sibling,
                ancestor_continues,
                is_active,
            );
            let branch_icon_color = if is_active {
                ui::accent()
            } else if hovered {
                ui::text_secondary()
            } else {
                ui::text_muted()
            };
            let child_tint = if is_active {
                Some(ui::accent_subtle())
            } else if hovered {
                Some(ui::with_alpha(ui::text_primary(), 0.045))
            } else if row.light == StatusLight::Attention {
                Some(ui::with_alpha(ui::color_attention(), 0.07))
            } else if show_done_tag {
                Some(ui::with_alpha(ui::color_success(), 0.055))
            } else {
                None
            };
            let child_border = if is_focused && editing.is_none() {
                ui::with_alpha(ui::accent(), 0.58)
            } else if is_active {
                ui::with_alpha(ui::accent(), 0.22)
            } else {
                ui::with_alpha(ui::accent(), 0.0)
            };
            let show_more = hovered && editing.is_none() && !drag_active;

            let pill = div()
                .relative()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap(px(5.0))
                .px(px(6.0))
                .py(px(3.5))
                .rounded(px(5.0))
                .border_1()
                .border_color(child_border)
                .text_size(ui::font_px(12.0))
                .text_color(if is_active || hovered || row.light >= StatusLight::Attention || show_done_tag {
                    ui::text_primary()
                } else {
                    ui::text_secondary()
                })
                .when_some(child_tint, |el, bg| el.bg(bg))
                // 领位换成轻量的矢量分支图标（不再重复父项目的大号技术栈图标）
                .child(VectorIcon::new(ICON_BRANCH, px(12.0)).ink(branch_icon_color))
                .child(self.child_row_label(editing.as_ref(), display, is_active, query_lower))
                // 远程子 worktree 正常态继承父项目连接，仅断链时单独标红提示
                .children(
                    row.remote
                        .clone()
                        .filter(|r| r.broken)
                        .map(|remote| remote_badge_chip(&row.id, remote)),
                )
                .children(row_right_status(
                    &row.id,
                    &row.ai_vendors,
                    CHILD_AI_ICON_SIZE,
                    show_done_tag,
                    done_tag,
                    row.light,
                    row.ssh_connected.as_deref(),
                ))
                // 零抖动悬浮「···」操作胶囊
                .when(show_more, |el| el.child(self.project_more_overlay(&row, tree, cx)));

            let row_el = self
                .project_row_interactions(&row, is_active, tooltip, dragging, filtering, tree, cx)
                .relative()
                .flex()
                .items_center()
                .pl(px(gutter_w))
                .pr(px(4.0))
                .py(px(1.5))
                .children(connectors)
                .child(pill);

            let ml = row_left_margin(row.group_depth);
            let guides = if !in_animated_tray {
                group_guide_lines(row.group_depth)
            } else {
                Vec::new()
            };

            return div()
                .relative()
                .children(guides)
                .when(hovered, |el| {
                    let this = cx.entity();
                    el.child(
                        canvas(
                            move |bounds, _window, cx| {
                                this.update(cx, |list: &mut ProjectList, _cx| {
                                    list.hover_rect = Some(bounds);
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                })
                .map(|el| {
                    if in_animated_tray {
                        el.child(row_el)
                    } else {
                        el.child(
                            div()
                                .ml(px(ml))
                                .mr(px(ROW_MARGIN_X))
                                .border_l_1()
                                .border_r_1()
                                .border_color(cluster_border)
                                .bg(cluster_bg)
                                .when(is_last_in_cluster, |el| {
                                    el.pb(px(3.0))
                                        .mb(px(3.0))
                                        .rounded_b(px(CLUSTER_RADIUS))
                                        .border_b_1()
                                })
                                .child(row_el),
                        )
                    }
                })
                .into_any_element();
        }

        // ── 独立项目 / 折叠父项目 / 展开微容器头部 ──────────────
        let Row {
            ref id,
            light,
            needs_attention,
            kind,
            is_child,
            group_depth,
            ..
        } = row;
        let name = row.name.clone();
        let description = row.description.clone();
        let ssh_connected = row.ssh_connected.clone();
        let ai_vendors = row.ai_vendors.clone();
        let worktree_branch = row.worktree_branch.clone();
        let remote = row.remote.clone();
        let is_expanded_parent =
            matches!(row.cluster_role, ProjectClusterRole::ExpandedParent { .. });
        // 箭头与胶囊状态认**真实折叠态**而非渲染外框态：收起动画最后一程时外框仍是
        // ExpandedParent（连着正在滑合的底抽屉），但箭头必须同步从 ▾ 转回 ▸
        let toggle_info = match row.cluster_role {
            ProjectClusterRole::ExpandedParent { child_count }
            | ProjectClusterRole::CollapsedParent { child_count }
                if !filtering =>
            {
                Some((child_count, self.collapsed_worktrees.contains(id)))
            }
            _ => None,
        };
        let hovered = self.hovered.as_deref() == Some(id.as_str());
        // 完成提示:非激活项目里有 AI 任务完成时画 DONE 标,否则才轮到状态灯
        let show_done_tag = shows_done_tag(needs_attention, is_active);
        let done_tag = self.done_tags.get(id.as_str());
        let editing = self
            .editing
            .as_ref()
            .filter(|e| !e.is_group && e.id == *id)
            .map(|e| e.input.clone());
        let show_more = hovered && editing.is_none() && !drag_active;

        // 左侧 2.5×14px 圆头语义微轨（绝对定位，不推挤图标与文字）
        let rail_color = if is_active {
            Some(ui::accent())
        } else if light == StatusLight::Error {
            Some(ui::color_error())
        } else if light == StatusLight::Attention {
            Some(ui::color_attention())
        } else if show_done_tag {
            Some(ui::color_success())
        } else {
            None
        };

        let row_el = self
            .project_row_shell(
                &row,
                is_active,
                is_focused && editing.is_none(),
                is_expanded_parent,
                show_done_tag,
                dragging,
                filtering,
                tree,
                cx,
            )
            .when_some(rail_color, |el, color| {
                el.child(
                    div()
                        .absolute()
                        .left(px(if is_expanded_parent { 2.0 } else { 2.5 }))
                        .top_0()
                        .bottom_0()
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .w(px(2.5))
                                .h(px(14.0))
                                .rounded_full()
                                .bg(color),
                        ),
                )
            })
            // 领位是**项目身份图标**,每行都有、项目名左缘严格垂直对齐
            .child(project_icon(kind, remote.clone()))
            .child(self.project_row_label(
                editing.as_ref(),
                name,
                description,
                is_active,
                query_lower,
            ))
            // 独立（已脱离父项目或单独加入）linked worktree 的矢量分支徽章
            .children(worktree_branch.map(|branch| worktree_badge_chip(&row.id, branch)))
            // 远程徽章:连接名(柔润 info 配色，断链时 error 配色)
            .children(remote.map(|remote| remote_badge_chip(&row.id, remote)))
            // 挂有 worktree 的父项目：可折叠计数胶囊 `[⑂ N ▾/▸]`
            .children(toggle_info.map(|(count, collapsed)| {
                self.worktree_toggle_chip(&row.id, count, collapsed, &row.collapsed_summary, cx)
            }))
            // 右侧状态区（AI 活跃胶囊 / 完成标 / 状态灯 / SSH 已连接灯）
            .children(row_right_status(
                id,
                &ai_vendors,
                AI_ICON_SIZE,
                show_done_tag,
                done_tag,
                light,
                ssh_connected.as_deref(),
            ))
            // 零抖动悬浮「···」操作胶囊（替代直接外露会引发整行左跳的 ✕）
            .when(show_more, |el| el.child(self.project_more_overlay(&row, tree, cx)));

        let ml = row_left_margin(group_depth);
        let guides = group_guide_lines(group_depth);

        div()
            .relative()
            .children(guides)
            // 被悬停的那一行挂一块 canvas 量矩形 —— 缩略图的锚点要它
            .when(hovered, |el| {
                let this = cx.entity();
                el.child(
                    canvas(
                        move |bounds, _window, cx| {
                            this.update(cx, |list: &mut ProjectList, _cx| {
                                list.hover_rect = Some(bounds);
                            });
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
            })
            .child(
                div()
                    .ml(px(ml))
                    .mr(px(ROW_MARGIN_X))
                    .when(is_expanded_parent, |el| {
                        el.mt(px(3.0))
                            .rounded_t(px(CLUSTER_RADIUS))
                            .border_t_1()
                            .border_l_1()
                            .border_r_1()
                            .border_color(cluster_border)
                            .bg(cluster_bg)
                    })
                    .when(!is_expanded_parent, |el| el.my(px(1.0)))
                    .child(row_el),
            )
            // 子项目不是落点,指示线自然也不该出现在它上下
            .when(!is_child && !filtering, |el| {
                el.children(self.drop_line(&row.id, DropPosition::Before, group_depth, drag_active))
                    .children(self.drop_line(
                        &row.id,
                        DropPosition::After,
                        group_depth,
                        drag_active,
                    ))
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 完成标只在「有完成待看 **且** 不是当前项目」时出现 —— 切过去看了就该没。
    /// 判据有两个读者(行渲染 + 进场表维护),这条钉住两边共用的那一个。
    #[test]
    fn 完成标只给非激活且有完成的项目() {
        assert!(shows_done_tag(true, false));
        assert!(!shows_done_tag(true, true), "正看着的项目不挂完成标");
        assert!(!shows_done_tag(false, false));
        assert!(!shows_done_tag(false, true));
    }

    /// 完成标的进出场**豁免**系统「减少动画」(2026-11-21 用户拍板:原版 `tagFadeIn` 被
    /// 通配规则压成瞬时,用户机器上系统动画关着,这条从来没播过),只服从「启用动画」总开关。
    #[test]
    fn 完成标进出场豁免减弱动效() {
        for spec in [mt_ui::motion::TAG_FADE_IN, DONE_TAG_OUT] {
            assert!(!spec.respects_reduce, "{spec:?}");
        }
        crate::motion::with_reduce(true, || {
            let tr = mt_ui::motion::Transition::new(mt_ui::motion::TAG_FADE_IN);
            if mt_ui::motion::animations_enabled() {
                assert!(tr.running(), "reduce 下照样播");
                assert!(tr.progress() < 1.0);
            }
        });
    }

    /// 完成标的形态:进场从 0 宽展开、宽度先到顶(60%)、内边距过冲后回落到 1;退场从起收
    /// 时露着的宽度线性收回 0。端点钉死 —— 残留零点几会让药丸永远缺一截或多一截。
    #[test]
    fn 完成标进出场的宽度与过冲() {
        let start = done_tag_frame(None, 0.0);
        assert_eq!(start.open, 0.0, "进场从 0 宽起");
        let peak = done_tag_frame(None, 0.6);
        assert!((peak.open - 1.0).abs() < 1e-6, "60% 处已全宽");
        assert!(peak.scale > 1.1, "过冲在内边距上:{}", peak.scale);
        let end = done_tag_frame(None, 1.0);
        assert_eq!(end.open, 1.0);
        assert!((end.scale - 1.0).abs() < 1e-6, "终点回到 1");
        // 宽度全程不回头
        let mut prev = -1.0;
        for i in 0..=20 {
            let open = done_tag_frame(None, i as f32 / 20.0).open;
            assert!(open >= prev - 1e-6, "在 {i} 处回头了");
            prev = open;
        }
        // 退场:从起收那一刻的宽度收,不跳到全宽再收
        assert_eq!(done_tag_frame(Some(0.4), 0.0).open, 0.4);
        assert!((done_tag_frame(Some(0.4), 0.5).open - 0.2).abs() < 1e-6);
        assert_eq!(done_tag_frame(Some(1.0), 1.0).open, 0.0);
        assert_eq!(done_tag_frame(Some(1.0), 0.3).scale, 1.0, "退场不过冲");
    }

    /// 退场途中又该出现:从眼下露着的宽度掉头接着展开,不从 0 宽重播;内边距停在 1。
    #[test]
    fn 完成标退场途中掉头从当前宽度接着展开() {
        // 收到一半(露着 0.5)时掉头
        let shown = done_tag_frame(Some(1.0), 0.5);
        let resumed = resumed_tag_frame(shown.open, 0.0);
        assert!(
            (resumed.open - shown.open).abs() < 1e-6,
            "掉头那一刻宽度不变"
        );
        assert_eq!(resumed.scale, shown.scale, "内边距也不跳");
        assert!((resumed_tag_frame(0.5, 0.5).open - 0.75).abs() < 1e-6);
        assert_eq!(resumed_tag_frame(0.5, 1.0).open, 1.0, "终点精确到全宽");
        assert_eq!(resumed_tag_frame(0.5, 1.0).scale, 1.0);
        // 掉头之后再收:从掉头途中露着的宽度收(`leave` 读的是 `frame()`)
        let mut tag = DoneTagAnim::enter();
        tag.resumed = Some(0.4);
        tag.transition = mt_ui::motion::Transition::settled(mt_ui::motion::TAG_FADE_IN);
        assert_eq!(tag.frame().open, 1.0, "掉头那一场播完就是全宽");
        tag.leave();
        assert_eq!(tag.leaving, Some(1.0));
        assert_eq!(tag.resumed, None);
    }

    /// 中栏收着期间变的完成标(本视图那时不渲染),再展开时直接对齐,不补播进出场;
    /// 平时照常播。
    #[test]
    fn 中栏收着期间变的完成标再展开时不补播() {
        let ids = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
        // 平时:新挂的播进场、不该挂的转退场
        let mut tags = HashMap::new();
        sync_tag_table(&mut tags, ids(&["a"]), false);
        assert!(tags.contains_key("a"));
        if mt_ui::motion::animations_enabled() {
            assert!(tags["a"].transition.running(), "新挂的播进场");
            sync_tag_table(&mut tags, ids(&[]), false);
            assert!(tags["a"].leaving.is_some(), "不该挂的转退场,收完才摘");
        }
        // 收着期间:a 不该挂了、b 新挂上 → 再画时 a 直接摘,b 直接到位
        let mut tags = HashMap::new();
        tags.insert("a".to_string(), DoneTagAnim::settled());
        sync_tag_table(&mut tags, ids(&["b"]), true);
        assert!(!tags.contains_key("a"), "早该没了的标不当面再收一次");
        assert!(!tags["b"].transition.running() && tags["b"].leaving.is_none());
        // 收起之前正在退场、再画时又该挂:直接到位,不接着收、也不掉头补播
        let mut leaving = DoneTagAnim::settled();
        leaving.leave();
        tags.insert("c".to_string(), leaving);
        sync_tag_table(&mut tags, ids(&["b", "c"]), true);
        assert!(tags["c"].leaving.is_none() && !tags["c"].transition.running());
        assert_eq!(tags["c"].frame().open, 1.0);
    }

    /// Project menu sequence and grouping are user-visible navigation contracts.
    #[test]
    fn 右键菜单项序与原版一致() {
        use ProjectMenuAction::*;
        assert_eq!(
            project_menu_actions(false),
            vec![
                Some(Rename),
                Some(EditDescription),
                Some(OpenInFolder),
                Some(CopyAbsolutePath),
                None,
                Some(AssociateSsh),
                Some(EnvVars),
                Some(Worktrees),
                Some(ProjectKind),
                Some(GroupSection),
                None,
                Some(Remove),
            ]
        );
    }

    #[test]
    fn 远程项目隐藏本地专属菜单项() {
        use ProjectMenuAction::*;
        assert_eq!(
            project_menu_actions(true),
            vec![
                Some(Rename),
                Some(EditDescription),
                Some(CopyAbsolutePath),
                None,
                Some(ToggleRemoteGit),
                Some(Worktrees),
                Some(GroupSection),
                None,
                Some(Remove),
            ]
        );
    }

    /// 远程徽章:本地项目没有;远程项目取连接名 + 摘要;连接被删 = 断链态。
    #[test]
    fn 远程徽章三态() {
        let conns = vec![mt_config::SshConnection {
            id: "c1".into(),
            name: "生产".into(),
            host: "h".into(),
            port: 2222,
            user: "root".into(),
            password: None,
            identity_file: None,
            group: None,
            use_system_ssh: false,
            extra: Default::default(),
        }];
        let mut p = project("p1", "/home/u/proj", None);
        assert!(remote_badge(&p, &conns).is_none(), "本地项目没有徽章");

        p.ssh_connection_id = Some("c1".into());
        let badge = remote_badge(&p, &conns).expect("远程项目要有徽章");
        assert_eq!(badge.name, "生产");
        assert_eq!(badge.summary, "root@h:2222");
        assert!(!badge.broken);

        p.ssh_connection_id = Some("gone".into());
        let broken = remote_badge(&p, &conns).expect("断链仍是远程项目");
        assert!(broken.broken);
        assert!(broken.name.is_empty(), "断链徽章画的是「断链」两字,不是连接名");
    }

    /// 服务器图标的顶点全在单位方框内(与边条图标同款体检)。
    #[test]
    fn 服务器图标在单位方框内() {
        let mut points = 0usize;
        for shape in SERVER {
            let (pts, _) = shape.geom.points();
            for (x, y) in pts {
                assert!(
                    (-0.001..=1.001).contains(&x) && (-0.001..=1.001).contains(&y),
                    "越界点 ({x}, {y})"
                );
                points += 1;
            }
        }
        assert!(points > 0);
        assert_eq!(SERVER.len(), 4, "两层机箱 + 两颗指示灯");
    }

    /// 领位徽标:手动指定压过探测,`'none'` 是「明确关掉」不回退。
    #[test]
    fn 领位徽标手动优先于探测() {
        // 没设过 → 用探测结果
        assert_eq!(
            resolve_project_kind(None, Some(ProjectKind::Rust)),
            Some(ProjectKind::Rust)
        );
        // 探测没就绪 / 认不出 → 通用图标
        assert_eq!(resolve_project_kind(None, None), None);
        // 手动指定压过探测
        assert_eq!(
            resolve_project_kind(Some("go"), Some(ProjectKind::Rust)),
            Some(ProjectKind::Go)
        );
        // 'none' = 明确关掉,**不回退到探测**
        assert_eq!(resolve_project_kind(Some("none"), Some(ProjectKind::Rust)), None);
        // 手改坏的覆盖值同样不回退(与原版 `?? ` 链一致:有值就不看探测)
        assert_eq!(
            resolve_project_kind(Some("莫名其妙的值"), Some(ProjectKind::Rust)),
            None
        );
    }

    /// 「自动识别」那一项在探测出结果时带**全角**括号(原版 `（Rust）`),
    /// 没探到就不带 —— 括号里空着比没有更糟。
    #[test]
    fn 自动识别项带探测结果括号() {
        let suffix = |detected: Option<ProjectKind>| {
            detected
                .map(|k| format!("（{}）", k.label()))
                .unwrap_or_default()
        };
        assert_eq!(suffix(Some(ProjectKind::Rust)), "（Rust）");
        assert_eq!(suffix(Some(ProjectKind::Node)), "（Node.js）");
        assert_eq!(suffix(None), "");
    }

    /// 勾选前缀是「✓ 」/ 全角空格 —— 两者等宽,菜单项文字才不会左右跳。
    #[test]
    fn 勾选前缀等宽() {
        assert_eq!(check_prefix(true), "✓ ");
        assert_eq!(check_prefix(false), "　");
        assert_ne!(check_prefix(true), check_prefix(false));
    }

    /// 「项目类型」子菜单:任何一份 `kindOverride` 取值下,
    /// **最多只有一项**被勾上(认不出的坏值一项都不勾)。
    #[test]
    fn 项目类型子菜单勾选唯一() {
        for current in [None, Some("none"), Some("rust"), Some("莫名其妙的值")] {
            let checked = std::iter::once(current.is_none())
                .chain(std::iter::once(current == Some("none")))
                .chain(ALL_PROJECT_KINDS.iter().map(|k| current == Some(k.as_str())))
                .filter(|c| *c)
                .count();
            // 认不出的值(手改坏了 config)一个都不勾 —— 与领位图标退回通用图标一致
            let expected = usize::from(current != Some("莫名其妙的值"));
            assert_eq!(checked, expected, "current={current:?}");
        }
    }

    /// 原版那 12 种一个不漏 —— 它们的取值**落在用户配置**里(`kindOverride`),
    /// 少一个,存量项目的手动指定就读不回来了。
    #[test]
    fn 项目类型子菜单列全集() {
        let keys: Vec<&str> = ALL_PROJECT_KINDS.iter().map(|k| k.as_str()).collect();
        for expected in [
            "java", "rust", "go", "python", "nodejs", "react", "vuejs", "nextjs", "svelte", "vite",
            "flutter", "php",
        ] {
            assert!(keys.contains(&expected), "少了 {expected}");
        }
        // 扩到五十多种是本次改造的目的;掉回十几种说明生成器的 CATALOG 被误删
        assert!(keys.len() >= 50, "只剩 {} 种", keys.len());
    }

    /// 菜单是「每个分组一个二级子菜单」,所以每个分组都得有货,
    /// 且每种类型必须**恰好**落进一个分组(不重不漏)。
    #[test]
    fn 项目类型按分组分完且无遗漏() {
        let mut covered = 0usize;
        for category in ALL_TECH_CATEGORIES {
            let n = ALL_PROJECT_KINDS
                .iter()
                .filter(|k| k.category() == *category)
                .count();
            assert!(n > 0, "{category:?} 分组是空的,菜单里会出现一个点不开的项");
            covered += n;
        }
        assert_eq!(
            covered,
            ALL_PROJECT_KINDS.len(),
            "有类型的分组不在菜单顺序表里,它会从菜单上消失"
        );
    }

    /// 技术栈枚举(mt-project)与徽标形状表(mt-ui)逐项对账。
    ///
    /// 两边出自 `gen_tech_icons.mjs` 的同一张 CATALOG,但分住两个 crate、按落盘字符串
    /// 衔接(`TechIcon::new(kind.as_str())`)——mt-ui 不依赖 mt-project,编译期没有穷尽
    /// 检查兜底。本 crate 是唯一同时看得见两边的地方:种类、字符串、顺序必须一一对上,
    /// 否则某个类型的领位徽标会画成空白。
    #[test]
    fn 每种技术栈都有徽标且与形状表逐项对上() {
        let kinds: Vec<&str> = ALL_PROJECT_KINDS.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            kinds,
            mt_ui::icons::TECH_ART_KINDS,
            "枚举与徽标表对不上:有人手改了生成物,改 CATALOG 后重跑生成器"
        );
    }

    // ─── 统一外边距、分组引导线与 AI 活跃判定 ────────────────

    /// 普通行 / 分组行 / Worktree 微容器共用 `row_left_margin`，
    /// 且第 g 层分组引导线恰好落在第 g 层分组折叠箭头水平中心，并与下层胶囊保持 4px 呼吸缝。
    #[test]
    fn 统一外边距与分组引导线精确对齐() {
        assert_eq!(row_left_margin(0), 6.0);
        assert_eq!(row_left_margin(1), 22.0);
        assert_eq!(row_left_margin(2), 38.0);
        assert_eq!(row_left_margin(3), 54.0);

        for level in 0..3usize {
            let guide_x = group_guide_x(level);
            // 对应层级分组的箭头中心：row_left_margin(level) + pl(6) + half_chevron(6)
            assert_eq!(guide_x, row_left_margin(level) + 12.0);
            // 下一层行左缘在引导线右侧留出 4px 呼吸缝
            assert_eq!(row_left_margin(level + 1) - guide_x, 4.0);
        }
    }

    #[test]
    fn 新增矢量图标均在单位方框内() {
        for (name, shapes) in [
            ("ICON_SEARCH", ICON_SEARCH),
            ("ICON_PLUS", ICON_PLUS),
            ("ICON_MORE", ICON_MORE),
            ("ICON_COLLAPSE_ALL", ICON_COLLAPSE_ALL),
            ("ICON_EXPAND_ALL", ICON_EXPAND_ALL),
            ("ICON_CLOSE_SM", ICON_CLOSE_SM),
        ] {
            let mut pts_count = 0usize;
            for shape in shapes {
                let (pts, _) = shape.geom.points();
                for (x, y) in pts {
                    assert!(
                        (-0.001..=1.001).contains(&x) && (-0.001..=1.001).contains(&y),
                        "{name} 越界点 ({x}, {y})"
                    );
                    pts_count += 1;
                }
            }
            assert!(pts_count > 0, "{name} 不应为空图形");
        }
    }

    #[test]
    fn ai活跃筛选命中工作待审批报错及未读完成() {
        assert!(!is_ai_active_project(StatusLight::Idle, false, false));
        assert!(!is_ai_active_project(StatusLight::AiIdle, false, false));
        assert!(is_ai_active_project(StatusLight::AiWorking, false, false));
        assert!(is_ai_active_project(StatusLight::Attention, false, false));
        assert!(is_ai_active_project(StatusLight::Error, false, false));
        assert!(is_ai_active_project(StatusLight::AiIdle, true, false));
        // 正看着的项目不算未读完成
        assert!(!is_ai_active_project(StatusLight::AiIdle, true, true));
    }

    // ─── AI 品牌堆叠(C.4) ───────────────────────────────────

    /// 按厂商去重、字母序、未知厂商排最后 —— 三条一次验完。
    #[test]
    fn ai堆叠去重并按字母序() {
        // 同款 AI 开三个 pane 只出一枚
        let stack = ai_vendor_stack([Some("claude"), Some("claude"), Some("claude")]);
        assert_eq!(stack, vec![Some(AiVendor::Claude)]);

        // 字母序:claude < codex(openai) ... 直接比 as_str
        let stack = ai_vendor_stack([Some("codex"), Some("claude"), Some("gemini")]);
        let keys: Vec<&str> = stack
            .iter()
            .map(|v| v.map(|v| v.as_str()).unwrap_or("unknown"))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted, "必须是厂商名的字母序");

        // 未知厂商固定排最后,且认不出的几个 agent 只占一枚
        let stack = ai_vendor_stack([
            Some("某个没听过的 agent"),
            Some("claude"),
            Some("另一个没听过的"),
        ]);
        assert_eq!(stack.len(), 2, "认不出的全算 unknown,合成一枚");
        assert_eq!(stack.first().copied(), Some(Some(AiVendor::Claude)));
        assert_eq!(stack.last().copied(), Some(None));

        // 一个 AI pane 都没有 = 一枚都不画(领位图标照旧)
        assert!(ai_vendor_stack(std::iter::empty()).is_empty());
    }

    /// 没有 agent 名(hook 没上报、输入检测也没认出)的 pane 归 unknown,不占两枚。
    #[test]
    fn ai堆叠里无名字的算unknown() {
        let stack = ai_vendor_stack([None, None, Some("claude")]);
        assert_eq!(stack, vec![Some(AiVendor::Claude), None]);
    }

    // ─── 失效 worktree 清理(C.5) ────────────────────────────

    fn project(id: &str, path: &str, parent: Option<&str>) -> ProjectConfig {
        ProjectConfig {
            parent_project_id: parent.map(str::to_string),
            ..ProjectConfig::new(id, id, path)
        }
    }

    /// 探测清单 = 子项目路径 + 父项目路径,**去重**;没有子项目就一条都不探。
    #[test]
    fn 探测清单含父项目且去重() {
        let projects = vec![
            project("root", r"D:\repo", None),
            project("wt1", r"D:\wt\a", Some("root")),
            project("wt2", r"D:\wt\b", Some("root")),
        ];
        let paths = collect_worktree_probe_paths(&projects);
        assert_eq!(paths, vec![r"D:\wt\a", r"D:\repo", r"D:\wt\b"]);

        // 没有 worktree 子项目 = 不必探测(原版 `probe.length === 0` 直接 return)
        assert!(collect_worktree_probe_paths(&[project("root", r"D:\repo", None)]).is_empty());
    }

    /// 只清「自己没了、父项目还在」的那些;整棵树一起消失时一个都不动。
    #[test]
    fn 只清父项目仍在的失效子项目() {
        let projects = vec![
            project("root", r"D:\repo", None),
            project("gone", r"D:\wt\gone", Some("root")),
            project("alive", r"D:\wt\alive", Some("root")),
        ];

        let alive: HashSet<String> = [r"D:\repo", r"D:\wt\alive"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(find_stale_worktree_projects(&projects, &alive), vec!["gone"]);

        // 盘符整个不见了(父项目也探不到)→ 一个都不清
        let none: HashSet<String> = HashSet::new();
        assert!(find_stale_worktree_projects(&projects, &none).is_empty());
    }

    /// UNC(WSL)路径两侧都不参与:存在性探测依赖 WSL 状态,误判风险高。
    #[test]
    fn unc路径不参与清理() {
        let projects = vec![
            project("root", r"\\wsl$\Ubuntu\repo", None),
            project("child", r"\\wsl$\Ubuntu\wt", Some("root")),
            project("local", r"D:\repo", None),
            // 本地子项目挂在 UNC 父项目下:父那侧不可靠,同样不清
            project("mixed", r"D:\wt\x", Some("root")),
        ];
        assert!(collect_worktree_probe_paths(&projects).is_empty());
        assert!(is_unc_path(r"\\wsl$\Ubuntu"));
        assert!(!is_unc_path(r"D:\repo"));
    }

    /// 父项目不存在(配置被手改坏)时不清 —— 判据缺一半,宁可留着。
    #[test]
    fn 父项目缺失时不清() {
        let projects = vec![project("orphan", r"D:\wt\x", Some("没有这个父项目"))];
        assert!(collect_worktree_probe_paths(&projects).is_empty());
        let alive: HashSet<String> = HashSet::new();
        assert!(find_stale_worktree_projects(&projects, &alive).is_empty());
    }

    #[test]
    fn worktree监听过滤只放行worktrees结构变化() {
        // 按本机分隔符拼路径：`D:\repo\.git` 在 Linux 上只是一个文件名，测不到父子关系
        let git_dir = PathBuf::from("repo").join(".git");
        let worktrees = git_dir.join("worktrees");
        let ev = |path: PathBuf, structural: bool| mt_project::watch::FsChange {
            project_path: git_dir.to_string_lossy().to_string(),
            path,
            kind: String::new(),
            structural,
        };
        // 首次建 `.git/worktrees` 与在里面建/删具体条目都命中
        assert!(is_worktree_watch_event(&ev(worktrees.clone(), true)));
        assert!(is_worktree_watch_event(&ev(worktrees.join("feat-a"), true)));
        // Windows 非递归监听因子目录内 index/HEAD 写入上报的 Modify（structural=false）必须丢弃
        assert!(!is_worktree_watch_event(&ev(
            worktrees.join("feat-a"),
            false
        )));
        assert!(!is_worktree_watch_event(&ev(worktrees.clone(), false)));
        // `.git` 下无关文件（index / HEAD / FETCH_HEAD）结构变化也忽略
        assert!(!is_worktree_watch_event(&ev(git_dir.join("HEAD"), true)));
        assert!(!is_worktree_watch_event(&ev(git_dir.join("index"), true)));
    }

    #[test]
    fn 自动发现worktree跳过已存在和手动移除项并去重() {
        let projects = vec![
            project("p1", r"D:\Git\repo", None),
            // 已存在（甚至已脱离父项目）的 worktree 不应重复添加
            project("existing-detached", r"d:/git/repo-old/", None),
        ];
        let dismissed = vec![crate::store::worktree_dismiss_key(
            None,
            r"D:\Git\repo-skip",
        )];
        let scanned = vec![
            (
                "p1".into(),
                mt_project::git::LocalRepoWorktrees {
                    git_dir: PathBuf::from(r"D:\Git\repo\.git"),
                    worktrees_dir_exists: true,
                    linked_paths: vec![
                        r"D:\Git\repo-old".into(),
                        r"D:\Git\repo-skip".into(),
                        r"D:\Git\repo-new".into(),
                        r"d:/git/repo-new/".into(),
                    ],
                    has_pending_invalid: false,
                },
            ),
            // 父项目在扫描期间被删掉 → 跳过
            (
                "gone-parent".into(),
                mt_project::git::LocalRepoWorktrees {
                    git_dir: PathBuf::from(r"D:\Git\gone\.git"),
                    worktrees_dir_exists: true,
                    linked_paths: vec![r"D:\Git\gone-wt".into()],
                    has_pending_invalid: false,
                },
            ),
        ];

        assert_eq!(
            plan_auto_added_worktrees(&projects, &dismissed, &scanned),
            vec![("p1".to_string(), r"D:\Git\repo-new".to_string())]
        );
    }

    // ─── 揭示 ───────────────────────────────────────────────

    fn tree_group(id: &str, collapsed: bool, children: Vec<ProjectTreeItem>) -> ProjectTreeItem {
        ProjectTreeItem::Group(mt_config::ProjectGroup {
            id: id.to_string(),
            name: id.to_string(),
            collapsed,
            children,
        })
    }

    fn tree_proj(id: &str) -> ProjectTreeItem {
        ProjectTreeItem::ProjectId(id.to_string())
    }

    /// 行号就是展平表里的位置:分组行也占一格(它在滚动容器里同样是一个子元素),
    /// 分组 id 本身不当揭示目标。
    #[test]
    fn 揭示行号计入分组行() {
        let ordered = vec![
            OrderedItem::Group {
                id: "g1".into(),
                name: "g1".into(),
                collapsed: false,
                count: 2,
                depth: 0,
                parent_group_id: None,
            },
            OrderedItem::Project {
                id: "p1".into(),
                depth: 1,
                parent_group_id: Some("g1".into()),
                is_child: false,
            },
            OrderedItem::Project {
                id: "p2".into(),
                depth: 1,
                parent_group_id: Some("g1".into()),
                is_child: false,
            },
        ];
        assert_eq!(reveal_row_index(&ordered, "p1"), Some(1));
        assert_eq!(reveal_row_index(&ordered, "p2"), Some(2));
        assert_eq!(reveal_row_index(&ordered, "g1"), None, "分组不是揭示目标");
        assert_eq!(reveal_row_index(&ordered, "查无此项"), None);
    }

    /// 端到端走一遍揭示的两步:折叠着时目标不在表里(`None`,不滚);按
    /// `collapsed_ancestors` 展开之后行号对得上 —— worktree 子项目紧跟父项目,
    /// 中间那层本来就展开的组也占一行。
    #[test]
    fn 揭示先展开折叠组再定行号() {
        let mut config = mt_config::AppConfig {
            projects: vec![
                project("外", r"D:\out", None),
                project("父", r"D:\repo", None),
                project("wt", r"D:\wt", Some("父")),
            ],
            project_tree: Some(vec![
                tree_proj("外"),
                tree_group(
                    "g1",
                    true,
                    vec![tree_group(
                        "g2",
                        false,
                        vec![tree_group("g3", true, vec![tree_proj("父")])],
                    )],
                ),
            ]),
            ..mt_config::AppConfig::default()
        };
        let before = project_tree::get_ordered_tree(&config);
        assert_eq!(
            reveal_row_index(&before, "wt"),
            None,
            "藏在折叠组里,这一帧不滚"
        );

        let groups = project_tree::collapsed_ancestors(&config, "wt");
        assert_eq!(groups, vec!["g1", "g3"]);
        let tree = config.project_tree.as_mut().expect("有树");
        for id in &groups {
            project_tree::find_group_in_tree_mut(tree, id)
                .expect("交出来的是树里的组")
                .collapsed = false;
        }
        // 外 / g1 / g2 / g3 / 父 / wt
        let after = project_tree::get_ordered_tree(&config);
        assert_eq!(reveal_row_index(&after, "父"), Some(4));
        assert_eq!(reveal_row_index(&after, "wt"), Some(5));
        assert_eq!(reveal_row_index(&after, "外"), Some(0));
    }

    // ─── B1 + C：worktree 子项文案、提示与树轨坐标 ────────────

    #[test]
    fn worktree子项默认分支优先_重命名后尊重自定义名() {
        // 默认目录名 + 探测到分支 → 主文直接显示分支名
        let d = resolve_child_display_label(
            "mini-term-plugin",
            r"D:\Git\mini-term-plugin",
            Some("mini-term"),
            Some("feat/plugin-system"),
            None,
        );
        assert_eq!(
            d,
            ChildDisplayLabel {
                primary: "feat/plugin-system".into(),
                secondary: None,
                primary_is_branch: true,
            }
        );

        // 用户手动 F2 重命名 → 自定义名作主文，分支名作浅色副文
        let renamed = resolve_child_display_label(
            "插件系统重构",
            r"D:\Git\mini-term-plugin",
            Some("mini-term"),
            Some("feat/plugin-system"),
            Some("一期"),
        );
        assert_eq!(
            renamed,
            ChildDisplayLabel {
                primary: "插件系统重构".into(),
                secondary: Some("feat/plugin-system · 一期".into()),
                primary_is_branch: false,
            }
        );

        // 游离 HEAD（无分支）→ 剥掉与父项目重复的前缀
        let detached = resolve_child_display_label(
            "mini-term-plugin-agents",
            r"D:\Git\mini-term-plugin-agents",
            Some("mini-term"),
            None,
            None,
        );
        assert_eq!(
            detached,
            ChildDisplayLabel {
                primary: "plugin-agents".into(),
                secondary: None,
                primary_is_branch: false,
            }
        );

        // 游离 HEAD 但目录名不含父项目前缀 → 原样显示
        let plain = resolve_child_display_label(
            "hotfix-v2",
            "/srv/worktrees/hotfix-v2",
            Some("mini-term"),
            None,
            None,
        );
        assert_eq!(plain.primary, "hotfix-v2");
    }

    #[test]
    fn worktree子项悬停提示交代目录与分支() {
        let display = resolve_child_display_label(
            "mini-term-plugin",
            r"D:\Git\mini-term-plugin",
            Some("mini-term"),
            Some("feat/plugin-system"),
            None,
        );
        assert_eq!(
            child_tooltip_text(
                "mini-term-plugin",
                r"D:\Git\mini-term-plugin",
                Some("feat/plugin-system"),
                &display
            ),
            "mini-term-plugin · feat/plugin-system\nD:\\Git\\mini-term-plugin"
        );
    }

    #[test]
    fn 树轨坐标对齐父项目领位图标水平中心() {
        // 微容器内父行 pl = ROW_INNER_PL(10) - CLUSTER_BORDER_W(1) = 9，14px 图标中心 = 9 + 7 = 16
        assert_eq!(cluster_rail_x(0), 16.0);
        // 嵌套孙项向右步进一个 CHILD_NEST_STEP
        assert_eq!(cluster_rail_x(1), 16.0 + CHILD_NEST_STEP);
    }
}
