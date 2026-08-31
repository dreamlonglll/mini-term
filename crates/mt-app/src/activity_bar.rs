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

use std::time::Duration;

use gpui::{
    AnyElement, App, Div, Hsla, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    SharedString, Stateful, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder as _, px, rems,
};
use gpui_component::ActiveTheme as _;
use mt_ui::icons::{Geom, Ink, Shape, StatusDot, StatusKind, VectorIcon};

use crate::tree::PaneStatus;
use crate::ui;

/// 边条宽度。原版 `style={{ width: 44 }}`。
pub const WIDTH: f32 = 44.0;
/// 按钮尺寸。原版 `w-8 h-8`(32px)。
const BUTTON: f32 = 32.0;
/// 图标尺寸。原版每个 svg 都是 `width="18" height="18"`。
const ICON: f32 = 18.0;
/// 一次新的边条悬停会话里,第一条文字提示要停多久才出现。
///
/// 这是**完整延迟**,不再叠加全局 [`mt_ui::tooltip::Tooltip`] 的额外 700ms。
pub const HOVER_SHOW_DELAY: Duration = Duration::from_millis(500);
/// 按钮在 44px 边条里左右各留 6px;提示从按钮右缘再跨过这 6px,
/// 正好从边条右缘开始画。
const LABEL_GAP: f32 = (WIDTH - BUTTON) / 2.0;
/// 与全局 tooltip 同一档字号(0.75rem),只把定位与计时收归 Activity Bar。
const LABEL_FONT_SIZE: f32 = 0.75;

/// 进入一颗 Activity Bar 按钮之后,宿主该做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoverEnter {
    /// 重复收到同一颗按钮的 enter,状态没有变化。
    Unchanged,
    /// 新会话的第一条提示要起表;值是本次计时的代号。
    Delay(u64),
    /// 本次会话已经热身,提示已在状态机里切到新按钮,直接重画即可。
    ShowNow,
}

/// Activity Bar 整组按钮共用的悬停会话。
///
/// 这台状态机不碰时钟:宿主按 [`HoverEnter::Delay`] 起计时,到点后带
/// `generation` 回来对账。Task drop 是第一道取消,代号是竞态下的第二道闸。
#[derive(Debug, Default)]
pub struct HoverSession {
    hovered: Option<&'static str>,
    visible: Option<&'static str>,
    warmed: bool,
    generation: u64,
}

impl HoverSession {
    /// 进入一颗按钮。热身前返回计时代号,热身后当场切换可见标签。
    pub fn enter(&mut self, key: &'static str) -> HoverEnter {
        if self.hovered == Some(key) {
            return HoverEnter::Unchanged;
        }

        self.generation = self.generation.wrapping_add(1);
        self.hovered = Some(key);
        if self.warmed {
            self.visible = Some(key);
            HoverEnter::ShowNow
        } else {
            self.visible = None;
            HoverEnter::Delay(self.generation)
        }
    }

    /// 离开一颗按钮。只有离开的仍是当前目标才清理 —— 相邻按钮的 enter/leave
    /// 到达顺序不保证,旧按钮的迟到 leave 不能抹掉新按钮。
    pub fn leave(&mut self, key: &'static str) -> bool {
        if self.hovered != Some(key) {
            return false;
        }
        self.generation = self.generation.wrapping_add(1);
        self.hovered = None;
        self.visible = None;
        true
    }

    /// 第一段停留到点。仍悬着同一颗且代号没过期才真正显示并热身。
    pub fn on_delay_elapsed(&mut self, generation: u64, key: &'static str) -> bool {
        if self.warmed || self.generation != generation || self.hovered != Some(key) {
            return false;
        }
        self.warmed = true;
        self.visible = Some(key);
        true
    }

    /// 离开整条边条:隐藏、降温并让所有在飞的旧计时失效。
    pub fn reset(&mut self) -> bool {
        let changed = self.hovered.is_some() || self.visible.is_some() || self.warmed;
        self.generation = self.generation.wrapping_add(1);
        self.hovered = None;
        self.visible = None;
        self.warmed = false;
        changed
    }

