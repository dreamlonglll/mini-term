//! 自绘日期选择浮层。
//!
//! # 为什么不用 `gpui_component::time::DatePicker`
//!
//! 与 [`crate::menu`] 拒绝 `PopupMenu` 是同一个坑,而且这次是全盲的:
//! `date_picker.rs:426` 的触发钮画 `Icon::new(IconName::Calendar)`、`calendar.rs`
//! 的翻月钮画 `IconName::ArrowLeft/ArrowRight`,三者都走 `AssetSource` 取 svg,
//! **0.5.1 的 crate 包里一个 svg 都没有**(上游把 lucide 放在示例程序的资产目录),
//! 宿主也没注册 `AssetSource` —— 渲染出来是三块空白,且编译期无感。
//! 图标改走 [`mt_ui::icons::vector`] 自绘,浮层壳照抄 `menu.rs` 那套。
//!
//! # 层级与定位
//!
//! ```text
//! deferred(priority 1)                  ← 画在所有常规内容之上
//!  └─ anchored(0,0)
//!      └─ 全窗口透明遮罩(occlude + on_mouse_down = 关闭)
//!          └─ anchored(触发钮下沿).snap_to_window_with_margin(4px)
//!              └─ 日历面板(occlude)
//! ```
//!
//! `deferred` 是**不吃祖先 ContentMask 的**(gpui 的 `DeferredDraw` 不保存
//! `content_mask_stack`),所以这件挂在抽屉式面板里也不会被抽屉裁掉 —— 宿主只要
//! 在自己的 render 里把它 `child` 出来即可,不必像 `menu.rs` 那样做成全局层。
//!
//! # 网格恒 6 行
//!
//! 42 格从「当月 1 号所在周的周日」起排,不足的前后补邻月日期。**行数固定**才不会
//! 出现「翻个月面板高度跳一下」——2 月正好 28 天且 1 号是周日时只要 4 行,不补
//! 就会缩掉两行高。邻月格照常可点(点了就跳到那个月),与浏览器原生日历同。

use chrono::{Datelike, NaiveDate};
use gpui::{
    AnyElement, App, Context, EventEmitter, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Point, Render,
    StatefulInteractiveElement, Styled, Window, anchored, deferred, div, point,
    prelude::FluentBuilder, px,
};
use mt_ui::icons::vector::{Geom, Ink, Shape, VectorIcon};

use crate::i18n::t;
use crate::overlay;
use crate::ui;

// ─── 图标(0..1 单位方框,与 `mt_ui::icons::vector` 同约定)────────

/// 描边宽度:16px 上看约 1.3px,与 `usage_panel` 的 KPI 图标同一档。
const STROKE: f32 = 0.085;

/// 日历(输入框旁的触发钮)。
pub const ICON_CALENDAR: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Rect {
            x: 0.14,
            y: 0.2,
            w: 0.72,
            h: 0.66,
            round: 0.1,
        },
    ),
    // 上沿横线:把「月份标题条」与日期格分开,一眼看出是日历不是相框
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(0.14, 0.4), (0.86, 0.4)]),
    ),
    // 两根挂钩
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(0.34, 0.12), (0.34, 0.28)]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(0.66, 0.12), (0.66, 0.28)]),
    ),
];

/// 上一月。
const ICON_CHEVRON_LEFT: &[Shape] = &[Shape::line(
    Ink::Current,
    0.11,
    Geom::Polyline(&[(0.62, 0.24), (0.38, 0.5), (0.62, 0.76)]),
)];

/// 下一月。
const ICON_CHEVRON_RIGHT: &[Shape] = &[Shape::line(
    Ink::Current,
    0.11,
    Geom::Polyline(&[(0.38, 0.24), (0.62, 0.5), (0.38, 0.76)]),
)];

// ─── 纯逻辑(可测)────────────────────────────────────────────

/// 该日期所在月的 1 号。
pub fn month_start(date: NaiveDate) -> NaiveDate {
    date.with_day(1).unwrap_or(date)
}

/// 平移 `delta` 个月(结果恒为 1 号)。日历减法,跨年正确。
pub fn shift_month(month: NaiveDate, delta: i32) -> NaiveDate {
    let total = month.year() * 12 + month.month0() as i32 + delta;
    let year = total.div_euclid(12);
    let month0 = total.rem_euclid(12) as u32;
    NaiveDate::from_ymd_opt(year, month0 + 1, 1).unwrap_or(month)
}

