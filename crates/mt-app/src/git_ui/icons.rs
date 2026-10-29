//! Git 面板全部线性图标的**唯一**形状表。
//!
//! 三条开发线(分支与同步 / 提交与 diff / 提交历史)原先各起了一份 `ICON_*`:
//! 同一个「复制」「关闭」「折角」在三处画法、线宽各不相同,摆在一个面板里粗细
//! 参差。这里收成一份 —— 几何一律原样搬 [Lucide](https://lucide.dev)(ISC 许可)
//! 的 `path` data(viewBox `0 0 24 24`,[`Geom::Path`] 收下后由 `svg_path` 解析
//! 离散),同一含义只留一枚图标、只留一份几何。
//!
//! # 线宽
//!
//! 统一 [`STROKE`] = 1.3/16(单位方框比例):14px 下约 1.1px,比 Activity Bar 的
//! 1.2/16 略粗 —— 面板里的图标多在 12~14px,同比例会发虚;与 Lucide 原生的
//! `stroke-width="2"`(2/24)几乎一致,视觉重量与原图标相同。
//!
//! # 点
//!
//! Lucide 里 `h.01` 那种零长线段靠圆头线帽成点,这里的描边没有圆头 —— 一律换成
//! 实心小圆([`dot`]),14px 下才看得见。
//!
//! # 尺寸
//!
//! 只有一组:[`ICON_SIZE`](按钮 / 行首)、[`ICON_SIZE_SM`](胶囊外的小提示、折叠箭头)、
//! [`ICON_SIZE_XS`](胶囊里)、[`ICON_SIZE_EMPTY`](空态)、[`ICON_BUTTON`](图标按钮点击区)。

use gpui::{Hsla, px};
use mt_ui::icons::{Geom, Ink, Shape, VectorIcon};

/// Lucide 的 viewBox。
const LUCIDE: (f32, f32, f32) = (0.0, 0.0, 24.0);

/// 24 格 → 单位方框。
const fn u(v: f32) -> f32 {
    v / 24.0
}

/// 统一线宽(见模块注释)。
const STROKE: f32 = 1.3 / 16.0;

/// 一笔 Lucide `path`。
const fn path(d: &'static str) -> Shape {
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

/// 描边圆(Lucide 的 `<circle>`)。
const fn ring(cx: f32, cy: f32, r: f32) -> Shape {
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Circle {
            c: (u(cx), u(cy)),
            r: u(r),
        },
    )
}

/// 实心小圆点(Lucide 里 `h.01` / `r=".5" fill` 那种「点」的替身)。
const fn dot(cx: f32, cy: f32) -> Shape {
    Shape::fill(
        Ink::Current,
        Geom::Circle {
            c: (u(cx), u(cy)),
            r: u(1.2),
        },
    )
}

// ─── 尺寸 ─────────────────────────────────────────────────────

/// 图标的标准尺寸(图标按钮、行首图标)。
pub const ICON_SIZE: f32 = 14.0;
/// 小一号:折叠箭头、提示行前的警告号、仓库栏里的附属标记。
pub const ICON_SIZE_SM: f32 = 12.0;
/// 胶囊里的图标(↑↓ 计数、标签)。
pub const ICON_SIZE_XS: f32 = 10.0;
/// 空态的大图标。
pub const ICON_SIZE_EMPTY: f32 = 32.0;
/// 图标按钮的点击区边长。
pub const ICON_BUTTON: f32 = 24.0;

/// 一枚图标。
pub fn icon(shapes: &'static [Shape], size: f32, color: Hsla) -> VectorIcon {
    VectorIcon::new(shapes, px(size)).ink(color)
}

// ─── 同步 ─────────────────────────────────────────────────────

/// Fetch(`download-cloud`)。
pub const ICON_FETCH: &[Shape] = &[
    path("M4 14.899A7 7 0 1 1 15.71 8h1.79a4.5 4.5 0 0 1 2.5 8.242"),
    path("M12 12v9"),
    path("m8 17 4 4 4-4"),
];
/// 拉取(`arrow-down-to-line`)。
pub const ICON_PULL: &[Shape] = &[path("M12 17V3"), path("m6 11 6 6 6-6"), path("M19 21H5")];
/// 推送(`arrow-up-from-line`)。
pub const ICON_PUSH: &[Shape] = &[path("m18 9-6-6-6 6"), path("M12 3v14"), path("M5 21h14")];
/// 刷新(`refresh-cw`)。
pub const ICON_REFRESH: &[Shape] = &[
    path("M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"),
    path("M21 3v5h-5"),
    path("M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"),
    path("M8 16H3v5"),
];

