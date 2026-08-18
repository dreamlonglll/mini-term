//! 应用配色与几个复用的小件。
//!
//! # 一张表,两个来源
//!
//! [`Palette`] 是壳的全部配色 token,逐条对应 `src/styles.css` 的 CSS 变量。
//! 取值有两条来源:
//!
//! - **内置外观**:`Palette::dark()` / `Palette::light()`,逐值抄 `:root` 与
//!   `:root[data-theme="light"]`;
//! - **外置主题包**:`Palette::from_pack`,映射逐条对齐
//!   `src/utils/themePackManager.ts::buildTokenMap`。
//!
//! 装配在 [`crate::theme`],这里只负责「当前是哪一份」。
//!
//! # 为什么用 thread_local 而不是给每个取色函数加 `cx`
//!
//! `ui::accent()` 这类调用散在十几个文件、上百处;为了换主题给它们统统加一个
//! `&App` 参数,收益只是省掉一个进程内单例。gpui 的视图全在主线程上跑,
//! 一份 `thread_local` 快照足够,而且**这是唯一的替换点** —— 换主题时
//! [`set_palette`] 改一次,下一帧所有视图自动跟着变。

use std::cell::RefCell;

use gpui::{
    Div, ElementId, Hsla, InteractiveElement, IntoElement, ParentElement, Stateful, Styled, div,
    px,
};
use mt_ui::rgb8;
use mt_ui::theme_bridge::{AppliedThemePack, ThemePackDef, parse_color};

use crate::tree::PaneStatus;

/// 壳的配色 token 表(对应 `styles.css` 的一组 CSS 变量)。
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub bg_base: Hsla,
    pub bg_surface: Hsla,
    pub bg_elevated: Hsla,
    pub bg_overlay: Hsla,
    pub bg_terminal: Hsla,
    pub text_primary: Hsla,
    pub text_secondary: Hsla,
    pub text_muted: Hsla,
    pub accent: Hsla,
    pub accent_subtle: Hsla,
    pub border_subtle: Hsla,
    pub border_default: Hsla,
    pub color_success: Hsla,
    pub color_error: Hsla,
    pub color_ai_working: Hsla,
    pub color_folder: Hsla,
    pub color_file: Hsla,
    pub color_info: Hsla,
}

/// 乘性改 alpha(与前端 `withAlpha` / `scaleAlpha` 同语义)。
fn alpha(color: Hsla, a: f32) -> Hsla {
    Hsla {
        a: (color.a * a).clamp(0.0, 1.0),
        ..color
    }
}

impl Palette {
    /// 暗色基线:逐值取自 `src/styles.css` 的 `:root`。
    pub fn dark() -> Self {
        Self {
            bg_base: rgb8(0x08, 0x07, 0x06),
            bg_surface: rgb8(0x12, 0x11, 0x10),
            bg_elevated: rgb8(0x1c, 0x1a, 0x18),
            bg_overlay: rgb8(0x25, 0x23, 0x20),
            bg_terminal: rgb8(0x0a, 0x09, 0x08),
            text_primary: rgb8(0xf0, 0xec, 0xe6),
            text_secondary: rgb8(0xa8, 0xa0, 0x98),
            text_muted: rgb8(0x6a, 0x62, 0x58),
            accent: rgb8(0xc8, 0x80, 0x5a),
            accent_subtle: Hsla {
                a: 0.10,
                ..rgb8(0xc8, 0x80, 0x5a)
            },
            border_subtle: Hsla {
                a: 0.05,
                ..rgb8(0xff, 0xff, 0xff)
            },
            border_default: Hsla {
                a: 0.08,
                ..rgb8(0xff, 0xff, 0xff)
            },
            color_success: rgb8(0x6b, 0xb8, 0x7a),
            color_error: rgb8(0xd4, 0x60, 0x5a),
            color_ai_working: rgb8(0xf5, 0xc5, 0x18),
            color_folder: rgb8(0xd4, 0xc8, 0xa0),
            color_file: rgb8(0x7d, 0xcf, 0xb8),
            color_info: rgb8(0x6a, 0x9f, 0xd4),
        }
    }

