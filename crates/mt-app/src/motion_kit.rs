//! 非终端内容的方向性换场公共件:整幅 push(新内容推入、旧内容同向推出)。
//!
//! 终端区那套(`terminal_area.rs` 文件头「方向性换场」)要顾 PTY 尺寸与「同一终端实体
//! 不许同帧双挂」,自成一体;这里是给**普通内容**用的同款形态 —— 抽屉换面板、设置页
//! 切页、会话面板下钻、文件页「预览 / 源码」、日期翻月这类。
//!
//! # 口径(用户定稿,别回退)
//!
//! - **方向性位移,不做透明度**:opacity 从 0 起播读作「闪一下」,被否掉过;
//! - **方向跟随入口控件的排列轴**:横排的段控件 / 页签 → 左右推,竖排的导航 → 上下推;
//!   往「后面」那一项切,新内容从末端(右 / 下)进、旧内容向起端(左 / 上)出,反向镜像;
//! - **零尺寸变化**:两层都是 absolute 全尺寸,只动 `left` / `top`(容器尺寸的分数),
//!   内容不重排;
//! - 豁免系统「减少动画」、服从「启用动画」总开关 —— 规格走 `mt_ui::motion`
//!   ([`mt_ui::motion::PANEL_SWAP`] 等),[`KeyedSwap`] 由 `ValueTween` 天然过闸。
//!
//! # 怎么用
//!
//! ```ignore
//! // 视图字段
//! page_swap: KeyedSwap<Page>,
//!
//! // render 里:先报这一帧谁在台上(变了就起一场),再取这一帧的出场信息
//! self.page_swap.observe(&self.page, |old, new| if new.index() > old.index() { 1.0 } else { -1.0 });
//! let body = match self.page_swap.frame(window) {
//!     Some((old, progress, dir)) => push_pair(
//!         self.render_page(new, ..),
//!         Some(self.render_page(old, ..)),
//!         progress, PushAxis::Vertical, dir,
//!     ).into_any_element(),
//!     None => self.render_page(new, ..),
//! };
//! ```
//!
//! ⚠️ 出场层画的是**旧内容的第二份**:同一个 gpui 实体(`Entity<V>`)不能在一帧里挂两次,
//! 旧内容是实体时只能挂「新旧两个不同实体」(抽屉的会话 / Git 两个面板就是),
//! 否则只播进场(`exit = None`)。
//!
//! ⚠️ [`push_pair`] 的容器是 `size_full` + 两个 absolute 层:**父级必须给出确定尺寸**
//! (flex_1 / size_full 链条),否则容器塌成 0 高。内容高度由内容决定的场合(滚动列表里
//! 的一段)别用它。

use std::time::Instant;

use gpui::{
    AnyElement, App, Bounds, Div, Element, ElementId, GlobalElementId, InspectorElementId,
    InteractiveElement, IntoElement, LayoutId, ParentElement, Pixels, Styled, Window, div, point,
    px,
};
use mt_ui::motion::{TransitionSpec, ValueTween};

/// 推移的轴。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushAxis {
    /// 左右推(横排的入口控件)。
    Horizontal,
    /// 上下推(竖排的入口控件)。
    Vertical,
}

/// push 这一帧两层的位置(容器尺寸的分数):`(进场层, 出场层)`。
///
/// `dir = +1`:新内容从 `+1.0`(整幅在右 / 下侧屏外)推进到 `0`,旧内容从 `0` 推出到
/// `-1.0` —— 两层首尾相接,像一条整带往起端拉。`dir = -1` 镜像。**纯函数**,端点单测钉住:
/// 终点残留零点几会让内容永久错位。
pub fn push_offsets(dir: f32, progress: f32) -> (f32, f32) {
    let p = progress.clamp(0.0, 1.0);
    let dir = if dir < 0.0 { -1.0 } else { 1.0 };
    if p >= 1.0 {
        return (0.0, -dir);
    }
    (dir * (1.0 - p), -dir * p)
}

