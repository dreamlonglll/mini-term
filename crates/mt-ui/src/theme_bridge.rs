//! 主题桥：mini-term 的外置主题包 → 终端配色 + gpui-component 主题层。
//!
//! # 一份 theme.json，两个下游
//!
//! ```text
//!                        ┌── TerminalTheme ─────→ TerminalElement（16 色/前后景/光标/选择）
//! themes/<id>/theme.json ┤
//!  (mt_config::ThemePacks)└── gpui_component::ThemeConfig → Theme 全局（面板/按钮/边框/tab）
//! ```
//!
//! 原来这两条在 Web 侧是「CSS 变量」与「xterm setTheme」，各走各的；GPUI 侧
//! 后者换成 gpui-component 自带的 JSON 主题层 + 运行时注册表，前者仍是我们自己的
//! [`TerminalTheme`]。**语义映射逐条对齐 `src/utils/themePackManager.ts`** ——
//! 同一个皮肤包在新旧两版里必须长得一样，否则用户会以为是自己的包坏了。
//!
//! # 为什么解析放在 mt-ui 而不是 mt-config
//!
//! `mt-config` 明确不依赖 gpui（它的文件层测试要能脱离 GPUI 跑）。而映射的产物
//! 全是 gpui 类型（`Hsla` / `ThemeConfig`），所以校验与映射整块归 mt-ui，
//! mt-config 只管「目录里有哪些包、原文是什么」。这条分界是 `theme_packs.rs`
//! 模块注释里就写好的。
//!
//! # 背景图
//!
//! 本模块只出**数据**（[`BackgroundArt`]：图片路径、焦点、压暗色），渲染在
//! [`crate::background`]：cover/contain 的 bounds 自算 + `focus` 的百分比定位 +
//! 压暗纱罩。终端侧「默认背景不发 quad」+ 半透明的 `TerminalTheme::background`
//! 是它能透上来的前提，两边合起来才是一套。

use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context as _, Result, anyhow, bail};
use gpui::{App, Hsla, Rgba, Window};
use gpui_component::{Theme, ThemeConfig, ThemeMode, ThemeRegistry};
use serde::Deserialize;

use crate::terminal::{TerminalTheme, rgb8};

// ───────────────────────── theme.json 的形状 ─────────────────────────

/// theme.json 的 10 个语义色（Dream Skin 契约）。前 7 个必填。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemePackColors {
    pub background: String,
    pub panel: String,
    pub panel_alt: String,
    pub accent: String,
    pub text: String,
    pub muted: String,
    pub line: String,
    #[serde(default)]
    pub accent_alt: Option<String>,
    #[serde(default)]
    pub secondary: Option<String>,
    #[serde(default)]
    pub highlight: Option<String>,
}

/// 背景图构图参数。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemePackArt {
    pub focus_x: Option<f32>,
    pub focus_y: Option<f32>,
}

/// 氛围层旋钮。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemePackEffects {
    pub surface_opacity: Option<f32>,
    pub background_dim: Option<f32>,
    pub terminal_opacity: Option<f32>,
    pub surface_radius: Option<String>,
    pub surface_blur: Option<String>,
}

/// 作者可覆盖的 24 个 xterm 字段（全部可选）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemePackTerminal {
    pub background: Option<String>,
    pub foreground: Option<String>,
    pub cursor: Option<String>,
    pub cursor_accent: Option<String>,
    pub selection_background: Option<String>,
    pub selection_foreground: Option<String>,
    pub black: Option<String>,
    pub red: Option<String>,
    pub green: Option<String>,
    pub yellow: Option<String>,
    pub blue: Option<String>,
    pub magenta: Option<String>,
    pub cyan: Option<String>,
    pub white: Option<String>,
    pub bright_black: Option<String>,
    pub bright_red: Option<String>,
    pub bright_green: Option<String>,
    pub bright_yellow: Option<String>,
    pub bright_blue: Option<String>,
    pub bright_magenta: Option<String>,
    pub bright_cyan: Option<String>,
    pub bright_white: Option<String>,
}

impl ThemePackTerminal {
    /// 按 ANSI 槽位取覆盖值（0..16）。
    fn ansi(&self, index: usize) -> Option<&String> {
        let slot = match index {
            0 => &self.black,
            1 => &self.red,
            2 => &self.green,
            3 => &self.yellow,
            4 => &self.blue,
            5 => &self.magenta,
            6 => &self.cyan,
            7 => &self.white,
            8 => &self.bright_black,
            9 => &self.bright_red,
            10 => &self.bright_green,
            11 => &self.bright_yellow,
            12 => &self.bright_blue,
            13 => &self.bright_magenta,
            14 => &self.bright_cyan,
            15 => &self.bright_white,
            _ => return None,
        };
        slot.as_ref()
    }
}

/// 明暗态。皮肤的明暗由作者在 theme.json 里定死（不跟随系统）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    Dark,
    Light,
}

impl Appearance {
    pub fn is_dark(self) -> bool {
        matches!(self, Appearance::Dark)
    }

    fn theme_mode(self) -> ThemeMode {
        match self {
            Appearance::Dark => ThemeMode::Dark,
            Appearance::Light => ThemeMode::Light,
        }
    }
}

/// 解析并校验之后的主题包定义。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemePackDef {
    pub id: String,
    pub name: String,
    pub appearance: Appearance,
    pub colors: ThemePackColors,
    /// 背景图文件名（相对包目录）。无 = 纯 token 主题。
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub art: ThemePackArt,
    #[serde(default)]
    pub effects: ThemePackEffects,
    #[serde(default)]
    pub terminal: ThemePackTerminal,
}

// ───────────────────────── 校验 ─────────────────────────

