//! 并排视图(对标 IDEA / Winc 的并排 diff):两栏**各自连续排行** —— 没有占位空行、没有
//! `@@` 头;两栏之间是一条中间栏(左栏行号 | 连接带 | 右栏行号,见 [`ribbon`]),每处改动
//! 画一条带子把左右两边连起来。两栏共用一个逻辑滚动位置。
//!
//! # 行模型
//!
//! [`Split`] 是 `Flat` 重排行模型时顺手排出的两栏行序:块里的上下文行、展开出来的上下文行
//! 两栏各一行([`SideRow::Line`],同一个拍平行),折着的上下文两栏各一条分隔行
//! ([`SideRow::Fold`]);块里连续的增删行是**一处改动**([`Change`]):删除行只进左栏、新增行
//! 只进右栏,两侧行数可以不同。改动按两侧有没有行分三种([`ChangeKind`]):两侧都有 = 修改、
//! 只有右侧 = 新增、只有左侧 = 删除。
//!
//! # 滚动映射
//!
//! 移植自 Winc(Apache-2.0)`crates/winc-editor/src/diff_view.rs` 的 `layout` 里的分段点与
//! `row_at` / `side_scroll`:
//!
//! - 滚动位置是两栏共用的坐标(单位:行):改动以外两栏逐行对应,每处改动占两侧里较多的
//!   行数,较短的一侧按比例移动(0 行的一侧不动)。只按右栏滚的话,左栏一大段删除在右栏
//!   只占一个点,滚动时会被直接越过;
//! - 两栏在同一个**对齐点**上对齐([`anchor`]):滚过半屏之后对齐点在视口正中,开头不足半屏
//!   时对齐点跟着往上挪、到顶时在顶边。同一处改动因此在两栏里一起进入、一起离开视口;
//! - 与 Winc 的差别:Winc 的编辑区能滚过末行,这里不能(与原先的 uniform_list 同口径),
//!   于是末尾半屏对称地把对齐点往下挪、到底时在底边 —— 否则最后一处改动里较短的那一侧
//!   滚不到自己的末行。
//!
//! # 两栏怎么跟着滚动位置走
//!
//! 两栏仍是各自的 `uniform_list`(虚拟化:只给视野里的行建元素),但它们自己的滚轮处理被
//! 截下([`scroll_layer`]):滚轮在捕获阶段改共用的滚动位置并停止冒泡,每帧
//! ([`DiffState::sync_split`])再按滚动位置算出两栏各自的顶边、写回两个列表的偏移。横向两栏
//! 照旧同步、各按自己的范围钳住(原先 `sync_columns` 的口径);纵向滚动条只有一条,画在
//! 右缘、表示共用的滚动位置([`SplitScroll`])。

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, Entity, HitboxBehavior, Hsla, IntoElement,
    ListHorizontalSizingBehavior, ParentElement, Pixels, Point, ScrollHandle, ScrollWheelEvent,
    Size, Styled, canvas, div, fill, point, prelude::FluentBuilder as _, px, size, uniform_list,
};
use gpui_component::scroll::{Scrollbar, ScrollbarHandle, ScrollbarMode};

use super::ribbon::{self, Mid, Shade};
use super::selection::{self, Pane, RowText};
use super::{DiffState, FlatLine, HunkUi, fold_row, h_scrollbar};
use crate::i18n::t;
use crate::ui;

/// 左栏(旧)
pub(super) const LEFT: usize = 0;
/// 右栏(新)
pub(super) const RIGHT: usize = 1;

/// 视口高度还没量到(打开后的第一帧)时按这么多行算可见区:多建几行无妨,下一帧就按量到的。
const FALLBACK_ROWS: f32 = 80.0;

// ─── 行模型 ───────────────────────────────────────────────────

/// 并排视图一栏里的一行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SideRow {
    /// `Flat::lines` 的下标
    Line(usize),
    /// 折着的上下文(`Flat::folds` 的下标):两栏各一条分隔行
    Fold(usize),
}

/// 一处改动的种类,决定配色(见 [`ribbon::shade`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChangeKind {
    /// 两侧都有行
    Modified,
    /// 只有右侧有行
    Added,
    /// 只有左侧有行
    Deleted,
}

/// 一处改动:块里连续的增删行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Change {
    /// 所在的 hunk(块操作按它交给后端)
    pub(super) hunk: usize,
    /// 在 `Flat::lines` 里的区间(块里的行是连续的)
    pub(super) lines: Range<usize>,
    /// 两栏各自的行区间;该侧没有行时是插入位置上的空区间
    pub(super) rows: [Range<usize>; 2],
}

impl Change {
    pub(super) fn kind(&self) -> ChangeKind {
        match (self.rows[LEFT].is_empty(), self.rows[RIGHT].is_empty()) {
            (false, false) => ChangeKind::Modified,
            (true, _) => ChangeKind::Added,
            (false, true) => ChangeKind::Deleted,
        }
    }
}

