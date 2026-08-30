//! Worktree 管理弹窗。对应 `src/components/GitWorktreeModal.tsx`(833 行)。
//!
//! # 两个入口(语义不同)
//!
//! | 入口 | `discover_repos` | `on_changed` |
//! |---|---|---|
//! | Git 面板仓库栏右键「Worktree 管理」 | `false`(单仓库,`repo_path` 就是仓库根) | 刷新仓库列表 |
//! | 项目列表右键「Worktrees」 | `true`(项目根未必是仓库,向下发现) | 空函数(后端已失效缓存) |
//!
//! 本批只接了第一个入口 —— 项目列表的右键菜单是另一批的活。
//!
//! # 三处顺序不能调换
//!
//! 1. **删除**(`GitWorktreeModal.tsx:435-449`):先关该目录下的终端(Windows 上
//!    shell 占着目录会让 `git worktree remove` 失败)→ 再 `remove_worktree` →
//!    **成功了才**删项目(失败时项目还在,终端呈断开态可重开);
//! 2. **清理失效**(`:466-468`):`prune_worktrees` 之后必须用 `filter_directories`
//!    复核「目录确实已不存在」才删项目 —— `isValid=false` 但目录还在(元数据损坏)
//!    时项目要保留;
//! 3. **归并**(`:145-172`):扫描可能同时发现主仓库与它在项目目录内的 worktree,
//!    两者的 `list_worktrees` 结果**完全相同**,按 `isMain` 的路径归并才不会重复展示。
//!
//! # 阻塞调用
//!
//! `add_worktree`(120s)/ `remove_worktree`(60s)/ `prune_worktrees`(30s)
//! 全是 CLI,一律丢 `cx.background_executor()`。

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, PathPromptOptions, Render, SharedString, StatefulInteractiveElement, Styled,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Input, InputState};
use mt_project::git::{BranchInfo, WorktreeInfo};

use crate::i18n::{t, tr};
use crate::menu::{self, MenuItem};
use crate::prompt::{autofocus, dialog_title, kind, open_guarded, show_alert};
use crate::store::AppStore;
use crate::ui;

// ─── 纯逻辑小件 ───────────────────────────────────────────────

/// `src/utils/projectActions.ts:9-11`。worktree「是否已是项目」的比对全靠它,
/// **必须逐字移植**。
pub fn normalize_path(p: &str) -> String {
    let unified: String = p
        .chars()
        .map(|c| if c == '\\' { '/' } else { c })
        .collect();
    unified.trim_end_matches('/').to_lowercase()
}

