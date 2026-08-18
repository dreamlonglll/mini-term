//! 左栏:项目列表。对应 `src/components/ProjectList.tsx` 的主干
//!(分组 / 拖拽排序 / 右键菜单 / worktree 子项目是后续批次)。

use gpui::{
    Context, Entity, InteractiveElement, IntoElement, ParentElement, PathPromptOptions, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};

use crate::store::AppStore;
use crate::tree::PaneStatus;
use crate::ui;

pub struct ProjectList {
    store: Entity<AppStore>,
    /// 已经点过一次「移除」的项目 —— 第二次点才真删。
    ///
    /// 移除项目是不可逆的(配置里的布局、展开目录一起没),旧版为此弹确认框;
    /// Modal 是后续批次的交付物,这里先用「点两次」把误触挡住,而不是让一个
    /// 单击就能抹掉用户的项目。
    pending_remove: Option<String>,
}

impl ProjectList {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            pending_remove: None,
        }
    }

    /// 选目录加项目。gpui 直接给了平台对话框,不必自己造。
    fn pick_project(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择项目目录".into()),
        });
        let store = self.store.clone();
        cx.spawn(async move |_this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = store.update(cx, |store, cx| store.add_project(&path, cx));
        })
        .detach();
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
                        // 点别处 = 放弃刚才那次「移除」的待确认
                        this.pending_remove = None;
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
                    .child({
                        let confirming = self.pending_remove.as_deref() == Some(id.as_str());
                        div()
                            .id(SharedString::from(format!("project-remove-{id}")))
                            .h(px(16.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.0))
                            .text_size(px(12.0))
                            .when(confirming, |el| {
                                el.px(px(5.0))
                                    .text_size(px(11.0))
                                    .text_color(ui::color_error())
                                    .bg(ui::bg_overlay())
                            })
                            .when(!confirming, |el| el.w(px(16.0)).text_color(ui::text_muted()))
                            .hover(|el| el.text_color(ui::color_error()).bg(ui::bg_overlay()))
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                if this.pending_remove.as_deref() == Some(id_remove.as_str()) {
                                    this.pending_remove = None;
                                    this.store.update(cx, |store, cx| {
                                        store.remove_project(&id_remove, cx)
                                    });
                                } else {
                                    this.pending_remove = Some(id_remove.clone());
                                    cx.notify();
                                }
                            }))
                            .child(if confirming { "确认移除" } else { "×" })
                    }),
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
                            .child("项目"),
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
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.pick_project(cx);
                            }))
                            .child("+"),
                    ),
            )
            .child(list)
    }
}
