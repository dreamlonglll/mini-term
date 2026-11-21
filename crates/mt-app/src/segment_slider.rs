//! 段控件 / 竖排导航的**滑动选中块**:选中底色是一块绝对定位的滑块,换选中项时从旧位置
//! 滑到新位置(`main.rs` 抽屉段控件的同款形态)。
//!
//! 抽屉那组是两个等宽的半格,滑块按百分比走就行;这里服务的是**各项尺寸随文字变**的那些
//! (diff 的「并排 / 内联」、worktree 的「已有 / 新分支」、用量统计的 scope / range、设置页
//! 左侧导航 —— 导航里还夹着分组标题,各项的 y 不规则),所以要**先量再滑**:
//!
//! - 每一项里挂一块 [`IndicatorSlider::probe`](不画东西的 canvas),prepaint 时把这一项的
//!   窗口矩形记进共享状态;滑块元素([`IndicatorSlider::block`])在自己的 prepaint 里记下
//!   容器的参照原点(自己的窗口位置减去被放的偏移 —— 与滑块同一个参照系,边框 / 滚动偏移
//!   都自动对齐);
//! - 滑块在 **layout 阶段**按上一帧量到的矩形定位:选中项变了就用 `ValueTween` 把
//!   x / y / 宽 / 高 四个量从当前显示值补到新项([`mt_ui::motion::TAB_INDICATOR`]),没变
//!   而矩形变了(窗口缩放、字号变)直接贴过去不补;
//! - **首次出现不滑**:还没量到时滑块不画,由调用方让选中项自己画底色([`IndicatorSlider::ready`]
//!   为 `false` 的那一帧),量到之后滑块落在选中项上、选中项撤掉自己的底色,两帧一模一样;
//! - 动画中由滑块元素自己要帧(`request_layout` 里 `request_animation_frame`,通知的是画它的
//!   视图),调用方 render 里不必 drive;服从「启用动画」总开关,豁免系统「减少动画」。
//!
//! ```ignore
//! // 视图 / 状态字段
//! scope_slider: IndicatorSlider,
//!
//! // render
//! let ready = self.scope_slider.ready(active);
//! let mut bar = segmented().relative()
//!     .child(self.scope_slider.block(active, |el| el.bg(ui::accent())));
//! for (i, item) in items.iter().enumerate() {
//!     bar = bar.child(segment(.., i == active, /* 自己画底色 */ !ready).child(self.scope_slider.probe(i)));
//! }
//! ```
//!
//! ⚠️ 滑块要排在各项**之前**(先画在底下,文字压在上面),容器要能当绝对定位的参照
//! (gpui 里任何 div 都是)。

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    AnyElement, App, Bounds, Div, Element, ElementId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, Point, Styled, Window, canvas, div, point, px,
};
use mt_ui::motion::{TAB_INDICATOR, TransitionSpec, ValueTween};

/// 相对参照原点的矩形:`[x, y, 宽, 高]`(像素)。
pub type SlotRect = [f32; 4];

/// 量值抖动多少以内当没变(免得亚像素抖动来回要帧)。
const EPS: f32 = 0.5;

#[derive(Debug)]
struct SliderState {
    spec: TransitionSpec,
    /// 各项的窗口矩形(探针 prepaint 写,下一帧 layout 读)。
    items: Vec<Option<Bounds<Pixels>>>,
    /// 滑块参照系的原点(窗口坐标)。
    origin: Option<Point<Pixels>>,
    /// 上一次排版时滑块落在哪一项。
    shown: Option<usize>,
    tweens: Option<[ValueTween; 4]>,
}

impl SliderState {
    fn rect_of(&self, index: usize) -> Option<SlotRect> {
        let origin = self.origin?;
        let b = self.items.get(index).copied().flatten()?;
        Some([
            f32::from(b.origin.x - origin.x),
            f32::from(b.origin.y - origin.y),
            f32::from(b.size.width),
            f32::from(b.size.height),
        ])
    }

