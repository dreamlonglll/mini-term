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
//!   ([`mt_ui::motion::PANEL_SWAP`] 等),[`KeyedSwap`] 由 `Transition` 天然过闸。
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

use gpui::{AnyElement, Div, InteractiveElement, IntoElement, ParentElement, Styled, Window, div};
use mt_ui::motion::{Transition, TransitionSpec};

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

/// 「谁在台上」变了就起一场换场的状态记录。挂在画这块内容的视图上。
///
/// 首次 [`observe`](Self::observe) 只记下、不播(视图刚建出来不该从侧面推进来);
/// 动画跑完由 [`frame`](Self::frame) 自己收掉出场记录 —— 没切换时零包装。
#[derive(Clone, Debug)]
pub struct KeyedSwap<K> {
    current: Option<K>,
    previous: Option<K>,
    dir: f32,
    transition: Transition,
}

impl<K: Clone + PartialEq> KeyedSwap<K> {
    pub fn new(spec: TransitionSpec) -> Self {
        Self {
            current: None,
            previous: None,
            dir: 1.0,
            transition: Transition::settled(spec),
        }
    }

    /// 报这一帧谁在台上。变了就起一场:`order(旧, 新)` 给方向 —— `> 0` 新内容从末端
    /// (右 / 下)进,`< 0` 从起端进,`== 0` 不做动画(瞬切)。总开关关着时一律瞬切。
    pub fn observe(&mut self, key: &K, order: impl FnOnce(&K, &K) -> f32) {
        let Some(current) = self.current.as_ref() else {
            self.current = Some(key.clone());
            return;
        };
        if current == key {
            return;
        }
        let dir = order(current, key);
        if dir != 0.0 && mt_ui::motion::animations_enabled() {
            self.previous = Some(current.clone());
            self.dir = if dir < 0.0 { -1.0 } else { 1.0 };
            self.transition.restart();
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
        let previous = self.previous.clone()?;
        if !self.transition.running() {
            self.previous = None;
            return None;
        }
        let progress = self.transition.drive(window);
        Some((previous, progress, self.dir))
    }

    /// 还在换场吗(不要帧,只判)。目前只有单测在问。
    #[cfg(test)]
    pub fn in_flight(&self) -> bool {
        self.previous.is_some() && self.transition.running()
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
}
