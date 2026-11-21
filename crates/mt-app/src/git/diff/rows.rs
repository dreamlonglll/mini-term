use super::*;

pub(super) fn line_bg(kind: &str) -> Option<gpui::Hsla> {
    match kind {
        "add" => Some(ui::diff_add_bg()),
        "delete" => Some(ui::diff_del_bg()),
        _ => None,
    }
}

pub(super) fn line_fg(kind: &str) -> gpui::Hsla {
    match kind {
        "add" => ui::diff_add_text(),
        "delete" => ui::diff_del_text(),
        _ => ui::text_primary(),
    }
}

/// 并排视图一栏行号列的宽(`DiffModal.tsx:38,116` 的 `w-[48px]`),在中间栏里(见 [`ribbon`])。
pub(super) const GUTTER: f32 = 48.0;

/// inline 视图的两列行号(旧 / 新)各自的宽度与 `+`/`-` 符号列宽。
/// 两列合起来与 hunk 头的留白一致,头里的 `@@` 才与正文左缘对齐。
pub(super) const INLINE_NUM: f32 = 44.0;
pub(super) const INLINE_SIGN: f32 = 16.0;
pub(super) const INLINE_GUTTER: f32 = INLINE_NUM * 2.0 + INLINE_SIGN;

/// 勾选框那一列的宽(只在允许按行操作时出现):行内视图在行号栏最左,并排视图贴着中间栏里
/// 的行号、靠正文一侧。
pub(super) const CHECK_W: f32 = 24.0;

/// 行内高亮的底色(与整行底色叠在一起,所以要更实)。
pub(super) fn word_bg(kind: &str) -> gpui::Hsla {
    if kind == "add" {
        ui::diff_add_word_bg()
    } else {
        ui::diff_del_word_bg()
    }
}

/// 横向滚动时**钉在左缘**的一层(行号列、hunk 头连同块操作按钮)。
///
/// 横向滚动是整份内容一起平移的(uniform_list 只有一个滚动偏移),要让某一块不动,
/// 就把它绝对定位、再往右挪回「已经滚出去的那一段」:`scroll_x` 是列表当前的横向偏移
/// (≤ 0),`left = -scroll_x`。偏移是每帧画行时现读的(滚轮改了偏移就重画),不落后一帧。
///
/// 绝对定位的元素不计入行的内容宽,uniform_list 量宽时只量正文 —— 横向可滚范围不变。
pub(super) fn pinned(scroll_x: Pixels) -> gpui::Div {
    div().absolute().top_0().left(-scroll_x).h_full()
}

