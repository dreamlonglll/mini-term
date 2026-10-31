//! 打开文件时的「定位到行」(终端文件链接、全局搜索点进来的两条路):目标行滚到视口
//! **顶上**,范围写法(`a.ts:20-35`)连同选中那几行。[`FileViewer`] 的又一半 `impl`。
//!
//! # 为什么不直接用 `set_cursor_position`
//!
//! 组件的 `set_cursor_position` 只管把光标「滚进视口」,两处不合用:
//!
//! - **新开的页签不滚**:滚动要上一帧的排版(`scroll_to` 开头 `last_layout` /
//!   `last_bounds` 是 `None` 就直接返回),刚建的编辑器一帧都没画过 —— 光标挪过去了,
//!   视口还停在文件头;已经开着的页签有排版,所以「再点一次才定位得过去」;
//! - **最小滚动**:目标行在视口下方时只滚到刚露出来,落在最底下一行。
//!
//! 于是光标 / 选区照旧交给组件摆,滚动自己算:目标行顶的纵坐标 → `set_scroll_offset`
//! (下一帧排版时生效,组件把它当成这一帧的滚动位置)。
//!
//! # 纵坐标从哪来
//!
//! - 上一帧排过版的那几十行([`EditorState::visible_row_range`]):用
//!   [`EditorState::range_to_bounds`] 量,软换行、折叠都准(与 [`super::git_gutter`]
//!   的对齐同一个办法);
//! - 视口外的行:不折行的文件行高均一,`行号 × 行高` 就是准数;软换行的(md / txt)
//!   显示行表没公开,先让组件按它自己的表把那一行滚进视口,下一帧再量。
//!
//! 内容总高同样没公开,滚动量的上限用下界代替(见 [`top_scroll`])。
//!
//! # 时机
//!
//! 编辑器把这一帧的排版写回 state 是在它自己的 paint 里,所以每一步都排在**文件页
//! 渲染过的那一帧之后**(`render` → [`FileViewer::schedule_reveal`] → `on_next_frame`):
//! 页签切走了、编辑器没画,就一直等着,不空转帧。新开的页签在定位落定前编辑器先
//! 不露面([`FileViewer::conceals_editor`]),否则会先闪一帧文件头。

use gpui::{Context, Pixels, Window, point, px};
use gpui_component::input::{EditorState, Position, RopeExt as _};

use super::FileViewer;
use super::file_kind::should_wrap;

/// 打开文件时要定位的行(1 起算)。`end` 有值 = 连同选中 `line..=end` 这几行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineTarget {
    pub line: u32,
    pub end: Option<u32>,
}

impl LineTarget {
    /// 只定位一行,不选中。
    pub fn line(line: u32) -> Self {
        Self { line, end: None }
    }

    /// 夹进一份 `total` 行的文件:起点越界不定位(`None`),终点越界夹到末行、
    /// 比起点小当没写。
    pub(super) fn clamp(self, total: u32) -> Option<Self> {
        (self.line >= 1 && self.line <= total).then(|| Self {
            line: self.line,
            end: self
                .end
                .filter(|&end| end >= self.line)
                .map(|end| end.min(total)),
        })
    }
}

/// 还没落定的一次定位。
#[derive(Clone, Copy, Debug)]
pub(super) struct PendingReveal {
    target: LineTarget,
    /// 已经让组件把目标行滚进过视口(软换行文件的视口外行要先这样才量得到)
    nudged: bool,
    /// 发起定位时编辑器还没画过:落定之前先不露面
    conceal: bool,
}

/// 一步定位的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// 滚动量已交给编辑器(下一帧生效),定位落定
    Done,
    /// 编辑器还没排过版,等它画出来
    NotLaidOut,
    /// 目标行在视口外、又量不出纵坐标:已让组件把它滚进视口,下一帧再量
    Nudged,
}

/// 行顶在 `top`、高 `height` 的那一行顶到视口顶时的滚动量(向下为正,从内容顶算起)。
///
/// 文件尾那一屏的行顶不到最上面:滚动量夹在「内容高 − 视口高」以内,多给了组件会先
/// 画出一帧越界的空白、再拉回来。内容总高组件没公开,用下界代替 —— 下面每行至少占
/// 一个行高:不折行、没折叠时就是准数;软换行时偏小,只会少滚,目标行仍整行在视口里。
pub(super) fn top_scroll(
    top: Pixels,
    height: Pixels,
    rows_below: usize,
    line_height: Pixels,
    viewport: Pixels,
) -> Pixels {
    let content = top + height + line_height * rows_below as f32;
    top.min((content - viewport).max(px(0.)))
}

