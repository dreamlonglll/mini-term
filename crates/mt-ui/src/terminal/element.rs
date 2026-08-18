//! [`TerminalElement`] —— 把 [`mt_terminal::TerminalEmulator`] 的 grid 画成 GPUI 元素。
//!
//! # 为什么是自定义 Element 而不是拼 div
//!
//! 一屏 200×50 = 一万个格子。用 div 拼会造出一万个 taffy 节点,布局阶段就废了;
//! 而且 flex 布局给不出「第 N 列的 x 恰好是 N × cell_width」这种硬保证。
//! 自定义 Element 直接进 `request_layout / prepaint / paint` 三段式,
//! 布局只有一个节点,格子位置全是算出来的。
//!
//! # 逐列对齐(验收项第 1 条)怎么保证
//!
//! cell 宽度取主字体 `'M'` 的 advance,**不取整**。然后把每个 cell 分成两类:
//!
//! - **可合并**:主字体有这个 glyph,且它的 advance 恰好等于 cell 宽度。
//!   连续的同款式可合并 cell 拼成一个 [`ShapedLine`] 一次画完 —— 因为每个字形的
//!   自然步进就等于列宽,shaping 出来的位置天生落在列格上,不需要任何事后校正。
//! - **不可合并**:宽字符(CJK / emoji)、主字体缺字要回退、带组合符号的格子。
//!   这些**单独 shape、单独画在 `col × cell_width` 上** —— 位置由我们指定,
//!   字形宽度对不上也只是它自己糊出边界,绝不会把后面的列顶歪。
//!
//! 这条分界是整个渲染器的地基:中英混排的对齐不依赖「CJK 恰好是两倍宽」这种
//! 字体侧的巧合,而是由「每个非等宽格子都自己定位」保证的。
//!
//! gpui 的 `shape_line(.., force_width)` **没被采用**:它按 glyph 序号硬掰位置,
//! 一是宽字符占两列的语义它不认(一个 glyph 只算一列),二是误差 ≤1px 时它不纠正,
//! 留下 ±1px 抖动。
//!
//! # 背景图透出
//!
//! 背景色是「默认背景」的格子**不发 quad**(见 [`super::colors::is_default_background`])。
//! 判据看的是 `Color::Named(Background)` 这个语义,不是解析后的 RGB —— 否则主题背景
//! 与某个 ANSI 色撞色时会误判成透明。

