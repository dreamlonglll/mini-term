//! diff 页签的三种正文:单文件 diff([`FilePage`])、提交 / 合并变更([`FilesPage`])、
//! 磁盘对比([`TextPage`])。单文件历史那一种在 `git_file_history`(左栏是提交列表)。
//!
//! 每页的工具栏与正文就是原先弹窗 builder 里那一套,外框换成铺满页签区:没有弹窗尺寸
//! (`modal_size`)、没有 ✕(关页签走页签条上的 × / 中键 / Ctrl+W),工具栏最右端换成「刷新」
//! (见 [`ToolbarTail`])。焦点容器与 key context 挂在每页的根上(页签把焦点句柄交给它),
//! F7 / Shift+F7 / Alt+←/→ / ↑/↓ / Ctrl+C/A 与弹窗时一样;Esc 只清行勾选,没有勾选就放行。

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Div, Entity, FocusHandle, InteractiveElement,
    IntoElement, ParentElement, ScrollStrategy, SharedString, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, WeakEntity, div, prelude::FluentBuilder as _, px, uniform_list,
};
use mt_ui::tooltip::TooltipExt as _;

use super::{
    COMMIT_DIFF_CONTEXT, COMMIT_LIST_W, CommitPick, DiffFile, DiffSide, DiffState,
    FILE_DIFF_CONTEXT, Fetch, FileDiff, FileDiffHooks, GitCommitDiffNextFile,
    GitCommitDiffPrevFile, GitDiffClearPicks, GitDiffNextChange, GitDiffNextFile,
    GitDiffPrevChange, GitDiffPrevFile, HunkUi, ModalFile, ModalHeading, SIDE_PAD, TOOLBAR_H,
    ToolbarLabels, ToolbarTail, choose_modal_file, empty_text, error_bar, file_heading, file_nav,
    file_row_of, load_diff_with, load_file, load_modal_file, modal_row, non_utf8_hunk_reason,
    picks_toolbar, refresh_unchanged, render_body, selection, step_modal_file, switch_file,
    title_block, view_toggle,
};
use crate::file_viewer::{FileViewer, read_compare_disk};
use crate::git_ui::{self, ChangeTreeRow, ICON_CLEAN, mono};
use crate::i18n::{t, tr};
use crate::store::AppStore;
use crate::ui;

/// 页签顶部的工具栏行(高 [`TOOLBAR_H`]、左右 16、底边一条细线):左半抬头、右半控件。
fn toolbar_row() -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.0))
        .h(px(TOOLBAR_H))
        .px(px(16.0))
        .flex_none()
        .border_b_1()
        .border_color(ui::border_subtle())
}

/// 正文外框:吃满剩下的高度;底色是不透明的 `bg_base` —— 钉在左缘的行号列靠同一种底色
/// 盖住从它底下滚过去的字(见 `pinned`)。
fn body_frame(body: AnyElement) -> Div {
    div()
        .flex_1()
        .min_h(px(0.0))
        .overflow_hidden()
        .bg(ui::bg_base())
        .child(body)
}

/// 单文件 diff 与磁盘对比工具栏右端那组的文案(两者同一套)。
fn file_labels() -> ToolbarLabels {
    ToolbarLabels {
        side: t("diffModal", "sideBySide"),
        inline: t("diffModal", "inline"),
        prev: t("gitCommit", "prevChange"),
        next: t("gitCommit", "nextChange"),
    }
}

// ─── 单文件 diff ──────────────────────────────────────────────

/// 单文件 diff 页签的正文:diff 内容 + 控制面(文件序列、块操作、就地错误)。
pub(super) struct FilePage {
    pub(super) state: Entity<DiffState>,
    pub(super) ctl: Entity<FileDiff>,
}

