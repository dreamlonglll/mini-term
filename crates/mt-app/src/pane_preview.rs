//! 悬停缩略图两张卡(`src/components/ProjectPanePreview.tsx` + `PaneTabPreview.tsx`)。
//!
//! - **项目行卡**:悬停项目列表某一行 250ms 后弹出,按该项目的 `SplitNode` 树
//!   等比嵌套排布出「微缩布局拼图」,每个分屏叶子画 active tab 的画面,隐藏 tab
//!   以 `+N` 徽章示数并附其中最高优先级的状态点。开闸权在调用方
//!   ([`has_ai_pane`]):只有跑着 AI 会话的项目才弹 —— 卡要回答的是「AI 在别的
//!   项目里跑到哪一步了」,普通 shell 项目不该被一张 520px 的卡打断视线。
//! - **tab 卡**:悬停**非激活** tab 250ms 后弹出,单格版、**不做 AI 开闸**
//!   (隐藏 tab 的内容无论跑不跑 AI 都同样不可见)。
//!
//! 两张卡都是**纯展示**:容器一律不带 `.id()` → 没有 hitbox → 不参与命中,
//! 等价于原版的 `pointer-events: none`。画面本身走
//! [`mt_ui::MiniTerminalElement`](mt_ui::MiniTerminalElement)(只读快照,不接
//! 输入、不接滚动、不动 grid 尺寸),缓存与失效见那边的模块注释。
//!
//! # 与原版的三处刻意偏差
//!
//! 1. **拼图分格用像素算,不用 `flex-grow`**:gpui 的 `Styled` 没有「设任意
//!    grow 系数」的口子(只有 `flex_grow()` = 1)。好在卡的尺寸是定值,直接
//!    按比例把像素分下去([`split_child_sizes`]),结果与原版的 `flexGrow: size`
//!    等价且可单测。
//! 2. **「已断开」判据取 [`AppStore::is_pty_exited`]**,那是本批新补的
//!    `exitedPtyIds` 等价物(`pty-exit` 登记),与原版同一时机同一集合。
//! 3. 原版卡上的 `menuPopIn` 进场(在 `prefers-reduced-motion` 豁免名单里)
//!    已经补上,**只是不缩放**:两张卡都收一个 `progress` 入参,由持有
//!    [`mt_ui::motion::Transition`] 的调用方(项目列表 / 终端区)喂进来 ——
//!    卡本身是纯函数,没有地方挂状态。`scale(0.96)` 丢掉的理由见
//!    [`mt_ui::motion::menu_pop_in`](缩放在 gpui 里只能改尺寸,而这张卡里
//!    `MiniTerminalElement` 会跟着每帧反解一次字号)。

use std::sync::Arc;

use gpui::{
    AnyElement, App, Bounds, Div, IntoElement, ParentElement, Pixels, Size, Styled, div,
    prelude::FluentBuilder, px, size,
};
use mt_terminal::TerminalEmulator;
use mt_ui::icons::{AiVendor, BrandIcon};
use mt_ui::{MiniTerminalElement, TerminalStyle, TerminalTheme};

use crate::i18n::t;
use crate::store::AppStore;
use crate::tree::{PaneState, PaneStatus, SplitDirection, SplitNode};
use crate::ui;

/// 悬停到弹出的延迟(`ProjectList.tsx:472` / `PaneGroup.tsx:254` 的 250ms)。
/// **是交互延迟不是动画**,不受「减少动画」影响。
pub const HOVER_DELAY_MS: u64 = 250;

/// 项目行卡宽度(`ProjectPanePreview.tsx:34` 的 `CARD_WIDTH`)。
pub const CARD_WIDTH: f32 = 520.0;
/// 拼图区固定高(同文件 `BOARD_HEIGHT`):约合终端区 3:2 观感。
pub const BOARD_HEIGHT: f32 = 340.0;
/// tab 卡尺寸(`PaneTabPreview.tsx:21-22`)。
pub const TAB_CARD_WIDTH: f32 = 380.0;
pub const TAB_CARD_HEIGHT: f32 = 232.0;

