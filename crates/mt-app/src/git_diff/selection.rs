//! diff 正文的鼠标选中与复制(对标 IDEA diff 查看器的选择体验)。
//!
//! # 选区记模型坐标
//!
//! 行是 `uniform_list` 虚拟化的:滚出视野的行当帧就不再有元素,选区要是记在元素里,
//! 滚出去再滚回来就没了。所以选区记成 [`TextPoint`] —— **视图行下标**(`inline_rows` /
//! `pair_rows` 的下标)+ 该行**原始文本**(tab 未展开)里的字节偏移;每帧画行时由
//! [`DiffState::row_selection`] 现算这一行落在选区里的那一段。
//!
//! # 命中测试
//!
//! 像素 → 字符按该行文字**实际的排版结果**换算,不按等宽格宽硬算(CJK 宽字符、回落字体
//! 的字宽都不是一格):正文那段文字是 [`RecordedText`],prepaint 之后把 `TextLayout`
//! 连同当时的滚动偏移记进 [`LayoutSink`],鼠标事件拿它 `closest_index_for_x`。行号
//! 由纵坐标按恒定行高算([`row_at`],uniform_list 的前提就是等高)。
//!
//! # tab 展开
//!
//! 屏幕上的是展开后的文本(见 `tab_expansion`),复制必须给原文(含真 tab):命中测试拿到
//! 的显示偏移经 [`display_to_raw`] 换回原文偏移再记;画底色时反过来走 [`raw_to_display`]。
//!
//! # 范围口径
//!
//! - 并排视图的选区限定在**起拖的那一栏**([`Pane`]),复制只给那一侧,对侧是空占位的行跳过;
//! - 行内视图复制的是行内容本身,不带行号、不带 `+` / `-`(它们本来就不在正文里);
//! - hunk 头、行号列不可选:按下去不起选区、不弹菜单,块操作按钮照常点;拖选途经
//!   hunk 头时它只占一个行位,复制时跳过。
//!
//! # 底色叠放
//!
//! 选区底色并进 `StyledText` 的高亮(与词级高亮合成一份,见 [`paint_segments`]):高亮底色
//! 画在字形**之下**、行底色**之上**,选区叠在增删底色与词级高亮上,字照样压在最上面。
//! 跨行选区里行尾的换行在正文末尾补一个涂了底色的空格(空行被选中也看得出来;补在同一段
//! 文字里,另起元素会因小数宽度漏出 1px 缝)。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    App, Bounds, ClipboardItem, Context, Div, Element, ElementId, Entity, GlobalElementId,
    HighlightStyle, Hsla, InspectorElementId, InteractiveElement, IntoElement, LayoutId,
    LineLayout, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Rgba,
    ScrollHandle, SharedString, Styled, StyledText, Task, TextLayout, Window, canvas, point, px,
};
use gpui_component::ActiveTheme as _;

use super::{
    CharClass, DiffState, Flat, GUTTER, GitDiffCopy, GitDiffSelectAll, INLINE_GUTTER, InlineRow,
    PairRow, ViewMode,
};
use crate::i18n::t;
use crate::menu::{self, MenuItem};
use crate::tab_expansion::TAB_WIDTH;

/// 拖选期间自动滚动的节拍。
const DRAG_TICK: Duration = Duration::from_millis(16);

/// 自动滚动每拍的像素数:指针刚越出视口时取下限,越出 [`SCROLL_RAMP`] 及以上取上限。
const SCROLL_MIN: f32 = 6.0;
const SCROLL_MAX: f32 = 60.0;
const SCROLL_RAMP: f32 = 120.0;

/// 选区底色透明度的下限,见 [`selection_bg`]。
const SELECTION_MIN_ALPHA: f32 = 0.35;

// ─── 模型 ─────────────────────────────────────────────────────

/// 选区所在的栏。行内视图只有一栏;并排视图左旧右新,选区**不跨栏**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Pane {
    Inline,
    Old,
    New,
}

impl Pane {
    /// 钉在左缘的行号列(不可选)有多宽。
    fn gutter(self) -> f32 {
        match self {
            Pane::Inline => INLINE_GUTTER,
            Pane::Old | Pane::New => GUTTER,
        }
    }
}

impl Flat {
    /// 某栏的行数(行内 = `inline_rows`,并排两栏 = `pair_rows`)。
    fn row_count(&self, pane: Pane) -> usize {
        match pane {
            Pane::Inline => self.inline_rows.len(),
            Pane::Old | Pane::New => self.pair_rows.len(),
        }
    }

    /// 某栏第 `row` 行对应的拍平行([`Flat::lines`] 的下标)。hunk 头、并排视图里对侧
    /// 占位的空格子都是 `None`。
    fn line_at(&self, pane: Pane, row: usize) -> Option<usize> {
        match pane {
            Pane::Inline => match self.inline_rows.get(row)? {
                InlineRow::Line(ix) => Some(*ix),
                InlineRow::Head(_) => None,
            },
            Pane::Old => match self.pair_rows.get(row)? {
                PairRow::Line(left, _) => *left,
                PairRow::Head(_) => None,
            },
            Pane::New => match self.pair_rows.get(row)? {
                PairRow::Line(_, right) => *right,
                PairRow::Head(_) => None,
            },
        }
    }

    fn is_head(&self, pane: Pane, row: usize) -> bool {
        match pane {
            Pane::Inline => matches!(self.inline_rows.get(row), Some(InlineRow::Head(_))),
            Pane::Old | Pane::New => matches!(self.pair_rows.get(row), Some(PairRow::Head(_))),
        }
    }

    /// 拍平行的**原始**文本(tab 未展开):复制给的就是它。
    fn raw_line(&self, ix: usize) -> &str {
        self.raw[ix].as_deref().unwrap_or(&self.lines[ix].content)
    }

    /// 某栏某行的原始文本。hunk 头 / 空占位是 `None`。
    fn row_text(&self, pane: Pane, row: usize) -> Option<&str> {
        self.line_at(pane, row).map(|ix| self.raw_line(ix))
    }
}

/// 选区里的一个点:视图行下标 + 该行原始文本里的字节偏移(字符边界)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(super) struct TextPoint {
    row: usize,
    offset: usize,
}

/// 一段选区。`anchor` 是起拖点,`head` 跟着指针走;两者先后不定,用时经 [`Self::ordered`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TextSelection {
    pane: Pane,
    anchor: TextPoint,
    head: TextPoint,
}