impl FilePage {
    pub(super) fn new(
        store: &Entity<AppStore>,
        repo_path: String,
        files: Vec<DiffFile>,
        current: usize,
        hooks: FileDiffHooks,
        cx: &mut App,
    ) -> Self {
        let font_size = store.read(cx).config().terminal_font_size as f32;
        let state = cx.new(|_| DiffState::new(font_size));
        let ctl = cx.new(|_| FileDiff {
            repo_path,
            files,
            current,
            on_changed: hooks.on_changed,
            on_switch: hooks.on_switch,
            reload_files: hooks.reload_files,
            busy: None,
            op_error: None,
            stale: false,
        });
        // 取数:`(repo, path, side)` 三元组一次到位(原版漏了 staged,见模块注释)
        load_file(&state, &ctl, Fetch::Fresh, cx);
        Self { state, ctl }
    }

    /// 当前文件(页签的身份与标题按它算)与所在仓库。
    pub(super) fn current(&self, cx: &App) -> (String, DiffFile) {
        let c = self.ctl.read(cx);
        (c.repo_path.clone(), c.file().clone())
    }

    /// 已开着的页签被再次打开(更改列表里又双击了同一个文件):换上新的仓库、文件序列与回调
    /// (列表眼下的分组顺序)。仓库与当前文件都没变就原地静默重取,任一变了就从头取。
    ///
    /// 仓库也要换:身份按「仓库 + 路径」拼出的绝对路径认,文件树传「项目根 + 相对项目根的
    /// 路径」、文件页 gutter 传「仓库根 + 仓库相对路径」,项目根是仓库的子目录(或项目里有
    /// 嵌套的子仓库)时两者撞到同一个页签上 —— 只换路径的话,拿旧仓库去取新路径,不是报错
    /// 就是显示成另一个文件。块操作在途时不换 —— 序列与回调留到它收尾再说。
    pub(super) fn adopt(
        &self,
        repo_path: String,
        files: Vec<DiffFile>,
        current: usize,
        hooks: FileDiffHooks,
        cx: &mut App,
    ) {
        let adopted = self.ctl.update(cx, |c, cx| {
            if c.busy.is_some() || files.is_empty() {
                return None;
            }
            let same_repo = c.repo_path == repo_path;
            let before = c.file().path.clone();
            c.repo_path = repo_path;
            c.current = current.min(files.len() - 1);
            c.files = files;
            c.on_changed = hooks.on_changed;
            c.on_switch = hooks.on_switch;
            c.reload_files = hooks.reload_files;
            c.op_error = None;
            c.stale = false;
            cx.notify();
            Some(same_repo && c.file().path == before)
        });
        match adopted {
            Some(true) => self.refresh(cx),
            // 换了仓库,或身份按规范化路径认、大小写不同的两种写法落到这里:按换了文件处理
            Some(false) => load_file(&self.state, &self.ctl, Fetch::Fresh, cx),
            None => {}
        }
    }

    /// 静默重取(见 [`Fetch::Refresh`])。块操作在途时不取:那一趟收尾时自己会原地重取。
    pub(super) fn refresh(&self, cx: &mut App) {
        if self.ctl.read(cx).busy.is_some() {
            return;
        }
        load_file(&self.state, &self.ctl, Fetch::Refresh, cx);
    }

    pub(super) fn render(&self, focus: &FocusHandle, refresh: AnyElement, cx: &mut App) -> Div {
        let (state, ctl) = (&self.state, &self.ctl);
        let (file, current, total, busy, op_error, stale) = {
            let c = ctl.read(cx);
            (
                c.file().clone(),
                c.current,
                c.files.len(),
                c.busy,
                c.op_error.clone(),
                c.stale,
            )
        };
        let blocked = state
            .read(cx)
            .result
            .as_ref()
            .and_then(|result| result.encoding)
            .map(non_utf8_hunk_reason);
        let hunk_ui = (file.hunk_actions && file.side != DiffSide::Working).then(|| HunkUi {
            state: state.clone(),
            ctl: ctl.clone(),
            side: file.side,
            busy,
            blocked,
        });
        let body = render_body(
            state,
            t("diffModal", "loading"),
            t("diffModal", "binaryNotSupported"),
            t("diffModal", "tooLarge"),
            Some(empty_text(&file)),
            hunk_ui,
            cx,
        );
        let toolbar = toolbar_row().child(title_block(&file, state, cx)).child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .children(picks_toolbar(state, ctl, file.side, busy, cx))
                .children(file_nav(state, ctl, current, total))
                .child(view_toggle(
                    state,
                    "git-diff",
                    file_labels(),
                    ToolbarTail::Refresh(refresh),
                    cx,
                )),
        );

