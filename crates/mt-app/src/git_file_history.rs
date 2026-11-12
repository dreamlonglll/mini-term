//! 单文件历史(对标 IDEA 的 Show History):工作区里的一个 diff 页签,左栏是改动过这个
//! 文件的提交,右栏是所选提交里这个文件的 diff。页签的宿主、身份与复用(同一个文件只开一个
//! 历史页签)见 `git_diff::tab`。
//!
//! - 入口:文件树右键「Git 历史」、更改列表右键;对外只有 [`open`],别处要接入时直接调它;
//! - 数据:[`mt_project::git::get_file_log`](判定 = 相对第一个父提交改动了该路径,
//!   **不跟随重命名**),分页游标由后端交回;后端每批最多扫 `SCAN_BUDGET` 个提交,
//!   没凑满一页就接着要;
//! - 右栏复用 `git_diff` 的渲染([`git_diff::CommitFileDiffPane`]),并排 / 行内、
//!   改动跳转与提交 diff 页签完全一致;双击 / 右键「在完整提交 diff 中查看」另开一个
//!   提交 diff 页签;
//! - 键盘:打开即选中第一条;↑/↓ 换提交,右栏跟着换;
//! - 防陈旧:工具栏的「刷新」整张列表重拉(选中的提交按哈希认回);页签切回来、更改列表的
//!   取数落地时比一次 HEAD([`HistoryPage::check_head`]),变了才重拉 —— 历史只随提交变,
//!   不必像单文件 diff 那样每次都重取。

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Div, Entity, FocusHandle,
    FontWeight, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    Render, ScrollStrategy, SharedString, StatefulInteractiveElement, Styled,
    UniformListScrollHandle, Window, div, px, uniform_list,
};
use mt_project::git::{FileLogEntry, GitLogCursor};
use mt_ui::tooltip::TooltipExt as _;

use crate::git_diff::{self, CommitFileDiffPane, ToolbarTail};
use crate::git_history::{format_absolute_time, format_relative_time};
use crate::git_ui::{self, ButtonKind, ICON_HISTORY, commit_file_status, empty_state, mono};
use crate::i18n::{t, tr};
use crate::menu;
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

/// 单文件历史页签的键位上下文(`hotkeys.rs` 按它绑 ↑ / ↓)。
pub(crate) const FILE_HISTORY_CONTEXT: &str = "GitFileHistory";

/// 每页条数。页签里一屏能放二十来条,一页给够两屏。
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
    /// 想要凑满的条数:首屏一页,触底加一页;重拉时取重拉前已有的条数。
    want: usize,
    selected: Option<usize>,
    /// 重拉之后要认回的提交(重拉前选中的那条);认到了或拉完了还没有就清掉。
    reselect: Option<String>,
    /// 请求令牌:重拉时 +1,旧的那一趟回来不许落地。
    request: u64,
    /// 首页那一刻的 HEAD(切回来时比它,变了才重拉)。没提交过的仓库是 `None`。
    head: Option<String>,
    /// 比 HEAD 的那一趟在途(切回来与仓库变了会凑到一起)。
    checking: bool,
    diff: CommitFileDiffPane,
    scroll: UniformListScrollHandle,
    /// 页签的焦点(行点击时要回来,之后 ↑/↓ 才有人接)。
    focus: FocusHandle,
}

impl FileHistory {
    fn file_name(&self) -> String {
        self.file_rel
            .rsplit('/')
            .next()
            .unwrap_or(&self.file_rel)
            .to_string()
    }
}