impl TextSelection {
    fn ordered(&self) -> (TextPoint, TextPoint) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// 第 `row` 行(该栏原文长 `len` 字节)落在选区里的那一段(原文字节区间),以及这一行
    /// 的行尾换行在不在选区里。不是这一栏 / 选区为空 / 这一行不在选区里 → `None`。
    fn row_span(&self, pane: Pane, row: usize, len: usize) -> Option<(Range<usize>, bool)> {
        if self.pane != pane || self.is_empty() {
            return None;
        }
        let (start, end) = self.ordered();
        if row < start.row || row > end.row {
            return None;
        }
        let from = if row == start.row {
            start.offset.min(len)
        } else {
            0
        };
        let to = if row == end.row {
            end.offset.min(len)
        } else {
            len
        };
        let newline = row < end.row;
        (from < to || newline).then_some((from..to.max(from), newline))
    }
}

/// 选区里的文本:逐行取原文,行间补 `\n`。hunk 头、对侧空占位的行整行跳过(连换行一起)。
fn selected_text(flat: &Flat, sel: &TextSelection) -> String {
    let count = flat.row_count(sel.pane);
    if count == 0 {
        return String::new();
    }
    let (start, end) = sel.ordered();
    let mut parts: Vec<&str> = Vec::new();
    for row in start.row..=end.row.min(count - 1) {
        let Some(text) = flat.row_text(sel.pane, row) else {
            continue;
        };
        let from = if row == start.row {
            floor_boundary(text, start.offset)
        } else {
            0
        };
        let to = if row == end.row {
            floor_boundary(text, end.offset)
        } else {
            text.len()
        };
        parts.push(&text[from..to.max(from)]);
    }
    parts.join("\n")
}

/// 整栏选中:第一行行首到最后一行行尾。该栏一行都没有 → `None`。
fn select_all_in(flat: &Flat, pane: Pane) -> Option<TextSelection> {
    let last = flat.row_count(pane).checked_sub(1)?;
    let end = flat.row_text(pane, last).map_or(0, str::len);
    Some(TextSelection {
        pane,
        anchor: TextPoint::default(),
        head: TextPoint {
            row: last,
            offset: end,
        },
    })
}

/// 不越过 `ix` 的最近字符边界。
fn floor_boundary(text: &str, ix: usize) -> usize {
    let mut ix = ix.min(text.len());
    while !text.is_char_boundary(ix) {
        ix -= 1;
    }
    ix
}

/// 双击选词的字符分类:Unicode 字母数字(含中日韩文字)与 `_` 连成一个词,空白连成一段,
/// 其余符号一个字符一段。
///
/// ⚠️ 与词级 diff 的 `char_class` 有意不同:那边中文逐字切(改一个字只点出那一个字),
/// 这边双击一句中文注释要整段选中才顺手。
fn word_class(c: char) -> CharClass {
    if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else if c.is_whitespace() {
        CharClass::Space
    } else {
        CharClass::Other
    }
}

/// 双击落在原文偏移 `at`(指针正下方那个字符的起点)时选中的那一段。行尾之外按最后一个
/// 字符算;空行是空区间。
fn word_at(text: &str, at: usize) -> Range<usize> {
    let mut at = floor_boundary(text, at);
    if at == text.len() {
        match text[..at].char_indices().next_back() {
            Some((ix, _)) => at = ix,
            None => return 0..0,
        }
    }
    let Some(ch) = text[at..].chars().next() else {
        return at..at;
    };
    let class = word_class(ch);
    if class == CharClass::Other {
        return at..at + ch.len_utf8();
    }
    let start = text[..at]
        .char_indices()
        .rev()
        .take_while(|(_, c)| word_class(*c) == class)
        .last()
        .map_or(at, |(ix, _)| ix);
    let end = text[at..]
        .char_indices()
        .find(|(_, c)| word_class(*c) != class)
        .map_or(text.len(), |(ix, _)| at + ix);
    start..end
}

/// 双击 / 三击后接着拖:选区 = 起拖那一段(词 / 行)∪ 指针所在那一段,返回 `(anchor, head)`。
/// 往前拖时 anchor 落在起拖段的尾、往后拖落在头,起拖段本身始终在选区里。
fn extend_by_unit(
    origin: (TextPoint, TextPoint),
    hit: (TextPoint, TextPoint),
) -> (TextPoint, TextPoint) {
    if hit.0 < origin.0 {
        (origin.1, hit.0)
    } else if hit.1 > origin.1 {
        (origin.0, hit.1)
    } else {
        origin
    }
}

// ─── tab 展开的偏移映射 ───────────────────────────────────────
//
// 两个方向都按 `expand_line` 的同一口径走制表位:列数按字符数算,`\t` 推到下一个
// [`TAB_WIDTH`] 的倍数,其余字符原样(字节长不变)。

/// 显示文本(tab 已展开)里的字节偏移 → 原文里的字节偏移。
///
/// 落在某个 tab 展开出的那几格空白中间时:`nearest = true` 取较近的一侧(光标落点),
/// `false` 取 tab 自己(「指针下面是哪个字符」)。
fn display_to_raw(raw: &str, display: usize, nearest: bool) -> usize {
    if !raw.contains('\t') {
        return floor_boundary(raw, display);
    }
    let (mut col, mut pos) = (0usize, 0usize);
    for (ix, c) in raw.char_indices() {
        let width = if c == '\t' {
            TAB_WIDTH - col % TAB_WIDTH
        } else {
            c.len_utf8()
        };
        if display < pos + width {
            if c != '\t' {
                return ix;
            }
            let into = display - pos;
            return if nearest && into * 2 >= width {
                ix + 1
            } else {
                ix
            };
        }
        pos += width;
        col += if c == '\t' { width } else { 1 };
    }
    raw.len()
}

/// 原文里的字节偏移 → 显示文本里的字节偏移(画选区底色用)。
fn raw_to_display(raw: &str, offset: usize) -> usize {
    if !raw.contains('\t') {
        return offset.min(raw.len());
    }
    let (mut col, mut pos) = (0usize, 0usize);
    for (ix, c) in raw.char_indices() {
        if ix >= offset {
            return pos;
        }
        if c == '\t' {
            let n = TAB_WIDTH - col % TAB_WIDTH;
            pos += n;
            col += n;
        } else {
            pos += c.len_utf8();
            col += 1;
        }
    }
    pos
}

// ─── 底色 ─────────────────────────────────────────────────────

/// 一段高亮涂什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Paint {
    /// 只有词级高亮
    Word,
    /// 只有选区
    Selected,
    /// 两者叠在一起
    Both,
}