        let (next_change, prev_change) = (state.clone(), state.clone());
        let (next_file, prev_file) = ((state.clone(), ctl.clone()), (state.clone(), ctl.clone()));
        let clear_picks = state.clone();
        // F7 / Shift+F7 / Alt+← / Alt+→ 与正文的 Ctrl/Cmd+C / A 挂在这里;Esc 先清行勾选,
        // 没有勾选时放行(页签里没有弹窗可关,按键照常往外走,焦点不动)
        selection::text_actions(div(), state)
            .track_focus(focus)
            .key_context(FILE_DIFF_CONTEXT)
            .on_action(move |_: &GitDiffClearPicks, _window, cx| {
                let cleared = clear_picks.update(cx, |s, cx| {
                    let cleared = s.clear_picks();
                    if cleared {
                        cx.notify();
                    }
                    cleared
                });
                if !cleared {
                    cx.propagate();
                }
            })
            .on_action(move |_: &GitDiffNextChange, _window, cx| {
                next_change.update(cx, |s, cx| {
                    s.jump_by(1);
                    cx.notify();
                });
            })
            .on_action(move |_: &GitDiffPrevChange, _window, cx| {
                prev_change.update(cx, |s, cx| {
                    s.jump_by(-1);
                    cx.notify();
                });
            })
            .on_action(move |_: &GitDiffNextFile, _window, cx| {
                switch_file(&next_file.0, &next_file.1, 1, cx);
            })
            .on_action(move |_: &GitDiffPrevFile, _window, cx| {
                switch_file(&prev_file.0, &prev_file.1, -1, cx);
            })
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(toolbar)
            .children(op_error.map(|err| error_bar(state, ctl, err, stale)))
            .child(body_frame(body))
    }
}

// ─── 提交 diff / 合并变更 ─────────────────────────────────────

/// 提交 diff / 合并变更页签的正文:左栏文件树 / 列表,右栏选中文件的 diff。
pub(super) struct FilesPage {
    state: Entity<DiffState>,
    pick: Entity<CommitPick>,
    files: Rc<Vec<ModalFile>>,
    repo: SharedString,
    heading: ModalHeading,
    /// 左栏的滚动(先选中的文件可能排在很后面,要滚到它那一行)
    file_scroll: UniformListScrollHandle,
    store: Entity<AppStore>,
}

impl FilesPage {
    /// 先选中路径为 `initial` 的文件,`None` 或找不到时选第一个。
    pub(super) fn new(
        store: Entity<AppStore>,
        repo_path: String,
        heading: ModalHeading,
        files: Vec<ModalFile>,
        initial: Option<String>,
        cx: &mut App,
    ) -> Self {
        let font_size = store.read(cx).config().terminal_font_size as f32;
        let tree = store.read(cx).git_commit_files_tree();
        let state = cx.new(|_| DiffState::new(font_size));
        let initial_ix = initial
            .as_deref()
            .and_then(|p| files.iter().position(|f| f.path == p));
        let first = initial_ix.unwrap_or(0);
        // 选中的文件可能排在很后面:左栏滚到它那一行(下一次布局时生效)。树形里行序与文件
        // 序不同,按拍平后的行找
        let file_scroll = UniformListScrollHandle::new();
        if let Some(row) = initial_ix.and_then(|ix| file_row_of(&files, tree, ix)) {
            file_scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        }
        let pick = cx.new(|_| CommitPick {
            selected: first,
            tree,
            collapsed: HashSet::new(),
            rows: None,
        });
        match files.get(first) {
            Some(file) => load_modal_file(&state, &repo_path, file, Fetch::Fresh, cx),
            None => state.update(cx, |s, _| s.loading = false),
        }
        Self {
            state,
            pick,
            files: Rc::new(files),
            repo: SharedString::from(repo_path),
            heading,
            file_scroll,
            store,
        }
    }

