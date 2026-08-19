//! 两个 diff 弹窗:
//!
//! - [`open_file_diff`] —— 工作区 / 暂存区的**单文件** diff(`src/components/DiffModal.tsx`)
//! - [`open_commit_diff`] —— 某次 commit 的**多文件** diff(`src/components/CommitDiffModal.tsx`)
//!
//! 两个视图(`InlineView` / `SideBySideView`)在原版是 `DiffModal.tsx` 导出、
//! 由 `CommitDiffModal` 复用的;这里同样只有一份([`render_inline`] / [`render_side_by_side`])。
//!
//! # 与原版的两处有意偏差
//!
//! 1. **行渲染走 [`gpui::uniform_list`](gpui::uniform_list) 虚拟化**。原版 `rows.map` 全量建 DOM ——
//!    1MB 上限(`MAX_DIFF_BYTES`)挡住了最坏情况,但一个 900KB 的文本文件仍能出
//!    ~20k 行,gpui 全量建元素会明显卡。行高恒定 = `round(fontSize*1.6)`,
//!    天然适配 uniform_list。这是**改进**而非偏差(规格 §6.6 明写)。
//! 2. **`staged` 依赖漏项顺修**。原版 effect 的依赖数组是 `[open, projectPath, status.path]`
//!    (`DiffModal.tsx:179`),**漏了 `staged`** —— 同一路径先点 staged 行再点 unstaged 行
//!    不会重新拉 diff。这里每次打开都是一个新弹窗、按 `(repo, path, staged)` 三元组
//!    取一次数,那个 bug 自然不存在(规格 §11 第 17 条建议顺修并注明)。
//!
//! # 判定顺序不能换
//!
//! `loading → error → isBinary → tooLarge → 正常`(`DiffModal.tsx:233-259`)。
//! 二进制文件的 `hunks` 是空的,先判 `tooLarge` 会把它显示成「文件过大」。

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel};
use mt_project::git::{CommitFileInfo, DiffHunk, DiffLine, GitDiffResult};

use crate::i18n::{t, tr};
use crate::prompt::{kind, open_guarded};
use crate::store::AppStore;
use crate::ui;

/// 视图模式。**组件态,不落盘**;默认 side-by-side(`DiffModal.tsx:158`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    SideBySide,
    Inline,
}

// ─── 行配对(SideBySideView 的核心算法) ────────────────────────

/// 把全部 hunk 的行拍平,并算出左右两栏的配对。
///
/// 逐条移植 `DiffModal.tsx:63-97`:
///
/// ```text
/// context        → 左右同一行
/// delete         → 连续吃掉所有 delete,再连续吃掉紧随的 add,按下标配对
///                  (短的一侧留空)
/// add            → 只出现在右栏
/// ```
///
/// 返回 `(拍平的行, 配对下标)`。配对**不跨 hunk**(原版每个 hunk 重置 `i`)。
pub fn pair_rows(hunks: &[DiffHunk]) -> (Vec<DiffLine>, Vec<(Option<usize>, Option<usize>)>) {
    let mut lines: Vec<DiffLine> = Vec::new();
    let mut rows: Vec<(Option<usize>, Option<usize>)> = Vec::new();

    for hunk in hunks {
        let base = lines.len();
        lines.extend(hunk.lines.iter().cloned());
        let hunk_lines = &hunk.lines;

        let mut i = 0usize;
        while i < hunk_lines.len() {
            match hunk_lines[i].kind.as_str() {
                "context" => {
                    rows.push((Some(base + i), Some(base + i)));
                    i += 1;
                }
                "delete" => {
                    let mut deletes = Vec::new();
                    while i < hunk_lines.len() && hunk_lines[i].kind == "delete" {
                        deletes.push(base + i);
                        i += 1;
                    }
                    let mut adds = Vec::new();
                    while i < hunk_lines.len() && hunk_lines[i].kind == "add" {
                        adds.push(base + i);
                        i += 1;
                    }
                    let max_len = deletes.len().max(adds.len());
                    for j in 0..max_len {
                        rows.push((deletes.get(j).copied(), adds.get(j).copied()));
                    }
                }
                "add" => {
                    rows.push((None, Some(base + i)));
                    i += 1;
                }
                // 认不出的 kind 直接跳过(原版的 else 分支)
                _ => i += 1,
            }
        }
    }

    (lines, rows)
}