/// 并排视图的两栏行序、改动与滚动映射(见模块注释)。
#[derive(Debug, Default)]
pub(super) struct Split {
    pub(super) rows: [Vec<SideRow>; 2],
    /// 按位置排列的各处改动
    pub(super) changes: Vec<Change>,
    /// 各条分隔行在两栏里的行号,与 `Flat::folds` 同序
    pub(super) fold_rows: Vec<[usize; 2]>,
    /// 每个 hunk 第一行的滚动位置(F7 跳转的落点)
    hunk_pos: Vec<f64>,
    /// 滚动位置的分段点:每处改动的开头、结尾各一个,`(滚动位置, [左栏行, 右栏行])`
    knots: Vec<(f64, [f64; 2])>,
    /// 滚动位置的总长(行)
    total: f64,
    /// 给 uniform_list 量宽用的「最宽的那一行」,两栏各一个
    pub(super) widest: [usize; 2],
}

impl Split {
    // ── 排行(`Flat::relayout` 逐段调用) ──

    /// 下一个 hunk 从这里开始。
    pub(super) fn begin_hunk(&mut self) {
        self.hunk_pos.push(self.total);
    }

    /// 两栏同一行(上下文行)。
    pub(super) fn push_line(&mut self, ix: usize) {
        for side in [LEFT, RIGHT] {
            self.rows[side].push(SideRow::Line(ix));
        }
        self.total += 1.0;
    }

    /// 两栏各一条分隔行,返回它在两栏里的行号。
    pub(super) fn push_fold(&mut self, fold: usize) -> [usize; 2] {
        let at = [self.rows[LEFT].len(), self.rows[RIGHT].len()];
        for side in [LEFT, RIGHT] {
            self.rows[side].push(SideRow::Fold(fold));
        }
        self.fold_rows.push(at);
        self.total += 1.0;
        at
    }

    /// 一处改动:删除行 `old` 进左栏、新增行 `new` 进右栏;滚动位置上占两侧里较多的行数。
    pub(super) fn push_change(
        &mut self,
        hunk: usize,
        lines: Range<usize>,
        old: &[usize],
        new: &[usize],
    ) {
        let start = [self.rows[LEFT].len(), self.rows[RIGHT].len()];
        self.rows[LEFT].extend(old.iter().map(|&ix| SideRow::Line(ix)));
        self.rows[RIGHT].extend(new.iter().map(|&ix| SideRow::Line(ix)));
        let end = [self.rows[LEFT].len(), self.rows[RIGHT].len()];
        let as_rows = |at: [usize; 2]| [at[LEFT] as f64, at[RIGHT] as f64];
        self.knots.push((self.total, as_rows(start)));
        self.total += old.len().max(new.len()) as f64;
        self.knots.push((self.total, as_rows(end)));
        self.changes.push(Change {
            hunk,
            lines,
            rows: [start[LEFT]..end[LEFT], start[RIGHT]..end[RIGHT]],
        });
    }

    // ── 查询 ──

    /// 第 `side` 栏第 `row` 行落在哪处改动里。
    pub(super) fn change_at(&self, side: usize, row: usize) -> Option<usize> {
        let ix = self.changes.partition_point(|c| c.rows[side].end <= row);
        self.changes
            .get(ix)
            .filter(|c| c.rows[side].contains(&row))
            .map(|_| ix)
    }

    /// 与两栏可见行区间有交集(插入点落在区间里也算)的改动。
    pub(super) fn changes_in(&self, view: &[Range<usize>; 2]) -> Vec<usize> {
        let touches = |c: &Change, side: usize| {
            c.rows[side].end >= view[side].start && c.rows[side].start <= view[side].end
        };
        let from = [LEFT, RIGHT]
            .map(|side| {
                self.changes
                    .partition_point(|c| c.rows[side].end < view[side].start)
            })
            .into_iter()
            .min()
            .unwrap_or(0);
        (from..self.changes.len())
            .take_while(|&ix| {
                let c = &self.changes[ix];
                c.rows[LEFT].start <= view[LEFT].end || c.rows[RIGHT].start <= view[RIGHT].end
            })
            .filter(|&ix| touches(&self.changes[ix], LEFT) || touches(&self.changes[ix], RIGHT))
            .collect()
    }

    /// 与两栏可见行区间有交集的分隔行(`fold_rows` 的下标)。
    pub(super) fn folds_in(&self, view: &[Range<usize>; 2]) -> Vec<usize> {
        let inside = |at: &[usize; 2]| (0..2).any(|side| view[side].contains(&at[side]));
        self.fold_rows
            .iter()
            .enumerate()
            .filter(|(_, at)| inside(at))
            .map(|(ix, _)| ix)
            .collect()
    }

    /// 一侧为空(新文件 / 删掉的文件):整份文件是一处改动,带子一端是点、另一端是全文,
    /// 画出来是一整块颜色,所以不画带子与插入线,空的一侧写「空文件」(Winc 同款取舍)。
    pub(super) fn empty_sides(&self) -> [bool; 2] {
        let changed = !self.changes.is_empty();
        [LEFT, RIGHT].map(|side| changed && self.rows[side].is_empty())
    }

    // ── 滚动映射 ──

    /// 滚动位置的总长(行)。
    pub(super) fn total(&self) -> f64 {
        self.total
    }

    /// 第 `hunk` 块第一行的滚动位置。
    pub(super) fn hunk_pos(&self, hunk: usize) -> f64 {
        self.hunk_pos.get(hunk).copied().unwrap_or(0.0)
    }

    /// 第 `i` 个分段点的前一个 `(滚动位置, 第 side 栏的行)`;`i` 为 0 时是起点。
    fn knot(&self, side: usize, i: usize) -> (f64, f64) {
        i.checked_sub(1)
            .and_then(|i| self.knots.get(i))
            .map_or((0.0, 0.0), |&(at, rows)| (at, rows[side]))
    }

