//! 浮层开合的通用件:弹窗外壳、右键菜单、日期选择器、图片放大浮层共用。
//!
//! [`crate::popover_reveal`] 只会「水平某点、顶边往下长」,时长也是它自己那两档。这里补齐
//! 另外几种浮层要的三样东西:
//!
//! 1. [`Presence`]:可掉头的进出场进度(0 = 收起,1 = 全开),进 / 出各取一条
//!    `mt_ui::motion` 规格。半路掉头从**当前显示值**起步、时长按剩余路程折算,不跳变 ——
//!    与 `popover_reveal::Reveal` 同一套语义,只是规格可配;服从「启用动画」总开关;
//! 2. [`reveal_box`]:任意原点、任意起点比例的裁剪窗展开。内容按最终尺寸排好,框体(底色 /
//!    边框 / 阴影)按插值矩形画,内容只被同样大小的裁剪窗露出来 —— 全程不透明、不重排;
//! 3. [`linger`]:离场托管。日期选择器 / 图片浮层的宿主收到关闭事件就 drop 实体,退场动画
//!    没处播;关闭那一刻把实体交给这里多活一个退场时长,由根视图挂着画完。
//!
//! # 口径(用户定稿)
//!
//! - 不做透明度淡入淡出:方向性展开,从触发它的那个点 / 边长出来;
//! - 有进场就有退场:收起时反向播完再卸;退场中不接鼠标(框体上盖一块拦鼠标的 hitbox),
//!   焦点在退场开始时就由各浮层自己还回去;
//! - 只在动画进行中要帧,播完就停。

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, AnyView, App, AppContext, Bounds, ContentMask, Div, Element, ElementId, Entity,
    Global, GlobalElementId, HitboxBehavior, InspectorElementId, IntoElement, LayoutId,
    ParentElement, Pixels, Point, Refineable as _, Render, Size, Style, Styled, Window, div, point,
    px, size,
};
use mt_ui::motion::{TransitionSpec, animations_enabled, ease};

use crate::popover_reveal::FrameRect;

// ─── 进度 ────────────────────────────────────────────────────

/// 一个浮层的「在场度」:0 = 收起,1 = 全开(已缓动)。
///
/// 与 `mt_ui::motion::Transition` 的区别:后者 `restart` 刻意从头播,这里要求半路反向操作
/// 从当前进度掉头。所以记「起点值 → 目标值 + 起始时刻 + 时长」,掉头时以当前显示值为新
/// 起点,时长按剩余路程折算。往 1 走用 `enter` 的曲线,往 0 走用 `exit` 的。
#[derive(Clone, Copy, Debug)]
pub struct Presence {
    from: f32,
    to: f32,
    start: Instant,
    duration: Duration,
    enter: TransitionSpec,
    exit: TransitionSpec,
}

impl Presence {
    /// 收起且静止。
    pub fn hidden(enter: TransitionSpec, exit: TransitionSpec) -> Self {
        Self {
            from: 0.0,
            to: 0.0,
            start: Instant::now(),
            duration: Duration::ZERO,
            enter,
            exit,
        }
    }

    /// 目标是全开吗(收起动画里是 `false`:内容不该再接鼠标)。
    pub fn target_open(&self) -> bool {
        self.to > 0.0
    }

