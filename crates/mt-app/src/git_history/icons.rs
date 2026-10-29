//! 提交历史 / 单文件历史共用的小件:线性图标表、图标按钮、引用胶囊、首字母头像、
//! 等宽数字字体、文件状态字母。
//!
//! 图标全部走 [`mt_ui::icons::VectorIcon`] 的形状 DSL(单位方框坐标,写法同
//! `activity_bar.rs`):原版 16×16 网格按 [`u`] 换算,统一线宽 [`STROKE`]。
//! 常量一律 `ICON_` 前缀 —— 三条 Git 开发线各自起了一份,合并时收进共享模块。

use gpui::{
    AnyElement, Div, ElementId, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement, SharedString, Stateful, Styled, div, prelude::FluentBuilder as _, px,
};
use mt_project::git::GitStatus;
use mt_ui::icons::{Geom, Ink, Shape, VectorIcon};

use crate::ui;

/// 16 网格 → 单位方框。
const fn u(v: f32) -> f32 {
    v / 16.0
}

/// 统一线宽。14px 下约 1.1px,比活动栏的 1.2/16 略粗一点 —— 面板里的图标更小,
/// 同比例会显得发虚。
const STROKE: f32 = 1.3 / 16.0;

/// 面板内图标的标准边长。
pub(crate) const ICON_SIZE: f32 = 14.0;

/// 放大镜(过滤框前缀 / 过滤无结果的空态)。
pub(crate) const ICON_SEARCH: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Circle {
            c: (u(7.0), u(7.0)),
            r: u(4.5),
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(10.4), u(10.4)), (u(13.8), u(13.8))]),
    ),
];

/// 叉(清空关键词)。
pub(crate) const ICON_CLOSE: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(4.0), u(4.0)), (u(12.0), u(12.0))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(12.0), u(4.0)), (u(4.0), u(12.0))]),
    ),
];

/// 标签(胶囊前缀 / 标签菜单)。左上角是挂绳孔。
pub(crate) const ICON_TAG: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polygon(&[
            (u(2.5), u(2.5)),
            (u(8.2), u(2.5)),
            (u(13.8), u(8.1)),
            (u(8.1), u(13.8)),
            (u(2.5), u(8.2)),
        ]),
    ),
    Shape::fill(
        Ink::Current,
        Geom::Circle {
            c: (u(5.6), u(5.6)),
            r: u(1.15),
        },
    ),
];

/// 复制(两张叠着的纸)。
pub(crate) const ICON_COPY: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Rect {
            x: u(5.5),
            y: u(5.5),
            w: u(8.0),
            h: u(8.0),
            round: u(1.4),
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[
            (u(3.5), u(10.5)),
            (u(2.5), u(10.5)),
            (u(2.5), u(2.5)),
            (u(10.5), u(2.5)),
            (u(10.5), u(3.5)),
        ]),
    ),
];

/// 向下的折角(详情区「收起」)。
pub(crate) const ICON_CHEVRON_DOWN: &[Shape] = &[Shape::line(
    Ink::Current,
    STROKE,
    Geom::Polyline(&[(u(4.0), u(6.0)), (u(8.0), u(10.0)), (u(12.0), u(6.0))]),
)];

/// 向上的折角(详情区「展开」)。
pub(crate) const ICON_CHEVRON_UP: &[Shape] = &[Shape::line(
    Ink::Current,
    STROKE,
    Geom::Polyline(&[(u(4.0), u(10.0)), (u(8.0), u(6.0)), (u(12.0), u(10.0))]),
)];

/// 人像(「仅我的」)。
pub(crate) const ICON_USER: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Circle {
            c: (u(8.0), u(5.5)),
            r: u(2.7),
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Arc {
            c: (u(8.0), u(15.0)),
            r: u(5.6),
            from: 200.0,
            sweep: 140.0,
        },
    ),
];

/// 时钟(单文件历史 / 历史为空的空态)。
pub(crate) const ICON_HISTORY: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Circle {
            c: (u(8.0), u(8.0)),
            r: u(5.8),
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(8.0), u(4.8)), (u(8.0), u(8.0)), (u(10.4), u(9.6))]),
    ),
];

/// 提交节点(git-commit:一条线穿过一个圈)。
pub(crate) const ICON_COMMIT: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(1.5), u(8.0)), (u(5.0), u(8.0))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Circle {
            c: (u(8.0), u(8.0)),
            r: u(3.0),
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(11.0), u(8.0)), (u(14.5), u(8.0))]),
    ),
];

/// 一枚图标。
pub(crate) fn icon(shapes: &'static [Shape], size: f32, color: Hsla) -> VectorIcon {
    VectorIcon::new(shapes, px(size)).ink(color)
}

