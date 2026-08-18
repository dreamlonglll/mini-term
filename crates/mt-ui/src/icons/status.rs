//! 四态状态灯(对照 `src/components/StatusDot.tsx`)。
//!
//! # 为什么是形状 + 颜色双编码
//!
//! 原版注释里写死了这条:四态原先只用颜色区分(6px 纯色圆点),红绿黄对色觉障碍
//! 用户几乎不可分辨。现在是:
//!
//! | 状态 | 形状 | 颜色变量 |
//! |---|---|---|
//! | `idle` | 空心细圈 | `--text-muted` |
//! | `ai-idle` | 实心圆 + **对勾** | `--color-success` |
//! | `ai-working` | 底环 + 一段亮弧(**真的在转**) | `--color-ai-working` |
//! | `error` | 实心圆 + **叉** | `--color-error` |
//!
//! 几何逐条照抄原版 SVG(viewBox 16 → 这里除以 16 落进单位方框):
//! `r=4.5 stroke=1.8` / `r=6.5 fill` + `M5 8.2l2 2 4-4.2` / `r=6 stroke=1.6 opacity=.3`
//! + `M8 2a6 6 0 0 1 6 6` / `M5.6 5.6l4.8 4.8 M10.4 5.6l-4.8 4.8`。
//!
//! **ai-working 那段弧必须真的转**:画着一段弧却纹丝不动,看上去就是个卡死的
//! 加载指示器 —— 原版为此专门加了 `animate-status-spin`,这里用
//! [`gpui::Animation`] 的 `repeat()` 驱动 [`VectorIcon::rotation`] 做到同一件事。
//!
//! # 与原版的已知偏差
//!
//! - 原版的呼吸动画由 `prefers-reduced-motion` 兜底(用户机器上就是 `reduce`)。
//!   GPUI 侧读不到这个系统偏好,旋转**恒定开启**;需要时用
//!   [`StatusDot::animated`] 关掉(传 `false` 就是静态的弧);
//! - tooltip 文案(`panels.statusDot.*`)走 i18n,归宿主 —— 本组件只画图形。
//!
//! # 宿主接线(mt-app)
//!
//! `crates/mt-app/src/ui.rs` 的 `status_dot()` 现在是三形圆点(div 拼的),整个换掉:
//!
//! ```ignore
//! use mt_ui::icons::{StatusDot, StatusKind};
//!
//! pub fn status_dot(status: PaneStatus) -> impl IntoElement {
//!     // PaneStatus 住在 mt-app(tree.rs),mt-ui 不能反向依赖,所以在这里转一次
//!     let kind = match status {
//!         PaneStatus::Idle => StatusKind::Idle,
//!         PaneStatus::AiIdle => StatusKind::AiIdle,
//!         PaneStatus::AiWorking => StatusKind::AiWorking,
//!         PaneStatus::Error => StatusKind::Error,
//!     };
//!     StatusDot::new(("status", status.priority() as usize), kind)
//!         .size(px(11.0))
//!         .color(status_color(status))   // 保留 ui.rs 自己那张色表
//!         .contrast(bg_elevated())       // 勾/叉画在实心圆上,用面板底色
//! }
//! ```
//!
//! ⚠️ **`id` 必须逐处唯一且稳定**:`with_animation` 拿它当元素状态的 key,
//! 同一帧里两个状态灯用同一个 id 会共享动画进度(看着像同步闪),而 id 随帧变化
//! 则每帧从头开始转(看着像卡住)。项目列表用 `("status", project_id)`,
//! pane tab 用 `("status", pty_id)` 这类稳定标识。

use gpui::{
    Animation, AnimationExt as _, App, ElementId, Hsla, IntoElement, Pixels, RenderOnce, Window,
    px,
};
use std::time::Duration;

use super::vector::{Geom, Ink, Shape, VectorIcon};
use crate::terminal::rgb8;

/// pane / 项目四态。与 `mt_app::tree::PaneStatus`、后端 `mt_ai::StatusChange::status`
/// 的字符串口径一致(`idle` / `ai-idle` / `ai-working` / `error`)。
///
/// mt-ui 不依赖 mt-app,所以这里独立定义一份;宿主在接线处转换即可。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum StatusKind {
    #[default]
    Idle,
    AiIdle,
    AiWorking,
    Error,
}

