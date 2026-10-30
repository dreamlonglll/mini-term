//! 全仓统一的 tooltip:比 gpui-component 默认款**字更小、要停得更久才弹**。
//!
//! # 字号:自绘气泡(仍然必要)
//!
//! gpui-component 的 `Tooltip::render` 把 `.text_sm()`(0.875rem)写在
//! `.refine_style(&self.style)` **之前** —— 0.6.2 起调用点确实能用 `.text_size()`
//! 盖过去,但那要求每个调用点都记得挂一次样式:全仓 50 多处,漏一处就花。
//! 于是这里把那段气泡样式整份抄过来改成 [`TOOLTIP_FONT_SIZE`],
//! **内容仍用它的 [`Text`]**,换行/富文本行为与上游逐字节一致。
//!
//! # 停留时长:交给上游的 `tooltip_show_delay`
//!
//! gpui-pre 0.3.5 把这个旋钮开成了公开面(`StatefulInteractiveElement::tooltip_show_delay`
//! / `Interactivity::tooltip_show_delay`)。生效路径是:写进
//! `Interactivity.tooltip_show_delay`(`Option<Duration>`)→ prepaint 里随
//! `register_tooltip_mouse_handlers` 传给鼠标处理器 → `None` 才回落上游的
//! `DEFAULT_TOOLTIP_SHOW_DELAY`(500ms)。计时本身是「鼠标进入排一个 timer,
//! 到点才 `show_tooltip` 建视图」,中途离开当场取消,期间移动**不重排**计时。
//!
//! 所以气泡自己不必再懂延迟:用 [`TooltipExt`] 的方法一次挂上两件事
//! (延迟 + 气泡闭包),别只挂裸的 `.tooltip()` —— 那是 500ms 档。
//!
//! 个别调用点就是要「快弹」(用量统计弹窗工具栏的纯图标按钮 —— 不弹提示
//! 就认不出是什么键),**故意**只挂裸 `.tooltip()` 回落到 500ms。
//! 默认仍是 [`SHOW_DELAY`],别顺手全仓铺开快弹。
//!
//! # 打断:点过 / 开了覆盖物之后不再弹
//!
//! gpui-pre 0.3.5 的计时有个洞(`div.rs` 的 `register_tooltip_mouse_handlers`):
//! 鼠标**移入**就排好延时任务(`WaitingForShow`),按下鼠标只清**已显示**的气泡
//! (`clear_active_tooltip_if_not_hoverable` 对 `WaitingForShow` 什么都不做);到点
//! `show_tooltip` 也不再问一次「还悬停着吗」,之后逐帧的可见判定只比**元素包围盒**、
//! 不看遮挡(上游自己在 `handle_tooltip_mouse_move` 的注释里记了这条 TODO)。
//! 于是「移上按钮 → 点开菜单 / 弹窗 → 鼠标不动」时,1.2s 后触发按钮的提示照样弹出来,
//! 而 tooltip 又画在所有 deferred 层之上 —— 盖住菜单第一项、弹窗标题、搜索框。
//!
//! 上游的状态机是私有的,不 fork 就只能在本模块能碰到的两处收口:
//!
//! - **打断时刻**([`interrupt`]):[`TooltipExt`] 给挂了提示的元素顺手挂一个
//!   capture 阶段的按下监听(任意键 —— 左键开菜单、右键开上下文菜单、双击开弹窗
//!   都算),应用层的覆盖物栈压栈时也调一次(异步回来才弹的失败详情框之类);
//! - **出气泡那一刻**:构造闭包是上游等满 [`SHOW_DELAY`] 才调的,所以「等待起点」
//!   不早于 `now - SHOW_DELAY`。这段时间里被打断过,就交一个空视图出去
//!   (上游照样记成 `Visible`,于是鼠标不移出元素就不会再排第二次);已经弹出来的
//!   气泡则逐帧自查,弹出之后被打断就画成空的(覆盖物由键盘以外的路径打开时,
//!   上游不会替我们收)。
//!
//! 键盘打开的覆盖物不用管:上游逐帧判定里有 `!window.last_input_was_keyboard()`。
//!
//! # 退役记档:曾经的「二段延迟」
//!
//! gpui 0.2.2 的 500ms 是 `elements/div.rs` 里的私有常量,当时只能在它后面
//! 再接一段:视图先渲染成空 div,等 700ms 的 `Task` 到点再画,靠
//! `cx.refresh_windows()` 唤醒重绘。两条代价随着换用上游 API 一并消失:
//!
//! - 每弹一次提示要**绕缓存全窗重绘一次**;
//! - 气泡锚点取的是 gpui 建视图那一刻(第 500ms)的鼠标位置,后面 700ms 里
//!   鼠标在同一元素内移动不会重新贴位。现在建视图发生在整个 [`SHOW_DELAY`]
//!   之后,`show_tooltip` 当场取 `window.mouse_position()`,锚点即最新位置。

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, AnyView, App, AppContext, Context, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, div, px, rems,
};
use gpui_component::{ActiveTheme, h_flex, text::Text};

