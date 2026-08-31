//! 左栏:项目列表。对应 `src/components/ProjectList.tsx` 的主干。
//!
//! # 领位图标
//!
//! 原版是「SSH > 技术栈 > 通用」三选一,**恒显**(每行都有图标,缩进才对得齐)。
//! BB-b 补上第一档:SSH 远程项目固定画 [`SERVER`] 服务器图标(断链时转 error 色),
//! 其后才轮到技术栈徽标([`mt_ui::icons::TechIcon`])与通用目录图标。
//!
//! 技术栈取值走 [`resolve_project_kind`]:手动 `kindOverride` 优先,没设过就用
//! [`crate::project_kind`] 的目录探测缓存(结果住在 store 的 `dir_kinds`,
//! 探测本身丢后台)。
//!
//! # 键盘与悬停(清尾批)
//!
//! - 每行 `track_focus` + `tab_index`(原版的 `tabIndex={0}`),Enter/Space 打开、
//!   Delete 移除(带确认)、F2 重命名。**F2 不在行上绑** —— 它与全局键位表
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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::{
    AnyElement, App, AppContext, Bounds, ClipboardItem, Context, DragMoveEvent, Entity,
    ExternalPaths, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement,
    Pixels, Render, ScrollWheelEvent, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, Window, anchored, canvas, deferred, div,
    prelude::FluentBuilder, px,
};
use gpui_component::input::{Input, InputEvent, InputState, SelectAll};
use mt_config::{ProjectConfig, ProjectTreeItem};
use mt_ui::icons::vector::VectorIcon;
use mt_ui::icons::{
    ALL_PROJECT_KINDS, ALL_TECH_CATEGORIES, AiVendor, BrandIcon, FileIcon, ProjectKind, TechIcon,
};

use crate::dnd::{
    self, DragProjectItem, DropPosition, ExternalDropKind, PreviewIcon,
};
use crate::fs_ops;
use crate::i18n::{t, tr};
use crate::menu::{self, MenuEntry, MenuItem};
use crate::modal;
use crate::pane_preview::{self, MiniLayout};
use crate::project_tree::{self, MAX_DEPTH, OrderedItem};
use crate::store::AppStore;
use crate::tree::PaneStatus;
use crate::ui;

/// AI 品牌图标尺寸(`ProjectList.tsx:144` 的 `AI_ICON_SIZE`)。
const AI_ICON_SIZE: f32 = 14.0;

/// worktree 徽章最多显示多宽(原版 `max-w-[100px] truncate`)。
const WORKTREE_BADGE_MAX_W: f32 = 100.0;

/// 远程连接名徽章最多多宽(原版 `max-w-[80px] truncate`)。
const REMOTE_BADGE_MAX_W: f32 = 80.0;

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
        Some(kind) => TechIcon::new(kind).size(px(14.0)).into_any_element(),
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

/// 完成标(原版的 `<DoneTag/>`,样式在 `styles.css:509-524`)。
///
/// 实心 success 底 + **底色字**(不是白字:浅色主题下白字配浅绿看不见,
/// 与 StatusDot 的 `contrast` 同一条理由)、圆角 10px、字号 `0.77rem`≈10px、粗体。
/// 原版还有一层 success 色的外发光,gpui 的 div 没有 box-shadow,省掉。
///
/// # 进场(`tagFadeIn` 0.3s)
///
/// `progress` 由调用方喂(状态在 [`ProjectList::done_tags`] 里)。关键帧的
/// `scale(.6) → 1.15 → 1` 在 gpui 里没有 transform 可用,**只让水平内边距跟着
/// 缩放**:药丸自己横向呼吸 ±2px,字号与行高一动不动 —— 动它们会让整行在这
/// 300ms 里抖高度。这一档**不在** reduce 豁免名单里(`TAG_FADE_IN` 过闸),
/// 用户机器上开着「减少动画」就是直接出现。
fn done_tag(progress: f32) -> AnyElement {
    let (opacity, scale) = mt_ui::motion::tag_fade_in(progress);
    div()
        .flex_shrink_0()
        .px(px(8.0 * scale))
        .py(px(2.0))
        .rounded(px(10.0))
        .bg(ui::color_success())
        .text_size(ui::font_px(10.0))
        .font_weight(FontWeight::BOLD)
        .text_color(ui::bg_base())
        .opacity(opacity)
        .child(t("panels", "done"))
        .into_any_element()
}

/// 行上的 AI 品牌堆叠:领位图标之后、名字之前,**只追加不覆盖**。
/// 负边距抵掉行内 gap(6px),与领位图标只留 2px;图标之间同样 2px。
fn ai_vendor_icons(vendors: &[Option<AiVendor>]) -> gpui::Div {
    let mut stack = div()
        .flex()
        .items_center()
        .flex_shrink_0()
        .ml(px(-4.0))
        .gap(px(2.0));
    for vendor in vendors {
        stack = stack.child(
            BrandIcon::new(*vendor)
                .size(px(AI_ICON_SIZE))
                // 固定 text-secondary 上下文:单色品牌图标不随
                // 选中行的 accent 变色(与 tab 上观感一致)
                .color(ui::text_secondary()),
        );
    }
    stack
}

/// worktree 徽章:`⎇ 分支名`(U+2387 是**文本**,不是图标)。
fn worktree_badge_chip(id: &str, branch: String) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(format!("worktree-{id}")))
        .flex_shrink_0()
        .max_w(px(WORKTREE_BADGE_MAX_W))
        .truncate()
        .px(px(3.0))
        .rounded(px(3.0))
        .text_size(ui::font_px(9.75))
        .text_color(ui::text_muted())
        .bg(ui::border_subtle())
        .tooltip({
            let branch = branch.clone();
            move |window, cx| {
                mt_ui::tooltip::Tooltip::new(tr!(
                    "projectList",
                    "worktreeBadgeTitle",
                    branch = branch.clone()
                ))
                .build(window, cx)
            }
        })
        .child(format!("⎇ {branch}"))
}

