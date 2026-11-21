//! 拖选手势的纯逻辑:左键按下算什么、⌥ 拖没拖过一格、拖到边外滚几行、宽字符的选中判定。
//!
//! 元素([`super::element`])只管接事件、借 grid、起停定时器;「这一下该怎么办」全在这里,
//! 好让判定能脱离 gpui 单测 —— 与 [`super::selection_dwell`] 同一个拆法。
//!
//! # 左键按下的优先级(本地处理那一路)
//!
//! 滚动条与 Ctrl(⌘)+单击链接排在更前面(它们看几何 / 要探链接,不在这里判);鼠标上报
//! 开着时**不按 Shift** 的按下归程序,走不到这里(闸门是 [`super::mouse::prefers_local_handling`])。
//! 能走到这里的按下依次看:
//!
//! 1. **⌥ + 单击** → 先不动选区:松手时没拖过一格就是「定位光标」(判据与合成一字未改,
//!    见 `element.rs` 的 `cursor_move_bytes`),拖过一格才转成块选。这一下定位不了光标
//!    (在回看里 / 宿主没接输入)时没什么可等的,按下当场就开块选。排在扩选前面:从前
//!    ⌥+单击不看 Shift,同时按着 Shift 也照样定位光标,这条判定不能因为加了扩选而变。
//! 2. **Shift + 单击 + 已经有一块看得见的本地选区** → 扩选:把选区的活动端挪到点击处,
//!    锚点与选区类型(普通 / 字 / 行 / 块)都不动;之后接着拖照样延伸。
//! 3. 其余:单击 / 双击 / 三击照旧开普通 / 字 / 行选区。
//!
//! ## Shift 的两个意思怎么不打架
//!
//! 程序开着鼠标上报时,按住 Shift 是把鼠标**借回**给本地选区(xterm 一系的约定)。
//! 两条规则的分工是:**借不借回**只看上报模式与 Shift(上面那道闸,本模块不重判);借回来
//! 之后**扩不扩选**只看手上有没有本地选区。于是:
//!
//! - 没开上报:Shift+单击 = 有选区就扩选,没有就是普通单击;
//! - 开着上报:Shift+单击 = 有本地选区(此前 Shift+拖出来的)就扩选,没有就**开始**一块新的
//!   本地选区 —— 与「Shift 借回鼠标」原来的行为一致。不按 Shift 的单击照旧报给程序,并把
//!   残留的本地选区抹掉,所以「想在上报模式里另起一块」先不按 Shift 点一下即可。
//!
//! 「看得见的选区」按 alacritty 自己的口径判(`Selection::to_range` 给得出区间):单击不拖
//! 留下的空选区不算,滚出回看上限的也不算 —— 否则 Shift+单击会从一个看不见的锚点扩出去。
//!
//! # 拖到边外自动滚屏
//!
//! 拖选中指针越过可视区上 / 下边(含拖出窗口),按固定节拍([`AUTOSCROLL_TICK`])朝那边滚
//! 回看并把活动端落到新露出的边行上;离边越远一拍滚得越多([`autoscroll_lines`],有上限)。
//! 元素贴着窗口上 / 下沿时,边内留一条窄带([`EDGE_BAND`])也算出界:窗口最大化、任务栏
//! 自动隐藏时指针根本出不了窗口,没有这条带就永远滚不起来(alacritty 同样留了 5px)。
//! 滚到头(回看顶 / 底部)就算 0 行 —— 节拍任务据此自己停,鼠标回到区内同样停。

use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line, Point as AlacPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use std::time::Duration;

/// 自动滚屏的节拍。20Hz:一拍至少一行,贴边时每秒 20 行,看得清在滚,又不至于一下冲过头。
pub(super) const AUTOSCROLL_TICK: Duration = Duration::from_millis(50);

/// 越过边沿每多这么多像素,一拍多滚一行。按指针离边的距离算而不是按行高 —— 速度跟手走,
/// 不该因为字号调大就变慢。
pub(super) const AUTOSCROLL_STEP_PX: f32 = 24.0;