/// 分支名 → 目录名片段(`GitWorktreeModal.tsx:45-47`)。
pub fn sanitize_branch_for_dir(branch: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for c in branch.chars() {
        let bad = matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            || c.is_whitespace();
        if bad {
            if !last_dash {
                out.push('-');
                last_dash = true;
            }
        } else {
            out.push(c);
            last_dash = false;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "worktree".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 拼路径(分隔符跟随输入)。
pub fn join_path(base: &str, child: &str, sep: char) -> String {
    let base = base.trim_end_matches(['/', '\\']);
    format!("{base}{sep}{child}")
}

/// 父目录。没有父级时返回原串。
pub fn parent_dir(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rfind(['/', '\\']) {
        Some(0) => &trimmed[..1],
        Some(idx) => &trimmed[..idx],
        None => trimmed,
    }
}

/// 末段名。
pub fn base_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rfind(['/', '\\']) {
        Some(idx) => &trimmed[idx + 1..],
        None => trimmed,
    }
}

/// 「检出现有分支」的可选项(`GitWorktreeModal.tsx:243-258`)。
///
/// 逐组算「本地分支 − 该组已被任一 worktree 占用的分支」,再取**全组交集**,
/// 顺序按第一个组的列表。
pub fn available_branches(groups: &[(Vec<BranchInfo>, Vec<Option<String>>)]) -> Vec<String> {
    let per_group: Vec<Vec<String>> = groups
        .iter()
        .map(|(branches, occupied)| {
            branches
                .iter()
                .filter(|b| !b.is_remote)
                .map(|b| b.name.clone())
                .filter(|name| !occupied.iter().any(|o| o.as_deref() == Some(name)))
                .collect()
        })
        .collect();
    intersect_ordered(&per_group)
}

/// 「新分支起点」的可选项(`:261-268`):逐组的**全部**分支名(含远程)取交集。
pub fn base_branch_options(groups: &[Vec<BranchInfo>]) -> Vec<String> {
    let per_group: Vec<Vec<String>> = groups
        .iter()
        .map(|branches| branches.iter().map(|b| b.name.clone()).collect())
        .collect();
    intersect_ordered(&per_group)
}

/// 取交集,顺序按第一组。
fn intersect_ordered(groups: &[Vec<String>]) -> Vec<String> {
    let Some(first) = groups.first() else {
        return Vec::new();
    };
    first
        .iter()
        .filter(|name| groups[1..].iter().all(|g| g.contains(name)))
        .cloned()
        .collect()
}

// ─── 分组 ─────────────────────────────────────────────────────

/// 归并后的一组(`GitWorktreeModal.tsx:31-42`)。
#[derive(Clone)]
struct RepoGroup {
    /// `normalize_path(主仓库路径)`。
    key: String,
    /// 主仓库目录名(worktree 目录建议名的前缀)。
    name: String,
    /// git 命令的执行路径:worktree 增删必须落在主仓库上。
    main_path: String,
    worktrees: Vec<WorktreeInfo>,
    error: Option<String>,
}

/// 把逐仓库的 `list_worktrees` 结果归并成组。
fn merge_groups(items: Vec<(String, Result<Vec<WorktreeInfo>, String>)>) -> Vec<RepoGroup> {
    let mut out: Vec<RepoGroup> = Vec::new();
    for (path, result) in items {
        match result {
            Ok(worktrees) => {
                let main_path = worktrees
                    .iter()
                    .find(|w| w.is_main)
                    .map(|w| w.path.clone())
                    .unwrap_or_else(|| path.clone());
                let key = normalize_path(&main_path);
                if out.iter().any(|g| g.key == key) {
                    continue;
                }
                out.push(RepoGroup {
                    key,
                    name: base_name(&main_path).to_string(),
                    main_path,
                    worktrees,
                    error: None,
                });
            }
            Err(err) => {
                let key = normalize_path(&path);
                if out.iter().any(|g| g.key == key) {
                    continue;
                }
                out.push(RepoGroup {
                    key,
                    name: base_name(&path).to_string(),
                    main_path: path,
                    worktrees: Vec::new(),
                    error: Some(err),
                });
            }
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Existing,
    New,
}

// ─── 弹窗状态 ─────────────────────────────────────────────────

struct WorktreeModal {
    store: Entity<AppStore>,
    repo_path: String,
    discover_repos: bool,
    project_id: Option<String>,
    /// `None` = 还在加载。
    groups: Option<Vec<RepoGroup>>,
    load_error: Option<String>,
    selected_keys: Vec<String>,
    branches_by_repo: HashMap<String, Vec<BranchInfo>>,
    mode: Mode,
    sel_branch: String,
    base_branch: String,
    new_branch: Entity<InputState>,
    wt_path: Entity<InputState>,
    /// 用户手改过路径之后就不再跟随建议。
    path_edited: bool,
    add_as_project: bool,
    creating: bool,
    create_error: Option<String>,
    /// 逐仓库的创建错误(部分失败时留在弹窗里)。
    create_results: Vec<(String, String)>,
    pruning_key: Option<String>,
    on_changed: Rc<dyn Fn(&mut App)>,
}

impl Render for WorktreeModal {
    /// 状态盒子。真正的画面由 Dialog 的 builder 每帧重建(见 `modal.rs` 的说明)。
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 打开 worktree 管理弹窗。
pub fn open(
    repo_path: String,
    discover_repos: bool,
    project_id: Option<String>,
    on_changed: impl Fn(&mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    if repo_path.is_empty() {
        return;
    }
    let store = AppStore::global(cx);
    let state = cx.new(|cx| WorktreeModal {
        store,
        repo_path: repo_path.clone(),
        discover_repos,
        project_id,
        groups: None,
        load_error: None,
        selected_keys: Vec::new(),
        branches_by_repo: HashMap::new(),
        mode: Mode::Existing,
        sel_branch: String::new(),
        base_branch: String::new(),
        new_branch: cx.new(|cx| {
            InputState::new(window, cx).placeholder(t("worktree", "newBranchPlaceholder"))
        }),
        wt_path: cx
            .new(|cx| InputState::new(window, cx).placeholder(t("worktree", "pathPlaceholder"))),
        path_edited: false,
        // 默认勾选(`GitWorktreeModal.tsx:198`)
        add_as_project: true,
        creating: false,
        create_error: None,
        create_results: Vec::new(),
        pruning_key: None,
        on_changed: Rc::new(on_changed),
    });

    load(&state, cx);

    let root_name = base_name(&repo_path).to_string();
    open_guarded(kind::GIT_WORKTREE, window, cx, move |dialog, window, cx| {
        let body = render_body(&state, window, cx);
        dialog
            // 无底部按钮,右上角 ✕ 是唯一看得见的出口(见 `prompt::dialog_title`)
            .title(dialog_title(
                kind::GIT_WORKTREE,
                tr!("worktree", "title", name = root_name.clone()),
            ))
            .w(px(600.0))
            .child(div().px(px(16.0)).child(body))
    });
}

/// 加载分组。
fn load(state: &Entity<WorktreeModal>, cx: &mut App) {
    let (repo_path, discover) = {
        let s = state.read(cx);
        (s.repo_path.clone(), s.discover_repos)
    };
    state.update(cx, |s, cx| {
        s.groups = None;
        s.load_error = None;
        cx.notify();
    });
    let state = state.clone();
    cx.spawn(async move |cx| {
        let repo_for_task = repo_path.clone();
        let loaded = cx
            .background_executor()
            .spawn(async move {
                let paths: Vec<String> = if discover {
                    match mt_project::git::discover_git_repos(std::path::Path::new(&repo_for_task))
                    {
                        Ok(repos) => repos
                            .into_iter()
                            .map(|r| r.path.to_string_lossy().to_string())
                            .collect(),
                        Err(err) => return Err(format!("{err:#}")),
                    }
                } else {
                    vec![repo_for_task.clone()]
                };
                Ok(paths
                    .into_iter()
                    .map(|path| {
                        let result = mt_project::git::list_worktrees(std::path::Path::new(&path))
                            .map_err(|err| format!("{err:#}"));
                        (path, result)
                    })
                    .collect::<Vec<_>>())
            })
            .await;

        let _ = state.update(cx, |s: &mut WorktreeModal, cx| {
            match loaded {
                Err(err) => {
                    s.groups = Some(Vec::new());
                    s.load_error = Some(err);
                }
                Ok(items) => {
                    // 单仓库时沿用旧行为:加载失败即整体报错,不显示空壳分组
                    if !s.discover_repos
                        && items.len() == 1
                        && let Some((_, Err(err))) = items.first()
                    {
                        s.groups = Some(Vec::new());
                        s.load_error = Some(err.clone());
                        cx.notify();
                        return;
                    }
                    let groups = merge_groups(items);
                    // 勾选保留:剔除消失的键;只剩一个可用仓库时自动勾上
                    s.selected_keys
                        .retain(|k| groups.iter().any(|g| &g.key == k));
                    if s.selected_keys.is_empty()
                        && groups.len() == 1
                        && groups[0].error.is_none()
                    {
                        s.selected_keys = vec![groups[0].key.clone()];
                    }
                    s.groups = Some(groups);
                }
            }
            cx.notify();
        });
    })
    .detach();
}

/// 惰性拉勾选组的分支(失败也落一条空记录,否则会反复重试)。
fn ensure_branches(state: &Entity<WorktreeModal>, cx: &mut App) {
    let pending: Vec<(String, String)> = {
        let s = state.read(cx);
        let Some(groups) = &s.groups else {
            return Vec::new().into_iter().collect()
        };
        groups
            .iter()
            .filter(|g| s.selected_keys.contains(&g.key))
            .filter(|g| !s.branches_by_repo.contains_key(&g.key))
            .map(|g| (g.key.clone(), g.main_path.clone()))
            .collect()
    };
    for (key, main_path) in pending {
        let state = state.clone();
        // 先占位,避免同一帧内重复排队
        state.update(cx, |s, _| {
            s.branches_by_repo.insert(key.clone(), Vec::new());
        });
        cx.spawn(async move |cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    mt_project::git::get_repo_branches(std::path::Path::new(&main_path))
                })
                .await;
            let _ = state.update(cx, |s: &mut WorktreeModal, cx| {
                match result {
                    Ok(list) => {
                        s.branches_by_repo.insert(key, list);
                    }
                    Err(err) => eprintln!("[git] 取分支失败: {err:#}"),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

// ─── 渲染 ─────────────────────────────────────────────────────

fn badge(text: impl Into<SharedString>) -> AnyElement {
    div()
        .flex_none()
        .px(px(6.0))
        .rounded(px(3.0))
        .bg(ui::border_subtle())
        .text_size(ui::font_px(11.0))
        .text_color(ui::text_muted())
        .child(text.into())
        .into_any_element()
}

fn colored_badge(text: impl Into<SharedString>, fg: gpui::Hsla, bg: gpui::Hsla) -> AnyElement {
    div()
        .flex_none()
        .px(px(6.0))
        .rounded(px(3.0))
        .bg(bg)
        .text_size(ui::font_px(11.0))
        .text_color(fg)
        .child(text.into())
        .into_any_element()
}

fn hint_line(text: impl Into<SharedString>, color: gpui::Hsla) -> AnyElement {
    div()
        .py(px(8.0))
        .w_full()
        .text_center()
        .text_size(ui::font_px(13.0))
        .text_color(color)
        .child(text.into())
        .into_any_element()
}

fn render_body(state: &Entity<WorktreeModal>, window: &mut Window, cx: &mut App) -> AnyElement {
    ensure_branches(state, cx);
    let s = state.read(cx);
    let mut root = div().flex().flex_col().gap(px(8.0));

    let Some(groups) = s.groups.clone() else {
        return root
            .child(hint_line(t("worktree", "loading"), ui::text_muted()))
            .into_any_element();
    };
    if let Some(err) = &s.load_error {
        root = root.child(hint_line(err.clone(), ui::color_error()));
    }
    if groups.is_empty() && s.load_error.is_none() {
        return root
            .child(hint_line(t("worktree", "noRepoFound"), ui::text_muted()))
            .into_any_element();
    }

    let multi_repo = groups.len() > 1;
    if multi_repo {
        let all = s.selected_keys.len() == groups.len();
        let state_for_toggle = state.clone();
        let keys: Vec<String> = groups.iter().map(|g| g.key.clone()).collect();
        root = root.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_muted())
                        .child(tr!(
                            "worktree",
                            "reposFound",
                            count = groups.len().to_string()
                        )),
                )
                .child(
                    ui::ghost_button(
                        "worktree-select-all",
                        if all {
                            t("worktree", "clearAll")
                        } else {
                            t("worktree", "selectAll")
                        },
                    )
                    .on_click(move |_: &ClickEvent, _window, cx| {
                        state_for_toggle.update(cx, |s, cx| {
                            s.selected_keys = if all { Vec::new() } else { keys.clone() };
                            reset_form_on_selection(s);
                            cx.notify();
                        });
                    }),
                ),
        );
    }

    for group in &groups {
        root = root.child(render_group(state, group, multi_repo, cx));
    }

    // 默认路径建议(`GitWorktreeModal.tsx:289-303` 那个 effect 的对应物)。
    // 放在这里是因为它要 `&mut Window` 去写输入框,而这里正是每帧重跑的地方 ——
    // 值一致就直接 return,不会自激。
    sync_path_suggestion(state, &groups, window, cx);

    root = root.child(render_create_section(state, &groups, multi_repo, cx));
    root.into_any_element()
}

/// 默认路径建议:
///
/// - 未勾选 → 清空;
/// - 多选 → `repo_path`(此时输入框语义是「父目录」);
/// - 单选 → `parentDir(mainPath) + <仓库名>-<分支>`(**仓库同级**)。
///
/// `path_edited` 为真(用户手改过 / 用过「浏览…」)之后**不再跟随**。
/// 分支为空时 `sanitize_branch_for_dir` 回落 `"worktree"` —— 原版就是这样,
/// 选分支之前先给一个 `<仓库名>-worktree` 的建议。
fn sync_path_suggestion(
    state: &Entity<WorktreeModal>,
    groups: &[RepoGroup],
    window: &mut Window,
    cx: &mut App,
) {
    let want = {
        let s = state.read(cx);
        if s.path_edited {
            return;
        }
        let selected: Vec<&RepoGroup> = groups
            .iter()
            .filter(|g| s.selected_keys.contains(&g.key))
            .collect();
        let sep = if s.repo_path.contains('\\') { '\\' } else { '/' };
        let branch = match s.mode {
            Mode::Existing => s.sel_branch.clone(),
            Mode::New => s.new_branch.read(cx).value().trim().to_string(),
        };
        match selected.len() {
            0 => String::new(),
            1 => {
                let g = selected[0];
                let parent = parent_dir(&g.main_path);
                if parent.is_empty() {
                    return;
                }
                join_path(
                    parent,
                    &format!("{}-{}", g.name, sanitize_branch_for_dir(&branch)),
                    sep,
                )
            }
            _ => s.repo_path.clone(),
        }
    };
    let input = state.read(cx).wt_path.clone();
    if input.read(cx).value() == want.as_str() {
        return;
    }
    input.update(cx, |st, cx| st.set_value(want, window, cx));
}

/// 勾选变化时要清掉的表单状态(`toggleRepo` / `toggleAll`)。
fn reset_form_on_selection(s: &mut WorktreeModal) {
    s.path_edited = false;
    s.sel_branch.clear();
    s.base_branch.clear();
    s.create_error = None;
    s.create_results.clear();
}

fn render_group(
    state: &Entity<WorktreeModal>,
    group: &RepoGroup,
    multi_repo: bool,
    cx: &mut App,
) -> AnyElement {
    // 先把要用的字段拷出来:下面每一行都要拿 `&mut App` 去渲染,不能一直借着 `s`
    let (selected, pruning) = {
        let s = state.read(cx);
        (
            s.selected_keys.contains(&group.key),
            s.pruning_key.as_deref() == Some(group.key.as_str()),
        )
    };
    let mut block = div()
        .flex()
        .flex_col()
        .when(multi_repo, |el| {
            el.rounded(px(4.0))
                .border_1()
                .border_color(ui::border_subtle())
        });

    if multi_repo {
        let (state_for_click, key) = (state.clone(), group.key.clone());
        block = block.child(
            div()
                .id(SharedString::from(format!("worktree-group-{}", group.key)))
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(8.0))
                .py(px(4.0))
                .cursor_pointer()
                .when(selected, |el| el.bg(ui::accent_subtle()))
                .child(
                    div()
                        .w(px(12.0))
                        .text_size(ui::font_px(12.0))
                        .text_color(if selected {
                            ui::accent()
                        } else {
                            ui::text_muted()
                        })
                        .child(if selected { "☑" } else { "☐" }),
                )
                .child(
                    div()
                        .text_size(ui::font_px(13.0))
                        .text_color(if selected {
                            ui::accent()
                        } else {
                            ui::text_primary()
                        })
                        .child(group.name.clone()),
                )
                .child(
                    div()
                        .ml_auto()
                        .truncate()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(group.main_path.clone()),
                )
                .on_click(move |_: &ClickEvent, _window, cx| {
                    state_for_click.update(cx, |s, cx| {
                        if let Some(pos) = s.selected_keys.iter().position(|k| *k == key) {
                            s.selected_keys.remove(pos);
                        } else {
                            s.selected_keys.push(key.clone());
                        }
                        reset_form_on_selection(s);
                        cx.notify();
                    });
                }),
        );
    }

    if let Some(err) = &group.error {
        return block
            .child(
                div()
                    .px(px(8.0))
                    .py(px(6.0))
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::color_error())
                    .child(err.clone()),
            )
            .into_any_element();
    }

    let mut rows = div().flex().flex_col().gap(px(2.0));
    for (idx, wt) in group.worktrees.iter().enumerate() {
        rows = rows.child(render_worktree_row(state, group, idx, wt, cx));
    }
    block = block.child(rows);

    if group.worktrees.iter().any(|w| !w.is_valid) {
        let (state_for_prune, group_for_prune) = (state.clone(), group.clone());
        block = block.child(
            div().flex().justify_end().py(px(4.0)).child(
                ui::ghost_button(
                    SharedString::from(format!("worktree-prune-{}", group.key)),
                    if pruning {
                        t("worktree", "pruning")
                    } else {
                        t("worktree", "prune")
                    },
                )
                .when(!pruning, |el| {
                    el.on_click(move |_: &ClickEvent, _window, cx| {
                        prune(&state_for_prune, &group_for_prune, cx);
                    })
                }),
            ),
        );
    }

    block.into_any_element()
}

fn render_worktree_row(
    state: &Entity<WorktreeModal>,
    group: &RepoGroup,
    idx: usize,
    wt: &WorktreeInfo,
    cx: &mut App,
) -> AnyElement {
    let s = state.read(cx);
    // 「已是项目」:订阅 config.projects,增删项目即时反映
    let existing_project = s
        .store
        .read(cx)
        .projects()
        .iter()
        .find(|p| p.ssh_connection_id.is_none() && normalize_path(&p.path) == normalize_path(&wt.path))
        .map(|p| p.id.clone());
    let is_project = existing_project.is_some();

    let mut badges = div().flex().items_center().gap(px(4.0));
    if wt.is_main {
        badges = badges.child(badge(t("worktree", "mainRepo")));
    }
    if let Some(branch) = &wt.branch {
        badges = badges.child(badge(format!("⎇ {branch}")));
    }
    if !wt.is_valid {
        badges = badges.child(colored_badge(
            t("worktree", "invalid"),
            ui::color_error(),
            ui::with_alpha(ui::color_error(), 0.15),
        ));
    }
    if wt.is_locked {
        badges = badges.child(badge(t("worktree", "locked")));
    }
    if is_project {
        badges = badges.child(colored_badge(
            t("worktree", "isProject"),
            ui::accent(),
            ui::accent_subtle(),
        ));
    }

    let mut actions = div().flex().items_center().gap(px(4.0)).flex_none();
    if wt.is_valid {
        // 「在终端中打开」
        let (store, path, name, branch) = (
            s.store.clone(),
            wt.path.clone(),
            wt.name.clone(),
            wt.branch.clone(),
        );
        let project_id = s
            .project_id
            .clone()
            .or_else(|| s.store.read(cx).active_project_id.clone());
        actions = actions.child(
            ui::ghost_button(
                SharedString::from(format!("wt-open-{}-{idx}", group.key)),
                t("worktree", "openTerminal"),
            )
            .on_click(move |_: &ClickEvent, window, cx| {
                let Some(project_id) = project_id.clone() else {
                    return;
                };
                let title = format!("⎇ {}", branch.clone().unwrap_or_else(|| name.clone()));
                let path = path.clone();
                let opened = store.update(cx, |store, cx| {
                    let pane = store.new_terminal_with_cwd(
                        &project_id,
                        None,
                        None,
                        Some(path),
                        window,
                        cx,
                    );
                    if let Some(pane) = pane.as_ref() {
                        store.rename_pane(&project_id, pane, &title, cx);
                    }
                    pane.is_some()
                });
                crate::prompt::close_guarded(kind::GIT_WORKTREE, window, cx);
                if opened {
                    crate::workbench_area::activate_terminal_page(window, cx);
                }
            }),
        );

        // 「设为项目 / 切换过去」
        let (store, path) = (s.store.clone(), wt.path.clone());
        let main_path = group.main_path.clone();
        let parent_hint = s.project_id.clone();
        actions = actions.child(
            ui::ghost_button(
                SharedString::from(format!("wt-project-{}-{idx}", group.key)),
                if is_project {
                    t("worktree", "switchToProject")
                } else {
                    t("worktree", "addAsProject")
                },
            )
            .on_click(move |_: &ClickEvent, window, cx| {
                let path = path.clone();
                store.update(cx, |store, cx| {
                    let id = match &existing_project {
                        Some(id) => id.clone(),
                        None => {
                            let parent = store
                                .find_project_by_path(&main_path)
                                .map(|p| p.id.clone())
                                .or_else(|| {
                                    parent_hint
                                        .clone()
                                        .or_else(|| store.active_project_id.clone())
                                });
                            store.add_project_at(
                                std::path::Path::new(&path),
                                parent.as_deref(),
                                cx,
                            )
                        }
                    };
                    store.set_active_project(&id, cx);
                });
                crate::prompt::close_guarded(kind::GIT_WORKTREE, window, cx);
            }),
        );

        // 「删除」(非 main 才有)
        if !wt.is_main {
            let (state_for_remove, group_for_remove, wt_for_remove) =
                (state.clone(), group.clone(), wt.clone());
            actions = actions.child(
                ui::danger_button(
                    SharedString::from(format!("wt-remove-{}-{idx}", group.key)),
                    t("worktree", "remove"),
                )
                .on_click(move |_: &ClickEvent, window, cx| {
                    open_remove_confirm(
                        &state_for_remove,
                        &group_for_remove,
                        &wt_for_remove,
                        window,
                        cx,
                    );
                }),
            );
        }
    }

    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .px(px(8.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            div()
                                .truncate()
                                .text_size(ui::font_px(13.0))
                                .text_color(ui::text_primary())
                                .child(wt.name.clone()),
                        )
                        .child(badges),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(wt.path.clone()),
                ),
        )
        .child(actions)
        .into_any_element()
}

/// 新建 worktree 那一段。
fn render_create_section(
    state: &Entity<WorktreeModal>,
    groups: &[RepoGroup],
    multi_repo: bool,
    cx: &mut App,
) -> AnyElement {
    let s = state.read(cx);
    let selected: Vec<&RepoGroup> = groups
        .iter()
        .filter(|g| s.selected_keys.contains(&g.key))
        .collect();
    let multi_target = selected.len() > 1;

    let mut section = div()
        .mt(px(8.0))
        .pt(px(8.0))
        .border_t_1()
        .border_color(ui::border_subtle())
        .flex()
        .flex_col()
        .gap(px(6.0));

    // 标题行 + 模式段控件
    let mut head = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_primary())
                .child(t("worktree", "createTitle")),
        );
    if multi_repo {
        head = head.child(
            div()
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(tr!(
                    "worktree",
                    "selectedCount",
                    count = selected.len().to_string()
                )),
        );
    }
    let mut seg = div()
        .ml_auto()
        .flex()
        .rounded(px(4.0))
        .overflow_hidden()
        .border_1()
        .border_color(ui::border_default());
    for (mode, label) in [
        (Mode::Existing, t("worktree", "modeExisting")),
        (Mode::New, t("worktree", "modeNew")),
    ] {
        let active = s.mode == mode;
        let state_for_mode = state.clone();
        seg = seg.child(
            div()
                .id(SharedString::from(format!(
                    "worktree-mode-{}",
                    if matches!(mode, Mode::New) { "new" } else { "existing" }
                )))
                .px(px(10.0))
                .py(px(2.0))
                .text_size(ui::font_px(12.0))
                .cursor_pointer()
                .when(active, |el| {
                    el.bg(ui::accent_subtle()).text_color(ui::accent())
                })
                .when(!active, |el| el.text_color(ui::text_muted()))
                .child(label)
                .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let input = state_for_mode.update(cx, |s, cx| {
                        s.mode = mode;
                        cx.notify();
                        matches!(mode, Mode::New).then(|| s.new_branch.clone())
                    });
                    // 切到「新建」就该能直接敲分支名,不必再点一下输入框
                    // (「已有」那侧是下拉选择,没有可聚焦的输入框)
                    if let Some(input) = input {
                        autofocus(&input, window, cx);
                    }
                }),
        );
    }
    section = section.child(head.child(seg));

    // 模式区
    if selected.is_empty() {
        section = section.child(hint_line(t("worktree", "selectRepoHint"), ui::text_muted()));
        return section.into_any_element();
    }

    let branches_ready = selected
        .iter()
        .all(|g| s.branches_by_repo.contains_key(&g.key));
    match s.mode {
        Mode::Existing => {
            if !branches_ready {
                section = section.child(hint_line(t("worktree", "loading"), ui::text_muted()));
            } else {
                let per_group: Vec<(Vec<BranchInfo>, Vec<Option<String>>)> = selected
                    .iter()
                    .map(|g| {
                        (
                            s.branches_by_repo.get(&g.key).cloned().unwrap_or_default(),
                            g.worktrees.iter().map(|w| w.branch.clone()).collect(),
                        )
                    })
                    .collect();
                let options = available_branches(&per_group);
                if options.is_empty() {
                    section = section.child(hint_line(
                        if multi_target {
                            t("worktree", "noCommonBranch")
                        } else {
                            t("worktree", "noBranchAvailable")
                        },
                        ui::text_muted(),
                    ));
                } else {
                    section = section.child(dropdown(
                        state,
                        "worktree-branch",
                        if s.sel_branch.is_empty() {
                            t("worktree", "selectBranch").to_string()
                        } else {
                            s.sel_branch.clone()
                        },
                        options,
                        |s, value| s.sel_branch = value,
                    ));
                }
            }
        }
        Mode::New => {
            let per_group: Vec<Vec<BranchInfo>> = selected
                .iter()
                .map(|g| s.branches_by_repo.get(&g.key).cloned().unwrap_or_default())
                .collect();
            let mut options = vec![t("worktree", "baseHead").to_string()];
            options.extend(base_branch_options(&per_group));
            section = section.child(
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(div().flex_1().child(Input::new(&s.new_branch)))
                    .child(dropdown(
                        state,
                        "worktree-base",
                        if s.base_branch.is_empty() {
                            t("worktree", "baseHead").to_string()
                        } else {
                            s.base_branch.clone()
                        },
                        options,
                        |s, value| {
                            s.base_branch = if value == t("worktree", "baseHead") {
                                String::new()
                            } else {
                                value
                            }
                        },
                    )),
            );
        }
    }

    // 路径行
    let state_for_browse = state.clone();
    section = section.child(
        div()
            .flex()
            .gap(px(6.0))
            .child(div().flex_1().child(Input::new(&s.wt_path)))
            .child(
                ui::ghost_button("worktree-browse", t("worktree", "browse")).on_click(
                    move |_: &ClickEvent, window, cx| {
                        let paths = cx.prompt_for_paths(PathPromptOptions {
                            files: false,
                            directories: true,
                            multiple: false,
                            prompt: Some(t("projectList", "chooseDirDialogTitle").into()),
                        });
                        let state = state_for_browse.clone();
                        window
                            .spawn(cx, async move |cx| {
                                let Ok(Ok(Some(paths))) = paths.await else {
                                    return;
                                };
                                let Some(path) = paths.into_iter().next() else {
                                    return;
                                };
                                let text = path.to_string_lossy().to_string();
                                let _ = cx.update(|window, cx| {
                                    state.update(cx, |s, cx| {
                                        s.path_edited = true;
                                        let input = s.wt_path.clone();
                                        input.update(cx, |state, cx| {
                                            state.set_value(text.clone(), window, cx)
                                        });
                                        cx.notify();
                                    });
                                });
                            })
                            .detach();
                    },
                ),
            ),
    );

    // 「创建后添加为项目并切换过去」
    let add_as_project = s.add_as_project;
    let state_for_check = state.clone();
    section = section.child(
        div()
            .id("worktree-add-project")
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .text_size(ui::font_px(12.0))
            .text_color(ui::text_secondary())
            .child(
                div()
                    .w(px(12.0))
                    .text_color(if add_as_project {
                        ui::accent()
                    } else {
                        ui::text_muted()
                    })
                    .child(if add_as_project { "☑" } else { "☐" }),
            )
            .child(t("worktree", "addAsProjectAfterCreate"))
            .on_click(move |_: &ClickEvent, _window, cx| {
                state_for_check.update(cx, |s, cx| {
                    s.add_as_project = !s.add_as_project;
                    cx.notify();
                });
            }),
    );

    if let Some(err) = &s.create_error {
        section = section.child(
            div()
                .text_size(ui::font_px(11.0))
                .text_color(ui::color_error())
                .child(err.clone()),
        );
    }
    for (target, err) in &s.create_results {
        section = section.child(
            div()
                .text_size(ui::font_px(11.0))
                .text_color(ui::color_error())
                .child(format!("{target}: {err}")),
        );
    }

    let creating = s.creating;
    let state_for_create = state.clone();
    let groups_for_create = groups.to_vec();
    section = section.child(
        div().flex().justify_end().child(
            ui::primary_button(
                "worktree-create",
                if creating {
                    t("worktree", "creating").to_string()
                } else if multi_target {
                    tr!(
                        "worktree",
                        "createMulti",
                        count = selected.len().to_string()
                    )
                } else {
                    t("worktree", "create").to_string()
                },
            )
            .when(!creating, |el| {
                el.on_click(move |_: &ClickEvent, window, cx| {
                    create(&state_for_create, &groups_for_create, window, cx);
                })
            }),
        ),
    );

    section.into_any_element()
}