impl Render for FileHistory {
    /// 只当状态盒子用(页签每帧按它现画)。
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 打开某个文件的 Git 历史页签(已开着就切过去)。`project_root` 可以是仓库里的子目录,
/// `file_rel` 是相对它的路径(`/` 或 `\` 分隔都行);所属仓库由后端就近发现。
pub fn open(
    store: Entity<AppStore>,
    project_root: PathBuf,
    file_rel: String,
    window: &mut Window,
    cx: &mut App,
) {
    git_diff::open_file_history(store, project_root, file_rel, window, cx);
}

/// 单文件历史页签的正文(宿主见 `git_diff::tab`)。
pub(crate) struct HistoryPage {
    state: Entity<FileHistory>,
}

impl HistoryPage {
    /// 建页并拉首页。`focus` 是页签的焦点句柄(左栏点行时要回来)。
    pub(crate) fn new(
        store: Entity<AppStore>,
        project_root: PathBuf,
        file_rel: String,
        focus: FocusHandle,
        cx: &mut App,
    ) -> Self {
        let diff = CommitFileDiffPane::new(&store, cx);
        let state = cx.new(|_| FileHistory {
            store,
            project_root,
            file_rel: file_rel.replace('\\', "/"),
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
            reselect: None,
            request: 0,
            head: None,
            checking: false,
            diff,
            scroll: UniformListScrollHandle::new(),
            focus,
        });
        load_page(&state, cx);
        Self { state }
    }

    /// 工具栏「刷新」:整张列表重拉(见 [`reload`])。
    pub(crate) fn reload(&self, cx: &mut App) {
        reload(&self.state, cx);
    }

    /// 页签切回来 / 仓库变了:后台比一次 HEAD,与首页那一刻的不一样(别处提交了、切了分支)
    /// 才整张重拉。还没拉到首页、正在拉、或已经在比的时候什么都不做。
    pub(crate) fn check_head(&self, cx: &mut App) {
        let Some((root, rel, known)) = self.state.update(cx, |s, _| {
            if s.loading || s.checking {
                return None;
            }
            let root = s.repo_root.clone()?;
            let rel = s.path_in_repo.clone()?;
            s.checking = true;
            Some((root, rel, s.head.clone()))
        }) else {
            return;
        };
        let state = self.state.clone();
        cx.spawn(async move |cx| {
            let signature = cx
                .background_executor()
                .spawn(async move { mt_project::git::head_signature(Path::new(&root), &rel) })
                .await;
            cx.update(|cx| {
                // 读不到(仓库被删 / 正被别的 git 进程改写)就等下一次
                let changed = state.update(cx, |s, _| {
                    s.checking = false;
                    signature.is_ok_and(|signature| signature.head != known)
                });
                if changed {
                    reload(&state, cx);
                }
            });
        })
        .detach();
    }

