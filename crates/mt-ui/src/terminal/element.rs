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
//! # 一帧怎么走(damage 追踪)
//!
//! ```text
//! ┌ 持 grid 锁 ────────────────────────────────────────────┐
//! │ 逐行:解析 cell → 行签名 → 查 RowCache                   │
//! │   命中 → 直接放置(零 shaping)                          │
//! │   未命中 → 攒成 RowPending(只有这些行要 shape)          │
//! └────────────────────────────────────────────────────────┘
//!   放锁
//! ┌ 无锁 ──────────────────────────────────────────────────┐
//! │ RowPending → shape_line → RowRender → 回填 RowCache      │
//! └────────────────────────────────────────────────────────┘
//! ```
//!
//! 缓存的键是**行内容签名**而不是行号,几何全部按行内相对坐标存 —— 于是滚屏时
//! 「只是换了个 y」的行照样命中。细节与量化数据见 [`super::damage`]。
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
use super::damage::{CellSignature, DamageStats, FrameKey, MAX_ZEROWIDTH_CHARS, RowCache, row_signature};
use super::mouse::{
    GridPos, MouseAction, MouseBtn, MouseMods, WheelDir, alt_screen_scroll_bytes,
    mouse_report_bytes, mouse_reporting_active, prefers_local_handling,
};
use super::theme::{TerminalStyle, TerminalTheme};

/// 一屏最多认多少列/行。窗口被拖到荒谬尺寸时防止 grid 爆掉。
const MAX_COLUMNS: usize = 1024;
const MAX_LINES: usize = 512;

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
/// [`super::TerminalView`] 就是干这件事的现成宿主 —— 除非有特殊需求,
/// 直接用它,不要自己接这个回调。
pub type InstallInputHandler = Rc<dyn Fn(Bounds<Pixels>, &mut Window, &mut App)>;
/// 元素要往 PTY 写字节时的出口(alt screen 下的滚轮、鼠标上报)。
/// 元素不持有 PTY,这条只能交回宿主。
pub type OnInput = Rc<dyn Fn(&[u8], &mut Window, &mut App)>;

/// 要浮在光标处显示的预编辑串(IME 组合中)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreeditText {
    pub text: SharedString,
    /// 光标在串内的**字节**偏移(UTF-16 → 字节的换算在 [`super::ime`] 里做完)。
    pub cursor_byte: usize,
}

/// 最近一帧的几何信息,由元素在 prepaint 里回填给宿主。
///
/// IME 的候选框定位(`bounds_for_range`)要的就是「光标那个格子在屏幕上的矩形」,
/// 而那是渲染阶段才算得出来的 —— 视图侧只能从这里读。
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameGeometry {
    pub origin: Point<Pixels>,
    pub cell_size: Size<Pixels>,
    pub columns: usize,
    pub screen_lines: usize,
    /// 光标格的屏幕矩形。光标隐藏时为 `None`。
    pub cursor: Option<Bounds<Pixels>>,
    /// 预编辑串内插入符的屏幕矩形。候选框要贴着它,不是贴着终端光标 ——
    /// 组合到第三个字时候选框还停在第一个字下面会挡住正在输入的内容。
    pub preedit_caret: Option<Bounds<Pixels>>,
}

pub struct TerminalElement {
    id: ElementId,
    emulator: Arc<TerminalEmulator>,
    style: TerminalStyle,
    theme: TerminalTheme,
    focus: FocusHandle,
    on_grid_resize: Option<OnGridResize>,
    install_input_handler: Option<InstallInputHandler>,
    on_input: Option<OnInput>,
    preedit: Option<PreeditText>,
    geometry_sink: Option<Rc<StdCell<FrameGeometry>>>,
    damage_sink: Option<Rc<StdCell<DamageStats>>>,
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
            preedit: None,
            geometry_sink: None,
            damage_sink: None,
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

    /// IME 预编辑串。浮在光标处、带下划线,**不进 grid**。
    pub fn preedit(mut self, preedit: Option<PreeditText>) -> Self {
        self.preedit = preedit;
        self
    }