/// 卡片贴着触发点的间距(原版 `anchorRect.right + 8` / `bottom + 6`)。
const CARD_GAP: f32 = 8.0;
/// 拼图里相邻格子的缝(原版 `gap-[2px]`)。
const BOARD_GAP: f32 = 2.0;
/// 拼图区内边距(原版 `p-2`)。
const BOARD_PAD: f32 = 8.0;

// ─── 开闸判定(与项目行的 AI 品牌堆叠同一把尺子) ───────────────

/// 项目里有没有「显示 AI 会话」的 pane —— 项目行卡的开闸条件
/// (`ProjectList.tsx:137` 的 `hasAiPane`,短路版)。
///
/// 与行上的 AI 品牌堆叠同判据,于是「行上亮着图标 → 悬停才有预览」一眼可预期。
pub fn has_ai_pane(layout: Option<&SplitNode>, auto_resume: bool) -> bool {
    layout
        .map(|l| l.panes().iter().any(|p| p.shows_ai_session(auto_resume)))
        .unwrap_or(false)
}

/// 隐藏 tab 里最要紧的状态(`ProjectPanePreview.tsx:89-91`)。
///
/// 口径限于 [`PaneStatus`] —— 真正的「等确认」是 `pane.attention`,不在
/// `PaneStatus` 编码内,这里同样看不到(与 tab 栏现有行为一致)。
pub fn hidden_top_status(hidden: &[&PaneState]) -> Option<PaneStatus> {
    hidden
        .iter()
        .max_by_key(|p| p.status.priority())
        .map(|p| p.status)
}

/// 按比例把主轴长度分给子节点(原版的 `flexGrow: sizes[i]` + `gap-[2px]`)。
///
/// `sizes` 与子节点数对不上 / 含非正值时**均分** —— 与
/// `terminal_area::split_fractions` 同一条处置(旧配置、塌陷过一次的树)。
pub fn split_child_sizes(total: f32, sizes: &[f64], count: usize, gap: f32) -> Vec<f32> {
    if count == 0 {
        return Vec::new();
    }
    let usable = sizes.len() == count && sizes.iter().all(|s| s.is_finite() && *s > 0.0);
    let fractions: Vec<f64> = if usable {
        let sum: f64 = sizes.iter().sum();
        sizes.iter().map(|s| s / sum).collect()
    } else {
        vec![1.0 / count as f64; count]
    };
    let inner = (total - gap * (count.saturating_sub(1)) as f32).max(0.0);
    fractions
        .into_iter()
        .map(|f| (inner as f64 * f) as f32)
        .collect()
}

// ─── 快照 ────────────────────────────────────────────────────────

/// 一个微缩格子要画的东西。渲染前从 store 抠出来 —— `store.read(cx)` 的借用
/// 活不过元素构造(与 `project_list::Row` 同一条理由)。
#[derive(Clone)]
pub struct MiniPaneInfo {
    pub pane_id: String,
    pub label: String,
    pub status: PaneStatus,
    /// 该显示 AI 品牌图标吗(`paneShowsAiSession`)。
    pub vendor: Option<AiVendor>,
    pub shows_ai: bool,
    /// `None` = 这个 pane 从没起过 PTY(画「未启动」占位)。
    pub grid: Option<(Arc<TerminalEmulator>, TerminalTheme)>,
    /// PTY 已退出 → 盖「已断开」遮罩。
    pub exited: bool,
    /// 同叶子的其余 tab 数(`+N` 徽章)。
    pub hidden_count: usize,
    /// 隐藏 tab 里最高优先级的状态(idle 不画)。
    pub hidden_top: Option<PaneStatus>,
}