/// 解析 theme.json 原文。不合法直接报错（错误文本面向设置页展示）。
///
/// 校验口径与前端 `parseThemePack` 一致：色值必须解析得出、`image` 必须是包内
/// 文件名（不许含路径分隔符与 `..`）、空串 `image` 归一成「没有背景图」。
///
/// 最后一条不是洁癖：`image: ""` 曾能通过校验，此后「有没有背景图」两处判据
/// 各说各话，终端被透明化而氛围层根本没挂上 —— 用户看到的是一个纯黑窗口。
pub fn parse_theme_pack(theme_id: &str, json: &str) -> Result<ThemePackDef> {
    let mut def: ThemePackDef =
        serde_json::from_str(json).map_err(|e| anyhow!("theme.json 解析失败: {e}"))?;
    if def.id.trim().is_empty() {
        bail!("theme.json 缺少 id");
    }
    if def.name.trim().is_empty() {
        bail!("theme.json 缺少 name");
    }
    if def.id != theme_id {
        // 与前端同样只警告：以目录名为准，包还是能用
        eprintln!(
            "[mt-ui] 主题包目录名 {theme_id} 与 theme.json id {} 不一致，以目录名为准",
            def.id
        );
    }

    let colors = &def.colors;
    for (label, value) in [
        ("background", &colors.background),
        ("panel", &colors.panel),
        ("panelAlt", &colors.panel_alt),
        ("accent", &colors.accent),
        ("text", &colors.text),
        ("muted", &colors.muted),
        ("line", &colors.line),
    ] {
        parse_color(value).with_context(|| format!("colors.{label} 不是合法色值: {value}"))?;
    }
    for (label, value) in [
        ("accentAlt", &colors.accent_alt),
        ("secondary", &colors.secondary),
        ("highlight", &colors.highlight),
    ] {
        if let Some(v) = value {
            parse_color(v).with_context(|| format!("colors.{label} 不是合法色值: {v}"))?;
        }
    }

    // terminal.* 与 colors 同一把尺子：坏色值必须在应用之前拦掉，
    // 否则换主题会换到一半，剩下的终端停在旧配色上
    for index in 0..16 {
        if let Some(v) = def.terminal.ansi(index) {
            parse_color(v).with_context(|| format!("terminal 第 {index} 号色不是合法色值: {v}"))?;
        }
    }
    for (label, value) in [
        ("background", &def.terminal.background),
        ("foreground", &def.terminal.foreground),
        ("cursor", &def.terminal.cursor),
        ("cursorAccent", &def.terminal.cursor_accent),
        ("selectionBackground", &def.terminal.selection_background),
        ("selectionForeground", &def.terminal.selection_foreground),
    ] {
        if let Some(v) = value {
            parse_color(v).with_context(|| format!("terminal.{label} 不是合法色值: {v}"))?;
        }
    }

    if let Some(image) = def.image.as_deref() {
        if image.trim().is_empty() {
            def.image = None;
        } else if image.contains(['/', '\\']) || image.contains("..") {
            bail!("image 必须是包内文件名: {image}");
        }
    }
    Ok(def)
}

// ───────────────────────── 色值解析 ─────────────────────────

/// 解析 `#rgb` / `#rgba` / `#rrggbb` / `#rrggbbaa` / `rgb()` / `rgba()`。
///
/// 命名色（`red` / `steelblue`）**不支持** —— Web 侧靠 `CSS.supports` 白送，
/// 这里没有 CSS 引擎；主题包写命名色会在校验阶段就被拒，不会静默变成黑色。
pub fn parse_color(input: &str) -> Result<Rgba> {
    let s = input.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let digits: Vec<u32> = hex
            .chars()
            .map(|c| c.to_digit(16).ok_or_else(|| anyhow!("非法十六进制: {s}")))
            .collect::<Result<_>>()?;
        let (r, g, b, a) = match digits.len() {
            3 | 4 => {
                let e = |i: usize| (digits[i] * 17) as f32 / 255.0;
                (
                    e(0),
                    e(1),
                    e(2),
                    if digits.len() == 4 { e(3) } else { 1.0 },
                )
            }
            6 | 8 => {
                let e = |i: usize| (digits[i] * 16 + digits[i + 1]) as f32 / 255.0;
                (
                    e(0),
                    e(2),
                    e(4),
                    if digits.len() == 8 { e(6) } else { 1.0 },
                )
            }
            _ => bail!("十六进制色值位数不对: {s}"),
        };
        return Ok(Rgba { r, g, b, a });
    }

    let lower = s.to_ascii_lowercase();
    let body = lower
        .strip_prefix("rgba(")
        .or_else(|| lower.strip_prefix("rgb("))
        .and_then(|rest| rest.strip_suffix(')'))
        .ok_or_else(|| anyhow!("无法解析色值: {s}"))?;
    let parts: Vec<&str> = body.split(',').map(str::trim).collect();
    if parts.len() < 3 || parts.len() > 4 {
        bail!("rgb() 分量个数不对: {s}");
    }
    let channel = |v: &str| -> Result<f32> {
        let n: f32 = v.parse().map_err(|_| anyhow!("非法分量 {v}"))?;
        Ok((n / 255.0).clamp(0.0, 1.0))
    };
    let a = match parts.get(3) {
        Some(v) => v.parse::<f32>().map_err(|_| anyhow!("非法 alpha {v}"))?.clamp(0.0, 1.0),
        None => 1.0,
    };
    Ok(Rgba {
        r: channel(parts[0])?,
        g: channel(parts[1])?,
        b: channel(parts[2])?,
        a,
    })
}

/// 解析成 [`Hsla`]（解析失败时用 `fallback`）。
fn color_or(input: &str, fallback: Hsla) -> Hsla {
    parse_color(input).map(Into::into).unwrap_or(fallback)
}

/// 换一个 alpha（乘性，与前端 `withAlpha` 同语义）。
fn with_alpha(color: Hsla, alpha: f32) -> Hsla {
    Hsla {
        a: (color.a * alpha).clamp(0.0, 1.0),
        ..color
    }
}

/// 输出成 gpui 认的 `#rrggbbaa` 字符串。
fn to_hex(color: Hsla) -> String {
    let rgba = Rgba::from(color);
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        byte(rgba.r),
        byte(rgba.g),
        byte(rgba.b),
        byte(rgba.a)
    )
}

// ───────────────────────── 内置基线 ─────────────────────────

const DEFAULT_SURFACE_OPACITY: f32 = 0.72;
const DEFAULT_BACKGROUND_DIM: f32 = 0.35;
const DEFAULT_TERMINAL_OPACITY: f32 = 0.6;

