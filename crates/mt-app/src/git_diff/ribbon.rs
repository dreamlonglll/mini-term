//! 并排视图的中间栏:左栏行号 | 连接带 | 右栏行号;允许块操作时,连接带上还有每处改动的按钮。
//!
//! # 带子
//!
//! 移植自 Winc(Apache-2.0)`crates/winc-app/src/view/diff.rs` 的 `draw_ribbon`:每处改动一条,
//! 左端是左栏那几行的上下沿、右端是右栏的上下沿,中间按 smoothstep `t²(3−2t)` 过渡 —— 两侧
//! 行数不同、位置不同时,带子跟着弯、变宽变窄。一侧没有行时那一端收成一个点,该侧在插入
//! 位置画一道线(正文那一段在 `split`,行号栏这一段在这里)。折着的上下文两栏用与分隔行同色的
//! 淡色带子相连(两栏的分隔行不一定在同一高度)。一侧为空(新文件 / 删掉的文件)不画带子。
//!
//! 画法与原版不同:原版逐 0.5 逻辑像素一列画矩形(它自己的渲染器画抗锯齿曲线一帧要一两
//! 毫秒)。gpui 的 path 由 GPU 做 4× MSAA 光栅化,而 smoothstep 恰好就是控制点在 1/3、2/3
//! 宽处的三次贝塞尔(见 [`curve`] 的单测),于是每条带子 = 一个填充 path + 一个 1px 描边 path
//! (上下两条边),边缘不出台阶。逐列矩形在 100% 缩放下列宽不到一个物理像素,半透明底色的
//! 相邻两列边缘抗锯齿会叠出竖纹,path 没有这个问题;每帧只画视野里那几条(十来个 path,
//! 细分在微秒级),同一画布里连着画、合成一批。
//!
//! # 配色
//!
//! 按改动种类([`shade`]):修改蓝(`color_info`)、新增绿(沿用 `diff_add_*`)、删除灰
//! (`text_muted`)。行底色与带子同色,改了的词深一档,带子的边与插入线再深一档 —— Winc 的
//! 行 / 词 / 边三档。都由主题色派生,深浅主题与主题包跟着走。行内视图不走这里,照旧红绿。
//!
//! # 按钮(只有允许块操作的单文件 diff 才有)
//!
//! 每处改动一组,箭头 = 把一侧的内容搬到另一侧:
//!
//! | 侧别 | 左「»」(与左栏第一行同高) | 中间勾选框(带子正中) | 右「«」(与右栏第一行同高) |
//! |---|---|---|---|
//! | 未暂存(左 = 暂存区,右 = 工作区) | 丢弃这一处(先确认) | 勾上 / 取消这一处的增删行 | 暂存这一处 |
//! | 已暂存(左 = HEAD,右 = 暂存区) | 取消暂存这一处 | 同上 | — |
//!
//! 这一处里勾了行时,两颗箭头只动勾上的行(与块头按钮变成「暂存所选 N 行」同口径),悬停写明
//! 这次点下去对几行做什么。一侧没有行时那颗箭头压在插入线上。按钮是 20×20 的实底小方块(压在
//! 带子上也看得清),三颗之间留 8px;非 UTF-8 文件按钮置灰、悬停说明原因。

use std::ops::Range;

use gpui::{
    AnyElement, Bounds, ClickEvent, ContentMask, ElementId, Entity, Hsla, InteractiveElement as _,
    IntoElement, ParentElement, PathBuilder, Pixels, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled, Window, canvas, div, fill, point,
    prelude::FluentBuilder as _, px, size,
};
use mt_ui::tooltip::TooltipExt as _;

use super::split::{ChangeKind, LEFT, RIGHT, SideRow, SplitFrame};
use super::{
    Busy, CHECK_W, ChangeKey, DiffSide, DiffState, GUTTER, HunkOp, HunkUi, OpScope, Tick, fold,
    fold_bg, fold_hover_bg, line_check, op_key, op_label, request_hunk_op, still_there, tick_mark,
};
use crate::i18n::{t, tr};
use crate::ui;

/// 没有按钮时连接带的宽(Winc 的 `DIFF_RIBBON`)。
const RIBBON_W: f32 = 48.0;
/// 有按钮时连接带的宽:三颗 20px 的按钮、彼此相隔 8px,两头各留 6px。
const RIBBON_ACTIONS_W: f32 = 88.0;
/// 按钮(点击区)的边长。
const BUTTON: f32 = 20.0;
/// 左右两颗按钮的中心离连接带两头的距离。
const SLOT_INSET: f32 = 16.0;