// ─── 引用 / 仓库 ──────────────────────────────────────────────

/// 本地分支(`git-branch`)。
pub const ICON_BRANCH: &[Shape] = &[
    path("M6 3v12"),
    ring(18.0, 6.0, 3.0),
    ring(6.0, 18.0, 3.0),
    path("M18 9a9 9 0 0 1-9 9"),
];
/// 远程分支(`cloud`)。
pub const ICON_REMOTE: &[Shape] = &[path("M17.5 19H9a7 7 0 1 1 6.71-9h1.79a4.5 4.5 0 1 1 0 9Z")];
/// 仓库(`folder`)。
pub const ICON_REPO: &[Shape] = &[path(
    "M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z",
)];
/// linked worktree(`git-fork`)。
pub const ICON_WORKTREE: &[Shape] = &[
    ring(12.0, 18.0, 3.0),
    ring(6.0, 6.0, 3.0),
    ring(18.0, 6.0, 3.0),
    path("M18 9v2c0 .6-.4 1-1 1H7c-.6 0-1-.4-1-1V9"),
    path("M12 12v3"),
];
/// 合并进行中(`git-merge`)。
pub const ICON_MERGE: &[Shape] = &[
    ring(18.0, 18.0, 3.0),
    ring(6.0, 6.0, 3.0),
    path("M6 21V9a9 9 0 0 0 9 9"),
];
/// 提交节点(`git-commit-horizontal`):历史为空 / 没有仓库的空态。
pub const ICON_COMMIT: &[Shape] = &[ring(12.0, 12.0, 3.0), path("M3 12h6"), path("M15 12h6")];
/// 标签(`tag`):标签胶囊的前缀。
pub const ICON_TAG: &[Shape] = &[
    path(
        "M12.586 2.586A2 2 0 0 0 11.172 2H4a2 2 0 0 0-2 2v7.172a2 2 0 0 0 .586 1.414l8.704 8.704a2.426 2.426 0 0 0 3.42 0l6.58-6.58a2.426 2.426 0 0 0 0-3.42z",
    ),
    dot(7.5, 7.5),
];
/// 历史(`history`):提交消息历史、单文件历史。
pub const ICON_HISTORY: &[Shape] = &[
    path("M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"),
    path("M3 3v5h5"),
    path("M12 7v5l4 2"),
];
/// 人像(`user`):「仅我的」。
pub const ICON_USER: &[Shape] = &[
    path("M19 21v-2a4 4 0 0 0-4-4H9a4 4 0 0 0-4 4v2"),
    ring(12.0, 7.0, 4.0),
];

// ─── 视图 ─────────────────────────────────────────────────────

/// 列表视图(`list`;行首小点换成实心圆点)。
pub const ICON_LIST: &[Shape] = &[
    dot(3.5, 6.0),
    dot(3.5, 12.0),
    dot(3.5, 18.0),
    path("M8 6h13"),
    path("M8 12h13"),
    path("M8 18h13"),
];
/// 树形视图(`list-tree`)。
pub const ICON_LIST_TREE: &[Shape] = &[
    path("M21 12h-8"),
    path("M21 6H8"),
    path("M21 18h-8"),
    path("M3 6v4c0 1.1.9 2 2 2h3"),
    path("M3 10v6c0 1.1.9 2 2 2h3"),
];
/// 搜索(`search`)。
pub const ICON_SEARCH: &[Shape] = &[ring(11.0, 11.0, 8.0), path("m21 21-4.3-4.3")];

// ─── 状态 ─────────────────────────────────────────────────────

/// 勾(`check`):当前分支、复选框。
pub const ICON_CHECK: &[Shape] = &[path("M20 6 9 17l-5-5")];
/// 「都处理完了」(`circle-check-big`):工作区干净 / 没有待推送 / 没有更多改动的空态。
pub const ICON_CLEAN: &[Shape] = &[
    path("M21.801 10A10 10 0 1 1 17 3.335"),
    path("m9 11 3 3L22 4"),
];
/// 冲突 / 警告(`triangle-alert`)。
pub const ICON_ALERT: &[Shape] = &[
    path("m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3"),
    path("M12 9v4"),
    dot(12.0, 17.0),
];

// ─── 动作 ─────────────────────────────────────────────────────

