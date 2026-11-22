use super::*;

/// 「这个字符在主字体里有字形、且步进正好是一列宽吗」的缓存 —— 答案是三档的
/// [`Coverage`],多出来的那档给私用区图标(见下文末节)。
///
/// 每帧对每个格子问一次,不缓存就是每帧几千次 DirectWrite 往返。挂在
/// [`TerminalElementState`] 上(跨帧存活),prepaint 开头借一次、整帧共用 ——
/// 从前它是个 thread_local,于是**每个格子**都要走一趟 TLS + RefCell + SipHash。
///
/// 分两层:
///
/// - **ASCII 直接下标**。一屏一万个格子里九成九是 ASCII,查一次表不该有哈希成本。
///   小表按「字形变体 × 码位」分层,`[Option<Coverage>; 128] × 4` 一共 512 字节;
///   四档 FontId / 字号 / 列宽任一变化就整体清掉(那时 [`FrameKey`] 也变,
///   行缓存本来就要全量重建)。
/// - **非 ASCII 走 HashMap**,键里带 font_id 与字号 —— 粗体 / 斜体是不同的
///   font_id,各自算各自的。列宽不进键:它本来就是 (font_id, 字号) 算出来的
///   ('M' 的 advance),同一个键对应的答案不会变。
///
/// # 为什么是真 shape 一次,而不是问 `TextSystem::advance`
///
/// `advance()` 走 `glyph_for_char`,而 DirectWrite 的 `GetGlyphIndices` 对**缺字**
/// 返回的是 glyph 0(`.notdef`)而不是「没有」——等宽字体的 `.notdef` 步进恰好
/// 一列宽,于是 ⚠ ✔ ⏺ ★ 箭头 框线这些主字体没有的符号被判成「可合并」、并进段里,
/// shape 时却回退到别的字体拿到宽窄不一的字形,**段内其后每个字符都被顶歪**
/// (2026-09-19 真机:40 个 `●○★☆→←↑↓─│┌┐└┘` 的行尾偏出两列,`[⚠]   1 列` 画成
/// `1列`)。模块注释写的「主字体缺字要回退 → 不可合并」这条地基,在 Windows 上就此漏了。
///
/// 所以这里按真实路径 shape 这一个字符,看落下的 run **是不是主字体族**、总宽
/// **是不是一列**;两条都成立才算可合并。回退到别的字族一律不合并 —— 单独定位,
/// 字形宽窄只糊它自己那一格。
///
/// # 私用区图标的第三档
///
/// Nerd Font 图标在私用区,主字体不是补丁版时它的结局是两种之一:落到主字体的
/// `.notdef`(还是「主字体族、恰好一列」,照上面的判据会被并进段里画成方块),
/// 或者被回退表接到某个碰巧也占了这个码位的字体上(私用区各家自定义,CJK 字体
/// 可能带着 GB18030 的私用字,画出来与图标毫不相干)。两种都判成
/// [`Coverage::Symbol`],交给内置图标字体单独画(见 [`super::symbols`])。
/// 判据多看一样:落下的字形里有没有 glyph 0。
pub(super) struct AdvanceCache {
    /// ASCII 快表的有效性判据:四档变体的 FontId + 字号 + 列宽。
    pub(super) key: Option<([FontId; 4], u32, u32)>,
    /// 下标 = [`VariantFonts::slot`] 的四档(正 / 粗 / 斜 / 粗斜)。
    pub(super) ascii: [[Option<Coverage>; 128]; 4],
    pub(super) wide: HashMap<(FontId, u32, char), Coverage>,
}

/// 一个字符在主字体(含回退表)下的着落,决定它怎么画。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Coverage {
    /// 主字体有字形、步进恰好一列:可以并进合并段。
    Merge,
    /// 单独 shape、单独定位,照常走主字体 + 回退表。
    Alone,
    /// 主字体没有真字形的私用区字符:单独定位,换内置图标字体画
    /// (没接管时退回 [`Coverage::Alone`] 的画法)。
    Symbol,
}