/// 一拍最多滚几行(节拍 50ms → 每秒 200 行)。拖到屏幕最边上也不至于一眨眼滚完整段回看。
pub(super) const AUTOSCROLL_MAX_LINES: i32 = 10;

/// 元素贴着窗口上 / 下沿时,边内这么宽的一条也算出界(见模块注释)。
pub(super) const EDGE_BAND: f32 = 5.0;

/// 这一下左键按下(已经确定走本地处理)该做什么。见模块注释的优先级。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LeftPress {
    /// 把已有选区的活动端挪到点击处,锚点与类型不动。
    Extend,
    /// ⌥+单击:先不动选区,松手前没拖过一格 = 定位光标,拖过了 = 转块选。
    AltClick,
    /// 新起一块选区。
    Start(SelectionType),
}

/// 判定一次本地左键按下。
///
/// - `click_count`:gpui 给的连击数(1 单击 / 2 双击 / 3+ 三击);
/// - `has_selection`:手上有一块看得见的本地选区(口径见模块注释);
/// - `cursor_move`:这一下能定位光标(没在回看里、宿主接了输入)。
pub(super) fn classify_left_press(
    click_count: usize,
    shift: bool,
    alt: bool,
    has_selection: bool,
    cursor_move: bool,
) -> LeftPress {
    if click_count <= 1 {
        if alt {
            return if cursor_move {
                LeftPress::AltClick
            } else {
                LeftPress::Start(SelectionType::Block)
            };
        }
        if shift && has_selection {
            return LeftPress::Extend;
        }
    }
    LeftPress::Start(match click_count {
        0 | 1 => SelectionType::Simple,
        2 => SelectionType::Semantic,
        _ => SelectionType::Lines,
    })
}

/// ⌥ 按下之后拖没拖过一格:横向满一列宽或纵向满一行高才算。
///
/// 按像素距离比而不是比「指针落在哪一格」:按在格子边上时手一抖就跨格,那会把一次定位
/// 光标的单击误判成一块空的块选(光标不动、选区也看不见,两头落空)。
pub(super) fn drag_exceeds_cell(dx: f32, dy: f32, cell_width: f32, cell_height: f32) -> bool {
    dx.abs() >= cell_width.max(1.0) || dy.abs() >= cell_height.max(1.0)
}

/// 自动滚屏的触发区(窗口坐标,纵向)。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct ScrollZone {
    /// 元素上沿。
    pub top: f32,
    /// 元素下沿(不含)。
    pub bottom: f32,
    /// 上沿往里算作出界的宽度(元素贴着窗口上沿时才有)。
    pub top_band: f32,
    /// 下沿往里算作出界的宽度(元素贴着窗口下沿时才有)。
    pub bottom_band: f32,
}

impl ScrollZone {
    /// 按元素上下沿与窗口高度定出触发区:贴着窗口哪条边,哪边就留一条 [`EDGE_BAND`]。
    pub fn new(top: f32, bottom: f32, viewport_height: f32) -> Self {
        let band = |touching: bool| if touching { EDGE_BAND } else { 0.0 };
        Self {
            top,
            bottom,
            top_band: band(top <= 0.5),
            bottom_band: band(bottom >= viewport_height - 0.5),
        }
    }
}

/// 拖选时指针在 `pointer_y`,这一拍朝哪边滚几行。正数 = 往回看(上),负数 = 往下,0 = 不滚。
///
/// 速度按越界距离分档:刚越过边沿一拍一行,每多 [`AUTOSCROLL_STEP_PX`] 多一行,封顶
/// [`AUTOSCROLL_MAX_LINES`]。已经滚到头(往上到了回看顶、往下已在底部)给 0 —— 调用方据此停表,
/// 不留一个空转的定时器。
pub(super) fn autoscroll_lines(
    pointer_y: f32,
    zone: ScrollZone,
    display_offset: usize,
    history: usize,
) -> i32 {
    let speed = |distance: f32| {
        let steps = (distance.max(0.0) / AUTOSCROLL_STEP_PX).floor() as i32;
        (1 + steps).min(AUTOSCROLL_MAX_LINES)
    };
    let upper = zone.top + zone.top_band;
    let lower = zone.bottom - zone.bottom_band;
    if pointer_y < upper {
        if display_offset >= history {
            return 0;
        }
        speed(upper - pointer_y)
    } else if pointer_y >= lower {
        if display_offset == 0 {
            return 0;
        }
        -speed(pointer_y - lower)
    } else {
        0
    }
}

