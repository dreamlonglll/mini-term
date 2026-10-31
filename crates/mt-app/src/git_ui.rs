//! Git 面板的**一套**视觉语言:线性图标([`icons`])+ 全部共用小件。
//!
//! 「分支与同步」「提交与 diff」「提交历史」三条线并行开发时各写了一套图标按钮、
//! 胶囊、空态、文件行与选中样式,合并后摆在同一个面板里粗细、圆角、配色都不一样。
//! 这里是收口后的唯一来源 —— Git 相关界面(面板、分支弹窗、推送 / diff / 历史 /
//! worktree 弹窗)一律从这里取件,不再各自手搓。
//!
//! # 口径一览
//!
//! | 件 | 口径 |
//! |---|---|
//! | 图标按钮 | 24px 点击区、圆角 4、悬停薄纱底 + 图标转主色、禁用压淡、**必带 tooltip**、点击不冒泡 |
//! | 语义胶囊 | 18px 高、全圆角、语义色 15% 底;HEAD = accent、本地 = success、远程 = info、标签 = warning、↑ = success、↓ = info |
//! | 区块 / 分组标题 | 折叠箭头 + 标题 + 数量胶囊 + 右侧操作,整行可点折叠 |
//! | 列表行 | 28px、左右 8、圆角 4;悬停薄纱底;选中 = `accent_subtle` 底 + 左侧 2px accent 竖条 |
//! | 改动文件树 | 每层缩进 16;单链目录压缩成 `a/b/c`、目录行后缀弱化文件数、目录在前(`change_tree` 子模块) |
//! | 字号 | 11(胶囊 / 角注)· 12(次要信息)· 13(正文 / 行)· 14(弹窗标题区) |
//! | 弹窗 | 正文与标题、页脚同一条左缘;页脚「取消 / 确认」,确认键主色或危险色 |
//!
//! # 深浅主题
//!
//! 颜色全部从 [`ui`] 的色板派生,不写死 rgb。两处按底色明暗微调(判据
//! [`is_light`],`git_graph::adapt_to_ground` 也用它):
//!
//! - 语义色写在自己淡底上的字([`soft_fg`]):浅底上同色字与 15% 淡底拉不开对比,
//!   亮度压一截(色相不变),11px 的胶囊字才读得清;
//! - 悬停底([`hover_bg`])用前景色的一层薄纱而不是 `bg_elevated`:弹窗左栏本身就是
//!   `bg_elevated`,同色悬停等于没有;薄纱叠在哪层底上都看得出。
//!
//! # 图标按钮的悬停变色
//!
//! [`mt_ui::icons::VectorIcon`] 的颜色是渲染时定死的,不吃 `hover` 样式里的文字色。
//! 这里叠两枚图标(常态色 / 悬停色),用 `group_hover` 切可见性 —— 纯样式,不必给每颗
//! 按钮记悬停态;被隐藏的那枚 `Visibility::Hidden` 整个跳过绘制。组名是固定的
//! [`BUTTON_GROUP`]:gpui 按最近的祖先认组,每颗按钮各用各的同名组互不串。
//!
//! # 写操作的公共骨架
//!
//! 会写仓库的操作一律经 [`spawn_gated`]:占按仓库的写操作闸([`gate`])→ 后台执行 →
//! 放闸 → 回 UI 线程交给调用方。各区自己的「进行中」标记(按钮转圈、确认键换「执行中…」)
//! 与失败怎么展示仍由各区定:面板上的操作失败走可选中复制的详情框([`show_git_error`]),
//! 弹窗里的就地红字(toast 在遮罩下看不见),成功走 toast([`toast`])。

mod change_tree;
pub mod gate;
mod icons;

pub use change_tree::*;
pub use icons::*;

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Div, ElementId, Entity, FontWeight, Hsla,
    InteractiveElement, IntoElement, ParentElement, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::ButtonVariants as _;
use gpui_component::dialog::{Cancel, Confirm as DialogConfirm, DialogFooter};
use gpui_component::input::{Textarea, TextareaState};
use mt_project::git::GitStatus;
use mt_ui::icons::{FileIcon, Shape};
use mt_ui::tooltip::TooltipExt as _;

use crate::i18n::t;
use crate::notify::ToastKind;
use crate::prompt::{FOCUS_RING_ROOM, close_guarded, footer_button, kind, open_guarded};
use crate::store::AppStore;
use crate::ui;

// ─── 颜色 ─────────────────────────────────────────────────────

/// 「亮底」的亮度阈值:底色亮于它就按浅色主题微调(胶囊字压暗、拓扑图线色压暗)。
const LIGHT_GROUND_L: f32 = 0.6;

/// 这个亮度的底色算不算亮的(`git_graph::adapt_to_ground` 也按它判)。
pub fn is_light(lightness: f32) -> bool {
    lightness > LIGHT_GROUND_L
}

/// 语义色的淡底(胶囊、计数、横幅里的标记)。
pub fn soft_bg(color: Hsla) -> Hsla {
    ui::with_alpha(color, 0.15)
}

/// 浅底上语义色字与胶囊底的最低对比度(WCAG AA 正文档)。11px 的胶囊字比正文还小,
/// 低于这一档就糊成一片(浅色主题的「2 个冲突」attention 胶囊踩过)。
const SOFT_FG_MIN_CONTRAST: f32 = 4.5;