/// 鼠标要在元素上停多久才弹提示。
///
/// **调这一个数就等于调全仓的停留时长**:它整个交给上游的
/// `tooltip_show_delay`,不再有「上游 500ms + 自己再等一段」的叠加。
/// 1200ms = 退役前那套二段延迟的总时长(gpui 的 500 + 自己的 700),手感不变。
pub const SHOW_DELAY: Duration = Duration::from_millis(1200);

/// 气泡字号。上游是 `text_sm`(0.875rem);这里降一档到 0.75rem。
///
/// 跟 `text_xs()` 同值,写成 `rems` 是为了「想再调时有个明确的旋钮」。
/// 用 rem 而非 px:随 `gpui_component` 主题的 `font_size`(= 窗口 rem 基准)
/// 缩放,与上游同一套相对关系。
const TOOLTIP_FONT_SIZE: f32 = 0.75;

thread_local! {
    /// 最近一次打断的时刻(见模块注释「打断」一节)。gpui 的视图全在主线程,
    /// 一份 `thread_local` 足够,单测也天然一线程一份。
    static LAST_INTERRUPT: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// 记一次打断:此刻还在等待中的提示不再弹出,已经弹出的收起。
///
/// [`TooltipExt`] 已在元素被按下时自动调;应用层在**打开覆盖物**(菜单 / 弹窗)时
/// 再调一次,接住「不是点出来的」那些(异步回来才弹的弹窗)。
pub fn interrupt() {
    LAST_INTERRUPT.with(|at| at.set(Some(Instant::now())));
}

fn last_interrupt() -> Option<Instant> {
    LAST_INTERRUPT.with(Cell::get)
}

/// 上游等满延迟、来要气泡的这一刻(`now`),该不该交空的出去:等待期间
/// (不早于 `now - delay` 起算)被打断过就不弹。
fn suppressed_at(last: Option<Instant>, now: Instant, delay: Duration) -> bool {
    let since = now.checked_sub(delay).unwrap_or(now);
    last.is_some_and(|at| at > since)
}

/// 已弹出的气泡(`shown` 时刻建的)在这之后被打断过没有。
fn interrupted_after(last: Option<Instant>, shown: Instant) -> bool {
    last.is_some_and(|at| at > shown)
}

/// 给可交互元素挂全仓统一手感的 tooltip:[`SHOW_DELAY`] + 本模块的小字气泡。
///
/// ```ignore
/// div().id("x").tip(t("app", "titleBar.settings"))          // 文本
/// div().id("y").tip_live(move |cx| ...)                      // 文本随状态现读
/// div().id("z").tip_with(move |window, cx| ...)             // 自造气泡视图
/// ```
///
/// 三个方法都会把延迟挂在同一个 `Interactivity` 上,顺序无关;都带「打断」
/// (见模块注释),点开菜单 / 弹窗后触发元素的提示不会再叠上去。
/// 想要 500ms 快弹的调用点**不要**用这里的方法,直接挂裸 `.tooltip()`。
pub trait TooltipExt: StatefulInteractiveElement + Sized {
    /// 纯文本提示。
    fn tip(self, text: impl Into<SharedString>) -> Self {
        let text: SharedString = text.into();
        self.tip_with(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
    }

    /// 文案**随状态变**的提示(开关的「显示 / 隐藏」、忙闲两种说明):气泡每次渲染
    /// 都调 `text` 现读。
    ///
    /// 别写成 `.tip(if on { a } else { b })`:gpui 在鼠标**移入**那一刻就把构造闭包
    /// 抓进延时任务,点一下切了状态,延时到点弹出来的仍是点击前的文案。
    fn tip_live(self, text: impl Fn(&App) -> SharedString + 'static) -> Self {
        let text = Rc::new(text);
        self.tip_with(move |window, cx| {
            let text = text.clone();
            Tooltip::element(move |_window, cx| text(cx)).build(window, cx)
        })
    }

    /// 自造气泡视图(富文本 / 自定义元素走这条,见 [`Tooltip::element`])。
    fn tip_with(self, build: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        self.capture_any_mouse_down(|_, _, _| interrupt())
            .tooltip_show_delay(SHOW_DELAY)
            .tooltip(move |window, cx| {
                let now = Instant::now();
                if suppressed_at(last_interrupt(), now, SHOW_DELAY) {
                    return cx
                        .new(|_| Guarded {
                            inner: None,
                            shown: now,
                        })
                        .into();
                }
                let inner = build(window, cx);
                cx.new(|_| Guarded {
                    inner: Some(inner),
                    shown: now,
                })
                .into()
            })
    }
}

impl<T: StatefulInteractiveElement> TooltipExt for T {}

/// 套在气泡外面的一层:出气泡那一刻就被判了不弹(`inner = None`),或弹出之后
/// 又被打断,都画成空的 —— 上游那边仍记着 `Visible`,鼠标移出元素才清。
struct Guarded {
    inner: Option<AnyView>,
    shown: Instant,
}

impl Render for Guarded {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match &self.inner {
            Some(inner) if !interrupted_after(last_interrupt(), self.shown) => {
                div().child(inner.clone())
            }
            _ => div(),
        }
    }
}

enum TooltipContent {
    Text(Text),
    Element(Box<dyn Fn(&mut Window, &mut App) -> AnyElement>),
}

/// 一条 tooltip 气泡。用法与 `gpui_component::tooltip::Tooltip` 完全一致:
///
/// ```ignore
/// .tip_with(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
/// ```
///
/// 停留时长不归它管 —— 那是元素侧的 [`TooltipExt`] / `tooltip_show_delay`。
pub struct Tooltip {
    content: TooltipContent,
}

impl Tooltip {
    /// 纯文本气泡。
    pub fn new(text: impl Into<Text>) -> Self {
        Self {
            content: TooltipContent::Text(text.into()),
        }
    }

    /// 自定义元素气泡(用量面板的趋势图六行详情就是这条)。
    pub fn element<E, F>(builder: F) -> Self
    where
        E: IntoElement,
        F: Fn(&mut Window, &mut App) -> E + 'static,
    {
        Self {
            content: TooltipContent::Element(Box::new(move |window, cx| {
                builder(window, cx).into_any_element()
            })),
        }
    }

    /// 建成 `AnyView` 交给 gpui。上游到这一刻已经等满了延迟,直接画。
    pub fn build(self, _: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|_| self).into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.content {
            TooltipContent::Text(text) => div().child(text.clone()),
            TooltipContent::Element(builder) => div().child(builder(window, cx)),
        };

        // 气泡本体。样式抄自 gpui-component 的 Tooltip,只改字号 ——
        // 外面那层 div 是上游留的:m_3 是相对鼠标的偏移,写在子级才生效。
        div()
            .child(
                h_flex()
                    .font_family(cx.theme().font_family.clone())
                    .m_3()
                    .bg(cx.theme().popover)
                    .text_color(cx.theme().popover_foreground)
                    .border_1()
                    .border_color(cx.theme().border)
                    .shadow_md()
                    .rounded(px(6.))
                    .justify_between()
                    .py_0p5()
                    .px_2()
                    .text_size(rems(TOOLTIP_FONT_SIZE))
                    .gap_3()
                    .child(content),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 停留时长必须明显长于 gpui 自己的 `DEFAULT_TOOLTIP_SHOW_DELAY`(500ms),
    /// 否则挂这套扩展就没意义了(退化成上游默认手感)。
    #[test]
    fn 停留时长长于上游默认档() {
        assert!(SHOW_DELAY > Duration::from_millis(500));
    }

    /// 退役二段延迟时的换算:老口径是 gpui 的 500ms + 自己的 700ms。
    /// 手感不该因为换实现而变,这里把那个总数钉住。
    #[test]
    fn 停留时长沿用二段延迟时代的总时长() {
        assert_eq!(SHOW_DELAY, Duration::from_millis(1200));
    }

    /// 字号必须比上游的 0.875rem 小,否则这个模块就白抄了。
    #[test]
    fn 字号小于上游默认档() {
        assert!(TOOLTIP_FONT_SIZE < 0.875);
    }

    /// 等待期间(出气泡前 `delay` 之内)被打断才不弹;更早的打断属于别的元素 / 上一轮
    /// 悬停,不能误伤这一次。
    #[test]
    fn 等待期间被打断才不弹() {
        let now = Instant::now() + Duration::from_secs(10);
        let delay = SHOW_DELAY;
        assert!(!suppressed_at(None, now, delay), "没打断过照常弹");
        let ms = Duration::from_millis;
        // 移入后 0.3s 点了一下(点开菜单 / 弹窗),到点不弹
        assert!(suppressed_at(Some(now - delay + ms(300)), now, delay));
        // 弹出前一刻才开的覆盖物也算
        assert!(suppressed_at(Some(now - ms(1)), now, delay));
        // 移入之前的按下(点别处、上一轮悬停)不算
        assert!(!suppressed_at(Some(now - delay - ms(1)), now, delay));
    }

    /// 已弹出的气泡:之后被打断才收,之前的打断不影响。
    #[test]
    fn 弹出之后被打断才收起() {
        let shown = Instant::now() + Duration::from_secs(10);
        let ms = Duration::from_millis;
        assert!(!interrupted_after(None, shown));
        assert!(!interrupted_after(Some(shown - ms(5)), shown));
        assert!(interrupted_after(Some(shown + ms(5)), shown));
    }

    /// `interrupt` 记的是「此刻」:记完之后,以此前任一时刻起算都算被打断过。
    #[test]
    fn 打断记在此刻() {
        let before = Instant::now() - Duration::from_millis(1);
        interrupt();
        assert!(interrupted_after(last_interrupt(), before));
        assert!(suppressed_at(last_interrupt(), Instant::now(), SHOW_DELAY));
    }
}