/// 图标按钮:24px 方形点击区、圆角 4、hover 底 `bg_elevated`、hover 时图标转
/// `text_primary`。禁用态图标 muted 并降透明度,不挂 hover(调用方也别挂 on_click)。
///
/// hover 换色靠两枚叠放的图标 + `group_hover` 切透明度:[`VectorIcon`] 是自绘元素,
/// 吃不到 hover 样式里的文字色。
pub(crate) fn icon_button(
    id: impl Into<ElementId>,
    shapes: &'static [Shape],
    enabled: bool,
) -> Stateful<Div> {
    const GROUP: &str = "git-log-icon-button";
    let layer = || {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
    };
    let base = div()
        .id(id)
        .relative()
        .flex_none()
        .w(px(24.0))
        .h(px(24.0))
        .rounded(px(4.0));
    if !enabled {
        return base
            .opacity(0.45)
            .child(layer().child(icon(shapes, ICON_SIZE, ui::text_muted())));
    }
    base.group(GROUP)
        .cursor_pointer()
        .hover(|el| el.bg(ui::bg_elevated()))
        .child(layer().group_hover(GROUP, |s| s.opacity(0.0)).child(icon(
            shapes,
            ICON_SIZE,
            ui::text_muted(),
        )))
        .child(
            layer()
                .opacity(0.0)
                .group_hover(GROUP, |s| s.opacity(1.0))
                .child(icon(shapes, ICON_SIZE, ui::text_primary())),
        )
}

/// 引用胶囊的语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefKind {
    /// HEAD / 当前分支。
    Head,
    /// 其它本地分支。
    Local,
    /// 远程跟踪分支。
    Remote,
    /// 标签。
    Tag,
}

impl RefKind {
    pub(crate) fn color(self) -> Hsla {
        match self {
            RefKind::Head => ui::accent(),
            RefKind::Local => ui::color_success(),
            RefKind::Remote => ui::color_info(),
            RefKind::Tag => ui::color_warning(),
        }
    }
}

/// 一个引用胶囊:高 18、全圆角、语义色 15% 底 + 语义色字,字号比正文小一档。
/// 名字太长时胶囊自己截断(保底 40px 认得出前缀),全名请调用方挂 tooltip。
pub(crate) fn ref_pill(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    kind: RefKind,
) -> Stateful<Div> {
    let color = kind.color();
    div()
        .id(id)
        .h(px(18.0))
        .min_w(px(40.0))
        .max_w(px(160.0))
        .flex()
        .items_center()
        .gap(px(4.0))
        .px(px(7.0))
        .rounded_full()
        .bg(ui::with_alpha(color, 0.15))
        .text_color(color)
        .text_size(ui::font_px(11.0))
        .when(kind == RefKind::Tag, |el| {
            el.child(icon(ICON_TAG, 10.0, color))
        })
        .child(div().min_w_0().truncate().child(label.into()))
}

/// 「+N」胶囊(其余引用收进 tooltip)。中性色,不与任何语义撞色。
pub(crate) fn more_pill(id: impl Into<ElementId>, hidden: usize) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .h(px(18.0))
        .flex()
        .items_center()
        .px(px(6.0))
        .rounded_full()
        .bg(ui::with_alpha(ui::text_muted(), 0.15))
        .text_color(ui::text_secondary())
        .text_size(ui::font_px(11.0))
        .child(format!("+{hidden}"))
}

/// 头像的语义色池(按作者稳定取一个)。
fn avatar_color(key: &str) -> Hsla {
    // FNV-1a:稳定、无依赖,同名作者永远同色
    let mut h: u32 = 0x811c_9dc5;
    for b in key.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    let pool = [
        ui::accent(),
        ui::color_success(),
        ui::color_info(),
        ui::color_warning(),
        ui::color_error(),
        ui::color_ai(),
        ui::color_file(),
        ui::color_folder(),
    ];
    pool[(h as usize) % pool.len()]
}

/// 作者名 → 头像上的字(首个字母 / 汉字,字母大写)。
pub(crate) fn avatar_initial(name: &str) -> String {
    name.trim()
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".to_string())
}

/// 16px 圆形首字母头像:底色 = 按作者取的语义色 18% 叠加,字 = 同色。
pub(crate) fn avatar(name: &str) -> Div {
    let color = avatar_color(name.trim());
    div()
        .flex_none()
        .w(px(16.0))
        .h(px(16.0))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(ui::with_alpha(color, 0.18))
        .text_color(color)
        .text_size(ui::font_px(9.0))
        .font_weight(FontWeight::SEMIBOLD)
        .child(avatar_initial(name))
}

