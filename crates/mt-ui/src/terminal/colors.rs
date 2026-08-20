//! alacritty 的 `Color` → gpui 的 `Hsla`。
//!
//! 三条来源按优先级:
//! 1. 转义序列里带的真彩值(`Color::Spec`)——直接用;
//! 2. OSC 4 / OSC 10-11 运行时改过的调色板(`term.colors()`)——覆盖主题;
//! 3. [`TerminalTheme`] 里的配色 —— 兜底。

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
}
