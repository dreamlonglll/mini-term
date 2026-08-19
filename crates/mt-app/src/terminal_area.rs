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
    Pixels, Render, SharedString, Size, StatefulInteractiveElement, Styled, Window, anchored,
    canvas, deferred, div, point, prelude::FluentBuilder, px,
};
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use gpui_component::tooltip::Tooltip;
use mt_ui::icons::{AiVendor, BrandIcon};

use crate::focus_nav::{self, Direction, PaneRect};
use crate::i18n::{t, tr};
use crate::markers;
use crate::menu::{self, MenuEntry, MenuItem, hotkey_label};
use crate::modal;
use crate::overlay;
use crate::pane_actions;
use crate::store::AppStore;
use crate::tree::{SplitDirection, SplitNode};
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
}

/// 控件簇里 marker 按钮**右缘**到叶子右边缘的距离。
///
/// 簇是 `.gap(2).px(6)` 后跟三个 22×22 的方钮(分屏右 / 分屏下 / 关整组),
/// marker 按钮排在它们之前(原版 `PaneGroup.tsx:489` 同样排在「终端内查找」之前,
/// 而 GPUI 侧没有查找按钮),自己还带 4px 右外边距(原版的 `mr-1`)。
/// 原版是 `getBoundingClientRect()` 量出来的,这里由布局常量算 ——
/// 加减控件时**必须同步改这个常量**,有单测钉着组成。
const MARKER_ANCHOR_INSET: f32 =
    CTRL_CLUSTER_PAD + 3.0 * (CTRL_BTN + CTRL_GAP) + MARKER_BTN_MARGIN_RIGHT;
const CTRL_CLUSTER_PAD: f32 = 6.0;
const CTRL_BTN: f32 = 22.0;
const CTRL_GAP: f32 = 2.0;
const MARKER_BTN_MARGIN_RIGHT: f32 = 4.0;

/// 浮层宽度。原版是 `min-w-[280px]` + 内容撑开,gpui 侧要给正文列一个确定宽度
/// 才truncate得动,取固定值 —— 差别只在「超长正文时原版会更宽」。
const MARKER_PANEL_WIDTH: f32 = 300.0;
/// 列表最大高度(`MarkerList.tsx:30` 的 `max-h-80` = 20rem)。
const MARKER_PANEL_MAX_HEIGHT: f32 = 320.0;
/// 正文截断字数(`MarkerList.tsx:16` 的 `truncate(s, 40)`)。
const MARKER_LINE_MAX: usize = 40;

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
/// 对照 `PaneGroup.tsx:336-383`。**跳过「分支会话 / 查看会话分支」** ——
/// fork 那套(能力位表 / 家族树面板 / 自记账)在 GPUI 侧还没有;
/// 原版那条「未获会话身份」的置灰提示同样属于 fork 位,一并不出现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabMenuAction {
    Rename,
    SplitRight,
    SplitDown,
    CloseTab,
    ClosePane,
}

fn tab_menu_actions() -> Vec<Option<TabMenuAction>> {
    use TabMenuAction::*;
    vec![
        Some(Rename),
        None,
        Some(SplitRight),
        Some(SplitDown),
        None,
        Some(CloseTab),
        Some(ClosePane),
    ]
}