    /// 这一刻的进度(已缓动)。
    pub fn value_at(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.start);
        if self.duration.is_zero() || elapsed >= self.duration {
            return self.to;
        }
        let t = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        let spec = if self.to >= self.from {
            self.enter
        } else {
            self.exit
        };
        self.from + (self.to - self.from) * ease(spec.easing, t)
    }

    /// 还在动吗(= 还要不要请求下一帧)。到终态之后恒 `false`。
    pub fn running_at(&self, now: Instant) -> bool {
        !self.duration.is_zero() && now.saturating_duration_since(self.start) < self.duration
    }

    /// 还在树上吗:开着,或者收起动画还没播完。
    pub fn mounted_at(&self, now: Instant) -> bool {
        self.value_at(now) > 0.0 || self.running_at(now)
    }

    /// 朝 `target`(0 / 1)走。从**当前显示值**起步,时长按剩余路程折算;
    /// `animate = false` 时直接到位。
    pub fn retarget(&mut self, target: f32, now: Instant, animate: bool) {
        let current = self.value_at(now);
        let spec = if target > current {
            self.enter
        } else {
            self.exit
        };
        self.from = current;
        self.to = target;
        self.start = now;
        self.duration = if animate {
            spec.duration.mul_f32((target - current).abs().min(1.0))
        } else {
            Duration::ZERO
        };
    }

    /// 播进场(总开关关着时直接到位)。
    pub fn show(&mut self, now: Instant) {
        self.retarget(1.0, now, animations_enabled());
    }

    /// 播退场(总开关关着时直接收起)。
    pub fn hide(&mut self, now: Instant) {
        self.retarget(0.0, now, animations_enabled());
    }

    /// render 里调:取进度,还在动就顺手要下一帧(`request_animation_frame` 通知的是
    /// **当前视图**,所以必须在画这个浮层的视图的 render 调用链里调)。
    pub fn drive(&self, window: &Window) -> f32 {
        let now = Instant::now();
        if self.running_at(now) {
            window.request_animation_frame();
        }
        self.value_at(now)
    }
}

// ─── 纯数:插值矩形 ───────────────────────────────────────────

/// 开合进度 `progress`(0 = 收起,1 = 全开,已缓动)下框体该画成什么矩形。
///
/// - `origin`:原点,**相对 `target` 左上角**的像素坐标,钳进终矩形;
/// - `start`:起点小框的宽、高占终尺寸的比例(0..1)。
///
/// 起点是以原点为中心的小框,整体平移进终矩形里(贴边的原点 —— 菜单的光标角 —— 不会让
/// 小框悬在框外);之后四条边各自线性插值到终矩形的四条边。原点在顶边、起点高为 0 时就是
/// `popover_reveal::reveal_rect` 那条「从顶边往下长」;起点宽为 0、高为 1 时是纯横向展开。
pub fn reveal_rect_at(
    progress: f32,
    target: FrameRect,
    origin: (f32, f32),
    start: (f32, f32),
) -> FrameRect {
    let p = progress.clamp(0.0, 1.0);
    if p >= 1.0 {
        // 全开时就是终矩形本身(免得浮点误差让框体差出零点几像素)
        return target;
    }
    let w = target.width.max(0.0);
    let h = target.height.max(0.0);
    let start_w = w * start.0.clamp(0.0, 1.0);
    let start_h = h * start.1.clamp(0.0, 1.0);
    let ox = origin.0.clamp(0.0, w);
    let oy = origin.1.clamp(0.0, h);
    let left0 = (ox - start_w / 2.0).clamp(0.0, w - start_w);
    let top0 = (oy - start_h / 2.0).clamp(0.0, h - start_h);
    let right0 = left0 + start_w;
    let bottom0 = top0 + start_h;
    let left = left0 * (1.0 - p);
    let top = top0 * (1.0 - p);
    let right = right0 + (w - right0) * p;
    let bottom = bottom0 + (h - bottom0) * p;
    FrameRect {
        left: target.left + left,
        top: target.top + top,
        width: right - left,
        height: bottom - top,
    }
}

fn rect_of(bounds: Bounds<Pixels>) -> FrameRect {
    FrameRect {
        left: f32::from(bounds.left()),
        top: f32::from(bounds.top()),
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
    }
}

fn bounds_of(rect: FrameRect) -> Bounds<Pixels> {
    Bounds::new(
        point(px(rect.left), px(rect.top)),
        size(px(rect.width.max(0.0)), px(rect.height.max(0.0))),
    )
}