/// 一种改动的三档颜色。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Shade {
    /// 改动的行(两栏正文与行号栏)和带子的底色
    pub(super) line: Hsla,
    /// 行里改了的词
    pub(super) word: Hsla,
    /// 带子的边与插入线
    pub(super) edge: Hsla,
}

/// 按改动种类取色:修改蓝、新增绿、删除灰(见模块注释「配色」)。
pub(super) fn shade(kind: ChangeKind) -> Shade {
    match kind {
        ChangeKind::Modified => Shade {
            line: ui::diff_mod_bg(),
            word: ui::diff_mod_word_bg(),
            edge: ui::diff_mod_edge(),
        },
        ChangeKind::Added => Shade {
            line: ui::diff_add_bg(),
            word: ui::diff_add_word_bg(),
            edge: ui::diff_add_edge(),
        },
        ChangeKind::Deleted => Shade {
            line: ui::diff_gray_bg(),
            word: ui::diff_gray_word_bg(),
            edge: ui::diff_gray_edge(),
        },
    }
}

/// 中间栏的几何:两侧行号栏(行号列 + 允许按行勾选时的勾选框列)与连接带的宽。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Mid {
    gutter: f32,
    ribbon: f32,
}

impl Mid {
    pub(super) fn new(can_pick: bool, actions: bool) -> Self {
        Self {
            gutter: GUTTER + if can_pick { CHECK_W } else { 0.0 },
            ribbon: if actions { RIBBON_ACTIONS_W } else { RIBBON_W },
        }
    }

    pub(super) fn width(self) -> f32 {
        self.gutter * 2.0 + self.ribbon
    }

    /// 三颗按钮的横向中心(相对中间栏左缘):左「»」、中间勾选框、右「«」。
    fn slots(self) -> [f32; 3] {
        let x0 = self.gutter;
        [
            x0 + SLOT_INSET,
            x0 + self.ribbon / 2.0,
            x0 + self.ribbon - SLOT_INSET,
        ]
    }
}

/// 带子一条边(从左端的 `y0` 到右端的 `y1`)的三次贝塞尔:起点、两个控制点、终点。控制点
/// 取在 1/3、2/3 宽处、高度分别等于两端,这条曲线恰好是 `y = y0 + (y1 − y0)·smoothstep(t)`、
/// `x` 随 `t` 匀速 —— 与 Winc 逐列插值的 S 形是同一条线。
pub(super) fn curve(x0: f32, x1: f32, y0: f32, y1: f32) -> [(f32, f32); 4] {
    let w = x1 - x0;
    [
        (x0, y0),
        (x0 + w / 3.0, y0),
        (x0 + w * 2.0 / 3.0, y1),
        (x1, y1),
    ]
}

/// 一条带子:两端的上下沿(相对正文顶边)、底色,以及边线(折叠行的淡色带子没有边)。
struct Ribbon {
    left: (f32, f32),
    right: (f32, f32),
    fill: Hsla,
    edge: Option<Hsla>,
}

/// 在 `[x0, x1]` 之间画一条带子(`top` 是正文顶边的窗口纵坐标)。
fn paint_ribbon(window: &mut Window, x0: Pixels, x1: Pixels, top: Pixels, ribbon: &Ribbon) {
    let at = |(x, y): (f32, f32)| point(px(x), top + px(y));
    let (x0, x1) = (f32::from(x0), f32::from(x1));
    let upper = curve(x0, x1, ribbon.left.0, ribbon.right.0);
    let lower = curve(x0, x1, ribbon.left.1, ribbon.right.1);
    // 底:上沿从左到右,下沿从右到左(同一条曲线倒着走,控制点对调)
    let mut body = PathBuilder::fill();
    body.move_to(at(upper[0]));
    body.cubic_bezier_to(at(upper[3]), at(upper[1]), at(upper[2]));
    body.line_to(at(lower[3]));
    body.cubic_bezier_to(at(lower[0]), at(lower[2]), at(lower[1]));
    body.close();
    if let Ok(path) = body.build() {
        window.paint_path(path, ribbon.fill);
    }
    if let Some(edge) = ribbon.edge {
        let mut lines = PathBuilder::stroke(px(1.0));
        for [p0, c1, c2, p1] in [upper, lower] {
            lines.move_to(at(p0));
            lines.cubic_bezier_to(at(p1), at(c1), at(c2));
        }
        if let Ok(path) = lines.build() {
            window.paint_path(path, edge);
        }
    }
}

