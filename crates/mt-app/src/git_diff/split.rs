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
//! 共用坐标与两栏行号的对应移植自 Winc(Apache-2.0)`crates/winc-editor/src/diff_view.rs` 的
//! `layout` 里的分段点与 `row_at`;对齐与钳位是这里自己的([`Split::side_tops`]):
//!
//! - **共用坐标**(单位:行):改动以外两栏逐行对应,每处改动占两侧里较多的行数,较短的一侧
//!   按比例移动(0 行的一侧不动)。只按右栏算的话,左栏一大段删除在右栏只占一个点,滚动时会被
//!   直接越过;
//! - **对齐**:对齐点固定在视口正中,两栏都把共用坐标上的同一处摆在那里,再各按自己的范围
//!   `[0, 行数 − 视口]` 钳住 —— 两栏都滚不过自己的首行与末行(与原先的 uniform_list 同口径)。
//!   被钳住的那一栏,对齐的那一行仍在视口里(开头在上半屏、末尾在下半屏),同一处改动两栏
//!   照样都看得见,带子不会伸出视口;
//! - **滚动位置**是两栏顶边走过的路程:每滚一行,走得快的那一栏恰好挪一行。两栏都被钳住的
//!   那几段不占滚动位置,滚轮不会空转;谁也不会一步跳好几行。两栏顶边都随滚动位置单调不减,
//!   起点都在第一行,终点都停在自己的末行。
//!
//! 与 Winc 的差别:Winc 的对齐点在开头半屏里从顶边挪到正中(编辑区能滚过末行,末尾不用挪)。
//! 对齐点一挪,对侧是纯新增 / 纯删除时这一栏的顶边会往回走 —— 顶边 = 对齐行 − 对齐点,对齐行
//! 不动而对齐点在下移。这里对齐点不动,开头末尾改由钳位兜住。
//!
//! # 两栏怎么跟着滚动位置走
//!
//! 两栏仍是各自的 `uniform_list`(虚拟化:只给视野里的行建元素),但它们自己的滚轮处理被
//! 截下([`scroll_layer`]):滚轮在捕获阶段改共用的滚动位置并停止冒泡,每帧
//! ([`DiffState::sync_split`])再按滚动位置算出两栏各自的顶边、写回两个列表的偏移。横向同样是
//! 一个共用的位置、两栏各按自己的范围钳住(见 [`shared_x`]);纵向滚动条只有一条,画在右缘、
//! 表示共用的滚动位置([`SplitScroll`])。

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
    /// 每个 hunk 第一行的共用坐标(F7 跳转的落点)
    hunk_pos: Vec<f64>,
    /// 共用坐标的分段点:每处改动的开头、结尾各一个,`(共用坐标, [左栏行, 右栏行])`
    knots: Vec<(f64, [f64; 2])>,
    /// 共用坐标的总长(行)
    total: f64,
    /// 给 uniform_list 量宽用的「最宽的那一行」,两栏各一个
    pub(super) widest: [usize; 2],
    /// 上一次算的滚动路径(按视口高认;只有改了窗口大小才重算,见 [`Split::path`])
    path: RefCell<Option<Rc<Path>>>,
}

/// 某个视口高度下两栏顶边的路径(见模块注释「滚动映射」):按共用坐标排的一串折点,相邻
/// 两点之间两栏顶边、滚动位置都随共用坐标线性变化。
#[derive(Debug)]
struct Path {
    visible: f64,
    stops: Vec<Stop>,
}

