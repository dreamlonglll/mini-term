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
//! # 查找高亮怎么与行缓存共存
//!
//! 接了 [`TerminalElement::search`] 之后,prepaint 会在**拿 grid 锁之前**替
//! [`TerminalSearch`] 跑一次(带去抖的)重搜,拿到按行拍平的命中索引;
//! 逐 cell 解析时把「这一格是普通命中 / 当前命中 / 没命中」写进
//! [`CellSignature::search`]。
//!
//! 于是命中集合变化只会让**真正带高亮的那几行**签名变、被重建,其余行照旧命中
//! 缓存;当前命中在两条之间移动时同理只重建两行。高亮配色则进
//! [`FrameKey`](super::damage::FrameKey) —— 它变了每一行的画面都会变而签名不动。
//! 两条路各管一段,既不会把整屏缓存打穿,也不会留下高亮残影。
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
use std::time::{Duration, Instant};

use alacritty_terminal::grid::{Dimensions as _, Scroll};
use alacritty_terminal::index::{Column, Line, Point as AlacPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::CursorShape;
use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Corners, DispatchPhase, Element, ElementId, FocusHandle,
    FontId, GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    ScrollDelta, ScrollWheelEvent, ShapedLine, SharedString, Size, Style, StrikethroughStyle,
    TextRun, UnderlineStyle, Window, fill, point, px, size,
};
use mt_terminal::{TermSize, TerminalEmulator};

use super::colors;
use super::damage::{CellSignature, DamageStats, FrameKey, MAX_ZEROWIDTH_CHARS, RowCache, row_signature};
use super::input::{Arrow, arrow_bytes};
use super::mouse::{
    GridPos, MouseAction, MouseBtn, MouseMods, WheelDir, alt_screen_scroll_bytes,
    mouse_report_bytes, mouse_reporting_active, prefers_local_handling,
};
use super::scrollbar::{self, ScrollbarDrag, ScrollbarHit, ScrollbarLayout, ScrollbarStyle};
use super::search::{HighlightSpan, SearchHighlights, TerminalSearch};
use super::selection_dwell::{DwellConfig, DwellTracker, OnSelectionCopied, ReleaseAction};
use super::theme::{SearchColors, TerminalStyle, TerminalTheme};
use crate::background::BackgroundArtElement;
use crate::theme_bridge::BackgroundArt;

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
    /// 光标格的屏幕矩形。**与光标可见性无关** —— TUI 藏起光标(`ESC[?25l`)
    /// 自己画插入符时,这里照样给出那一格,否则 IME 候选框会退到元素左上角。
    /// `None` 只有一个含义:还没画过任何一帧。
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
    scrollbar: ScrollbarStyle,
    dwell: DwellConfig,
    on_selection_copied: Option<OnSelectionCopied>,
    background_art: Option<BackgroundArtElement>,
    search: Option<Rc<RefCell<TerminalSearch>>>,
    search_colors: SearchColors,
    flash: Option<FlashLine>,
}

/// 一次性的「整行闪一下」。跳到某一行之后给的可见反馈,到期由**宿主**撤掉
/// (元素不持有计时器 —— 它每帧重建,存不下跨帧状态)。
///
/// `line` 是 **grid 绝对行号**,与 [`super::search::SearchMatch`] 的 `Point::line`
/// 同一坐标系(0 = 屏幕第一行,负数 = 回看缓冲),所以它跟着 display_offset 走:
/// 用户滚开之后那一行自然滚出视口、这一帧就不画了。
///
/// **不进行签名**:它是 paint 阶段独立发的一块 quad,不参与 `CellSignature`
/// (查找高亮那条路必须进签名,因为它改的是**格子**的画法;这里改的是叠在
/// 行上的一层浮标,每帧照当前状态重发,撤掉时自然消失)。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FlashLine {
    pub line: i32,
    pub color: Hsla,
}

/// 闪烁行在**本帧**落在屏幕第几行。`None` = 不在视口里(这一帧不画)。
///
/// `row = line + display_offset`,与 prepaint 里那句
/// `indexed.point.line.0 + display_offset` 同一换算。
fn flash_row(line: i32, display_offset: usize, screen_lines: usize) -> Option<usize> {
    let row = line + display_offset as i32;
    (row >= 0 && (row as usize) < screen_lines).then_some(row as usize)
}