/// 一处改动三颗按钮的纵向中心(相对正文顶边):左右两颗与该侧第一行同高(该侧没有行时压在
/// 插入线上),中间那颗取两者中点 —— 恰在带子正中(smoothstep 在 t = ½ 处取一半)。
pub(super) fn control_ys(rows: &[Range<usize>; 2], frame: &SplitFrame) -> [f32; 3] {
    let at = |side: usize| {
        let y = frame.y(side, rows[side].start);
        if rows[side].is_empty() {
            y
        } else {
            y + frame.lh / 2.0
        }
    };
    let (left, right) = (at(LEFT), at(RIGHT));
    [left, (left + right) / 2.0, right]
}

/// 按钮中心落在视口外就不画;贴着上下缘的往里挪半颗,免得被正文上下沿裁掉一半(Winc 同款)。
pub(super) fn place_y(y: f32, view_h: f32) -> Option<f32> {
    if !(0.0..=view_h).contains(&y) {
        return None;
    }
    let half = BUTTON / 2.0;
    Some(y.clamp(half, (view_h - half).max(half)))
}

/// 中间栏:底下一层画布画带子与行号栏里的插入线,上面是两侧行号栏的各行与按钮。
pub(super) fn middle(
    state: &Entity<DiffState>,
    s: &DiffState,
    frame: &SplitFrame,
    mid: Mid,
    hunks: Option<&HunkUi>,
) -> AnyElement {
    let split = &s.flat.split;
    let lh = frame.lh;
    let view = [LEFT, RIGHT].map(|side| frame.rows(side, split.rows[side].len()));
    let changes = split.changes_in(&view);
    let empty = split.empty_sides();

    let mut ribbons: Vec<Ribbon> = Vec::new();
    let mut marks: Vec<(usize, f32, Hsla)> = Vec::new();
    if !empty[LEFT] && !empty[RIGHT] {
        for f in split.folds_in(&view) {
            let at = split.fold_rows[f];
            let (l, r) = (frame.y(LEFT, at[LEFT]), frame.y(RIGHT, at[RIGHT]));
            ribbons.push(Ribbon {
                left: (l, l + lh),
                right: (r, r + lh),
                fill: fold_bg(),
                edge: None,
            });
        }
        for &c in &changes {
            let change = &split.changes[c];
            let span = |side: usize| {
                (
                    frame.y(side, change.rows[side].start),
                    frame.y(side, change.rows[side].end),
                )
            };
            let (left, right) = (span(LEFT), span(RIGHT));
            let below = frame.view_h > 0.0 && left.0.min(right.0) > frame.view_h;
            if left.1.max(right.1) < 0.0 || below {
                continue;
            }
            let shade = shade(change.kind());
            ribbons.push(Ribbon {
                left,
                right,
                fill: shade.line,
                edge: Some(shade.edge),
            });
            for side in [LEFT, RIGHT] {
                if change.rows[side].is_empty() {
                    marks.push((side, span(side).0, shade.edge));
                }
            }
        }
    }
    let gutter_w = px(mid.gutter);
    let ribbon_w = px(mid.ribbon);
    let rule = ui::border_subtle();
    let layer = canvas(
        |_, _, _| {},
        move |bounds, (), window, _cx| {
            // 正文与行号栏之间的淡竖线(Winc 同款):最先画,行号栏各行的底色叠在它上面
            for x in [bounds.left(), bounds.right() - px(1.0)] {
                let origin = point(x, bounds.top());
                window.paint_quad(fill(
                    Bounds::new(origin, size(px(1.0), bounds.size.height)),
                    rule,
                ));
            }
            let x0 = bounds.left() + gutter_w;
            let x1 = x0 + ribbon_w;
            let mask = ContentMask {
                bounds: Bounds::from_corners(point(x0, bounds.top()), point(x1, bounds.bottom())),
            };
            window.with_content_mask(Some(mask), |window| {
                for ribbon in &ribbons {
                    paint_ribbon(window, x0, x1, bounds.top(), ribbon);
                }
            });
            for &(side, y, color) in &marks {
                let x = if side == LEFT { bounds.left() } else { x1 };
                let origin = point(x, bounds.top() + px(y - 0.5));
                window.paint_quad(fill(Bounds::new(origin, size(gutter_w, px(1.0))), color));
            }
        },
    )
    .absolute()
    .size_full();

    let mut strip = div()
        .relative()
        .flex_none()
        .w(px(mid.width()))
        .h_full()
        .overflow_hidden()
        .child(layer);
    for side in [LEFT, RIGHT] {
        for row in view[side].clone() {
            strip = strip.child(gutter_cell(state, s, frame, mid, side, row));
        }
    }
    if let Some(hunk_ui) = hunks {
        for &c in &changes {
            strip = strip.children(change_controls(state, s, hunk_ui, frame, mid, c));
        }
    }
    strip.into_any_element()
}