/// 把新旧两份内容摆成一帧 push。见模块注释的「怎么用」与两条 ⚠️。
///
/// 出场层上面盖一张拦鼠标的透明罩:旧内容只是在退场,不该再接悬停与点击。
pub fn push_pair(
    enter: impl IntoElement,
    exit: Option<AnyElement>,
    progress: f32,
    axis: PushAxis,
    dir: f32,
) -> Div {
    let (enter_at, exit_at) = push_offsets(dir, progress);
    let place = |el: Div, at: f32| match axis {
        PushAxis::Horizontal => el.top_0().left(gpui::relative(at)),
        PushAxis::Vertical => el.left_0().top(gpui::relative(at)),
    };
    div()
        .relative()
        .size_full()
        .overflow_hidden()
        .children(exit.map(|old| {
            place(div().absolute().size_full(), exit_at)
                .child(old)
                .child(div().absolute().inset_0().occlude())
        }))
        .child(place(div().absolute().size_full(), enter_at).child(enter))
}

/// 只挪**绘制位置**(连同命中框)、不改排版的包装:prepaint 时把子树整体平移 `(dx, dy)`
/// 像素(`with_element_offset`,与滚动容器挪子元素同一个口子)。
///
/// 与 `relative().top(..)` 的区别:后者改的是排版位置,放进滚动容器里会把容器的内容尺寸
/// (gpui 按**直接子元素的排版包围盒**算)一起撑大 —— 入场位移那几百毫秒里滚动范围凭空
/// 变大。这里排版原封不动,滚动范围、兄弟元素的位置都不受影响;画出去的部分由祖先的
/// 裁剪(滚动容器 / `overflow_hidden`)裁掉。
pub fn shifted(child: impl IntoElement, dx: f32, dy: f32) -> Shifted {
    Shifted {
        child: child.into_any_element(),
        offset: (dx, dy),
    }
}

/// 见 [`shifted`]。
pub struct Shifted {
    child: AnyElement,
    offset: (f32, f32),
}