/// 词级高亮片段与选区(都是显示文本的字节区间)切成互不重叠、按序排列的若干段 ——
/// `StyledText::with_highlights` 要的就是这种形状。没被涂的部分不出段。
fn paint_segments(
    marks: &[Range<usize>],
    selected: Option<&Range<usize>>,
) -> Vec<(Range<usize>, Paint)> {
    let selected = selected.filter(|r| r.start < r.end);
    let mut cuts: Vec<usize> = marks
        .iter()
        .chain(selected)
        .flat_map(|r| [r.start, r.end])
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out: Vec<(Range<usize>, Paint)> = Vec::new();
    for pair in cuts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let covers = |r: &Range<usize>| r.start <= a && b <= r.end;
        let paint = match (marks.iter().any(covers), selected.is_some_and(covers)) {
            (true, true) => Paint::Both,
            (true, false) => Paint::Word,
            (false, true) => Paint::Selected,
            (false, false) => continue,
        };
        match out.last_mut() {
            Some((range, last)) if *last == paint && range.end == a => range.end = b,
            _ => out.push((a..b, paint)),
        }
    }
    out
}

/// `top` 叠在 `bottom` 上的颜色(标准 source-over)。词级高亮与选区都是半透明的,
/// 合成一份再交给高亮,行底色上的叠放效果与分两层画一致。
fn over(top: Hsla, bottom: Hsla) -> Hsla {
    let (t, b) = (Rgba::from(top), Rgba::from(bottom));
    let a = t.a + b.a * (1.0 - t.a);
    if a <= 0.0 {
        return Hsla::default();
    }
    let mix = |ct: f32, cb: f32| (ct * t.a + cb * b.a * (1.0 - t.a)) / a;
    Rgba {
        r: mix(t.r, b.r),
        g: mix(t.g, b.g),
        b: mix(t.b, b.b),
        a,
    }
    .into()
}

/// 选区底色:主题里的文本选中色(输入框 / 文件查看器用的同一个 `theme.selection`,
/// 即 accent 的 22% 一档),透明度至少 [`SELECTION_MIN_ALPHA`] —— diff 行本身就铺着
/// 红 / 绿底色和更实一档的词级高亮,22% 叠上去在浅色主题里几乎看不出选中了哪段。
pub(super) fn selection_bg(cx: &App) -> Hsla {
    let color = cx.theme().selection;
    Hsla {
        a: color.a.max(SELECTION_MIN_ALPHA),
        ..color
    }
}

// ─── 指针几何 ─────────────────────────────────────────────────

/// 列表内容坐标里的纵坐标(视口顶 + 滚动偏移已减掉)→ 第几行。落在最后一行之下
/// (列表比视口短)是 `None`;负值算第 0 行。
fn row_at(y: f32, item_h: f32, count: usize) -> Option<usize> {
    if count == 0 || item_h <= 0.0 {
        return None;
    }
    let row = (y.max(0.0) / item_h).floor() as usize;
    (row < count).then_some(row)
}

/// 光标落点:离 `x` 最近的字符边界(显示文本的字节偏移)。
///
/// gpui 的 `closest_index_for_x` 指针一越过最后一个字形的起点就直接给行尾,不比中点 ——
/// 拖到末字的左半边也会把它选进来。这里补上末字的中点判断。
fn closest_boundary(line: &LineLayout, x: Pixels) -> usize {
    let last = line
        .runs
        .iter()
        .flat_map(|run| run.glyphs.iter())
        .last()
        .map(|glyph| (glyph.index, f32::from(glyph.position.x)));
    settle_last_glyph(
        line.closest_index_for_x(x),
        line.len,
        last,
        f32::from(line.width),
        f32::from(x),
    )
}

/// [`closest_boundary`] 的纯算部分:`closest` 是 gpui 给的落点,`last` 是最后一个字形的
/// (起点偏移, 起点横坐标)。落点给到行尾、而指针还没过末字中点时,改落在末字之前。
fn settle_last_glyph(
    closest: usize,
    len: usize,
    last: Option<(usize, f32)>,
    width: f32,
    x: f32,
) -> usize {
    match last {
        Some((index, start)) if closest == len && x < (start + width) / 2.0 => index,
        _ => closest,
    }
}

/// 指针越出 `[start, end]` 多远 → 自动滚动这一拍滚多少像素(带符号:负 = 往开头滚)。
/// 区间内是 0。
fn edge_speed(pos: f32, start: f32, end: f32) -> f32 {
    let over = if pos < start {
        pos - start
    } else if pos > end {
        pos - end
    } else {
        return 0.0;
    };
    let t = (over.abs() / SCROLL_RAMP).min(1.0);
    (SCROLL_MIN + t * (SCROLL_MAX - SCROLL_MIN)).copysign(over)
}

// ─── 排版登记 ─────────────────────────────────────────────────

/// 一行正文的排版结果,连同它 prepaint 时所处的滚动偏移(之后滚过了就按差值平移)。
pub(super) struct RowLayout {
    layout: TextLayout,
    scroll: Point<Pixels>,
}

/// `(栏, 视图行)` → 这一行上一帧的排版结果。只留视野里的行(见
/// [`DiffState::retain_layouts`]),换一份 diff 时清空。
pub(super) type LayoutSink = Rc<RefCell<HashMap<(Pane, usize), RowLayout>>>;

/// 正文那段文字:画法与 `StyledText` 完全一样,只多做一件事 —— prepaint 之后把
/// 排版结果登记进 [`LayoutSink`]。只登记真正 prepaint 过的(uniform_list 量宽时那次
/// 只 layout 不 prepaint,不会混进来)。
pub(super) struct RecordedText {
    text: StyledText,
    sink: LayoutSink,
    key: (Pane, usize),
    scroll: Point<Pixels>,
}

impl IntoElement for RecordedText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RecordedText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.text.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text
            .prepaint(id, inspector_id, bounds, request_layout, window, cx);
        self.sink.borrow_mut().insert(
            self.key,
            RowLayout {
                layout: self.text.layout().clone(),
                scroll: self.scroll,
            },
        );
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut (),
        prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text.paint(
            id,
            inspector_id,
            bounds,
            request_layout,
            prepaint,
            window,
            cx,
        );
    }
}

/// 一行正文画选区、登记排版要的东西(视图的行回调逐行给,见 [`DiffState::row_text`])。
pub(super) struct RowText {
    key: (Pane, usize),
    /// 选中的那一段(**显示文本**的字节区间)+ 行尾换行在不在选区里
    selected: Option<(Range<usize>, bool)>,
    selection_bg: Hsla,
    sink: LayoutSink,
}

impl RowText {
    /// 行尾换行在选区里:正文末尾补一格选区底色(空行被选中也看得出来)。
    fn newline_mark(&self) -> Option<Hsla> {
        self.selected
            .as_ref()
            .is_some_and(|(_, newline)| *newline)
            .then_some(self.selection_bg)
    }
}