/// 一行:若干行号列 + 内容列。`gutters` 是各行号列的 `(文本, 宽)`(并排视图一列,
/// inline 视图旧 / 新两列 + 符号列),`marks` 是这一行里**真正变了的片段**
/// (空 = 整行一个底色,与原版一致)。`check` 是行号栏最左那一列勾选框(宽 [`CHECK_W`],
/// 只在允许按行操作时给;不能勾的行给空格子占住列宽)。
///
/// 行号列钉在左缘([`pinned`]),正文从它底下横向滚过:行号列先铺一层不透明的正文底、
/// 再叠这一行的增删底色,滚过去的字不会透出来;正文左侧留出行号列的宽。勾选框是行号列的
/// 一部分,随它一起钉住;按在上面不起拖选(行号列本来就不可选,见 [`selection`])。
///
/// 行根 `w_full()`:横向不受限的 uniform_list 里行宽 = 内容区宽,少了它增删行的底色
/// 只涂到文字为止,长短行参差不齐。
///
/// 正文可拖选(见 [`selection`]):`row` 带着这一行的选区与排版登记;正文区是 I 形光标,
/// 行号列照旧是箭头。行高同时设成文字行高 —— 高亮底色按文字行高画,两者不一致时
/// 跨行选区的底色会一条压一条、叠出深浅相间的横纹。
pub(super) fn diff_line_row(
    line: &FlatLine,
    gutters: &[(String, f32)],
    check: Option<AnyElement>,
    line_height: f32,
    marks: &[Range<usize>],
    scroll: Point<Pixels>,
    row: RowText,
) -> AnyElement {
    let kind = line.kind.as_str();
    // 只加引用计数,不复制整行(见 [`FlatLine::content`])
    let content = line.content.clone();
    let check_w = if check.is_some() { CHECK_W } else { 0.0 };
    let gutter_w: f32 = check_w + gutters.iter().map(|(_, width)| *width).sum::<f32>();
    // `whitespace-pre`:diff 行不换行 —— 少了它 gpui 会把长行回绕,而行高是恒定的
    // (uniform_list 的前提),回绕出来的第二行直接被裁掉,看着像「缺了半行」。
    // `flex_none`:宽度就是整行文字的宽,uniform_list 按它量横向可滚范围
    let text = div()
        .flex_none()
        .flex()
        .pl(px(gutter_w + 8.0))
        .pr(px(8.0))
        .whitespace_nowrap()
        .text_color(line_fg(kind))
        .child(selection::line_text(
            content,
            marks,
            word_bg(kind),
            &row,
            scroll,
        ));
    let numbers = pinned(scroll.x).w(px(gutter_w)).bg(ui::bg_base()).child(
        div()
            .size_full()
            .flex()
            // 行号列不可选:光标照旧是箭头
            .cursor_default()
            .when_some(line_bg(kind), |el, bg| el.bg(bg))
            .children(check)
            // 行号直接用 muted,不再叠 0.6 的透明度:叠上之后深浅两套主题里都只剩
            // 2:1 上下的对比,行号基本读不出来
            .children(gutters.iter().map(|(no, width)| {
                div()
                    .w(px(*width))
                    .flex_none()
                    .pr(px(8.0))
                    .text_right()
                    .text_color(ui::text_muted())
                    .child(no.clone())
            })),
    );

    div()
        .relative()
        .w_full()
        .flex()
        .h(px(line_height))
        .line_height(px(line_height))
        .cursor_text()
        .when_some(line_bg(kind), |el, bg| el.bg(bg))
        .child(text)
        .child(numbers)
        .into_any_element()
}

/// hunk 头(`@@ -a,b +c,d @@`),后面可以跟一组块操作按钮。
///
/// 整块钉在左缘([`pinned`]):横向滚到长行的尽头时,`@@` 与「暂存此块 / 丢弃此块」
/// 仍在视野左侧,不用先滚回去才点得到;底色铺满整行,分隔照旧。
///
/// ⚠️ 高度必须**恰好**是 `line_height`:uniform_list 按「量到的那一行」的高度
/// 摆所有行,这里多一像素边框,整份 diff 的行位就会与滚动条对不上。分隔靠底色;
/// 按钮比行高矮 4px、居中摆。
///
/// `gutter` 是整个行号栏的宽(含勾选框列);`check` 是块头的勾选框(一下勾上 / 取消这一块,
/// 宽 [`CHECK_W`]),占在行号栏最左。
pub(super) fn hunk_row(
    text: SharedString,
    line_height: f32,
    gutter: f32,
    check: Option<AnyElement>,
    actions: Option<AnyElement>,
    scroll_x: Pixels,
) -> AnyElement {
    let check_w = if check.is_some() { CHECK_W } else { 0.0 };
    let head = pinned(scroll_x)
        .flex()
        .items_center()
        .children(check)
        .child(div().w(px(gutter - check_w)).flex_none())
        .child(
            div()
                .flex_none()
                .px(px(8.0))
                .whitespace_nowrap()
                .text_color(ui::text_muted())
                .child(text),
        )
        .children(actions.map(|actions| {
            div()
                .flex_none()
                .h(px((line_height - 4.0).max(16.0)))
                .ml(px(8.0))
                .flex()
                .items_stretch()
                .gap(px(8.0))
                .child(actions)
        }));
    div()
        .relative()
        .w_full()
        .h(px(line_height))
        .bg(ui::bg_elevated())
        .child(head)
        .into_any_element()
}