/// 摆光标 / 选区:单行把光标放到行首,范围选中起点行首到终点行尾。组件顺带把光标
/// 滚进视口(排过版时才滚得动,滚多少由 [`scroll_to_top`] 接着改)。
fn place_cursor(
    state: &mut EditorState,
    target: LineTarget,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    let row = target.line - 1;
    match target.end {
        Some(end) => {
            let rope = state.text();
            let range =
                rope.line_start_offset(row as usize)..rope.line_end_offset(end as usize - 1);
            state.set_selected_range(range, cx);
            // 与 `set_cursor_position` 一样把焦点交给编辑器
            state.focus(window, cx);
        }
        None => state.set_cursor_position(Position::new(row, 0), window, cx),
    }
}

/// 上一帧排过版的那一行的 `(行顶, 行高)`,行顶从内容顶算起;不在排版里给 `None`。
fn row_extent(state: &EditorState, row: usize) -> Option<(Pixels, Pixels)> {
    // `range_to_bounds` 对排版之外的偏移会落到最近的可见行上(饱和减法),先挡掉
    if !state.visible_row_range()?.contains(&row) {
        return None;
    }
    let rope = state.text();
    let range = rope.line_start_offset(row)..rope.line_end_offset(row);
    let bounds = state.range_to_bounds(&range)?;
    // 两者同一份排版、同一个原点,相减就是内容坐标
    let origin = state.text_bounds()?.origin.y;
    Some((bounds.origin.y - origin, bounds.size.height))
}

/// 把目标行滚到视口顶上(办法见模块注释)。
fn scroll_to_top(
    state: &mut EditorState,
    pending: &PendingReveal,
    soft_wrap: bool,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) -> Step {
    let Some(line_height) = state.line_height() else {
        return Step::NotLaidOut;
    };
    let row = pending.target.line as usize - 1;
    let (top, height) = match row_extent(state, row) {
        Some(extent) => extent,
        None if !soft_wrap => (line_height * row as f32, line_height),
        None if !pending.nudged => {
            place_cursor(state, pending.target, window, cx);
            return Step::Nudged;
        }
        // 滚进视口了还量不到(不该发生):停在组件滚到的位置
        None => return Step::Done,
    };
    let rows_below = state.text().lines_len().saturating_sub(row + 1);
    let viewport = state.input_bounds().size.height;
    let y = top_scroll(top, height, rows_below, line_height, viewport);
    state.set_scroll_offset(point(px(0.), -y), cx);
    Step::Done
}

impl FileViewer {
    /// 摆光标 / 选区,再把目标行滚到视口顶上。编辑器还没画过(新开的页签)就等它画出来。
    pub(super) fn reveal(
        &mut self,
        target: LineTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.editor.clone() else {
            return;
        };
        let laid_out = editor.update(cx, |state, cx| {
            place_cursor(state, target, window, cx);
            state.line_height().is_some()
        });
        self.pending_reveal = Some(PendingReveal {
            target,
            nudged: false,
            conceal: !laid_out,
        });
        // 排过版就当场算:开着的页签多半一步落定,不必等一帧
        if laid_out {
            self.advance_reveal(window, cx);
        }
        cx.notify();
    }

    /// 渲染时调:有没落定的定位,就排到这一帧画完之后接着做(那时编辑器才有这一帧的
    /// 排版)。
    pub(super) fn schedule_reveal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_reveal.is_none() || self.reveal_scheduled {
            return;
        }
        self.reveal_scheduled = true;
        cx.on_next_frame(window, |this, window, cx| {
            this.reveal_scheduled = false;
            this.advance_reveal(window, cx);
        });
    }

    fn advance_reveal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut pending) = self.pending_reveal else {
            return;
        };
        let Some(editor) = self.editor.clone() else {
            self.pending_reveal = None;
            return;
        };
        let soft_wrap = should_wrap(&self.file_name());
        let step = editor.update(cx, |state, cx| {
            scroll_to_top(state, &pending, soft_wrap, window, cx)
        });
        match step {
            // 编辑器没画出来(页签切走了):等下一次渲染
            Step::NotLaidOut => return,
            Step::Nudged => {
                pending.nudged = true;
                self.pending_reveal = Some(pending);
            }
            Step::Done => self.pending_reveal = None,
        }
        // 要再画一帧:没落定的靠它排下一步,落定的靠它撤掉遮挡
        cx.notify();
    }

    /// 新开的页签定位落定之前编辑器先不露面(见模块注释「时机」)。
    pub(super) fn conceals_editor(&self) -> bool {
        self.pending_reveal.is_some_and(|pending| pending.conceal)
    }
}

#[cfg(test)]
#[path = "reveal_tests.rs"]
mod tests;
