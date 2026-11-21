//! 浮层开合动画的公共件:SSH 交互悬浮框、命令库浮层、终端查找条三处共用。
//!
//! # 效果(用户口径)
//!
//! 打开:浮层的可见框体(底色 / 边框 / 阴影)从「对应标题栏图标的水平中心、浮层顶边」那一点
//! 长出来 —— 起点是以原点为中心、宽 [`REVEAL_START_W`]、高 [`REVEAL_START_H`] 比例的小框,
//! 左右两边各自线性插值到最终的左、右边(原点不在中间时不对称),高度从顶边往下长;
//! ease-out [`REVEAL_OPEN`]。收起完全反向收回那一点,ease-in [`REVEAL_CLOSE`],播完才卸载。
//! 动画中途再点从当前进度反向播,不跳变([`Reveal::retarget`])。
//!
//! 只有位移与尺寸的方向性展开,**不做透明度淡入淡出**;只在动画进行中要帧,播完就停
//! (不用 `with_animation(..).repeat()`);豁免系统「减少动画」(与其它浮层进出场一致,
//! `respects_reduce = false` 的口径),但服从设置里的终端动画总开关 `terminal_animations`
//! ([`animations_enabled`]),关掉时直接显示、直接收起。
//!
//! # 怎么画:[`RevealFrame`] 元素
//!
//! gpui 的 div 没有缩放变换。这里用一个自定义元素包住浮层内容:
//! - **布局**完全交给内容,按最终(自然)尺寸排 —— 外壳从第一帧起就占满最终尺寸,同列 / 同层的
//!   别的东西不会跟着动画跳,文字也不重排、不缩放,虚拟列表的滚动位置不受影响;
//! - **框体**(底色 / 边框 / 阴影,[`Chrome`])按 [`reveal_rect`] 插值出的矩形画;
//! - **内容**原地不动,只被一个同样大小(扣掉边框)的裁剪窗(content mask)裁掉 —— 裁剪窗外的
//!   内容既不显示、也接不到鼠标(gpui 的命中测试与 content mask 求交)。
//!
//! 原点的横坐标是窗口坐标,元素在 prepaint 里拿自己的实际位置换算成相对值,不等下一帧。
//!
//! # 原点:标题栏按钮上的探针
//!
//! [`anchor_probe`] 是一个不画东西、不插 hitbox 的 canvas,挂在标题栏按钮里,把按钮的窗口坐标
//! 报给持有 [`RevealState`] 的那个视图。只有坐标变了、且浮层开着或在动画中时才在下一帧
//! notify **那个视图**(`on_next_frame`)—— 按钮平时挪位不惊动任何人,也不会让 TerminalArea
//! 每帧重画。

use std::time::{Duration, Instant};

use gpui::{
    AbsoluteLength, AnyElement, App, Bounds, ContentMask, DispatchPhase, Element, ElementId,
    Entity, GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement,
    LayoutId, MouseMoveEvent, Pixels, Refineable as _, Style, Styled, Window, canvas, div, point,
    px, size,
};
use mt_ui::motion::{Easing, cubic_bezier_at, ease};

/// 打开动画的全程时长(ease-out)。半路掉头时按剩余路程折算。
pub const REVEAL_OPEN: Duration = Duration::from_millis(220);
/// 收起动画的全程时长(ease-in)。
pub const REVEAL_CLOSE: Duration = Duration::from_millis(155);
/// 起点小框的宽 = 终宽 × 这个比例(以原点为中心)。
pub const REVEAL_START_W: f32 = 0.10;
/// 起点小框的高 = 终高 × 这个比例(从顶边往下长)。
pub const REVEAL_START_H: f32 = 0.0;
/// 收起用的 ease-in 曲线控制点(CSS 关键字 `ease-in`;`mt_ui::motion::Easing` 里没有这一条)。
const EASE_IN: (f32, f32, f32, f32) = (0.42, 0.0, 1.0, 1.0);

// ─── 纯数:插值矩形 ───────────────────────────────────────────

/// 一个矩形(像素,f32)。插值在纯数里做,便于单测。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameRect {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

impl FrameRect {
    fn from_bounds(bounds: Bounds<Pixels>) -> Self {
        Self {
            left: f32::from(bounds.left()),
            top: f32::from(bounds.top()),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
        }
    }

