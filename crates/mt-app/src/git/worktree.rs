//! Worktree 管理弹窗。对应 `src/components/GitWorktreeModal.tsx`(833 行)。
//!
//! # 两个入口(语义不同)
//!
//! | 入口 | `discover_repos` | `on_changed` |
//! |---|---|---|
//! | Git 面板仓库栏右键「Worktree 管理」 | `false`(单仓库,`repo_path` 就是仓库根) | 刷新仓库列表 |
//! | 项目列表右键「Worktrees」 | `true`(项目根未必是仓库,向下发现) | 空函数(后端已失效缓存) |
//!
//! # 三处顺序不能调换
//!
//! 1. **删除**(`GitWorktreeModal.tsx:435-449`):先关该目录下的终端(Windows 上
//!    shell 占着目录会让 `git worktree remove` 删到一半:目录删不掉、登记照样摘掉)——
//!    指向它的项目的终端,加上别的项目里启动目录在它里面的终端(确认框里逐个列出,
//!    见 [`panes_inside`])→ 再 `remove_worktree`(自带占用预检:还被别的程序占着就
//!    一步不做,报原因与下一步)→ **成功了才**删项目(失败时项目还在,终端呈断开态
//!    可重开);
//! 2. **清理失效**:prune 失败不阻断存在性复核；只有同 backend 的
//!    `existing_directories` 明确确认 NotFound 才删项目。权限 / 网络错误必须保留项目；
//!    `isValid=false` 但目录还在(元数据损坏)时项目也要保留。
//! 3. **归并**(`:145-172`):扫描可能同时发现主仓库与它在项目目录内的 worktree,
//!    两者的 `list_worktrees` 结果**完全相同**,按 `isMain` 的路径归并才不会重复展示。
//!
//! # 阻塞调用
//!
//! `add_worktree`(120s)/ `remove_worktree`(60s)/ `prune_worktrees`(30s)
//! 全是阻塞 IO，一律走 `mt_remote::run_git_on` 的本地/远程独立配额工作线程。
//! 创建、删除与清理先在 UI 线程按 `repository_key()` 占写操作闸；分组仍按来源上下文 `key()` 区分。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, PathPromptOptions, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use mt_config::ProjectConfig;
use mt_core::path_key::windows_eq_key;
use mt_project::git::{BranchInfo, WorktreeInfo};
use mt_remote::GitContext;

use crate::git::ui::{self as git_ui, ButtonKind};
use crate::i18n::{t, tr};
use crate::menu::{self, MenuItem};
use crate::prompt::{autofocus, kind, open_guarded, show_alert};
use crate::store::AppStore;
use crate::ui;

// ─── 纯逻辑小件 ───────────────────────────────────────────────

// 本地沿用既有 Windows 兼容路径比较;远端只按 POSIX 分隔符、区分大小写。

/// 分支名 → 目录名片段(`GitWorktreeModal.tsx:45-47`)。
pub fn sanitize_branch_for_dir(branch: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for c in branch.chars() {
        let bad =
            matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_whitespace();
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

fn separator(context: &GitContext) -> char {
    if context.is_remote() {
        '/'
    } else {
        std::path::MAIN_SEPARATOR
    }
}

fn path_key(remote: bool, path: &str) -> String {
    if remote {
        let path = path.trim_end_matches('/');
        if path.is_empty() {
            "/".into()
        } else {
            path.into()
        }
    } else {
        windows_eq_key(path)
    }
}

fn context_parent_dir<'a>(context: &GitContext, path: &'a str) -> &'a str {
    if !context.is_remote() {
        return parent_dir(path);
    }
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => &trimmed[..1],
        Some(idx) => &trimmed[..idx],
        None => trimmed,
    }
}

fn context_base_name<'a>(context: &GitContext, path: &'a str) -> &'a str {
    if context.is_remote() {
        path.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(path)
    } else {
        base_name(path)
    }
}

/// 拼路径(分隔符由调用方给)。
pub fn join_path(base: &str, child: &str, sep: char) -> String {
    let base = base.trim_end_matches(['/', '\\']);
    format!("{base}{sep}{child}")
}

