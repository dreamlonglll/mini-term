//! Git「分支与同步」这条线的共用小件:线性图标表(`ICON_*`)、图标按钮、胶囊、
//! 失败详情弹窗与结果 toast。
//!
//! # 图标
//!
//! 面板里原先的 unicode 字形按钮(↻ ↓ ↑ ▾ ⎇ ✓ ✕)换成 [`VectorIcon`] 自绘线性图标。
//! 几何原样搬 [Lucide](https://lucide.dev)(ISC 许可)的 `path` data —— viewBox
//! `0 0 24 24`,[`Geom::Path`] 收下后由 `svg_path` 解析离散,描边照常走 `Pen::Line`。
//! 线宽与 Activity Bar 同一口径(`1.2/16`),14px 下约 1px。
//!
//! # 图标按钮的悬停变色
//!
//! `VectorIcon` 的颜色是渲染时定死的,不吃 `hover` 样式里的文字色。这里叠两枚图标
//! (常态色 / 悬停色),用 `group_hover` 切可见性 —— 纯样式,不必给每颗按钮记悬停态。
//! 被隐藏的那枚 `Visibility::Hidden` 整个跳过绘制,不多画一遍。

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Div, Entity, Hsla, InteractiveElement,
    IntoElement, ParentElement, SharedString, Stateful, StatefulInteractiveElement, Styled, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Textarea, TextareaState};
use mt_ui::icons::{Geom, Ink, Shape, VectorIcon};

use crate::i18n::t;
use crate::notify::ToastKind;
use crate::prompt::{close_guarded, dialog_title, kind, open_guarded};
use crate::store::AppStore;
use crate::ui;

// ─── 图标表 ───────────────────────────────────────────────────

/// Lucide 的 viewBox。
const LUCIDE: (f32, f32, f32) = (0.0, 0.0, 24.0);
/// 单位方框换算(24 格)。
const fn u(v: f32) -> f32 {
    v / 24.0
}
/// 与 Activity Bar 同一线宽口径。
const STROKE: f32 = 1.2 / 16.0;

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

/// 实心小圆点(Lucide 里 `h.01` 那种「点」的替身)。
const fn dot(cx: f32, cy: f32) -> Shape {
    Shape::fill(
        Ink::Current,
        Geom::Circle {
            c: (u(cx), u(cy)),
            r: u(1.2),
        },
    )
}