    pub(crate) fn render(&self, focus: &FocusHandle, refresh: AnyElement, cx: &mut App) -> Div {
        let state = &self.state;
        let (prev, next) = (state.clone(), state.clone());
        let (diff, file_name) = {
            let s = state.read(cx);
            (s.diff.clone(), s.file_name())
        };
        // 右栏 diff 正文的 Ctrl/Cmd+C / A 也挂在这个焦点容器上;Esc 没有绑定,照常往外走
        diff.text_actions(div())
            .track_focus(focus)
            .key_context(FILE_HISTORY_CONTEXT)
            .on_action(move |_: &FileHistoryPrev, _window, cx| move_selection(&prev, -1, cx))
            .on_action(move |_: &FileHistoryNext, _window, cx| move_selection(&next, 1, cx))
            .flex()
            .overflow_hidden()
            .child(render_list_column(state, &file_name, cx))
            .child(render_diff_column(state, refresh, cx))
    }
}

/// 整张列表重拉:清空、令牌 +1(在途的那一趟回来落不了地),按重拉前的条数拉回来,选中的
/// 提交按哈希认回(认不到就选第一条)。
fn reload(state: &Entity<FileHistory>, cx: &mut App) {
    state.update(cx, |s, cx| {
        s.reselect = s
            .selected
            .and_then(|index| s.entries.get(index))
            .map(|entry| entry.commit.hash.clone())
            .or_else(|| s.reselect.take());
        s.want = s.entries.len().max(PAGE_SIZE);
        s.request += 1;
        s.entries.clear();
        s.cursor = None;
        s.started = false;
        s.loading = false;
        s.error = None;
        s.scanned = 0;
        s.selected = None;
        s.head = None;
        cx.notify();
    });
    load_page(state, cx);
}

/// 要下一页(首页 / 触底 / 没凑满自动续)。到底或在途时什么都不做。首页那一趟顺手记下
/// 当时的 HEAD(见 [`HistoryPage::check_head`])。
fn load_page(state: &Entity<FileHistory>, cx: &mut App) {
    let Some((root, rel, after, req)) = state.update(cx, |s, _| {
        if s.loading || (s.started && s.cursor.is_none()) {
            return None;
        }
        s.loading = true;
        s.started = true;
        Some((
            s.project_root.clone(),
            s.file_rel.clone(),
            s.cursor.take(),
            s.request,
        ))
    }) else {
        return;
    };
    let first_page = after.is_none();
    let state = state.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                let page = mt_project::git::get_file_log(&root, &rel, after, PAGE_SIZE)?;
                let head = if first_page {
                    mt_project::git::head_signature(&page.repo_root, &page.path_in_repo)
                        .ok()
                        .and_then(|signature| signature.head)
                } else {
                    None
                };
                anyhow::Ok((page, head))
            })
            .await;
        cx.update(|cx| {
            let landed = state.update(cx, |s, cx| {
                if s.request != req {
                    return None;
                }
                s.loading = false;
                match result {
                    Ok((page, head)) => {
                        s.repo_root = Some(page.repo_root.to_string_lossy().to_string());
                        s.path_in_repo = Some(page.path_in_repo);
                        if first_page {
                            s.head = head;
                        }
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
                let need_more = s.cursor.is_some() && s.entries.len() < s.want;
                let pick = if s.selected.is_some() {
                    None
                } else {
                    let hashes: Vec<&str> =
                        s.entries.iter().map(|e| e.commit.hash.as_str()).collect();
                    let (pick, keep) = pick_after_load(&hashes, s.reselect.as_deref(), need_more);
                    if !keep {
                        s.reselect = None;
                    }
                    pick
                };
                Some((need_more, pick))
            });
            let Some((need_more, pick)) = landed else {
                return;
            };
            if let Some(index) = pick {
                select(&state, index, cx);
            }
            if need_more {
                load_page(&state, cx);
            }
        });
    })
    .detach();
}

/// 还没选中过任何一条时,一批落地之后该选中哪一条:返回 `(选中的下标, 要认回的提交还留不留着)`。
///
/// `hashes` 是眼下列表里的提交,`reselect` 是重拉前选中的那条。有要认回的就等它出现(出现了
/// 选它;还有下一批就再等;拉完了还没有就放弃、选第一条),没有就打开即选中第一条。列表还空着
/// 时什么都不选,要认回的留着。
fn pick_after_load(
    hashes: &[&str],
    reselect: Option<&str>,
    need_more: bool,
) -> (Option<usize>, bool) {
    if hashes.is_empty() {
        return (None, reselect.is_some());
    }
    let Some(hash) = reselect else {
        return (Some(0), false);
    };
    match hashes.iter().position(|h| *h == hash) {
        Some(index) => (Some(index), false),
        None if need_more => (None, true),
        None => (Some(0), false),
    }
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

/// 打开该提交的完整 diff(另一个提交 diff 页签),先选中这个文件。
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
    // 取文件失败弹详情框(见 `open_commit_diff_loading`)
    git_diff::open_commit_diff_loading(store, repo, hash, message, Some(path), window, cx).detach();
}

// ─── 渲染 ─────────────────────────────────────────────────────