/// 一个走 [`menu`] 的下拉选择器(替原版的 `<select>`)。
fn dropdown(
    state: &Entity<WorktreeModal>,
    id: &'static str,
    current: String,
    options: Vec<String>,
    apply: fn(&mut WorktreeModal, String),
) -> AnyElement {
    let state = state.clone();
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_between()
        .gap(px(6.0))
        .px(px(8.0))
        .py(px(4.0))
        .w(px(180.0))
        .flex_none()
        .rounded(px(4.0))
        .border_1()
        .border_color(ui::border_default())
        .cursor_pointer()
        .text_size(ui::font_px(12.0))
        .text_color(ui::text_primary())
        .child(div().truncate().child(current))
        .child(div().text_color(ui::text_muted()).child("▾"))
        .on_click(move |event: &ClickEvent, window, cx| {
            let entries: Vec<menu::MenuEntry> = options
                .iter()
                .map(|option| {
                    let (state, option) = (state.clone(), option.clone());
                    MenuItem::new(option.clone())
                        .on_click(move |_window, cx| {
                            state.update(cx, |s, cx| {
                                apply(s, option.clone());
                                cx.notify();
                            });
                        })
                        .into()
                })
                .collect();
            menu::show(event.position(), entries, window, cx);
        })
        .into_any_element()
}