use std::cell::{Cell as StdCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Line, Point as AlacPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::CursorShape;
use gpui::{
    App, Bounds, ClipboardItem, ContentMask, DispatchPhase, Element, ElementId, FocusHandle,
    FontId, GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    ScrollDelta, ScrollWheelEvent, ShapedLine, SharedString, Size, Style, StrikethroughStyle,
    TextRun, UnderlineStyle, Window, fill, point, px, size,
};
use mt_terminal::{TermSize, TerminalEmulator};

use super::colors;
use super::theme::{TerminalStyle, TerminalTheme};

/// 一屏最多认多少列/行。窗口被拖到荒谬尺寸时防止 grid 爆掉。
const MAX_COLUMNS: usize = 1024;
const MAX_LINES: usize = 512;

/// 一个格子最多带几个组合符号。alacritty 内部也是这个上限。
const MAX_ZEROWIDTH_CHARS: usize = 5;

/// `Pixels` 的内部字段是 `pub(crate)`,取标量只能走 `From`。写成短名字省得刷屏。
#[inline]
fn f(p: Pixels) -> f32 {
    f32::from(p)
}

/// grid 尺寸变化的通知。宿主拿到后要把 PTY 也 resize 到同样大小。
pub type OnGridResize = Rc<dyn Fn(TermSize, &mut Window, &mut App)>;
/// IME 挂载点:paint 阶段拿元素 bounds 回调宿主,宿主在里面调
/// `window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx)`。
///
/// 元素本身不是 Entity,拿不出 `EntityInputHandler`,所以这个位子只能由宿主填。
/// 本轮没人填 —— 中文 IME 是验收项第 2 条,留待下一轮。
pub type InstallInputHandler = Rc<dyn Fn(Bounds<Pixels>, &mut Window, &mut App)>;
/// 元素要往 PTY 写字节时的出口(alt screen 下的滚轮、将来的鼠标上报)。
/// 元素不持有 PTY,这条只能交回宿主。
pub type OnInput = Rc<dyn Fn(&[u8], &mut Window, &mut App)>;

pub struct TerminalElement {
    id: ElementId,
    emulator: Arc<TerminalEmulator>,
    style: TerminalStyle,
    theme: TerminalTheme,
    focus: FocusHandle,
    on_grid_resize: Option<OnGridResize>,
    install_input_handler: Option<InstallInputHandler>,
    on_input: Option<OnInput>,
}

impl TerminalElement {
    pub fn new(
        id: impl Into<ElementId>,
        emulator: Arc<TerminalEmulator>,
        focus: FocusHandle,
        style: TerminalStyle,
        theme: TerminalTheme,
    ) -> Self {
        Self {
            id: id.into(),
            emulator,
            style,
            theme,
            focus,
            on_grid_resize: None,
            install_input_handler: None,
            on_input: None,
        }
    }

    /// grid 尺寸变了就回调。宿主在这里把 PTY resize 到同样的 rows/cols。
    pub fn on_grid_resize(mut self, f: impl Fn(TermSize, &mut Window, &mut App) + 'static) -> Self {
        self.on_grid_resize = Some(Rc::new(f));
        self
    }

    /// 见 [`InstallInputHandler`]。
    pub fn with_input_handler(
        mut self,
        f: impl Fn(Bounds<Pixels>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.install_input_handler = Some(Rc::new(f));
        self
    }

    /// 见 [`OnInput`]。
    pub fn on_input(mut self, f: impl Fn(&[u8], &mut Window, &mut App) + 'static) -> Self {
        self.on_input = Some(Rc::new(f));
        self
    }
}

/// 跨帧保留的一点点交互状态。元素每帧重建,这些必须挂在 `GlobalElementId` 上。
#[derive(Clone, Default)]
struct TerminalElementState {
    /// 滚轮的像素余量。触控板给的是零点几行,不攒起来会一直滚不动。
    scroll_remainder: Rc<StdCell<f32>>,
    /// 左键是否正在拖选。
    selecting: Rc<StdCell<bool>>,
}

/// 一个待绘制的文本片段:已 shape 好的一行(或一格),画在 `origin` 的左上角。
struct TextPiece {
    origin: Point<Pixels>,
    line: ShapedLine,
}

struct CursorLayout {
    bounds: Bounds<Pixels>,
    shape: CursorShape,
    color: Hsla,
}

pub struct PreparedFrame {
    hitbox: Hitbox,
    state: TerminalElementState,
    cell_size: Size<Pixels>,
    origin: Point<Pixels>,
    columns: usize,
    screen_lines: usize,
    mode: TermMode,
    backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    selections: Vec<Bounds<Pixels>>,
    texts: Vec<TextPiece>,
    cursor: Option<CursorLayout>,
}

// 「这个字符在这套字体里的步进正好是一列宽吗」的缓存。
//
// 每帧对每个格子问一次,不缓存就是每帧几千次 DirectWrite 往返。key 带 font_id
// 与字号,粗体/斜体是不同的 font_id,各自算各自的。
thread_local! {
    static ADVANCE_FITS: RefCell<HashMap<(FontId, u32, char), bool>> = RefCell::new(HashMap::new());
}

fn advance_fits_cell(
    window: &Window,
    font_id: FontId,
    font_size: Pixels,
    ch: char,
    cell_width: Pixels,
) -> bool {
    let key = (font_id, f(font_size).to_bits(), ch);
    if let Some(hit) = ADVANCE_FITS.with(|c| c.borrow().get(&key).copied()) {
        return hit;
    }
    let fits = window
        .text_system()
        .advance(font_id, font_size, ch)
        .map(|adv| (f(adv.width) - f(cell_width)).abs() < 0.01)
        .unwrap_or(false);
    ADVANCE_FITS.with(|c| c.borrow_mut().insert(key, fits));
    fits
}

/// 参与「能否与相邻格子合并成一个 ShapedLine」判定的款式。
#[derive(Clone, Copy, PartialEq)]
struct RunStyle {
    fg: Hsla,
    bold: bool,
    italic: bool,
    underline: Option<UnderlineStyle>,
    strikethrough: Option<StrikethroughStyle>,
}

impl RunStyle {
    fn same(&self, other: &Self) -> bool {
        self.fg == other.fg
            && self.bold == other.bold
            && self.italic == other.italic
            && underline_eq(&self.underline, &other.underline)
            && strikethrough_eq(&self.strikethrough, &other.strikethrough)
    }
}

fn underline_eq(a: &Option<UnderlineStyle>, b: &Option<UnderlineStyle>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.thickness == b.thickness && a.color == b.color && a.wavy == b.wavy,
        _ => false,
    }
}

fn strikethrough_eq(a: &Option<StrikethroughStyle>, b: &Option<StrikethroughStyle>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.thickness == b.thickness && a.color == b.color,
        _ => false,
    }
}

