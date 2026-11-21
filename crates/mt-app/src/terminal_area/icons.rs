use super::*;

// ─── 控件簇的描边图标(照抄 `PaneGroup.tsx:40-62` 的 SVG,viewBox 16 归一化;
//     自绘理由见 `mt_ui::icons::vector` 模块注释,换算套路同 `activity_bar`)───

/// 单位方框换算:原版 viewBox 是 `0 0 16 16`,除以 16 即可。
pub(super) const fn cu(v: f32) -> f32 {
    v / 16.0
}
/// 原版这组图标统一 `stroke-width="1.3"`。
pub(super) const CTRL_STROKE: f32 = 1.3 / 16.0;

/// 终端内查找。原版 `ICON_SEARCH`:`<circle cx="7" cy="7" r="4.2"/>` +
/// `<path d="M10.2 10.2L14 14"/>`。
pub(super) const ICON_SEARCH: &[Shape] = &[
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Circle {
            c: (cu(7.0), cu(7.0)),
            r: cu(4.2),
        },
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(10.2), cu(10.2)), (cu(14.0), cu(14.0))]),
    ),
];

/// 向右分屏。原版 `ICON_SPLIT_RIGHT`:
/// `<rect x="2" y="3" width="12" height="10" rx="1.5"/>` + `<path d="M8 3v10"/>`。
pub(super) const ICON_SPLIT_RIGHT: &[Shape] = &[
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Rect {
            x: cu(2.0),
            y: cu(3.0),
            w: cu(12.0),
            h: cu(10.0),
            round: cu(1.5),
        },
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(8.0), cu(3.0)), (cu(8.0), cu(13.0))]),
    ),
];

/// 向下分屏。原版 `ICON_SPLIT_DOWN`:同一只外框 + `<path d="M2 8h12"/>`。
pub(super) const ICON_SPLIT_DOWN: &[Shape] = &[
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Rect {
            x: cu(2.0),
            y: cu(3.0),
            w: cu(12.0),
            h: cu(10.0),
            round: cu(1.5),
        },
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(2.0), cu(8.0)), (cu(14.0), cu(8.0))]),
    ),
];

/// 最大化。原版 `ICON_MAXIMIZE`:`M9.5 2.5h4v4` / `M13.5 2.5L9 7` /
/// `M6.5 13.5h-4v-4` / `M2.5 13.5L7 9`(右上、左下两只往外指的角标)。
pub(super) const ICON_MAXIMIZE: &[Shape] = &[
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(9.5), cu(2.5)), (cu(13.5), cu(2.5)), (cu(13.5), cu(6.5))]),
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(13.5), cu(2.5)), (cu(9.0), cu(7.0))]),
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(6.5), cu(13.5)), (cu(2.5), cu(13.5)), (cu(2.5), cu(9.5))]),
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(2.5), cu(13.5)), (cu(7.0), cu(9.0))]),
    ),
];

/// 还原。原版 `ICON_RESTORE`:同一组角标翻过来往内指。
pub(super) const ICON_RESTORE: &[Shape] = &[
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(13.5), cu(6.5)), (cu(9.5), cu(6.5)), (cu(9.5), cu(2.5))]),
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(9.5), cu(6.5)), (cu(14.0), cu(2.0))]),
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(2.5), cu(9.5)), (cu(6.5), cu(9.5)), (cu(6.5), cu(13.5))]),
    ),
    Shape::line(
        Ink::Current,
        CTRL_STROKE,
        Geom::Polyline(&[(cu(6.5), cu(9.5)), (cu(2.0), cu(14.0))]),
    ),
];

/// 图标在 22×22 方钮里的边长(原版 SVG 是 13×13)。
pub(super) const CTRL_ICON: f32 = 13.0;