    /// 已开着的页签被再次打开、带着初始文件:在左栏选中它(看得见就滚过去),右栏换成它的 diff。
    pub(super) fn select_path(&self, path: &str, cx: &mut App) {
        let Some(index) = self.files.iter().position(|f| f.path == path) else {
            return;
        };
        let rows = self.pick.update(cx, |p, _| p.rows(&self.files));
        let row = rows
            .iter()
            .position(|r| matches!(r, ChangeTreeRow::File { index: i, .. } if *i == index));
        if let Some(row) = row {
            self.file_scroll
                .scroll_to_item(row, ScrollStrategy::Nearest);
        }
        choose_modal_file(&self.pick, &self.state, &self.files, &self.repo, index, cx);
    }

    /// 刷新:原地重取右栏这份(提交的内容不会变,刷新只为出错之后重试一次;内容没变什么都不动)。
    pub(super) fn refresh(&self, cx: &mut App) {
        let selected = self.pick.read(cx).selected;
        if let Some(file) = self.files.get(selected) {
            load_modal_file(&self.state, &self.repo, file, Fetch::Refresh, cx);
        }
    }

    pub(super) fn render(&self, focus: &FocusHandle, refresh: AnyElement, cx: &mut App) -> Div {
        let (selected, tree) = {
            let p = self.pick.read(cx);
            (p.selected, p.tree)
        };
        let rows = self.pick.update(cx, |p, _| p.rows(&self.files));
        let files = &self.files;
        let heading = &self.heading;

        // 左栏文件列表:与提交详情同一种文件行(FileIcon + 状态色文件名 + 弱化目录
        // + 状态字母;树形按目录分组、单链目录压缩),选中 = accent 淡底 + 左侧竖条。
        // 行等高,走 uniform_list:只给可视区里的那几行建元素
        let file_list = {
            let (files, pick, state, repo) = (
                files.clone(),
                self.pick.clone(),
                self.state.clone(),
                self.repo.clone(),
            );
            div().flex_1().min_h(px(0.0)).child(
                uniform_list(
                    "git-commit-files",
                    rows.len(),
                    move |range, _window, _cx: &mut App| {
                        range
                            .map(|ix| {
                                rows.get(ix)
                                    .and_then(|row| {
                                        modal_row(row, &files, &pick, &state, &repo, selected, tree)
                                    })
                                    // 数不上的行(不会出现)占住一行高,别把后面的行挤乱
                                    .unwrap_or_else(|| {
                                        div().h(px(git_ui::ROW_H)).into_any_element()
                                    })
                            })
                            .collect::<Vec<_>>()
                    },
                )
                .size_full()
                .track_scroll(&self.file_scroll)
                .p(px(4.0)),
            )
        };

        // 左栏头部:标题(与单文件历史左栏头部同一档:14px 半粗)+ 等宽副标题,
        // 副标题那一行右侧是「列表 / 树形」切换
        let toggle = {
            let (pick, store) = (self.pick.clone(), self.store.clone());
            git_ui::view_mode_toggle("git-commit-files-view-mode", tree).on_click(
                move |_: &ClickEvent, _window, cx| {
                    let next = !pick.read(cx).tree;
                    pick.update(cx, |p, cx| {
                        p.set_tree(next);
                        cx.notify();
                    });
                    store.update(cx, |s, cx| s.set_git_commit_files_tree(next, cx));
                },
            )
        };
        let subtitle = mono(div())
            .id("git-commit-diff-subtitle")
            .flex_1()
            .min_w_0()
            .truncate()
            .text_size(ui::font_px(12.0))
            .text_color(ui::text_muted())
            .when_some(heading.subtitle_tip, |el, tip| el.tip(tip))
            .child(heading.subtitle.clone());
        let left = div()
            .w(px(COMMIT_LIST_W))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(ui::border_subtle())
            .bg(ui::bg_elevated())
            .child(
                div()
                    .px(px(SIDE_PAD))
                    .py(px(10.0))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .border_b_1()
                    .border_color(ui::border_subtle())
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(ui::font_px(14.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(ui::text_primary())
                            .child(heading.title.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(subtitle)
                            .when(!files.is_empty(), |el| el.child(toggle)),
                    ),
            )
            .child(file_list)
            .child(
                div()
                    .px(px(SIDE_PAD))
                    .py(px(8.0))
                    .flex_none()
                    .border_t_1()
                    .border_color(ui::border_subtle())
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .child(tr!(
                        "commitDiff",
                        "fileCount",
                        count = files.len().to_string()
                    )),
            );

        // 右栏
        let selected_file = files.get(selected);
        let selected_path = selected_file.map(|f| f.path.clone()).unwrap_or_default();
        let selected_status = selected_file.map(|f| git_ui::commit_file_status(&f.status));
        // 纯重命名的文件没有改动块:与单文件 diff 同一句说明,别留一片空白
        let rename_only = selected_file
            .filter(|f| f.status == "renamed")
            .map(|_| t("gitCommit", "renameOnly"));
        let body = if files.is_empty() {
            git_ui::empty_state(ICON_CLEAN, heading.empty, None)
                .size_full()
                .into_any_element()
        } else {
            render_body(
                &self.state,
                t("commitDiff", "loading"),
                t("commitDiff", "binaryFile"),
                t("commitDiff", "tooLarge"),
                rename_only,
                None,
                cx,
            )
        };
        let right = div()
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .flex()
            .flex_col()
            .child(
                toolbar_row()
                    .child(file_heading(&selected_path, selected_status))
                    .child(view_toggle(
                        &self.state,
                        "git-commit-diff",
                        ToolbarLabels {
                            side: t("commitDiff", "sideBySide"),
                            inline: t("commitDiff", "inline"),
                            prev: t("commitDiff", "prevChange"),
                            next: t("commitDiff", "nextChange"),
                        },
                        ToolbarTail::Refresh(refresh),
                        cx,
                    )),
            )
            .child(body_frame(body));

        // 左栏换文件:↑ / ↓ 挂在整页的焦点容器上(焦点就这一个,正文的拖选 / 滚动不占方向键),
        // 右栏跟着换、选中行滚到看得见
        let step = {
            let (pick, state, files, repo, scroll) = (
                self.pick.clone(),
                self.state.clone(),
                files.clone(),
                self.repo.clone(),
                self.file_scroll.clone(),
            );
            Rc::new(move |delta: isize, cx: &mut App| {
                step_modal_file(&pick, &state, &files, &repo, &scroll, delta, cx);
            })
        };
        let (step_next, step_prev) = (step.clone(), step);
        // 左栏的 ↑ / ↓ 与正文的 Ctrl/Cmd+C / A 挂在这里;Esc 没有绑定,照常往外走
        selection::text_actions(div(), &self.state)
            .track_focus(focus)
            .key_context(COMMIT_DIFF_CONTEXT)
            .on_action(move |_: &GitCommitDiffNextFile, _window, cx| step_next(1, cx))
            .on_action(move |_: &GitCommitDiffPrevFile, _window, cx| step_prev(-1, cx))
            .flex()
            .overflow_hidden()
            .child(left)
            .child(right)
    }
}

// ─── 磁盘对比 ─────────────────────────────────────────────────

/// 磁盘对比页签的正文:左 = 磁盘上的版本,右 = 编辑器里未保存的修改(文件页外部改动提示条
/// 的「对比」)。只读:没有块操作、不换文件。
pub(super) struct TextPage {
    state: Entity<DiffState>,
    /// 工具栏抬头(「对比:a.rs」)
    heading: String,
    /// 那个文件页签:刷新时现取它编辑器里的内容与磁盘上的版本。文件页签关了就停在最后一次
    /// 取到的那份;再点一次「对比」时换成发起的那个(见 [`Self::set_texts`])
    viewer: WeakEntity<FileViewer>,
}

impl TextPage {
    pub(super) fn new(
        store: &Entity<AppStore>,
        name: &str,
        viewer: WeakEntity<FileViewer>,
        disk: String,
        mine: String,
        cx: &mut App,
    ) -> Self {
        let font_size = store.read(cx).config().terminal_font_size as f32;
        let state = cx.new(|_| DiffState::new(font_size));
        state.update(cx, |s, _| {
            s.apply(Ok(mt_project::git::diff_two_texts(disk, mine)));
        });
        Self {
            state,
            heading: tr!("fileViewer", "compareTitle", name = name),
            viewer,
        }
    }

    /// 已开着的页签被再次打开(提示条上又点了「对比」):换上这一次取到的两侧内容(比在途的
    /// 静默重取新,它回来也落不了地)与发起的那个文件页签 —— 原先那个可能已经关掉、文件
    /// 重开过,旧的弱引用升不起来,之后切回来 / 刷新就都静默不动了。内容与屏幕上一样就不重画。
    pub(super) fn set_texts(
        &mut self,
        viewer: WeakEntity<FileViewer>,
        disk: String,
        mine: String,
        cx: &mut App,
    ) {
        self.viewer = viewer;
        let result = Ok(mt_project::git::diff_two_texts(disk, mine));
        self.state.update(cx, |s, cx| {
            s.request += 1;
            s.refreshing = false;
            if refresh_unchanged(s.result.as_ref(), s.error.as_deref(), &result) {
                return;
            }
            s.apply(result);
            cx.notify();
        });
    }

    /// 静默重取:现取文件页签编辑器里的内容,后台重读磁盘(见 [`Fetch::Refresh`])。文件页签
    /// 已经关了就不动(编辑器里的内容随它没了)。
    pub(super) fn refresh(&self, cx: &mut App) {
        let Some(viewer) = self.viewer.upgrade() else {
            return;
        };
        let Some(input) = viewer.read(cx).disk_compare_input(cx) else {
            return;
        };
        load_diff_with(&self.state, Fetch::Refresh, cx, move || {
            let disk =
                read_compare_disk(&input.project_root, &input.path).map_err(anyhow::Error::msg)?;
            Ok(mt_project::git::diff_two_texts(disk, input.mine))
        });
    }

    pub(super) fn render(&self, focus: &FocusHandle, refresh: AnyElement, cx: &mut App) -> Div {
        let state = &self.state;
        let body = render_body(
            state,
            t("diffModal", "loading"),
            t("diffModal", "binaryNotSupported"),
            t("diffModal", "tooLarge"),
            Some(t("fileViewer", "compareIdentical")),
            None,
            cx,
        );
        let sides = format!(
            "{} → {}",
            t("fileViewer", "compareSideDisk"),
            t("fileViewer", "compareSideUnsaved")
        );
        let toolbar = toolbar_row()
            .child(
                file_heading(&self.heading, None).child(
                    div()
                        .flex_none()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_muted())
                        .child(sides),
                ),
            )
            .child(div().flex().flex_none().items_center().child(view_toggle(
                state,
                "text-diff",
                file_labels(),
                ToolbarTail::Refresh(refresh),
                cx,
            )));
        let (next_change, prev_change) = (state.clone(), state.clone());
        selection::text_actions(div(), state)
            .track_focus(focus)
            .key_context(FILE_DIFF_CONTEXT)
            .on_action(move |_: &GitDiffNextChange, _window, cx| {
                next_change.update(cx, |s, cx| {
                    s.jump_by(1);
                    cx.notify();
                });
            })
            .on_action(move |_: &GitDiffPrevChange, _window, cx| {
                prev_change.update(cx, |s, cx| {
                    s.jump_by(-1);
                    cx.notify();
                });
            })
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(toolbar)
            .child(body_frame(body))
    }
}
