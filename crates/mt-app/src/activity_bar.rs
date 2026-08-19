//! 左侧窄边条(对照 `src/components/ActivityBar.tsx`)。
//!
//! # 为什么图标是自绘而不是 `gpui_component::IconName`
//!
//! `IconName` 只是一张「枚举 → `icons/xxx.svg` 路径」的映射表,**图形本体不在
//! gpui-component 这个 crate 里** —— crates.io 上的 0.5.1 包里既没有 `assets/`
//! 也没有任何 `AssetSource` 实现(上游仓库把 lucide 的 svg 放在示例程序的资产
//! 目录里,由宿主自己注册)。mt-app 现在没有注册 asset source,直接用
//! `IconName::Settings` 的结果是**一片空白**,而且这种失败在编译期看不出来。
//!
//! 于是走 mt-ui 已经在用的那条路:[`mt_ui::icons::VectorIcon`] 的形状 DSL。
//! 好处是几何直接照抄原版 SVG 的 `path`(下面每张表的注释里都留着原文),
//! 与状态灯/品牌图标同一个渲染器,不必再引一套资产管线。
//!
//! # 只画有落点的按钮
//!
//! 原版 8 个按钮里 SSH / 更新提醒两个在 GPUI 侧还没有对应功能,
//! **不放占位**(灰着点不动的按钮比没有更让人困惑)。其余六个:
//! 折叠中间栏 / AI 历史 / Git 变更 / 用量统计 / 移动端 / 设置,外加一个原版没有的
//! 「跳到已完成」。(Git 那颗由 V 批补上,与右抽屉的 sessions⇄git 段控件同一个开关;
//! 移动端那颗由 U 批补上,位置照原版排在「设置」之前。)

use gpui::{
    AnimationExt as _, AnyElement, Animation, Div, ElementId, InteractiveElement, IntoElement,
    ParentElement, SharedString, Stateful, StatefulInteractiveElement, Styled, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::tooltip::Tooltip;
use mt_ui::icons::{Geom, Ink, Shape, VectorIcon};

use crate::tree::PaneStatus;
use crate::ui;

/// 边条宽度。原版 `style={{ width: 44 }}`。
pub const WIDTH: f32 = 44.0;
/// 按钮尺寸。原版 `w-8 h-8`(32px)。
const BUTTON: f32 = 32.0;
/// 图标尺寸。原版每个 svg 都是 `width="18" height="18"`。
const ICON: f32 = 18.0;

/// 单位方框换算:原版 viewBox 是 `0 0 16 16`,除以 16 即可。
const fn u(v: f32) -> f32 {
    v / 16.0
}
/// 原版全部图标统一 `stroke-width="1.2"`。
const STROKE: f32 = 1.2 / 16.0;

/// 折叠 / 展开中间栏。原版 `ICON_PANEL`:
/// `<rect x="2" y="3" width="12" height="10" rx="1.5"/>` + `<path d="M6.5 3v10"/>`。
pub const PANEL: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Rect {
            x: u(2.0),
            y: u(3.0),
            w: u(12.0),
            h: u(10.0),
            round: u(1.5),
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(6.5), u(3.0)), (u(6.5), u(13.0))]),
    ),
];

/// AI 历史抽屉。原版 `ICON_SESSIONS`:`<path d="M2 3h12v8H5l-3 3V3z"/>`
/// —— 一个带小尾巴的对话气泡(闭合路径)。
pub const SESSIONS: &[Shape] = &[Shape::line(
    Ink::Current,
    STROKE,
    Geom::Polygon(&[
        (u(2.0), u(3.0)),
        (u(14.0), u(3.0)),
        (u(14.0), u(11.0)),
        (u(5.0), u(11.0)),
        (u(2.0), u(14.0)),
    ]),
)];

/// Git 变更。原版 `ICON_GIT`(`ActivityBar.tsx:24-31`)—— 三个节点 + 一条主干
/// 加一条并回主干的分支:
///
/// ```text
/// <circle cx="5"  cy="4"  r="1.5"/>
/// <circle cx="11" cy="4"  r="1.5"/>
/// <circle cx="5"  cy="12" r="1.5"/>
/// <path d="M5 5.5v5M11 5.5v1a2 2 0 01-2 2H5"/>
/// ```
///
/// 那条 path 的圆角拐弯(`a2 2 0 01-2 2`)在 18px 下半径只有 2px,
/// 用折线近似(取圆弧的起点 / 45° 中点 / 终点三个顶点)。
pub const GIT: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Circle {
            c: (u(5.0), u(4.0)),
            r: u(1.5),
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Circle {
            c: (u(11.0), u(4.0)),
            r: u(1.5),
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Circle {
            c: (u(5.0), u(12.0)),
            r: u(1.5),
        },
    ),
    // 左侧主干:M5 5.5 v5
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(5.0), u(5.5)), (u(5.0), u(10.5))]),
    ),
    // 右侧分支:M11 5.5 v1,再向左拐回主干
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[
            (u(11.0), u(5.5)),
            (u(11.0), u(6.5)),
            (u(10.41), u(7.91)),
            (u(9.0), u(8.5)),
            (u(5.0), u(8.5)),
        ]),
    ),
];