    fn to_bounds(self) -> Bounds<Pixels> {
        Bounds::new(
            point(px(self.left), px(self.top)),
            size(px(self.width.max(0.0)), px(self.height.max(0.0))),
        )
    }

    /// 四边各往里收 `by`(裁剪窗要扣掉框体边框)。收过头时宽高落到 0。
    pub fn inset(self, by: f32) -> Self {
        Self {
            left: self.left + by,
            top: self.top + by,
            width: (self.width - 2.0 * by).max(0.0),
            height: (self.height - 2.0 * by).max(0.0),
        }
    }
}

/// 开合进度 `progress`(0 = 收起,1 = 全开,已缓动)下,可见框体该画成什么矩形。
///
/// `origin_x` 是原点的横坐标,**相对 `target.left`**,钳在终矩形左右边之间(pane 很窄时按钮
/// 可能落在框外);原点的纵坐标就是终矩形顶边。起点是以原点为中心的小框,左右两边各自线性
/// 插值到终矩形的左、右边,高度从顶边往下长 —— 顶边始终不动。
pub fn reveal_rect(progress: f32, target: FrameRect, origin_x: f32) -> FrameRect {
    let p = progress.clamp(0.0, 1.0);
    if p >= 1.0 {
        // 全开时就是终矩形本身(免得浮点误差让框体差出零点几像素)
        return target;
    }
    let origin = origin_x.clamp(0.0, target.width.max(0.0));
    let half = target.width * REVEAL_START_W / 2.0;
    let left = (origin - half) * (1.0 - p);
    let right = (origin + half) + (target.width - (origin + half)) * p;
    let height = target.height * (REVEAL_START_H + (1.0 - REVEAL_START_H) * p);
    FrameRect {
        left: target.left + left,
        top: target.top,
        width: right - left,
        height,
    }
}

// ─── 进度 ────────────────────────────────────────────────────

/// 开合进度(0 = 收起,1 = 全开)。
///
/// 不用 `mt_ui::motion::Transition`:它的 `restart` 刻意从头播,而这里要求动画中途再点时
/// **从当前进度反向播、不许跳变**。于是记「起点值 → 目标值 + 起始时刻 + 时长」,掉头时以
/// 当前显示值为新起点,时长按剩余路程折算。打开 ease-out、收起 ease-in。
#[derive(Clone, Copy, Debug)]
pub struct Reveal {
    from: f32,
    to: f32,
    start: Instant,
    duration: Duration,
}

impl Reveal {
    /// 收起且静止。
    pub fn hidden(now: Instant) -> Self {
        Self {
            from: 0.0,
            to: 0.0,
            start: now,
            duration: Duration::ZERO,
        }
    }

    /// 目标是全开吗(按钮亮不亮看它)。
    pub fn target_open(&self) -> bool {
        self.to > 0.0
    }

    /// 这一刻的开合进度(已缓动)。
    pub fn value_at(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.start);
        if self.duration.is_zero() || elapsed >= self.duration {
            return self.to;
        }
        let t = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        let eased = if self.to >= self.from {
            ease(Easing::EaseOut, t)
        } else {
            let (x1, y1, x2, y2) = EASE_IN;
            cubic_bezier_at(x1, y1, x2, y2, t)
        };
        self.from + (self.to - self.from) * eased
    }

    /// 还在动吗(= 还要不要请求下一帧)。到终态之后恒 `false`。
    pub fn running_at(&self, now: Instant) -> bool {
        !self.duration.is_zero() && now.saturating_duration_since(self.start) < self.duration
    }

    /// 还在树上吗:开着、或者收起动画还没播完。
    pub fn mounted_at(&self, now: Instant) -> bool {
        self.value_at(now) > 0.0 || self.running_at(now)
    }

    /// 从现在起还要动多久(0 = 已静止)。
    pub fn remaining_at(&self, now: Instant) -> Duration {
        (self.start + self.duration).saturating_duration_since(now)
    }

    /// 朝 `target`(0 / 1)走。从**当前显示值**起步,时长按剩余路程折算;
    /// `animate = false` 时直接到位。
    pub fn retarget(&mut self, target: f32, now: Instant, animate: bool) {
        let current = self.value_at(now);
        let full = if target > current {
            REVEAL_OPEN
        } else {
            REVEAL_CLOSE
        };
        self.from = current;
        self.to = target;
        self.start = now;
        self.duration = if animate {
            full.mul_f32((target - current).abs().min(1.0))
        } else {
            Duration::ZERO
        };
    }
}