/// 正文那段文字:词级高亮(`marks`,底色 `word_bg`)与选区合成一份高亮,外面包一层
/// [`RecordedText`] 登记排版。
pub(super) fn line_text(
    content: SharedString,
    marks: &[Range<usize>],
    word_bg: Hsla,
    row: &RowText,
    scroll: Point<Pixels>,
) -> RecordedText {
    let selected = row.selected.as_ref().map(|(range, _)| range);
    let paint_bg = |bg: Hsla| HighlightStyle {
        background_color: Some(bg),
        ..Default::default()
    };
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = paint_segments(marks, selected)
        .into_iter()
        .map(|(range, paint)| {
            let bg = match paint {
                Paint::Word => word_bg,
                Paint::Selected => row.selection_bg,
                Paint::Both => over(row.selection_bg, word_bg),
            };
            (range, paint_bg(bg))
        })
        .collect();
    // 行尾换行也在选区里:正文末尾补一个空格涂上选区底色(空行被选中也看得出来)。补在
    // 同一段文字里而不是另起一个元素 —— 正文宽度是小数,另起的元素按整像素排,两块底色
    // 之间会漏出 1px 缝。多出来的这一格命中时由 `display_to_raw` 钳回行尾
    let content = match row.newline_mark() {
        Some(bg) => {
            let end = content.len();
            highlights.push((end..end + 1, paint_bg(bg)));
            SharedString::from(format!("{content} "))
        }
        None => content,
    };
    RecordedText {
        text: StyledText::new(content).with_highlights(highlights),
        sink: row.sink.clone(),
        key: row.key,
        scroll,
    }
}

// ─── 交互状态 ─────────────────────────────────────────────────

/// 拖选的延伸粒度:单击按字符,双击按词,三击按行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragUnit {
    Char,
    Word,
    Line,
}

#[derive(Debug, Clone, Copy)]
struct DragState {
    pane: Pane,
    unit: DragUnit,
    /// 起拖时的那一段:单击 = 一个点,双击 = 那个词,三击 = 那一行
    origin: (TextPoint, TextPoint),
    /// 最近一次的指针位置(自动滚动的节拍里拿它重算落点)
    pointer: Point<Pixels>,
}

/// 一次命中:光标落点(最近的字符边界)与指针正下方那个字符的起点,都是原文偏移。
#[derive(Debug, Clone, Copy)]
struct Hit {
    row: usize,
    caret: usize,
    under: usize,
}

/// 选区相关的全部状态,整块挂在 [`DiffState`] 上。
pub(super) struct SelectState {
    selection: Option<TextSelection>,
    drag: Option<DragState>,
    /// 拖选期间的自动滚动节拍。拖选结束就丢掉(= 取消任务)
    ticker: Option<Task<()>>,
    /// 并排视图里最近一次点击 / 选择所在的栏(Ctrl+A 选哪一栏),缺省新侧
    last_side: Pane,
    layouts: LayoutSink,
}

impl Default for SelectState {
    fn default() -> Self {
        Self {
            selection: None,
            drag: None,
            ticker: None,
            last_side: Pane::New,
            layouts: LayoutSink::default(),
        }
    }
}

impl DiffState {
    /// 某栏列表的滚动句柄:视口位置、滚动偏移、可滚范围都从它读。
    fn pane_scroll(&self, pane: Pane) -> ScrollHandle {
        let handle = match pane {
            Pane::Inline => &self.inline_scroll,
            Pane::Old => &self.left_scroll,
            Pane::New => &self.right_scroll,
        };
        handle.0.borrow().base_handle.clone()
    }

    /// 指针 → 选区里的点。
    ///
    /// `clamp = false`(按下):落在视口外、行号列、hunk 头上都不算(`None`);落在最后一行
    /// 之下算最后一行的行尾。`clamp = true`(拖动):先把指针钳回正文可见区,hunk 头也算
    /// (只占一个行位)。
    fn hit(&self, pane: Pane, pos: Point<Pixels>, clamp: bool) -> Option<Hit> {
        let count = self.flat.row_count(pane);
        if count == 0 {
            return None;
        }
        let scroll = self.pane_scroll(pane);
        let view = scroll.bounds();
        let (left, right) = (f32::from(view.left()), f32::from(view.right()));
        let (top, bottom) = (f32::from(view.top()), f32::from(view.bottom()));
        if right <= left || bottom <= top {
            return None;
        }
        let text_left = left + pane.gutter();
        let (mut x, mut y) = (f32::from(pos.x), f32::from(pos.y));
        if clamp {
            x = x.clamp(text_left.min(right), right);
            y = y.clamp(top, (bottom - 1.0).max(top));
        } else if x < text_left || x >= right || y < top || y >= bottom {
            return None;
        }
        let offset = scroll.offset();
        let in_content = y - top - f32::from(offset.y);
        let (row, past_end) = match row_at(in_content, self.line_height(), count) {
            Some(row) => (row, false),
            None => (count - 1, true),
        };
        if !clamp && !past_end && self.flat.is_head(pane, row) {
            return None;
        }
        let Some(raw) = self.flat.row_text(pane, row) else {
            return Some(Hit {
                row,
                caret: 0,
                under: 0,
            });
        };
        let end = Hit {
            row,
            caret: raw.len(),
            under: raw.len(),
        };
        if past_end {
            return Some(end);
        }
        let layouts = self.select.layouts.borrow();
        let line = layouts.get(&(pane, row)).and_then(|entry| {
            // 上一帧 prepaint 之后又滚过(自动滚动的节拍):按滚动差值把原点挪到现在的位置
            let origin = entry.layout.bounds().left() + (offset.x - entry.scroll.x);
            let line = entry.layout.line_layout_for_index(0)?;
            Some((line, px(x) - origin))
        });
        let Some((line, rel)) = line else {
            // 这一行上一帧没画(自动滚动刚把它滚进来):粗取行首 / 行尾,下一拍就有排版了
            return Some(if x >= right {
                end
            } else {
                Hit {
                    row,
                    caret: 0,
                    under: 0,
                }
            });
        };
        let line = &line.unwrapped_layout;
        let caret = closest_boundary(line, rel);
        let under = line.index_for_x(rel).unwrap_or(line.len);
        Some(Hit {
            row,
            caret: display_to_raw(raw, caret, true),
            under: display_to_raw(raw, under, false),
        })
    }

    fn word_span(&self, pane: Pane, row: usize, at: usize) -> (TextPoint, TextPoint) {
        let word = word_at(self.flat.row_text(pane, row).unwrap_or(""), at);
        (
            TextPoint {
                row,
                offset: word.start,
            },
            TextPoint {
                row,
                offset: word.end,
            },
        )
    }

    fn line_span(&self, pane: Pane, row: usize) -> (TextPoint, TextPoint) {
        let len = self.flat.row_text(pane, row).map_or(0, str::len);
        (TextPoint { row, offset: 0 }, TextPoint { row, offset: len })
    }