/// 行号栏的一行:改动里的行铺该处改动的底色,删除行 / 新增行的勾选框贴着行号、靠正文一侧
/// (左栏在行号左边,右栏在右边)。分隔行只铺淡色底,点它与点正文里的分隔行一样整段展开。
fn gutter_cell(
    state: &Entity<DiffState>,
    s: &DiffState,
    frame: &SplitFrame,
    mid: Mid,
    side: usize,
    row: usize,
) -> AnyElement {
    let split = &s.flat.split;
    let lh = frame.lh;
    let x = if side == LEFT {
        0.0
    } else {
        mid.gutter + mid.ribbon
    };
    let cell = div()
        .absolute()
        .top(px(frame.y(side, row)))
        .left(px(x))
        .w(px(mid.gutter))
        .h(px(lh))
        .line_height(px(lh))
        .flex();
    match split.rows[side][row] {
        SideRow::Fold(f) => {
            let (gap, start) = (s.flat.folds[f].gap, s.flat.folds[f].start);
            let state = state.clone();
            cell.id(SharedString::from(format!(
                "git-diff-fold-gutter-{side}-{gap}-{start}"
            )))
            .cursor_pointer()
            .bg(fold_bg())
            .hover(|el| el.bg(fold_hover_bg()))
            .on_click(move |_: &ClickEvent, _window, cx| {
                state.update(cx, |s, cx| {
                    s.expand(gap, start, fold::Expand::All);
                    cx.notify();
                })
            })
            .into_any_element()
        }
        SideRow::Line(ix) => {
            let line = &s.flat.lines[ix];
            let no = if side == LEFT {
                line.old_lineno
            } else {
                line.new_lineno
            };
            let number = div()
                .w(px(GUTTER))
                .flex_none()
                .pr(px(8.0))
                .text_right()
                .text_color(ui::text_muted())
                .child(no.map(|n| n.to_string()).unwrap_or_default());
            let check = s.can_pick().then(|| line_check(state, s, ix));
            let bg = split
                .change_at(side, row)
                .map(|c| shade(split.changes[c].kind()).line);
            let cell = cell.when_some(bg, |el, bg| el.bg(bg));
            if side == LEFT {
                cell.children(check).child(number).into_any_element()
            } else {
                cell.child(number).children(check).into_any_element()
            }
        }
    }
}

/// 一处改动的按钮(见模块注释「按钮」)。看不见的不画。
fn change_controls(
    state: &Entity<DiffState>,
    s: &DiffState,
    hunk_ui: &HunkUi,
    frame: &SplitFrame,
    mid: Mid,
    c: usize,
) -> Vec<AnyElement> {
    let change = &s.flat.split.changes[c];
    let ys = control_ys(&change.rows, frame).map(|y| place_y(y, frame.view_h));
    let xs = mid.slots();
    let total = s.flat.change_lines(c).len();
    let picked = s.picks_in_change(c);
    let key = s.flat.change_key(c);
    let (left_op, right_op) = match hunk_ui.side {
        DiffSide::Unstaged => (Some(HunkOp::Discard), Some(HunkOp::Stage)),
        DiffSide::Staged => (Some(HunkOp::Unstage), None),
        DiffSide::Working => (None, None),
    };
    let target = ArrowTarget {
        change: c,
        key,
        total,
        picked,
    };
    let mut out = Vec::new();
    let arrow = |op: HunkOp, glyph: &'static str, x: f32, y: Option<f32>| {
        y.map(|y| op_button(hunk_ui, op, glyph, (x, y), target))
    };
    out.extend(left_op.and_then(|op| arrow(op, "»", xs[0], ys[0])));
    if s.can_pick() && total > 0 {
        out.extend(ys[1].map(|y| change_check(state, target, (xs[1], y))));
    }
    out.extend(right_op.and_then(|op| arrow(op, "«", xs[2], ys[2])));
    out
}