/// WCAG 相对亮度(按 sRGB 线性化)。
fn luminance(color: Hsla) -> f32 {
    let rgb = color.to_rgb();
    let lin = |c: f32| {
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(rgb.r) + 0.7152 * lin(rgb.g) + 0.0722 * lin(rgb.b)
}

/// 两色的 WCAG 对比度(1 ~ 21)。
pub(crate) fn contrast_ratio(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// 语义色写在自己淡底上的字色:浅底上亮度压一截(色相不变),见模块注释。
///
/// 压多少按对比度定,不是固定比例:先压到 0.8 倍,与胶囊实际底色(面板底 + 15% 淡底)
/// 的对比度还不到 [`SOFT_FG_MIN_CONTRAST`] 就继续压 —— 黄 / 琥珀一类本身很亮的
/// 语义色(attention、warning)固定 0.8 倍远远不够。
pub fn soft_fg(color: Hsla) -> Hsla {
    soft_fg_on(color, ui::bg_surface())
}

/// 面板底上叠一层半透明 `tint` 之后的实际底色(不透明)。胶囊放在染过色的条带里时,
/// 拿它给 [`soft_fg_on`] / [`pill_on`]。
pub fn ground_with(tint: Hsla) -> Hsla {
    opaque(ui::bg_surface().blend(tint))
}

/// 同 [`soft_fg`],但胶囊不是直接放在面板底上,而是放在不透明的 `ground` 上(进行中
/// 操作横幅那种已经染过一层 attention 的条带):对比度按 `ground` + 15% 淡底算。
/// 染过的底比面板底深,按面板底算出来的字色在那里不够 AA(「2 个冲突」踩过 4.32)。
pub fn soft_fg_on(color: Hsla, ground: Hsla) -> Hsla {
    if !is_light(ground.l) {
        return color;
    }
    let ground = opaque(ground.blend(soft_bg(color)));
    darken_to_contrast(
        Hsla {
            l: color.l * 0.8,
            ..color
        },
        ground,
    )
}

/// 语义色直接当**字色**写在面板底上(文件名 / 状态字母 / 冲突组标题):浅底上亮度压到与
/// 面板底的对比度够 [`SOFT_FG_MIN_CONTRAST`](色相不变);暗色主题原样。浅色主题的
/// attention / warning(#B08620)写在 #F5F5F5 上只有 3.07(E2E r4 C10),取色思路与
/// 胶囊字([`soft_fg`])同一套,只是不垫淡底。
pub fn text_fg(color: Hsla) -> Hsla {
    let ground = ui::bg_surface();
    if !is_light(ground.l) {
        return color;
    }
    darken_to_contrast(color, ground)
}

/// 把 `fg` 的亮度一档档往下压,直到与不透明的 `ground` 对比度够 AA。
fn darken_to_contrast(mut fg: Hsla, ground: Hsla) -> Hsla {
    while fg.l > 0.05 && contrast_ratio(fg, ground) < SOFT_FG_MIN_CONTRAST {
        fg.l -= 0.02;
    }
    fg
}

/// 去掉 alpha(预混出来的底色当不透明色用)。
fn opaque(color: Hsla) -> Hsla {
    Hsla { a: 1.0, ..color }
}

/// 行的悬停底:前景色的一层薄纱(见模块注释)。
pub fn hover_bg() -> Hsla {
    ui::with_alpha(ui::text_primary(), 0.06)
}

/// 图标按钮 / 小按钮的悬停底:比行实一档 —— 按钮小,太淡认不出点击区。
pub fn button_hover_bg() -> Hsla {
    ui::with_alpha(ui::text_primary(), 0.09)
}

// ─── 字体 ─────────────────────────────────────────────────────

/// 等宽字体链(哈希、计数、时间、状态字母):各数位等宽,竖着排才对得齐。CJK 回落
/// 雅黑(「3 小时前」)。
///
/// 不用 `"monospace"` 这种泛型族名:Windows 的 DirectWrite 解析不到,会静默回落成
/// 界面字体(命令库那里踩过)。
pub fn mono<E: Styled>(mut el: E) -> E {
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

/// 估一段界面文字的宽:全角按 1em、其余按 0.6em。只用来定截断 / 让位的保底(仓库栏的
/// 仓库名、历史行的提交标题),不求精确。
pub fn estimate_text_width(text: &str, font: f32) -> f32 {
    text.chars()
        .map(|c| if c.is_ascii() { 0.6 } else { 1.0 })
        .sum::<f32>()
        * font
}

// ─── 按钮 ─────────────────────────────────────────────────────

/// 图标按钮 / 小按钮的悬停分组名(见模块注释)。
const BUTTON_GROUP: &str = "git-icon-button";

/// 两枚叠放的图标,随 [`BUTTON_GROUP`] 悬停切换(常态 `normal` / 悬停 `hover`)。
fn swap_icon(shapes: &'static [Shape], size: f32, normal: Hsla, hover: Hsla) -> Div {
    let layer = || div().absolute().top_0().left_0().size(px(size));
    div()
        .relative()
        .flex_none()
        .size(px(size))
        .child(
            layer()
                .group_hover(BUTTON_GROUP, |s| s.invisible())
                .child(icon(shapes, size, normal)),
        )
        .child(
            layer()
                .invisible()
                .group_hover(BUTTON_GROUP, |s| s.visible())
                .child(icon(shapes, size, hover)),
        )
}

/// 图标按钮的本体。`busy` 时图标换成转圈;`enabled = false`(含 busy)时压淡、
/// 不挂手型、不响应悬停 —— 调用方也别挂 `on_click`。
fn icon_button_with(
    id: ElementId,
    shapes: &'static [Shape],
    tip: SharedString,
    busy: bool,
    enabled: bool,
    hover_ink: Hsla,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .group(BUTTON_GROUP)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(ICON_BUTTON))
        .rounded(px(4.0))
        .tip(tip)
        // 点击到此为止:按钮常住在可点的行 / 分组标题里,禁用态不挂回调时点击会冒泡
        // 上去误触折叠或选中。调用方自己的 on_click 照样会被调到(同一元素的点击监听
        // 全部执行)
        .on_click(|_: &ClickEvent, _window, cx| cx.stop_propagation());
    if busy {
        return base.child(ui::spinner(px(ICON_SIZE), ui::accent()));
    }
    if !enabled {
        return base
            .opacity(0.5)
            .child(icon(shapes, ICON_SIZE, ui::text_muted()));
    }
    base.cursor_pointer()
        .hover(|el| el.bg(button_hover_bg()))
        .child(swap_icon(
            shapes,
            ICON_SIZE,
            ui::text_secondary(),
            hover_ink,
        ))
}

/// 图标按钮:24px 方形点击区、圆角 4;悬停薄纱底 + 图标转主色;禁用压淡。
/// **tooltip 是必填参数**(纯图标按钮没有文字,不挂提示就是哑谜),内部已挂好,
/// 调用方别再 `.tip(..)`(gpui 一个元素只许设一次 tooltip)。
pub fn icon_button(
    id: impl Into<ElementId>,
    shapes: &'static [Shape],
    tip: impl Into<SharedString>,
    enabled: bool,
) -> Stateful<Div> {
    icon_button_with(
        id.into(),
        shapes,
        tip.into(),
        false,
        enabled,
        ui::text_primary(),
    )
}

/// 危险动作(丢弃 / 删除)的图标按钮:悬停时图标转错误色。
pub fn danger_icon_button(
    id: impl Into<ElementId>,
    shapes: &'static [Shape],
    tip: impl Into<SharedString>,
    enabled: bool,
) -> Stateful<Div> {
    icon_button_with(
        id.into(),
        shapes,
        tip.into(),
        false,
        enabled,
        ui::color_error(),
    )
}

/// 带「进行中」态的图标按钮(仓库栏的 Fetch / 拉取 / 推送 / 刷新):`busy` 时转圈,
/// `disabled` 或 `busy` 时都不可点。
pub fn busy_icon_button(
    id: impl Into<ElementId>,
    shapes: &'static [Shape],
    tip: impl Into<SharedString>,
    busy: bool,
    disabled: bool,
) -> Stateful<Div> {
    icon_button_with(
        id.into(),
        shapes,
        tip.into(),
        busy,
        !busy && !disabled,
        ui::text_primary(),
    )
}

/// 「图标 + 文字」的小按钮(分支弹窗顶部的「新建分支」「Fetch」):26px 高、圆角 4,
/// 悬停薄纱底、文字与图标转主色;`busy` 时图标换转圈,禁用时压淡。
pub fn chip_button(
    id: impl Into<ElementId>,
    shapes: &'static [Shape],
    label: impl Into<SharedString>,
    busy: bool,
    disabled: bool,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .group(BUTTON_GROUP)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.0))
        .h(px(26.0))
        .px(px(8.0))
        .rounded(px(4.0))
        .text_size(ui::font_px(12.0));
    if busy {
        return base
            .text_color(ui::text_muted())
            .child(ui::spinner(px(ICON_SIZE_SM), ui::accent()))
            .child(label.into());
    }
    if disabled {
        return base
            .opacity(0.5)
            .text_color(ui::text_muted())
            .child(icon(shapes, ICON_SIZE, ui::text_muted()))
            .child(label.into());
    }
    base.cursor_pointer()
        .text_color(ui::text_secondary())
        .hover(|el| el.bg(button_hover_bg()).text_color(ui::text_primary()))
        .child(swap_icon(
            shapes,
            ICON_SIZE,
            ui::text_secondary(),
            ui::text_primary(),
        ))
        .child(label.into())
}

/// 面板里文字按钮的三种语气。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// 实心 accent(「继续」「清除过滤」这类主操作)。
    Primary,
    /// 边框 + 淡字,悬停转 accent(次要操作)。
    Ghost,
    /// 边框 + 淡字,悬停转错误色(删除类)。
    Danger,
}