/// 6×7 的日期网格,从当月 1 号所在周的**周日**起(见模块注释「网格恒 6 行」)。
pub fn month_grid(month: NaiveDate) -> Vec<NaiveDate> {
    let first = month_start(month);
    let lead = first.weekday().num_days_from_sunday() as i64;
    let start = first - chrono::Duration::days(lead);
    (0..42)
        .map(|i| start + chrono::Duration::days(i))
        .collect()
}

/// 月份标题。走 `YYYY-MM` 纯数字 —— 与日期输入框里的 `YYYY-MM-DD` 同源,
/// 用户一眼能对上,也省掉 12 个月份名进字典。
pub fn month_label(month: NaiveDate) -> String {
    format!("{:04}-{:02}", month.year(), month.month())
}

/// 星期表头(周日起,与 [`month_grid`] 同序)。
fn weekday_labels() -> [&'static str; 7] {
    [
        t("time", "weekday.sun"),
        t("time", "weekday.mon"),
        t("time", "weekday.tue"),
        t("time", "weekday.wed"),
        t("time", "weekday.thu"),
        t("time", "weekday.fri"),
        t("time", "weekday.sat"),
    ]
}

// ─── 浮层 ────────────────────────────────────────────────────

pub enum DatePickerEvent {
    /// 选了一天。宿主收到后自己关闭浮层(drop 掉实体)。
    Picked(NaiveDate),
    /// 点外 / Esc 关闭。
    Dismissed,
}

/// 版式常量。
const CELL: f32 = 30.0;
const CELL_H: f32 = 26.0;

pub struct DatePicker {
    /// 浮层锚点(窗口坐标),一般给触发钮的左下角。
    anchor: Point<Pixels>,
    /// 当前显示的月(恒为 1 号)。
    month: NaiveDate,
    selected: Option<NaiveDate>,
    today: NaiveDate,
    /// 可选范围(闭区间),越界的格子灰掉且点不动。
    min: Option<NaiveDate>,
    max: Option<NaiveDate>,
    /// 打开浮层前的焦点,关闭时还回去(顺序纪律同 `menu.rs`:先还焦点再跑动作)。
    prev_focus: Option<FocusHandle>,
    focus: FocusHandle,
    /// 进场(与右键菜单同一条 `menuPopIn`,浮层豁免「减少动画」)。
    pop_in: mt_ui::motion::Transition,
}

impl EventEmitter<DatePickerEvent> for DatePicker {}

impl DatePicker {
    /// `selected` 给当前输入框里的值(没有/非法就传 `None`,面板落在 `today` 那个月)。
    ///
    /// ⚠️ **宿主换浮层时必须先把旧实体清掉再建新的**(`self.picker = None;` 单独一行)。
    /// 直接 `self.picker = Some(cx.new(..))` 是「先建后 drop」:新的 push 撞上还没摘的
    /// 旧登记会被防叠开挡掉,紧接着旧的 drop 又把栈里那条摘了 —— 浮层开着但栈是空的,
    /// 全局快捷键不再让路。
    pub fn new(
        anchor: Point<Pixels>,
        selected: Option<NaiveDate>,
        today: NaiveDate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        overlay::push(overlay::key(overlay::kind::DATE_PICKER));
        let prev_focus = window.focused(cx);
        let focus = cx.focus_handle();
        window.focus(&focus);
        Self {
            anchor,
            month: month_start(selected.unwrap_or(today)),
            selected,
            today,
            min: None,
            max: None,
            prev_focus,
            focus,
            pop_in: mt_ui::motion::Transition::new(mt_ui::motion::MENU_IN),
        }
    }

    /// 可选范围(闭区间)。
    pub fn range(mut self, min: Option<NaiveDate>, max: Option<NaiveDate>) -> Self {
        self.min = min;
        self.max = max;
        self
    }

    fn in_range(&self, date: NaiveDate) -> bool {
        self.min.is_none_or(|m| date >= m) && self.max.is_none_or(|m| date <= m)
    }

    /// 还焦点。三条关闭路(点外 / Esc / 选中)共用 —— 与 `menu.rs` 同一条纪律:
    /// **先还焦点再发事件**,宿主收到事件后可能立刻聚焦别的输入框,反过来会被抢。
    fn restore_focus(&mut self, window: &mut Window) {
        if let Some(prev) = self.prev_focus.take() {
            window.focus(&prev);
        }
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.restore_focus(window);
        cx.emit(DatePickerEvent::Dismissed);
    }

    fn pick(&mut self, date: NaiveDate, window: &mut Window, cx: &mut Context<Self>) {
        if !self.in_range(date) {
            return;
        }
        self.restore_focus(window);
        cx.emit(DatePickerEvent::Picked(date));
    }