    /// 滚动位置 `pos` 对应第 `side` 栏的第几行(可以是小数):二分找第一个不小于 `pos` 的
    /// 分段点,在它与前一个之间插值。改动以外两栏逐行对应,改动里按比例。
    pub(super) fn row_at(&self, side: usize, pos: f64) -> f64 {
        let i = self.knots.partition_point(|k| k.0 < pos);
        let from = self.knot(side, i);
        let Some(&(at, rows)) = self.knots.get(i) else {
            return from.1 + (pos - from.0);
        };
        let to = (at, rows[side]);
        let t = if to.0 > from.0 {
            (pos - from.0) / (to.0 - from.0)
        } else {
            1.0
        };
        from.1 + (to.1 - from.1) * t
    }

    /// 视口 `visible` 行时滚动位置的上限。
    pub(super) fn max_pos(&self, visible: f64) -> f64 {
        (self.total - visible).max(0.0)
    }

    /// 滚动位置为 `pos`、视口 `visible` 行时两栏顶边各是第几行(可以是小数):两栏在同一个
    /// 对齐点([`anchor`])上对齐,再各按自己的行数钳住(uniform_list 滚不过自己的末行)。
    pub(super) fn side_tops(&self, pos: f64, visible: f64) -> [f64; 2] {
        let max_pos = self.max_pos(visible);
        let pos = pos.clamp(0.0, max_pos);
        let a = anchor(pos, max_pos, visible);
        [LEFT, RIGHT].map(|side| {
            let top = self.row_at(side, pos + a) - a;
            let max_top = (self.rows[side].len() as f64 - visible).max(0.0);
            top.clamp(0.0, max_top)
        })
    }
}

/// 对齐点离视口顶边几行:开头的半屏里从顶边(0)匀速挪到正中,中段停在正中,末尾的半屏
/// 里再挪到底边。滚到顶时两栏都从第一行起,滚到底时两栏都停在自己的末行。内容不满两屏
/// 时没有「中段」,对齐点从顶边匀速挪到底边。
pub(super) fn anchor(pos: f64, max_pos: f64, visible: f64) -> f64 {
    if max_pos <= 0.0 {
        return 0.0;
    }
    if max_pos < visible {
        return pos / max_pos * visible;
    }
    let half = visible / 2.0;
    if pos < half {
        pos
    } else if pos > max_pos - half {
        visible - (max_pos - pos)
    } else {
        half
    }
}

// ─── 共用的滚动位置 ───────────────────────────────────────────

/// 两栏共用的纵向滚动位置(像素,≥ 0)与正文视口。纵向滚动条拖动时只拿得到它
/// (`ScrollbarHandle::set_offset` 没有 `cx`),所以放在共享的格子里,下一帧
/// [`DiffState::sync_split`] 再按它摆两栏(上限也在那时按最新的总长钳)。
#[derive(Clone, Default)]
pub(super) struct SplitScroll(Rc<RefCell<ScrollState>>);

#[derive(Default)]
struct ScrollState {
    pos: f32,
    /// 滚动位置的总长(像素)
    total: f32,
    /// 正文视口(上一帧量到的)
    viewport: Bounds<Pixels>,
}

impl SplitScroll {
    pub(super) fn pos(&self) -> f32 {
        self.0.borrow().pos
    }

    pub(super) fn set_pos(&self, pos: f32) {
        self.0.borrow_mut().pos = pos.max(0.0);
    }

    fn viewport(&self) -> Bounds<Pixels> {
        self.0.borrow().viewport
    }

    fn set_viewport(&self, bounds: Bounds<Pixels>) {
        self.0.borrow_mut().viewport = bounds;
    }

    fn set_total(&self, total: f32) {
        self.0.borrow_mut().total = total;
    }
}

impl ScrollbarHandle for SplitScroll {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.viewport()
    }

    fn offset(&self) -> Point<Pixels> {
        point(px(0.0), px(-self.pos()))
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.set_pos(-f32::from(offset.y));
    }

    fn content_size(&self) -> Size<Pixels> {
        let s = self.0.borrow();
        let height = s.total.max(f32::from(s.viewport.size.height));
        size(s.viewport.size.width, px(height))
    }
}

/// 这一帧两栏的纵向几何:顶边各是第几行(小数)、行高、视口高(上一帧量到的,0 = 还没量)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SplitFrame {
    pub(super) tops: [f64; 2],
    pub(super) lh: f32,
    pub(super) view_h: f32,
}

impl SplitFrame {
    /// 第 `side` 栏第 `row` 行的顶边(相对正文顶边)。
    pub(super) fn y(&self, side: usize, row: usize) -> f32 {
        ((row as f64 - self.tops[side]) * f64::from(self.lh)) as f32
    }

    /// 第 `side` 栏(共 `len` 行)这一帧看得见的行。
    pub(super) fn rows(&self, side: usize, len: usize) -> Range<usize> {
        let visible = if self.view_h > 0.0 {
            self.view_h / self.lh
        } else {
            FALLBACK_ROWS
        };
        let top = self.tops[side].max(0.0);
        let first = top.floor() as usize;
        let last = ((top + f64::from(visible)).ceil() as usize + 1).min(len);
        first.min(last)..last
    }
}