/// 面板里的文字按钮。几何与 `ui::primary_button` / `ui::ghost_button` 一致(26px 高、
/// 圆角 4、12px 字),多了**禁用态**:压淡、不挂手型、**不响应悬停** —— `ui::` 那几颗
/// 的悬停样式一旦设上就摘不掉,禁用时套个 `opacity` 悬停照样会亮起来。
/// 禁用时调用方别挂 `on_click`。
pub fn action_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    kind: ButtonKind,
    enabled: bool,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(26.0))
        .px(px(if kind == ButtonKind::Primary {
            12.0
        } else {
            10.0
        }))
        .rounded(px(4.0))
        .text_size(ui::font_px(12.0))
        .child(label.into());
    let base = match kind {
        ButtonKind::Primary => base.bg(ui::accent()).text_color(ui::bg_base()),
        ButtonKind::Ghost | ButtonKind::Danger => base
            .border_1()
            .border_color(ui::border_default())
            .text_color(ui::text_secondary()),
    };
    if !enabled {
        return base.opacity(0.5);
    }
    let tone = if kind == ButtonKind::Danger {
        ui::color_error()
    } else {
        ui::accent()
    };
    base.cursor_pointer().hover(move |el| match kind {
        ButtonKind::Primary => el.opacity(0.9),
        ButtonKind::Ghost | ButtonKind::Danger => el.border_color(tone).text_color(tone),
    })
}

/// 行内的小号文字按钮(diff hunk 头上的「暂存此块」这类):边框 + 弱化文字,悬停转
/// accent / 危险色。禁用时整颗压淡、不挂手型。
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
        .when(!enabled, |el| el.opacity(0.5))
}

/// 分段控件的外框(diff 的「并排 / 内联」、worktree 的「已有分支 / 新分支」)。
pub fn segmented() -> Div {
    div()
        .flex()
        .flex_none()
        .rounded(px(4.0))
        .overflow_hidden()
        .border_1()
        .border_color(ui::border_default())
}

/// 分段控件里的一段:24px 高、12px 字;选中 = accent 淡底 + accent 字。
pub fn segment(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(24.0))
        .flex()
        .items_center()
        .px(px(10.0))
        .text_size(ui::font_px(12.0))
        .cursor_pointer()
        .when(active, |el| {
            el.bg(ui::accent_subtle()).text_color(ui::accent())
        })
        .when(!active, |el| {
            el.text_color(ui::text_secondary())
                .hover(|el| el.bg(hover_bg()).text_color(ui::text_primary()))
        })
        .child(label.into())
}

// ─── 胶囊 ─────────────────────────────────────────────────────

/// 胶囊高度。
pub const PILL_H: f32 = 18.0;

/// 胶囊的底子:18px 高、全圆角、语义色淡底 + 语义色字,字号比正文小一档。
fn pill_base(color: Hsla) -> Div {
    pill_base_on(color, ui::bg_surface())
}

/// 同 [`pill_base`],字色按胶囊实际所在的底 `ground` 定(见 [`soft_fg_on`])。
fn pill_base_on(color: Hsla, ground: Hsla) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.0))
        .h(px(PILL_H))
        .px(px(7.0))
        .rounded_full()
        .bg(soft_bg(color))
        .text_color(soft_fg_on(color, ground))
        .text_size(ui::font_px(11.0))
}

/// 语义胶囊(状态字母、冲突数、起点分支…)。
pub fn pill(text: impl Into<SharedString>, color: Hsla) -> Div {
    pill_base(color).child(text.into())
}

/// 放在染过色的条带里的语义胶囊:`ground` 是条带的实际底色(用 [`ground_with`] 求),
/// 字色按它压到够 AA。直接放在面板底上的用 [`pill`]。
pub fn pill_on(text: impl Into<SharedString>, color: Hsla, ground: Hsla) -> Div {
    pill_base_on(color, ground).child(text.into())
}

/// 带前缀小图标的胶囊(worktree 行上的分支名)。
pub fn icon_pill(shapes: &'static [Shape], text: impl Into<SharedString>, color: Hsla) -> Div {
    pill_base(color)
        .pl(px(6.0))
        .child(icon(shapes, ICON_SIZE_XS, soft_fg(color)))
        .child(text.into())
}

/// 数量胶囊(分组标题的文件数):等宽数字。
pub fn count_pill(count: usize, color: Hsla) -> Div {
    mono(pill_base(color)).child(count.to_string())
}

/// ↑n 待推送(success)。
pub fn ahead_pill(count: usize) -> Div {
    sync_pill(ICON_ARROW_UP, count, ui::color_success())
}

/// ↓m 待拉取(info)。
pub fn behind_pill(count: usize) -> Div {
    sync_pill(ICON_ARROW_DOWN, count, ui::color_info())
}

fn sync_pill(shapes: &'static [Shape], count: usize, color: Hsla) -> Div {
    pill_base(color)
        .gap(px(2.0))
        .pl(px(5.0))
        .child(icon(shapes, ICON_SIZE_XS, soft_fg(color)))
        .child(mono(div()).child(count.to_string()))
}

/// 引用胶囊的语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
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
    pub fn color(self) -> Hsla {
        match self {
            RefKind::Head => ui::accent(),
            RefKind::Local => ui::color_success(),
            RefKind::Remote => ui::color_info(),
            RefKind::Tag => ui::color_warning(),
        }
    }
}

/// 可截断的名字胶囊:名字太长时胶囊自己截断(保底 40px 认得出前缀),全名请调用方
/// 挂 tooltip;要封顶宽度的调用方自己 `.max_w(..)`。`lead` 是名字前的小图标。
/// 颜色按语义走 [`RefKind`] 的用 [`ref_pill`];这里留给语义表之外的状态色
/// (仓库栏 detached HEAD 的警告色)。
pub fn name_pill(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    color: Hsla,
    lead: Option<&'static [Shape]>,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(PILL_H))
        .min_w(px(40.0))
        .flex()
        .items_center()
        .gap(px(4.0))
        .px(px(7.0))
        .rounded_full()
        .bg(soft_bg(color))
        .text_color(soft_fg(color))
        .text_size(ui::font_px(11.0))
        .when_some(lead, |el, shapes| {
            el.pl(px(6.0))
                .child(icon(shapes, ICON_SIZE_XS, soft_fg(color)))
        })
        .child(div().min_w_0().truncate().child(label.into()))
}

/// 引用胶囊(分支 / 标签 / HEAD),配色见 [`RefKind::color`];标签带前缀图标。
pub fn ref_pill(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    kind: RefKind,
) -> Stateful<Div> {
    name_pill(
        id,
        label,
        kind.color(),
        (kind == RefKind::Tag).then_some(ICON_TAG),
    )
}

/// 「+N」胶囊(其余引用收进 tooltip)。中性色,不与任何语义撞色。
pub fn more_pill(id: impl Into<ElementId>, hidden: usize) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .h(px(PILL_H))
        .flex()
        .items_center()
        .px(px(6.0))
        .rounded_full()
        .bg(ui::with_alpha(ui::text_muted(), 0.15))
        .text_color(ui::text_secondary())
        .text_size(ui::font_px(11.0))
        .child(format!("+{hidden}"))
}

// ─── 标题 ─────────────────────────────────────────────────────