    /// 左键按下:起一段选区并进入拖选。没落在正文上(行号列 / hunk 头 / 视口外)返回 `false`。
    fn begin_selection(
        &mut self,
        pane: Pane,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(hit) = self.hit(pane, event.position, false) else {
            return false;
        };
        if pane != Pane::Inline {
            self.select.last_side = pane;
        }
        let at = TextPoint {
            row: hit.row,
            offset: hit.caret,
        };
        let (unit, origin) = match event.click_count {
            0 | 1 => (DragUnit::Char, (at, at)),
            2 => (DragUnit::Word, self.word_span(pane, hit.row, hit.under)),
            _ => (DragUnit::Line, self.line_span(pane, hit.row)),
        };
        // Shift+单击:同一栏里已有选区就把它延伸到这里,起拖点不变
        let extend = self
            .select
            .selection
            .filter(|prev| unit == DragUnit::Char && event.modifiers.shift && prev.pane == pane);
        let (selection, origin) = match extend {
            Some(prev) => (
                TextSelection {
                    pane,
                    anchor: prev.anchor,
                    head: at,
                },
                (prev.anchor, prev.anchor),
            ),
            None => (
                TextSelection {
                    pane,
                    anchor: origin.0,
                    head: origin.1,
                },
                origin,
            ),
        };
        self.select.selection = Some(selection);
        self.select.drag = Some(DragState {
            pane,
            unit,
            origin,
            pointer: event.position,
        });
        self.select.ticker = Some(drag_ticker(cx));
        cx.notify();
        true
    }

    /// 拖选中指针动了。左键已经不在按着(抬起丢在了窗口外)就收尾。
    fn drag_to(&mut self, pos: Point<Pixels>, pressed: bool, cx: &mut Context<Self>) {
        if self.select.drag.is_none() {
            return;
        }
        if !pressed {
            self.end_drag();
            return;
        }
        if let Some(drag) = self.select.drag.as_mut() {
            drag.pointer = pos;
        }
        self.extend_drag(cx);
    }

    /// 按最近一次的指针位置重算选区的 head。
    fn extend_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.select.drag else {
            return;
        };
        let Some(hit) = self.hit(drag.pane, drag.pointer, true) else {
            return;
        };
        let at = TextPoint {
            row: hit.row,
            offset: hit.caret,
        };
        let (anchor, head) = match drag.unit {
            DragUnit::Char => (drag.origin.0, at),
            DragUnit::Word => {
                extend_by_unit(drag.origin, self.word_span(drag.pane, hit.row, hit.under))
            }
            DragUnit::Line => extend_by_unit(drag.origin, self.line_span(drag.pane, hit.row)),
        };
        let next = TextSelection {
            pane: drag.pane,
            anchor,
            head,
        };
        if self.select.selection != Some(next) {
            self.select.selection = Some(next);
            cx.notify();
        }
    }

    /// 自动滚动的一拍:指针在正文可见区外就按越界距离滚,再把选区跟过去。
    /// 返回 `false` = 拖选已结束,节拍任务收工。
    fn drag_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(drag) = self.select.drag else {
            return false;
        };
        let scroll = self.pane_scroll(drag.pane);
        let view = scroll.bounds();
        let dx = edge_speed(
            f32::from(drag.pointer.x),
            f32::from(view.left()) + drag.pane.gutter(),
            f32::from(view.right()),
        );
        let dy = edge_speed(
            f32::from(drag.pointer.y),
            f32::from(view.top()),
            f32::from(view.bottom()),
        );
        if dx != 0.0 || dy != 0.0 {
            let (offset, max) = (scroll.offset(), scroll.max_offset());
            let next = point(
                px((f32::from(offset.x) - dx).clamp(-f32::from(max.x).max(0.0), 0.0)),
                px((f32::from(offset.y) - dy).clamp(-f32::from(max.y).max(0.0), 0.0)),
            );
            if next != offset {
                // 并排视图的另一栏由下一帧的 `sync_columns` 跟上
                scroll.set_offset(next);
                cx.notify();
            }
        }
        // 滚轮在拖选途中也会改偏移:每拍都按现在的位置重算一次(没变就不重画)
        self.extend_drag(cx);
        true
    }

    /// 拖选结束(选区保留)。丢掉节拍任务 = 取消。
    fn end_drag(&mut self) {
        self.select.drag = None;
        self.select.ticker = None;
    }

    /// 清空选区。换文件、重取 diff(含块操作后原地刷新)、切换并排 / 行内时调;
    /// 关弹窗时整个状态随弹窗释放。
    pub(super) fn clear_selection(&mut self) {
        self.end_drag();
        self.select.selection = None;
        self.select.layouts.borrow_mut().clear();
    }

    /// 选区里的文本。没有选区、或只框住了 hunk 头 / 空占位 → `None`。
    fn copy_text(&self) -> Option<String> {
        let sel = self.select.selection.filter(|sel| !sel.is_empty())?;
        let text = selected_text(&self.flat, &sel);
        (!text.is_empty()).then_some(text)
    }

    /// 全选一栏。`None` = 当前栏:行内视图就是那一栏,并排视图取最近一次点击 / 选择
    /// 所在的栏(缺省新侧)。没有可选的行返回 `false`。
    fn select_all(&mut self, pane: Option<Pane>) -> bool {
        let pane = pane.unwrap_or(match self.view {
            ViewMode::Inline => Pane::Inline,
            ViewMode::SideBySide => self.select.last_side,
        });
        let Some(selection) = select_all_in(&self.flat, pane) else {
            return false;
        };
        if pane != Pane::Inline {
            self.select.last_side = pane;
        }
        self.end_drag();
        self.select.selection = Some(selection);
        true
    }

    /// 视图的行回调开头调:只留这一栏视野里(`rows`)的排版登记,滚出去的扔掉。
    ///
    /// uniform_list 量宽时也会用单独一行的区间调一次回调,那次之后紧跟着的正式回调与
    /// prepaint 会把视野里的行重新登记上,鼠标事件拿到的总是整屏。
    pub(super) fn retain_layouts(&self, pane: Pane, rows: &Range<usize>) {
        self.select
            .layouts
            .borrow_mut()
            .retain(|(p, row), _| *p != pane || rows.contains(row));
    }

    /// 第 `row` 行(拍平行 `ix`)画正文要的选区与登记上下文。
    pub(super) fn row_text(&self, pane: Pane, row: usize, ix: usize, bg: Hsla) -> RowText {
        let raw = self.flat.raw_line(ix);
        let selected = self
            .select
            .selection
            .and_then(|sel| sel.row_span(pane, row, raw.len()))
            .map(|(range, newline)| {
                (
                    raw_to_display(raw, range.start)..raw_to_display(raw, range.end),
                    newline,
                )
            });
        RowText {
            key: (pane, row),
            selected,
            selection_bg: bg,
            sink: self.select.layouts.clone(),
        }
    }
}