/// 内置暗色终端配色（= 旧版 `src/utils/builtinThemes.ts` 的 `DARK_TERMINAL_THEME`）。
///
/// 主题包只给了 10 个语义色时，ANSI 16 色**照抄这份基线**而不是从语义色乱推 ——
/// 推出来的 16 色会毁掉 TUI 的可读性（红绿撞色、亮色暗于暗色）。
pub fn builtin_dark_terminal_theme() -> TerminalTheme {
    TerminalTheme {
        background: rgb8(0x0a, 0x09, 0x08),
        foreground: rgb8(0xd8, 0xd4, 0xcc),
        bright_foreground: rgb8(0xe5, 0xe0, 0xd8),
        dim_foreground: rgb8(0x8f, 0x8b, 0x84),
        cursor: rgb8(0xc8, 0x80, 0x5a),
        cursor_text: rgb8(0x0a, 0x09, 0x08),
        selection: with_alpha(rgb8(0xc8, 0x80, 0x5a), 0.19),
        ansi: [
            rgb8(0x2a, 0x28, 0x24),
            rgb8(0xd4, 0x60, 0x5a),
            rgb8(0x6b, 0xb8, 0x7a),
            rgb8(0xd4, 0xa8, 0x4a),
            rgb8(0x68, 0x96, 0xc8),
            rgb8(0xb0, 0x8c, 0xd4),
            rgb8(0x7d, 0xcf, 0xb8),
            rgb8(0xd8, 0xd4, 0xcc),
            rgb8(0x5c, 0x58, 0x50),
            rgb8(0xe0, 0x70, 0x60),
            rgb8(0x80, 0xd0, 0x90),
            rgb8(0xe0, 0xb8, 0x60),
            rgb8(0x80, 0xaa, 0xd8),
            rgb8(0xc0, 0xa0, 0xe0),
            rgb8(0x90, 0xe0, 0xc8),
            rgb8(0xe5, 0xe0, 0xd8),
        ],
    }
}

/// 内置亮色终端配色（= `LIGHT_TERMINAL_THEME`）。
pub fn builtin_light_terminal_theme() -> TerminalTheme {
    TerminalTheme {
        background: rgb8(0xfa, 0xfa, 0xfa),
        foreground: rgb8(0x1a, 0x1a, 0x1a),
        bright_foreground: rgb8(0x00, 0x00, 0x00),
        dim_foreground: rgb8(0x66, 0x66, 0x66),
        cursor: rgb8(0xb0, 0x68, 0x30),
        cursor_text: rgb8(0xfa, 0xfa, 0xfa),
        selection: with_alpha(rgb8(0xb0, 0x68, 0x30), 0.19),
        ansi: [
            rgb8(0x1a, 0x1a, 0x1a),
            rgb8(0xc0, 0x39, 0x2b),
            rgb8(0x2d, 0x8a, 0x46),
            rgb8(0xb0, 0x86, 0x20),
            rgb8(0x28, 0x60, 0xa0),
            rgb8(0x8a, 0x5c, 0xb8),
            rgb8(0x1a, 0x8a, 0x6a),
            rgb8(0x80, 0x80, 0x80),
            rgb8(0x66, 0x66, 0x66),
            rgb8(0xe0, 0x40, 0x30),
            rgb8(0x38, 0xa0, 0x58),
            rgb8(0xc8, 0x98, 0x30),
            rgb8(0x38, 0x70, 0xb8),
            rgb8(0xa0, 0x70, 0xd0),
            rgb8(0x28, 0xa0, 0x80),
            rgb8(0xa0, 0xa0, 0xa0),
        ],
    }
}

/// 按明暗取内置终端配色。
pub fn builtin_terminal_theme(appearance: Appearance) -> TerminalTheme {
    match appearance {
        Appearance::Dark => builtin_dark_terminal_theme(),
        Appearance::Light => builtin_light_terminal_theme(),
    }
}

// ───────────────────────── 映射 ─────────────────────────

/// 背景图氛围层的参数。渲染在 [`crate::background`]（`BackgroundArtElement`）——
/// cover 缩放、focus 百分比定位、纱罩三条语义照搬原版，判据见那个模块的注释。
#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundArt {
    /// 图片绝对路径。
    pub image: PathBuf,
    /// 焦点（0..1），图片焦点落在视口的哪个位置。
    pub focus: (f32, f32),
    /// 压在图上的底色纱罩（已含 alpha）。
    pub dim: Hsla,
}

/// 一次主题应用的产物。
#[derive(Debug, Clone)]
pub struct AppliedThemePack {
    /// 主题包身份 = **themes/ 下的目录名**（`config.custom_theme_id` 存的就是它）。
    ///
    /// 不是 theme.json 里的 `id` 字段：两者允许不一致（用户改过目录名），
    /// 一致性口径见 [`mt_config::ThemePackEntry::theme_id`]。目录未知时
    /// （单测直接喂 `def`）才退回 `def.id`。
    pub theme_id: String,
    pub name: String,
    pub appearance: Appearance,
    /// 主题包声明的 10 个语义色**原文**（`panel` / `panelAlt` / `muted` / `line` …）。
    ///
    /// `gpui_theme` 只承载 gpui-component 认得的那套键，而 mt-app 的壳配色表
    /// （`ui.rs` 里 `bg_surface` / `bg_elevated` / `text_muted` / `border_*` 那一串）
    /// 是 mini-term 自己的语义，两边不是一一对应。少了这份原文，宿主就得绕开
    /// [`switch_to_theme_pack`] 自己手拆四步（read → parse → resolve → install）
    /// 才拿得到 —— 所以直接带出来。
    ///
    /// 要 `Hsla` 用 [`Self::color`]。
    pub colors: ThemePackColors,
    /// 递给 [`crate::TerminalElement`] / `TerminalView` 的终端配色。
    pub terminal: TerminalTheme,
    /// 递给 gpui-component 主题层的配置。
    pub gpui_theme: ThemeConfig,
    /// 有背景图时的氛围层参数。
    pub background: Option<BackgroundArt>,
    /// 面板不透明度（背景图模式下 UI 表面要半透明才看得见图）。
    pub surface_opacity: f32,
}

/// [`AppliedThemePack::colors`] 里的一个语义槽位。
///
/// 命名与 theme.json 的键一致（camelCase → 这里的蛇形），与
/// `src/utils/themePackManager.ts` 往 CSS 变量里塞的那批是同一批。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThemeSlot {
    /// 窗口底色 `--bg-base`
    Background,
    /// 面板底色 `--bg-surface`
    Panel,
    /// 次级面板 / 悬浮层 `--bg-elevated`
    PanelAlt,
    /// 强调色 `--accent`
    Accent,
    /// 主文本 `--text-primary`
    Text,
    /// 次要文本 `--text-muted`
    Muted,
    /// 分隔线 `--border-default`
    Line,
    /// 第二强调色（可选，未声明时回落 `Accent`）
    AccentAlt,
    /// 辅助色（可选，回落 `Accent`）
    Secondary,
    /// 高亮色（可选，回落 `Accent`）
    Highlight,
}