/// 按钮作用的那一处改动:第几处、它的身份(点下去时核对序号还指不指着它,见
/// [`ChangeKey`])、能勾的行数、勾了几行。
#[derive(Debug, Clone, Copy)]
struct ArrowTarget {
    change: usize,
    key: Option<ChangeKey>,
    total: usize,
    picked: usize,
}

/// 按钮的实底小方块(中心在 `(x, y)`,相对中间栏)。
fn chip(id: impl Into<ElementId>, (x, y): (f32, f32)) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .absolute()
        .left(px(x - BUTTON / 2.0))
        .top(px(y - BUTTON / 2.0))
        .size(px(BUTTON))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .border_1()
        .border_color(ui::border_default())
        // 浮层底恒不透明:压在半透明的带子上不透出底下的颜色
        .bg(ui::bg_overlay())
        .text_size(ui::font_px(13.0))
        .line_height(px(BUTTON))
        .font_weight(gpui::FontWeight::BOLD)
}

/// 整处改动(没勾行)时箭头的悬停说明:做什么、动几行。
fn change_tip(op: HunkOp, count: usize) -> SharedString {
    let count = count.to_string();
    match op {
        HunkOp::Stage => tr!("gitCommit", "stageChangeTip", count = count),
        HunkOp::Unstage => tr!("gitCommit", "unstageChangeTip", count = count),
        HunkOp::Discard => tr!("gitCommit", "discardChangeTip", count = count),
    }
    .into()
}

/// 「»」/「«」。正在处理的那一处换成「…」,其余的一并禁用;文件不是 UTF-8 时置灰并说明原因。
fn op_button(
    hunk_ui: &HunkUi,
    op: HunkOp,
    glyph: &'static str,
    center: (f32, f32),
    target: ArrowTarget,
) -> AnyElement {
    let c = target.change;
    let working = hunk_ui.busy == Some(Busy::Change(c));
    let enabled = hunk_ui.busy.is_none() && hunk_ui.blocked.is_none();
    let tip: SharedString = if working {
        t("gitCommit", "hunkWorking").into()
    } else if let Some(reason) = &hunk_ui.blocked {
        reason.clone()
    } else if target.picked > 0 {
        op_label(op, target.picked)
    } else {
        change_tip(op, target.total)
    };
    let color = if !enabled {
        ui::text_muted()
    } else if op == HunkOp::Discard {
        ui::color_error()
    } else {
        ui::accent()
    };
    let (state, ctl) = (hunk_ui.state.clone(), hunk_ui.ctl.clone());
    let key = target.key;
    chip(
        SharedString::from(format!("git-diff-ribbon-{}-{c}", op_key(op))),
        center,
    )
    .text_color(color)
    .child(if working { "…" } else { glyph })
    .when(!enabled, |el| el.opacity(0.6))
    .when(enabled, move |el| {
        el.cursor_pointer()
            .hover(move |el| el.border_color(color))
            .on_click(move |_: &ClickEvent, window, cx| {
                cx.stop_propagation();
                if still_there(&state, cx, |s| s.change_is(c, key)) {
                    request_hunk_op(&state, &ctl, op, OpScope::Change(c), window, cx);
                }
            })
    })
    .tip(tip)
    .into_any_element()
}

