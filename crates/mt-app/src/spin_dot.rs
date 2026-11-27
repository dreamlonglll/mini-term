//! ai-working 转圈灯的隔离小视图。
//!
//! # 为什么要有它
//!
//! 转圈灯的相位来自 `mt_ui::motion::pulse_phase` 的低频泵:前台每 100ms 一拍,
//! notify 的是**调它时正在 render 的那个视图**(`window.current_view()`)。灯内联在
//! 项目列表 / 终端区里时,那就是整个宿主 —— AI 工作期间每一拍都把项目列表、终端区
//! 整块重跑一遍 render 与布局(实测各约 47 次 / 5s)。
//!
//! 把 ai-working 那颗灯挂进自己的小视图([`SpinDot`]),泵就只 notify 它。宿主没被
//! notify、读过的东西也没变,gpui-fast 从上一帧拼接宿主,只在灯的位置把这一颗重画
//! (`fast/splice.rs`:cached 的宿主走 `splice_cached`,要求脏的内层视图**也是 cached**,
//! 所以小视图一律以 `AnyView::cached(固定尺寸)` 挂载;非 cached 的宿主走
//! `splice_layout`,两种都接得住)。与活动栏角标的 `CornerDotView` 同一个思路。
//!
//! # 用法
//!
//! 宿主持一个 [`SpinDots`],render 一开头调 [`SpinDots::begin`](顺手回收上一轮没
//! 用到的灯),凡是画状态灯的地方改调 [`SpinDots::dot`]。只有 ai-working —— 唯一会转
//! 的一档 —— 走小视图,其余档位照旧内联:它们不动,不进泵。
//!
//! # 边界
//!
//! - 同一个 key 一轮里只挂一次:同一个实体在一帧里画在两处,两份保留记录会互相顶掉。
//!   重复的 key 退回内联(不会出现在现有调用点上,只是兜底)。
//! - 滚动容器被提成瓦片层(连续两帧以上在滚,`fast/layers/policy.rs`)期间,层里的视图
//!   不留保留记录,拼接退回整块重建 —— 与改造前一样,不更差。滚动一停、内容又一直在变
//!   (灯在转)的层很快被降级回普通路径(同文件的 `DEMOTE_AFTER_CHANGED_FRAMES`)。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use gpui::{
    AnyElement, AnyView, App, AppContext as _, Context, Entity, IntoElement, Pixels, Render,
    SharedString, StyleRefinement, Styled as _, Window,
};
use mt_ui::icons::StatusDot;

use crate::tree::StatusLight;
use crate::ui;

/// 一颗状态灯。尺寸与配色和 [`ui::status_dot`] 同一口径(挖空笔画取 `bg_elevated`)。
fn status_dot(light: StatusLight, size: Pixels) -> StatusDot {
    StatusDot::new(ui::status_kind(light))
        .size(size)
        .color(ui::status_color(light))
        .contrast(ui::bg_elevated())
}

/// 一颗 ai-working 转圈灯的小视图。只画灯、不读任何实体:泵 notify 它时宿主不受牵连。
pub struct SpinDot {
    size: Pixels,
}

impl Render for SpinDot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        tests::DOT_RENDERS.with(|n| n.set(n.get() + 1));
        status_dot(StatusLight::AiWorking, self.size)
    }
}

struct Slot {
    view: Entity<SpinDot>,
    size: Pixels,
    /// 最近一次被 [`SpinDots::dot`] 取用是在宿主的第几轮 render。
    round: u64,
}

/// 宿主持有的一组转圈灯(按调用方给的 key 复用同一个实体,画面上的那颗灯身份稳定)。
///
/// 取用走 `&self`(内部可变):宿主拼行的方法大多只借 `&self`。
#[derive(Default)]
pub struct SpinDots {
    slots: RefCell<HashMap<SharedString, Slot>>,
    round: Cell<u64>,
}

impl SpinDots {
    /// 宿主 render 一开头调:回收上一轮 render 没用到的灯,开始新的一轮。
    ///
    /// 回收放在下一轮开头而不是这一轮结尾:宿主 render 有好几处提前 return,结尾那一刻
    /// 不好找;而没被 render 用到的灯不在树上、不 render,也就不进泵,多留一轮无害。
    pub fn begin(&self) {
        let round = self.round.get();
        self.slots
            .borrow_mut()
            .retain(|_, slot| slot.round == round);
        self.round.set(round.wrapping_add(1));
    }

    /// 一颗状态灯。ai-working 挂成独立的 cached 小视图(泵只 notify 它),其余档位内联。
    ///
    /// `key` 在这个宿主里要能唯一标出这颗灯的位置(如 `row:<项目 id>`、`tab:<pane id>`)。
    pub fn dot(
        &self,
        key: impl Into<SharedString>,
        light: StatusLight,
        size: Pixels,
        cx: &mut App,
    ) -> AnyElement {
        if light != StatusLight::AiWorking {
            return status_dot(light, size).into_any_element();
        }
        let key = key.into();
        let round = self.round.get();
        let mut slots = self.slots.borrow_mut();
        if slots.get(&key).is_some_and(|slot| slot.round == round) {
            return status_dot(light, size).into_any_element();
        }
        let slot = slots.entry(key).or_insert_with(|| Slot {
            view: cx.new(|_| SpinDot { size }),
            size,
            round,
        });
        if slot.size != size {
            slot.view = cx.new(|_| SpinDot { size });
            slot.size = size;
        }
        slot.round = round;
        // 缓存视图按这里的样式起占位节点、不量内容:与灯本体(`VectorIcon`:定宽定高、
        // 不参与 flex 压缩)等价,行里的排布与内联时一个像素不差
        AnyView::from(slot.view.clone())
            .cached(StyleRefinement::default().w(size).h(size).flex_shrink_0())
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        InteractiveElement as _, ParentElement as _, ScrollHandle, StatefulInteractiveElement as _,
        div, px,
    };
    use std::rc::Rc;