    fn step_month(&mut self, delta: i32, cx: &mut Context<Self>) {
        self.month = shift_month(self.month, delta);
        cx.notify();
    }

    /// 翻月钮。
    fn nav_button(
        &self,
        id: &'static str,
        icon: &'static [Shape],
        tip: &'static str,
        delta: i32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .w(px(22.0))
            .h(px(22.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .text_color(ui::text_muted())
            .hover(|el| el.bg(ui::border_subtle()).text_color(ui::text_primary()))
            .tooltip(move |window, cx| {
                mt_ui::tooltip::Tooltip::new(tip).build(window, cx)
            })
            .on_click(cx.listener(move |this: &mut Self, _, _window, cx| {
                this.step_month(delta, cx);
            }))
            .child(VectorIcon::new(icon, px(12.0)).ink(ui::text_muted()))
            .into_any_element()
    }

    fn render_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let (opacity, dy) = mt_ui::motion::menu_pop_in(self.pop_in.drive(window));
        let month = self.month;

        // 星期表头
        let mut head = div().flex();
        for label in weekday_labels() {
            head = head.child(
                div()
                    .w(px(CELL))
                    .flex()
                    .items_center()
                    .justify_center()
                    .py(px(4.0))
                    .text_size(ui::font_px(10.0))
                    .text_color(ui::text_muted())
                    .child(label),
            );
        }

        // 6 行日期
        let mut grid = div().flex().flex_col();
        for (row, week) in month_grid(month).chunks(7).enumerate() {
            let mut line = div().flex();
            for (col, date) in week.iter().enumerate() {
                let date = *date;
                let enabled = self.in_range(date);
                let current_month = date.month() == month.month() && date.year() == month.year();
                let selected = self.selected == Some(date);
                let is_today = date == self.today;
                let text_color = if selected {
                    ui::bg_base()
                } else if !current_month {
                    ui::text_muted()
                } else {
                    ui::text_secondary()
                };
                line = line.child(
                    div()
                        // 元组 id:逐格唯一且跨帧稳定,不必每帧拼 42 个字符串
                        .id(("cal-day", row * 7 + col))
                        .w(px(CELL))
                        .h(px(CELL_H))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .text_size(ui::font_px(12.0))
                        .text_color(text_color)
                        // 邻月的格子压暗,但**照样可点**(点了跳到那个月)
                        .when(!current_month && !selected, |el| el.opacity(0.55))
                        .when(selected, |el| el.bg(ui::accent()))
                        // 今天:描边而不是填色 —— 填色会与「选中」撞样式
                        .when(is_today && !selected, |el| {
                            el.border_1().border_color(ui::accent())
                        })
                        .when(!enabled, |el| el.opacity(0.3))
                        .when(enabled, |el| {
                            el.cursor_pointer()
                                .when(!selected, |el| el.hover(|el| el.bg(ui::border_subtle())))
                                .on_click(cx.listener(move |this: &mut Self, _, window, cx| {
                                    this.pick(date, window, cx);
                                }))
                        })
                        .child(date.day().to_string()),
                );
            }
            grid = grid.child(line);
        }

        div()
            .track_focus(&self.focus)
            .key_context("DatePicker")
            .on_key_down(cx.listener(|this: &mut Self, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key.as_str() == "escape" {
                    this.dismiss(window, cx);
                    cx.stop_propagation();
                }
            }))
            .opacity(opacity)
            .mt(px(dy))
            .flex()
            .flex_col()
            .p(px(6.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(ui::border_default())
            .bg(ui::bg_overlay())
            .shadow_lg()
            // 面板内的按下不算「点外」(同 `menu.rs`)
            .occlude()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(2.0))
                    .pb(px(4.0))
                    .child(self.nav_button(
                        "cal-prev",
                        ICON_CHEVRON_LEFT,
                        t("time", "prevMonth"),
                        -1,
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(ui::font_px(12.0))
                            .text_color(ui::text_primary())
                            .child(month_label(month)),
                    )
                    .child(self.nav_button(
                        "cal-next",
                        ICON_CHEVRON_RIGHT,
                        t("time", "nextMonth"),
                        1,
                        cx,
                    )),
            )
            .child(head)
            .child(grid)
            .into_any_element()
    }
}

/// 实体被 drop(宿主关掉浮层)时摘掉登记 —— 三条关闭路都是「宿主收事件后 drop」,
/// 把摘栈放在这里就不会漏(放 `dismiss` 里的话,宿主直接丢弃实体那条路会泄漏)。
impl Drop for DatePicker {
    fn drop(&mut self) {
        overlay::pop(overlay::key(overlay::kind::DATE_PICKER));
    }
}

impl Render for DatePicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let size = window.viewport_size();
        let anchor = self.anchor;
        let panel = self.render_panel(window, cx);