// ─── 状态:同步、滚轮、跳转 ─────────────────────────────────────

impl DiffState {
    /// 并排两栏的列表滚动句柄(视口、偏移、可滚范围都从它读)。
    fn side_handle(&self, side: usize) -> ScrollHandle {
        let handle = if side == LEFT {
            &self.left_scroll
        } else {
            &self.right_scroll
        };
        handle.0.borrow().base_handle.clone()
    }

    /// 两栏眼下的横向偏移:上一帧之后谁动了(横向滚动条)谁当主,另一边**先按自己的范围钳住**
    /// 再跟上 —— 不钳的话窄的一栏会在下一帧把越界的 x 抹回去、被当成「它动了」,反过来把
    /// 宽的那栏拽回来,越同步越抖。
    fn synced_x(&self) -> [Pixels; 2] {
        let handles = [self.side_handle(LEFT), self.side_handle(RIGHT)];
        let mut x = handles.clone().map(|h| h.offset().x);
        if x[LEFT] != self.synced_x[LEFT] {
            x[RIGHT] = clamp_x(x[LEFT], &handles[RIGHT]);
        } else if x[RIGHT] != self.synced_x[RIGHT] {
            x[LEFT] = clamp_x(x[RIGHT], &handles[LEFT]);
        }
        x
    }

    /// 每帧一次(画并排视图之前):横向同步,纵向按共用的滚动位置摆两栏。
    pub(super) fn sync_split(&mut self) -> SplitFrame {
        let x = self.synced_x();
        self.place_split(x)
    }

    /// 按共用的滚动位置算出两栏的顶边,连同横向偏移 `x` 写回两个列表。
    fn place_split(&mut self, x: [Pixels; 2]) -> SplitFrame {
        let lh = self.line_height();
        let view_h = f32::from(self.split_scroll.viewport().size.height);
        let split = &self.flat.split;
        let visible = f64::from(view_h / lh);
        self.split_scroll.set_total(split.total() as f32 * lh);
        let max = (split.max_pos(visible) * f64::from(lh)) as f32;
        let pos = self.split_scroll.pos().min(max);
        self.split_scroll.set_pos(pos);
        let tops = split.side_tops(f64::from(pos / lh), visible);
        for side in [LEFT, RIGHT] {
            let handle = self.side_handle(side);
            let target = point(x[side], px(-(tops[side] as f32) * lh));
            if handle.offset() != target {
                handle.set_offset(target);
            }
        }
        self.synced_x = x;
        SplitFrame { tops, lh, view_h }
    }

    /// 滚轮(像素,`delta` 与 gpui 的滚动偏移同号):纵向改共用的滚动位置,横向改指针下那一栏、
    /// 另一栏按自己的范围跟上。两个方向都有时只取幅度大的那个。动了返回 `true`。
    pub(super) fn wheel_split(&mut self, delta: Point<Pixels>, at: Point<Pixels>) -> bool {
        let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
        let side = if at.x < self.side_handle(LEFT).bounds().right() {
            LEFT
        } else {
            RIGHT
        };
        if dx.abs() > dy.abs() {
            self.scroll_split(side, -dx, 0.0)
        } else {
            self.scroll_split(side, 0.0, -dy)
        }
    }

    /// 往后滚 `dy`(纵向,两栏共用)、`dx`(横向,第 `side` 栏,另一栏跟上),当场写回两个
    /// 列表(拖选的自动滚动紧接着就要按新偏移算落点)。动了返回 `true`。
    pub(super) fn scroll_split(&mut self, side: usize, dx: f32, dy: f32) -> bool {
        let before = (self.split_scroll.pos(), self.synced_x);
        let mut x = self.synced_x();
        if dx != 0.0 {
            let handles = [self.side_handle(LEFT), self.side_handle(RIGHT)];
            x[side] = clamp_x(x[side] - px(dx), &handles[side]);
            x[1 - side] = clamp_x(x[side], &handles[1 - side]);
        }
        if dy != 0.0 {
            self.split_scroll.set_pos(self.split_scroll.pos() + dy);
        }
        self.place_split(x);
        (self.split_scroll.pos(), self.synced_x) != before
    }

    /// 跳到第 `hunk` 块:它的第一行停在顶边(较长的一侧恰在顶边,较短的一侧稍靠下,两栏
    /// 都在视野里,见模块注释「滚动映射」)。
    pub(super) fn split_jump(&mut self, hunk: usize) {
        let pos = self.flat.split.hunk_pos(hunk) as f32 * self.line_height();
        self.split_scroll.set_pos(pos);
    }

    /// 两栏回到顶部、左缘(换文件时)。
    pub(super) fn reset_split(&mut self) {
        self.split_scroll.set_pos(0.0);
        for side in [LEFT, RIGHT] {
            self.side_handle(side).set_offset(point(px(0.0), px(0.0)));
        }
        self.synced_x = [px(0.0); 2];
    }
}

/// 横向偏移按该栏自己的范围钳住(≤ 0)。
fn clamp_x(x: Pixels, handle: &ScrollHandle) -> Pixels {
    let max = f32::from(handle.max_offset().x).max(0.0);
    px(f32::from(x).clamp(-max, 0.0))
}

// ─── 渲染 ─────────────────────────────────────────────────────