/// 四边各往里收(裁剪窗要扣掉框体边框);收过头时宽高落到 0。
fn inset_edges(rect: FrameRect, left: f32, top: f32, right: f32, bottom: f32) -> FrameRect {
    FrameRect {
        left: rect.left + left,
        top: rect.top + top,
        width: (rect.width - left - right).max(0.0),
        height: (rect.height - top - bottom).max(0.0),
    }
}

// ─── 框体元素 ────────────────────────────────────────────────

/// 原点怎么定。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RevealOrigin {
    /// 终矩形里的比例点:`(0.5, 0.0)` = 顶边水平中点,`(0.0, 0.5)` = 左缘中点。
    Fraction(f32, f32),
    /// 窗口坐标里的一点(光标点 / 触发钮),钳进终矩形。
    Window(Point<Pixels>),
}

/// 用 div 的样式构造器拼出框体外观(底色 / 边框 / 圆角 / 阴影),与 `.bg(..)` 等完全同值。
pub fn chrome_style(build: impl FnOnce(Div) -> Div) -> Style {
    let mut el = build(div());
    let mut style = Style::default();
    style.refine(el.style());
    style
}

/// 见模块注释第 2 条。用 [`reveal_box`] 构造。
pub struct RevealBox {
    child: AnyElement,
    progress: f32,
    origin: RevealOrigin,
    start: (f32, f32),
    chrome: Option<Style>,
    interactive: bool,
    occlude_native: bool,
    measure: Option<Rc<Cell<Option<Size<Pixels>>>>>,
}

/// 把 `child` 包成带开合动画的浮层,`progress` 是 [`Presence`] 这一帧的值。默认:原点在顶边
/// 水平中点、起点宽 10% 高 0(与 `popover_reveal` 同一语言)、只裁剪不画框体、可交互。
pub fn reveal_box(child: impl IntoElement, progress: f32) -> RevealBox {
    RevealBox {
        child: child.into_any_element(),
        progress,
        origin: RevealOrigin::Fraction(0.5, 0.0),
        start: (0.10, 0.0),
        chrome: None,
        interactive: true,
        occlude_native: false,
        measure: None,
    }
}

impl RevealBox {
    pub fn origin(mut self, origin: RevealOrigin) -> Self {
        self.origin = origin;
        self
    }

    /// 起点小框占终尺寸的比例(宽, 高)。
    pub fn start(mut self, width: f32, height: f32) -> Self {
        self.start = (width, height);
        self
    }

    /// 框体外观(见 [`chrome_style`])。内容自己就别再画底色 / 边框色 / 阴影了 —— 那几样
    /// 由这里按插值矩形画;边框宽度照留(版式要它),裁剪窗按它往里收。阴影浓度随进度走,
    /// 起点那个零高的小框不拖一团影子。
    pub fn chrome(mut self, style: Style) -> Self {
        self.chrome = Some(style);
        self
    }

    /// `false` = 退场中:框体上盖一块拦鼠标的 hitbox,正在收的内容不再接悬停与点击。
    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    /// `true` = 这一帧画出来的框体矩形登记成 HTML 预览 WebView 之上的不透明浮层
    /// ([`crate::native_view::occlude_rect`]):这块从网页的点击区域里让出来。给退场中的
    /// 模态外壳用 —— 它收起那一刻 WebView 就恢复收鼠标了,正在收的面板那块不能被网页抢走。
    pub fn occlude_native(mut self, occlude: bool) -> Self {
        self.occlude_native = occlude;
        self
    }

    /// 每帧把内容的最终尺寸(布局盒)写进 `cell`。弹窗外壳拿它在退场时把面板钉在
    /// 最后一帧开着时的尺寸上(见 `dialog_host::render_one`)。
    pub fn measure_into(mut self, cell: Rc<Cell<Option<Size<Pixels>>>>) -> Self {
        self.measure = Some(cell);
        self
    }
}

/// prepaint 交给 paint 的东西。
pub struct RevealBoxPrepaint {
    frame: Bounds<Pixels>,
    /// `None` = 全开静止:不另加裁剪,与不包这一层逐像素相同。
    mask: Option<Bounds<Pixels>>,
}

