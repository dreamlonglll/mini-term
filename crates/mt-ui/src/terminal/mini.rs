//! [`MiniTerminalElement`] —— 终端画面的**只读缩略快照**。
//!
//! 对应原版 `src/utils/panePreview.ts` + `panePreviewCanvas.ts` 那一对:
//! 项目行悬停的 pane 预览卡(`ProjectPanePreview`)与非激活 tab 悬停的缩略图
//! (`PaneTabPreview`)共用它,回答「那个看不见的终端里现在是什么画面」。
//!
//! # 为什么不复用 [`TerminalElement`](super::TerminalElement)
//!
//! 那个件是**交互式**的:它 resize emulator(缩略图一挂上去就会把别人的 PTY
//! 拉成 200×20)、插 hitbox、注册滚轮/选择/拖拽监听、维护光标与 IME。缩略图
//! 一条都不要 —— 它只画字,不接输入、不接滚动、不动 grid 尺寸。
//!
//! # 画什么
//!
//! 与原版 `extractPreviewGrid` 逐条对齐:
//!
//! - 取 **viewport**(`display_iter`,已含 display_offset),用户滚上去看历史时
//!   缩略图跟着看历史,alt screen(vim/htop)经 `renderable_content` 自然生效;
//! - **只画前景**:整块底先铺一次 `theme.background`,逐格背景色一概不画
//!   (原版 canvas 就是 `fillRect(background)` + 逐 run `fillText`);
//! - 同色连续窄字符合成一个 run 一次 shape,**宽字符(CJK/emoji)单独成 run**
//!   并钉在 `col × cell_w` 上 —— 与主渲染器同一条「不可合并格自己定位」的地基;
//! - 空格与空 cell 断开 run(不画,底色透出)。
//!
//! # 怎么摆(cover + 左下锚定)
//!
//! 原版是 canvas 按 8px 字号建内部位图、再用 CSS `object-fit: cover` +
//! `object-position: left bottom` 缩到卡片里。gpui 没有「先画大再缩放」的路,
//! 于是反过来算:**由卡片尺寸反解字号**([`mini_geometry`]),让缩放后的
//! cell 网格恰好盖满卡片,超出的部分裁右、裁顶 —— 左下角(最新输出与 TUI
//! 输入区)一定留得住,与原版同像素同取舍。
//!
//! # 缓存与失效(Y 批「不许每帧重 shape 全屏文本」)
//!
//! 缩略图挂在浮层上,浮层活着的每一帧都会走 `prepaint`,而一屏文本的 shaping
//! 是几百次 DirectWrite 往返 —— 每帧重做等于把悬停做成卡顿源。所以:
//!
//! | 触发 | 动作 |
//! |---|---|
//! | 首帧 / 几何键变了(卡片尺寸、grid 行列、字号、配色、字体) | 立即重取 + 重 shape |
//! | 距上次取数 ≥ [`MiniTerminalElement::refresh`](Self::refresh)(默认 500ms) | 重取 grid,**算内容指纹**;指纹变了才重 shape |
//! | 其余帧 | 直接画缓存,零 shaping、零 grid 锁 |
//!
//! 500ms 这个节拍照抄原版(`setInterval(…, 500)` 重画一次,预览是活的);
//! 重画由宿主的定时器 `cx.notify()` 唤起,元素本身不请求帧。

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::term::cell::Flags;
use gpui::{
    App, Bounds, ContentMask, Element, ElementId, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, LayoutId, Pixels, Point, ShapedLine, SharedString, Style, TextRun, Window, fill,
    point, px,
};
use mt_terminal::TerminalEmulator;

use super::colors;
use super::theme::{TerminalStyle, TerminalTheme};

/// 缩略图默认的取数节拍(原版 `ProjectPanePreview` / `PaneTabPreview` 的 500ms)。
pub const MINI_REFRESH_MS: u64 = 500;

/// 缩略图字号下限。再小 shaping 出来就是一团噪点,还白费 CPU。
const MIN_FONT_SIZE: f32 = 2.0;
/// 缩略图字号上限。grid 很小(比如 20×5)时不该把字放到比正文还大。
const MAX_FONT_SIZE: f32 = 14.0;

/// `Pixels` 取标量。与 [`super::element`] 里那个同名短函数同一用途。
#[inline]
fn f(p: Pixels) -> f32 {
    f32::from(p)
}