    /// 每帧回填几何信息的出口(IME 候选框定位)。见 [`FrameGeometry`]。
    pub fn geometry_sink(mut self, sink: Rc<StdCell<FrameGeometry>>) -> Self {
        self.geometry_sink = Some(sink);
        self
    }

    /// 每帧回填 damage 统计(诊断 / 测试用)。
    pub fn damage_sink(mut self, sink: Rc<StdCell<DamageStats>>) -> Self {
        self.damage_sink = Some(sink);
        self
    }
}

/// 跨帧保留的交互状态与缓存。元素每帧重建,这些必须挂在 `GlobalElementId` 上。
#[derive(Clone)]
struct TerminalElementState {
    /// 滚轮的像素余量。触控板给的是零点几行,不攒起来会一直滚不动。
    scroll_remainder: Rc<StdCell<f32>>,
    /// 左键是否正在**本地**拖选。
    selecting: Rc<StdCell<bool>>,
    /// 正在被上报的按键(按下时上报过,松开要配对上报)。
    reported_button: Rc<StdCell<Option<MouseBtn>>>,
    /// 上一次上报过的格子。移动上报只在**跨格**时发,否则一个像素一条消息。
    last_reported_cell: Rc<StdCell<Option<(usize, usize)>>>,
    /// 行渲染缓存,见 [`super::damage`]。
    rows: Rc<RefCell<RowCache<Rc<RowRender>>>>,
}

impl Default for TerminalElementState {
    fn default() -> Self {
        Self {
            scroll_remainder: Rc::new(StdCell::new(0.0)),
            selecting: Rc::new(StdCell::new(false)),
            reported_button: Rc::new(StdCell::new(None)),
            last_reported_cell: Rc::new(StdCell::new(None)),
            rows: Rc::new(RefCell::new(RowCache::new())),
        }
    }
}

/// 一个待绘制的文本片段:已 shape 好的一行(或一格)。
///
/// `origin` 是**行内相对坐标**(x 相对行首,y 恒为 0)—— 这是缓存能跨行复用的前提。
#[derive(Clone)]
struct TextPiece {
    origin: Point<Pixels>,
    line: ShapedLine,
}

#[derive(Clone)]
struct CursorLayout {
    /// 行内相对坐标。
    bounds: Bounds<Pixels>,
    shape: CursorShape,
    color: Hsla,
}

/// 一行的完整渲染产物,几何全部相对该行左上角。
struct RowRender {
    backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    selections: Vec<Bounds<Pixels>>,
    texts: Vec<TextPiece>,
    cursor: Option<CursorLayout>,
}

/// 预编辑浮层的布局结果。
struct PreeditLayout {
    origin: Point<Pixels>,
    line: ShapedLine,
    /// 组合串内插入符的 x(相对 `origin`)。
    caret_x: Pixels,
    width: Pixels,
}

pub struct PreparedFrame {
    hitbox: Hitbox,
    state: TerminalElementState,
    cell_size: Size<Pixels>,
    origin: Point<Pixels>,
    columns: usize,
    screen_lines: usize,
    mode: TermMode,
    /// 本帧要画的行:`(行首 y,渲染产物)`。y 相对元素原点。
    rows: Vec<(Pixels, Rc<RowRender>)>,
    /// 光标(已换算到元素相对坐标)。
    cursor: Option<CursorLayout>,
    preedit: Option<PreeditLayout>,
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

/// 光标形状 → 签名里的判别码(0 留给「不是光标格」)。
fn cursor_code(shape: CursorShape) -> u8 {
    match shape {
        CursorShape::Block => 1,
        CursorShape::Underline => 2,
        CursorShape::Beam => 3,
        CursorShape::HollowBlock => 4,
        CursorShape::Hidden => 5,
    }
}

fn cursor_shape(code: u8) -> Option<CursorShape> {
    Some(match code {
        1 => CursorShape::Block,
        2 => CursorShape::Underline,
        3 => CursorShape::Beam,
        4 => CursorShape::HollowBlock,
        5 => CursorShape::Hidden,
        _ => return None,
    })
}

impl TerminalElement {
    /// 像素坐标 → 可视区行列。行号是**屏幕行**(0 = 最上面那行),与 display_offset 无关。
    fn hit_cell(
        pos: Point<Pixels>,
        origin: Point<Pixels>,
        cell_size: Size<Pixels>,
        columns: usize,
        screen_lines: usize,
    ) -> (usize, usize, Side) {
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
        (col, row, side)
    }

