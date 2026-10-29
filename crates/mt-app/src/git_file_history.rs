//! 单文件历史弹窗(对标 IDEA 的 Show History):左栏是改动过这个文件的提交,
//! 右栏是所选提交里这个文件的 diff。
//!
//! - 入口:文件树右键「Git 历史」;对外只有 [`open`],别处(变更列表、文件查看器)
//!   要接入时直接调它;
//! - 数据:[`mt_project::git::get_file_log`](判定 = 相对第一个父提交改动了该路径,
//!   **不跟随重命名**),分页游标由后端交回;后端每批最多扫 `SCAN_BUDGET` 个提交,
//!   没凑满一页就接着要;
//! - 右栏复用 `git_diff` 的渲染([`git_diff::CommitFileDiffPane`]),并排 / 行内、
//!   改动跳转与提交 diff 弹窗完全一致;
//! - 键盘:打开即选中第一条;↑/↓ 换提交,右栏跟着换;Esc 关闭(Dialog 自带)。
//!
//! 尺寸与提交 diff 弹窗同一口径(`git_diff::modal_size`)。

use std::path::PathBuf;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, FocusHandle,
    FontWeight, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    Render, ScrollStrategy, SharedString, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, Window, div, px, uniform_list,
};
use mt_project::git::{FileLogEntry, GitLogCursor};
use mt_ui::tooltip::TooltipExt as _;

use crate::git_diff::{self, CommitFileDiffPane};
use crate::git_history::icons::{ICON_HISTORY, empty_state, file_status, mono};
use crate::git_history::{format_absolute_time, format_relative_time};
use crate::i18n::{t, tr};
use crate::menu;
use crate::prompt::{is_open, kind, open_guarded};
use crate::store::AppStore;
use crate::ui;

gpui::actions!(
    mini_term,
    [
        /// 单文件历史:选中上一条提交(↑)
        FileHistoryPrev,
        /// 单文件历史:选中下一条提交(↓)
        FileHistoryNext,
    ]
);

/// 每页条数。弹窗里一屏能放二十来条,一页给够两屏。
const PAGE_SIZE: usize = 50;
/// 左栏行高(两行布局)。uniform_list 要求等高。
const ROW_HEIGHT: f32 = 52.0;
/// 左栏宽度。
const LIST_WIDTH: f32 = 320.0;

struct FileHistory {
    store: Entity<AppStore>,
    project_root: PathBuf,
    file_rel: String,
    /// 后端换算出来的「仓库根 + 仓库内路径」(取 diff 用)。首页回来之前是 `None`。
    repo_root: Option<String>,
    path_in_repo: Option<String>,
    entries: Vec<FileLogEntry>,
    cursor: Option<GitLogCursor>,
    /// 首页请求过没有(区分「还没开始」与「已经到底」)。
    started: bool,
    loading: bool,
    error: Option<String>,
    scanned: usize,
    /// 想要凑满的条数:首屏一页,触底加一页。
    want: usize,
    selected: Option<usize>,
    diff: CommitFileDiffPane,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
}