    /// 这一次排版滑块画在哪(`None` = 还没量到,不画)+ 还在不在动。见模块注释的三条规则。
    fn plan(&mut self, active: usize, now: Instant) -> Option<(SlotRect, bool)> {
        let target = self.rect_of(active)?;
        let spec = self.spec;
        let settled = |rect: SlotRect| rect.map(|v| ValueTween::settled(v, spec));
        let moved = self.shown.is_some_and(|prev| prev != active);
        match self.tweens.as_mut() {
            // 换了选中项:四个量从当前显示值补过去
            Some(tweens) if moved => {
                for (tween, v) in tweens.iter_mut().zip(target) {
                    tween.retarget_at(v, now);
                }
            }
            // 同一项、矩形变了(缩放 / 字号):贴过去不补;正在滑向它的照旧滑
            Some(tweens) => {
                if tweens
                    .iter()
                    .zip(target)
                    .any(|(tween, v)| (tween.target() - v).abs() > EPS)
                {
                    self.tweens = Some(settled(target));
                }
            }
            // 头一回画:要是选中项在量到之前就换过,从旧项滑过来;否则直接落位(首次出现不滑)
            None => {
                let from = self
                    .shown
                    .filter(|_| moved)
                    .and_then(|prev| self.rect_of(prev));
                let mut tweens = settled(from.unwrap_or(target));
                for (tween, v) in tweens.iter_mut().zip(target) {
                    tween.retarget_at(v, now);
                }
                self.tweens = Some(tweens);
            }
        }
        self.shown = Some(active);
        let tweens = self.tweens.as_ref()?;
        let rect = [0, 1, 2, 3].map(|i| tweens[i].value_at(now));
        let running = tweens.iter().any(|t| moving(t, now));
        Some((rect, running))
    }
}

/// 这条补间这一刻还在**动**吗。`ValueTween::settled` 按建它那一刻的时钟把起点拨到过去,
/// 拿稍早的 `now` 去问 `running_at` 会差那么一点没跑完 —— 起止同值的静止补间不该为此
/// 多要一帧,所以再看一眼值是不是已经在目标上。
fn moving(tween: &ValueTween, now: Instant) -> bool {
    tween.running_at(now) && (tween.value_at(now) - tween.target()).abs() > 1e-3
}

/// `a` 盖住 `b` 的面积比例(0..1)。
fn overlap(a: SlotRect, b: SlotRect) -> f32 {
    let w = (a[0] + a[2]).min(b[0] + b[2]) - a[0].max(b[0]);
    let h = (a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1]);
    let area = b[2] * b[3];
    if w <= 0.0 || h <= 0.0 || area <= 0.0 {
        return 0.0;
    }
    (w * h / area).clamp(0.0, 1.0)
}

fn bounds_differ(a: Bounds<Pixels>, b: Bounds<Pixels>) -> bool {
    let d = |x: Pixels, y: Pixels| f32::from(x - y).abs() > EPS;
    d(a.origin.x, b.origin.x)
        || d(a.origin.y, b.origin.y)
        || d(a.size.width, b.size.width)
        || d(a.size.height, b.size.height)
}

/// 一组项的滑动选中块(共享句柄,`Clone` 只加引用计数)。挂在画这组控件的视图 / 状态上。
#[derive(Clone, Debug)]
pub struct IndicatorSlider(Rc<RefCell<SliderState>>);

impl Default for IndicatorSlider {
    fn default() -> Self {
        Self::new()
    }
}

impl IndicatorSlider {
    pub fn new() -> Self {
        Self::with_spec(TAB_INDICATOR)
    }

    pub fn with_spec(spec: TransitionSpec) -> Self {
        Self(Rc::new(RefCell::new(SliderState {
            spec,
            items: Vec::new(),
            origin: None,
            shown: None,
            tweens: None,
        })))
    }

    /// 这一帧滑块画不画得出来(第 `active` 项与参照原点都量到了)。`false` 时调用方让选中项
    /// 自己画底色。
    pub fn ready(&self, active: usize) -> bool {
        self.0.borrow().rect_of(active).is_some()
    }

    /// 滑块**此刻**盖住第 `index` 项的面积比例(0..1)。还没画过 / 没量到为 `None`。
    ///
    /// 实底滑块上的选中字是反白的:字色要跟着滑块走(盖过一半才反白),否则新选中那一段
    /// 在滑块到达之前是一行白字压在面板底上,几乎看不见。
    pub fn coverage(&self, index: usize) -> Option<f32> {
        let s = self.0.borrow();
        let item = s.rect_of(index)?;
        let tweens = s.tweens.as_ref()?;
        let now = Instant::now();
        Some(overlap([0, 1, 2, 3].map(|i| tweens[i].value_at(now)), item))
    }