/// 远程徽章:连接名(断链时「断链」两字 + error 配色)。
fn remote_badge_chip(id: &str, remote: RemoteBadge) -> gpui::Stateful<gpui::Div> {
    let (fg, bg) = if remote.broken {
        (ui::color_error(), ui::with_alpha(ui::color_error(), 0.15))
    } else {
        (ui::text_muted(), ui::border_subtle())
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
        .max_w(px(REMOTE_BADGE_MAX_W))
        .truncate()
        .px(px(3.0))
        .rounded(px(3.0))
        .font_family("monospace")
        .text_size(ui::font_px(9.75))
        .text_color(fg)
        .bg(bg)
        .tooltip(move |window, cx| {
            mt_ui::tooltip::Tooltip::new(tip.clone())
                .build(window, cx)
        })
        .child(if remote.broken {
            SharedString::from(t("projectList", "remoteBrokenBadge"))
        } else {
            SharedString::from(remote.name.clone())
        })
}

/// 完成标 / 状态灯二选一,**idle 且没有完成标时两个都不画**(原版 `ProjectList.tsx:912`)。
fn row_status_mark(show_done_tag: bool, done_tag_in: f32, status: PaneStatus) -> Option<AnyElement> {
    if show_done_tag {
        Some(done_tag(done_tag_in))
    } else if status != PaneStatus::Idle {
        Some(ui::status_dot(status).into_any_element())
    } else {
        None
    }
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

/// 项目行的左内边距。原版这两条公式**不能合并**(`ProjectList.tsx:660-666` 有
/// 踩坑记录):组内项目要对齐父级分组那个倒三角的位置;顶层项目及其 worktree
/// 子项目以 10px 为基准每层 +16 —— 共用组内公式会把顶层子项目的相对缩进压到 6px。
fn project_indent(depth: usize, in_group: bool) -> f32 {
    if in_group {
        depth.saturating_sub(1) as f32 * 16.0 + 16.0
    } else {
        10.0 + depth as f32 * 16.0
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

/// 一行要画的东西。渲染前先从 store 抠出来 —— `store.read(cx)` 的借用
/// 活不过 `cx.listener`,一行 6 个字段用元组已经读不清了。
#[derive(Clone)]
struct Row {
    id: String,
    name: String,
    path: String,
    status: PaneStatus,
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
    /// 递归含子组的项目数(行尾括号里那个数)。
    count: usize,
    depth: usize,
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
/// 本地专属入口一律隐藏:**资源管理器打开 / 关联 SSH / 环境变量 / Worktree 管理
/// / 项目类型**。理由照抄原版注释:agent 已在远程机、envVars 不注入远程 shell
/// (二期)、路径也不是本机可打开的位置、远程项目领位固定 SSH 图标。
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
                let path = row.path.clone();
                let id = row.id.clone();
                menu::item(t("projectList", "menu.worktrees"), move |window, cx| {
                    crate::git_worktree::open(
                        path.clone(),
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
                    let path = path.clone();
                    // spawn 外部进程会卡(网络盘 / 杀软),丢后台
                    cx.background_executor()
                        .spawn(async move {
                            if let Err(err) = fs_ops::reveal_in_file_manager(&path) {
                                eprintln!("[projects] 打开文件夹失败: {err}");
                            }
                        })
                        .detach();
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

/// 「新建分组」入口(列表标题栏空白右键)。底部那条 `+` 按钮是 C 批的事。
fn new_group_menu(store: &Entity<AppStore>) -> Vec<MenuEntry> {
    let store = store.clone();
    vec![menu::item(t("projectList", "newGroup"), move |window, cx| {
        let store = store.clone();
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
    })]
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

/// 悬停缩略图的开启态。`anchor` 是弹出那一刻行的屏幕矩形 ——
/// 与原版「到点时才 `getBoundingClientRect()`」同一时机(悬停这 250ms 里
/// 列表可能增删过行,进入那一刻的矩形已经不作数)。
struct RowPreview {
    project_id: String,
    anchor: Bounds<Pixels>,
    /// 卡片进场(`menuPopIn`)。状态挂在**这里**而不是卡上 —— 卡是纯函数,
    /// 而这一份的生命周期恰好就是「卡在不在」:收起时随 `RowPreview` 一起没,
    /// 下次悬停自然从头播。
    fade: mt_ui::motion::Transition,
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
    /// 鼠标停在哪一行 —— 行尾的 ✕ 只在 hover 时出现(原版 `hidden group-hover:inline`)。
    hovered: Option<String>,
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
    /// 悬停缩略图当前开在哪一行。
    preview: Option<RowPreview>,
    /// 250ms 计时 + 开着之后的 500ms 续活节拍。**丢掉句柄就等于 `clearTimeout`**。
    _preview_task: Option<Task<()>>,
    /// 正被悬停的那一行的屏幕矩形(只给被悬停的那一行挂 `canvas` 量)。
    hover_rect: Option<Bounds<Pixels>>,
    /// 刚进编辑态、还等着「默认全选」的那个输入框。见 [`Self::start_rename`]。
    pending_select_all: Option<FocusHandle>,
    /// 各行完成标的进场(`tagFadeIn`),按项目 id 索引。
    ///
    /// 生命周期照抄 DOM:**标出现时建、消失时丢** —— 丢掉之后再出现按「新挂载」
    /// 处理,从头播一遍。表在 `render` 的前置段维护(`render_project` 拿的是
    /// `&self`,建不了)。
    done_tags: HashMap<String, mt_ui::motion::Transition>,
}

impl ProjectList {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |this: &mut Self, _, cx| {
            // 项目路径集合变了(增删项目 / worktree 变项目)→ 重探徽章
            this.probe_worktrees(false, cx);
            // 技术栈探测(原版 `useProjectKinds` 那个 effect):列表变了就补探,
            // 缓存失效(`removeDirKind`)后同样由这条重跑补上
            this.ensure_project_kinds(cx);
            // 窗口重新聚焦:分支切换与 `git worktree remove` 都发生在窗外,
            // 回来时既重探徽章也做一次失效清理(原版 `onFocusChanged` 同款)
            let focused = this.store.read(cx).window_focused();
            if focused && !this.was_focused {
                this.probe_worktrees(true, cx);
                this.reconcile_worktrees(cx);
            }
            this.was_focused = focused;
            cx.notify();
        })
        .detach();
        let mut this = Self {
            store,
            dragging: None,
            drop_indicator: None,
            external: None,
            editing: None,
            hovered: None,
            worktree_branches: HashMap::new(),
            probe_key: String::new(),
            kinds_key: String::new(),
            was_focused: true,
            row_focus: HashMap::new(),
            preview: None,
            _preview_task: None,
            hover_rect: None,
            pending_select_all: None,
            done_tags: HashMap::new(),
            };
        // 挂载时先探一次(原版两个 effect 都在挂载时跑一遍)
        this.probe_worktrees(true, cx);
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
        // 去重闸,与 [`Self::probe_worktrees`] 同款:这个方法挂在 store 观察者上,
        // 每次 notify 都会走一遍(AI 状态跳一下就有一次),此前每次都要把全部
        // 项目路径克隆成一个 `Vec<String>` 再喂给一个只会全部命中缓存的去重表。
        // 先只拼一条比较用的键,确定有新东西要探了才真去收集路径。
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
            _ => {}
        }
    }

    /// 分组行按键(原版只有 Enter/Space 折叠 + F2 改名,**没有 Delete**——
    /// 删组是不可逆的结构变更,只留在右键菜单里并带确认)。
    fn on_group_key(&mut self, event: &KeyDownEvent, id: &str, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return;
        }
        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
            cx.stop_propagation();
            self.store
                .update(cx, |store, cx| store.toggle_group_collapse(id, cx));
        }
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

    /// 收起浮层并取消在飞的计时(原版 `closePreview`)。
    fn close_preview(&mut self, cx: &mut Context<Self>) {
        self._preview_task = None;
        if self.preview.take().is_some() {
            cx.notify();
        }
    }

    /// 进入某一行:排一次 250ms 的计时。
    ///
    /// **AI 判定放在到点时而非进入时**(原版专门记了这条):这 250ms 里 AI 完全
    /// 可能刚起来,读当下的 store 才拿得到最新一份。矩形同理到点再取。
    fn schedule_preview(&mut self, project_id: String, cx: &mut Context<Self>) {
        self._preview_task = None;
        self.preview = None;
        self._preview_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(pane_preview::HOVER_DELAY_MS))
                .await;
            if this
                .update(cx, |this: &mut ProjectList, cx| this.fire_preview(&project_id, cx))
                .unwrap_or(false)
            {
                // 开着期间按原版的 500ms 节拍重画 —— 缩略图是活的。
                // MiniTerminalElement 自己也按这个节拍取数,更密的重画只命中缓存
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(mt_ui::MINI_REFRESH_MS))
                        .await;
                    let alive = this
                        .update(cx, |this: &mut ProjectList, cx| {
                            if this.preview.is_some() {
                                cx.notify();
                                true
                            } else {
                                false
                            }
                        })
                        .unwrap_or(false);
                    if !alive {
                        return;
                    }
                }
            }
        }));
    }

    /// 计时到点:三道闸(还悬着同一行 / 量到过矩形 / 项目里真有 AI 会话)。
    fn fire_preview(&mut self, project_id: &str, cx: &mut Context<Self>) -> bool {
        if self.hovered.as_deref() != Some(project_id) {
            return false;
        }
        let Some(anchor) = self.hover_rect else {
            return false;
        };
        let store = self.store.read(cx);
        let auto_resume = store.config().ai_auto_resume.unwrap_or(true);
        let layout = store.project_state(project_id).and_then(|s| s.active_layout());
        if !pane_preview::has_ai_pane(layout, auto_resume) {
            return false;
        }
        self.preview = Some(RowPreview {
            project_id: project_id.to_string(),
            anchor,
            fade: mt_ui::motion::Transition::new(mt_ui::motion::MENU_IN),
        });
        cx.notify();
        true
    }

    /// 组装浮层。渲染处**每帧重判开闸**(原版的双闸模式):AI 退出后不光不画,
    /// 状态本身也收掉 —— 留着的话同一次悬停里 AI 再起来会拿**旧锚点**复活。
    fn render_preview(&mut self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let preview = self.preview.as_ref()?;
        // 进场进度先取(后面 `self.preview = None` 那两条早退路径会把它拿走)
        let fade = preview.fade.drive(window);
        let store = self.store.read(cx);
        let auto_resume = store.config().ai_auto_resume.unwrap_or(true);
        let Some(project) = store.project(&preview.project_id) else {
            self.preview = None;
            return None;
        };
        let (name, path) = (project.name.clone(), project.path.clone());
        let layout = store
            .project_state(&preview.project_id)
            .and_then(|s| s.active_layout());
        if !pane_preview::has_ai_pane(layout, auto_resume) {
            self.preview = None;
            return None;
        }
        let mini: Option<MiniLayout> = layout.and_then(|l| {
            pane_preview::snapshot_layout(l, &preview.project_id, store, auto_resume, cx)
        });
        let style = pane_preview::preview_style(store);
        let at = pane_preview::project_anchor(preview.anchor);
        Some(
            deferred(
                anchored()
                    .position(at)
                    // 原版那两句 `Math.min/max` 的贴边收拢在 gpui 里是白拿的
                    .snap_to_window_with_margin(px(8.0))
                    .child(pane_preview::project_preview_card(
                        &name,
                        &path,
                        mini.as_ref(),
                        &style,
                        fade,
                    )),
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
        // 这个方法挂在 store 观察者上、每次 notify 都会走一遍(AI 状态变化就有一次),
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
                cx.notify();
            }
            return;
        };
        if dragged.id == row_id {
            // 拖到自己身上不给任何指示(原版 `handleMouseMoveOver` 开头那道 return)
            if self.drop_indicator.as_ref().is_some_and(|d| d.id == row_id) {
                self.drop_indicator = None;
                cx.notify();
            }
            return;
        }

        let position = dnd::drop_position(ratio, allow_inside);
        let forbidden = {
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
            match dragged_item {
                None => false,
                Some(item) => match position {
                    DropPosition::Inside => !project_tree::can_drop(tree, row_id, &item),
                    // before/after 只有拖「组」才可能超深(项目恒 0 层)
                    _ if dragged.is_group => !project_tree::can_drop_at(tree, row_id, &item),
                    _ => false,
                },
            }
        };

        let next = DropIndicator {
            id: row_id.to_string(),
            position,
            forbidden,
        };
        if self.drop_indicator.as_ref() != Some(&next) {
            self.drop_indicator = Some(next);
            cx.notify();
        }
    }

    /// `on_drop` 落地。位置来自上一次 `on_drag_move` 存下的 indicator ——
    /// gpui 的 `on_drop` 不带坐标,这是硬约束。
    fn on_row_drop(&mut self, dragged: &DragProjectItem, row_id: &str, cx: &mut Context<Self>) {
        let indicator = self.drop_indicator.take();
        self.dragging = None;
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

    /// 外部文件落地。语义照抄 `ProjectList.tsx:295-319`:
    /// 逐个加,**新增过任何一个就只落盘不切换**;一个没新增但撞上已有项目 → 切过去。
    fn on_external_drop(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.external = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let dirs = cx
                .background_executor()
                .spawn(async move { mt_project::fs::filter_directories(paths) })
                .await;
            if dirs.is_empty() {
                return;
            }
            let _ = this.update(cx, |this: &mut ProjectList, cx| {
                this.store.update(cx, |store, cx| {
                    let mut added_any = false;
                    let mut existing_id: Option<String> = None;
                    for dir in &dirs {
                        let path_str = dir.to_string_lossy().to_string();
                        if let Some(existing) = store.find_project_by_path(&path_str) {
                            existing_id = Some(existing.id.clone());
                            continue;
                        }
                        store.add_project_at(dir, None, cx);
                        added_any = true;
                    }
                    if !added_any && let Some(id) = existing_id {
                        store.set_active_project(&id, cx);
                    }
                });
            });
        })
        .detach();
    }

    /// 落点指示线。2px accent 横线,before 贴上沿、after 贴下沿;
    /// **非法落点不画线**(原版 `renderDropLine` 遇 forbidden 直接 return null)。
    fn drop_line(&self, id: &str, position: DropPosition, active: bool) -> Option<AnyElement> {
        let indicator = self.drop_indicator.as_ref()?;
        if !active || indicator.id != id || indicator.position != position || indicator.forbidden {
            return None;
        }
        Some(
            div()
                .absolute()
                .left(px(4.0))
                .right(px(4.0))
                .h(px(2.0))
                .rounded_full()
                .bg(ui::accent())
                .map(|el| match position {
                    DropPosition::Before => el.top(px(-1.0)),
                    _ => el.bottom(px(-1.0)),
                })
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
        // 拖拽中断(松手在窗外 / 被别人吃掉)后 gpui 会清 active_drag 并重画:
        // 借这一帧把残留的 view state 一并清掉(**不 notify** —— 正在渲染,
        // 再触发一次重画就是死循环)。高亮另外还与 `drag_active` 与门,
        // 保证即使这次没轮到重画也不会画出过期的指示。
        let drag_active = cx.has_active_drag();
        if !drag_active {
            self.dragging = None;
            self.drop_indicator = None;
            // 这一份不清的话,下次拖同一批路径会命中缓存、沿用过期的三态结论
            self.external = None;
        }
        let dragging = self.dragging.clone();
        let external = self.external.as_ref().map(|e| e.kind);

        let ordered = {
            let store = self.store.read(cx);
            project_tree::get_ordered_tree(store.config())
        };
        self.sync_row_focus(&ordered, cx);
        self.sync_done_tags(&ordered, window, cx);
        let preview = self.render_preview(window, cx);
        let list = self.render_rows(ordered, dragging.as_deref(), drag_active, cx);
        let footer = self.render_footer(cx);
        let header = self.render_list_header(cx);

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
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.on_external_drop(paths.paths().to_vec(), cx);
            }))
            .child(header)
            .child(list)
            .child(footer)
            // 悬停缩略图。`deferred(priority 1)` 画在所有常规内容之上,
            // 卡本身不带 `.id()` → 无 hitbox → 等价原版的 pointer-events-none
            .children(preview)
            // 三态提示框:盖住整栏,`pointer-events` 不用管 —— gpui 的 drop 分发
            // 按 hitbox 命中走,这层没有 `.id()` 也就没有 hitbox
            .children(external.map(external_drop_hint))
    }
}

