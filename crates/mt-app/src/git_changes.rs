//! Git 面板的「更改」区。对应 `src/components/GitChanges.tsx`(442 行)。
//!
//! # 三个分组的口径(`GitChanges.tsx:109-111`)
//!
//! ```text
//! staged    = 有 stagedStatus
//! unstaged  = 有 unstagedStatus 且不是 untracked
//! untracked = unstagedStatus === 'untracked'
//! ```
//!
//! ⚠️ **同一个文件可以同时出现在 staged 与 unstaged 两组**(部分暂存),这是正确
//! 行为 —— 所以行的 `ElementId` 必须带区名前缀(原版 key 是 `${area}-${path}`,
//! 规格 §11 第 29 条),否则 gpui 会撞 id。
//!
//! # 失败一律静默
//!
//! 原版每个 `invoke` 的 catch 都只 `console.error`(不弹 toast、不显红),
//! 这里对应 `eprintln!`。**唯一的例外**是「丢弃」前的确认框(§4.5)。
//!
//! # 阻塞调用
//!
//! `get_changes_status` / `git_stage_all` / `git_unstage_all` / `git_discard_file`
//! 是 git2 的同步 IO,`git_commit` 是带 60s 超时的 git CLI —— 全部丢
//! `cx.background_executor()`(范式照 `file_tree.rs:138-156`)。

use std::collections::HashSet;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, AppContext as _, ClickEvent, Context, Entity, EventEmitter, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Input, InputState};
use mt_project::git::{ChangeFileStatus, GitStatus};

use crate::i18n::{t, tr};
use crate::menu;
use crate::prompt::Confirm;
use crate::store::AppStore;
use crate::ui;
use crate::{git_diff, git_watch};

gpui::actions!(
    mini_term,
    [
        /// 提交(Ctrl+Enter / Cmd+Enter,`GitChanges.tsx:411-415`)
        GitCommitMessage
    ]
);

/// 「更改」区往上冒的事件。原版是 `onCommitSuccess` 这个 prop。
pub enum GitChangesEvent {
    /// 提交成功 —— 容器要刷新历史与分支(分支头已前移)。
    Committed,
}

/// 三个分组。`area` 既决定取哪一个 status,也进 `ElementId` 前缀。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    Staged,
    Unstaged,
    Untracked,
}

impl Area {
    fn key(self) -> &'static str {
        match self {
            Area::Staged => "staged",
            Area::Unstaged => "unstaged",
            Area::Untracked => "untracked",
        }
    }
}

/// 该文件在该区里生效的状态(staged 区取 `stagedStatus`,其余取 `unstagedStatus`)。
pub fn status_in_area(file: &ChangeFileStatus, area: Area) -> Option<&GitStatus> {
    match area {
        Area::Staged => file.staged_status.as_ref(),
        _ => file.unstaged_status.as_ref(),
    }
}

/// 状态 → 单字符(`statusLabelFor`,`GitChanges.tsx:60-70`)。认不出的是**空格**。
pub fn status_label_for(status: Option<&GitStatus>) -> &'static str {
    match status {
        Some(GitStatus::Modified) => "M",
        Some(GitStatus::Added) => "A",
        Some(GitStatus::Deleted) => "D",
        Some(GitStatus::Renamed) => "R",
        Some(GitStatus::Untracked) => "?",
        Some(GitStatus::Conflicted) => "C",
        None => " ",
    }
}

/// 状态 → 颜色(`statusColor`,`GitChanges.tsx:72-82`)。
///
/// ⚠️ `conflicted` 落到 default 的 muted —— 原版如此,照抄。
pub fn status_color(status: Option<&GitStatus>) -> gpui::Hsla {
    match status {
        Some(GitStatus::Modified) => ui::color_warning(),
        Some(GitStatus::Added) => ui::color_success(),
        Some(GitStatus::Deleted) => ui::color_error(),
        Some(GitStatus::Renamed) => ui::color_info(),
        Some(GitStatus::Untracked) => ui::color_success(),
        _ => ui::text_muted(),
    }
}

// ─── 树形视图 ─────────────────────────────────────────────────