    /// 量第 `index` 项位置的探针:挂进那一项里(absolute 全尺寸、不画东西、不插 hitbox)。
    ///
    /// ⚠️ **必须钉 `top_0().left_0()`**:taffy 对不写 inset 的绝对定位子元素按「静态位置」摆
    /// —— flex 容器里落在内容盒起点(差一个 padding),block 容器里还会落到前面兄弟的下方
    /// (`git_diff/split.rs`、`menu.rs` 记过同一个坑)。漏了它,量出来的矩形整体偏右 / 偏下,
    /// 滑块跟着错位。
    pub fn probe(&self, index: usize) -> impl IntoElement + use<> {
        let state = self.0.clone();
        canvas(
            move |bounds: Bounds<Pixels>, window: &mut Window, _cx: &mut App| {
                let changed = {
                    let mut s = state.borrow_mut();
                    if s.items.len() <= index {
                        s.items.resize(index + 1, None);
                    }
                    let changed = s.items[index].is_none_or(|old| bounds_differ(old, bounds));
                    s.items[index] = Some(bounds);
                    changed
                };
                // 量值变了:下一帧按新值排(首帧、窗口缩放、列表滚动)
                if changed {
                    window.request_animation_frame();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    /// 滑块本体。`style` 给它上底色 / 圆角 / 内部装饰(位置与尺寸由本件定)。排在各项之前。
    pub fn block(&self, active: usize, style: impl FnOnce(Div) -> Div + 'static) -> SliderBlock {
        SliderBlock {
            state: self.0.clone(),
            active,
            style: Some(Box::new(style)),
        }
    }
}

/// 见 [`IndicatorSlider::block`]。
pub struct SliderBlock {
    state: Rc<RefCell<SliderState>>,
    active: usize,
    style: Option<Box<dyn FnOnce(Div) -> Div>>,
}

/// 排版时定下的东西:内层元素 + 被放的偏移(prepaint 拿自己的位置减它得参照原点)。
pub struct SliderLayout {
    inner: AnyElement,
    offset: (f32, f32),
}

impl Element for SliderBlock {
    type RequestLayoutState = SliderLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, SliderLayout) {
        let plan = self.state.borrow_mut().plan(self.active, Instant::now());
        let (inner, offset) = match plan {
            Some(([x, y, w, h], running)) => {
                if running {
                    window.request_animation_frame();
                }
                let style = self.style.take().unwrap_or_else(|| Box::new(|el| el));
                let el = style(
                    div()
                        .absolute()
                        .left(px(x))
                        .top(px(y))
                        .w(px(w.max(0.0)))
                        .h(px(h.max(0.0))),
                );
                (el.into_any_element(), (x, y))
            }
            // 还没量到:不画,只占一个零尺寸的绝对定位点 —— 它在 prepaint 里照样能报出
            // 参照原点
            None => (
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .size_0()
                    .into_any_element(),
                (0.0, 0.0),
            ),
        };
        let mut inner = inner;
        let layout_id = inner.request_layout(window, cx);
        (layout_id, SliderLayout { inner, offset })
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut SliderLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let origin = point(
            bounds.origin.x - px(layout.offset.0),
            bounds.origin.y - px(layout.offset.1),
        );
        let changed = {
            let mut s = self.state.borrow_mut();
            let changed = s.origin.is_none_or(|old| {
                f32::from(old.x - origin.x).abs() > EPS || f32::from(old.y - origin.y).abs() > EPS
            });
            s.origin = Some(origin);
            changed
        };
        if changed {
            window.request_animation_frame();
        }
        layout.inner.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        layout: &mut SliderLayout,
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        layout.inner.paint(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::size;
    use std::time::Duration;

    fn state() -> SliderState {
        let rect =
            |x: f32, w: f32| Some(Bounds::new(point(px(x), px(10.0)), size(px(w), px(24.0))));
        SliderState {
            spec: TransitionSpec::exempt(Duration::from_millis(200), mt_ui::motion::Easing::Linear),
            // 三项宽度各不相同:40 / 64 / 30
            items: vec![rect(101.0, 40.0), rect(141.0, 64.0), rect(205.0, 30.0)],
            origin: Some(point(px(100.0), px(10.0))),
            shown: None,
            tweens: None,
        }
    }

    #[test]
    fn 覆盖比例按面积算() {
        let item = [10.0, 0.0, 40.0, 24.0];
        assert_eq!(overlap(item, item), 1.0);
        assert_eq!(overlap([60.0, 0.0, 40.0, 24.0], item), 0.0, "不相交");
        assert!(
            (overlap([30.0, 0.0, 40.0, 24.0], item) - 0.5).abs() < 1e-6,
            "盖住一半"
        );
        assert_eq!(overlap([0.0, 0.0, 100.0, 24.0], item), 1.0, "整个罩住");
        assert_eq!(overlap(item, [0.0, 0.0, 0.0, 24.0]), 0.0, "零宽的项不除零");
    }

    #[test]
    fn 没量到不画() {
        let mut s = state();
        s.origin = None;
        assert!(s.plan(0, Instant::now()).is_none());
        let mut s = state();
        assert!(s.plan(7, Instant::now()).is_none(), "越界的项也算没量到");
    }

    #[test]
    fn 首次出现直接落位_换项才滑() {
        let t0 = Instant::now();
        let mut s = state();
        let (rect, running) = s.plan(1, t0).unwrap();
        assert_eq!(rect, [41.0, 0.0, 64.0, 24.0], "相对参照原点");
        assert!(!running, "首次出现不滑");
        // 同一项再排一次:不动
        assert!(!s.plan(1, t0 + Duration::from_millis(10)).unwrap().1);

        // 换到第 2 项:x 与宽一起补
        let t1 = t0 + Duration::from_millis(20);
        let (start, running) = s.plan(2, t1).unwrap();
        if mt_ui::motion::animations_enabled() {
            assert!(running);
            assert_eq!(start, [41.0, 0.0, 64.0, 24.0], "起点是旧项");
            let (mid, _) = s.plan(2, t1 + Duration::from_millis(100)).unwrap();
            assert!((mid[0] - 73.0).abs() < 1e-3, "{mid:?}");
            assert!((mid[2] - 47.0).abs() < 1e-3, "{mid:?}");
        }
        let (end, running) = s.plan(2, t1 + Duration::from_millis(200)).unwrap();
        assert_eq!(end, [105.0, 0.0, 30.0, 24.0]);
        assert!(!running, "到位就不再要帧");
    }

    #[test]
    fn 同一项矩形变了直接贴过去() {
        let t0 = Instant::now();
        let mut s = state();
        s.plan(0, t0);
        // 窗口缩放:第 0 项右移 20、变宽
        s.items[0] = Some(Bounds::new(
            point(px(121.0), px(10.0)),
            size(px(50.0), px(24.0)),
        ));
        let (rect, running) = s.plan(0, t0 + Duration::from_millis(5)).unwrap();
        assert_eq!(rect, [21.0, 0.0, 50.0, 24.0]);
        assert!(!running, "没换项不补间");
    }

    /// 探针量到的必须是**段自身**的矩形 —— 段是 flex 带 padding(diff / worktree / 设置导航)
    /// 还是 block 带 padding、探针排在文字之后(用量统计),都一样。漏写 inset 时 taffy 按
    /// 静态位置摆:flex 段偏右一个 padding,block 段落到文字下方。
    #[gpui::test]
    fn 探针量的是段自身的矩形(cx: &mut gpui::TestAppContext) {
        use gpui::{AnyWindowHandle, AppContext as _, Context, ParentElement, Render};

        // 探针在 prepaint 里要帧(`request_animation_frame` 认的是「正在画的视图」),
        // 所以得挂在一个真视图里画,不能走 `VisualTestContext::draw`
        struct Host(IndicatorSlider);
        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let s = &self.0;
                // 外面再包一层:窗口根元素被拉成整窗高,段控件直接当根的话各段会被拉高
                div().child(
                    div()
                        .flex()
                        .border_1()
                        .child(s.block(0, |el| el))
                        // flex 段:24 高、左右 padding 10、内容 30 宽 → 50 × 24
                        .child(
                            div()
                                .h(px(24.0))
                                .flex()
                                .items_center()
                                .px(px(10.0))
                                .child(div().w(px(30.0)).h(px(12.0)))
                                .child(s.probe(0)),
                        )
                        // block 段:padding 10 / 4、内容 20 × 16 → 40 × 24,探针排在内容之后
                        .child(
                            div()
                                .px(px(10.0))
                                .py(px(4.0))
                                .child(div().w(px(20.0)).h(px(16.0)))
                                .child(s.probe(1)),
                        ),
                )
            }
        }

        let slider = IndicatorSlider::new();
        let host = slider.clone();
        let window = AnyWindowHandle::from(cx.add_window(move |_, _| Host(host)));
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        let state = slider.0.borrow();
        // 参照原点 = 外框内边(1px 边框)
        assert_eq!(state.origin, Some(point(px(1.0), px(1.0))));
        assert_eq!(state.rect_of(0), Some([0.0, 0.0, 50.0, 24.0]), "flex 段");
        assert_eq!(state.rect_of(1), Some([50.0, 0.0, 40.0, 24.0]), "block 段");
    }

    #[test]
    fn 量到之前就换过项_头一回画从旧项滑过来() {
        let t0 = Instant::now();
        let mut s = state();
        s.shown = Some(0);
        let (rect, running) = s.plan(2, t0).unwrap();
        if mt_ui::motion::animations_enabled() {
            assert!(running);
            assert_eq!(rect, [1.0, 0.0, 40.0, 24.0]);
        }
    }
}