impl AppliedThemePack {
    /// 取某个语义色。可选槽位未声明时回落到 `accent`——与
    /// `themePackManager.ts` 写 CSS 变量时的回落一致（那边是
    /// `c.accentAlt ?? c.accent`）。
    ///
    /// 色值在 [`parse_theme_pack`] 阶段就校验过，这里不会失败。
    pub fn color(&self, slot: ThemeSlot) -> Hsla {
        let c = &self.colors;
        let fallback = || color_or(&c.accent, Hsla::default());
        let pick = |v: &Option<String>| v.as_ref().map(|s| color_or(s, fallback()));
        match slot {
            ThemeSlot::Background => color_or(&c.background, Hsla::default()),
            ThemeSlot::Panel => color_or(&c.panel, Hsla::default()),
            ThemeSlot::PanelAlt => color_or(&c.panel_alt, Hsla::default()),
            ThemeSlot::Accent => fallback(),
            ThemeSlot::Text => color_or(&c.text, Hsla::default()),
            ThemeSlot::Muted => color_or(&c.muted, Hsla::default()),
            ThemeSlot::Line => color_or(&c.line, Hsla::default()),
            ThemeSlot::AccentAlt => pick(&c.accent_alt).unwrap_or_else(fallback),
            ThemeSlot::Secondary => pick(&c.secondary).unwrap_or_else(fallback),
            ThemeSlot::Highlight => pick(&c.highlight).unwrap_or_else(fallback),
        }
    }
}

fn surface_opacity_of(effects: &ThemePackEffects) -> f32 {
    clamp01(effects.surface_opacity, DEFAULT_SURFACE_OPACITY)
}

fn terminal_opacity_of(effects: &ThemePackEffects) -> f32 {
    clamp01(effects.terminal_opacity, DEFAULT_TERMINAL_OPACITY)
}

fn clamp01(value: Option<f32>, fallback: f32) -> f32 {
    match value {
        Some(v) if (0.0..=1.0).contains(&v) => v,
        _ => fallback,
    }
}

/// theme.json → [`TerminalTheme`]。
///
/// `with_background` = 这个包带背景图且图找得到。带图时**丢掉作者写的
/// `terminal.background`**：一个照着内置主题抄全 24 个字段的皮肤（声明里写的就是
/// 「完整/部分 xterm 24 字段」，抄全是自然做法）会把氛围图整块盖死，而且毫无提示。
pub fn to_terminal_theme(def: &ThemePackDef, with_background: bool) -> TerminalTheme {
    let base = builtin_terminal_theme(def.appearance);
    let c = &def.colors;
    let background = color_or(&c.background, base.background);
    let text = color_or(&c.text, base.foreground);
    let accent = color_or(&c.accent, base.cursor);
    let muted = color_or(&c.muted, base.dim_foreground);

    let mut theme = TerminalTheme {
        // 带背景图时终端自身背景透明，着色交给氛围层
        background: if with_background {
            with_alpha(background, terminal_opacity_of(&def.effects))
        } else {
            background
        },
        foreground: text,
        // 主题包没有「bold 默认前景」这一项：用作者给的 brightWhite，
        // 没有就退回文本色（不自己提亮 —— 提亮量没有任何依据）
        bright_foreground: def
            .terminal
            .bright_white
            .as_deref()
            .map(|v| color_or(v, text))
            .unwrap_or(text),
        dim_foreground: muted,
        cursor: accent,
        cursor_text: background,
        selection: with_alpha(accent, 0.22),
        ansi: base.ansi,
    };

    for index in 0..16 {
        if let Some(v) = def.terminal.ansi(index) {
            theme.ansi[index] = color_or(v, theme.ansi[index]);
        }
    }
    // 作者显式写的几个字段最后覆盖（与前端 `...overrides` 的展开顺序一致）
    if let Some(v) = &def.terminal.foreground {
        theme.foreground = color_or(v, theme.foreground);
    }
    if let Some(v) = &def.terminal.cursor {
        theme.cursor = color_or(v, theme.cursor);
    }
    if let Some(v) = &def.terminal.cursor_accent {
        theme.cursor_text = color_or(v, theme.cursor_text);
    }
    if let Some(v) = &def.terminal.selection_background {
        theme.selection = color_or(v, theme.selection);
    }
    if !with_background && let Some(v) = &def.terminal.background {
        theme.background = color_or(v, theme.background);
    }
    theme
}