/// Fetch(Lucide `download-cloud`)。
pub const ICON_FETCH: &[Shape] = &[
    path("M4 14.899A7 7 0 1 1 15.71 8h1.79a4.5 4.5 0 0 1 2.5 8.242"),
    path("M12 12v9"),
    path("m8 17 4 4 4-4"),
];
/// 拉取(Lucide `arrow-down-to-line`)。
pub const ICON_PULL: &[Shape] = &[path("M12 17V3"), path("m6 11 6 6 6-6"), path("M19 21H5")];
/// 推送(Lucide `arrow-up-from-line`)。
pub const ICON_PUSH: &[Shape] = &[path("m18 9-6-6-6 6"), path("M12 3v14"), path("M5 21h14")];
/// 刷新(Lucide `refresh-cw`)。
pub const ICON_REFRESH: &[Shape] = &[
    path("M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"),
    path("M21 3v5h-5"),
    path("M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"),
    path("M8 16H3v5"),
];
/// 本地分支(Lucide `git-branch`)。
pub const ICON_BRANCH: &[Shape] = &[
    path("M6 3v12"),
    ring(18.0, 6.0, 3.0),
    ring(6.0, 18.0, 3.0),
    path("M18 9a9 9 0 0 1-9 9"),
];
/// 远程分支(Lucide `cloud`)。
pub const ICON_REMOTE: &[Shape] = &[path("M17.5 19H9a7 7 0 1 1 6.71-9h1.79a4.5 4.5 0 1 1 0 9Z")];
/// 仓库(Lucide `folder`)。
pub const ICON_REPO: &[Shape] = &[path(
    "M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z",
)];
/// 下拉箭头 / 展开态(Lucide `chevron-down`)。
pub const ICON_CHEVRON_DOWN: &[Shape] = &[path("m6 9 6 6 6-6")];
/// 折叠态(Lucide `chevron-right`)。
pub const ICON_CHEVRON_RIGHT: &[Shape] = &[path("m9 18 6-6-6-6")];
/// linked worktree 标记(Lucide `git-fork`)。
pub const ICON_WORKTREE: &[Shape] = &[
    ring(12.0, 18.0, 3.0),
    ring(6.0, 6.0, 3.0),
    ring(18.0, 6.0, 3.0),
    path("M18 9v2c0 .6-.4 1-1 1H7c-.6 0-1-.4-1-1V9"),
    path("M12 12v3"),
];
/// 列表视图(Lucide `list`;行首的 `h.01` 小点换成实心圆点,14px 下才看得见)。
pub const ICON_LIST: &[Shape] = &[
    dot(3.5, 6.0),
    dot(3.5, 12.0),
    dot(3.5, 18.0),
    path("M8 6h13"),
    path("M8 12h13"),
    path("M8 18h13"),
];
/// 树形视图(Lucide `list-tree`)。
pub const ICON_LIST_TREE: &[Shape] = &[
    path("M21 12h-8"),
    path("M21 6H8"),
    path("M21 18h-8"),
    path("M3 6v4c0 1.1.9 2 2 2h3"),
    path("M3 10v6c0 1.1.9 2 2 2h3"),
];
/// 勾(Lucide `check`)。
pub const ICON_CHECK: &[Shape] = &[path("M20 6 9 17l-5-5")];
/// 搜索(Lucide `search`)。
pub const ICON_SEARCH: &[Shape] = &[ring(11.0, 11.0, 8.0), path("m21 21-4.3-4.3")];
/// 新建(Lucide `plus`)。
pub const ICON_PLUS: &[Shape] = &[path("M5 12h14"), path("M12 5v14")];
/// 指向(Lucide `arrow-right`)。
pub const ICON_ARROW_RIGHT: &[Shape] = &[path("M5 12h14"), path("m12 5 7 7-7 7")];
/// 待推送计数(Lucide `arrow-up`)。
pub const ICON_ARROW_UP: &[Shape] = &[path("m5 12 7-7 7 7"), path("M12 19V5")];
/// 待拉取计数(Lucide `arrow-down`)。
pub const ICON_ARROW_DOWN: &[Shape] = &[path("M12 5v14"), path("m19 12-7 7-7-7")];
/// 合并进行中(Lucide `git-merge`)。
pub const ICON_MERGE: &[Shape] = &[
    ring(18.0, 18.0, 3.0),
    ring(6.0, 6.0, 3.0),
    path("M6 21V9a9 9 0 0 0 9 9"),
];
/// 冲突 / 警告(Lucide `triangle-alert`)。原版的 `h.01` 小点 14px 下看不见,换成实心圆点。
pub const ICON_ALERT: &[Shape] = &[
    path("m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3"),
    path("M12 9v4"),
    dot(12.0, 17.0),
];

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
    ICON_CHEVRON_DOWN,
    ICON_CHEVRON_RIGHT,
    ICON_WORKTREE,
    ICON_LIST,
    ICON_LIST_TREE,
    ICON_CHECK,
    ICON_SEARCH,
    ICON_PLUS,
    ICON_ARROW_RIGHT,
    ICON_ARROW_UP,
    ICON_ARROW_DOWN,
    ICON_MERGE,
    ICON_ALERT,
];

/// 图标的标准尺寸(按钮里 14px)。
pub const ICON_SIZE: f32 = 14.0;
/// 图标按钮的点击区边长。
pub const ICON_BUTTON: f32 = 24.0;

/// 一枚图标。
pub fn icon(shapes: &'static [Shape], size: f32, color: Hsla) -> VectorIcon {
    VectorIcon::new(shapes, px(size)).ink(color)
}

/// 随 `group` 悬停变色的图标(常态 `normal`,悬停 `hover`)。见模块注释。
pub fn hover_icon(
    group: impl Into<SharedString>,
    shapes: &'static [Shape],
    size: f32,
    normal: Hsla,
    hover: Hsla,
) -> Div {
    let group: SharedString = group.into();
    let layer = || div().absolute().top_0().left_0().size(px(size));
    div()
        .relative()
        .flex_none()
        .size(px(size))
        .child(
            layer()
                .group_hover(group.clone(), |s| s.invisible())
                .child(icon(shapes, size, normal)),
        )
        .child(
            layer()
                .invisible()
                .group_hover(group, |s| s.visible())
                .child(icon(shapes, size, hover)),
        )
}