    /// 当前是否该在这颗按钮右侧画文字。
    pub fn is_visible(&self, key: &'static str) -> bool {
        self.visible == Some(key)
    }
}

/// Activity Bar 专用的文字表面。定位壳高 32px 并 `items_center`,所以文字高度
/// 无论随主题字号怎么变,都始终相对按钮垂直居中。
#[derive(IntoElement)]
struct HoverLabel {
    text: SharedString,
}

impl RenderOnce for HoverLabel {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .absolute()
            .left(px(BUTTON + LABEL_GAP))
            .top_0()
            .h(px(BUTTON))
            .flex()
            .items_center()
            .child(
                div()
                    .font_family(cx.theme().font_family.clone())
                    .whitespace_nowrap()
                    .bg(cx.theme().popover)
                    .text_color(cx.theme().popover_foreground)
                    .border_1()
                    .border_color(cx.theme().border)
                    .shadow_md()
                    .rounded(px(6.0))
                    .py(px(2.0))
                    .px(px(8.0))
                    .text_size(rems(LABEL_FONT_SIZE))
                    .child(self.text),
            )
    }
}

/// 三种按钮共用的 hover 接线与标签表面,避免 update / unread 走出另一套手感。
fn with_hover_label<F>(
    button: Stateful<Div>,
    tip: SharedString,
    label_visible: bool,
    on_hover: F,
) -> Stateful<Div>
where
    F: Fn(&bool, &mut Window, &mut App) + 'static,
{
    button
        .on_hover(on_hover)
        .when(label_visible, move |el| el.child(HoverLabel { text: tip }))
}

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

/// 终端列表竖条(GPUI 版新增,原版没有对应按钮,无 SVG 可抄)。字形是竖条
/// 自己的缩影 —— 一列带树状连接线的条目:左侧 `⌐`+`├`+`∟` 的连线骨架,
/// 右侧三道短横当条目。骨架拆两笔:上下拐角连成一条折线,中间那根分支单独一笔
/// (连进折线会多出斜边,与 [`UPDATE`] 同款拆笔理由)。
pub const TERMINALS: &[Shape] = &[
    // 连线骨架:⌐ 拐角 → 竖干 → ∟ 拐角
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[
            (u(7.0), u(4.0)),
            (u(4.0), u(4.0)),
            (u(4.0), u(12.0)),
            (u(7.0), u(12.0)),
        ]),
    ),
    // 中间分支
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(4.0), u(8.0)), (u(7.0), u(8.0))]),
    ),
    // 三道条目短横
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(9.0), u(4.0)), (u(13.5), u(4.0))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(9.0), u(8.0)), (u(13.5), u(8.0))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(9.0), u(12.0)), (u(13.5), u(12.0))]),
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