// ─── 纯逻辑:run 合并 ────────────────────────────────────────────

/// 一段同色连续文本。`col` 是它在 grid 里的**起始列**,绘制时钉在 `col × cell_w`。
#[derive(Clone, Debug, PartialEq)]
pub struct MiniRun {
    pub col: usize,
    pub text: String,
    pub color: Hsla,
}

/// 逐格喂进来的最小信息(列号、字符、占几列、前景色)。
#[derive(Clone, Copy, Debug)]
pub struct MiniCell {
    pub col: usize,
    pub ch: char,
    /// 1 = 窄字符;2 = 宽字符;0 = 宽字符的占位尾格(跳过)。
    pub width: usize,
    pub color: Hsla,
}

/// 把一行的 cell 合成 run(原版 `extractPreviewGrid` 的行内循环)。
///
/// 三条规则:空白/空 cell 断开 run;换色断开 run;**宽字符自成一 run** ——
/// run 内部靠等宽字体自身 advance 定位,CJK 字形不保证恰好两倍半角宽,
/// 混排会一路漂过去。
pub fn build_row_runs(cells: &[MiniCell]) -> Vec<MiniRun> {
    let mut runs: Vec<MiniRun> = Vec::new();
    // 「上一格还能接着往里塞吗」——None = 必须另起
    let mut open: Option<usize> = None;
    for cell in cells {
        if cell.width == 0 {
            // 宽字符的 0 宽尾格:前一格已经把 run 关掉了,直接跳过
            continue;
        }
        if cell.ch == ' ' || cell.ch == '\0' {
            open = None;
            continue;
        }
        match open {
            Some(idx) if cell.width == 1 && runs[idx].color == cell.color => {
                runs[idx].text.push(cell.ch);
            }
            _ => {
                runs.push(MiniRun {
                    col: cell.col,
                    text: cell.ch.to_string(),
                    color: cell.color,
                });
                // 宽字符不接后续字符,窄字符可以继续攒
                open = if cell.width == 1 {
                    Some(runs.len() - 1)
                } else {
                    None
                };
            }
        }
    }
    runs
}

// ─── 纯逻辑:几何反解 ────────────────────────────────────────────

/// 缩略图的排版结果。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MiniGeometry {
    pub font_size: Pixels,
    pub cell_w: Pixels,
    pub cell_h: Pixels,
    /// 网格左上角相对卡片左上角的偏移(x 恒为 0,y ≤ 0 = 裁掉顶部若干行)。
    pub offset: Point<Pixels>,
}

/// 由卡片尺寸反解字号(cover 语义)。
///
/// - `advance_ratio` = 主字体 `'M'` 的 advance ÷ 字号(等宽字体上是个常数,
///   典型 0.6);`line_ratio` = 行高 ÷ 字号。
/// - 取 `max(按宽算, 按高算)` = **cover**:网格一定盖满卡片,多出来的裁掉;
///   取 `min` 就是 contain,四周会留黑边,与原版观感不同。
/// - 左对齐 + 下对齐:`offset.y = 卡片高 − 网格高`(≤ 0),裁的是顶部老内容。
pub fn mini_geometry(
    cols: usize,
    rows: usize,
    area: gpui::Size<Pixels>,
    advance_ratio: f32,
    line_ratio: f32,
) -> MiniGeometry {
    let cols = cols.max(1) as f32;
    let rows = rows.max(1) as f32;
    let advance_ratio = if advance_ratio > 0.01 {
        advance_ratio
    } else {
        0.6
    };
    let line_ratio = if line_ratio > 0.01 { line_ratio } else { 1.35 };
    let by_width = f(area.width) / (cols * advance_ratio);
    let by_height = f(area.height) / (rows * line_ratio);
    let font_size = by_width.max(by_height).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
    let cell_w = font_size * advance_ratio;
    let cell_h = font_size * line_ratio;
    let grid_h = cell_h * rows;
    MiniGeometry {
        font_size: px(font_size),
        cell_w: px(cell_w),
        cell_h: px(cell_h),
        // 网格比卡片矮(字号被上限钳住)时不上移,贴顶画即可
        offset: point(px(0.0), px((f(area.height) - grid_h).min(0.0))),
    }
}

// ─── 元素 ────────────────────────────────────────────────────────

