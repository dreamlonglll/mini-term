//! 拖选**停留**自动复制(改造清单 #16 的 mt-ui 半边)。
//!
//! # 原版是什么(`src/components/TerminalInstance.tsx:242-318`)
//!
//! 不是「松手就复制」,而是**按住左键期间鼠标静止超过 `selectionAutoCopySecs` 秒**
//! (可配置,默认 1s;设 0 = 整个特性关闭)就复制当前选区,并在鼠标旁弹一个
//! 「已复制」小气泡。四条容易漏掉的细节,这里逐条保住:
//!
//! 1. **4px 抖动阈值**:不加阈值的话定时器永远在被 `mousemove` 重置,永远等不到;
//! 2. **一次按压只复制一次**(`copiedThisPress`):不然停住不动会每秒复制一遍;
//! 3. **松手补一刀**:拖到边缘触发自动滚屏时鼠标可以保持静止,dwell 会在选区还在
//!    增长时提前复制半截;松手时选区若已变化就再复制一次,让剪贴板与用户最终看到的
//!    选区一致(**气泡不重弹** —— 「已复制」对最终内容依然成立);
//! 4. **mouseup 挂在 document 上**:拖选常在终端区域外松手。GPUI 侧等价物是
//!    `window.on_mouse_event` 的全局派发(本来就不判 hover),天然满足。
//!
//! # GPUI 侧现状与兼容
//!
//! `element.rs` 原本是**松手立即复制**(X11 primary selection 的习惯)。
//! 为了「宿主零改动仍编译、且行为不变」,判据放在 [`DwellConfig::dwell`]:
//!
//! | `dwell` | 行为 |
//! |---|---|
//! | `0`(默认) | 松手立即复制 —— 与改造前一字不差 |
//! | `> 0` | 原版语义:按住期间停留 `dwell` 后复制 + 回调宿主弹气泡;松手不再自动复制(只做第 3 条的补刀) |
//!
//! # 为什么状态机要独立出来
//!
//! 定时器要靠 `window.spawn` + `background_executor().timer()` 驱动,那段没法单测。
//! 把「什么时候该起表、什么时候该作废、超时了到底复不复制」抽成一台不碰时钟的
//! 状态机([`DwellTracker`]),超时回调只带一个**代号**回来对账 —— 中途动过鼠标
//! 代号就变了,晚到的定时器自己认赔。上面四条细节全部落在这台状态机的单测里。

use std::rc::Rc;
use std::time::Duration;

use gpui::{
    App, Hsla, IntoElement, ParentElement, Pixels, Point, RenderOnce, Styled, Window, div, px,
};

use crate::terminal::rgb8;

/// 复制发生时回调宿主。参数:复制到剪贴板的文本、气泡建议落点(**元素相对坐标**)。
///
/// 宿主拿它弹 [`CopiedTip`];不接也行,剪贴板照样写。
pub type OnSelectionCopied = Rc<dyn Fn(&str, Point<Pixels>, &mut Window, &mut App)>;

/// 停留复制的参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DwellConfig {
    /// 停留多久触发。`Duration::ZERO` = 关闭停留语义,退回「松手立即复制」。
    /// 对应前端 config 的 `selectionAutoCopySecs`(秒)。
    pub dwell: Duration,
    /// 抖动阈值(px)。超过它才算「鼠标动了」并重新计时。原版是 4px。
    pub move_threshold: f32,
    /// 气泡自动消失的时长(交给宿主用,mt-ui 不管计时)。原版 1s。
    pub tip_duration: Duration,
}

impl Default for DwellConfig {
    /// 默认**关闭**停留语义 —— 宿主不配置就维持 GPUI 侧原有的「松手即复制」。
    fn default() -> Self {
        Self {
            dwell: Duration::ZERO,
            move_threshold: 4.0,
            tip_duration: Duration::from_millis(1000),
        }
    }
}

impl DwellConfig {
    /// 从前端那个「秒」的配置项造一份。`secs <= 0` 就是关闭(与前端同)。
    pub fn from_secs(secs: f32) -> Self {
        Self {
            dwell: if secs > 0.0 {
                Duration::from_secs_f32(secs)
            } else {
                Duration::ZERO
            },
            ..Default::default()
        }
    }

    pub fn enabled(&self) -> bool {
        !self.dwell.is_zero()
    }
}

/// 松开左键时该做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseAction {
    /// 什么也不做。
    Nothing,
    /// 立刻复制(停留语义关闭时的旧行为)。
    CopyNow,
    /// 这一按压里已经复制过:选区**变了**才补一次,且不重弹气泡。
    ReconcileIfChanged,
}