/// SSH 连接。原版 `ICON_SSH`(`ActivityBar.tsx:42-47`)—— 一个终端窗口:
///
/// ```text
/// <rect x="2" y="3" width="12" height="10" rx="1.5" />
/// <path d="M4.8 6.5 6.6 8l-1.8 1.5M8.4 10h2.8" />
/// ```
///
/// 那条 path 是**两笔**:提示符 `>`(折线 4.8,6.5 → 6.6,8 → 4.8,9.5)与光标
/// 下划线(8.4,10 → 11.2,10)。中间的 `M` 是抬笔,形状 DSL 没有抬笔语义,
/// 连成一条会多出一道从 (4.8,9.5) 斜拉到 (8.4,10) 的假边(与 [`UPDATE`] 同款拆笔)。
pub const SSH: &[Shape] = &[
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
    // 提示符 `>`:`M4.8 6.5 L6.6 8 l-1.8 1.5`
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(4.8), u(6.5)), (u(6.6), u(8.0)), (u(4.8), u(9.5))]),
    ),
    // 光标下划线:`M8.4 10 h2.8`
    Shape::line(
        Ink::Current,
        STROKE,
        Geom::Polyline(&[(u(8.4), u(10.0)), (u(11.2), u(10.0))]),
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
/// 右上角那颗 accent 圆点带 `animate-blink`(0.8s),与全局 AI 徽标共用
/// [`corner_dot`]。⚠️ 闪烁过 [`mt_ui::motion`] 的闸:`.animate-blink` **不在**
/// 原版 reduce 段的豁免名单里,系统开着「减少动画」时装机版本来就不闪 ——
/// 这里同样静止,见 [`update_dot_blinks`]。
pub fn update_button<F>(
    key: &'static str,
    tip: impl Into<SharedString>,
    label_visible: bool,
    on_hover: F,
) -> Stateful<Div>
where
    F: Fn(&bool, &mut Window, &mut App) + 'static,
{
    let button = div()
        .id(key)
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
        // `absolute -top-0.5 -right-0.5 w-2 h-2 rounded-full bg-accent
        //  border border-[var(--bg-surface)] animate-blink`
        .child(CornerDot {
            color: ui::accent(),
            blinking: update_dot_blinks(),
        });

    with_hover_label(button, tip.into(), label_visible, on_hover)
}

/// 一个边条按钮的外壳(不含 `on_click`,由调用方挂)。
///
/// 配色逐条对照原版 `btnClass`:激活 = 主文本色 + `--border-subtle` 底,
/// 未激活 = 淡字、hover 转主文本色;激活时左侧还有一根 accent 竖条
/// (原版 `ACCENT_BAR`,**始终占位**靠透明度切换的写法在 gpui 里没必要,
/// 这里直接按需追加子元素)。
pub fn strip_button<F>(
    key: &'static str,
    shapes: &'static [Shape],
    tip: impl Into<SharedString>,
    active: bool,
    label_visible: bool,
    on_hover: F,
) -> Stateful<Div>
where
    F: Fn(&bool, &mut Window, &mut App) + 'static,
{
    let color = if active {
        ui::text_primary()
    } else {
        ui::text_muted()
    };
    let button = div()
        .id(key)
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
        });

    with_hover_label(button, tip.into(), label_visible, on_hover)
}

/// 未读完成入口。图形与行为仍是原来的成功状态点,只把 Activity Bar 三类按钮的
/// 尺寸、hover 接线和右侧标签统一到同一个构造路径。
pub fn done_button<F>(
    key: &'static str,
    tip: impl Into<SharedString>,
    label_visible: bool,
    on_hover: F,
) -> Stateful<Div>
where
    F: Fn(&bool, &mut Window, &mut App) + 'static,
{
    let button = div()
        .id(key)
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .w(px(BUTTON))
        .h(px(BUTTON))
        .flex_none()
        .rounded(px(4.0))
        .cursor_pointer()
        .hover(|el| el.bg(ui::border_subtle()))
        .child(
            StatusDot::new(StatusKind::AiIdle)
                .size(px(14.0))
                .color(ui::color_success())
                .contrast(ui::bg_surface()),
        );

    with_hover_label(button, tip.into(), label_visible, on_hover)
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
pub fn status_badge(status: PaneStatus) -> AnyElement {
    CornerDot {
        color: ui::status_color(status),
        blinking: badge_blinks(status),
    }
    .into_any_element()
}

/// 按钮右上角那颗 8px 圆点。全局 AI 徽标([`status_badge`])与「有新版本」按钮
/// ([`update_button`])共用同一颗,差别只有配色与闪不闪。
///
/// 闪烁相位来自 `mt_ui::motion::pulse_phase` 的低频泵 —— **不用**
/// `with_animation(..repeat())`,那条路每帧请求重绘,一颗 8px 的点就能把
/// 整窗钉在满帧率上。静态档连泵都不挂。
#[derive(IntoElement)]
struct CornerDot {
    color: Hsla,
    blinking: bool,
}

impl RenderOnce for CornerDot {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let dot = div()
            .absolute()
            .top(px(DOT_INSET))
            .right(px(DOT_INSET))
            .w(px(DOT_SIZE))
            .h(px(DOT_SIZE))
            .rounded_full()
            .border_1()
            .border_color(ui::bg_surface())
            .bg(self.color);

        if !self.blinking {
            return dot;
        }
        let delta = mt_ui::motion::pulse_phase(BLINK_PERIOD, window, cx);
        let (side, inset, opacity) = blink_dot_frame(crate::title_bar::blink_phase(delta));
        dot.w(px(side))
            .h(px(side))
            .top(px(inset))
            .right(px(inset))
            .opacity(opacity)
    }
}