/// 等宽字体链(哈希 / 时间 / 计数):列对得齐。CJK 回落雅黑(「3 小时前」)。
pub(crate) fn mono<E: Styled>(mut el: E) -> E {
    let ts = el.text_style();
    ts.font_family = Some("Cascadia Mono".into());
    ts.font_fallbacks = Some(gpui::FontFallbacks::from_fonts(vec![
        "Consolas".into(),
        "JetBrains Mono".into(),
        "Menlo".into(),
        "Microsoft YaHei".into(),
    ]));
    el
}

/// `CommitFileInfo.status`(`added` / `modified` / …)→ 状态字母与颜色。
/// 颜色的唯一来源是 [`crate::git_changes::status_color`]。
pub(crate) fn file_status(status: &str) -> (&'static str, Hsla) {
    let (letter, status) = match status {
        "added" => ("A", Some(GitStatus::Added)),
        "deleted" => ("D", Some(GitStatus::Deleted)),
        "renamed" => ("R", Some(GitStatus::Renamed)),
        "modified" => ("M", Some(GitStatus::Modified)),
        _ => ("?", None),
    };
    (letter, crate::git_changes::status_color(status.as_ref()))
}

/// 仓库内路径 → (文件名, 所在目录)。目录为空串表示在仓库根。
pub(crate) fn split_path(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some((dir, name)) => (name.to_string(), dir.to_string()),
        None => (path.to_string(), String::new()),
    }
}

/// 一行改动文件:FileIcon + 文件名(已删除加删除线)+ 弱化目录 + 右端状态字母。
/// 行高 28、左右内边距 8;外层的点击 / 选中由调用方挂。
pub(crate) fn file_row(id: impl Into<ElementId>, path: &str, status: &str) -> Stateful<Div> {
    let (name, dir) = split_path(path);
    let (letter, color) = file_status(status);
    let deleted = status == "deleted";
    div()
        .id(id)
        .w_full()
        .h(px(28.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .px(px(8.0))
        .rounded(px(4.0))
        .child(mt_ui::icons::FileIcon::new(&name, false, false))
        .child(
            div()
                .flex_none()
                .max_w(px(220.0))
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(13.0))
                .text_color(if deleted {
                    ui::text_muted()
                } else {
                    ui::text_primary()
                })
                .when(deleted, |el| el.line_through())
                .child(name),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(dir),
        )
        .child(
            mono(div())
                .flex_none()
                .w(px(14.0))
                .text_right()
                .text_size(ui::font_px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .child(letter),
        )
}

/// 空态:居中的 32px muted 图标 + 一句说明 + 可选的主操作。
pub(crate) fn empty_state(
    shapes: &'static [Shape],
    text: impl Into<SharedString>,
    action: Option<AnyElement>,
) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(12.0))
        .py(px(24.0))
        .child(icon(shapes, 32.0, ui::text_muted()))
        .child(
            div()
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_muted())
                .text_center()
                .child(text.into()),
        )
        .children(action)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 图标几何全在单位方框里(画出界会压到邻居)。
    #[test]
    fn 图标都在单位方框内() {
        for shapes in [
            ICON_SEARCH,
            ICON_CLOSE,
            ICON_TAG,
            ICON_COPY,
            ICON_CHEVRON_DOWN,
            ICON_CHEVRON_UP,
            ICON_USER,
            ICON_HISTORY,
            ICON_COMMIT,
        ] {
            for shape in shapes {
                let (pts, _) = shape.geom.points();
                for (x, y) in pts {
                    assert!(
                        (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y),
                        "({x},{y})"
                    );
                }
            }
        }
    }

    #[test]
    fn 头像取首字() {
        assert_eq!(avatar_initial("alice"), "A");
        assert_eq!(avatar_initial("  张三"), "张");
        assert_eq!(avatar_initial("(bot)"), "B");
        assert_eq!(avatar_initial(""), "?");
    }

    #[test]
    fn 文件状态字母与路径拆分() {
        ui::set_palette(ui::Palette::dark());
        assert_eq!(file_status("added").0, "A");
        assert_eq!(file_status("deleted").0, "D");
        assert_eq!(file_status("renamed").0, "R");
        assert_eq!(file_status("modified").0, "M");
        assert_eq!(file_status("weird").0, "?");
        assert_eq!(file_status("weird").1, ui::text_muted());
        assert_eq!(
            split_path("src/git/mod.rs"),
            ("mod.rs".to_string(), "src/git".to_string())
        );
        assert_eq!(
            split_path("README.md"),
            ("README.md".to_string(), String::new())
        );
    }
}