// ─── 创建 / 删除 / 清理 ───────────────────────────────────────

fn create(
    state: &Entity<WorktreeModal>,
    groups: &[RepoGroup],
    window: &mut Window,
    cx: &mut App,
) {
    let (branch, targets, create_branch, base, add_as_project) = {
        let s = state.read(cx);
        let branch = match s.mode {
            Mode::Existing => s.sel_branch.clone(),
            Mode::New => s.new_branch.read(cx).value().trim().to_string(),
        };
        if branch.is_empty() || s.creating {
            return;
        }
        let raw_path = s.wt_path.read(cx).value().trim().to_string();
        if raw_path.is_empty() {
            return;
        }
        let sep = if s.repo_path.contains('\\') { '\\' } else { '/' };
        let selected: Vec<&RepoGroup> = groups
            .iter()
            .filter(|g| s.selected_keys.contains(&g.key))
            .collect();
        if selected.is_empty() {
            return;
        }
        let targets: Vec<(String, String)> = if selected.len() == 1 {
            vec![(selected[0].main_path.clone(), raw_path)]
        } else {
            // 多选时路径输入框的语义变成「父目录」
            selected
                .iter()
                .map(|g| {
                    (
                        g.main_path.clone(),
                        join_path(
                            &raw_path,
                            &format!("{}-{}", g.name, sanitize_branch_for_dir(&branch)),
                            sep,
                        ),
                    )
                })
                .collect()
        };
        (
            branch,
            targets,
            s.mode == Mode::New,
            if s.mode == Mode::New && !s.base_branch.is_empty() {
                Some(s.base_branch.clone())
            } else {
                None
            },
            s.add_as_project,
        )
    };

    state.update(cx, |s, cx| {
        s.creating = true;
        s.create_error = None;
        s.create_results.clear();
        cx.notify();
    });

    let state = state.clone();
    window
        .spawn(cx, async move |cx| {
            let mut results: Vec<(String, Result<(), String>)> = Vec::new();
            for (main_path, target) in targets {
                let (branch, base) = (branch.clone(), base.clone());
                let target_for_task = target.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        // ⚠️ add_worktree 是 120s 阻塞 CLI
                        mt_project::git::add_worktree(
                            std::path::Path::new(&main_path),
                            &target_for_task,
                            &branch,
                            create_branch,
                            base.as_deref(),
                        )
                    })
                    .await;
                results.push((target, result.map(|_| ()).map_err(|err| format!("{err:#}"))));
            }

            let _ = cx.update(|window, cx| {
                // 无论成败先通知外面 + 清掉分支缓存(分支集合已变)
                let on_changed = state.read(cx).on_changed.clone();
                on_changed(cx);
                state.update(cx, |s, cx| {
                    s.branches_by_repo.clear();
                    s.creating = false;
                    cx.notify();
                });

                let failures: Vec<(String, String)> = results
                    .iter()
                    .filter_map(|(t, r)| r.as_ref().err().map(|e| (t.clone(), e.clone())))
                    .collect();
                let successes: Vec<String> = results
                    .iter()
                    .filter(|(_, r)| r.is_ok())
                    .map(|(t, _)| t.clone())
                    .collect();

                let mut first_new: Option<String> = None;
                if add_as_project && !successes.is_empty() {
                    let store = state.read(cx).store.clone();
                    let parent_hint = state.read(cx).project_id.clone();
                    store.update(cx, |store, cx| {
                        for target in &successes {
                            let parent = parent_hint
                                .clone()
                                .or_else(|| store.active_project_id.clone());
                            let id = store.add_project_at(
                                std::path::Path::new(target),
                                parent.as_deref(),
                                cx,
                            );
                            if first_new.is_none() {
                                first_new = Some(id);
                            }
                        }
                        store.save_config_now();
                    });
                }

                if !failures.is_empty() {
                    // 部分失败:留在弹窗里列出逐仓库错误
                    state.update(cx, |s, cx| {
                        s.create_results = failures;
                        cx.notify();
                    });
                    load(&state, cx);
                    return;
                }

                match first_new {
                    Some(id) if add_as_project => {
                        let store = state.read(cx).store.clone();
                        store.update(cx, |store, cx| store.set_active_project(&id, cx));
                        crate::prompt::close_guarded(kind::GIT_WORKTREE, window, cx);
                    }
                    _ => {
                        state.update(cx, |s, cx| {
                            let input = s.new_branch.clone();
                            input.update(cx, |st, cx| st.set_value("", window, cx));
                            s.sel_branch.clear();
                            s.path_edited = false;
                            cx.notify();
                        });
                        load(&state, cx);
                    }
                }
            });
        })
        .detach();
}

