//! 左侧窄边条(对照 `src/components/ActivityBar.tsx`)。
//!
//! # 为什么图标是自绘而不是 `gpui_component::IconName`
//!
//! `IconName` 只是一张「枚举 → `icons/xxx.svg` 路径」的映射表,**图形本体不在
//! gpui-component 这个 crate 里** —— crates.io 上的 0.5.1 包里既没有 `assets/`
//! 也没有任何 `AssetSource` 实现(上游仓库把 lucide 的 svg 放在示例程序的资产
//! 目录里,由宿主自己注册)。mt-app 现在没有注册 asset source,直接用
//! `IconName::Settings` 的结果是**一片空白**,而且这种失败在编译期看不出来。
//!
//! 于是走 mt-ui 已经在用的那条路:[`mt_ui::icons::VectorIcon`] 的形状 DSL。
//! 好处是几何直接照抄原版 SVG 的 `path`(下面每张表的注释里都留着原文),
//! 与状态灯/品牌图标同一个渲染器,不必再引一套资产管线。
//!
//! # 只画有落点的按钮
//!
//! 原版 8 个按钮里 SSH / 更新提醒两个在 GPUI 侧还没有对应功能,
//! **不放占位**(灰着点不动的按钮比没有更让人困惑)。其余六个:
//! 折叠中间栏 / AI 历史 / Git 变更 / 用量统计 / 移动端 / 设置,外加一个原版没有的
//! 「跳到已完成」。(Git 那颗由 V 批补上,与右抽屉的 sessions⇄git 段控件同一个开关;
//! 移动端那颗由 U 批补上,位置照原版排在「设置」之前。)

use gpui::{
    Div, ElementId, InteractiveElement, ParentElement, Stateful, StatefulInteractiveElement,
    Styled, div, prelude::FluentBuilder as _, px,
};
use gpui_component::tooltip::Tooltip;
use mt_ui::icons::{Geom, Ink, Shape, VectorIcon};

use crate::ui;

/// 边条宽度。原版 `style={{ width: 44 }}`。
pub const WIDTH: f32 = 44.0;
/// 按钮尺寸。原版 `w-8 h-8`(32px)。
const BUTTON: f32 = 32.0;
/// 图标尺寸。原版每个 svg 都是 `width="18" height="18"`。
const ICON: f32 = 18.0;

/// 单位方框换算:原版 viewBox 是 `0 0 16 16`,除以 16 即可。
const fn u(v: f32) -> f32 {
    v / 16.0
}
/// 原版全部图标统一 `stroke-width="1.2"`。
const STROKE: f32 = 1.2 / 16.0;

/// 折叠 / 展开中间栏。原版 `ICON_PANEL`:
/// `<rect x="2" y="3" width="12" height="10" rx="1.5"/>` + `<path d="M6.5 3v10"/>`。
pub const PANEL: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Rect {
            x: u(2.0),
            y: u(3.0),
            w: u(12.0),
            h: u(10.0),
            round: u(1.5),
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(6.5), u(3.0)), (u(6.5), u(13.0))]),
    ),
];

/// AI 历史抽屉。原版 `ICON_SESSIONS`:`<path d="M2 3h12v8H5l-3 3V3z"/>`
/// —— 一个带小尾巴的对话气泡(闭合路径)。
pub const SESSIONS: &[Shape] = &[Shape::line(
    Ink::Current,
    STROKE,
    Geom::Polygon(&[
        (u(2.0), u(3.0)),
        (u(14.0), u(3.0)),
        (u(14.0), u(11.0)),
        (u(5.0), u(11.0)),
        (u(2.0), u(14.0)),
    ]),
)];

/// Git 变更。原版 `ICON_GIT`(`ActivityBar.tsx:24-31`)—— 三个节点 + 一条主干
/// 加一条并回主干的分支:
///
/// ```text
/// <circle cx="5"  cy="4"  r="1.5"/>
/// <circle cx="11" cy="4"  r="1.5"/>
/// <circle cx="5"  cy="12" r="1.5"/>
/// <path d="M5 5.5v5M11 5.5v1a2 2 0 01-2 2H5"/>
/// ```
///
/// 那条 path 的圆角拐弯(`a2 2 0 01-2 2`)在 18px 下半径只有 2px,
/// 用折线近似(取圆弧的起点 / 45° 中点 / 终点三个顶点)。
pub const GIT: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Circle {
            c: (u(5.0), u(4.0)),
            r: u(1.5),
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Circle {
            c: (u(11.0), u(4.0)),
            r: u(1.5),
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Circle {
            c: (u(5.0), u(12.0)),
            r: u(1.5),
        },
    ),
    // 左侧主干:M5 5.5 v5
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(5.0), u(5.5)), (u(5.0), u(10.5))]),
    ),
    // 右侧分支:M11 5.5 v1,再向左拐回主干
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[
            (u(11.0), u(5.5)),
            (u(11.0), u(6.5)),
            (u(10.41), u(7.91)),
            (u(9.0), u(8.5)),
            (u(5.0), u(8.5)),
        ]),
    ),
];

/// 用量统计。原版 `ICON_STATS`:一条底轴 + 三根高低不同的柱子
/// (`M2.5 13.5h11` / `M4 13.5V9M8 13.5V4.5M12 13.5V7`)。
pub const STATS: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(2.5), u(13.5)), (u(13.5), u(13.5))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(4.0), u(13.5)), (u(4.0), u(9.0))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(8.0), u(13.5)), (u(8.0), u(4.5))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(12.0), u(13.5)), (u(12.0), u(7.0))]),
    ),
];

