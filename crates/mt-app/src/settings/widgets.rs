//! 设置面板的小件:页/节骨架、设置行原语,以及各分页共用的零碎视图件。
//!
//! 都是**无状态纯视图**(唯一的例外是 [`toggle_row`],它要 `cx.listener`
//! 才能把点击回调挂到 [`SettingsView`] 上)。通用原语 Toggle / SettingRow /
//! ChoiceGroup / 滑块 / 键帽自绘在 [`crate::ui`],这里只放「只有设置面板用」
//! 的那一批。
//!
//! 开关滑块的走位补间([`knob_toggle`] / [`flip_knob`])住在本模块的 `thread_local`:
//! 开关是无状态构建函数、调用点散在十几处,补间表跟着视图走就得给每处多传一个参数。
//! 只让**刚翻的那一颗**动(`fold_motion::FlipTweens`),设置面板打开时一颗都不播。

use std::cell::RefCell;

use gpui::{
    Context, Entity, Hsla, InteractiveElement, IntoElement, ParentElement, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::input::{Input, InputState};

use crate::i18n::t;
use crate::ui;

use super::SettingsView;

thread_local! {
    /// 开关滑块的补间表(见模块注释)。开关 id 全是静态字面量,设置面板同一时刻只开
    /// 一个,按 id 索引不会串。
    static KNOBS: RefCell<crate::fold_motion::FlipTweens> =
        RefCell::new(crate::fold_motion::FlipTweens::new(mt_ui::motion::TOGGLE_KNOB));
}

/// 一颗会动的开关:静止时就是 `ui::toggle(id, checked)`,刚被 [`flip_knob`] 翻过的那颗
/// 滑块横向走位、底色同步过渡([`mt_ui::motion::TOGGLE_KNOB`])。点击回调由调用方挂。
pub(super) fn knob_toggle(id: &'static str, checked: bool) -> Stateful<gpui::Div> {
    let travel = KNOBS.with(|knobs| knobs.borrow().value_open(id, checked, 1.0));
    ui::toggle_at(id, travel)
}

/// 开关 `id` 刚被翻成 `checked`(点击回调里、改状态之前或之后都行)。
pub(super) fn flip_knob(id: &str, checked: bool) {
    KNOBS.with(|knobs| knobs.borrow_mut().flip_open(id, checked, 1.0));
}

/// 还有滑块在走就要下一帧(设置视图的 render 里调)。
pub(super) fn drive_knobs(window: &Window) {
    KNOBS.with(|knobs| knobs.borrow().drive(window));
}

/// 清空残留的开关滑块补间(打开/关闭设置面板时调,避免快速关开继承上一次的残留)。
pub(super) fn clear_knobs() {
    KNOBS.with(|knobs| knobs.borrow_mut().clear());
}

/// 页根节点:`space-y-6`。
pub(super) fn page_root() -> gpui::Div {
    div().flex().flex_col().gap(px(24.0))
}

/// 分节:标题 + `space-y-2` 的内容。
pub(super) fn section(title_key: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(ui::settings_section_title(t("settings", title_key)))
}

/// 一行开关。`disabled` 时置灰**并且不挂点击**(gpui 没有 pointer-events)。
pub(super) fn toggle_row(
    id: &'static str,
    title_key: &'static str,
    desc_key: &'static str,
    checked: bool,
    disabled: bool,
    on_toggle: impl Fn(&mut SettingsView, bool, &mut Window, &mut Context<SettingsView>) + 'static,
    cx: &mut Context<SettingsView>,
) -> gpui::Div {
    let control = knob_toggle(id, checked).when(!disabled, |el| {
        el.on_click(cx.listener(move |this, _, window, cx| {
            flip_knob(id, !checked);
            on_toggle(this, !checked, window, cx);
        }))
    });
    ui::setting_row(
        t("settings", title_key),
        Some(ui::desc_text(t("settings", desc_key)).into_any_element()),
        disabled,
        control,
    )
}

/// 一行数字输入(草稿态)。宽度 `w-24`、等宽右对齐,与原版一致。
pub(super) fn number_row(
    title_key: &'static str,
    desc_key: &'static str,
    input: &Entity<InputState>,
    disabled: bool,
) -> gpui::Div {
    ui::setting_row(
        t("settings", title_key),
        Some(ui::desc_text(t("settings", desc_key)).into_any_element()),
        disabled,
        div().w(px(96.0)).child(Input::new(input)),
    )
}

// ─── 小件 ─────────────────────────────────────────────────────

/// 默认项的单选圆点(shell / 编辑器列表共用)。`w-3 h-3 rounded-full border-2`。
pub(super) fn radio_dot(id: String, selected: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .w(px(12.0))
        .h(px(12.0))
        .flex_none()
        .rounded_full()
        .border_2()
        .cursor_pointer()
        .border_color(if selected {
            ui::accent()
        } else {
            ui::border_strong()
        })
        .when(selected, |el| el.bg(ui::accent()))
}

/// 「+ 添加…」的虚线按钮。gpui 没有虚线边框,用 accent 淡底 + 实线描边近似。
pub(super) fn dashed_button(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .w_full()
        .flex()
        .justify_center()
        .py(px(10.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(ui::border_default())
        .cursor_pointer()
        .text_size(ui::font_px(13.0))
        .text_color(ui::text_muted())
        .hover(|el| el.border_color(ui::accent()).text_color(ui::accent()))
        .child(label)
}

/// 行内编辑表单的外壳。新增态用 accent 描边(原版是 accent 虚线)。
pub(super) fn form_card(adding: bool) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(12.0))
        .rounded(px(6.0))
        .bg(ui::bg_base())
        .border_1()
        .border_color(if adding {
            ui::accent()
        } else {
            ui::border_default()
        })
}

/// 字体输入框:上标签下整宽输入框(原版 `FontFamilyInput`)。
pub(super) fn font_family_input(label: &'static str, input: &Entity<InputState>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_primary())
                .child(label),
        )
        .child(Input::new(input))
}

/// 单选段容器(`flex gap-2`)。
pub(super) fn choice_group() -> gpui::Div {
    div().flex().gap(px(8.0))
}

/// 一条带色描边的提示条(错误 / 成功 / 警告共用)。
pub(super) fn banner(text: String, color: Hsla) -> gpui::Div {
    div()
        .px(px(12.0))
        .py(px(8.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(color)
        .text_size(ui::font_px(11.0))
        .text_color(color)
        .children(
            text.split('\n')
                .map(|line| div().child(line.to_string()))
                .collect::<Vec<_>>(),
        )
}

/// 皮肤预览里的小横杠。
pub(super) fn mini_bar(width: f32, color: Hsla, alpha: f32) -> gpui::Div {
    div()
        .h(px(4.0))
        .w(px(width))
        .rounded_full()
        .bg(ui::with_alpha(color, alpha))
}

/// 配置片段里的文件名行(带 `(note)`)。
pub(super) fn snippet_file_name(file: &str, note: Option<&str>) -> gpui::Div {
    let text = match note {
        Some(note) if !note.is_empty() => format!("{file} ({note})"),
        _ => file.to_string(),
    };
    div()
        .mb(px(4.0))
        .text_color(ui::text_secondary())
        .child(text)
}

/// 配置片段正文。gpui 的文本不认 `\n`,拆成一行一个 child。
pub(super) fn snippet_lines(content: &str) -> Vec<gpui::Div> {
    content
        .split('\n')
        .map(|line| {
            div().child(if line.is_empty() {
                SharedString::from(" ")
            } else {
                SharedString::from(line.to_string())
            })
        })
        .collect()
}