    /// 亮色基线:逐值取自 `src/styles.css` 的 `:root[data-theme="light"]`。
    pub fn light() -> Self {
        Self {
            bg_base: rgb8(0xff, 0xff, 0xff),
            bg_surface: rgb8(0xf5, 0xf5, 0xf5),
            bg_elevated: rgb8(0xeb, 0xeb, 0xeb),
            bg_overlay: rgb8(0xe0, 0xe0, 0xe0),
            bg_terminal: rgb8(0xfa, 0xfa, 0xfa),
            text_primary: rgb8(0x0a, 0x0a, 0x0a),
            text_secondary: rgb8(0x50, 0x50, 0x50),
            text_muted: rgb8(0x80, 0x80, 0x80),
            accent: rgb8(0xb0, 0x68, 0x30),
            accent_subtle: Hsla {
                a: 0.094, // #b0683018
                ..rgb8(0xb0, 0x68, 0x30)
            },
            border_subtle: Hsla {
                a: 0.06,
                ..rgb8(0x00, 0x00, 0x00)
            },
            border_default: Hsla {
                a: 0.10,
                ..rgb8(0x00, 0x00, 0x00)
            },
            color_success: rgb8(0x2d, 0x8a, 0x46),
            color_error: rgb8(0xc0, 0x39, 0x2b),
            color_ai_working: rgb8(0xc4, 0x52, 0x1a),
            color_folder: rgb8(0x8a, 0x7a, 0x40),
            color_file: rgb8(0x1a, 0x8a, 0x6a),
            color_info: rgb8(0x28, 0x60, 0xa0),
        }
    }

    /// 外置主题包 → token 表。映射逐条对齐 `themePackManager.ts::buildTokenMap`。
    ///
    /// `applied` 只用来拿两个已经算好的量:`surface_opacity`(面板半透明度,
    /// 无背景图时是 1.0)与终端背景色(已含 `terminalOpacity`)。
    ///
    /// 包里没有的语义色(error / ai-working / folder / file)保留该明暗的内置值 ——
    /// 前端同样不映射它们。`accentAlt` 在前端归到 `--color-warning`,壳里目前没有
    /// 对应 token,丢掉不用。
    pub fn from_pack(def: &ThemePackDef, applied: &AppliedThemePack) -> Self {
        let base = if def.appearance.is_dark() {
            Self::dark()
        } else {
            Self::light()
        };
        let c = &def.colors;
        let color = |raw: &str, fallback: Hsla| -> Hsla {
            parse_color(raw).map(Into::into).unwrap_or(fallback)
        };
        let background = color(&c.background, base.bg_base);
        let panel = color(&c.panel, background);
        let panel_alt = color(&c.panel_alt, panel);
        let accent = color(&c.accent, base.accent);
        let text = color(&c.text, base.text_primary);
        let muted = color(&c.muted, base.text_muted);
        let line = color(&c.line, base.border_default);
        let so = applied.surface_opacity;

        Self {
            bg_base: background,
            // 面板半透明才透得出背景图;无背景图时 surface_opacity = 1.0
            bg_surface: alpha(panel, so),
            bg_elevated: alpha(panel_alt, so),
            // 浮层始终不透明:弹窗叠在任意内容上,半透明是拿可读性换观感
            bg_overlay: panel_alt,
            bg_terminal: applied.terminal.background,
            text_primary: text,
            text_secondary: alpha(text, 0.75),
            text_muted: muted,
            accent,
            accent_subtle: alpha(accent, 0.18),
            border_subtle: alpha(line, 0.6),
            border_default: line,
            color_success: c
                .highlight
                .as_deref()
                .map(|v| color(v, base.color_success))
                .unwrap_or(base.color_success),
            color_info: c
                .secondary
                .as_deref()
                .map(|v| color(v, base.color_info))
                .unwrap_or(base.color_info),
            ..base
        }
    }
}

thread_local! {
    /// 当前生效的配色。改它的唯一入口是 [`set_palette`]。
    static CURRENT: RefCell<Palette> = RefCell::new(Palette::dark());
}

/// 换一整套配色(换主题包 / 切亮暗)。**唯一替换点**。
pub fn set_palette(palette: Palette) {
    CURRENT.with(|p| *p.borrow_mut() = palette);
}

/// 当前配色的一份拷贝。
#[allow(dead_code)] // 设置面板的预览卡片要整套取(下一批)
pub fn palette() -> Palette {
    CURRENT.with(|p| p.borrow().clone())
}

fn token(pick: impl Fn(&Palette) -> Hsla) -> Hsla {
    CURRENT.with(|p| pick(&p.borrow()))
}

// --- 背景 ---
/// `--bg-base`
pub fn bg_base() -> Hsla {
    token(|p| p.bg_base)
}
/// `--bg-surface`
pub fn bg_surface() -> Hsla {
    token(|p| p.bg_surface)
}
/// `--bg-elevated`
pub fn bg_elevated() -> Hsla {
    token(|p| p.bg_elevated)
}
/// `--bg-overlay`
pub fn bg_overlay() -> Hsla {
    token(|p| p.bg_overlay)
}
/// `--bg-terminal`
pub fn bg_terminal() -> Hsla {
    token(|p| p.bg_terminal)
}

// --- 前景 ---
/// `--text-primary`
pub fn text_primary() -> Hsla {
    token(|p| p.text_primary)
}
/// `--text-secondary`
pub fn text_secondary() -> Hsla {
    token(|p| p.text_secondary)
}
/// `--text-muted`
pub fn text_muted() -> Hsla {
    token(|p| p.text_muted)
}