/// 一次正在进行中的本地拖选的起选锚点。
///
/// `mt-terminal` 已在清行 / 清屏时保护 `Term::selection`，正常拖选只需 `sel.update(...)`；
/// 但某些局部滚动区操作（例如 `CSI nM` / `CSI nL` 让 `Selection::rotate` 越界返回 `None`）
/// 仍可能在按住鼠标拖动期间把 `term.selection` 置空。留一份锚点后，下一发 mouse-move 或
/// 自动滚屏节拍就能在同一块屏上补建选区，避免整次拖动后续全部变成空操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DragAnchor {
    pub ty: SelectionType,
    pub point: AlacPoint,
    pub side: Side,
    /// 起选那一刻的 `history_size`：主屏新输出把行顶进 scrollback 时按差值上移锚点行号。
    pub history: usize,
    /// 起选时是否在 alt screen：中途切屏后不再拿旧屏锚点去重建。
    pub alt_screen: bool,
}

/// 拖选移动 / 自动滚屏时更新选区活动端；若 `term.selection` 中途被置空且仍在同一块屏上，
/// 按 `anchor` 重建后再延伸到 `(target, side)`。
pub(super) fn update_or_restore_selection<T>(
    term: &mut Term<T>,
    anchor: Option<DragAnchor>,
    target: AlacPoint,
    side: Side,
) {
    if let Some(sel) = term.selection.as_mut() {
        sel.update(target, side);
        return;
    }
    let Some(anchor) = anchor else {
        return;
    };
    let alt_screen = term.mode().contains(TermMode::ALT_SCREEN);
    if alt_screen != anchor.alt_screen {
        return;
    }
    let scroll_delta = if alt_screen {
        0
    } else {
        term.history_size().saturating_sub(anchor.history) as i32
    };
    let line = Line(
        (anchor.point.line.0 - scroll_delta).clamp(term.topmost_line().0, term.bottommost_line().0),
    );
    let col = Column(anchor.point.column.0.min(term.last_column().0));
    let mut sel = Selection::new(anchor.ty, AlacPoint::new(line, col), anchor.side);
    sel.update(target, side);
    term.selection = Some(sel);
}