/// `AdvanceCache::wide` 的最大条目数，防止长会话缩放字号或海量 Unicode 字符下无界增长。
pub(super) const WIDE_CACHE_LIMIT: usize = 4096;

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
    /// 每帧开头对一次表:字体 / 字号 / 列宽换了就把 ASCII 与 wide 缓存一并清掉。
    pub(super) fn begin_frame(
        &mut self,
        fonts: &VariantFonts,
        font_size: Pixels,
        cell_width: Pixels,
    ) {
        let key = (fonts.ids, f(font_size).to_bits(), f(cell_width).to_bits());
        if self.key != Some(key) {
            self.key = Some(key);
            self.ascii = [[None; 128]; 4];
            self.wide.clear();
        }
    }

    /// `base` 是主字体(含回退表),`slot` 选粗 / 斜变体;`font_id` 只当缓存键用。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn classify(
        &mut self,
        window: &Window,
        base: &gpui::Font,
        slot: usize,
        font_id: FontId,
        font_size: Pixels,
        ch: char,
        cell_width: Pixels,
    ) -> Coverage {
        let code = ch as usize;
        if code < 128 {
            if let Some(hit) = self.ascii[slot][code] {
                return hit;
            }
            let coverage = self.measure(window, base, slot, font_id, font_size, ch, cell_width);
            self.ascii[slot][code] = Some(coverage);
            return coverage;
        }
        self.measure(window, base, slot, font_id, font_size, ch, cell_width)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn measure(
        &mut self,
        window: &Window,
        base: &gpui::Font,
        slot: usize,
        font_id: FontId,
        font_size: Pixels,
        ch: char,
        cell_width: Pixels,
    ) -> Coverage {
        let key = (font_id, f(font_size).to_bits(), ch);
        if let Some(hit) = self.wide.get(&key).copied() {
            return hit;
        }
        let coverage = classify_glyph(window, base, slot, font_size, ch, cell_width);
        self.insert_wide(key, coverage);
        coverage
    }

    fn insert_wide(&mut self, key: (FontId, u32, char), coverage: Coverage) {
        if self.wide.len() >= WIDE_CACHE_LIMIT {
            self.wide.clear();
        }
        self.wide.insert(key, coverage);
    }

    #[cfg(test)]
    pub(super) fn insert_wide_for_test(
        &mut self,
        font_id: FontId,
        font_size: Pixels,
        ch: char,
        coverage: Coverage,
    ) {
        self.insert_wide((font_id, f(font_size).to_bits(), ch), coverage);
    }

    #[cfg(test)]
    pub(super) fn wide_len(&self) -> usize {
        self.wide.len()
    }
}

/// 按真实路径 shape 单个字符:落下的每个 run 都是主字体族、且总宽恰好一列,才算
/// 「可合并」。回退字族按**族名**比,不比 `FontId` —— 同一张字面在 gpui 里会因
/// features / fallbacks 不同注册成多个 id,而 `font_id_by_identifier` 只记最后一个。
/// (回退表现接的字面根本不进 `get_font_for_id` 的表,查出来是 `None`,同样算「不是
/// 主字体族」。)
///
/// 私用区字符另判:没落在主字体族上、或者落下的是 glyph 0(`.notdef`),就是
/// [`Coverage::Symbol`]。只对私用区这么判 —— 别的码位缺字时 `.notdef` 照旧并段,
/// 画面与以前一致。
pub(super) fn classify_glyph(
    window: &Window,
    base: &gpui::Font,
    slot: usize,
    font_size: Pixels,
    ch: char,
    cell_width: Pixels,
) -> Coverage {
    let text = SharedString::from(ch.to_string());
    let run = TextRun {
        len: text.len(),
        font: VariantFonts::variant(base, slot),
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let text_system = window.text_system();
    let shaped = text_system.shape_line(text, font_size, std::slice::from_ref(&run), None);
    let primary_only = shaped.runs.iter().all(|r| {
        text_system
            .get_font_for_id(r.font_id)
            .is_some_and(|font| font.family == base.family)
    });
    if symbols::is_private_use(ch) {
        let notdef = shaped
            .runs
            .iter()
            .any(|r| r.glyphs.iter().any(|g| g.id.0 == 0));
        if notdef || !primary_only {
            return Coverage::Symbol;
        }
    }
    if primary_only && (f(shaped.width) - f(cell_width)).abs() < 0.01 {
        Coverage::Merge
    } else {
        Coverage::Alone
    }
}

/// 参与「能否与相邻格子合并成一个 ShapedLine」判定的款式。
#[derive(Clone, Copy, PartialEq)]
pub(super) struct RunStyle {
    pub(super) fg: Hsla,
    pub(super) bold: bool,
    pub(super) italic: bool,
    pub(super) underline: Option<UnderlineStyle>,
    pub(super) strikethrough: Option<StrikethroughStyle>,
}

impl RunStyle {
    pub(super) fn same(&self, other: &Self) -> bool {
        self.fg == other.fg
            && self.bold == other.bold
            && self.italic == other.italic
            && underline_eq(&self.underline, &other.underline)
            && strikethrough_eq(&self.strikethrough, &other.strikethrough)
    }
}

pub(super) fn underline_eq(a: &Option<UnderlineStyle>, b: &Option<UnderlineStyle>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.thickness == b.thickness && a.color == b.color && a.wavy == b.wavy,
        _ => false,
    }
}

