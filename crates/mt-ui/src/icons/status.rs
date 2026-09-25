//! 五态状态灯(前四态对照 `src/components/StatusDot.tsx`)。
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
//! | `attention` | 实心圆 + **感叹号** | 宿主的 attention 色(内置橙) |
//! | `error` | 实心圆 + **叉** | `--color-error` |
//!
//! 几何逐条照抄原版 SVG(viewBox 16 → 这里除以 16 落进单位方框):
//! `r=4.5 stroke=1.8` / `r=6.5 fill` + `M5 8.2l2 2 4-4.2` / `r=6 stroke=1.6 opacity=.3`
//! + `M8 2a6 6 0 0 1 6 6` / `M5.6 5.6l4.8 4.8 M10.4 5.6l-4.8 4.8`。
//!
//! `attention`(「AI 在等你处理」:授权 / 表单 / 回合因 API 错误结束)是原版没有的
//! 第五态 —— 原版只在托盘和标题栏状态灯上亮黄灯,页签上看不出来:Claude 等授权是
//! 绿勾(与「做完了」一模一样),Codex 等授权是转圈(像在干活)。它**静止不动**:
//! 等人的状态靠形状与颜色说话,不跟 spinner 抢眼,也不挂任何动画泵。
//!
//! **ai-working 那段弧必须真的转**:画着一段弧却纹丝不动,看上去就是个卡死的
//! 加载指示器 —— 原版为此专门加了 `animate-status-spin`,这里由
//! [`crate::motion::pulse_phase`] 的墙钟相位驱动 [`VectorIcon::rotation`]。
//! **刻意不用** `with_animation(..repeat())`:那条路每帧请求重绘,一颗灯就能把
//! 整窗钉在满帧率上、前后台通吃(见 `motion` 模块「永续动画低频泵」一节)。
//!
//! # 减弱动效
//!
//! 原版在 `prefers-reduced-motion: reduce` 下**不停这条旋转**,只把周期从 0.9s
//! 放慢到 2.4s(`styles.css:404-413` 的豁免段,理由写在那儿:一个停住的 spinner
//! 不是「安静」,是在说谎)。这里照抄:周期过一道 [`crate::motion::spin_period`],
//! 停不停由它说了算,组件自己不做判断。想彻底静止仍可用 [`StatusDot::animated`]。
//!
//! # 与原版的已知偏差
//!
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
//!     StatusDot::new(kind)
//!         .size(px(11.0))
//!         .color(status_color(status))   // 保留 ui.rs 自己那张色表
//!         .contrast(bg_elevated())       // 勾/叉画在实心圆上,用面板底色
//! }
//! ```
//!
//! 旋转不带逐元素状态(相位来自进程级墙钟),所以**不需要 id**;同状态的
//! 多颗灯天然同相 —— 原版 CSS animation 各自挂载反而会错相,这里顺手更整齐。

use gpui::{App, Hsla, IntoElement, Pixels, RenderOnce, Window, px};
use std::time::Duration;

use super::vector::{Geom, Ink, Shape, VectorIcon};
use crate::terminal::rgb8;

/// 状态灯的五档。前四档与 `mt_app::tree::PaneStatus`、后端 `mt_ai::StatusChange::status`
/// 的字符串口径一致(`idle` / `ai-idle` / `ai-working` / `error`);`attention` 是
/// **显示层**的第五档(宿主拿 pane 的状态叠上 attention 位得出),后端与移动端协议里
/// 没有这个值。
///
/// mt-ui 不依赖 mt-app,所以这里独立定义一份;宿主在接线处转换即可。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum StatusKind {
    #[default]
    Idle,
    AiIdle,
    AiWorking,
    /// AI 停下来等你处理:授权 / 表单 / 回合因 API 错误结束。
    Attention,
    Error,
}

impl StatusKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::AiIdle => "ai-idle",
            Self::AiWorking => "ai-working",
            Self::Attention => "attention",
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
            "attention" => Self::Attention,
            "error" => Self::Error,
            _ => return None,
        })
    }

    /// 默认色,前四档逐值取自 `src/styles.css` 的暗色变量(与 `mt_app::ui` 那张表同源);
    /// `attention` 取 `mt_app::ui` 暗色表里的 `color_attention`。
    pub fn default_color(self) -> Hsla {
        match self {
            Self::Idle => rgb8(0x6a, 0x62, 0x58),      // --text-muted
            Self::AiIdle => rgb8(0x6b, 0xb8, 0x7a),    // --color-success
            Self::AiWorking => rgb8(0xf5, 0xc5, 0x18), // --color-ai-working
            Self::Attention => rgb8(0xf0, 0x88, 0x3e), // color_attention(暗色)
            Self::Error => rgb8(0xd4, 0x60, 0x5a),     // --color-error
        }
    }

    fn shapes(self) -> &'static [Shape] {
        match self {
            Self::Idle => IDLE,
            Self::AiIdle => AI_IDLE,
            Self::AiWorking => AI_WORKING,
            Self::Attention => ATTENTION,
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

/// `attention`:实心圆 + 感叹号(竖笔 + 圆点,都是挖空语义)。
///
/// 与 `ai-idle` / `error` 同一只 `r=6.5` 实心圆 —— 三者靠圆里的字形区分(勾 / 叉 / 叹号),
/// 不靠颜色。竖笔与圆点之间留出 1.3 的缝,11px 的灯上也读得出是两笔。
const ATTENTION: &[Shape] = &[
    Shape::fill(
        Ink::Current,
        Geom::Circle {
            c: (0.5, 0.5),
            r: 6.5 / 16.0,
        },
    ),
    Shape::line(
        Ink::Contrast,
        2.2 / 16.0,
        Geom::Polyline(&[(8.0 / 16.0, 4.3 / 16.0), (8.0 / 16.0, 8.8 / 16.0)]),
    ),
    Shape::fill(
        Ink::Contrast,
        Geom::Circle {
            c: (8.0 / 16.0, 11.3 / 16.0),
            r: 1.25 / 16.0,
        },
    ),
];

/// 全部五态(遍历/演示用)。
pub const ALL_STATUS_KINDS: &[StatusKind] = &[
    StatusKind::Idle,
    StatusKind::AiIdle,
    StatusKind::AiWorking,
    StatusKind::Attention,
    StatusKind::Error,
];

/// 所有形状表(单测遍历用)。
#[cfg(test)]
pub(super) fn shape_tables() -> Vec<&'static [Shape]> {
    ALL_STATUS_KINDS.iter().map(|k| k.shapes()).collect()
}