/// 拖选期间的节拍任务(见 [`DiffState::drag_tick`])。
fn drag_ticker(cx: &mut Context<DiffState>) -> Task<()> {
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(DRAG_TICK).await;
            let alive = this.update(cx, |s, cx| s.drag_tick(cx)).unwrap_or(false);
            if !alive {
                break;
            }
        }
    })
}

// ─── 接线 ─────────────────────────────────────────────────────

/// 列表外壳上的鼠标处理(每栏一份):左键在正文上按下起选区(单击按字符、双击按词、
/// 三击按行,Shift+单击把同一栏的现有选区延伸过来),右键弹「复制 / 全选」。
///
/// 按在行号列 / hunk 头上两样都不做,块操作按钮的点击照常归它自己;横向滚动条按下时
/// 自己会停止冒泡,到不了这里。左键**不**停止冒泡 —— 外层的焦点容器要靠这一下拿到
/// 焦点(之后 Ctrl+C 才有人接),只压掉这一下的窗口级选区(gpui-component `Root` 的
/// 选区层),免得它也跟着开始拖选。
pub(super) fn text_mouse(el: Div, state: &Entity<DiffState>, pane: Pane) -> Div {
    let (down, context) = (state.clone(), state.clone());
    el.on_mouse_down(
        MouseButton::Left,
        move |event: &MouseDownEvent, _window, cx| {
            if down.update(cx, |s, cx| s.begin_selection(pane, event, cx)) {
                gpui_component::GlobalState::suppress_text_selection(cx);
            }
        },
    )
    .on_mouse_down(
        MouseButton::Right,
        move |event: &MouseDownEvent, window, cx| {
            if context.read(cx).hit(pane, event.position, false).is_none() {
                return;
            }
            // 菜单打开即收焦点:不停止冒泡的话外层焦点容器随后又把焦点抢回去
            cx.stop_propagation();
            show_menu(&context, pane, event.position, window, cx);
        },
    )
}

/// 右键菜单:「复制」(没有选区时置灰)+「全选」(选的是点在的那一栏)。
fn show_menu(
    state: &Entity<DiffState>,
    pane: Pane,
    position: Point<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let has_selection = state
        .read(cx)
        .select
        .selection
        .is_some_and(|sel| !sel.is_empty());
    let (copy, all) = (state.clone(), state.clone());
    let entries: Vec<menu::MenuEntry> = vec![
        MenuItem::new(t("gitCommit", "copySelection"))
            .shortcut(menu::hotkey_label(true, false, false, "C"))
            .disabled(!has_selection)
            .on_click(move |_window, cx| {
                copy_selection(&copy, cx);
            })
            .into(),
        MenuItem::new(t("gitCommit", "selectAll"))
            .shortcut(menu::hotkey_label(true, false, false, "A"))
            .on_click(move |_window, cx| {
                select_all(&all, Some(pane), cx);
            })
            .into(),
    ];
    menu::show(position, entries, window, cx);
}

/// 拖选期间的窗口级鼠标监听:`on_mouse_move` / `on_mouse_up` 只在悬停时触发,指针拖出
/// 列表、拖出弹窗就收不到了。这里走 `window.on_mouse_event`(每帧注册、随帧清掉),
/// 捕获相处理,不怕途经的元素停止冒泡。不在拖选时只读一下状态就返回。
pub(super) fn drag_listener(state: &Entity<DiffState>) -> impl IntoElement {
    let state = state.clone();
    canvas(
        |_, _, _| {},
        move |_, _, window, _| {
            let moved = state.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
                if phase.capture() && moved.read(cx).select.drag.is_some() {
                    moved.update(cx, |s, cx| s.drag_to(event.position, event.dragging(), cx));
                }
            });
            let released = state.clone();
            window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
                if phase.capture()
                    && event.button == MouseButton::Left
                    && released.read(cx).select.drag.is_some()
                {
                    released.update(cx, |s, cx| {
                        s.drag_to(event.position, true, cx);
                        s.end_drag();
                    });
                }
            });
        },
    )
    .absolute()
    .size_full()
}

/// 把选区复制进剪贴板。没有可复制的内容返回 `false`(调用方据此 `propagate`)。
fn copy_selection(state: &Entity<DiffState>, cx: &mut App) -> bool {
    let Some(text) = state.read(cx).copy_text() else {
        return false;
    };
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    true
}

/// 全选(见 [`DiffState::select_all`])。
fn select_all(state: &Entity<DiffState>, pane: Option<Pane>, cx: &mut App) -> bool {
    state.update(cx, |s, cx| {
        let done = s.select_all(pane);
        if done {
            cx.notify();
        }
        done
    })
}

/// 弹窗焦点容器上挂 Ctrl/Cmd+C、Ctrl/Cmd+A(键位绑在 `hotkeys.rs`,三个 diff 弹窗各自
/// 的 key context 上)。没有可复制 / 可选的内容时 `propagate`,按键照常往外走。
pub(super) fn text_actions(el: Div, state: &Entity<DiffState>) -> Div {
    let (copy, all) = (state.clone(), state.clone());
    el.on_action(move |_: &GitDiffCopy, _window, cx| {
        if !copy_selection(&copy, cx) {
            cx.propagate();
        }
    })
    .on_action(move |_: &GitDiffSelectAll, _window, cx| {
        if !select_all(&all, None, cx) {
            cx.propagate();
        }
    })
}

#[cfg(test)]
mod tests {
    use mt_project::git::{DiffHunk, DiffLine};

    use super::super::flatten;
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

    fn pt(row: usize, offset: usize) -> TextPoint {
        TextPoint { row, offset }
    }

    fn sel(pane: Pane, anchor: (usize, usize), head: (usize, usize)) -> TextSelection {
        TextSelection {
            pane,
            anchor: pt(anchor.0, anchor.1),
            head: pt(head.0, head.1),
        }
    }

