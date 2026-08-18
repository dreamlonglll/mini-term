//! 左栏:项目列表。对应 `src/components/ProjectList.tsx` 的主干
//!(分组 / 拖拽排序 / 右键菜单 / worktree 子项目是后续批次)。
//!
//! # 领位图标
//!
//! 原版是「SSH > 技术栈 > 通用」三选一,**恒显**(每行都有图标,缩进才对得齐)。
//! 这里没有 SSH 远程项目(mt-ssh 未进 crates/),所以只剩两档:
//! 技术栈徽标([`mt_ui::icons::TechIcon`])/ 通用目录图标。
//!
//! ⚠️ 技术栈**只认 `kindOverride`**:原版还有一路 `useProjectKinds` 探测
//! (扫项目根的 `Cargo.toml` / `package.json` 之类),那是带缓存的异步批量任务,
//! 留给后续批次;探测接上之前,没手动设过类型的项目一律走通用图标。

use gpui::{
    AnyElement, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use mt_ui::icons::{FileIcon, FileKind, ProjectKind, TechIcon};

use crate::i18n::t;
use crate::modal;
use crate::store::AppStore;
use crate::tree::PaneStatus;
use crate::ui;

/// 项目行的领位图标。`kind` 认得出就是技术栈徽标,否则退通用目录图标
/// (对应原版认不出时的 `Package` 兜底,同样取 `--color-file`)。
fn project_icon(kind: Option<ProjectKind>) -> AnyElement {
    match kind {
        Some(kind) => TechIcon::new(kind).size(px(14.0)).into_any_element(),
        None => FileIcon::of_kind(FileKind::Directory)
            .size(px(14.0))
            .color(ui::color_file())
            .into_any_element(),
    }
}

/// 一行要画的东西。渲染前先从 store 抠出来 —— `store.read(cx)` 的借用
/// 活不过 `cx.listener`,一行 6 个字段用元组已经读不清了。
struct Row {
    id: String,
    name: String,
    path: String,
    status: PaneStatus,
    /// 非激活项目里有 AI 任务完成(行尾那颗绿点)。
    needs_attention: bool,
    /// 领位图标的技术栈;`None` = 走通用目录图标。
    kind: Option<ProjectKind>,
}

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
        let rows: Vec<Row> = store
            .projects()
            .iter()
            .map(|p| {
                let state = store.project_state(&p.id);
                Row {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    path: p.path.clone(),
                    status: state.map(|s| s.status).unwrap_or(PaneStatus::Idle),
                    needs_attention: state.map(|s| s.needs_attention).unwrap_or(false),
                    // "none" = 用户选了「不显示」,认不出的值同样落到通用图标
                    kind: p.kind_override.as_deref().and_then(ProjectKind::from_str),
                }
            })
            .collect();

        let mut list = div().flex().flex_col().flex_1().overflow_hidden();
        for Row {
            id,
            name,
            path,
            status,
            needs_attention,
            kind,
        } in rows
        {
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
                    // 领位是**项目身份图标**(原版的顺序:身份图标 → … → 状态灯),
                    // 每行都有、缩进才对得齐
                    .child(project_icon(kind))
                    // 状态灯的动画 id 拿项目 id 拼:跨帧稳定、逐行唯一
                    .child(ui::status_dot(
                        SharedString::from(format!("status-project-{id}")),
                        status,
                    ))
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