/// 移动端。原版 `ICON_MOBILE`(`ActivityBar.tsx:48-53`)—— 一部竖着的手机:
/// `<rect x="4.5" y="1.5" width="7" height="13" rx="1.5"/>` + `<path d="M7 12.5h2"/>`
/// (机身圆角矩形 + 底部那道 Home 键短横)。
pub const MOBILE: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Rect {
            x: u(4.5),
            y: u(1.5),
            w: u(7.0),
            h: u(13.0),
            round: u(1.5),
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(7.0), u(12.5)), (u(9.0), u(12.5))]),
    ),
];

/// 设置。原版 `ICON_SETTINGS` 的 6 齿齿轮轮廓 + 中心轴孔 —— 那条 `path` 的
/// 24 个顶点逐个抄下来(原版注释写明:轮缘必须是连续的、齿长在轮廓上,
/// 「中心小圆 + 放射短线」画出来是太阳不是齿轮;18px 下取 6 齿才咬得出形状)。
pub const SETTINGS: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polygon(&[
            (u(6.40), u(1.60)),
            (u(9.60), u(1.60)),
            (u(9.53), u(3.56)),
            (u(11.08), u(4.45)),
            (u(12.75), u(3.42)),
            (u(14.34), u(6.18)),
            (u(12.61), u(7.10)),
            (u(12.61), u(8.90)),
            (u(14.34), u(9.82)),
            (u(12.75), u(12.58)),
            (u(11.08), u(11.55)),
            (u(9.53), u(12.44)),
            (u(9.60), u(14.40)),
            (u(6.40), u(14.40)),
            (u(6.47), u(12.44)),
            (u(4.92), u(11.55)),
            (u(3.25), u(12.58)),
            (u(1.66), u(9.82)),
            (u(3.39), u(8.90)),
            (u(3.39), u(7.10)),
            (u(1.66), u(6.18)),
            (u(3.25), u(3.42)),
            (u(4.92), u(4.45)),
            (u(6.47), u(3.56)),
        ]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Circle {
            c: (0.5, 0.5),
            r: u(2.3),
        },
    ),
];

/// 一个边条按钮的外壳(不含 `on_click`,由调用方挂)。
///
/// 配色逐条对照原版 `btnClass`:激活 = 主文本色 + `--border-subtle` 底,
/// 未激活 = 淡字、hover 转主文本色;激活时左侧还有一根 accent 竖条
/// (原版 `ACCENT_BAR`,**始终占位**靠透明度切换的写法在 gpui 里没必要,
/// 这里直接按需追加子元素)。
pub fn strip_button(
    id: impl Into<ElementId>,
    shapes: &'static [Shape],
    tip: &'static str,
    active: bool,
) -> Stateful<Div> {
    let color = if active {
        ui::text_primary()
    } else {
        ui::text_muted()
    };
    div()
        .id(id)
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .w(px(BUTTON))
        .h(px(BUTTON))
        .flex_none()
        .rounded(px(4.0))
        .cursor_pointer()
        .when(active, |el| el.bg(ui::border_subtle()))
        .hover(|el| el.bg(ui::border_subtle()))
        .child(VectorIcon::new(shapes, px(ICON)).ink(color))
        .when(active, |el| {
            el.child(
                div()
                    .absolute()
                    .left_0()
                    .top(px((BUTTON - 16.0) / 2.0))
                    .w(px(2.0))
                    .h(px(16.0))
                    .rounded(px(1.0))
                    .bg(ui::accent()),
            )
        })
        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
}

/// 按钮分组之间的细分隔线(原版 `w-6 h-px bg-[var(--border-default)] my-1`)。
pub fn divider() -> Div {
    div()
        .w(px(24.0))
        .h(px(1.0))
        .my(px(4.0))
        .bg(ui::border_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 形状表的点必须全落在单位方框内 —— 越界会画到相邻按钮上。
    /// (mt-ui 对自己那批图标有同名约束,这里是本 crate 这四张表的同款体检。)
    #[test]
    fn 边条图标的点全在单位方框内() {
        let mut points = 0usize;
        for shapes in [PANEL, SESSIONS, GIT, STATS, SETTINGS, MOBILE] {
            for shape in shapes {
                let (pts, _) = shape.geom.points();
                for (x, y) in pts {
                    assert!(
                        (-0.001..=1.001).contains(&x) && (-0.001..=1.001).contains(&y),
                        "越界点 ({x}, {y})"
                    );
                    points += 1;
                }
            }
        }
        assert!(points > 60, "形状表看起来没被遍历到(只有 {points} 个点)");
    }

    /// 齿轮是「连续轮缘 + 中心轴孔」两笔,顶点数就是原版 path 的 24 个 ——
    /// 少一个都说明抄漏了(缺口会让轮廓不闭合)。
    #[test]
    fn 齿轮轮廓是二十四个顶点加一个轴孔() {
        assert_eq!(SETTINGS.len(), 2);
        let Geom::Polygon(pts) = SETTINGS[0].geom else {
            panic!("第一笔应该是闭合轮廓");
        };
        assert_eq!(pts.len(), 24);
        assert!(matches!(SETTINGS[1].geom, Geom::Circle { .. }));
    }
}
