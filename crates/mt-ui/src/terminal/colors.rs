//! alacritty 的 `Color` → gpui 的 `Hsla`。
//!
//! 三条来源按优先级:
//! 1. 转义序列里带的真彩值(`Color::Spec`)——直接用;
//! 2. OSC 4 / OSC 10-11 运行时改过的调色板(`term.colors()`)——覆盖主题;
//! 3. [`TerminalTheme`] 里的配色 —— 兜底。
//!
//! 解析完还有**最后一道**:[`ensure_contrast`] —— 前景与背景近似同色时把前景推开,
//! 见该函数的文档。

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use gpui::Hsla;

use super::theme::{TerminalTheme, rgb8};

fn from_rgb(rgb: Rgb) -> Hsla {
    rgb8(rgb.r, rgb.g, rgb.b)
}

/// 主题里 `NamedColor` 对应的颜色。`colors` 里若有 OSC 改过的值优先。
fn named(color: NamedColor, colors: &Colors, theme: &TerminalTheme) -> Hsla {
    if let Some(rgb) = colors[color] {
        return from_rgb(rgb);
    }
    match color {
        NamedColor::Foreground => theme.foreground,
        NamedColor::Background => theme.background,
        NamedColor::Cursor => theme.cursor,
        NamedColor::BrightForeground => theme.bright_foreground,
        NamedColor::DimForeground => theme.dim_foreground,
        // Dim 系没有独立配色时,按 alacritty 的做法拿基础色乘 2/3。
        NamedColor::DimBlack
        | NamedColor::DimRed
        | NamedColor::DimGreen
        | NamedColor::DimYellow
        | NamedColor::DimBlue
        | NamedColor::DimMagenta
        | NamedColor::DimCyan
        | NamedColor::DimWhite => {
            let base = theme.ansi[color as usize - NamedColor::DimBlack as usize];
            dim(base)
        }
        // 0..16 的具名 ANSI 色。
        other => theme
            .ansi
            .get(other as usize)
            .copied()
            .unwrap_or(theme.foreground),
    }
}

/// 256 色索引 → 颜色。16..232 是 6×6×6 色立方,232..256 是 24 级灰阶。
fn indexed(index: u8, colors: &Colors, theme: &TerminalTheme) -> Hsla {
    if let Some(rgb) = colors[index as usize] {
        return from_rgb(rgb);
    }
    match index {
        0..=15 => theme.ansi[index as usize],
        16..=231 => {
            let i = index as u32 - 16;
            let step = |v: u32| if v == 0 { 0u8 } else { (v * 40 + 55) as u8 };
            rgb8(step(i / 36), step((i / 6) % 6), step(i % 6))
        }
        232..=255 => {
            let level = (index as u32 - 232) * 10 + 8;
            rgb8(level as u8, level as u8, level as u8)
        }
    }
}

fn dim(color: Hsla) -> Hsla {
    Hsla {
        l: color.l * 0.66,
        ..color
    }
}

/// 解析一个 cell 的前景色。
///
/// - `BOLD` 命中 0..8 的具名色时提亮到对应的 bright 槽(xterm 的老规矩,
///   xterm.js 侧现有行为一致);
/// - `DIM` 走 alacritty 的 dim 槽;
/// - `HIDDEN` 由调用方处理(前景=背景),这里不管。
pub fn foreground(cell_fg: Color, flags: Flags, colors: &Colors, theme: &TerminalTheme) -> Hsla {
    match cell_fg {
        Color::Spec(rgb) => {
            let c = from_rgb(rgb);
            if flags.contains(Flags::DIM) { dim(c) } else { c }
        }
        Color::Named(name) => {
            let name = match (
                flags.contains(Flags::BOLD),
                flags.contains(Flags::DIM),
                name,
            ) {
                (true, false, n) => n.to_bright(),
                (false, true, n) => n.to_dim(),
                // bold + dim 同时点亮:alacritty 认 dim。
                (true, true, n) => n.to_dim(),
                (false, false, n) => n,
            };
            named(name, colors, theme)
        }
        Color::Indexed(i) => {
            // bold 提亮只对基础 8 色成立,超过就是用户明确点名的色号,不动它。
            let i = if flags.contains(Flags::BOLD) && i < 8 {
                i + 8
            } else {
                i
            };
            let c = indexed(i, colors, theme);
            if flags.contains(Flags::DIM) { dim(c) } else { c }
        }
    }
}

