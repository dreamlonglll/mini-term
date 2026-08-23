//! 终端区:SplitNode 树 → 嵌套 resizable + 每个叶子一条 tab 栏。
//!
//! 对应 `src/components/TerminalArea.tsx` + `SplitLayout.tsx` + `PaneGroup.tsx`。
//!
//! - split 节点 → [`gpui_component::resizable`](gpui_component::resizable)
//!   的 `h_resizable` / `v_resizable`(替 Allotment),每个节点一份
//!   `ResizableState`,按节点 id 缓存,拖动后把比例写回 store 并落盘;
//! - leaf 节点 → tab 栏 + 当前激活 pane 的 [`crate::pane::TerminalPane`] 实体。
//!   同一个叶子里的多个 pane 就是「终端标签」,与旧版一致(项目级 tab 层早已删除)。
//!
//! # 分屏比例的跨重启恢复
//!
//! `ResizablePanel` 只吃**像素**初值(`ResizableState` 内部一律按像素算,百分比
//! 没有入口),而布局树与磁盘格式存的是百分比。于是渲染时自上而下带一个「本节点
//! 可用尺寸」参数,逐层按百分比换算成像素喂给 `.size()`:
//!
//! ```text
//! 终端区 bounds(canvas 量出来,跨帧保留)
//!   └─ Split(h, [30,70]) → 子 0 宽 = 可用宽 × 0.30,子 1 宽 = 可用宽 × 0.70
//!        └─ Split(v, [50,50]) → 各自再按自己那块可用高度分
//! ```
//!
//! 初值只在该节点的 `ResizableState` 第一次落地时起作用;用户拖过之后
//! `panel.size` 变成 `Some`,我们喂的初值自动让位,不会与拖动打架。
//!
//! 正因为「只认第一帧」,**首帧必须已经量到真实尺寸**:canvas 是在本帧 prepaint
//! 才回填 `area_size` 的,元素树早在那之前就构造完了。拿兜底尺寸铺出去的话,
//! `ResizableState` 会把按 1200×800 算出来的像素当成自己的初值锁死,窗口比它宽时
//! 多出来的空间被各面板**等分**(每个 panel 都是 `flex_grow: 1`),20/80 的分屏就
//! 会恢复成 35/65。于是首帧只放量尺的 canvas,下一帧再铺分屏树 —— 代价是一帧空白。

use std::collections::HashMap;

use gpui::{
    AnyElement, App, AppContext, Bounds, ClickEvent, Context, Entity, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement,
    Pixels, Render, SharedString, Size, StatefulInteractiveElement, Styled, Task, Window, anchored,
    canvas, deferred, div, point, prelude::FluentBuilder, px,
};
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use mt_ui::tooltip::Tooltip;
use mt_ui::icons::{AiVendor, BrandIcon, Geom, Ink, Shape, VectorIcon};

use crate::branch_family;
use crate::focus_nav::{self, Direction, PaneRect};
use crate::i18n::{t, tr};
use crate::markers;
use crate::menu::{self, MenuEntry, MenuItem, hotkey_label};
use crate::modal;
use crate::overlay;
use crate::pane_actions;
use crate::pane_preview;
use crate::session_branch::{BranchMenuSegment, branch_menu_segment};
use crate::store::AppStore;
use crate::tree::{DropZone, PaneStatus, SplitDirection, SplitNode};
use crate::ui;

/// 终端区还没量出尺寸时的兜底(首帧)。比例照样对,只是绝对值不准。
const FALLBACK_AREA: Size<Pixels> = Size {
    width: px(1200.0),
    height: px(800.0),
};

pub struct TerminalArea {
    store: Entity<AppStore>,
    /// 每个 split 节点一份分隔条状态(跨帧保留,否则每帧都重置回均分)。
    split_states: HashMap<String, Entity<ResizableState>>,
    /// 终端区自身的可用尺寸(canvas 量出来,用于把百分比换算成像素初值)。
    area_size: Size<Pixels>,
    /// 是否已经量到过真实尺寸。没量到之前不铺分屏树(见模块注释)。
    measured: bool,
    /// 每个 pane 在屏幕上的矩形 —— 方向导航按几何最近邻挑目标。
    pane_rects: HashMap<String, PaneRect>,
    /// AI 任务标记浮层开在哪个 pane 上(`None` = 没开)。存 `(pane_id, pty_id)`:
    /// 前者用来实现原版那条「activePane 的 ptyId 一变就无条件关」。
    marker_open: Option<(String, u32)>,
    /// 浮层的焦点句柄。收着焦点才有人接 Esc(与 `menu.rs` 同一套路)。
    marker_focus: FocusHandle,
    /// 开浮层之前焦点在谁身上,关的时候还回去。
    ///
    /// 不还的话焦点停在已经不画了的浮层上,用户接着敲的字全部落空,还得先用鼠标
    /// 点一下终端才能继续 —— 与 `pane.rs::dismiss_search` 那句 `window.focus` 同一条红线
    /// (原版这个浮层压根不收焦点,所以没有这个问题)。
    marker_prev_focus: Option<FocusHandle>,
    /// 正拖着文件悬停在哪个 pane 上(文件树的 `DragFilePath` 与系统的
    /// `ExternalPaths` 共用)。`on_drop` 不带位置,高亮只能从这里来。
    file_drop_pane: Option<String>,
    /// 每个叶子的进场动画(`.pane-enter`),按 `项目\u{1}叶子` 索引。
    ///
    /// 键里带项目 id 是为了**切项目不重播**:原版切项目是 `display:none`
    /// 留着不卸载,CSS 动画自然不会重来(`PaneGroup.tsx:391` 的注释原文)。
    /// 这张表也照此**不按帧回收** —— 跑完的条目只剩一个 `Instant`,与
    /// `split_states` 同属「关掉的项目会留下几十字节」那一档,不值得为它
    /// 每帧去遍历全部项目的布局树。
    pane_enter: HashMap<String, mt_ui::motion::Transition>,
    /// 每个 tab 一个焦点句柄(原版 tab 上的 `tabIndex` + `role="tab"`)。
    /// 拿到焦点后 Enter/Space 才能激活它。
    tab_focus: HashMap<String, FocusHandle>,
    /// 鼠标停在哪个 tab 上(缩略图计时要它)。
    hovered_tab: Option<String>,
    /// 每个 tab 的屏幕矩形(每个 tab 挂一片只量不画的 `canvas`)。
    ///
    /// 两个消费方:① 悬停缩略图的锚点;② tab 栏拖拽插入位要 tab 中线
    /// (原版是 `bar.querySelectorAll('[data-pane-tab]')` 逐个 `getBoundingClientRect`,
    /// 这张表就是它的等价物)。
    tab_rects: HashMap<String, Bounds<Pixels>>,
    /// 缩略图开在哪个 tab 上 + 弹出那一刻的矩形 + 卡片进场(`menuPopIn`)。
    /// 进场状态与这一份同生共死:收起时一起没,下次悬停从头播。
    tab_preview: Option<(String, Bounds<Pixels>, mt_ui::motion::Transition)>,
    /// 250ms 计时 + 开着之后的 500ms 续活节拍。**丢掉句柄等于 `clearTimeout`**。
    _tab_preview_task: Option<Task<()>>,

    // ─── pane 拖拽(v0.14.0 / 原版 PR #49)───────────────────────
    //
    // 三份状态都只在拖拽期间有值,`render` 开头与 `cx.has_active_drag()` 对账后
    // 统一清 —— 与 `file_drop_pane` 同一套(拖拽被中断时 gpui 会自己清 active_drag
    // 并重画,残留状态自动失效,不必到处补清理)。
    /// 正被拖着的 pane id —— 源 tab 变淡靠它(原版 `el.style.opacity = '0.4'`)。
    pane_drag: Option<String>,
    /// 终端区落点预览:`(leaf_id, 档位)`。**`on_drop` 不带位置,这是唯一通道**。
    pane_drop: Option<(String, DropZone)>,
    /// tab 栏插入位:`(leaf_id, 插入下标, 指示线相对 tab 栏左缘的 x)`。
    tab_drop: Option<(String, usize, f32)>,
}

/// 控件簇里 marker 按钮**右缘**到叶子右边缘的距离(最大化钮不在场时)。
///
/// 簇是 `.gap(2).px(6)` 后跟四个 22×22 的方钮(终端内查找 / 分屏右 / 分屏下 /
/// 关整组,与原版 `PaneGroup.tsx:489-541` 同序),marker 按钮排在它们之前,
/// 自己还带 4px 右外边距(原版的 `mr-1`)。查找按钮与 marker 按钮同以
/// 「pane 有 pty」为前提 —— 凡是 marker 浮层要用这个锚点的场景四钮必然齐。
/// 原版是 `getBoundingClientRect()` 量出来的,这里由布局常量算 ——
/// 加减控件时**必须同步改这个常量**,有单测钉着组成。
const MARKER_ANCHOR_INSET: f32 =
    CTRL_CLUSTER_PAD + 4.0 * (CTRL_BTN + CTRL_GAP) + MARKER_BTN_MARGIN_RIGHT;
const CTRL_CLUSTER_PAD: f32 = 6.0;
const CTRL_BTN: f32 = 22.0;
const CTRL_GAP: f32 = 2.0;
const MARKER_BTN_MARGIN_RIGHT: f32 = 4.0;

/// marker 锚点的实际内缩。**最大化钮是条件出现的**(只有真分了屏才画,
/// `PaneGroup.tsx:686` 的 `layoutIsSplit || isMaximized`),在场时簇里就是五个
/// 方钮而不是四个,锚点要多让出一颗的宽度 —— 原版量 DOM 天然不会错,这边
/// 靠常量算就必须显式分档。
fn marker_anchor_inset(has_maximize: bool) -> f32 {
    if has_maximize {
        MARKER_ANCHOR_INSET + CTRL_BTN + CTRL_GAP
    } else {
        MARKER_ANCHOR_INSET
    }
}

/// tab 栏高度 —— 折叠标题条与它等高(折叠条就是「只剩标题栏的那一格」,
/// 高度对不上会让最大化前后的视线落点跳一下)。
const TAB_BAR_H: f32 = 26.0;

/// 折叠标题条区最多吃掉终端区多高。叶子多到码不下时那一区自己滚,
/// **绝不挤掉铺满的那一格** —— 最大化的本意就是给它腾地方。
const COLLAPSED_ZONE_MAX: f32 = 0.4;

// ─── 控件簇的描边图标(照抄 `PaneGroup.tsx:40-62` 的 SVG,viewBox 16 归一化;
//     自绘理由见 `mt_ui::icons::vector` 模块注释,换算套路同 `activity_bar`)───

/// 单位方框换算:原版 viewBox 是 `0 0 16 16`,除以 16 即可。
const fn cu(v: f32) -> f32 {
    v / 16.0
}
/// 原版这组图标统一 `stroke-width="1.3"`。
const CTRL_STROKE: f32 = 1.3 / 16.0;

/// 终端内查找。原版 `ICON_SEARCH`:`<circle cx="7" cy="7" r="4.2"/>` +
/// `<path d="M10.2 10.2L14 14"/>`。
const ICON_SEARCH: &[Shape] = &[
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Circle {
            c: (cu(7.0), cu(7.0)),
            r: cu(4.2),
        },
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(10.2), cu(10.2)), (cu(14.0), cu(14.0))]),
    ),
];

/// 向右分屏。原版 `ICON_SPLIT_RIGHT`:
/// `<rect x="2" y="3" width="12" height="10" rx="1.5"/>` + `<path d="M8 3v10"/>`。
const ICON_SPLIT_RIGHT: &[Shape] = &[
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Rect {
            x: cu(2.0),
            y: cu(3.0),
            w: cu(12.0),
            h: cu(10.0),
            round: cu(1.5),
        },
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(8.0), cu(3.0)), (cu(8.0), cu(13.0))]),
    ),
];

/// 向下分屏。原版 `ICON_SPLIT_DOWN`:同一只外框 + `<path d="M2 8h12"/>`。
const ICON_SPLIT_DOWN: &[Shape] = &[
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Rect {
            x: cu(2.0),
            y: cu(3.0),
            w: cu(12.0),
            h: cu(10.0),
            round: cu(1.5),
        },
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(2.0), cu(8.0)), (cu(14.0), cu(8.0))]),
    ),
];

/// 最大化。原版 `ICON_MAXIMIZE`:`M9.5 2.5h4v4` / `M13.5 2.5L9 7` /
/// `M6.5 13.5h-4v-4` / `M2.5 13.5L7 9`(右上、左下两只往外指的角标)。
const ICON_MAXIMIZE: &[Shape] = &[
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(9.5), cu(2.5)), (cu(13.5), cu(2.5)), (cu(13.5), cu(6.5))]),
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(13.5), cu(2.5)), (cu(9.0), cu(7.0))]),
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(6.5), cu(13.5)), (cu(2.5), cu(13.5)), (cu(2.5), cu(9.5))]),
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(2.5), cu(13.5)), (cu(7.0), cu(9.0))]),
    ),
];

/// 还原。原版 `ICON_RESTORE`:同一组角标翻过来往内指。
const ICON_RESTORE: &[Shape] = &[
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(13.5), cu(6.5)), (cu(9.5), cu(6.5)), (cu(9.5), cu(2.5))]),
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(9.5), cu(6.5)), (cu(14.0), cu(2.0))]),
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(2.5), cu(9.5)), (cu(6.5), cu(9.5)), (cu(6.5), cu(13.5))]),
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(6.5), cu(9.5)), (cu(2.0), cu(14.0))]),
    ),
];

/// 图标在 22×22 方钮里的边长(原版 SVG 是 13×13)。
const CTRL_ICON: f32 = 13.0;

/// tab 栏插入指示线的宽度。原版最初是 2px 细线,评审实测「肉眼难辨」后
/// 换成 **3px 圆头 + accent 双层光晕**(`0 0 6px` + `0 0 2px`),这里一步到位
/// 抄加强版,不复刻中间那一版。
const TAB_DROP_LINE_W: f32 = 3.0;

/// 浮层宽度。原版是 `min-w-[280px]` + 内容撑开,gpui 侧要给正文列一个确定宽度
/// 才truncate得动,取固定值 —— 差别只在「超长正文时原版会更宽」。
///
/// ⚠️ 下面这三个尺寸都是**13px 基准下的值**,用的时候一律过
/// [`ui::font_px`] 换算 —— 原版是 `rem`,跟着根字号走;这边写死 `px` 的话,
/// 用户把 `uiFontSize` 调大之后列就装不下内容了(时间列 `15:29` 会折成两行)。
const MARKER_PANEL_WIDTH: f32 = 300.0;
/// 列表最大高度(`MarkerList.tsx:30` 的 `max-h-80` = 20rem)。
const MARKER_PANEL_MAX_HEIGHT: f32 = 320.0;
/// 正文截断字数(`MarkerList.tsx:16` 的 `truncate(s, 40)`)。
const MARKER_LINE_MAX: usize = 40;
/// `#seq` 列宽(`MarkerList.tsx:41` 的 `w-8`)。
const MARKER_SEQ_W: f32 = 32.0;
/// 时间列宽(`MarkerList.tsx:42` 的 `w-10`)。**别再往下调**:`HH:mm` 是五个
/// 字符,收窄到装不下就会折行 —— 两列都用 `min_w` 而不是 `w`,宽度算漏时
/// 宁可把列撑开也别把字折了。
const MARKER_TIME_W: f32 = 40.0;