/// 折着的上下文:一条可点的分隔行(点它 = 整段展开),写明折了多少行;`buttons` 时后面再跟
/// 「向下展开 20 行」「向上展开 20 行」「全部展开」(哪几颗见 [`fold::step_buttons`])。
/// 并排视图只有右栏带按钮(同一段别出两组),两栏的分隔行(连同中间栏里那一段)都能点;
/// 并排视图的行号在中间栏,这里 `gutter` 给 0。
///
/// 与 [`hunk_row`] 同一套:整块钉在左缘、高度**恰好**一行。分隔行按 `(gap, start)` 认是
/// 哪一段(见 [`DiffState::expand`]),点下去时行模型换过了也不会展开错。
pub(super) fn fold_row(
    state: &Entity<DiffState>,
    fold: &fold::Fold,
    pane: Pane,
    line_height: f32,
    gutter: f32,
    buttons: bool,
    scroll_x: Pixels,
) -> AnyElement {
    let (gap, start) = (fold.gap, fold.start);
    let expand = |how: fold::Expand| {
        let state = state.clone();
        move |cx: &mut App| {
            state.update(cx, |s, cx| {
                s.expand(gap, start, how);
                cx.notify();
            })
        }
    };
    let button = |key: &str, label: SharedString, how: fold::Expand| {
        let run = expand(how);
        text_button(
            SharedString::from(format!("git-diff-fold-{key}-{gap}-{start}")),
            label,
            true,
            false,
        )
        .on_click(move |_: &ClickEvent, _window, cx| {
            // 别让分隔行自己的点击(整段展开)也跟着触发
            cx.stop_propagation();
            run(cx);
        })
    };
    let actions = buttons.then(|| {
        let (down, up) = fold::step_buttons(fold);
        let step = fold::EXPAND_STEP.to_string();
        div()
            .flex_none()
            .h(px((line_height - 4.0).max(16.0)))
            .ml(px(8.0))
            .flex()
            .items_stretch()
            .gap(px(8.0))
            .when(down, |el| {
                el.child(button(
                    "down",
                    tr!("gitCommit", "expandDown", count = step.clone()).into(),
                    fold::Expand::Down,
                ))
            })
            .when(up, |el| {
                el.child(button(
                    "up",
                    tr!("gitCommit", "expandUp", count = step.clone()).into(),
                    fold::Expand::Up,
                ))
            })
            .child(button(
                "all",
                t("gitCommit", "expandAll").into(),
                fold::Expand::All,
            ))
    });
    let head = pinned(scroll_x)
        .flex()
        .items_center()
        .child(div().w(px(gutter)).flex_none())
        .child(
            div()
                .flex_none()
                .px(px(8.0))
                .whitespace_nowrap()
                .text_color(ui::text_secondary())
                .child(tr!(
                    "gitCommit",
                    "foldedLines",
                    count = fold.len.to_string()
                )),
        )
        .children(actions);
    let all = expand(fold::Expand::All);
    let pane_key = match pane {
        Pane::Inline => "inline",
        Pane::Old => "old",
        Pane::New => "new",
    };
    div()
        .id(SharedString::from(format!(
            "git-diff-fold-{pane_key}-{gap}-{start}"
        )))
        .relative()
        .w_full()
        .h(px(line_height))
        .cursor_pointer()
        .bg(fold_bg())
        .hover(|el| el.bg(fold_hover_bg()))
        .on_click(move |_: &ClickEvent, _window, cx| all(cx))
        .child(head)
        .into_any_element()
}

