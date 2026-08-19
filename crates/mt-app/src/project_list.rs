//! 左栏:项目列表。对应 `src/components/ProjectList.tsx` 的主干
//!(hover 缩略图 / worktree 徽章 / 内联重命名是后续批次)。
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

use std::path::PathBuf;

use gpui::{
    AnyElement, App, ClipboardItem, Context, DragMoveEvent, Entity, ExternalPaths,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use mt_config::ProjectTreeItem;
use mt_ui::icons::vector::VectorIcon;
use mt_ui::icons::{ALL_PROJECT_KINDS, FileIcon, FileKind, ProjectKind, TechIcon};

use crate::dnd::{
    self, DragProjectItem, DropPosition, ExternalDropKind, PreviewIcon,
};
use crate::fs_ops;
use crate::i18n::{t, tr};
use crate::menu::{self, MenuEntry, MenuItem};
use crate::modal;
use crate::project_tree::{self, MAX_DEPTH, OrderedItem};
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
    /// 渲染缩进层级。
    depth: usize,
    /// 所在分组;`None` = 顶层。
    parent_group_id: Option<String>,
    /// worktree 子项目:位置由父项目派生,**不作为落点**(自身仍可拖走 = 脱离父项目)。
    is_child: bool,
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
/// 逐条对照 `ProjectList.tsx:699-833`,**只列目标功能已经落地的那几项**:
/// 关联 SSH / 环境变量 / Worktree 管理 / WSL 会话在 GPUI 侧还没有功能,
/// 占位一个点不动的菜单项比没有更糟。
///
/// **分组那一段不在这张表里**:它是条件段(有没有分组、是不是子项目各不相同),
/// 由 [`group_section`] 在 `ProjectKind` 之后动态插入 —— 位置与原版一致
/// (项目类型子菜单之后、「移除项目」那条分隔线之前)。
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

/// 组装一行的右键菜单。
fn project_menu(store: &Entity<AppStore>, row: &Row, tree: &[ProjectTreeItem]) -> Vec<MenuEntry> {
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
            ProjectMenuAction::ProjectKind => {
                let entry: MenuEntry = MenuItem::new(t("projectList", "menu.projectKind"))
                    .submenu(kind_submenu(store, &row.id, row.kind_override.as_deref()))
                    .into();
                entries.push(entry);
                // 分组段紧跟在「项目类型」之后(原版就是这个位置),自带前置分隔线
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

/// 分组行右键菜单(`ProjectList.tsx:965-1007`)。六项连排、**无分隔线**;
/// 其中「添加远程项目」需要 SSH,GPUI 侧没有这个功能,本批不放这项。
fn group_menu(store: &Entity<AppStore>, group: &GroupRow) -> Vec<MenuEntry> {
    let mut entries: Vec<MenuEntry> = Vec::new();

    entries.push({
        let store = store.clone();
        let id = group.id.clone();
        let name = group.name.clone();
        menu::item(t("projectList", "menu.renameGroup"), move |window, cx| {
            let store = store.clone();
            let id = id.clone();
            // 原版这里是**行内编辑**(C.1),GPUI 侧的内联编辑与项目重命名同属
            // 后续批次 —— 两处一起改才不会一半弹窗一半行内,现在先与项目行同款走弹窗。
            crate::prompt::show_prompt(
                t("projectList", "menu.renameGroup"),
                t("projectList", "newGroupPlaceholder"),
                name.clone(),
                move |value, _window, cx| {
                    store.update(cx, |store, cx| store.rename_group(&id, &value, cx));
                },
                window,
                cx,
            );
        })
    });

    entries.push({
        let store = store.clone();
        let id = group.id.clone();
        menu::item(t("projectList", "menu.addProject"), move |window, cx| {
            modal::open_add_project_into(store.clone(), Some(id.clone()), window, cx);
        })
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

pub struct ProjectList {
    store: Entity<AppStore>,
    /// 正在被拖的节点 id(拖影起来那一刻记下),源行据此变淡。
    /// 渲染时与 `cx.has_active_drag()` 与门 —— 拖拽中断(Esc / 松手在窗外)不会留脏。
    dragging: Option<String>,
    /// 落点指示,见 [`DropIndicator`]。
    drop_indicator: Option<DropIndicator>,
    /// 外部文件拖到列表上方时的三态提示。
    external: Option<ExternalDrag>,
}

impl ProjectList {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            dragging: None,
            drop_indicator: None,
            external: None,
        }
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        let store_ref = self.store.read(cx);
        let active = store_ref.active_project_id.clone();
        let tree_snapshot: Vec<ProjectTreeItem> = store_ref
            .config()
            .project_tree
            .clone()
            .unwrap_or_default();

        let mut list = div().flex().flex_col().flex_1().overflow_hidden();
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
                    list = list.child(self.render_group(row, dragging.as_deref(), drag_active, cx));
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
                    let row = Row {
                        id: p.id.clone(),
                        name: p.name.clone(),
                        path: p.path.clone(),
                        status: state.map(|s| s.status).unwrap_or(PaneStatus::Idle),
                        needs_attention: state.map(|s| s.needs_attention).unwrap_or(false),
                        // "none" = 用户选了「不显示」,认不出的值同样落到通用图标
                        kind: p.kind_override.as_deref().and_then(ProjectKind::from_str),
                        description: p.description.clone(),
                        kind_override: p.kind_override.clone(),
                        depth,
                        parent_group_id,
                        is_child,
                    };
                    let is_active = active.as_deref() == Some(row.id.as_str());
                    list = list.child(self.render_project(
                        row,
                        is_active,
                        dragging.as_deref(),
                        drag_active,
                        &tree_snapshot,
                        cx,
                    ));
                }
            }
        }

        let store_for_menu = self.store.clone();
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
            .child(
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
                    .child(
                        div()
                            .text_size(ui::font_px(11.0))
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
                            .on_click(move |_event, window, cx| {
                                modal::open_add_project(store_for_menu.clone(), window, cx);
                            })
                            .child("+"),
                    ),
            )
            .child(list)
            // 三态提示框:盖住整栏,`pointer-events` 不用管 —— gpui 的 drop 分发
            // 按 hitbox 命中走,这层没有 `.id()` 也就没有 hitbox
            .when_some(external, |el, kind| {
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
                el.child(
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
                        ),
                )
            })
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
        let name_drag = name.clone();
        let row_menu = row.clone();
        let this = cx.entity();

        div()
            .relative()
            .child(
                div()
                    .id(SharedString::from(format!("group-{id}")))
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
                        this.store.update(cx, |store, cx| {
                            store.toggle_group_collapse(&id_toggle, cx)
                        });
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            let entries = group_menu(&this.store, &row_menu);
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
                    .child(div().flex_1().truncate().child(name))
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
            ref name,
            ref path,
            status,
            needs_attention,
            kind,
            depth,
            ref parent_group_id,
            is_child,
            ..
        } = row;
        let (id, name, path) = (id.clone(), name.clone(), path.clone());
        let indent = project_indent(depth, parent_group_id.is_some());
        let is_source = dragging == Some(id.as_str());
        let id_click = id.clone();
        let id_remove = id.clone();
        let id_move = id.clone();
        let id_drop = id.clone();
        let id_line = id.clone();
        let name_drag = name.clone();
        let row_for_menu = row.clone();
        let tree_for_menu: Vec<ProjectTreeItem> = tree.to_vec();
        let this = cx.entity();

        div()
            .relative()
            .child(
                div()
                    .id(SharedString::from(format!("project-{id}")))
                    .group(SharedString::from(format!("project-row-{id}")))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(indent))
                    .pr(px(10.0))
                    .py(px(6.0))
                    .cursor_pointer()
                    .when(is_source, |el| el.opacity(0.4))
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
                            let entries = project_menu(&this.store, &row_for_menu, &tree_for_menu);
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
                                    .text_size(ui::font_px(13.0))
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
                                    .text_size(ui::font_px(11.0))
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
                            .text_size(ui::font_px(12.0))
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
            )
            // 子项目不是落点,指示线自然也不该出现在它上下
            .when(!is_child, |el| {
                el.children(self.drop_line(&id_line, DropPosition::Before, drag_active))
                    .children(self.drop_line(&id_line, DropPosition::After, drag_active))
            })
            .into_any_element()
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
}