/// 图标按钮:24px 方形点击区、圆角 4px;悬停 `bg_elevated` 底 + 图标转主色;
/// `busy` 时图标换成转圈,`disabled`(含 busy)时压淡且不再响应悬停。
///
/// `id` 同时用作悬停分组名,所以同一画面里要唯一。点击回调由调用方按需挂
/// (禁用态别挂)。
pub fn icon_button(
    id: &'static str,
    shapes: &'static [Shape],
    busy: bool,
    disabled: bool,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .group(id)
        .size(px(ICON_BUTTON))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(4.0));
    if busy {
        return base.child(ui::spinner(px(ICON_SIZE), ui::accent()));
    }
    if disabled {
        return base
            .opacity(0.45)
            .child(icon(shapes, ICON_SIZE, ui::text_muted()));
    }
    base.cursor_pointer()
        .hover(|el| el.bg(ui::bg_elevated()))
        .child(hover_icon(
            id,
            shapes,
            ICON_SIZE,
            ui::text_muted(),
            ui::text_primary(),
        ))
}

/// 等宽字族(与终端同一字族)。哈希 / 计数用它,位数不同也对得齐。
///
/// 不用 `"monospace"` 这种泛型族名:Windows 的 DirectWrite 解析不到,会静默回落成
/// 界面字体(命令库那里踩过)。
pub fn mono_font(store: &Entity<AppStore>, cx: &App) -> SharedString {
    crate::pane_preview::preview_style(store.read(cx)).font_family
}

/// 胶囊:约 18px 高、全圆角、语义色 alpha 0.15 的底 + 语义色字,字号比正文小一档。
pub fn pill(text: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.0))
        .h(px(18.0))
        .px(px(7.0))
        .rounded_full()
        .bg(ui::with_alpha(color, 0.15))
        .text_color(color)
        .text_size(ui::font_px(11.0))
        .child(text.into())
}

/// 计数胶囊(↑n 待推送 / ↓m 待拉取):小箭头图标 + 等宽数字。
pub fn count_pill(shapes: &'static [Shape], count: usize, color: Hsla, mono: SharedString) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(2.0))
        .h(px(18.0))
        .pl(px(5.0))
        .pr(px(7.0))
        .rounded_full()
        .bg(ui::with_alpha(color, 0.15))
        .text_color(color)
        .child(icon(shapes, 10.0, color))
        .child(
            div()
                .font_family(mono)
                .text_size(ui::font_px(11.0))
                .child(count.to_string()),
        )
}

/// 分组标题(弹窗列表里):小一号、muted、略开字距。gpui 没有 letter-spacing,
/// 字距靠左右留白与上方间隔撑开层次。
pub fn group_header(text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_center()
        .h(px(26.0))
        .px(px(12.0))
        .pt(px(6.0))
        .text_size(ui::font_px(11.0))
        .text_color(ui::text_muted())
        .child(text.into())
}

/// 静态的「处理中」提示行(spinner + 文案)。
pub fn busy_line(text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(ui::font_px(12.0))
        .text_color(ui::text_muted())
        .child(ui::spinner(px(12.0), ui::accent()))
        .child(text.into())
}

/// 复选框:勾上时用 `color` 实心底 + 对比色勾(危险选项传错误色)。
pub fn check_box(checked: bool, color: Hsla) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(14.0))
        .rounded(px(3.0))
        .border_1()
        .border_color(if checked { color } else { ui::border_strong() })
        .when(checked, |el| {
            el.bg(color).child(icon(ICON_CHECK, 11.0, ui::bg_base()))
        })
}

/// 只读、可选中复制的多行原文框(git 的 stderr)。高度按行数估,封顶 `max_rows`。
pub fn error_text_state(
    text: &str,
    max_rows: usize,
    window: &mut Window,
    cx: &mut App,
) -> Entity<TextareaState> {
    let rows = text.lines().count().clamp(2, max_rows);
    let text = text.to_string();
    cx.new(|cx| {
        TextareaState::new(window, cx)
            .rows(rows)
            .default_value(text)
    })
}

/// 原文框元素。
pub fn error_text_box(state: &Entity<TextareaState>) -> AnyElement {
    div()
        .w_full()
        .rounded(px(4.0))
        .border_1()
        .border_color(ui::with_alpha(ui::color_error(), 0.35))
        .bg(ui::with_alpha(ui::color_error(), 0.06))
        .child(Textarea::new(state).readonly(true).bordered(false))
        .into_any_element()
}

