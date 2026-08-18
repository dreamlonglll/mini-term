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
//! # 背景图（未完成，见文件末尾 TODO）
//!
//! 本轮只把**数据**准备好（[`BackgroundArt`]：图片路径、焦点、压暗、终端透明度），
//! 渲染没做。终端侧「默认背景不发 quad」的路早就留好了，缺的是窗口级的
//! 背景图 element 与 cover/contain 的 bounds 自算。

use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context as _, Result, anyhow, bail};
use gpui::{App, Hsla, Rgba, Window};
use gpui_component::{Theme, ThemeConfig, ThemeMode};
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

/// 背景图氛围层的参数。渲染尚未实现，见文件末尾 TODO。
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
    pub theme_id: String,
    pub name: String,
    pub appearance: Appearance,
    /// 递给 [`crate::TerminalElement`] / `TerminalView` 的终端配色。
    pub terminal: TerminalTheme,
    /// 递给 gpui-component 主题层的配置。
    pub gpui_theme: ThemeConfig,
    /// 有背景图时的氛围层参数。
    pub background: Option<BackgroundArt>,
    /// 面板不透明度（背景图模式下 UI 表面要半透明才看得见图）。
    pub surface_opacity: f32,
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
        theme_id: def.id.clone(),
        name: def.name.clone(),
        appearance: def.appearance,
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
/// （它才知道要不要连带改 `theme` / `skin` 字段）。
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
pub fn switch_to_builtin(
    appearance: Appearance,
    window: Option<&mut Window>,
    cx: &mut App,
) -> TerminalTheme {
    Theme::change(appearance.theme_mode(), window, cx);
    builtin_terminal_theme(appearance)
}

/// 扫一遍 themes/ 目录，返回能用的包（坏包跳过并打日志，不阻塞列表）。
///
/// 设置页的皮肤列表用这个：一个坏包不该让整张列表打不开。
pub fn list_theme_packs(packs: &mt_config::ThemePacks) -> Result<Vec<(ThemePackDef, PathBuf)>> {
    let mut out = Vec::new();
    for entry in packs.list()? {
        match parse_theme_pack(&entry.theme_id, &entry.theme_json) {
            Ok(def) => out.push((def, entry.dir)),
            Err(e) => eprintln!("[mt-ui] 主题包 {} 无效，已跳过: {e:#}", entry.theme_id),
        }
    }
    Ok(out)
}

// TODO(背景图，本轮未做)：
// 1. 数据已经齐了（[`BackgroundArt`]：图片路径 / 焦点 / 压暗色）；
// 2. 缺的是一个窗口级的背景 element：`img(path)` 铺在三栏之下，按 cover 语义
//    自算 bounds —— 容器宽高比 vs 图片宽高比，取较大的缩放系数，
//    再按 focus 把溢出的那一维平移 `(container - scaled) * focus`；
//    contain 是取较小系数 + 居中，两者共用同一个函数，差一个 `min`/`max`；
// 3. 压暗层就是在图上盖一个 `fill(bounds, art.dim)` 的 quad；
// 4. 终端侧的路已经通了：「默认背景不发 quad」+ 本模块给的半透明 `background`，
//    背景图会自动透上来，**不需要改 TerminalElement 一行**；
// 5. 唯一要留神的是 overdraw：三栏面板都半透明时会叠好几层，
//    `docs/gpui-migration.md` 第 5 节的坑位表里点了这条。

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