fn line_bg(kind: &str) -> Option<gpui::Hsla> {
    match kind {
        "add" => Some(ui::diff_add_bg()),
        "delete" => Some(ui::diff_del_bg()),
        _ => None,
    }
}

fn line_fg(kind: &str) -> gpui::Hsla {
    match kind {
        "add" => ui::diff_add_text(),
        "delete" => ui::diff_del_text(),
        _ => ui::text_primary(),
    }
}

/// 行号列宽(`DiffModal.tsx:38,116` 的 `w-[48px]`)。
const GUTTER: f32 = 48.0;

/// 一行:行号列 + 内容列。`gutter` 是行号列要显示的文本。
fn diff_line_row(line: &DiffLine, gutter: String, line_height: f32) -> AnyElement {
    let kind = line.kind.as_str();
    div()
        .flex()
        .h(px(line_height))
        .when_some(line_bg(kind), |el, bg| el.bg(bg))
        .child(
            div()
                .w(px(GUTTER))
                .flex_none()
                .pr(px(8.0))
                .text_right()
                .text_color(ui::text_muted())
                .opacity(0.5)
                .child(gutter),
        )
        .child(
            div()
                .flex_1()
                .px(px(8.0))
                .text_color(line_fg(kind))
                // `whitespace-pre`:diff 行不换行,横向滚动交给外层
                .child(SharedString::from(line.content.clone())),
        )
        .into_any_element()
}

/// 空格子(`DiffModal.tsx:100-107`)。
fn empty_cell(line_height: f32) -> AnyElement {
    div()
        .flex()
        .h(px(line_height))
        .bg(ui::bg_base())
        .opacity(0.3)
        .child(div().w(px(GUTTER)).flex_none())
        .child(div().flex_1())
        .into_any_element()
}

// ─── 弹窗内的状态实体 ─────────────────────────────────────────

/// 两个弹窗共用的 diff 内容状态。
struct DiffState {
    loading: bool,
    error: Option<String>,
    result: Option<GitDiffResult>,
    /// 拍平的行 + 左右配对(结果一到就算好,不在每帧的 uniform_list 回调里重算)。
    lines: Vec<DiffLine>,
    pairs: Vec<(Option<usize>, Option<usize>)>,
    view: ViewMode,
    font_size: f32,
    /// 请求令牌:换文件时旧响应不许覆盖。
    request: u64,
    split: Entity<ResizableState>,
}

impl DiffState {
    fn new(font_size: f32, cx: &mut App) -> Self {
        Self {
            loading: true,
            error: None,
            result: None,
            lines: Vec::new(),
            pairs: Vec::new(),
            view: ViewMode::SideBySide,
            font_size,
            request: 0,
            split: cx.new(|_| ResizableState::default()),
        }
    }

    fn line_height(&self) -> f32 {
        (self.font_size * 1.6).round()
    }

    fn apply(&mut self, result: anyhow::Result<GitDiffResult>) {
        self.loading = false;
        match result {
            Ok(diff) => {
                let (lines, pairs) = pair_rows(&diff.hunks);
                self.lines = lines;
                self.pairs = pairs;
                self.result = Some(diff);
                self.error = None;
            }
            Err(err) => {
                self.error = Some(format!("{err:#}"));
                self.result = None;
                self.lines.clear();
                self.pairs.clear();
            }
        }
    }
}