impl StatusKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::AiIdle => "ai-idle",
            Self::AiWorking => "ai-working",
            Self::Error => "error",
        }
    }

    // 与 `mt_app::tree::PaneStatus::from_str` 取同一个命名,不实现 `FromStr` trait
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "idle" => Self::Idle,
            "ai-idle" => Self::AiIdle,
            "ai-working" => Self::AiWorking,
            "error" => Self::Error,
            _ => return None,
        })
    }

    /// 默认色,逐值取自 `src/styles.css` 的暗色变量(与 `mt_app::ui` 那张表同源)。
    pub fn default_color(self) -> Hsla {
        match self {
            Self::Idle => rgb8(0x6a, 0x62, 0x58),      // --text-muted
            Self::AiIdle => rgb8(0x6b, 0xb8, 0x7a),    // --color-success
            Self::AiWorking => rgb8(0xf5, 0xc5, 0x18), // --color-ai-working
            Self::Error => rgb8(0xd4, 0x60, 0x5a),     // --color-error
        }
    }

    fn shapes(self) -> &'static [Shape] {
        match self {
            Self::Idle => IDLE,
            Self::AiIdle => AI_IDLE,
            Self::AiWorking => AI_WORKING,
            Self::Error => ERROR,
        }
    }

    /// 这一态需要旋转吗。
    pub fn spins(self) -> bool {
        self == Self::AiWorking
    }
}

/// `idle`:空心细圈(原版 `r=4.5 stroke-width=1.8`)。
const IDLE: &[Shape] = &[Shape::line(
    Ink::Current,
    1.8 / 16.0,
    Geom::Circle {
        c: (0.5, 0.5),
        r: 4.5 / 16.0,
    },
)];

/// `ai-idle`:实心圆 + 对勾(原版 `r=6.5` + `M5 8.2l2 2 4-4.2`)。
const AI_IDLE: &[Shape] = &[
    Shape::fill(
        Ink::Current,
        Geom::Circle {
            c: (0.5, 0.5),
            r: 6.5 / 16.0,
        },
    ),
    Shape::line(
        Ink::Contrast,
        2.0 / 16.0,
        Geom::Polyline(&[
            (5.0 / 16.0, 8.2 / 16.0),
            (7.0 / 16.0, 10.2 / 16.0),
            (11.0 / 16.0, 6.0 / 16.0),
        ]),
    ),
];

/// `ai-working`:底环(30% 透明)+ 一段 90° 亮弧。整体旋转 = spinner。
///
/// 原版 `M8 2a6 6 0 0 1 6 6`:从 12 点顺时针到 3 点,正好 90°。
const AI_WORKING: &[Shape] = &[
    Shape::line(
        Ink::CurrentAlpha(0.3),
        1.6 / 16.0,
        Geom::Circle {
            c: (0.5, 0.5),
            r: 6.0 / 16.0,
        },
    ),
    Shape::line(
        Ink::Current,
        2.4 / 16.0,
        Geom::Arc {
            c: (0.5, 0.5),
            r: 6.0 / 16.0,
            from: -90.0,
            sweep: 90.0,
        },
    ),
];

/// `error`:实心圆 + 叉(原版 `r=6.5` + `M5.6 5.6l4.8 4.8 M10.4 5.6l-4.8 4.8`)。
const ERROR: &[Shape] = &[
    Shape::fill(
        Ink::Current,
        Geom::Circle {
            c: (0.5, 0.5),
            r: 6.5 / 16.0,
        },
    ),
    Shape::line(
        Ink::Contrast,
        2.0 / 16.0,
        Geom::Polyline(&[(5.6 / 16.0, 5.6 / 16.0), (10.4 / 16.0, 10.4 / 16.0)]),
    ),
    Shape::line(
        Ink::Contrast,
        2.0 / 16.0,
        Geom::Polyline(&[(10.4 / 16.0, 5.6 / 16.0), (5.6 / 16.0, 10.4 / 16.0)]),
    ),
];

/// 全部四态(遍历/演示用)。
pub const ALL_STATUS_KINDS: &[StatusKind] = &[
    StatusKind::Idle,
    StatusKind::AiIdle,
    StatusKind::AiWorking,
    StatusKind::Error,
];

/// 所有形状表(单测遍历用)。
#[cfg(test)]
pub(super) fn shape_tables() -> Vec<&'static [Shape]> {
    ALL_STATUS_KINDS.iter().map(|k| k.shapes()).collect()
}

/// spinner 转一圈的时长。原版 `animate-status-spin` 是 0.9s 匀速。
pub const SPIN_PERIOD: Duration = Duration::from_millis(900);