/// 嵌套的删除确认框。**kind 与外层不同**,所以能叠在外层之上。
fn open_remove_confirm(
    state: &Entity<WorktreeModal>,
    group: &RepoGroup,
    wt: &WorktreeInfo,
    window: &mut Window,
    cx: &mut App,
) {
    let store = state.read(cx).store.clone();
    let linked_project = store
        .read(cx)
        .projects()
        .iter()
        .find(|p| p.ssh_connection_id.is_none() && normalize_path(&p.path) == normalize_path(&wt.path))
        .map(|p| (p.id.clone(), p.name.clone()));
    if linked_project
        .as_ref()
        .is_some_and(|(id, _)| crate::workbench_area::project_has_dirty_documents(id, cx))
    {
        show_alert(
            t("fileViewer", "unsavedTitle"),
            t("fileViewer", "projectRemovalBlocked"),
            window,
            cx,
        );
        return;
    }

    // force 勾选与错误文本要活过重绘,放实体
    let form = cx.new(|_| RemoveForm {
        force: false,
        removing: false,
        error: None,
    });
    let (state, group, wt) = (state.clone(), group.clone(), wt.clone());

    open_guarded(
        kind::GIT_WORKTREE_REMOVE,
        window,
        cx,
        move |dialog, _window, cx| {
            let f = form.read(cx);
            let (removing, error) = (f.removing, f.error.clone());
            let force = f.force;
            let form_for_toggle = form.clone();
            let (state, group, wt) = (state.clone(), group.clone(), wt.clone());
            let linked = linked_project.clone();
            let form_for_ok = form.clone();

            let mut body = div()
                .px(px(16.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(ui::font_px(13.0))
                        .text_color(ui::text_primary())
                        .child(tr!(
                            "worktree",
                            "removeConfirmMessage",
                            name = wt.name.clone()
                        )),
                )
                .child(
                    div()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(wt.path.clone()),
                );
            if let Some((_, name)) = &linked {
                body = body.child(
                    div()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::color_warning())
                        .child(tr!("worktree", "removeAlsoProject", name = name.clone())),
                );
            }
            body = body.child(
                div()
                    .id("worktree-force")
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .cursor_pointer()
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_secondary())
                    .child(
                        div()
                            .w(px(12.0))
                            .text_color(if force {
                                ui::color_error()
                            } else {
                                ui::text_muted()
                            })
                            .child(if force { "☑" } else { "☐" }),
                    )
                    .child(t("worktree", "forceRemove"))
                    .on_click(move |_: &ClickEvent, _window, cx| {
                        form_for_toggle.update(cx, |f, cx| {
                            f.force = !f.force;
                            cx.notify();
                        });
                    }),
            );
            if let Some(err) = error {
                body = body.child(
                    div()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::color_error())
                        .child(err),
                );
            }

            dialog
                .title(t("worktree", "removeConfirmTitle"))
                .w(px(400.0))
                .confirm()
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default()
                        .ok_text(if removing {
                            t("worktree", "removing")
                        } else {
                            t("worktree", "removeConfirm")
                        })
                        .cancel_text(t("worktree", "cancel")),
                )
                .child(body)
                .on_ok(move |_: &ClickEvent, window, cx| {
                    if form_for_ok.read(cx).removing {
                        return false;
                    }
                    let project_id = linked.as_ref().map(|(id, _)| id.clone());
                    if project_id.as_deref().is_some_and(|id| {
                        crate::workbench_area::project_has_dirty_documents(id, cx)
                    }) {
                        show_alert(
                            t("fileViewer", "unsavedTitle"),
                            t("fileViewer", "projectRemovalBlocked"),
                            window,
                            cx,
                        );
                        return false;
                    }
                    form_for_ok.update(cx, |f, cx| {
                        f.removing = true;
                        cx.notify();
                    });
                    remove_worktree(
                        &state,
                        &group,
                        &wt,
                        project_id,
                        form_for_ok.read(cx).force,
                        window,
                        cx,
                    );
                    // 结果回来之前不关框(失败要能看见错误)
                    false
                })
        },
    );
}