    /// 屏幕行列 → alacritty 的 grid 坐标(选择区要用)。
    fn grid_point(col: usize, row: usize, display_offset: usize) -> AlacPoint {
        AlacPoint::new(Line(row as i32 - display_offset as i32), Column(col))
    }

    fn paint_mouse_listeners(&self, prepared: &PreparedFrame, window: &mut Window, _cx: &mut App) {
        let hitbox = prepared.hitbox.clone();
        let origin = prepared.origin;
        let cell_size = prepared.cell_size;
        let columns = prepared.columns;
        let screen_lines = prepared.screen_lines;
        let state = prepared.state.clone();
        let mode = prepared.mode;
        let alt_screen = mode.contains(TermMode::ALT_SCREEN);

        // ── 滚轮
        //
        //  优先级:鼠标上报 > alt screen 方向键 > 本地回看。
        //  上报优先是因为开了上报的 TUI(htop / lazygit / fzf)自己有滚动语义,
        //  我们代劳只会让它收到一堆无意义的方向键。
        {
            let emulator = self.emulator.clone();
            let hitbox = hitbox.clone();
            let remainder = state.scroll_remainder.clone();
            let on_input = self.on_input.clone();
            let app_cursor = mode.contains(TermMode::APP_CURSOR);
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
                let mods = modifiers_of(&event.modifiers);

                if !prefers_local_handling(mode, mods) {
                    let Some(on_input) = on_input.as_ref() else {
                        return;
                    };
                    let (col, row, _) =
                        Self::hit_cell(event.position, origin, cell_size, columns, screen_lines);
                    let dir = if whole > 0.0 {
                        WheelDir::Up
                    } else {
                        WheelDir::Down
                    };
                    let mut payload = Vec::new();
                    for _ in 0..whole.abs() as usize {
                        if let Some(bytes) = mouse_report_bytes(
                            mode,
                            MouseAction::Wheel(dir),
                            mods,
                            GridPos::new(col, row),
                        ) {
                            payload.extend_from_slice(&bytes);
                        }
                    }
                    if !payload.is_empty() {
                        on_input(&payload, window, cx);
                    }
                    return;
                }

                if alt_screen {
                    // alt screen(vim / less 这类全屏程序)没有回看缓冲,
                    // 改成等价地敲方向键 —— 这也是 xterm 一贯的做法。
                    let Some(on_input) = on_input.as_ref() else {
                        return;
                    };
                    let payload = alt_screen_scroll_bytes(whole as i32, app_cursor);
                    on_input(&payload, window, cx);
                    return;
                }

                emulator.with_term_mut(|term| term.scroll_display(Scroll::Delta(whole as i32)));
                window.refresh();
            });
        }