/// 这一格要不要画选中底色。
///
/// 普通格按 alacritty 的 [`SelectionRange::contains`] 逐格判(块选按列区间,这条它自己就管)。
/// 宽字符跨在选区边上时**任一半在里面就整字高亮**:复制时 alacritty 的 `line_to_string`
/// 遇到被选中的 spacer 会把宽字符本体一并带上、选区停在本体上时也带着整个字,画只亮半个字
/// 会与剪贴板对不上 —— 判法与 `SelectionRange::contains_cell` 同一口径,再补上 spacer 那半。
pub(super) fn cell_in_selection(range: &SelectionRange, point: AlacPoint, flags: Flags) -> bool {
    if range.contains(point) {
        return true;
    }
    if flags.contains(Flags::WIDE_CHAR) {
        return range.contains(AlacPoint::new(point.line, point.column + 1));
    }
    if flags.contains(Flags::WIDE_CHAR_SPACER) && point.column.0 > 0 {
        return range.contains(AlacPoint::new(point.line, point.column - 1));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::mouse::{MouseMods, prefers_local_handling};
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::TermMode;

    #[test]
    fn 普通单击双击三击照旧开三种选区() {
        let simple = classify_left_press(1, false, false, false, true);
        assert_eq!(simple, LeftPress::Start(SelectionType::Simple));
        let word = classify_left_press(2, false, false, true, true);
        assert_eq!(word, LeftPress::Start(SelectionType::Semantic));
        let line = classify_left_press(3, false, false, true, true);
        assert_eq!(line, LeftPress::Start(SelectionType::Lines));
        assert_eq!(classify_left_press(5, false, false, false, true), line);
    }

    #[test]
    fn shift_单击有选区才扩选() {
        assert_eq!(
            classify_left_press(1, true, false, true, true),
            LeftPress::Extend
        );
        // 没有选区:就是普通单击,从点击处起一块新的
        assert_eq!(
            classify_left_press(1, true, false, false, true),
            LeftPress::Start(SelectionType::Simple)
        );
        // Shift+双击不是扩选:照旧选词
        assert_eq!(
            classify_left_press(2, true, false, true, true),
            LeftPress::Start(SelectionType::Semantic)
        );
    }

    /// 鼠标上报开着时 Shift 先把鼠标借回本地,借回来之后扩不扩选只看有没有本地选区。
    #[test]
    fn 鼠标上报开着时_shift_先借回本地再判扩选() {
        let reporting = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let shift = MouseMods::new(true, false, false);
        assert!(prefers_local_handling(reporting, shift), "Shift 借回鼠标");
        assert!(
            !prefers_local_handling(reporting, MouseMods::default()),
            "不按 Shift 归程序,走不到本地判定"
        );
        // 借回来、手上没有本地选区:开始一块新的(Shift 借鼠标的原有行为)
        assert_eq!(
            classify_left_press(1, true, false, false, true),
            LeftPress::Start(SelectionType::Simple)
        );
        // 借回来、手上已有此前 Shift+拖出来的本地选区:扩选
        assert_eq!(
            classify_left_press(1, true, false, true, true),
            LeftPress::Extend
        );
    }

    #[test]
    fn alt_单击先挂起_定位不了光标就直接块选() {
        assert_eq!(
            classify_left_press(1, false, true, false, true),
            LeftPress::AltClick
        );
        // 在回看里 / 宿主没接输入:没什么可等的,当场开块选
        assert_eq!(
            classify_left_press(1, false, true, true, false),
            LeftPress::Start(SelectionType::Block)
        );
        // ⌥+双击照旧选词(与改动前一致)
        assert_eq!(
            classify_left_press(2, false, true, false, true),
            LeftPress::Start(SelectionType::Semantic)
        );
    }

    /// 从前 ⌥+单击不看 Shift:同时按着 Shift、手上又有选区,照旧是定位光标 / 块选,不是扩选。
    #[test]
    fn alt_的判定不因_shift_扩选而变() {
        assert_eq!(
            classify_left_press(1, true, true, true, true),
            LeftPress::AltClick
        );
        assert_eq!(
            classify_left_press(1, true, true, true, false),
            LeftPress::Start(SelectionType::Block)
        );
    }

    #[test]
    fn alt_拖满一格才转块选() {
        let (w, h) = (9.0, 18.0);
        assert!(!drag_exceeds_cell(0.0, 0.0, w, h));
        assert!(!drag_exceeds_cell(8.9, -17.9, w, h), "格内抖动不算");
        assert!(drag_exceeds_cell(9.0, 0.0, w, h), "横向满一列");
        assert!(drag_exceeds_cell(-9.5, 0.0, w, h), "往左同样算");
        assert!(drag_exceeds_cell(0.0, 18.0, w, h), "纵向满一行");
        // 尺寸还没量出来(0)时按 1px 兜底,不至于一按就算拖
        assert!(!drag_exceeds_cell(0.5, 0.5, 0.0, 0.0));
    }

    fn zone() -> ScrollZone {
        ScrollZone {
            top: 100.0,
            bottom: 500.0,
            top_band: 0.0,
            bottom_band: 0.0,
        }
    }

    #[test]
    fn 区内不滚_越过上下边朝外滚() {
        let z = zone();
        assert_eq!(autoscroll_lines(100.0, z, 5, 100), 0, "上沿本身在区内");
        assert_eq!(autoscroll_lines(300.0, z, 5, 100), 0);
        assert_eq!(autoscroll_lines(499.9, z, 5, 100), 0);
        assert_eq!(autoscroll_lines(99.0, z, 5, 100), 1, "越过上沿往回看");
        assert_eq!(
            autoscroll_lines(500.0, z, 5, 100),
            -1,
            "下沿不含,到下沿就往下"
        );
    }

    #[test]
    fn 离边越远滚得越快且有上限() {
        let z = zone();
        let step = AUTOSCROLL_STEP_PX;
        assert_eq!(autoscroll_lines(100.0 - 1.0, z, 0, 1000), 1);
        assert_eq!(autoscroll_lines(100.0 - step - 1.0, z, 0, 1000), 2);
        assert_eq!(autoscroll_lines(100.0 - step * 3.0 - 1.0, z, 0, 1000), 4);
        assert_eq!(
            autoscroll_lines(-10_000.0, z, 0, 1000),
            AUTOSCROLL_MAX_LINES,
            "拖到屏幕最边上也封顶"
        );
        assert_eq!(autoscroll_lines(500.0 + step * 2.0, z, 50, 1000), -3);
        assert_eq!(
            autoscroll_lines(10_000.0, z, 50, 1000),
            -AUTOSCROLL_MAX_LINES
        );
    }

    #[test]
    fn 滚到头就不滚() {
        let z = zone();
        // 已在回看顶:往上没得滚
        assert_eq!(autoscroll_lines(50.0, z, 100, 100), 0);
        // 已在底部:往下没得滚
        assert_eq!(autoscroll_lines(600.0, z, 0, 100), 0);
        // 没有回看(alt screen 的 grid 就是这样):两头都不滚
        assert_eq!(autoscroll_lines(50.0, z, 0, 0), 0);
        assert_eq!(autoscroll_lines(600.0, z, 0, 0), 0);
    }

    #[test]
    fn 贴着窗口边时边内窄带也算出界() {
        // 元素下沿就是窗口下沿:指针出不了窗口,边内 EDGE_BAND 要能触发
        let z = ScrollZone::new(40.0, 800.0, 800.0);
        assert_eq!(z.top_band, 0.0, "上沿没贴窗口,不留带");
        assert_eq!(z.bottom_band, EDGE_BAND);
        assert_eq!(autoscroll_lines(800.0 - EDGE_BAND, z, 10, 100), -1);
        assert_eq!(autoscroll_lines(800.0 - EDGE_BAND - 1.0, z, 10, 100), 0);
        // 分屏里的下半格不贴窗口:不留带,只有真出界才滚
        let inner = ScrollZone::new(400.0, 600.0, 800.0);
        assert_eq!(inner.bottom_band, 0.0);
        assert_eq!(autoscroll_lines(599.0, inner, 10, 100), 0);
        // 贴着窗口上沿同理
        let top = ScrollZone::new(0.0, 300.0, 800.0);
        assert_eq!(top.top_band, EDGE_BAND);
        assert_eq!(autoscroll_lines(2.0, top, 0, 100), 1);
    }

    fn p(line: i32, col: usize) -> AlacPoint {
        AlacPoint::new(Line(line), Column(col))
    }

    #[test]
    fn 块选按列区间判() {
        // 第 0..=2 行、第 2..=4 列的矩形
        let block = SelectionRange::new(p(0, 2), p(2, 4), true);
        assert!(cell_in_selection(&block, p(1, 3), Flags::empty()));
        assert!(
            !cell_in_selection(&block, p(1, 0), Flags::empty()),
            "矩形左边外"
        );
        assert!(
            !cell_in_selection(&block, p(1, 7), Flags::empty()),
            "矩形右边外"
        );
        assert!(
            !cell_in_selection(&block, p(3, 3), Flags::empty()),
            "矩形下边外"
        );
        // 同样的端点按普通选区算,中间那行整行都在里面
        let simple = SelectionRange::new(p(0, 2), p(2, 4), false);
        assert!(cell_in_selection(&simple, p(1, 0), Flags::empty()));
        assert!(cell_in_selection(&simple, p(1, 7), Flags::empty()));
    }

    #[test]
    fn 宽字符任一半在选区里就整字高亮() {
        // 矩形从第 3 列起:第 2 列宽字符的 spacer 落在第 3 列
        let block = SelectionRange::new(p(0, 3), p(1, 5), true);
        assert!(
            cell_in_selection(&block, p(0, 2), Flags::WIDE_CHAR),
            "本体在边外、spacer 在里面:本体也亮"
        );
        assert!(cell_in_selection(&block, p(0, 3), Flags::WIDE_CHAR_SPACER));
        // 矩形停在第 5 列:第 5 列宽字符的 spacer(第 6 列)跟着亮
        assert!(cell_in_selection(&block, p(0, 6), Flags::WIDE_CHAR_SPACER));
        // 普通窄字符不沾边
        assert!(!cell_in_selection(&block, p(0, 2), Flags::empty()));
        assert!(!cell_in_selection(&block, p(0, 6), Flags::empty()));
        // 第 0 列的 spacer(理论上不存在)不往左越界
        assert!(!cell_in_selection(&block, p(0, 0), Flags::WIDE_CHAR_SPACER));
    }

    #[test]
    fn 拖选中途选区被清时按锚点重建并跟滚屏上移() {
        use mt_terminal::{TermSize, TerminalEmulator};

        let e = TerminalEmulator::with_scrollback(TermSize::new(20, 4), 50);
        e.advance(b"row-0\r\nrow-1\r\nrow-2\r\nrow-3");
        let anchor = e.with_term_mut(|term| {
            let point = p(1, 0);
            term.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
            DragAnchor {
                ty: SelectionType::Simple,
                point,
                side: Side::Left,
                history: term.grid().history_size(),
                alt_screen: false,
            }
        });

        // 模拟拖选中途有新行滚入历史且局部滚动区把 term.selection 置空
        e.advance(b"\r\nrow-4\r\nrow-5");
        e.with_term_mut(|term| {
            term.selection = None;
            // 原来的 row-1 已从 Line(1) 滚到 Line(-1)；把活动端拖到当前视口 Line(0) 第 4 列
            update_or_restore_selection(term, Some(anchor), p(0, 4), Side::Right);
        });
        assert_eq!(
            e.with_term(|term| term.selection_to_string()).as_deref(),
            Some("row-1\nrow-2")
        );
    }

    #[test]
    fn 拖选中途切屏后不拿旧屏锚点重建() {
        use mt_terminal::{TermSize, TerminalEmulator};

        let e = TerminalEmulator::new(TermSize::new(20, 4));
        e.advance(b"primary");
        let anchor = DragAnchor {
            ty: SelectionType::Simple,
            point: p(0, 0),
            side: Side::Left,
            history: 0,
            alt_screen: false,
        };
        // 切入 alt screen 会清空主屏选区
        e.advance(b"\x1b[?1049h\x1b[Halt-text");
        e.with_term_mut(|term| {
            update_or_restore_selection(term, Some(anchor), p(0, 5), Side::Right);
            assert!(term.selection.is_none(), "跨屏不该复活旧屏锚点");
        });
    }

    #[test]
    fn 块选锚点中途被清也能按块选重建且安全钳界() {
        use mt_terminal::{TermSize, TerminalEmulator};

        let e = TerminalEmulator::new(TermSize::new(10, 3));
        e.advance(b"012345\r\nabcdef");
        let anchor = DragAnchor {
            ty: SelectionType::Block,
            point: p(0, 1),
            side: Side::Left,
            history: 0,
            alt_screen: false,
        };
        e.with_term_mut(|term| {
            term.selection = None;
            update_or_restore_selection(term, Some(anchor), p(1, 3), Side::Right);
            let range = term
                .selection
                .as_ref()
                .and_then(|s| s.to_range(term))
                .unwrap();
            assert!(range.is_block, "应保持块选类型");
            assert_eq!(term.selection_to_string().as_deref(), Some("123\nbcd"));
        });
    }
}