/// `SideBySideView`(`DiffModal.tsx:62-152`)的新排版:左栏正文 | 中间栏(左栏行号、带子、
/// 右栏行号)| 右栏正文。块操作按钮在中间栏(见 [`ribbon`]),只有单文件 diff 且允许块操作
/// 时才有(`hunks`)。
pub(super) fn render_side_by_side(
    state: &Entity<DiffState>,
    hunks: Option<HunkUi>,
    cx: &mut App,
) -> AnyElement {
    // 每帧一次:横向谁滚了另一边跟上,纵向按共用的滚动位置摆两栏(见 [`DiffState::sync_split`])
    let frame = state.update(cx, |s, _| s.sync_split());
    let s = state.read(cx);
    let mid = Mid::new(s.can_pick(), hunks.is_some());
    let empty = s.flat.split.empty_sides();
    let left = text_pane(state, s, LEFT, &frame, empty[LEFT]);
    let middle = ribbon::middle(state, s, &frame, mid, hunks.as_ref());
    let right = text_pane(state, s, RIGHT, &frame, empty[RIGHT]);
    let scroll = s.split_scroll.clone();
    div()
        .relative()
        .size_full()
        .flex()
        .text_size(ui::font_px(s.font_size))
        .child(left)
        .child(middle)
        .child(right)
        // 共用滚动位置的那一条纵向滚动条(常显,与两栏的横向滚动条同一口径)。壳的理由见
        // `h_scrollbar`
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .child(
                    Scrollbar::vertical(&scroll)
                        .id("git-diff-split-vbar")
                        .mode(ScrollbarMode::Always),
                ),
        )
        .child(scroll_layer(state, scroll, frame))
        .child(selection::drag_listener(state))
        .into_any_element()
}

/// 一栏正文:uniform_list(只有正文与分隔行,行号在中间栏)+ 插入线 + 空文件说明 + 横向滚动条。
fn text_pane(
    state: &Entity<DiffState>,
    s: &DiffState,
    side: usize,
    frame: &SplitFrame,
    empty: bool,
) -> AnyElement {
    let pane = if side == LEFT { Pane::Old } else { Pane::New };
    let split = &s.flat.split;
    let count = split.rows[side].len();
    let widest = (count > 0).then_some(split.widest[side]);
    let handle = if side == LEFT {
        s.left_scroll.clone()
    } else {
        s.right_scroll.clone()
    };
    let lh = frame.lh;
    let rows_state = state.clone();
    let list_handle = handle.clone();
    let list = uniform_list(
        if side == LEFT {
            "git-diff-left"
        } else {
            "git-diff-right"
        },
        count,
        move |range, _window, cx: &mut App| {
            let selection_bg = selection::selection_bg(cx);
            let s = rows_state.read(cx);
            s.retain_layouts(pane, &range);
            let scroll = list_handle.0.borrow().base_handle.offset();
            let split = &s.flat.split;
            range
                .map(|i| match split.rows[side][i] {
                    // 分隔行两栏都画、都能点;展开按钮只在右栏(同一段别出两组)
                    SideRow::Fold(f) => fold_row(
                        &rows_state,
                        &s.flat.folds[f],
                        pane,
                        lh,
                        0.0,
                        side == RIGHT,
                        scroll.x,
                    ),
                    SideRow::Line(ix) => {
                        let shade = split
                            .change_at(side, i)
                            .map(|c| ribbon::shade(split.changes[c].kind()));
                        line_row(
                            &s.flat.lines[ix],
                            shade,
                            &s.flat.marks[ix],
                            lh,
                            scroll,
                            s.row_text(pane, i, ix, selection_bg),
                        )
                    }
                })
                .collect::<Vec<_>>()
        },
    )
    .size_full()
    .track_scroll(&handle)
    // 见 `git_diff` 模块注释第 3 条:量宽只量这一行
    .with_width_from_item(widest)
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained);

    // 这一栏没有行的改动:在插入位置画一道线(横贯整栏,中间栏里那一段由 `ribbon` 画)
    let marks: Vec<(f32, Hsla)> = if empty {
        Vec::new()
    } else {
        let view = [LEFT, RIGHT].map(|side| frame.rows(side, split.rows[side].len()));
        split
            .changes_in(&view)
            .into_iter()
            .map(|c| &split.changes[c])
            .filter(|c| c.rows[side].is_empty())
            .map(|c| {
                (
                    frame.y(side, c.rows[side].start),
                    ribbon::shade(c.kind()).edge,
                )
            })
            .collect()
    };
    let insertion = canvas(
        |_, _, _| {},
        move |bounds, (), window, _cx| {
            for (y, color) in &marks {
                let origin = point(bounds.left(), bounds.top() + px(*y - 0.5));
                window.paint_quad(fill(
                    Bounds::new(origin, size(bounds.size.width, px(1.0))),
                    *color,
                ));
            }
        },
    )
    .absolute()
    .size_full();

    let body = div()
        .relative()
        .flex_1()
        .min_w_0()
        .h_full()
        .child(list)
        .child(insertion)
        .when(empty, |el| el.child(empty_note(lh)))
        .child(h_scrollbar(
            if side == LEFT {
                "git-diff-left-hbar"
            } else {
                "git-diff-right-hbar"
            },
            &handle,
        ));
    // 选区限定在起拖的那一栏:两栏各挂各的,按下时记下是哪一栏
    selection::text_mouse(body, state, pane).into_any_element()
}