        // ── 按下:上报,或开本地选择
        {
            let emulator = self.emulator.clone();
            let hitbox = hitbox.clone();
            let selecting = state.selecting.clone();
            let reported = state.reported_button.clone();
            let last_cell = state.last_reported_cell.clone();
            let on_input = self.on_input.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                let mods = modifiers_of(&event.modifiers);
                let (col, row, side) =
                    Self::hit_cell(event.position, origin, cell_size, columns, screen_lines);

                if !prefers_local_handling(mode, mods) {
                    let Some(btn) = map_button(event.button) else {
                        return;
                    };
                    if let Some(on_input) = on_input.as_ref()
                        && let Some(bytes) = mouse_report_bytes(
                            mode,
                            MouseAction::Press(btn),
                            mods,
                            GridPos::new(col, row),
                        )
                    {
                        on_input(&bytes, window, cx);
                    }
                    reported.set(Some(btn));
                    last_cell.set(Some((col, row)));
                    // 程序接管鼠标了,残留的本地高亮只会让人误以为还能复制
                    emulator.with_term_mut(|term| term.selection = None);
                    selecting.set(false);
                    window.refresh();
                    return;
                }

                // 本地:左键开选(双击 = 语义选词,三击 = 选整行)
                if event.button != MouseButton::Left {
                    return;
                }
                let display_offset = emulator.with_term(|t| t.grid().display_offset());
                let ty = match event.click_count {
                    1 => SelectionType::Simple,
                    2 => SelectionType::Semantic,
                    _ => SelectionType::Lines,
                };
                emulator.with_term_mut(|term| {
                    term.selection = Some(Selection::new(
                        ty,
                        Self::grid_point(col, row, display_offset),
                        side,
                    ));
                });
                selecting.set(event.click_count == 1);
                window.refresh();
            });
        }

        // ── 移动:上报拖动 / 上报纯移动 / 延伸本地选择
        {
            let emulator = self.emulator.clone();
            let hitbox = hitbox.clone();
            let selecting = state.selecting.clone();
            let reported = state.reported_button.clone();
            let last_cell = state.last_reported_cell.clone();
            let on_input = self.on_input.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let mods = modifiers_of(&event.modifiers);
                let held = reported.get();

                if held.is_some() || (mouse_reporting_active(mode) && !selecting.get()) {
                    // 纯移动上报(1003)要求指针真的在元素上;拖动则允许拖出去
                    if held.is_none() && !hitbox.is_hovered(window) {
                        return;
                    }
                    let (col, row, _) =
                        Self::hit_cell(event.position, origin, cell_size, columns, screen_lines);
                    // 跨格才发。不然一个像素一条消息,TUI 那头光解析就跑满一个核。
                    if last_cell.get() == Some((col, row)) {
                        return;
                    }
                    if let Some(on_input) = on_input.as_ref()
                        && let Some(bytes) = mouse_report_bytes(
                            mode,
                            MouseAction::Motion(held),
                            mods,
                            GridPos::new(col, row),
                        )
                    {
                        last_cell.set(Some((col, row)));
                        on_input(&bytes, window, cx);
                        return;
                    }
                    if held.is_some() {
                        // 拖动中但这次不该报(模式只有 1000):记住格子,别漏掉后面的松开配对
                        last_cell.set(Some((col, row)));
                        return;
                    }
                }

                if !selecting.get() || event.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                // 拖出元素外也要继续选,所以这里不判 hover
                let display_offset = emulator.with_term(|t| t.grid().display_offset());
                let (col, row, side) =
                    Self::hit_cell(event.position, origin, cell_size, columns, screen_lines);
                emulator.with_term_mut(|term| {
                    if let Some(sel) = term.selection.as_mut() {
                        sel.update(Self::grid_point(col, row, display_offset), side);
                    }
                });
                window.refresh();
            });
        }

        // ── 松开:配对上报,或结束拖选并把选中文本送进剪贴板
        //    (X11 primary selection 的习惯;Ctrl+Shift+C 由宿主再走一遍也无妨)
        {
            let emulator = self.emulator.clone();
            let selecting = state.selecting.clone();
            let reported = state.reported_button.clone();
            let last_cell = state.last_reported_cell.clone();
            let on_input = self.on_input.clone();
            window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                // 按下时上报过的键,松开必须配对上报 —— 否则 TUI 会一直以为鼠标还按着。
                // **不看当前 mode**:程序可能在按住期间关掉了上报模式,那也得把这一次收尾。
                if let Some(btn) = reported.get()
                    && map_button(event.button) == Some(btn)
                {
                    reported.set(None);
                    last_cell.set(None);
                    let (col, row, _) =
                        Self::hit_cell(event.position, origin, cell_size, columns, screen_lines);
                    // shift 在这里必须抹掉:按住期间**中途按下 Shift** 会让
                    // `prefers_local_handling` 把松开事件吞掉,TUI 从此认为
                    // 鼠标一直按着(拖动框永远不结束)
                    let release_mods = MouseMods {
                        shift: false,
                        ..modifiers_of(&event.modifiers)
                    };
                    if let Some(on_input) = on_input.as_ref()
                        && let Some(bytes) = mouse_report_bytes(
                            mode,
                            MouseAction::Release(btn),
                            release_mods,
                            GridPos::new(col, row),
                        )
                    {
                        on_input(&bytes, window, cx);
                    }
                    return;
                }

                if event.button != MouseButton::Left || !selecting.replace(false) {
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

/// gpui 的按键 → 协议按键。没有对应编码的一律丢弃。
fn map_button(button: MouseButton) -> Option<MouseBtn> {
    match button {
        MouseButton::Left => Some(MouseBtn::Left),
        MouseButton::Middle => Some(MouseBtn::Middle),
        MouseButton::Right => Some(MouseBtn::Right),
        // 侧键:gpui 给的是 0/1(后退/前进),协议里是 8/9
        MouseButton::Navigate(gpui::NavigationDirection::Back) => Some(MouseBtn::Other(8)),
        MouseButton::Navigate(gpui::NavigationDirection::Forward) => Some(MouseBtn::Other(9)),
    }
}

fn modifiers_of(m: &gpui::Modifiers) -> MouseMods {
    MouseMods::new(m.shift, m.alt, m.control)
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

        // ── 帧指纹:这些变了,每一行的画面都会变而行签名不动 → 整表作废
        let frame_key = FrameKey::builder()
            .push_f32(f(cell_width))
            .push_f32(f(line_height))
            .push_f32(f(font_size))
            .push(columns)
            .push(focused)
            .push(self.style.font_family.as_ref())
            .push(
                self.style
                    .font_fallbacks
                    .iter()
                    .map(|s| s.as_ref())
                    .collect::<Vec<_>>(),
            )
            .push_hsla(self.theme.selection)
            .push_hsla(self.theme.cursor)
            .push_hsla(self.theme.cursor_text)
            .finish();
        state.rows.borrow_mut().begin_frame(frame_key);

        let mut placed: Vec<(usize, Rc<RowRender>)> = Vec::with_capacity(screen_lines);
        let mut pending: Vec<RowPending> = Vec::new();
        let mode;
        let mut rows_seen = 0usize;

        {
            let term_lock = self.emulator.term().lock();
            let content = term_lock.renderable_content();
            let display_offset = content.display_offset;
            mode = content.mode;
            let colors_table = content.colors;
            let selection_range = content.selection;
            let cursor_point = content.cursor.point;
            let cursor_shape = content.cursor.shape;

            let mut cache = state.rows.borrow_mut();
            let mut scratch: Vec<CellSignature> = Vec::with_capacity(columns);
            let mut current_row: Option<usize> = None;

            let flush_row = |row: usize,
                                 scratch: &mut Vec<CellSignature>,
                                 cache: &mut RowCache<Rc<RowRender>>,
                                 placed: &mut Vec<(usize, Rc<RowRender>)>,
                                 pending: &mut Vec<RowPending>| {
                if scratch.is_empty() {
                    return;
                }
                let sig = row_signature(scratch);
                match cache.get(sig) {
                    Some(render) => placed.push((row, render)),
                    None => pending.push(RowPending {
                        row,
                        sig,
                        cells: std::mem::take(scratch),
                    }),
                }
                scratch.clear();
            };

            for indexed in content.display_iter {
                let row = (indexed.point.line.0 + display_offset as i32).max(0) as usize;
                if current_row != Some(row) {
                    if let Some(prev) = current_row {
                        flush_row(prev, &mut scratch, &mut cache, &mut placed, &mut pending);
                        rows_seen += 1;
                    }
                    current_row = Some(row);
                    scratch.reserve(columns);
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

                let selected = selection_range
                    .map(|r| r.contains(indexed.point))
                    .unwrap_or(false);
                let is_cursor = indexed.point == cursor_point && cursor_shape != CursorShape::Hidden;
                if is_cursor && focused && cursor_shape == CursorShape::Block {
                    // 块状光标底下的字反白
                    fg = self.theme.cursor_text;
                }

                let mut zerowidth = ['\0'; MAX_ZEROWIDTH_CHARS];
                if let Some(zw) = cell.zerowidth() {
                    for (slot, ch) in zerowidth.iter_mut().zip(zw.iter().copied()) {
                        *slot = ch;
                    }
                }

                scratch.push(CellSignature {
                    col,
                    ch: cell.c,
                    zerowidth,
                    fg,
                    bg,
                    bg_default: bg_is_default,
                    flags,
                    selected,
                    cursor: if is_cursor {
                        cursor_code(cursor_shape)
                    } else {
                        0
                    },
                });
            }
            if let Some(row) = current_row {
                flush_row(row, &mut scratch, &mut cache, &mut placed, &mut pending);
                rows_seen += 1;
            }
        }

        // ── shape 只发生在「内容真的变了」的行上。
        //    锁已经放掉了 —— shaping 会往 DirectWrite 里跑,别拿着 grid 锁做。
        for row in pending {
            let render = Rc::new(build_row(
                window,
                &row.cells,
                &font,
                font_size,
                &variant_fonts,
                cell_width,
                line_height,
                &self.theme,
            ));
            state.rows.borrow_mut().insert(row.sig, render.clone());
            placed.push((row.row, render));
        }

        {
            let mut cache = state.rows.borrow_mut();
            cache.end_frame(rows_seen);
            if let Some(sink) = self.damage_sink.as_ref() {
                sink.set(cache.stats());
            }
        }

        // ── 摆到元素坐标系
        let mut cursor: Option<CursorLayout> = None;
        let rows: Vec<(Pixels, Rc<RowRender>)> = placed
            .into_iter()
            .map(|(row, render)| {
                let y = line_height * row as f32;
                if let Some(c) = render.cursor.as_ref() {
                    cursor = Some(CursorLayout {
                        bounds: translate(c.bounds, point(bounds.origin.x, bounds.origin.y + y)),
                        shape: c.shape,
                        color: c.color,
                    });
                }
                (y, render)
            })
            .collect();

        // ── IME 预编辑浮层
        let preedit = self.preedit.as_ref().and_then(|p| {
            if p.text.is_empty() {
                return None;
            }
            let anchor = cursor
                .as_ref()
                .map(|c| c.bounds.origin)
                .unwrap_or(bounds.origin);
            let run = TextRun {
                len: p.text.len(),
                font: font.clone(),
                color: self.theme.foreground,
                background_color: None,
                // 组合中的下划线是 IME 的通用视觉约定,少了它用户分不清
                // 「已经上屏」和「还在候选」
                underline: Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(self.theme.foreground),
                    wavy: false,
                }),
                strikethrough: None,
            };
            let line = window
                .text_system()
                .shape_line(p.text.clone(), font_size, &[run], None);
            let caret_x = line.x_for_index(p.cursor_byte.min(p.text.len()));
            let width = line.width;
            Some(PreeditLayout {
                origin: anchor,
                line,
                caret_x,
                width,
            })
        });

        if let Some(sink) = self.geometry_sink.as_ref() {
            sink.set(FrameGeometry {
                origin: bounds.origin,
                cell_size,
                columns,
                screen_lines,
                cursor: cursor.as_ref().map(|c| c.bounds),
                preedit_caret: preedit.as_ref().map(|p| {
                    Bounds::new(
                        point(p.origin.x + p.caret_x, p.origin.y),
                        size(px(2.0), line_height),
                    )
                }),
            });
        }

        PreparedFrame {
            hitbox,
            state,
            cell_size,
            origin: bounds.origin,
            columns,
            screen_lines,
            mode,
            rows,
            cursor,
            preedit,
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

        let origin = bounds.origin;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (y, render) in prepared.rows.iter() {
                let delta = point(origin.x, origin.y + *y);
                for (rect, color) in render.backgrounds.iter() {
                    window.paint_quad(fill(translate(*rect, delta), *color));
                }
            }
            for (y, render) in prepared.rows.iter() {
                let delta = point(origin.x, origin.y + *y);
                for rect in render.selections.iter() {
                    window.paint_quad(fill(translate(*rect, delta), self.theme.selection));
                }
            }
            // 块状光标画在文字底下(文字用反白色);其余形状画在文字之上。
            if let Some(c) = prepared.cursor.as_ref()
                && focused
                && c.shape == CursorShape::Block
            {
                window.paint_quad(fill(c.bounds, c.color));
            }
            for (y, render) in prepared.rows.iter() {
                let delta = point(origin.x, origin.y + *y);
                for piece in render.texts.iter() {
                    _ = piece.line.paint(
                        piece.origin + delta,
                        prepared.cell_size.height,
                        window,
                        cx,
                    );
                }
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

            // ── IME 预编辑浮层:盖住底下的 grid 内容再画,否则组合串会与残留字符叠糊
            if let Some(p) = prepared.preedit.as_ref() {
                let height = prepared.cell_size.height;
                window.paint_quad(fill(
                    Bounds::new(p.origin, size(p.width, height)),
                    self.theme.background,
                ));
                _ = p.line.paint(p.origin, height, window, cx);
                // 组合串内的插入符:细竖线,颜色跟光标走
                window.paint_quad(fill(
                    Bounds::new(
                        point(p.origin.x + p.caret_x, p.origin.y),
                        size(px(2.0), height),
                    ),
                    self.theme.cursor,
                ));
            }
        });

        self.paint_mouse_listeners(prepared, window, cx);
    }
}