/// 终端画面的只读缩略快照。见模块注释。
pub struct MiniTerminalElement {
    id: ElementId,
    emulator: Arc<TerminalEmulator>,
    style: TerminalStyle,
    theme: TerminalTheme,
    refresh: Duration,
}

impl MiniTerminalElement {
    pub fn new(
        id: impl Into<ElementId>,
        emulator: Arc<TerminalEmulator>,
        style: TerminalStyle,
        theme: TerminalTheme,
    ) -> Self {
        Self {
            id: id.into(),
            emulator,
            style,
            theme,
            refresh: Duration::from_millis(MINI_REFRESH_MS),
        }
    }

    /// 取数节拍。默认 [`MINI_REFRESH_MS`];**与重绘频率无关** ——
    /// 更密的重绘只会命中缓存。
    pub fn refresh(mut self, refresh: Duration) -> Self {
        self.refresh = refresh;
        self
    }
}

/// 一段已 shape 好的文本,几何相对**网格左上角**。
struct MiniPiece {
    origin: Point<Pixels>,
    line: ShapedLine,
}

/// 跨帧保留的缓存。见模块注释的失效表。
struct MiniCache {
    /// 几何/配色指纹。变了必须整份重建。
    key: u64,
    /// grid 内容指纹。只有它变了才重 shape。
    content: u64,
    built_at: Instant,
    pieces: Rc<Vec<MiniPiece>>,
}

#[derive(Clone, Default)]
struct MiniState {
    cache: Rc<RefCell<Option<MiniCache>>>,
}

pub struct MiniPrepared {
    geometry: MiniGeometry,
    pieces: Rc<Vec<MiniPiece>>,
}

impl MiniTerminalElement {
    /// 从 emulator 抓一屏 run,同时算内容指纹。**持 grid 锁的时间只有这一段**。
    fn snapshot(&self) -> (Vec<Vec<MiniRun>>, u64) {
        let mut lines: Vec<Vec<MiniRun>> = Vec::new();
        let mut hasher = DefaultHasher::new();
        {
            let term = self.emulator.term().lock();
            let content = term.renderable_content();
            let display_offset = content.display_offset;
            let colors_table = content.colors;
            let mut cells: Vec<MiniCell> = Vec::new();
            let mut current_row: Option<usize> = None;
            let mut contrast = colors::ContrastMemo::default();
            for indexed in content.display_iter {
                let row = (indexed.point.line.0 + display_offset as i32).max(0) as usize;
                if current_row != Some(row) {
                    if current_row.is_some() {
                        lines.push(build_row_runs(&cells));
                        cells.clear();
                    }
                    current_row = Some(row);
                }
                let cell = indexed.cell;
                let flags = cell.flags;
                let width = if flags.contains(Flags::WIDE_CHAR_SPACER) {
                    0
                } else if flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
                // **INVERSE 不换色**:原版 `extractPreviewGrid` 只读前景,不认
                // 反显。缩略图不画逐格背景,真按反显换成背景色的话,反显块
                // (fzf 选中行 / vim 状态栏)的文字会变成「底色画在底色上」——
                // 整段消失,比配色不准糟得多。
                let mut color = colors::foreground(cell.fg, flags, colors_table, &self.theme);
                // HIDDEN(SGR 8,`read -s` 之类)当空白处理。原版没有这条
                // (它的 canvas 照画)—— **刻意加严**:缩略图会出现在别的
                // 项目的悬停浮层里,把隐藏输入摊在那儿是实打实的泄露面
                let ch = if flags.contains(Flags::HIDDEN) {
                    ' '
                } else {
                    cell.c
                };
                // 最小对比度。这里参照色恒为 `theme.background` —— 缩略图不画逐格
                // 背景,整块底就是它(见模块注释),所以对得上真正画出来的那一对。
                // 顺序在 HIDDEN 转空白之后:隐藏输入已经没有笔画,不必也不该修正。
                // 色块类字形(powerline 分隔符/块元素)同样豁免,见 [`colors::is_fill_glyph`]
                // —— 缩略图里它们是「画」,推成近黑/近白只会多出一块假亮斑。
                if ch != ' ' && !colors::is_fill_glyph(ch) {
                    color = contrast.resolve(color, self.theme.background, colors::MIN_CONTRAST_RATIO);
                }
                cells.push(MiniCell {
                    col: indexed.point.column.0,
                    ch,
                    width,
                    color,
                });
            }
            if current_row.is_some() {
                lines.push(build_row_runs(&cells));
            }
        }
        for (row, runs) in lines.iter().enumerate() {
            for run in runs {
                row.hash(&mut hasher);
                run.col.hash(&mut hasher);
                run.text.hash(&mut hasher);
                run.color.h.to_bits().hash(&mut hasher);
                run.color.s.to_bits().hash(&mut hasher);
                run.color.l.to_bits().hash(&mut hasher);
                run.color.a.to_bits().hash(&mut hasher);
            }
        }
        (lines, hasher.finish())
    }