/// 动画总开关(设置页「启用动画」,缺省开;管全部动画)。真身是
/// [`mt_ui::motion::animations_enabled`],由 `crate::motion::sync_switch` 跟配置同步。
pub fn animations_enabled(_cx: &App) -> bool {
    mt_ui::motion::animations_enabled()
}

/// 一个浮层的开合状态 + 原点(标题栏按钮的窗口坐标)。挂在负责画这个浮层的视图上。
#[derive(Clone, Copy, Debug)]
pub struct RevealState {
    reveal: Reveal,
    anchor: Option<Bounds<Pixels>>,
}

impl Default for RevealState {
    fn default() -> Self {
        Self::new()
    }
}

/// 这一帧该怎么画浮层(见 [`RevealState::frame`])。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RevealFrameState {
    /// 开合进度(已缓动)。
    pub progress: f32,
    /// 原点横坐标(窗口坐标);按钮还没测到时 `None`,退回水平中点。
    pub origin_x: Option<Pixels>,
    /// 目标是全开(收起动画里是 `false`:内容不该再接鼠标)。
    pub open: bool,
    /// 全开且静止。
    pub settled: bool,
}

impl RevealState {
    /// 收起且静止。
    pub fn new() -> Self {
        Self {
            reveal: Reveal::hidden(Instant::now()),
            anchor: None,
        }
    }

    /// 播打开动画(总开关关着时直接到位)。收起到一半时从当前进度往回长。
    pub fn show(&mut self, cx: &App) {
        self.reveal
            .retarget(1.0, Instant::now(), animations_enabled(cx));
    }

    /// 播收起动画(总开关关着时直接收起)。打开到一半时从当前进度往回收。
    pub fn hide(&mut self, cx: &App) {
        self.reveal
            .retarget(0.0, Instant::now(), animations_enabled(cx));
    }

    /// 直接收起,不播动画(浮层挂的 pane / pty 已经没了)。
    pub fn hide_now(&mut self) {
        self.reveal = Reveal::hidden(Instant::now());
    }

    /// 目标是全开吗。
    #[cfg(test)]
    pub fn is_shown(&self) -> bool {
        self.reveal.target_open()
    }

    /// 还在树上吗(开着,或收起动画没播完)。
    pub fn is_mounted(&self) -> bool {
        self.reveal.mounted_at(Instant::now())
    }

    /// 收起动画还要播多久(打开中 / 静止时为 0)。
    pub fn hide_remaining(&self) -> Duration {
        if self.reveal.target_open() {
            Duration::ZERO
        } else {
            self.reveal.remaining_at(Instant::now())
        }
    }

    /// 标题栏按钮上的探针报来的按钮窗口坐标。返回「变了、且浮层开着或在动画中」——
    /// 只有那时才需要再要一帧;平时按钮挪了只记下来。
    pub fn note_anchor(&mut self, bounds: Bounds<Pixels>) -> bool {
        if self.anchor == Some(bounds) {
            return false;
        }
        self.anchor = Some(bounds);
        self.is_mounted()
    }

    /// render 里调:没挂着返回 `None`(浮层不画);动画还在跑就顺手要下一帧
    /// (`request_animation_frame` 通知的是**当前视图**,所以必须在画这个浮层的视图的
    /// render 调用链里调)。
    pub fn frame(&self, window: &Window) -> Option<RevealFrameState> {
        let now = Instant::now();
        if !self.reveal.mounted_at(now) {
            return None;
        }
        let running = self.reveal.running_at(now);
        if running {
            window.request_animation_frame();
        }
        let progress = self.reveal.value_at(now);
        Some(RevealFrameState {
            progress,
            origin_x: self
                .anchor
                .map(|anchor| anchor.left() + anchor.size.width / 2.0),
            open: self.reveal.target_open(),
            settled: !running && progress >= 1.0,
        })
    }
}

// ─── 原点探针 ────────────────────────────────────────────────