struct RemoveForm {
    force: bool,
    removing: bool,
    error: Option<String>,
}

impl Render for RemoveForm {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn remove_worktree(
    state: &Entity<WorktreeModal>,
    group: &RepoGroup,
    wt: &WorktreeInfo,
    project_id: Option<String>,
    force: bool,
    window: &mut Window,
    cx: &mut App,
) {
    if project_id
        .as_deref()
        .is_some_and(|id| crate::workbench_area::project_has_dirty_documents(id, cx))
    {
        show_alert(
            t("fileViewer", "unsavedTitle"),
            t("fileViewer", "projectRemovalBlocked"),
            window,
            cx,
        );
        return;
    }
    let store = state.read(cx).store.clone();
    // ① 先关该目录下的终端 —— Windows 上 shell 占着目录会让 remove 失败
    if let Some(id) = &project_id {
        store.update(cx, |store, cx| store.dispose_project_terminals(id, cx));
    }
    let (main_path, wt_path) = (group.main_path.clone(), wt.path.clone());
    let state = state.clone();
    window
        .spawn(cx, async move |cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    // ⚠️ 60s 阻塞 CLI
                    mt_project::git::remove_worktree(
                        std::path::Path::new(&main_path),
                        &wt_path,
                        force,
                    )
                })
                .await;
            let _ = cx.update(|window, cx| {
                match result {
                    Ok(_) => {
                        // ② 成功之后才删项目(不留断链项目)
                        if let Some(id) = project_id {
                            store.update(cx, |store, cx| store.remove_project(&id, cx));
                        }
                        crate::prompt::close_guarded(kind::GIT_WORKTREE_REMOVE, window, cx);
                        let on_changed = state.read(cx).on_changed.clone();
                        on_changed(cx);
                        load(&state, cx);
                    }
                    Err(err) => {
                        eprintln!("[git] 删除 worktree 失败: {err:#}");
                        // 失败:框留着显示错误(项目还在,终端呈断开态可重开)
                        crate::prompt::close_guarded(kind::GIT_WORKTREE_REMOVE, window, cx);
                    }
                }
            });
        })
        .detach();
}