/// spinner 转一圈的时长。原版 `animate-status-spin` 是 0.9s 匀速。
///
/// ⚠️ 实际用的是 [`crate::motion::spin_period`] 过闸之后的值(减弱动效下 2.4s)。
pub const SPIN_PERIOD: Duration = Duration::from_millis(900);

/// 状态灯。
#[derive(IntoElement)]
pub struct StatusDot {
    status: StatusKind,
    size: Pixels,
    color: Option<Hsla>,
    contrast: Option<Hsla>,
    animated: bool,
}

impl StatusDot {
    /// 默认 10px —— 与原版 `size='sm'` 的 10px 一致(`'md'` 是 13px)。
    pub fn new(status: StatusKind) -> Self {
        Self {
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
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let mut icon = VectorIcon::new(self.status.shapes(), self.size)
            .ink(self.color.unwrap_or_else(|| self.status.default_color()));
        if let Some(c) = self.contrast {
            icon = icon.contrast(c);
        }
        if self.status.spins() && self.animated {
            // 相位来自低频泵的墙钟([`crate::motion::pulse_phase`],0..1 一圈,
            // `VectorIcon::rotation` 的单位也是「圈」)—— 不用 `with_animation(
            // ..repeat())`:那条路每帧请求重绘,一颗灯就能把整窗钉在满帧率上
            let period = crate::motion::spin_period(SPIN_PERIOD);
            icon = icon.rotation(crate::motion::pulse_phase(period, window, cx));
        }
        icon
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
    fn 五态形状互不相同_不是只换颜色() {
        use super::super::vector::Pen;
        // 色觉障碍下的可分辨性全靠这条:形状必须真的不一样
        let counts: Vec<usize> = ALL_STATUS_KINDS.iter().map(|k| k.shapes().len()).collect();
        assert_eq!(
            counts,
            vec![1, 2, 2, 3, 3],
            "笔画数:圈 / 圆+勾 / 环+弧 / 圆+叹号(竖笔+点) / 圆+叉"
        );
        // idle 是描边(空心),ai-idle / attention / error 是填充(实心)—— 填充与否是第一层区分
        assert!(matches!(IDLE[0].pen, Pen::Line(_)));
        assert!(matches!(AI_IDLE[0].pen, Pen::Fill));
        assert!(matches!(ATTENTION[0].pen, Pen::Fill));
        assert!(matches!(ERROR[0].pen, Pen::Fill));
        // 同为「实心圆 + 三笔」的叹号与叉靠第三笔区分:叹号的第三笔是填充的圆点,
        // 叉的第三笔是描边 —— 两枚灯的字形不会只差颜色
        assert!(matches!(ATTENTION[2].pen, Pen::Fill));
        assert!(matches!(ERROR[2].pen, Pen::Line(_)));
    }

    /// 叹号的两笔都在实心圆里,且竖笔与圆点之间真的留了缝(小尺寸下不糊成一条)。
    #[test]
    fn 叹号在圆内且竖笔与圆点分开() {
        use super::super::vector::Pen;
        let Geom::Polyline(stem) = ATTENTION[1].geom else {
            panic!("第二笔应该是竖笔");
        };
        let Geom::Circle { c: dot_c, r: dot_r } = ATTENTION[2].geom else {
            panic!("第三笔应该是圆点");
        };
        let stem_bottom = stem.iter().map(|p| p.1).fold(f32::MIN, f32::max);
        let stem_top = stem.iter().map(|p| p.1).fold(f32::MAX, f32::min);
        // 缝 = 圆点上沿 - 竖笔下端(再减半个笔宽的端帽余量也要大于 0)
        let Pen::Line(width) = ATTENTION[1].pen else {
            panic!("竖笔应该是描边");
        };
        assert!(dot_c.1 - dot_r - stem_bottom - width / 2.0 > 0.0);
        // 都落在 r=6.5 的实心圆内
        let inner = 6.5 / 16.0;
        assert!(0.5 - stem_top < inner && dot_c.1 + dot_r - 0.5 < inner);
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