/// 一行「内容变了、需要重建」的暂存。
struct RowPending {
    row: usize,
    sig: u64,
    cells: Vec<CellSignature>,
}

/// 未 shape 的合并运行段。
struct PendingRun {
    start: usize,
    len: usize,
    text: String,
    style: RunStyle,
}

/// 未 shape 的绘制片段(合并段落地后、或单格的宽字符)。
struct PendingPiece {
    start: usize,
    text: String,
    style: RunStyle,
}

/// 把一行解析好的格子变成可绘制产物。**几何全部相对行首**。
#[allow(clippy::too_many_arguments)]
fn build_row(
    window: &Window,
    cells: &[CellSignature],
    font: &gpui::Font,
    font_size: Pixels,
    variant_fonts: &VariantFonts,
    cell_width: Pixels,
    line_height: Pixels,
    theme: &TerminalTheme,
) -> RowRender {
    let cell_size = size(cell_width, line_height);
    let mut backgrounds: Vec<(Bounds<Pixels>, Hsla)> = Vec::new();
    let mut selections: Vec<Bounds<Pixels>> = Vec::new();
    let mut pieces: Vec<PendingPiece> = Vec::new();
    let mut cursor: Option<CursorLayout> = None;

    let mut bg_run: Option<(usize, usize, Hsla)> = None;
    let mut sel_run: Option<(usize, usize)> = None;
    let mut text_run: Option<PendingRun> = None;

    for cell in cells {
        let col = cell.col;

        // ── 背景:默认背景不发 quad(背景图从这里透出来)
        if cell.bg_default {
            flush_bg(&mut bg_run, cell_size, &mut backgrounds);
        } else {
            match bg_run.as_mut() {
                Some((_, end, color)) if *color == cell.bg && *end + 1 == col => *end = col,
                _ => {
                    flush_bg(&mut bg_run, cell_size, &mut backgrounds);
                    bg_run = Some((col, col, cell.bg));
                }
            }
        }

        // ── 选择区
        if cell.selected {
            match sel_run.as_mut() {
                Some((_, end)) if *end + 1 == col => *end = col,
                _ => {
                    flush_sel(&mut sel_run, cell_size, &mut selections);
                    sel_run = Some((col, col));
                }
            }
        } else {
            flush_sel(&mut sel_run, cell_size, &mut selections);
        }

        // ── 光标
        if let Some(shape) = cursor_shape(cell.cursor) {
            let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                cell_width * 2.0
            } else {
                cell_width
            };
            cursor = Some(CursorLayout {
                bounds: Bounds::new(
                    point(cell_width * col as f32, px(0.0)),
                    size(width, line_height),
                ),
                shape,
                color: theme.cursor,
            });
        }

        // ── 文本
        //    WIDE_CHAR 的第二列(spacer)没有字形,跳过;它的背景已经由
        //    上面那段处理过了。
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            flush_text(&mut text_run, &mut pieces);
            continue;
        }

        let style_key = RunStyle {
            fg: cell.fg,
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: underline_style(cell.flags, cell.fg),
            strikethrough: cell
                .flags
                .contains(Flags::STRIKEOUT)
                .then(|| StrikethroughStyle {
                    thickness: px(1.0),
                    color: Some(cell.fg),
                }),
        };

        let has_zerowidth = cell.zerowidth[0] != '\0';
        let run_font_id = variant_fonts.id(style_key.bold, style_key.italic);
        // 可合并的条件:窄字符、无组合符号、不是光标格(光标格颜色单独)、
        // 且主字体里这个字形的步进恰好一列宽。
        let mergeable = !cell.flags.contains(Flags::WIDE_CHAR)
            && !has_zerowidth
            && cell.cursor == 0
            && advance_fits_cell(window, run_font_id, font_size, cell.ch, cell_width);

        if mergeable {
            match text_run.as_mut() {
                Some(run) if run.style.same(&style_key) && run.start + run.len == col => {
                    run.text.push(cell.ch);
                    run.len += 1;
                }
                _ => {
                    flush_text(&mut text_run, &mut pieces);
                    let mut text = String::new();
                    text.push(cell.ch);
                    text_run = Some(PendingRun {
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
            text.push(cell.ch);
            for z in cell.zerowidth.iter().take_while(|c| **c != '\0') {
                text.push(*z);
            }
            pieces.push(PendingPiece {
                start: col,
                text,
                style: style_key,
            });
        }
    }
    flush_bg(&mut bg_run, cell_size, &mut backgrounds);
    flush_sel(&mut sel_run, cell_size, &mut selections);
    flush_text(&mut text_run, &mut pieces);

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
        let shaped =
            window
                .text_system()
                .shape_line(SharedString::from(piece.text), font_size, &[run], None);
        texts.push(TextPiece {
            origin: point(cell_width * piece.start as f32, px(0.0)),
            line: shaped,
        });
    }

    RowRender {
        backgrounds,
        selections,
        texts,
        cursor,
    }
}

fn translate(bounds: Bounds<Pixels>, delta: Point<Pixels>) -> Bounds<Pixels> {
    Bounds::new(bounds.origin + delta, bounds.size)
}

fn flush_text(run: &mut Option<PendingRun>, out: &mut Vec<PendingPiece>) {
    if let Some(r) = run.take() {
        out.push(PendingPiece {
            start: r.start,
            text: r.text,
            style: r.style,
        });
    }
}

fn flush_bg(
    run: &mut Option<(usize, usize, Hsla)>,
    cell: Size<Pixels>,
    out: &mut Vec<(Bounds<Pixels>, Hsla)>,
) {
    let Some((start, end, color)) = run.take() else {
        return;
    };
    out.push((rect_for(start, end, cell), color));
}

fn flush_sel(run: &mut Option<(usize, usize)>, cell: Size<Pixels>, out: &mut Vec<Bounds<Pixels>>) {
    let Some((start, end)) = run.take() else {
        return;
    };
    out.push(rect_for(start, end, cell));
}

/// 行内相对矩形(y 恒为 0)。
fn rect_for(start: usize, end: usize, cell: Size<Pixels>) -> Bounds<Pixels> {
    Bounds::new(
        point(cell.width * start as f32, px(0.0)),
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
