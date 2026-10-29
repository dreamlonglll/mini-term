//! 「提交与变更」这条线(更改列表 / 提交区 / 单文件 diff 弹窗)共用的小件:
//! 线性图标的形状表、图标按钮、计数胶囊、等宽数字。
//!
//! # 图标
//!
//! 面板里原先的 unicode 字形按钮(`+ − ▾ ✕ ↻`…)在不同字体下粗细、基线各不一样,
//! 换成 [`VectorIcon`] 自绘的线性图标:几何原样搬 [Lucide](https://lucide.dev)
//! (ISC 许可)的 path data,viewBox `0 0 24 24`,线宽口径与 `activity_bar.rs` 的
//! `STROKE` 一致(1.2 / 16 单位方框)。常量统一 `ICON_` 前缀,合并后收进共享模块。
//!
//! # 图标按钮的手感
//!
//! 点击区 24px 方形、圆角 4px;hover 底 `ui::bg_elevated()`、图标转 `ui::text_primary()`;
//! 禁用态压成 `ui::text_muted()` 并降透明度、不挂手型。图标是自绘元素,不吃 `text_color`,
//! 「hover 转色」靠叠一层只在组 hover 时可见的同形图标做(见 [`icon_button`])。

use gpui::{
    Div, ElementId, Hsla, InteractiveElement as _, ParentElement as _, SharedString, Stateful,
    Styled, div, prelude::FluentBuilder as _, px,
};
use mt_ui::icons::{Geom, Ink, Shape, VectorIcon};

use crate::ui;

/// 线宽:与 `activity_bar.rs` 的 `STROKE` 同一口径(原版 `stroke-width="1.2"` / 16)。
const STROKE: f32 = 1.2 / 16.0;
/// Lucide 的画布。
const LUCIDE: (f32, f32, f32) = (0.0, 0.0, 24.0);

/// 一笔 Lucide 描边。
const fn stroke(d: &'static str) -> Shape {
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Path {
            d,
            view: LUCIDE,
            even_odd: false,
        },
    )
}

/// 暂存(`plus`)。
pub const ICON_PLUS: &[Shape] = &[stroke("M5 12h14"), stroke("M12 5v14")];
/// 取消暂存(`minus`)。
pub const ICON_MINUS: &[Shape] = &[stroke("M5 12h14")];
/// 丢弃 / 回滚(`undo-2`)。
pub const ICON_UNDO: &[Shape] = &[
    stroke("M9 14 4 9l5-5"),
    stroke("M4 9h10.5a5.5 5.5 0 0 1 5.5 5.5a5.5 5.5 0 0 1-5.5 5.5H11"),
];
/// 展开态 / 下拉(`chevron-down`)。
pub const ICON_CHEVRON_DOWN: &[Shape] = &[stroke("m6 9 6 6 6-6")];
/// 折叠态 / 下一个(`chevron-right`)。
pub const ICON_CHEVRON_RIGHT: &[Shape] = &[stroke("m9 18 6-6-6-6")];
/// 上一个(`chevron-left`)。
pub const ICON_CHEVRON_LEFT: &[Shape] = &[stroke("m15 18-6-6 6-6")];
/// 上一处改动(`arrow-up`)。
pub const ICON_ARROW_UP: &[Shape] = &[stroke("m5 12 7-7 7 7"), stroke("M12 19V5")];
/// 下一处改动(`arrow-down`)。
pub const ICON_ARROW_DOWN: &[Shape] = &[stroke("M12 5v14"), stroke("m19 12-7 7-7-7")];
/// 提交消息历史(`history`)。
pub const ICON_HISTORY: &[Shape] = &[
    stroke("M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"),
    stroke("M3 3v5h5"),
    stroke("M12 7v5l4 2"),
];
/// 关闭(`x`)。
pub const ICON_CLOSE: &[Shape] = &[stroke("M18 6 6 18"), stroke("m6 6 12 12")];
/// 复制(`copy`)。
pub const ICON_COPY: &[Shape] = &[
    stroke("M10 8h10a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H10a2 2 0 0 1-2-2V10a2 2 0 0 1 2-2z"),
    stroke("M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"),
];
/// 刷新(`refresh-cw`)。
pub const ICON_REFRESH: &[Shape] = &[
    stroke("M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"),
    stroke("M21 3v5h-5"),
    stroke("M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"),
    stroke("M8 16H3v5"),
];
/// 冲突 / 警告(`triangle-alert`)。末尾那一点 Lucide 是 `M12 17h.01` 的零长线段
/// (靠圆头线帽成点),这里的描边没有圆头,改成一个实心小圆。
pub const ICON_ALERT: &[Shape] = &[
    stroke("m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3"),
    stroke("M12 9v4"),
    Shape::fill(
        Ink::Current,
        Geom::Circle {
            c: (12.0 / 24.0, 17.0 / 24.0),
            r: 1.1 / 24.0,
        },
    ),
];
/// 工作区干净(`circle-check-big`),空态的大图标。
pub const ICON_CLEAN: &[Shape] = &[
    stroke("M21.801 10A10 10 0 1 1 17 3.335"),
    stroke("m9 11 3 3L22 4"),
];