/// 光标格在元素坐标系里的矩形。**与「画不画光标」无关**。
///
/// 两件事必须分开:「光标可见吗」是 `CursorShape::Hidden` 说了算,
/// 「光标在哪一格」是 grid 坐标说了算。绑在一起会出这个 bug —— Ink 系的 TUI
/// (Claude Code 就是)开局发 `ESC[?25l` 藏掉终端光标、自己画一个反色块当插入符,
/// 于是这一帧没有任何 cell 带光标标记,IME 的候选框和预编辑串双双退回元素左上角,
/// 中文输入的候选窗糊在终端顶行上。滚出视口同理(往回翻历史时光标在视口下方)。
///
/// 所以这里只做换算不做判空:行号与列号一律**钳到视口内最近的边缘**,
/// 保证任何时候都有一个可用的锚点。行换算与 [`flash_row`] 同一口径
/// (`row = line + display_offset`),列直接是 grid 列。
fn cursor_cell_bounds(
    origin: Point<Pixels>,
    cell_size: Size<Pixels>,
    columns: usize,
    screen_lines: usize,
    cursor_line: i32,
    cursor_column: usize,
    display_offset: usize,
) -> Bounds<Pixels> {
    let row = (cursor_line + display_offset as i32).clamp(0, screen_lines.saturating_sub(1) as i32);
    let col = cursor_column.min(columns.saturating_sub(1));
    Bounds::new(
        point(
            origin.x + cell_size.width * col as f32,
            origin.y + cell_size.height * row as f32,
        ),
        cell_size,
    )
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
            scrollbar: ScrollbarStyle::default(),
            // 默认 `Duration::ZERO` = 停留语义关闭,维持「松手即复制」的旧行为
            dwell: DwellConfig::default(),
            on_selection_copied: None,
            background_art: None,
            search: None,
            search_colors: SearchColors::default(),
            flash: None,
        }
    }

    /// 接上查找引擎:每帧同步命中集合并把命中格子画上底色。
    ///
    /// 引擎实例由宿主持有(`Rc<RefCell<_>>`),同一份还要交给
    /// [`super::TerminalSearchBar`] —— 计数与高亮共用一份状态,不必对账。
    pub fn search(mut self, search: Option<Rc<RefCell<TerminalSearch>>>) -> Self {
        self.search = search;
        self
    }

    /// 查找高亮的两档底色与当前命中描边。见 [`SearchColors`]。
    pub fn search_colors(mut self, colors: SearchColors) -> Self {
        self.search_colors = colors;
        self
    }

    /// 让某一行整行闪一下。见 [`FlashLine`]。
    pub fn flash(mut self, flash: Option<FlashLine>) -> Self {
        self.flash = flash;
        self
    }

    /// 滚动条外观与行为。见 [`ScrollbarStyle`];`enabled: false` 可整条关掉。
    pub fn scrollbar(mut self, style: ScrollbarStyle) -> Self {
        self.scrollbar = style;
        self
    }

    /// 拖选停留自动复制。见 [`DwellConfig`] —— **不设就是旧的「松手即复制」**。
    pub fn selection_dwell(mut self, dwell: DwellConfig) -> Self {
        self.dwell = dwell;
        self
    }

    /// 复制发生时通知宿主(弹「已复制」气泡)。见 [`OnSelectionCopied`]。
    pub fn on_selection_copied(
        mut self,
        f: impl Fn(&str, Point<Pixels>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_selection_copied = Some(Rc::new(f));
        self
    }

    /// 在 grid **底下**铺一层主题包背景图。
    ///
    /// 窗口级已经铺过就别再开这个(见 [`crate::background`] 的 overdraw 提醒)。
    pub fn background_art(mut self, art: Option<BackgroundArt>) -> Self {
        self.background_art = art.map(BackgroundArtElement::new);
        self
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
    /// 逐行解析出来的格子的暂存区(arena)。**跨帧复用**:每帧开头清空、容量留着 ——
    /// 未命中的行原地留在里面等 shaping,命中的行解析完就把尾巴截掉。
    ///
    /// 从前这里是「一根 scratch + `mem::take`」:每个未命中的行都要重新分配一次
    /// (200 列 × 72B ≈ 14KB),而刷屏时整屏都是未命中。容量按「一帧未命中行数 ×
    /// 列数」的高水位收敛,一屏满打满算几百 KB —— 与行缓存自己那份产物同一量级。
    cells: Rc<RefCell<Vec<CellSignature>>>,
    /// 「字符步进是否恰好一列宽」的缓存,见 [`AdvanceCache`]。
    advance: Rc<RefCell<AdvanceCache>>,
    /// 滚动条的拖动/悬停状态。
    scrollbar: Rc<StdCell<ScrollbarDrag>>,
    /// 上一次「滚动条该亮起来」的时刻(滚动、拖动、悬停)。淡出计时的起点。
    scrollbar_touched: Rc<StdCell<Option<Instant>>>,
    /// 上一帧的 display_offset。变了就说明滚过,滚动条要重新亮起。
    last_offset: Rc<StdCell<usize>>,
    /// 拖选停留复制的状态机。
    dwell: Rc<RefCell<DwellTracker>>,
    /// 在飞的 dwell 定时器。**必须留着句柄** —— gpui 的 `Task` 一 drop 就取消。
    dwell_task: Rc<RefCell<Option<gpui::Task<()>>>>,
}

impl Default for TerminalElementState {
    fn default() -> Self {
        Self {
            scroll_remainder: Rc::new(StdCell::new(0.0)),
            selecting: Rc::new(StdCell::new(false)),
            reported_button: Rc::new(StdCell::new(None)),
            last_reported_cell: Rc::new(StdCell::new(None)),
            rows: Rc::new(RefCell::new(RowCache::new())),
            cells: Rc::new(RefCell::new(Vec::new())),
            advance: Rc::new(RefCell::new(AdvanceCache::default())),
            scrollbar: Rc::new(StdCell::new(ScrollbarDrag::default())),
            scrollbar_touched: Rc::new(StdCell::new(None)),
            last_offset: Rc::new(StdCell::new(0)),
            dwell: Rc::new(RefCell::new(DwellTracker::default())),
            dwell_task: Rc::new(RefCell::new(None)),
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
    /// 查找命中段。`bool` = 是不是当前命中(要额外描一圈边)。
    searches: Vec<(Bounds<Pixels>, bool)>,
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
    /// 滚动条几何。`None` = 这一帧不画(无回看缓冲 / alt screen / 被关掉)。
    scrollbar: Option<ScrollbarLayout>,
    /// 整行闪烁的矩形与颜色(已换算到窗口坐标)。`None` = 没设 / 不在视口里。
    flash: Option<(Bounds<Pixels>, Hsla)>,
}

/// 「这个字符在这套字体里的步进正好是一列宽吗」的缓存。
///
/// 每帧对每个格子问一次,不缓存就是每帧几千次 DirectWrite 往返。挂在
/// [`TerminalElementState`] 上(跨帧存活),prepaint 开头借一次、整帧共用 ——
/// 从前它是个 thread_local,于是**每个格子**都要走一趟 TLS + RefCell + SipHash。
///
/// 分两层:
///
/// - **ASCII 直接下标**。一屏一万个格子里九成九是 ASCII,查一次表不该有哈希成本。
///   小表按「字形变体 × 码位」分层,`[Option<bool>; 128] × 4` 一共 512 字节;
///   四档 FontId / 字号 / 列宽任一变化就整体清掉(那时 [`FrameKey`] 也变,
///   行缓存本来就要全量重建)。
/// - **非 ASCII 走 HashMap**,键里带 font_id 与字号 —— 粗体 / 斜体是不同的
///   font_id,各自算各自的。列宽不进键:它本来就是 (font_id, 字号) 算出来的
///   ('M' 的 advance),同一个键对应的答案不会变。
struct AdvanceCache {
    /// ASCII 快表的有效性判据:四档变体的 FontId + 字号 + 列宽。
    key: Option<([FontId; 4], u32, u32)>,
    /// 下标 = [`VariantFonts::slot`] 的四档(正 / 粗 / 斜 / 粗斜)。
    ascii: [[Option<bool>; 128]; 4],
    wide: HashMap<(FontId, u32, char), bool>,
}

impl Default for AdvanceCache {
    fn default() -> Self {
        Self {
            key: None,
            ascii: [[None; 128]; 4],
            wide: HashMap::new(),
        }
    }
}

impl AdvanceCache {
    /// 每帧开头对一次表:字体度量换了就把 ASCII 快表清掉。
    fn begin_frame(&mut self, fonts: &VariantFonts, font_size: Pixels, cell_width: Pixels) {
        let key = (fonts.ids, f(font_size).to_bits(), f(cell_width).to_bits());
        if self.key != Some(key) {
            self.key = Some(key);
            self.ascii = [[None; 128]; 4];
        }
    }

    fn fits(
        &mut self,
        window: &Window,
        slot: usize,
        font_id: FontId,
        font_size: Pixels,
        ch: char,
        cell_width: Pixels,
    ) -> bool {
        let code = ch as usize;
        if code < 128 {
            if let Some(hit) = self.ascii[slot][code] {
                return hit;
            }
            let fits = self.measure(window, font_id, font_size, ch, cell_width);
            self.ascii[slot][code] = Some(fits);
            return fits;
        }
        self.measure(window, font_id, font_size, ch, cell_width)
    }

    fn measure(
        &mut self,
        window: &Window,
        font_id: FontId,
        font_size: Pixels,
        ch: char,
        cell_width: Pixels,
    ) -> bool {
        let key = (font_id, f(font_size).to_bits(), ch);
        if let Some(hit) = self.wide.get(&key).copied() {
            return hit;
        }
        let fits = window
            .text_system()
            .advance(font_id, font_size, ch)
            .map(|adv| (f(adv.width) - f(cell_width)).abs() < 0.01)
            .unwrap_or(false);
        self.wide.insert(key, fits);
        fits
    }
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

    /// 滚动条的两个 quad(轨道可选 + 滑块)。
    ///
    /// 淡出是「按时间算 alpha + 没淡完就再要一帧」,不走 `with_animation` ——
    /// 那玩意要求一个稳定的 ElementId 且会持续请求帧,而滚动条**大多数时候
    /// 应该是完全静止的**,空转帧对一个终端来说太贵。
    fn paint_scrollbar(&self, prepared: &PreparedFrame, window: &mut Window) {
        let Some(layout) = prepared.scrollbar else {
            return;
        };
        let drag = prepared.state.scrollbar.get();
        let idle = prepared
            .state
            .scrollbar_touched
            .get()
            .map(|t| t.elapsed())
            .unwrap_or(Duration::MAX);
        let active = drag.active();
        // 减弱动效下淡出补间归零(延迟不变),见 `ScrollbarStyle::gated`
        let style = self.scrollbar.gated();
        let alpha = scrollbar::alpha(&style, idle, active, layout.at_bottom());

        if alpha > 0.004 {
            if let Some(track) = self.scrollbar.track {
                window.paint_quad(
                    fill(
                        layout.track,
                        Hsla {
                            a: track.a * alpha,
                            ..track
                        },
                    )
                    .corner_radii(Corners::all(self.scrollbar.radius)),
                );
            }
            let base = if active {
                self.scrollbar.thumb_active.unwrap_or(Hsla {
                    a: self.scrollbar.active_alpha,
                    ..self.theme.foreground
                })
            } else {
                self.scrollbar.thumb.unwrap_or(Hsla {
                    a: self.scrollbar.idle_alpha,
                    ..self.theme.foreground
                })
            };
            window.paint_quad(
                fill(
                    layout.thumb,
                    Hsla {
                        a: base.a * alpha,
                        ..base
                    },
                )
                .corner_radii(Corners::all(self.scrollbar.radius)),
            );
        }

        // 淡出还没走完就再要一帧;走完了就此打住,不空转
        if scrollbar::needs_animation_frame(&style, idle, active) {
            window.request_animation_frame();
        }
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
        let bar = prepared.scrollbar;
        let element_size = (
            f32::from(prepared.hitbox.bounds.size.width),
            f32::from(prepared.hitbox.bounds.size.height),
        );
        // 6px 的条子精确命中太难点,左右各放宽 3px(与原生滚动条的手感一致)
        const GRAB_SLACK: Pixels = px(3.0);

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
            let touched = state.scrollbar_touched.clone();
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
                // 滚了就把滚动条亮起来(淡出重新计时)
                touched.set(Some(Instant::now()));
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
            let bar_drag = state.scrollbar.clone();
            let touched = state.scrollbar_touched.clone();
            let dwell_state = state.dwell.clone();
            let dwell_task = state.dwell_task.clone();
            let dwell_cfg = self.dwell;
            let this_emulator = self.emulator.clone();
            let on_copied = self.on_selection_copied.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                let mods = modifiers_of(&event.modifiers);
                let (col, row, side) =
                    Self::hit_cell(event.position, origin, cell_size, columns, screen_lines);

                // ── 滚动条先于一切:滑块上按下只能是拖滚动条,不能顺带起一个选区,
                //    也不能被鼠标上报吃掉(TUI 收到一个它没法解释的点击更糟)
                if let Some(layout) = bar
                    && event.button == MouseButton::Left
                {
                    match layout.hit(event.position, GRAB_SLACK) {
                        ScrollbarHit::Thumb => {
                            let grab =
                                f32::from(event.position.y) - f32::from(layout.thumb.origin.y);
                            bar_drag.set(ScrollbarDrag {
                                grab_offset: Some(grab),
                                hovered: true,
                            });
                            touched.set(Some(Instant::now()));
                            window.refresh();
                            return;
                        }
                        ScrollbarHit::Track => {
                            let target = layout.offset_for_track_click(event.position);
                            let delta = target as i32 - layout.display_offset as i32;
                            if delta != 0 {
                                this_emulator
                                    .with_term_mut(|t| t.scroll_display(Scroll::Delta(delta)));
                            }
                            bar_drag.set(ScrollbarDrag {
                                grab_offset: None,
                                hovered: true,
                            });
                            touched.set(Some(Instant::now()));
                            window.refresh();
                            return;
                        }
                        ScrollbarHit::Miss => {}
                    }
                }

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

                // ⌥+单击:把光标挪到点中的格子。判定与合成见 [`cursor_move_bytes`]。
                //
                // **修饰键不能省**:裸左键是拖选区的起手式,两者抢同一个手势。
                // Terminal.app 同样挂在 ⌥+click 上,这里照它。
                //
                // 只在**没滚动回看**时生效:滚上去之后视口行号与光标所在的 grid 行
                // 不是一回事,差值算出来会把光标带到莫名其妙的地方。
                if event.modifiers.alt
                    && event.click_count == 1
                    && display_offset == 0
                    && let Some(on_input) = on_input.as_ref()
                {
                    let (cur_line, cur_col) = emulator.with_term(|t| {
                        let p = t.grid().cursor.point;
                        (p.line.0, p.column.0 as i32)
                    });
                    let payload =
                        cursor_move_bytes(cur_line, cur_col, row as i32, col as i32, mode);
                    if !payload.is_empty() {
                        on_input(&payload, window, cx);
                    }
                    return;
                }

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

                // ── 拖选停留复制:按下就起表(停留语义关掉时 on_press 返回 None)
                // 存**元素相对**坐标:气泡落点要按元素宽度贴边收拢
                //(原版是 `lastX - rect.left`),存绝对坐标会让分屏右侧的终端算歪
                let generation = dwell_state.borrow_mut().on_press(
                    &dwell_cfg,
                    (
                        f32::from(event.position.x - origin.x),
                        f32::from(event.position.y - origin.y),
                    ),
                );
                if let Some(generation) = generation {
                    *dwell_task.borrow_mut() = Some(arm_dwell(
                        generation,
                        dwell_cfg,
                        dwell_state.clone(),
                        this_emulator.clone(),
                        on_copied.clone(),
                        element_size,
                        window,
                        cx,
                    ));
                }
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
            let bar_drag = state.scrollbar.clone();
            let touched = state.scrollbar_touched.clone();
            let dwell_state = state.dwell.clone();
            let dwell_task = state.dwell_task.clone();
            let dwell_cfg = self.dwell;
            let on_copied = self.on_selection_copied.clone();
            let this_emulator = self.emulator.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }

                // ── 拖着滑块:全局跟手,拖出元素也继续(和所有滚动条一样)
                if let Some(layout) = bar {
                    let drag = bar_drag.get();
                    if let Some(grab) = drag.grab_offset {
                        let target = layout
                            .offset_for_thumb_top(px(f32::from(event.position.y) - grab));
                        let delta = target as i32 - layout.display_offset as i32;
                        if delta != 0 {
                            this_emulator.with_term_mut(|t| t.scroll_display(Scroll::Delta(delta)));
                        }
                        touched.set(Some(Instant::now()));
                        window.refresh();
                        return;
                    }
                    let hovered = hitbox.is_hovered(window)
                        && layout.hit(event.position, GRAB_SLACK) != ScrollbarHit::Miss;
                    if hovered != drag.hovered {
                        bar_drag.set(ScrollbarDrag { hovered, ..drag });
                        touched.set(Some(Instant::now()));
                        window.refresh();
                    }
                    // 悬在条上时不要顺手延伸选区;已经在拖选的除外(选区拖过条子底下)
                    if hovered && !selecting.get() {
                        return;
                    }
                } else if bar_drag.get() != ScrollbarDrag::default() {
                    bar_drag.set(ScrollbarDrag::default());
                }

                // ── 停留复制:越过 4px 阈值才重新计时
                let rearm = dwell_state.borrow_mut().on_move(
                    &dwell_cfg,
                    (
                        f32::from(event.position.x - origin.x),
                        f32::from(event.position.y - origin.y),
                    ),
                );
                if let Some(generation) = rearm {
                    *dwell_task.borrow_mut() = Some(arm_dwell(
                        generation,
                        dwell_cfg,
                        dwell_state.clone(),
                        this_emulator.clone(),
                        on_copied.clone(),
                        element_size,
                        window,
                        cx,
                    ));
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
            let bar_drag = state.scrollbar.clone();
            let touched = state.scrollbar_touched.clone();
            let dwell_state = state.dwell.clone();
            let dwell_task = state.dwell_task.clone();
            let dwell_cfg = self.dwell;
            window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }

                // ── 松开滑块。**不判 hover**:拖到窗口外松手也得收尾,
                //    否则滑块会一直粘在鼠标上
                let drag = bar_drag.get();
                if drag.grab_offset.is_some() && event.button == MouseButton::Left {
                    bar_drag.set(ScrollbarDrag {
                        grab_offset: None,
                        ..drag
                    });
                    touched.set(Some(Instant::now()));
                    window.refresh();
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

                if event.button != MouseButton::Left {
                    return;
                }
                let was_selecting = selecting.replace(false);
                // 定时器一律作废:松手之后不该再弹「已复制」
                dwell_task.borrow_mut().take();
                let action = dwell_state.borrow_mut().on_release(&dwell_cfg);
                match action {
                    // 停留语义关掉 = 旧行为,松手即复制(X11 primary selection 的习惯)
                    ReleaseAction::CopyNow if was_selecting => {
                        if let Some(text) = emulator.with_term(|t| t.selection_to_string())
                            && !text.is_empty()
                        {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        }
                    }
                    // 停留期间已经复制过一次:拖到边缘触发自动滚屏时选区可能还在长,
                    // 松手时若已变化就补一刀,让剪贴板与用户最终看到的选区一致。
                    // **不重弹气泡** —— 「已复制」对最终内容依然成立。
                    ReleaseAction::ReconcileIfChanged => {
                        let text = emulator
                            .with_term(|t| t.selection_to_string())
                            .unwrap_or_default();
                        if !text.is_empty() && dwell_state.borrow().needs_reconcile(&text) {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                            dwell_state.borrow_mut().note_copied(text);
                        }
                    }
                    _ => {}
                }
            });
        }
    }
}

