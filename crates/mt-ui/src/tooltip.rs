//! 全仓统一的 tooltip:比 gpui-component 默认款**字更小、要停得更久才弹**。
//!
//! # 为什么不直接用 `gpui_component::tooltip::Tooltip`
//!
//! 两个想调的量都被上游写死,且都不在可配置面上:
//!
//! - **字号**:gpui-component 的 `Tooltip::render` 里把 `.text_sm()`(0.875rem)
//!   钉死在气泡上。它虽然实现了 `Styled` + `refine_style`,但那要求每个调用点
//!   都记得挂一次样式 —— 全仓 40 多处,漏一处就花。于是这里把那段气泡样式整份
//!   抄过来改成 [`TOOLTIP_FONT_SIZE`],**内容仍用它的 [`Text`]**,换行/富文本
//!   行为与之前逐字节一致。
//! - **停留时长**:gpui 的 `TOOLTIP_SHOW_DELAY`(500ms)是 `elements/div.rs` 里的
//!   **私有常量**,crates.io 版没有任何入口能改。
//!
//! # 「二段延迟」怎么做到的
//!
//! 不碰 gpui 那 500ms,而是在它后面再接一段:gpui 到点后照常把本视图建出来,
//! 但视图**先渲染成一个空 div**(零尺寸、不画任何东西),再等
//! [`EXTRA_SHOW_DELAY`] 才把气泡画出来。总停留 ≈ 500 + 700 = 1200ms。
//!
//! 鼠标中途离开 → gpui 把视图整个收掉 → 计时 `Task` 随之 drop,不会补弹一下。
//! 这是「延后」而不是「延后+补偿」,正是想要的语义。
//!
//! 已知的小代价:气泡锚点取的是 gpui 建视图那一刻(第 500ms)的鼠标位置,
//! 后面 700ms 里鼠标在同一元素内移动不会重新贴位。与上游非 hoverable tooltip
//! 一贯的行为一致(它本来也只在建视图时取一次 `mouse_position`)。

use std::time::Duration;

use gpui::{
    AnyElement, AnyView, App, AppContext, Context, IntoElement, ParentElement, Render, Styled,
    Task, Window, div, px, rems,
};
use gpui_component::{ActiveTheme, h_flex, text::Text};

/// 在 gpui 自己那 500ms 之上**再**等多久才把气泡画出来。
///
/// 调这一个数就等于调「鼠标要停多久才弹提示」:实际总时长 = 500ms + 本值。
pub const EXTRA_SHOW_DELAY: Duration = Duration::from_millis(700);

/// 气泡字号。上游是 `text_sm`(0.875rem);这里降一档到 0.75rem。
///
/// 跟 `text_xs()` 同值,写成 `rems` 是为了「想再调时有个明确的旋钮」。
/// 用 rem 而非 px:随 `gpui_component` 主题的 `font_size`(= 窗口 rem 基准)
/// 缩放,与上游同一套相对关系。
const TOOLTIP_FONT_SIZE: f32 = 0.75;

enum TooltipContent {
    Text(Text),
    Element(Box<dyn Fn(&mut Window, &mut App) -> AnyElement>),
}

/// 一条 tooltip。用法与 `gpui_component::tooltip::Tooltip` 完全一致:
///
/// ```ignore
/// .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
/// ```
pub struct Tooltip {
    content: TooltipContent,
    /// 额外停留时间到了没。false 期间 render 出的是空 div。
    visible: bool,
    /// 持有即计时;视图被 gpui 收掉时一起 drop,计时随之作废。
    _delay: Option<Task<()>>,
}

impl Tooltip {
    /// 纯文本气泡。
    pub fn new(text: impl Into<Text>) -> Self {
        Self {
            content: TooltipContent::Text(text.into()),
            visible: false,
            _delay: None,
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
            visible: false,
            _delay: None,
        }
    }

    /// 建成 `AnyView` 交给 gpui。**额外的停留计时从这一刻起算**。
    pub fn build(self, _: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| {
            let delay = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(EXTRA_SHOW_DELAY).await;
                let _ = this.update(cx, |this: &mut Self, cx| {
                    this.visible = true;
                    cx.notify();
                    // tooltip 视图挂在窗口的 tooltip 通道上而不是常规元素树里,
                    // 这里再补一发全窗刷新兜底 —— 漏帧的后果是「提示再也不弹」,
                    // 比多刷一帧难查得多。gpui 自己弹 tooltip 时也会 refresh 一次。
                    cx.refresh_windows();
                });
            });
            Self {
                _delay: Some(delay),
                ..self
            }
        })
        .into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.visible {
            // 还没停够:占位但不画。零尺寸空 div 既不出边框也不投影,
            // 视觉上等同于「tooltip 还没出现」。
            return div().into_any_element();
        }

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

    /// 两段延迟加起来才是用户感知到的「停留时长」。gpui 那 500ms 改不了,
    /// 这里守住自己这段别被顺手调回 0(调 0 就退化成上游默认手感)。
    #[test]
    fn 额外延迟明显大于零() {
        assert!(EXTRA_SHOW_DELAY >= Duration::from_millis(300));
    }

    /// 字号必须比上游的 0.875rem 小,否则这个模块就白抄了。
    #[test]
    fn 字号小于上游默认档() {
        assert!(TOOLTIP_FONT_SIZE < 0.875);
    }
}
