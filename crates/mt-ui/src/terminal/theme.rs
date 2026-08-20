//! 终端配色与字体度量参数。
//!
//! 这里只是**渲染参数**,不是应用主题 —— 应用主题(gpui-component 那套 JSON 主题)
//! 由 `mt-config` / 主题桥负责,最终转成一份 [`TerminalTheme`] 递给
//! [`super::TerminalElement`]。

use gpui::{Hsla, Pixels, Rgba, SharedString, px};

/// 把 8bit RGB 转成 gpui 的 [`Hsla`]。alacritty 侧的颜色全是 `Rgb { r, g, b }`。
pub fn rgb8(r: u8, g: u8, b: u8) -> Hsla {
    Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

/// 终端配色表。索引 0..16 是 ANSI 16 色,256 色的色立方与灰阶按公式算,
/// truecolor 直接用转义序列里带的值。
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalTheme {
    /// 默认背景。**渲染时背景色等于它的格子不发 quad**,给背景图留透出的路。
    pub background: Hsla,
    pub foreground: Hsla,
    /// SGR 1(bold)命中默认前景时用的亮色。
    pub bright_foreground: Hsla,
    /// SGR 2(dim)命中默认前景时用的暗色。
    pub dim_foreground: Hsla,
    pub cursor: Hsla,
    /// 光标块底下那个字符的颜色(反白)。
    pub cursor_text: Hsla,
    /// 选择区高亮。带 alpha,叠在格子背景之上。
    pub selection: Hsla,
    pub ansi: [Hsla; 16],
}

impl Default for TerminalTheme {
    /// 对齐现有 xterm.js 侧的暗色配色(`src/utils/terminalCache.ts`)。
    fn default() -> Self {
        Self {
            background: rgb8(0x1a, 0x1a, 0x1a),
            foreground: rgb8(0xe6, 0xe6, 0xe6),
            bright_foreground: rgb8(0xff, 0xff, 0xff),
            dim_foreground: rgb8(0x9a, 0x9a, 0x9a),
            cursor: rgb8(0xe6, 0xe6, 0xe6),
            cursor_text: rgb8(0x1a, 0x1a, 0x1a),
            selection: Hsla {
                a: 0.30,
                ..rgb8(0x5c, 0x9c, 0xff)
            },
            ansi: [
                rgb8(0x1a, 0x1a, 0x1a), // 0 black
                rgb8(0xe5, 0x5f, 0x5f), // 1 red
                rgb8(0x5f, 0xd7, 0x87), // 2 green
                rgb8(0xe5, 0xc0, 0x7b), // 3 yellow
                rgb8(0x61, 0xaf, 0xef), // 4 blue
                rgb8(0xc6, 0x78, 0xdd), // 5 magenta
                rgb8(0x56, 0xb6, 0xc2), // 6 cyan
                rgb8(0xc8, 0xc8, 0xc8), // 7 white
                rgb8(0x6b, 0x6b, 0x6b), // 8 bright black
                rgb8(0xff, 0x7b, 0x7b), // 9 bright red
                rgb8(0x7d, 0xf2, 0xa5), // 10 bright green
                rgb8(0xff, 0xdb, 0x94), // 11 bright yellow
                rgb8(0x84, 0xc7, 0xff), // 12 bright blue
                rgb8(0xdd, 0x96, 0xf2), // 13 bright magenta
                rgb8(0x74, 0xd3, 0xdd), // 14 bright cyan
                rgb8(0xff, 0xff, 0xff), // 15 bright white
            ],
        }
    }
}

/// 查找命中的高亮配色(Ctrl+F 的两档底色 + 当前命中的描边)。
///
/// **刻意不并进 [`TerminalTheme`]**:旧版这三个色是写死在
/// `terminalSearch.ts` 的 `decorations` 里的,不随主题包走 —— 主题一换,
/// 「哪个是当前命中」这条最要紧的信息就可能被配色淹掉。默认值逐字照抄旧版,
/// 需要跟主题时由宿主自己算一份传进来。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SearchColors {
    /// 普通命中的底色(旧版 `matchBackground: #c8805a55`)。
    pub matched: Hsla,
    /// 当前命中的底色(旧版 `activeMatchBackground: #c8805aaa`)。
    pub current: Hsla,
    /// 当前命中的描边(旧版 `activeMatchBorder: #f0ece6`)。
    pub current_border: Hsla,
}