    /// 显示偏移与原文偏移互换:tab 按制表位展开,落在 tab 展开出的空白中间时,光标取
    /// 较近的一侧、「指针下的字符」取 tab 自己。
    #[test]
    fn tab映射_显示与原文偏移互换() {
        // "\tab\tc" → "    ab  c":首个 tab 占 4 格,"ab" 之后在第 6 列,第二个 tab 补 2 格
        let raw = "\tab\tc";
        assert_eq!(raw_to_display(raw, 0), 0);
        assert_eq!(raw_to_display(raw, 1), 4);
        assert_eq!(raw_to_display(raw, 3), 6);
        assert_eq!(raw_to_display(raw, 4), 8);
        assert_eq!(raw_to_display(raw, 5), 9);

        assert_eq!(display_to_raw(raw, 0, true), 0);
        assert_eq!(display_to_raw(raw, 1, true), 0, "离 tab 左缘更近");
        assert_eq!(display_to_raw(raw, 2, true), 1, "过半落到 tab 之后");
        assert_eq!(display_to_raw(raw, 3, false), 0, "指针下的字符就是这个 tab");
        assert_eq!(display_to_raw(raw, 4, true), 1);
        assert_eq!(display_to_raw(raw, 6, true), 3);
        assert_eq!(display_to_raw(raw, 7, true), 4);
        assert_eq!(display_to_raw(raw, 7, false), 3);
        assert_eq!(display_to_raw(raw, 9, true), 5);
        assert_eq!(display_to_raw(raw, 100, true), 5, "越界钳到行尾");

        // 没有 tab:恒等(越界钳住、落在字符中间退回字符起点)
        assert_eq!(display_to_raw("中文", 3, true), 3);
        assert_eq!(display_to_raw("中文", 4, true), 3);
        assert_eq!(raw_to_display("中文", 6), 6);

        // CJK 后面的 tab:列按字符数算(与 `expand_line` 同口径),「中」在第 0 列
        let raw = "中\tx";
        assert_eq!(raw_to_display(raw, 3), 3);
        assert_eq!(raw_to_display(raw, 4), 6);
        assert_eq!(display_to_raw(raw, 6, true), 4);
    }

    /// 选区先后不定,用时规范化;逐行区间:首行从起点到行尾、中间整行、末行从行首到终点,
    /// 末行之前的行尾换行都在选区里。别的栏、空选区一律不涂。
    #[test]
    fn 选区_规范化与逐行区间() {
        let s = sel(Pane::Old, (3, 5), (1, 2));
        assert_eq!(s.ordered(), (pt(1, 2), pt(3, 5)));
        assert_eq!(s.row_span(Pane::Old, 0, 10), None);
        assert_eq!(s.row_span(Pane::Old, 1, 10), Some((2..10, true)));
        assert_eq!(
            s.row_span(Pane::Old, 2, 0),
            Some((0..0, true)),
            "空行只涂换行"
        );
        assert_eq!(s.row_span(Pane::Old, 3, 10), Some((0..5, false)));
        assert_eq!(
            s.row_span(Pane::Old, 3, 3),
            Some((0..3, false)),
            "偏移越界钳到行尾"
        );
        assert_eq!(s.row_span(Pane::Old, 4, 10), None);
        assert_eq!(s.row_span(Pane::New, 1, 10), None, "另一栏不涂");

        let caret = sel(Pane::Inline, (2, 3), (2, 3));
        assert!(caret.is_empty());
        assert_eq!(caret.row_span(Pane::Inline, 2, 10), None);
        // 终点恰在下一行行首:上一行的换行在选区里,下一行一个字也不涂
        let s = sel(Pane::Inline, (1, 4), (2, 0));
        assert_eq!(s.row_span(Pane::Inline, 1, 4), Some((4..4, true)));
        assert_eq!(s.row_span(Pane::Inline, 2, 5), None);
    }

    /// 行内视图复制的是行内容本身:不带行号、不带 +/-,行间补换行。
    #[test]
    fn 复制_行内只给行内容() {
        let f = flatten(&[hunk(vec![
            line("context", "alpha"),
            line("delete", "beta"),
            line("add", "gamma"),
        ])]);
        // 行 0 是 hunk 头
        assert_eq!(
            selected_text(&f, &sel(Pane::Inline, (1, 0), (3, 5))),
            "alpha\nbeta\ngamma"
        );
        assert_eq!(
            selected_text(&f, &sel(Pane::Inline, (3, 2), (1, 2))),
            "pha\nbeta\nga"
        );
        // 终点在下一行行首:带上那个换行
        assert_eq!(
            selected_text(&f, &sel(Pane::Inline, (1, 0), (2, 0))),
            "alpha\n"
        );
        // 从 hunk 头拖起:头行跳过
        assert_eq!(
            selected_text(&f, &sel(Pane::Inline, (0, 0), (1, 5))),
            "alpha"
        );
    }

    /// 并排视图只给起拖那一栏:旧侧拿删除行与上下文,新侧拿新增行与上下文,对侧空占位
    /// 的行连同换行整行跳过。
    #[test]
    fn 复制_并排只给单侧且跳过空占位() {
        let f = flatten(&[hunk(vec![
            line("delete", "old1"),
            line("delete", "old2"),
            line("add", "new1"),
            line("context", "ctx"),
        ])]);
        // 行:头 / (old1, new1) / (old2, 空) / (ctx, ctx)
        assert_eq!(f.pair_rows.len(), 4);
        assert_eq!(
            selected_text(&f, &sel(Pane::Old, (0, 0), (3, 3))),
            "old1\nold2\nctx"
        );
        assert_eq!(
            selected_text(&f, &sel(Pane::New, (0, 0), (3, 3))),
            "new1\nctx"
        );
        assert_eq!(selected_text(&f, &sel(Pane::New, (2, 0), (2, 0))), "");
    }

    /// 跨 hunk 拖选:中间的 `@@` 头不进剪贴板。
    #[test]
    fn 复制_跨_hunk_不带头行() {
        let f = flatten(&[
            hunk(vec![line("context", "first")]),
            hunk(vec![line("context", "second")]),
        ]);
        // 行:头 / first / 头 / second
        assert_eq!(
            selected_text(&f, &sel(Pane::Inline, (1, 0), (3, 6))),
            "first\nsecond"
        );
        assert_eq!(
            selected_text(&f, &sel(Pane::New, (1, 2), (3, 3))),
            "rst\nsec"
        );
    }

    /// 屏幕上 tab 是展开的,复制出来必须是原文里的真 tab。
    #[test]
    fn 复制_给原文里的真_tab() {
        let f = flatten(&[hunk(vec![
            line("context", "\tfoo\tbar"),
            line("context", "plain"),
        ])]);
        assert_eq!(f.lines[0].content, "    foo bar", "显示用的是展开后的");
        assert_eq!(f.raw[0].as_deref(), Some("\tfoo\tbar"));
        assert_eq!(f.raw[1], None, "没有 tab 的行不另存一份");
        assert_eq!(
            selected_text(&f, &sel(Pane::Inline, (1, 0), (2, 5))),
            "\tfoo\tbar\nplain"
        );
        assert_eq!(selected_text(&f, &sel(Pane::Inline, (1, 1), (1, 4))), "foo");
    }