impl Element for Shifted {
    type RequestLayoutState = ();
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
    ) -> (LayoutId, Self::RequestLayoutState) {
        // 排版整个交给子元素:本元素就是它在父级里的那一项
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let (dx, dy) = self.offset;
        window.with_element_offset(point(px(dx), px(dy)), |window| {
            self.child.prepaint(window, cx);
        });
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

/// 半路掉头时新一场的起步进度:旧的一场推到 `p`(已缓动),掉头后新旧互换、方向取反,
/// 从 `1 − p` 接着推 —— 两层此刻都还在原地(推导见 [`KeyedSwap::observe`])。
pub fn reversed_progress(p: f32) -> f32 {
    1.0 - p.clamp(0.0, 1.0)
}

/// 从进度 `from` 推到 1 的补间(`from` 已在 1 上 = 一建出来就是跑完的)。半路掉头的
/// 一场从 [`reversed_progress`] 起步,用它代替只能从头播的 [`mt_ui::motion::Transition`]。
pub fn resume_band(from: f32, spec: TransitionSpec, now: Instant) -> ValueTween {
    let mut band = ValueTween::settled(from.clamp(0.0, 1.0), spec);
    band.retarget_at(1.0, now);
    band
}

/// 「谁在台上」变了就起一场换场的状态记录。挂在画这块内容的视图上。
///
/// 首次 [`observe`](Self::observe) 只记下、不播(视图刚建出来不该从侧面推进来);
/// 动画跑完由 [`frame`](Self::frame) 自己收掉出场记录 —— 没切换时零包装。
#[derive(Clone, Debug)]
pub struct KeyedSwap<K> {
    current: Option<K>,
    previous: Option<K>,
    dir: f32,
    spec: TransitionSpec,
    /// 这一场推到哪了(0 = 旧内容整幅在台上,1 = 新内容到位;已缓动)。用 `ValueTween`
    /// 而不是 `Transition`:半路掉头要从中间接着推,`Transition` 只会从头播。
    band: ValueTween,
}

impl<K: Clone + PartialEq> KeyedSwap<K> {
    pub fn new(spec: TransitionSpec) -> Self {
        Self {
            current: None,
            previous: None,
            dir: 1.0,
            spec,
            band: ValueTween::settled(1.0, spec),
        }
    }

    /// 报这一帧谁在台上。变了就起一场:`order(旧, 新)` 给方向 —— `> 0` 新内容从末端
    /// (右 / 下)进,`< 0` 从起端进,`== 0` 不做动画(瞬切)。总开关关着时一律瞬切。
    ///
    /// **半路掉头**(在飞那一场还没播完,又切回它的旧内容):不从头播 —— 新旧互换、方向
    /// 取反、从 `1 − 进度` 接着推。设掉头前 X 出、Y 进、方向 `d`、进度 `p`:X 在 `−d·p`、
    /// Y 在 `d·(1−p)`;掉头后 X 进、Y 出、方向 `−d`、进度 `1−p`:X 在 `−d·(1−(1−p)) = −d·p`、
    /// Y 在 `d·(1−p)` —— 两层原地不动,接着往回推(与段控件滑块的平滑掉头同步)。
    pub fn observe(&mut self, key: &K, order: impl FnOnce(&K, &K) -> f32) {
        self.observe_at(key, order, Instant::now());
    }

    fn observe_at(&mut self, key: &K, order: impl FnOnce(&K, &K) -> f32, now: Instant) {
        let Some(current) = self.current.as_ref() else {
            self.current = Some(key.clone());
            return;
        };
        if current == key {
            return;
        }
        if self.previous.as_ref() == Some(key)
            && self.band.running_at(now)
            && mt_ui::motion::animations_enabled()
        {
            let from = reversed_progress(self.band.value_at(now));
            self.previous = self.current.replace(key.clone());
            self.dir = -self.dir;
            self.band = resume_band(from, self.spec, now);
            return;
        }
        let dir = order(current, key);
        if dir != 0.0 && mt_ui::motion::animations_enabled() {
            self.previous = Some(current.clone());
            self.dir = if dir < 0.0 { -1.0 } else { 1.0 };
            self.band = resume_band(0.0, self.spec, now);
        } else {
            self.previous = None;
        }
        self.current = Some(key.clone());
    }

    /// 不经动画直接换到 `key`(并收掉在飞的那一场)。切项目这类「换了一整块上下文」
    /// 的变化用它,免得把上一个项目的页面推出来。
    pub fn jump(&mut self, key: &K) {
        self.current = Some(key.clone());
        self.previous = None;
    }

    /// 这一帧的出场信息:`(旧 key, 已缓动进度, 方向)`。动画中自动要下一帧;
    /// 跑完返回 `None` 并收掉记录。**render 调用链里调**(`request_animation_frame`
    /// 只 notify 当前视图)。
    pub fn frame(&mut self, window: &Window) -> Option<(K, f32, f32)> {
        let frame = self.frame_at(Instant::now());
        if frame.is_some() {
            window.request_animation_frame();
        }
        frame
    }

    fn frame_at(&mut self, now: Instant) -> Option<(K, f32, f32)> {
        let previous = self.previous.clone()?;
        if !self.band.running_at(now) {
            self.previous = None;
            return None;
        }
        Some((previous, self.band.value_at(now), self.dir))
    }

    /// 还在换场吗(不要帧,只判)。目前只有单测在问。
    #[cfg(test)]
    pub fn in_flight(&self) -> bool {
        self.previous.is_some() && self.band.running_at(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn push_端点精确且两层首尾相接() {
        for dir in [1.0_f32, -1.0] {
            let (enter0, exit0) = push_offsets(dir, 0.0);
            assert_eq!(enter0, dir, "进场层起点整幅在屏外");
            assert_eq!(exit0, 0.0, "出场层起点就在原位");
            let (enter1, exit1) = push_offsets(dir, 1.0);
            assert_eq!(enter1, 0.0, "终点必须精确归零");
            assert_eq!(exit1, -dir);
            for i in 0..=20 {
                let (e, x) = push_offsets(dir, i as f32 / 20.0);
                assert!((e - x - dir).abs() < 1e-5, "两层间距恒为一整幅:{e} {x}");
            }
        }
        // 越界钳住;方向只认正负
        assert_eq!(push_offsets(1.0, 1.7), (0.0, -1.0));
        assert_eq!(push_offsets(1.0, -0.5), (1.0, 0.0));
        assert_eq!(push_offsets(3.0, 0.0), push_offsets(1.0, 0.0));
        assert_eq!(push_offsets(-0.2, 0.0), push_offsets(-1.0, 0.0));
    }

    #[test]
    fn 换场记录_首帧不播_变了才播_方向零即瞬切() {
        let spec =
            TransitionSpec::exempt(Duration::from_millis(200), mt_ui::motion::Easing::Linear);
        let mut swap = KeyedSwap::new(spec);
        swap.observe(&1, |_, _| 1.0);
        assert!(!swap.in_flight(), "首次只记下");
        swap.observe(&1, |_, _| 1.0);
        assert!(!swap.in_flight(), "没变不播");

        swap.observe(&2, |old, new| if new > old { 1.0 } else { -1.0 });
        // 总开关是进程级的:别的用例不拧它,这里按「开着」断言
        if mt_ui::motion::animations_enabled() {
            assert!(swap.in_flight());
            assert_eq!(swap.previous, Some(1));
            assert_eq!(swap.dir, 1.0);
        }

        swap.observe(&3, |_, _| 0.0);
        assert!(!swap.in_flight(), "方向给 0 = 瞬切,并收掉在飞那一场");
        assert_eq!(swap.current, Some(3));

        swap.observe(&1, |_, _| -5.0);
        if mt_ui::motion::animations_enabled() {
            assert_eq!(swap.dir, -1.0, "方向只取正负");
        }
        swap.jump(&4);
        assert!(!swap.in_flight());
        assert_eq!(swap.current, Some(4));
    }

    /// 半路掉头不跳:「会话 → Git」推到一半又点回「会话」,两层此刻原地不动,接着往回推;
    /// 跑完的那一刻精确落位、收掉记录。
    #[test]
    fn 换场半路掉头_从当前位置接着往回推() {
        if !mt_ui::motion::animations_enabled() {
            return;
        }
        let spec =
            TransitionSpec::exempt(Duration::from_millis(200), mt_ui::motion::Easing::Linear);
        let t0 = Instant::now();
        let mut swap = KeyedSwap::new(spec);
        swap.observe_at(&"sessions", |_, _| 1.0, t0);
        swap.observe_at(&"git", |_, _| 1.0, t0);
        // 推了 30%:Git 从右边进到 0.7,会话向左出到 −0.3
        let t1 = t0 + Duration::from_millis(60);
        let (old, p, dir) = swap.frame_at(t1).expect("在飞");
        assert_eq!((old, dir), ("sessions", 1.0));
        let (git_at, sessions_at) = push_offsets(dir, p);
        // 点回会话(方向回调不该被问到:掉头的方向由在飞那一场决定)
        swap.observe_at(&"sessions", |_, _| panic!("掉头不重新定方向"), t1);
        let (old, p, dir) = swap.frame_at(t1).expect("掉头后接着飞");
        assert_eq!((old, dir), ("git", -1.0), "新旧互换、方向取反");
        let (enter_at, exit_at) = push_offsets(dir, p);
        assert!(
            (enter_at - sessions_at).abs() < 1e-4,
            "会话原地不动:{enter_at} vs {sessions_at}"
        );
        assert!(
            (exit_at - git_at).abs() < 1e-4,
            "Git 原地不动:{exit_at} vs {git_at}"
        );
        // 接着往回推、播完精确落位
        let (_, later, _) = swap.frame_at(t1 + Duration::from_millis(100)).unwrap();
        assert!(later > p, "朝终点走");
        assert!(swap.frame_at(t1 + Duration::from_millis(201)).is_none());
        assert_eq!(swap.current, Some("sessions"));
        // 起步那一刻就掉头(进度 0):直接到位,不播
        let mut swap = KeyedSwap::new(spec);
        swap.observe_at(&1, |_, _| 1.0, t0);
        swap.observe_at(&2, |_, _| 1.0, t0);
        swap.observe_at(&1, |_, _| 1.0, t0);
        assert!(swap.frame_at(Instant::now()).is_none());
        assert_eq!(swap.current, Some(1));
        assert_eq!(reversed_progress(0.3), 0.7);
        assert_eq!(reversed_progress(1.4), 0.0, "越界钳住");
    }

    /// 平移包装只挪绘制位置:子树画在偏移后的位置,兄弟元素与滚动容器的滚动范围都按
    /// 原排版算。对照:`relative().top(..)` 与一块 `h_full` 的绝对定位探针都会撑大滚动范围
    /// (用量统计主体入场 / 量高探针踩过的坑)。
    #[gpui::test]
    fn 平移包装不改排版_滚动范围不变(cx: &mut gpui::TestAppContext) {
        use gpui::{
            AnyWindowHandle, AppContext as _, Context, Render, ScrollHandle,
            StatefulInteractiveElement, canvas,
        };
        use std::cell::Cell;
        use std::rc::Rc;

        // 一个 100 高、上下 padding 14 的滚动容器,里面一条 10 高的副标题 + 一块 20 高的短内容
        // (gpui 按直接子元素包围盒的**跨度**算内容尺寸,所以要有一个不动的兄弟才看得出撑大)。
        // 滚动容器在 prepaint 里认「正在画的视图」,得挂在真视图里画
        struct Host {
            kind: u8,
            handle: ScrollHandle,
            painted: Rc<Cell<f32>>,
        }
        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let painted = self.painted.clone();
                let probe = canvas(
                    move |bounds, _, _| painted.set(f32::from(bounds.origin.y)),
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full();
                let content = div().h(px(20.0)).relative().child(probe);
                let body = div()
                    .id("body")
                    .h(px(100.0))
                    .py(px(14.0))
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .track_scroll(&self.handle)
                    .child(div().h(px(10.0)));
                match self.kind {
                    0 => body.child(shifted(content, 0.0, 60.0)),
                    1 => body.child(content.top(px(60.0))),
                    _ => body
                        .child(content)
                        .child(div().absolute().top_0().left_0().w(px(0.0)).h_full()),
                }
            }
        }

        let mut scroll_max = |kind: u8| -> (f32, f32) {
            let handle = ScrollHandle::new();
            let painted = Rc::new(Cell::new(0.0_f32));
            let host = Host {
                kind,
                handle: handle.clone(),
                painted: painted.clone(),
            };
            let window = AnyWindowHandle::from(cx.add_window(move |_, _| host));
            cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            (f32::from(handle.max_offset().y), painted.get())
        };
        let (max, painted) = scroll_max(0);
        assert_eq!(max, 0.0, "平移包装不撑大滚动范围");
        assert_eq!(painted, 14.0 + 10.0 + 60.0, "子树画在偏移之后的位置");
        let (max, _) = scroll_max(1);
        assert!(max > 0.0, "对照:relative + top 会撑大滚动范围");
        let (max, _) = scroll_max(2);
        assert!(
            max >= 28.0,
            "对照:滚动容器里一块 h_full 的探针让内容恒 ≥ 可视高 + padding"
        );
    }
}