/// theme.json → gpui-component 的 [`ThemeConfig`]。
///
/// 用 JSON 中转而不是直接填 `ThemeConfigColors` 的字段：那个结构有 120+ 个字段、
/// 字段名与 JSON 键名各一套，gpui-component 升个版就可能改。走 JSON 键名等于
/// 用它对外承诺的 schema，**没写到的键一律 `None`**，由
/// `Theme::apply_config` 回落到内置 dark/light 基线 —— 我们只覆盖十个语义色
/// 说得清归宿的那些，其余保持组件库自己的搭配。
pub fn to_gpui_theme_config(def: &ThemePackDef, with_background: bool) -> ThemeConfig {
    let c = &def.colors;
    let fallback = if def.appearance.is_dark() {
        builtin_dark_terminal_theme()
    } else {
        builtin_light_terminal_theme()
    };
    let background = color_or(&c.background, fallback.background);
    let panel = color_or(&c.panel, background);
    let panel_alt = color_or(&c.panel_alt, panel);
    let accent = color_or(&c.accent, fallback.cursor);
    let text = color_or(&c.text, fallback.foreground);
    let muted = color_or(&c.muted, fallback.dim_foreground);
    let line = color_or(&c.line, muted);

    // 背景图模式下面板半透明，图才透得出来；浮层（popover / overlay）保持不透明 ——
    // 弹窗叠在任意内容上，半透明是拿可读性换观感
    let surface_alpha = if with_background {
        surface_opacity_of(&def.effects)
    } else {
        1.0
    };
    let panel_surface = with_alpha(panel, surface_alpha);
    let panel_alt_surface = with_alpha(panel_alt, surface_alpha);

    let mut map = serde_json::Map::new();
    let mut put = |key: &str, color: Hsla| {
        map.insert(key.to_string(), serde_json::Value::String(to_hex(color)));
    };

    put("background", background);
    put("foreground", text);
    put("border", line);
    put("input.border", line);
    put("window.border", line);
    put("ring", accent);
    put("caret", accent);
    put("link", accent);
    put("link.hover", accent);
    put("link.active", accent);
    put("selection.background", with_alpha(accent, 0.22));
    put("drag.border", accent);
    put("drop_target.background", with_alpha(accent, 0.18));

    put("muted.background", panel_alt_surface);
    put("muted.foreground", muted);
    put("accent.background", panel_alt_surface);
    put("accent.foreground", text);

    put("primary.background", accent);
    put("primary.foreground", background);
    put("primary.hover.background", with_alpha(accent, 0.85));
    put("primary.active.background", with_alpha(accent, 0.7));

    put("secondary.background", panel_surface);
    put("secondary.foreground", text);
    put("secondary.hover.background", panel_alt_surface);
    put("secondary.active.background", panel_alt_surface);

    put("popover.background", panel_alt);
    put("popover.foreground", text);
    put("overlay", with_alpha(background, 0.55));

    put("list.background", panel_surface);
    put("list.hover.background", with_alpha(accent, 0.12));
    put("list.active.background", with_alpha(accent, 0.2));
    put("list.active.border", accent);
    put("list.head.background", panel_alt_surface);

    put("table.background", panel_surface);
    put("table.head.background", panel_alt_surface);
    put("table.head.foreground", muted);
    put("table.hover.background", with_alpha(accent, 0.12));
    put("table.active.background", with_alpha(accent, 0.2));
    put("table.active.border", accent);
    put("table.row.border", line);

    put("sidebar.background", panel_surface);
    put("sidebar.foreground", text);
    put("sidebar.border", line);
    put("sidebar.accent.background", with_alpha(accent, 0.16));
    put("sidebar.accent.foreground", text);
    put("sidebar.primary.background", accent);
    put("sidebar.primary.foreground", background);

    put("title_bar.background", panel_surface);
    put("title_bar.border", line);
    put("tab_bar.background", panel_alt_surface);
    put("tab_bar.segmented.background", panel_alt_surface);
    put("tab.background", panel_alt_surface);
    put("tab.foreground", muted);
    put("tab.active.background", panel_surface);
    put("tab.active.foreground", text);

    put("group_box.background", panel_surface);
    put("group_box.foreground", text);
    put("group_box.title.foreground", text);
    put("progress.bar.background", accent);
    put("slider.background", panel_alt_surface);
    put("slider.thumb.background", accent);
    put("switch.background", panel_alt_surface);
    put("switch.thumb.background", background);
    put("skeleton.background", panel_alt_surface);
    put("scrollbar.background", panel_alt_surface);
    put("scrollbar.thumb.background", with_alpha(line, 0.8));
    put("scrollbar.thumb.hover.background", line);
    put("accordion.background", panel_surface);
    put("accordion.hover.background", panel_alt_surface);
    put("tiles.background", background);

    // 三个可选语义色的近似归宿，与前端 buildTokenMap 一一对应
    if let Some(v) = &c.accent_alt {
        let warning = color_or(v, accent);
        put("warning.background", warning);
        put("warning.hover.background", with_alpha(warning, 0.85));
        put("warning.active.background", with_alpha(warning, 0.7));
        put("warning.foreground", background);
    }
    if let Some(v) = &c.secondary {
        let info = color_or(v, accent);
        put("info.background", info);
        put("info.hover.background", with_alpha(info, 0.85));
        put("info.active.background", with_alpha(info, 0.7));
        put("info.foreground", background);
    }
    if let Some(v) = &c.highlight {
        let success = color_or(v, accent);
        put("success.background", success);
        put("success.hover.background", with_alpha(success, 0.85));
        put("success.active.background", with_alpha(success, 0.7));
        put("success.foreground", background);
    }

    let value = serde_json::json!({
        "name": def.name,
        "mode": if def.appearance.is_dark() { "dark" } else { "light" },
        "colors": serde_json::Value::Object(map),
    });
    // 这里的 unwrap 有 schema 保证：值全是我们自己塞的字符串。
    // 真炸了说明 gpui-component 换了 schema，属于必须立刻发现的编译期级事故。
    serde_json::from_value(value).expect("主题桥生成的 ThemeConfig 必须能被 gpui-component 解析")
}

/// theme.json + 包目录 → 完整的应用产物（不改任何全局状态，可单测）。
pub fn resolve_theme_pack(def: &ThemePackDef, dir: Option<&Path>) -> AppliedThemePack {
    let background = def.image.as_deref().zip(dir).and_then(|(image, dir)| {
        let path = dir.join(image);
        // 图不在盘上就当没有背景图：否则终端被透明化、氛围层却是空的
        if !path.is_file() {
            eprintln!(
                "[mt-ui] 主题包 {} 声明了背景图 {image}，但文件不存在，按无背景图处理",
                def.id
            );
            return None;
        }
        let base = color_or(&def.colors.background, Hsla::default());
        Some(BackgroundArt {
            image: path,
            focus: (
                def.art.focus_x.unwrap_or(0.5).clamp(0.0, 1.0),
                def.art.focus_y.unwrap_or(0.5).clamp(0.0, 1.0),
            ),
            dim: with_alpha(
                base,
                clamp01(def.effects.background_dim, DEFAULT_BACKGROUND_DIM),
            ),
        })
    });
    let with_background = background.is_some();

    AppliedThemePack {
        // 身份取目录名（`themes/<theme_id>/`），theme.json 的 `id` 只是包作者
        // 自己写的展示值 —— 两者不一致时按目录名走，与 `packs.read()` 同一把尺子
        theme_id: dir
            .and_then(|d| d.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| def.id.clone()),
        name: def.name.clone(),
        appearance: def.appearance,
        colors: def.colors.clone(),
        terminal: to_terminal_theme(def, with_background),
        gpui_theme: to_gpui_theme_config(def, with_background),
        background,
        surface_opacity: if with_background {
            surface_opacity_of(&def.effects)
        } else {
            1.0
        },
    }
}

// ───────────────────────── 运行时切换 ─────────────────────────

/// 把一份 [`AppliedThemePack`] 装进 gpui-component 的全局主题并刷新窗口。
///
/// 明暗跟着皮肤的 `appearance` 走，不跟随系统 —— 与旧版一致（皮肤的明暗由作者
/// 定死，切明暗 = 退出皮肤回内置）。
pub fn install_gpui_theme(applied: &AppliedThemePack, window: Option<&mut Window>, cx: &mut App) {
    let mode = applied.appearance.theme_mode();
    // Theme 全局可能还没初始化（gpui_component::init 之前）：先建一个再改
    if !cx.has_global::<Theme>() {
        Theme::change(mode, None, cx);
    }
    let config = Rc::new(applied.gpui_theme.clone());
    {
        let theme = Theme::global_mut(cx);
        if mode.is_dark() {
            theme.dark_theme = config;
        } else {
            theme.light_theme = config;
        }
    }
    Theme::change(mode, window, cx);
}