impl TerminalElement {
    /// 像素坐标 → grid 坐标 + 落在格子的哪半边(选择区要靠 side 决定端点归属)。
    fn hit_grid(
        pos: Point<Pixels>,
        origin: Point<Pixels>,
        cell_size: Size<Pixels>,
        columns: usize,
        screen_lines: usize,
        display_offset: usize,
    ) -> (AlacPoint, Side) {
        let rel_x = f(pos.x - origin.x).max(0.0);
        let rel_y = f(pos.y - origin.y).max(0.0);
        let col_f = rel_x / f(cell_size.width).max(1.0);
        let row_f = rel_y / f(cell_size.height).max(1.0);
        let col = (col_f.floor() as usize).min(columns.saturating_sub(1));
        let row = (row_f.floor() as usize).min(screen_lines.saturating_sub(1));
        let side = if col_f - col_f.floor() > 0.5 {
            Side::Right
        } else {
            Side::Left
        };
        (
            AlacPoint::new(Line(row as i32 - display_offset as i32), Column(col)),
            side,
        )
    }

    fn paint_mouse_listeners(&self, prepared: &PreparedFrame, window: &mut Window, _cx: &mut App) {
        let hitbox = prepared.hitbox.clone();
        let origin = prepared.origin;
        let cell_size = prepared.cell_size;
        let columns = prepared.columns;
        let screen_lines = prepared.screen_lines;
        let state = prepared.state.clone();
        let alt_screen = prepared.mode.contains(TermMode::ALT_SCREEN);

        // ── 滚轮:改 display_offset(回看)。alt screen(vim / less 这类全屏程序)
        //    没有回看缓冲,改成等价地敲方向键 —— 这也是 xterm 一贯的做法。
        {
            let emulator = self.emulator.clone();
            let hitbox = hitbox.clone();
            let remainder = state.scroll_remainder.clone();
            let on_input = self.on_input.clone();
            let app_cursor = prepared.mode.contains(TermMode::APP_CURSOR);
            window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.should_handle_scroll(window) {
                    return;
                }
                let lines = match event.delta {
                    ScrollDelta::Lines(p) => p.y,
                    ScrollDelta::Pixels(p) => f(p.y) / f(cell_size.height).max(1.0),
                };
                let total = remainder.get() + lines;
                let whole = total.trunc();
                remainder.set(total - whole);
                if whole == 0.0 {
                    return;
                }
                if alt_screen {
                    let Some(on_input) = on_input.as_ref() else {
                        return;
                    };
                    let seq: &[u8] = match (whole > 0.0, app_cursor) {
                        (true, false) => b"\x1b[A",
                        (true, true) => b"\x1bOA",
                        (false, false) => b"\x1b[B",
                        (false, true) => b"\x1bOB",
                    };
                    let mut payload = Vec::new();
                    for _ in 0..whole.abs() as usize {
                        payload.extend_from_slice(seq);
                    }
                    on_input(&payload, window, cx);
                    return;
                }
                emulator.with_term_mut(|term| term.scroll_display(Scroll::Delta(whole as i32)));
                window.refresh();
            });
        }

        // ── 左键按下:开选。双击 = 语义选词,三击 = 选整行。
        {
            let emulator = self.emulator.clone();
            let hitbox = hitbox.clone();
            let selecting = state.selecting.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, _cx| {
                if phase != DispatchPhase::Bubble
                    || event.button != MouseButton::Left
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                let display_offset = emulator.with_term(|t| t.grid().display_offset());
                let (point, side) = Self::hit_grid(
                    event.position,
                    origin,
                    cell_size,
                    columns,
                    screen_lines,
                    display_offset,
                );
                let ty = match event.click_count {
                    1 => SelectionType::Simple,
                    2 => SelectionType::Semantic,
                    _ => SelectionType::Lines,
                };
                emulator.with_term_mut(|term| {
                    term.selection = Some(Selection::new(ty, point, side));
                });
                selecting.set(event.click_count == 1);
                window.refresh();
            });
        }

        // ── 拖动:延伸选择区。
        {
            let emulator = self.emulator.clone();
            let hitbox = hitbox.clone();
            let selecting = state.selecting.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, _cx| {
                if phase != DispatchPhase::Bubble
                    || !selecting.get()
                    || event.pressed_button != Some(MouseButton::Left)
                {
                    return;
                }
                let _ = &hitbox; // 拖出元素外也要继续选,所以这里不判 hover
                let display_offset = emulator.with_term(|t| t.grid().display_offset());
                let (point, side) = Self::hit_grid(
                    event.position,
                    origin,
                    cell_size,
                    columns,
                    screen_lines,
                    display_offset,
                );
                emulator.with_term_mut(|term| {
                    if let Some(sel) = term.selection.as_mut() {
                        sel.update(point, side);
                    }
                });
                window.refresh();
            });
        }

        // ── 松开:结束拖选,顺手把选中文本送进剪贴板。
        //    (X11 primary selection 的习惯;Ctrl+Shift+C 由宿主再走一遍也无妨)
        {
            let emulator = self.emulator.clone();
            let selecting = state.selecting.clone();
            window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                    return;
                }
                if !selecting.replace(false) {
                    return;
                }
                if let Some(text) = emulator.with_term(|t| t.selection_to_string())
                    && !text.is_empty()
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            });
        }
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = PreparedFrame;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = gpui::relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let state: TerminalElementState = window
            .with_element_state::<TerminalElementState, _>(id.unwrap(), |prev, _window| {
                let s = prev.unwrap_or_default();
                (s.clone(), s)
            });

        let font = self.style.font();
        let font_size = self.style.font_size;
        let variant_fonts = VariantFonts::resolve(window, &font);
        let font_id = variant_fonts.id(false, false);
        // cell 宽度 = 主字体 'M' 的 advance。**不取整** —— 一取整就与字形的自然
        // 步进对不上,合并绘制的那条快路就失去「天生落在列格上」的前提。
        let cell_width = window
            .text_system()
            .advance(font_id, font_size, 'M')
            .map(|s| s.width)
            .unwrap_or_else(|_| px(f(font_size) * 0.6));
        let line_height = self.style.line_height_px();
        let cell_size = size(cell_width, line_height);
        report_metrics_once(window, font_id, font_size, cell_width, line_height, &self.style);

        // ── grid 尺寸随可用像素走
        let columns = ((f(bounds.size.width) / f(cell_width).max(1.0)).floor() as usize)
            .clamp(2, MAX_COLUMNS);
        let screen_lines = ((f(bounds.size.height) / f(line_height).max(1.0)).floor() as usize)
            .clamp(1, MAX_LINES);
        let target = TermSize::new(columns, screen_lines);
        if self.emulator.term_size() != target {
            self.emulator.resize(target);
            if let Some(cb) = self.on_grid_resize.clone() {
                cb(target, window, cx);
            }
        }

        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let focused = self.focus.is_focused(window);

        // ── 读 grid,攒出这一帧要画的东西
        let mut backgrounds: Vec<(Bounds<Pixels>, Hsla)> = Vec::new();
        let mut selections: Vec<Bounds<Pixels>> = Vec::new();
        let mut pieces: Vec<PendingPiece> = Vec::new();
        let mut cursor: Option<CursorLayout> = None;
        let mode;

        {
            let term_lock = self.emulator.term().lock();
            let content = term_lock.renderable_content();
            let display_offset = content.display_offset;
            mode = content.mode;
            let colors_table = content.colors;
            let selection_range = content.selection;
            let cursor_point = content.cursor.point;
            let cursor_shape = content.cursor.shape;

            // 每行的累加器
            let mut row_ix: Option<usize> = None;
            let mut bg_run: Option<(usize, usize, Hsla)> = None; // (start_col, end_col, color)
            let mut sel_run: Option<(usize, usize)> = None;
            let mut text_run: Option<PendingRun> = None;

            for indexed in content.display_iter {
                let row = (indexed.point.line.0 + display_offset as i32).max(0) as usize;
                if row_ix != Some(row) {
                    flush_bg(&mut bg_run, row_ix, cell_size, &mut backgrounds);
                    flush_sel(&mut sel_run, row_ix, cell_size, &mut selections);
                    flush_text(&mut text_run, &mut pieces);
                    row_ix = Some(row);
                }
                let col = indexed.point.column.0;
                let cell: &Cell = indexed.cell;
                let flags = cell.flags;

                // ── 颜色:INVERSE 就把前后景对调
                let mut fg = colors::foreground(cell.fg, flags, colors_table, &self.theme);
                let mut bg = colors::background(cell.bg, colors_table, &self.theme);
                let mut bg_is_default = colors::is_default_background(cell.bg, flags);
                if flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                    bg_is_default = false;
                }
                if flags.contains(Flags::HIDDEN) {
                    fg = bg;
                }

                // ── 背景:默认背景不发 quad(背景图从这里透出来)
                if bg_is_default {
                    flush_bg(&mut bg_run, row_ix, cell_size, &mut backgrounds);
                } else {
                    match bg_run.as_mut() {
                        Some((_, end, color)) if *color == bg && *end + 1 == col => *end = col,
                        _ => {
                            flush_bg(&mut bg_run, row_ix, cell_size, &mut backgrounds);
                            bg_run = Some((col, col, bg));
                        }
                    }
                }

                // ── 选择区
                let selected = selection_range
                    .map(|r| r.contains(indexed.point))
                    .unwrap_or(false);
                if selected {
                    match sel_run.as_mut() {
                        Some((_, end)) if *end + 1 == col => *end = col,
                        _ => {
                            flush_sel(&mut sel_run, row_ix, cell_size, &mut selections);
                            sel_run = Some((col, col));
                        }
                    }
                } else {
                    flush_sel(&mut sel_run, row_ix, cell_size, &mut selections);
                }

                // ── 光标
                let is_cursor = indexed.point == cursor_point && cursor_shape != CursorShape::Hidden;
                if is_cursor {
                    let width = if flags.contains(Flags::WIDE_CHAR) {
                        cell_width * 2.0
                    } else {
                        cell_width
                    };
                    cursor = Some(CursorLayout {
                        bounds: Bounds::new(
                            point(
                                bounds.origin.x + cell_width * col as f32,
                                bounds.origin.y + line_height * row as f32,
                            ),
                            size(width, line_height),
                        ),
                        shape: cursor_shape,
                        color: self.theme.cursor,
                    });
                    if focused && cursor_shape == CursorShape::Block {
                        // 块状光标底下的字反白
                        fg = self.theme.cursor_text;
                    }
                }

                // ── 文本
                //    WIDE_CHAR 的第二列(spacer)没有字形,跳过;它的背景已经由
                //    上面那段处理过了。
                if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    flush_text(&mut text_run, &mut pieces);
                    continue;
                }

                let style_key = RunStyle {
                    fg,
                    bold: flags.contains(Flags::BOLD),
                    italic: flags.contains(Flags::ITALIC),
                    underline: underline_style(flags, fg),
                    strikethrough: flags.contains(Flags::STRIKEOUT).then(|| StrikethroughStyle {
                        thickness: px(1.0),
                        color: Some(fg),
                    }),
                };

                let zerowidth = cell.zerowidth().unwrap_or(&[]);
                let run_font_id = variant_fonts.id(style_key.bold, style_key.italic);
                // 可合并的条件:窄字符、无组合符号、不是光标格(光标格颜色单独)、
                // 且主字体里这个字形的步进恰好一列宽。
                let mergeable = !flags.contains(Flags::WIDE_CHAR)
                    && zerowidth.is_empty()
                    && !is_cursor
                    && advance_fits_cell(window, run_font_id, font_size, cell.c, cell_width);

                if mergeable {
                    match text_run.as_mut() {
                        Some(run)
                            if run.style.same(&style_key) && run.start + run.len == col =>
                        {
                            run.text.push(cell.c);
                            run.len += 1;
                        }
                        _ => {
                            flush_text(&mut text_run, &mut pieces);
                            let mut text = String::new();
                            text.push(cell.c);
                            text_run = Some(PendingRun {
                                row,
                                start: col,
                                len: 1,
                                text,
                                style: style_key,
                            });
                        }
                    }
                } else {
                    flush_text(&mut text_run, &mut pieces);
                    let mut text = String::new();
                    text.push(cell.c);
                    for z in zerowidth.iter().take(MAX_ZEROWIDTH_CHARS) {
                        text.push(*z);
                    }
                    pieces.push(PendingPiece {
                        row,
                        start: col,
                        text,
                        style: style_key,
                    });
                }
            }
            flush_bg(&mut bg_run, row_ix, cell_size, &mut backgrounds);
            flush_sel(&mut sel_run, row_ix, cell_size, &mut selections);
            flush_text(&mut text_run, &mut pieces);
        }

        // ── 把攒好的片段 shape 成 ShapedLine。
        //    锁已经放掉了 —— shaping 会往 DirectWrite 里跑,别拿着 grid 锁做。
        let mut texts = Vec::with_capacity(pieces.len());
        for piece in pieces {
            if piece.style.underline.is_none()
                && piece.style.strikethrough.is_none()
                && piece.text.chars().all(|c| c == ' ')
            {
                continue; // 纯空白且无下划线/删除线:没有任何像素,不必 shape
            }
            let mut run_font = font.clone();
            if piece.style.bold {
                run_font.weight = gpui::FontWeight::BOLD;
            }
            if piece.style.italic {
                run_font.style = gpui::FontStyle::Italic;
            }
            let run = TextRun {
                len: piece.text.len(),
                font: run_font,
                color: piece.style.fg,
                background_color: None,
                underline: piece.style.underline,
                strikethrough: piece.style.strikethrough,
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(piece.text),
                font_size,
                &[run],
                None,
            );
            texts.push(TextPiece {
                origin: point(
                    bounds.origin.x + cell_width * piece.start as f32,
                    bounds.origin.y + line_height * piece.row as f32,
                ),
                line: shaped,
            });
        }

        // 背景/选择区的 bounds 上面是按「相对 0」算的,这里统一平移到元素原点。
        for (b, _) in backgrounds.iter_mut() {
            b.origin += bounds.origin;
        }
        for b in selections.iter_mut() {
            b.origin += bounds.origin;
        }

        PreparedFrame {
            hitbox,
            state,
            cell_size,
            origin: bounds.origin,
            columns,
            screen_lines,
            mode,
            backgrounds,
            selections,
            texts,
            cursor,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepared: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focused = self.focus.is_focused(window);

        if let Some(install) = self.install_input_handler.clone() {
            install(bounds, window, cx);
        }

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (rect, color) in prepared.backgrounds.iter() {
                window.paint_quad(fill(*rect, *color));
            }
            for rect in prepared.selections.iter() {
                window.paint_quad(fill(*rect, self.theme.selection));
            }
            // 块状光标画在文字底下(文字用反白色);其余形状画在文字之上。
            if let Some(c) = prepared.cursor.as_ref()
                && focused
                && c.shape == CursorShape::Block
            {
                window.paint_quad(fill(c.bounds, c.color));
            }
            for piece in prepared.texts.iter() {
                _ = piece.line.paint(piece.origin, prepared.cell_size.height, window, cx);
            }
            if let Some(c) = prepared.cursor.as_ref() {
                match (focused, c.shape) {
                    (true, CursorShape::Block) => {}
                    (false, CursorShape::Block) | (_, CursorShape::HollowBlock) => {
                        paint_hollow_rect(window, c.bounds, c.color);
                    }
                    (_, CursorShape::Beam) => {
                        window.paint_quad(fill(
                            Bounds::new(c.bounds.origin, size(px(2.0), c.bounds.size.height)),
                            c.color,
                        ));
                    }
                    (_, CursorShape::Underline) => {
                        window.paint_quad(fill(
                            Bounds::new(
                                point(
                                    c.bounds.origin.x,
                                    c.bounds.origin.y + c.bounds.size.height - px(2.0),
                                ),
                                size(c.bounds.size.width, px(2.0)),
                            ),
                            c.color,
                        ));
                    }
                    (_, CursorShape::Hidden) => {}
                }
            }
        });

        self.paint_mouse_listeners(prepared, window, cx);
    }
}