/// 区块 / 分组标题:折叠箭头 + (图标)+ 标题 + 数量胶囊 + 右侧操作。
///
/// 两档:区块(面板里的「更改」「提交历史」「提交详情」,30px、13px 主色半粗)与
/// 分组([`Self::group`]:「已暂存」「未暂存」这类二级标题,28px、12px 次要色半粗)。
/// 可折叠时整行可点,点击回调由调用方挂在 [`Self::build`] 的结果上;右侧操作里的
/// 按钮用本模块的图标按钮,它们自己拦住冒泡,不会误触折叠。
pub struct SectionHeader {
    id: ElementId,
    title: SharedString,
    group: bool,
    open: Option<bool>,
    tone: Option<Hsla>,
    icon: Option<&'static [Shape]>,
    count: Option<usize>,
    trailing: Vec<AnyElement>,
}

impl SectionHeader {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            group: false,
            open: None,
            tone: None,
            icon: None,
            count: None,
            trailing: Vec::new(),
        }
    }

    /// 二级分组标题。
    pub fn group(mut self) -> Self {
        self.group = true;
        self
    }

    /// 可折叠,`open` = 当前是否展开。
    pub fn collapsible(mut self, open: bool) -> Self {
        self.open = Some(open);
        self
    }

    /// 强调色(冲突组):标题、图标、数量胶囊一起换色。
    pub fn tone(mut self, color: Hsla) -> Self {
        self.tone = Some(color);
        self
    }

    /// 标题前的语义图标。
    pub fn icon(mut self, shapes: &'static [Shape]) -> Self {
        self.icon = Some(shapes);
        self
    }

    /// 标题后的数量胶囊。
    pub fn count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }

    /// 右侧的一个操作 / 附注(可多次调用,依次排开)。
    pub fn trailing(mut self, el: impl IntoElement) -> Self {
        self.trailing.push(el.into_any_element());
        self
    }

    pub fn build(self) -> Stateful<Div> {
        // 强调色当字色 / 图标色用:浅色主题下压到够 AA(冲突组标题,见 [`text_fg`]);
        // 数量胶囊另按自己的淡底取色,仍传原色
        let tone = self.tone;
        let ink = tone.map(text_fg);
        let title_color = ink.unwrap_or(if self.group {
            ui::text_secondary()
        } else {
            ui::text_primary()
        });
        let has_trailing = !self.trailing.is_empty();
        div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .h(px(if self.group { 28.0 } else { 30.0 }))
            .px(px(8.0))
            .when(self.group, |el| el.rounded(px(4.0)))
            .when_some(self.open, |el, open| {
                el.cursor_pointer().hover(|el| el.bg(hover_bg())).child(
                    div()
                        .flex_none()
                        .w(px(ICON_SIZE))
                        .flex()
                        .justify_center()
                        .child(icon(
                            if open {
                                ICON_CHEVRON_DOWN
                            } else {
                                ICON_CHEVRON_RIGHT
                            },
                            ICON_SIZE_SM,
                            ui::text_muted(),
                        )),
                )
            })
            .when_some(self.icon, |el, shapes| {
                el.child(icon(shapes, ICON_SIZE, ink.unwrap_or(ui::text_muted())))
            })
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(ui::font_px(if self.group { 12.0 } else { 13.0 }))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(title_color)
                    .child(self.title),
            )
            .when_some(self.count, |el, count| {
                el.child(count_pill(count, tone.unwrap_or(ui::text_muted())))
            })
            .when(has_trailing, |el| {
                el.child(div().flex_1()).child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(2.0))
                        .children(self.trailing),
                )
            })
    }
}

/// 弹窗列表里的分组小标题(分支弹窗的「当前分支 / 最近 / 本地 / 远程」):小一号、
/// muted,不可点。gpui 没有 letter-spacing,层次靠留白撑开。左右 8 与 [`list_row`]
/// 的内边距一致,标题字与行首图标对齐。
pub fn list_caption(text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_center()
        .h(px(26.0))
        .px(px(8.0))
        .pt(px(6.0))
        .text_size(ui::font_px(11.0))
        .text_color(ui::text_muted())
        .child(text.into())
}

// ─── 空态 / 加载 / 错误 ───────────────────────────────────────

/// 空态:居中的 32px muted 图标 + 一句说明 + 可选的主操作。要撑满容器的调用方
/// 自己 `.size_full()`。
pub fn empty_state(
    shapes: &'static [Shape],
    text: impl Into<SharedString>,
    action: Option<AnyElement>,
) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(12.0))
        .py(px(24.0))
        .px(px(16.0))
        .child(icon(shapes, ICON_SIZE_EMPTY, ui::text_muted()))
        .child(
            div()
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_muted())
                .text_center()
                .child(text.into()),
        )
        .children(action)
}

/// 「处理中」提示行(spinner + 文案)。spinner 走 `mt_ui::motion::pulse_phase` 的低频泵。
pub fn busy_line(text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(ui::font_px(12.0))
        .text_color(ui::text_muted())
        .child(ui::spinner(px(ICON_SIZE_SM), ui::accent()))
        .child(text.into())
}

/// 加载态:居中的 [`busy_line`]。要撑满容器的调用方自己 `.size_full()`。
pub fn loading_state(text: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .py(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .child(busy_line(text))
}

/// 列表区里顶替空态的错误原文:错误色、左对齐、能折行(原文常带路径,居中折行难读)。
pub fn error_state(text: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .flex()
        .items_start()
        .gap(px(8.0))
        .py(px(16.0))
        .px(px(12.0))
        .text_size(ui::font_px(12.0))
        .text_color(ui::color_error())
        .child(div().flex_none().pt(px(1.0)).child(icon(
            ICON_ALERT,
            ICON_SIZE_SM,
            ui::color_error(),
        )))
        .child(div().flex_1().min_w_0().child(text.into()))
}

// ─── 列表行 ───────────────────────────────────────────────────

/// 单行列表项的行高(更改列表、分支弹窗、提交详情 / 提交 diff 的文件列表、推送提交)。
pub const ROW_H: f32 = 28.0;

/// 选中行的左侧 2px accent 竖条(行要 `relative()`;[`list_row`] 已带)。
pub fn selection_bar() -> Div {
    div()
        .absolute()
        .left_0()
        .top(px(6.0))
        .bottom(px(6.0))
        .w(px(2.0))
        .rounded(px(1.0))
        .bg(ui::accent())
}

/// 列表行的外壳:28px、左右 8、圆角 4、手型;选中 = `accent_subtle` 底 + 左侧竖条,
/// 未选中悬停薄纱底。行内容、点击、tooltip 由调用方挂(**别再设 hover**:gpui 一个
/// 元素只许设一次)。行高不同的(提交历史 32、单文件历史两行)调用方自己 `.h(..)`。
pub fn list_row(id: impl Into<ElementId>, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .w_full()
        .h(px(ROW_H))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.0))
        .px(px(8.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .when(selected, |el| {
            el.bg(ui::accent_subtle()).child(selection_bar())
        })
        .when(!selected, |el| el.hover(|el| el.bg(hover_bg())))
}

// ─── 文件行 ───────────────────────────────────────────────────

/// 仓库内路径 → `(所在目录, 文件名)`;根目录下的文件目录为空。
pub fn split_path(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((dir, name)) => (dir, name),
        None => ("", path),
    }
}

/// 状态字母(`M` / `A` / …):等宽、加粗、状态色,定宽右对齐 —— 竖着一列对得齐。
pub fn status_letter(letter: impl Into<SharedString>, color: Hsla) -> Div {
    mono(div())
        .flex_none()
        .w(px(14.0))
        .text_right()
        .text_size(ui::font_px(12.0))
        .font_weight(FontWeight::BOLD)
        .text_color(color)
        .child(letter.into())
}