/// 一侧为空(新文件 / 删掉的文件)时那一栏的说明,与 Winc 一样放在第三行的高度、居中。
fn empty_note(lh: f32) -> impl IntoElement {
    div()
        .absolute()
        .top(px(lh * 2.0))
        .left_0()
        .right_0()
        .h(px(lh))
        .flex()
        .items_center()
        .justify_center()
        .text_size(ui::font_px(12.0))
        .text_color(ui::text_muted())
        .child(t("gitCommit", "emptySide"))
}

/// 并排视图的一行正文。行号在中间栏,这里只有正文:改动里的行铺该处改动种类的底色
/// (`shade`,见 [`ribbon::shade`]),改了的词用同色系深一档;字一律是正文色(左右位置已经
/// 说明了新旧,红绿字压在蓝底上反而打架)。
///
/// 与 `diff_line_row` 同一套:行根 `w_full()`(底色铺满整行),正文 `flex_none` + 不换行
/// (uniform_list 按它量宽),行高同时设成文字行高(跨行选区不叠出横纹)。
fn line_row(
    line: &FlatLine,
    shade: Option<Shade>,
    marks: &[Range<usize>],
    lh: f32,
    scroll: Point<Pixels>,
    row: RowText,
) -> AnyElement {
    let word = shade.map_or_else(ui::diff_mod_word_bg, |shade| shade.word);
    let text = div()
        .flex_none()
        .flex()
        .px(px(8.0))
        .whitespace_nowrap()
        .text_color(ui::text_primary())
        .child(selection::line_text(
            line.content.clone(),
            marks,
            word,
            &row,
            scroll,
        ));
    div()
        .relative()
        .w_full()
        .flex()
        .h(px(lh))
        .line_height(px(lh))
        .cursor_text()
        .when_some(shade, |el, shade| el.bg(shade.line))
        .child(text)
        .into_any_element()
}

/// 盖在整个并排视图上的一层(不画东西):
///
/// - prepaint 量正文视口(两栏摆位、纵向滚动条都按它)。这一帧摆位用的是上一帧量到的高度,
///   对不上(第一帧、改了窗口大小)就再要一帧;
/// - 滚轮在**捕获阶段**截下:改共用的滚动位置(见 [`DiffState::wheel_split`])并停止冒泡,
///   两个 uniform_list 自己的滚轮处理(冒泡阶段)就不会再各滚各的。
fn scroll_layer(
    state: &Entity<DiffState>,
    scroll: SplitScroll,
    frame: SplitFrame,
) -> impl IntoElement {
    let state = state.clone();
    let lh = frame.lh;
    canvas(
        move |bounds, window, _cx| {
            if (f32::from(bounds.size.height) - frame.view_h).abs() > 0.5 {
                window.request_animation_frame();
            }
            scroll.set_viewport(bounds);
            window.insert_hitbox(bounds, HitboxBehavior::Normal)
        },
        move |_bounds, hitbox, window, _cx| {
            window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                if !phase.capture() || !hitbox.should_handle_scroll(window) {
                    return;
                }
                let delta = event.delta.pixel_delta(px(lh));
                let position = event.position;
                state.update(cx, |s, cx| {
                    if s.wheel_split(delta, position) {
                        cx.notify();
                    }
                });
                cx.stop_propagation();
            });
        },
    )
    .absolute()
    .size_full()
}

#[cfg(test)]
mod tests {
    use mt_project::git::{DiffHunk, DiffLine, diff_two_texts};

    use super::super::{flatten, flatten_diff, fold};
    use super::*;

    fn line(kind: &str, content: &str) -> DiffLine {
        DiffLine {
            kind: kind.to_string(),
            content: content.to_string(),
            old_lineno: Some(1),
            new_lineno: Some(1),
        }
    }

    fn hunk(lines: Vec<DiffLine>) -> DiffHunk {
        DiffHunk {
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            lines,
        }
    }

    fn rows_of(split: &Split, side: usize) -> Vec<SideRow> {
        split.rows[side].clone()
    }

    /// 两栏各自连续排行:没有占位空行,删除行只在左栏、新增行只在右栏,上下文两栏同一行。
    #[test]
    fn 两栏各自连续排行() {
        let f = flatten(&[hunk(vec![
            line("context", "a"),
            line("delete", "b"),
            line("add", "B1"),
            line("add", "B2"),
            line("add", "B3"),
            line("context", "c"),
        ])]);
        use SideRow::Line;
        assert_eq!(rows_of(&f.split, LEFT), vec![Line(0), Line(1), Line(5)]);
        assert_eq!(
            rows_of(&f.split, RIGHT),
            vec![Line(0), Line(2), Line(3), Line(4), Line(5)]
        );
        assert_eq!(f.split.changes.len(), 1);
        let c = &f.split.changes[0];
        assert_eq!((c.hunk, c.lines.clone()), (0, 1..5));
        assert_eq!(c.rows, [1..2, 1..4]);
        assert_eq!(c.kind(), ChangeKind::Modified);
    }