/// 组装一个 tab 的右键菜单。`label` 是它当前的显示名(重命名的默认值)。
fn tab_menu(
    store: &Entity<AppStore>,
    project_id: &str,
    pane_id: &str,
    label: &str,
) -> Vec<MenuEntry> {
    let mut entries = Vec::new();
    for action in tab_menu_actions() {
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
        }
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
        let anchor = point(
            px(rect.left + rect.width - MARKER_ANCHOR_INSET - MARKER_PANEL_WIDTH),
            // 原版是「按钮下缘 + 4」;按钮在 26px 的 tab 栏里居中,下缘约在栏底上方 2px
            px(rect.top + 2.0),
        );

        let markers = self.store.read(cx).markers_for_pty(pty_id).to_vec();
        // 列表本体单独一层:`overflow_y_scroll` 要 Stateful(必须带 id),
        // 而外层要 `track_focus`(Esc 的落点),两件事分层最省心
        let mut list = div()
            .id(SharedString::from(format!("marker-list-{pty_id}")))
            .w_full()
            .max_h(px(MARKER_PANEL_MAX_HEIGHT))
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
            for marker in markers {
                let marker_id = marker.id.clone();
                let store = self.store.clone();
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
                        .text_color(ui::text_primary())
                        // `--bg-hover` 在 ui::Palette 里没有对应项,统一用 bg_overlay
                        // (与文件树行 hover 同一档)
                        .hover(|el| el.bg(ui::bg_overlay()))
                        // 悬停看全文(含粘贴多行时的换行)
                        .tooltip({
                            let full = SharedString::from(marker.line.clone());
                            move |window, cx| Tooltip::new(full.clone()).build(window, cx)
                        })
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            cx.stop_propagation();
                            let id = marker_id.clone();
                            store.update(cx, |store, cx| store.jump_to_marker(pty_id, &id, cx));
                            // 跳转**并关闭浮层**(`MarkerList.tsx:36-39`)
                            this.close_marker_popover(window, cx);
                        }))
                        .child(
                            div()
                                .flex_none()
                                .w(px(28.0))
                                .text_color(ui::text_muted())
                                .child(format!("#{}", marker.seq)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(36.0))
                                .text_color(ui::text_muted())
                                .child(markers::format_time(marker.ts)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
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
            .w(px(MARKER_PANEL_WIDTH))
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
            SplitNode::Leaf { .. } => self.render_leaf(node, project_id, window, cx),
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
        let focused_pane = store.focused_pane_id.clone();
        let active = panes
            .iter()
            .find(|p| &p.id == active_pane_id)
            .or_else(|| panes.first());
        let Some(active) = active else {
            return div().into_any_element();
        };
        let terminal = active.pty_id.and_then(|id| store.terminal(id)).cloned();
        // AI 任务标记数。**列表为空就整个不画按钮**(`PaneGroup.tsx:489`),
        // 这就是「⚑ 平时看不见」的直接原因 —— 见 `markers` 模块注释的 alt screen 段。
        let marker_count = active
            .pty_id
            .map(|id| store.markers_for_pty(id).len())
            .unwrap_or(0);
        // 焦点落在本组内 = 高亮边框(旧版靠 tab 的 accent 条 + xterm 焦点两处表达)
        let group_focused = focused_pane
            .as_deref()
            .map(|id| panes.iter().any(|p| p.id == id))
            .unwrap_or(false);
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
            .h(px(26.0))
            .overflow_x_scroll()
            .bg(ui::bg_elevated())
            .border_b_1()
            .border_color(ui::border_subtle())
            .text_size(ui::font_px(12.0));

        for (idx, pane) in panes.iter().enumerate() {
            let is_active = pane.id == active_id;
            let pane_id = pane.id.clone();
            let pane_id_rename = pane.id.clone();
            let pid_click = pid.clone();
            let pane_id_close = pane.id.clone();
            let pane_id_menu = pane.id.clone();
            let pid_close = pid.clone();
            let pid_rename = pid.clone();
            let pid_menu = pid.clone();
            let label = pane.label().to_string();
            let label_menu = label.clone();
            let has_unread = unread.get(idx).copied().unwrap_or(false);
            let vendor = vendors.get(idx).copied().flatten();
            bar = bar.child(
                div()
                    .id(gpui::SharedString::from(format!("tab-{}", pane.id)))
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
                    // 单击切 tab,双击改名(旧版是右键菜单里的「重命名」)
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
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
                            let entries =
                                tab_menu(&this.store, &pid_menu, &pane_id_menu, &label_menu);
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
                                })
                                .contrast(ui::bg_elevated()),
                        )
                    })
                    .child(div().child(pane.label().to_string()))
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

        // 右侧:分屏 / 关整组
        let ctrl = |label: &'static str| {
            div()
                .flex()
                .items_center()
                .justify_center()
                .w(px(CTRL_BTN))
                .h(px(CTRL_BTN))
                .rounded(px(3.0))
                .text_color(ui::text_muted())
                .child(label)
        };
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
                .child(
                    div()
                        .id(gpui::SharedString::from(format!("split-right-{leaf}")))
                        .cursor_pointer()
                        .hover(|el| el.text_color(ui::accent()))
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.split_pane(
                                    &pid_right,
                                    &anchor_right,
                                    SplitDirection::Horizontal,
                                    window,
                                    cx,
                                );
                            });
                        }))
                        .child(ctrl("⬓")),
                )
                .child(
                    div()
                        .id(gpui::SharedString::from(format!("split-down-{leaf}")))
                        .cursor_pointer()
                        .hover(|el| el.text_color(ui::accent()))
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.split_pane(
                                    &pid_down,
                                    &anchor_down,
                                    SplitDirection::Vertical,
                                    window,
                                    cx,
                                );
                            });
                        }))
                        .child(ctrl("⬒")),
                )
                .child(
                    div()
                        .id(gpui::SharedString::from(format!("close-leaf-{leaf}")))
                        .cursor_pointer()
                        .hover(|el| el.text_color(ui::color_error()))
                        // 控制条的 × 关的是**整组**,同样先确认(原版 closeLeaf)
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            pane_actions::close_leaf(
                                this.store.clone(),
                                pid_close_leaf.clone(),
                                leaf_for_close.clone(),
                                window,
                                cx,
                            );
                        }))
                        .child(ctrl("×")),
                ),
        );

        let pid_focus = pid.clone();
        let active_for_focus = active_id.clone();
        let pid_drop = pid.clone();
        let drop_pane_id = active_id.clone();
        // 拖拽中断(松手在窗外)后 gpui 会清 active_drag 并重画,与它与门就不必
        // 到处补清理 —— 与 `project_list.rs` 里那份高亮同一套判据。
        let file_drop_over =
            cx.has_active_drag() && self.file_drop_pane.as_deref() == Some(active_id.as_str());
        // 方向导航要知道每个 pane 画在哪 —— canvas 只量不画,量完存进本视图。
        // 这里**故意不 notify**:量尺寸再触发重画就是每帧一个死循环。
        let this = cx.entity();
        let rect_pane_id = active_id.clone();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(ui::bg_terminal())
            .border_1()
            .border_color(if group_focused {
                ui::accent()
            } else {
                ui::border_subtle()
            })
            .child(bar)
            .child(
                div()
                    .id(gpui::SharedString::from(format!("pane-body-{leaf}")))
                    .flex_1()
                    .relative()
                    .overflow_hidden()
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
                    .when(file_drop_over, |el| el.child(drop_hint())),
            )
            .into_any_element()
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
fn click_count(event: &ClickEvent) -> usize {
    match event {
        ClickEvent::Mouse(e) => e.up.click_count,
        ClickEvent::Keyboard(_) => 1,
    }
}