/// 解析一个 cell 的背景色。
pub fn background(cell_bg: Color, colors: &Colors, theme: &TerminalTheme) -> Hsla {
    match cell_bg {
        Color::Spec(rgb) => from_rgb(rgb),
        Color::Named(name) => named(name, colors, theme),
        Color::Indexed(i) => indexed(i, colors, theme),
    }
}

/// OSC 调色板查询(`ColorRequest`)的应答色。
///
/// # 这个函数解决什么
///
/// `OSC 4 ; n ; ?`(查第 n 号色)、`OSC 10/11/12 ; ?`(查前景/背景/光标)是 TUI
/// 程序**探测终端配色**的标准手段:vim 的 `background` 自动判定、delta / bat 的
/// 主题自适应、Claude Code 的配色协商全靠它。答错的直接后果是暗色终端里跑出
/// 一套浅色高亮(或反过来),而且用户完全不知道该去哪调。
///
/// alacritty 把 index 直接给成 [`NamedColor`] 的判别值,所以取值范围**不是**
/// 0..16,而是三段:
///
/// | index | 含义 |
/// |---|---|
/// | 0..=15 | ANSI 16 色 |
/// | 16..=255 | 256 色的色立方与灰阶(按公式算,不查表) |
/// | 256.. | [`NamedColor`] 的具名槽:前景 / 背景 / 光标 / Dim 系 / 亮前景 |
///
/// 只按 `theme.ansi.get(index)` 取、越界回前景的写法会让「查背景色」答成前景色 ——
/// 对比度算出来是 1.0,程序多半会认定这是一个纯黑终端。
///
/// `colors` 里 OSC 4 改过的值优先(程序自己刚设过的色,查回去必须是它设的那个)。
pub fn color_request_rgb(index: usize, colors: &Colors, theme: &TerminalTheme) -> Rgb {
    let hsla = match index {
        0..=15 => named_by_index(index, colors, theme),
        16..=255 => indexed(index as u8, colors, theme),
        _ => match named_from_index(index) {
            Some(name) => named(name, colors, theme),
            // 认不出来的槽位:回默认前景。比回黑色好 —— 至少是个「亮」的答案,
            // 不会让程序把终端判成纯黑。
            None => theme.foreground,
        },
    };
    to_rgb(hsla)
}

/// 0..16 走 `NamedColor`(这样 OSC 4 改过的值同样优先)。
fn named_by_index(index: usize, colors: &Colors, theme: &TerminalTheme) -> Hsla {
    match named_from_index(index) {
        Some(name) => named(name, colors, theme),
        None => theme.foreground,
    }
}

/// 判别值 → [`NamedColor`]。`NamedColor` 没有 `TryFrom<usize>`,只能自己列。
fn named_from_index(index: usize) -> Option<NamedColor> {
    use NamedColor::*;
    Some(match index {
        0 => Black,
        1 => Red,
        2 => Green,
        3 => Yellow,
        4 => Blue,
        5 => Magenta,
        6 => Cyan,
        7 => White,
        8 => BrightBlack,
        9 => BrightRed,
        10 => BrightGreen,
        11 => BrightYellow,
        12 => BrightBlue,
        13 => BrightMagenta,
        14 => BrightCyan,
        15 => BrightWhite,
        256 => Foreground,
        257 => Background,
        258 => Cursor,
        259 => DimBlack,
        260 => DimRed,
        261 => DimGreen,
        262 => DimYellow,
        263 => DimBlue,
        264 => DimMagenta,
        265 => DimCyan,
        266 => DimWhite,
        267 => BrightForeground,
        268 => DimForeground,
        _ => return None,
    })
}

/// [`Hsla`] → alacritty 的 [`Rgb`](应答里要写十六进制原值)。
pub fn to_rgb(color: Hsla) -> Rgb {
    let rgba = gpui::Rgba::from(color);
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgb {
        r: byte(rgba.r),
        g: byte(rgba.g),
        b: byte(rgba.b),
    }
}

/// 这个 cell 的背景是不是「默认背景」。
///
/// **这是背景图能透出来的唯一判据** —— 返回 true 的格子一律不发背景 quad。
/// 注意不能拿解析后的颜色去比:主题背景与某个 ANSI 色撞色时会误判成透明。
pub fn is_default_background(cell_bg: Color, flags: Flags) -> bool {
    matches!(cell_bg, Color::Named(NamedColor::Background)) && !flags.contains(Flags::INVERSE)
}