impl Element for RevealBox {
    type RequestLayoutState = ();
    type PrepaintState = RevealBoxPrepaint;

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
    ) -> (LayoutId, Self::RequestLayoutState) {
        // 布局整个交给内容:本元素的 bounds 就是内容的最终尺寸
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        if let Some(cell) = self.measure.as_ref() {
            cell.set(Some(bounds.size));
        }
        let target = rect_of(bounds);
        let origin = match self.origin {
            RevealOrigin::Fraction(fx, fy) => (target.width * fx, target.height * fy),
            RevealOrigin::Window(at) => {
                (f32::from(at.x) - target.left, f32::from(at.y) - target.top)
            }
        };
        let rect = reveal_rect_at(self.progress, target, origin, self.start);
        let frame = bounds_of(rect);
        let settled = self.interactive && self.progress >= 1.0;
        let mask = (!settled).then(|| {
            let border = self
                .chrome
                .as_ref()
                .map(|style| style.border_widths.to_pixels(window.rem_size()));
            let (l, t, r, b) = border.map_or((0.0, 0.0, 0.0, 0.0), |e| {
                (
                    f32::from(e.left),
                    f32::from(e.top),
                    f32::from(e.right),
                    f32::from(e.bottom),
                )
            });
            bounds_of(inset_edges(rect, l, t, r, b))
        });
        if self.interactive {
            // 垫在内容底下、按**终尺寸**拦:展开途中还没露出来的那块也不算「点外」
            window.insert_hitbox(bounds, HitboxBehavior::BlockMouse);
        }
        match mask {
            Some(mask) => {
                window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
                    self.child.prepaint(window, cx);
                });
            }
            None => {
                self.child.prepaint(window, cx);
            }
        }
        if !self.interactive {
            // 盖在内容上面:正在收的浮层不再接任何悬停与点击
            window.insert_hitbox(frame, HitboxBehavior::BlockMouse);
        }
        if self.occlude_native {
            crate::native_view::occlude_rect(frame, cx);
        }
        RevealBoxPrepaint { frame, mask }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mask = prepaint.mask;
        let child = &mut self.child;
        let mut paint_child = |window: &mut Window, cx: &mut App| match mask {
            Some(mask) => window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
                child.paint(window, cx);
            }),
            None => {
                child.paint(window, cx);
            }
        };
        let Some(chrome) = self.chrome.as_ref() else {
            paint_child(window, cx);
            return;
        };
        let mut style = chrome.clone();
        let strength = self.progress.clamp(0.0, 1.0);
        for shadow in style.box_shadow.iter_mut() {
            shadow.color.a *= strength;
        }
        // 底色 + 阴影 → 内容(裁剪窗内)→ 边框压在最上面(`Style::paint` 的顺序,与 div 自己画
        // 同一条路径,全开时与不包这一层逐像素相同)
        style.paint(prepaint.frame, window, cx, paint_child);
    }
}

// ─── 离场托管 ────────────────────────────────────────────────

/// 托管期比退场规格多留的一点余量:最后一帧(进度落到 0 的那帧)要真画出来,
/// 不能让定时器抢在它前面把实体丢掉。不是动画时长,只是收尾的宽限。
const LINGER_SLACK: Duration = Duration::from_millis(50);

/// 一条托管中的浮层。
struct Lingering {
    serial: u64,
    view: AnyView,
}

/// 离场托管层:把宿主已经丢掉、但退场动画还没播完的浮层实体多挂一会儿。
#[derive(Default)]
pub struct ExitStash {
    items: Vec<Lingering>,
    next: u64,
}

struct GlobalExitStash(Entity<ExitStash>);
impl Global for GlobalExitStash {}