/// 文件行的主体:FileIcon + 文件名(状态色,删除加删除线)+ 弱化的目录。
/// `show_dir = false` 时不写目录(树形视图里目录已经是上面那几行)。
/// 文件名先占位、目录吃剩下的,窄下去时目录先截断。
pub fn file_label(path: &str, color: Hsla, deleted: bool, show_dir: bool) -> Div {
    let (dir, name) = split_path(path);
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(FileIcon::new(name, false, false).size(px(ICON_SIZE)))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(13.0))
                .text_color(color)
                .when(deleted, |el| el.line_through())
                .child(name.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .when(show_dir, |el| el.child(dir.to_string())),
        )
}

// 提交里的一行改动文件(提交详情、提交 diff 左栏)见 [`commit_file_row_at`]:平铺与
// 树形两种画法,与目录树放在一起(`change_tree` 子模块)。

// ─── 状态 → 字母 / 颜色 ───────────────────────────────────────

/// 状态 → 单字符(`statusLabelFor`,`GitChanges.tsx:60-70`)。认不出的是**空格**。
pub fn status_label_for(status: Option<&GitStatus>) -> &'static str {
    match status {
        Some(GitStatus::Modified) => "M",
        Some(GitStatus::Added) => "A",
        Some(GitStatus::Deleted) => "D",
        Some(GitStatus::Renamed) => "R",
        Some(GitStatus::Untracked) => "?",
        Some(GitStatus::Conflicted) => "C",
        None => " ",
    }
}

/// 状态 → 颜色。**全部 Git 界面的唯一来源**(M 橙 / A、? 绿 / D 红 / R 蓝 / C 醒目)。
///
/// 冲突原版落到 default 的 muted —— 冲突恰恰是最需要人处理的状态,改成
/// `color_attention`(与「等你处理」那一档灯同色)。
///
/// 这些颜色都是当**字色**用的(文件名、状态字母):浅色主题下按 [`text_fg`] 压到对比度
/// 够 AA,暗色主题原样。
pub fn status_color(status: Option<&GitStatus>) -> Hsla {
    let color = match status {
        Some(GitStatus::Modified) => ui::color_warning(),
        Some(GitStatus::Added) => ui::color_success(),
        Some(GitStatus::Deleted) => ui::color_error(),
        Some(GitStatus::Renamed) => ui::color_info(),
        Some(GitStatus::Untracked) => ui::color_success(),
        Some(GitStatus::Conflicted) => ui::color_attention(),
        None => return ui::text_muted(),
    };
    text_fg(color)
}

/// 单字符状态 → 颜色(diff 弹窗标题栏的状态胶囊只拿得到字母)。
pub fn status_color_for_label(label: &str) -> Hsla {
    let status = match label.trim() {
        "M" => Some(GitStatus::Modified),
        "A" => Some(GitStatus::Added),
        "D" => Some(GitStatus::Deleted),
        "R" => Some(GitStatus::Renamed),
        "?" => Some(GitStatus::Untracked),
        "C" => Some(GitStatus::Conflicted),
        _ => None,
    };
    status_color(status.as_ref())
}

/// `CommitFileInfo.status`(`added` / `modified` / …)→ 状态字母与颜色。
/// 查不到(conflicted / untracked 之类)回落 `?` + muted。
pub fn commit_file_status(status: &str) -> (&'static str, Hsla) {
    let (letter, status) = match status {
        "added" => ("A", Some(GitStatus::Added)),
        "deleted" => ("D", Some(GitStatus::Deleted)),
        "renamed" => ("R", Some(GitStatus::Renamed)),
        "modified" => ("M", Some(GitStatus::Modified)),
        _ => ("?", None),
    };
    (letter, status_color(status.as_ref()))
}

// ─── 头像 ─────────────────────────────────────────────────────

/// 头像色相的档数(色环等分,30° 一档)。
///
/// 早先是从八个语义色里挑,可那八个里 accent / ai / file / folder 有好几个本来就是
/// 相近的紫蓝,`Developer` 与 `Alice` 撞成一样的紫。改成色环十二等分:相邻两档差
/// 30°,一眼分得开。
const AVATAR_HUES: u32 = 12;

/// 作者 → 色相档(`0..AVATAR_HUES`)。FNV-1a 再过一遍 murmur3 的 fmix32 打散:
/// FNV 的低位对短名字分布很差(取模只看低位),不打散的话常见名字扎堆在几档里。
/// 不分大小写(`alice` 与 `Alice` 是同一个人)。
fn avatar_hue_index(key: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in key.trim().to_lowercase().bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h % AVATAR_HUES
}

/// 头像的主色:色相按作者取档,饱和度 / 亮度跟着主题的 info 色走(深浅主题各自合适)。
fn avatar_color(key: &str) -> Hsla {
    let base = ui::color_info();
    Hsla {
        h: avatar_hue_index(key) as f32 / AVATAR_HUES as f32,
        s: base.s.clamp(0.45, 0.75),
        l: base.l,
        a: 1.0,
    }
}

/// 头像底色:主色 22% 预混到面板底色上,**不透明**。
///
/// 不用半透明底:E2E 三路截图里,弹窗毛玻璃背板之下头像圆周冒出白色弧线;预混成
/// 实色后圆周的弧线不再出现。⚠️ 偶尔仍能在头像字形 / 彩色 emoji 上截到零星白点,
/// 与底色透明与否无关 —— 都是全分辨率的锐利像素,不可能来自 1/4 降采样再模糊的
/// 背板,疑在 PrintWindow 截 DirectComposition 表面那条链路,待真机截屏确认。
fn avatar_bg(color: Hsla) -> Hsla {
    opaque(ui::bg_surface().blend(ui::with_alpha(color, 0.22)))
}

/// 作者名 → 头像上的字(首个字母 / 汉字,字母大写)。
pub fn avatar_initial(name: &str) -> String {
    name.trim()
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".to_string())
}

/// 16px 圆形首字母头像:按作者取的色相淡底(不透明,见 [`avatar_bg`])+ 同色字。
pub fn avatar(name: &str) -> Div {
    let color = avatar_color(name);
    div()
        .flex_none()
        .size(px(16.0))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(avatar_bg(color))
        .text_color(soft_fg(color))
        .text_size(ui::font_px(9.0))
        .font_weight(FontWeight::SEMIBOLD)
        .child(avatar_initial(name))
}

// ─── 弹窗 ─────────────────────────────────────────────────────

/// 确认类小弹窗(危险确认、删除 worktree)。
pub const DIALOG_W_SM: f32 = 420.0;
/// 表单类弹窗(新建 / 重命名分支、新建标签)。
pub const DIALOG_W_MD: f32 = 460.0;
/// 内容多的弹窗(推送、失败详情、修改消息、重置)。
pub const DIALOG_W_LG: f32 = 560.0;

/// **没有底部按钮**的弹窗(worktree 管理)的标题行:标题 + 右上角关闭图标按钮。
/// 带「取消」的弹窗不必用 —— 底部那颗就是出口(口径见 `prompt::dialog_title`)。
/// 关窗走 [`close_guarded`](自己摘覆盖物栈的登记)。
pub fn dialog_title(kind: &'static str, title: impl Into<SharedString>) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .child(div().flex_1().min_w_0().truncate().child(title.into()))
        .child(
            icon_button(
                SharedString::from(format!("{kind}-close")),
                ICON_CLOSE,
                t("prompt", "close"),
                true,
            )
            .on_click(move |_: &ClickEvent, window, cx| {
                close_guarded(kind, window, cx);
            }),
        )
}