impl Render for FileHistory {
    /// 只当状态盒子用(Dialog 的 builder 每帧重跑)。
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 打开某个文件的 Git 历史。`project_root` 可以是仓库里的子目录,`file_rel` 是
/// 相对它的路径(`/` 或 `\` 分隔都行);所属仓库由后端就近发现。
pub fn open(
    store: Entity<AppStore>,
    project_root: PathBuf,
    file_rel: String,
    window: &mut Window,
    cx: &mut App,
) {
    if is_open(kind::GIT_FILE_HISTORY) {
        return;
    }
    let diff = CommitFileDiffPane::new(&store, cx);
    let file_rel = file_rel.replace('\\', "/");
    let state = cx.new(|cx| FileHistory {
        store,
        project_root,
        file_rel: file_rel.clone(),
        repo_root: None,
        path_in_repo: None,
        entries: Vec::new(),
        cursor: None,
        started: false,
        loading: false,
        error: None,
        scanned: 0,
        want: PAGE_SIZE,
        selected: None,
        diff,
        scroll: UniformListScrollHandle::new(),
        focus: cx.focus_handle(),
    });
    load_page(&state, cx);

    let file_name = file_rel.rsplit('/').next().unwrap_or(&file_rel).to_string();
    let focus = state.read(cx).focus.clone();

    open_guarded(
        kind::GIT_FILE_HISTORY,
        window,
        cx,
        move |dialog, window, cx| {
            let viewport = window.viewport_size();
            let (width, body_h) = git_diff::modal_size(viewport);
            let (prev, next) = (state.clone(), state.clone());
            let focus = state.read(cx).focus.clone();
            dialog.w(width).p_0().close_button(false).child(
                div()
                    .w_full()
                    .h(body_h)
                    .flex()
                    .overflow_hidden()
                    .key_context("GitFileHistory")
                    .track_focus(&focus)
                    .on_action(move |_: &FileHistoryPrev, _window, cx| {
                        move_selection(&prev, -1, cx)
                    })
                    .on_action(move |_: &FileHistoryNext, _window, cx| move_selection(&next, 1, cx))
                    .child(render_list_column(&state, &file_name, cx))
                    .child(render_diff_column(&state, cx)),
            )
        },
    );
    // 方向键要落在弹窗里:等弹窗抢完焦点再交给列表(同 `prompt::autofocus`)
    window.defer(cx, move |window, cx| window.focus(&focus, cx));
}

/// 要下一页(首页 / 触底 / 没凑满自动续)。到底或在途时什么都不做。
fn load_page(state: &Entity<FileHistory>, cx: &mut App) {
    let Some((root, rel, after)) = state.update(cx, |s, _| {
        if s.loading || (s.started && s.cursor.is_none()) {
            return None;
        }
        s.loading = true;
        s.started = true;
        Some((s.project_root.clone(), s.file_rel.clone(), s.cursor.take()))
    }) else {
        return;
    };
    let state = state.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move { mt_project::git::get_file_log(&root, &rel, after, PAGE_SIZE) })
            .await;
        cx.update(|cx| {
            let need_more = state.update(cx, |s, cx| {
                s.loading = false;
                match result {
                    Ok(page) => {
                        s.repo_root = Some(page.repo_root.to_string_lossy().to_string());
                        s.path_in_repo = Some(page.path_in_repo);
                        s.scanned += page.scanned;
                        for entry in page.entries {
                            if !s.entries.iter().any(|e| e.commit.hash == entry.commit.hash) {
                                s.entries.push(entry);
                            }
                        }
                        s.cursor = page.next;
                        s.error = None;
                    }
                    Err(err) => {
                        eprintln!("[git] 取文件历史失败: {err:#}");
                        s.error = Some(format!("{err:#}"));
                    }
                }
                cx.notify();
                s.cursor.is_some() && s.entries.len() < s.want
            });
            // 打开即选中第一条
            if state.read(cx).selected.is_none() && !state.read(cx).entries.is_empty() {
                select(&state, 0, cx);
            }
            if need_more {
                load_page(&state, cx);
            }
        });
    })
    .detach();
}

/// 触底:多要一页。
fn load_more(state: &Entity<FileHistory>, cx: &mut App) {
    let go = state.update(cx, |s, _| {
        if s.loading || s.cursor.is_none() {
            return false;
        }
        s.want = s.entries.len() + PAGE_SIZE;
        true
    });
    if go {
        load_page(state, cx);
    }
}

/// 选中第 `index` 条:右栏换成它的 diff,列表滚到看得见。
fn select(state: &Entity<FileHistory>, index: usize, cx: &mut App) {
    let target = state.update(cx, |s, cx| {
        let entry = s.entries.get(index)?;
        s.selected = Some(index);
        s.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        cx.notify();
        Some((
            s.diff.clone(),
            s.repo_root.clone()?,
            entry.commit.hash.clone(),
            s.path_in_repo.clone()?,
        ))
    });
    if let Some((diff, repo, hash, path)) = target {
        diff.load(&repo, &hash, &path, None, cx);
    }
    let near_end = {
        let s = state.read(cx);
        index + 5 >= s.entries.len()
    };
    if near_end {
        load_more(state, cx);
    }
}

fn move_selection(state: &Entity<FileHistory>, delta: isize, cx: &mut App) {
    let (len, cur) = {
        let s = state.read(cx);
        (s.entries.len(), s.selected)
    };
    if len == 0 {
        return;
    }
    let next = match cur {
        None => 0,
        Some(cur) => (cur as isize + delta).clamp(0, len as isize - 1) as usize,
    };
    if Some(next) != cur {
        select(state, next, cx);
    }
}