/// 起一个 dwell 定时器。返回的 `Task` **必须被宿主持有** —— 一 drop 就取消,
/// 这正好是「鼠标动了要重新计时」的实现方式(直接覆盖掉旧句柄)。
///
/// 代号(`generation`)是第二道闸:定时器已经飞出去、句柄又被覆盖不掉的竞态下,
/// 回来的那一发靠代号自己认赔。
#[allow(clippy::too_many_arguments)]
fn arm_dwell(
    generation: u64,
    cfg: DwellConfig,
    tracker: Rc<RefCell<DwellTracker>>,
    emulator: Arc<TerminalEmulator>,
    on_copied: Option<OnSelectionCopied>,
    element_size: (f32, f32),
    window: &mut Window,
    cx: &mut App,
) -> gpui::Task<()> {
    let delay = cfg.dwell;
    window.spawn(cx, async move |cx| {
        cx.background_executor().timer(delay).await;
        let _ = cx.update(|window, cx| {
            let text = emulator
                .with_term(|t| t.selection_to_string())
                .unwrap_or_default();
            if !tracker
                .borrow()
                .on_dwell_elapsed(generation, !text.is_empty())
            {
                return;
            }
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            let origin = tracker.borrow().tip_origin(element_size);
            tracker.borrow_mut().note_copied(text.clone());
            if let Some(cb) = on_copied.as_ref() {
                // 落点是**元素相对**坐标(tip_origin 已按元素尺寸贴边收拢):
                // 宿主把气泡 absolute 放在终端容器里就地对得上
                cb(&text, origin, window, cx);
            }
            window.refresh();
        });
    })
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

        // ── 查找命中:在拿 grid 锁**之前**同步(引擎内部自己会短暂持锁)。
        //    引擎有去抖 + 内容指纹两道闸,这一句在空闲帧几乎是零成本。
        let highlights: Option<Rc<SearchHighlights>> = self.search.as_ref().map(|search| {
            let mut search = search.borrow_mut();
            search.sync(&self.emulator);
            search.highlights()
        });
        let highlights = highlights.filter(|h| !h.is_empty());

        // ── 帧指纹:这些变了,每一行的画面都会变而行签名不动 → 整表作废
        //
        //  命中集合**不进**帧指纹:它进的是每个格子的行签名(`CellSignature::search`),
        //  于是查找条一开只重建真正带高亮的那几行,整屏缓存不被打穿。
        //  只有配色这种「每一行都会变、签名却不动」的参数才配进这里。
        let mut key = FrameKey::builder()
            .push_f32(f(cell_width))
            .push_f32(f(line_height))
            .push_f32(f(font_size))
            .push(columns)
            .push(focused)
            .push(self.style.font_family.as_ref())
            // 连字开关**必须**进帧指纹:它改的是 shaping 结果而不是任何一个 cell 的
            // 内容,行签名一个字节都不会动 —— 漏了这一条,切开关后整屏行缓存照旧命中,
            // 表现成「开关点了没反应」。
            .push(self.style.ligatures)
            // 回退字族逐个进,不再 collect 成一个中间 Vec(每帧每 pane 一次分配)。
            // 条数先进哈希,免得「少一条回退」与「某条名字接在了前一条后面」撞上。
            .push(self.style.font_fallbacks.len());
        for fallback in &self.style.font_fallbacks {
            key = key.push(fallback.as_ref());
        }
        let frame_key = key
            .push_hsla(self.theme.selection)
            .push_hsla(self.theme.cursor)
            .push_hsla(self.theme.cursor_text)
            .push_hsla(self.search_colors.matched)
            .push_hsla(self.search_colors.current)
            .push_hsla(self.search_colors.current_border)
            .finish();
        state.rows.borrow_mut().begin_frame(frame_key);

        let mut placed: Vec<(usize, Rc<RowRender>)> = Vec::with_capacity(screen_lines);
        let mut pending: Vec<RowPending> = Vec::new();
        let mode;
        let mut rows_seen = 0usize;
        // 滚动条要用:整条内容有多长、视口顶在哪
        let total_lines;
        let frame_display_offset;
        // IME 锚点要用:光标在哪一格。**不受 `CursorShape::Hidden` 影响**,
        // 藏起来的光标照样占着一个格子 —— 见 [`cursor_cell_bounds`]。
        let frame_cursor_point;

        {
            let term_lock = self.emulator.term().lock();
            total_lines = term_lock.grid().total_lines();
            let content = term_lock.renderable_content();
            let display_offset = content.display_offset;
            frame_display_offset = display_offset;
            mode = content.mode;
            let colors_table = content.colors;
            let selection_range = content.selection;
            let cursor_point = content.cursor.point;
            let cursor_shape = content.cursor.shape;
            frame_cursor_point = cursor_point;

            let mut cache = state.rows.borrow_mut();
            // 逐行解析出来的格子写进这根跨帧复用的 arena(见 `TerminalElementState::cells`):
            // 一行解析完就算签名,命中缓存的把尾巴截掉、未命中的原地留着,
            // 只把「这一段的下标」交给 shaping —— 全程零分配、零拷贝。
            let mut arena = state.cells.borrow_mut();
            arena.clear();
            // 一帧最多用到「可视行 × 列」这么多格子。窗口拖小 / 字号调大之后按新的
            // 上限收一收容量,免得旧的高水位一路留到进程退出(只在真的超了才重分配)。
            arena.shrink_to(columns.saturating_mul(screen_lines));
            // 当前行在 arena 里的起点。命中就退回它,未命中就前进到行尾。
            let mut row_start = 0usize;
            let mut current_row: Option<usize> = None;
            // 每帧新建:阈值在一帧内恒定,不必进键(见 `ContrastMemo` 的说明)。
            let mut contrast = colors::ContrastMemo::default();
            // 命中按行取一次就够:逐格去问 1000 条命中是一屏一千万次比较。
            let mut row_spans: &[HighlightSpan] = &[];

            let flush_row = |row: usize,
                                 arena: &mut Vec<CellSignature>,
                                 row_start: &mut usize,
                                 cache: &mut RowCache<Rc<RowRender>>,
                                 placed: &mut Vec<(usize, Rc<RowRender>)>,
                                 pending: &mut Vec<RowPending>| {
                let start = *row_start;
                if arena.len() == start {
                    return;
                }
                let sig = row_signature(&arena[start..]);
                match cache.get(sig) {
                    // 命中:格子已经没用了,截回行首让下一行接着写这段容量
                    Some(render) => {
                        placed.push((row, render));
                        arena.truncate(start);
                    }
                    // 未命中:留在 arena 里等放锁之后 shape,只记一段下标
                    None => {
                        pending.push(RowPending {
                            row,
                            sig,
                            cells: start..arena.len(),
                        });
                        *row_start = arena.len();
                    }
                }
            };

            for indexed in content.display_iter {
                let row = (indexed.point.line.0 + display_offset as i32).max(0) as usize;
                if current_row != Some(row) {
                    if let Some(prev) = current_row {
                        flush_row(
                            prev,
                            &mut arena,
                            &mut row_start,
                            &mut cache,
                            &mut placed,
                            &mut pending,
                        );
                        rows_seen += 1;
                    }
                    current_row = Some(row);
                    arena.reserve(columns);
                    row_spans = highlights
                        .as_ref()
                        .map(|h| h.row(indexed.point.line.0))
                        .unwrap_or(&[]);
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
                // ── 最小对比度:前景与背景近似同色时把前景推开。
                //
                //    **夹在 INVERSE 与 HIDDEN 之间是硬要求**:在 INVERSE 之后才对得上
                //    「真正画出来的那一对」;在 HIDDEN 之前才不会把 `read -s` 的密码
                //    强行显形(HIDDEN 就是靠 fg = bg 实现的,修正跑在它后面等于撤销它)。
                //    块状光标那格随后会把 fg 覆盖成 cursor_text,这里白算一次但不出错。
                //    powerline 分隔符/块元素这类「拿字符当色块画」的字形不在修正之列,
                //    理由见 [`colors::is_fill_glyph`]。
                if colors::wants_contrast_fix(cell.c, flags) {
                    fg = contrast.resolve(fg, bg, colors::MIN_CONTRAST_RATIO);
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

                let search = row_spans
                    .iter()
                    .find(|s| col >= s.start && col <= s.end)
                    .map(|s| s.kind.code())
                    .unwrap_or(0);

                arena.push(CellSignature {
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
                    search,
                });
            }
            if let Some(row) = current_row {
                flush_row(
                    row,
                    &mut arena,
                    &mut row_start,
                    &mut cache,
                    &mut placed,
                    &mut pending,
                );
                rows_seen += 1;
            }
        }

        // ── shape 只发生在「内容真的变了」的行上。
        //    锁已经放掉了 —— shaping 会往 DirectWrite 里跑,别拿着 grid 锁做。
        {
            // arena 与「步进是否一列宽」的表都借一次,整批 shape 共用。
            let arena = state.cells.borrow();
            let mut advance = state.advance.borrow_mut();
            advance.begin_frame(&variant_fonts, font_size, cell_width);
            for row in pending {
                let render = Rc::new(build_row(
                    window,
                    &arena[row.cells],
                    &font,
                    font_size,
                    &variant_fonts,
                    &mut advance,
                    cell_width,
                    line_height,
                    &self.theme,
                    self.style.ligatures,
                ));
                state.rows.borrow_mut().insert(row.sig, render.clone());
                placed.push((row.row, render));
            }
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

        // ── 光标格。上面那个 `cursor`(CursorLayout)是「要画的光标」,可能没有;
        //    这个是「光标占的那一格」,永远有 —— IME 拿它当锚点。
        let cursor_cell = cursor_cell_bounds(
            bounds.origin,
            cell_size,
            columns,
            screen_lines,
            frame_cursor_point.line.0,
            frame_cursor_point.column.0,
            frame_display_offset,
        );

        // ── IME 预编辑浮层
        let preedit = self.preedit.as_ref().and_then(|p| {
            if p.text.is_empty() {
                return None;
            }
            let anchor = cursor_cell.origin;
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
                cursor: Some(cursor_cell),
                preedit_caret: preedit.as_ref().map(|p| {
                    Bounds::new(
                        point(p.origin.x + p.caret_x, p.origin.y),
                        size(px(2.0), line_height),
                    )
                }),
            });
        }

        // ── 滚动条几何
        //
        //  alt screen(vim / less / htop)没有回看缓冲,画一条永远满格的条子纯属误导。
        //  offset 变了就把淡出计时重新起表 —— 包括程序化滚动,不只是鼠标滚。
        if state.last_offset.replace(frame_display_offset) != frame_display_offset {
            state.scrollbar_touched.set(Some(Instant::now()));
        }
        let scrollbar = if mode.contains(TermMode::ALT_SCREEN) {
            None
        } else {
            scrollbar::layout(
                bounds,
                &self.scrollbar,
                total_lines,
                screen_lines,
                frame_display_offset,
            )
        };

        // ── 整行闪烁:跳到 marker 之后那 300ms 的提示。落在视口外就不画
        //    (用户跳完又自己滚开了)。
        let flash = self.flash.and_then(|flash| {
            let row = flash_row(flash.line, frame_display_offset, screen_lines)?;
            Some((
                Bounds::new(
                    point(bounds.origin.x, bounds.origin.y + line_height * row as f32),
                    size(bounds.size.width, line_height),
                ),
                flash.color,
            ))
        });

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
            scrollbar,
            flash,
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

        // ── 背景图铺在最底下。grid 侧「默认背景不发 quad」的路早就通了,
        //    再加上半透明的 TerminalTheme::background,图自然透上来。
        if let Some(art) = self.background_art.as_ref() {
            art.paint_into(bounds, window, cx);
        }

        let origin = bounds.origin;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (y, render) in prepared.rows.iter() {
                let delta = point(origin.x, origin.y + *y);
                for (rect, color) in render.backgrounds.iter() {
                    window.paint_quad(fill(translate(*rect, delta), *color));
                }
            }
            // ── 整行闪烁垫在格子底色之上、查找命中之下:它是半透明的一次性提示,
            //    压住底色即可,不该盖过查找/选择这两种「持续态」的表达。
            if let Some((rect, color)) = prepared.flash {
                window.paint_quad(fill(rect, color));
            }
            // ── 查找命中垫在选择区**下面**:选择区是半透明蓝(alpha 0.30),
            //    压在命中底色之上仍然认得出「这块被选中了」;反过来叠的话
            //    命中的暖色会把选择区吃掉,拖选复制搜索结果时看不出选到哪。
            for (y, render) in prepared.rows.iter() {
                let delta = point(origin.x, origin.y + *y);
                for (rect, current) in render.searches.iter() {
                    let rect = translate(*rect, delta);
                    let color = if *current {
                        self.search_colors.current
                    } else {
                        self.search_colors.matched
                    };
                    window.paint_quad(fill(rect, color));
                    // 当前命中再描一圈亮边:一屏几十条同色底块时,光靠深浅
                    // 分不出「我现在在第几条」
                    if *current {
                        paint_hollow_rect(window, rect, self.search_colors.current_border);
                    }
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

            // ── 滚动条画在最上层。**只发 quad,不碰 RowCache 也不碰帧指纹** ——
            //    拖滑块时行缓存该命中还是命中(滚屏行只是换了个 y)。
            self.paint_scrollbar(prepared, window);
        });

        self.paint_mouse_listeners(prepared, window, cx);
    }
}

/// 一行「内容变了、需要重建」的暂存。
///
/// `cells` 是它在 arena(`TerminalElementState::cells`)里的下标区间,不是自己的
/// 一份 Vec —— 每个未命中的行一次分配是刷屏时最贵的一笔。
struct RowPending {
    row: usize,
    sig: u64,
    cells: std::ops::Range<usize>,
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
    /// 合并段占的列数(段内每个格子都是窄字符、无组合符号,所以列数 = 字符数)。
    ///
    /// `None` = 单格片段(宽字符 / 缺字回退 / 带组合符号)。它本来就允许糊出格子
    /// 边界(模块注释的第二类),没有「该占几列」可言,不参与总宽校验。
    cols: Option<usize>,
    text: String,
    style: RunStyle,
}

/// 合并段总宽与「列数 × 列宽」的容差(px)。
///
/// 守恒的连字字体这里恒等于 0,给 0.5px 是留给浮点累加的余量 —— 半个像素以内
/// 肉眼看不出,超过就说明这个字体的连字压根不按等宽网格设计。
const LIGATURE_WIDTH_SLACK: f32 = 0.5;

/// 合并段 shape 完还落在列格上吗。
///
/// 连字把 N 个字符换成别的字形是允许的,**换完总共还得占 N 列**——
/// 这是段内后续字符仍对得上列的唯一前提。见 [`super::theme::TerminalStyle::font`]。
fn width_fits_columns(shaped: Pixels, cell_width: Pixels, cols: usize) -> bool {
    (f(shaped) - f(cell_width) * cols as f32).abs() <= LIGATURE_WIDTH_SLACK
}

/// 「⌥+点击定位光标」一次最多合成多少个方向键。
///
/// 手滑点到几千列开外时别把 PTY 灌爆:超了就整个不动,比走一半停下强
/// (走一半的结果既不是用户要的位置,又没法撤销)。
const MAX_CURSOR_MOVE_STEPS: usize = 512;

/// 「⌥+点击定位光标」要发的方向键序列。
///
/// 这是个**启发式**:光标最终落在哪由前台程序的行编辑器说了算(readline /
/// PSReadLine / Ink 各有各的规则),模拟器能做的只是按列差把方向键发过去。任何终端
/// 里的这个功能都是这么实现的 —— shell 与 TUI 自己都不认鼠标定位。
///
/// # 只走同一行
///
/// **跨行一律不动**。竖向位移在行编辑器里往往不是「移动光标」而是**召回历史**:
/// pwsh(PSReadLine)里点上一行,一个 Up 就把当前输入整行换成了历史条目 —— 用户
/// 想挪个光标,结果正在编辑的内容没了。Terminal.app 的 ⌥+click 有这个坑,这里不抄。
/// 多行编辑器里的跨行定位留给以后配开关(上游评审实测出的这条,见 PR #59)。
///
/// # 对 Ink 类 TUI 不保证
///
/// Claude CLI / Codex 这类 Ink 应用把**硬件光标停在输入行末尾**(可见光标块之后),
/// 而这里的起点取的是 grid 光标 —— 两者对不上,落点会偏。上游评审实测:输入
/// `abc def` 去点 `d`,字符落到了空格前。只有可见光标恰在行末时才近似可用。
/// 这不是本实现的缺陷(Terminal.app 在 Claude Code 里同样如此),但别指望它。
/// shell 提示符(bash / zsh / pwsh)下则是逐格准确的。
fn cursor_move_bytes(
    from_line: i32,
    from_col: i32,
    to_line: i32,
    to_col: i32,
    mode: TermMode,
) -> Vec<u8> {
    if from_line != to_line {
        return Vec::new();
    }
    let dx = to_col - from_col;
    let steps = dx.unsigned_abs() as usize;
    if steps == 0 || steps > MAX_CURSOR_MOVE_STEPS {
        return Vec::new();
    }
    let dir = if dx > 0 { Arrow::Right } else { Arrow::Left };
    let seq = arrow_bytes(dir, mode.contains(TermMode::APP_CURSOR));
    let mut out = Vec::with_capacity(seq.len() * steps);
    for _ in 0..steps {
        out.extend_from_slice(&seq);
    }
    out
}

/// 把一行解析好的格子变成可绘制产物。**几何全部相对行首**。
#[allow(clippy::too_many_arguments)]
fn build_row(
    window: &Window,
    cells: &[CellSignature],
    font: &gpui::Font,
    font_size: Pixels,
    variant_fonts: &VariantFonts,
    advance: &mut AdvanceCache,
    cell_width: Pixels,
    line_height: Pixels,
    theme: &TerminalTheme,
    ligatures: bool,
) -> RowRender {
    let cell_size = size(cell_width, line_height);
    let mut backgrounds: Vec<(Bounds<Pixels>, Hsla)> = Vec::new();
    let mut selections: Vec<Bounds<Pixels>> = Vec::new();
    let mut searches: Vec<(Bounds<Pixels>, bool)> = Vec::new();
    let mut pieces: Vec<PendingPiece> = Vec::new();
    let mut cursor: Option<CursorLayout> = None;

    let mut bg_run: Option<(usize, usize, Hsla)> = None;
    let mut sel_run: Option<(usize, usize)> = None;
    let mut search_run: Option<(usize, usize, u8)> = None;
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

        // ── 查找命中(普通 / 当前两档,同档连续格子并成一段)
        if cell.search != 0 {
            match search_run.as_mut() {
                Some((_, end, kind)) if *kind == cell.search && *end + 1 == col => *end = col,
                _ => {
                    flush_search(&mut search_run, cell_size, &mut searches);
                    search_run = Some((col, col, cell.search));
                }
            }
        } else {
            flush_search(&mut search_run, cell_size, &mut searches);
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
        let slot = VariantFonts::slot(style_key.bold, style_key.italic);
        let run_font_id = variant_fonts.id_at(slot);
        // 可合并的条件:窄字符、无组合符号、不是光标格(光标格颜色单独)、
        // 且主字体里这个字形的步进恰好一列宽。
        let mergeable = !cell.flags.contains(Flags::WIDE_CHAR)
            && !has_zerowidth
            && cell.cursor == 0
            && advance.fits(window, slot, run_font_id, font_size, cell.ch, cell_width);

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
                cols: None,
                text,
                style: style_key,
            });
        }
    }
    flush_bg(&mut bg_run, cell_size, &mut backgrounds);
    flush_sel(&mut sel_run, cell_size, &mut selections);
    flush_search(&mut search_run, cell_size, &mut searches);
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
        let text = SharedString::from(piece.text);
        let mut shaped =
            window
                .text_system()
                .shape_line(text.clone(), font_size, std::slice::from_ref(&run), None);

        // ── 连字兜底:合并段的总宽必须仍等于「列数 × 列宽」。
        //
        //  编程连字字体都守恒(N 个字符换成的连字仍占 N 列),这一句恒不触发。
        //  但字体是用户配的,遇上一个把连字做成「两个字符宽的形状塞进一列」的
        //  字体,段内连字之后的每个字符都会左移一列 —— 段与段之间各按列定位挡住了
        //  扩散,这里把这一段也拉回来:退回禁连字重 shape 一次。
        //
        //  只在开了连字时校验。关着还对不上说明这个字族根本不等宽,那是
        //  `report_metrics_once` 该喊的事,重 shape 一遍也救不回来。
        if ligatures
            && let Some(cols) = piece.cols
            && !width_fits_columns(shaped.width, cell_width, cols)
        {
            let mut plain_font = run.font.clone();
            plain_font.features = gpui::FontFeatures::disable_ligatures();
            let plain = TextRun {
                font: plain_font,
                ..run
            };
            shaped =
                window
                    .text_system()
                    .shape_line(text, font_size, std::slice::from_ref(&plain), None);
        }

        texts.push(TextPiece {
            origin: point(cell_width * piece.start as f32, px(0.0)),
            line: shaped,
        });
    }

    RowRender {
        backgrounds,
        selections,
        searches,
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
            cols: Some(r.len),
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

fn flush_search(
    run: &mut Option<(usize, usize, u8)>,
    cell: Size<Pixels>,
    out: &mut Vec<(Bounds<Pixels>, bool)>,
) {
    let Some((start, end, kind)) = run.take() else {
        return;
    };
    out.push((rect_for(start, end, cell), kind == 2));
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

    /// 四档变体的下标。[`AsciiAdvance`] 的表也按它分层。
    fn slot(bold: bool, italic: bool) -> usize {
        usize::from(bold) + 2 * usize::from(italic)
    }

    fn id(&self, bold: bool, italic: bool) -> FontId {
        self.ids[Self::slot(bold, italic)]
    }

    fn id_at(&self, slot: usize) -> FontId {
        self.ids[slot]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// ⌥+点击定位光标:同一行按列差发左右键,零位移不发。
    #[test]
    fn 点击定位光标合成方向键() {
        let m = TermMode::empty();
        // 同行右移 3 格
        assert_eq!(
            cursor_move_bytes(0, 2, 0, 5, m),
            b"\x1b[C\x1b[C\x1b[C".to_vec()
        );
        // 同行左移 2 格
        assert_eq!(cursor_move_bytes(0, 5, 0, 3, m), b"\x1b[D\x1b[D".to_vec());
        // 点在光标本身:什么都不发,别让一次点击白白喂 PTY 一个空包
        assert!(cursor_move_bytes(3, 7, 3, 7, m).is_empty());
    }

    /// **跨行一律不动**。竖向位移在行编辑器里往往是召回历史而不是移动光标 ——
    /// pwsh 里点上一行,一个 Up 就把正在编辑的整行换成历史条目(上游评审实测)。
    /// 这条钉住「宁可不动也不能毁掉用户正在输入的内容」。
    #[test]
    fn 点击定位光标跨行不动() {
        let m = TermMode::empty();
        assert!(cursor_move_bytes(1, 4, 2, 5, m).is_empty(), "往下跨行");
        assert!(cursor_move_bytes(2, 4, 1, 3, m).is_empty(), "往上跨行");
        // 跨行且同列同样不动
        assert!(cursor_move_bytes(1, 4, 5, 4, m).is_empty(), "跨行同列");
    }

    /// DECCKM(`APP_CURSOR`)下方向键换 SS3 前缀 —— 与 `keystroke_to_bytes` 同一口径,
    /// 两处若分叉,vim 这类开了应用光标键的程序会收到它不认的序列。
    #[test]
    fn 点击定位光标跟随应用光标键模式() {
        assert_eq!(
            cursor_move_bytes(0, 0, 0, 2, TermMode::APP_CURSOR),
            b"\x1bOC\x1bOC".to_vec()
        );
    }

    /// 点到几千列开外时整个不动:走一半的结果既不是用户要的位置,又没法撤销。
    #[test]
    fn 点击定位光标超出步数上限即放弃() {
        let far = MAX_CURSOR_MOVE_STEPS as i32 + 1;
        assert!(cursor_move_bytes(0, 0, 0, far, TermMode::empty()).is_empty());
        // 上限本身要放行
        let at_limit = cursor_move_bytes(0, 0, 0, MAX_CURSOR_MOVE_STEPS as i32, TermMode::empty());
        assert_eq!(at_limit.len(), MAX_CURSOR_MOVE_STEPS * 3);
    }

    /// 闪烁行的 grid 绝对行号 → 屏幕行:`row = line + display_offset`。
    /// 视口顶在最新内容时(offset = 0),正数行号就是屏幕行本身。
    #[test]
    fn 闪烁行按显示偏移换算屏幕行() {
        assert_eq!(flash_row(0, 0, 24), Some(0));
        assert_eq!(flash_row(23, 0, 24), Some(23));
        // 回看缓冲里的行(负号)只有在往回滚够了之后才落进视口
        assert_eq!(flash_row(-5, 0, 24), None);
        assert_eq!(flash_row(-5, 5, 24), Some(0));
        assert_eq!(flash_row(-5, 10, 24), Some(5));
    }

    /// 落在视口外一律不画 —— 用户跳完又自己滚开了,不该在别的行上留一道底色。
    #[test]
    fn 闪烁行滚出视口就不画() {
        assert_eq!(flash_row(24, 0, 24), None, "越过最后一行");
        assert_eq!(flash_row(-1, 0, 24), None, "在视口上方");
        // 往回滚太多:那一行被顶到视口下面去了
        assert_eq!(flash_row(0, 24, 24), None);
        assert_eq!(flash_row(0, 23, 24), Some(23));
    }

    /// 光标格 = 元素原点 + 行列步进。IME 的候选框贴的就是这个矩形。
    #[test]
    fn 光标格按行列换算元素坐标() {
        let origin = point(px(100.0), px(50.0));
        let cell = size(px(8.0), px(16.0));
        // 左上角那一格
        let b = cursor_cell_bounds(origin, cell, 80, 24, 0, 0, 0);
        assert_eq!(b.origin, origin);
        assert_eq!(b.size, cell);
        // 第 3 行第 5 列
        let b = cursor_cell_bounds(origin, cell, 80, 24, 3, 5, 0);
        assert_eq!(b.origin, point(px(140.0), px(98.0)));
        // 回看缓冲里的负行号:滚够了才落回视口(与 flash_row 同一口径)
        let b = cursor_cell_bounds(origin, cell, 80, 24, -5, 0, 5);
        assert_eq!(b.origin.y, px(50.0));
    }

    /// **本 bug 的回归**:Ink 系 TUI(Claude Code)发 `ESC[?25l` 藏掉光标后,
    /// 没有任何 cell 带光标标记 —— 但光标格照样要算得出来,否则 IME 候选框
    /// 和预编辑串双双退回元素左上角,中文候选窗糊在终端顶行上。
    ///
    /// 这个函数**根本不看 `CursorShape`**,可见性进不来就是这条保证。
    #[test]
    fn 光标藏起来时照样给得出格子() {
        let origin = point(px(0.0), px(0.0));
        let cell = size(px(10.0), px(20.0));
        // 光标在第 12 行第 30 列,shape 是 Hidden 与否都不影响这里
        let b = cursor_cell_bounds(origin, cell, 80, 24, 12, 30, 0);
        assert_eq!(b.origin, point(px(300.0), px(240.0)));
        assert_ne!(b.origin, origin, "绝不能退回元素左上角");
    }

    /// 连字可以换字形,但**换完总共还得占 N 列**。守恒才放行,不守恒就退回禁连字
    /// 重 shape —— 否则这一段里连字之后的每个字符都会整体左移。
    #[test]
    fn 连字总宽守恒才放行() {
        let cell = px(8.0);
        // `=>` 换成一个两列宽的连字:总宽不变
        assert!(width_fits_columns(px(16.0), cell, 2));
        // 浮点累加的余量以内照样算守恒
        assert!(width_fits_columns(px(16.4), cell, 2));
        assert!(width_fits_columns(px(15.6), cell, 2));
        // 两个字符缩成一列宽:段内后面的字全要左移一列
        assert!(!width_fits_columns(px(8.0), cell, 2));
        assert!(!width_fits_columns(px(16.6), cell, 2));
        // 长段里塌掉一列也判负(80 列的整行是常态)
        assert!(width_fits_columns(px(640.0), cell, 80));
        assert!(!width_fits_columns(px(632.0), cell, 80));
    }

    /// 滚出视口的光标钳到最近边缘,而不是没有 —— 往回翻历史时光标在视口下方,
    /// 这时候开始打字,候选框贴在底边比贴在顶角合理。
    #[test]
    fn 光标滚出视口钳到最近边缘() {
        let origin = point(px(0.0), px(0.0));
        let cell = size(px(10.0), px(20.0));
        // 往回滚 100 行:光标被顶到视口下面 → 钳到最后一行
        let b = cursor_cell_bounds(origin, cell, 80, 24, 0, 0, 100);
        assert_eq!(b.origin.y, px(460.0), "第 23 行 = (24-1) * 20");
        // 列越界钳到最后一列(宽字符占位等边界情形)
        let b = cursor_cell_bounds(origin, cell, 80, 24, 0, 999, 0);
        assert_eq!(b.origin.x, px(790.0), "第 79 列 = (80-1) * 10");
        // 退化尺寸不 panic、不算出负坐标
        let b = cursor_cell_bounds(origin, cell, 0, 0, 0, 0, 0);
        assert_eq!(b.origin, origin);
    }
}