/// 用量统计。原版 `ICON_STATS`:一条底轴 + 三根高低不同的柱子
/// (`M2.5 13.5h11` / `M4 13.5V9M8 13.5V4.5M12 13.5V7`)。
pub const STATS: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(2.5), u(13.5)), (u(13.5), u(13.5))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(4.0), u(13.5)), (u(4.0), u(9.0))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(8.0), u(13.5)), (u(8.0), u(4.5))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(12.0), u(13.5)), (u(12.0), u(7.0))]),
    ),
];

/// 移动端。原版 `ICON_MOBILE`(`ActivityBar.tsx:48-53`)—— 一部竖着的手机:
/// `<rect x="4.5" y="1.5" width="7" height="13" rx="1.5"/>` + `<path d="M7 12.5h2"/>`
/// (机身圆角矩形 + 底部那道 Home 键短横)。
pub const MOBILE: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Rect {
            x: u(4.5),
            y: u(1.5),
            w: u(7.0),
            h: u(13.0),
            round: u(1.5),
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(7.0), u(12.5)), (u(9.0), u(12.5))]),
    ),
];

/// 设置。原版 `ICON_SETTINGS` 的 6 齿齿轮轮廓 + 中心轴孔 —— 那条 `path` 的
/// 24 个顶点逐个抄下来(原版注释写明:轮缘必须是连续的、齿长在轮廓上,
/// 「中心小圆 + 放射短线」画出来是太阳不是齿轮;18px 下取 6 齿才咬得出形状)。
pub const SETTINGS: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polygon(&[
            (u(6.40), u(1.60)),
            (u(9.60), u(1.60)),
            (u(9.53), u(3.56)),
            (u(11.08), u(4.45)),
            (u(12.75), u(3.42)),
            (u(14.34), u(6.18)),
            (u(12.61), u(7.10)),
            (u(12.61), u(8.90)),
            (u(14.34), u(9.82)),
            (u(12.75), u(12.58)),
            (u(11.08), u(11.55)),
            (u(9.53), u(12.44)),
            (u(9.60), u(14.40)),
            (u(6.40), u(14.40)),
            (u(6.47), u(12.44)),
            (u(4.92), u(11.55)),
            (u(3.25), u(12.58)),
            (u(1.66), u(9.82)),
            (u(3.39), u(8.90)),
            (u(3.39), u(7.10)),
            (u(1.66), u(6.18)),
            (u(3.25), u(3.42)),
            (u(4.92), u(4.45)),
            (u(6.47), u(3.56)),
        ]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Circle {
            c: (0.5, 0.5),
            r: u(2.3),
        },
    ),
];

/// 有新版本时才出现的「更新提醒」。原版 `ICON_UPDATE`(`ActivityBar.tsx:60-65`)——
/// 一根向上的箭头 + 底下一道横线(「上传/升级」的常见字形):
///
/// ```text
/// <path d="M8 10.5V3M5 6l3-3 3 3" />
/// <path d="M3 12.5h10" />
/// ```
///
/// 第一条 path 是两笔:竖干 `M8 10.5 V3`,再抬笔画箭头 `M5 6 l3 -3 l3 3`。
/// 这里拆成两条 `Polyline` —— 形状 DSL 没有「抬笔」语义,一条折线连起来会
/// 多出一道从 (8,3) 斜拉到 (5,6) 的假边。
pub const UPDATE: &[Shape] = &[
    // 竖干
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(8.0), u(10.5)), (u(8.0), u(3.0))]),
    ),
    // 箭头(左肩 → 顶点 → 右肩)
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(5.0), u(6.0)), (u(8.0), u(3.0)), (u(11.0), u(6.0))]),
    ),
    // 底线
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(3.0), u(12.5)), (u(13.0), u(12.5))]),
    ),
];