impl ProjectList {
    fn render_group(
        &self,
        row: GroupRow,
        dragging: Option<&str>,
        drag_active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let GroupRow {
            id,
            name,
            collapsed,
            count,
            depth,
        } = row.clone();
        let inside_target = drag_active
            && self
                .drop_indicator
                .as_ref()
                .is_some_and(|d| d.id == id && d.position == DropPosition::Inside);
        let forbidden =
            inside_target && self.drop_indicator.as_ref().is_some_and(|d| d.forbidden);
        let is_source = dragging == Some(id.as_str());

        let id_toggle = id.clone();
        let id_move = id.clone();
        let id_drop = id.clone();
        let id_drag = id.clone();
        let id_key = id.clone();
        let id_focus = id.clone();
        let name_drag = name.clone();
        let row_menu = row.clone();
        let this = cx.entity();
        let focus = self.row_focus.get(id.as_str()).cloned();
        // 编辑中的那一行:名字换成内联输入框,行本身的点击/按键让给它
        let editing = self
            .editing
            .as_ref()
            .filter(|e| e.is_group && e.id == id)
            .map(|e| e.input.clone());

        div()
            .relative()
            .child(
                div()
                    .id(SharedString::from(format!("group-{id}")))
                    // 分组行同样可 Tab 可聚焦(原版 `tabIndex={0}` + `role=treeitem`)
                    .when_some(focus, |el, focus| el.track_focus(&focus).tab_index(0))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _window, cx| {
                        this.on_group_key(event, &id_key, cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event: &MouseDownEvent, window, cx| {
                            this.close_preview(cx);
                            if this.editing.is_none()
                                && let Some(focus) = this.row_focus.get(id_focus.as_str())
                            {
                                window.focus(focus);
                            }
                        }),
                    )
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(depth as f32 * 16.0))
                    .pr(px(10.0))
                    .py(px(6.0))
                    .rounded(px(3.0))
                    .cursor_pointer()
                    .text_size(ui::font_px(11.4))
                    .text_color(ui::text_muted())
                    .when(is_source, |el| el.opacity(0.4))
                    .when(forbidden, |el| {
                        el.border_1().border_dashed().border_color(ui::color_error())
                    })
                    .when(inside_target && !forbidden, |el| {
                        el.bg(ui::accent_subtle())
                            .border_1()
                            .border_dashed()
                            .border_color(ui::accent())
                    })
                    .when(!inside_target, |el| {
                        el.hover(|el| el.bg(ui::border_subtle()).text_color(ui::text_primary()))
                    })
                    .on_drag(
                        DragProjectItem {
                            id: id_drag.clone(),
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
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        // 编辑态里点自己不该顺带折叠(原版行的 onKeyDown 整体 return
                        // 是同一个道理:这一行的交互全让给输入框)
                        if this.editing.is_some() {
                            return;
                        }
                        this.store.update(cx, |store, cx| {
                            store.toggle_group_collapse(&id_toggle, cx)
                        });
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
                    // 折叠箭头。原版是一个恒定的 ▾ 加 `rotate(-90deg)`,
                    // gpui 的 div 没有 transform —— 直接换字形,视觉等价
                    .child(
                        div()
                            .w(px(12.0))
                            .flex_shrink_0()
                            .text_size(ui::font_px(9.75))
                            .child(if collapsed { "▸" } else { "▾" }),
                    )
                    // 「分组 = 空间」:容器图标,着主题文件夹色
                    .child(
                        VectorIcon::new(dnd::BOXES_SHAPES, px(13.0))
                            .ink(ui::color_folder()),
                    )
                    .map(|el| match &editing {
                        // 组名输入框比项目行小一档(原版 `text-sm` vs `text-base`)
                        Some(input) => el.child(self.rename_input(input, 11.4)),
                        None => el.child(
                            div()
                                .flex_1()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .child(name),
                        ),
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(ui::font_px(9.75))
                            .text_color(ui::text_muted())
                            .child(format!("({count})")),
                    ),
            )
            .children(self.drop_line(&id_drag, DropPosition::Before, drag_active))
            .children(self.drop_line(&id_drag, DropPosition::After, drag_active))
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

    /// 完成标的进场表:这一帧哪些行挂着标就留哪些(等价于 DOM 的挂载/卸载)。
    /// 进度在 `render_project` 里读,**请求下一帧只能在这儿**(那边没有 window)。
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
        self.done_tags.retain(|id, _| showing.contains(id));
        for id in showing {
            self.done_tags
                .entry(id)
                .or_insert_with(|| mt_ui::motion::Transition::new(mt_ui::motion::TAG_FADE_IN));
        }
        if self.done_tags.values().any(|tr| tr.running()) {
            window.request_animation_frame();
        }
    }

    /// 列表本体:`get_ordered_tree` 展平出来的分组行 / 项目行逐条铺开。
    fn render_rows(
        &self,
        ordered: Vec<OrderedItem>,
        dragging: Option<&str>,
        drag_active: bool,
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
        let mut list = div()
            .id("project-list-rows")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll()
            // 列表一滚锚点就失效 —— 原版挂 window 的 scroll/wheel 直接关掉
            .on_scroll_wheel(cx.listener(|this, _event: &ScrollWheelEvent, _window, cx| {
                this.close_preview(cx);
            }));
        for item in ordered {
            match item {
                OrderedItem::Group {
                    id,
                    name,
                    collapsed,
                    count,
                    depth,
                    ..
                } => {
                    let row = GroupRow {
                        id,
                        name,
                        collapsed,
                        count,
                        depth,
                    };
                    list = list.child(self.render_group(row, dragging, drag_active, cx));
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
                    let row = Row {
                        id: p.id.clone(),
                        name: p.name.clone(),
                        path: p.path.clone(),
                        status: state.map(|s| s.status).unwrap_or(PaneStatus::Idle),
                        needs_attention: state.map(|s| s.needs_attention).unwrap_or(false),
                        kind: resolve_project_kind(p.kind_override.as_deref(), detected_kind),
                        detected_kind,
                        description: p.description.clone(),
                        kind_override: p.kind_override.clone(),
                        depth,
                        parent_group_id,
                        is_child,
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
                    list = list.child(self.render_project(
                        row,
                        is_active,
                        dragging,
                        drag_active,
                        &tree_snapshot,
                        cx,
                    ));
                }
            }
        }

        list
    }

    /// 栏头:只有 "PROJECTS" 文本 + 空白右键菜单(原版没有 `+` 按钮)。
    fn render_list_header(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        div()
            .id("project-list-header")
            .flex()
            .items_center()
            .justify_between()
            .px(px(10.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            // 标题栏空白右键 = 新建分组(原版 `ProjectList.tsx:1069-1074`)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let entries = new_group_menu(&this.store);
                    menu::show(event.position, entries, window, cx);
                }),
            )
            // 原版头部只有 "PROJECTS" 文本 + 空白右键菜单,没有 `+` 按钮
            // (添加项目在底部按钮条)
            .child(
                div()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .child(t("panels", "projects")),
            )
    }

    /// 底部按钮条。
    fn render_footer(&self, cx: &mut Context<Self>) -> gpui::Div {
        // 底部按钮条(`ProjectList.tsx:1087-1113`):添加项目 / SSH / +。
        // 中间那颗 `SSH` 走 `remote_project::open`(根层),BB-b 补齐。
        let dashed_button = |id: &'static str, label: SharedString, wide: bool| {
            div()
                .id(id)
                .when(wide, |el| el.flex_1())
                .flex()
                .items_center()
                .justify_center()
                .px(px(12.0))
                .py(px(8.0))
                .rounded(px(6.0))
                .border_1()
                .border_dashed()
                .border_color(ui::border_default())
                .cursor_pointer()
                .text_size(ui::font_px(11.4))
                .text_color(ui::text_muted())
                .hover(|el| el.border_color(ui::accent()).text_color(ui::accent()))
                .child(label)
        };
        let store_for_add = self.store.clone();
        div()
            .flex()
            .flex_none()
            .gap(px(6.0))
            .p(px(8.0))
            .child(
                dashed_button(
                    "add-project",
                    t("projectList", "addProject").into(),
                    true,
                )
                .on_click(move |_event, window, cx| {
                    modal::open_add_project(store_for_add.clone(), window, cx);
                }),
            )
            .child(
                dashed_button("add-remote-project", "SSH".into(), false)
                    .tooltip(|window, cx| {
                        mt_ui::tooltip::Tooltip::new(t(
                            "projectList",
                            "addRemoteProject",
                        ))
                        .build(window, cx)
                    })
                    .on_click(cx.listener(|this, _event, window, cx| {
                        crate::remote_project::open(this.store.clone(), None, window, cx);
                    })),
            )
            .child(
                dashed_button("new-group", "+".into(), false)
                    .tooltip(|window, cx| {
                        mt_ui::tooltip::Tooltip::new(t("projectList", "newGroup"))
                            .build(window, cx)
                    })
                    .on_click(cx.listener(|this, _event, window, cx| {
                        let store = this.store.clone();
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
                    })),
            )
    }

    /// 项目行的名字位:编辑态换成内联输入框,否则是「名字 + 描述」。
    fn project_row_label(
        &self,
        editing: Option<&Entity<InputState>>,
        name: String,
        description: Option<String>,
    ) -> AnyElement {
        match editing {
            Some(input) => self.rename_input(input, 13.0),
            None => {
                div()
                    .flex_1()
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    // 原版**没有**副行显示路径:路径只在 title / 预览卡头里出现
                    .child(div().truncate().child(name))
                    .when_some(description, |el, desc| {
                        el.child(
                            div()
                                .truncate()
                                .text_size(ui::font_px(9.75))
                                .text_color(ui::text_muted())
                                .child(desc),
                        )
                    })
                .into_any_element()
            }
        }
    }

    /// 行尾的移除按钮:弹确认框(不可逆,布局与展开目录一起没)。
    fn project_remove_button(
        &self,
        id: &str,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id_remove = id.to_string();
        div()
            .id(SharedString::from(format!("project-remove-{id}")))
            .w(px(16.0))
            .h(px(16.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.0))
            .text_size(ui::font_px(11.4))
            .text_color(ui::text_muted())
            .hover(|el| el.text_color(ui::color_error()).bg(ui::bg_overlay()))
            .on_click(cx.listener(move |this, _event, window, cx| {
                cx.stop_propagation();
                let Some((name, path)) = this
                    .store
                    .read(cx)
                    .project(&id_remove)
                    .map(|p| (p.name.clone(), p.path.clone()))
                else {
                    return;
                };
                modal::open_confirm_remove_project(
                    this.store.clone(),
                    id_remove.clone(),
                    name,
                    path,
                    window,
                    cx,
                );
            }))
            .child("✕")
    }

    /// 项目行的**交互挂载**:焦点 / 键盘 / 悬停 / 拖放 / 点击 / 右键菜单 + 行样式。
    /// 行里的内容(图标、名字、徽章、状态灯……)由 [`Self::render_project`] 往里塞。
    fn project_row_shell(
        &self,
        row: &Row,
        is_active: bool,
        dragging: Option<&str>,
        tree: &[ProjectTreeItem],
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id = row.id.clone();
        let path = row.path.clone();
        let kind = row.kind;
        let is_child = row.is_child;
        let indent = project_indent(row.depth, row.parent_group_id.is_some());
        let is_source = dragging == Some(id.as_str());
        let id_click = id.clone();
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
        // 编辑中的那一行:行本身的点击/按键让给输入框(输入框本体在 `project_row_label`)
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
            .flex()
            .items_center()
            .gap(px(6.0))
            .pl(px(indent))
            .pr(px(10.0))
            .py(px(6.0))
            .rounded(px(3.0))
            .cursor_pointer()
            .text_size(ui::font_px(13.0))
            .when(is_source, |el| el.opacity(0.4))
            .when(is_active, |el| {
                el.bg(ui::accent_subtle()).text_color(ui::accent())
            })
            .when(!is_active, |el| {
                el.text_color(ui::text_secondary())
                    .hover(|el| el.bg(ui::border_subtle()).text_color(ui::text_primary()))
            })
            // 绝对路径挂 tooltip。原版是 `title={aiVendors.length>0 ? undefined
            // : project.path}` —— 有 AI 会话时路径改由缩略图卡头显示,
            // 原生 tooltip 会盖住那张卡。这里同款条件挂
            .when(row.ai_vendors.is_empty(), |el| {
                el.tooltip({
                    let path = path.clone();
                    move |window, cx| {
                        mt_ui::tooltip::Tooltip::new(path.clone())
                            .build(window, cx)
                    }
                })
            })
            // 悬停记到 view state 上 —— 行尾 ✕ 的显隐与缩略图计时都要它。
            // ⚠️ 离开分支必须先核对「离开的正是我们记着的那一行」:相邻
            // 行的 enter/leave 到达顺序不保证,直接清会把刚进来的那一行
            // 抹掉(鼠标沿列表纵扫时预览再也弹不出来)
            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                let mine = this.hovered.as_deref() == Some(id_hover.as_str());
                if *hovered {
                    if mine {
                        return;
                    }
                    this.hovered = Some(id_hover.clone());
                    this.schedule_preview(id_hover.clone(), cx);
                } else {
                    if !mine {
                        return;
                    }
                    this.hovered = None;
                    // 移出即关(原版 onMouseLeave 的 closePreview)
                    this.close_preview(cx);
                }
                cx.notify();
            }))
            .on_drag(
                DragProjectItem {
                    id: id.clone(),
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
                let id_move = id_move.clone();
                let id_drop = id_drop.clone();
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
            // 按下即收缩略图(原版 onMouseDown 的第一句),顺带把行焦点
            // 收过来 —— 浏览器点 `tabIndex=0` 的元素就会聚焦,原版的
            // 「点完项目按 Delete 能删」正是靠这一条
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event: &MouseDownEvent, window, cx| {
                    this.close_preview(cx);
                    if this.editing.is_none()
                        && let Some(focus) = this.row_focus.get(id_focus.as_str())
                    {
                        window.focus(focus);
                    }
                }),
            )
            .on_click(cx.listener(move |this, _event, _window, cx| {
                // 编辑态里点自己不切项目(交互全让给输入框)
                if this.editing.is_some() {
                    return;
                }
                this.store
                    .update(cx, |store, cx| store.set_active_project(&id_click, cx));
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
            // 右键菜单(`ProjectList.tsx` 的 onContextMenu),开菜单前先
            // 收掉悬停缩略图(原版第一句就是 `closePreview()`)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.close_preview(cx);
                    let entries = project_menu(
                        &cx.entity(),
                        &this.store,
                        &row_for_menu,
                        &tree_for_menu,
                    );
                    menu::show(event.position, entries, window, cx);
                }),
            )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_project(
        &self,
        row: Row,
        is_active: bool,
        dragging: Option<&str>,
        drag_active: bool,
        tree: &[ProjectTreeItem],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Row {
            ref id,
            status,
            needs_attention,
            kind,
            is_child,
            ..
        } = row;
        let name = row.name.clone();
        let description = row.description.clone();
        let ai_vendors = row.ai_vendors.clone();
        let worktree_branch = row.worktree_branch.clone();
        let remote = row.remote.clone();
        // 行尾的 ✕ 只在**这一行**被悬停时出现(原版 `hidden group-hover:inline`)。
        // 走 view state 而不是 `group_hover` + 透明度:透明的按钮仍然吃点击,
        // 而这个按钮的动作是「移除项目」,看不见还能点中是实打实的事故。
        let hovered = self.hovered.as_deref() == Some(id.as_str());
        // 完成提示:非激活项目里有 AI 任务完成时画 DONE 标,否则才轮到状态灯;
        // **idle 且没有完成标时两个都不画**(原版 `ProjectList.tsx:912`)
        let show_done_tag = shows_done_tag(needs_attention, is_active);
        // 进场进度。表由 `render` 前置段维护;万一没赶上(理论上不会)按已就位画
        let done_tag_in = self
            .done_tags
            .get(id.as_str())
            .map(|tr| tr.progress())
            .unwrap_or(1.0);
        let editing = self
            .editing
            .as_ref()
            .filter(|e| !e.is_group && e.id == *id)
            .map(|e| e.input.clone());

        let row_el = self
            .project_row_shell(&row, is_active, dragging, tree, cx)
            // 行首那道 accent 竖条(原版 `w-0.5 h-4 rounded-full`)。
            // ⚠️ 原版只在选中时才渲染这个 span,于是选中的一瞬整行内容右移
            // 10px;这里**恒占位**、未选中时透明,视觉一致但不抖。
            .child(
                div()
                    .w(px(2.0))
                    .h(px(16.0))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(if is_active {
                        ui::accent()
                    } else {
                        ui::with_alpha(ui::accent(), 0.0)
                    }),
            )
            // 领位是**项目身份图标**,每行都有、缩进才对得齐
            // (SSH 远程 > 技术栈 > 通用,原版同序)
            .child(project_icon(kind, remote.clone()))
            // AI 品牌堆叠:领位图标之后、名字之前,**只追加不覆盖**。
            // 负边距抵掉行内 gap(6px),与领位图标只留 2px;图标之间同样 2px
            .children((!ai_vendors.is_empty()).then(|| ai_vendor_icons(&ai_vendors)))
            .child(self.project_row_label(editing.as_ref(), name, description))
            // worktree 徽章:`⎇ 分支名`(U+2387 是**文本**,不是图标)
            .children(worktree_branch.map(|branch| worktree_badge_chip(&row.id, branch)))
            // 远程徽章:连接名(断链时「断链」两字 + error 配色)。
            // 位置照原版 —— worktree 徽章之后、完成标/状态灯之前
            .children(remote.map(|remote| remote_badge_chip(&row.id, remote)))
            // 完成标 / 状态灯二选一,**idle 时两个都不画**
            .children(row_status_mark(show_done_tag, done_tag_in, status))
            // 移除:弹确认框(不可逆,布局与展开目录一起没)。只在行悬停时出现
            .children(hovered.then(|| self.project_remove_button(&row.id, cx)));

        div()
            .relative()
            // 被悬停的那一行挂一块 canvas 量矩形 —— 缩略图的锚点要它。
            // **只给悬停中的那一行挂**:每行常驻一块的话,一屏几十个项目就是
            // 几十个白量的元素
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
            .child(row_el)
            // 子项目不是落点,指示线自然也不该出现在它上下
            .when(!is_child, |el| {
                el.children(self.drop_line(&row.id, DropPosition::Before, drag_active))
                    .children(self.drop_line(&row.id, DropPosition::After, drag_active))
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

    /// 完成标的进场**不在** reduce 豁免名单里(原版 `tagFadeIn` 被通配规则
    /// 压成瞬时),开着减弱动效时第一帧就是终态。
    #[test]
    fn 完成标进场过减弱动效的闸() {
        let spec = mt_ui::motion::TAG_FADE_IN;
        assert!(spec.respects_reduce);
        crate::motion::with_reduce(true, || {
            let tr = mt_ui::motion::Transition::new(spec);
            assert_eq!(tr.progress(), 1.0);
            assert!(!tr.running(), "reduce 下一帧都不该请求");
        });
    }

    /// 菜单项序照抄原版。
    ///
    /// ⚠️ 逐批同步的那条断言(V/Y 批口径):Y 批接上「Worktree 管理」;
    /// BB-b 接上「关联 SSH / 环境变量」,并把分组段从 `ProjectKind` 的实现里
    /// 提成显式位标 `GroupSection`(远程项目没有项目类型那一项,分组段得另有
    /// 落点)。仍未建的只剩「WSL 会话」子菜单,不占位。
    #[test]
    fn 右键菜单项序与原版一致() {
        use ProjectMenuAction::*;
        let actions = project_menu_actions(false);
        assert_eq!(
            actions,
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
        assert_eq!(actions.iter().filter(|a| a.is_none()).count(), 2);
    }

    /// 远程项目的 gate(原版 `isRemote ? [] : [...]`):本地专属入口一律隐藏,
    /// 保留重命名 / 编辑描述 / 复制绝对路径 / 分组段 / 移除。
    #[test]
    fn 远程项目隐藏本地专属菜单项() {
        use ProjectMenuAction::*;
        let actions = project_menu_actions(true);
        assert_eq!(
            actions,
            vec![
                Some(Rename),
                Some(EditDescription),
                Some(CopyAbsolutePath),
                Some(GroupSection),
                None,
                Some(Remove),
            ]
        );
        for hidden in [OpenInFolder, AssociateSsh, EnvVars, Worktrees, ProjectKind] {
            assert!(
                !actions.contains(&Some(hidden)),
                "远程项目不该出现 {hidden:?}"
            );
        }
        // 「复制绝对路径」必须留着 —— 远程 POSIX 路径照样要能复制走
        assert!(actions.contains(&Some(CopyAbsolutePath)));
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

    // ─── 缩进(`ProjectList.tsx:660-666` 的两条公式) ─────────

    /// 两条公式不能合并:组内项目对齐父级分组的倒三角区域,
    /// 顶层项目及其 worktree 子项目以 10px 为基准每层 +16。
    #[test]
    fn 项目缩进两条公式各走各的() {
        // 顶层项目
        assert_eq!(project_indent(0, false), 10.0);
        // 顶层项目的 worktree 子项目:10 + 16
        assert_eq!(project_indent(1, false), 26.0);
        // 一级分组里的项目
        assert_eq!(project_indent(1, true), 16.0);
        // 二级分组里的项目
        assert_eq!(project_indent(2, true), 32.0);
        // 组内项目的 worktree 子项目
        assert_eq!(project_indent(3, true), 48.0);
    }

    /// 组内公式**不能**拿来算顶层:那会把顶层 worktree 子项目的相对缩进压到 6px。
    #[test]
    fn 组内公式不适用于顶层() {
        let 顶层子项目 = project_indent(1, false);
        let 若误用组内公式 = project_indent(1, true);
        assert_eq!(顶层子项目 - project_indent(0, false), 16.0);
        assert_eq!(若误用组内公式 - project_indent(0, false), 6.0);
    }

    /// 分组行缩进就是 `depth * 16`(B.2),与项目行的两条公式都不同。
    #[test]
    fn 分组行缩进按层数() {
        for depth in 0..3usize {
            assert_eq!(depth as f32 * 16.0, (depth * 16) as f32);
        }
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
            id: id.to_string(),
            name: id.to_string(),
            path: path.to_string(),
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
}