/// 各子节点占主轴的比例(和为 1)。
///
/// 存的百分比与子节点数对不上(旧配置 / 塌陷过一次)时**均分**,与
/// `src/utils/layoutOps.ts` 里「子节点数变化后均分而不是按旧值截断」同一处置。
fn split_fractions(sizes: &[f64], count: usize) -> Vec<f64> {
    if count == 0 {
        return Vec::new();
    }
    let usable = sizes.len() == count && sizes.iter().all(|s| s.is_finite() && *s > 0.0);
    if !usable {
        return vec![1.0 / count as f64; count];
    }
    let total: f64 = sizes.iter().sum();
    sizes.iter().map(|s| s / total).collect()
}

/// 分屏进场这一帧的不透明度。**纯函数**,单测钉在这上面。
///
/// 只有淡入没有缩放,理由见 [`TerminalArea::wrap_pane_enter`]。
fn pane_enter_opacity(progress: f32) -> f32 {
    progress.clamp(0.0, 1.0)
}

/// 分隔条拖完后的像素 → 百分比(和为 100,与磁盘格式同口径)。
///
/// 总和非正(面板还没量出来 / 全被折叠)时返回 `None`,调用方据此**不写回** ——
/// 把一串 0 写进布局树会让下次恢复全部退化成均分。
fn sizes_to_percent(pixels: &[f64]) -> Option<Vec<f64>> {
    let total: f64 = pixels.iter().filter(|p| p.is_finite()).sum();
    if !(total > 0.0) {
        return None;
    }
    Some(pixels.iter().map(|p| p / total * 100.0).collect())
}

// ─── tab 右键菜单 ─────────────────────────────────────────────

/// tab 右键菜单的**项序**。`None` = 分隔线。
///
/// 对照 `PaneGroup.tsx:336-383`,逐项照抄(含分支那一段的条件出现)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabMenuAction {
    Rename,
    SplitRight,
    SplitDown,
    /// 「分支会话到新分屏」。有会话身份 + 该 agent 有 fork 能力位时才出。
    ForkSession,
    /// 「查看会话分支」—— 悬停展开家族面板,与上一项同进同出。
    ViewSessionBranches,
    /// 「分支会话(未获会话身份…)」置灰提示。与上面两项**互斥**:
    /// 前者要有身份,它要的恰恰是没身份。
    ForkNeedsIdentity,
    CloseTab,
    ClosePane,
}

/// 项序按分支段的两种形态展开。两个入参互斥(有身份就不会缺身份),
/// 判据在 [`branch_menu_segment`] 一处。
fn tab_menu_actions(can_fork: bool, identity_missing: bool) -> Vec<Option<TabMenuAction>> {
    use TabMenuAction::*;
    let mut actions = vec![
        Some(Rename),
        None,
        Some(SplitRight),
        Some(SplitDown),
    ];
    if can_fork {
        actions.extend([None, Some(ForkSession), Some(ViewSessionBranches)]);
    }
    if identity_missing {
        actions.extend([None, Some(ForkNeedsIdentity)]);
    }
    actions.extend([None, Some(CloseTab), Some(ClosePane)]);
    actions
}

/// 组装一个 tab 的右键菜单。`label` 是它当前的显示名(重命名的默认值)。
/// `pub(crate)`:终端列表竖条([`crate::terminals_panel`])的行右键与 tab 右键
/// **必须同一份菜单** —— 同一个 pane 在两处的操作面不该有差异。
pub(crate) fn tab_menu(
    store: &Entity<AppStore>,
    project_id: &str,
    pane_id: &str,
    label: &str,
    cx: &App,
) -> Vec<MenuEntry> {
    let pane_state = store
        .read(cx)
        .project_state(project_id)
        .and_then(|s| s.pane(pane_id));
    let segment = pane_state
        .map(|p| branch_menu_segment(p.ai_session.as_ref(), p.detected_agent.as_deref()))
        .unwrap_or(BranchMenuSegment::None);
    let project_path = store
        .read(cx)
        .project(project_id)
        .map(|p| p.path.clone())
        .unwrap_or_default();
    let fork_session_id = match &segment {
        BranchMenuSegment::Fork { session_id, .. } => session_id.clone(),
        _ => String::new(),
    };

    let mut entries = Vec::new();
    for action in tab_menu_actions(
        matches!(segment, BranchMenuSegment::Fork { .. }),
        segment == BranchMenuSegment::NeedsIdentity,
    ) {
        let Some(action) = action else {
            entries.push(menu::separator());
            continue;
        };
        let store = store.clone();
        let pid = project_id.to_string();
        let pane = pane_id.to_string();
        entries.push(match action {
            TabMenuAction::Rename => {
                let label = label.to_string();
                MenuItem::new(t("paneGroup", "rename"))
                    // 键位表见 main.rs 的 KeyBinding(F2 = RenamePane)
                    .shortcut(hotkey_label(false, false, false, "F2"))
                    .on_click(move |window, cx| {
                        // 复用既有的重命名对话框(双击 tab 走的也是它)
                        modal::open_rename_pane(
                            store.clone(),
                            pid.clone(),
                            pane.clone(),
                            label.clone(),
                            window,
                            cx,
                        );
                    })
                    .into()
            }
            TabMenuAction::SplitRight => MenuItem::new(t("paneGroup", "splitRight"))
                .shortcut(hotkey_label(true, true, false, "D"))
                .on_click(move |window, cx| {
                    store.update(cx, |store, cx| {
                        store.split_pane(&pid, &pane, SplitDirection::Horizontal, window, cx);
                    });
                })
                .into(),
            TabMenuAction::SplitDown => MenuItem::new(t("paneGroup", "splitDown"))
                .shortcut(hotkey_label(true, true, false, "E"))
                .on_click(move |window, cx| {
                    store.update(cx, |store, cx| {
                        store.split_pane(&pid, &pane, SplitDirection::Vertical, window, cx);
                    });
                })
                .into(),
            // 分支三项:出不出由 `segment` 决定(项序表已经据此排好),
            // 内容与终端本体右键**同一份实现**(`branch_family` 里那三个构造器)
            TabMenuAction::ForkSession => branch_family::fork_menu_item(&store, pid, pane),
            TabMenuAction::ViewSessionBranches => branch_family::view_branches_menu_item(
                &store,
                project_path.clone(),
                fork_session_id.clone(),
            ),
            TabMenuAction::ForkNeedsIdentity => branch_family::needs_identity_menu_item(),
            // 关闭两项都走 pane_actions —— 与 tab 上的 ×、Ctrl+Shift+W 同一个
            // AI 感知确认入口
            TabMenuAction::CloseTab => MenuItem::new(t("paneGroup", "closeTab"))
                .on_click(move |window, cx| {
                    pane_actions::close_pane(store.clone(), pid.clone(), pane.clone(), window, cx);
                })
                .into(),
            TabMenuAction::ClosePane => MenuItem::new(t("paneGroup", "closePane"))
                .danger()
                .shortcut(hotkey_label(true, true, false, "W"))
                .on_click(move |window, cx| {
                    pane_actions::close_leaf_of_pane(
                        store.clone(),
                        pid.clone(),
                        pane.clone(),
                        window,
                        cx,
                    );
                })
                .into(),
        });
    }
    entries
}