pub(super) fn strikethrough_eq(
    a: &Option<StrikethroughStyle>,
    b: &Option<StrikethroughStyle>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.thickness == b.thickness && a.color == b.color,
        _ => false,
    }
}

/// 光标形状 → 签名里的判别码(0 留给「不是光标格」)。
pub(super) fn cursor_code(shape: CursorShape) -> u8 {
    match shape {
        CursorShape::Block => 1,
        CursorShape::Underline => 2,
        CursorShape::Beam => 3,
        CursorShape::HollowBlock => 4,
        CursorShape::Hidden => 5,
    }
}

pub(super) fn cursor_shape(code: u8) -> Option<CursorShape> {
    Some(match code {
        1 => CursorShape::Block,
        2 => CursorShape::Underline,
        3 => CursorShape::Beam,
        4 => CursorShape::HollowBlock,
        5 => CursorShape::Hidden,
        _ => return None,
    })
}
/// 一行「内容变了、需要重建」的暂存。
///
/// `cells` 是它在 arena(`TerminalElementState::cells`)里的下标区间,不是自己的
/// 一份 Vec —— 每个未命中的行一次分配是刷屏时最贵的一笔。
pub(super) struct RowPending {
    pub(super) row: usize,
    pub(super) sig: u64,
    pub(super) cells: std::ops::Range<usize>,
}

/// 未 shape 的合并运行段。
pub(super) struct PendingRun {
    pub(super) start: usize,
    pub(super) len: usize,
    pub(super) text: String,
    pub(super) style: RunStyle,
}

/// 未 shape 的绘制片段(合并段落地后、或单格的宽字符)。
pub(super) struct PendingPiece {
    pub(super) start: usize,
    /// 合并段占的列数(段内每个格子都是窄字符、无组合符号,所以列数 = 字符数)。
    ///
    /// `None` = 单格片段(宽字符 / 缺字回退 / 带组合符号)。它本来就允许糊出格子
    /// 边界(模块注释的第二类),没有「该占几列」可言,不参与总宽校验。
    pub(super) cols: Option<usize>,
    pub(super) text: String,
    pub(super) style: RunStyle,
    /// `Some(n)` = 换内置图标字体画,图标至多占 `n` 列(见 [`symbol_columns`])。
    /// 只出现在单格片段上。
    pub(super) symbol: Option<usize>,
}

/// 内置图标字体画的这一格,图标至多能占几列。
///
/// 后面紧跟一个空格时放宽到两列:「图标 + 空格 + 文字」是提示符 / 状态栏的通行
/// 写法,1em 见方的图标硬缩进一列(0.6em 上下)小得像个点;借右边那个空格的位置
/// 就能按正文字号画(WezTerm 的 `allow_square_glyphs_to_overflow_width` 同一口径)。
/// 后面不是空格就老老实实一列,绝不压到别的字上。
///
/// 借格还要两格「底下长得一样」:底色(含反显换出来的底)、选区、查找高亮都相同,
/// 且两格都不是光标格。否则放大的图标有半截落在另一种底上 —— E2E 实拍:反显图标
/// 后跟普通空格,右半截白字画在白底上只剩一个「┤」;块光标压在图标上,图标按反白色
/// 画出两列宽,光标块被挖空成一个「[」。
///
/// powerline 分隔符这类色块字形(见 [`colors::is_fill_glyph`])永远一列 ——
/// 它们要跟隔壁段严丝合缝,多糊出去就盖住下一段的底色。
pub(super) fn symbol_columns(cell: &CellSignature, next: Option<&CellSignature>) -> usize {
    let followed_by_blank = next.is_some_and(|next| {
        next.col == cell.col + 1
            && next.ch == ' '
            && !next.has_zerowidth()
            && next.bg_default == cell.bg_default
            && (cell.bg_default || next.bg == cell.bg)
            && next.selected == cell.selected
            && next.search == cell.search
            && next.cursor == 0
            && cell.cursor == 0
    });
    if followed_by_blank && !colors::is_fill_glyph(cell.ch) {
        2
    } else {
        1
    }
}