/// 未 shape 的合并运行段。
struct PendingRun {
    row: usize,
    start: usize,
    len: usize,
    text: String,
    style: RunStyle,
}

/// 未 shape 的绘制片段(合并段落地后、或单格的宽字符)。
struct PendingPiece {
    row: usize,
    start: usize,
    text: String,
    style: RunStyle,
}

fn flush_text(run: &mut Option<PendingRun>, out: &mut Vec<PendingPiece>) {
    if let Some(r) = run.take() {
        out.push(PendingPiece {
            row: r.row,
            start: r.start,
            text: r.text,
            style: r.style,
        });
    }
}

fn flush_bg(
    run: &mut Option<(usize, usize, Hsla)>,
    row: Option<usize>,
    cell: Size<Pixels>,
    out: &mut Vec<(Bounds<Pixels>, Hsla)>,
) {
    let (Some((start, end, color)), Some(row)) = (run.take(), row) else {
        return;
    };
    out.push((rect_for(start, end, row, cell), color));
}

fn flush_sel(
    run: &mut Option<(usize, usize)>,
    row: Option<usize>,
    cell: Size<Pixels>,
    out: &mut Vec<Bounds<Pixels>>,
) {
    let (Some((start, end)), Some(row)) = (run.take(), row) else {
        return;
    };
    out.push(rect_for(start, end, row, cell));
}

