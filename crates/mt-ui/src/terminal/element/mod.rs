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
//!   主字体缺的私用区图标(Nerd Font)也在这一类,只是换内置图标字体、按格子
//!   缩了字号画;其中 powerline 分隔符不走字体,按格子几何直接画路径。
//!   见 [`super::symbols`]。
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
    App, Bounds, ClipboardItem, ContentMask, Corners, CursorStyle, DispatchPhase, Element,
    ElementId, EntityId, FocusHandle, FontId, GlobalElementId, Hitbox, HitboxBehavior, Hsla,
    InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseExitEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollDelta, ScrollWheelEvent, ShapedLine,
    SharedString, Size, StrikethroughStyle, Style, TextRun, UnderlineStyle, Window, fill, point,
    px, size,
};
use mt_terminal::{SettledCursor, TermSize, TerminalEmulator};

use super::colors;
use super::damage::{
    CellSignature, DamageStats, FrameKey, MAX_ZEROWIDTH_CHARS, RowCache, row_signature,
};
use super::drag_select::{
    AUTOSCROLL_TICK, DragAnchor, LeftPress, ScrollZone, autoscroll_lines, cell_in_selection,
    classify_left_press, drag_exceeds_cell, update_or_restore_selection,
};
use super::input::{Arrow, arrow_bytes};
use super::links::{
    self, FileLinks, HoveredLink, LINK_HINT_DELAY, LinkCache, LinkHint, LinkHintLabel, LinkTarget,
};
use super::mouse::{
    GridPos, MouseAction, MouseBtn, MouseMods, WheelDir, alt_screen_scroll_bytes,
    mouse_report_bytes, mouse_reporting_active, prefers_local_handling,
};
use super::scrollbar::{self, ScrollbarDrag, ScrollbarHit, ScrollbarLayout, ScrollbarStyle};
use super::search::{HighlightSpan, SearchHighlights, TerminalSearch};
use super::selection_dwell::{
    DwellConfig, DwellTracker, OnSelectionCopied, ReleaseAction, tip_colors,
};
use super::symbols;
use super::theme::{SearchColors, TerminalStyle, TerminalTheme};
use crate::background::BackgroundArtElement;
use crate::theme_bridge::BackgroundArt;

/// 一屏最多认多少列/行。窗口被拖到荒谬尺寸时防止 grid 爆掉。
const MAX_COLUMNS: usize = 1024;
const MAX_LINES: usize = 512;

/// 6px 的滚动条精确命中太难点,左右各放宽 3px(与原生滚动条的手感一致)。
const GRAB_SLACK: Pixels = px(3.0);

/// `Pixels` 的内部字段是 `pub(crate)`,取标量只能走 `From`。写成短名字省得刷屏。
#[inline]
fn f(p: Pixels) -> f32 {
    f32::from(p)
}

mod layout;
mod mouse;
mod paint;

use layout::*;
use mouse::*;
use paint::*;

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
/// Ctrl+滚轮缩放字号的出口:`+1` 放大一档、`-1` 缩小一档。字号是宿主的全局配置
/// (所有终端一起变、要落盘),元素只报方向。
pub type OnZoom = Rc<dyn Fn(i32, &mut Window, &mut App)>;

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
/// 而网格的像素原点与格子大小是渲染阶段才算得出来的 —— 视图侧只能从这里读。
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameGeometry {
    pub origin: Point<Pixels>,
    pub cell_size: Size<Pixels>,
    pub columns: usize,
    pub screen_lines: usize,
    pub display_offset: usize,
    /// 这一帧 IME 贴着的那一格,也是预编辑串的起点。**与光标可见性无关** ——
    /// TUI 藏起光标(`ESC[?25l`)自己画插入符时,这里照样给出那一格,否则 IME
    /// 候选框会退到元素左上角。`None` 只有一个含义:还没画过任何一帧。
    pub cursor: Option<Bounds<Pixels>>,
    /// 这一帧画着预编辑串吗。
    pub composing: bool,
}