/// 「有新版本」按钮(不含 `on_click`,由调用方挂 —— 点下去是外链到 release 页)。
///
/// 与 [`strip_button`] **不同形**,所以另起一个构造器:原版这颗是 accent 配色、
/// 没有激活态也没有左侧竖条,而且右上角恒挂一颗 accent 圆点
/// (`ActivityBar.tsx:173-182`)。
///
/// ⚠️ **圆点在原版带 `animate-blink`(0.8s 闪烁),这里是静态的**。两条理由:
/// ① 用户机器的「减少动画」是开着的,原版 `styles.css:391` 的通配 reduce 规则
///    正好把 `.animate-blink` 停掉 —— 装机版在这台机器上本来就不闪;
/// ② GPUI 没有媒体查询等价物,要闪就得先有全局「减少动画」闸(并行批在做)。
///    闸就位后在这里补 `with_animation` 即可,**这里就是唯一挂接点**。
pub fn update_button(id: impl Into<ElementId>, tip: SharedString) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .w(px(BUTTON))
        .h(px(BUTTON))
        .flex_none()
        .rounded(px(4.0))
        .cursor_pointer()
        // 原版 `hover:bg-[var(--accent)]/15`
        .hover(|el| el.bg(ui::with_alpha(ui::accent(), 0.15)))
        .child(VectorIcon::new(UPDATE, px(ICON)).ink(ui::accent()))
        .child(
            // `absolute -top-0.5 -right-0.5 w-2 h-2 rounded-full bg-accent
            //  border border-[var(--bg-surface)]`(位置取值与上方全局 AI 徽标同款)
            div()
                .absolute()
                .top(px(-1.0))
                .right(px(-1.0))
                .w(px(8.0))
                .h(px(8.0))
                .rounded_full()
                .border_1()
                .border_color(ui::bg_surface())
                .bg(ui::accent()),
        )
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
}

/// 一个边条按钮的外壳(不含 `on_click`,由调用方挂)。
///
/// 配色逐条对照原版 `btnClass`:激活 = 主文本色 + `--border-subtle` 底,
/// 未激活 = 淡字、hover 转主文本色;激活时左侧还有一根 accent 竖条
/// (原版 `ACCENT_BAR`,**始终占位**靠透明度切换的写法在 gpui 里没必要,
/// 这里直接按需追加子元素)。
pub fn strip_button(
    id: impl Into<ElementId>,
    shapes: &'static [Shape],
    tip: &'static str,
    active: bool,
) -> Stateful<Div> {
    let color = if active {
        ui::text_primary()
    } else {
        ui::text_muted()
    };
    div()
        .id(id)
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .w(px(BUTTON))
        .h(px(BUTTON))
        .flex_none()
        .rounded(px(4.0))
        .cursor_pointer()
        .when(active, |el| el.bg(ui::border_subtle()))
        .hover(|el| el.bg(ui::border_subtle()))
        .child(VectorIcon::new(shapes, px(ICON)).ink(color))
        .when(active, |el| {
            el.child(
                div()
                    .absolute()
                    .left_0()
                    .top(px((BUTTON - 16.0) / 2.0))
                    .w(px(2.0))
                    .h(px(16.0))
                    .rounded(px(1.0))
                    .bg(ui::accent()),
            )
        })
        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
}

/// 全局 AI 状态徽标(挂在「折叠中间栏」那颗按钮的右上角)。
///
/// 原版 `ActivityBar.tsx:122-129`:`absolute -top-0.5 -right-0.5 w-2 h-2
/// rounded-full border border-[var(--bg-surface)]`,**`ai-working` 档加
/// `animate-blink`**(`alertBlink 0.8s ease-in-out infinite`:
/// `50%` 处 `opacity .2` + `scale(.75)`)。
///
/// gpui 没有 transform,缩放用「改宽高 + 同步挪 top/right 半个差值」等价 ——
/// 差值补偿是为了绕**中心**缩,不补的话会朝右上角缩过去。
///
/// ⚠️ 闪烁过 [`crate::motion`] 的闸:原版 reduce 段的通配规则把 `.animate-blink`
/// 停在第一帧(它**不在**豁免名单里),用户机器上装机版就是不闪的。
pub fn status_badge(id: impl Into<ElementId>, status: PaneStatus) -> AnyElement {
    /// 原版 `w-2 h-2`。
    const SIZE: f32 = 8.0;
    /// 原版 `-top-0.5 -right-0.5`(Tailwind 的 0.5 = 2px;这里的边框占 1px,
    /// 与 M 批落地时的取值保持一致)。
    const INSET: f32 = -1.0;

    let badge = div()
        .absolute()
        .top(px(INSET))
        .right(px(INSET))
        .w(px(SIZE))
        .h(px(SIZE))
        .rounded_full()
        .border_1()
        .border_color(ui::bg_surface())
        .bg(ui::status_color(status));

    if !badge_blinks(status) {
        return badge.into_any_element();
    }
    badge
        .with_animation(
            id.into(),
            Animation::new(BLINK_PERIOD).repeat(),
            |el, delta| {
                let phase = crate::title_bar::blink_phase(delta);
                let side = SIZE - SIZE * 0.25 * phase;
                let inset = INSET + (SIZE - side) / 2.0;
                el.w(px(side))
                    .h(px(side))
                    .top(px(inset))
                    .right(px(inset))
                    .opacity(1.0 - 0.8 * phase)
            },
        )
        .into_any_element()
}