/// 托管层实体(没有就现建)。根视图把它当子视图画出来 —— 托管的浮层都是
/// `deferred` + 窗口坐标 `anchored`,挂哪一层都画在原位。
pub fn exit_layer(cx: &mut App) -> Entity<ExitStash> {
    if let Some(global) = cx.try_global::<GlobalExitStash>() {
        return global.0.clone();
    }
    let entity = cx.new(|_| ExitStash::default());
    cx.set_global(GlobalExitStash(entity.clone()));
    entity
}

/// 托管层里还有没播完的浮层吗(毛玻璃快照的去留看它:退场途中又开浮层就沿用旧图,
/// 这时重抓会把正在收起的那个抓进去)。
pub fn lingering(cx: &App) -> bool {
    cx.try_global::<GlobalExitStash>()
        .is_some_and(|global| !global.0.read(cx).items.is_empty())
}

/// 让 `view` 再活 `exit` 这么久(加一点收尾余量),期间由托管层画它。
///
/// 调用方是浮层自己:关闭那一刻先还焦点、摘覆盖物栈登记、播退场,再调这里,最后才发
/// 关闭事件(宿主收到就 drop 自己那份句柄,实体靠这里的强引用活到退场播完)。
/// ⚠️ 宿主收到关闭事件**必须**丢掉自己那份(不再画它),否则同一实体一帧挂两次。
pub fn linger(view: AnyView, exit: Duration, cx: &mut App) {
    let stash = exit_layer(cx);
    let serial = stash.update(cx, |stash, cx| {
        stash.next += 1;
        let serial = stash.next;
        stash.items.push(Lingering { serial, view });
        cx.notify();
        serial
    });
    cx.spawn(async move |cx| {
        cx.background_executor().timer(exit + LINGER_SLACK).await;
        stash.update(cx, |stash, cx| {
            stash.items.retain(|item| item.serial != serial);
            cx.notify();
        });
    })
    .detach();
}