    /// run → 已 shape 的文本片段。锁**必须已经放掉**(shaping 会跑进 DirectWrite)。
    fn shape(
        &self,
        lines: &[Vec<MiniRun>],
        geometry: MiniGeometry,
        window: &mut Window,
    ) -> Vec<MiniPiece> {
        let font = self.style.font();
        let mut pieces = Vec::new();
        for (row, runs) in lines.iter().enumerate() {
            let y = geometry.cell_h * row as f32;
            for run in runs {
                let text: SharedString = run.text.clone().into();
                let text_run = TextRun {
                    len: text.len(),
                    font: font.clone(),
                    color: run.color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped =
                    window
                        .text_system()
                        .shape_line(text, geometry.font_size, &[text_run], None);
                pieces.push(MiniPiece {
                    origin: point(geometry.cell_w * run.col as f32, y),
                    line: shaped,
                });
            }
        }
        pieces
    }
}

impl IntoElement for MiniTerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for MiniTerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = MiniPrepared;

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
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let state: MiniState = window.with_element_state::<MiniState, _>(id.unwrap(), |prev, _w| {
            let s = prev.unwrap_or_default();
            (s.clone(), s)
        });

        // 字号是反解出来的,advance 比例先按参考字号量一次(等宽字体上线性)
        let font = self.style.font();
        let font_id = window.text_system().resolve_font(&font);
        const PROBE: Pixels = px(16.0);
        let advance_ratio = window
            .text_system()
            .advance(font_id, PROBE, 'M')
            .map(|s| f(s.width) / f(PROBE))
            .unwrap_or(0.6);
        let size = self.emulator.term_size();
        let geometry = mini_geometry(
            size.columns,
            size.screen_lines,
            bounds.size,
            advance_ratio,
            // 原版缩略图行密度按 1.35 定(`panePreviewCanvas.ts` 的 CELL_H)
            1.35,
        );

        let mut hasher = DefaultHasher::new();
        size.columns.hash(&mut hasher);
        size.screen_lines.hash(&mut hasher);
        f(bounds.size.width).to_bits().hash(&mut hasher);
        f(bounds.size.height).to_bits().hash(&mut hasher);
        f(geometry.font_size).to_bits().hash(&mut hasher);
        self.style.font_family.hash(&mut hasher);
        // 与 `TerminalElement` 的帧指纹同理:连字改的是 shaping 结果而不是内容,
        // 不进键的话切开关后缩略图会一直停在旧画面
        self.style.ligatures.hash(&mut hasher);
        self.theme.background.h.to_bits().hash(&mut hasher);
        self.theme.foreground.h.to_bits().hash(&mut hasher);
        self.theme.foreground.l.to_bits().hash(&mut hasher);
        let key = hasher.finish();

        let stale = {
            let cache = state.cache.borrow();
            match cache.as_ref() {
                None => true,
                Some(c) => c.key != key || c.built_at.elapsed() >= self.refresh,
            }
        };

        let pieces = if stale {
            let (lines, content) = self.snapshot();
            let reuse = {
                let cache = state.cache.borrow();
                cache
                    .as_ref()
                    .filter(|c| c.key == key && c.content == content)
                    .map(|c| c.pieces.clone())
            };
            let pieces = match reuse {
                // 只是到点复查,内容没变 —— 时间戳往前推,shaping 一次都不做
                Some(pieces) => pieces,
                None => Rc::new(self.shape(&lines, geometry, window)),
            };
            *state.cache.borrow_mut() = Some(MiniCache {
                key,
                content,
                built_at: Instant::now(),
                pieces: pieces.clone(),
            });
            pieces
        } else {
            state
                .cache
                .borrow()
                .as_ref()
                .map(|c| c.pieces.clone())
                .unwrap_or_default()
        };

