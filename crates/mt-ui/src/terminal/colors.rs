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

/// 这个 cell 的背景是不是「默认背景」。
///
/// **这是背景图能透出来的唯一判据** —— 返回 true 的格子一律不发背景 quad。
/// 注意不能拿解析后的颜色去比:主题背景与某个 ANSI 色撞色时会误判成透明。
pub fn is_default_background(cell_bg: Color, flags: Flags) -> bool {
    matches!(cell_bg, Color::Named(NamedColor::Background)) && !flags.contains(Flags::INVERSE)
}