impl FrameGeometry {
    /// 光标位置 → 屏幕矩形。IME 取坐标时现读光标,不必等下一帧才对得上。
    pub fn cell_of(&self, cursor: SettledCursor) -> Bounds<Pixels> {
        cursor_cell_bounds(
            self.origin,
            self.cell_size,
            self.columns,
            self.screen_lines,
            cursor.line,
            cursor.column,
            self.display_offset,
        )
    }
}

/// 这一帧要不要把 IME 坐标重新报给平台。
///
/// gpui 的 Windows 后端**一轮组合只在 `WM_IME_STARTCOMPOSITION` 定位一次**,之后的
/// 组合更新都不再设坐标,所以那一下的坐标不对,整轮候选窗就一直停在错的地方
/// (issue #88)。这里补两个时机:
///
/// - **组合后的第一帧**:哪怕坐标没变也报一次。输入法可能先于上面那次定位取到
///   上一轮留下的坐标,再报一次才有机会纠正;
/// - **组合中锚点挪了**:输入框被流式正文往下推、回显把光标带走。
///
/// 打字本身不会触发:锚点是组合串的起点,不是串内的插入符,候选窗不随字母右移。
fn ime_anchor_needs_sync(prev: &FrameGeometry, next: &FrameGeometry) -> bool {
    next.composing && (!prev.composing || prev.cursor != next.cursor)
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
    on_zoom: Option<OnZoom>,
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
    file_links: Option<FileLinks>,
    /// 不按修饰键的悬停提示文案。`None` = 不出提示。见 [`super::links`] 模块注释。
    link_hint: Option<LinkHintLabel>,
    /// Kitty 图片的纹理缓存,由视图持有。见 [`super::images`]。
    image_cache: Option<Rc<RefCell<super::images::TerminalImageCache>>>,
}