/// 打开该提交的完整 diff(多文件弹窗,叠在本弹窗之上),先选中这个文件。
fn open_in_commit_diff(
    state: &Entity<FileHistory>,
    index: usize,
    window: &mut Window,
    cx: &mut App,
) {
    let target = {
        let s = state.read(cx);
        s.entries.get(index).and_then(|e| {
            Some((
                s.store.clone(),
                s.repo_root.clone()?,
                e.commit.hash.clone(),
                e.commit.message.clone(),
                s.path_in_repo.clone()?,
            ))
        })
    };
    let Some((store, repo, hash, message, path)) = target else {
        return;
    };
    window
        .spawn(cx, async move |cx| {
            let (task_repo, task_hash) = (PathBuf::from(&repo), hash.clone());
            let files = cx
                .background_executor()
                .spawn(async move { mt_project::git::get_commit_files(&task_repo, &task_hash) })
                .await;
            let _ = cx.update(|window, cx| match files {
                Ok(files) => git_diff::open_commit_diff_at(
                    store,
                    repo,
                    hash,
                    message,
                    files,
                    Some(path),
                    window,
                    cx,
                ),
                // 弹窗遮罩下 toast 看不见,失败退到提示框
                Err(err) => crate::prompt::show_alert(
                    t("gitLog", "fileHistory.viewInCommit"),
                    format!("{err:#}"),
                    window,
                    cx,
                ),
            });
        })
        .detach();
}

// ─── 渲染 ─────────────────────────────────────────────────────

fn render_list_column(state: &Entity<FileHistory>, file_name: &str, cx: &mut App) -> AnyElement {
    let s = state.read(cx);
    let count = s.entries.len();
    let path_hint = s.path_in_repo.clone().unwrap_or_else(|| s.file_rel.clone());

    let header = div()
        .flex_none()
        .px(px(14.0))
        .py(px(12.0))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .border_b_1()
        .border_color(ui::border_subtle())
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(crate::git_history::icons::icon(
                    ICON_HISTORY,
                    crate::git_history::icons::ICON_SIZE,
                    ui::accent(),
                ))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(ui::font_px(14.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(ui::text_primary())
                        .child(tr!("gitLog", "fileHistory.title", name = file_name)),
                ),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(path_hint),
        );

    let body: AnyElement = if let Some(err) = s.error.clone().filter(|_| count == 0) {
        div()
            .flex_1()
            .p(px(16.0))
            .text_size(ui::font_px(12.0))
            .text_color(ui::color_error())
            .child(tr!("gitLog", "fileHistory.failed", detail = err))
            .into_any_element()
    } else if count == 0 {
        let text = if s.loading || s.cursor.is_some() {
            if s.scanned > 0 {
                tr!(
                    "gitLog",
                    "fileHistory.scanning",
                    count = s.scanned.to_string()
                )
            } else {
                t("gitLog", "fileHistory.loading").to_string()
            }
        } else {
            t("gitLog", "fileHistory.empty").to_string()
        };
        div()
            .flex_1()
            .min_h(px(0.0))
            .child(empty_state(ICON_HISTORY, text, None))
            .into_any_element()
    } else {
        let list_state = state.clone();
        let scroll = s.scroll.clone();
        div()
            .flex_1()
            .min_h(px(0.0))
            .px(px(4.0))
            .py(px(4.0))
            .child(
                uniform_list(
                    "git-file-history-list",
                    count,
                    move |range, _window, cx: &mut App| {
                        if range.end >= count {
                            let state = list_state.clone();
                            cx.spawn(async move |cx| {
                                cx.update(|cx| load_more(&state, cx));
                            })
                            .detach();
                        }
                        range
                            .map(|i| render_row(&list_state, i, cx))
                            .collect::<Vec<_>>()
                    },
                )
                .track_scroll(&scroll)
                .size_full(),
            )
            .into_any_element()
    };

    let s = state.read(cx);
    let footer_count = if s.cursor.is_some() {
        tr!("gitLog", "fileHistory.countMore", count = count.to_string())
    } else {
        tr!("gitLog", "fileHistory.count", count = count.to_string())
    };
    let footer = div()
        .flex_none()
        .px(px(14.0))
        .py(px(8.0))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .border_t_1()
        .border_color(ui::border_subtle())
        .text_size(ui::font_px(11.0))
        .text_color(ui::text_muted())
        .child(mono(div()).child(footer_count))
        .child(t("gitLog", "fileHistory.noRename"));

    div()
        .w(px(LIST_WIDTH))
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .border_r_1()
        .border_color(ui::border_subtle())
        .bg(ui::bg_elevated())
        .child(header)
        .child(body)
        .child(footer)
        .into_any_element()
}