fn render_list_column(state: &Entity<FileHistory>, file_name: &str, cx: &mut App) -> AnyElement {
    let s = state.read(cx);
    let count = s.entries.len();
    let path_hint = s.path_in_repo.clone().unwrap_or_else(|| s.file_rel.clone());

    // 头部与提交 diff 左栏同一口径(左右 12、上下 10、14px 半粗标题 + 12px 弱化副行)
    let header = div()
        .flex_none()
        .px(px(git_diff::SIDE_PAD))
        .py(px(10.0))
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
                .min_w_0()
                .child(git_ui::icon(ICON_HISTORY, git_ui::ICON_SIZE, ui::accent()))
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
            .child(git_ui::error_state(tr!(
                "gitLog",
                "fileHistory.failed",
                detail = err
            )))
            .into_any_element()
    } else if count == 0 {
        let pending = s.loading || s.cursor.is_some();
        let state_el = if pending {
            git_ui::loading_state(if s.scanned > 0 {
                tr!(
                    "gitLog",
                    "fileHistory.scanning",
                    count = s.scanned.to_string()
                )
            } else {
                t("gitLog", "fileHistory.loading").to_string()
            })
        } else {
            empty_state(ICON_HISTORY, t("gitLog", "fileHistory.empty"), None)
        };
        div()
            .flex_1()
            .min_h(px(0.0))
            .child(state_el.size_full())
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
        .px(px(git_diff::SIDE_PAD))
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
    let (letter, color) = commit_file_status(&entry.status);
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

    // 共用列表行(选中 = accent 淡底 + 左侧竖条、悬停薄纱底),两行布局:
    // 状态字母 + 提交标题 / 作者 · 时间 · 短哈希(与标题左缘对齐)
    git_ui::list_row(
        SharedString::from(format!("git-file-history-{index}")),
        selected,
    )
    .h(px(ROW_HEIGHT))
    .flex_col()
    .items_stretch()
    .justify_center()
    .gap(px(4.0))
    .px(px(10.0))
    .child(
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(git_ui::status_letter(letter, color))
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
            .pl(px(22.0))
            .min_w_0()
            .truncate()
            .text_size(ui::font_px(12.0))
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

fn render_diff_column(
    state: &Entity<FileHistory>,
    refresh: AnyElement,
    cx: &mut App,
) -> AnyElement {
    let (picked, diff) = {
        let s = state.read(cx);
        (
            s.selected
                .and_then(|i| s.entries.get(i).map(|e| (i, e.commit.clone()))),
            s.diff.clone(),
        )
    };
    let toolbar = diff.toolbar("git-file-history", ToolbarTail::Refresh(refresh), cx);

    // 与另几种 diff 页签的工具栏同高同距
    let mut head = div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(16.0))
        .h(px(git_diff::TOOLBAR_H))
        .px(px(16.0))
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
                                .text_size(ui::font_px(14.0))
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
                    git_ui::action_button(
                        "git-file-history-open-commit",
                        t("gitLog", "fileHistory.viewInCommit"),
                        ButtonKind::Ghost,
                        true,
                    )
                    .on_click(move |_: &ClickEvent, window, cx| {
                        open_in_commit_diff(&state, index, window, cx);
                    }),
                );
            diff.body(cx)
        }
        None => {
            head = head.child(div().flex_1());
            empty_state(ICON_HISTORY, t("gitLog", "fileHistory.selectHint"), None)
                .size_full()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 重拉之后认回原先选中的提交:在这一批里就选它;还有下一批就先等;拉完了还没有(提交
    /// 被改写掉了)就放弃、选第一条。没有要认回的就打开即选中第一条;列表空着什么都不选。
    #[test]
    fn 重拉后按哈希认回选中的提交() {
        assert_eq!(
            pick_after_load(&["a", "b", "c"], None, true),
            (Some(0), false)
        );
        assert_eq!(
            pick_after_load(&["a", "b", "c"], Some("c"), true),
            (Some(2), false)
        );
        assert_eq!(pick_after_load(&["a", "b"], Some("z"), true), (None, true));
        assert_eq!(
            pick_after_load(&["a", "b"], Some("z"), false),
            (Some(0), false)
        );
        assert_eq!(pick_after_load(&[], Some("z"), false), (None, true));
        assert_eq!(pick_after_load(&[], None, false), (None, false));
    }
}