/// 图标的标准尺寸。
pub const ICON_SIZE: f32 = 14.0;
/// 图标按钮的点击区边长。
pub const ICON_BUTTON: f32 = 24.0;

/// 图标按钮的组名(叠层图标靠它判「这颗按钮被 hover 了」;gpui 的组按最近祖先认,
/// 每颗按钮各用各的同名组互不串)。
const ICON_BUTTON_GROUP: &str = "git-commit-icon-button";

/// 图标按钮。`enabled = false` 时整颗变灰、不挂手型 —— 调用方也别挂 `on_click`。
/// tooltip 由调用方挂(`.tip(..)`),这里不管文案。
pub fn icon_button(
    id: impl Into<ElementId>,
    shapes: &'static [Shape],
    enabled: bool,
) -> Stateful<Div> {
    icon_button_toned(id, shapes, enabled, ui::text_primary())
}

/// 危险动作(丢弃)的图标按钮:hover 时图标转错误色。
pub fn danger_icon_button(
    id: impl Into<ElementId>,
    shapes: &'static [Shape],
    enabled: bool,
) -> Stateful<Div> {
    icon_button_toned(id, shapes, enabled, ui::color_error())
}

fn icon_button_toned(
    id: impl Into<ElementId>,
    shapes: &'static [Shape],
    enabled: bool,
    hover_ink: Hsla,
) -> Stateful<Div> {
    let base = if enabled {
        ui::text_secondary()
    } else {
        ui::text_muted()
    };
    div()
        .id(id)
        .group(ICON_BUTTON_GROUP)
        .relative()
        .flex_none()
        .w(px(ICON_BUTTON))
        .h(px(ICON_BUTTON))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .child(VectorIcon::new(shapes, px(ICON_SIZE)).ink(base))
        .when(enabled, |el| {
            el.cursor_pointer()
                .hover(|el| el.bg(ui::bg_elevated()))
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .invisible()
                        .group_hover(ICON_BUTTON_GROUP, |s| s.visible())
                        .child(VectorIcon::new(shapes, px(ICON_SIZE)).ink(hover_ink)),
                )
        })
        .when(!enabled, |el| el.opacity(0.45))
}

/// 等宽数字(计数、行号、+/− 统计):各数位等宽,竖着排才对得齐。
pub fn mono(mut el: Div) -> Div {
    let style = el.text_style();
    style.font_family = Some("Cascadia Mono".into());
    style.font_fallbacks = Some(gpui::FontFallbacks::from_fonts(vec![
        "Consolas".into(),
        "JetBrains Mono".into(),
        "Menlo".into(),
        "DejaVu Sans Mono".into(),
        "Microsoft YaHei".into(),
    ]));
    el
}

/// 胶囊(计数 / 状态):约 18px 高、全圆角,底 = 语义色 15% alpha,字 = 语义色。
pub fn pill(text: impl Into<SharedString>, color: Hsla) -> Div {
    mono(div())
        .flex_none()
        .h(px(18.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .rounded_full()
        .bg(ui::with_alpha(color, 0.15))
        .text_color(color)
        .text_size(ui::font_px(11.0))
        .child(text.into())
}

/// 小号文字按钮(hunk 头上的「暂存此块」这类):边框 + 弱化文字,hover 转 accent /
/// 危险色。禁用时整颗压灰、不挂手型。
pub fn text_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
    danger: bool,
) -> Stateful<Div> {
    let tone = if danger {
        ui::color_error()
    } else {
        ui::accent()
    };
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .px(px(8.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(ui::border_default())
        .text_size(ui::font_px(11.0))
        .text_color(if danger && enabled {
            ui::color_error()
        } else {
            ui::text_secondary()
        })
        .child(label.into())
        .when(enabled, |el| {
            el.cursor_pointer()
                .bg(ui::bg_surface())
                .hover(move |el| el.border_color(tone).text_color(tone))
        })
        .when(!enabled, |el| el.opacity(0.45))
}

/// 一组图标都还能画:全部点落在单位方框内(Lucide 的 path 原样搬进来,写错一位
/// 数字就会画出框)。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 图标的点都在单位方框内() {
        for table in [
            ICON_PLUS,
            ICON_MINUS,
            ICON_UNDO,
            ICON_CHEVRON_DOWN,
            ICON_CHEVRON_RIGHT,
            ICON_CHEVRON_LEFT,
            ICON_ARROW_UP,
            ICON_ARROW_DOWN,
            ICON_HISTORY,
            ICON_CLOSE,
            ICON_COPY,
            ICON_REFRESH,
            ICON_ALERT,
            ICON_CLEAN,
        ] {
            for shape in table {
                let subpaths = shape.geom.subpaths();
                assert!(!subpaths.is_empty());
                for (pts, _) in subpaths.iter() {
                    assert!(pts.len() >= 2, "{:?}", shape.geom);
                    for (x, y) in pts {
                        assert!(
                            (-0.01..=1.01).contains(x) && (-0.01..=1.01).contains(y),
                            "{:?} 越界 ({x}, {y})",
                            shape.geom
                        );
                    }
                }
            }
        }
    }
}