/// git 操作失败的详情弹窗:标题 + 一句说明 + **可选中复制**的完整原文 + 「复制」「知道了」。
///
/// 面板上的操作(Fetch / 拉取 / 检出…)失败走这里而不是 toast:toast 5s 就收,
/// 而 git 的报错常是多行(哪些文件挡住了检出、hook 的输出),得留给人读、能复制。
pub fn show_git_error(
    title: impl Into<SharedString>,
    detail: String,
    window: &mut Window,
    cx: &mut App,
) {
    if crate::prompt::is_open(kind::GIT_ERROR) {
        return;
    }
    let title: SharedString = title.into();
    let detail = match detail.trim() {
        "" => t("gitBranch", "errorNoOutput").to_string(),
        trimmed => trimmed.to_string(),
    };
    let state = error_text_state(&detail, 14, window, cx);
    open_guarded(kind::GIT_ERROR, window, cx, move |dialog, _window, _cx| {
        let copy_text = detail.clone();
        dialog
            .title(dialog_title(kind::GIT_ERROR, title.clone()))
            .w(px(520.0))
            .overlay_closable(true)
            .child(
                div()
                    .px(px(16.0))
                    .pb(px(16.0))
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(ui::font_px(12.0))
                            .text_color(ui::text_secondary())
                            .child(icon(ICON_ALERT, ICON_SIZE, ui::color_error()))
                            .child(t("gitBranch", "errorLead")),
                    )
                    .child(error_text_box(&state))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                ui::ghost_button("git-error-copy", t("gitBranch", "copy"))
                                    .on_click(move |_: &ClickEvent, _window, cx| {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            copy_text.clone(),
                                        ));
                                    }),
                            )
                            .child(
                                ui::primary_button("git-error-ok", t("gitBranch", "gotIt"))
                                    .on_click(|_: &ClickEvent, window, cx| {
                                        close_guarded(kind::GIT_ERROR, window, cx);
                                    }),
                            ),
                    ),
            )
            // 回车 = 知道了
            .on_ok(|_, _, _| true)
    });
}

/// 结果 toast 的语气。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// 成功 / 信息(`i`)。
    Info,
    /// 需要留意(冲突、本地已有同名分支没同步之类,`!`)。
    Attention,
}

/// 推一条带结果摘要的 toast(「已检出 feature/x」「拉取了 5 个提交」)。标题是当前项目名。
pub fn toast(store: &Entity<AppStore>, message: String, tone: Tone, cx: &mut App) {
    let (project_id, project_name) = store
        .read(cx)
        .active_project()
        .map(|p| (p.id.clone(), p.name.clone()))
        .unwrap_or_else(|| (String::new(), "Git".to_string()));
    let kind = match tone {
        Tone::Info => ToastKind::WslInfo,
        Tone::Attention => ToastKind::PasteError,
    };
    crate::toast::push_message(kind, project_id, project_name, message, cx);
}

/// 空态:居中的大号 muted 图标 + 一句说明 + 可选的主操作按钮。
pub fn empty_state(
    shapes: &'static [Shape],
    text: impl Into<SharedString>,
    action: Option<Stateful<Div>>,
) -> Div {
    div()
        .w_full()
        .py(px(24.0))
        .flex()
        .flex_col()
        .items_center()
        .gap(px(10.0))
        .child(icon(shapes, 32.0, ui::with_alpha(ui::text_muted(), 0.7)))
        .child(
            div()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(text.into()),
        )
        .when_some(action, |el, action| el.child(action))
}

/// 选中行的左侧 2px accent 竖条(行要 `relative()`)。
pub fn selection_bar() -> Div {
    div()
        .absolute()
        .left_0()
        .top(px(4.0))
        .bottom(px(4.0))
        .w(px(2.0))
        .rounded(px(1.0))
        .bg(ui::accent())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每枚图标:每一笔都解析得出点、所有点落在单位方框内(容差同 mt-ui 那条:
    /// 原样搬的 path 允许 1%)。路径解析失败会表现为「没有点」。
    #[test]
    fn 图标几何都在单位方框内() {
        for (i, shapes) in ALL_ICONS.iter().enumerate() {
            assert!(!shapes.is_empty(), "第 {i} 枚图标是空的");
            for shape in shapes.iter() {
                let (pts, _) = shape.geom.points();
                assert!(
                    pts.len() >= 2,
                    "第 {i} 枚图标有一笔没解析出点: {:?}",
                    shape.geom
                );
                for (x, y) in pts {
                    assert!(
                        (-0.01..=1.01).contains(&x) && (-0.01..=1.01).contains(&y),
                        "第 {i} 枚图标越界点 ({x}, {y})"
                    );
                }
            }
        }
    }
}