/// 悬停提示那条下划线的透明度(相对前景色)。Ctrl 悬停那条是满色,这条只是「这里能点」
/// 的暗示,压到三分之一上下,一眼分得出两种状态。
const LINK_HINT_UNDERLINE_ALPHA: f32 = 0.35;

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
            on_zoom: None,
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
            file_links: None,
            link_hint: None,
            image_cache: None,
        }
    }

    /// Kitty 图片的纹理缓存。不接就不画图(宿主没开图形时就是这样)。
    pub(super) fn image_cache(
        mut self,
        cache: Rc<RefCell<super::images::TerminalImageCache>>,
    ) -> Self {
        self.image_cache = Some(cache);
        self
    }

    /// Ctrl(⌘)+悬停 / 单击文件路径。见 [`super::links`]。
    pub fn file_links(mut self, links: Option<FileLinks>) -> Self {
        self.file_links = links;
        self
    }

    /// 不按修饰键悬停在链接上停一会儿,出弱化下划线 +「Ctrl+单击打开」气泡。
    /// 要先接 [`Self::file_links`] 才有用。见 [`super::links`] 模块注释。
    pub fn link_hint(mut self, label: Option<LinkHintLabel>) -> Self {
        self.link_hint = label;
        self
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

    /// 见 [`OnZoom`]。不接 = Ctrl+滚轮照旧按普通滚轮处理。
    pub fn on_zoom(mut self, f: impl Fn(i32, &mut Window, &mut App) + 'static) -> Self {
        self.on_zoom = Some(Rc::new(f));
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
    /// 当前拖选的起选锚点(中途 `term.selection` 意外被清时用于重建，见 [`DragAnchor`])。
    drag_anchor: Rc<StdCell<Option<DragAnchor>>>,
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
    /// 「这个字符归哪类画法」(并段 / 单画 / 换内置图标字体)的缓存,见 [`AdvanceCache`]。
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
    /// 按住 Ctrl 时鼠标上一次所在的格子(没按 / 不在元素上 = `None`)。只用来判断
    /// 「该不该要一帧」,悬停态本身在 paint 里现算(见 [`super::links`])。
    link_pointer: Rc<StdCell<Option<(usize, usize)>>>,
    /// 宿主对路径候选的裁决缓存。
    link_cache: Rc<RefCell<LinkCache>>,
    /// 不按修饰键的悬停提示(见 [`super::links`] 模块注释)。
    link_hint: Rc<RefCell<LinkHint>>,
    /// 悬停提示的停留计时器。**必须留着句柄** —— 换格时覆盖掉旧的就是取消。
    link_hint_task: Rc<RefCell<Option<gpui::Task<()>>>>,
    /// 悬停提示自己那份裁决缓存。与 Ctrl 那份分开:那份一松开 Ctrl 就清,而悬停提示
    /// 根本不按 Ctrl;这份在每次停留到点时清,刚生成的文件下一次停留就认得出。
    link_hint_cache: Rc<RefCell<LinkCache>>,
    /// ⌥+按下、还没拖过一格的那一下(见 [`AltPress`])。松手时还在 = 定位光标。
    alt_press: Rc<StdCell<Option<AltPress>>>,
    /// 拖选自动滚屏与节拍任务共享的那份(见 [`AutoScroll`])。
    autoscroll: Rc<StdCell<AutoScroll>>,
    /// 自动滚屏的节拍任务。**必须留着句柄** —— 松手 / 新的按下 / 元素销毁时随之 drop,
    /// 就是确定能停的那一下。任务的闭包**不许**持有这个格子(那是一个循环引用,元素销毁
    /// 时任务永远 drop 不掉)。
    autoscroll_task: Rc<RefCell<Option<gpui::Task<()>>>>,
}

/// ⌥+按下还没拖过一格:选区先不动,松手时没拖过 = 定位光标,拖过一格 = 从这里开块选。
#[derive(Clone, Copy, Debug)]
struct AltPress {
    /// 按下处(窗口坐标)。拖没拖过一格按它比(见 [`drag_exceeds_cell`])。
    position: Point<Pixels>,
    /// 按下那一格的屏幕列与行(定位光标的目标格)。
    col: usize,
    row: usize,
    /// 块选的锚点(grid 坐标)与半格。
    anchor: AlacPoint,
    side: Side,
    /// 按下那一刻的 `history_size` 与是否在 alt screen(转块选时带进 [`DragAnchor`])。
    history: usize,
    alt_screen: bool,
}

/// 拖选自动滚屏的共享状态。指针停在区外不动时没有 mouse-move 可等,节拍任务只能读这里。
#[derive(Clone, Copy, Debug, Default)]
struct AutoScroll {
    /// 节拍任务在不在跑。由起停两头维护,任务自己判停时也会把它放下。
    running: bool,
    /// 最近一次拖动事件的指针位置(窗口坐标)。
    pointer: Point<Pixels>,
    /// 最近一帧的网格几何 —— 滚完要按它把活动端落到边行上。每帧 paint 刷新。
    origin: Point<Pixels>,
    cell_size: Size<Pixels>,
    columns: usize,
    screen_lines: usize,
    zone: ScrollZone,
}

impl Default for TerminalElementState {
    fn default() -> Self {
        Self {
            scroll_remainder: Rc::new(StdCell::new(0.0)),
            selecting: Rc::new(StdCell::new(false)),
            drag_anchor: Rc::new(StdCell::new(None)),
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
            link_pointer: Rc::new(StdCell::new(None)),
            link_cache: Rc::new(RefCell::new(LinkCache::default())),
            link_hint: Rc::new(RefCell::new(LinkHint::default())),
            link_hint_task: Rc::new(RefCell::new(None)),
            link_hint_cache: Rc::new(RefCell::new(LinkCache::default())),
            alt_press: Rc::new(StdCell::new(None)),
            autoscroll: Rc::new(StdCell::new(AutoScroll::default())),
            autoscroll_task: Rc::new(RefCell::new(None)),
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

/// 一个按几何直接画的 powerline 分隔符(见 [`symbols::powerline_separator`])。
///
/// 存的是 [`symbols::Separator`] 而不是建好的 Path:Path 是窗口绝对坐标,
/// 行缓存要跨行复用就只能每帧现建 —— 一屏也就提示符上那几个,不值得再缓存。
#[derive(Clone)]
struct SeparatorPiece {
    /// 行内相对坐标(格子左上角)。
    origin: Point<Pixels>,
    separator: symbols::Separator,
    color: Hsla,
    stroke: Pixels,
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
    separators: Vec<SeparatorPiece>,
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
    /// 本帧的回看偏移。屏幕行 ↔ grid 行号的换算(`line = row - display_offset`)。
    display_offset: usize,
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
    /// 这一帧看得见的 Kitty 图片。没开图形或没有图时是空的。
    images: Vec<mt_terminal::kitty::VisibleImage>,
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
        let state: TerminalElementState =
            window.with_element_state::<TerminalElementState, _>(id.unwrap(), |prev, _window| {
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
        report_metrics_once(
            window,
            font_id,
            font_size,
            cell_width,
            line_height,
            &self.style,
        );
        // Kitty 图形要知道一格多少设备像素:查询应答、按显示尺寸缩图都靠它
        if let Some(kitty) = self.emulator.kitty() {
            let scale = window.scale_factor();
            kitty.set_cell_pixels(f(cell_width) * scale, f(line_height) * scale);
        }
        // 主字体缺的私用区图标换哪份字体画;`None` = 这一帧不接管(见 [`symbols`])。
        let symbol_font = symbols::symbol_font(&self.style, window);

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
        //    引擎有去抖 + 变化检测两道闸,这一句在空闲帧几乎是零成本。
        let highlights: Option<Rc<SearchHighlights>> = self.search.as_ref().map(|search| {
            let mut engine = search.borrow_mut();
            let frame = engine.frame_sync(&self.emulator, Instant::now());
            // 结果集这一帧变了:高亮本帧就画对了,但查找条的「n/总数」在同一帧的
            // render 阶段已经读过(扫描之前的数)。再要一帧 —— notify 的是本视图,
            // 沿 view 路径标脏到宿主 pane,查找条(pane 的子视图)随之重画。
            // pane 套着 view 级缓存,不要这一帧的话窗口别处的重绘碰不到它,
            // 输出一停计数就停在旧值,直到滚一下滚轮
            if frame.repaint {
                window.request_animation_frame();
            }
            // 这一帧因为没到去抖点被挡下了:排一发延后重绘兜底。输出恰好停在去抖
            // 窗口里时,没有它就要等别的事碰巧触发一帧,命中与计数才会跟上
            if let Some(delay) = frame.rescan_after {
                schedule_search_rescan(search.clone(), delay, window.current_view(), window, cx);
            }
            engine.highlights()
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
        // 接不接管图标同样只改 shaping 不改格子:启动时字体晚一步注册上,
        // 已缓存的那几行也得跟着重建,别一直画着方块
        key = key.push(symbol_font.is_some());
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
        let images;

        {
            let term_lock = self.emulator.term().lock();
            // 与这一帧的格子同一把锁:图片跟着格子走,不能错开
            images = self
                .emulator
                .kitty()
                .map_or_else(Vec::new, |kitty| kitty.visible_images(&term_lock));
            total_lines = term_lock.grid().total_lines();
            let content = term_lock.renderable_content();
            let display_offset = content.display_offset;
            frame_display_offset = display_offset;
            mode = content.mode;
            let colors_table = content.colors;
            let selection_range = content.selection;
            let cursor_point = content.cursor.point;
            let cursor_shape = content.cursor.shape;

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

                // 逐格按 alacritty 的区间判(块选按列区间);宽字符跨在边上时整字一起亮,
                // 与复制出来的内容对得上(见 [`cell_in_selection`])
                let selected = selection_range
                    .as_ref()
                    .is_some_and(|r| cell_in_selection(r, indexed.point, flags));
                let is_cursor =
                    indexed.point == cursor_point && cursor_shape != CursorShape::Hidden;
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
                    symbol_font.as_ref(),
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

        // ── 光标格。上面那个 `cursor`(CursorLayout)是「要画的光标」,可能没有,而且
        //    这一帧可能正赶上 TUI 重绘途中;这个是「光标停稳的那一格」,永远有
        //    —— **不受 `CursorShape::Hidden` 影响**,IME 拿它当锚点。
        let ime_cursor = self.emulator.settled_cursor();
        let cursor_cell = cursor_cell_bounds(
            bounds.origin,
            cell_size,
            columns,
            screen_lines,
            ime_cursor.line,
            ime_cursor.column,
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
            let geometry = FrameGeometry {
                origin: bounds.origin,
                cell_size,
                columns,
                screen_lines,
                display_offset: frame_display_offset,
                cursor: Some(cursor_cell),
                composing: preedit.is_some(),
            };
            if ime_anchor_needs_sync(&sink.replace(geometry), &geometry) {
                window.invalidate_character_coordinates();
            }
            // 光标刚挪、还没停稳。不再要一帧的话,输出一停这一帧就是最后一帧,
            // 预编辑串和候选窗都会留在旧位置上
            if geometry.composing && ime_cursor.pending {
                window.request_animation_frame();
            }
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
            display_offset: frame_display_offset,
            mode,
            rows,
            cursor,
            preedit,
            scrollbar,
            flash,
            images,
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
        // 链接的悬停态:这一帧现算(理由见 `super::links` 模块注释)。
        // 不进行签名、不碰 RowCache —— 下划线是叠在格子上的一层浮标,与整行闪烁同一口径
        let hovered_link = self.hovered_link(prepared, window, cx);
        // 不按修饰键的悬停提示:Ctrl 那条在画的时候让位(两条下划线不叠)
        let hinted_link = if hovered_link.is_none() {
            self.hinted_link(prepared, window, cx)
        } else {
            None
        };
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
                for piece in render.separators.iter() {
                    symbols::paint_separator(
                        window,
                        piece.separator,
                        Bounds::new(piece.origin + delta, prepared.cell_size),
                        piece.color,
                        piece.stroke,
                    );
                }
                for piece in render.texts.iter() {
                    _ = piece.line.paint(
                        piece.origin + delta,
                        prepared.cell_size.height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
            }
            // ── Kitty 图片压在文字之上(协议的缺省 z 序),细光标与浮层之下。
            //    **不进行缓存**:每帧照当前摆放重发,摆放变了不用作废任何一行
            if let (Some(kitty), Some(cache)) = (self.emulator.kitty(), self.image_cache.as_ref()) {
                let layer = super::images::ImageLayer {
                    origin,
                    cell: prepared.cell_size,
                    display_offset: prepared.display_offset,
                    clip: bounds,
                };
                super::images::paint_images(kitty, &prepared.images, &layer, cache, window, cx);
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

            // ── 链接下划线压在文字之上、预编辑浮层之下(组合串要盖住它底下的一切)
            if let Some(link) = hovered_link.as_ref() {
                self.paint_link_underline(prepared, link, self.theme.foreground, window);
                if let Some(target) = link.revealed_target() {
                    self.paint_link_bubble(prepared, link, Some(target), None, bounds, window, cx);
                }
            } else if let Some(link) = hinted_link.as_ref() {
                // 弱化的下划线 +「Ctrl+单击打开」;OSC 8 的真实目标并进同一个气泡
                let weak = Hsla {
                    a: self.theme.foreground.a * LINK_HINT_UNDERLINE_ALPHA,
                    ..self.theme.foreground
                };
                self.paint_link_underline(prepared, link, weak, window);
                let label = self.link_hint.as_ref().map(|label| label());
                self.paint_link_bubble(
                    prepared,
                    link,
                    link.revealed_target(),
                    label.as_deref(),
                    bounds,
                    window,
                    cx,
                );
            }

            // ── IME 预编辑浮层:盖住底下的 grid 内容再画,否则组合串会与残留字符叠糊
            if let Some(p) = prepared.preedit.as_ref() {
                let height = prepared.cell_size.height;
                window.paint_quad(fill(
                    Bounds::new(p.origin, size(p.width, height)),
                    self.theme.background,
                ));
                _ = p
                    .line
                    .paint(p.origin, height, gpui::TextAlign::Left, None, window, cx);
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

        if hovered_link.is_some() {
            window.set_cursor_style(CursorStyle::PointingHand, &prepared.hitbox);
        }

        self.paint_mouse_listeners(prepared, window, cx);
    }
}

#[cfg(test)]
mod tests;