        MiniPrepared { geometry, pieces }
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
        // 整块底色。缩略图不画逐格背景(见模块注释),这一层就是「终端底」
        window.paint_quad(fill(bounds, self.theme.background));
        let origin = bounds.origin + prepared.geometry.offset;
        let line_height = prepared.geometry.cell_h;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for piece in prepared.pieces.iter() {
                let at = origin + piece.origin;
                // 裁出卡片外的行不必进渲染队列(cover 之下顶部经常裁掉十几行)
                if at.y + line_height < bounds.origin.y
                    || at.y > bounds.origin.y + bounds.size.height
                {
                    continue;
                }
                _ = piece.line.paint(at, line_height, window, cx);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::size;

    fn c(col: usize, ch: char, width: usize, color: Hsla) -> MiniCell {
        MiniCell {
            col,
            ch,
            width,
            color,
        }
    }

    fn red() -> Hsla {
        super::super::theme::rgb8(255, 0, 0)
    }

    fn blue() -> Hsla {
        super::super::theme::rgb8(0, 0, 255)
    }

    #[test]
    fn 同色窄字符合成一个_run() {
        let cells = vec![
            c(0, 'a', 1, red()),
            c(1, 'b', 1, red()),
            c(2, 'c', 1, red()),
        ];
        let runs = build_row_runs(&cells);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].col, 0);
        assert_eq!(runs[0].text, "abc");
    }

    #[test]
    fn 换色断开_run_且新_run_记真实列号() {
        let cells = vec![
            c(0, 'a', 1, red()),
            c(1, 'b', 1, blue()),
            c(2, 'c', 1, blue()),
        ];
        let runs = build_row_runs(&cells);
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].col, runs[0].text.as_str()), (0, "a"));
        assert_eq!((runs[1].col, runs[1].text.as_str()), (1, "bc"));
    }

    #[test]
    fn 空格断开_run_而且自己不入表() {
        let cells = vec![
            c(0, 'a', 1, red()),
            c(1, ' ', 1, red()),
            c(2, 'b', 1, red()),
        ];
        let runs = build_row_runs(&cells);
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].col, runs[0].text.as_str()), (0, "a"));
        // 第二段从第 2 列起 —— 空格靠列号跳过去,不靠往 run 里塞空格
        assert_eq!((runs[1].col, runs[1].text.as_str()), (2, "b"));
    }

    /// 宽字符单独成 run:run 内定位靠字体 advance,CJK 不保证恰好两倍宽,
    /// 混在一起会一路漂。后继窄字符必须按**真实列号**另起。
    #[test]
    fn 宽字符自成一_run_后继另起() {
        let cells = vec![
            c(0, 'a', 1, red()),
            c(1, '你', 2, red()),
            c(2, '\0', 0, red()), // 宽字符的占位尾格
            c(3, 'b', 1, red()),
        ];
        let runs = build_row_runs(&cells);
        assert_eq!(runs.len(), 3);
        assert_eq!((runs[0].col, runs[0].text.as_str()), (0, "a"));
        assert_eq!((runs[1].col, runs[1].text.as_str()), (1, "你"));
        assert_eq!((runs[2].col, runs[2].text.as_str()), (3, "b"));
    }

    #[test]
    fn 空行出空表() {
        assert!(build_row_runs(&[]).is_empty());
        let blanks = vec![c(0, ' ', 1, red()), c(1, '\0', 1, red())];
        assert!(build_row_runs(&blanks).is_empty());
    }

    /// cover:两个方向取**大**的那个缩放,网格一定盖满卡片。
    #[test]
    fn 几何按cover反解字号() {
        // 80 列 × 24 行,卡片 380×232:按宽 380/(80*0.6)=7.92,按高 232/(24*1.35)=7.16
        let g = mini_geometry(80, 24, size(px(380.0), px(232.0)), 0.6, 1.35);
        assert!((f(g.font_size) - 7.916).abs() < 0.01, "{:?}", g.font_size);
        // 网格宽 = 80 * 7.916 * 0.6 ≈ 380 恰好盖满;高 = 24 * 7.916 * 1.35 ≈ 256 > 232
        assert!(f(g.cell_w) * 80.0 >= 379.9);
        assert!(f(g.cell_h) * 24.0 >= 232.0);
    }

    /// 左下锚定:超出的高度从**顶部**裁掉(offset.y 为负),保住最新输出那几行。
    #[test]
    fn 超高时裁顶不裁底() {
        let g = mini_geometry(80, 50, size(px(380.0), px(232.0)), 0.6, 1.35);
        let grid_h = f(g.cell_h) * 50.0;
        assert!(grid_h > 232.0, "cover 之下必然超高");
        assert!((f(g.offset.y) - (232.0 - grid_h)).abs() < 0.01);
        assert_eq!(f(g.offset.x), 0.0, "左对齐,x 永远是 0");
    }

    /// 网格比卡片矮(小 grid 被字号上限钳住)时贴顶画,不能把 y 顶成正数。
    #[test]
    fn 网格偏矮时不下移() {
        let g = mini_geometry(4, 2, size(px(380.0), px(232.0)), 0.6, 1.35);
        assert_eq!(f(g.font_size), MAX_FONT_SIZE, "被上限钳住");
        assert_eq!(f(g.offset.y), 0.0);
    }

    /// 退化输入不能把字号算成 0 / NaN(0 列 0 行、比例为 0)。
    #[test]
    fn 退化输入有兜底() {
        let g = mini_geometry(0, 0, size(px(100.0), px(50.0)), 0.0, 0.0);
        assert!(f(g.font_size) >= MIN_FONT_SIZE);
        assert!(f(g.cell_w) > 0.0 && f(g.cell_h) > 0.0);
    }

    // -- 真 grid 抓取(不需要 Window,shaping 才需要) ------------------

    fn element(emulator: Arc<TerminalEmulator>) -> MiniTerminalElement {
        MiniTerminalElement::new(
            gpui::SharedString::from("mini-test"),
            emulator,
            TerminalStyle::default(),
            TerminalTheme::default(),
        )
    }

    #[test]
    fn 从真_grid_抓一屏() {
        let e = Arc::new(TerminalEmulator::new(mt_terminal::TermSize::new(20, 3)));
        e.advance(b"ab cd\r\n\xe4\xbd\xa0\xe5\xa5\xbdxy");
        let (lines, hash) = element(e.clone()).snapshot();
        assert_eq!(lines.len(), 3);
        // 第一行:空格断开成两段,列号是真实列号
        assert_eq!(lines[0].len(), 2);
        assert_eq!((lines[0][0].col, lines[0][0].text.as_str()), (0, "ab"));
        assert_eq!((lines[0][1].col, lines[0][1].text.as_str()), (3, "cd"));
        // 第二行:两个宽字符各自成 run(列 0 / 列 2),后面的窄字符从列 4 起
        let row = &lines[1];
        assert_eq!(row.len(), 3, "{row:?}");
        assert_eq!((row[0].col, row[0].text.as_str()), (0, "你"));
        assert_eq!((row[1].col, row[1].text.as_str()), (2, "好"));
        assert_eq!((row[2].col, row[2].text.as_str()), (4, "xy"));
        // 第三行空 → 空表
        assert!(lines[2].is_empty());
        // 同样内容再抓一次:指纹必须一致(缓存靠它判「要不要重 shape」)
        let (_, hash2) = element(e).snapshot();
        assert_eq!(hash, hash2);
    }

    /// 内容变了指纹必须跟着变 —— 否则缩略图会永远停在第一屏。
    #[test]
    fn 内容变了指纹就变() {
        let e = Arc::new(TerminalEmulator::new(mt_terminal::TermSize::new(20, 2)));
        e.advance(b"one");
        let (_, before) = element(e.clone()).snapshot();
        e.advance(b"\r\ntwo");
        let (_, after) = element(e).snapshot();
        assert_ne!(before, after);
    }

    /// SGR 8(隐藏)的格子不进 run —— 缩略图会出现在别的项目的浮层里,
    /// `read -s` 的内容不该摊在那儿。
    #[test]
    fn 隐藏属性的字不进缩略图() {
        let e = Arc::new(TerminalEmulator::new(mt_terminal::TermSize::new(20, 1)));
        e.advance(b"\x1b[8msecret\x1b[0m");
        let (lines, _) = element(e).snapshot();
        assert!(lines[0].is_empty(), "{:?}", lines[0]);
    }
}