/// 微缩布局树(`SplitNode` 的展示侧投影)。
#[derive(Clone)]
pub enum MiniLayout {
    Leaf(Box<MiniPaneInfo>),
    Split {
        vertical: bool,
        sizes: Vec<f64>,
        children: Vec<MiniLayout>,
    },
}

/// 从 store 抠一棵微缩布局树。
pub fn snapshot_layout(
    node: &SplitNode,
    project_id: &str,
    store: &AppStore,
    auto_resume: bool,
    cx: &App,
) -> Option<MiniLayout> {
    match node {
        SplitNode::Leaf {
            panes,
            active_pane_id,
            ..
        } => {
            let active = panes
                .iter()
                .find(|p| &p.id == active_pane_id)
                .or_else(|| panes.first())?;
            let hidden: Vec<&PaneState> = panes.iter().filter(|p| p.id != active.id).collect();
            Some(MiniLayout::Leaf(Box::new(snapshot_pane(
                active,
                project_id,
                hidden.len(),
                hidden_top_status(&hidden),
                store,
                auto_resume,
                cx,
            ))))
        }
        SplitNode::Split {
            direction,
            children,
            sizes,
            ..
        } => Some(MiniLayout::Split {
            vertical: *direction == SplitDirection::Vertical,
            sizes: sizes.clone(),
            children: children
                .iter()
                .filter_map(|c| snapshot_layout(c, project_id, store, auto_resume, cx))
                .collect(),
        }),
    }
}

/// 单个 pane 的快照(tab 卡直接用这一条)。
#[allow(clippy::too_many_arguments)]
/// 卡上的名字走 store 的三级口径(自定义名 > 远程连接名 > shell 名),
/// 所以要 `project_id` —— 远程那一档得查连接表。
pub fn snapshot_pane(
    pane: &PaneState,
    project_id: &str,
    hidden_count: usize,
    hidden_top: Option<PaneStatus>,
    store: &AppStore,
    auto_resume: bool,
    cx: &App,
) -> MiniPaneInfo {
    let grid = pane.pty_id.and_then(|id| store.terminal(id)).map(|entity| {
        let pane = entity.read(cx);
        (pane.emulator(), pane.theme().clone())
    });
    let shows_ai = pane.shows_ai_session(auto_resume);
    MiniPaneInfo {
        pane_id: pane.id.clone(),
        // 与 tab 栏同一口径(自定义名 > 远程连接名 > shell 名)
        label: store.pane_display_label(project_id, pane),
        status: pane.status,
        // tab 上那条口径:CLI 名直取,其余走词匹配
        vendor: pane.ai_agent().and_then(|agent| {
            AiVendor::from_session_type(agent).or_else(|| AiVendor::infer(Some(agent), None))
        }),
        shows_ai,
        grid,
        exited: pane.pty_id.map(|id| store.is_pty_exited(id)).unwrap_or(false),
        hidden_count,
        hidden_top: hidden_top.filter(|s| *s != PaneStatus::Idle),
    }
}

// ─── 渲染 ────────────────────────────────────────────────────────

/// 终端画面本体(缺终端时是「未启动」占位)。`area` 是这一格的实际像素尺寸。
fn mini_screen(info: &MiniPaneInfo, style: &TerminalStyle, area: Size<Pixels>) -> AnyElement {
    match info.grid.as_ref() {
        Some((emulator, theme)) => div()
            .absolute()
            .top_0()
            .left_0()
            .w(area.width)
            .h(area.height)
            .child(MiniTerminalElement::new(
                gpui::SharedString::from(format!("mini-term-{}", info.pane_id)),
                emulator.clone(),
                style.clone(),
                theme.clone(),
            ))
            .into_any_element(),
        None => div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .text_size(ui::font_px(11.0))
            .text_color(ui::text_muted())
            .child(t("projectList", "preview.notStarted"))
            .into_any_element(),
    }
}