/// 原版 `w-2 h-2`。
const DOT_SIZE: f32 = 8.0;
/// 原版 `-top-0.5 -right-0.5`(Tailwind 的 0.5 = 2px;这里的边框占 1px,
/// 与 M 批落地时的取值保持一致)。
const DOT_INSET: f32 = -1.0;

/// `alertBlink` 这一帧的 **(边长, inset, 不透明度)**。
///
/// 原版 `50% { opacity: .2; transform: scale(.75) }`。gpui 没有 transform,
/// 缩放用「改宽高 + 同步挪 top/right 半个差值」等价 —— 差值补偿是为了绕**中心**
/// 缩,不补的话圆点会朝右上角缩过去(单测钉的就是这条中心不动)。
fn blink_dot_frame(phase: f32) -> (f32, f32, f32) {
    let side = DOT_SIZE - DOT_SIZE * 0.25 * phase;
    (
        side,
        DOT_INSET + (DOT_SIZE - side) / 2.0,
        1.0 - 0.8 * phase,
    )
}

/// `alertBlink` 的周期(原版 `animation: alertBlink 0.8s ease-in-out infinite`)。
const BLINK_PERIOD: std::time::Duration = std::time::Duration::from_millis(800);

/// 这一档该不该闪。**纯判定**,单测钉在这上面。
pub fn badge_blinks(status: PaneStatus) -> bool {
    status == PaneStatus::AiWorking && mt_ui::motion::blinks()
}