impl Render for DiffState {
    /// `DiffState` 只当状态盒子用(Dialog 的 builder 是 `Fn`,每帧重跑,
    /// 编辑中的状态不能藏在闭包捕获里)。它自己不画东西。
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

// ─── 内容区渲染 ───────────────────────────────────────────────

fn centered(text: impl Into<SharedString>, color: gpui::Hsla) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(13.0))
        .text_color(color)
        .child(text.into())
        .into_any_element()
}

/// 内容区的五选一(顺序不能换,见模块注释)。`labels` 是四条文案
/// (loading / binary / tooLarge / 额外的空态),两个弹窗各给各的命名空间。
fn render_body(
    state: &Entity<DiffState>,
    loading: &'static str,
    binary: &'static str,
    too_large: &'static str,
    cx: &mut App,
) -> AnyElement {
    let s = state.read(cx);
    if s.loading {
        return centered(loading, ui::text_muted());
    }
    if let Some(err) = &s.error {
        return centered(err.clone(), ui::color_error());
    }
    let Some(result) = &s.result else {
        return div().into_any_element();
    };
    if result.is_binary {
        return centered(binary, ui::text_muted());
    }
    if result.too_large {
        return centered(too_large, ui::text_muted());
    }
    match s.view {
        ViewMode::Inline => render_inline(state, cx),
        ViewMode::SideBySide => render_side_by_side(state, cx),
    }
}

/// `InlineView`(`DiffModal.tsx:22-58`)。
///
/// ⚠️ hunk 之间**没有** `@@ -a,b +c,d @@` 头 —— 原版不画,别自作主张加。
fn render_inline(state: &Entity<DiffState>, cx: &mut App) -> AnyElement {
    let s = state.read(cx);
    let count = s.lines.len();
    let line_height = s.line_height();
    let font_size = s.font_size;
    let state = state.clone();
    uniform_list(
        "git-diff-inline",
        count,
        move |range, _window, cx: &mut App| {
            let s = state.read(cx);
            range
                .map(|i| {
                    let line = &s.lines[i];
                    let gutter = match line.kind.as_str() {
                        "add" => "+".to_string(),
                        "delete" => "-".to_string(),
                        _ => line.old_lineno.map(|n| n.to_string()).unwrap_or_default(),
                    };
                    diff_line_row(line, gutter, line_height)
                })
                .collect::<Vec<_>>()
        },
    )
    .size_full()
    .text_size(px(font_size))
    .into_any_element()
}

/// `SideBySideView`(`DiffModal.tsx:62-152`)。
///
/// ⚠️ **两栏滚动不同步** —— 原版就没同步,别顺手加。
fn render_side_by_side(state: &Entity<DiffState>, cx: &mut App) -> AnyElement {
    let s = state.read(cx);
    let count = s.pairs.len();
    let line_height = s.line_height();
    let font_size = s.font_size;
    let split = s.split.clone();

    let column = |side_left: bool| {
        let state = state.clone();
        uniform_list(
            if side_left {
                "git-diff-left"
            } else {
                "git-diff-right"
            },
            count,
            move |range, _window, cx: &mut App| {
                let s = state.read(cx);
                range
                    .map(|i| {
                        let (left, right) = s.pairs[i];
                        let index = if side_left { left } else { right };
                        match index {
                            None => empty_cell(line_height),
                            Some(index) => {
                                let line = &s.lines[index];
                                // 左栏显示 oldLineno、右栏显示 newLineno
                                let no = if side_left {
                                    line.old_lineno
                                } else {
                                    line.new_lineno
                                };
                                diff_line_row(
                                    line,
                                    no.map(|n| n.to_string()).unwrap_or_default(),
                                    line_height,
                                )
                            }
                        }
                    })
                    .collect::<Vec<_>>()
            },
        )
        .size_full()
    };

    div()
        .size_full()
        .text_size(px(font_size))
        .child(
            h_resizable("git-diff-columns")
                .with_state(&split)
                .child(resizable_panel().child(div().size_full().child(column(true))))
                .child(resizable_panel().child(div().size_full().child(column(false)))),
        )
        .into_any_element()
}