/// 点击位置(弹菜单要它)。键盘触发的「点击」没有坐标,退回当前鼠标位置 ——
/// 菜单总得有个锚点,而这一条在真机上走不到(那个 `+` 没有键盘入口)。
fn click_position(event: &ClickEvent, window: &Window) -> gpui::Point<gpui::Pixels> {
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
        }

        // 切走项目 / 关光了终端 → 浮层无处可挂。下面两条早退路径压根走不到浮层
        // 组装那一步,不在这里收掉的话覆盖物栈里会留一条永远摘不掉的登记。
        if self.marker_open.is_some() && self.store.read(cx).active_layout().is_none() {
            self.close_marker_popover(window, cx);
        }

        let store = self.store.read(cx);
        let Some(project) = store.active_project() else {
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
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.new_terminal(&pid, None, None, window, cx);
                            });
                        }))
                        .child(format!(
                            "+ {}  (Ctrl+Shift+T)",
                            t("terminalArea", "newTerminal")
                        )),
                );
        };

        // 关掉的 pane 的矩形残影一并清掉,免得方向导航挑到不存在的格子
        let alive: std::collections::HashSet<String> =
            layout.panes().into_iter().map(|p| p.id.clone()).collect();
        self.pane_rects.retain(|id, _| alive.contains(id));

        // 首帧只量不画:百分比要按真实可用尺寸换算,而 ResizablePanel 只认第一帧的
        // 初值(见模块注释)。量到之后主动 notify 一次,下一帧把分屏树铺上去。
        let content = self
            .measured
            .then(|| self.render_node(&layout, &project_id, self.area_size, window, cx));
        // 浮层在分屏树**之后**组装:它要读 render_node 刚更新过的 pane 矩形,
        // 而且要画在所有常规内容之上(deferred priority 1)
        let marker_popover = self.render_marker_popover(&layout, window, cx);
        let this = cx.entity();
        div()
            .size_full()
            .bg(ui::bg_terminal())
            .flex()
            .relative()
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

    /// tab 右键菜单的项序照抄原版(去掉 fork 那一段),两条分隔线。
    #[test]
    fn tab_右键菜单项序与原版一致() {
        use TabMenuAction::*;
        let actions = tab_menu_actions();
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
    /// 加减控件时这条会提醒同步改 [`MARKER_ANCHOR_INSET`]。
    #[test]
    fn 标记浮层锚点按控件簇布局算() {
        // 右侧簇:px-6 + 三个 22×22 方钮(各带 2px gap)+ marker 自己的 4px 右边距
        assert_eq!(MARKER_ANCHOR_INSET, 6.0 + 3.0 * 24.0 + 4.0);
        assert_eq!(MARKER_ANCHOR_INSET, 82.0);
        // 面板右缘贴按钮右缘 → 左缘 = 叶右缘 - inset - 面板宽
        let leaf_right = 1000.0_f32;
        let left = leaf_right - MARKER_ANCHOR_INSET - MARKER_PANEL_WIDTH;
        assert_eq!(left, 1000.0 - 82.0 - 300.0);
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