// --- 强调与边框 ---
/// `--accent`
pub fn accent() -> Hsla {
    token(|p| p.accent)
}
/// `--accent-subtle`(原值是带 alpha 的 accent)
pub fn accent_subtle() -> Hsla {
    token(|p| p.accent_subtle)
}
/// `--border-subtle`
pub fn border_subtle() -> Hsla {
    token(|p| p.border_subtle)
}
/// `--border-default`
pub fn border_default() -> Hsla {
    token(|p| p.border_default)
}

// --- 语义色 ---
/// `--color-success`
pub fn color_success() -> Hsla {
    token(|p| p.color_success)
}
/// `--color-error`
pub fn color_error() -> Hsla {
    token(|p| p.color_error)
}
/// `--color-ai-working`
pub fn color_ai_working() -> Hsla {
    token(|p| p.color_ai_working)
}
/// `--color-folder`
pub fn color_folder() -> Hsla {
    token(|p| p.color_folder)
}
/// `--color-file`
pub fn color_file() -> Hsla {
    token(|p| p.color_file)
}

/// `--color-info`(统计面板的区块标题竖条等)
pub fn color_info() -> Hsla {
    token(|p| p.color_info)
}

// --- 复用小件 ---
//
// 面板/Modal 里反复出现的三种东西:次要按钮、主按钮、区块标题。写死在各处的话
// 改一次配色要翻十个文件,而 i18n 与主题桥都指着这一张表做替换点。

/// 次要按钮(边框 + 淡色文字,hover 转 accent)。
pub fn ghost_button(id: impl Into<ElementId>, label: impl Into<String>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px(px(10.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(border_default())
        .text_size(px(12.0))
        .text_color(text_secondary())
        .cursor_pointer()
        .hover(|el| el.border_color(accent()).text_color(accent()))
        .child(label.into())
}

/// 主按钮(实心 accent)。
pub fn primary_button(id: impl Into<ElementId>, label: impl Into<String>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px(px(12.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .bg(accent())
        .text_size(px(12.0))
        .text_color(bg_base())
        .cursor_pointer()
        .hover(|el| el.opacity(0.9))
        .child(label.into())
}

/// 危险动作按钮(删除类)。
///
/// **单独一个函数而不是 `ghost_button(..).hover(..)`** —— gpui 的 `Div` 只允许设一次
/// hover 样式,第二次直接 panic(`hover style already set`),而 `ghost_button`
/// 里已经设过了。
pub fn danger_button(id: impl Into<ElementId>, label: impl Into<String>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px(px(10.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(border_default())
        .text_size(px(12.0))
        .text_color(text_secondary())
        .cursor_pointer()
        .hover(|el| el.border_color(color_error()).text_color(color_error()))
        .child(label.into())
}

/// 区块标题:左侧竖条 + 文字(对齐 `usage/UsageStatsModal.tsx` 的 `Section`)。
pub fn section_title(text: impl Into<String>) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .mb(px(8.0))
        .child(
            div()
                .w(px(2.0))
                .h(px(12.0))
                .rounded(px(1.0))
                .bg(color_info()),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(text_primary())
                .child(text.into()),
        )
}

/// 状态灯的颜色(对齐 `src/components/StatusDot.tsx` 的 `STATUS_COLORS`)。
pub fn status_color(status: PaneStatus) -> Hsla {
    match status {
        PaneStatus::Idle => text_muted(),
        PaneStatus::AiIdle => color_success(),
        PaneStatus::AiWorking => color_ai_working(),
        PaneStatus::Error => color_error(),
    }
}

/// 四态状态灯。
///
/// 原版是 SVG 的**形状 + 颜色**双编码(空心圈 / 实心带勾 / 半填充圆环 / 实心带叉),
/// 这里先用「空心圈 vs 实心点 + 外环」表达同一层区分:色觉障碍下仍能靠填充与否
/// 分出 idle 与其余三态。勾/叉字形等图标体系接上后再补(见交付说明的已知缺口)。
pub fn status_dot(status: PaneStatus) -> impl IntoElement {
    let color = status_color(status);
    let outer = div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(11.0))
        .h(px(11.0));
    match status {
        // 空心细圈
        PaneStatus::Idle => outer.child(
            div()
                .w(px(9.0))
                .h(px(9.0))
                .rounded_full()
                .border_1()
                .border_color(color),
        ),
        // 实心点(已完成 / 异常)
        PaneStatus::AiIdle | PaneStatus::Error => {
            outer.child(div().w(px(9.0)).h(px(9.0)).rounded_full().bg(color))
        }
        // 环 + 芯:「进行中」在静态下也要能与「已完成」分开
        PaneStatus::AiWorking => outer.child(
            div()
                .w(px(11.0))
                .h(px(11.0))
                .rounded_full()
                .border_2()
                .border_color(color)
                .flex()
                .items_center()
                .justify_center()
                .child(div().w(px(3.0)).h(px(3.0)).rounded_full().bg(color)),
        ),
    }
}
