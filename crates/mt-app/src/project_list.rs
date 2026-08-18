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

use std::path::PathBuf;

use gpui::{
    AnyElement, App, ClipboardItem, Context, Entity, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Window, div, prelude::FluentBuilder, px,
};
use mt_ui::icons::{ALL_PROJECT_KINDS, FileIcon, FileKind, ProjectKind, TechIcon};

use crate::fs_ops;
use crate::i18n::t;
use crate::menu::{self, MenuEntry, MenuItem};
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
#[derive(Clone)]
struct Row {
    id: String,
    name: String,
    path: String,
    status: PaneStatus,
    /// 非激活项目里有 AI 任务完成(行尾那颗绿点)。
    needs_attention: bool,
    /// 领位图标的技术栈;`None` = 走通用目录图标。
    kind: Option<ProjectKind>,
    /// 需求描述(右键「编辑描述」的默认值)。
    description: Option<String>,
    /// `kindOverride` 原文:`None` = 自动,`Some("none")` = 不显示,
    /// 其余是技术栈 key。子菜单的勾要按它打,不能用解析后的 `kind`
    /// (那一路把 "none" 和「认不出」压成了同一个 `None`)。
    kind_override: Option<String>,
}

// ─── 右键菜单 ─────────────────────────────────────────────────

/// 项目行右键菜单的**项序**。`None` = 分隔线。
///
/// 逐条对照 `ProjectList.tsx:699-833`,**只列目标功能已经落地的那几项**:
/// 关联 SSH / 环境变量 / Worktree 管理 / WSL 会话 / 脱离父项目 / 分组相关
/// 在 GPUI 侧还没有功能,占位一个点不动的菜单项比没有更糟。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectMenuAction {
    Rename,
    EditDescription,
    OpenInFolder,
    CopyAbsolutePath,
    /// 「项目类型」子菜单。
    ProjectKind,
    Remove,
}

fn project_menu_actions() -> Vec<Option<ProjectMenuAction>> {
    use ProjectMenuAction::*;
    vec![
        Some(Rename),
        Some(EditDescription),
        Some(OpenInFolder),
        Some(CopyAbsolutePath),
        Some(ProjectKind),
        None,
        Some(Remove),
    ]
}

/// 子菜单里「当前选中」的标记。原版是文本方案(不是图标):选中 `✓ `,
/// 未选中一个**全角**空格 —— 两者宽度相同,菜单项文字才不会左右跳。
fn check_prefix(selected: bool) -> &'static str {
    if selected { "✓ " } else { "　" }
}

/// 「项目类型」子菜单。`current` 是 `kindOverride` 原文。
fn kind_submenu(store: &Entity<AppStore>, project_id: &str, current: Option<&str>) -> Vec<MenuEntry> {
    let mut entries: Vec<MenuEntry> = Vec::new();

    // 自动识别。原版这里还会把探测到的类型写进括号里(`（Rust）`),
    // GPUI 侧还没有探测那一路(useProjectKinds 未移植),所以不带括号。
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
            "{}{}",
            check_prefix(current.is_none()),
            t("projectList", "menu.projectKindAuto")
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
    for kind in ALL_PROJECT_KINDS {
        let key = kind.as_str();
        entries.push(
            MenuItem::new(format!(
                "{}{}",
                check_prefix(current == Some(key)),
                kind.label()
            ))
            .on_click(set(Some(key)))
            .into(),
        );
    }
    entries
}

/// 组装一行的右键菜单。
fn project_menu(store: &Entity<AppStore>, row: &Row) -> Vec<MenuEntry> {
    let mut entries = Vec::new();
    for action in project_menu_actions() {
        let Some(action) = action else {
            entries.push(menu::separator());
            continue;
        };
        entries.push(match action {
            ProjectMenuAction::Rename => {
                let store = store.clone();
                let id = row.id.clone();
                let name = row.name.clone();
                menu::item(t("projectList", "menu.rename"), move |window, cx| {
                    let store = store.clone();
                    let id = id.clone();
                    crate::prompt::show_prompt(
                        t("projectList", "menu.rename"),
                        t("fileTree", "prompt.renameMessage"),
                        name.clone(),
                        move |value, _window, cx| {
                            store.update(cx, |store, cx| store.rename_project(&id, &value, cx));
                        },
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
            ProjectMenuAction::ProjectKind => MenuItem::new(t("projectList", "menu.projectKind"))
                .submenu(kind_submenu(store, &row.id, row.kind_override.as_deref()))
                .into(),
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
                    description: p.description.clone(),
                    kind_override: p.kind_override.clone(),
                }
            })
            .collect();

        let mut list = div().flex().flex_col().flex_1().overflow_hidden();
        for row in rows {
            let Row {
                ref id,
                ref name,
                ref path,
                status,
                needs_attention,
                kind,
                ..
            } = row;
            let (id, name, path) = (id.clone(), name.clone(), path.clone());
            let is_active = active.as_deref() == Some(id.as_str());
            let id_click = id.clone();
            let id_remove = id.clone();
            let row_for_menu = row.clone();
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
                    // 右键菜单(`ProjectList.tsx` 的 onContextMenu)。原版会先
                    // `closePreview()` 收掉悬停缩略图 —— GPUI 侧还没有那层浮层。
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            let entries = project_menu(&this.store, &row_for_menu);
                            menu::show(event.position, entries, window, cx);
                        }),
                    )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 菜单项序照抄原版(去掉功能未建的那几项),分隔线只在「移除项目」之前一条。
    #[test]
    fn 右键菜单项序与原版一致() {
        use ProjectMenuAction::*;
        let actions = project_menu_actions();
        assert_eq!(
            actions,
            vec![
                Some(Rename),
                Some(EditDescription),
                Some(OpenInFolder),
                Some(CopyAbsolutePath),
                Some(ProjectKind),
                None,
                Some(Remove),
            ]
        );
        // 未建功能不占位:SSH / 环境变量 / Worktree / WSL / 分组一个都不许出现
        assert_eq!(actions.iter().filter(|a| a.is_none()).count(), 1);
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

    /// 12 种技术栈一个不漏(原版 `PROJECT_KINDS` 就是这 12 个)。
    #[test]
    fn 项目类型子菜单列全集() {
        assert_eq!(ALL_PROJECT_KINDS.len(), 12);
        let keys: Vec<&str> = ALL_PROJECT_KINDS.iter().map(|k| k.as_str()).collect();
        for expected in [
            "java", "rust", "go", "python", "nodejs", "react", "vuejs", "nextjs", "svelte", "vite",
            "flutter", "php",
        ] {
            assert!(keys.contains(&expected), "少了 {expected}");
        }
    }
}