/// 折叠分隔行的淡色底(并排视图里连接两栏分隔行的带子、中间栏行号栏那一段同色)。
pub(super) fn fold_bg() -> gpui::Hsla {
    ui::with_alpha(ui::accent(), 0.07)
}

/// 悬停在分隔行上时的底色。
pub(super) fn fold_hover_bg() -> gpui::Hsla {
    ui::with_alpha(ui::accent(), 0.14)
}

/// 勾选框的三种样子(块头的勾选框会有「勾了一部分」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tick {
    Off,
    Partial,
    On,
}

impl Tick {
    /// 块里能勾的 `total` 行勾了 `picked` 行。
    pub(super) fn of(picked: usize, total: usize) -> Self {
        if picked == 0 {
            Tick::Off
        } else if picked >= total {
            Tick::On
        } else {
            Tick::Partial
        }
    }
}

/// 勾选框本身(14px 的方框;勾了一部分画成「−」)。
pub(super) fn tick_mark(tick: Tick) -> gpui::Div {
    match tick {
        Tick::On => git_ui::check_box(true, ui::accent()),
        Tick::Off => git_ui::check_box(false, ui::accent()),
        Tick::Partial => div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(14.0))
            .rounded(px(3.0))
            .border_1()
            .border_color(ui::accent())
            .bg(ui::accent())
            .child(git_ui::icon(ICON_MINUS, 11.0, ui::bg_base())),
    }
}

/// 勾选框那一格(宽 [`CHECK_W`]、高一整行)。点它只翻勾选:停止冒泡,不碰块操作与拖选。
pub(super) fn check_cell(
    id: impl Into<ElementId>,
    tick: Tick,
    on_toggle: impl Fn(&mut App) + 'static,
) -> AnyElement {
    let mark = tick_mark(tick);
    div()
        .id(id)
        .w(px(CHECK_W))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .child(mark)
        .on_click(move |_: &ClickEvent, _window, cx| {
            cx.stop_propagation();
            on_toggle(cx);
        })
        .into_any_element()
}

/// 勾选框列里不能勾的格子(上下文行、展开出来的行、块里没有这一种行的块头):占住列宽对齐。
pub(super) fn check_blank() -> AnyElement {
    div().w(px(CHECK_W)).h_full().flex_none().into_any_element()
}

/// 增删行的勾选框;不能勾的行(见 [`DiffState::pickable_line`])给空格子。
pub(super) fn line_check(state: &Entity<DiffState>, s: &DiffState, ix: usize) -> AnyElement {
    if !s.pickable_line(ix) {
        return check_blank();
    }
    let state = state.clone();
    check_cell(
        ("git-diff-pick", ix),
        Tick::of(usize::from(s.picks.contains(&ix)), 1),
        move |cx| {
            state.update(cx, |s, cx| {
                s.toggle_pick(ix);
                cx.notify();
            })
        },
    )
}

/// 行内视图块头的勾选框:一下勾上 / 取消这一块能勾的行,勾了一部分时画成「−」。块里没有
/// 增删行(不会出现)给空格子。并排视图的对应物在中间栏(见 [`ribbon`])。
pub(super) fn hunk_check(state: &Entity<DiffState>, s: &DiffState, hunk: usize) -> AnyElement {
    let lines = s.flat.pickable_in(hunk);
    if lines.is_empty() {
        return check_blank();
    }
    let picked = lines.iter().filter(|ix| s.picks.contains(ix)).count();
    let state = state.clone();
    let head = s.flat.hunk_key(hunk);
    check_cell(
        SharedString::from(format!("git-diff-pick-hunk-{hunk}")),
        Tick::of(picked, lines.len()),
        move |cx| {
            state.update(cx, |s, cx| {
                // 块号还指着渲染时那一块才翻(见 [`ChangeKey`])
                if head.as_ref().is_some_and(|head| s.hunk_is(hunk, head)) {
                    s.toggle_lines(s.flat.pickable_in(hunk));
                }
                cx.notify();
            })
        },
    )
}
