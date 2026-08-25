//! 用量面板 KPI 图标(逐点移植 `KpiCards.tsx` 的 16 viewBox stroke path)。
//!
//! 六枚都是**纯几何数据**,没有组件:宿主拿 [`VectorIcon`](super::vector::VectorIcon)
//! 自己定尺寸与墨色(KPI 卡 16px、刷新钮 14px)。放这儿是因为形状表按本仓分层
//! 属于 mt-ui —— mt-app 只负责摆位置。

use super::vector::{Geom, Ink, Shape};

/// 单位方框换算:原版 viewBox 是 `0 0 16 16`。
const fn u(v: f32) -> f32 {
    v / 16.0
}
/// 原版全部 KPI 图标 `stroke-width="1.2"`。
const KPI_STROKE: f32 = 1.2 / 16.0;

/// `<rect x="2" y="4" width="12" height="9" rx="1.5"/>` + `<path d="M2 6.5h12M10.5 9.5h1.5"/>`
pub const ICON_WALLET: &[Shape] = &[
    Shape::line(
        Ink::Current,
        KPI_STROKE,
        Geom::Rect {
            x: u(2.0),
            y: u(4.0),
            w: u(12.0),
            h: u(9.0),
            round: u(1.5),
        },
    ),
    Shape::line(
        Ink::Current,
        KPI_STROKE,
        Geom::Polyline(&[(u(2.0), u(6.5)), (u(14.0), u(6.5))]),
    ),
    Shape::line(
        Ink::Current,
        KPI_STROKE,
        Geom::Polyline(&[(u(10.5), u(9.5)), (u(12.0), u(9.5))]),
    ),
];

/// `<ellipse cx="8" cy="3.8" rx="5" ry="1.8"/>` + 两道桶身曲线。
/// 曲线是三次贝塞尔,这里用折线近似(16px 上看不出差别)。
pub const ICON_STACK: &[Shape] = &[
    Shape::line(
        Ink::Current,
        KPI_STROKE,
        Geom::Ellipse {
            c: (u(8.0), u(3.8)),
            r: (u(5.0), u(1.8)),
            tilt: 0.0,
        },
    ),
    Shape::line(
        Ink::Current,
        KPI_STROKE,
        Geom::Polyline(&[(u(3.0), u(3.8)), (u(3.0), u(12.2))]),
    ),
    Shape::line(
        Ink::Current,
        KPI_STROKE,
        Geom::Polyline(&[(u(13.0), u(3.8)), (u(13.0), u(12.2))]),
    ),
    Shape::line(
        Ink::Current,
        KPI_STROKE,
        Geom::Polyline(&[
            (u(3.0), u(12.2)),
            (u(4.6), u(13.6)),
            (u(8.0), u(14.0)),
            (u(11.4), u(13.6)),
            (u(13.0), u(12.2)),
        ]),
    ),
    Shape::line(
        Ink::Current,
        KPI_STROKE,
        Geom::Polyline(&[
            (u(3.0), u(8.0)),
            (u(4.6), u(9.4)),
            (u(8.0), u(9.8)),
            (u(11.4), u(9.4)),
            (u(13.0), u(8.0)),
        ]),
    ),
];

/// `<path d="M2 8h3l1.5-4 3 8L11 8h3"/>`
pub const ICON_PULSE: &[Shape] = &[Shape::line(
    Ink::Current,
    KPI_STROKE,
    Geom::Polyline(&[
        (u(2.0), u(8.0)),
        (u(5.0), u(8.0)),
        (u(6.5), u(4.0)),
        (u(9.5), u(12.0)),
        (u(11.0), u(8.0)),
        (u(14.0), u(8.0)),
    ]),
)];

/// `<path d="M8 2.5a5.5 5.5 0 1 0-4.9 8L2.5 13.5l3.1-.6A5.5 5.5 0 1 0 8 2.5z"/>`
/// —— 圆形气泡 + 左下小尾巴。
pub const ICON_CHAT: &[Shape] = &[
    Shape::line(
        Ink::Current,
        KPI_STROKE,
        Geom::Arc {
            c: (u(8.0), u(8.0)),
            r: u(5.5),
            from: 155.0,
            sweep: 300.0,
        },
    ),
    Shape::line(
        Ink::Current,
        KPI_STROKE,
        Geom::Polyline(&[(u(3.1), u(10.5)), (u(2.5), u(13.5)), (u(5.6), u(12.9))]),
    ),
];

/// `<path d="M8.5 2 3.5 9h3.5l-.5 5 5-7H8l.5-5z"/>`
pub const ICON_BOLT: &[Shape] = &[Shape::line(
    Ink::Current,
    KPI_STROKE,
    Geom::Polygon(&[
        (u(8.5), u(2.0)),
        (u(3.5), u(9.0)),
        (u(7.0), u(9.0)),
        (u(6.5), u(14.0)),
        (u(11.5), u(7.0)),
        (u(8.0), u(7.0)),
    ]),
)];

/// 刷新:`<path d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9M13.5 2.5v2.7h-2.7"/>`
pub const ICON_REFRESH: &[Shape] = &[
    Shape::line(
        Ink::Current,
        1.3 / 16.0,
        Geom::Arc {
            c: (u(8.0), u(8.0)),
            r: u(5.5),
            from: 0.0,
            sweep: 315.0,
        },
    ),
    Shape::line(
        Ink::Current,
        1.3 / 16.0,
        Geom::Polyline(&[(u(13.5), u(2.5)), (u(13.5), u(5.2)), (u(10.8), u(5.2))]),
    ),
];

/// 全部六枚(遍历/演示用)。
pub const ALL_USAGE_GLYPHS: &[&[Shape]] = &[
    ICON_WALLET,
    ICON_STACK,
    ICON_PULSE,
    ICON_CHAT,
    ICON_BOLT,
    ICON_REFRESH,
];

/// 所有形状表(单测遍历用)。
#[cfg(test)]
pub(super) fn shape_tables() -> Vec<&'static [Shape]> {
    ALL_USAGE_GLYPHS.to_vec()
}