impl Render for ExitStash {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        div().children(self.items.iter().map(|item| item.view.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mt_ui::motion::{Easing, OVERLAY_IN, OVERLAY_OUT};

    fn target() -> FrameRect {
        FrameRect {
            left: 10.0,
            top: 20.0,
            width: 400.0,
            height: 300.0,
        }
    }

    /// 顶边中点 + 起点 10% 宽 0 高:与 `popover_reveal::reveal_rect` 原点居中时逐值相同。
    #[test]
    fn 顶边中点展开与_popover_reveal_同一语言() {
        for step in 0..=20 {
            let p = step as f32 / 20.0;
            let ours = reveal_rect_at(p, target(), (200.0, 0.0), (0.10, 0.0));
            let theirs = crate::popover_reveal::reveal_rect(p, target(), 200.0);
            assert!((ours.left - theirs.left).abs() < 1e-3, "{p}");
            assert!((ours.width - theirs.width).abs() < 1e-3, "{p}");
            assert!((ours.height - theirs.height).abs() < 1e-3, "{p}");
            assert_eq!(ours.top, theirs.top, "顶边不动");
        }
    }

    /// 起点小框贴在原点所在的那个角上(不悬在框外),四边单调走向终矩形,终点严丝合缝。
    #[test]
    fn 从角上长出来_起点小框钳在框内() {
        let t = target();
        // 右下角(菜单贴边翻转后光标落在右下角)
        let start = reveal_rect_at(0.0, t, (400.0, 300.0), (0.2, 0.1));
        assert!((start.left + start.width - (t.left + t.width)).abs() < 1e-3);
        assert!((start.top + start.height - (t.top + t.height)).abs() < 1e-3);
        assert!((start.width - 80.0).abs() < 1e-3 && (start.height - 30.0).abs() < 1e-3);
        // 原点在框外也钳到边上
        let outside = reveal_rect_at(0.0, t, (-50.0, -70.0), (0.2, 0.1));
        assert_eq!((outside.left, outside.top), (t.left, t.top));
        let mut prev = start;
        for step in 1..=20 {
            let rect = reveal_rect_at(step as f32 / 20.0, t, (400.0, 300.0), (0.2, 0.1));
            assert!(
                rect.left <= prev.left && rect.top <= prev.top,
                "左上两边只往外走"
            );
            assert!(rect.width >= prev.width && rect.height >= prev.height);
            prev = rect;
        }
        assert_eq!(reveal_rect_at(1.0, t, (400.0, 300.0), (0.2, 0.1)), t);
        assert_eq!(
            reveal_rect_at(3.0, t, (0.0, 0.0), (0.0, 0.0)),
            t,
            "越界进度钳住"
        );
    }

    /// 起点宽 0、高 1:纯横向展开(子菜单从父项那一侧飞出),高度全程不变。
    #[test]
    fn 纯横向展开高度不变() {
        let t = target();
        for step in 0..20 {
            let p = step as f32 / 20.0;
            let rect = reveal_rect_at(p, t, (0.0, 150.0), (0.0, 1.0));
            assert_eq!(rect.left, t.left, "左缘钉在父项那一侧");
            assert_eq!((rect.top, rect.height), (t.top, t.height));
            assert!((rect.width - t.width * p).abs() < 1e-3);
        }
    }

    #[test]
    fn 裁剪窗按四边边框往里收() {
        let rect = FrameRect {
            left: 0.0,
            top: 0.0,
            width: 10.0,
            height: 1.0,
        };
        let inner = inset_edges(rect, 1.0, 1.0, 2.0, 1.0);
        assert_eq!((inner.left, inner.top, inner.width), (1.0, 1.0, 7.0));
        assert_eq!(inner.height, 0.0, "收过头时落到 0,不出负数");
    }

    /// 进出各走各的规格;半路掉头从当前进度接着走,时长按剩余路程折算。
    #[test]
    fn 在场度进出场与半路掉头() {
        let t0 = Instant::now();
        let mut p = Presence::hidden(OVERLAY_IN, OVERLAY_OUT);
        assert!(!p.mounted_at(t0) && !p.target_open());
        p.retarget(1.0, t0, true);
        assert!(p.mounted_at(t0) && p.running_at(t0));
        assert_eq!(p.value_at(t0 + OVERLAY_IN.duration), 1.0);
        assert!(!p.running_at(t0 + OVERLAY_IN.duration), "播完就不再要帧");

        let mid = t0 + OVERLAY_IN.duration / 4;
        let before = p.value_at(mid);
        assert!(before > 0.0 && before < 1.0);
        p.retarget(0.0, mid, true);
        assert!((p.value_at(mid) - before).abs() < 1e-6, "掉头那一刻不跳变");
        assert_eq!(
            p.remaining_for_test(mid),
            OVERLAY_OUT.duration.mul_f32(before),
            "只走回去那一段"
        );
        let end = mid + OVERLAY_OUT.duration.mul_f32(before);
        assert_eq!(p.value_at(end), 0.0);
        assert!(!p.mounted_at(end), "收起播完才从树上卸下");

        // 早就开着的一层收起:从 1 起播,走退场曲线
        let mut q = Presence::hidden(OVERLAY_IN, OVERLAY_OUT);
        q.retarget(1.0, t0, false);
        assert!(q.mounted_at(t0) && !q.running_at(t0));
        q.retarget(0.0, t0, true);
        let half = t0 + OVERLAY_OUT.duration / 2;
        let expected = 1.0 - ease(Easing::EaseOverlayOut, 0.5);
        assert!((q.value_at(half) - expected).abs() < 1e-4);

        // 总开关关掉:直接到位
        let mut r = Presence::hidden(OVERLAY_IN, OVERLAY_OUT);
        r.retarget(1.0, t0, false);
        assert_eq!(r.value_at(t0), 1.0);
        r.retarget(0.0, t0, false);
        assert!(!r.mounted_at(t0));
    }

    impl Presence {
        fn remaining_for_test(&self, now: Instant) -> Duration {
            (self.start + self.duration).saturating_duration_since(now)
        }
    }
}