fn context_join_path(context: &GitContext, base: &str, child: &str) -> String {
    if context.is_remote() {
        format!("{}/{}", base.trim_end_matches('/'), child)
    } else {
        join_path(base, child, separator(context))
    }
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

/// `path` 是 `dir` 本身或在它里面。Windows 语义比较(`windows_eq_key`:分隔符、
/// 大小写、尾斜杠不计),`D:\x\wt-2` 不算在 `D:\x\wt` 里。
fn path_within(path: &str, dir: &str) -> bool {
    let (path, dir) = (windows_eq_key(path), windows_eq_key(dir));
    !dir.is_empty()
        && (path == dir
            || path
                .strip_prefix(&dir)
                .is_some_and(|rest| rest.starts_with('/')))
}

fn backend_path_within(context: &GitContext, path: &str, dir: &str) -> bool {
    if !context.is_remote() {
        return path_within(path, dir);
    }
    if dir.is_empty() {
        return false;
    }
    let (path, dir) = (path_key(true, path), path_key(true, dir));
    path == dir
        || (dir == "/" && path.starts_with('/'))
        || path
            .strip_prefix(&dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// 停在 worktree 里的一个终端(删除前要先关掉的那些)。
#[derive(Clone, Debug, PartialEq, Eq)]
struct BusyPane {
    project_id: String,
    pane_id: String,
    /// 确认框里列出来的名字:`终端名 · 项目名`。
    label: String,
}

/// 本应用里工作目录在 `dir` 之内的终端。pane 自带 cwd 的(worktree「开终端」、仓库栏
/// 「在终端打开」开出来的那种)按它算,没有的按所在项目的根算。
///
/// `skip_project`(指向该 worktree 的项目)整个跳过:它的终端另由
/// `dispose_project_terminals` 关,确认框里也另有一行说明。只查同一 backend 的项目;
/// 只看**启动目录**:shell 里后来 `cd` 进去的占用由后端预检兜住。
fn panes_inside(
    store: &AppStore,
    context: &GitContext,
    dir: &str,
    skip_project: Option<&str>,
) -> Vec<BusyPane> {
    let mut out = Vec::new();
    for project in store.projects() {
        if project.ssh_connection_id.as_deref() != context.connection_id()
            || skip_project == Some(project.id.as_str())
        {
            continue;
        }
        let Some(state) = store.project_state(&project.id) else {
            continue;
        };
        for pane in state.all_panes() {
            let cwd = pane.cwd.as_deref().unwrap_or(&project.path);
            if backend_path_within(context, cwd, dir) {
                out.push(BusyPane {
                    project_id: project.id.clone(),
                    pane_id: pane.id.clone(),
                    label: format!(
                        "{} · {}",
                        store.pane_display_label(&project.id, pane),
                        project.name
                    ),
                });
            }
        }
    }
    out
}

/// 删除失败的界面文案:预检拦下的占用、删到一半的残留各给原因与下一步,
/// 其余是 git 的原文([`remove_error_text`])。
fn remove_failure_text(err: &anyhow::Error) -> String {
    match err.downcast_ref::<mt_project::git::WorktreeRemoveError>() {
        Some(mt_project::git::WorktreeRemoveError::InUse { busy, .. }) => {
            tr!("worktree", "removeInUse", busy = busy)
        }
        Some(mt_project::git::WorktreeRemoveError::Leftover { path, detail }) => {
            tr!("worktree", "removeLeftover", detail = detail, path = path)
        }
        None => remove_error_text(err),
    }
}

/// 「检出现有分支」的可选项(`GitWorktreeModal.tsx:243-258`)。
///
/// 逐组算「本地分支 − 该组已被任一 worktree 占用的分支」,再取**全组交集**,
/// 顺序按第一个组的列表。
pub fn available_branches(groups: &[(&[BranchInfo], Vec<Option<&str>>)]) -> Vec<String> {
    let per_group: Vec<Vec<&str>> = groups
        .iter()
        .map(|(branches, occupied)| {
            let occupied: HashSet<&str> = occupied.iter().flatten().copied().collect();
            branches
                .iter()
                .filter(|b| !b.is_remote)
                .map(|b| b.name.as_str())
                .filter(|name| !occupied.contains(name))
                .collect()
        })
        .collect();
    intersect_ordered(&per_group)
}

/// 「新分支起点」的可选项(`:261-268`):逐组的**全部**分支名(含远程)取交集。
pub fn base_branch_options(groups: &[&[BranchInfo]]) -> Vec<String> {
    let per_group: Vec<Vec<&str>> = groups
        .iter()
        .map(|branches| branches.iter().map(|b| b.name.as_str()).collect())
        .collect();
    intersect_ordered(&per_group)
}

/// 取交集,顺序按第一组。其余各组先建成集合再查(几个仓库各两千条分支时,逐条
/// `Vec::contains` 是千万次比较)。
fn intersect_ordered(groups: &[Vec<&str>]) -> Vec<String> {
    let Some(first) = groups.first() else {
        return Vec::new();
    };
    let rest: Vec<HashSet<&str>> = groups[1..]
        .iter()
        .map(|g| g.iter().copied().collect())
        .collect();
    first
        .iter()
        .filter(|name| rest.iter().all(|g| g.contains(*name)))
        .map(|name| name.to_string())
        .collect()
}

// ─── 分组 ─────────────────────────────────────────────────────

/// 归并后的一组(`GitWorktreeModal.tsx:31-42`)。
#[derive(Clone)]
struct RepoGroup {
    /// backend 身份与主仓库路径。
    key: String,
    /// 主仓库目录名(worktree 目录建议名的前缀)。
    name: String,
    /// git 命令的执行路径:worktree 增删必须落在主仓库上。
    main_path: String,
    context: GitContext,
    worktrees: Vec<WorktreeInfo>,
    /// 主仓库根下是否已存在 `.worktrees` 目录（后台探测，仅用于智能默认路径习惯）。
    has_dot_worktrees: bool,
    error: Option<String>,
}

/// 单个仓库新建 worktree 的目标目录模板。
#[derive(Debug, Clone, PartialEq, Eq)]
struct PathTemplate {
    /// 目标父目录（例如 `D:\Git\mini-term\.worktrees` 或 `D:\Git`）。
    parent_dir: String,
    /// 目录名前是否加 `<仓库名>-` 前缀：
    /// 放在仓库内部子目录（如 `.worktrees/`）时不重复仓库名，直接 `<分支>`；
    /// 放在仓库外部（同级目录或外部目录）时用 `<仓库名>-<分支>` 避免撞名。
    prefix_repo_name: bool,
}

impl PathTemplate {
    fn format_dir(&self, context: &GitContext, repo_name: &str, leaf: &str) -> String {
        let child = if self.prefix_repo_name {
            format!("{repo_name}-{leaf}")
        } else {
            leaf.to_string()
        };
        context_join_path(context, &self.parent_dir, &child)
    }
}

/// 按仓库已有的 worktree 存放习惯推断新 worktree 的默认目录模板：
/// 1. 若已有有效 linked worktree 落在主仓库内部子目录（如 `<main>/.worktrees/<x>`），
///    或主仓库根下已存在 `.worktrees/` 目录 → 优先沿用该内部子目录（`.worktrees` 优先），不加仓库名前缀；
/// 2. 否则若已有有效 linked worktree 落在仓库外部目录 → 沿用出现最多的那个外部父目录，加 `<仓库名>-` 前缀；
/// 3. 都没有 → 回落经典同级目录 `<parent(main)>/<仓库名>-<分支>`。
fn infer_worktree_path_template(group: &RepoGroup) -> Option<PathTemplate> {
    let remote = group.context.is_remote();
    let main_key = path_key(remote, &group.main_path);

    let mut inside_counts: Vec<(String, String, usize)> = Vec::new();
    let mut outside_counts: Vec<(String, String, usize)> = Vec::new();

    for wt in &group.worktrees {
        if wt.is_main || !wt.is_valid {
            continue;
        }
        let parent = context_parent_dir(&group.context, &wt.path);
        if parent.is_empty() {
            continue;
        }
        let pkey = path_key(remote, parent);
        if pkey == main_key {
            continue;
        }
        let bucket = if backend_path_within(&group.context, parent, &group.main_path) {
            &mut inside_counts
        } else {
            &mut outside_counts
        };
        if let Some(entry) = bucket.iter_mut().find(|(k, _, _)| *k == pkey) {
            entry.2 += 1;
        } else {
            bucket.push((pkey, parent.to_string(), 1));
        }
    }

    if !inside_counts.is_empty() {
        let dot_wt_key = path_key(
            remote,
            &context_join_path(&group.context, &group.main_path, ".worktrees"),
        );
        if let Some((_, dir, _)) = inside_counts.iter().find(|(k, _, _)| *k == dot_wt_key) {
            return Some(PathTemplate {
                parent_dir: dir.clone(),
                prefix_repo_name: false,
            });
        }
        if let Some((_, dir, _)) = inside_counts.iter().max_by_key(|(_, _, count)| *count) {
            return Some(PathTemplate {
                parent_dir: dir.clone(),
                prefix_repo_name: false,
            });
        }
    }

    if group.has_dot_worktrees {
        return Some(PathTemplate {
            parent_dir: context_join_path(&group.context, &group.main_path, ".worktrees"),
            prefix_repo_name: false,
        });
    }

    if let Some((_, dir, _)) = outside_counts.iter().max_by_key(|(_, _, count)| *count) {
        return Some(PathTemplate {
            parent_dir: dir.clone(),
            prefix_repo_name: true,
        });
    }

    let parent = context_parent_dir(&group.context, &group.main_path);
    if parent.is_empty() {
        return None;
    }
    Some(PathTemplate {
        parent_dir: parent.to_string(),
        prefix_repo_name: true,
    })
}

/// 把逐仓库的 `list_worktrees` 结果归并成组。
fn merge_groups(
    root: &GitContext,
    items: Vec<(String, Result<Vec<WorktreeInfo>, String>)>,
) -> Vec<RepoGroup> {
    let mut out: Vec<RepoGroup> = Vec::new();
    for (path, result) in items {
        match result {
            Ok(worktrees) => {
                let main_path = worktrees
                    .iter()
                    .find(|w| w.is_main)
                    .map(|w| w.path.clone())
                    .unwrap_or_else(|| path.clone());
                let context = root.with_path(main_path.clone());
                let key = context.key();
                if out.iter().any(|g| g.key == key) {
                    continue;
                }
                out.push(RepoGroup {
                    key,
                    name: context_base_name(&context, &main_path).to_string(),
                    main_path,
                    context,
                    worktrees,
                    has_dot_worktrees: false,
                    error: None,
                });
            }
            Err(err) => {
                let context = root.with_path(path.clone());
                let key = context.key();
                if out.iter().any(|g| g.key == key) {
                    continue;
                }
                out.push(RepoGroup {
                    key,
                    name: context_base_name(&context, &path).to_string(),
                    main_path: path,
                    context,
                    worktrees: Vec::new(),
                    has_dot_worktrees: false,
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
    context: GitContext,
    discover_repos: bool,
    project_id: Option<String>,
    /// `None` = 还在加载。共享一份:Dialog 的 builder 每帧重跑,按钮闭包也要拿着它,
    /// 不每帧深拷贝。
    groups: Option<Rc<Vec<RepoGroup>>>,
    load_error: Option<String>,
    selected_keys: Vec<String>,
    /// 各组的分支(按组键),同样共享。
    branches_by_repo: HashMap<String, Rc<Vec<BranchInfo>>>,
    /// 正在异步拉取分支的组键（避免拉取未返回前把「加载中」误判成「没有可用分支」）。
    branches_loading: HashSet<String>,
    /// 两个下拉的选项(见 [`OptionsCache`])。
    options_cache: RefCell<Option<OptionsCache>>,
    /// 「新建 worktree」区域是否展开（默认收起，优先清爽浏览与管理已有 worktree）。
    create_open: bool,
    /// 「新建 worktree」展开 / 收起高度裁剪动画。
    create_reveal: crate::height_reveal::HeightReveal,
    /// 「新建 worktree」标题栏折叠箭头补间。
    create_chevron: crate::fold_motion::FlipTweens,
    /// 用户是否手动切换过「检出现有分支 / 新建分支」；未手切且可用现有分支为空时自动切到「新建分支」。
    mode_touched: bool,
    /// 本次展开「新建 worktree」后是否已为新分支输入框自动聚焦过。
    auto_focused_on_open: bool,
    mode: Mode,
    /// 「已有分支 / 新分支」段控件的滑动选中块(见 `segment_slider`)。
    mode_slider: crate::segment_slider::IndicatorSlider,
    sel_branch: String,
    base_branch: String,
    new_branch: Entity<InputState>,
    wt_path: Entity<InputState>,
    /// 用户手改过路径之后就不再跟随建议。
    path_edited: bool,
    /// 上一次自动写入 `wt_path` 的建议路径（用于在 `InputEvent::Change` 里无竞态区分自动填入与手动编辑）。
    last_suggested_path: String,
    /// 上一次写入 `wt_path` 的占位预览文案（值一致不重复 `set_placeholder`）。
    last_path_placeholder: String,
    switch_after_create: bool,
    creating: bool,
    create_error: Option<String>,
    /// 逐仓库的创建错误(部分失败时留在弹窗里)。
    create_results: Vec<(String, String)>,
    pruning_key: Option<String>,
    on_changed: Rc<dyn Fn(&mut App)>,
    _subs: Vec<Subscription>,
}

/// 「检出现有分支」/「新分支起点」下拉的选项缓存。
///
/// 选项是各勾选组分支列表的交集(几个仓库各两千条分支时是千万次比较的活),而弹窗开着
/// 时 builder 每帧都要画 —— 只在模式、勾选、分组或分支列表换了时才重算。认法是比指针:
/// 分组与各组分支都是共享的 `Rc`,数据一换就是新的一份;缓存自己也握着旧的那几份,
/// 它们不会被释放后又在同一个地址冒出来,所以不会认错。
struct OptionsCache {
    mode: Mode,
    groups: Rc<Vec<RepoGroup>>,
    /// 勾选的组(分组里的先后)与当时各组的分支。
    selected: Vec<(String, Option<Rc<Vec<BranchInfo>>>)>,
    options: Rc<Vec<String>>,
}

impl OptionsCache {
    fn matches(
        &self,
        mode: Mode,
        groups: &Rc<Vec<RepoGroup>>,
        selected: &[(String, Option<Rc<Vec<BranchInfo>>>)],
    ) -> bool {
        self.mode == mode
            && Rc::ptr_eq(&self.groups, groups)
            && self.selected.len() == selected.len()
            && self
                .selected
                .iter()
                .zip(selected)
                .all(|((ka, a), (kb, b))| {
                    ka == kb
                        && match (a, b) {
                            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
                            (None, None) => true,
                            _ => false,
                        }
                })
    }
}

/// 下拉的选项:「检出现有分支」是 [`available_branches`],「新分支起点」是 HEAD +
/// [`base_branch_options`]。`selected` 是勾选的组(分组里的先后)。
fn dropdown_options(
    mode: Mode,
    selected: &[&RepoGroup],
    branches_by_repo: &HashMap<String, Rc<Vec<BranchInfo>>>,
) -> Vec<String> {
    let branches_of = |g: &RepoGroup| -> &[BranchInfo] {
        branches_by_repo
            .get(&g.key)
            .map(|b| b.as_slice())
            .unwrap_or_default()
    };
    match mode {
        Mode::Existing => {
            let per_group: Vec<(&[BranchInfo], Vec<Option<&str>>)> = selected
                .iter()
                .map(|g| {
                    (
                        branches_of(g),
                        g.worktrees.iter().map(|w| w.branch.as_deref()).collect(),
                    )
                })
                .collect();
            available_branches(&per_group)
        }
        Mode::New => {
            let per_group: Vec<&[BranchInfo]> = selected.iter().map(|g| branches_of(g)).collect();
            let mut options = vec![t("worktree", "baseHead").to_string()];
            options.extend(base_branch_options(&per_group));
            options
        }
    }
}

impl WorktreeModal {
    /// 眼下的下拉选项(缓存着,见 [`OptionsCache`])。
    fn options(&self, groups: &Rc<Vec<RepoGroup>>, selected: &[&RepoGroup]) -> Rc<Vec<String>> {
        let key: Vec<(String, Option<Rc<Vec<BranchInfo>>>)> = selected
            .iter()
            .map(|g| (g.key.clone(), self.branches_by_repo.get(&g.key).cloned()))
            .collect();
        if let Some(cache) = self
            .options_cache
            .borrow()
            .as_ref()
            .filter(|c| c.matches(self.mode, groups, &key))
        {
            return cache.options.clone();
        }
        let options = Rc::new(dropdown_options(
            self.mode,
            selected,
            &self.branches_by_repo,
        ));
        *self.options_cache.borrow_mut() = Some(OptionsCache {
            mode: self.mode,
            groups: groups.clone(),
            selected: key,
            options: options.clone(),
        });
        options
    }
}

impl Render for WorktreeModal {
    /// 状态盒子。真正的画面由 Dialog 的 builder 每帧重建(见 `modal.rs` 的说明)。
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 打开 worktree 管理弹窗。
pub fn open(
    context: GitContext,
    discover_repos: bool,
    project_id: Option<String>,
    on_changed: impl Fn(&mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let store = AppStore::global(cx);
    let source_project_id = context
        .project_id()
        .map(str::to_string)
        .or_else(|| project_id.clone())
        .or_else(|| store.read(cx).active_project_id.clone());
    let remote_project = source_project_id
        .as_deref()
        .and_then(|id| store.read(cx).project(id))
        .is_some_and(crate::ssh_conn::is_remote_project);
    if context.is_remote() || remote_project {
        let Some(source_id) = source_project_id.as_deref() else {
            return;
        };
        if !crate::git::backend::remote_git_enabled(store.read(cx), source_id) {
            crate::git::backend::request_enable(store, source_id.to_string(), window, cx);
            return;
        }
    }
    if context.path().is_empty() {
        return;
    }
    let repo_path = context.path().to_string();
    let project_id = source_project_id.filter(|id| {
        store
            .read(cx)
            .project(id)
            .is_some_and(|project| project.ssh_connection_id.as_deref() == context.connection_id())
    });
    let root_name = context_base_name(&context, &repo_path).to_string();
    let state = cx.new(|cx| {
        let new_branch = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t("worktree", "newBranchPlaceholder"))
        });
        let wt_path = cx
            .new(|cx| InputState::new(window, cx).placeholder(t("worktree", "pathPlaceholder")));
        let sub_branch = cx.subscribe_in(
            &new_branch,
            window,
            |this: &mut WorktreeModal, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.create_error = None;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    let Some(groups) = this.groups.clone() else {
                        return;
                    };
                    let entity = cx.entity();
                    window.defer(cx, move |window, cx| {
                        create(&entity, &groups, window, cx);
                    });
                }
                _ => {}
            },
        );
        let sub_path = cx.subscribe_in(
            &wt_path,
            window,
            |this: &mut WorktreeModal, input, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    let cur = input.read(cx).value().to_string();
                    if cur != this.last_suggested_path {
                        this.path_edited = true;
                    }
                    this.create_error = None;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    let Some(groups) = this.groups.clone() else {
                        return;
                    };
                    let entity = cx.entity();
                    window.defer(cx, move |window, cx| {
                        create(&entity, &groups, window, cx);
                    });
                }
                _ => {}
            },
        );
        WorktreeModal {
            store,
            repo_path: repo_path.clone(),
            context,
            discover_repos,
            project_id,
            groups: None,
            load_error: None,
            selected_keys: Vec::new(),
            branches_by_repo: HashMap::new(),
            branches_loading: HashSet::new(),
            options_cache: RefCell::new(None),
            create_open: false,
            create_reveal: crate::height_reveal::HeightReveal::new(false),
            create_chevron: crate::fold_motion::FlipTweens::new(mt_ui::motion::CHEVRON_ROTATE),
            mode_touched: false,
            auto_focused_on_open: false,
            mode: Mode::Existing,
            mode_slider: crate::segment_slider::IndicatorSlider::new(),
            sel_branch: String::new(),
            base_branch: String::new(),
            new_branch,
            wt_path,
            path_edited: false,
            last_suggested_path: String::new(),
            last_path_placeholder: t("worktree", "pathPlaceholder").to_string(),
            // 默认勾选：创建完自动切到新项目（worktree 本身无论是否勾选都会自动添加为项目）
            switch_after_create: true,
            creating: false,
            create_error: None,
            create_results: Vec::new(),
            pruning_key: None,
            on_changed: Rc::new(on_changed),
            _subs: vec![sub_branch, sub_path],
        }
    });

    load(&state, cx);

    open_guarded(kind::GIT_WORKTREE, window, cx, move |dialog, window, cx| {
        let body = render_body(&state, window, cx);
        dialog
            // 无底部按钮,右上角关闭是唯一看得见的出口(见 `git_ui::dialog_title`)
            .title(git_ui::dialog_title(
                kind::GIT_WORKTREE,
                tr!("worktree", "title", name = root_name.clone()),
            ))
            .w(px(600.0))
            .child(body)
    });
}

/// 将弹窗加载到的有效 linked worktree 自动同步为项目（覆盖远程 SSH 仓库与多仓库父目录项目；
/// 已存在或被用户手动移除忽略的路径跳过）。
fn sync_loaded_worktrees_to_projects(
    store: &Entity<AppStore>,
    groups: &[RepoGroup],
    parent_hint: Option<&str>,
    cx: &mut App,
) {
    let mut to_add: Vec<(String, String, Option<String>, Option<String>)> = Vec::new();
    {
        let store_ref = store.read(cx);
        for group in groups {
            if group.error.is_some() {
                continue;
            }
            let conn_id = group.context.connection_id();
            let parent = store_ref
                .find_project_by_backend_path(conn_id, &group.main_path)
                .map(|p| p.id.clone())
                .or_else(|| parent_hint.map(str::to_string));
            for wt in &group.worktrees {
                if wt.is_main || !wt.is_valid {
                    continue;
                }
                if store_ref
                    .find_project_by_backend_path(conn_id, &wt.path)
                    .is_some()
                    || store_ref.is_worktree_dismissed(conn_id, &wt.path)
                {
                    continue;
                }
                let name = context_base_name(&group.context, &wt.path).to_string();
                to_add.push((
                    name,
                    wt.path.clone(),
                    conn_id.map(str::to_string),
                    parent.clone(),
                ));
            }
        }
    }
    if to_add.is_empty() {
        return;
    }
    store.update(cx, |store, cx| {
        for (name, path, conn_id, parent) in to_add {
            store.add_git_worktree_project(name, path, conn_id, parent, cx);
        }
    });
}

/// 加载分组。
fn load(state: &Entity<WorktreeModal>, cx: &mut App) {
    let (context, discover) = {
        let s = state.read(cx);
        (s.context.clone(), s.discover_repos)
    };
    state.update(cx, |s, cx| {
        s.groups = None;
        s.load_error = None;
        cx.notify();
    });
    let state = state.clone();
    cx.spawn(async move |cx| {
        let repo_for_task = context.clone();
        let loaded = mt_remote::run_git_on(&context, move || {
            let paths: Vec<String> = if discover {
                match repo_for_task.discover_git_repos() {
                    Ok(repos) => repos
                        .into_iter()
                        .map(|r| r.path.to_string_lossy().to_string())
                        .collect(),
                    Err(err) => return Err(git_ui::error_text(&err)),
                }
            } else {
                vec![repo_for_task.path().to_string()]
            };
            let is_remote = repo_for_task.is_remote();
            let mut dot_worktrees_keys: HashSet<String> = HashSet::new();
            let items = paths
                .into_iter()
                .map(|path| {
                    let result = repo_for_task
                        .with_path(path.clone())
                        .list_worktrees()
                        .map_err(|err| git_ui::error_text(&err));
                    if !is_remote
                        && let Ok(ref list) = result
                    {
                        let main_path = list
                            .iter()
                            .find(|w| w.is_main)
                            .map(|w| w.path.as_str())
                            .unwrap_or(path.as_str());
                        if std::path::Path::new(main_path).join(".worktrees").is_dir() {
                            dot_worktrees_keys.insert(path_key(false, main_path));
                        }
                    }
                    (path, result)
                })
                .collect::<Vec<_>>();
            Ok((items, dot_worktrees_keys))
        })
        .await;

        let synced = state.update(cx, |s: &mut WorktreeModal, cx| match loaded {
            Err(err) => {
                s.groups = Some(Rc::default());
                s.load_error = Some(err);
                cx.notify();
                None
            }
            Ok((items, dot_worktrees_keys)) => {
                // 单仓库时沿用旧行为:加载失败即整体报错,不显示空壳分组
                if !s.discover_repos
                    && items.len() == 1
                    && let Some((_, Err(err))) = items.first()
                {
                    s.groups = Some(Rc::default());
                    s.load_error = Some(err.clone());
                    cx.notify();
                    return None;
                }
                let mut groups = merge_groups(&context, items);
                for g in &mut groups {
                    if !g.context.is_remote()
                        && dot_worktrees_keys.contains(&path_key(false, &g.main_path))
                    {
                        g.has_dot_worktrees = true;
                    }
                }
                let groups = Rc::new(groups);
                // 勾选保留:剔除消失的键;只剩一个可用仓库时自动勾上
                s.selected_keys
                    .retain(|k| groups.iter().any(|g| &g.key == k));
                if s.selected_keys.is_empty() && groups.len() == 1 && groups[0].error.is_none() {
                    s.selected_keys = vec![groups[0].key.clone()];
                }
                s.groups = Some(groups.clone());
                cx.notify();
                Some((s.store.clone(), groups, s.project_id.clone()))
            }
        });
        if let Some((store, groups, parent_hint)) = synced {
            cx.update(|cx| {
                sync_loaded_worktrees_to_projects(&store, &groups, parent_hint.as_deref(), cx);
            });
        }
    })
    .detach();
}

/// 惰性拉勾选组的分支(失败也落一条空记录,否则会反复重试)。
fn ensure_branches(state: &Entity<WorktreeModal>, cx: &mut App) {
    let pending: Vec<(String, GitContext)> = {
        let s = state.read(cx);
        let Some(groups) = &s.groups else {
            return;
        };
        groups
            .iter()
            .filter(|g| s.selected_keys.contains(&g.key))
            .filter(|g| {
                !s.branches_by_repo.contains_key(&g.key) && !s.branches_loading.contains(&g.key)
            })
            .map(|g| (g.key.clone(), g.context.clone()))
            .collect()
    };
    for (key, context) in pending {
        let state = state.clone();
        // 先登记 loading，避免同一帧内重复排队，同时不污染 branches_by_repo
        state.update(cx, |s, _| {
            s.branches_loading.insert(key.clone());
        });
        cx.spawn(async move |cx| {
            let execution = context.clone();
            let result =
                mt_remote::run_git_on(&execution, move || context.get_repo_branches()).await;
            state.update(cx, |s: &mut WorktreeModal, cx| {
                s.branches_loading.remove(&key);
                match result {
                    Ok(list) => {
                        s.branches_by_repo.insert(key, Rc::new(list));
                    }
                    Err(err) => {
                        tracing::warn!("[git] 取分支失败: {err:#}");
                        s.branches_by_repo.insert(key, Rc::default());
                        s.create_error = Some(git_ui::error_text(&err));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

// ─── 渲染 ─────────────────────────────────────────────────────

/// 居中的弱化说明(「请先勾选仓库」「没有可用分支」这类)。
fn hint_line(text: impl Into<SharedString>) -> AnyElement {
    git_ui::hint_line(text)
        .w_full()
        .py(px(8.0))
        .text_center()
        .into_any_element()
}

fn render_body(state: &Entity<WorktreeModal>, window: &mut Window, cx: &mut App) -> AnyElement {
    ensure_branches(state, cx);
    state.read(cx).create_chevron.drive(window);
    let s = state.read(cx);
    let mut root = div().flex().flex_col().gap(px(8.0));

    // 共享的那一份(加引用计数,不深拷贝:下面每一行都要拿 `&mut App` 渲染,不能一直借着 `s`)
    let Some(groups) = s.groups.clone() else {
        return root
            .child(git_ui::loading_state(t("worktree", "loading")))
            .into_any_element();
    };
    if let Some(err) = &s.load_error {
        root = root.child(git_ui::error_block(err.clone()));
    }
    if groups.is_empty() && s.load_error.is_none() {
        return root
            .child(git_ui::empty_state(
                git_ui::ICON_REPO,
                t("worktree", "noRepoFound"),
                None,
            ))
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
                    git_ui::action_button(
                        "worktree-select-all",
                        if all {
                            t("worktree", "clearAll")
                        } else {
                            t("worktree", "selectAll")
                        },
                        ButtonKind::Ghost,
                        true,
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

    // 路径 → 项目 id + 当前激活项目 id，用于整行点击切换与「当前」高亮
    let (projects, active_project_id) = {
        let s = state.read(cx);
        let store = s.store.read(cx);
        (
            backend_project_ids(store.projects(), &s.context),
            store.active_project_id.clone(),
        )
    };
    let mut list = div()
        .id("worktree-list-scroll")
        .flex()
        .flex_col()
        .gap(px(8.0))
        .max_h(px(300.0))
        .overflow_y_scroll();
    for index in 0..groups.len() {
        list = list.child(render_group(
            state,
            &groups,
            index,
            multi_repo,
            &projects,
            active_project_id.as_deref(),
            cx,
        ));
    }
    root = root.child(list);

    // 展开新建区且分支列表就绪后：没有可检出的现有分支时自动切到「新建分支」并聚焦分支输入框
    sync_create_mode_and_focus(state, &groups, window, cx);

    // 默认路径建议 + 占位预览（值 / 占位未变化时直接 return，不会自激）
    sync_path_suggestion(state, &groups, window, cx);

    root = root.child(render_create_section(state, &groups, multi_repo, window, cx));
    root.into_any_element()
}

/// 创建按钮启用条件（按钮禁用态与 `create()` 入口共用同一判据）。
fn can_create_worktree(creating: bool, has_selected: bool, branch: &str, path: &str) -> bool {
    !creating && has_selected && !branch.trim().is_empty() && !path.trim().is_empty()
}

/// 展开「新建 worktree」区后，若用户未手动切过模式且可用现有分支为空，
/// 自动切到「新建分支」；停在「新建分支」且本次展开尚未聚焦过时自动聚焦分支输入框。
fn sync_create_mode_and_focus(
    state: &Entity<WorktreeModal>,
    groups: &Rc<Vec<RepoGroup>>,
    window: &mut Window,
    cx: &mut App,
) {
    let (should_switch_new, focus_target) = {
        let s = state.read(cx);
        if !s.create_open {
            return;
        }
        let selected: Vec<&RepoGroup> = groups
            .iter()
            .filter(|g| s.selected_keys.contains(&g.key))
            .collect();
        if selected.is_empty() {
            return;
        }
        let branches_ready = selected
            .iter()
            .all(|g| s.branches_by_repo.contains_key(&g.key));
        if !branches_ready {
            return;
        }
        let switch_new = !s.mode_touched
            && s.mode == Mode::Existing
            && s.options(groups, &selected).is_empty();
        let next_mode = if switch_new { Mode::New } else { s.mode };
        let focus = (next_mode == Mode::New && !s.auto_focused_on_open)
            .then(|| s.new_branch.clone());
        (switch_new, focus)
    };
    if should_switch_new || focus_target.is_some() {
        state.update(cx, |s, cx| {
            if should_switch_new {
                s.mode = Mode::New;
            }
            if focus_target.is_some() {
                s.auto_focused_on_open = true;
            }
            cx.notify();
        });
    }
    if let Some(input) = focus_target {
        autofocus(&input, window, cx);
    }
}

/// 计算路径输入框的「自动建议值」与「灰色占位预览」：
/// - 未勾选 → 建议清空，占位为通用路径提示；
/// - 单选 → 按 [`infer_worktree_path_template`] 推断习惯目录；分支为空时不填假路径，
///   仅在 placeholder 预览 `<模板>/<分支名>`，填入分支后再写入真实建议路径；
/// - 多选 → 建议 `repo_path`（语义为父目录），占位为父目录提示。
fn compute_path_suggestion(
    selected: &[&RepoGroup],
    branch: &str,
    repo_path: &str,
) -> (Option<String>, String) {
    match selected.len() {
        0 => (
            Some(String::new()),
            t("worktree", "pathPlaceholder").to_string(),
        ),
        1 => {
            let g = selected[0];
            let Some(tpl) = infer_worktree_path_template(g) else {
                return (None, t("worktree", "pathPlaceholder").to_string());
            };
            let placeholder =
                tpl.format_dir(&g.context, &g.name, t("worktree", "branchDirPlaceholder"));
            let suggested = if branch.trim().is_empty() {
                Some(String::new())
            } else {
                Some(tpl.format_dir(&g.context, &g.name, &sanitize_branch_for_dir(branch)))
            };
            (suggested, placeholder)
        }
        _ => (
            Some(repo_path.to_string()),
            t("worktree", "parentPathPlaceholder").to_string(),
        ),
    }
}

/// 同步路径输入框的智能默认建议与占位预览。
/// `path_edited` 为真（用户手改过或用过「浏览…」）之后不再覆盖输入值，但仍保持占位预览准确。
fn sync_path_suggestion(
    state: &Entity<WorktreeModal>,
    groups: &[RepoGroup],
    window: &mut Window,
    cx: &mut App,
) {
    let (want, placeholder, path_edited, placeholder_changed, input) = {
        let s = state.read(cx);
        let selected: Vec<&RepoGroup> = groups
            .iter()
            .filter(|g| s.selected_keys.contains(&g.key))
            .collect();
        let branch = match s.mode {
            Mode::Existing => s.sel_branch.clone(),
            Mode::New => s.new_branch.read(cx).value().trim().to_string(),
        };
        let (want, placeholder) = compute_path_suggestion(&selected, &branch, &s.repo_path);
        let placeholder_changed = placeholder != s.last_path_placeholder;
        (
            want,
            placeholder,
            s.path_edited,
            placeholder_changed,
            s.wt_path.clone(),
        )
    };
    if placeholder_changed {
        let next_placeholder = placeholder.clone();
        state.update(cx, |s, _| {
            s.last_path_placeholder = next_placeholder;
        });
        input.update(cx, |st, cx| st.set_placeholder(placeholder, window, cx));
    }
    if path_edited {
        return;
    }
    let Some(want) = want else {
        return;
    };
    if input.read(cx).value() == want.as_str() {
        return;
    }
    let suggested = want.clone();
    state.update(cx, |s, _| {
        s.last_suggested_path = suggested;
    });
    input.update(cx, |st, cx| st.set_value(want, window, cx));
}

/// 勾选变化时要清掉的表单状态(`toggleRepo` / `toggleAll`)。
fn reset_form_on_selection(s: &mut WorktreeModal) {
    s.path_edited = false;
    s.last_suggested_path.clear();
    s.mode_touched = false;
    s.auto_focused_on_open = false;
    s.sel_branch.clear();
    s.base_branch.clear();
    s.create_error = None;
    s.create_results.clear();
}

/// 同 backend 的项目路径 → 项目 id,同一路径取最先的那个。
fn backend_project_ids(
    projects: &[ProjectConfig],
    context: &GitContext,
) -> HashMap<String, String> {
    let mut ids = HashMap::new();
    for project in projects
        .iter()
        .filter(|p| p.ssh_connection_id.as_deref() == context.connection_id())
    {
        ids.entry(path_key(context.is_remote(), &project.path))
            .or_insert_with(|| project.id.clone());
    }
    ids
}

/// 第 `index` 组。`groups` 是共享的那一份:按钮闭包拿着它与下标,点的时候再取,
/// 不每帧把整组(连同全部 worktree)深拷贝进闭包。
fn render_group(
    state: &Entity<WorktreeModal>,
    groups: &Rc<Vec<RepoGroup>>,
    index: usize,
    multi_repo: bool,
    projects: &HashMap<String, String>,
    active_project_id: Option<&str>,
    cx: &mut App,
) -> AnyElement {
    let group = &groups[index];
    // 先把要用的字段拷出来:下面每一行都要拿 `&mut App` 去渲染,不能一直借着 `s`
    let (selected, pruning) = {
        let s = state.read(cx);
        (
            s.selected_keys.contains(&group.key),
            s.pruning_key.as_deref() == Some(group.key.as_str()),
        )
    };
    let mut block = div().flex().flex_col().when(multi_repo, |el| {
        el.rounded(px(6.0))
            .overflow_hidden()
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
                .gap(px(8.0))
                .h(px(git_ui::ROW_H + 4.0))
                .px(px(8.0))
                .cursor_pointer()
                .when(selected, |el| el.bg(ui::accent_subtle()))
                .when(!selected, |el| el.hover(|el| el.bg(git_ui::hover_bg())))
                .child(git_ui::check_box(selected, ui::accent()))
                .child(git_ui::icon(
                    git_ui::ICON_REPO,
                    git_ui::ICON_SIZE,
                    ui::color_folder(),
                ))
                .child(
                    div()
                        .flex_none()
                        .text_size(ui::font_px(13.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(ui::text_primary())
                        .child(group.name.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_muted())
                        .text_right()
                        .truncate()
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
                    .child(git_ui::notice_line(err.clone(), ui::color_error())),
            )
            .into_any_element();
    }

    let mut rows = div().flex().flex_col().gap(px(2.0)).p(px(4.0));
    for idx in 0..group.worktrees.len() {
        rows = rows.child(render_worktree_row(
            state,
            groups,
            index,
            idx,
            projects,
            active_project_id,
            cx,
        ));
    }
    block = block.child(rows);

    if group.worktrees.iter().any(|w| !w.is_valid) {
        let (state_for_prune, groups_for_prune) = (state.clone(), groups.clone());
        block = block.child(
            div().flex().justify_end().px(px(8.0)).pb(px(8.0)).child(
                git_ui::action_button(
                    SharedString::from(format!("worktree-prune-{}", group.key)),
                    if pruning {
                        t("worktree", "pruning")
                    } else {
                        t("worktree", "prune")
                    },
                    ButtonKind::Ghost,
                    !pruning,
                )
                .when(!pruning, |el| {
                    el.on_click(move |_: &ClickEvent, _window, cx| {
                        prune(&state_for_prune, &groups_for_prune[index], cx);
                    })
                }),
            ),
        );
    }

    block.into_any_element()
}

/// 第 `group_index` 组的第 `idx` 个 worktree。项目只匹配同 backend。
/// 整行点击直接切换到对应项目（当前项目常驻高亮不可点）；右侧仅保留紧凑图标按钮。
fn render_worktree_row(
    state: &Entity<WorktreeModal>,
    groups: &Rc<Vec<RepoGroup>>,
    group_index: usize,
    idx: usize,
    projects: &HashMap<String, String>,
    active_project_id: Option<&str>,
    cx: &mut App,
) -> AnyElement {
    let s = state.read(cx);
    let group = &groups[group_index];
    let wt = &group.worktrees[idx];
    let existing_project = projects
        .get(&path_key(group.context.is_remote(), &wt.path))
        .cloned();
    let is_current = existing_project
        .as_deref()
        .is_some_and(|id| active_project_id == Some(id));
    let switch_target = (wt.is_valid && !is_current)
        .then_some(existing_project)
        .flatten();
    let store_for_switch = s.store.clone();

    // 胶囊全走共用的语义胶囊:当前 = accent、主仓库 = 中性、分支 = 本地分支色(带分支图标)、
    // 失效 = 错误、锁定 = 警告（worktree 自动同步为项目，不再展示「已是项目」标记）
    let mut badges = div().flex().items_center().gap(px(4.0)).min_w_0();
    if is_current {
        badges = badges.child(git_ui::pill_on(
            t("worktree", "current"),
            ui::accent(),
            git_ui::ground_with(ui::accent_subtle()),
        ));
    }
    if wt.is_main {
        badges = badges.child(git_ui::pill(t("worktree", "mainRepo"), ui::text_muted()));
    }
    if let Some(branch) = &wt.branch {
        badges = badges.child(git_ui::icon_pill(
            git_ui::ICON_BRANCH,
            branch.clone(),
            git_ui::RefKind::Local.color(),
        ));
    }
    if !wt.is_valid {
        badges = badges.child(git_ui::pill(t("worktree", "invalid"), ui::color_error()));
    }
    if wt.is_locked {
        badges = badges.child(git_ui::pill(t("worktree", "locked"), ui::color_warning()));
    }

    // 右侧紧凑图标按钮（均内置 stop_propagation，不会冒泡误触整行切换）
    let mut actions = div().flex().items_center().gap(px(2.0)).flex_none();
    if wt.is_valid {
        let can_open_terminal = s.project_id.is_some();
        let (store, path, name, branch) = (
            s.store.clone(),
            wt.path.clone(),
            wt.name.clone(),
            wt.branch.clone(),
        );
        let project_id = s.project_id.clone();
        let terminal_context = group.context.clone();
        actions = actions.child(
            git_ui::icon_button(
                SharedString::from(format!("wt-open-{}-{idx}", group.key)),
                git_ui::ICON_TERMINAL,
                t("worktree", "openTerminal"),
                can_open_terminal,
            )
            .when(can_open_terminal, |el| {
                el.on_click(move |_: &ClickEvent, window, cx| {
                    let Some(project_id) = project_id.clone() else {
                        return;
                    };
                    let title = format!("⎇ {}", branch.clone().unwrap_or_else(|| name.clone()));
                    let path = path.clone();
                    let opened = store.update(cx, |store, cx| {
                        if !store.project(&project_id).is_some_and(|project| {
                            project.ssh_connection_id.as_deref() == terminal_context.connection_id()
                        }) {
                            return false;
                        }
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
                })
            }),
        );

        // 「删除」(非 main 才有)
        if !wt.is_main {
            let (state_for_remove, groups_for_remove) = (state.clone(), groups.clone());
            actions = actions.child(
                git_ui::danger_icon_button(
                    SharedString::from(format!("wt-remove-{}-{idx}", group.key)),
                    git_ui::ICON_TRASH,
                    t("worktree", "remove"),
                    true,
                )
                .on_click(move |_: &ClickEvent, window, cx| {
                    let group = &groups_for_remove[group_index];
                    open_remove_confirm(
                        &state_for_remove,
                        group,
                        &group.worktrees[idx],
                        window,
                        cx,
                    );
                }),
            );
        }
    }

    // 两行:名字 + 胶囊 / 弱化的路径。行首是 worktree 图标；当前行 accent 底 + 左侧 2px 竖条，
    // 其余已关联项目整行可点切换。
    div()
        .id(SharedString::from(format!("wt-row-{}-{idx}", group.key)))
        .relative()
        .flex()
        .items_center()
        .gap(px(8.0))
        .px(px(8.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .when(is_current, |el| {
            el.bg(ui::accent_subtle()).child(git_ui::selection_bar())
        })
        .when_some(switch_target, |el, target_project_id| {
            el.cursor_pointer()
                .hover(|el| el.bg(git_ui::hover_bg()))
                .on_click(move |_: &ClickEvent, window, cx| {
                    store_for_switch.update(cx, |store, cx| {
                        store.open_added_project(&target_project_id, window, cx);
                    });
                    crate::prompt::close_guarded(kind::GIT_WORKTREE, window, cx);
                })
        })
        .child(git_ui::icon(
            if wt.is_main {
                git_ui::ICON_REPO
            } else {
                git_ui::ICON_WORKTREE
            },
            git_ui::ICON_SIZE,
            if wt.is_main {
                ui::color_folder()
            } else if is_current {
                ui::accent()
            } else {
                ui::text_muted()
            },
        ))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .min_w_0()
                        .child(
                            div()
                                .min_w(px(0.0))
                                .text_size(ui::font_px(13.0))
                                .text_color(ui::text_primary())
                                .truncate()
                                .child(wt.name.clone()),
                        )
                        .child(badges),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_muted())
                        .child(wt.path.clone()),
                ),
        )
        .child(actions)
        .into_any_element()
}

/// 新建 worktree 折叠区（默认收起，点击触发条平滑高度展开）。
fn render_create_section(
    state: &Entity<WorktreeModal>,
    groups: &Rc<Vec<RepoGroup>>,
    multi_repo: bool,
    window: &Window,
    cx: &mut App,
) -> AnyElement {
    let s = state.read(cx);
    let selected: Vec<&RepoGroup> = groups
        .iter()
        .filter(|g| s.selected_keys.contains(&g.key))
        .collect();
    let multi_target = selected.len() > 1;
    let create_open = s.create_open;
    let turns = s
        .create_chevron
        .value_open("create", create_open, crate::fold_motion::OPEN_TURNS);

    let mut section = div()
        .mt(px(4.0))
        .pt(px(8.0))
        .border_t_1()
        .border_color(ui::border_subtle())
        .flex()
        .flex_col();

    // 折叠触发条：+ 新建 worktree [已选 N] …… ▸/▾
    let state_for_toggle = state.clone();
    let mut trigger = div()
        .id("worktree-create-toggle")
        .flex()
        .items_center()
        .gap(px(8.0))
        .h(px(32.0))
        .px(px(8.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(|el| el.bg(git_ui::hover_bg()))
        .child(git_ui::icon(
            git_ui::ICON_PLUS,
            git_ui::ICON_SIZE,
            if create_open {
                ui::accent()
            } else {
                ui::text_secondary()
            },
        ))
        .child(
            div()
                .text_size(ui::font_px(13.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(ui::text_primary())
                .child(t("worktree", "createTitle")),
        );
    if multi_repo {
        trigger = trigger.child(git_ui::pill(
            tr!(
                "worktree",
                "selectedCount",
                count = selected.len().to_string()
            ),
            ui::accent(),
        ));
    }
    trigger = trigger
        .child(div().ml_auto().child(git_ui::chevron(turns)))
        .on_click(move |_: &ClickEvent, _window, cx| {
            state_for_toggle.update(cx, |s, cx| {
                let next = !s.create_open;
                s.create_open = next;
                s.create_reveal.set_open(next);
                s.create_chevron
                    .flip_open("create", next, crate::fold_motion::OPEN_TURNS);
                if next {
                    s.auto_focused_on_open = false;
                }
                cx.notify();
            });
        });
    section = section.child(trigger);

    if !s.create_reveal.is_mounted() {
        return section.into_any_element();
    }

    // 展开的表单体（留出 2~3px 边距避免聚焦环被 HeightReveal 裁剪容器切边）
    let mut form = div()
        .pt(px(8.0))
        .pb(px(3.0))
        .px(px(2.0))
        .flex()
        .flex_col()
        .gap(px(8.0));

    if selected.is_empty() {
        form = form.child(hint_line(t("worktree", "selectRepoHint")));
        return section
            .child(s.create_reveal.render(form, window))
            .into_any_element();
    }

    // 模式切换段控件（选中底色是一块滑动的绝对定位块，见 `segment_slider`）
    let slider = s.mode_slider.clone();
    let active_ix = usize::from(s.mode == Mode::New);
    let own_bg = !slider.ready(active_ix);
    let mut seg = git_ui::segmented().child(git_ui::segmented_slider(&slider, active_ix));
    for (ix, (mode, label)) in [
        (Mode::Existing, t("worktree", "modeExisting")),
        (Mode::New, t("worktree", "modeNew")),
    ]
    .into_iter()
    .enumerate()
    {
        let active = s.mode == mode;
        let state_for_mode = state.clone();
        let id = match mode {
            Mode::New => "worktree-mode-new",
            Mode::Existing => "worktree-mode-existing",
        };
        let segment = git_ui::segment(id, label, active, own_bg).child(slider.probe(ix));
        seg = seg.child(segment.on_click(
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let input = state_for_mode.update(cx, |s, cx| {
                    s.mode_touched = true;
                    s.mode = mode;
                    s.create_error = None;
                    if matches!(mode, Mode::New) {
                        s.auto_focused_on_open = true;
                    }
                    cx.notify();
                    matches!(mode, Mode::New).then(|| s.new_branch.clone())
                });
                // 切到「新建」就该能直接敲分支名,不必再点一下输入框
                if let Some(input) = input {
                    autofocus(&input, window, cx);
                }
            },
        ));
    }
    form = form.child(div().flex().items_center().child(seg));

    // 分支区
    let branches_ready = selected
        .iter()
        .all(|g| s.branches_by_repo.contains_key(&g.key));
    match s.mode {
        Mode::Existing => {
            if !branches_ready {
                form = form.child(git_ui::loading_state(t("worktree", "loading")));
            } else {
                let options = s.options(groups, &selected);
                if options.is_empty() {
                    form = form.child(hint_line(if multi_target {
                        t("worktree", "noCommonBranch")
                    } else {
                        t("worktree", "noBranchAvailable")
                    }));
                } else {
                    form = form.child(dropdown(
                        state,
                        "worktree-branch",
                        if s.sel_branch.is_empty() {
                            t("worktree", "selectBranch").to_string()
                        } else {
                            s.sel_branch.clone()
                        },
                        options,
                        true,
                        |s, value| {
                            s.sel_branch = value;
                            s.create_error = None;
                        },
                    ));
                }
            }
        }
        Mode::New => {
            let options = s.options(groups, &selected);
            form = form.child(
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
                        false,
                        |s, value| {
                            s.base_branch = if value == t("worktree", "baseHead") {
                                String::new()
                            } else {
                                value
                            };
                            s.create_error = None;
                        },
                    )),
            );
        }
    }

    // 路径行
    let state_for_browse = state.clone();
    form = form.child(
        div()
            .flex()
            .gap(px(6.0))
            .child(div().flex_1().child(Input::new(&s.wt_path)))
            .child(
                git_ui::action_button(
                    "worktree-browse",
                    t("worktree", "browse"),
                    ButtonKind::Ghost,
                    true,
                )
                .on_click(move |_: &ClickEvent, window, cx| {
                    let s = state_for_browse.read(cx);
                    if let Some(connection) = s.context.connection().cloned() {
                        let initial = s.wt_path.read(cx).value().to_string();
                        let state = state_for_browse.clone();
                        crate::remote_directory_picker::open(
                            connection,
                            initial,
                            move |path, window, cx| {
                                state.update(cx, |s, cx| {
                                    s.path_edited = true;
                                    s.create_error = None;
                                    s.wt_path.update(cx, |input, cx| {
                                        input.set_value(path.clone(), window, cx)
                                    });
                                    cx.notify();
                                });
                            },
                            window,
                            cx,
                        );
                        return;
                    }
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
                                    s.create_error = None;
                                    let input = s.wt_path.clone();
                                    input.update(cx, |state, cx| {
                                        state.set_value(text.clone(), window, cx)
                                    });
                                    cx.notify();
                                });
                            });
                        })
                        .detach();
                }),
            ),
    );

    if let Some(err) = &s.create_error {
        form = form.child(git_ui::error_block(err.clone()));
    }
    for (target, err) in &s.create_results {
        form = form.child(git_ui::error_block(format!("{target}: {err}")));
    }

    // 合并底栏行：左「创建后切换到该项目」勾选框 + 右主按钮「创建」（实时校验禁用态）
    let current_branch = match s.mode {
        Mode::Existing => s.sel_branch.as_str().to_string(),
        Mode::New => s.new_branch.read(cx).value().to_string(),
    };
    let current_path = s.wt_path.read(cx).value().to_string();
    let creating = s.creating;
    let can_create =
        can_create_worktree(creating, !selected.is_empty(), &current_branch, &current_path);
    let switch_after_create = s.switch_after_create;
    let state_for_check = state.clone();
    let state_for_create = state.clone();
    let groups_for_create = groups.clone();
    form = form.child(
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .pt(px(2.0))
            .child(
                git_ui::check_row(
                    "worktree-switch-project",
                    switch_after_create,
                    t("worktree", "switchAfterCreate"),
                    false,
                )
                .on_click(move |_: &ClickEvent, _window, cx| {
                    state_for_check.update(cx, |s, cx| {
                        s.switch_after_create = !s.switch_after_create;
                        cx.notify();
                    });
                }),
            )
            .child(
                git_ui::action_button(
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
                    ButtonKind::Primary,
                    can_create,
                )
                .when(can_create, |el| {
                    el.on_click(move |_: &ClickEvent, window, cx| {
                        create(&state_for_create, &groups_for_create, window, cx);
                    })
                }),
            ),
    );

    section
        .child(s.create_reveal.render(form, window))
        .into_any_element()
}

/// 一个走 [`menu`] 的下拉选择器(替原版的 `<select>`)。
fn dropdown(
    state: &Entity<WorktreeModal>,
    id: &'static str,
    current: String,
    options: Rc<Vec<String>>,
    full_width: bool,
    apply: fn(&mut WorktreeModal, String),
) -> AnyElement {
    let state = state.clone();
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_between()
        .gap(px(6.0))
        .h(px(30.0))
        .px(px(8.0))
        .when(full_width, |el| el.w_full())
        .when(!full_width, |el| el.w(px(180.0)).flex_none())
        .rounded(px(4.0))
        .border_1()
        .border_color(ui::border_default())
        .cursor_pointer()
        .hover(|el| el.bg(git_ui::hover_bg()))
        .text_size(ui::font_px(12.0))
        .text_color(ui::text_primary())
        .child(div().min_w(px(0.0)).truncate().child(current))
        .child(git_ui::icon(
            git_ui::ICON_CHEVRON_DOWN,
            git_ui::ICON_SIZE_SM,
            ui::text_muted(),
        ))
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

fn create(state: &Entity<WorktreeModal>, groups: &[RepoGroup], window: &mut Window, cx: &mut App) {
    let (branch, targets, create_branch, base, switch_after_create) = {
        let s = state.read(cx);
        let branch = match s.mode {
            Mode::Existing => s.sel_branch.clone(),
            Mode::New => s.new_branch.read(cx).value().trim().to_string(),
        };
        let raw_path = s.wt_path.read(cx).value().trim().to_string();
        let selected: Vec<&RepoGroup> = groups
            .iter()
            .filter(|g| s.selected_keys.contains(&g.key))
            .collect();
        if !can_create_worktree(s.creating, !selected.is_empty(), &branch, &raw_path) {
            return;
        }
        let targets: Vec<(GitContext, String)> = if selected.len() == 1 {
            vec![(selected[0].context.clone(), raw_path)]
        } else {
            // 多选时路径输入框的语义变成「父目录」
            selected
                .iter()
                .map(|g| {
                    (
                        g.context.clone(),
                        context_join_path(
                            &g.context,
                            &raw_path,
                            &format!("{}-{}", g.name, sanitize_branch_for_dir(&branch)),
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
            s.switch_after_create,
        )
    };
    let mut guards = Vec::with_capacity(targets.len());
    for (context, _) in &targets {
        let Some(guard) = git_ui::gate::try_acquire_key(&context.repository_key()) else {
            state.update(cx, |s, cx| {
                s.create_error = Some(git_ui::gate_busy_text().into());
                cx.notify();
            });
            return;
        };
        guards.push(guard);
    }
    cx.refresh_windows();

    state.update(cx, |s, cx| {
        s.creating = true;
        s.create_error = None;
        s.create_results.clear();
        cx.notify();
    });

    let state = state.clone();
    window
        .spawn(cx, async move |cx| {
            let mut results: Vec<(GitContext, String, anyhow::Result<()>)> = Vec::new();
            for (context, target) in targets {
                let (branch, base) = (branch.clone(), base.clone());
                let target_for_task = target.clone();
                let repo_ctx = context.clone();
                let execution = context.clone();
                let result = mt_remote::run_git_on(&execution, move || {
                    // ⚠️ add_worktree 是 120s 阻塞 CLI
                    context.add_worktree(&target_for_task, &branch, create_branch, base.as_deref())
                })
                .await;
                results.push((repo_ctx, target, result.map(|_| ())));
            }
            drop(guards);

            let _ = cx.update(|window, cx| {
                cx.refresh_windows();
                // 无论成败先通知外面 + 清掉分支缓存(分支集合已变)
                let on_changed = state.read(cx).on_changed.clone();
                on_changed(cx);
                state.update(cx, |s, cx| {
                    s.branches_by_repo.clear();
                    s.branches_loading.clear();
                    s.creating = false;
                    cx.notify();
                });

                let store = state.read(cx).store.clone();
                let failures: Vec<(String, String)> = results
                    .iter()
                    .filter_map(|(_, target, result)| {
                        result.as_ref().err().map(|error| {
                            (
                                target.clone(),
                                git_ui::error_text_for_store(error, &store, cx),
                            )
                        })
                    })
                    .collect();
                let successes: Vec<(GitContext, String)> = results
                    .iter()
                    .filter(|(_, _, r)| r.is_ok())
                    .map(|(ctx, t, _)| (ctx.clone(), t.clone()))
                    .collect();

                // 新增的 worktree 始终自动添加为项目（优先挂到对应主仓库项目下，否则回落到来源项目）
                let mut first_new: Option<String> = None;
                if !successes.is_empty() {
                    let store = state.read(cx).store.clone();
                    let parent_hint = state.read(cx).project_id.clone();
                    store.update(cx, |store, cx| {
                        for (repo_ctx, target) in &successes {
                            let parent = store
                                .find_project_by_backend_path(
                                    repo_ctx.connection_id(),
                                    repo_ctx.path(),
                                )
                                .map(|p| p.id.clone())
                                .or_else(|| parent_hint.clone());
                            let id = store.add_git_worktree_project(
                                context_base_name(repo_ctx, target).to_string(),
                                target.clone(),
                                repo_ctx.connection_id().map(str::to_string),
                                parent,
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
                    Some(id) if switch_after_create => {
                        let store = state.read(cx).store.clone();
                        store.update(cx, |store, cx| store.open_added_project(&id, window, cx));
                        crate::prompt::close_guarded(kind::GIT_WORKTREE, window, cx);
                    }
                    _ => {
                        state.update(cx, |s, cx| {
                            let new_branch = s.new_branch.clone();
                            let wt_path = s.wt_path.clone();
                            s.sel_branch.clear();
                            s.path_edited = false;
                            s.last_suggested_path.clear();
                            s.mode_touched = false;
                            s.auto_focused_on_open = false;
                            new_branch.update(cx, |st, cx| st.set_value("", window, cx));
                            wt_path.update(cx, |st, cx| st.set_value("", window, cx));
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
        .find_project_by_backend_path(group.context.connection_id(), &wt.path)
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
            // 停在这个 worktree 里的终端(每帧重算:框开着时开关终端也跟得上)。
            // 删之前不关掉它们,Windows 上 git 会删到一半(见 `remove_worktree`)
            let busy = panes_inside(
                store.read(cx),
                &group.context,
                &wt.path,
                linked.as_ref().map(|(id, _)| id.as_str()),
            );

            let mut body = git_ui::form_body().child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
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
                    .child(git_ui::hint_line(wt.path.clone())),
            );
            if let Some((_, name)) = &linked {
                body = body.child(git_ui::notice_line(
                    tr!("worktree", "removeAlsoProject", name = name.clone()),
                    ui::color_warning(),
                ));
            }
            if !busy.is_empty() {
                // 个数进正文,名字列成灰字补充行(与「关整组」列 AI 终端同一口径)
                let mut list = div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(git_ui::notice_line(
                        tr!("worktree", "removeBusyTerminals", count = busy.len()),
                        ui::color_warning(),
                    ));
                for pane in &busy {
                    list = list.child(git_ui::hint_line(pane.label.clone()));
                }
                body = body.child(list);
            }
            body = body.child(
                git_ui::check_row("worktree-force", force, t("worktree", "forceRemove"), true)
                    .on_click(move |_: &ClickEvent, _window, cx| {
                        form_for_toggle.update(cx, |f, cx| {
                            f.force = !f.force;
                            cx.notify();
                        });
                    }),
            );
            if let Some(err) = error {
                body = body.child(git_ui::error_block(err));
            }

            dialog
                .title(t("worktree", "removeConfirmTitle"))
                .w(px(git_ui::DIALOG_W_SM))
                .footer(git_ui::dialog_footer(
                    if removing {
                        t("worktree", "removing")
                    } else {
                        t("worktree", "removeConfirm")
                    },
                    Some(t("worktree", "cancel").into()),
                    true,
                    removing,
                ))
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
                        f.error = None;
                        cx.notify();
                    });
                    remove_worktree(&state, &form_for_ok, &group, &wt, project_id, window, cx);
                    // 结果回来之前不关框:成功由 `remove_worktree` 关,失败把原因写回
                    // 表单、框留着给用户看(可勾「强制删除」再试)
                    false
                })
        },
    );
}

struct RemoveForm {
    force: bool,
    removing: bool,
    /// 上一次删除失败的原因(git 的 stderr 原文)。画在「强制删除」下面,
    /// 再点「删除」时清掉。
    error: Option<String>,
}

impl Render for RemoveForm {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 删 worktree(确认框「删除」的落点)。`form` 是确认框的状态实体:成功关框,
/// 失败把原因写回 `form.error`、复位 `removing`,框留着。
fn remove_worktree(
    state: &Entity<WorktreeModal>,
    form: &Entity<RemoveForm>,
    group: &RepoGroup,
    wt: &WorktreeInfo,
    project_id: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let project_id = state
        .read(cx)
        .store
        .read(cx)
        .find_project_by_backend_path(group.context.connection_id(), &wt.path)
        .map(|project| project.id.clone())
        .or_else(|| {
            project_id.filter(|id| {
                state
                    .read(cx)
                    .store
                    .read(cx)
                    .project(id)
                    .is_some_and(|project| {
                        project.ssh_connection_id.as_deref() == group.context.connection_id()
                            && path_key(group.context.is_remote(), &project.path)
                                == path_key(group.context.is_remote(), &wt.path)
                    })
            })
        });
    if project_id
        .as_deref()
        .is_some_and(|id| crate::workbench_area::project_has_dirty_documents(id, cx))
    {
        // 调用方已置 `removing`,不复位的话按钮永远卡在「删除中」
        form.update(cx, |f, cx| {
            f.removing = false;
            cx.notify();
        });
        show_alert(
            t("fileViewer", "unsavedTitle"),
            t("fileViewer", "projectRemovalBlocked"),
            window,
            cx,
        );
        return;
    }
    let force = form.read(cx).force;
    let store = state.read(cx).store.clone();
    let Some(guard) = git_ui::gate::try_acquire_key(&group.context.repository_key()) else {
        form.update(cx, |f, cx| {
            f.removing = false;
            f.error = Some(git_ui::gate_busy_text().into());
            cx.notify();
        });
        return;
    };
    cx.refresh_windows();
    // ① 先关该目录下的终端 —— Windows 上 shell 占着目录会让 remove 失败。
    //    ⚠️ 这一步必须在 remove **之前**:删失败时终端也已关掉(项目还在,pane 呈
    //    断开态、可重开),代价换来的是 Windows 上能删得掉,别为「失败不关终端」调序
    if let Some(id) = &project_id {
        store.update(cx, |store, cx| store.dispose_project_terminals(id, cx));
    }
    //    别的项目里停在 worktree 里的终端(worktree「开终端」开在当前项目下的那种)
    //    确认框里已逐个列出,这里整个关掉。shell 进程退出、放掉当前目录要一小会儿,
    //    由 `mt_project::git::remove_worktree` 的占用预检等它
    let busy = panes_inside(
        store.read(cx),
        &group.context,
        &wt.path,
        project_id.as_deref(),
    );
    if !busy.is_empty() {
        store.update(cx, |store, cx| {
            for pane in &busy {
                store.close_pane(&pane.project_id, &pane.pane_id, cx);
            }
        });
    }
    let (context, wt_path) = (group.context.clone(), wt.path.clone());
    let state = state.clone();
    let form = form.clone();
    window
        .spawn(cx, async move |cx| {
            let execution = context.clone();
            let wt_path_for_task = wt_path.clone();
            let result = mt_remote::run_git_on(&execution, move || {
                // ⚠️ 60s 阻塞 CLI
                context.remove_worktree(&wt_path_for_task, force)
            })
            .await;
            drop(guard);
            let _ = cx.update(|window, cx| {
                cx.refresh_windows();
                match result {
                    Ok(_) => {
                        // ② 成功之后才删项目(不留断链项目)，并清除同路径的旧忽略标记
                        store.update(cx, |store, cx| {
                            if store.clear_dismissed_worktree(execution.connection_id(), &wt_path) {
                                store.save_config_now();
                            }
                            if let Some(id) = project_id {
                                store.remove_project(&id, cx);
                            }
                        });
                        crate::prompt::close_guarded(kind::GIT_WORKTREE_REMOVE, window, cx);
                        let on_changed = state.read(cx).on_changed.clone();
                        on_changed(cx);
                        load(&state, cx);
                    }
                    Err(err) => {
                        tracing::warn!("[git] 删除 worktree 失败: {err:#}");
                        let message = if err.is::<mt_project::git::WorktreeRemoveError>() {
                            remove_failure_text(&err)
                        } else {
                            git_ui::error_text_for_store(&err, &store, cx)
                        };
                        // 删到一半、登记已经没了:列表里不会再有这一条,框里再点「删除」
                        // 也无从删起 —— 关框、刷新列表,原因与下一步退到提示框
                        if matches!(
                            err.downcast_ref::<mt_project::git::WorktreeRemoveError>(),
                            Some(mt_project::git::WorktreeRemoveError::Leftover { .. })
                        ) {
                            crate::prompt::close_guarded(kind::GIT_WORKTREE_REMOVE, window, cx);
                            let on_changed = state.read(cx).on_changed.clone();
                            on_changed(cx);
                            load(&state, cx);
                            show_alert(t("worktree", "removeConfirmTitle"), message, window, cx);
                            return;
                        }
                        // 其余失败:**不关框**,原因写回表单就地显示(被占用时是「未做任何
                        // 改动」与下一步;git 拒绝时是 stderr 原文,如「contains modified or
                        // untracked files, use --force」),按钮复位,用户关掉占用或勾「强制
                        // 删除」再点一次。项目还在,终端已在 ① 关掉,pane 呈断开态可重开。
                        form.update(cx, |f, cx| {
                            f.removing = false;
                            f.error = Some(message.clone());
                            cx.notify();
                        });
                        // 等待期间用户按 Esc 把确认框关了:框没了原因也不能丢,
                        // 退到提示框(叠在 worktree 弹窗之上,toast 会被遮罩挡住)
                        if !crate::prompt::is_open(kind::GIT_WORKTREE_REMOVE) {
                            show_alert(t("worktree", "removeConfirmTitle"), message, window, cx);
                        }
                    }
                }
            });
        })
        .detach();
}

/// 删除失败的可展示原因。`run_git_command` 把 git 的 stderr 原样装进错误,
/// 末尾带换行,这里收掉首尾空白;万一是空的(git 没吭声就非零退出)也别画一行空白。
fn remove_error_text(err: &anyhow::Error) -> String {
    git_ui::error_text(err)
}

/// A successful probe may omit only explicit NotFound results. Any ambiguous
/// metadata/network error invalidates the probe and must never authorize removal.
fn confirmed_missing_paths<'a>(
    context: &GitContext,
    candidates: &'a [String],
    existing: Result<&[String], ()>,
) -> Vec<&'a String> {
    let Ok(existing) = existing else {
        return Vec::new();
    };
    let present: HashSet<String> = existing
        .iter()
        .map(|path| path_key(context.is_remote(), path))
        .collect();
    candidates
        .iter()
        .filter(|path| !present.contains(&path_key(context.is_remote(), path)))
        .collect()
}

fn prune_and_probe(
    context: &GitContext,
    invalid: &[String],
) -> (anyhow::Result<String>, anyhow::Result<Vec<String>>) {
    let pruned = context.prune_worktrees();
    let existing = context.existing_directories(invalid);
    (pruned, existing)
}

/// 清理失效条目。只有同 backend 复核目录确实不存在后才删除关联项目。
fn prune(state: &Entity<WorktreeModal>, group: &RepoGroup, cx: &mut App) {
    let Some(guard) = git_ui::gate::try_acquire_key(&group.context.repository_key()) else {
        state.update(cx, |s, cx| {
            s.load_error = Some(git_ui::gate_busy_text().into());
            cx.notify();
        });
        return;
    };
    cx.refresh_windows();
    let invalid: Vec<String> = group
        .worktrees
        .iter()
        .filter(|w| !w.is_valid)
        .map(|w| w.path.clone())
        .collect();
    state.update(cx, |s, cx| {
        s.pruning_key = Some(group.key.clone());
        cx.notify();
    });
    let context = group.context.clone();
    let state = state.clone();
    cx.spawn(async move |cx| {
        let invalid_for_task = invalid.clone();
        let context_for_task = context.clone();
        let result = mt_remote::run_git_on(&context, move || {
            prune_and_probe(&context_for_task, &invalid_for_task)
        })
        .await;
        drop(guard);

        cx.update(|cx| {
            cx.refresh_windows();
            state.update(cx, |s, cx| {
                s.pruning_key = None;
                cx.notify();
            });
            let (pruned, existing) = result;
            match existing {
                Ok(still_there) => {
                    let missing = confirmed_missing_paths(&context, &invalid, Ok(&still_there));
                    let store = state.read(cx).store.clone();
                    store.update(cx, |store, cx| {
                        let mut cleared = false;
                        for path in missing {
                            if store.clear_dismissed_worktree(context.connection_id(), path) {
                                cleared = true;
                            }
                            let id = store
                                .find_project_by_backend_path(context.connection_id(), path)
                                .map(|p| p.id.clone());
                            if let Some(id) = id {
                                store.remove_project(&id, cx);
                            }
                        }
                        if cleared {
                            store.save_config_now();
                        }
                    });
                }
                Err(err) => {
                    let store = state.read(cx).store.clone();
                    let message = git_ui::error_text_for_store(&err, &store, cx);
                    state.update(cx, |s, cx| {
                        s.load_error = Some(message);
                        cx.notify();
                    });
                    return;
                }
            }
            let on_changed = state.read(cx).on_changed.clone();
            on_changed(cx);
            load(&state, cx);
            if let Err(err) = pruned {
                let store = state.read(cx).store.clone();
                let message = git_ui::error_text_for_store(&err, &store, cx);
                state.update(cx, |s, cx| {
                    s.load_error = Some(message);
                    cx.notify();
                });
            }
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune失败仍可确认不存在且保留存在目录() {
        let root = std::env::temp_dir().join(format!(
            "mt-worktree-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let kept = root.join("kept");
        std::fs::create_dir(&kept).unwrap();
        let missing = root.join("gone");
        let candidates = vec![
            kept.to_string_lossy().into_owned(),
            missing.to_string_lossy().into_owned(),
        ];
        let context = GitContext::local(root.to_string_lossy().into_owned());
        // 真实的非仓库路径让 git worktree prune 失败，但不阻断 metadata 核查。
        let (pruned, existing) = prune_and_probe(&context, &candidates);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(pruned.is_err());
        let existing = existing.expect("prune 失败不影响目录核查");
        assert_eq!(
            confirmed_missing_paths(&context, &candidates, Ok(&existing)),
            vec![&candidates[1]]
        );
    }

    #[test]
    fn 清理只删除成功核查中明确缺失的目录() {
        let candidates = vec!["/repo/kept".into(), "/repo/gone".into()];
        let existing = vec!["/repo/kept".into()];
        for context in [GitContext::local("/repo"), remote_context("ssh1")] {
            let missing = confirmed_missing_paths(&context, &candidates, Ok(&existing));
            assert_eq!(missing, vec![&candidates[1]]);
            assert!(
                confirmed_missing_paths(&context, &candidates, Err(())).is_empty(),
                "权限或网络错误不得授权删除任何项目"
            );
            assert!(
                confirmed_missing_paths(&context, &candidates, Ok(&candidates)).is_empty(),
                "prune 后仍存在的失效目录必须保留"
            );
        }
    }

    #[test]
    fn 远程清理存在性比较不混大小写目录() {
        let candidates = vec!["/repo/Worktree".into(), "/repo/worktree".into()];
        let existing = vec!["/repo/Worktree/".into()];
        assert_eq!(
            confirmed_missing_paths(&remote_context("ssh1"), &candidates, Ok(&existing)),
            vec![&candidates[1]]
        );
    }

    fn remote_context(connection_id: &str) -> GitContext {
        GitContext::remote(
            "source-project".into(),
            mt_config::SshConnection {
                id: connection_id.into(),
                name: "test".into(),
                host: "example.test".into(),
                port: 22,
                user: "test".into(),
                password: None,
                identity_file: None,
                group: None,
                use_system_ssh: false,
                extra: Default::default(),
            },
            "/repo".into(),
        )
    }

    #[test]
    fn 同仓库跨项目共用写闸但保留分组来源身份() {
        let source = remote_context("shared-worktree-gate");
        let other = GitContext::remote(
            "other-project".into(),
            source.connection().unwrap().clone(),
            "/repo".into(),
        );
        assert_ne!(source.key(), other.key());
        assert_eq!(source.repository_key(), other.repository_key());
        let guard = git_ui::gate::try_acquire_key(&source.repository_key()).unwrap();
        assert!(git_ui::gate::try_acquire_key(&other.repository_key()).is_none());
        assert!(
            git_ui::gate::try_acquire_key(&source.with_path("/another-repo").repository_key())
                .is_some()
        );
        drop(guard);
        assert!(git_ui::gate::try_acquire_key(&other.repository_key()).is_some());
    }

    #[test]
    fn 远程路径保留大小写和反斜杠() {
        let context = remote_context("ssh1");
        assert_eq!(separator(&context), '/');
        assert_ne!(path_key(true, "/Repo"), path_key(true, "/repo"));
        assert_ne!(path_key(true, r"/repo\wt"), path_key(true, "/repo/wt"));
        assert!(backend_path_within(&context, "/repo/wt/src", "/repo/wt/"));
        assert!(!backend_path_within(&context, "/Repo/wt/src", "/repo/wt"));
        assert!(!backend_path_within(&context, "/repo/wt-2", "/repo/wt"));
        assert!(!backend_path_within(&context, r"/repo/wt\src", "/repo/wt"));
        assert_eq!(context_join_path(&context, r"/repo\", "wt"), r"/repo\/wt");
        assert_eq!(context_parent_dir(&context, r"/repo/wt\name"), "/repo");
        assert_eq!(context_base_name(&context, r"/repo/wt\name"), r"wt\name");
        let groups = merge_groups(
            &context,
            vec![
                ("/Repo".into(), Ok(vec![wt("Repo", "/Repo", true, None)])),
                ("/repo".into(), Ok(vec![wt("repo", "/repo", true, None)])),
            ],
        );
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].context.connection_id(), Some("ssh1"));
        assert_eq!(groups[0].context.path(), "/Repo");
    }

    #[test]
    fn 远程项目匹配限制连接且区分大小写() {
        let context = remote_context("ssh1");
        let project = |id: &str, path: &str, ssh: Option<&str>| ProjectConfig {
            id: id.into(),
            path: path.into(),
            ssh_connection_id: ssh.map(str::to_string),
            ..Default::default()
        };
        let ids = backend_project_ids(
            &[
                project("local", "/repo", None),
                project("other", "/repo", Some("ssh2")),
                project("remote", "/repo", Some("ssh1")),
                project("uppercase", "/Repo", Some("ssh1")),
            ],
            &context,
        );
        assert_eq!(ids.len(), 2);
        assert_eq!(ids.get("/repo").map(String::as_str), Some("remote"));
        assert_eq!(ids.get("/Repo").map(String::as_str), Some("uppercase"));
    }

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
            upstream: None,
        }
    }

    /// 删除失败的原因要能直接画出来:git stderr 的尾换行收掉,空的不画空白行。
    #[test]
    fn 删除失败原因去掉首尾空白且不为空() {
        let err = anyhow::anyhow!(
            "fatal: 'D:/repo-wt' contains modified or untracked files, use --force to delete it\n"
        );
        assert_eq!(
            remove_error_text(&err),
            "fatal: 'D:/repo-wt' contains modified or untracked files, use --force to delete it"
        );
        assert!(!remove_error_text(&anyhow::anyhow!("\n")).is_empty());
    }

    /// 「终端停在 worktree 里」的判据:目录本身与其子目录算,同名前缀的兄弟目录不算;
    /// 分隔符 / 大小写 / 尾斜杠不计(pane cwd 与 libgit2 路径写法常不一样)。
    #[test]
    fn 终端目录落在_worktree_里的判据() {
        let wt = r"D:\Git\repo-wt";
        assert!(path_within(r"D:\Git\repo-wt", wt));
        assert!(path_within(r"d:/git/repo-wt/", wt));
        assert!(path_within(r"D:\Git\repo-wt\src\deep", wt));
        assert!(
            !path_within(r"D:\Git\repo-wt-2", wt),
            "同名前缀的兄弟目录不算"
        );
        assert!(!path_within(r"D:\Git", wt), "父目录不算");
        assert!(!path_within(r"D:\Git\repo-wt", ""), "空目录什么都不包含");
    }

    /// 删除失败的文案:占用 / 半删两种给原因与下一步(带上路径),其余是 git 原文。
    #[test]
    fn 删除失败按类型给原因与下一步() {
        use mt_project::git::WorktreeRemoveError;
        let in_use: anyhow::Error = WorktreeRemoveError::InUse {
            path: r"D:\repo-wt".into(),
            busy: r"D:\repo-wt\sub".into(),
        }
        .into();
        let text = remove_failure_text(&in_use);
        assert!(text.contains(r"D:\repo-wt\sub"), "{text}");
        assert!(!text.contains('{'), "占位符没换干净: {text}");

        let leftover: anyhow::Error = WorktreeRemoveError::Leftover {
            path: r"D:\repo-wt".into(),
            detail: "拒绝访问。".into(),
        }
        .into();
        let text = remove_failure_text(&leftover);
        assert!(
            text.contains(r"D:\repo-wt") && text.contains("拒绝访问"),
            "{text}"
        );
        assert!(!text.contains('{'), "占位符没换干净: {text}");

        let plain = anyhow::anyhow!("fatal: 'x' contains modified files, use --force\n");
        assert_eq!(
            remove_failure_text(&plain),
            "fatal: 'x' contains modified files, use --force"
        );
    }

    /// 归并键:分隔符统一、去尾斜杠、转小写(`windows_eq_key`)。三条都不能少 ——
    /// libgit2 给的主工作区路径(`D:/Git/Repo/`)与用户填的(`d:\git\repo`)要合成一组。
    #[cfg(windows)]
    #[test]
    fn 归并键容忍分隔符大小写与尾斜杠() {
        let list = vec![wt("Repo", "D:/Git/Repo/", true, Some("main"))];
        let groups = merge_groups(
            &GitContext::local("D:/Git/Repo"),
            vec![
                (r"D:\Git\Repo".into(), Ok(list)),
                (
                    r"d:\git\repo\".into(),
                    Err("另一条路径扫到的同一个仓库".into()),
                ),
            ],
        );
        assert_eq!(groups.len(), 1, "同一仓库的不同写法只留一组");
        assert_eq!(groups[0].main_path, "D:/Git/Repo/");
        // 不同仓库不许被合并
        let groups = merge_groups(
            &GitContext::local("D:/Git/Repo"),
            vec![
                (r"D:\Git\Repo".into(), Err("x".into())),
                (r"D:\Git\Repo2".into(), Err("y".into())),
            ],
        );
        assert_eq!(groups.len(), 2);
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
        let groups = merge_groups(
            &GitContext::local("/a"),
            vec![
                ("/a/repo".into(), Ok(list.clone())),
                // 同一份结果(从 worktree 目录扫到的)—— 不该重复展示
                ("/a/repo-wt1".into(), Ok(list.clone())),
            ],
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].main_path, "/a/repo");
        assert_eq!(groups[0].name, "repo");
        assert_eq!(groups[0].worktrees.len(), 2);
    }

    /// 某个仓库 `list_worktrees` 失败时只让那一组显示错误,别的组照常。
    #[test]
    fn 单组失败不拖垮别的组() {
        let groups = merge_groups(
            &GitContext::local("/a"),
            vec![
                ("/a/ok".into(), Ok(vec![wt("ok", "/a/ok", true, None)])),
                ("/a/bad".into(), Err("仓库损坏".into())),
            ],
        );
        assert_eq!(groups.len(), 2);
        assert!(groups[0].error.is_none());
        assert_eq!(groups[1].error.as_deref(), Some("仓库损坏"));
        assert!(groups[1].worktrees.is_empty());
    }

    /// 「检出现有分支」:本地分支减去已被占用的,再取全组交集。
    #[test]
    fn 可用分支取交集且排除已占用() {
        let b1 = vec![
            branch("main", false),
            branch("feat", false),
            branch("origin/main", true),
        ];
        // main 被主工作区占着
        let g1 = (b1.as_slice(), vec![Some("main"), None]);
        assert_eq!(available_branches(std::slice::from_ref(&g1)), vec!["feat"]);

        let b2 = vec![
            branch("main", false),
            branch("feat", false),
            branch("x", false),
        ];
        let g2 = (b2.as_slice(), vec![Some("main")]);
        // 两组交集仍是 feat(x 只有第二组有)
        assert_eq!(available_branches(&[g1.clone(), g2]), vec!["feat"]);

        // 没有共同可用分支
        let b3 = vec![branch("main", false)];
        let g3 = (b3.as_slice(), vec![Some("main")]);
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
            base_branch_options(&[&a, &b]),
            vec!["main", "origin/main"],
            "顺序按第一组"
        );
        assert_eq!(base_branch_options(&[&a]).len(), 3);
        assert!(base_branch_options(&[]).is_empty());
    }

    /// 交集改成先建集合再查之后,结果(连同次序、第一组里的重名)与逐条 `Vec::contains`
    /// 的旧写法一致。
    #[test]
    fn 交集与旧写法一致且保持次序() {
        let reference = |groups: &[Vec<&str>]| -> Vec<String> {
            let Some(first) = groups.first() else {
                return Vec::new();
            };
            first
                .iter()
                .filter(|name| groups[1..].iter().all(|g| g.contains(name)))
                .map(|name| name.to_string())
                .collect()
        };
        let mut state = 11u64;
        let mut rand = move |n: u64| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) % n.max(1)
        };
        let names: Vec<String> = (0..60).map(|i| format!("b{i}")).collect();
        for round in 0..30 {
            let groups: Vec<Vec<&str>> = (0..1 + rand(4))
                .map(|_| {
                    (0..rand(80))
                        .map(|_| names[rand(names.len() as u64) as usize].as_str())
                        .collect()
                })
                .collect();
            assert_eq!(
                intersect_ordered(&groups),
                reference(&groups),
                "第 {round} 轮"
            );
        }
        assert!(intersect_ordered(&[]).is_empty());
    }

    /// 下拉选项缓存:同一份分组 / 分支 / 模式命中;勾选、模式、某组分支换了一份都要重算。
    #[test]
    fn 下拉选项缓存按指针认新旧() {
        let groups = Rc::new(merge_groups(
            &GitContext::local("/"),
            vec![
                ("/a".into(), Ok(vec![wt("a", "/a", true, Some("main"))])),
                ("/b".into(), Ok(vec![wt("b", "/b", true, Some("dev"))])),
            ],
        ));
        let branches = Rc::new(vec![branch("main", false), branch("dev", false)]);
        let key = |list: &Rc<Vec<BranchInfo>>| -> Vec<(String, Option<Rc<Vec<BranchInfo>>>)> {
            vec![(groups[0].key.clone(), Some(list.clone()))]
        };
        let cache = OptionsCache {
            mode: Mode::Existing,
            groups: groups.clone(),
            selected: key(&branches),
            options: Rc::new(Vec::new()),
        };
        assert!(cache.matches(Mode::Existing, &groups, &key(&branches)));
        assert!(
            !cache.matches(Mode::New, &groups, &key(&branches)),
            "换了模式"
        );
        let same_content = Rc::new((*branches).clone());
        assert!(
            !cache.matches(Mode::Existing, &groups, &key(&same_content)),
            "分支换了一份(哪怕内容一样)"
        );
        assert!(!cache.matches(Mode::Existing, &groups, &[]), "勾选变了");
        let regrouped = Rc::new((*groups).clone());
        assert!(
            !cache.matches(Mode::Existing, &regrouped, &key(&branches)),
            "分组重载了"
        );

        // 选项本身:已有分支去掉被占用的,起点选项以 HEAD 打头
        let mut by_repo = HashMap::new();
        by_repo.insert(groups[0].key.clone(), branches.clone());
        let picked = [&groups[0]];
        assert_eq!(
            dropdown_options(Mode::Existing, &picked, &by_repo),
            vec!["dev"]
        );
        let base = dropdown_options(Mode::New, &picked, &by_repo);
        assert_eq!(base[0], t("worktree", "baseHead"));
        assert_eq!(&base[1..], ["main", "dev"]);
    }

    /// 「已是项目」的表:远程项目不算,同一路径(换写法)取最先的那个。
    #[test]
    fn 已是项目按归一路径查() {
        let project = |id: &str, path: &str, ssh: Option<&str>| ProjectConfig {
            id: id.into(),
            path: path.into(),
            ssh_connection_id: ssh.map(str::to_string),
            ..Default::default()
        };
        let ids = backend_project_ids(
            &[
                project("p1", "D:/Git/Repo", None),
                project("p2", "d:\\git\\repo\\", None),
                project("p3", "D:/Git/Other", Some("ssh1")),
            ],
            &GitContext::local("D:/Git/Repo"),
        );
        assert_eq!(
            ids.get(&windows_eq_key("D:\\Git\\Repo"))
                .map(String::as_str),
            Some("p1")
        );
        assert!(!ids.contains_key(&windows_eq_key("D:/Git/Other")));
    }

    fn make_group(
        context: GitContext,
        main_path: &str,
        worktrees: Vec<WorktreeInfo>,
        has_dot_worktrees: bool,
    ) -> RepoGroup {
        RepoGroup {
            key: context.key(),
            name: context_base_name(&context, main_path).to_string(),
            main_path: main_path.to_string(),
            context,
            worktrees,
            has_dot_worktrees,
            error: None,
        }
    }

    #[test]
    fn 智能默认路径按仓库存放习惯推断() {
        let ctx = GitContext::local(r"D:\Git\mini-term");
        let main_wt = wt("mini-term", r"D:\Git\mini-term", true, Some("main"));

        // 1. 已有 .worktrees 下的有效 worktree → 沿用且不加仓库名前缀
        let g = make_group(
            ctx.clone(),
            r"D:\Git\mini-term",
            vec![
                main_wt.clone(),
                wt(
                    "worktree-ui",
                    r"D:\Git\mini-term\.worktrees\worktree-ui",
                    false,
                    Some("feat/worktree-list-ui"),
                ),
            ],
            false,
        );
        let tpl = infer_worktree_path_template(&g).unwrap();
        assert_eq!(tpl.parent_dir, r"D:\Git\mini-term\.worktrees");
        assert!(!tpl.prefix_repo_name);
        assert_eq!(
            tpl.format_dir(&g.context, &g.name, "feat-a"),
            r"D:\Git\mini-term\.worktrees\feat-a"
        );

        // 2. .worktrees 与其他仓库内子目录并存时优先 .worktrees
        let g_prefer = make_group(
            ctx.clone(),
            r"D:\Git\mini-term",
            vec![
                main_wt.clone(),
                wt("a", r"D:\Git\mini-term\wts\a", false, Some("a")),
                wt("b", r"D:\Git\mini-term\wts\b", false, Some("b")),
                wt("c", r"D:\Git\mini-term\.worktrees\c", false, Some("c")),
            ],
            false,
        );
        assert_eq!(
            infer_worktree_path_template(&g_prefer).unwrap().parent_dir,
            r"D:\Git\mini-term\.worktrees"
        );

        // 3. 无有效 linked worktree，但磁盘存在 .worktrees 目录 → 走 .worktrees/<分支>
        let mut invalid_wt = wt("dead", r"D:\Other\mini-term-dead", false, Some("dead"));
        invalid_wt.is_valid = false;
        let g_dot = make_group(
            ctx.clone(),
            r"D:\Git\mini-term",
            vec![main_wt.clone(), invalid_wt.clone()],
            true,
        );
        let tpl_dot = infer_worktree_path_template(&g_dot).unwrap();
        assert_eq!(tpl_dot.parent_dir, r"D:\Git\mini-term\.worktrees");
        assert!(!tpl_dot.prefix_repo_name);

        // 4. 已有外部 worktree → 沿用出现最多的外部父目录并加 <仓库名>- 前缀
        let g_ext = make_group(
            ctx.clone(),
            r"D:\Git\mini-term",
            vec![
                main_wt.clone(),
                wt("mini-term-a", r"E:\WT\mini-term-a", false, Some("a")),
                wt("mini-term-b", r"E:\WT\mini-term-b", false, Some("b")),
                wt("mini-term-c", r"D:\Git\mini-term-c", false, Some("c")),
            ],
            false,
        );
        let tpl_ext = infer_worktree_path_template(&g_ext).unwrap();
        assert_eq!(tpl_ext.parent_dir, r"E:\WT");
        assert!(tpl_ext.prefix_repo_name);
        assert_eq!(
            tpl_ext.format_dir(&g_ext.context, &g_ext.name, "feat-x"),
            r"E:\WT\mini-term-feat-x"
        );

        // 5. 无历史且无 .worktrees → 回落经典同级目录
        let g_fallback = make_group(
            ctx.clone(),
            r"D:\Git\mini-term",
            vec![main_wt, invalid_wt],
            false,
        );
        let tpl_fb = infer_worktree_path_template(&g_fallback).unwrap();
        assert_eq!(tpl_fb.parent_dir, r"D:\Git");
        assert!(tpl_fb.prefix_repo_name);

        // 6. 远端 POSIX 仓库同样按习惯推断
        let rctx = remote_context("ssh1").with_path("/srv/app");
        let g_remote = make_group(
            rctx,
            "/srv/app",
            vec![
                wt("app", "/srv/app", true, Some("main")),
                wt("ui", "/srv/app/.worktrees/ui", false, Some("ui")),
            ],
            false,
        );
        let tpl_r = infer_worktree_path_template(&g_remote).unwrap();
        assert_eq!(
            tpl_r.format_dir(&g_remote.context, &g_remote.name, "dev"),
            "/srv/app/.worktrees/dev"
        );
    }

    #[test]
    fn 空分支不填假路径并生成占位预览() {
        let ctx = GitContext::local(r"D:\Git\mini-term");
        let g1 = make_group(
            ctx.clone(),
            r"D:\Git\mini-term",
            vec![wt("mini-term", r"D:\Git\mini-term", true, Some("main"))],
            true,
        );
        let g2 = make_group(
            GitContext::local(r"D:\Git\other"),
            r"D:\Git\other",
            vec![wt("other", r"D:\Git\other", true, Some("main"))],
            false,
        );

        // 单选 + 空分支：不填假 -worktree，只在 placeholder 给预览
        let (val, ph) = compute_path_suggestion(&[&g1], "   ", r"D:\Git\mini-term");
        assert_eq!(val.as_deref(), Some(""));
        assert_eq!(
            ph,
            format!(
                r"D:\Git\mini-term\.worktrees\{}",
                t("worktree", "branchDirPlaceholder")
            )
        );

        // 单选 + 非空分支：给出真实建议路径
        let (val, _) = compute_path_suggestion(&[&g1], "feat/new-ui", r"D:\Git\mini-term");
        assert_eq!(
            val.as_deref(),
            Some(r"D:\Git\mini-term\.worktrees\feat-new-ui")
        );

        // 多选：语义变为父目录
        let (val, ph) = compute_path_suggestion(&[&g1, &g2], "", r"D:\Git");
        assert_eq!(val.as_deref(), Some(r"D:\Git"));
        assert_eq!(ph, t("worktree", "parentPathPlaceholder"));

        // 未勾选：清空
        let (val, ph) = compute_path_suggestion(&[], "feat", r"D:\Git");
        assert_eq!(val.as_deref(), Some(""));
        assert_eq!(ph, t("worktree", "pathPlaceholder"));
    }

    #[test]
    fn 创建按钮启用条件实时校验() {
        assert!(can_create_worktree(false, true, "feat", r"D:\wt"));
        assert!(!can_create_worktree(true, true, "feat", r"D:\wt"));
        assert!(!can_create_worktree(false, false, "feat", r"D:\wt"));
        assert!(!can_create_worktree(false, true, "  ", r"D:\wt"));
        assert!(!can_create_worktree(false, true, "feat", "  "));
    }
}
