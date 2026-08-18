//! 左栏:项目列表。对应 `src/components/ProjectList.tsx` 的主干
//!(分组 / 拖拽排序 / 右键菜单 / worktree 子项目是后续批次)。

use gpui::{
    Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};

use crate::i18n::t;
use crate::modal;
use crate::store::AppStore;
use crate::tree::PaneStatus;
use crate::ui;

pub struct ProjectList {
    store: Entity<AppStore>,
}

impl ProjectList {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self { store }
    }
}

impl Render for ProjectList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let active = store.active_project_id.clone();
        let rows: Vec<(String, String, String, PaneStatus, bool)> = store
            .projects()
            .iter()
            .map(|p| {
                let state = store.project_state(&p.id);
                (
                    p.id.clone(),
                    p.name.clone(),
                    p.path.clone(),
                    state.map(|s| s.status).unwrap_or(PaneStatus::Idle),
                    state.map(|s| s.needs_attention).unwrap_or(false),
                )
            })
            .collect();

        let mut list = div().flex().flex_col().flex_1().overflow_hidden();
        for (id, name, path, status, needs_attention) in rows {
            let is_active = active.as_deref() == Some(id.as_str());
            let id_click = id.clone();
            let id_remove = id.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("project-{id}")))
                    .group(SharedString::from(format!("project-row-{id}")))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(10.0))
                    .py(px(6.0))
                    .cursor_pointer()
                    .when(is_active, |el| {
                        el.bg(ui::accent_subtle()).border_l_2().border_color(ui::accent())
                    })
                    .when(!is_active, |el| {
                        el.border_l_2().border_color(gpui::Hsla {
                            a: 0.0,
                            ..ui::accent()
                        })
                    })
                    .hover(|el| el.bg(ui::bg_overlay()))
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.store
                            .update(cx, |store, cx| store.set_active_project(&id_click, cx));
                    }))
                    .child(ui::status_dot(status))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(13.0))
                                    .text_color(if is_active {
                                        ui::text_primary()
                                    } else {
                                        ui::text_secondary()
                                    })
                                    .child(name),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(11.0))
                                    .text_color(ui::text_muted())
                                    .child(path),
                            ),
                    )
                    // 完成提示点:非激活项目里有 AI 任务完成
                    .when(needs_attention, |el| {
                        el.child(
                            div()
                                .w(px(6.0))
                                .h(px(6.0))
                                .rounded_full()
                                .bg(ui::color_success()),
                        )
                    })
                    // 移除:弹确认框(不可逆,布局与展开目录一起没)
                    .child(
                        div()
                            .id(SharedString::from(format!("project-remove-{id}")))
                            .w(px(16.0))
                            .h(px(16.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.0))
                            .text_size(px(12.0))
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
                            .child("×"),
                    ),
            );
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(ui::bg_surface())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(10.0))
                    .py(px(6.0))
                    .border_b_1()
                    .border_color(ui::border_subtle())
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(ui::text_muted())
                            .child(t("panels", "projects")),
                    )
                    .child(
                        div()
                            .id("add-project")
                            .w(px(18.0))
                            .h(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.0))
                            .cursor_pointer()
                            .text_color(ui::text_muted())
                            .hover(|el| el.text_color(ui::accent()).bg(ui::bg_overlay()))
                            .on_click(cx.listener(|this, _event, window, cx| {
                                modal::open_add_project(this.store.clone(), window, cx);
                            }))
                            .child("+"),
                    ),
            )
            .child(list)
    }
}