/// 停留复制的状态机。不碰时钟 —— 定时器由宿主起,超时带代号回来对账。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DwellTracker {
    pressed: bool,
    copied: bool,
    last_pos: (f32, f32),
    /// 计时代号。每次(重新)计时 +1;晚到的定时器代号对不上就作废。
    generation: u64,
    /// 上次复制出去的文本(松手对账用)。
    copied_text: Option<String>,
}

impl DwellTracker {
    /// 左键按下。返回需要起表的代号;`None` = 不用起表(停留语义关闭)。
    pub fn on_press(&mut self, cfg: &DwellConfig, pos: (f32, f32)) -> Option<u64> {
        self.pressed = true;
        self.copied = false;
        self.copied_text = None;
        self.last_pos = pos;
        cfg.enabled().then(|| {
            self.generation += 1;
            self.generation
        })
    }

    /// 鼠标移动。移动量越过阈值才重新计时;返回新代号。
    pub fn on_move(&mut self, cfg: &DwellConfig, pos: (f32, f32)) -> Option<u64> {
        if !self.pressed || !cfg.enabled() {
            return None;
        }
        // 4px 阈值过滤手抖 —— 少了它定时器永远在被重置,永远等不到
        if (pos.0 - self.last_pos.0).abs() < cfg.move_threshold
            && (pos.1 - self.last_pos.1).abs() < cfg.move_threshold
        {
            return None;
        }
        self.last_pos = pos;
        self.generation += 1;
        Some(self.generation)
    }

    /// 定时器到点了。`generation` 是起表时拿到的代号,`has_selection` 是当下有没有选区。
    ///
    /// 返回 `true` = 现在就复制。调用方复制成功后要调 [`Self::note_copied`]。
    pub fn on_dwell_elapsed(&self, generation: u64, has_selection: bool) -> bool {
        // 三道闸,任缺一条都会出现「松手后还弹一次」「停住不动每秒复制一遍」这类 bug
        self.pressed && !self.copied && generation == self.generation && has_selection
    }

    /// 复制成功后记账。
    pub fn note_copied(&mut self, text: String) {
        self.copied = true;
        self.copied_text = Some(text);
    }

    /// 左键松开。
    pub fn on_release(&mut self, cfg: &DwellConfig) -> ReleaseAction {
        let was_pressed = self.pressed;
        self.pressed = false;
        // 代号 +1:此刻还在飞的定时器一律作废(松手之后不该再弹气泡)
        self.generation += 1;
        if !cfg.enabled() {
            return if was_pressed {
                ReleaseAction::CopyNow
            } else {
                ReleaseAction::Nothing
            };
        }
        if was_pressed && self.copied {
            ReleaseAction::ReconcileIfChanged
        } else {
            ReleaseAction::Nothing
        }
    }

    /// 松手补刀的判据:选区与上次复制出去的不一样才补。
    pub fn needs_reconcile(&self, current: &str) -> bool {
        match self.copied_text.as_deref() {
            Some(prev) => prev != current,
            None => false,
        }
    }

    /// 气泡落点(元素相对坐标),照抄原版的贴边收拢:
    /// `x = min(鼠标x + 12, 宽 - 70)`,`y = max(鼠标y - 30, 4)`。
    pub fn tip_origin(&self, element_size: (f32, f32)) -> Point<Pixels> {
        let x = (self.last_pos.0 + 12.0).min(element_size.0 - 70.0).max(0.0);
        let y = (self.last_pos.1 - 30.0).max(4.0);
        gpui::point(px(x), px(y))
    }

    pub fn is_pressed(&self) -> bool {
        self.pressed
    }

    pub fn has_copied(&self) -> bool {
        self.copied
    }
}

/// 「已复制」小气泡。
///
/// mt-ui 只出这一个视觉件,**什么时候显示、什么时候消失由宿主管**
/// (原版是 `setCopiedTip(...)` + 1s 后 `setCopiedTip(null)`)。
///
/// ```ignore
/// // 宿主:TerminalPane 的 render 里,叠在终端之上
/// .when_some(self.copied_tip, |this, origin| {
///     this.child(
///         div().absolute().left(origin.x).top(origin.y)
///             .child(CopiedTip::new("已复制")),
///     )
/// })
/// ```
#[derive(IntoElement)]
pub struct CopiedTip {
    label: String,
    background: Hsla,
    foreground: Hsla,
    font_size: Pixels,
}

impl CopiedTip {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            // --bg-overlay / --text-primary(与 mt_app::ui 那张表同源)
            background: rgb8(0x25, 0x23, 0x20),
            foreground: rgb8(0xf0, 0xec, 0xe6),
            font_size: px(11.0),
        }
    }

    pub fn colors(mut self, background: Hsla, foreground: Hsla) -> Self {
        self.background = background;
        self.foreground = foreground;
        self
    }

    pub fn font_size(mut self, size: Pixels) -> Self {
        self.font_size = size;
        self
    }
}

