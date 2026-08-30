//! 终端配色与字体度量参数。
//!
//! 这里只是**渲染参数**,不是应用主题 —— 应用主题(gpui-component 那套 JSON 主题)
//! 由 `mt-config` / 主题桥负责,最终转成一份 [`TerminalTheme`] 递给
//! [`super::TerminalElement`]。

use std::cell::RefCell;

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

/// 默认等宽字族栈(主字体 + 回退),按平台选。
///
/// **主字体必须随平台走**:`font_fallbacks` 只在主字体缺**字形**时才往下找,
/// 主字体族本身不存在时 gpui 不会顺着回退表试,而是直接落到平台 UI 字体 ——
/// 那是比例字体,[`super::TerminalElement`] 的逐列对齐当场失效(症状是每个字形
/// 钉在列格左沿、窄字母后面拖一片空白)。原先三平台共用 Windows 那一套,
/// macOS / Linux 上五个名字全落空,没手填字族就是开箱即坏。
///
/// 取值一律挑该平台**开箱即有**的:Windows 11 的 Cascadia Mono、macOS 自带的
/// Menlo(SF Mono 没被登记成 CoreText family,点名点不到)、主流发行版随
/// fontconfig 一起装的 DejaVu Sans Mono。
fn default_font_stack() -> (&'static str, &'static [&'static str]) {
    if cfg!(target_os = "macos") {
        ("Menlo", &["Monaco", "PingFang SC", "Apple Color Emoji"])
    } else if cfg!(target_os = "linux") {
        (
            "DejaVu Sans Mono",
            &[
                "Noto Sans Mono",
                "Liberation Mono",
                "Noto Sans CJK SC",
                "Noto Color Emoji",
            ],
        )
    } else {
        (
            "Cascadia Mono",
            &[
                "Consolas",
                "JetBrains Mono",
                "Microsoft YaHei",
                "Segoe UI Emoji",
            ],
        )
    }
}

impl Default for TerminalStyle {
    fn default() -> Self {
        let (family, fallbacks) = default_font_stack();
        Self {
            font_family: family.into(),
            font_fallbacks: fallbacks.iter().map(|f| SharedString::from(*f)).collect(),
            font_size: px(14.0),
            line_height: 1.3,
            // 默认关:三家的默认字族都是去连字版,开了也没东西可连,徒增一次总宽
            // 校验。要连字的用户得先把字族换成 Cascadia Code 这类带 `calt` 表的
            // 字体 —— 设置页那行提示说的就是这件事。
            ligatures: false,
        }
    }
}

/// [`TerminalStyle::font`] 的缓存键 —— 就是那个方法真正读到的三个字段。
///
/// 字号 / 行高**不在**里面:它们不参与 Font 的组装(字号是 shape 时另给的参数)。
struct FontKey {
    family: SharedString,
    fallbacks: Vec<SharedString>,
    ligatures: bool,
}

impl FontKey {
    fn of(style: &TerminalStyle) -> Self {
        Self {
            family: style.font_family.clone(),
            fallbacks: style.font_fallbacks.clone(),
            ligatures: style.ligatures,
        }
    }

    /// 先比布尔与条数,不相等就不必去比字符串。
    fn matches(&self, style: &TerminalStyle) -> bool {
        self.ligatures == style.ligatures
            && self.fallbacks.len() == style.font_fallbacks.len()
            && self.family == style.font_family
            && self.fallbacks == style.font_fallbacks
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
        assert!(!style.ligatures, "默认关:三家默认字族都是去连字版");
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

    /// [`TerminalStyle::font`] 带缓存,而这个结构体的字段是 `pub` 且可变的 ——
    /// 缓存键必须盖住它读到的**每一个**字段,少一个就会发回一份陈的字体
    /// (只在「先问过一次再改字段」这个顺序下复现,是个很难查的鬼故事)。
    #[test]
    fn 字体缓存跟着字段走() {
        let mut style = TerminalStyle::default();
        assert_eq!(style.font().family, style.font_family);

        style.font_family = "Fira Code".into();
        assert_eq!(style.font().family, SharedString::from("Fira Code"));

        style.font_fallbacks = vec!["Consolas".into()];
        let fallbacks = style.font().fallbacks.expect("回退列表还在");
        assert_eq!(fallbacks.fallback_list(), ["Consolas".to_string()]);

        style.font_fallbacks.clear();
        assert!(style.font().fallbacks.is_none(), "回退清空了就不该再发旧列表");

        // 同一份样式连问两次必须一模一样(命中缓存那条路)
        assert_eq!(style.font(), style.font());
    }

    /// 默认字族栈必须是**本平台**开箱即有的等宽字体。
    ///
    /// 曾经三平台共用 Windows 那一套(Cascadia Mono + Consolas / JetBrains Mono /
    /// Microsoft YaHei / Segoe UI Emoji),macOS 与 Linux 上五个名字一个都点不到。
    /// 主字体族点不到时 gpui 不会顺着回退表试,而是直接回落平台 UI 字体,于是
    /// 终端拿比例字体去做逐列对齐 —— 没手填字族的 mac 用户开箱就撞上。
    /// 这条钉住三家各自的主字体与 emoji 回退,别再退回单一平台。
    #[test]
    fn 默认字族栈按平台选() {
        let style = TerminalStyle::default();
        let (family, emoji) = if cfg!(target_os = "macos") {
            ("Menlo", "Apple Color Emoji")
        } else if cfg!(target_os = "linux") {
            ("DejaVu Sans Mono", "Noto Color Emoji")
        } else {
            ("Cascadia Mono", "Segoe UI Emoji")
        };
        assert_eq!(style.font_family.as_ref(), family);
        assert!(
            style.font_fallbacks.iter().any(|f| f.as_ref() == emoji),
            "缺本平台 emoji 回退 {emoji}"
        );
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
    ///
    /// # 为什么带缓存,又为什么缓存不挂在自己身上
    ///
    /// 造一份 Font 要七八次堆分配(4 个回退字族名各一次 `to_string`、装它们的 Vec、
    /// `FontFallbacks` 与 `FontFeatures` 两个 Arc、features 里那个小 vec 与
    /// `"calt"`),而**每帧每个 pane 都要造一次**(`element.rs` 的 prepaint 开头、
    /// mini 预览同理),内容却几乎从不变。
    ///
    /// 缓存没有做成 `TerminalStyle` 的字段,是因为这个结构体的字段是 `pub` 且
    /// 可变的:宿主用 `TerminalStyle { .., ..Default::default() }` 造完还会接着改
    /// `ligatures` / 字号(本文件的 `连字开关只切_calt` 测试就是这个形状),
    /// 惰性缓存一填就有读到陈值的路。而且加私有字段会直接堵死跨 crate 的
    /// `..Default::default()` 构造(E0451)。
    ///
    /// 所以缓存放在线程局部的一张小表上,键就是 `font()` **真正读到**的那三个
    /// 字段 —— 改完任何一个立刻算另一份,不存在陈值。表按线性扫描:同时在用的样式
    /// 最多两三份(主终端 + 预览),比哈希一串字符串还便宜。
    pub fn font(&self) -> gpui::Font {
        thread_local! {
            static MEMO: RefCell<Vec<(FontKey, gpui::Font)>> = const { RefCell::new(Vec::new()) };
        }
        MEMO.with(|memo| {
            let mut memo = memo.borrow_mut();
            if let Some((_, font)) = memo.iter().find(|(key, _)| key.matches(self)) {
                // Font 的 clone 只是引用计数:family 是 SharedString,
                // features / fallbacks 各是一个 Arc(gpui 0.2.2),不碰堆
                return font.clone();
            }
            let font = self.build_font();
            // 涨过头就整表丢掉重来 —— 设置页里逐字符改字族名时不该把每个中间值都留着
            if memo.len() >= 8 {
                memo.clear();
            }
            memo.push((FontKey::of(self), font.clone()));
            font
        })
    }

    /// 真正组装一份 [`gpui::Font`]。缓存未命中时才走。
    fn build_font(&self) -> gpui::Font {
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
