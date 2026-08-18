//! 终端区:SplitNode 树 → 嵌套 resizable + 每个叶子一条 tab 栏。
//!
//! 对应 `src/components/TerminalArea.tsx` + `SplitLayout.tsx` + `PaneGroup.tsx`。
//!
//! - split 节点 → [`gpui_component::resizable`](gpui_component::resizable)
//!   的 `h_resizable` / `v_resizable`(替 Allotment),每个节点一份
//!   `ResizableState`,按节点 id 缓存,拖动后把比例写回 store 并落盘;
//! - leaf 节点 → tab 栏 + 当前激活 pane 的 [`crate::pane::TerminalPane`] 实体。
//!   同一个叶子里的多个 pane 就是「终端标签」,与旧版一致(项目级 tab 层早已删除)。

use std::collections::HashMap;

use gpui::{
    AnyElement, App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};

use crate::store::AppStore;
use crate::tree::{SplitDirection, SplitNode};
use crate::ui;

pub struct TerminalArea {
    store: Entity<AppStore>,
    /// 每个 split 节点一份分隔条状态(跨帧保留,否则每帧都重置回均分)。
    split_states: HashMap<String, Entity<ResizableState>>,
}

impl TerminalArea {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            split_states: HashMap::new(),
        }
    }

    fn split_state(&mut self, node_id: &str, cx: &mut App) -> Entity<ResizableState> {
        self.split_states
            .entry(node_id.to_string())
            .or_insert_with(|| cx.new(|_| ResizableState::default()))
            .clone()
    }

    fn render_node(
        &mut self,
        node: &SplitNode,
        project_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            SplitNode::Leaf { .. } => self.render_leaf(node, project_id, window, cx),
            SplitNode::Split {
                id,
                direction,
                children,
                ..
            } => {
                let state = self.split_state(id, cx);
                let panels: Vec<_> = children
                    .iter()
                    .map(|child| {
                        let el = self.render_node(child, project_id, window, cx);
                        resizable_panel().child(el)
                    })
                    .collect();

                let element_id = SharedString::from(format!("split-{id}"));
                let group = match direction {
                    SplitDirection::Horizontal => h_resizable(element_id),
                    SplitDirection::Vertical => v_resizable(element_id),
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
                        let sizes: Vec<f64> =
                            state.read(cx).sizes().iter().map(|p| f32::from(*p) as f64).collect();
                        let total: f64 = sizes.iter().sum();
                        if total <= 0.0 {
                            return;
                        }
                        let pct: Vec<f64> = sizes.iter().map(|s| s / total * 100.0).collect();
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

        for pane in panes {
            let is_active = pane.id == active_id;
            let pane_id = pane.id.clone();
            let pid_click = pid.clone();
            let pane_id_close = pane.id.clone();
            let pid_close = pid.clone();
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
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.store.update(cx, |store, cx| {
                            store.activate_pane(&pid_click, &pane_id, window, cx)
                        });
                    }))
                    .child(ui::status_dot(pane.status))
                    .child(div().child(pane.label().to_string()))
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
                    .overflow_hidden()
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.store.update(cx, |store, cx| {
                            store.focus_pane(&pid_focus, &active_for_focus, window, cx)
                        });
                    }))
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

impl Render for TerminalArea {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        let content = self.render_node(&layout, &project_id, window, cx);
        div()
            .size_full()
            .bg(ui::bg_terminal())
            .flex()
            .child(div().size_full().child(content))
    }
}