impl TerminalArea {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            split_states: HashMap::new(),
            area_size: FALLBACK_AREA,
            measured: false,
            pane_rects: HashMap::new(),
            marker_open: None,
            marker_focus: cx.focus_handle(),
            marker_prev_focus: None,
            file_drop_pane: None,
            pane_enter: HashMap::new(),
            tab_focus: HashMap::new(),
            hovered_tab: None,
            tab_rects: HashMap::new(),
            tab_preview: None,
            _tab_preview_task: None,
            pane_drag: None,
            pane_drop: None,
            tab_drop: None,
        }
    }

    // ─── 非激活 tab 的悬停缩略图(`PaneGroup.tsx:234-277`) ─────

    /// 收起浮层并取消在飞的计时。
    fn close_tab_preview(&mut self, cx: &mut Context<Self>) {
        self._tab_preview_task = None;
        if self.tab_preview.take().is_some() {
            cx.notify();
        }
    }

    /// 悬停到某个**非激活** tab:排一次 250ms 计时;到点后按 500ms 节拍续活。
    fn schedule_tab_preview(&mut self, pane_id: String, cx: &mut Context<Self>) {
        self._tab_preview_task = None;
        self.tab_preview = None;
        self._tab_preview_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(
                    pane_preview::HOVER_DELAY_MS,
                ))
                .await;
            let opened = this
                .update(cx, |this: &mut TerminalArea, cx| {
                    // 到点时再核对:还悬着同一个 tab、且量到过矩形
                    if this.hovered_tab.as_deref() != Some(pane_id.as_str()) {
                        return false;
                    }
                    let Some(rect) = this.tab_rects.get(&pane_id).copied() else {
                        return false;
                    };
                    this.tab_preview = Some((
                        pane_id.clone(),
                        rect,
                        mt_ui::motion::Transition::new(mt_ui::motion::MENU_IN),
                    ));
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !opened {
                return;
            }
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(mt_ui::MINI_REFRESH_MS))
                    .await;
                let alive = this
                    .update(cx, |this: &mut TerminalArea, cx| {
                        if this.tab_preview.is_some() {
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
        }));
    }

    /// 组装 tab 缩略图浮层。
    ///
    /// 渲染处每帧重判(原版的双闸模式):tab 被关掉 / 被激活的那一帧就不画,
    /// **状态本身也收掉** —— 用 ✕ 关掉被悬停的 tab 时点击被 `stop_propagation`
    /// 拦下,`on_hover(false)` 不一定来得及,旧锚点会残留到下次悬停。
    fn render_tab_preview(
        &mut self,
        layout: &SplitNode,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (pane_id, rect, fade) = self.tab_preview.clone()?;
        let fade = fade.drive(window);
        let store = self.store.read(cx);
        // 卡上的名字与 tab 同口径,要项目 id 才查得到远程连接名
        let project_id = store.active_project_id.clone().unwrap_or_default();
        let leaf = layout.leaf_of_pane(&pane_id);
        // 最大化时**折叠掉的那些组整组都不在屏幕上**,连它们的「激活 tab」也该给
        // 预览 —— 展开态那条「只有非激活 tab 需要」的判据在这里不成立
        let folded_away = store
            .maximized_pane_id(&project_id)
            .and_then(|id| layout.leaf_of_pane(id))
            .is_some_and(|max_leaf| leaf.map(|l| l.id()) != Some(max_leaf.id()));
        let still_hidden = folded_away
            || match leaf {
                Some(SplitNode::Leaf { active_pane_id, .. }) => active_pane_id != &pane_id,
                _ => false,
            };
        let Some(pane) = layout.pane(&pane_id).filter(|_| still_hidden) else {
            self.tab_preview = None;
            return None;
        };
        let auto_resume = store.config().ai_auto_resume.unwrap_or(true);
        let info =
            pane_preview::snapshot_pane(pane, &project_id, 0, None, store, auto_resume, cx);
        let style = pane_preview::preview_style(store);
        Some(
            deferred(
                anchored()
                    .position(pane_preview::tab_anchor(rect))
                    // 「底下放不下就翻到 tab 上方」由贴边收拢代劳
                    .snap_to_window_with_margin(px(6.0))
                    .child(pane_preview::tab_preview_card(&info, &style, fade)),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    /// 开 / 关标记浮层(按钮是 **toggle**,与 Ctrl+F 的「只开不关」不同)。
    fn toggle_marker_popover(
        &mut self,
        pane_id: &str,
        pty_id: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.marker_open.is_some() {
            self.close_marker_popover(window, cx);
            return;
        }
        if !overlay::push(overlay::key(overlay::kind::MARKER_LIST)) {
            return;
        }
        // 开之前收拾一遍:还挂着的条目趁这一下补锚(AI 刚把排队的那条处理掉的话,
        // 它就从「灰的、点不动」变回可跳)。放在这里而不是渲染里 —— 回扫要读
        // scrollback,不能每帧跑,见 `store::refresh_markers`
        self.store
            .update(cx, |store, cx| store.refresh_markers_for_pty(pty_id, cx));
        self.marker_open = Some((pane_id.to_string(), pty_id));
        self.marker_prev_focus = window.focused(cx);
        window.focus(&self.marker_focus);
        cx.notify();
    }

    /// 收起浮层(幂等),焦点还给打开浮层前的那个元素(多半是终端)。
    fn close_marker_popover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.marker_open.take().is_none() {
            return;
        }
        overlay::pop(overlay::key(overlay::kind::MARKER_LIST));
        if let Some(prev) = self.marker_prev_focus.take() {
            window.focus(&prev);
        }
        cx.notify();
    }

    fn split_state(&mut self, node_id: &str, cx: &mut App) -> Entity<ResizableState> {
        self.split_states
            .entry(node_id.to_string())
            .or_insert_with(|| cx.new(|_| ResizableState::default()))
            .clone()
    }

    /// 把键盘焦点移到相邻分屏(`focusAdjacentPane`)。
    pub fn focus_adjacent(&mut self, dir: Direction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.store.read(cx).active_project_id.clone() else {
            return;
        };
        let Some(from) = self.store.read(cx).active_pane_id(&project_id) else {
            return;
        };
        // 只在当前项目的 pane 里挑:别的项目的矩形是上一次渲染留下的残影
        let live: Vec<PaneRect> = {
            let store = self.store.read(cx);
            let Some(layout) = store.active_layout() else {
                return;
            };
            layout
                .panes()
                .into_iter()
                .filter_map(|p| self.pane_rects.get(&p.id).cloned())
                .collect()
        };
        let Some(target) = focus_nav::adjacent_pane(&live, &from, dir) else {
            return;
        };
        self.store.update(cx, |store, cx| {
            store.activate_pane(&project_id, &target, window, cx)
        });
    }

    /// AI 任务标记浮层。`None` = 没开 / 那个 pane 已经不在了。
    ///
    /// 层级照 `menu.rs` 的套路:`deferred(priority 1)` → 全窗透明遮罩(`occlude` +
    /// 按下即关)→ `anchored(按钮下缘).snap_to_window_with_margin(4px)` → 面板。
    /// **不复用 `menu::show`**:`MenuItem` 只有 label/shortcut/danger/disabled/submenu
    /// 五种表达,装不下「#seq + 时间 + 正文 + 进行中圆点」四栏。
    fn render_marker_popover(
        &mut self,
        layout: &SplitNode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (pane_id, pty_id) = self.marker_open.clone()?;
        // 切 tab / 关 pane / 分屏切换 → 无条件关(`PaneGroup.tsx:306-308`)
        if !marker_popover_alive(layout, &pane_id, pty_id) {
            self.close_marker_popover(window, cx);
            return None;
        }
        // 按钮的位置由 pane 矩形反推:pane body 的上缘就是 tab 栏下缘,
        // 右缘就是叶子右缘(见 MARKER_ANCHOR_INSET 的说明)
        let rect = self.pane_rects.get(&pane_id)?;
        // 最大化钮在场与否会让簇宽差一颗 —— 判据与 `render_leaf` 那边同一条
        // (真分了屏才画;最大化态本身就以「分了屏」为前提)
        let inset = marker_anchor_inset(matches!(layout, SplitNode::Split { .. }));
        // 面板宽度跟着界面字号缩放,锚点必须用**换算之后**的值,否则字号一调大
        // 面板就从按钮右缘溢出去了
        let panel_width = ui::font_px(MARKER_PANEL_WIDTH);
        let anchor = point(
            px(rect.left + rect.width - inset - f32::from(panel_width)),
            // 原版是「按钮下缘 + 4」;按钮在 26px 的 tab 栏里居中,下缘约在栏底上方 2px
            px(rect.top + 2.0),
        );

        // 只画验明正身的那些 —— 与 `⚑ N` 的计数同一个口,否则会出现
        // 「按钮写着 5 条、点开只有 3 条」
        let markers = self.store.read(cx).visible_markers_for_pty(pty_id);
        // 列表本体单独一层:`overflow_y_scroll` 要 Stateful(必须带 id),
        // 而外层要 `track_focus`(Esc 的落点),两件事分层最省心
        let mut list = div()
            .id(SharedString::from(format!("marker-list-{pty_id}")))
            .w_full()
            .max_h(ui::font_px(MARKER_PANEL_MAX_HEIGHT))
            .overflow_y_scroll();

        if markers.is_empty() {
            // 到不了(按钮在 count == 0 时就不画了),照抄空态兜底 `MarkerList.tsx:22-28`
            list = list.child(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(t("markerList", "empty")),
            );
        } else {
            list = list.py(px(4.0));
            for (idx, marker) in markers.into_iter().enumerate() {
                let marker_id = marker.id.clone();
                let store = self.store.clone();
                // 还没定位的条目:那条消息还在 AI 的队列里没上屏,没有行可跳。
                // **照样进列表**(用户看得到自己追加过什么),只是画成灰的,
                // 见 `markers::MarkerAnchor::Pending`
                let pending = marker.anchor.is_pending();
                list = list.child(
                    div()
                        .id(SharedString::from(format!("marker-{}", marker.id)))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(12.0))
                        .py(px(6.0))
                        .cursor_pointer()
                        .text_size(ui::font_px(12.0))
                        .text_color(if pending {
                            ui::text_muted()
                        } else {
                            ui::text_primary()
                        })
                        // `--bg-hover` 在 ui::Palette 里没有对应项,统一用 bg_overlay
                        // (与文件树行 hover 同一档)
                        .hover(|el| el.bg(ui::bg_overlay()))
                        // 悬停看全文(含粘贴多行时的换行);挂着的再补一句为什么跳不了
                        .tooltip({
                            let full = SharedString::from(if pending {
                                format!("{}\n\n{}", marker.line, t("markerList", "pendingAnchor"))
                            } else {
                                marker.line.clone()
                            });
                            move |window, cx| Tooltip::new(full.clone()).build(window, cx)
                        })
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            cx.stop_propagation();
                            let id = marker_id.clone();
                            // 挂着的条目点一下也有意义:`jump_to_marker` 会先补一次锚,
                            // AI 刚把它处理掉的话这一下就跳过去了
                            let jumped = store
                                .update(cx, |store, cx| store.jump_to_marker(pty_id, &id, cx));
                            // 跳转**并关闭浮层**(`MarkerList.tsx:36-39`);跳不动就不关 ——
                            // 关掉的话「点了没反应」会让人以为是坏的
                            if jumped {
                                this.close_marker_popover(window, cx);
                            }
                        }))
                        // 两列都是 min_w + nowrap:宽度跟着界面字号缩放,万一还是
                        // 算窄了(字族不同、`#100` 这种三位数),让它把列撑开,
                        // 而不是把 `15:29` 折成两行
                        .child(
                            div()
                                .flex_none()
                                .min_w(ui::font_px(MARKER_SEQ_W))
                                .whitespace_nowrap()
                                .text_color(ui::text_muted())
                                // 按**可见列表**里的位置编号,不用 `marker.seq`
                                // (那是全量列表里的位置,过滤掉候选条目之后会跳号:
                                // #1、#3、#4)。原版 seq 的语义本就是「它在列表里
                                // 排第几」,这里的「列表」就是用户看到的这一份
                                .child(format!("#{}", idx + 1)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .min_w(ui::font_px(MARKER_TIME_W))
                                .whitespace_nowrap()
                                .text_color(ui::text_muted())
                                .child(markers::format_time(marker.ts)),
                        )
                        // 原版是 CSS `truncate`(= hidden + ellipsis + nowrap):
                        // 40 字是**字数**上限,窄面板里照样宽过一行,不 nowrap 会折
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(markers::truncate_line(&marker.line, MARKER_LINE_MAX)),
                        )
                        // 进行中圆点。⚠️ 最后一条**永远**亮着 —— 原版没有任何地方
                        // 在 AI 完成时把它翻掉,照抄(见 markers::AiMarker::in_progress)
                        .when(marker.in_progress, |el| {
                            el.child(
                                div()
                                    .id(SharedString::from(format!("marker-dot-{}", marker.id)))
                                    .flex_none()
                                    .w(px(6.0))
                                    .h(px(6.0))
                                    .rounded_full()
                                    .bg(ui::color_ai_working())
                                    // 原版是 aria-label,gpui 没有 aria,落成 tooltip
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(t("markerList", "inProgress")).build(window, cx)
                                    }),
                            )
                        }),
                );
            }
        }

        let panel = div()
            .track_focus(&self.marker_focus)
            .key_context("MarkerList")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.close_marker_popover(window, cx);
                }
            }))
            .w(panel_width)
            .rounded(px(6.0))
            .border_1()
            .border_color(ui::border_subtle())
            .bg(ui::bg_elevated())
            .shadow_lg()
            // 面板内的按下不算「点外」—— 遮罩的 on_mouse_down 靠 hitbox 判定
            .occlude()
            .child(list);

        let size = window.viewport_size();
        Some(
            deferred(
                anchored().position(point(px(0.0), px(0.0))).child(
                    div()
                        .w(size.width)
                        .h(size.height)
                        // 点浮层外任意处关闭(原版挂 document 的 mousedown)。
                        // occlude 让这一层吃掉这次按下 —— 否则关浮层那一下会顺手
                        // 点到底下的终端/tab
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _event: &MouseDownEvent, window, cx| {
                                this.close_marker_popover(window, cx);
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(|this, _event: &MouseDownEvent, window, cx| {
                                this.close_marker_popover(window, cx);
                            }),
                        )
                        .child(
                            anchored()
                                .position(anchor)
                                .snap_to_window_with_margin(px(4.0))
                                .child(panel),
                        ),
                ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    fn render_node(
        &mut self,
        node: &SplitNode,
        project_id: &str,
        available: Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            SplitNode::Leaf { id, .. } => {
                let leaf_id = id.clone();
                let el = self.render_leaf(node, project_id, window, cx);
                self.wrap_pane_enter(&leaf_id, project_id, el, window)
            }
            SplitNode::Split {
                id,
                direction,
                children,
                sizes,
            } => {
                let state = self.split_state(id, cx);
                let horizontal = *direction == SplitDirection::Horizontal;
                let fractions = split_fractions(sizes, children.len());

                let panels: Vec<_> = children
                    .iter()
                    .enumerate()
                    .map(|(i, child)| {
                        let fraction = fractions.get(i).copied().unwrap_or(0.0) as f32;
                        // 子节点自己的可用尺寸:主轴按比例切,交叉轴照抄
                        let child_available = if horizontal {
                            Size {
                                width: available.width * fraction,
                                height: available.height,
                            }
                        } else {
                            Size {
                                width: available.width,
                                height: available.height * fraction,
                            }
                        };
                        let el = self.render_node(child, project_id, child_available, window, cx);
                        let main = if horizontal {
                            child_available.width
                        } else {
                            child_available.height
                        };
                        resizable_panel().size(main.max(px(1.0))).child(el)
                    })
                    .collect();

                let element_id = SharedString::from(format!("split-{id}"));
                let group = if horizontal {
                    h_resizable(element_id)
                } else {
                    v_resizable(element_id)
                };

                let store = self.store.clone();
                let node_id = id.clone();
                let pid = project_id.to_string();
                group
                    .with_state(&state)
                    .children(panels)
                    .on_resize(move |state, _window, cx| {
                        // ResizableState 给的是像素,布局树里存的是百分比(与磁盘
                        // 格式同口径),这里换算一次再写回。
                        let sizes: Vec<f64> = state
                            .read(cx)
                            .sizes()
                            .iter()
                            .map(|p| f32::from(*p) as f64)
                            .collect();
                        let Some(pct) = sizes_to_percent(&sizes) else {
                            return;
                        };
                        store.update(cx, |store, cx| {
                            store.set_split_sizes(&pid, &node_id, pct, cx)
                        });
                    })
                    .into_any_element()
            }
        }
    }

    /// 叶子的进场动画(`styles.css` 的 `.pane-enter` /
    /// `@keyframes paneEnter`:`opacity 0→1` + `scale(0.97)→scale(1)`,
    /// 0.26s `--ease-overlay-in`)。
    ///
    /// # 三条照抄来的语义
    ///
    /// 1. **每个叶子只播一次**:第一次渲染到这个 `(项目, 叶子)` 时起表,
    ///    之后拿同一条进度 —— 等价于 React 里「这层 DOM 只挂载一次」;
    /// 2. **切项目不重播**(原版是 `display:none` 不卸载),所以键里带项目 id
    ///    且不按帧回收;
    /// 3. **不过减弱动效的闸**:`.pane-enter` 在原版 reduce 段里被**点名豁免**
    ///    (`styles.css:441-443`),开了「减弱动态效果」照样播。
    ///
    /// # ⚠️ 刻意只做淡入,不做 `scale(0.97)`
    ///
    /// gpui 没有 transform,能等价缩放的只有「改内边距/尺寸」——而那是**会改布局**
    /// 的:pane 内容框在这 260ms 里窄十几个像素,`TerminalView` 就会按小一号的
    /// 格子数回调 `on_grid_resize`,一路 resize 到 PTY(启动时每个叶子都来一遍,
    /// 首个 PTY 甚至会以缩小后的尺寸建出来再被改回去)。原版的 `transform: scale`
    /// 压根不参与布局,没有这条代价。
    ///
    /// 3% 的缩放本来就是「看得出这里多了一块」的附加提示,淡入才是主信号 ——
    /// 拿一次终端重排去换它不划算。上游哪天给 Element 通用变换再补。
    fn wrap_pane_enter(
        &mut self,
        leaf_id: &str,
        project_id: &str,
        el: AnyElement,
        window: &Window,
    ) -> AnyElement {
        let key = format!("{project_id}\u{1}{leaf_id}");
        let progress = self
            .pane_enter
            .entry(key)
            .or_insert_with(|| mt_ui::motion::Transition::new(mt_ui::motion::PANE_ENTER))
            .drive(window);
        if progress >= 1.0 {
            // 跑完就把包装层整个摘掉:少一层空 div,也少一次 opacity 合成。
            // (匿名 div 不带 ElementId,加/摘不影响子树的元素状态路径)
            return el;
        }
        div()
            .size_full()
            .opacity(pane_enter_opacity(progress))
            .child(el)
            .into_any_element()
    }

    /// 最大化视图 = 铺满的那一格 + 其余叶子的折叠标题条(码在底部)。
    ///
    /// v0.14.0 的最大化是「整树其余 pane 直接不进元素树」,别的终端连状态灯都
    /// 看不见 —— 另一格里的 AI 跑完了毫无提示,只能先还原再找。折叠条把
    /// 「看得见 + 点得回去」补回来,代价只有每格 26px。
    ///
    /// **折叠条不按原方位摆**(左右分屏的那格也码到底部):26px 宽的竖条上放不下
    /// 横排文字,只剩一颗状态灯的话还不如统一成整宽横条 —— 标题、品牌图标、
    /// 未读标都能完整显示。原布局树一个字没动,还原后照旧。
    fn render_maximized(
        &mut self,
        layout: &SplitNode,
        leaf: &SplitNode,
        project_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = self.render_leaf(leaf, project_id, window, cx);
        let leaf_id = leaf.id().to_string();
        let others: Vec<&SplitNode> = layout
            .leaves()
            .into_iter()
            .filter(|l| l.id() != leaf_id)
            .collect();
        let bars: Vec<AnyElement> = others
            .into_iter()
            .map(|l| self.render_collapsed_leaf(l, project_id, cx))
            .collect();
        // 折叠区自己滚,铺满那格恒占剩下的全部(`min_h(0)` 是 flex 子项能收缩的
        // 前提 —— 缺了它内容撑高时会把折叠区顶出可视区)
        let zone_max = (self.area_size.height * COLLAPSED_ZONE_MAX).max(px(TAB_BAR_H));
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(div().flex_1().min_h(px(0.0)).child(body))
            .child(
                div()
                    .id("collapsed-zone")
                    .flex()
                    .flex_col()
                    .flex_none()
                    .max_h(zone_max)
                    .overflow_y_scroll()
                    .children(bars),
            )
            .into_any_element()
    }

    /// 一条折叠标题条:该叶子的每个 tab 一颗状态灯 + 品牌图标 + 标题 + 未读标。
    ///
    /// 刻意**不画**新建 / 查找 / 分屏 / 关组那套控件簇 —— 折叠条是导航件不是
    /// 工作区,按钮挤在 26px 里全是误点。关闭 / 重命名仍走 tab 右键菜单(与展开态
    /// 同一个 [`tab_menu`]),悬停缩略图也照给。
    fn render_collapsed_leaf(
        &mut self,
        node: &SplitNode,
        project_id: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let SplitNode::Leaf {
            id: leaf_id,
            panes,
            active_pane_id,
        } = node
        else {
            return div().into_any_element();
        };
        let store = self.store.read(cx);
        let Some(active_id) = panes
            .iter()
            .find(|p| &p.id == active_pane_id)
            .or_else(|| panes.first())
            .map(|p| p.id.clone())
        else {
            return div().into_any_element();
        };
        let auto_resume = store.config().ai_auto_resume.unwrap_or(true);
        let pid = project_id.to_string();
        let leaf = leaf_id.clone();
        // 显示数据先摘成 owned:下面每个 tab 都要往 move 闭包里搬,借着 store 走不了
        let tabs: Vec<(String, String, PaneStatus, Option<AiVendor>, bool)> = panes
            .iter()
            .map(|pane| {
                // 品牌图标的取值口径与展开态 tab 逐字一致(见 render_leaf 的 vendors)
                let vendor = pane
                    .shows_ai_session(auto_resume)
                    .then(|| pane.ai_agent())
                    .flatten()
                    .and_then(|agent| {
                        AiVendor::from_session_type(agent)
                            .or_else(|| AiVendor::infer(Some(agent), None))
                    });
                (
                    pane.id.clone(),
                    store.pane_display_label(&pid, pane),
                    pane.status,
                    vendor,
                    store.is_pane_unread_done(&pane.id),
                )
            })
            .collect();

        // 焦点句柄与展开态共用 `tab_focus` 那张表(它按整棵树保留,见 render 里的
        // retain):折叠 ↔ 展开来回切时 Tab 焦点不丢
        for (pane_id, ..) in &tabs {
            if !self.tab_focus.contains_key(pane_id) {
                let handle = cx.focus_handle();
                self.tab_focus.insert(pane_id.clone(), handle);
            }
        }

        let mut bar = div()
            .id(SharedString::from(format!("collapsed-{leaf}")))
            .flex()
            .items_center()
            .flex_none()
            .h(px(TAB_BAR_H))
            .w_full()
            .overflow_hidden()
            .bg(ui::bg_elevated())
            .border_t_1()
            .border_color(ui::border_subtle())
            .text_size(ui::font_px(12.0))
            .cursor_pointer()
            .hover(|el| el.bg(ui::bg_overlay()))
            // 条上任意空白处点一下 = 这一组接管铺满(不必瞄准 tab)
            .on_click(cx.listener({
                let (pid, anchor) = (pid.clone(), active_id.clone());
                move |this: &mut TerminalArea, _event: &ClickEvent, window, cx| {
                    this.take_over_maximized(&pid, &anchor, window, cx);
                }
            }));

        let this_area = cx.entity();
        for (pane_id, label, status, vendor, unread) in tabs {
            let is_active = pane_id == active_id;
            let focus = self.tab_focus.get(&pane_id).cloned();
            let (pid_click, pane_click) = (pid.clone(), pane_id.clone());
            let (pid_key, pane_key) = (pid.clone(), pane_id.clone());
            let (pid_menu, pane_menu, label_menu) =
                (pid.clone(), pane_id.clone(), label.clone());
            let pane_hover = pane_id.clone();
            let pane_rect = pane_id.clone();
            let this_rect = this_area.clone();
            bar = bar.child(
                div()
                    .id(SharedString::from(format!("collapsed-tab-{pane_id}")))
                    .relative()
                    .flex()
                    .items_center()
                    .h_full()
                    .gap(px(6.0))
                    .px(px(10.0))
                    .flex_none()
                    .when_some(focus, |el, focus| el.track_focus(&focus).tab_index(0))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            cx.stop_propagation();
                            this.take_over_maximized(&pid_key, &pane_key, window, cx);
                        }
                    }))
                    // 悬停缩略图:折叠条上**每个** tab 都值得预览 —— 这一整组的画面
                    // 一个都不在屏幕上(展开态是「只有非激活 tab 需要」,判据在
                    // `render_tab_preview` 里按最大化态放宽)
                    .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                        let mine = this.hovered_tab.as_deref() == Some(pane_hover.as_str());
                        if *hovered {
                            if mine {
                                return;
                            }
                            this.hovered_tab = Some(pane_hover.clone());
                            this.schedule_tab_preview(pane_hover.clone(), cx);
                        } else {
                            if !mine {
                                return;
                            }
                            this.hovered_tab = None;
                            this.close_tab_preview(cx);
                        }
                        cx.notify();
                    }))
                    // 缩略图的锚点(与展开态 tab 共用 `tab_rects`:两者互斥,
                    // 同一个 pane 不会在一帧里既折叠又展开)。故意不 notify
                    .child({
                        canvas(
                            move |bounds, _window, cx| {
                                this_rect.update(cx, |area: &mut TerminalArea, _cx| {
                                    area.tab_rects.insert(pane_rect.clone(), bounds);
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full()
                    })
                    .text_color(if is_active {
                        ui::text_primary()
                    } else {
                        ui::text_muted()
                    })
                    .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        this.take_over_maximized(&pid_click, &pane_click, window, cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.close_tab_preview(cx);
                            let entries =
                                tab_menu(&this.store, &pid_menu, &pane_menu, &label_menu, cx);
                            menu::show(event.position, entries, window, cx);
                        }),
                    )
                    .child(ui::status_dot(
                        SharedString::from(format!("collapsed-status-{pane_id}")),
                        status,
                    ))
                    .when_some(vendor, |el, vendor| {
                        el.child(BrandIcon::new(Some(vendor)).size(px(12.0)).color(
                            if is_active {
                                ui::text_primary()
                            } else {
                                ui::text_muted()
                            },
                        ))
                    })
                    .child(div().child(label))
                    .when(unread, |el| {
                        el.child(
                            div()
                                .w(px(5.0))
                                .h(px(5.0))
                                .rounded_full()
                                .bg(ui::color_success()),
                        )
                    }),
            );
        }

        // 右端那颗「点这里铺满」的提示图标。**不挂自己的点击** —— 整条都是热区,
        // 它只是把可点性说出来,顺便当那句 tooltip 的锚点。
        //
        // tooltip 刻意**不挂在整条上**:挂上去的话鼠标停在 tab 上会与悬停缩略图
        // 两个浮层一起弹(tooltip 认的是父元素的 hitbox,子元素挡不住)。
        bar.child(
            div()
                .id(SharedString::from(format!("collapsed-hint-{leaf}")))
                .ml_auto()
                .flex()
                .items_center()
                .h_full()
                .px(px(CTRL_CLUSTER_PAD))
                .opacity(0.5)
                .hover(|el| el.opacity(1.0))
                .tooltip(|window, cx| Tooltip::new(t("paneGroup", "collapsedHint")).build(window, cx))
                .child(VectorIcon::new(ICON_MAXIMIZE, px(CTRL_ICON)).ink(ui::text_muted())),
        )
        .into_any_element()
    }

    /// 折叠条的落点:这一组接管铺满,原先铺满的那组缩回折叠区。
    ///
    /// 顺序是**先换铺满再激活**:`activate_pane` 末尾会把焦点交给终端,那时候
    /// 布局状态得已经是新的,否则焦点会落在这一帧还没画出来的那格上。
    /// [`AppStore::toggle_maximized_leaf`] 在这里恒等于「设成最大化」——
    /// 折叠条上的叶子按定义就不是当前铺满的那一个。
    fn take_over_maximized(
        &mut self,
        project_id: &str,
        pane_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_tab_preview(cx);
        self.hovered_tab = None;
        self.store.update(cx, |store, cx| {
            store.toggle_maximized_leaf(project_id, pane_id, cx);
            store.activate_pane(project_id, pane_id, window, cx);
        });
    }

    fn render_leaf(
        &mut self,
        node: &SplitNode,
        project_id: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let SplitNode::Leaf {
            id: leaf_id,
            panes,
            active_pane_id,
        } = node
        else {
            return div().into_any_element();
        };

        let store = self.store.read(cx);
        let active = panes
            .iter()
            .find(|p| &p.id == active_pane_id)
            .or_else(|| panes.first());
        let Some(active) = active else {
            return div().into_any_element();
        };
        let terminal = active.pty_id.and_then(|id| store.terminal(id)).cloned();
        // 远程 pane 的断线覆盖层(`PaneGroup.tsx:329-333` 的 `showReconnect`):
        // 判据是「项目是 SSH 远程项目 **且** 这条 PTY 已登记退出」。
        // ⚠️ **本地 pane 不进这条路** —— 本地退出仍走既有的 error 状态 + 右下角
        // 「shell 已退出」角标(原版 `remote &&` 那一半就是这个闸)。
        // 断链(连接被删)照样算远程项目,遮罩照出:重连会再失败一次并把明确的
        // 断链错误画进 pane,比静默什么都不发生强。
        let show_reconnect = store.is_remote_project(project_id)
            && active
                .pty_id
                .map(|id| store.is_pty_exited(id))
                .unwrap_or(false);
        // AI 任务标记数。**列表为空就整个不画按钮**(`PaneGroup.tsx:489`),
        // 这就是「⚑ 平时看不见」的直接原因 —— 见 `markers` 模块注释的 alt screen 段。
        let marker_count = active
            .pty_id
            .map(|id| markers::visible(store.markers_for_pty(id)).count())
            .unwrap_or(0);
        let unread: Vec<bool> = panes.iter().map(|p| store.is_pane_unread_done(&p.id)).collect();
        // tab 上的 AI 品牌图标:显示条件与 agent 取值都照抄原版(见 PaneState 上
        // 的两个方法);`aiAutoResume` 缺省开启,与 store 里那处取值同口径
        let auto_resume = store.config().ai_auto_resume.unwrap_or(true);
        let vendors: Vec<Option<AiVendor>> = panes
            .iter()
            .map(|p| {
                if !p.shows_ai_session(auto_resume) {
                    return None;
                }
                let agent = p.ai_agent()?;
                // CLI 名直取(claude/codex/grok),其余 CLI(opencode/pi/gemini…)
                // 走与前端 `inferVendor` 同规则同优先级的词匹配 —— 原版 tab 上
                // 调的就是 `inferVendor({ agent })`,只认三家会漏掉它们的图标
                AiVendor::from_session_type(agent).or_else(|| AiVendor::infer(Some(agent), None))
            })
            .collect();

        let active_id = active.id.clone();
        let pid = project_id.to_string();
        let leaf = leaf_id.clone();
        // 拖拽相关的三份视图状态一律与它与门(见字段注释)
        let dragging = cx.has_active_drag();
        // 最大化钮的出现条件(`PaneGroup.tsx:686`):真分了屏才有意义。
        // `maximized_pane_id()` 自带「布局是 split」这道闸,所以这里一个判据够用
        let layout_is_split = store
            .project_state(project_id)
            .and_then(|s| s.active_layout())
            .is_some_and(|l| matches!(l, SplitNode::Split { .. }));
        let is_maximized = store
            .maximized_pane_id(project_id)
            .is_some_and(|id| panes.iter().any(|p| p.id == id));

        // tab 栏横向滚动(E.2):tab **不压缩**(`min_w` 之下就溢出),
        // 溢出时整条可横向滚。`overflow_x_scroll` 要求元素是 stateful(有 `.id()`)。
        //
        // **垂直滚轮不必自己映射**:gpui 只在 `overflow.x == Scroll && overflow.y != Scroll`
        // 且 `restrict_scroll_to_axis == false`(默认)时把 `delta.y` 记到 x 上
        // (gpui-0.2.2 `elements/div.rs:2422-2428`,默认值见 `style.rs:741`)——
        // 与原版靠 WebView 免费拿到的那条行为等价。
        let mut bar = div()
            .id(gpui::SharedString::from(format!("tabbar-{leaf}")))
            .flex()
            .items_center()
            .flex_none()
            .h(px(TAB_BAR_H))
            .overflow_x_scroll()
            .bg(ui::bg_elevated())
            .border_b_1()
            .border_color(ui::border_subtle())
            .text_size(ui::font_px(12.0));

        // tab 焦点句柄先补齐(下面的循环里 `cx` 要交给 listener,腾不出可变借用)
        for pane in panes.iter() {
            if !self.tab_focus.contains_key(&pane.id) {
                let handle = cx.focus_handle();
                self.tab_focus.insert(pane.id.clone(), handle);
            }
        }

        for (idx, pane) in panes.iter().enumerate() {
            let is_active = pane.id == active_id;
            let pane_id = pane.id.clone();
            let pane_id_key = pane.id.clone();
            let pane_id_hover = pane.id.clone();
            let pid_key = pid.clone();
            let tab_focus = self.tab_focus.get(&pane.id).cloned();
            let pane_id_rename = pane.id.clone();
            let pid_click = pid.clone();
            let pane_id_close = pane.id.clone();
            let pane_id_menu = pane.id.clone();
            let pid_close = pid.clone();
            let pid_rename = pid.clone();
            let pid_menu = pid.clone();
            // tab 标题走 store 的统一口径:自定义名 > 远程连接名 > shell 名。
            // 恢复布局时远程 pane 的 shellName 会被映射成本地 shell 名、**不可信**,
            // 所以远程那一档必须由 store 查连接表补上(`remoteProject.ts::paneDisplayLabel`)
            let label = store.pane_display_label(&pid, pane);
            let label_menu = label.clone();
            let label_text = label.clone();
            let has_unread = unread.get(idx).copied().unwrap_or(false);
            let vendor = vendors.get(idx).copied().flatten();
            let this_area = cx.entity();
            let pid_drag = pid.clone();
            // 与 `has_active_drag` 与门:拖拽被中断(松手在窗外)时 gpui 会清
            // active_drag 并重画,变淡自动撤销,不必到处补清理
            let is_dragging_self =
                dragging && self.pane_drag.as_deref() == Some(pane.id.as_str());
            bar = bar.child(
                div()
                    .id(gpui::SharedString::from(format!("tab-{}", pane.id)))
                    // 量矩形的 canvas 要一个定位上下文;`relative` 对 flex 项
                    // 本身的布局没有影响
                    .relative()
                    // tab 键盘可达(原版 `role="tab"` + `tabIndex`):拿到焦点后
                    // Enter/Space 激活。原版是 roving tabindex(只有激活 tab 是
                    // 0),这里所有 tab 都可 Tab 到 —— gpui 没有 roving 语义,
                    // 而「Tab 只能到激活 tab」等于隐藏 tab 键盘不可达
                    .when_some(tab_focus, |el, focus| el.track_focus(&focus).tab_index(0))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            cx.stop_propagation();
                            this.store.update(cx, |store, cx| {
                                store.activate_pane(&pid_key, &pane_id_key, window, cx)
                            });
                        }
                    }))
                    // 激活 tab 的画面就在眼前,只有隐藏 tab 需要预览。
                    // ⚠️ 离开分支必须先核对「离开的正是我们记着的那一个」:相邻
                    // 元素的 enter/leave 到达顺序不保证,直接清会把刚进来的那个
                    // 抹掉(鼠标沿 tab 栏横扫时预览再也弹不出来)
                    .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                        let mine = this.hovered_tab.as_deref() == Some(pane_id_hover.as_str());
                        if *hovered {
                            if mine {
                                return;
                            }
                            this.hovered_tab = Some(pane_id_hover.clone());
                            if is_active {
                                this.close_tab_preview(cx);
                            } else {
                                this.schedule_tab_preview(pane_id_hover.clone(), cx);
                            }
                        } else {
                            if !mine {
                                return;
                            }
                            this.hovered_tab = None;
                            this.close_tab_preview(cx);
                        }
                        cx.notify();
                    }))
                    // 只量不画:缩略图锚点 + tab 栏拖拽插入位都要 tab 的屏幕矩形。
                    // **每个 tab 都挂**(原先只挂悬停中的那一个):插入位要算的是
                    // 「指针落在哪个 tab 的中线哪一侧」,那需要**全部** tab 的横向
                    // 区间,只有悬停那一个不够用。故意不 notify —— 量完再触发重画
                    // 就是每帧一个死循环。
                    .child({
                        let this = this_area.clone();
                        let id = pane.id.clone();
                        canvas(
                            move |bounds, _window, cx| {
                                this.update(cx, |area: &mut TerminalArea, _cx| {
                                    area.tab_rects.insert(id.clone(), bounds);
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full()
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(6.0))
                    .px(px(10.0))
                    .min_w(px(110.0))
                    .cursor_pointer()
                    .when(is_active, |el| {
                        el.bg(ui::bg_terminal())
                            .text_color(ui::text_primary())
                            .border_t_2()
                            .border_color(ui::accent())
                    })
                    .when(!is_active, |el| {
                        el.text_color(ui::text_muted()).border_t_2().border_color(
                            gpui::Hsla {
                                a: 0.0,
                                ..ui::accent()
                            },
                        )
                    })
                    // ── tab 拖起(v0.14.0):移动 / 合并 / 重排 ─────────────
                    //
                    // 照 `project_list.rs` 的既有模式:记下源 id 让本行变淡 +
                    // 交出一个跟着鼠标走的拖影。**起拖阈值是 gpui 内建的 2px**
                    // (原版是 5px 曼哈顿),差异见 `dnd` 模块注释第 3 条。
                    // 原版那套「松手后一次性抑制 click」不需要 —— gpui 拖起时
                    // 会把 `clicked_state` 清掉。
                    .on_drag(
                        crate::dnd::DragPane {
                            project_id: pid_drag.clone(),
                            pane_id: pane.id.clone(),
                        },
                        {
                            let this = this_area.clone();
                            let label = label_text.clone();
                            move |item: &crate::dnd::DragPane, _offset, _window, cx| {
                                let id = item.pane_id.clone();
                                this.update(cx, |area: &mut TerminalArea, _cx| {
                                    area.pane_drag = Some(id);
                                });
                                crate::dnd::preview(
                                    label.clone(),
                                    crate::dnd::PreviewIcon::Terminal,
                                    cx,
                                )
                            }
                        },
                    )
                    // 源 tab 变淡(原版 `el.style.opacity = '0.4'`)
                    .when(is_dragging_self, |el| el.opacity(0.4))
                    // 单击切 tab,双击改名(旧版是右键菜单里的「重命名」)。
                    // ⚠️ 必须 `stop_propagation`:tab 栏那层挂着「双击空白处最大化」,
                    // 不截断的话在 tab 上双击会**同时**改名和最大化
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        this.close_tab_preview(cx);
                        if click_count(event) >= 2 {
                            let (label, store) = (label.clone(), this.store.clone());
                            modal::open_rename_pane(
                                store,
                                pid_rename.clone(),
                                pane_id_rename.clone(),
                                label,
                                window,
                                cx,
                            );
                            return;
                        }
                        this.store.update(cx, |store, cx| {
                            store.activate_pane(&pid_click, &pane_id, window, cx)
                        });
                    }))
                    // tab 右键菜单(`PaneGroup.tsx` 的 paneContextMenu)
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            // 开菜单前先收缩略图(原版 onContextMenu 第一句)
                            this.close_tab_preview(cx);
                            let entries =
                                tab_menu(&this.store, &pid_menu, &pane_id_menu, &label_menu, cx);
                            menu::show(event.position, entries, window, cx);
                        }),
                    )
                    // 动画 id 拿 pane id 拼(跨帧稳定、逐 tab 唯一);**不能用循环
                    // 下标** —— 删掉中间一个 tab 会让后面所有状态灯的动画进度跳一格
                    .child(ui::status_dot(
                        gpui::SharedString::from(format!("status-pane-{}", pane.id)),
                        pane.status,
                    ))
                    // AI 品牌图标(原版 `PaneGroup.tsx` 的 `aiActive && <BrandIcon/>`):
                    // 只在这个 pane 真有 AI 会话身份时出现,认不出厂商就不占位
                    .when_some(vendor, |el, vendor| {
                        el.child(
                            BrandIcon::new(Some(vendor))
                                .size(px(12.0))
                                // VectorIcon 不继承 text_color,跟着 tab 的明暗自己喂
                                .color(if is_active {
                                    ui::text_primary()
                                } else {
                                    ui::text_muted()
                                }),
                        )
                    })
                    .child(div().child(label_text.clone()))
                    // 未读完成标(窗口没聚焦时完成的任务)
                    .when(has_unread, |el| {
                        el.child(
                            div()
                                .w(px(5.0))
                                .h(px(5.0))
                                .rounded_full()
                                .bg(ui::color_success()),
                        )
                    })
                    .child(
                        div()
                            .id(gpui::SharedString::from(format!("tab-close-{}", pane.id)))
                            .w(px(14.0))
                            .h(px(14.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.0))
                            .text_color(ui::text_muted())
                            .hover(|el| el.bg(ui::bg_overlay()).text_color(ui::color_error()))
                            // tab 上的 × 与右键「关闭此终端」同一个入口:关之前
                            // 盘点 AI 会话并确认(原版 `closePane` 默认 confirm)
                            .on_click(cx.listener(move |this, _event, window, cx| {
                                cx.stop_propagation();
                                pane_actions::close_pane(
                                    this.store.clone(),
                                    pid_close.clone(),
                                    pane_id_close.clone(),
                                    window,
                                    cx,
                                );
                            }))
                            .child("×"),
                    ),
            );
        }

        // 新建终端
        let pid_new = pid.clone();
        let anchor_new = active_id.clone();
        bar = bar.child(
            div()
                .id(gpui::SharedString::from(format!("tab-new-{leaf}")))
                .px(px(8.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .text_color(ui::text_muted())
                .hover(|el| el.text_color(ui::accent()))
                // 左键单击**直接弹 shell 选择菜单**(不是长按、不是下拉箭头);
                // 只有一个 shell 时不弹 —— 否则单 shell 用户每次多点一下
                // (`PaneGroup.tsx:218-232` 那道 `<= 1` 的闸)
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    // 与 tab 同理:别让这一下冒到 tab 栏的「双击空白处最大化」上
                    cx.stop_propagation();
                    let shells = this.store.read(cx).config().available_shells.clone();
                    if shells.len() <= 1 {
                        this.store.update(cx, |store, cx| {
                            store.new_terminal(&pid_new, None, Some(anchor_new.clone()), window, cx);
                        });
                        return;
                    }
                    // 无勾选标记、无分隔线,就是一列 shell 名
                    let entries: Vec<menu::MenuEntry> = shells
                        .into_iter()
                        .map(|shell| {
                            let store = this.store.clone();
                            let (pid, anchor) = (pid_new.clone(), anchor_new.clone());
                            let name = shell.name.clone();
                            menu::item(name, move |window, cx| {
                                let (pid, anchor, shell) =
                                    (pid.clone(), anchor.clone(), shell.clone());
                                store.update(cx, |store, cx| {
                                    store.new_terminal(&pid, Some(shell), Some(anchor), window, cx);
                                });
                            })
                        })
                        .collect();
                    menu::show(click_position(event, window), entries, window, cx);
                }))
                .child("+"),
        );

        // 右侧:查找 / 分屏 / 关整组(原版 `ctrlBtn`:常驻 60% 透明度,hover 全亮
        // + `--border-subtle` 底。图标是 `PaneGroup.tsx:40-62` 那几条 SVG 的自绘
        // 搬运;VectorIcon 的 ink 定死在构造期,hover 换色进不去 —— 与
        // `activity_bar::strip_button` 同取舍,反馈靠透明度与底色变化)
        let ctrl_icon = |id: gpui::SharedString, shapes: &'static [Shape]| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .w(px(CTRL_BTN))
                .h(px(CTRL_BTN))
                .rounded(px(3.0))
                .cursor_pointer()
                .opacity(0.6)
                .hover(|el| el.opacity(1.0).bg(ui::border_subtle()))
                .child(VectorIcon::new(shapes, px(CTRL_ICON)).ink(ui::text_muted()))
        };
        // 终端内查找:与 marker 按钮同一道「有 pty 才画」的闸
        // (原版 `PaneGroup.tsx:504`,`activePane.ptyId !== undefined`)
        let search_btn = active.pty_id.map(|pty_id| {
            ctrl_icon(
                gpui::SharedString::from(format!("term-search-{leaf}")),
                ICON_SEARCH,
            )
            .on_click(cx.listener(move |this, _event, window, cx| {
                cx.stop_propagation();
                let pane = this.store.read(cx).terminal(pty_id).cloned();
                if let Some(pane) = pane {
                    pane.update(cx, |pane, cx| pane.open_search(window, cx));
                }
            }))
        });
        // ⚑ N:图标是**文本字符**,不是 SVG(与 menu.rs 的 `✓ ` 同一套理由);
        // 宽度不固定,所以不复用上面那个 22×22 的方钮。
        let marker_pty = active.pty_id.filter(|_| marker_count > 0);
        let marker_pane_id = active_id.clone();
        let marker_btn = marker_pty.map(|pty_id| {
            div()
                .id(gpui::SharedString::from(format!("markers-{leaf}")))
                .mr(px(MARKER_BTN_MARGIN_RIGHT))
                .px(px(6.0))
                .py(px(2.0))
                .rounded(px(3.0))
                .flex()
                .items_center()
                .gap(px(4.0))
                .cursor_pointer()
                .text_color(ui::text_muted())
                .hover(|el| el.text_color(ui::accent()).bg(ui::border_subtle()))
                .tooltip(move |window, cx| {
                    // `{mod}` 的插值不能走 `tr!`:那个宏的参数位是 `$name:ident`,
                    // 而 `mod` 是 Rust 关键字塞不进去(`search_modal.rs:320` 同样的坑)
                    Tooltip::new(mt_i18n::t_args(
                        "paneGroup",
                        "markerTooltip",
                        &[("mod", mod_label())],
                    ))
                    .build(window, cx)
                })
                .on_click(cx.listener(move |this, _event, window, cx| {
                    cx.stop_propagation();
                    this.toggle_marker_popover(&marker_pane_id, pty_id, window, cx);
                }))
                .child("⚑")
                .child(div().child(marker_count.to_string()))
        });
        // 最大化 / 还原(v0.14.0)。只有真分了屏才画 —— 单格布局下「铺满」是空操作,
        // 常驻一颗按不动的按钮不如不画(原版 `layoutIsSplit || isMaximized` 同款)。
        let pid_max = pid.clone();
        let anchor_max = active_id.clone();
        let maximize_btn = (layout_is_split || is_maximized).then(|| {
            ctrl_icon(
                gpui::SharedString::from(format!("maximize-{leaf}")),
                if is_maximized {
                    ICON_RESTORE
                } else {
                    ICON_MAXIMIZE
                },
            )
            .tooltip(move |window, cx| {
                Tooltip::new(t(
                    "paneGroup",
                    if is_maximized {
                        "restorePane"
                    } else {
                        "maximizePane"
                    },
                ))
                .build(window, cx)
            })
            .on_click(cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                let (pid, anchor) = (pid_max.clone(), anchor_max.clone());
                this.store
                    .update(cx, |store, cx| store.toggle_maximized_leaf(&pid, &anchor, cx));
            }))
        });
        let pid_right = pid.clone();
        let anchor_right = active_id.clone();
        let pid_down = pid.clone();
        let anchor_down = active_id.clone();
        let pid_close_leaf = pid.clone();
        let leaf_for_close = leaf.clone();
        bar = bar.child(
            div()
                .ml_auto()
                .flex()
                .items_center()
                .gap(px(CTRL_GAP))
                .px(px(CTRL_CLUSTER_PAD))
                .children(marker_btn)
                .children(search_btn)
                .children(maximize_btn)
                .child(
                    ctrl_icon(
                        gpui::SharedString::from(format!("split-right-{leaf}")),
                        ICON_SPLIT_RIGHT,
                    )
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        cx.stop_propagation();
                        this.store.update(cx, |store, cx| {
                            store.split_pane(
                                &pid_right,
                                &anchor_right,
                                SplitDirection::Horizontal,
                                window,
                                cx,
                            );
                        });
                    })),
                )
                .child(
                    ctrl_icon(
                        gpui::SharedString::from(format!("split-down-{leaf}")),
                        ICON_SPLIT_DOWN,
                    )
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        cx.stop_propagation();
                        this.store.update(cx, |store, cx| {
                            store.split_pane(
                                &pid_down,
                                &anchor_down,
                                SplitDirection::Vertical,
                                window,
                                cx,
                            );
                        });
                    })),
                )
                .child(
                    // × 仍是文本字形:hover 转 error 色要跟随文本色,VectorIcon
                    // 进不去(见 ctrl_icon 的注释);盒子样式与图标钮同一套
                    div()
                        .id(gpui::SharedString::from(format!("close-leaf-{leaf}")))
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(CTRL_BTN))
                        .h(px(CTRL_BTN))
                        .rounded(px(3.0))
                        .cursor_pointer()
                        .opacity(0.6)
                        .text_color(ui::text_muted())
                        .hover(|el| {
                            el.opacity(1.0)
                                .bg(ui::border_subtle())
                                .text_color(ui::color_error())
                        })
                        // 控制条的 × 关的是**整组**,同样先确认(原版 closeLeaf)
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            cx.stop_propagation();
                            pane_actions::close_leaf(
                                this.store.clone(),
                                pid_close_leaf.clone(),
                                leaf_for_close.clone(),
                                window,
                                cx,
                            );
                        }))
                        .child("×"),
                ),
        );

        // 双击 tab 栏**空白处**最大化 / 还原。原版靠 `e.target.closest('[data-pane-tab],button')`
        // 排除 tab 与按钮;gpui 侧改由那些子元素自己 `stop_propagation`(见各处注释)
        // —— 效果相同,而且不需要在这里维护一张「哪些子元素算控件」的名单。
        let pid_dblclick = pid.clone();
        let anchor_dblclick = active_id.clone();
        bar = bar.on_click(cx.listener(move |this, event: &ClickEvent, _window, cx| {
            if click_count(event) < 2 {
                return;
            }
            let (pid, anchor) = (pid_dblclick.clone(), anchor_dblclick.clone());
            this.store
                .update(cx, |store, cx| store.toggle_maximized_leaf(&pid, &anchor, cx));
        }));

        // ── tab 栏的落点层 ────────────────────────────────────────
        //
        // 为什么要在可滚动的 tab 栏**外面**再包一层:
        // ① 插入指示线是绝对定位的,放进滚动容器里会跟着内容偏移,而 x 又是由
        //    屏幕坐标现算的(已含滚动量)—— 两下叠加就双算了;包一层非滚动的
        //    父级,`指示线 x = tab 屏幕左缘 − tab 栏屏幕左缘` 直接可用。
        // ② `on_drag_move` 的 `event.bounds` 就是挂监听那个元素的矩形,挂在这层
        //    等于白拿「tab 栏在屏幕上的位置」,不必再为它单开一片量尺 canvas。
        let leaf_for_bar = leaf.clone();
        let leaf_for_drop = leaf.clone();
        let pid_tab_drop = pid.clone();
        let tab_indicator = self
            .tab_drop
            .as_ref()
            .filter(|(id, _, _)| dragging && id == &leaf)
            .map(|(_, _, x)| *x);
        // ⚠️ 必须是**纵向** flex:`bar` 自己带 `flex_none`,放进默认的横向 flex 里
        // 就变成「宽度按内容撑」,右侧控件簇的 `ml_auto` 会跟着缩到 tab 后面去。
        // 纵向下 `flex_none` 只钉高度(26),宽度照旧横向铺满 —— 与它原先直接
        // 挂在叶子那个 `flex_col` 容器里时的表现一字不差。
        let bar_layer = div()
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .on_drag_move(cx.listener({
                let leaf_id = leaf_for_bar.clone();
                move |this: &mut TerminalArea,
                      event: &gpui::DragMoveEvent<crate::dnd::DragPane>,
                      _window,
                      cx| {
                    let dragged = event.drag(cx).clone();
                    this.note_tab_drag_over(&leaf_id, &dragged, event.bounds, event.event.position, cx);
                }
            }))
            .on_drop(cx.listener({
                let leaf_id = leaf_for_drop.clone();
                move |this: &mut TerminalArea, item: &crate::dnd::DragPane, window, cx| {
                    this.drop_on_tab_bar(&pid_tab_drop, &leaf_id, item, window, cx);
                }
            }))
            .child(bar)
            // 插入位指示线:3px 圆头 + accent 双层光晕(评审定稿口径,见 TAB_DROP_LINE_W)
            .children(tab_indicator.map(|x| {
                div()
                    .absolute()
                    .top(px(2.0))
                    .h(px(22.0))
                    .left(px((x - TAB_DROP_LINE_W / 2.0).max(0.0)))
                    .w(px(TAB_DROP_LINE_W))
                    .rounded_full()
                    .bg(ui::accent())
                    .shadow(vec![
                        gpui::BoxShadow {
                            color: ui::accent(),
                            offset: point(px(0.0), px(0.0)),
                            blur_radius: px(6.0),
                            spread_radius: px(0.0),
                        },
                        gpui::BoxShadow {
                            color: ui::accent(),
                            offset: point(px(0.0), px(0.0)),
                            blur_radius: px(2.0),
                            spread_radius: px(0.0),
                        },
                    ])
            }));

        let pid_focus = pid.clone();
        let active_for_focus = active_id.clone();
        let pid_drop = pid.clone();
        let drop_pane_id = active_id.clone();
        let pid_reconnect = pid.clone();
        let pane_reconnect = active_id.clone();
        // 拖拽中断(松手在窗外)后 gpui 会清 active_drag 并重画,与它与门就不必
        // 到处补清理 —— 与 `project_list.rs` 里那份高亮同一套判据。
        let file_drop_over =
            cx.has_active_drag() && self.file_drop_pane.as_deref() == Some(active_id.as_str());
        // 方向导航要知道每个 pane 画在哪 —— canvas 只量不画,量完存进本视图。
        // 这里**故意不 notify**:量尺寸再触发重画就是每帧一个死循环。
        let this = cx.entity();
        let rect_pane_id = active_id.clone();

        // 终端区落点预览的档位(只画本组自己那一份)
        let body_zone = self
            .pane_drop
            .as_ref()
            .filter(|(id, _)| dragging && id == &leaf)
            .map(|(_, zone)| *zone);
        let leaf_for_body = leaf.clone();
        let leaf_for_body_drop = leaf.clone();
        let pid_pane_drop = pid.clone();
        let anchor_pane_drop = active_id.clone();

        // ⚠️ 不刷底色:终端区着色**只保留 TerminalArea 根容器一层**(见 render 尾部
        // 那句 `.bg(ui::bg_terminal())`)。原版同款口径 —— `styles.css:151` 与
        // `themePackManager.ts:294`「着色统一由 --bg-terminal 容器层承担,避免
        // 容器/wrapper/xterm 三层叠加」:背景图主题下 bg_terminal 是半透明 rgba,
        // 这里再刷一层就是透明度叠乘,图被盖死(原版 PaneGroup 根也没有背景)。
        //
        // 也**不画边框**:原版 PaneGroup 根没有任何 border(`PaneGroup.tsx:393`),
        // 焦点表达靠激活 tab 的 accent 顶线 + 光标实心/空心两处;此前的
        // group_focused accent 描边是 GPUI 侧自加的,真机上整圈橙线喧宾夺主
        // (用户报障),按原版口径删除。
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(bar_layer)
            .child(
                div()
                    .id(gpui::SharedString::from(format!("pane-body-{leaf}")))
                    .flex_1()
                    .relative()
                    .overflow_hidden()
                    // ── pane 拖到终端区:四边分屏 / 中央并组 ────────────
                    .on_drag_move(cx.listener({
                        let leaf_id = leaf_for_body.clone();
                        move |this: &mut TerminalArea,
                              event: &gpui::DragMoveEvent<crate::dnd::DragPane>,
                              _window,
                              cx| {
                            let dragged = event.drag(cx).clone();
                            this.note_pane_drag_over(
                                &leaf_id,
                                &dragged,
                                event.bounds,
                                event.event.position,
                                cx,
                            );
                        }
                    }))
                    .on_drop(cx.listener({
                        let (pid, leaf_id, anchor) = (
                            pid_pane_drop.clone(),
                            leaf_for_body_drop.clone(),
                            anchor_pane_drop.clone(),
                        );
                        move |this: &mut TerminalArea,
                              item: &crate::dnd::DragPane,
                              window,
                              cx| {
                            this.drop_on_pane_body(&pid, &leaf_id, &anchor, item, window, cx);
                        }
                    }))
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.store.update(cx, |store, cx| {
                            store.focus_pane(&pid_focus, &active_for_focus, window, cx)
                        });
                    }))
                    // ── 拖文件进终端(改造清单 #8 链路③)────────────────
                    //
                    // 两个来源共用这一处落点:文件树的 `DragFilePath` 与资源管理器的
                    // `ExternalPaths`(gpui 把系统 FileDrop 翻译成内部 drag,见 `dnd` 模块)。
                    // 写入走 `AppStore::write_to_pane` —— 它刻意经 `TerminalPane::write`,
                    // 好让 AI 输入检测那条链路看得见这段文本,**不许改成裸 PTY 写**。
                    .on_drag_move(cx.listener({
                        let pane_id = drop_pane_id.clone();
                        move |this: &mut TerminalArea,
                              event: &gpui::DragMoveEvent<crate::dnd::DragFilePath>,
                              _window,
                              cx| {
                            this.note_file_drag_over(&pane_id, event.bounds, event.event.position, cx);
                        }
                    }))
                    .on_drag_move(cx.listener({
                        let pane_id = drop_pane_id.clone();
                        move |this: &mut TerminalArea,
                              event: &gpui::DragMoveEvent<gpui::ExternalPaths>,
                              _window,
                              cx| {
                            this.note_file_drag_over(&pane_id, event.bounds, event.event.position, cx);
                        }
                    }))
                    .on_drop(cx.listener({
                        let (pid, pane_id) = (pid_drop.clone(), drop_pane_id.clone());
                        move |this: &mut TerminalArea,
                              item: &crate::dnd::DragFilePath,
                              window,
                              cx| {
                            let text = crate::dnd::quote_path(&item.0);
                            this.insert_path_into_pane(&pid, &pane_id, &text, window, cx);
                        }
                    }))
                    .on_drop(cx.listener({
                        let (pid, pane_id) = (pid_drop.clone(), drop_pane_id.clone());
                        move |this: &mut TerminalArea,
                              item: &gpui::ExternalPaths,
                              window,
                              cx| {
                            let text = crate::dnd::quote_paths(item.paths());
                            this.insert_path_into_pane(&pid, &pane_id, &text, window, cx);
                        }
                    }))
                    .child(
                        canvas(
                            move |bounds: Bounds<Pixels>, _window, cx| {
                                this.update(cx, |area: &mut TerminalArea, _cx| {
                                    area.pane_rects.insert(
                                        rect_pane_id.clone(),
                                        PaneRect {
                                            pane_id: rect_pane_id.clone(),
                                            left: bounds.origin.x.into(),
                                            top: bounds.origin.y.into(),
                                            width: bounds.size.width.into(),
                                            height: bounds.size.height.into(),
                                        },
                                    );
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .map(|el| match terminal {
                        Some(entity) => el.child(entity),
                        None => el.child(
                            div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(ui::text_muted())
                                .child(t("paneGroup", "starting")),
                        ),
                    })
                    // 「释放以插入路径」的虚线框。与 `cx.has_active_drag()` 与门:
                    // 拖拽被中断时 gpui 会清 active_drag 并重画,残留状态自动失效。
                    .when(file_drop_over, |el| el.child(drop_hint()))
                    // pane 落点预览:center 铺满 = 并入本组,半屏 = 往那个方向分屏。
                    // **落下没有动作的场景压根不会有档位**(判档时就滤掉了,
                    // 见 `note_pane_drag_over`)—— 原版三轮评审的最终口径
                    .children(body_zone.map(zone_overlay))
                    // 远程断线覆盖层:**保留 pane**,点一下在同一 pane 重连
                    .when(show_reconnect, |el| {
                        let (pid, pane_id) = (pid_reconnect.clone(), pane_reconnect.clone());
                        el.child(
                            div()
                                .id(gpui::SharedString::from(format!("reconnect-{leaf}")))
                                .absolute()
                                .inset_0()
                                // 遮罩自己吃点击:底下终端的聚焦监听不该抢走这一下
                                .occlude()
                                .flex()
                                .flex_col()
                                .items_center()
                                .justify_center()
                                .gap(px(12.0))
                                // 原版 `bg-black/55 backdrop-blur-[1px]`;gpui 没有
                                // backdrop-filter,只留半透明黑
                                .bg(gpui::rgba(0x0000008c))
                                .child(
                                    div()
                                        .text_size(ui::font_px(12.0))
                                        .text_color(ui::text_secondary())
                                        .child(t("paneGroup", "remoteDisconnected")),
                                )
                                .child(
                                    ui::ghost_button(
                                        gpui::SharedString::from(format!("reconnect-btn-{leaf}")),
                                        t("paneGroup", "reconnect"),
                                    )
                                    .on_click(cx.listener(
                                        move |this: &mut TerminalArea, _event, window, cx| {
                                            let (pid, pane_id) = (pid.clone(), pane_id.clone());
                                            this.store.update(cx, |store, cx| {
                                                // 一步含:kill 旧 PTY / 清标记与退出登记 /
                                                // 起新 PTY / 回写 pane(见 store 那边的注释)
                                                store.reset_pane_for_reconnect(&pid, &pane_id, cx);
                                                store.focus_pane(&pid, &pane_id, window, cx);
                                            });
                                        },
                                    )),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    // ─── pane 拖拽的四个落点处理(v0.14.0)─────────────────────
    //
    // 见 [`crate::dnd`] 模块注释第 2 条:`on_drag_move` 会打给**每一个**注册者
    // (不只鼠标底下那个),所以四个处理里第一句都是自己的命中判定。

    /// 终端区落点判档。
    ///
    /// **落下没有动作的场景一律不给预览**(原版三轮评审的最终口径,
    /// `PaneGroup.tsx::handleBodyDragMove`):
    /// - 独占一组的 pane 拖回自己身上 —— 四边等价原位、中央本就在同组;
    /// - 拖到自己所在组的中央 —— 已经在这一组里了。
    ///
    /// 这两条与 [`SplitNode::move_pane_in_layout`] 返回 `None` 的条件严格同集,
    /// 所以「有预览 ⟺ 落下有动作」,不会出现指示了却静默无动作。
    fn note_pane_drag_over(
        &mut self,
        leaf_id: &str,
        dragged: &crate::dnd::DragPane,
        bounds: Bounds<Pixels>,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let mut next = None;
        if self.accepts_pane_drag(dragged, cx)
            && let Some(zone) = crate::dnd::pane_drop_zone(bounds, position)
            && self.zone_has_effect(leaf_id, dragged, zone, cx)
        {
            next = Some((leaf_id.to_string(), zone));
        }

        // 不命中本组时只收自己那一份,别人的留给别人清
        if next.is_none() && self.pane_drop.as_ref().is_none_or(|(id, _)| id != leaf_id) {
            return;
        }
        if self.pane_drop != next {
            self.pane_drop = next;
            cx.notify();
        }
    }

    /// 终端区落地:读上一帧存下的档位,交给 store 的纯函数变换。
    ///
    /// 锚点用**本组当前激活的 pane**(原版 `movePane(…, node.activePaneId, …)`),
    /// `move_pane_in_layout` 内部会在锚点恰是被拖 pane 时自动换锚。
    fn drop_on_pane_body(
        &mut self,
        project_id: &str,
        leaf_id: &str,
        anchor_pane_id: &str,
        dragged: &crate::dnd::DragPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let zone = self
            .pane_drop
            .take()
            .filter(|(id, _)| id == leaf_id)
            .map(|(_, zone)| zone);
        self.pane_drag = None;
        cx.notify();
        let (Some(zone), true) = (zone, self.accepts_pane_drag(dragged, cx)) else {
            return;
        };
        let (pid, pane_id, anchor) = (
            project_id.to_string(),
            dragged.pane_id.clone(),
            anchor_pane_id.to_string(),
        );
        self.store.update(cx, |store, cx| {
            store.move_pane(&pid, &pane_id, &anchor, zone, window, cx);
        });
    }

    /// tab 栏插入位判定。
    ///
    /// 本组只有被拖的这一个 tab 时**不给指示线** —— 组内换位无意义,落下也不会
    /// 有动作,画了就是「指示了却静默无动作」的口径不一致(原版评审结论)。
    fn note_tab_drag_over(
        &mut self,
        leaf_id: &str,
        dragged: &crate::dnd::DragPane,
        bounds: Bounds<Pixels>,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let mut next = None;
        if bounds.contains(&position)
            && self.accepts_pane_drag(dragged, cx)
            && let Some(slots) = self.tab_slots(leaf_id, dragged, cx)
        {
            let (index, line_x) = crate::dnd::tab_insert_index(&slots, f32::from(position.x));
            next = Some((
                leaf_id.to_string(),
                index,
                line_x - f32::from(bounds.origin.x),
            ));
        }

        if next.is_none() && self.tab_drop.as_ref().is_none_or(|(id, ..)| id != leaf_id) {
            return;
        }
        if self.tab_drop != next {
            self.tab_drop = next;
            cx.notify();
        }
    }

    /// tab 栏落地:按插入位落子。
    ///
    /// 锚点必须是**本组里不是被拖 pane 的那一个**:它只用来定位目标叶子,
    /// 找不到就说明本组只有被拖的这一个 tab,组内换位无意义(与判档同一道闸)。
    fn drop_on_tab_bar(
        &mut self,
        project_id: &str,
        leaf_id: &str,
        dragged: &crate::dnd::DragPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = self
            .tab_drop
            .take()
            .filter(|(id, ..)| id == leaf_id)
            .map(|(_, index, _)| index);
        self.pane_drag = None;
        cx.notify();
        let (Some(index), true) = (index, self.accepts_pane_drag(dragged, cx)) else {
            return;
        };
        let Some(anchor) = self
            .store
            .read(cx)
            .project_state(project_id)
            .and_then(|s| s.active_layout())
            .and_then(|l| l.node(leaf_id))
            .and_then(|node| match node {
                SplitNode::Leaf { panes, .. } => {
                    panes.iter().find(|p| p.id != dragged.pane_id).map(|p| p.id.clone())
                }
                _ => None,
            })
        else {
            return;
        };
        let (pid, pane_id) = (project_id.to_string(), dragged.pane_id.clone());
        self.store.update(cx, |store, cx| {
            store.move_pane_to_tab(&pid, &pane_id, &anchor, index, window, cx);
        });
    }

    /// 本组能不能接这次 pane 拖拽(原版 `acceptsPaneDrag`):只接**同项目**的。
    fn accepts_pane_drag(&self, dragged: &crate::dnd::DragPane, cx: &App) -> bool {
        self.store.read(cx).active_project_id.as_deref() == Some(dragged.project_id.as_str())
    }

    /// 这一档落下去会不会真有动作 —— 判据与 [`SplitNode::move_pane_in_layout`]
    /// 返回 `None` 的条件严格同集(见 [`Self::note_pane_drag_over`] 的说明)。
    fn zone_has_effect(
        &self,
        leaf_id: &str,
        dragged: &crate::dnd::DragPane,
        zone: DropZone,
        cx: &App,
    ) -> bool {
        let store = self.store.read(cx);
        let Some(layout) = store
            .project_state(&dragged.project_id)
            .and_then(|s| s.active_layout())
        else {
            return false;
        };
        let Some(SplitNode::Leaf { panes, .. }) = layout.node(leaf_id) else {
            return false;
        };
        if !panes.iter().any(|p| p.id == dragged.pane_id) {
            // 别的组:哪一档都有动作
            return true;
        }
        // 自己就在这一组里:独占一组时四档全是空操作,多 tab 时只有中央是
        !(panes.len() == 1 || zone == DropZone::Center)
    }

    /// 本组各 tab 的横向区间(屏幕坐标,按 tab 顺序)。
    ///
    /// 返回 `None` = 这一组不该给指示线:布局里找不到、或组里只有被拖的这一个 tab。
    /// 矩形取自 [`Self::tab_rects`](每个 tab 挂的量尺 canvas),等价于原版
    /// `bar.querySelectorAll('[data-pane-tab]')` 那一趟。
    fn tab_slots(
        &self,
        leaf_id: &str,
        dragged: &crate::dnd::DragPane,
        cx: &App,
    ) -> Option<Vec<(f32, f32)>> {
        let store = self.store.read(cx);
        let layout = store
            .project_state(&dragged.project_id)
            .and_then(|s| s.active_layout())?;
        let SplitNode::Leaf { panes, .. } = layout.node(leaf_id)? else {
            return None;
        };
        if !panes.iter().any(|p| p.id != dragged.pane_id) {
            return None;
        }
        // 有一个 tab 还没量到就整份作废:区间与 tab 顺序必须**一一对应**,
        // 缺一个会让后面所有的插入位错一格(比不给指示线糟得多)
        panes
            .iter()
            .map(|p| {
                self.tab_rects
                    .get(&p.id)
                    .map(|r| (f32::from(r.origin.x), f32::from(r.origin.x + r.size.width)))
            })
            .collect()
    }

    /// `on_drag_move` 的落点记录。见 [`crate::dnd`] 模块注释第 2 条:这个回调会
    /// 打给**每一个**注册者(不只鼠标底下那个),命中判定必须自己做。
    fn note_file_drag_over(
        &mut self,
        pane_id: &str,
        bounds: Bounds<Pixels>,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let hit = bounds.contains(&position);
        let next = if hit {
            Some(pane_id.to_string())
        } else if self.file_drop_pane.as_deref() == Some(pane_id) {
            // 只收自己那一份,别人的留给别人清
            None
        } else {
            return;
        };
        if self.file_drop_pane != next {
            self.file_drop_pane = next;
            cx.notify();
        }
    }

    /// 把路径文本当作用户键入写进 pane,并把键盘还给终端(原版 `term.focus()`)。
    fn insert_path_into_pane(
        &mut self,
        project_id: &str,
        pane_id: &str,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.file_drop_pane = None;
        if text.is_empty() {
            cx.notify();
            return;
        }
        self.store.update(cx, |store, cx| {
            store.write_to_pane(project_id, pane_id, text, cx);
            store.focus_pane(project_id, pane_id, window, cx);
        });
        cx.notify();
    }
}

/// 拖文件悬停时盖在终端上的虚线提示框(`TerminalInstance.tsx:430-442`)。
fn drop_hint() -> AnyElement {
    div()
        .absolute()
        .inset(px(4.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .border_2()
        .border_dashed()
        .border_color(ui::accent())
        .bg(ui::accent_subtle())
        .child(
            div()
                .px(px(12.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .bg(ui::bg_overlay())
                .text_size(ui::font_px(9.75))
                .text_color(ui::accent())
                .child(t("terminal", "dropToInsertPath")),
        )
        .into_any_element()
}

/// pane 拖拽的落点预览:半透明 accent 盖住「松手之后这块地归谁」。
///
/// 逐条对照 `PaneGroup.tsx:529-535` 的 `overlayRect`:center 铺满(= 并入本组的
/// tab 栏),四边各占**半屏**(= 往那个方向分屏之后新格子的位置)。
/// 注意四边预览是**半屏**而不是判档用的 1/4 进深 —— 前者画的是结果,后者是手感。
fn zone_overlay(zone: DropZone) -> AnyElement {
    let base = div()
        .absolute()
        .border_2()
        .border_color(ui::accent())
        .bg(ui::with_alpha(ui::accent(), 0.2));
    match zone {
        DropZone::Center => base.inset_0(),
        DropZone::Left => base.top_0().bottom_0().left_0().w(gpui::relative(0.5)),
        DropZone::Right => base.top_0().bottom_0().right_0().w(gpui::relative(0.5)),
        DropZone::Top => base.left_0().right_0().top_0().h(gpui::relative(0.5)),
        DropZone::Bottom => base.left_0().right_0().bottom_0().h(gpui::relative(0.5)),
    }
    .into_any_element()
}

/// `paneGroup.markerTooltip` 里 `{mod}` 的取值。与 `search_modal.rs:324-326`
/// 那一份同源(那边是私有的,不为一行去开放它)。
fn mod_label() -> &'static str {
    if cfg!(target_os = "macos") { "⌘" } else { "Ctrl" }
}

/// 标记浮层还该开着吗:那个 pane 得还在布局里、pty 没换,而且**仍是所在叶子的
/// 激活 tab**。
///
/// 对应原版 `PaneGroup.tsx:306-308` 的
/// `useEffect(() => setMarkerOpen(false), [activePane?.ptyId])` —— 切 tab、
/// 关 pane、分屏切换都靠它收场(浮层里那份列表是**激活 pane** 的,换了人还开着
/// 就是在看别人的标记)。
fn marker_popover_alive(layout: &SplitNode, pane_id: &str, pty_id: u32) -> bool {
    let Some(SplitNode::Leaf {
        panes,
        active_pane_id,
        ..
    }) = layout.leaf_of_pane(pane_id)
    else {
        return false;
    };
    // 激活 tab 的解析与 render_leaf 同口径:找不到就退回第一个
    let active = panes
        .iter()
        .find(|p| &p.id == active_pane_id)
        .or_else(|| panes.first());
    active.is_some_and(|p| p.id == pane_id && p.pty_id == Some(pty_id))
}

/// 点击次数(键盘触发的「点击」按一次算)。
pub(crate) fn click_count(event: &ClickEvent) -> usize {
    match event {
        ClickEvent::Mouse(e) => e.up.click_count,
        ClickEvent::Keyboard(_) => 1,
    }
}

/// 点击位置(弹菜单要它)。键盘触发的「点击」没有坐标,退回当前鼠标位置 ——
/// 菜单总得有个锚点,而这一条在真机上走不到(那个 `+` 没有键盘入口)。
pub(crate) fn click_position(event: &ClickEvent, window: &Window) -> gpui::Point<gpui::Pixels> {
    match event {
        ClickEvent::Mouse(e) => e.up.position,
        ClickEvent::Keyboard(_) => window.mouse_position(),
    }
}

impl Render for TerminalArea {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 塌陷/关闭掉的节点的分隔条状态在这里回收 —— 不清的话每分一次屏就多留
        // 一个 Entity(极小但确实的泄漏,看板已记)。
        let live_nodes = self.store.read(cx).live_node_ids();
        self.split_states.retain(|id, _| live_nodes.contains(id));
        // 拖拽结束后借这一帧清掉落点残留(**不 notify**,正在渲染)。
        // 高亮另外还与 `has_active_drag` 与门,见 `render_leaf`。
        if !cx.has_active_drag() {
            self.file_drop_pane = None;
            self.pane_drag = None;
            self.pane_drop = None;
            self.tab_drop = None;
        }

        // 切走项目 / 关光了终端 → 浮层无处可挂。下面两条早退路径压根走不到浮层
        // 组装那一步,不在这里收掉的话覆盖物栈里会留一条永远摘不掉的登记。
        if self.marker_open.is_some() && self.store.read(cx).active_layout().is_none() {
            self.close_marker_popover(window, cx);
        }

        let store = self.store.read(cx);
        let Some(project) = store.active_project() else {
            // 一个项目都没有 = 首启,换成引导页(audit #30 的 FirstRunGuide):
            // 原版判据就是 `config.projects.length === 0`(`App.tsx:534`),
            // 没有任何「首启标记」字段,添完项目自然消失。
            //
            // 「有项目但没选中」不走引导页 —— 原版那种情形是**整块空白**
            // (FirstRunGuide 不显示、每个 TerminalArea 都 display:none),
            // 下面这句是 GPUI 侧原有的兜底提示,保持不动。
            if store.config().projects.is_empty() {
                return crate::first_run::guide(self.store.clone());
            }
            return div()
                .size_full()
                .bg(ui::bg_terminal())
                .flex()
                .items_center()
                .justify_center()
                .text_color(ui::text_muted())
                .text_size(ui::font_px(13.0))
                .child(t("app", "emptyState"));
        };
        let project_id = project.id.clone();
        let project_name = project.name.clone();
        let layout = store.active_layout().cloned();

        let Some(layout) = layout else {
            let pid = project_id.clone();
            return div()
                .size_full()
                .bg(ui::bg_terminal())
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(14.0))
                .child(
                    div()
                        .text_color(ui::text_secondary())
                        .text_size(ui::font_px(13.0))
                        .child(tr!("terminalArea", "emptyTitle", project = project_name)),
                )
                .child(
                    div()
                        .id("empty-new-terminal")
                        .px(px(18.0))
                        .py(px(8.0))
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(ui::border_default())
                        .text_color(ui::text_muted())
                        .text_size(ui::font_px(13.0))
                        .cursor_pointer()
                        .hover(|el| el.border_color(ui::accent()).text_color(ui::accent()))
                        // 空态那颗按钮**也弹 shell 选择菜单**(`TerminalArea.tsx:32-46`
                        // 的 `handleNewTabClick`,与 tab 栏那颗「+」逐字同形、同一份
                        // `config.availableShells` 数据源、同一道 `<= 1` 闸)。
                        // 唯一差别是 anchor:空态没有「当前 pane」可挨着放,传 None
                        // (原版那边也是 `newTerminal(projectId)` 不带 targetPaneId)。
                        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                            let shells = this.store.read(cx).config().available_shells.clone();
                            if shells.len() <= 1 {
                                this.store.update(cx, |store, cx| {
                                    store.new_terminal(&pid, None, None, window, cx);
                                });
                                return;
                            }
                            let entries: Vec<menu::MenuEntry> = shells
                                .into_iter()
                                .map(|shell| {
                                    let store = this.store.clone();
                                    let pid = pid.clone();
                                    let name = shell.name.clone();
                                    menu::item(name, move |window, cx| {
                                        let (pid, shell) = (pid.clone(), shell.clone());
                                        store.update(cx, |store, cx| {
                                            store.new_terminal(&pid, Some(shell), None, window, cx);
                                        });
                                    })
                                })
                                .collect();
                            menu::show(click_position(event, window), entries, window, cx);
                        }))
                        .child(format!("+ {}", t("terminalArea", "newTerminal"))),
                )
                // 键位提示是**独立一行**(原版 `TerminalArea.tsx:85-88`):
                // 「也可以按」+ 一颗键帽,不是塞进按钮文字里的括号。串从键位表取,
                // 改键位不会漏这里(与首启引导同一条路)。
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(t("terminalArea", "emptyHint"))
                        .child(ui::kbd(crate::hotkeys::hotkey_label("newTerminal"))),
                );
        };

        // 双击最大化(v0.14.0):只渲染目标叶子,整树其余 pane 的**终端主体**
        // 不进元素树 —— 它们只留一条 26px 的折叠标题条码在底部
        // (见 [`Self::render_maximized`];v0.14.0 时是整组消失,连状态灯都没有)。
        //
        // ⚠️ **终端实体不受影响** —— `TerminalPane` 按 `pty_id` 挂在
        // `AppStore::terminals` 表里(旧版 `terminalCache` 的等价物),这里只是
        // 换个容器把同一个 `Entity` 画出来:PTY 不断、回滚缓冲不丢、光标不动。
        // 这就是原版 `getNodeKey` 那轮修复要保住的东西,GPUI 侧由「实体不在树里」
        // 这条结构天然成立(移动 / 重排 / 最大化都只动布局树的形状)。
        //
        // `maximized_pane_id()` 自带「布局是 split」这道闸;pane 被关掉后按 id 查不到
        // 叶子,自然回落整树(store 那边还会顺手把陈旧 id 清掉)。
        let maximized_leaf = self
            .store
            .read(cx)
            .maximized_pane_id(&project_id)
            .and_then(|id| layout.leaf_of_pane(id))
            .cloned();

        // 关掉的 pane 的矩形残影一并清掉,免得方向导航挑到不存在的格子。
        // **最大化时只留被铺满那一组的矩形**:方向导航挑的是「屏幕上相邻的格子」,
        // 藏起来的那些格子不该被挑中(原版 `findAdjacentPtyId` 查的是 DOM,
        // 卸载掉的 PaneGroup 天然查不到,这里把那条性质补回来)。
        // 折叠标题条**只写 `tab_rects` 不写 `pane_rects`**,这条性质原样成立 ——
        // 条上没有终端主体,方向导航跳过去也没有格子可落。
        let alive: std::collections::HashSet<String> = match &maximized_leaf {
            Some(leaf) => leaf.panes().into_iter().map(|p| p.id.clone()).collect(),
            None => layout.panes().into_iter().map(|p| p.id.clone()).collect(),
        };
        self.pane_rects.retain(|id, _| alive.contains(id));
        // tab 焦点句柄与 tab 矩形按**整棵树**回收(切项目/关 tab 之后那些行不在了)。
        // ⚠️ 句柄必须跨帧稳定,不能每帧重建 —— 那样 Tab 过去的焦点每帧都会丢;
        // 折叠掉的组照样在画(标题条上那些 tab 要焦点、要缩略图锚点),
        // 更不跟着 `alive` 收窄。
        let in_layout: std::collections::HashSet<String> =
            layout.panes().into_iter().map(|p| p.id.clone()).collect();
        self.tab_focus.retain(|id, _| in_layout.contains(id));
        self.tab_rects.retain(|id, _| in_layout.contains(id));

        // 首帧只量不画:百分比要按真实可用尺寸换算,而 ResizablePanel 只认第一帧的
        // 初值(见模块注释)。量到之后主动 notify 一次,下一帧把分屏树铺上去。
        let content = self.measured.then(|| match &maximized_leaf {
            Some(leaf) => self.render_maximized(&layout, leaf, &project_id, window, cx),
            None => self.render_node(&layout, &project_id, self.area_size, window, cx),
        });
        // 浮层在分屏树**之后**组装:它要读 render_node 刚更新过的 pane 矩形,
        // 而且要画在所有常规内容之上(deferred priority 1)
        let marker_popover = self.render_marker_popover(&layout, window, cx);
        let tab_preview = self.render_tab_preview(&layout, window, cx);
        let this = cx.entity();
        div()
            .size_full()
            .bg(ui::bg_terminal())
            .flex()
            .relative()
            // Esc 中途取消 pane 拖拽(X 批「Esc 取消未做」的结清)。
            //
            // **必须是捕获相**:按键沿「根 → 焦点节点」下行时先经过这里,而焦点
            // 在终端上、`TerminalView` 会把 Esc 翻成 `\x1b` 写进 PTY 并
            // `stop_propagation` —— 冒泡相挂在这里根本收不到。原版那句
            // `window.addEventListener('keydown', onKeyDown, true)` 是同一个道理
            // (`paneDragState.ts` 里点名了这个坑)。
            //
            // 只在**真有 pane 拖拽在飞**时吞掉这次 Esc:没拖拽时照常放行,
            // 终端里按 Esc 的行为一个字节都不变。
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key != "escape" || this.pane_drag.is_none() {
                    return;
                }
                this.pane_drag = None;
                this.pane_drop = None;
                this.tab_drop = None;
                cx.stop_active_drag(window);
                cx.stop_propagation();
                cx.notify();
            }))
            .child(
                canvas(
                    move |bounds: Bounds<Pixels>, _window, cx| {
                        this.update(cx, |area: &mut TerminalArea, cx| {
                            if bounds.size.width > px(0.0) && bounds.size.height > px(0.0) {
                                let first = !area.measured;
                                area.area_size = bounds.size;
                                area.measured = true;
                                // 只在第一次量到时唤起重画 —— 之后每帧都 notify
                                // 就是个死循环
                                if first {
                                    cx.notify();
                                }
                            }
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .children(content.map(|c| div().size_full().child(c)))
            .children(marker_popover)
            // 非激活 tab 的悬停缩略图。卡不带 `.id()` → 无 hitbox → 不吃鼠标
            .children(tab_preview)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 存的百分比正常时按它切,和为 1。
    #[test]
    fn 比例按存的百分比换算() {
        let f = split_fractions(&[30.0, 70.0], 2);
        assert!((f[0] - 0.3).abs() < 1e-9 && (f[1] - 0.7).abs() < 1e-9);
        // 和不是 100 的老数据(拖动写回时有浮点误差)照样归一
        let f = split_fractions(&[1.0, 1.0, 2.0], 3);
        assert_eq!(f, vec![0.25, 0.25, 0.5]);
    }

    /// 进场只淡入:终点必须是**完全**不透明,越界进度一律钳住
    /// (残留的 0.99 会让整块 pane 永远蒙一层灰)。
    #[test]
    fn 进场淡入端点严丝合缝() {
        assert_eq!(pane_enter_opacity(0.0), 0.0);
        assert_eq!(pane_enter_opacity(1.0), 1.0);
        assert_eq!(pane_enter_opacity(1.5), 1.0);
        assert_eq!(pane_enter_opacity(-0.5), 0.0);
        assert!((pane_enter_opacity(0.5) - 0.5).abs() < 1e-6);
    }

    /// 进场动画属原版 reduce 段**点名豁免**的那一档:开着减弱动效照播。
    #[test]
    fn 进场动画不受减弱动效影响() {
        assert!(
            !mt_ui::motion::PANE_ENTER.respects_reduce,
            "styles.css:441-443 明确豁免 .pane-enter"
        );
        assert_eq!(
            mt_ui::motion::PANE_ENTER.duration,
            std::time::Duration::from_millis(260),
            "--motion-pane-enter: 0.26s"
        );
        crate::motion::with_reduce(true, || {
            let spec = mt_ui::motion::PANE_ENTER;
            assert!(spec.running_at(std::time::Duration::ZERO, true));
            assert!(spec.progress_at(std::time::Duration::ZERO, true) < 0.01);
        });
    }

    /// 子节点数与存的百分比对不上 / 有非法值 → 均分,不许拿 0 去乘。
    #[test]
    fn 比例对不上时均分() {
        assert_eq!(split_fractions(&[30.0, 70.0], 3), vec![1.0 / 3.0; 3]);
        assert_eq!(split_fractions(&[], 2), vec![0.5, 0.5]);
        assert_eq!(split_fractions(&[0.0, 100.0], 2), vec![0.5, 0.5]);
        assert_eq!(split_fractions(&[f64::NAN, 1.0], 2), vec![0.5, 0.5]);
        assert!(split_fractions(&[50.0], 0).is_empty());
    }

    /// 拖完的像素换回百分比,和恒为 100。
    #[test]
    fn 像素换回百分比和为一百() {
        let pct = sizes_to_percent(&[300.0, 700.0]).unwrap();
        assert!((pct[0] - 30.0).abs() < 1e-9 && (pct[1] - 70.0).abs() < 1e-9);
        assert!((pct.iter().sum::<f64>() - 100.0).abs() < 1e-9);
    }

    /// 还没量出来 / 全是 0 时不写回 —— 写进去下次恢复就全退化成均分了。
    #[test]
    fn 总和非正时不写回() {
        assert!(sizes_to_percent(&[0.0, 0.0]).is_none());
        assert!(sizes_to_percent(&[]).is_none());
        assert!(sizes_to_percent(&[f64::NAN]).is_none());
    }

    /// tab 右键菜单的项序照抄原版(没有 AI 会话的那一支:分支段整段不出),
    /// 两条分隔线。
    #[test]
    fn tab_右键菜单项序与原版一致() {
        use TabMenuAction::*;
        let actions = tab_menu_actions(false, false);
        assert_eq!(
            actions,
            vec![
                Some(Rename),
                None,
                Some(SplitRight),
                Some(SplitDown),
                None,
                Some(CloseTab),
                Some(ClosePane),
            ]
        );
        assert_eq!(actions.iter().filter(|a| a.is_none()).count(), 2);
    }

    /// 分支段的两种形态各自插在**分屏两项之后、关闭段之前**,各带一条前导分隔线
    /// (逐条对照 `PaneGroup.tsx:354-372`)。
    #[test]
    fn tab_菜单分支段项序() {
        use TabMenuAction::*;
        // 有会话身份 + 有 fork 能力位
        assert_eq!(
            tab_menu_actions(true, false),
            vec![
                Some(Rename),
                None,
                Some(SplitRight),
                Some(SplitDown),
                None,
                Some(ForkSession),
                Some(ViewSessionBranches),
                None,
                Some(CloseTab),
                Some(ClosePane),
            ]
        );

        // 输入检测认出 AI 但没拿到 hook 身份 —— 置灰提示占同一个位置
        assert_eq!(
            tab_menu_actions(false, true),
            vec![
                Some(Rename),
                None,
                Some(SplitRight),
                Some(SplitDown),
                None,
                Some(ForkNeedsIdentity),
                None,
                Some(CloseTab),
                Some(ClosePane),
            ]
        );

        // 两者互斥(有身份就不会缺身份),真同时为真也不该塌成畸形菜单:
        // 段与段之间各自带分隔线,关闭段照旧在最后
        let both = tab_menu_actions(true, true);
        assert_eq!(both.last(), Some(&Some(ClosePane)));
        assert_eq!(both.iter().filter(|a| a.is_none()).count(), 4);
    }

    /// 菜单显隐的判据与 `session_branch` 那份纯逻辑同源 —— 两处漂了就会出现
    /// 「菜单出了 fork 项、点下去什么都不发生」。
    #[test]
    fn tab_菜单分支段判据取自能力位表() {
        use crate::tree::AiSessionRef;
        let id = "0199a1b2-c3d4-7e8f-9012-3456789abcde";
        let session = AiSessionRef {
            agent: Some("claude-code".into()),
            session_id: id.into(),
            cwd: None,
        };
        assert!(matches!(
            branch_menu_segment(Some(&session), None),
            BranchMenuSegment::Fork { .. }
        ));
        // 输入检测到 claude 但没身份 → 置灰提示那一支
        assert_eq!(
            branch_menu_segment(None, Some("claude")),
            BranchMenuSegment::NeedsIdentity
        );
        // 普通 shell:整段不出
        assert_eq!(branch_menu_segment(None, None), BranchMenuSegment::None);
    }

    /// 快捷键标签与 `main.rs` 里绑的键位一致(改键位时这条会提醒改标签)。
    #[test]
    fn tab_菜单快捷键标签() {
        if cfg!(target_os = "macos") {
            return;
        }
        assert_eq!(hotkey_label(false, false, false, "F2"), "F2");
        assert_eq!(hotkey_label(true, true, false, "D"), "Ctrl+Shift+D");
        assert_eq!(hotkey_label(true, true, false, "E"), "Ctrl+Shift+E");
        assert_eq!(hotkey_label(true, true, false, "W"), "Ctrl+Shift+W");
    }

    /// marker 按钮的锚点由控件簇的布局常量算出(原版是量 DOM 矩形)。
    /// 加减控件时这条会提醒同步改 [`MARKER_ANCHOR_INSET`] / [`marker_anchor_inset`]。
    #[test]
    fn 标记浮层锚点按控件簇布局算() {
        // 右侧簇:px-6 + 四个 22×22 方钮(查找/分屏右/分屏下/关整组,各带 2px gap)
        // + marker 自己的 4px 右边距;查找钮与 marker 同以「有 pty」为前提,
        // 浮层用到锚点时四钮必然齐
        assert_eq!(MARKER_ANCHOR_INSET, 6.0 + 4.0 * 24.0 + 4.0);
        assert_eq!(MARKER_ANCHOR_INSET, 106.0);
        assert_eq!(marker_anchor_inset(false), 106.0);
        // 分了屏时簇里多一颗「最大化 / 还原」(22 + 2 gap),锚点跟着往左让
        assert_eq!(marker_anchor_inset(true), 106.0 + 24.0);
        assert_eq!(marker_anchor_inset(true), 130.0);
        // 面板右缘贴按钮右缘 → 左缘 = 叶右缘 - inset - 面板宽
        let leaf_right = 1000.0_f32;
        let left = leaf_right - marker_anchor_inset(false) - MARKER_PANEL_WIDTH;
        assert_eq!(left, 1000.0 - 106.0 - 300.0);
    }

    /// 浮层里那些写死的像素尺寸**必须跟着界面字号一起缩放**。
    ///
    /// 回归:写死 `px` 的那一版有实际后果 —— 用户把 `uiFontSize` 调大之后,
    /// 36px 的时间列装不下 `15:29`,五个字符被折成了两行。
    #[test]
    fn 标记浮层尺寸跟着界面字号缩放() {
        // 默认基准下就是常量本身(所以锚点那条测试拿裸常量算仍然成立)
        assert_eq!(f32::from(crate::ui::font_px(MARKER_TIME_W)), MARKER_TIME_W);
        assert_eq!(f32::from(crate::ui::font_px(MARKER_SEQ_W)), MARKER_SEQ_W);

        // 调到滑块上限:三个尺寸都得跟着变大,不能纹丝不动
        crate::ui::set_ui_font(20.0, None);
        for (base, name) in [
            (MARKER_TIME_W, "时间列"),
            (MARKER_SEQ_W, "序号列"),
            (MARKER_PANEL_WIDTH, "面板宽"),
        ] {
            let scaled = f32::from(crate::ui::font_px(base));
            assert!(scaled > base, "{name}在大字号下必须变宽,实际 {scaled}");
        }
        // 「装不装得下」的真正判据是**列宽相对正文字号的倍数**,它必须恒定
        let ratio = f32::from(crate::ui::font_px(MARKER_TIME_W))
            / f32::from(crate::ui::font_px(12.0));
        assert!(
            (ratio - MARKER_TIME_W / 12.0).abs() < 0.01,
            "列宽与字号脱钩了,实际倍数 {ratio}"
        );

        crate::ui::set_ui_font(13.0, None);
    }

    /// 浮层的存活判据:pane 还在、还是激活 tab、pty 没换。
    #[test]
    fn 切换激活_tab_后浮层判定为该关() {
        use crate::tree::PaneState;

        let mut first = PaneState::new("pwsh");
        first.pty_id = Some(7);
        let first_id = first.id.clone();
        let mut layout = SplitNode::leaf(first);

        let mut second = PaneState::new("pwsh");
        second.pty_id = Some(8);
        let second_id = second.id.clone();
        layout.append_pane(Some(&first_id), second);

        // append_pane 会把新 tab 设成激活的 → 原来那条浮层该关
        assert!(layout.activate_pane(&first_id));
        assert!(marker_popover_alive(&layout, &first_id, 7));
        assert!(!marker_popover_alive(&layout, &second_id, 8), "不是激活 tab");

        assert!(layout.activate_pane(&second_id));
        assert!(!marker_popover_alive(&layout, &first_id, 7), "切走了就该关");
        assert!(marker_popover_alive(&layout, &second_id, 8));

        // pty 换了(重连 / 重建)同样算不在了
        assert!(!marker_popover_alive(&layout, &second_id, 99));
        // pane 压根不在布局里
        assert!(!marker_popover_alive(&layout, "pane-nonexistent", 7));
    }

    /// 换算一圈回来不变形:百分比 → 像素 → 百分比。
    #[test]
    fn 百分比像素往返不变形() {
        let stored = [20.0, 55.0, 25.0];
        let area = 1234.0_f64;
        let pixels: Vec<f64> = split_fractions(&stored, 3).iter().map(|f| f * area).collect();
        let back = sizes_to_percent(&pixels).unwrap();
        for (a, b) in stored.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1e-9, "{a} vs {b}");
        }
    }
}