/// 合并段总宽与「列数 × 列宽」的容差(px)。
///
/// 守恒的连字字体这里恒等于 0,给 0.5px 是留给浮点累加的余量 —— 半个像素以内
/// 肉眼看不出,超过就说明这个字体的连字压根不按等宽网格设计。
pub(super) const LIGATURE_WIDTH_SLACK: f32 = 0.5;

/// 合并段 shape 完还落在列格上吗。
///
/// 连字把 N 个字符换成别的字形是允许的,**换完总共还得占 N 列**——
/// 这是段内后续字符仍对得上列的唯一前提。见 [`super::theme::TerminalStyle::font`]。
pub(super) fn width_fits_columns(shaped: Pixels, cell_width: Pixels, cols: usize) -> bool {
    (f(shaped) - f(cell_width) * cols as f32).abs() <= LIGATURE_WIDTH_SLACK
}

/// 「⌥+点击定位光标」一次最多合成多少个方向键。
///
/// 手滑点到几千列开外时别把 PTY 灌爆:超了就整个不动,比走一半停下强
/// (走一半的结果既不是用户要的位置,又没法撤销)。
pub(super) const MAX_CURSOR_MOVE_STEPS: usize = 512;

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
pub(super) fn cursor_move_bytes(
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
///
/// `symbol_font`:主字体缺的私用区图标换这份字体画;`None` = 不接管,照旧走回退表。
#[allow(clippy::too_many_arguments)]
pub(super) fn build_row(
    window: &Window,
    cells: &[CellSignature],
    zerowidths: &[char],
    font: &gpui::Font,
    font_size: Pixels,
    variant_fonts: &VariantFonts,
    advance: &mut AdvanceCache,
    symbol_font: Option<&gpui::Font>,
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
    let mut separators: Vec<SeparatorPiece> = Vec::new();
    let mut cursor: Option<CursorLayout> = None;

    let mut bg_run: Option<(usize, usize, Hsla)> = None;
    let mut sel_run: Option<(usize, usize)> = None;
    let mut search_run: Option<(usize, usize, u8)> = None;
    let mut text_run: Option<PendingRun> = None;

    for (index, cell) in cells.iter().enumerate() {
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

        let has_zerowidth = cell.has_zerowidth();
        let slot = VariantFonts::slot(style_key.bold, style_key.italic);
        let run_font_id = variant_fonts.id_at(slot);
        // 字形归属只问窄字符(宽字符一律单独画)。光标格 / 带组合符号的格子本来就
        // 进不了合并段,不必 shape 着去问 —— 私用区除外:光标停在图标上,
        // 那一格照样得换图标字体画。
        let coverage = if !cell.flags.contains(Flags::WIDE_CHAR)
            && ((!has_zerowidth && cell.cursor == 0) || symbols::is_private_use(cell.ch))
        {
            advance.classify(
                window,
                font,
                slot,
                run_font_id,
                font_size,
                cell.ch,
                cell_width,
            )
        } else {
            Coverage::Alone
        };
        // 可合并的条件:窄字符、无组合符号、不是光标格(光标格颜色单独)、
        // 且主字体里有这个字形、步进恰好一列宽。
        let mergeable = coverage == Coverage::Merge && !has_zerowidth && cell.cursor == 0;
        let takeover = coverage == Coverage::Symbol && symbol_font.is_some();
        // 接管的 powerline 分隔符不进字体,按格子几何直接画(理由见 [`symbols`] 模块注释)
        let separator = takeover
            .then(|| symbols::powerline_separator(cell.ch))
            .flatten();
        let symbol =
            (takeover && separator.is_none()).then(|| symbol_columns(cell, cells.get(index + 1)));

        if let Some(separator) = separator {
            flush_text(&mut text_run, &mut pieces);
            separators.push(SeparatorPiece {
                origin: point(cell_width * col as f32, px(0.0)),
                separator,
                color: style_key.fg,
                stroke: symbols::separator_stroke(font_size),
            });
        } else if mergeable {
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
            for &z in cell.zerowidth(zerowidths) {
                text.push(z);
            }
            pieces.push(PendingPiece {
                start: col,
                cols: None,
                text,
                style: style_key,
                symbol,
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
        // 图标片段换内置字体、按格子缩字号;粗 / 斜不套(那份字体只有 Regular)。
        // 竖直位置不用另算:paint 按这个 run 自己的 ascent + descent 在行内居中。
        let (run_font, piece_size) = match (piece.symbol, symbol_font) {
            (Some(cols), Some(symbol_font)) => (
                symbol_font.clone(),
                symbols::symbol_font_size(font_size, cell_width, line_height, cols),
            ),
            _ => {
                let mut run_font = font.clone();
                if piece.style.bold {
                    run_font.weight = gpui::FontWeight::BOLD;
                }
                if piece.style.italic {
                    run_font.style = gpui::FontStyle::Italic;
                }
                (run_font, font_size)
            }
        };
        let run = TextRun {
            len: piece.text.len(),
            font: run_font,
            color: piece.style.fg,
            background_color: None,
            underline: piece.style.underline,
            strikethrough: piece.style.strikethrough,
        };
        let text = SharedString::from(piece.text);
        let mut shaped = window.text_system().shape_line(
            text.clone(),
            piece_size,
            std::slice::from_ref(&run),
            None,
        );

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
            shaped = window.text_system().shape_line(
                text,
                font_size,
                std::slice::from_ref(&plain),
                None,
            );
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
        separators,
        cursor,
    }
}

pub(super) fn translate(bounds: Bounds<Pixels>, delta: Point<Pixels>) -> Bounds<Pixels> {
    Bounds::new(bounds.origin + delta, bounds.size)
}

pub(super) fn flush_text(run: &mut Option<PendingRun>, out: &mut Vec<PendingPiece>) {
    if let Some(r) = run.take() {
        out.push(PendingPiece {
            start: r.start,
            cols: Some(r.len),
            text: r.text,
            style: r.style,
            symbol: None,
        });
    }
}

pub(super) fn flush_bg(
    run: &mut Option<(usize, usize, Hsla)>,
    cell: Size<Pixels>,
    out: &mut Vec<(Bounds<Pixels>, Hsla)>,
) {
    let Some((start, end, color)) = run.take() else {
        return;
    };
    out.push((rect_for(start, end, cell), color));
}

pub(super) fn flush_sel(
    run: &mut Option<(usize, usize)>,
    cell: Size<Pixels>,
    out: &mut Vec<Bounds<Pixels>>,
) {
    let Some((start, end)) = run.take() else {
        return;
    };
    out.push(rect_for(start, end, cell));
}

pub(super) fn flush_search(
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
pub(super) fn rect_for(start: usize, end: usize, cell: Size<Pixels>) -> Bounds<Pixels> {
    Bounds::new(
        point(cell.width * start as f32, px(0.0)),
        size(cell.width * (end - start + 1) as f32, cell.height),
    )
}

pub(super) fn underline_style(flags: Flags, color: Hsla) -> Option<UnderlineStyle> {
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
pub(super) fn report_metrics_once(
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
            tracing::warn!(
                "[mt-ui] 警告:字体 `{}` 解析结果不是等宽(M={m:.3} i={i:.3} W={w:.3}),\
                 终端逐列对齐会失效 —— 多半是这个字体族在本机不存在,被回退成了 UI 字体",
                style.font_family
            );
        }
        if std::env::var_os("MT_UI_DEBUG_METRICS").is_some() {
            tracing::info!(
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
pub(super) struct VariantFonts {
    pub(super) ids: [FontId; 4],
}

impl VariantFonts {
    pub(super) fn resolve(window: &Window, base: &gpui::Font) -> Self {
        let make = |bold: bool, italic: bool| {
            window
                .text_system()
                .resolve_font(&Self::variant(base, Self::slot(bold, italic)))
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

    /// 按档位给 `base` 套上粗 / 斜。
    pub(super) fn variant(base: &gpui::Font, slot: usize) -> gpui::Font {
        let mut font = base.clone();
        if slot & 1 != 0 {
            font.weight = gpui::FontWeight::BOLD;
        }
        if slot & 2 != 0 {
            font.style = gpui::FontStyle::Italic;
        }
        font
    }

    /// 四档变体的下标。[`AsciiAdvance`] 的表也按它分层。
    pub(super) fn slot(bold: bool, italic: bool) -> usize {
        usize::from(bold) + 2 * usize::from(italic)
    }

    pub(super) fn id(&self, bold: bool, italic: bool) -> FontId {
        self.ids[Self::slot(bold, italic)]
    }

    pub(super) fn id_at(&self, slot: usize) -> FontId {
        self.ids[slot]
    }
}