/// 状态灯。
#[derive(IntoElement)]
pub struct StatusDot {
    id: ElementId,
    status: StatusKind,
    size: Pixels,
    color: Option<Hsla>,
    contrast: Option<Hsla>,
    animated: bool,
}

impl StatusDot {
    /// `id` 见模块注释的告警:必须逐处唯一且跨帧稳定。
    ///
    /// 默认 10px —— 与原版 `size='sm'` 的 10px 一致(`'md'` 是 13px)。
    pub fn new(id: impl Into<ElementId>, status: StatusKind) -> Self {
        Self {
            id: id.into(),
            status,
            size: px(10.0),
            color: None,
            contrast: None,
            animated: true,
        }
    }

    pub fn size(mut self, size: Pixels) -> Self {
        self.size = size;
        self
    }

    /// 覆盖状态色。不给就用 [`StatusKind::default_color`]。
    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }

    /// 实心圆上的勾/叉用什么色。不给就用 `--bg-elevated`。
    ///
    /// 主题包换了底色一定要跟着换:勾是**挖空**语义,颜色对不上就糊成一团。
    pub fn contrast(mut self, color: Hsla) -> Self {
        self.contrast = Some(color);
        self
    }

    /// 关掉旋转(`prefers-reduced-motion` 的等价开关;宿主自己决定何时关)。
    pub fn animated(mut self, animated: bool) -> Self {
        self.animated = animated;
        self
    }
}

impl RenderOnce for StatusDot {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let mut icon = VectorIcon::new(self.status.shapes(), self.size)
            .ink(self.color.unwrap_or_else(|| self.status.default_color()));
        if let Some(c) = self.contrast {
            icon = icon.contrast(c);
        }
        if self.status.spins() && self.animated {
            icon.with_animation(self.id, Animation::new(SPIN_PERIOD).repeat(), |icon, delta| {
                // delta 就是 0..1 的一圈,VectorIcon::rotation 的单位也是「圈」
                icon.rotation(delta)
            })
            .into_any_element()
        } else {
            icon.into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 状态字符串与后端口径一致() {
        for k in ALL_STATUS_KINDS {
            assert_eq!(StatusKind::from_str(k.as_str()), Some(*k));
        }
        assert_eq!(StatusKind::as_str(StatusKind::AiWorking), "ai-working");
        assert_eq!(StatusKind::from_str("running"), None);
    }

    #[test]
    fn 四态形状互不相同_不是只换颜色() {
        // 色觉障碍下的可分辨性全靠这条:形状必须真的不一样
        let counts: Vec<usize> = ALL_STATUS_KINDS.iter().map(|k| k.shapes().len()).collect();
        assert_eq!(counts, vec![1, 2, 2, 3], "笔画数:圈 / 圆+勾 / 环+弧 / 圆+叉");
        // idle 是描边(空心),ai-idle / error 是填充(实心)—— 填充与否是第一层区分
        assert!(matches!(IDLE[0].pen, super::super::vector::Pen::Line(_)));
        assert!(matches!(AI_IDLE[0].pen, super::super::vector::Pen::Fill));
        assert!(matches!(ERROR[0].pen, super::super::vector::Pen::Fill));
    }

    #[test]
    fn 只有_ai_working_会转() {
        for k in ALL_STATUS_KINDS {
            assert_eq!(k.spins(), *k == StatusKind::AiWorking);
        }
    }

    #[test]
    fn 亮弧是从十二点顺时针九十度() {
        // 原版 `M8 2a6 6 0 0 1 6 6`。方向反了 spinner 会看着「倒着转」
        let Geom::Arc { from, sweep, r, c } = AI_WORKING[1].geom else {
            panic!("第二笔应该是弧");
        };
        assert_eq!((from, sweep), (-90.0, 90.0));
        assert_eq!(c, (0.5, 0.5));
        assert!((r - 6.0 / 16.0).abs() < f32::EPSILON);
    }

    #[test]
    fn 默认色对齐样式表变量() {
        assert_eq!(StatusKind::AiIdle.default_color(), rgb8(0x6b, 0xb8, 0x7a));
        assert_eq!(StatusKind::AiWorking.default_color(), rgb8(0xf5, 0xc5, 0x18));
        assert_eq!(StatusKind::Error.default_color(), rgb8(0xd4, 0x60, 0x5a));
        assert_eq!(StatusKind::Idle.default_color(), rgb8(0x6a, 0x62, 0x58));
    }
}