// ─────────────────────────── 最小对比度 ───────────────────────────

/// 前景/背景的最低对比度(WCAG AA 的正文档,与 VS Code、xterm.js 的推荐值同为 4.5)。
///
/// 逐字沿用 Tauri 版 `terminalCache.ts` 的 `minimumContrastRatio: 4.5` —— 那条配置
/// 是为 **Claude Code 的 AskUserQuestion 提问行**加的:它用近黑前景配默认背景,
/// 暗色主题下与底色几乎同色,不选中根本看不见(修复见 `0e1fea8`)。GPUI 迁移期
/// 随 `src/` 整块删除而丢失,这里是等价物。
///
/// 硬编码而不是配置项:旧版就是硬编码跑了一个多月的行为基线。真要可配,落点是
/// [`TerminalTheme`] 加一个字段(0 或 1.0 = 关闭),让主题包作者能对自己调好的
/// 低对比配色关掉它 —— 而不是加一个用户得先知道「对比度」是什么才会去开的开关。
pub const MIN_CONTRAST_RATIO: f32 = 4.5;

/// 这个格子有没有可见笔画。
///
/// 没有笔画的格子前景色不影响画面,可以整个跳过对比度修正 —— 一屏里空格占绝大
/// 多数,**这条是修正开销能被摊平的关键**(见 [`ContrastMemo`] 的性能说明)。
/// 注意空格也可能带下划线/删除线,那时前景色是画得出来的。
pub fn has_visible_ink(ch: char, flags: Flags) -> bool {
    ch != ' ' || flags.intersects(Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
}

/// 「拿字符当色块画」的字形:powerline 私用区 + 块元素/框线绘制。
///
/// # 为什么必须把它们排除在对比度修正之外
///
/// 对比度修正的前提是「前景是要**读**的字」。这两段不是:
///
/// - **powerline 分隔符**(`U+E0B0` 系那些实心三角/半圆)那一格的前景色语义是
///   **隔壁段的底色** —— 它跟本格背景一起拼出一条斜边。相邻两段的底色天然常
///   落在 4.5 以下(实测 ccstatusline 的 monokai 主题里紫→淡黄是 1.81、青→紫
///   是 1.64),修正会把三角推到近黑/近白,分隔处变成一块脏色;
/// - **块元素与框线**(`▄█▌▐░▒▓` / `─│┌┐`)同理 —— TUI 的面板边框、进度条、
///   半格块拼出来的图,前景色是「画」的一部分,不是可读性问题。
///
/// 这条豁免不是本仓的发明:Tauri 版靠 xterm.js 的 `minimumContrastRatio` 拿到
/// 同一效果,而 xterm.js 自带 `excludeFromContrastRatioDemands`(powerline
/// `U+E0A4..=U+E0D6` + 块/框 `U+2500..=U+259F`)。GPUI 重写时把 4.5 这个阈值搬
/// 过来了、**豁免名单漏了**,于是所有 powerline 提示符(Claude Code 状态栏、
/// oh-my-posh、starship)的三角箭头集体串色。范围逐字沿用 xterm.js 的两段。
pub fn is_fill_glyph(ch: char) -> bool {
    matches!(ch as u32, 0x2500..=0x259F | 0xE0A4..=0xE0D6)
}

/// 这一格该不该跑对比度修正 —— 有笔画、且不是拿来当色块用的字形。
///
/// 两个调用方(主终端 `element.rs` 与缩略图 `mini.rs`)共用这一条,别各判各的。
pub fn wants_contrast_fix(ch: char, flags: Flags) -> bool {
    has_visible_ink(ch, flags) && !is_fill_glyph(ch)
}

/// WCAG 的单通道线性化。
fn linearize(v: u8) -> f32 {
    let c = v as f32 / 255.0;
    if c <= 0.03928 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG 相对亮度。
fn relative_luminance(rgb: Rgb) -> f32 {
    0.2126 * linearize(rgb.r) + 0.7152 * linearize(rgb.g) + 0.0722 * linearize(rgb.b)
}

/// 两个亮度之间的对比度,值域 1.0..=21.0。
fn contrast_ratio(a: f32, b: f32) -> f32 {
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

/// 逐轮把每个通道压掉 10%,直到达标或触底。
///
/// 按通道等比推进而不是转 HSL 调 L,是 xterm.js 的刻意取舍(转换比整个修正还贵)。
/// 任一非零通道每轮至少减 1(`ceil`),所以一定收敛。
fn darken(fg: Rgb, bg_lum: f32, ratio: f32) -> Rgb {
    let mut out = fg;
    while contrast_ratio(relative_luminance(out), bg_lum) < ratio
        && (out.r > 0 || out.g > 0 || out.b > 0)
    {
        let step = |v: u8| v.saturating_sub((v as f32 * 0.1).ceil() as u8);
        out = Rgb {
            r: step(out.r),
            g: step(out.g),
            b: step(out.b),
        };
    }
    out
}

/// [`darken`] 的反向:逐轮把每个通道往 255 推 10%。
fn lighten(fg: Rgb, bg_lum: f32, ratio: f32) -> Rgb {
    let mut out = fg;
    while contrast_ratio(relative_luminance(out), bg_lum) < ratio
        && (out.r < 255 || out.g < 255 || out.b < 255)
    {
        let step = |v: u8| v.saturating_add(((255 - v) as f32 * 0.1).ceil() as u8);
        out = Rgb {
            r: step(out.r),
            g: step(out.g),
            b: step(out.b),
        };
    }
    out
}

/// 前景与背景对比度不足时,把前景推到达标为止;够了就原样返回。
///
/// # 为什么要**双向**试
///
/// 直觉做法是「暗前景就压得更暗、亮前景就提得更亮」,把它推离背景。但这在中间调
/// 背景上会失效:灰底(约 50% 亮度)上的近黑前景一路压到纯黑,对比度也只有 4 左右
/// —— 单向做法到这里就放弃了,等于没修。所以先按远离方向推一遍,**够不到就换另一
/// 个方向重推,取对比度高的那个**。少了这一步,中灰底(TUI 状态栏、选中行)上的
/// 低对比文字仍然看不见。
///
/// # 参照色的两条已知不准
///
/// 1. **背景图皮肤下 `bg` 是半透明的**(`theme_bridge::to_terminal_theme` 带图时会
///    给 `TerminalTheme::background` 打 alpha),真正在后面的是氛围图,亮度不可知。
///    这里与 xterm.js 一样**只看 RGB、忽略 alpha**,算出来是名义对比度;图特别亮的
///    区域仍可能偏淡。
/// 2. 选中/查找命中的半透明高亮是**画在文字之下、修正之后**的,不进参照。
///
/// 两条都是旧版就有的口径,不是本次引入的回退。
///
/// `ratio <= 1.0` 视为关闭。前景的 alpha 原样保留。
pub fn ensure_contrast(fg: Hsla, bg: Hsla, ratio: f32) -> Hsla {
    if ratio <= 1.0 {
        return fg;
    }
    let fg_rgb = to_rgb(fg);
    let bg_lum = relative_luminance(to_rgb(bg));
    let fg_lum = relative_luminance(fg_rgb);
    if contrast_ratio(fg_lum, bg_lum) >= ratio {
        return fg;
    }

    // 先往「远离背景」的方向推
    let (away, back): (fn(Rgb, f32, f32) -> Rgb, fn(Rgb, f32, f32) -> Rgb) = if fg_lum < bg_lum {
        (darken, lighten)
    } else {
        (lighten, darken)
    };
    let first = away(fg_rgb, bg_lum, ratio);
    let first_ratio = contrast_ratio(relative_luminance(first), bg_lum);
    let best = if first_ratio < ratio {
        // 推到头还是不够:换方向重推,取更好的那个(两边都不达标时也要取最优,
        // 「都不达标就放弃」会把中灰底上的近黑文字原样留下)
        let second = back(fg_rgb, bg_lum, ratio);
        if first_ratio >= contrast_ratio(relative_luminance(second), bg_lum) {
            first
        } else {
            second
        }
    } else {
        first
    };
    Hsla {
        a: fg.a,
        ..rgb8(best.r, best.g, best.b)
    }
}

/// `(前景, 背景) → 修正后前景` 的小型轮转缓存。
///
/// # 为什么非有不可
///
/// 取色发生在**每帧遍历全部可见格子**的那个循环里(`element.rs` 的 `display_iter`),
/// 行缓存只缓存 shaping、不缓存取色。一次相对亮度是 3 次 `powf`,一屏 200×50 的
/// 格子按前后景各算一次就是 6 万次 `powf`/帧,60fps 下能吃掉 1~2ms —— 纯粹为了
/// 得出「绝大多数格子本来就达标」这个结论。
///
/// 两条对策叠加基本抹平:调用方先用 [`has_visible_ink`] 滤掉空格(一屏的大头),
/// 剩下的走这里 —— 终端一屏用到的色对通常是个位数,线性扫几条 f32 比较远比
/// `powf` 便宜。槽位满了轮转覆盖:配色异常丰富的画面(真彩色图片、彩虹输出)
/// 退化成每格现算,与没有缓存时持平,不会更差。
///
/// **`ratio` 不进键**:调用方每帧新建一个 memo,阈值在一帧内恒定。要把 memo 提成
/// 跨帧缓存的话,阈值必须一起进键或在阈值变化时清空。
pub struct ContrastMemo {
    slots: [Option<(Hsla, Hsla, Hsla)>; Self::SLOTS],
    next: usize,
}

impl Default for ContrastMemo {
    fn default() -> Self {
        Self {
            slots: [None; Self::SLOTS],
            next: 0,
        }
    }
}

impl ContrastMemo {
    const SLOTS: usize = 8;

    /// 查缓存,没有就算一次 [`ensure_contrast`] 并记下。
    pub fn resolve(&mut self, fg: Hsla, bg: Hsla, ratio: f32) -> Hsla {
        for (cached_fg, cached_bg, fixed) in self.slots.iter().flatten() {
            if *cached_fg == fg && *cached_bg == bg {
                return *fixed;
            }
        }
        let fixed = ensure_contrast(fg, bg, ratio);
        self.slots[self.next] = Some((fg, bg, fixed));
        self.next = (self.next + 1) % Self::SLOTS;
        fixed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_查询答的是真配色而不是占位值() {
        let theme = TerminalTheme::default();
        let colors = Colors::default();

        // 具名槽:背景 / 前景 / 光标各答各的,不能一律回前景
        assert_eq!(color_request_rgb(257, &colors, &theme), to_rgb(theme.background));
        assert_eq!(color_request_rgb(256, &colors, &theme), to_rgb(theme.foreground));
        assert_eq!(color_request_rgb(258, &colors, &theme), to_rgb(theme.cursor));
        assert_ne!(
            color_request_rgb(257, &colors, &theme),
            color_request_rgb(256, &colors, &theme),
            "查背景答成前景 → 程序算出的对比度是 1.0,会把终端判成纯黑"
        );

        // ANSI 16 色
        assert_eq!(color_request_rgb(1, &colors, &theme), to_rgb(theme.ansi[1]));
        assert_eq!(color_request_rgb(15, &colors, &theme), to_rgb(theme.ansi[15]));

        // 256 色的色立方与灰阶按公式算
        assert_eq!(color_request_rgb(196, &colors, &theme), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(color_request_rgb(232, &colors, &theme), Rgb { r: 8, g: 8, b: 8 });

        // 认不出来的槽位不崩
        let _ = color_request_rgb(9999, &colors, &theme);
    }

    #[test]
    fn osc_4_改过的调色板优先于主题() {
        let theme = TerminalTheme::default();
        let mut colors = Colors::default();
        let custom = Rgb {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        };
        colors[NamedColor::Red] = Some(custom);
        // 程序自己刚设过的色,查回去必须是它设的那个
        assert_eq!(color_request_rgb(1, &colors, &theme), custom);

        colors[NamedColor::Background] = Some(custom);
        assert_eq!(color_request_rgb(257, &colors, &theme), custom);
    }

    // ── 最小对比度

    fn ratio_of(fg: Hsla, bg: Hsla) -> f32 {
        contrast_ratio(
            relative_luminance(to_rgb(fg)),
            relative_luminance(to_rgb(bg)),
        )
    }

    fn lum_of(color: Hsla) -> f32 {
        relative_luminance(to_rgb(color))
    }

    #[test]
    fn 对比度够的色对原样不动() {
        let theme = TerminalTheme::default();
        assert!(ratio_of(theme.foreground, theme.background) >= MIN_CONTRAST_RATIO);
        assert_eq!(
            ensure_contrast(theme.foreground, theme.background, MIN_CONTRAST_RATIO),
            theme.foreground,
            "达标的色对必须一个 bit 都不改 —— 否则整套主题配色会被悄悄改写"
        );
    }

    #[test]
    fn 暗底上的近黑前景被提亮到达标() {
        // 这就是 Claude Code 的 AskUserQuestion 提问行:近黑前景配默认暗背景
        let bg = rgb8(0x1a, 0x1a, 0x1a);
        let fg = rgb8(0x30, 0x30, 0x30);
        assert!(
            ratio_of(fg, bg) < MIN_CONTRAST_RATIO,
            "前提:这一对原本就是看不见的"
        );

        let fixed = ensure_contrast(fg, bg, MIN_CONTRAST_RATIO);
        assert!(ratio_of(fixed, bg) >= MIN_CONTRAST_RATIO);
        assert!(lum_of(fixed) > lum_of(fg), "暗底上只能往亮里推");
    }

    #[test]
    fn 亮底上的近白前景被压暗到达标() {
        let bg = rgb8(0xfa, 0xfa, 0xfa);
        let fg = rgb8(0xf0, 0xf0, 0xf0);
        assert!(ratio_of(fg, bg) < MIN_CONTRAST_RATIO);

        let fixed = ensure_contrast(fg, bg, MIN_CONTRAST_RATIO);
        assert!(ratio_of(fixed, bg) >= MIN_CONTRAST_RATIO);
        assert!(lum_of(fixed) < lum_of(fg), "亮底上只能往暗里推");
    }

    #[test]
    fn 中灰底上单向推到头不够时换方向() {
        // 单向做法(「暗前景就压得更暗」)在中间调背景上会失效:这个底色上前景
        // 一路压到纯黑也只有 3.9x,到这里就放弃 = 等于没修。必须换方向重推。
        let bg = rgb8(0x6b, 0x6b, 0x6b);
        let fg = rgb8(0x1a, 0x1a, 0x1a);
        assert!(ratio_of(fg, bg) < MIN_CONTRAST_RATIO);
        assert!(
            ratio_of(rgb8(0, 0, 0), bg) < MIN_CONTRAST_RATIO,
            "前提:这个底色上连纯黑都不达标,才轮得到反向兜底"
        );

        let fixed = ensure_contrast(fg, bg, MIN_CONTRAST_RATIO);
        assert!(ratio_of(fixed, bg) >= MIN_CONTRAST_RATIO);
        assert!(
            lum_of(fixed) > lum_of(bg),
            "压暗那侧封顶了,结果必须落在背景的另一侧"
        );
    }

    #[test]
    fn 两侧都推到头仍不达标时取更优的一侧() {
        // 4.5 这个阈值下总有一侧够得着(纯黑/纯白两个 ratio 的较大者下界是 4.58),
        // 所以这条分支要用极端阈值才走得到。它的意义是「都不达标也别放弃」。
        let bg = rgb8(0x80, 0x80, 0x80);
        // 比背景略亮 → 优先往亮里推,但这个底色上提到纯白只有 3.9x、
        // 压到纯黑有 5.3x —— 优先方向恰恰是差的那个
        let fg = rgb8(0x85, 0x85, 0x85);

        let fixed = ensure_contrast(fg, bg, 21.0);
        assert_eq!(
            to_rgb(fixed),
            Rgb { r: 0, g: 0, b: 0 },
            "两侧都不达标时要取对比度高的那一侧,而不是优先方向那一侧"
        );
        assert!(ratio_of(fixed, bg) > ratio_of(fg, bg));
    }

    #[test]
    fn 阈值不高于_1_视为关闭() {
        let bg = rgb8(0x1a, 0x1a, 0x1a);
        let fg = rgb8(0x1b, 0x1b, 0x1b);
        assert_eq!(ensure_contrast(fg, bg, 1.0), fg);
        assert_eq!(ensure_contrast(fg, bg, 0.0), fg);
    }

    #[test]
    fn 前景的_alpha_原样保留() {
        let bg = rgb8(0x1a, 0x1a, 0x1a);
        let fg = Hsla {
            a: 0.5,
            ..rgb8(0x20, 0x20, 0x20)
        };
        let fixed = ensure_contrast(fg, bg, MIN_CONTRAST_RATIO);
        assert_ne!(to_rgb(fixed), to_rgb(fg), "前提:这一对确实被改过");
        assert_eq!(fixed.a, 0.5);
    }

    #[test]
    fn 极端色对不死循环也不倒退() {
        for (fg, bg) in [
            (rgb8(0, 0, 0), rgb8(0, 0, 0)),
            (rgb8(255, 255, 255), rgb8(255, 255, 255)),
            (rgb8(0, 0, 0), rgb8(1, 1, 1)),
            (rgb8(255, 255, 255), rgb8(254, 254, 254)),
            (rgb8(0x80, 0x00, 0x40), rgb8(0x80, 0x00, 0x40)),
        ] {
            let fixed = ensure_contrast(fg, bg, MIN_CONTRAST_RATIO);
            assert!(
                ratio_of(fixed, bg) >= ratio_of(fg, bg),
                "修完不许比原样还差"
            );
        }
    }

    #[test]
    fn 空格没有笔画但带下划线的空格有() {
        assert!(!has_visible_ink(' ', Flags::empty()));
        assert!(has_visible_ink('a', Flags::empty()));
        assert!(
            has_visible_ink(' ', Flags::UNDERLINE),
            "下划线用的是前景色,空格也画得出来"
        );
        assert!(has_visible_ink(' ', Flags::STRIKEOUT));
    }

    #[test]
    fn 色块类字形豁免对比度修正() {
        // 三组都是从截图逐像素量出来的真实色对(ccstatusline 的 monokai powerline
        // 配本仓默认底),原始对比度 1.98 / 1.81 / 1.64,右列是没有豁免时被推成的
        // 那个脏色 —— 分隔符三角当时就是这么串色的。
        for (fg, bg, 串成) in [
            (rgb8(0x44, 0x44, 0x44), rgb8(0x0d, 0x0c, 0x1e), rgb8(0x86, 0x86, 0x86)),
            (rgb8(0xaf, 0x87, 0xff), rgb8(0xd7, 0xd7, 0x87), rgb8(0x65, 0x4e, 0x95)),
            (rgb8(0x5f, 0xd7, 0xff), rgb8(0xaf, 0x87, 0xff), rgb8(0x18, 0x39, 0x46)),
        ] {
            assert_eq!(
                to_rgb(ensure_contrast(fg, bg, MIN_CONTRAST_RATIO)),
                to_rgb(串成),
                "前提:这一对不豁免的话确实会被改写成这个脏色"
            );
        }

        // powerline 分隔符/端帽:那一格的前景是「隔壁段的底色」,不是要读的字
        for ch in ['\u{e0b0}', '\u{e0b2}', '\u{e0b4}', '\u{e0b6}'] {
            assert!(is_fill_glyph(ch));
            assert!(!wants_contrast_fix(ch, Flags::empty()));
        }
        // 块元素与框线同理:TUI 边框、进度条、半格块拼的图都是「画」
        for ch in ['█', '▄', '▌', '░', '─', '┌'] {
            assert!(!wants_contrast_fix(ch, Flags::empty()));
        }
        // 正文照常修正,空格照常跳过
        assert!(wants_contrast_fix('a', Flags::empty()));
        assert!(wants_contrast_fix('中', Flags::empty()));
        assert!(!wants_contrast_fix(' ', Flags::empty()));

        // 两段范围的上下界各差一位都不许误伤
        for (ch, 命中) in [
            ('\u{24ff}', false),
            ('\u{2500}', true),
            ('\u{259f}', true),
            ('\u{25a0}', false),
            ('\u{e0a3}', false),
            ('\u{e0a4}', true),
            ('\u{e0d6}', true),
            ('\u{e0d7}', false),
        ] {
            assert_eq!(is_fill_glyph(ch), 命中, "U+{:04X}", ch as u32);
        }
    }

    #[test]
    fn memo_轮转覆盖后仍与直算一致() {
        let mut memo = ContrastMemo::default();
        let bg = rgb8(0x1a, 0x1a, 0x1a);
        // 20 组色对把 8 个槽位轮转好几圈,再复查一遍每一组
        let fgs: Vec<Hsla> = (0..20u8).map(|i| rgb8(i * 12, i * 9, i * 5)).collect();
        for _ in 0..3 {
            for fg in &fgs {
                assert_eq!(
                    memo.resolve(*fg, bg, MIN_CONTRAST_RATIO),
                    ensure_contrast(*fg, bg, MIN_CONTRAST_RATIO)
                );
            }
        }
    }
}