/// 「取消 / 确认」页脚。按钮不直接调回调,而是派发 Dialog 自己的 `Cancel` /
/// `Confirm`,与 Esc / 回车走同一条路(见 `prompt::confirm_footer`)。
///
/// `danger` 时确认键用危险色(强推、Hard 重置、删除);`disabled` 时确认键禁用
/// (等待结果期间防重复提交,文案由调用方换成「执行中…」这类)。`cancel` 为 `None`
/// 时只有确认键。
pub fn dialog_footer(
    ok: impl Into<SharedString>,
    cancel: Option<SharedString>,
    danger: bool,
    disabled: bool,
) -> DialogFooter {
    let ok_button = footer_button("ok", ok)
        .disabled(disabled)
        .on_click(|_, window, cx| {
            window.dispatch_action(Box::new(DialogConfirm { secondary: false }), cx)
        });
    let ok_button = if danger {
        ok_button.danger()
    } else {
        ok_button.primary()
    };
    DialogFooter::new()
        .children(cancel.map(|cancel| {
            footer_button("cancel", cancel)
                .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx))
        }))
        .child(ok_button)
}

/// 弹窗正文的外壳。左右不另加内边距 —— Dialog 已给正文、标题、页脚同一份 16px,
/// 再加一层正文就比标题与页脚缩进一截。上下各垫 [`FOCUS_RING_ROOM`]:正文区是裁剪
/// 容器,排在首尾的输入框的聚焦环(画在边框外)贴边就被切掉一条。
pub fn form_body() -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .py(px(FOCUS_RING_ROOM))
}

/// 表单字段标签。
pub fn field_label(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(ui::font_px(12.0))
        .text_color(ui::text_secondary())
        .child(text.into())
}

/// 弱化的说明行。
pub fn hint_line(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(ui::font_px(12.0))
        .text_color(ui::text_muted())
        .child(text.into())
}

/// 带警告号的提示行(校验失败 = 错误色、需要留意 = 警告色)。
pub fn notice_line(text: impl Into<SharedString>, color: Hsla) -> Div {
    notice_row(text.into(), color)
}

/// 同 [`notice_line`],正文是现成的元素(要分段排版的说明,如分支弹窗的游离 HEAD 提示)。
pub fn notice_row(content: impl IntoElement, color: Hsla) -> Div {
    div()
        .flex()
        .items_start()
        .gap(px(8.0))
        .text_size(ui::font_px(12.0))
        .text_color(color)
        .child(
            div()
                .flex_none()
                .pt(px(1.0))
                .child(icon(ICON_ALERT, ICON_SIZE_SM, color)),
        )
        .child(div().flex_1().min_w_0().child(content))
}

/// git 失败原文的就地展示:红框淡底、可折行。要能选中复制的用 [`error_text_box`]。
pub fn error_block(text: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .px(px(10.0))
        .py(px(8.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(ui::with_alpha(ui::color_error(), 0.35))
        .bg(ui::with_alpha(ui::color_error(), 0.06))
        .text_size(ui::font_px(12.0))
        .text_color(ui::color_error())
        .child(text.into())
}

/// 复选框:勾上时 `color` 实心底 + 底色勾(危险选项传错误色)。
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

/// 复选框 + 文字的一行(整行可点,点击回调由调用方挂)。`danger` 的选项勾上后
/// 框与文字都转错误色(强推、强制删除)。
pub fn check_row(
    id: impl Into<ElementId>,
    checked: bool,
    label: impl Into<SharedString>,
    danger: bool,
) -> Stateful<Div> {
    let color = if danger {
        ui::color_error()
    } else {
        ui::accent()
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.0))
        .h(px(24.0))
        .cursor_pointer()
        .text_size(ui::font_px(12.0))
        .text_color(if danger && checked {
            ui::color_error()
        } else {
            ui::text_secondary()
        })
        .child(check_box(checked, color))
        .child(label.into())
}

/// 原文框最少几行高(一行的报错也留出第二行,不至于扁成一条)。
const ERROR_TEXT_MIN_ROWS: usize = 2;

/// 只读、可选中复制的多行原文框(git 的 stderr)。高度随内容(按折行后的行数)在
/// [`ERROR_TEXT_MIN_ROWS`] ~ `max_rows` 之间长,再多就在框里滚。
///
/// ⚠️ 必须是 `auto_grow`:`TextareaState::rows` 在普通多行模式下只是个记录值,
/// 布局时最小高度恒为一行(gpui-base `input/base/element.rs` 只有 AutoGrow 才按
/// 行数撑高),原文框于是只有一行多高,`fatal:` 那行要滚好几格才看得到。
pub fn error_text_state(
    text: &str,
    max_rows: usize,
    window: &mut Window,
    cx: &mut App,
) -> Entity<TextareaState> {
    let text = text.to_string();
    cx.new(|cx| {
        TextareaState::new(window, cx)
            .auto_grow(ERROR_TEXT_MIN_ROWS, max_rows.max(ERROR_TEXT_MIN_ROWS))
            .default_value(text)
    })
}

/// 原文框元素(与 [`error_block`] 同一套红框淡底):等宽字(git 的原文常按列对齐,
/// 路径、哈希也好认),可选中复制。
pub fn error_text_box(state: &Entity<TextareaState>) -> AnyElement {
    mono(div())
        .w_full()
        .rounded(px(4.0))
        .border_1()
        .border_color(ui::with_alpha(ui::color_error(), 0.35))
        .bg(ui::with_alpha(ui::color_error(), 0.06))
        .child(
            Textarea::new(state)
                .readonly(true)
                .bordered(false)
                .text_size(ui::font_px(12.0)),
        )
        .into_any_element()
}

/// 失败的可展示原因:错误链原文收掉首尾空白;空的(git 没吭声就非零退出)给句兜底。
/// Git 界面里唯一的一份(历史区、贮藏弹窗、更改区都用它)。
pub fn error_text(err: &anyhow::Error) -> String {
    let text = format!("{err:#}");
    match text.trim() {
        "" => t("gitBranch", "errorNoOutput").to_string(),
        trimmed => trimmed.to_string(),
    }
}

/// 写操作闸被占时的提示(「另一项 Git 操作正在进行」):面板上的入口推 toast,弹窗里的
/// 就地显示,按钮的禁用原因也是它。
pub fn gate_busy_text() -> &'static str {
    t("gitBranch", "gateBusy")
}

/// 写操作的公共骨架(见模块注释):占 `repo` 的写操作闸 → 后台执行 `op` → 放闸 → 回 UI
/// 线程把结果交给 `done`。闸被占时什么都不做、返回 `false`,由调用方就地提示。
///
/// 占闸与放闸后各请求一次整窗重画:别的区按 [`gate::is_busy`] 画的禁用态要跟着变。
/// 窗口在等待期间关掉的话 `done` 不再调,闸照样随任务结束放掉。
pub fn spawn_gated<T: Send + 'static>(
    repo: &str,
    op: impl FnOnce(&Path) -> anyhow::Result<T> + Send + 'static,
    done: impl FnOnce(anyhow::Result<T>, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let Some(guard) = gate::try_acquire(repo) else {
        return false;
    };
    cx.refresh_windows();
    let path = PathBuf::from(repo);
    window
        .spawn(cx, async move |cx| {
            // ⚠️ 全是阻塞调用(git CLI 最多 120s),必须后台执行器
            let result = cx
                .background_executor()
                .spawn(async move { op(&path) })
                .await;
            drop(guard);
            let _ = cx.update(|window, cx| {
                cx.refresh_windows();
                done(result, window, cx);
            });
        })
        .detach();
    true
}