    /// 改动的三种:两侧都有 = 修改,只有右侧 = 新增(左栏是插入位置上的空区间),只有左侧 = 删除;
    /// 一个 hunk 里被上下文隔开的两段是两处改动;认不出的种类不进任何一栏。
    #[test]
    fn 改动的区间与种类() {
        let f = flatten(&[
            hunk(vec![
                line("context", "a"),
                line("add", "new"),
                line("context", "b"),
                line("delete", "gone"),
                line("weird", "?"),
                line("context", "c"),
            ]),
            hunk(vec![line("delete", "x"), line("add", "y")]),
        ]);
        let kinds: Vec<ChangeKind> = f.split.changes.iter().map(Change::kind).collect();
        assert_eq!(
            kinds,
            vec![ChangeKind::Added, ChangeKind::Deleted, ChangeKind::Modified]
        );
        let [added, deleted, modified] = [0, 1, 2].map(|c| f.split.changes[c].clone());
        assert_eq!(added.rows, [1..1, 1..2], "左栏插在 a 之后");
        assert_eq!(deleted.rows, [2..3, 3..3]);
        assert_eq!(deleted.lines, 3..5, "认不出的种类也算在这一段里,只是不进栏");
        assert_eq!((modified.hunk, modified.rows.clone()), (1, [4..5, 4..5]));
        assert_eq!(f.split.rows[LEFT].len(), 5, "a b gone c x");
        assert_eq!(f.split.change_at(RIGHT, 1), Some(0));
        assert_eq!(f.split.change_at(LEFT, 1), None, "b 是上下文");
        assert_eq!(f.split.change_at(LEFT, 2), Some(1));
        assert_eq!(f.split.change_at(RIGHT, 4), Some(2));
        assert_eq!(f.split.change_at(RIGHT, 9), None);
    }

    /// 滚动位置 ↔ 行:改动以外两栏逐行对应;改动里较短的一侧按比例走;0 行的一侧不动;
    /// 末尾之后两栏一起往后。
    #[test]
    fn 滚动位置换行_块外逐行块内按比例() {
        // 上下文 2 行,1 行改成 3 行,上下文 2 行:滚动总长 2 + 3 + 2
        let mut lines = vec![line("context", "a"), line("context", "b")];
        lines.push(line("delete", "x"));
        lines.extend((0..3).map(|i| line("add", &format!("X{i}"))));
        lines.extend([line("context", "c"), line("context", "d")]);
        let f = flatten(&[hunk(lines)]);
        let s = &f.split;
        assert_eq!(s.total(), 7.0);
        assert_eq!(s.hunk_pos(0), 0.0);
        for pos in [0.0, 1.0, 1.5, 2.0] {
            assert_eq!(s.row_at(LEFT, pos), pos);
            assert_eq!(s.row_at(RIGHT, pos), pos);
        }
        // 改动里:右栏 1:1,左栏 1 行摊在 3 个位置上
        assert_eq!(s.row_at(RIGHT, 3.5), 3.5);
        assert!((s.row_at(LEFT, 3.5) - 2.5).abs() < 1e-9);
        assert_eq!(s.row_at(LEFT, 5.0), 3.0);
        // 改动之后两栏又逐行对应,左栏比右栏少 2 行
        assert_eq!(s.row_at(LEFT, 6.0), 4.0);
        assert_eq!(s.row_at(RIGHT, 6.0), 6.0);
        assert_eq!(s.row_at(RIGHT, 9.0), 9.0, "末尾之后照样外推");

        // 纯新增:左栏在整段里不动
        let f = flatten(&[hunk(vec![
            line("context", "a"),
            line("add", "n1"),
            line("add", "n2"),
            line("context", "b"),
        ])]);
        assert_eq!(f.split.row_at(LEFT, 1.0), 1.0);
        assert_eq!(f.split.row_at(LEFT, 2.5), 1.0);
        assert_eq!(f.split.row_at(LEFT, 3.0), 1.0);
        assert_eq!(f.split.row_at(LEFT, 3.5), 1.5);
    }

    /// 对齐点:到顶时在顶边、中段在正中、到底时在底边;内容不满两屏时从顶边匀速挪到底边;
    /// 不用滚时恒为 0。
    #[test]
    fn 对齐点_首尾挪到边上() {
        assert_eq!(anchor(0.0, 100.0, 40.0), 0.0);
        assert_eq!(anchor(10.0, 100.0, 40.0), 10.0);
        assert_eq!(anchor(20.0, 100.0, 40.0), 20.0);
        assert_eq!(anchor(50.0, 100.0, 40.0), 20.0);
        assert_eq!(anchor(90.0, 100.0, 40.0), 30.0);
        assert_eq!(anchor(100.0, 100.0, 40.0), 40.0);
        assert_eq!(anchor(5.0, 10.0, 40.0), 20.0, "不满两屏:按比例");
        assert_eq!(anchor(0.0, 0.0, 40.0), 0.0);
    }