/// 视图段控件 + ✕(两个弹窗共用的右上角)。
fn view_toggle(
    state: &Entity<DiffState>,
    id_prefix: &'static str,
    side_label: &'static str,
    inline_label: &'static str,
    close_kind: &'static str,
    cx: &mut App,
) -> AnyElement {
    let current = state.read(cx).view;
    let mut seg = div()
        .flex()
        .rounded(px(4.0))
        .overflow_hidden()
        .border_1()
        .border_color(ui::border_default());
    for (mode, label) in [
        (ViewMode::SideBySide, side_label),
        (ViewMode::Inline, inline_label),
    ] {
        let active = mode == current;
        let state = state.clone();
        seg = seg.child(
            div()
                .id(SharedString::from(format!(
                    "{id_prefix}-view-{}",
                    if matches!(mode, ViewMode::Inline) {
                        "inline"
                    } else {
                        "side"
                    }
                )))
                .px(px(12.0))
                .py(px(4.0))
                .text_size(px(13.0))
                .cursor_pointer()
                .when(active, |el| {
                    el.bg(ui::accent_subtle()).text_color(ui::accent())
                })
                .when(!active, |el| {
                    el.text_color(ui::text_muted())
                        .hover(|el| el.text_color(ui::text_primary()))
                })
                .child(label)
                .on_click(move |_: &ClickEvent, _window, cx| {
                    state.update(cx, |s, cx| {
                        s.view = mode;
                        cx.notify();
                    });
                }),
        );
    }

    div()
        .flex()
        .items_center()
        .child(seg)
        .child(
            div()
                .id(SharedString::from(format!("{id_prefix}-close")))
                .ml(px(8.0))
                .text_size(px(18.0))
                .text_color(ui::text_muted())
                .cursor_pointer()
                .hover(|el| el.text_color(ui::color_error()))
                .child("✕")
                .on_click(move |_: &ClickEvent, window, cx| {
                    crate::prompt::close_guarded(close_kind, window, cx);
                }),
        )
        .into_any_element()
}

// ─── DiffModal(工作区/暂存区单文件) ──────────────────────────

/// 打开单文件 diff。`staged` 决定取暂存区还是工作区那一侧。
pub fn open_file_diff(
    store: Entity<AppStore>,
    repo_path: String,
    file_path: String,
    staged: bool,
    status_label: String,
    window: &mut Window,
    cx: &mut App,
) {
    if repo_path.is_empty() {
        return;
    }
    let font_size = store.read(cx).config().terminal_font_size as f32;
    let state = cx.new(|cx| DiffState::new(font_size, cx));

    // 取数:`(repo, path, staged)` 三元组一次到位(原版漏了 staged,见模块注释)
    {
        let (repo, path) = (repo_path.clone(), file_path.clone());
        let state = state.clone();
        cx.spawn(async move |cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    mt_project::git::get_git_diff(
                        std::path::Path::new(&repo),
                        &path,
                        Some(staged),
                    )
                })
                .await;
            let _ = state.update(cx, |s: &mut DiffState, cx| {
                s.apply(result);
                cx.notify();
            });
        })
        .detach();
    }

    let file_name = file_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&file_path)
        .to_string();

    open_guarded(kind::GIT_DIFF, window, cx, move |dialog, window, cx| {
        let viewport = window.viewport_size();
        let body = render_body(
            &state,
            t("diffModal", "loading"),
            t("diffModal", "binaryNotSupported"),
            t("diffModal", "tooLarge"),
            cx,
        );
        let toolbar = div()
            .flex()
            .items_center()
            .justify_between()
            .px(px(16.0))
            .py(px(12.0))
            .flex_none()
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .min_w(px(0.0))
                    .child(
                        div()
                            .text_size(px(15.0))
                            .text_color(ui::accent())
                            .child(file_name.clone()),
                    )
                    .child(
                        div()
                            .max_w(px(300.0))
                            .truncate()
                            .text_size(px(13.0))
                            .text_color(ui::text_muted())
                            .child(file_path.clone()),
                    )
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(2.0))
                            .rounded(px(4.0))
                            .bg(ui::bg_elevated())
                            .border_1()
                            .border_color(ui::border_subtle())
                            .text_size(px(11.0))
                            .text_color(ui::text_muted())
                            .child(status_label.clone()),
                    ),
            )
            .child(view_toggle(
                &state,
                "git-diff",
                t("diffModal", "sideBySide"),
                t("diffModal", "inline"),
                kind::GIT_DIFF,
                cx,
            ));

        dialog
            .w(viewport.width * 0.9)
            .child(
                div()
                    .h(viewport.height * 0.8)
                    .flex()
                    .flex_col()
                    .child(toolbar)
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.0))
                            .overflow_hidden()
                            .bg(ui::bg_base())
                            .child(body),
                    ),
            )
    });
}