/// 一个微缩格子:画面 + 顶部标签条 + 断开遮罩。
fn mini_pane(info: &MiniPaneInfo, style: &TerminalStyle, area: Size<Pixels>) -> AnyElement {
    let mut label_bar = div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .flex()
        .items_center()
        .gap(px(4.0))
        .px(px(6.0))
        .py(px(2.0))
        .text_size(ui::font_px(10.0))
        .text_color(ui::text_secondary())
        // 原版是 `color-mix(bg-overlay 80%)` + `backdrop-blur(6px)`;
        // gpui 没有 backdrop-filter,退成同色 80% 不透明度的实底
        .bg(ui::with_alpha(ui::bg_overlay(), 0.8))
        .child(ui::status_dot(info.status));
    if info.shows_ai {
        label_bar = label_bar.child(
            BrandIcon::new(info.vendor)
                .size(px(11.0))
                .color(ui::text_secondary()),
        );
    }
    label_bar = label_bar.child(div().flex_1().truncate().child(info.label.clone()));
    if info.hidden_count > 0 {
        let mut tail = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.0))
            .text_color(ui::text_muted());
        if let Some(status) = info.hidden_top {
            tail = tail.child(ui::status_dot(status));
        }
        label_bar = label_bar.child(tail.child(format!("+{}", info.hidden_count)));
    }

    div()
        .relative()
        .w(area.width)
        .h(area.height)
        .overflow_hidden()
        .rounded(px(3.0))
        .border_1()
        .border_color(ui::border_subtle())
        .bg(ui::bg_terminal())
        .child(mini_screen(info, style, area))
        .child(label_bar)
        .when(info.exited, |el| {
            el.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui::Hsla {
                        h: 0.0,
                        s: 0.0,
                        l: 0.0,
                        a: 0.45,
                    })
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_secondary())
                    .child(t("projectList", "preview.disconnected")),
            )
        })
        .into_any_element()
}

/// 递归铺微缩拼图。`area` 是这一层可用的像素尺寸。
fn mini_node(layout: &MiniLayout, style: &TerminalStyle, area: Size<Pixels>) -> AnyElement {
    match layout {
        MiniLayout::Leaf(info) => mini_pane(info, style, area),
        MiniLayout::Split {
            vertical,
            sizes,
            children,
        } => {
            let count = children.len();
            let main = if *vertical {
                f32::from(area.height)
            } else {
                f32::from(area.width)
            };
            let parts = split_child_sizes(main, sizes, count, BOARD_GAP);
            let mut row = div()
                .flex()
                .w(area.width)
                .h(area.height)
                .gap(px(BOARD_GAP))
                .when(*vertical, |el| el.flex_col());
            for (child, part) in children.iter().zip(parts) {
                let child_area = if *vertical {
                    size(area.width, px(part))
                } else {
                    size(px(part), area.height)
                };
                row = row.child(mini_node(child, style, child_area));
            }
            row.into_any_element()
        }
    }
}

/// 卡片外壳:半透明底 + 强边框 + 阴影(与 `.ctx-menu` 同配方)。
///
/// `progress` 是 `menuPopIn` 这一帧的进度(1.0 = 已就位)。两张卡都挂在
/// `anchored` 里,负 margin 只挪自己、不影响任何别的东西。
fn card_shell(progress: f32) -> Div {
    let (opacity, dy) = mt_ui::motion::menu_pop_in(progress);
    div()
        .rounded(px(6.0))
        .border_1()
        .border_color(ui::border_strong())
        .bg(ui::bg_overlay())
        .shadow_lg()
        .opacity(opacity)
        .mt(px(dy))
}

