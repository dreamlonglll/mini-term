//! 高度裁剪展开 / 收起的公共件:内容按自然高度排好,外壳(裁剪窗)的高度从 0 补到
//! 自然高,收起反向、播完再卸。
//!
//! # 口径(用户定稿)
//!
//! - **裁剪窗,不淡入**:内容自始至终按最终尺寸原地排(宽度不变、文字不重排),只是被一个
//!   逐帧变高的 `overflow_hidden` 外壳露出来;外壳本身参与布局,下方的东西跟着平滑让位;
//! - 有进场就有退场:[`HeightReveal::set_open(false)`] 之后内容照旧挂在树上、外壳从当前
//!   高度收回 0,播完 [`HeightReveal::is_mounted`] 才转 `false`;退场期间内容上盖一张
//!   拦鼠标的透明罩,不再接悬停与点击;
//! - 半路反向从当前高度掉头(`ValueTween` 从当前显示值接着补),不跳变;
//! - 时长曲线默认 [`mt_ui::motion::SECTION_TOGGLE`];服从「启用动画」总开关(关掉即直达
//!   终态),豁免系统「减少动画」;
//! - 只限**非终端**内容:外壳高度逐帧变,里面放终端会逐帧 PTY resize。
//!
//! # 自然高度怎么来
//!
//! 内容外面包一层不受外壳约束的内层(`flex_none`,高度 = 内容高),内层里挂一块不画东西的
//! canvas 探针,prepaint 时把内层高度记进 [`HeightReveal`] 共享的格子里,**下一帧**用。
//! 展开的第一帧还没量过也无妨:那一帧进度≈0,外壳高度本来就≈0。展开到位且静止时外壳
//! 不设高度(内容怎么变高都跟着走),探针照量,留给下一次收起当起点。
//!
//! # 怎么用
//!
//! ```ignore
//! // 视图字段(首次出现不播:按初始状态建成静止的)
//! banner: HeightReveal,                    // HeightReveal::new(op.is_some())
//!
//! // 状态变了:播。换了一整块上下文(换项目 / 换仓库)就 `jump`,不播
//! self.banner.set_open(op.is_some());
//!
//! // render:挂着才画(退场期间也挂着)
//! if self.banner.is_mounted() {
//!     root = root.child(self.banner.render(self.render_banner(..), window));
//! }
//! ```
//!
//! 画这块内容的视图要在它的 render 调用链里调 [`HeightReveal::render`]
//! (`request_animation_frame` 通知的是当前视图)。

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    AnyElement, InteractiveElement, IntoElement, ParentElement, Styled, Window, canvas, div,
    prelude::FluentBuilder as _, px,
};
use mt_ui::motion::{SECTION_TOGGLE, TransitionSpec, ValueTween};

/// 这一帧外壳怎么画。纯判定,单测钉它。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RevealMode {
    /// 收起且静止:整块不进树。
    Hidden,
    /// 动画中:外壳定高 `height`(像素)、裁掉多出的部分。`closing` = 正在收(内容不再接鼠标)。
    Clipped { height: f32, closing: bool },
    /// 展开且静止:外壳不设高度,内容按自然高度排。
    Natural,
}

/// 由目标、是否在跑、当前进度、量到的自然高度定这一帧的画法。
pub fn reveal_mode(open: bool, running: bool, progress: f32, natural: f32) -> RevealMode {
    if !running {
        return if open {
            RevealMode::Natural
        } else {
            RevealMode::Hidden
        };
    }
    RevealMode::Clipped {
        height: natural.max(0.0) * progress.clamp(0.0, 1.0),
        closing: !open,
    }
}

/// 一块可高度裁剪展开 / 收起的内容的状态。挂在画它的视图上。见模块注释。
#[derive(Debug)]
pub struct HeightReveal {
    spec: TransitionSpec,
    /// 0 = 收起,1 = 展开。
    tween: ValueTween,
    /// 上一帧量到的内容自然高度(像素;0 = 还没量过)。探针在 prepaint 里写。
    natural: Rc<Cell<f32>>,
}

fn target_of(open: bool) -> f32 {
    if open { 1.0 } else { 0.0 }
}

impl HeightReveal {
    /// 按初始状态建成**静止**的(首次出现不播)。
    pub fn new(open: bool) -> Self {
        Self::with_spec(open, SECTION_TOGGLE)
    }

    pub fn with_spec(open: bool, spec: TransitionSpec) -> Self {
        Self {
            spec,
            tween: ValueTween::settled(target_of(open), spec),
            natural: Rc::new(Cell::new(0.0)),
        }
    }

    /// 目标是展开吗。
    pub fn is_open(&self) -> bool {
        self.tween.target() > 0.5
    }

    /// 朝展开 / 收起播。与当前目标相同则什么都不做;半路反向从当前高度掉头。
    pub fn set_open(&mut self, open: bool) {
        self.tween.retarget_at(target_of(open), Instant::now());
    }

    /// 不播,直接到位(换了一整块上下文:换项目、换仓库、面板重新打开)。
    pub fn jump(&mut self, open: bool) {
        self.tween = ValueTween::settled(target_of(open), self.spec);
    }

    /// 还在动吗。
    pub fn is_running(&self) -> bool {
        self.tween.running_at(Instant::now())
    }

    /// 还在树上吗:展开着,或收起动画还没播完。
    pub fn is_mounted(&self) -> bool {
        self.is_open() || self.is_running()
    }