impl RenderOnce for CopiedTip {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        div()
            .px(px(6.0))
            .py(px(2.0))
            .rounded(px(4.0))
            .bg(self.background)
            .text_size(self.font_size)
            .text_color(self.foreground)
            .child(self.label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> DwellConfig {
        DwellConfig::from_secs(1.0)
    }

    #[test]
    fn 关闭时退回松手即复制() {
        let cfg = DwellConfig::default();
        assert!(!cfg.enabled(), "默认必须是关的 —— 宿主零改动要维持旧语义");
        let mut t = DwellTracker::default();
        assert_eq!(t.on_press(&cfg, (0.0, 0.0)), None, "不起表");
        assert_eq!(t.on_move(&cfg, (100.0, 100.0)), None);
        assert_eq!(t.on_release(&cfg), ReleaseAction::CopyNow);
        // 没按过就松手(比如上报模式配对松开)不该复制
        assert_eq!(t.on_release(&cfg), ReleaseAction::Nothing);
    }

    #[test]
    fn 秒数为零或负都当关闭() {
        assert!(!DwellConfig::from_secs(0.0).enabled());
        assert!(!DwellConfig::from_secs(-1.0).enabled());
        assert!(DwellConfig::from_secs(0.5).enabled());
    }

    #[test]
    fn 抖动阈值以内不重新计时() {
        let cfg = on();
        let mut t = DwellTracker::default();
        let g = t.on_press(&cfg, (10.0, 10.0)).unwrap();
        // 3px 抖动:不重置,否则定时器永远等不到
        assert_eq!(t.on_move(&cfg, (13.0, 12.0)), None);
        assert!(t.on_dwell_elapsed(g, true), "代号还有效");
        // 越过阈值:换代
        let g2 = t.on_move(&cfg, (20.0, 10.0)).unwrap();
        assert_eq!(g2, g + 1);
        assert!(!t.on_dwell_elapsed(g, true), "旧代号必须作废");
        assert!(t.on_dwell_elapsed(g2, true));
    }

    #[test]
    fn 一次按压只复制一次() {
        let cfg = on();
        let mut t = DwellTracker::default();
        let g = t.on_press(&cfg, (0.0, 0.0)).unwrap();
        assert!(t.on_dwell_elapsed(g, true));
        t.note_copied("hello".into());
        // 停住不动时定时器可能被反复安排,但只准复制一次
        assert!(!t.on_dwell_elapsed(g, true));
    }

    #[test]
    fn 没有选区时不复制() {
        let cfg = on();
        let mut t = DwellTracker::default();
        let g = t.on_press(&cfg, (0.0, 0.0)).unwrap();
        assert!(!t.on_dwell_elapsed(g, false));
    }

    #[test]
    fn 松手后飞在路上的定时器作废() {
        let cfg = on();
        let mut t = DwellTracker::default();
        let g = t.on_press(&cfg, (0.0, 0.0)).unwrap();
        assert_eq!(t.on_release(&cfg), ReleaseAction::Nothing);
        assert!(!t.on_dwell_elapsed(g, true), "松手之后不该再弹气泡");
    }

    #[test]
    fn 松手补刀只在选区变过时发生() {
        let cfg = on();
        let mut t = DwellTracker::default();
        let g = t.on_press(&cfg, (0.0, 0.0)).unwrap();
        assert!(t.on_dwell_elapsed(g, true));
        t.note_copied("half".into());
        assert_eq!(t.on_release(&cfg), ReleaseAction::ReconcileIfChanged);
        // 边缘自动滚屏把选区撑长了 → 补一次
        assert!(t.needs_reconcile("half and more"));
        // 没变就别白写一次剪贴板
        assert!(!t.needs_reconcile("half"));
    }

    #[test]
    fn 气泡落点贴边收拢() {
        let cfg = on();
        let mut t = DwellTracker::default();
        t.on_press(&cfg, (100.0, 100.0));
        let p = t.tip_origin((800.0, 600.0));
        assert_eq!((f32::from(p.x), f32::from(p.y)), (112.0, 70.0));
        // 贴右边:往容器里收,免得气泡被裁掉
        t.on_press(&cfg, (790.0, 5.0));
        let p = t.tip_origin((800.0, 600.0));
        assert_eq!(f32::from(p.x), 730.0);
        // 贴上边:不许跑到容器外
        assert_eq!(f32::from(p.y), 4.0);
    }
}
