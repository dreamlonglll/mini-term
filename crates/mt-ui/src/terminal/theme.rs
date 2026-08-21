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
    /// 连体字(`=>` `!=` `->` 合成一个字形)。见 [`TerminalStyle::font`]。
    pub ligatures: bool,
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
            // 默认关:默认字族 Cascadia **Mono** 本来就是去连字版,开了也没东西可连,
            // 徒增一次总宽校验。要连字的用户得先把字族换成 Cascadia Code 这类带
            // `calt` 表的字体 —— 设置页那行提示说的就是这件事。
            ligatures: false,
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

    /// 连字开关只动 `calt` 一个 feature —— 编程连字(`=>` `!=` `->`)恰好全在它里面。
    ///
    /// **两档都必须是显式值,`None`(空 features)是错的**:gpui 的 Windows 后端
    /// 无条件下发 `SetTypography`,空 typography 会被 DirectWrite 当成「一个排版特性
    /// 都不要」,连字反而全灭。这条钉住那个坑,见 [`TerminalStyle::font`]。
    #[test]
    fn 连字开关只切_calt() {
        let mut style = TerminalStyle::default();
        assert!(!style.ligatures, "默认关:默认字族 Cascadia Mono 是去连字版");
        assert_eq!(style.font().features.is_calt_enabled(), Some(false));

        style.ligatures = true;
        assert_eq!(
            style.font().features.is_calt_enabled(),
            Some(true),
            "开 = 显式 calt=1,**不能**是 None —— 空 features 会被 DirectWrite 当成全关"
        );
        // 其余解析结果不受连字开关影响
        assert_eq!(style.font().family, style.font_family);
        assert_eq!(style.font().weight, gpui::FontWeight::NORMAL);
    }
}

impl TerminalStyle {
    /// 组装 gpui 的 [`gpui::Font`]。
    ///
    /// # 连体字为什么开得起
    ///
    /// 这里原先硬关 `calt`,理由写的是「`=>` 合成一个 glyph 会让字符数与 glyph 数
    /// 对不上,逐列对齐直接崩」—— 那说的是 gpui `shape_line(.., force_width)`
    /// 那条按 glyph 序号硬掰位置的路,而本渲染器**从来没走那条路**
    /// (见 `mt_ui::terminal::element` 的模块注释)。现在的摆法是:同款式相邻窄
    /// 字符合并成一段、整段一次 shape,段的原点钉死在 `cell_width × 起始列`,
    /// 段内位置由 shaping 的自然步进给出。于是
    ///
    /// - 连字总 advance 守恒(编程连字字体的通行设计:N 个字符 → N 列宽)时,
    ///   段内后续字符照旧落在列格上;
    /// - 万一某个字体不守恒,错位也**只在这一段里** —— 段与段之间各自按列定位,
    ///   传不到下一段。`build_row` 另有一道总宽校验把这一段也救回来。
    ///
    /// 背景 / 选区 / 查找高亮 / 光标 / 鼠标命中一律按列独立算,一个 glyph 都不看。
    ///
    /// 动的只有 `calt` 一个 tag —— 编程连字恰好全在它里面。
    ///
    /// ⚠️ **开的那一档必须显式给 `calt = 1`,不能图省事传 `FontFeatures::default()`**:
    /// gpui 的 Windows 后端**无条件**调 `IDWriteTextLayout::SetTypography`
    /// (`direct_write.rs` 的 `layout_line`),而它的 `apply_font_features` 对空
    /// features 直接 return —— 于是交给 DirectWrite 的是一个**空 typography 对象**,
    /// 那被理解成「显式指定了排版特性、且一个都不要」,liga/clig/calt 反而全灭。
    /// 空 features ≠ 平台默认,这条 2026-08-21 实测栽过。
    /// 显式给了值之后 gpui 会连 `liga`/`clig` 一起补成 1,三个 tag 都到位。
    pub fn font(&self) -> gpui::Font {
        gpui::Font {
            family: self.font_family.clone(),
            features: if self.ligatures {
                gpui::FontFeatures(std::sync::Arc::new(vec![("calt".into(), 1)]))
            } else {
                gpui::FontFeatures::disable_ligatures()
            },
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