fn rect_for(start: usize, end: usize, row: usize, cell: Size<Pixels>) -> Bounds<Pixels> {
    Bounds::new(
        point(cell.width * start as f32, cell.height * row as f32),
        size(cell.width * (end - start + 1) as f32, cell.height),
    )
}

fn underline_style(flags: Flags, color: Hsla) -> Option<UnderlineStyle> {
    if !flags.intersects(Flags::ALL_UNDERLINES) {
        return None;
    }
    Some(UnderlineStyle {
        // gpui 只有 wavy 一个花样,DOUBLE / DOTTED / DASHED 统一降级成实线。
        thickness: if flags.contains(Flags::DOUBLE_UNDERLINE) {
            px(2.0)
        } else {
            px(1.0)
        },
        color: Some(color),
        wavy: flags.contains(Flags::UNDERCURL),
    })
}

/// 首帧自检:量一遍 `M` / `i` / `W` 的步进。
///
/// 三者不等 = 解析到的根本不是等宽字体(配的字体名在本机不存在,gpui 悄悄回退到
/// 了 UI 字体)。这种情况下画面会整体歪掉,但从代码里看不出任何异常 ——
/// 所以在这里主动喊一声。`MT_UI_DEBUG_METRICS=1` 时无论正常与否都把度量打出来,
/// 「双终端对照 + 逐列测量」验收时对得上号。
fn report_metrics_once(
    window: &Window,
    font_id: FontId,
    font_size: Pixels,
    cell_width: Pixels,
    line_height: Pixels,
    style: &TerminalStyle,
) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let adv = |ch: char| {
            window
                .text_system()
                .advance(font_id, font_size, ch)
                .map(|s| f(s.width))
                .unwrap_or(f32::NAN)
        };
        let (m, i, w) = (adv('M'), adv('i'), adv('W'));
        let monospaced = (m - i).abs() < 0.01 && (m - w).abs() < 0.01;
        if !monospaced {
            eprintln!(
                "[mt-ui] 警告:字体 `{}` 解析结果不是等宽(M={m:.3} i={i:.3} W={w:.3}),\
                 终端逐列对齐会失效 —— 多半是这个字体族在本机不存在,被回退成了 UI 字体",
                style.font_family
            );
        }
        if std::env::var_os("MT_UI_DEBUG_METRICS").is_some() {
            eprintln!(
                "[mt-ui] 终端度量: family={} size={:.1} cell={:.3}x{:.3} 等宽={monospaced}",
                style.font_family,
                f(font_size),
                f(cell_width),
                f(line_height),
            );
        }
    });
}