/// 新建 / 暂存(`plus`)。
pub const ICON_PLUS: &[Shape] = &[path("M5 12h14"), path("M12 5v14")];
/// 取消暂存(`minus`)。
pub const ICON_MINUS: &[Shape] = &[path("M5 12h14")];
/// 丢弃 / 回滚(`undo-2`)。
pub const ICON_UNDO: &[Shape] = &[
    path("M9 14 4 9l5-5"),
    path("M4 9h10.5a5.5 5.5 0 0 1 5.5 5.5a5.5 5.5 0 0 1-5.5 5.5H11"),
];
/// 关闭 / 清空(`x`)。
pub const ICON_CLOSE: &[Shape] = &[path("M18 6 6 18"), path("m6 6 12 12")];
/// 复制(`copy`)。
pub const ICON_COPY: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Rect {
            x: u(8.0),
            y: u(8.0),
            w: u(14.0),
            h: u(14.0),
            round: u(2.0),
        },
    ),
    path("M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"),
];

// ─── 方向 ─────────────────────────────────────────────────────

/// 展开态 / 下拉(`chevron-down`)。
pub const ICON_CHEVRON_DOWN: &[Shape] = &[path("m6 9 6 6 6-6")];
/// 折叠态 / 下一个(`chevron-right`)。
pub const ICON_CHEVRON_RIGHT: &[Shape] = &[path("m9 18 6-6-6-6")];
/// 上一个(`chevron-left`)。
pub const ICON_CHEVRON_LEFT: &[Shape] = &[path("m15 18-6-6 6-6")];
/// 指向(`arrow-right`):推送对话框「本地 → 远程」。
pub const ICON_ARROW_RIGHT: &[Shape] = &[path("M5 12h14"), path("m12 5 7 7-7 7")];
/// 待推送 / 上一处改动(`arrow-up`)。
pub const ICON_ARROW_UP: &[Shape] = &[path("m5 12 7-7 7 7"), path("M12 19V5")];
/// 待拉取 / 下一处改动(`arrow-down`)。
pub const ICON_ARROW_DOWN: &[Shape] = &[path("M12 5v14"), path("m19 12-7 7-7-7")];

/// 本模块全部图标表(单测遍历用)。
#[cfg(test)]
const ALL_ICONS: &[&[Shape]] = &[
    ICON_FETCH,
    ICON_PULL,
    ICON_PUSH,
    ICON_REFRESH,
    ICON_BRANCH,
    ICON_REMOTE,
    ICON_REPO,
    ICON_WORKTREE,
    ICON_MERGE,
    ICON_COMMIT,
    ICON_TAG,
    ICON_HISTORY,
    ICON_USER,
    ICON_LIST,
    ICON_LIST_TREE,
    ICON_SEARCH,
    ICON_CHECK,
    ICON_CLEAN,
    ICON_ALERT,
    ICON_PLUS,
    ICON_MINUS,
    ICON_UNDO,
    ICON_CLOSE,
    ICON_COPY,
    ICON_CHEVRON_DOWN,
    ICON_CHEVRON_RIGHT,
    ICON_CHEVRON_LEFT,
    ICON_ARROW_RIGHT,
    ICON_ARROW_UP,
    ICON_ARROW_DOWN,
];

#[cfg(test)]
mod tests {
    use mt_ui::icons::Pen;

    use super::*;

    /// 每枚图标:每一笔都解析得出点、所有点落在单位方框内(原样搬的 path 允许 1%
    /// 容差)。路径解析失败会表现为「没有点」,写错一位数字会表现为越界。
    #[test]
    fn 图标几何都在单位方框内() {
        for (i, shapes) in ALL_ICONS.iter().enumerate() {
            assert!(!shapes.is_empty(), "第 {i} 枚图标是空的");
            for shape in shapes.iter() {
                let subpaths = shape.geom.subpaths();
                assert!(!subpaths.is_empty(), "第 {i} 枚图标有一笔没解析出路径");
                for (pts, _) in subpaths.iter() {
                    assert!(
                        pts.len() >= 2,
                        "第 {i} 枚图标有一笔没解析出点: {:?}",
                        shape.geom
                    );
                    for (x, y) in pts {
                        assert!(
                            (-0.01..=1.01).contains(x) && (-0.01..=1.01).contains(y),
                            "第 {i} 枚图标越界点 ({x}, {y})"
                        );
                    }
                }
            }
        }
    }

    /// 线宽只有一个口径:所有描边都是 [`STROKE`](三条线合并前各有各的线宽)。
    #[test]
    fn 描边线宽统一() {
        for (i, shapes) in ALL_ICONS.iter().enumerate() {
            for shape in shapes.iter() {
                if let Pen::Line(width) = shape.pen {
                    assert!(
                        (width - STROKE).abs() < f32::EPSILON,
                        "第 {i} 枚图标线宽 {width} 不是统一口径"
                    );
                }
            }
        }
    }
}