/// 弹窗表单的「执行中 / 上次失败」两态(历史区的新建分支 / 标签 / 改消息 / 重置、分支弹窗
/// 的名称弹窗、贮藏的小弹窗共用)。`E` 是失败原文的形态:就地红字是 `String`,可选中复制
/// 的原文框是 `Entity<TextareaState>`。
pub struct FormOp<E = String> {
    /// 确认后、结果回来之前:确认键禁用,防重复提交。
    pub busy: bool,
    /// 上一次执行失败的原因(就地显示)。
    pub error: Option<E>,
}

impl<E> Default for FormOp<E> {
    fn default() -> Self {
        Self {
            busy: false,
            error: None,
        }
    }
}

impl<E> FormOp<E> {
    /// 开始执行:已在执行时返回 `false`(防重复提交);否则置忙并清掉上次的错误。
    pub fn begin(&mut self) -> bool {
        if self.busy {
            return false;
        }
        self.busy = true;
        self.error = None;
        true
    }

    /// 执行结束(成功;失败用 [`Self::fail`])。
    pub fn finish(&mut self) {
        self.busy = false;
    }

    /// 失败(含没能开始,比如写操作闸被占):复位并记下原因。
    pub fn fail(&mut self, error: E) {
        self.busy = false;
        self.error = Some(error);
    }
}

/// git 操作失败的详情弹窗:标题 + 一句说明 + **可选中复制**的完整原文 +「复制」「知道了」。
///
/// 面板上的操作(Fetch / 拉取 / 检出 / 历史区的拣选 / 更改区的暂存…)失败走这里而不是
/// toast:toast 5s 就收,而 git 的报错常是多行(哪些文件挡住了检出、hook 的输出),
/// 得留给人读、能复制。弹窗里的操作失败就地红字,不走这里(除非等待期间弹窗被关了)。
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
            .title(title.clone())
            .w(px(DIALOG_W_LG))
            .overlay_closable(true)
            .footer(
                DialogFooter::new()
                    .child(footer_button("copy", t("gitBranch", "copy")).on_click(
                        move |_, _window, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                copy_text.clone(),
                            ));
                        },
                    ))
                    .child(
                        footer_button("ok", t("gitBranch", "gotIt"))
                            .primary()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(
                                    Box::new(DialogConfirm { secondary: false }),
                                    cx,
                                )
                            }),
                    ),
            )
            .child(
                form_body()
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
                    .child(error_text_box(&state)),
            )
            // 回车 / 「知道了」= 关窗
            .on_ok(|_, _, _| true)
    });
}

// ─── 结果 toast ───────────────────────────────────────────────

/// 结果 toast 的语气。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// 成功 / 信息(`i`)。
    Info,
    /// 需要留意 / 失败(冲突、本地已有同名分支没同步、暂存失败之类,`!`)。
    Attention,
}