fn render_row(state: &Entity<FileHistory>, index: usize, cx: &mut App) -> AnyElement {
    let s = state.read(cx);
    let Some(entry) = s.entries.get(index) else {
        return div().into_any_element();
    };
    let selected = s.selected == Some(index);
    let commit = &entry.commit;
    let (letter, color) = file_status(&entry.status);
    let now = chrono::Utc::now().timestamp();
    let meta = format!(
        "{} · {} · {}",
        commit.author,
        format_relative_time(commit.timestamp, now),
        commit.short_hash
    );
    let tip = format!(
        "{}\n{} · {} · {}",
        commit.message,
        commit.author,
        format_absolute_time(commit.timestamp),
        commit.short_hash
    );
    let (click_state, menu_state) = (state.clone(), state.clone());
    let hash = commit.hash.clone();

    let mut row = div()
        .id(SharedString::from(format!("git-file-history-{index}")))
        .relative()
        .w_full()
        .h(px(ROW_HEIGHT))
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(4.0))
        .px(px(10.0))
        .rounded(px(4.0))
        .cursor_pointer();
    row = if selected {
        row.bg(ui::accent_subtle()).child(
            div()
                .absolute()
                .left_0()
                .top(px(8.0))
                .bottom(px(8.0))
                .w(px(2.0))
                .rounded(px(1.0))
                .bg(ui::accent()),
        )
    } else {
        row.hover(|el| el.bg(ui::bg_overlay()))
    };
    row.child(
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                mono(div())
                    .flex_none()
                    .w(px(12.0))
                    .text_size(ui::font_px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(color)
                    .child(letter),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui::font_px(13.0))
                    .text_color(ui::text_primary())
                    .child(commit.message.clone()),
            ),
    )
    .child(
        mono(div())
            .pl(px(20.0))
            .min_w_0()
            .truncate()
            .text_size(ui::font_px(11.0))
            .text_color(ui::text_muted())
            .child(meta),
    )
    .tip(tip)
    .on_click(move |event: &ClickEvent, window, cx| {
        let focus = click_state.read(cx).focus.clone();
        window.focus(&focus, cx);
        if event.click_count() >= 2 {
            open_in_commit_diff(&click_state, index, window, cx);
        } else if click_state.read(cx).selected != Some(index) {
            select(&click_state, index, cx);
        }
    })
    .on_mouse_down(
        MouseButton::Right,
        move |event: &MouseDownEvent, window, cx| {
            cx.stop_propagation();
            let (state, hash) = (menu_state.clone(), hash.clone());
            let entries = vec![
                menu::item(t("gitLog", "menu.copyFullHash"), move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(hash.clone()));
                }),
                menu::separator(),
                menu::item(
                    t("gitLog", "fileHistory.viewInCommit"),
                    move |window, cx| {
                        open_in_commit_diff(&state, index, window, cx);
                    },
                ),
            ];
            menu::show(event.position, entries, window, cx);
        },
    )
    .into_any_element()
}

fn render_diff_column(state: &Entity<FileHistory>, cx: &mut App) -> AnyElement {
    let (picked, diff) = {
        let s = state.read(cx);
        (
            s.selected
                .and_then(|i| s.entries.get(i).map(|e| (i, e.commit.clone()))),
            s.diff.clone(),
        )
    };
    let toolbar = diff.toolbar("git-file-history", kind::GIT_FILE_HISTORY, cx);

    let mut head = div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(10.0))
        .px(px(16.0))
        .py(px(10.0))
        .border_b_1()
        .border_color(ui::border_subtle());
    let body: AnyElement = match picked {
        Some((index, commit)) => {
            let state = state.clone();
            head = head
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(ui::font_px(13.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(ui::text_primary())
                                .child(commit.message.clone()),
                        )
                        .child(
                            mono(div())
                                .flex_none()
                                .text_size(ui::font_px(12.0))
                                .text_color(ui::text_muted())
                                .child(commit.short_hash.clone()),
                        ),
                )
                .child(
                    ui::ghost_button(
                        "git-file-history-open-commit",
                        t("gitLog", "fileHistory.viewInCommit"),
                    )
                    .flex_none()
                    .on_click(move |_: &ClickEvent, window, cx| {
                        open_in_commit_diff(&state, index, window, cx);
                    }),
                );
            diff.body(cx)
        }
        None => {
            head = head.child(div().flex_1());
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_muted())
                .child(t("gitLog", "fileHistory.selectHint"))
                .into_any_element()
        }
    };
    head = head.child(toolbar);

    div()
        .flex_1()
        .min_w(px(0.0))
        .h_full()
        .flex()
        .flex_col()
        .child(head)
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .bg(ui::bg_base())
                .child(body),
        )
        .into_any_element()
}