/// 「有新版本」那颗圆点该不该闪。与 AI 徽标**不同档**:原版这颗恒带
/// `animate-blink`(`ActivityBar.tsx:180`),没有状态之分,只过减弱动效那道闸。
pub fn update_dot_blinks() -> bool {
    mt_ui::motion::blinks()
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

    #[test]
    fn 首条提示延迟显示并把会话热身() {
        let mut session = HoverSession::default();
        let HoverEnter::Delay(generation) = session.enter("one") else {
            panic!("新会话第一颗按钮必须起表");
        };
        assert!(!session.is_visible("one"), "停够 500ms 前不能先画出来");
        assert!(session.on_delay_elapsed(generation, "one"));
        assert!(session.is_visible("one"));
        assert!(session.warmed);
    }

    #[test]
    fn 热身后跨空隙切按钮立即显示() {
        let mut session = HoverSession::default();
        let HoverEnter::Delay(generation) = session.enter("one") else {
            panic!("第一颗应起表");
        };
        assert!(session.on_delay_elapsed(generation, "one"));

        assert!(session.leave("one"), "进空隙时隐藏当前标签");
        assert!(!session.is_visible("one"));
        assert!(session.warmed, "空隙不等于离开整条边条");

        assert_eq!(session.enter("two"), HoverEnter::ShowNow);
        assert!(session.is_visible("two"));
    }

    #[test]
    fn 离开整条边条后下次重新等待() {
        let mut session = HoverSession::default();
        let HoverEnter::Delay(generation) = session.enter("one") else {
            panic!("第一颗应起表");
        };
        assert!(session.on_delay_elapsed(generation, "one"));
        assert!(session.reset());
        assert!(!session.warmed);
        assert!(!session.is_visible("one"));
        assert!(matches!(session.enter("two"), HoverEnter::Delay(_)));
    }

    #[test]
    fn 旧计时与旧按钮的迟到离开都不能覆盖新目标() {
        let mut session = HoverSession::default();
        let HoverEnter::Delay(old_generation) = session.enter("one") else {
            panic!("第一颗应起表");
        };
        let HoverEnter::Delay(new_generation) = session.enter("two") else {
            panic!("热身前切换目标应为新目标重新起表");
        };

        assert!(!session.leave("one"), "旧按钮的 leave 不能清掉新按钮");
        assert!(!session.on_delay_elapsed(old_generation, "one"));
        assert!(!session.is_visible("one"));
        assert!(session.on_delay_elapsed(new_generation, "two"));
        assert!(session.is_visible("two"));

        assert_eq!(session.enter("three"), HoverEnter::ShowNow);
        assert!(!session.leave("two"), "热身后的旧 leave 同样不能清新标签");
        assert!(session.is_visible("three"));
    }

    #[test]
    fn 延迟与标签起点钉在边条几何上() {
        assert_eq!(HOVER_SHOW_DELAY, Duration::from_millis(500));
        // 按钮左侧留白 + 标签相对按钮的 left = 44px 边条右缘。
        assert_eq!(LABEL_GAP + BUTTON + LABEL_GAP, WIDTH);
        assert_eq!(BUTTON, 32.0, "定位壳高度跟按钮高度同源");
    }

    /// 形状表的点必须全落在单位方框内 —— 越界会画到相邻按钮上。
    /// (mt-ui 对自己那批图标有同名约束,这里是本 crate 这四张表的同款体检。)
    #[test]
    fn 边条图标的点全在单位方框内() {
        let mut points = 0usize;
        for shapes in [
            PANEL, SESSIONS, GIT, TERMINALS, STATS, SETTINGS, MOBILE, SSH, UPDATE,
        ] {
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

    /// SSH 图标是**三笔**:窗口框 / 提示符 `>` / 光标下划线。
    ///
    /// 合并成两笔(把提示符与下划线连成一条折线)会多出一道
    /// (4.8,9.5)→(8.4,10) 的假边 —— 原版那条 path 在那里是 `M`(抬笔)。
    #[test]
    fn ssh图标是三笔且提示符对称() {
        assert_eq!(SSH.len(), 3);
        assert!(matches!(SSH[0].geom, Geom::Rect { .. }), "第一笔是窗口框");
        let Geom::Polyline(caret) = SSH[1].geom else {
            panic!("第二笔应该是提示符折线");
        };
        // `>` 的上下两臂等长、尖端在中间高度
        assert_eq!(caret.len(), 3);
        assert_eq!(caret[0].0, caret[2].0, "两臂起点横坐标相同");
        assert!(
            ((caret[1].1 - caret[0].1) - (caret[2].1 - caret[1].1)).abs() < 1e-6,
            "尖端上下对称"
        );
        let Geom::Polyline(underline) = SSH[2].geom else {
            panic!("第三笔应该是光标下划线");
        };
        assert_eq!(underline.len(), 2);
        assert_eq!(underline[0].1, underline[1].1, "下划线是水平的");
    }

    /// 更新圆点**恒闪**(原版没有状态之分),只过减弱动效那道闸。
    #[test]
    fn 更新圆点恒闪但过减弱动效的闸() {
        crate::motion::with_reduce(false, || assert!(update_dot_blinks()));
        crate::motion::with_reduce(true, || {
            assert!(!update_dot_blinks(), "减弱动效下装机版本来就不闪")
        });
    }

    /// `alertBlink` 的缩放绕**中心**:边长变小的同时 inset 补一半,
    /// 圆点中心在整轮里一动不动(不补的话会朝右上角缩过去)。
    #[test]
    fn 圆点闪烁绕中心缩放() {
        let center = |(side, inset, _): (f32, f32, f32)| inset + side / 2.0;
        let at0 = blink_dot_frame(0.0);
        let at1 = blink_dot_frame(1.0);
        assert_eq!(at0, (DOT_SIZE, DOT_INSET, 1.0), "相位 0 = 原样");
        assert!((at1.0 - DOT_SIZE * 0.75).abs() < 1e-6, "相位 1 缩到 75%");
        assert!((at1.2 - 0.2).abs() < 1e-6, "相位 1 的不透明度 = .2");
        for i in 0..=20 {
            let f = blink_dot_frame(i as f32 / 20.0);
            assert!((center(f) - center(at0)).abs() < 1e-6, "中心挪了:{f:?}");
            assert!(f.0 > 0.0 && f.2 > 0.0);
        }
    }
}