// ─── CommitDiffModal(某次 commit 的多文件) ──────────────────

/// 左栏文件列表的状态字母表(`CommitDiffModal.tsx:21-26`)。
/// 查不到(conflicted / untracked 之类)回落 `?` + muted。
fn commit_file_badge(status: &str) -> (&'static str, gpui::Hsla) {
    match status {
        "added" => ("A", ui::color_success()),
        "modified" => ("M", ui::color_warning()),
        "deleted" => ("D", ui::color_error()),
        "renamed" => ("R", ui::color_info()),
        _ => ("?", ui::text_muted()),
    }
}

/// 左栏选中项。放进实体是因为 Dialog 的 builder 每帧重跑。
struct CommitPick {
    selected: String,
}

impl Render for CommitPick {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 打开某次 commit 的多文件 diff。
pub fn open_commit_diff(
    store: Entity<AppStore>,
    repo_path: String,
    commit_hash: String,
    commit_message: String,
    files: Vec<CommitFileInfo>,
    window: &mut Window,
    cx: &mut App,
) {
    let font_size = store.read(cx).config().terminal_font_size as f32;
    let state = cx.new(|cx| DiffState::new(font_size, cx));
    let first = files.first().map(|f| f.path.clone()).unwrap_or_default();
    let pick = cx.new(|_| CommitPick {
        selected: first.clone(),
    });

    if !first.is_empty() {
        load_commit_file(&state, &repo_path, &commit_hash, &files, &first, cx);
    } else {
        state.update(cx, |s, _| {
            s.loading = false;
        });
    }

    let short_hash: String = commit_hash.chars().take(7).collect();

    open_guarded(
        kind::GIT_COMMIT_DIFF,
        window,
        cx,
        move |dialog, window, cx| {
            let viewport = window.viewport_size();
            let selected = pick.read(cx).selected.clone();

            // 左栏
            let mut file_list = div()
                .id("git-commit-files")
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll();
            for (idx, file) in files.iter().enumerate() {
                let (letter, color) = commit_file_badge(&file.status);
                let active = file.path == selected;
                let name = file
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&file.path)
                    .to_string();
                let (pick, state) = (pick.clone(), state.clone());
                let (repo, hash, files_for_click, path) = (
                    repo_path.clone(),
                    commit_hash.clone(),
                    files.clone(),
                    file.path.clone(),
                );
                file_list = file_list.child(
                    div()
                        .id(SharedString::from(format!("git-commit-file-{idx}")))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(12.0))
                        .py(px(6.0))
                        .cursor_pointer()
                        .text_size(px(13.0))
                        .when(active, |el| {
                            el.bg(ui::accent_subtle()).text_color(ui::accent())
                        })
                        .when(!active, |el| {
                            el.text_color(ui::text_primary())
                                .hover(|el| el.bg(ui::border_subtle()))
                        })
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(11.0))
                                .text_color(color)
                                .child(letter),
                        )
                        .child(div().truncate().child(name))
                        .on_click(move |_: &ClickEvent, _window, cx| {
                            if pick.read(cx).selected == path {
                                return;
                            }
                            pick.update(cx, |p, cx| {
                                p.selected = path.clone();
                                cx.notify();
                            });
                            load_commit_file(&state, &repo, &hash, &files_for_click, &path, cx);
                        }),
                );
            }

            let left = div()
                .w(px(224.0))
                .flex_none()
                .h_full()
                .flex()
                .flex_col()
                .border_r_1()
                .border_color(ui::border_subtle())
                .bg(ui::bg_elevated())
                .child(
                    div()
                        .px(px(12.0))
                        .py(px(12.0))
                        .flex_none()
                        .border_b_1()
                        .border_color(ui::border_subtle())
                        .child(
                            div()
                                .truncate()
                                .text_size(px(13.0))
                                .text_color(ui::accent())
                                .child(commit_message.clone()),
                        )
                        .child(
                            div()
                                .mt(px(4.0))
                                .text_size(px(11.0))
                                .text_color(ui::text_muted())
                                .child(short_hash.clone()),
                        ),
                )
                .child(file_list)
                .child(
                    div()
                        .px(px(12.0))
                        .py(px(8.0))
                        .flex_none()
                        .border_t_1()
                        .border_color(ui::border_subtle())
                        .text_size(px(11.0))
                        .text_color(ui::text_muted())
                        .child(tr!(
                            "commitDiff",
                            "fileCount",
                            count = files.len().to_string()
                        )),
                );

            // 右栏
            let body = if files.is_empty() {
                centered(t("commitDiff", "noChanges"), ui::text_muted())
            } else {
                render_body(
                    &state,
                    t("commitDiff", "loading"),
                    t("commitDiff", "binaryFile"),
                    t("commitDiff", "tooLarge"),
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
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(16.0))
                        .py(px(12.0))
                        .flex_none()
                        .border_b_1()
                        .border_color(ui::border_subtle())
                        .child(
                            div()
                                .max_w(px(400.0))
                                .truncate()
                                .text_size(px(13.0))
                                .text_color(ui::text_primary())
                                .child(selected.clone()),
                        )
                        .child(view_toggle(
                            &state,
                            "git-commit-diff",
                            t("commitDiff", "sideBySide"),
                            t("commitDiff", "inline"),
                            kind::GIT_COMMIT_DIFF,
                            cx,
                        )),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_hidden()
                        .bg(ui::bg_base())
                        .child(body),
                );

            dialog.w(viewport.width * 0.92).child(
                div()
                    .h(viewport.height * 0.85)
                    .flex()
                    .child(left)
                    .child(right),
            )
        },
    );
}