    /// 两栏顶边:同一处改动在对齐点上对齐;到顶时两栏都从第一行起,到底时两栏都停在自己的
    /// 末行(较短的一侧也滚得到底);一侧只有几行时钉在顶上。
    #[test]
    fn 两栏顶边_首尾都到得了() {
        // 100 行上下文 + 1 行改成 30 行 + 100 行上下文(不折叠)
        let mut lines: Vec<DiffLine> = (0..100)
            .map(|i| line("context", &format!("a{i}")))
            .collect();
        lines.push(line("delete", "x"));
        lines.extend((0..30).map(|i| line("add", &format!("N{i}"))));
        lines.extend((0..100).map(|i| line("context", &format!("b{i}"))));
        let f = flatten(&[hunk(lines)]);
        let s = &f.split;
        assert_eq!((s.rows[LEFT].len(), s.rows[RIGHT].len()), (201, 230));
        let visible = 40.0;
        let max = s.max_pos(visible);
        assert_eq!(s.side_tops(0.0, visible), [0.0, 0.0]);
        let end = s.side_tops(max, visible);
        assert_eq!(end, [201.0 - visible, 230.0 - visible]);
        assert_eq!(s.side_tops(max + 50.0, visible), end, "越界钳回上限");
        // 对齐点(正中)落在改动的一半处:右栏是第 15 行新增,左栏是那一行删除的正中
        let c = &s.changes[0];
        let at = s.knots[0].0 + 15.0;
        let tops = s.side_tops(at - visible / 2.0, visible);
        let center = |side: usize| tops[side] + visible / 2.0;
        assert!((center(RIGHT) - (c.rows[RIGHT].start as f64 + 15.0)).abs() < 1e-6);
        assert!((center(LEFT) - (c.rows[LEFT].start as f64 + 0.5)).abs() < 1e-6);
        // 滚动是单调的:往下滚两栏都不往回走(中段)
        let mut last = s.side_tops(visible, visible);
        for step in 1..100 {
            let now = s.side_tops(visible + f64::from(step), visible);
            assert!(now[LEFT] >= last[LEFT] - 1e-9 && now[RIGHT] >= last[RIGHT] - 1e-9);
            last = now;
        }

        // 新文件:左栏没有行,恒在顶上;右栏照常滚
        let f = flatten(&[hunk(
            (0..100).map(|i| line("add", &format!("n{i}"))).collect(),
        )]);
        assert_eq!(f.split.empty_sides(), [true, false]);
        let tops = f.split.side_tops(30.0, 20.0);
        assert_eq!(tops[LEFT], 0.0);
        assert!(tops[RIGHT] > 0.0);
    }

    /// 折叠行两栏成对:同一条分隔行在两栏里的行号记在 `fold_rows` 与 `Fold` 上;改动在两侧
    /// 行数不同,之后的分隔行两栏的行号随之错开。
    #[test]
    fn 折叠行两栏成对() {
        let old: Vec<String> = (1..=60).map(|i| format!("l{i}")).collect();
        let mut new = old.clone();
        new.insert(5, "a".into());
        new.insert(6, "b".into());
        new[51] = "FIFTY".into();
        let text = |v: &[String]| v.iter().map(|l| format!("{l}\n")).collect::<String>();
        let diff = diff_two_texts(text(&old), text(&new));
        let (f, _) = flatten_diff(&diff, &fold::Revealed::default());
        assert_eq!(f.split.fold_rows.len(), f.folds.len());
        for (ix, fold) in f.folds.iter().enumerate() {
            let at = f.split.fold_rows[ix];
            assert_eq!([fold.old_row, fold.new_row], at);
            assert_eq!(f.split.rows[LEFT][at[LEFT]], SideRow::Fold(ix));
            assert_eq!(f.split.rows[RIGHT][at[RIGHT]], SideRow::Fold(ix));
        }
        // 第一处改动(插两行)之后的分隔行:右栏比左栏靠后两行
        let mid = f.folds.iter().position(|fold| fold.gap == 1).unwrap();
        let at = f.split.fold_rows[mid];
        assert_eq!(at[RIGHT] - at[LEFT], 2);
        // 可见区里的分隔行:哪一栏看得见都算
        let view = [at[LEFT]..at[LEFT] + 1, 0..0];
        assert_eq!(f.split.folds_in(&view), vec![mid]);
    }

    /// 可见区里的改动:任一栏看得见都算,插入点压在可见区边上也算。
    #[test]
    fn 可见区里的改动() {
        let mut lines = Vec::new();
        for i in 0..5 {
            lines.push(line("context", &format!("c{i}")));
            lines.push(line("add", &format!("a{i}")));
        }
        let f = flatten(&[hunk(lines)]);
        // 右栏:c0 a0 c1 a1 …(改动 k 在右栏第 2k+1 行),左栏:c0 c1 …(插入点在第 k+1 行)
        let s = &f.split;
        assert_eq!(s.changes_in(&[0..0, 3..4]), vec![1]);
        assert_eq!(
            s.changes_in(&[3..4, 100..100]),
            vec![2, 3],
            "左栏插入点 3、4"
        );
        assert_eq!(s.changes_in(&[0..10, 0..10]), vec![0, 1, 2, 3, 4]);
    }

    /// 一帧里看得见的行:按顶边取整、多带一行,不越过行数;视口还没量到时按兜底行数。
    #[test]
    fn 一帧的可见行与纵坐标() {
        let frame = SplitFrame {
            tops: [2.5, 0.0],
            lh: 20.0,
            view_h: 100.0,
        };
        assert_eq!(frame.rows(LEFT, 100), 2..9);
        assert_eq!(frame.rows(RIGHT, 3), 0..3);
        assert_eq!(frame.y(LEFT, 3), 10.0);
        assert_eq!(frame.y(LEFT, 2), -10.0);
        let unknown = SplitFrame {
            view_h: 0.0,
            ..frame
        };
        assert_eq!(unknown.rows(RIGHT, 1000).end, FALLBACK_ROWS as usize + 1);
    }
}