        div().child(
            deferred(
                anchored().position(point(px(0.0), px(0.0))).child(
                    div()
                        .w(size.width)
                        .h(size.height)
                        // 点浮层外任意处关闭。occlude 让这层吃掉这一下 ——
                        // 否则关闭那一下会顺手点到底下的东西(同 `menu.rs`)
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this: &mut Self, _: &MouseDownEvent, window, cx| {
                                this.dismiss(window, cx);
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(|this: &mut Self, _: &MouseDownEvent, window, cx| {
                                this.dismiss(window, cx);
                            }),
                        )
                        .child(
                            anchored()
                                .position(anchor)
                                .snap_to_window_with_margin(px(4.0))
                                .child(panel),
                        ),
                ),
            )
            .with_priority(1),
        )
    }
}

/// 触发钮:输入框右侧那颗日历图标。回调拿到的是**浮层锚点**(窗口坐标)。
///
/// 锚点取点击位置往下挪一点,与 `menu::show` 弹在鼠标点是同一套路 ——
/// gpui 的 `ClickEvent` 给不到元素 bounds(键盘触发那支才有),想贴着钮的下沿
/// 展开就得再插一个 canvas 去量,为这点差别不值得。
pub fn trigger_button(
    id: &'static str,
    tip: &'static str,
    on_open: impl Fn(Point<Pixels>, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(24.0))
        .h(px(24.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .text_color(ui::text_muted())
        .hover(|el| el.bg(ui::border_subtle()).text_color(ui::text_primary()))
        .tooltip(move |window, cx| mt_ui::tooltip::Tooltip::new(tip).build(window, cx))
        .on_click(move |event: &gpui::ClickEvent, window, cx| {
            let at = event.position();
            on_open(point(at.x - px(24.0), at.y + px(14.0)), window, cx);
        })
        .child(VectorIcon::new(ICON_CALENDAR, px(14.0)).ink(ui::text_muted()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn 月份平移跨年正确() {
        assert_eq!(shift_month(d(2026, 8, 20), 0), d(2026, 8, 1));
        assert_eq!(shift_month(d(2026, 8, 20), 1), d(2026, 9, 1));
        assert_eq!(shift_month(d(2026, 12, 5), 1), d(2027, 1, 1));
        assert_eq!(shift_month(d(2026, 1, 5), -1), d(2025, 12, 1));
        assert_eq!(shift_month(d(2026, 3, 31), -1), d(2026, 2, 1), "月末回退不越界");
        // 连续平移 24 个月正好回到同月
        let mut m = d(2026, 8, 1);
        for _ in 0..24 {
            m = shift_month(m, 1);
        }
        assert_eq!(m, d(2028, 8, 1));
    }

    #[test]
    fn 网格恒六行且从周日起() {
        for (y, m) in [(2026, 2), (2026, 8), (2027, 1), (2024, 2)] {
            let grid = month_grid(d(y, m, 1));
            assert_eq!(grid.len(), 42, "{y}-{m} 行数变了");
            assert_eq!(
                grid[0].weekday().num_days_from_sunday(),
                0,
                "{y}-{m} 没从周日起"
            );
            // 连续、无重复
            for pair in grid.windows(2) {
                assert_eq!(pair[1], pair[0] + chrono::Duration::days(1));
            }
            // 当月每一天都在网格里
            let first = d(y, m, 1);
            assert!(grid.contains(&first));
            let last = shift_month(first, 1) - chrono::Duration::days(1);
            assert!(grid.contains(&last), "{y}-{m} 月末 {last} 不在网格里");
        }
    }

    /// 1 号恰好是周日时不许整周留白(否则首行全是上个月)。
    #[test]
    fn 一号是周日时首格就是一号() {
        // 2026-02-01 是周日
        let first = d(2026, 2, 1);
        assert_eq!(first.weekday().num_days_from_sunday(), 0);
        assert_eq!(month_grid(first)[0], first);
    }

    #[test]
    fn 月份标题与输入框同源() {
        assert_eq!(month_label(d(2026, 8, 20)), "2026-08");
        assert_eq!(month_label(d(2026, 12, 1)), "2026-12");
        assert_eq!(month_label(d(999, 1, 1)), "0999-01");
    }

    #[test]
    fn 取月初() {
        assert_eq!(month_start(d(2026, 8, 20)), d(2026, 8, 1));
        assert_eq!(month_start(d(2026, 8, 1)), d(2026, 8, 1));
    }
}