/// 四种字形变体的 `FontId`,prepaint 开头解析一次。
///
/// `resolve_font` 每次都要克隆 Font 再进哈希表拿锁,放在逐 cell 的循环里
/// 是每帧上万次 —— 一屏的 cell 数量就是它的调用次数。
struct VariantFonts {
    ids: [FontId; 4],
}

impl VariantFonts {
    fn resolve(window: &Window, base: &gpui::Font) -> Self {
        let make = |bold: bool, italic: bool| {
            let mut font = base.clone();
            if bold {
                font.weight = gpui::FontWeight::BOLD;
            }
            if italic {
                font.style = gpui::FontStyle::Italic;
            }
            window.text_system().resolve_font(&font)
        };
        Self {
            ids: [
                make(false, false),
                make(true, false),
                make(false, true),
                make(true, true),
            ],
        }
    }

    fn id(&self, bold: bool, italic: bool) -> FontId {
        self.ids[usize::from(bold) + 2 * usize::from(italic)]
    }
}

fn paint_hollow_rect(window: &mut Window, bounds: Bounds<Pixels>, color: Hsla) {
    let t = px(1.0);
    let Bounds { origin, size: s } = bounds;
    window.paint_quad(fill(Bounds::new(origin, size(s.width, t)), color));
    window.paint_quad(fill(
        Bounds::new(point(origin.x, origin.y + s.height - t), size(s.width, t)),
        color,
    ));
    window.paint_quad(fill(Bounds::new(origin, size(t, s.height)), color));
    window.paint_quad(fill(
        Bounds::new(point(origin.x + s.width - t, origin.y), size(t, s.height)),
        color,
    ));
}