/// 标题栏按钮上的原点探针:absolute 全尺寸、不画东西、不插 hitbox。
///
/// 每次按钮所在的视图画到它时,把按钮的窗口坐标交给 `note`(在 `entity` 上调);`note` 返回
/// `true`(坐标变了且浮层开着 / 在动画中)时下一帧 notify `entity` —— 只惊动持有浮层的那个视图。
pub fn anchor_probe<T: 'static>(
    entity: Entity<T>,
    note: impl FnOnce(&mut T, Bounds<Pixels>) -> bool + 'static,
) -> impl IntoElement {
    canvas(
        move |bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App| {
            if entity.update(cx, |this, _| note(this, bounds)) {
                window.on_next_frame(move |_, cx| entity.update(cx, |_, cx| cx.notify()));
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

// ─── 框体元素 ────────────────────────────────────────────────

/// 阴影什么时候画。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShadowMode {
    Always,
    /// 鼠标在框体上才画(SSH 悬浮框的实底态)。
    OnHover,
}

/// 框体的外观:按插值矩形画的底色 / 边框 / 阴影,外加要不要拦鼠标。
#[derive(Clone, Copy, Debug)]
pub struct Chrome {
    pub bg: Hsla,
    /// 鼠标在框体上时换的底色(`None` = 不随悬停变)。
    pub hover_bg: Option<Hsla>,
    pub border_color: Hsla,
    pub border_width: Pixels,
    pub radius: AbsoluteLength,
    pub shadow: ShadowMode,
    /// 框体拦鼠标(滚轮 / 点击不穿透到下面的终端,也不算「点外面」)。
    pub occlude: bool,
}

impl Chrome {
    fn hover_dependent(&self) -> bool {
        self.hover_bg.is_some() || self.shadow == ShadowMode::OnHover
    }

    fn style(&self, hovered: bool) -> Style {
        let bg = match self.hover_bg {
            Some(hover_bg) if hovered => hover_bg,
            _ => self.bg,
        };
        let shadow = match self.shadow {
            ShadowMode::Always => true,
            ShadowMode::OnHover => hovered,
        };
        // 借 div 的样式构造器拿到与 `.shadow_lg()` 等完全相同的取值
        let mut el = div()
            .bg(bg)
            .border(self.border_width)
            .border_color(self.border_color)
            .rounded(self.radius);
        if shadow {
            el = el.shadow_lg();
        }
        let mut style = Style::default();
        style.refine(el.style());
        style
    }
}

/// 见模块注释「怎么画」。用 [`reveal_frame`] 构造。
pub struct RevealFrame {
    child: AnyElement,
    state: RevealFrameState,
    chrome: Option<Chrome>,
}

/// 把 `child` 包成带开合动画的浮层。`chrome` 为 `None` 时只裁剪、不画框体。
pub fn reveal_frame(
    child: impl IntoElement,
    state: RevealFrameState,
    chrome: Option<Chrome>,
) -> RevealFrame {
    RevealFrame {
        child: child.into_any_element(),
        state,
        chrome,
    }
}

/// prepaint 交给 paint 的东西。
pub struct RevealFramePrepaint {
    frame: Bounds<Pixels>,
    mask: Bounds<Pixels>,
    hitbox: Option<Hitbox>,
}

impl IntoElement for RevealFrame {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RevealFrame {
    type RequestLayoutState = ();
    type PrepaintState = RevealFramePrepaint;

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
        // 布局整个交给内容:本元素的 bounds 就是内容的最终(自然)尺寸
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
        let target = FrameRect::from_bounds(bounds);
        let origin = self
            .state
            .origin_x
            .map_or(target.width / 2.0, |x| f32::from(x) - target.left);
        let rect = reveal_rect(self.state.progress, target, origin);
        let border = self
            .chrome
            .as_ref()
            .map_or(0.0, |chrome| f32::from(chrome.border_width));
        let frame = rect.to_bounds();
        let mask = rect.inset(border).to_bounds();

        let occlude = self.chrome.as_ref().is_some_and(|chrome| chrome.occlude);
        let hover = self.chrome.as_ref().is_some_and(Chrome::hover_dependent);
        // 开着:框体垫在内容底下(内容照常接鼠标,框体挡住下面的终端);
        // 收起动画里:盖在内容上面,正在收的浮层不再接任何点击
        let mut hitbox = if self.state.open && (occlude || hover) {
            Some(window.insert_hitbox(
                frame,
                if occlude {
                    HitboxBehavior::BlockMouse
                } else {
                    HitboxBehavior::Normal
                },
            ))
        } else {
            None
        };
        window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
            self.child.prepaint(window, cx);
        });
        if !self.state.open {
            hitbox = Some(window.insert_hitbox(frame, HitboxBehavior::BlockMouse));
        }
        RevealFramePrepaint {
            frame,
            mask,
            hitbox,
        }
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
        let mask = ContentMask {
            bounds: prepaint.mask,
        };
        let Some(chrome) = self.chrome else {
            window.with_content_mask(Some(mask), |window| self.child.paint(window, cx));
            return;
        };
        let hovered = prepaint
            .hitbox
            .as_ref()
            .is_some_and(|hitbox| hitbox.is_hovered(window));
        // 底色 + 阴影 → 内容(裁剪窗内)→ 边框压在最上面(`Style::paint` 的顺序)
        chrome
            .style(hovered)
            .paint(prepaint.frame, window, cx, |window, cx| {
                window.with_content_mask(Some(mask), |window| self.child.paint(window, cx));
            });
        // 悬停态变了要重画(与 gpui div 的 hover 样式同一手法)
        if chrome.hover_dependent()
            && let Some(hitbox) = prepaint.hitbox.clone()
        {
            let view = window.current_view();
            window.on_mouse_event(move |_: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture && hitbox.is_hovered(window) != hovered {
                    cx.notify(view);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 开合矩形从原点展开() {
        let target = FrameRect {
            left: 10.0,
            top: 20.0,
            width: 400.0,
            height: 300.0,
        };
        let half = 400.0 * REVEAL_START_W / 2.0;
        // 原点在左侧 / 正中 / 右侧(相对终矩形左边)
        for origin in [60.0_f32, 200.0, 360.0] {
            // 起点:以原点为中心的小框,顶边就是终矩形顶边
            let start = reveal_rect(0.0, target, origin);
            assert_eq!(start.top, 20.0);
            assert_eq!(start.height, 300.0 * REVEAL_START_H);
            assert!((start.width - 2.0 * half).abs() < 1e-3);
            let center = start.left + start.width / 2.0;
            assert!(
                (center - (10.0 + origin)).abs() < 1e-3,
                "起点小框以原点为中心"
            );
            // 终点就是终矩形;越界进度钳住
            assert_eq!(reveal_rect(1.0, target, origin), target);
            assert_eq!(reveal_rect(1.7, target, origin), target);
            assert_eq!(reveal_rect(-0.3, target, origin), start);
            // 全程:顶边不动,左右两边各自单调走向终矩形的左、右边,宽高单调长大
            let mut prev = start;
            for step in 1..=20 {
                let rect = reveal_rect(step as f32 / 20.0, target, origin);
                assert_eq!(rect.top, target.top, "高度从上往下展开,顶边不动");
                assert!(rect.left <= prev.left, "左边只往左走");
                assert!(
                    rect.left + rect.width >= prev.left + prev.width,
                    "右边只往右走"
                );
                assert!(rect.width > prev.width && rect.height > prev.height);
                assert!(rect.width <= target.width && rect.height <= target.height);
                prev = rect;
            }
        }
        // 原点不在中间时两边走的路程不同(不对称):原点偏左,半程时左边离终点更近
        let mid = reveal_rect(0.5, target, 60.0);
        let left_gap = mid.left - target.left;
        let right_gap = (target.left + target.width) - (mid.left + mid.width);
        assert!(left_gap < right_gap);
        // 原点落在框外(pane 很窄、按钮跑到框左 / 右边去了):钳到框的左右边
        let clamped_left = reveal_rect(0.0, target, -80.0);
        assert!((clamped_left.left + clamped_left.width / 2.0 - 10.0).abs() < 1e-3);
        let clamped_right = reveal_rect(0.0, target, 900.0);
        assert!((clamped_right.left + clamped_right.width / 2.0 - 410.0).abs() < 1e-3);
        assert_eq!(reveal_rect(1.0, target, -80.0), target);
    }

    #[test]
    fn 裁剪窗扣掉边框() {
        let rect = FrameRect {
            left: 10.0,
            top: 20.0,
            width: 100.0,
            height: 1.0,
        };
        assert_eq!(
            rect.inset(1.0),
            FrameRect {
                left: 11.0,
                top: 21.0,
                width: 98.0,
                height: 0.0,
            },
            "收过头时高度落到 0,不出负数"
        );
    }

    #[test]
    fn 开合进度与半路掉头() {
        let t0 = Instant::now();
        let mut reveal = Reveal::hidden(t0);
        assert!(!reveal.mounted_at(t0));
        assert!(!reveal.target_open());

        // 打开:ease-out,从 0 单调走到 1,跑满 REVEAL_OPEN 就停
        reveal.retarget(1.0, t0, true);
        assert!(reveal.target_open());
        assert_eq!(reveal.value_at(t0), 0.0);
        assert!(reveal.running_at(t0) && reveal.mounted_at(t0));
        let mut prev = 0.0;
        for ms in (10..=220).step_by(10) {
            let v = reveal.value_at(t0 + Duration::from_millis(ms));
            assert!(v >= prev, "打开过程单调");
            prev = v;
        }
        let opened = t0 + REVEAL_OPEN;
        assert_eq!(reveal.value_at(opened), 1.0);
        assert!(!reveal.running_at(opened), "播完就不再要帧");
        assert_eq!(reveal.remaining_at(opened), Duration::ZERO);

        // 半路掉头:打开到一半再点,从当前进度反向收,不跳变
        let mut reveal = Reveal::hidden(t0);
        reveal.retarget(1.0, t0, true);
        let mid = t0 + Duration::from_millis(80);
        let before = reveal.value_at(mid);
        assert!(before > 0.0 && before < 1.0);
        reveal.retarget(0.0, mid, true);
        assert!(
            (reveal.value_at(mid) - before).abs() < 1e-6,
            "掉头那一刻不跳变"
        );
        // 只走回去那一段:时长按剩余路程折算
        let back = mid + REVEAL_CLOSE.mul_f32(before);
        assert_eq!(reveal.remaining_at(mid), REVEAL_CLOSE.mul_f32(before));
        assert!(reveal.running_at(back - Duration::from_millis(1)));
        assert_eq!(reveal.value_at(back), 0.0);
        assert!(!reveal.running_at(back));
        assert!(!reveal.mounted_at(back), "收起播完才从树上卸下");

        // 收起途中再点开:同样从当前进度接着往回长
        let mut reveal = Reveal::hidden(t0);
        reveal.retarget(1.0, t0, false);
        reveal.retarget(0.0, t0, true);
        let half = t0 + Duration::from_millis(70);
        let shown = reveal.value_at(half);
        assert!(shown > 0.0 && shown < 1.0 && reveal.mounted_at(half));
        reveal.retarget(1.0, half, true);
        assert!((reveal.value_at(half) - shown).abs() < 1e-6);
    }

    #[test]
    fn 动画总开关关掉时直接开合() {
        let t0 = Instant::now();
        let mut reveal = Reveal::hidden(t0);
        reveal.retarget(1.0, t0, false);
        assert_eq!(reveal.value_at(t0), 1.0);
        assert!(!reveal.running_at(t0));
        reveal.retarget(0.0, t0, false);
        assert_eq!(reveal.value_at(t0), 0.0);
        assert!(!reveal.mounted_at(t0));
    }

    #[test]
    fn 原点探针只在浮层挂着时要帧() {
        let bounds = |x: f32| Bounds::new(point(px(x), px(4.0)), size(px(30.0), px(22.0)));
        let mut state = RevealState::new();
        // 收起时按钮挪位:只记下,不要帧
        assert!(!state.note_anchor(bounds(100.0)));
        assert!(!state.note_anchor(bounds(100.0)), "没变不算");
        // 挂着时变了才要帧
        state.reveal.retarget(1.0, Instant::now(), false);
        assert!(state.is_shown() && state.is_mounted());
        assert!(!state.note_anchor(bounds(100.0)));
        assert!(state.note_anchor(bounds(140.0)));
        // 直接收起
        state.hide_now();
        assert!(!state.is_mounted() && !state.is_shown());
        assert!(!state.note_anchor(bounds(180.0)));
        assert_eq!(state.hide_remaining(), Duration::ZERO);
    }
}