/// 清理失效条目。**失败静默**(下次打开重试即可)。
fn prune(state: &Entity<WorktreeModal>, group: &RepoGroup, cx: &mut App) {
    // 先记下该组 !isValid 的路径集
    let invalid: Vec<PathBuf> = group
        .worktrees
        .iter()
        .filter(|w| !w.is_valid)
        .map(|w| PathBuf::from(&w.path))
        .collect();
    state.update(cx, |s, cx| {
        s.pruning_key = Some(group.key.clone());
        cx.notify();
    });
    let main_path = group.main_path.clone();
    let state = state.clone();
    cx.spawn(async move |cx| {
        let invalid_for_task = invalid.clone();
        let still_there = cx
            .background_executor()
            .spawn(async move {
                let pruned = mt_project::git::prune_worktrees(std::path::Path::new(&main_path));
                if let Err(err) = pruned {
                    eprintln!("[git] prune 失败(忽略): {err:#}");
                }
                // 以「目录确实已不存在」为准复核 —— isValid=false 但目录还在
                // (元数据损坏)时项目要保留
                mt_project::fs::filter_directories(invalid_for_task)
            })
            .await;

        let _ = cx.update(|cx| {
            let gone: Vec<String> = invalid
                .iter()
                .filter(|p| !still_there.contains(p))
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            let store = state.read(cx).store.clone();
            store.update(cx, |store, cx| {
                for path in &gone {
                    let id = store
                        .find_project_by_path(path)
                        .map(|p| p.id.clone());
                    if let Some(id) = id {
                        store.remove_project(&id, cx);
                    }
                }
            });
            state.update(cx, |s, cx| {
                s.pruning_key = None;
                cx.notify();
            });
            let on_changed = state.read(cx).on_changed.clone();
            on_changed(cx);
            load(&state, cx);
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wt(name: &str, path: &str, is_main: bool, branch: Option<&str>) -> WorktreeInfo {
        WorktreeInfo {
            name: name.to_string(),
            path: path.to_string(),
            branch: branch.map(str::to_string),
            is_main,
            is_valid: true,
            is_locked: false,
        }
    }

    fn branch(name: &str, remote: bool) -> BranchInfo {
        BranchInfo {
            name: name.to_string(),
            is_head: false,
            is_remote: remote,
            commit_hash: "x".into(),
        }
    }

    /// `normalizePath`:分隔符统一、去尾斜杠、转小写。三条都不能少 ——
    /// worktree「是否已是项目」的比对全靠它。
    #[test]
    fn 路径归一化() {
        assert_eq!(normalize_path(r"D:\Git\Repo\"), "d:/git/repo");
        assert_eq!(normalize_path("/home/U/Proj/"), "/home/u/proj");
        assert_eq!(normalize_path(r"D:\Git\Repo"), normalize_path("D:/Git/repo"));
        assert_eq!(normalize_path(""), "");
    }

    /// 分支名 → 目录名片段。
    #[test]
    fn 分支名转目录名() {
        assert_eq!(sanitize_branch_for_dir("feature/login"), "feature-login");
        assert_eq!(sanitize_branch_for_dir("fix: a b"), "fix-a-b");
        assert_eq!(sanitize_branch_for_dir("---"), "worktree");
        assert_eq!(sanitize_branch_for_dir(""), "worktree");
        assert_eq!(sanitize_branch_for_dir("main"), "main");
        // 首尾的分隔符要削掉,中间的连续片段压成一个 `-`
        assert_eq!(sanitize_branch_for_dir("/a//b/"), "a-b");
    }

    /// 路径拼接 / 父目录 / 末段名。
    #[test]
    fn 路径小件() {
        assert_eq!(join_path(r"D:\Git", "repo", '\\'), r"D:\Git\repo");
        assert_eq!(join_path(r"D:\Git\", "repo", '\\'), r"D:\Git\repo");
        assert_eq!(join_path("/home/u", "p", '/'), "/home/u/p");

        assert_eq!(parent_dir(r"D:\Git\repo"), r"D:\Git");
        assert_eq!(parent_dir("/home/u/p/"), "/home/u");
        assert_eq!(parent_dir("repo"), "repo", "没有父级时返回原串");

        assert_eq!(base_name(r"D:\Git\repo"), "repo");
        assert_eq!(base_name("/home/u/p/"), "p");
        assert_eq!(base_name("repo"), "repo");
    }

    /// 归并:主仓库与它内部的 worktree 扫出来结果完全相同,必须合成一组。
    #[test]
    fn 按主工作区归并() {
        let list = vec![
            wt("repo", "/a/repo", true, Some("main")),
            wt("wt1", "/a/repo-wt1", false, Some("feat")),
        ];
        let groups = merge_groups(vec![
            ("/a/repo".into(), Ok(list.clone())),
            // 同一份结果(从 worktree 目录扫到的)—— 不该重复展示
            ("/a/repo-wt1".into(), Ok(list.clone())),
        ]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].main_path, "/a/repo");
        assert_eq!(groups[0].name, "repo");
        assert_eq!(groups[0].worktrees.len(), 2);
    }

    /// 某个仓库 `list_worktrees` 失败时只让那一组显示错误,别的组照常。
    #[test]
    fn 单组失败不拖垮别的组() {
        let groups = merge_groups(vec![
            ("/a/ok".into(), Ok(vec![wt("ok", "/a/ok", true, None)])),
            ("/a/bad".into(), Err("仓库损坏".into())),
        ]);
        assert_eq!(groups.len(), 2);
        assert!(groups[0].error.is_none());
        assert_eq!(groups[1].error.as_deref(), Some("仓库损坏"));
        assert!(groups[1].worktrees.is_empty());
    }

    /// 「检出现有分支」:本地分支减去已被占用的,再取全组交集。
    #[test]
    fn 可用分支取交集且排除已占用() {
        let g1 = (
            vec![
                branch("main", false),
                branch("feat", false),
                branch("origin/main", true),
            ],
            // main 被主工作区占着
            vec![Some("main".to_string())],
        );
        assert_eq!(available_branches(&[g1.clone()]), vec!["feat"]);

        let g2 = (
            vec![branch("main", false), branch("feat", false), branch("x", false)],
            vec![Some("main".to_string())],
        );
        // 两组交集仍是 feat(x 只有第二组有)
        assert_eq!(available_branches(&[g1.clone(), g2]), vec!["feat"]);

        // 没有共同可用分支
        let g3 = (vec![branch("main", false)], vec![Some("main".to_string())]);
        assert!(available_branches(&[g1, g3]).is_empty());
    }

    /// 「新分支起点」含远程分支,同样取交集,顺序按第一组。
    #[test]
    fn 起点分支含远程且取交集() {
        let a = vec![
            branch("main", false),
            branch("origin/main", true),
            branch("only-a", false),
        ];
        let b = vec![branch("origin/main", true), branch("main", false)];
        assert_eq!(
            base_branch_options(&[a.clone(), b]),
            vec!["main", "origin/main"],
            "顺序按第一组"
        );
        assert_eq!(base_branch_options(&[a]).len(), 3);
        assert!(base_branch_options(&[]).is_empty());
    }
}