/// 中间那颗三态勾选框:一下勾上 / 取消这一处所有能勾的行(`total` 行里勾了 `picked` 行)。
fn change_check(state: &Entity<DiffState>, target: ArrowTarget, center: (f32, f32)) -> AnyElement {
    let ArrowTarget {
        change: c,
        key,
        total,
        picked,
    } = target;
    let state = state.clone();
    chip(
        SharedString::from(format!("git-diff-ribbon-pick-{c}")),
        center,
    )
    .cursor_pointer()
    .hover(|el| el.border_color(ui::accent()))
    .child(tick_mark(Tick::of(picked, total)))
    .on_click(move |_: &ClickEvent, _window, cx| {
        cx.stop_propagation();
        state.update(cx, |s, cx| {
            // 序号还指着渲染时那一处才翻(见 [`ChangeKey`])
            if s.change_is(c, key) {
                s.toggle_change_picks(c);
            }
            cx.notify();
        });
    })
    .tip(tr!(
        "gitCommit",
        "toggleChangeTip",
        count = total.to_string()
    ))
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ease(t: f32) -> f32 {
        t * t * (3.0 - 2.0 * t)
    }

    /// 三次贝塞尔在参数 `s` 处的点。
    fn bezier(p: [(f32, f32); 4], s: f32) -> (f32, f32) {
        let u = 1.0 - s;
        let w = [u * u * u, 3.0 * u * u * s, 3.0 * u * s * s, s * s * s];
        let x = w.iter().zip(&p).map(|(w, p)| w * p.0).sum::<f32>();
        let y = w.iter().zip(&p).map(|(w, p)| w * p.1).sum::<f32>();
        (x, y)
    }

    /// 带子的边就是 smoothstep 曲线:x 随参数匀速,y = y0 + (y1 − y0)·smoothstep(t);两端
    /// 水平切入(控制点与端点同高),正中恰好过一半的高度。
    #[test]
    fn 带子的边是_smoothstep() {
        let p = curve(10.0, 58.0, 100.0, 40.0);
        for k in 0..=20 {
            let s = k as f32 / 20.0;
            let (x, y) = bezier(p, s);
            assert!((x - (10.0 + 48.0 * s)).abs() < 1e-3, "x 匀速:{x}");
            assert!((y - (100.0 - 60.0 * ease(s))).abs() < 1e-3, "y:{y}");
        }
        assert_eq!((p[0].1, p[1].1), (100.0, 100.0));
        assert_eq!((p[2].1, p[3].1), (40.0, 40.0));
        assert_eq!(ease(0.5), 0.5);
    }

    /// 一侧没有行时那一端收成一个点:上下两条边从同一点出发,带子是个楔形。
    #[test]
    fn 一侧没有行时一端收成点() {
        let (left, right) = ((50.0, 50.0), (40.0, 100.0));
        let upper = curve(0.0, 48.0, left.0, right.0);
        let lower = curve(0.0, 48.0, left.1, right.1);
        assert_eq!(upper[0], lower[0]);
        assert_ne!(upper[3], lower[3]);
        // 中途两条边分开,宽度单调变大
        let gap = |s: f32| bezier(lower, s).1 - bezier(upper, s).1;
        assert!(gap(0.25) > 0.0 && gap(0.5) > gap(0.25) && gap(1.0) > gap(0.5));
    }

    /// 按钮槽位:三颗都在连接带里,点击区 20px,彼此至少隔 8px,左右对称;中间栏宽 = 两侧
    /// 行号栏 + 连接带,勾选框列只在允许按行勾选时算进行号栏。
    #[test]
    fn 按钮槽位不重叠且点击区够大() {
        let mid = Mid::new(true, true);
        assert_eq!(mid.width(), (GUTTER + CHECK_W) * 2.0 + RIBBON_ACTIONS_W);
        let xs = mid.slots();
        let (from, to) = (mid.gutter, mid.gutter + mid.ribbon);
        for x in xs {
            assert!(x - BUTTON / 2.0 >= from && x + BUTTON / 2.0 <= to, "{x}");
        }
        for pair in xs.windows(2) {
            assert!(pair[1] - pair[0] - BUTTON >= 8.0, "{pair:?}");
        }
        assert_eq!(xs[0] - from, to - xs[2], "左右对称");
        let plain = Mid::new(false, false);
        assert_eq!(plain.width(), GUTTER * 2.0 + RIBBON_W);
    }

    /// 按钮的高度:左右两颗与该侧第一行同高(该侧没有行时压在插入线上),中间那颗取中点;
    /// 视口外的不画,贴边的挪进来半颗。
    #[test]
    fn 按钮高度与视口裁切() {
        let frame = SplitFrame {
            tops: [0.0, 2.0],
            lh: 20.0,
            view_h: 200.0,
        };
        // 左栏第 5 行、右栏插在第 9 行之前(右栏顶边在第 2 行)
        let ys = control_ys(&[5..6, 9..9], &frame);
        assert_eq!(ys, [110.0, 125.0, 140.0]);
        assert_eq!(place_y(110.0, 200.0), Some(110.0));
        assert_eq!(place_y(3.0, 200.0), Some(BUTTON / 2.0), "贴着顶边往下挪");
        assert_eq!(place_y(197.0, 200.0), Some(200.0 - BUTTON / 2.0));
        assert_eq!(place_y(-1.0, 200.0), None);
        assert_eq!(place_y(201.0, 200.0), None);
    }
}