    /// 上一帧量到的自然高度(还没量过为 `None`)。
    pub fn natural_height(&self) -> Option<f32> {
        let h = self.natural.get();
        (h > 0.0).then_some(h)
    }

    /// 这一帧的画法(见 [`reveal_mode`])。
    pub fn mode(&self) -> RevealMode {
        let now = Instant::now();
        reveal_mode(
            self.is_open(),
            self.tween.running_at(now),
            self.tween.value_at(now),
            self.natural.get(),
        )
    }

    /// 这一帧外壳占多高:静止展开 = 自然高度(还没量过用 `fallback`),动画中 = 插值高度,
    /// 收起 = 0。给「按固定件高度换算剩余空间」的调用方用(与真正画出来的是同一个数)。
    pub fn shown_height(&self, fallback: f32) -> f32 {
        match self.mode() {
            RevealMode::Hidden => 0.0,
            RevealMode::Clipped { height, .. } => height,
            RevealMode::Natural => self.natural_height().unwrap_or(fallback),
        }
    }

    /// 把 `child` 包成这一帧的样子。收起且静止时返回一个空的零高 div(调用方最好先看
    /// [`Self::is_mounted`],连它都不挂)。动画中自动要下一帧。
    pub fn render(&self, child: impl IntoElement, window: &Window) -> AnyElement {
        let mode = self.mode();
        if matches!(mode, RevealMode::Clipped { .. }) {
            window.request_animation_frame();
        }
        if mode == RevealMode::Hidden {
            return div().into_any_element();
        }
        let natural = self.natural.clone();
        let probe = canvas(
            move |bounds, _window, _cx| {
                let h = f32::from(bounds.size.height);
                if (natural.get() - h).abs() > 0.5 {
                    // 动画中本来就每帧要帧,下一帧自然用上新值;静止展开时外壳不设高度,
                    // 这个数只留给下一次收起当起点 —— 都不必额外要帧
                    natural.set(h);
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        // 内层不受外壳约束:高度 = 内容自然高度,宽度随外壳
        let inner = div()
            .relative()
            .w_full()
            .flex_none()
            .child(child)
            .child(probe);
        match mode {
            RevealMode::Clipped { height, closing } => div()
                .w_full()
                .flex_none()
                .flex()
                .flex_col()
                .h(px(height))
                .overflow_hidden()
                .child(inner.when(closing, |el| {
                    // 正在收的内容不再接悬停与点击
                    el.child(div().absolute().inset_0().occlude())
                }))
                .into_any_element(),
            _ => div()
                .w_full()
                .flex_none()
                .flex()
                .flex_col()
                .child(inner)
                .into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 画法_静止只分挂与不挂_动画中按进度裁() {
        assert_eq!(reveal_mode(true, false, 1.0, 120.0), RevealMode::Natural);
        assert_eq!(reveal_mode(false, false, 0.0, 120.0), RevealMode::Hidden);
        // 展开中:高度 = 自然高 × 进度,进度越界钳住
        assert_eq!(
            reveal_mode(true, true, 0.25, 120.0),
            RevealMode::Clipped {
                height: 30.0,
                closing: false
            }
        );
        assert_eq!(
            reveal_mode(true, true, 1.7, 120.0),
            RevealMode::Clipped {
                height: 120.0,
                closing: false
            }
        );
        // 收起中:同一套插值,标记成「在收」(内容不接鼠标)
        assert_eq!(
            reveal_mode(false, true, 0.5, 120.0),
            RevealMode::Clipped {
                height: 60.0,
                closing: true
            }
        );
        // 还没量过自然高度:动画中按 0 画(第一帧进度≈0,本来就该≈0)
        assert_eq!(
            reveal_mode(true, true, 0.1, 0.0),
            RevealMode::Clipped {
                height: 0.0,
                closing: false
            }
        );
    }

    #[test]
    fn 新建即静止_首次出现不播() {
        let shown = HeightReveal::new(true);
        assert!(shown.is_open() && shown.is_mounted() && !shown.is_running());
        assert_eq!(shown.mode(), RevealMode::Natural);
        let hidden = HeightReveal::new(false);
        assert!(!hidden.is_mounted());
        assert_eq!(hidden.mode(), RevealMode::Hidden);
        assert_eq!(hidden.shown_height(36.0), 0.0);
    }

    #[test]
    fn 收起播完才卸_跳转不播() {
        let mut reveal = HeightReveal::new(true);
        reveal.natural.set(80.0);
        reveal.set_open(false);
        assert!(!reveal.is_open());
        if mt_ui::motion::animations_enabled() {
            // 收起刚开始:仍挂着,外壳从自然高度起收
            assert!(reveal.is_mounted() && reveal.is_running());
            assert!(reveal.shown_height(0.0) > 70.0);
            assert!(matches!(
                reveal.mode(),
                RevealMode::Clipped { closing: true, .. }
            ));
        }
        reveal.jump(false);
        assert!(!reveal.is_mounted(), "jump 直接到位");
        reveal.jump(true);
        assert!(reveal.is_open() && !reveal.is_running());
        // 静止展开:外壳高度 = 量到的自然高度;没量过用兜底
        assert_eq!(reveal.shown_height(36.0), 80.0);
        reveal.natural.set(0.0);
        assert_eq!(reveal.shown_height(36.0), 36.0);
    }
}