impl Default for SearchColors {
    fn default() -> Self {
        Self {
            matched: Hsla {
                a: 0x55 as f32 / 255.0,
                ..rgb8(0xc8, 0x80, 0x5a)
            },
            current: Hsla {
                a: 0xaa as f32 / 255.0,
                ..rgb8(0xc8, 0x80, 0x5a)
            },
            current_border: rgb8(0xf0, 0xec, 0xe6),
        }
    }
}

/// 终端字体参数。cell 宽高由这套参数经字体度量算出,不由调用方指定 ——
/// 逐列对齐的前提就是 cell 宽度**只有一个来源**。
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalStyle {
    /// 主字体族。必须是等宽字体。
    pub font_family: SharedString,
    /// 回退字体族(CJK / emoji / Nerd Font 图标)。主字体缺字时按顺序找。
    pub font_fallbacks: Vec<SharedString>,
    pub font_size: Pixels,
    /// 行高倍数(相对 font_size)。
    pub line_height: f32,
}

impl Default for TerminalStyle {
    fn default() -> Self {
        Self {
            // Windows 11 自带;Cascadia Mono 缺席时由 gpui 的 fallback 栈兜底。
            font_family: "Cascadia Mono".into(),
            font_fallbacks: vec![
                "Consolas".into(),
                "JetBrains Mono".into(),
                "Microsoft YaHei".into(),
                "Segoe UI Emoji".into(),
            ],
            font_size: px(14.0),
            line_height: 1.3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 查找高亮的三个色**逐字**照抄旧版 `terminalSearch.ts` 的 `decorations`。
    /// 这条钉住「换渲染器不换外观」——颜色对不上,用户第一眼就会说「不一样了」。
    #[test]
    fn 查找高亮配色对齐旧版() {
        let c = SearchColors::default();
        let base = rgb8(0xc8, 0x80, 0x5a);
        // #c8805a55 / #c8805aaa:同一个底色,两档不透明度
        assert_eq!(c.matched.h, base.h);
        assert_eq!(c.matched.s, base.s);
        assert_eq!(c.matched.l, base.l);
        assert!((c.matched.a - 0x55 as f32 / 255.0).abs() < 1e-6);
        assert_eq!(c.current.h, base.h);
        assert!((c.current.a - 0xaa as f32 / 255.0).abs() < 1e-6);
        assert!(c.current.a > c.matched.a, "当前命中必须更实");
        // #f0ece6:描边不透明
        assert_eq!(c.current_border, rgb8(0xf0, 0xec, 0xe6));
        assert_eq!(c.current_border.a, 1.0);
    }
}

impl TerminalStyle {
    /// 组装 gpui 的 [`gpui::Font`]。
    ///
    /// **连字必须关掉**:渲染是按「一个字符一列」摆的,`=>` 合成一个 glyph 会让
    /// 字符数与 glyph 数对不上,逐列对齐直接崩。终端里要连字得另设计,
    /// 不能靠字体的 `calt` 白送。
    pub fn font(&self) -> gpui::Font {
        gpui::Font {
            family: self.font_family.clone(),
            features: gpui::FontFeatures::disable_ligatures(),
            fallbacks: if self.font_fallbacks.is_empty() {
                None
            } else {
                Some(gpui::FontFallbacks::from_fonts(
                    self.font_fallbacks
                        .iter()
                        .map(|f| f.to_string())
                        .collect::<Vec<_>>(),
                ))
            },
            weight: gpui::FontWeight::NORMAL,
            style: gpui::FontStyle::Normal,
        }
    }

    pub fn line_height_px(&self) -> Pixels {
        // 取整:行高留小数会让第 N 行的 y 累积出半像素偏移,文字发虚。
        px((f32::from(self.font_size) * self.line_height).round().max(1.0))
    }
}