/// 路径上的一个折点。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stop {
    /// 对齐点(视口正中)所在的共用坐标
    at: f64,
    /// 滚动位置
    pos: f64,
    /// 两栏顶边(行)
    tops: [f64; 2],
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

    /// 第 `i` 个分段点的前一个 `(共用坐标, 第 side 栏的行)`;`i` 为 0 时是起点。
    #[cfg(test)]
    fn knot(&self, side: usize, i: usize) -> (f64, f64) {
        i.checked_sub(1)
            .and_then(|i| self.knots.get(i))
            .map_or((0.0, 0.0), |&(at, rows)| (at, rows[side]))
    }

    /// 共用坐标 `at` 对应第 `side` 栏的第几行(可以是小数):二分找第一个不小于 `at` 的
    /// 分段点,在它与前一个之间插值。改动以外两栏逐行对应,改动里按比例。只有单测用 ——
    /// 排滚动路径时([`Split::build_path`])直接在折点之间插值,同一条折线。
    #[cfg(test)]
    pub(super) fn row_at(&self, side: usize, at: f64) -> f64 {
        let i = self.knots.partition_point(|k| k.0 < at);
        let from = self.knot(side, i);
        let Some(&(to_at, rows)) = self.knots.get(i) else {
            return from.1 + (at - from.0);
        };
        let to = (to_at, rows[side]);
        let t = if to.0 > from.0 {
            (at - from.0) / (to.0 - from.0)
        } else {
            1.0
        };
        from.1 + (to.1 - from.1) * t
    }

    /// 视口 `visible` 行时的滚动路径。只按视口高缓存一份:行模型一换整个 `Split` 跟着重建,
    /// 视口高只在改窗口大小时变。
    fn path(&self, visible: f64) -> Rc<Path> {
        if let Some(path) = self.path.borrow().as_ref()
            && path.visible == visible
        {
            return path.clone();
        }
        let path = Rc::new(Path {
            visible,
            stops: self.build_path(visible),
        });
        *self.path.borrow_mut() = Some(path.clone());
        path
    }

    /// 排出滚动路径(见模块注释「滚动映射」):对齐点在视口正中,共用坐标 `at` 处两栏顶边 =
    /// 该栏的对齐行 − 半屏,钳进 `[0, 行数 − 视口]`;滚动位置是顶边走过的路程(两栏里挪得多
    /// 的那一栏的行数)。
    ///
    /// 两栏行号对共用坐标是折线(顶点:起点、各分段点、终点),钳位又在每栏「对齐行 = 半屏」
    /// 「对齐行 = 半屏 + 最大顶边」两处各折一下,所以把这些点都排成折点,两点之间就全是线性的。
    fn build_path(&self, visible: f64) -> Vec<Stop> {
        let half = visible / 2.0;
        let len = [LEFT, RIGHT].map(|side| self.rows[side].len() as f64);
        let max_top = len.map(|n| (n - visible).max(0.0));
        let tops_of = |rows: [f64; 2]| {
            [LEFT, RIGHT].map(|side| (rows[side] - half).clamp(0.0, max_top[side]))
        };
        let mut corners: Vec<(f64, [f64; 2])> = Vec::with_capacity(self.knots.len() + 2);
        corners.push((0.0, [0.0, 0.0]));
        corners.extend(self.knots.iter().copied());
        corners.push((self.total, len));

        let mut stops = vec![Stop {
            at: 0.0,
            pos: 0.0,
            tops: [0.0, 0.0],
        }];
        let mut cuts: Vec<f64> = Vec::with_capacity(5);
        for pair in corners.windows(2) {
            let [(a0, r0), (a1, r1)] = [pair[0], pair[1]];
            if a1 <= a0 {
                continue;
            }
            // 这一段里两栏各自碰到钳位边界的地方
            cuts.clear();
            for side in [LEFT, RIGHT] {
                let (from, to) = (r0[side], r1[side]);
                for edge in [half, half + max_top[side]] {
                    if from < edge && edge < to {
                        cuts.push(a0 + (edge - from) / (to - from) * (a1 - a0));
                    }
                }
            }
            cuts.sort_by(f64::total_cmp);
            cuts.push(a1);
            for &at in &cuts {
                let t = (at - a0) / (a1 - a0);
                let tops = tops_of([LEFT, RIGHT].map(|side| r0[side] + (r1[side] - r0[side]) * t));
                let last = stops[stops.len() - 1];
                let step = (tops[LEFT] - last.tops[LEFT]).max(tops[RIGHT] - last.tops[RIGHT]);
                stops.push(Stop {
                    at,
                    pos: last.pos + step,
                    tops,
                });
            }
        }
        stops
    }

    /// 视口 `visible` 行时滚动位置的上限(行)。
    pub(super) fn max_pos(&self, visible: f64) -> f64 {
        self.path(visible).stops.last().map_or(0.0, |stop| stop.pos)
    }

    /// 滚动位置为 `pos`、视口 `visible` 行时两栏顶边各是第几行(可以是小数)。两栏都随 `pos`
    /// 单调不减,`pos` 为 0 时都是 0,到上限时都停在自己的末行(见模块注释「滚动映射」)。
    pub(super) fn side_tops(&self, pos: f64, visible: f64) -> [f64; 2] {
        let path = self.path(visible);
        let stops = &path.stops;
        let i = stops.partition_point(|stop| stop.pos < pos);
        match (i.checked_sub(1).map(|j| stops[j]), stops.get(i)) {
            // 前一个点严格小于 `pos`、这一个不小于:两点的滚动位置必然不同
            (Some(a), Some(b)) => {
                let t = (pos - a.pos) / (b.pos - a.pos);
                [LEFT, RIGHT].map(|side| a.tops[side] + (b.tops[side] - a.tops[side]) * t)
            }
            (None, Some(b)) => b.tops,
            (Some(a), None) => a.tops,
            (None, None) => [0.0, 0.0],
        }
    }

    /// 对齐点(视口正中)落在共用坐标 `at` 上时的滚动位置。
    fn pos_at(&self, at: f64, visible: f64) -> f64 {
        let path = self.path(visible);
        let stops = &path.stops;
        let i = stops.partition_point(|stop| stop.at < at);
        match (i.checked_sub(1).map(|j| stops[j]), stops.get(i)) {
            (Some(a), Some(b)) if b.at > a.at => {
                a.pos + (b.pos - a.pos) * (at - a.at) / (b.at - a.at)
            }
            (_, Some(b)) => b.pos,
            (Some(a), None) => a.pos,
            (None, None) => 0.0,
        }
    }

    /// 第 `hunk` 块第一行的共用坐标。
    #[cfg(test)]
    pub(super) fn hunk_pos(&self, hunk: usize) -> f64 {
        self.hunk_pos.get(hunk).copied().unwrap_or(0.0)
    }

    /// F7 跳到第 `hunk` 块时的滚动位置:对齐点落在它第一行之下半屏处,于是它的第一行停在
    /// 顶边(之后半屏里有行数不等的改动时,较长的一侧恰在顶边、较短的一侧稍靠下;开头、末尾
    /// 那一屏按钳位走)。
    pub(super) fn jump_pos(&self, hunk: usize, visible: f64) -> f64 {
        let at = self.hunk_pos.get(hunk).copied().unwrap_or(0.0);
        self.pos_at(at + visible / 2.0, visible)
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
    /// 滚动条的内容高(像素):可滚的距离 + 一屏
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

    /// 正文视口能放几行(上一帧量到的;还没量到是 0)。
    fn split_visible(&self) -> f64 {
        let view_h = f32::from(self.split_scroll.viewport().size.height);
        f64::from(view_h / self.line_height())
    }

    /// 按共用的滚动位置算出两栏的顶边,连同横向偏移 `x` 写回两个列表。
    fn place_split(&mut self, x: [Pixels; 2]) -> SplitFrame {
        let lh = self.line_height();
        let view_h = f32::from(self.split_scroll.viewport().size.height);
        let visible = self.split_visible();
        let split = &self.flat.split;
        let max_pos = split.max_pos(visible);
        // 滚动条的内容高 = 可滚的距离 + 一屏
        self.split_scroll.set_total((max_pos + visible) as f32 * lh);
        let max = (max_pos * f64::from(lh)) as f32;
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

    /// 滚轮(像素,`delta` 与 gpui 的滚动偏移同号):纵向改共用的滚动位置,横向改共用的横向
    /// 位置(见 [`shared_x`])。两个方向都有时只取幅度大的那个。动了返回 `true`。
    pub(super) fn wheel_split(&mut self, delta: Point<Pixels>) -> bool {
        let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
        if dx.abs() > dy.abs() {
            self.scroll_split(-dx, 0.0)
        } else {
            self.scroll_split(0.0, -dy)
        }
    }

    /// 往后滚 `dy`(纵向)、往右滚 `dx`(横向),两个方向都是两栏共用的位置;当场写回两个
    /// 列表(拖选的自动滚动紧接着就要按新偏移算落点)。动了返回 `true`。
    pub(super) fn scroll_split(&mut self, dx: f32, dy: f32) -> bool {
        let before = (self.split_scroll.pos(), self.synced_x);
        let mut x = self.synced_x();
        if dx != 0.0 {
            let reach = [LEFT, RIGHT].map(|side| max_x(&self.side_handle(side)));
            x = shared_x(x.map(f32::from), reach, dx).map(px);
        }
        if dy != 0.0 {
            self.split_scroll.set_pos(self.split_scroll.pos() + dy);
        }
        self.place_split(x);
        (self.split_scroll.pos(), self.synced_x) != before
    }

    /// 跳到第 `hunk` 块:它的第一行停在顶边(见 [`Split::jump_pos`])。
    pub(super) fn split_jump(&mut self, hunk: usize) {
        let pos = self.flat.split.jump_pos(hunk, self.split_visible());
        self.split_scroll.set_pos(pos as f32 * self.line_height());
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

/// 这一栏横向最多能滚多远(像素,≥ 0)。
fn max_x(handle: &ScrollHandle) -> f32 {
    f32::from(handle.max_offset().x).max(0.0)
}

/// 横向偏移按该栏自己的范围钳住(≤ 0)。
fn clamp_x(x: Pixels, handle: &ScrollHandle) -> Pixels {
    px(f32::from(x).clamp(-max_x(handle), 0.0))
}

/// 滚轮 / 拖选往右滚 `dx`(像素)之后两栏的横向偏移(≤ 0)。`x` 是两栏眼下的偏移,`reach` 是
/// 各自最多能滚多远(≥ 0)。
///
/// 横向是两栏共用的一个位置:两栏里滚得远的那个就是它(另一栏只是被自己的范围钳住了),在两栏
/// 范围的并集里挪,再各按自己的范围钳住。于是窄的一栏到头之后宽的那栏还能接着滚,窄栏那边
/// 再往前滚也不会把宽栏拽回来;往回滚时宽栏先动,共用位置回到窄栏的范围里窄栏才跟上 ——
/// 两栏只要都滚得到,看的就是同一列。
pub(super) fn shared_x(x: [f32; 2], reach: [f32; 2], dx: f32) -> [f32; 2] {
    let lead = x[LEFT].min(x[RIGHT]);
    let next = (lead - dx).clamp(-reach[LEFT].max(reach[RIGHT]), 0.0);
    reach.map(|reach| next.max(-reach))
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
    // ⚠️ 必须钉 top/left:绝对定位不写 inset 会落在静态位置(列表之后),整层往下偏一个视口高
    .absolute()
    .top_0()
    .left_0()
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
                state.update(cx, |s, cx| {
                    if s.wheel_split(delta) {
                        cx.notify();
                    }
                });
                cx.stop_propagation();
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

#[cfg(test)]
mod tests {
    use mt_project::git::{DiffHunk, DiffLine, diff_two_texts};

    use super::super::{Flat, flatten, flatten_diff, fold};
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

    /// 共用坐标 ↔ 行:改动以外两栏逐行对应;改动里较短的一侧按比例走;0 行的一侧不动;
    /// 末尾之后两栏一起往后。
    #[test]
    fn 共用坐标换行_块外逐行块内按比例() {
        // 上下文 2 行,1 行改成 3 行,上下文 2 行:共用坐标总长 2 + 3 + 2
        let mut lines = vec![line("context", "a"), line("context", "b")];
        lines.push(line("delete", "x"));
        lines.extend((0..3).map(|i| line("add", &format!("X{i}"))));
        lines.extend([line("context", "c"), line("context", "d")]);
        let f = flatten(&[hunk(lines)]);
        let s = &f.split;
        assert_eq!(s.total, 7.0);
        assert_eq!(s.hunk_pos(0), 0.0);
        for at in [0.0, 1.0, 1.5, 2.0] {
            assert_eq!(s.row_at(LEFT, at), at);
            assert_eq!(s.row_at(RIGHT, at), at);
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

    /// 按 `(种类, 行数)` 一段段拼出一块(不折叠)。
    fn shape(parts: &[(&str, usize)]) -> Flat {
        let mut lines = Vec::new();
        for &(kind, n) in parts {
            let at = lines.len();
            lines.extend((0..n).map(|i| line(kind, &format!("{kind}{}", at + i))));
        }
        flatten(&[hunk(lines)])
    }

    /// 在整个滚动范围上密采样:两栏顶边单调不减、不越出自己的范围,每一步既不跳行(挪得多的
    /// 那一栏不超过这一步)也不空转(两栏加起来至少挪这一步);起点两栏都是 0,终点两栏都停在
    /// 自己的末行。路径上每个折点处,对齐的那一行在两栏里都看得见。
    fn check_scroll(s: &Split, visible: f64, case: &str) {
        const EPS: f64 = 1e-6;
        let len = [LEFT, RIGHT].map(|side| s.rows[side].len() as f64);
        let max_top = len.map(|n| (n - visible).max(0.0));
        let max = s.max_pos(visible);
        assert_eq!(s.side_tops(0.0, visible), [0.0, 0.0], "{case}:起点");
        let end = s.side_tops(max, visible);
        for side in [LEFT, RIGHT] {
            assert!(
                (end[side] - max_top[side]).abs() < EPS,
                "{case}:终点 {end:?}"
            );
        }
        assert_eq!(s.side_tops(max + 7.0, visible), end, "{case}:越界钳回上限");
        if max_top == [0.0, 0.0] {
            assert_eq!(max, 0.0, "{case}:两栏都放得下就不用滚");
        }
        let n = 3000;
        let step = max / f64::from(n);
        let mut last = [0.0, 0.0];
        for k in 1..=n {
            let pos = max * f64::from(k) / f64::from(n);
            let now = s.side_tops(pos, visible);
            let d = [now[LEFT] - last[LEFT], now[RIGHT] - last[RIGHT]];
            assert!(
                d[LEFT] >= -EPS && d[RIGHT] >= -EPS,
                "{case}:往回走 pos={pos}:{last:?} → {now:?}"
            );
            assert!(d[LEFT].max(d[RIGHT]) <= step + EPS, "{case}:跳行 pos={pos}");
            assert!(d[LEFT] + d[RIGHT] >= step - EPS, "{case}:空转 pos={pos}");
            for side in [LEFT, RIGHT] {
                assert!(now[side] <= max_top[side] + EPS, "{case}:越界 {now:?}");
            }
            last = now;
        }
        for stop in &s.path(visible).stops {
            for side in [LEFT, RIGHT] {
                let y = s.row_at(side, stop.at.min(s.total)) - stop.tops[side];
                assert!(
                    (-EPS..=visible + EPS).contains(&y),
                    "{case}:对齐行出了视口 {stop:?}"
                );
            }
        }
    }

    /// 两栏顶边随滚动位置单调不减、首尾都到得了,各种形状、几种视口高都一样。旧口径(对齐点
    /// 在开头末尾半屏里挪动)下,左 50 行、右 70 行(第 25 行后右栏纯新增 20 行)、视口 40 行,
    /// 往下滚左栏顶边是 0→10→0→10。
    #[test]
    fn 两栏顶边单调且首尾都到得了() {
        let shapes: [(&str, &[(&str, usize)]); 15] = [
            (
                "左 50 右 70(中段纯新增)",
                &[("context", 25), ("add", 20), ("context", 25)],
            ),
            (
                "中段纯删除",
                &[("context", 25), ("delete", 20), ("context", 25)],
            ),
            (
                "修改",
                &[("context", 30), ("delete", 5), ("add", 12), ("context", 30)],
            ),
            ("首行改动", &[("delete", 1), ("add", 10), ("context", 60)]),
            ("末行改动", &[("context", 60), ("delete", 3), ("add", 15)]),
            ("首行纯新增", &[("add", 30), ("context", 50)]),
            ("末尾纯删除", &[("context", 50), ("delete", 30)]),
            ("新文件", &[("add", 100)]),
            ("删掉的文件", &[("delete", 100)]),
            ("比视口短", &[("context", 10), ("add", 5)]),
            ("恰好一屏", &[("context", 20), ("add", 20)]),
            ("一屏多", &[("context", 30), ("add", 15), ("context", 10)]),
            (
                "恰好两屏",
                &[
                    ("context", 40),
                    ("delete", 10),
                    ("add", 30),
                    ("context", 10),
                ],
            ),
            (
                "纯增纯删交替",
                &[
                    ("context", 2),
                    ("delete", 10),
                    ("context", 2),
                    ("add", 10),
                    ("context", 2),
                    ("delete", 10),
                    ("context", 2),
                    ("add", 10),
                    ("context", 2),
                ],
            ),
            (
                "一行改成一屏",
                &[("context", 3), ("delete", 1), ("add", 45), ("context", 3)],
            ),
        ];
        for (name, parts) in shapes {
            let f = shape(parts);
            for visible in [40.0, 10.0, 23.5, 0.0] {
                check_scroll(&f.split, visible, &format!("{name} / 视口 {visible}"));
            }
        }
        // 很多小改动
        let mut many = Vec::new();
        for _ in 0..30 {
            many.extend([("context", 1), ("delete", 1), ("add", 3)]);
        }
        check_scroll(&shape(&many).split, 40.0, "很多小改动");

        // 评审的例子:左栏顶边 0 → 5 → 停 → 10,右栏 0 → 30
        let f = shape(&[("context", 25), ("add", 20), ("context", 25)]);
        let s = &f.split;
        assert_eq!((s.rows[LEFT].len(), s.rows[RIGHT].len()), (50, 70));
        assert_eq!(s.max_pos(40.0), 30.0);
        assert_eq!(s.side_tops(5.0, 40.0), [5.0, 5.0]);
        assert_eq!(
            s.side_tops(25.0, 40.0),
            [5.0, 25.0],
            "新增那 20 行滚过对齐点时左栏停着"
        );
        assert_eq!(s.side_tops(30.0, 40.0), [10.0, 30.0]);
    }

    /// 中段两栏在视口正中对齐:对齐点落在一行改成 30 行的正中时,右栏正中是第 15 行新增,
    /// 左栏正中是那一行删除的正中;两栏行数不同照样都滚到自己的末行。新文件左栏恒在顶上。
    #[test]
    fn 两栏在视口正中对齐() {
        // 100 行上下文 + 1 行改成 30 行 + 100 行上下文(不折叠)
        let f = shape(&[
            ("context", 100),
            ("delete", 1),
            ("add", 30),
            ("context", 100),
        ]);
        let s = &f.split;
        assert_eq!((s.rows[LEFT].len(), s.rows[RIGHT].len()), (201, 230));
        let visible = 40.0;
        let end = s.side_tops(s.max_pos(visible), visible);
        assert_eq!(end, [201.0 - visible, 230.0 - visible]);
        let c = &s.changes[0];
        let tops = s.side_tops(s.pos_at(s.knots[0].0 + 15.0, visible), visible);
        let center = |side: usize| tops[side] + visible / 2.0;
        assert!((center(RIGHT) - (c.rows[RIGHT].start as f64 + 15.0)).abs() < 1e-6);
        assert!((center(LEFT) - (c.rows[LEFT].start as f64 + 0.5)).abs() < 1e-6);

        let f = shape(&[("add", 100)]);
        assert_eq!(f.split.empty_sides(), [true, false]);
        let tops = f.split.side_tops(30.0, 20.0);
        assert_eq!(tops, [0.0, 30.0]);
    }

    /// F7 的落点:块的第一行停在顶边(这块里右栏多一行:右栏恰在顶边,左栏稍靠下一行);
    /// 第一块从顶上起;末尾那一屏钳到上限。
    #[test]
    fn 跳到某块时它的第一行在顶边() {
        let mut first = vec![line("context", "top")];
        first.extend((0..60).map(|i| line("context", &format!("a{i}"))));
        first.extend([line("delete", "x"), line("add", "X")]);
        first.extend((0..3).map(|i| line("context", &format!("b{i}"))));
        let mut second: Vec<DiffLine> = (0..3).map(|i| line("context", &format!("c{i}"))).collect();
        second.extend([line("delete", "y"), line("add", "Y"), line("add", "Y2")]);
        second.extend((0..80).map(|i| line("context", &format!("d{i}"))));
        let mut third: Vec<DiffLine> = (0..3).map(|i| line("context", &format!("e{i}"))).collect();
        third.extend([line("delete", "z"), line("add", "Z")]);
        let f = flatten(&[hunk(first), hunk(second), hunk(third)]);
        let s = &f.split;
        let visible = 40.0;
        assert_eq!(s.jump_pos(0, visible), 0.0);
        let at = s.hunk_pos(1);
        assert_eq!(s.side_tops(s.jump_pos(1, visible), visible), [at - 1.0, at]);
        let max = s.max_pos(visible);
        assert_eq!(s.jump_pos(2, visible), max, "末尾那一屏钳到上限");
    }

    /// 横向共用一个位置:窄栏到头后再往右滚,已经滚得更远的宽栏不会被拽回来;两栏都停在窄栏
    /// 尽头时宽栏接着滚;往回滚宽栏先动,回到窄栏的范围里两栏重新对齐;两头都钳住。
    #[test]
    fn 横向共用位置_窄栏到头不拽回宽栏() {
        let reach = [800.0, 300.0];
        assert_eq!(shared_x([-800.0, -300.0], reach, 50.0), [-800.0, -300.0]);
        assert_eq!(shared_x([-300.0, -300.0], reach, 50.0), [-350.0, -300.0]);
        assert_eq!(shared_x([-800.0, -300.0], reach, -100.0), [-700.0, -300.0]);
        assert_eq!(shared_x([-700.0, -300.0], reach, -600.0), [-100.0, -100.0]);
        assert_eq!(shared_x([-100.0, -100.0], reach, -500.0), [0.0, 0.0]);
        // 左右对调一样
        assert_eq!(
            shared_x([-300.0, -800.0], [300.0, 800.0], 50.0),
            [-300.0, -800.0]
        );
        // 两栏都没有长行:不动
        assert_eq!(shared_x([0.0, 0.0], [0.0, 0.0], 50.0), [0.0, 0.0]);
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