/// `buildFileTree` 建出来的节点(`GitChanges.tsx:36-56`)。
///
/// `file` 是**下标**而不是拷贝 —— 建树只是换个排列方式,内容仍在原数组里。
#[derive(Debug, PartialEq)]
pub struct FileTreeNode {
    pub name: String,
    pub full_path: String,
    pub file: Option<usize>,
    pub children: Vec<FileTreeNode>,
}

/// 按 `/` 切 path 建目录树。**不做单链目录压缩**(与 FileTree 的
/// `compactDirChains` 不同,这里没有)。
pub fn build_file_tree(paths: &[(usize, &str)]) -> Vec<FileTreeNode> {
    let mut root: Vec<FileTreeNode> = Vec::new();
    for (index, path) in paths {
        let parts: Vec<&str> = path.split('/').collect();
        let mut current = &mut root;
        let mut path_so_far = String::new();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                path_so_far.push('/');
            }
            path_so_far.push_str(part);
            if i == parts.len() - 1 {
                current.push(FileTreeNode {
                    name: (*part).to_string(),
                    full_path: path_so_far.clone(),
                    file: Some(*index),
                    children: Vec::new(),
                });
            } else {
                let pos = current
                    .iter()
                    .position(|n| n.name == *part && n.file.is_none());
                let pos = match pos {
                    Some(pos) => pos,
                    None => {
                        current.push(FileTreeNode {
                            name: (*part).to_string(),
                            full_path: path_so_far.clone(),
                            file: None,
                            children: Vec::new(),
                        });
                        current.len() - 1
                    }
                };
                current = &mut current[pos].children;
            }
        }
    }
    root
}

/// 拍平后的一行。
enum TreeRow {
    Dir {
        name: String,
        full_path: String,
        depth: usize,
        collapsed: bool,
    },
    File {
        index: usize,
        depth: usize,
    },
}

fn flatten_tree(
    nodes: &[FileTreeNode],
    area: Area,
    collapsed: &HashSet<String>,
    depth: usize,
    out: &mut Vec<TreeRow>,
) {
    for node in nodes {
        match node.file {
            Some(index) => out.push(TreeRow::File { index, depth }),
            None => {
                // 折叠集合 key 带区名 —— 同一路径在三个区各自独立折叠
                // (`GitChanges.tsx:285`、`:292`)
                let key = format!("{}:{}", area.key(), node.full_path);
                let is_collapsed = collapsed.contains(&key);
                out.push(TreeRow::Dir {
                    name: node.name.clone(),
                    full_path: node.full_path.clone(),
                    depth,
                    collapsed: is_collapsed,
                });
                if !is_collapsed {
                    flatten_tree(&node.children, area, collapsed, depth + 1, out);
                }
            }
        }
    }
}

// ─── 组件 ─────────────────────────────────────────────────────

pub struct GitChanges {
    store: Entity<AppStore>,
    /// 空串 = 无仓库。此时 `load` 直接 return(既不 loading 也不报错,
    /// 停在「暂无变更」——`GitChanges.tsx:115`)。
    repo_path: String,
    changes: Vec<ChangeFileStatus>,
    loading: bool,
    commit_input: Entity<InputState>,
    committing: bool,
    /// 组件态,**不落盘**(抽屉一关就没,规格 §11 第 25 条)。
    collapsed_dirs: HashSet<String>,
    /// 迟到响应丢弃。
    request: u64,
    /// pty-output 嗅探的 500ms 去抖终点。
    debounce_until: Option<Instant>,
}

impl EventEmitter<GitChangesEvent> for GitChanges {}

