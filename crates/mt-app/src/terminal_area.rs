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
    AnyElement, App, AppContext, Bounds, ClickEvent, Context, Entity, InteractiveElement,
    IntoElement, ParentElement, Pixels, Render, SharedString, Size, StatefulInteractiveElement,
    Styled, Window, canvas, div, prelude::FluentBuilder, px,
};
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};

use crate::focus_nav::{self, Direction, PaneRect};
use crate::modal;
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
}

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

impl TerminalArea {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            split_states: HashMap::new(),
            area_size: FALLBACK_AREA,
            measured: false,
            pane_rects: HashMap::new(),
        }
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
        // 焦点落在本组内 = 高亮边框(旧版靠 tab 的 accent 条 + xterm 焦点两处表达)
        let group_focused = focused_pane
            .as_deref()
            .map(|id| panes.iter().any(|p| p.id == id))
            .unwrap_or(false);
        let unread: Vec<bool> = panes.iter().map(|p| store.is_pane_unread_done(&p.id)).collect();

        let active_id = active.id.clone();
        let pid = project_id.to_string();
        let leaf = leaf_id.clone();

        let mut bar = div()
            .flex()
            .items_center()
            .flex_none()
            .h(px(26.0))
            .bg(ui::bg_elevated())
            .border_b_1()
            .border_color(ui::border_subtle())
            .text_size(px(12.0));

        for (idx, pane) in panes.iter().enumerate() {
            let is_active = pane.id == active_id;
            let pane_id = pane.id.clone();
            let pane_id_rename = pane.id.clone();
            let pid_click = pid.clone();
            let pane_id_close = pane.id.clone();
            let pid_close = pid.clone();
            let pid_rename = pid.clone();
            let label = pane.label().to_string();
            let has_unread = unread.get(idx).copied().unwrap_or(false);
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
                    .child(ui::status_dot(pane.status))
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
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.store.update(cx, |store, cx| {
                                    store.close_pane(&pid_close, &pane_id_close, cx)
                                });
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
                .on_click(cx.listener(move |this, _event, window, cx| {
                    this.store.update(cx, |store, cx| {
                        store.new_terminal(&pid_new, None, Some(anchor_new.clone()), window, cx);
                    });
                }))
                .child("+"),
        );

        // 右侧:分屏 / 关整组
        let ctrl = |label: &'static str| {
            div()
                .flex()
                .items_center()
                .justify_center()
                .w(px(22.0))
                .h(px(22.0))
                .rounded(px(3.0))
                .text_color(ui::text_muted())
                .child(label)
        };
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
                .gap(px(2.0))
                .px(px(6.0))
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
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.close_leaf(&pid_close_leaf, &leaf_for_close, cx)
                            });
                        }))
                        .child(ctrl("×")),
                ),
        );

        let pid_focus = pid.clone();
        let active_for_focus = active_id.clone();
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
                                .child("终端启动中…"),
                        ),
                    }),
            )
            .into_any_element()
    }
}

/// 点击次数(键盘触发的「点击」按一次算)。
fn click_count(event: &ClickEvent) -> usize {
    match event {
        ClickEvent::Mouse(e) => e.up.click_count,
        ClickEvent::Keyboard(_) => 1,
    }
}

impl Render for TerminalArea {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 塌陷/关闭掉的节点的分隔条状态在这里回收 —— 不清的话每分一次屏就多留
        // 一个 Entity(极小但确实的泄漏,看板已记)。
        let live_nodes = self.store.read(cx).live_node_ids();
        self.split_states.retain(|id, _| live_nodes.contains(id));

        let store = self.store.read(cx);
        let Some(project) = store.active_project() else {
            return div()
                .size_full()
                .bg(ui::bg_terminal())
                .flex()
                .items_center()
                .justify_center()
                .text_color(ui::text_muted())
                .text_size(px(13.0))
                .child("先在左侧添加一个项目");
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
                        .text_size(px(13.0))
                        .child(format!("{project_name} 还没有终端")),
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
                        .text_size(px(13.0))
                        .cursor_pointer()
                        .hover(|el| el.border_color(ui::accent()).text_color(ui::accent()))
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.new_terminal(&pid, None, None, window, cx);
                            });
                        }))
                        .child("+ 新建终端  (Ctrl+Shift+T)"),
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
