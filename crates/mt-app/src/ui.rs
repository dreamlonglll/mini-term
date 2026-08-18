//! 应用配色与几个复用的小件。
//!
//! 配色逐值取自 `src/styles.css` 的**暗色**变量(`:root`)。i18n 与亮色/主题包
//! 桥接是后续批次的事,这里先把值钉死,免得各处各写一个近似色。

use gpui::{Hsla, IntoElement, ParentElement, Styled, div, px};
use mt_ui::rgb8;

use crate::tree::PaneStatus;

// --- 背景 ---
/// `--bg-base`
pub fn bg_base() -> Hsla {
    rgb8(0x08, 0x07, 0x06)
}
/// `--bg-surface`
pub fn bg_surface() -> Hsla {
    rgb8(0x12, 0x11, 0x10)
}
/// `--bg-elevated`
pub fn bg_elevated() -> Hsla {
    rgb8(0x1c, 0x1a, 0x18)
}
/// `--bg-overlay`
pub fn bg_overlay() -> Hsla {
    rgb8(0x25, 0x23, 0x20)
}
/// `--bg-terminal`
pub fn bg_terminal() -> Hsla {
    rgb8(0x0a, 0x09, 0x08)
}

// --- 前景 ---
/// `--text-primary`
pub fn text_primary() -> Hsla {
    rgb8(0xf0, 0xec, 0xe6)
}
/// `--text-secondary`
pub fn text_secondary() -> Hsla {
    rgb8(0xa8, 0xa0, 0x98)
}
/// `--text-muted`
pub fn text_muted() -> Hsla {
    rgb8(0x6a, 0x62, 0x58)
}

// --- 强调与边框 ---
/// `--accent`
pub fn accent() -> Hsla {
    rgb8(0xc8, 0x80, 0x5a)
}
/// `--accent-subtle`(原值是带 alpha 的 accent)
pub fn accent_subtle() -> Hsla {
    Hsla {
        a: 0.10,
        ..accent()
    }
}
/// `--border-subtle`
pub fn border_subtle() -> Hsla {
    Hsla {
        a: 0.05,
        ..rgb8(0xff, 0xff, 0xff)
    }
}
/// `--border-default`
pub fn border_default() -> Hsla {
    Hsla {
        a: 0.08,
        ..rgb8(0xff, 0xff, 0xff)
    }
}

// --- 语义色 ---
/// `--color-success`
pub fn color_success() -> Hsla {
    rgb8(0x6b, 0xb8, 0x7a)
}
/// `--color-error`
pub fn color_error() -> Hsla {
    rgb8(0xd4, 0x60, 0x5a)
}
/// `--color-ai-working`
pub fn color_ai_working() -> Hsla {
    rgb8(0xf5, 0xc5, 0x18)
}
/// `--color-folder`
pub fn color_folder() -> Hsla {
    rgb8(0xd4, 0xc8, 0xa0)
}
/// `--color-file`
pub fn color_file() -> Hsla {
    rgb8(0x7d, 0xcf, 0xb8)
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