    thread_local! {
        /// [`SpinDot::render`] 跑了几次(只在单测里计)。
        pub(super) static DOT_RENDERS: Cell<usize> = const { Cell::new(0) };
    }

    /// 泵 notify 转圈灯时宿主不跟着重建:gpui-fast 从上一帧拼接宿主,只重画这一颗。
    /// 两种宿主都钉:cached 的(项目列表,走 `splice_cached`)与普通子视图(终端区,走
    /// `splice_layout`);灯都挂在宿主的滚动容器里(两者的行 / 页签都在滚动容器里)。
    #[gpui::test]
    fn 灯重画时宿主不跟着重建(cx: &mut gpui::TestAppContext) {
        struct Host {
            dots: SpinDots,
            scroll: ScrollHandle,
            renders: Rc<Cell<usize>>,
        }
        impl Render for Host {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                self.renders.set(self.renders.get() + 1);
                self.dots.begin();
                let dot = self.dots.dot("row:p", StatusLight::AiWorking, px(11.0), cx);
                div()
                    .id("rows")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(div().flex().items_center().child("row").child(dot))
            }
        }
        struct Root {
            cached: Entity<Host>,
            plain: Entity<Host>,
        }
        impl Render for Root {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .size_full()
                    .flex()
                    .child(
                        div().w(px(200.0)).h_full().child(
                            AnyView::from(self.cached.clone())
                                .cached(StyleRefinement::default().size_full()),
                        ),
                    )
                    .child(div().flex_1().h_full().child(self.plain.clone()))
            }
        }
        let host = |renders: &Rc<Cell<usize>>| Host {
            dots: SpinDots::default(),
            scroll: ScrollHandle::new(),
            renders: renders.clone(),
        };
        let (cached_renders, plain_renders) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let cached = cx.new(|_| host(&cached_renders));
        let plain = cx.new(|_| host(&plain_renders));
        let (_, cx) = cx.add_window_view(|_, _| Root {
            cached: cached.clone(),
            plain: plain.clone(),
        });
        cx.run_until_parked();
        let dots = |host: &Entity<Host>, cx: &mut gpui::VisualTestContext| {
            host.read_with(&*cx, |host, _| {
                host.dots
                    .slots
                    .borrow()
                    .values()
                    .map(|slot| slot.view.clone())
                    .collect::<Vec<_>>()
            })
        };
        let spinning = [dots(&cached, cx), dots(&plain, cx)].concat();
        assert_eq!(spinning.len(), 2, "两个宿主各挂一颗转圈灯");
        // 泵的一拍:notify 登记过的视图,也就是这两颗灯
        let tick = |cx: &mut gpui::VisualTestContext| {
            for dot in &spinning {
                dot.update(cx, |_, cx| cx.notify());
            }
            cx.run_until_parked();
        };
        // 灯刚挂上后的第一拍宿主还会再重建一次(实测,灯的实体是在宿主 render 里建的),
        // 之后才进入稳态 —— 真机上就是 AI 开工后多一次重建
        tick(cx);
        let first = (cached_renders.get(), plain_renders.get(), DOT_RENDERS.get());
        for _ in 0..3 {
            tick(cx);
        }
        assert_eq!(DOT_RENDERS.get(), first.2 + 6, "灯每拍都重画了");
        assert_eq!(cached_renders.get(), first.0, "cached 宿主没跟着重建");
        assert_eq!(plain_renders.get(), first.1, "普通宿主没跟着重建");
    }

    #[gpui::test]
    fn 只有转圈那一档挂小视图_同key复用_没用到的下一轮回收(
        cx: &mut gpui::TestAppContext,
    ) {
        let dots = SpinDots::default();
        cx.update(|cx| {
            dots.begin();
            let _ = dots.dot("a", StatusLight::AiWorking, px(11.0), cx);
            let _ = dots.dot("b", StatusLight::AiWorking, px(11.0), cx);
            // 不转的档位内联,不占槽
            let _ = dots.dot("c", StatusLight::AiIdle, px(11.0), cx);
            let _ = dots.dot("d", StatusLight::Attention, px(11.0), cx);
            // 同一轮重复的 key 退回内联,不顶掉已挂的那颗
            let _ = dots.dot("a", StatusLight::AiWorking, px(11.0), cx);
            assert_eq!(dots.slots.borrow().len(), 2);
            let first_a = dots.slots.borrow()["a"].view.entity_id();

            dots.begin();
            let _ = dots.dot("a", StatusLight::AiWorking, px(11.0), cx);
            assert_eq!(
                dots.slots.borrow()["a"].view.entity_id(),
                first_a,
                "同 key 跨轮复用同一个实体"
            );
            // 换尺寸就换实体(占位样式跟着变)
            let _ = dots.dot("b", StatusLight::AiWorking, px(8.5), cx);
            assert_eq!(dots.slots.borrow()["b"].size, px(8.5));

            dots.begin();
            let _ = dots.dot("a", StatusLight::AiIdle, px(11.0), cx);
            dots.begin();
            assert!(dots.slots.borrow().is_empty(), "连着一轮没用到的灯被回收");
        });
    }
}