/// 项目行悬停卡。`layout` 为 `None` 时画「尚未打开过终端」占位
/// (原版那个防御性分支:卡头仍要把绝对路径显出来)。
pub fn project_preview_card(
    project_name: &str,
    project_path: &str,
    layout: Option<&MiniLayout>,
    style: &TerminalStyle,
    progress: f32,
) -> AnyElement {
    let board_area = size(px(CARD_WIDTH - BOARD_PAD * 2.0), px(BOARD_HEIGHT));
    card_shell(progress)
        .w(px(CARD_WIDTH))
        // 卡头:项目名 + 绝对路径。原版把路径从行 title 挪到这里 ——
        // 原生 tooltip 会盖住浮层
        .child(
            div()
                .flex()
                .items_baseline()
                .gap(px(8.0))
                .px(px(8.0))
                .pt(px(8.0))
                .child(
                    div()
                        .flex_none()
                        .text_size(ui::font_px(12.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(ui::text_primary())
                        .child(project_name.to_string()),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(project_path.to_string()),
                ),
        )
        .child(
            div()
                .p(px(BOARD_PAD))
                .h(px(BOARD_HEIGHT))
                .map(|el| match layout {
                    Some(layout) => el.child(mini_node(layout, style, board_area)),
                    None => el.child(
                        div()
                            .w(board_area.width)
                            .h(board_area.height)
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.0))
                            .border_1()
                            .border_dashed()
                            .border_color(ui::border_subtle())
                            .bg(ui::bg_terminal())
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .child(t("projectList", "preview.neverOpened")),
                    ),
                }),
        )
        .into_any_element()
}

/// 非激活 tab 悬停卡(单格版,无卡头、无标签条 —— 原版就只有画面与断开遮罩)。
pub fn tab_preview_card(info: &MiniPaneInfo, style: &TerminalStyle, progress: f32) -> AnyElement {
    let area = size(px(TAB_CARD_WIDTH), px(TAB_CARD_HEIGHT));
    card_shell(progress)
        .relative()
        .w(area.width)
        .h(area.height)
        .overflow_hidden()
        .child(mini_screen(info, style, area))
        .when(info.exited, |el| {
            el.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui::Hsla {
                        h: 0.0,
                        s: 0.0,
                        l: 0.0,
                        a: 0.45,
                    })
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_secondary())
                    .child(t("projectList", "preview.disconnected")),
            )
        })
        .into_any_element()
}

// ─── 锚点 ────────────────────────────────────────────────────────

/// 项目行卡的锚点:行右缘 + 8,顶边对齐行顶(原版 `left/top` 那两句)。
///
/// 越界收拢交给 `anchored().snap_to_window_with_margin()`,不自己钳 ——
/// 原版那两句 `Math.min/max` 在 gpui 里是白拿的。
pub fn project_anchor(row: Bounds<Pixels>) -> gpui::Point<Pixels> {
    gpui::point(row.origin.x + row.size.width + px(CARD_GAP), row.origin.y)
}

/// tab 卡的锚点:贴 tab 下缘 + 6(原版 `bottom + 6`;放不下时翻到上方那一支
/// 由 `snap_to_window_with_margin` 代劳)。
pub fn tab_anchor(tab: Bounds<Pixels>) -> gpui::Point<Pixels> {
    gpui::point(tab.origin.x, tab.origin.y + tab.size.height + px(6.0))
}