/// **「按主题包 id 切换」的入口**（mt-app 接线点）。
///
/// ```ignore
/// let packs = mt_config::ThemePacks::open()?;
/// let applied = mt_ui::theme_bridge::switch_to_theme_pack(&packs, "dracula", Some(window), cx)?;
/// store.set_terminal_theme(applied.terminal.clone(), cx); // 逐 pane 下发
/// ```
///
/// 只做「读包 → 校验 → 应用」。**不写 config.json**：持久化归 mt-app
/// （它才知道要不要连带改 `theme` 字段）。
pub fn switch_to_theme_pack(
    packs: &mt_config::ThemePacks,
    theme_id: &str,
    window: Option<&mut Window>,
    cx: &mut App,
) -> Result<AppliedThemePack> {
    let data = packs.read(theme_id)?;
    let def = parse_theme_pack(theme_id, &data.theme_json)?;
    let applied = resolve_theme_pack(&def, Some(&data.dir));
    install_gpui_theme(&applied, window, cx);
    Ok(applied)
}

/// 退出皮肤，回内置明暗态。返回该明暗的内置终端配色。
///
/// ⚠️ **不能只调 `Theme::change`**。[`install_gpui_theme`] 是把皮肤的
/// `ThemeConfig` **持久写进** `Theme::dark_theme` / `light_theme` 的
/// —— 那两个字段就是「这个明暗态长什么样」的唯一来源。只切 mode 的话
/// 全局主题仍然指着皮肤那份配置，面板/按钮/浮层会原地停在皮肤配色上，
/// 用户会看到「退出皮肤了但界面没变」。所以这里必须先把两份配置从
/// `ThemeRegistry` 的内置基线恢复回去，再切 mode。
pub fn switch_to_builtin(
    appearance: Appearance,
    window: Option<&mut Window>,
    cx: &mut App,
) -> TerminalTheme {
    restore_builtin_gpui_theme(cx);
    Theme::change(appearance.theme_mode(), window, cx);
    builtin_terminal_theme(appearance)
}

/// 把 gpui-component 全局主题的明暗两份配置恢复成内置基线。
///
/// 单独暴露是给「只想撤掉皮肤、暂时不切明暗」的宿主用（比如主题包读取失败
/// 要回退时）。调完记得 `Theme::change(mode, window, cx)` 让窗口重绘。
pub fn restore_builtin_gpui_theme(cx: &mut App) {
    if !cx.has_global::<Theme>() {
        // 还没 init 过：没有被污染的字段，也就没什么好恢复的
        return;
    }
    let (light, dark) = {
        let registry = ThemeRegistry::global(cx);
        (
            registry.default_light_theme().clone(),
            registry.default_dark_theme().clone(),
        )
    };
    let theme = Theme::global_mut(cx);
    theme.light_theme = light;
    theme.dark_theme = dark;
}

/// 皮肤列表里的一项：**目录名（身份）+ 解析好的定义 + 包目录**。
///
/// 对应原版 `ThemePackMeta`（`themePackManager.ts:75-81`）的三个字段，
/// 一字不差地包括那条注释：`themeId` 是「themes/ 下目录名（read_theme_pack
/// 用它定位）」。此前这里只返回 `(def, dir)` 把目录名丢了，调用方只好拿
/// `def.id` 当身份 —— 目录名与 `id` 不一致的包（`themes/ember-new/` 里写着
/// `"id": "ember-dusk"`）于是列得出来、一点「应用」就 `read("ember-dusk")`
/// 找不到目录，报「皮肤应用失败」。
pub struct ThemePackListing {
    /// themes/ 下的目录名。应用 / 删除 / 读资源全用它。
    pub theme_id: String,
    /// theme.json 解析校验后的定义（`def.id` 只用于展示与告警）。
    pub def: ThemePackDef,
    /// 包目录绝对路径（缩略图、背景图拼路径用）。
    pub dir: PathBuf,
}

/// 扫一遍 themes/ 目录，返回能用的包（坏包跳过并打日志，不阻塞列表）。
///
/// 设置页的皮肤列表用这个：一个坏包不该让整张列表打不开。
pub fn list_theme_packs(packs: &mt_config::ThemePacks) -> Result<Vec<ThemePackListing>> {
    let mut out = Vec::new();
    for entry in packs.list()? {
        match parse_theme_pack(&entry.theme_id, &entry.theme_json) {
            Ok(def) => out.push(ThemePackListing {
                theme_id: entry.theme_id,
                def,
                dir: entry.dir,
            }),
            Err(e) => eprintln!("[mt-ui] 主题包 {} 无效，已跳过: {e:#}", entry.theme_id),
        }
    }
    Ok(out)
}

// 背景图渲染已落在 [`crate::background`]：
//   - `fit_bounds` 按 cover/contain 算 bounds（取较大/较小缩放系数，
//     再按 focus 平移 `(container - scaled) * focus`）；
//   - `BackgroundArtElement` 画图 + 盖一层 `fill(bounds, art.dim)` 的纱罩；
//   - 终端侧不用改一行：「默认背景不发 quad」+ 本模块给的半透明 `background`。
// 仍要留神的是 overdraw：窗口级铺一张之后**不要**再给每个终端铺一张
// （两层纱罩会把 dim 平方），`docs/gpui-migration.md` 第 5 节的坑位表里点了这条。

#[cfg(test)]
mod tests {
    use super::*;

    /// 用 mt-config 的示例主题包生成函数造数据 —— 文档模板与这里共用同一份文件，
    /// 模板改了这个测试立刻会知道。
    fn example_pack() -> (ThemePackDef, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "mt-ui-theme-bridge-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let packs = mt_config::ThemePacks::at(root.join("themes"));
        let id = packs.create_example().unwrap();
        let data = packs.read(&id).unwrap();
        let def = parse_theme_pack(&id, &data.theme_json).unwrap();
        let dir = data.dir.clone();
        // 目录留到测试结束由调用方删；这里先把定义与目录交出去
        (def, dir)
    }

    #[test]
    fn 示例主题包能解析并映射() {
        let (def, dir) = example_pack();
        assert_eq!(def.id, "example");
        assert_eq!(def.appearance, Appearance::Dark);
        assert!(def.image.is_none(), "示例包不带背景图");

        let applied = resolve_theme_pack(&def, Some(&dir));
        assert!(applied.background.is_none());
        assert_eq!(applied.surface_opacity, 1.0);

        // theme.json 里 background=#0f1115 / text=#e6e9ef / accent=#6aa9ff
        assert_eq!(to_hex(applied.terminal.background), "#0f1115ff");
        assert_eq!(to_hex(applied.terminal.foreground), "#e6e9efff");
        assert_eq!(to_hex(applied.terminal.cursor), "#6aa9ffff");
        // 光标块下的字用背景色反白
        assert_eq!(to_hex(applied.terminal.cursor_text), "#0f1115ff");
        // 示例包写全了 16 色，ANSI 必须来自包而不是内置基线
        assert_eq!(to_hex(applied.terminal.ansi[1]), "#e06c75ff"); // red
        assert_eq!(to_hex(applied.terminal.ansi[15]), "#f5f7faff"); // brightWhite
        // brightWhite 同时是 bold 默认前景的来源
        assert_eq!(to_hex(applied.terminal.bright_foreground), "#f5f7faff");
        // dim 前景取 muted
        assert_eq!(to_hex(applied.terminal.dim_foreground), "#8b93a4ff");

        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }

    /// 仓库 `theme/` 下分发的成品皮肤要经得起**语义**校验。mt-config 那侧只管
    /// 文件层（能不能导入、manifest 对不对）；色值合不合法、`image` 是不是包内
    /// 文件名、背景图最终能不能真解析成氛围层，只有走完这条路才知道。
    #[test]
    fn 仓库分发的成品皮肤能解析出氛围层() {
        let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../theme");
        let mut count = 0;
        for entry in std::fs::read_dir(&shipped).unwrap().flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue; // theme/README.md 这类说明文件不是皮肤
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let json = std::fs::read_to_string(dir.join("theme.json"))
                .unwrap_or_else(|e| panic!("{id}/theme.json 读不到: {e}"));
            let def =
                parse_theme_pack(&id, &json).unwrap_or_else(|e| panic!("{id} 校验不过: {e:#}"));

            let applied = resolve_theme_pack(&def, Some(&dir));
            assert_eq!(applied.theme_id, id, "身份取目录名");
            if def.image.is_some() {
                // 图在盘上才有氛围层。拿不到就是 resolve 按「无背景图」降级了 ——
                // 而这类皮肤的 effects 是照着有图配的，降级后终端只剩一片纯色底
                assert!(applied.background.is_some(), "{id} 的背景图没解析出来");
                assert!(applied.surface_opacity < 1.0, "{id} 带图却没开面板透明");
            }
            count += 1;
        }
        assert!(count > 0, "theme/ 下一个成品皮肤都没有 —— 路径写错了？");
    }

    /// `Option<SharedString>` 取 `&str`（`as_deref` 给的是 `&ArcCow<str>`，比不了）
    fn slot(value: &Option<gpui::SharedString>) -> Option<&str> {
        value.as_ref().map(|s| s.as_ref())
    }

    #[test]
    fn 示例主题包映射进_gpui_主题层() {
        let (def, dir) = example_pack();
        let config = to_gpui_theme_config(&def, false);
        assert_eq!(config.name.as_ref(), def.name.as_str());
        assert_eq!(config.mode, ThemeMode::Dark);
        assert_eq!(slot(&config.colors.background), Some("#0f1115ff"));
        assert_eq!(slot(&config.colors.foreground), Some("#e6e9efff"));
        assert_eq!(slot(&config.colors.border), Some("#2a3140ff"));
        // primary = accent，前景用背景色（按钮上的字）
        assert_eq!(slot(&config.colors.primary), Some("#6aa9ffff"));
        assert_eq!(slot(&config.colors.primary_foreground), Some("#0f1115ff"));
        // 可选语义色的近似归宿
        assert_eq!(slot(&config.colors.warning), Some("#f0b429ff")); // accentAlt
        assert_eq!(slot(&config.colors.info), Some("#7dd3c0ff")); // secondary
        assert_eq!(slot(&config.colors.success), Some("#7bd88fff")); // highlight
        // 没写到的键保持 None，由 gpui-component 回落内置暗色基线
        assert!(config.colors.danger.is_none());

        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }

    fn minimal_json(extra: &str) -> String {
        format!(
            r##"{{
              "id": "t", "name": "T", "appearance": "dark",
              "colors": {{
                "background": "#101010", "panel": "#202020", "panelAlt": "#303030",
                "accent": "#4080ff", "text": "#eeeeee", "muted": "#888888", "line": "#404040"
              }}{extra}
            }}"##
        )
    }

    #[test]
    fn 只给十色时_ansi_照抄内置基线() {
        let def = parse_theme_pack("t", &minimal_json("")).unwrap();
        let theme = to_terminal_theme(&def, false);
        let base = builtin_dark_terminal_theme();
        // 乱推 16 色会毁掉 TUI 可读性，必须原样照抄
        assert_eq!(theme.ansi, base.ansi);
        assert_eq!(to_hex(theme.foreground), "#eeeeeeff");
        // 没写 brightWhite 时 bold 默认前景退回文本色
        assert_eq!(to_hex(theme.bright_foreground), "#eeeeeeff");
    }

    #[test]
    fn 亮色包走亮色基线() {
        let json = minimal_json("").replace("\"dark\"", "\"light\"");
        let def = parse_theme_pack("t", &json).unwrap();
        assert_eq!(def.appearance, Appearance::Light);
        let theme = to_terminal_theme(&def, false);
        assert_eq!(theme.ansi, builtin_light_terminal_theme().ansi);
        assert_eq!(to_gpui_theme_config(&def, false).mode, ThemeMode::Light);
    }

    #[test]
    fn 带背景图时丢掉作者写的终端背景() {
        let def = parse_theme_pack(
            "t",
            &minimal_json(r##", "terminal": {"background": "#000000"}, "image": "bg.jpg""##),
        )
        .unwrap();

        // 无背景图：作者写的 terminal.background 生效
        let opaque = to_terminal_theme(&def, false);
        assert_eq!(to_hex(opaque.background), "#000000ff");

        // 有背景图：丢掉它，改成按 terminalOpacity 半透明的语义背景色
        let transparent = to_terminal_theme(&def, true);
        assert_eq!(&to_hex(transparent.background)[..7], "#101010"); // RGB 分量不变
        assert!(
            (transparent.background.a - DEFAULT_TERMINAL_OPACITY).abs() < 0.01,
            "实际 alpha {}",
            transparent.background.a
        );
    }

    #[test]
    fn 背景图文件不存在时按无背景图处理() {
        let def = parse_theme_pack("t", &minimal_json(r#", "image": "missing.jpg""#)).unwrap();
        let applied = resolve_theme_pack(&def, Some(Path::new("/definitely/not/here")));
        assert!(applied.background.is_none());
        // 终端不能被透明化 —— 否则用户看到的是一个纯黑窗口
        assert_eq!(applied.terminal.background.a, 1.0);
    }

    #[test]
    fn 产物带出主题包的十个语义色原文() {
        // mt-app 的壳配色表(bg_surface / text_muted / border_* …)要的是这份原文,
        // gpui_theme 那份键名对不上。少了它宿主只能绕开 switch_to_theme_pack 手拆四步
        // 注意 minimal_json 的 extra 是插在 colors **之后**(根级),
        // 可选语义色必须写进 colors 里面,所以这条自己拼
        let json = minimal_json("").replace(
            r##""line": "#404040""##,
            r##""line": "#404040", "accentAlt": "#f0b429", "highlight": "#7bd88f""##,
        );
        let def = parse_theme_pack("t", &json).unwrap();
        let applied = resolve_theme_pack(&def, None);
        assert_eq!(applied.colors.panel, "#202020");
        assert_eq!(applied.colors.panel_alt, "#303030");
        assert_eq!(to_hex(applied.color(ThemeSlot::Panel)), "#202020ff");
        assert_eq!(to_hex(applied.color(ThemeSlot::PanelAlt)), "#303030ff");
        assert_eq!(to_hex(applied.color(ThemeSlot::Muted)), "#888888ff");
        assert_eq!(to_hex(applied.color(ThemeSlot::Line)), "#404040ff");
        assert_eq!(to_hex(applied.color(ThemeSlot::AccentAlt)), "#f0b429ff");
        assert_eq!(to_hex(applied.color(ThemeSlot::Highlight)), "#7bd88fff");
        // 可选槽位没写就回落 accent —— 与 themePackManager.ts 的 `?? c.accent` 同
        assert_eq!(
            to_hex(applied.color(ThemeSlot::Secondary)),
            to_hex(applied.color(ThemeSlot::Accent))
        );
    }

    #[test]
    fn 坏色值在校验阶段就被拦下() {
        // 命名色没有 CSS 引擎支撑，直接拒（而不是静默变黑）
        let err = parse_theme_pack("t", &minimal_json("").replace("#101010", "rebeccapurple"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("colors.background"), "实际: {err}");

        // terminal.* 与 colors 同一把尺子
        let err = parse_theme_pack("t", &minimal_json(r#", "terminal": {"red": "nope"}"#))
            .unwrap_err()
            .to_string();
        assert!(err.contains("terminal"), "实际: {err}");
    }

    #[test]
    fn image_必须是包内文件名_空串归一成没有() {
        for bad in ["../../evil.png", "a/b.png", "a\\b.png"] {
            let json = minimal_json(&format!(r#", "image": "{}""#, bad.replace('\\', "\\\\")));
            assert!(parse_theme_pack("t", &json).is_err(), "应拒绝 {bad}");
        }
        // 空串：归一成「没有背景图」，两处判据不能各说各话
        let def = parse_theme_pack("t", &minimal_json(r#", "image": "  ""#)).unwrap();
        assert!(def.image.is_none());
    }

    /// 回归测试（用户真机 v0.13.x GPUI 版）：目录名与 theme.json 的 `id` 不一致的
    /// 包，列表项的身份必须是**目录名**，拿它回头 `read` 得读得到。
    ///
    /// 此前 `list_theme_packs` 只返回 `(def, dir)`，设置页只好拿 `def.id` 当身份
    /// 去应用 —— 落到 `packs.read("ember-dusk")` 上找不到 `themes/ember-dusk/`，
    /// 红条报「皮肤应用失败: ember-dusk」。
    #[test]
    fn 列表项的身份是目录名而非_json_里的_id() {
        let root = std::env::temp_dir().join(format!(
            "mt-ui-theme-listing-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let packs = mt_config::ThemePacks::at(root.join("themes"));
        let dir = packs.root().join("ember-new");
        std::fs::create_dir_all(&dir).unwrap();
        let json = minimal_json("").replace(r#""id": "t""#, r#""id": "ember-dusk""#);
        std::fs::write(dir.join("theme.json"), &json).unwrap();

        let listed = list_theme_packs(&packs).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].theme_id, "ember-new", "身份=目录名");
        assert_eq!(listed[0].def.id, "ember-dusk", "定义里仍是作者写的 id");
        assert_eq!(listed[0].dir, dir);

        // 拿列表项的身份回头读包 —— 这正是「应用皮肤」那条路的第一步
        assert!(packs.read(&listed[0].theme_id).is_ok());
        assert!(
            packs.read(&listed[0].def.id).is_err(),
            "按 json 的 id 定位必然落空，别再走这条"
        );

        // resolve 出来的产物同样带目录名，免得下游又把 def.id 当身份写回配置
        let applied = resolve_theme_pack(&listed[0].def, Some(&dir));
        assert_eq!(applied.theme_id, "ember-new");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 色值解析覆盖四种写法() {
        let cases: [(&str, [u8; 4]); 6] = [
            ("#abc", [0xaa, 0xbb, 0xcc, 0xff]),
            ("#abcd", [0xaa, 0xbb, 0xcc, 0xdd]),
            ("#0f1115", [0x0f, 0x11, 0x15, 0xff]),
            ("#0f111580", [0x0f, 0x11, 0x15, 0x80]),
            ("rgb(15, 17, 21)", [15, 17, 21, 0xff]),
            ("rgba(15, 17, 21, 0.5)", [15, 17, 21, 128]),
        ];
        for (input, expect) in cases {
            let rgba = parse_color(input).unwrap_or_else(|e| panic!("{input}: {e}"));
            let byte = |v: f32| (v * 255.0).round() as u8;
            assert_eq!(
                [byte(rgba.r), byte(rgba.g), byte(rgba.b), byte(rgba.a)],
                expect,
                "输入 {input}"
            );
        }
        for bad in ["#ab", "#12345", "rgb(1,2)", "hsl(0,0%,0%)", "", "red"] {
            assert!(parse_color(bad).is_err(), "应拒绝 {bad:?}");
        }
    }

    #[test]
    fn 背景图参数的默认值与钳位() {
        let json = minimal_json(
            r#", "image": "bg.png", "art": {"focusX": 1.8, "focusY": 0.25},
               "effects": {"backgroundDim": 2.0, "surfaceOpacity": 0.5}"#,
        );
        let def = parse_theme_pack("t", &json).unwrap();

        // 造一个真有图的目录
        let dir = std::env::temp_dir().join(format!(
            "mt-ui-bg-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bg.png"), b"not really a png").unwrap();

        let applied = resolve_theme_pack(&def, Some(&dir));
        let art = applied.background.expect("图在盘上就该有氛围层");
        assert_eq!(art.focus, (1.0, 0.25), "focusX 越界要钳到 1.0");
        // backgroundDim 越界回默认值而不是钳到 1（与前端 clamp01 同语义）
        assert!((art.dim.a - DEFAULT_BACKGROUND_DIM).abs() < 0.01);
        assert!((applied.surface_opacity - 0.5).abs() < 0.01);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