/// 推一条带结果摘要的 toast(「已检出 feature/x」「拉取了 5 个提交」)。标题是当前项目名。
/// 抽屉不是弹窗,toast 层画在它之上看得见;弹窗里的结果别走这里(遮罩下看不见)。
pub fn toast(store: &Entity<AppStore>, message: String, tone: Tone, cx: &mut App) {
    let (project_id, project_name) = store
        .read(cx)
        .active_project()
        .map(|p| (p.id.clone(), p.name.clone()))
        .unwrap_or_else(|| (String::new(), "Git".to_string()));
    let kind = match tone {
        Tone::Info => ToastKind::Info,
        Tone::Attention => ToastKind::Error,
    };
    crate::toast::push_message(kind, project_id, project_name, message, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表单两态:执行中不许再开始;失败复位并记原因;再开始时清掉上次的原因。
    #[test]
    fn 表单执行态() {
        let mut op: FormOp = FormOp::default();
        assert!(!op.busy && op.error.is_none());
        assert!(op.begin());
        assert!(op.busy);
        assert!(!op.begin(), "执行中不许重复提交");
        op.fail("hook 拒绝".into());
        assert!(!op.busy);
        assert_eq!(op.error.as_deref(), Some("hook 拒绝"));
        assert!(op.begin());
        assert!(op.error.is_none(), "重新开始时清掉上次的原因");
        op.finish();
        assert!(!op.busy && op.error.is_none());
    }

    /// 错误原文:收掉首尾空白,空的给兜底(不再写死英文 `git failed`)。
    #[test]
    fn 错误原文与兜底() {
        assert_eq!(error_text(&anyhow::anyhow!("  fatal: x\n")), "fatal: x");
        assert_eq!(
            error_text(&anyhow::anyhow!("  ")),
            t("gitBranch", "errorNoOutput")
        );
    }

    /// 亮底阈值:git_ui 与拓扑图共用一个判据。
    #[test]
    fn 亮底判据() {
        assert!(is_light(0.97));
        assert!(!is_light(0.08));
        assert!(!is_light(LIGHT_GROUND_L), "等于阈值不算亮");
    }

    /// E2E 里 Developer 与 Alice 同为紫色认不出来:三个演示作者两两不同档;
    /// 同一个人不分大小写、首尾空白同色;常见名字铺得开(20 个名字至少占 8 档)。
    #[test]
    fn 头像色相分得开() {
        let (dev, alice, bob) = (
            avatar_hue_index("Developer"),
            avatar_hue_index("Alice Chen"),
            avatar_hue_index("Bob Wang"),
        );
        assert!(dev != alice && dev != bob && alice != bob);
        assert_eq!(
            avatar_hue_index(" Alice Chen "),
            avatar_hue_index("alice chen")
        );
        let names = [
            "Developer",
            "Alice Chen",
            "Bob Wang",
            "Carol",
            "Dave",
            "Eve",
            "Mallory",
            "Trent",
            "张三",
            "李四",
            "王五",
            "赵六",
            "Linus Torvalds",
            "dependabot[bot]",
            "github-actions[bot]",
            "renovate[bot]",
            "John Smith",
            "Jane Doe",
            "Mike",
            "Lucy",
        ];
        let used: std::collections::HashSet<u32> =
            names.iter().map(|n| avatar_hue_index(n)).collect();
        assert!(used.len() >= 8, "只占了 {} 档", used.len());
        assert!(used.iter().all(|h| *h < AVATAR_HUES));
    }

    /// 头像底色预混成实色(半透明小圆在毛玻璃背板下出白弧,见 `avatar_bg`),深浅主题都是。
    #[test]
    fn 头像底色不透明() {
        for palette in [ui::Palette::dark(), ui::Palette::light()] {
            ui::set_palette(palette);
            for name in ["Developer", "Alice Chen", "张三"] {
                assert_eq!(avatar_bg(avatar_color(name)).a, 1.0);
            }
        }
        ui::set_palette(ui::Palette::dark());
    }

    /// 浅色主题下胶囊字(语义色写在自己 15% 淡底上)对比度够 AA:attention / warning
    /// 这类亮色固定压 0.8 倍不够(「2 个冲突」胶囊读不清),要按对比度压到够为止。
    #[test]
    fn 浅色主题胶囊字对比度够() {
        ui::set_palette(ui::Palette::light());
        for color in [
            ui::color_attention(),
            ui::color_warning(),
            ui::color_success(),
            ui::color_info(),
            ui::color_error(),
            ui::accent(),
        ] {
            let ground = opaque(ui::bg_surface().blend(soft_bg(color)));
            let ratio = contrast_ratio(soft_fg(color), ground);
            assert!(ratio >= SOFT_FG_MIN_CONTRAST - 1e-3, "{color:?}: {ratio}");
            assert_eq!(soft_fg(color).h, color.h, "色相不变");
        }
        ui::set_palette(ui::Palette::dark());
    }

    /// 染过色的条带上的胶囊(进行中操作横幅 = 面板底 + 12% attention,「2 个冲突」按面板底
    /// 算字色时只有 4.32):按条带的实际底算,浅色主题下仍够 AA。直接放在面板底上的
    /// 胶囊不受影响 —— 底给面板底时与 [`soft_fg`] 逐位相同。
    #[test]
    fn 染色底上的胶囊字对比度够() {
        ui::set_palette(ui::Palette::light());
        for color in [
            ui::color_attention(),
            ui::color_warning(),
            ui::color_success(),
            ui::color_info(),
            ui::color_error(),
            ui::accent(),
        ] {
            let banner = ground_with(ui::with_alpha(color, 0.12));
            assert!(banner.l < ui::bg_surface().l, "染过的底更深");
            let fg = soft_fg_on(color, banner);
            let ratio = contrast_ratio(fg, opaque(banner.blend(soft_bg(color))));
            assert!(ratio >= SOFT_FG_MIN_CONTRAST - 1e-3, "{color:?}: {ratio}");
            assert_eq!(fg.h, color.h, "色相不变");
            assert_eq!(soft_fg_on(color, ui::bg_surface()), soft_fg(color));
        }
        // 暗色主题:底不亮,字色原样(与 `soft_fg` 同一口径)
        ui::set_palette(ui::Palette::dark());
        let c = ui::color_attention();
        assert_eq!(soft_fg_on(c, ground_with(ui::with_alpha(c, 0.12))), c);
    }

    /// 浅色主题下状态色当字色(冲突文件名、冲突组标题、M / A 文件名)直接写在面板底上,
    /// 对比度够 AA(E2E r4 C10:#B08620 on #F5F5F5 只有 3.07);色相不变。暗色主题原样
    /// —— 不能被改坏。
    #[test]
    fn 浅色主题状态字色对比度够() {
        use GitStatus::*;
        ui::set_palette(ui::Palette::light());
        let ground = ui::bg_surface();
        let raw = ui::color_attention();
        assert!(
            contrast_ratio(raw, ground) < SOFT_FG_MIN_CONTRAST,
            "前提:原色不够"
        );
        for status in [Modified, Added, Deleted, Renamed, Untracked, Conflicted] {
            let fg = status_color(Some(&status));
            let ratio = contrast_ratio(fg, ground);
            assert!(ratio >= SOFT_FG_MIN_CONTRAST - 1e-3, "{status:?}: {ratio}");
        }
        let conflict = status_color(Some(&Conflicted));
        assert_eq!(conflict, text_fg(raw), "冲突组标题与冲突文件名同色");
        assert_eq!(conflict.h, raw.h, "色相不变");
        // 原本就够的颜色不动(D 红 #C0392B 约 4.99)
        assert_eq!(text_fg(ui::color_error()), ui::color_error());

        ui::set_palette(ui::Palette::dark());
        for color in [
            ui::color_attention(),
            ui::color_warning(),
            ui::color_success(),
        ] {
            assert_eq!(text_fg(color), color, "暗色主题原样");
        }
    }

    /// 对比度的两个端点:同色 1:1,黑白 21:1。
    #[test]
    fn 对比度端点() {
        let black = gpui::hsla(0.0, 0.0, 0.0, 1.0);
        let white = gpui::hsla(0.0, 0.0, 1.0, 1.0);
        assert!((contrast_ratio(black, black) - 1.0).abs() < 1e-3);
        assert!((contrast_ratio(black, white) - 21.0).abs() < 0.05);
    }

    #[test]
    fn 头像取首字() {
        assert_eq!(avatar_initial("alice"), "A");
        assert_eq!(avatar_initial("  张三"), "张");
        assert_eq!(avatar_initial("(bot)"), "B");
        assert_eq!(avatar_initial(""), "?");
    }

    #[test]
    fn 路径拆成目录与文件名() {
        assert_eq!(split_path("src/a/b.rs"), ("src/a", "b.rs"));
        assert_eq!(split_path("README.md"), ("", "README.md"));
    }

    /// commit 文件的状态字母表:四种已知 + 回落 `?`;颜色与更改列表同源。
    #[test]
    fn commit_文件状态字母与颜色() {
        ui::set_palette(ui::Palette::dark());
        assert_eq!(commit_file_status("added").0, "A");
        assert_eq!(commit_file_status("modified").0, "M");
        assert_eq!(commit_file_status("deleted").0, "D");
        assert_eq!(commit_file_status("renamed").0, "R");
        assert_eq!(commit_file_status("conflicted").0, "?");
        assert_eq!(commit_file_status("").1, ui::text_muted());
        assert_eq!(
            commit_file_status("modified").1,
            status_color(Some(&GitStatus::Modified))
        );
        assert_eq!(commit_file_status("deleted").1, status_color_for_label("D"));
    }

    /// 六种状态 → 单字符。认不出(None)的是**空格**,不是空串;字母 → 颜色与
    /// 状态 → 颜色同一张表。
    #[test]
    fn 状态字母表() {
        use GitStatus::*;
        ui::set_palette(ui::Palette::dark());
        let table = [
            (Modified, "M"),
            (Added, "A"),
            (Deleted, "D"),
            (Renamed, "R"),
            (Untracked, "?"),
            (Conflicted, "C"),
        ];
        for (status, label) in table {
            assert_eq!(status_label_for(Some(&status)), label);
            assert_eq!(status_color_for_label(label), status_color(Some(&status)));
        }
        assert_eq!(status_label_for(None), " ");
    }

    /// 状态色:冲突是最需要人处理的状态,走 attention 色(原版落到 muted)。
    #[test]
    fn 状态色映射() {
        use GitStatus::*;
        ui::set_palette(ui::Palette::dark());
        assert_eq!(status_color(Some(&Modified)), ui::color_warning());
        assert_eq!(status_color(Some(&Added)), ui::color_success());
        assert_eq!(status_color(Some(&Deleted)), ui::color_error());
        assert_eq!(status_color(Some(&Renamed)), ui::color_info());
        assert_eq!(status_color(Some(&Untracked)), ui::color_success());
        assert_eq!(status_color(Some(&Conflicted)), ui::color_attention());
        assert_eq!(status_color(None), ui::text_muted());
        assert_eq!(status_color_for_label(" "), ui::text_muted());
    }

    /// 语义色的字在浅底上压暗、在暗底上原样;淡底恒为 15%。
    #[test]
    fn 语义色随底色明暗调整() {
        ui::set_palette(ui::Palette::dark());
        let c = ui::color_warning();
        assert!(!is_light(ui::bg_surface().l));
        assert_eq!(soft_fg(c), c);
        assert!((soft_bg(c).a - 0.15).abs() < 1e-6);

        ui::set_palette(ui::Palette::light());
        let c = ui::color_warning();
        assert!(is_light(ui::bg_surface().l));
        assert!(soft_fg(c).l < c.l, "浅色主题下胶囊字要压暗");
        assert_eq!(soft_fg(c).h, c.h, "色相不变");
        ui::set_palette(ui::Palette::dark());
    }

    /// 引用胶囊的语义配色与规格表一致。
    #[test]
    fn 引用胶囊配色() {
        ui::set_palette(ui::Palette::dark());
        assert_eq!(RefKind::Head.color(), ui::accent());
        assert_eq!(RefKind::Local.color(), ui::color_success());
        assert_eq!(RefKind::Remote.color(), ui::color_info());
        assert_eq!(RefKind::Tag.color(), ui::color_warning());
    }
}