    /// 全选取整栏:第一行行首到最后一行行尾;新文件的旧侧没有一行内容,复制为空。
    #[test]
    fn 全选_取整栏() {
        let f = flatten(&[hunk(vec![line("add", "a"), line("add", "bc")])]);
        let all = select_all_in(&f, Pane::New).expect("有行可选");
        assert_eq!(all.ordered(), (pt(0, 0), pt(2, 2)));
        assert_eq!(selected_text(&f, &all), "a\nbc");
        let old = select_all_in(&f, Pane::Old).expect("行位照样有");
        assert_eq!(selected_text(&f, &old), "");
        assert!(select_all_in(&Flat::default(), Pane::Inline).is_none());
    }

    /// 双击选词:字母数字连成一个词(含中日韩文字),空白连成一段,符号一个字符一段;
    /// 行尾之外按最后一个字符算。
    #[test]
    fn 双击选词() {
        let text = "let foo_bar = 1;";
        assert_eq!(word_at(text, 5), 4..11);
        assert_eq!(word_at(text, 3), 3..4);
        assert_eq!(word_at(text, 12), 12..13, "`=` 单独一段");
        assert_eq!(word_at(text, 15), 15..16);
        assert_eq!(word_at(text, 99), 15..16, "行尾之外取最后一个字符");
        assert_eq!(word_at("a   b", 2), 1..4);
        assert_eq!(word_at("a==b", 1), 1..2);
        assert_eq!(word_at("注释说明 text", 3), 0..12, "中文整段选中");
        assert_eq!(word_at("中文abc", 0), 0..9);
        assert_eq!(word_at("", 0), 0..0);
        assert_eq!(word_at("foo", 3), 0..3);
    }

    /// 双击 / 三击后接着拖:起拖那一段始终在选区里,往前拖 anchor 落在它的尾,往后拖落在头。
    #[test]
    fn 按词按行拖_延伸() {
        let origin = (pt(2, 4), pt(2, 9));
        assert_eq!(
            extend_by_unit(origin, (pt(1, 0), pt(1, 3))),
            (pt(2, 9), pt(1, 0))
        );
        assert_eq!(
            extend_by_unit(origin, (pt(2, 12), pt(2, 15))),
            (pt(2, 4), pt(2, 15))
        );
        assert_eq!(extend_by_unit(origin, (pt(2, 4), pt(2, 9))), origin);
    }

    /// 词级高亮与选区切成互不重叠的段,重叠处单独一段;没被涂的部分不出段。
    #[test]
    fn 底色分段_词级与选区叠加() {
        let marks = [2..5, 8..10];
        assert_eq!(
            paint_segments(&marks, Some(&(4..9))),
            vec![
                (2..4, Paint::Word),
                (4..5, Paint::Both),
                (5..8, Paint::Selected),
                (8..9, Paint::Both),
                (9..10, Paint::Word),
            ]
        );
        assert_eq!(
            paint_segments(&marks, None),
            vec![(2..5, Paint::Word), (8..10, Paint::Word)]
        );
        assert_eq!(
            paint_segments(&[], Some(&(1..3))),
            vec![(1..3, Paint::Selected)]
        );
        assert_eq!(paint_segments(&[], Some(&(3..3))), vec![], "空区间不出段");
        assert_eq!(
            paint_segments(&[0..2, 2..4], None),
            vec![(0..4, Paint::Word)],
            "相接的同类段并成一段"
        );
    }

    /// 叠色是标准 source-over:不透明的顶层盖死底层,透明的顶层等于没叠,
    /// 半透明白叠在黑上是中灰。
    #[test]
    fn 叠色_标准_source_over() {
        let rgba = |r: f32, g: f32, b: f32, a: f32| -> Hsla { Rgba { r, g, b, a }.into() };
        let close = |x: Hsla, y: Hsla| {
            let (x, y) = (Rgba::from(x), Rgba::from(y));
            [(x.r, y.r), (x.g, y.g), (x.b, y.b), (x.a, y.a)]
                .iter()
                .all(|(p, q)| (p - q).abs() < 1e-3)
        };
        let (red, black) = (rgba(1.0, 0.0, 0.0, 1.0), rgba(0.0, 0.0, 0.0, 1.0));
        let half_white = rgba(1.0, 1.0, 1.0, 0.5);
        assert!(close(over(red, black), red));
        assert!(close(over(Hsla::default(), black), black));
        assert!(close(over(half_white, black), rgba(0.5, 0.5, 0.5, 1.0)));
        // 两层半透明:合成后的不透明度 = 0.5 + 0.5 × 0.5,颜色按各自的权重混
        let both = over(half_white, rgba(0.0, 0.0, 0.0, 0.5));
        assert!(close(both, rgba(2.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0, 0.75)));
    }

    /// 纵坐标换行号:按恒定行高取整,落在最后一行之下是 `None`。
    #[test]
    fn 纵坐标换行号() {
        assert_eq!(row_at(0.0, 20.0, 5), Some(0));
        assert_eq!(row_at(39.9, 20.0, 5), Some(1));
        assert_eq!(row_at(-5.0, 20.0, 5), Some(0));
        assert_eq!(row_at(100.0, 20.0, 5), None);
        assert_eq!(row_at(10.0, 20.0, 0), None);
    }

    /// 末字中点:「末尾行」三个全角字各 16px 宽(末字起点 32、行宽 48、中点 40)。gpui
    /// 越过末字起点就给行尾(9),没过中点要改落在末字之前(6);别处的落点原样放行。
    #[test]
    fn 光标落点_末字按中点判断() {
        let last = Some((6, 32.0));
        assert_eq!(settle_last_glyph(9, 9, last, 48.0, 35.0), 6, "末字左半边");
        assert_eq!(settle_last_glyph(9, 9, last, 48.0, 41.0), 9, "末字右半边");
        assert_eq!(settle_last_glyph(9, 9, last, 48.0, 80.0), 9, "行尾之外");
        assert_eq!(settle_last_glyph(3, 9, last, 48.0, 20.0), 3, "行中间不动");
        assert_eq!(settle_last_glyph(0, 0, None, 0.0, 5.0), 0, "空行");
    }

    /// 自动滚动:指针在区间内不滚,越出越远滚得越快,封顶;两个方向对称。
    #[test]
    fn 越界自动滚动_速度() {
        assert_eq!(edge_speed(50.0, 0.0, 100.0), 0.0);
        assert_eq!(edge_speed(0.0, 0.0, 100.0), 0.0, "压在边上还不滚");
        let near = edge_speed(110.0, 0.0, 100.0);
        let far = edge_speed(400.0, 0.0, 100.0);
        assert!(near >= SCROLL_MIN && near < far, "{near} {far}");
        assert_eq!(far, SCROLL_MAX);
        assert_eq!(edge_speed(-10.0, 0.0, 100.0), -near);
    }
}