/// `alertBlink` 的周期(原版 `animation: alertBlink 0.8s ease-in-out infinite`)。
const BLINK_PERIOD: std::time::Duration = std::time::Duration::from_millis(800);

/// 这一档该不该闪。**纯判定**,单测钉在这上面。
pub fn badge_blinks(status: PaneStatus) -> bool {
    status == PaneStatus::AiWorking && mt_ui::motion::blinks()
}

/// 按钮分组之间的细分隔线(原版 `w-6 h-px bg-[var(--border-default)] my-1`)。
pub fn divider() -> Div {
    div()
        .w(px(24.0))
        .h(px(1.0))
        .my(px(4.0))
        .bg(ui::border_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 形状表的点必须全落在单位方框内 —— 越界会画到相邻按钮上。
    /// (mt-ui 对自己那批图标有同名约束,这里是本 crate 这四张表的同款体检。)
    #[test]
    fn 边条图标的点全在单位方框内() {
        let mut points = 0usize;
        for shapes in [PANEL, SESSIONS, GIT, STATS, SETTINGS, MOBILE, UPDATE] {
            for shape in shapes {
                let (pts, _) = shape.geom.points();
                for (x, y) in pts {
                    assert!(
                        (-0.001..=1.001).contains(&x) && (-0.001..=1.001).contains(&y),
                        "越界点 ({x}, {y})"
                    );
                    points += 1;
                }
            }
        }
        assert!(points > 60, "形状表看起来没被遍历到(只有 {points} 个点)");
    }

    /// 齿轮是「连续轮缘 + 中心轴孔」两笔,顶点数就是原版 path 的 24 个 ——
    /// 少一个都说明抄漏了(缺口会让轮廓不闭合)。
    #[test]
    fn 齿轮轮廓是二十四个顶点加一个轴孔() {
        assert_eq!(SETTINGS.len(), 2);
        let Geom::Polygon(pts) = SETTINGS[0].geom else {
            panic!("第一笔应该是闭合轮廓");
        };
        assert_eq!(pts.len(), 24);
        assert!(matches!(SETTINGS[1].geom, Geom::Circle { .. }));
    }

    /// 更新图标是**三笔**:竖干 / 箭头 / 底线。
    ///
    /// 合并成两笔(把竖干与箭头连成一条折线)会多出一道 (8,3)→(5,6) 的假边 ——
    /// 原版那条 path 在那里是 `M`(抬笔),形状 DSL 没有抬笔语义,只能拆笔。
    #[test]
    fn 更新图标是三笔且箭头顶点与竖干顶端重合() {
        assert_eq!(UPDATE.len(), 3);
        let Geom::Polyline(stem) = UPDATE[0].geom else {
            panic!("第一笔应该是竖干");
        };
        let Geom::Polyline(head) = UPDATE[1].geom else {
            panic!("第二笔应该是箭头");
        };
        // 竖干顶端 = 箭头顶点,错开一点点在 18px 下就是肉眼可见的缺口
        assert_eq!(stem[1], head[1]);
        // 箭头左右肩对称
        assert_eq!(head[0].1, head[2].1);
        assert!((head[1].0 - head[0].0 - (head[2].0 - head[1].0)).abs() < 1e-6);
    }

    /// 只有 `ai-working` 闪,且减弱动效下一律不闪(原版 `.animate-blink`
    /// 不在 reduce 豁免名单里)。
    #[test]
    fn 徽标只在跑起来时闪且过减弱动效的闸() {
        crate::motion::with_reduce(false, || {
            assert!(badge_blinks(PaneStatus::AiWorking));
            for s in [PaneStatus::Idle, PaneStatus::AiIdle, PaneStatus::Error] {
                assert!(!badge_blinks(s), "{s:?} 不该闪");
            }
        });
        crate::motion::with_reduce(true, || {
            for s in [
                PaneStatus::Idle,
                PaneStatus::AiIdle,
                PaneStatus::AiWorking,
                PaneStatus::Error,
            ] {
                assert!(!badge_blinks(s), "减弱动效下 {s:?} 一律不闪");
            }
        });
    }
}