/// 取一个文件在该 commit 里的 diff。
///
/// ⚠️ **重命名文件必须传 `oldPath`**(`CommitDiffModal.tsx:57`),否则父树里查不到
/// 旧内容,diff 会显示成「整文件新增」。
fn load_commit_file(
    state: &Entity<DiffState>,
    repo_path: &str,
    commit_hash: &str,
    files: &[CommitFileInfo],
    path: &str,
    cx: &mut App,
) {
    let old_path = files
        .iter()
        .find(|f| f.path == path)
        .and_then(|f| f.old_path.clone());
    let req = state.update(cx, |s, cx| {
        s.loading = true;
        s.error = None;
        s.result = None;
        s.lines.clear();
        s.pairs.clear();
        s.request += 1;
        cx.notify();
        s.request
    });

    let (repo, hash, path) = (
        repo_path.to_string(),
        commit_hash.to_string(),
        path.to_string(),
    );
    let state = state.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                mt_project::git::get_commit_file_diff(
                    std::path::Path::new(&repo),
                    &hash,
                    &path,
                    old_path.as_deref(),
                )
            })
            .await;
        let _ = state.update(cx, |s: &mut DiffState, cx| {
            if s.request != req {
                return;
            }
            s.apply(result);
            cx.notify();
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: &str, old: Option<u32>, new: Option<u32>) -> DiffLine {
        DiffLine {
            kind: kind.to_string(),
            content: format!("{kind}{old:?}{new:?}"),
            old_lineno: old,
            new_lineno: new,
        }
    }

    fn hunk(lines: Vec<DiffLine>) -> DiffHunk {
        DiffHunk {
            old_start: 1,
            old_lines: 0,
            new_start: 1,
            new_lines: 0,
            lines,
        }
    }

    /// context 行左右同格。
    #[test]
    fn 配对_纯上下文() {
        let h = hunk(vec![
            line("context", Some(1), Some(1)),
            line("context", Some(2), Some(2)),
        ]);
        let (lines, pairs) = pair_rows(&[h]);
        assert_eq!(lines.len(), 2);
        assert_eq!(pairs, vec![(Some(0), Some(0)), (Some(1), Some(1))]);
    }

    /// 纯新增只出现在右栏。
    #[test]
    fn 配对_纯新增() {
        let h = hunk(vec![
            line("add", None, Some(1)),
            line("add", None, Some(2)),
        ]);
        let (_, pairs) = pair_rows(&[h]);
        assert_eq!(pairs, vec![(None, Some(0)), (None, Some(1))]);
    }

    /// delete 后紧跟 add:按下标配对,**长度不等时短的一侧留空**。
    #[test]
    fn 配对_删增不等长() {
        // 3 删 2 增 → 3 行,第三行右侧空
        let h = hunk(vec![
            line("delete", Some(1), None),
            line("delete", Some(2), None),
            line("delete", Some(3), None),
            line("add", None, Some(1)),
            line("add", None, Some(2)),
        ]);
        let (_, pairs) = pair_rows(&[h]);
        assert_eq!(
            pairs,
            vec![(Some(0), Some(3)), (Some(1), Some(4)), (Some(2), None)]
        );

        // 1 删 3 增 → 3 行,后两行左侧空
        let h = hunk(vec![
            line("delete", Some(1), None),
            line("add", None, Some(1)),
            line("add", None, Some(2)),
            line("add", None, Some(3)),
        ]);
        let (_, pairs) = pair_rows(&[h]);
        assert_eq!(
            pairs,
            vec![(Some(0), Some(1)), (None, Some(2)), (None, Some(3))]
        );
    }

    /// 配对**不跨 hunk**:上一 hunk 末尾的 delete 不会与下一 hunk 开头的 add 配对。
    #[test]
    fn 配对不跨_hunk() {
        let a = hunk(vec![line("delete", Some(1), None)]);
        let b = hunk(vec![line("add", None, Some(9))]);
        let (lines, pairs) = pair_rows(&[a, b]);
        assert_eq!(lines.len(), 2);
        assert_eq!(pairs, vec![(Some(0), None), (None, Some(1))]);
    }

    /// 认不出的 kind 直接跳过(原版的 else 分支),不产生行也不打乱下标。
    #[test]
    fn 配对_未知种类跳过() {
        let h = hunk(vec![
            line("weird", None, None),
            line("context", Some(1), Some(1)),
        ]);
        let (lines, pairs) = pair_rows(&[h]);
        assert_eq!(lines.len(), 2, "拍平的行仍含未知种类,只是不进配对");
        assert_eq!(pairs, vec![(Some(1), Some(1))]);
    }

    /// commit 文件的状态字母表:四种已知 + 回落 `?`。
    #[test]
    fn commit_文件状态字母() {
        ui::set_palette(ui::Palette::dark());
        assert_eq!(commit_file_badge("added").0, "A");
        assert_eq!(commit_file_badge("modified").0, "M");
        assert_eq!(commit_file_badge("deleted").0, "D");
        assert_eq!(commit_file_badge("renamed").0, "R");
        assert_eq!(commit_file_badge("conflicted").0, "?");
        assert_eq!(commit_file_badge("").1, ui::text_muted());
    }
}