/// 缩略图用的终端字体参数:只有字族/回退族有意义,字号由
/// [`MiniTerminalElement`] 按卡片尺寸自己反解。
pub fn preview_style(store: &AppStore) -> TerminalStyle {
    crate::store::terminal_style_from(
        store.config().terminal_font_size,
        store.config().terminal_font_family.as_deref(),
        store.config().terminal_ligatures,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, status: PaneStatus) -> PaneState {
        let mut p = PaneState::new("bash");
        p.id = id.to_string();
        p.status = status;
        p
    }

    #[test]
    fn 按比例分格且扣掉缝() {
        // 300 宽、两格、2px 缝 → 内容 298,按 30:70 分
        let parts = split_child_sizes(300.0, &[30.0, 70.0], 2, 2.0);
        assert_eq!(parts.len(), 2);
        assert!((parts[0] - 89.4).abs() < 0.1, "{parts:?}");
        assert!((parts[1] - 208.6).abs() < 0.1, "{parts:?}");
        assert!((parts[0] + parts[1] + 2.0 - 300.0).abs() < 0.1, "分完要正好填满");
    }

    /// 存的比例与子节点数对不上 / 含非正值 → 均分(与布局树同一条处置)。
    #[test]
    fn 比例对不上时均分() {
        let parts = split_child_sizes(302.0, &[50.0], 2, 2.0);
        assert!((parts[0] - parts[1]).abs() < 0.01);
        let parts = split_child_sizes(302.0, &[0.0, 100.0], 2, 2.0);
        assert!((parts[0] - parts[1]).abs() < 0.01, "0 也算不可用");
        let parts = split_child_sizes(302.0, &[f64::NAN, 1.0], 2, 2.0);
        assert!((parts[0] - parts[1]).abs() < 0.01, "NaN 也算不可用");
    }

    #[test]
    fn 分格不返负数() {
        // 缝比总宽还大(极端窄格):内容长度钳到 0,不能出负数尺寸
        let parts = split_child_sizes(1.0, &[1.0, 1.0, 1.0], 3, 2.0);
        assert!(parts.iter().all(|p| *p >= 0.0), "{parts:?}");
    }

    #[test]
    fn 无子节点出空表() {
        assert!(split_child_sizes(300.0, &[], 0, 2.0).is_empty());
    }

    /// 隐藏 tab 的状态取**最高优先级**那一个(error > ai-working > ai-idle > idle)。
    #[test]
    fn 隐藏_tab_取最高优先级状态() {
        let a = pane("a", PaneStatus::Idle);
        let b = pane("b", PaneStatus::AiWorking);
        let c = pane("c", PaneStatus::AiIdle);
        assert_eq!(
            hidden_top_status(&[&a, &b, &c]),
            Some(PaneStatus::AiWorking)
        );
        let e = pane("e", PaneStatus::Error);
        assert_eq!(hidden_top_status(&[&b, &e]), Some(PaneStatus::Error));
        assert_eq!(hidden_top_status(&[]), None);
    }

    /// 开闸与项目行的 AI 品牌堆叠同判据:`shows_ai_session` 有一个就算。
    #[test]
    fn 开闸看有没有_ai_pane() {
        let mut ai = pane("a", PaneStatus::AiWorking);
        ai.detected_agent = Some("claude".into());
        let plain = pane("b", PaneStatus::Idle);
        assert!(!has_ai_pane(None, true));
        assert!(!has_ai_pane(Some(&SplitNode::leaf(plain.clone())), true));
        assert!(has_ai_pane(Some(&SplitNode::leaf(ai)), true));
    }

    /// 待续接的 pane:开关关着时不算「有 AI」——与 tab 上的品牌图标同口径。
    #[test]
    fn 待续接的开闸跟随自动续接开关() {
        let mut p = pane("a", PaneStatus::Idle);
        p.ai_session = Some(crate::tree::AiSessionRef {
            agent: Some("claude".into()),
            session_id: "s".into(),
            cwd: None,
        });
        p.resume_pending = true;
        let layout = SplitNode::leaf(p);
        assert!(has_ai_pane(Some(&layout), true));
        assert!(!has_ai_pane(Some(&layout), false));
    }

    #[test]
    fn 项目卡锚点贴行右缘() {
        let row = Bounds {
            origin: gpui::point(px(0.0), px(100.0)),
            size: size(px(240.0), px(28.0)),
        };
        let at = project_anchor(row);
        assert_eq!(f32::from(at.x), 248.0);
        assert_eq!(f32::from(at.y), 100.0, "顶边对齐行顶,不是行中");
    }

    #[test]
    fn tab_卡锚点贴下缘() {
        let tab = Bounds {
            origin: gpui::point(px(50.0), px(30.0)),
            size: size(px(110.0), px(26.0)),
        };
        let at = tab_anchor(tab);
        assert_eq!(f32::from(at.x), 50.0);
        assert_eq!(f32::from(at.y), 62.0);
    }
}