impl GitChanges {
    pub fn new(store: Entity<AppStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let commit_input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .rows(3)
                .placeholder(t("panels", "commitPlaceholder"))
        });
        Self {
            store,
            repo_path: String::new(),
            changes: Vec::new(),
            loading: false,
            commit_input,
            committing: false,
            collapsed_dirs: HashSet::new(),
            request: 0,
            debounce_until: None,
        }
    }

    /// 换仓库(容器调)。空串 = 无仓库。
    pub fn set_repo(&mut self, repo_path: &str, cx: &mut Context<Self>) {
        if self.repo_path == repo_path {
            return;
        }
        self.repo_path = repo_path.to_string();
        self.changes.clear();
        self.collapsed_dirs.clear();
        self.load(cx);
        cx.notify();
    }

    /// 容器的 pty-output 嗅探命中了 —— 起(或推后)自己那个 500ms 去抖窗口。
    ///
    /// 原版是两个面板**各有一个**定时器(规格 §11 第 27 条),所以这里的窗口
    /// 与提交历史区各算各的。
    pub fn note_pty_hit(&mut self) {
        self.debounce_until = Some(Instant::now() + Duration::from_millis(git_watch::DEBOUNCE_MS));
    }

    /// 容器的节拍:去抖窗口到点了就重取。
    pub fn tick(&mut self, cx: &mut Context<Self>) {
        if self.debounce_until.is_some_and(|at| Instant::now() >= at) {
            self.debounce_until = None;
            self.load(cx);
        }
    }

    pub fn load(&mut self, cx: &mut Context<Self>) {
        if self.repo_path.is_empty() {
            return;
        }
        self.loading = true;
        self.request += 1;
        let req = self.request;
        let repo = std::path::PathBuf::from(&self.repo_path);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { mt_project::git::get_changes_status(&repo) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.request != req {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(list) => this.changes = list,
                    Err(err) => {
                        // 原版 `.catch(() => setChanges([]))` —— 失败即清空
                        eprintln!("[git] 取变更失败: {err:#}");
                        this.changes.clear();
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn group_indices(&self, area: Area) -> Vec<usize> {
        self.changes
            .iter()
            .enumerate()
            .filter(|(_, c)| match area {
                Area::Staged => c.staged_status.is_some(),
                Area::Unstaged => matches!(
                    c.unstaged_status,
                    Some(ref s) if *s != GitStatus::Untracked
                ),
                Area::Untracked => c.unstaged_status == Some(GitStatus::Untracked),
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// 跑一件阻塞的 git 动作,完成后重取列表。失败静默(照原版)。
    fn run_op(
        &mut self,
        op: impl FnOnce(&std::path::Path) -> anyhow::Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.repo_path.is_empty() {
            return;
        }
        let repo = std::path::PathBuf::from(&self.repo_path);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { op(&repo) })
                .await;
            if let Err(err) = result {
                eprintln!("[git] 变更操作失败: {err:#}");
            }
            let _ = this.update(cx, |this: &mut Self, cx| this.load(cx));
        })
        .detach();
    }

    fn stage(&mut self, path: String, cx: &mut Context<Self>) {
        self.run_op(
            move |repo| mt_project::git::git_stage(repo, &[path]),
            cx,
        );
    }

    fn unstage(&mut self, path: String, cx: &mut Context<Self>) {
        self.run_op(
            move |repo| mt_project::git::git_unstage(repo, &[path]),
            cx,
        );
    }

    fn stage_all(&mut self, cx: &mut Context<Self>) {
        self.run_op(mt_project::git::git_stage_all, cx);
    }

    fn unstage_all(&mut self, cx: &mut Context<Self>) {
        self.run_op(mt_project::git::git_unstage_all, cx);
    }

    fn discard(&mut self, paths: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        let this = cx.entity();
        Confirm::new(
            t("gitChanges", "discardTitle"),
            tr!("gitChanges", "discardConfirm", count = count.to_string()),
        )
        .ok_text(t("gitChanges", "discardOk"))
        .cancel_text(t("gitChanges", "discardCancel"))
        .open(
            move |_window, cx| {
                let paths = paths.clone();
                this.update(cx, |this, cx| {
                    this.run_op(
                        move |repo| mt_project::git::git_discard_file(repo, &paths),
                        cx,
                    );
                });
            },
            window,
            cx,
        );
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = self.commit_input.read(cx).value().trim().to_string();
        // 前置:消息非空 + 有暂存内容(`GitChanges.tsx:186`)
        if message.is_empty()
            || self.group_indices(Area::Staged).is_empty()
            || self.committing
            || self.repo_path.is_empty()
        {
            return;
        }
        self.committing = true;
        cx.notify();
        let repo = std::path::PathBuf::from(&self.repo_path);
        let input = self.commit_input.clone();
        cx.spawn_in(window, async move |this, cx| {
            // git_commit 走 git CLI(60s 超时兜底 GPG/pre-commit hook 挂起)——
            // 即便如此也不上主线程跑,hook 慢的仓库会把 UI 卡满整个超时窗口
            let result = cx
                .background_executor()
                .spawn(async move { mt_project::git::git_commit(&repo, &message) })
                .await;
            let _ = this.update_in(cx, |this: &mut Self, window, cx| {
                this.committing = false;
                match result {
                    Ok(_) => {
                        input.update(cx, |state, cx| state.set_value("", window, cx));
                        this.load(cx);
                        cx.emit(GitChangesEvent::Committed);
                    }
                    Err(err) => eprintln!("[git] 提交失败: {err:#}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn view_diff(&self, index: usize, area: Area, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.changes.get(index) else {
            return;
        };
        git_diff::open_file_diff(
            self.store.clone(),
            self.repo_path.clone(),
            file.path.clone(),
            area == Area::Staged,
            status_label_for(status_in_area(file, area)).to_string(),
            window,
            cx,
        );
    }

    fn on_commit_action(
        &mut self,
        _: &GitCommitMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit(window, cx);
    }
}

// ─── 渲染 ─────────────────────────────────────────────────────

/// 文案样式统一:`text-center text-[var(--text-muted)] text-sm py-6`。
fn placeholder_text(text: impl Into<SharedString>) -> AnyElement {
    div()
        .py(px(24.0))
        .w_full()
        .text_center()
        .text_size(ui::font_px(13.0))
        .text_color(ui::text_muted())
        .child(text.into())
        .into_any_element()
}

impl Render for GitChanges {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tree_mode = self.store.read(cx).git_changes_view_mode() == "tree";
        let staged = self.group_indices(Area::Staged);
        let staged_count = staged.len();
        let unstaged = self.group_indices(Area::Unstaged);
        let untracked = self.group_indices(Area::Untracked);
        let empty = self.changes.is_empty();

        // ① 文件列表(原先上方还有一条 刷新/视图切换 工具栏:刷新并入仓库栏的
        // ↻,视图切换上移到「更改」标题栏右侧,整条撤掉)
        let mut list = div()
            .id("git-changes-list")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .px(px(4.0))
            .pt(px(4.0));

        if self.loading && empty {
            list = list.child(placeholder_text(t("gitChanges", "loading")));
        } else if empty {
            list = list.child(placeholder_text(t("gitChanges", "empty")));
        } else {
            for (area, title, action_label) in [
                (
                    Area::Staged,
                    t("panels", "stagedChanges"),
                    t("gitChanges", "unstageAll"),
                ),
                (
                    Area::Unstaged,
                    t("panels", "unstagedChanges"),
                    t("gitChanges", "stageAll"),
                ),
                (
                    Area::Untracked,
                    t("panels", "untrackedFiles"),
                    t("gitChanges", "stageAll"),
                ),
            ] {
                let indices = match area {
                    Area::Staged => &staged,
                    Area::Unstaged => &unstaged,
                    Area::Untracked => &untracked,
                };
                if indices.is_empty() {
                    continue;
                }
                list = list.child(self.render_group(area, title, action_label, indices, tree_mode, cx));
            }
        }

        // ② 提交区
        let can_commit = staged_count > 0 && !self.committing;
        let commit_label = if self.committing {
            t("gitChanges", "committing").to_string()
        } else {
            tr!("panels", "commit", count = staged_count.to_string())
        };
        let commit_area = div()
            .flex_none()
            .border_t_1()
            .border_color(ui::border_subtle())
            .p(px(8.0))
            .child(Input::new(&self.commit_input))
            .child(
                div()
                    .id("git-commit-button")
                    .mt(px(6.0))
                    .py(px(6.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .text_size(ui::font_px(13.0))
                    .when(can_commit, |el| {
                        el.bg(ui::accent())
                            .text_color(gpui::white())
                            .cursor_pointer()
                            .hover(|el| el.opacity(0.9))
                    })
                    .when(!can_commit, |el| {
                        el.bg(ui::bg_elevated()).text_color(ui::text_muted())
                    })
                    .child(commit_label)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.commit(window, cx)
                    })),
            );

        div()
            .size_full()
            .flex()
            .flex_col()
            // Ctrl+Enter / Cmd+Enter 直接提交(键位绑在 main.rs,谓词
            // `"GitChanges > Input"` —— 与项目切换器的方向键同一套路)
            .key_context("GitChanges")
            .on_action(cx.listener(Self::on_commit_action))
            .child(list)
            .child(commit_area)
    }
}

impl GitChanges {
    fn render_group(
        &self,
        area: Area,
        title: &'static str,
        action_label: &'static str,
        indices: &[usize],
        tree_mode: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .px(px(8.0))
            .py(px(4.0))
            .child(
                div()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    // 原版 `「{title} ({count})」`,大写与字距靠 CSS,
                    // gpui 没有 text-transform,照文案原样显示
                    .child(format!("{title} ({})", indices.len())),
            )
            .child(
                div()
                    .id(SharedString::from(format!("git-group-action-{}", area.key())))
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .cursor_pointer()
                    .hover(|el| el.text_color(ui::text_primary()))
                    .child(action_label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        // ⚠️ 未跟踪组的按钮虽写着「全部暂存」,走的也是 git_stage_all
                        // (`add_all("*")` 会连未跟踪一起暂存)—— 原版行为
                        match area {
                            Area::Staged => this.unstage_all(cx),
                            _ => this.stage_all(cx),
                        }
                    })),
            );

        let mut group = div().child(header);
        if tree_mode {
            let paths: Vec<(usize, &str)> = indices
                .iter()
                .map(|&i| (i, self.changes[i].path.as_str()))
                .collect();
            let nodes = build_file_tree(&paths);
            let mut rows = Vec::new();
            flatten_tree(&nodes, area, &self.collapsed_dirs, 0, &mut rows);
            for row in rows {
                group = group.child(match row {
                    TreeRow::Dir {
                        name,
                        full_path,
                        depth,
                        collapsed,
                    } => self.render_dir_row(area, name, full_path, depth, collapsed, cx),
                    TreeRow::File { index, depth } => self.render_file_row(index, area, depth, cx),
                });
            }
        } else {
            for &index in indices {
                group = group.child(self.render_file_row(index, area, 0, cx));
            }
        }
        group.into_any_element()
    }

    fn render_dir_row(
        &self,
        area: Area,
        name: String,
        full_path: String,
        depth: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = format!("{}:{}", area.key(), full_path);
        div()
            .id(SharedString::from(format!("git-dir-{key}")))
            .flex()
            .items_center()
            .gap(px(4.0))
            .py(px(2.0))
            .pr(px(8.0))
            .pl(px(depth as f32 * 16.0 + 8.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .text_size(ui::font_px(13.0))
            .text_color(ui::text_muted())
            .hover(|el| el.bg(ui::border_subtle()))
            .child(
                div()
                    .w(px(12.0))
                    .text_center()
                    .child(if collapsed { "▸" } else { "▾" }),
            )
            .child(div().truncate().child(name))
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                if !this.collapsed_dirs.remove(&key) {
                    this.collapsed_dirs.insert(key.clone());
                }
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_file_row(
        &self,
        index: usize,
        area: Area,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(file) = self.changes.get(index) else {
            return div().into_any_element();
        };
        let status = status_in_area(file, area);
        let label = status_label_for(status);
        let color = status_color(status);
        let path = file.path.clone();
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        let display = if depth > 0 { name } else { path.clone() };
        let is_staged = area == Area::Staged;
        // 行 id 必须带区名前缀:同一文件可同时在 staged 与 unstaged 两组
        let row_id = SharedString::from(format!("git-file-{}-{}", area.key(), path));
        let menu_path = path.clone();

        div()
            .id(row_id)
            .group("git-file-row")
            .flex()
            .items_center()
            .justify_between()
            .py(px(4.0))
            .pr(px(8.0))
            .pl(px(depth as f32 * 16.0 + 8.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .text_size(ui::font_px(13.0))
            .hover(|el| el.bg(ui::border_subtle()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .flex_1()
                    .min_w(px(0.0))
                    .child(
                        div()
                            .flex_none()
                            .w(px(16.0))
                            .text_center()
                            .text_size(ui::font_px(11.0))
                            .text_color(color)
                            .child(label),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_color(ui::text_primary())
                            .child(display),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!(
                        "git-file-act-{}-{}",
                        area.key(),
                        path
                    )))
                    .w(px(20.0))
                    .h(px(20.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .flex_none()
                    .text_size(ui::font_px(13.0))
                    .text_color(ui::text_muted())
                    // 原版 `opacity-0 group-hover:opacity-100`
                    .opacity(0.0)
                    .group_hover("git-file-row", |el| el.opacity(1.0))
                    .hover(|el| el.text_color(ui::text_primary()))
                    .child(if is_staged { "−" } else { "+" })
                    .on_click({
                        let path = path.clone();
                        cx.listener(move |this, event: &ClickEvent, _window, cx| {
                            // 行内按钮:别把整行的「看 diff」也触发了
                            let _ = event;
                            cx.stop_propagation();
                            if is_staged {
                                this.unstage(path.clone(), cx);
                            } else {
                                this.stage(path.clone(), cx);
                            }
                        })
                    }),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.view_diff(index, area, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let entries = this.file_menu(index, area, menu_path.clone(), cx);
                    menu::show(event.position, entries, window, cx);
                }),
            )
            .into_any_element()
    }

    /// 文件行右键菜单(`GitChanges.tsx:242-256`)。
    fn file_menu(
        &self,
        index: usize,
        area: Area,
        path: String,
        cx: &mut Context<Self>,
    ) -> Vec<menu::MenuEntry> {
        let this = cx.entity();
        let mut entries = vec![
            {
                let this = this.clone();
                menu::item(t("gitChanges", "contextViewDiff"), move |window, cx| {
                    this.update(cx, |this, cx| this.view_diff(index, area, window, cx));
                })
            },
            menu::separator(),
        ];
        entries.push(if area == Area::Staged {
            let (this, path) = (this.clone(), path.clone());
            menu::item(t("panels", "unstage"), move |_window, cx| {
                this.update(cx, |this, cx| this.unstage(path.clone(), cx));
            })
        } else {
            let (this, path) = (this.clone(), path.clone());
            menu::item(t("panels", "stage"), move |_window, cx| {
                this.update(cx, |this, cx| this.stage(path.clone(), cx));
            })
        });
        if area != Area::Staged {
            entries.push(menu::separator());
            entries.push(menu::item(
                t("gitChanges", "contextDiscard"),
                move |window, cx| {
                    let path = path.clone();
                    this.update(cx, |this, cx| this.discard(vec![path], window, cx));
                },
            ));
        }
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(
        path: &str,
        staged: Option<GitStatus>,
        unstaged: Option<GitStatus>,
    ) -> ChangeFileStatus {
        ChangeFileStatus {
            path: path.to_string(),
            old_path: None,
            staged_status: staged,
            unstaged_status: unstaged,
            status_label: String::new(),
        }
    }

    fn group(changes: &[ChangeFileStatus], area: Area) -> Vec<&str> {
        changes
            .iter()
            .filter(|c| match area {
                Area::Staged => c.staged_status.is_some(),
                Area::Unstaged => {
                    matches!(c.unstaged_status, Some(ref s) if *s != GitStatus::Untracked)
                }
                Area::Untracked => c.unstaged_status == Some(GitStatus::Untracked),
            })
            .map(|c| c.path.as_str())
            .collect()
    }

    /// 三分组口径:未跟踪不落进「未暂存」,而**同一个文件可以同时进 staged 与
    /// unstaged**(部分暂存)。
    #[test]
    fn 三分组口径含部分暂存() {
        let changes = [
            change("a.rs", Some(GitStatus::Added), None),
            // 部分暂存:两组都要出现
            change(
                "b.rs",
                Some(GitStatus::Modified),
                Some(GitStatus::Modified),
            ),
            change("c.rs", None, Some(GitStatus::Modified)),
            change("d.rs", None, Some(GitStatus::Untracked)),
        ];
        assert_eq!(group(&changes, Area::Staged), vec!["a.rs", "b.rs"]);
        assert_eq!(group(&changes, Area::Unstaged), vec!["b.rs", "c.rs"]);
        assert_eq!(group(&changes, Area::Untracked), vec!["d.rs"]);
    }

    /// 取哪一个 status 由所在区决定 —— 部分暂存的文件在两组里显示的字母不同。
    #[test]
    fn 状态按区取值() {
        let file = change("b.rs", Some(GitStatus::Added), Some(GitStatus::Deleted));
        assert_eq!(status_label_for(status_in_area(&file, Area::Staged)), "A");
        assert_eq!(status_label_for(status_in_area(&file, Area::Unstaged)), "D");
        assert_eq!(
            status_label_for(status_in_area(&file, Area::Untracked)),
            "D",
            "untracked 区同样取 unstagedStatus"
        );
    }

    /// 六种状态 → 单字符。认不出(None)的是**空格**,不是空串。
    #[test]
    fn 状态字母表() {
        use GitStatus::*;
        let table = [
            (Modified, "M"),
            (Added, "A"),
            (Deleted, "D"),
            (Renamed, "R"),
            (Untracked, "?"),
            (Conflicted, "C"),
        ];
        for (status, label) in table {
            assert_eq!(status_label_for(Some(&status)), label);
        }
        assert_eq!(status_label_for(None), " ");
    }

    /// 状态色:`conflicted` **落到 muted**(原版 default 分支),不是错误色。
    #[test]
    fn 状态色映射() {
        use GitStatus::*;
        ui::set_palette(ui::Palette::dark());
        assert_eq!(status_color(Some(&Modified)), ui::color_warning());
        assert_eq!(status_color(Some(&Added)), ui::color_success());
        assert_eq!(status_color(Some(&Deleted)), ui::color_error());
        assert_eq!(status_color(Some(&Renamed)), ui::color_info());
        assert_eq!(status_color(Some(&Untracked)), ui::color_success());
        assert_eq!(
            status_color(Some(&Conflicted)),
            ui::text_muted(),
            "conflicted 走 default 分支 —— 原版如此"
        );
        assert_eq!(status_color(None), ui::text_muted());
    }

    /// 建树:嵌套路径共用目录节点,**不做单链压缩**。
    #[test]
    fn 建树共用目录节点() {
        let paths = [(0, "src/a.rs"), (1, "src/b.rs"), (2, "README.md")];
        let tree = build_file_tree(&paths);
        assert_eq!(tree.len(), 2, "src 目录 + README.md");
        assert_eq!(tree[0].name, "src");
        assert_eq!(tree[0].full_path, "src");
        assert!(tree[0].file.is_none());
        assert_eq!(tree[0].children.len(), 2);
        assert_eq!(tree[0].children[0].full_path, "src/a.rs");
        assert_eq!(tree[0].children[0].file, Some(0));
        assert_eq!(tree[1].name, "README.md");
        assert_eq!(tree[1].file, Some(2));

        // 深嵌套不压缩:a/b/c/d.rs 是三层目录 + 一个文件
        let deep = build_file_tree(&[(0, "a/b/c/d.rs")]);
        assert_eq!(deep[0].name, "a");
        assert_eq!(deep[0].children[0].name, "b");
        assert_eq!(deep[0].children[0].children[0].name, "c");
        assert_eq!(deep[0].children[0].children[0].children[0].name, "d.rs");
    }

    /// 同名的目录与文件互不合并(`find` 带 `&& !n.file` 那道判定)。
    #[test]
    fn 同名目录与文件不合并() {
        let tree = build_file_tree(&[(0, "build"), (1, "build/out.js")]);
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].file, Some(0), "先来的是文件");
        assert!(tree[1].file.is_none(), "后来的目录另起一个节点");
        assert_eq!(tree[1].children[0].full_path, "build/out.js");
    }

    /// 折叠 key 带区名:同一路径在三个区各自独立折叠。
    #[test]
    fn 折叠_key_按区隔离() {
        let tree = build_file_tree(&[(0, "src/a.rs")]);
        let mut collapsed = HashSet::new();
        collapsed.insert("staged:src".to_string());

        let mut rows = Vec::new();
        flatten_tree(&tree, Area::Staged, &collapsed, 0, &mut rows);
        assert_eq!(rows.len(), 1, "staged 区的 src 折起来了,只剩目录行");

        let mut rows = Vec::new();
        flatten_tree(&tree, Area::Unstaged, &collapsed, 0, &mut rows);
        assert_eq!(rows.len(), 2, "unstaged 区不受影响");
        assert!(matches!(rows[1], TreeRow::File { depth: 1, .. }));
    }
}
