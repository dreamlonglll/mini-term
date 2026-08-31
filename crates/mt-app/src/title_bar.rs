//! 自绘标题栏(对应 `src/components/TitleBar.tsx`,审计缺口 #20)。
//!
//! ```text
//! ┌─ 32px ─────────────────────────────────────────────────────────────────┐
//! │ [mac 交通灯占位] logo Mini-Term v0.13.1 │ ●项目名 ▾  ● │  拖拽  │ ─ □ ✕ │
//! │ └──────── Drag ────────┘                 胶囊   灯   └Drag┘  └Min/Max/Close┘
//! └────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # 拖拽与三键:全部走 gpui 原生的 [`WindowControlArea`]
//!
//! 装机版为此写了 283 行 `src-tauri/src/window_snap.rs`(子类化窗口过程、上报最大化
//! 按钮矩形、DPI 换算、`TrackMouseEvent` 回传悬停态),**整套不搬**:
//! `.window_control_area(..)` 在 paint 阶段登记一个 hitbox,gpui 的 Windows 平台层
//! 在 `WM_NCHITTEST` 里把命中结果翻成 `HTCAPTION` / `HTMINBUTTON` / `HTMAXBUTTON` /
//! `HTCLOSE`(`platform/windows/events.rs:855-880`)。于是:
//!
//! | 能力 | 谁给的 |
//! |---|---|
//! | 拖拽窗口 | `HTCAPTION` + 系统默认处理 |
//! | 双击标题栏最大化/还原 | `WM_NCLBUTTONDBLCLK` 落到 `HTCAPTION`,gpui 不吞就交给 `DefWindowProc`(`events.rs:62-65` 的原注释写明了这条) |
//! | **Win11 贴靠布局菜单** | `HTMAXBUTTON` —— 系统看到这个应答就自己弹,不需要上报矩形 |
//! | 三键的按下/抬起动作 | `handle_nc_mouse_up_msg`(`events.rs:1032-1058`):按下与抬起在同一区域才动作 |
//! | 三键的 hover 态 | `WM_NCMOUSEMOVE` 照常翻译成 `MouseMove` 喂进 gpui(`events.rs:921-946`),所以本模块的 `on_hover` 在非客户区照常收得到 |
//!
//! ## ⚠️ Drag 区必须「正列」,不能像原版那样「挖洞」
//!
//! 原版把整条 bar 设成拖拽区、给胶囊/状态灯/三键挂 `data-no-drag` 挖洞。这里不行:
//! gpui 的命中回调是**按 paint 顺序**遍历 `window_control_hitboxes`、返回第一个命中的
//! (`window.rs:1133-1147`),父元素永远排在子元素前面 —— 给根容器挂 `Drag` 的话
//! 三颗按钮就永远被父级的 `HTCAPTION` 挡住了。所以只给「品牌段」和「中段空白」
//! 两块**显式**挂 `Drag`,其余区域自然是 `HTCLIENT`,等价于原版的 no-drag 洞。
//!
//! 副作用:品牌段内不能再放可点元素(原版品牌段本来就是纯展示)。
//!
//! ## ⚠️ Windows 上三键不许挂 `on_click`
//!
//! `HTMINBUTTON` 系区域的 `WM_NCLBUTTONDOWN/UP` 会**先**被翻成 gpui 的
//! MouseDown/MouseUp 派发一遍,没人吞才轮到平台层动作。也就是说 `on_click` 与
//! 系统动作会**双双触发** —— 关闭键上尤其致命:`remove_window()` 绕过
//! `on_window_should_close`,会把 Z 批的关窗确认框整个跳过。
//! 因此 `on_click` 只在 Linux 挂(那边 `on_hit_test_window_control` 是空实现,
//! 见 `platform/linux/x11/window.rs:1466`),与 gpui-component 的 `TitleBar` 同一取舍。
//!
//! # 为什么不用 `gpui_component::TitleBar`
//!
//! 与 M 批边条、P 批菜单同源的四条硬伤:图标走 `IconName` → SVG 资产(本仓没注册
//! `AssetSource`,渲染出来是空白且编译期无感);高度写死 34px / 按钮 34px 宽(原版
//! 32 / 46);配色取 `cx.theme()` 而不是壳的 [`crate::ui`];布局是两段式,塞不下
//! 「品牌 / 版本 / 胶囊 / 状态灯 / 中段空白」这套五段结构。
//! 可抄的两点(`.window_control_area` 只在 Windows 挂、Drag 区用一个元素声明)已照抄。

use gpui::{
    AnyElement, App, Context, Div, ElementId, Entity, FocusHandle, Hsla, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Window, WindowControlArea, anchored, deferred, div, point,
    prelude::FluentBuilder, px, relative,
};
use mt_ui::tooltip::Tooltip;
use mt_ui::icons::{Geom, Ink, Shape, VectorIcon};
use mt_ui::rgb8;

use crate::i18n::t;
use crate::prompt::Confirm;
use crate::store::{AiProjectEntry, AiProjectKind, AppStore, TitleBarLight};
use crate::ui;

/// 标题栏高度。原版 `TITLE_BAR_HEIGHT = 32`(注释:对齐 Windows 原生 32px,
/// 「窗口按钮的手感才对得上」)。
pub const HEIGHT: f32 = 32.0;

/// macOS 交通灯占位:三颗灯 + 左右留白,内容从这条线之后开始。
const MAC_TRAFFIC_LIGHT_WIDTH: f32 = 78.0;

/// 单颗窗口控制键的宽度(原版 `w-[46px]`)。
const BUTTON_WIDTH: f32 = 46.0;

/// 关闭键 hover 的**Windows 系统红**。原版就是写死的 `#c42b1c` 字面量而不是
/// CSS 变量 —— 它不属于配色主题,换主题包也不该变。
const CLOSE_HOVER_BG: (u8, u8, u8) = (0xc4, 0x2b, 0x1c);

// ─── 图形 ────────────────────────────────────────────────────
//
// 窗口控制三键全部是 **10×10 viewBox / stroke=currentColor / strokeWidth=1** 的细线
// (原版注释:「Windows 的画法是 10×10 内的细线,不是 Material 那种粗描边」)。
// 单位方框换算 = 除以 10。

const fn u10(v: f32) -> f32 {
    v / 10.0
}
/// 三个窗口控制图标统一 `stroke-width="1"`。
const STROKE10: f32 = 1.0 / 10.0;

/// 最小化。原版 `<path d="M0 5.5h10"/>` —— 一条横线。
pub const ICON_MINIMIZE: &[Shape] = &[Shape::line(
    Ink::Current,
    STROKE10,
    Geom::Polyline(&[(u10(0.0), u10(5.5)), (u10(10.0), u10(5.5))]),
)];

/// 最大化。原版 `<rect x="0.5" y="0.5" width="9" height="9"/>`。
pub const ICON_MAXIMIZE: &[Shape] = &[Shape::line(
    Ink::Current,
    STROKE10,
    Geom::Rect {
        x: u10(0.5),
        y: u10(0.5),
        w: u10(9.0),
        h: u10(9.0),
        round: 0.0,
    },
)];

/// 还原(已最大化态)。原版 `<rect x="0.5" y="2.5" width="7" height="7"/>` +
/// `<path d="M2.5 2.5V0.5h7v7h-2"/>` —— 后面那层窗口**只画露出来的两条边**
/// (原注释:画成完整方框会糊成一团)。
pub const ICON_RESTORE: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE10,
        Geom::Rect {
            x: u10(0.5),
            y: u10(2.5),
            w: u10(7.0),
            h: u10(7.0),
            round: 0.0,
        },
    ),
    Shape::line(
        Ink::Current,
        STROKE10,
        Geom::Polyline(&[
            (u10(2.5), u10(2.5)),
            (u10(2.5), u10(0.5)),
            (u10(9.5), u10(0.5)),
            (u10(9.5), u10(7.5)),
            (u10(7.5), u10(7.5)),
        ]),
    ),
];

/// 关闭。原版 `<path d="M0.5 0.5l9 9M9.5 0.5l-9 9"/>` —— 两条对角线。
pub const ICON_CLOSE: &[Shape] = &[
    Shape::line(
        Ink::Current,
        STROKE10,
        Geom::Polyline(&[(u10(0.5), u10(0.5)), (u10(9.5), u10(9.5))]),
    ),
    Shape::line(
        Ink::Current,
        STROKE10,
        Geom::Polyline(&[(u10(9.5), u10(0.5)), (u10(0.5), u10(9.5))]),
    ),
];

/// 品牌 logo。原版 viewBox 16 / strokeWidth 1.3:
/// `<rect x="1.5" y="2.5" width="13" height="11" rx="1.5"/>` +
/// `<path d="M4.5 6.5L6.5 8l-2 1.5M8.5 10h3"/>`(终端窗口 + 提示符 + 光标横线)。
pub const ICON_LOGO: &[Shape] = &[
    Shape::line(
        Ink::Current,
        1.3 / 16.0,
        Geom::Rect {
            x: 1.5 / 16.0,
            y: 2.5 / 16.0,
            w: 13.0 / 16.0,
            h: 11.0 / 16.0,
            round: 1.5 / 16.0,
        },
    ),
    Shape::line(
        Ink::Current,
        1.3 / 16.0,
        Geom::Polyline(&[(4.5 / 16.0, 6.5 / 16.0), (6.5 / 16.0, 8.0 / 16.0), (4.5 / 16.0, 9.5 / 16.0)]),
    ),
    Shape::line(
        Ink::Current,
        1.3 / 16.0,
        Geom::Polyline(&[(8.5 / 16.0, 10.0 / 16.0), (11.5 / 16.0, 10.0 / 16.0)]),
    ),
];

/// 项目切换胶囊的下拉箭头。原版 viewBox 10 / strokeWidth 1.2 /
/// `<path d="M1.5 3.25L5 6.75l3.5-3.5"/>`,比窗口控制图标小一号、同样的细线风格。
pub const ICON_CHEVRON_DOWN: &[Shape] = &[Shape::line(
    Ink::Current,
    1.2 / 10.0,
    Geom::Polyline(&[
        (u10(1.5), u10(3.25)),
        (u10(5.0), u10(6.75)),
        (u10(8.5), u10(3.25)),
    ]),
)];

// ─── 纯函数(可测) ──────────────────────────────────────────

/// 最大化键的两态:已最大化画「还原」,否则画「最大化」。tooltip 跟着换。
///
/// 原版靠 `appWindow.isMaximized()` + `onResized` 订阅维护一个 state;GPUI 每帧
/// 重绘,`window.is_maximized()` 在 `render` 里直接读即可 —— 那套订阅整个删掉。
pub fn max_button_face(maximized: bool) -> (&'static [Shape], &'static str) {
    if maximized {
        (ICON_RESTORE, "titleBar.restore")
    } else {
        (ICON_MAXIMIZE, "titleBar.maximize")
    }
}

/// 状态灯 / 胶囊状态点的取色。**与边条那颗徽标不是一套**:
/// 这里 `error` 是最高档而不是被压成 idle,另外多一个 `done` 档。
pub fn light_color(light: TitleBarLight) -> Hsla {
    match light {
        TitleBarLight::Error => ui::color_error(),
        TitleBarLight::Attention => ui::color_warning(),
        TitleBarLight::Working => ui::color_ai_working(),
        TitleBarLight::Done => ui::color_success(),
        TitleBarLight::Idle => ui::text_muted(),
    }
}

/// 下拉行左侧那颗 6px 状态点的取色。档位与状态灯共用一张色表。
pub fn kind_color(kind: AiProjectKind) -> Hsla {
    light_color(match kind {
        AiProjectKind::Attention => TitleBarLight::Attention,
        AiProjectKind::Working => TitleBarLight::Working,
        AiProjectKind::Done => TitleBarLight::Done,
        AiProjectKind::Idle => TitleBarLight::Idle,
    })
}

/// 0.8s 一轮的「呼吸」进度:`0 → 1 → 0` 的三角波再抹平成 ease-in-out。
///
/// 对应 `styles.css` 的 `@keyframes alertBlink`
/// (`0%,100% {opacity:1; scale(1)}` / `50% {opacity:0.2; scale(0.75)}`)——
/// 输入是 0..1 的线性进度(来自 `mt_ui::motion::pulse_phase`),中点折返得自己算。
pub fn blink_phase(delta: f32) -> f32 {
    let triangle = 1.0 - (delta * 2.0 - 1.0).abs();
    // smoothstep,等价于 CSS 的 ease-in-out
    triangle * triangle * (3.0 - 2.0 * triangle)
}

// ─── 窗口动作 ────────────────────────────────────────────────

// ─── 关窗确认(audit #30 / `App.tsx:389-422`)─────────────────
//
// # 一条闸,四个入口
//
// ```text
// 系统 WM_CLOSE(标题栏 ✕ / Alt+F4 / 任务栏右键关闭)
//     └→ gpui handle_close_msg → on_window_should_close(main.rs 注册)
//                                        ↓
// Linux 降级路径的 ✕ on_click → request_close_window ─→ allow_close()
//                                        ↓
//                 有活着的 AI 或未保存文件? ── 否 ─→ 落盘 → true(放行)
//                              │
//                              是 ─→ 弹 Confirm → false(这次不关)
//                                        └─ 点「确定」→ 置 FORCE_CLOSE
//                                                     → 落盘 → remove_window()
// ```
//
// 托盘没有「退出」项(菜单里只有项目行,见 `tray.rs`),所以关窗路径就这两条口。
//
// # 设计意图(`App.tsx:389-391` 原注释,务必保留)
//
// > 只在真的会毁掉什么时才拦一下。之前无条件弹确认,日常开关十几次全是噪音,
// > 用户学会的是「闭眼点确定」——那正好让确认框在唯一该起作用的时候
// > (AI 正在跑或文件尚未保存)也失效。
//
// # 防重入
//
// 两道,各管一头:
//
// 1. **确认框开着时再点 ✕**:`Confirm::open` 内部走 `open_guarded`,同种类第二次
//    直接忽略 —— 于是摞不出第二个框,`allow_close` 照常返回 false,窗口留着。
//    不需要额外的标志位。
// 2. **确认之后**:`FORCE_CLOSE` 置位,`allow_close` 从此立刻放行。
//    `remove_window()` 只是把 `Window::removed` 置 true、由 `App` 下一轮把窗口丢掉
//    (`gpui-0.2.2/src/window.rs:1375` + `app.rs:1374`),**不会**再投一次
//    `WM_CLOSE`,所以这一条严格说是冗余的 —— 留着是防线:万一哪个平台的
//    `remove_window` 走回 should_close,没有它就是「确认框弹到天荒地老」。

thread_local! {
    /// 已经确认过要关了 —— 之后任何一次询问都直接放行。见上面的防重入说明。
    static FORCE_CLOSE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const CLOSE_RISK_PREVIEW_LIMIT: usize = 5;

fn close_risk_preview(items: &[String]) -> String {
    let mut lines = items
        .iter()
        .take(CLOSE_RISK_PREVIEW_LIMIT)
        .cloned()
        .collect::<Vec<_>>();
    let remaining = items.len().saturating_sub(lines.len());
    if remaining > 0 {
        lines.push(crate::i18n::tr!(
            "app",
            "closeConfirm.remaining",
            count = remaining
        ));
    }
    lines.join("\n")
}

/// 现在可以关窗吗。`false` = 这次别关(确认框已经弹出来了 / 或正开着)。
///
/// 这是 `on_window_should_close` 的**同步**回调体:gpui 要求当场返回 bool,
/// 而确认框是异步的(用户点了才有结果)。套路即上图 —— 先返回 false 把这次关闭
/// 吞掉,确认回调里再走 `remove_window()`(它绕过本函数)。
pub fn allow_close(window: &mut Window, cx: &mut App) -> bool {
    if FORCE_CLOSE.with(|f| f.get()) {
        return true;
    }

    let live = crate::pane_actions::collect_live_ai_panes(AppStore::global(cx).read(cx));
    let dirty_documents = crate::workbench_area::dirty_document_names(cx);
    if live.is_empty() && dirty_documents.is_empty() {
        finish_close(cx);
        return true;
    }

    // 正文里那两个换行符由 `prompt::body` 拆成多个 child —— gpui 的文本不认转义符
    let (title, message) = match (dirty_documents.is_empty(), live.is_empty()) {
        (true, false) => (
            t("app", "closeConfirm.titleAi"),
            crate::i18n::tr!(
                "app",
                "closeConfirm.messageWithSessions",
                count = live.len(),
                names = close_risk_preview(&live)
            ),
        ),
        (false, true) => (
            t("app", "closeConfirm.titleUnsaved"),
            crate::i18n::tr!(
                "app",
                "closeConfirm.messageWithDocuments",
                count = dirty_documents.len(),
                names = close_risk_preview(&dirty_documents)
            ),
        ),
        (false, false) => (
            t("app", "closeConfirm.titleUnsaved"),
            crate::i18n::tr!(
                "app",
                "closeConfirm.messageWithDocumentsAndSessions",
                document_count = dirty_documents.len(),
                document_names = close_risk_preview(&dirty_documents),
                session_count = live.len(),
                session_names = close_risk_preview(&live)
            ),
        ),
        (true, true) => unreachable!("close risks were checked above"),
    };
    Confirm::new(title, message).open(
        move |window, cx| {
            FORCE_CLOSE.with(|f| f.set(true));
            finish_close(cx);
            window.remove_window();
        },
        window,
        cx,
    );
    false
}

/// 真关之前把配置刷下去。
///
/// 原版在这里逐项目 `flushLayoutToConfig` / `flushExpandedDirsToConfig` 再
/// `persistConfig()`;GPUI 侧那两步是**即时**写进 `self.config` 的(布局与展开目录
/// 一变就落进配置结构,只有写盘走 500ms 去抖),所以只差最后这一次强刷。
///
/// `cx.on_app_quit` 里也有一次 `save_config_now`(S 批记档)—— 那条管「进程退出」,
/// 这条管「窗口关闭」,两条都要有:写盘本身是幂等的。
fn finish_close(cx: &mut App) {
    AppStore::global(cx).update(cx, |store, _| store.save_config_now());
}

/// 关闭窗口(Linux 降级路径的 ✕;Windows 上关闭键走系统 `WM_CLOSE`,
/// 进的是 `main.rs` 注册的 `on_window_should_close`)。
///
/// 两条路都过 [`allow_close`],确认口径于是只有一份。
pub fn request_close_window(window: &mut Window, cx: &mut App) {
    if allow_close(window, cx) {
        window.remove_window();
    }
}

/// 最小化窗口(Linux 降级路径;Windows 由 `WindowControlArea::Min` 接管)。
pub fn minimize(window: &mut Window, _cx: &mut App) {
    window.minimize_window();
}

/// 最大化 / 还原(Linux 降级路径)。
///
/// ⚠️ gpui 的 `zoom_window()` 在 **Windows 上只会最大化**
/// (`platform/windows/window.rs:782` 直接 `ShowWindowAsync(SW_MAXIMIZE)`,没有还原
/// 分支)。这条路径在 Windows 上本来就到不了(`WindowControlArea::Max` 会把
/// `WM_NCLBUTTONUP` 变成系统的 `is_maximized() ? SW_NORMAL : SW_MAXIMIZE`),
/// 所以照抄 gpui-component 的 Linux 分支即可;真在 Windows 上退化到这里
/// (窗口不可移动 / 全屏时命中测试早退)只会少一个「还原」方向。
pub fn toggle_maximize(window: &mut Window, _cx: &mut App) {
    window.zoom_window();
}

/// `focusAttentionTarget()` 等价物(`src/utils/attentionJump.ts`):
/// 跳到「下一件该我做的事」。挑目标的优先级 待确认/异常 > 最先完成 > 处理中
/// 由 [`AppStore::next_attention_target`] 给,与托盘菜单同一套落点。
///
/// 返回 `false` = 全都闲着 —— 调用方据此决定要不要退而求其次(下拉行会改为
/// 只把项目切过去,「定位不到目标也不能没反应」)。
fn focus_attention_target(
    store: &Entity<AppStore>,
    only_project: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let Some((project_id, pane_id)) = store.read(cx).next_attention_target(only_project) else {
        return false;
    };
    store.update(cx, |store, cx| {
        store.set_active_project(&project_id, cx);
        store.activate_pane(&project_id, &pane_id, window, cx);
    });
    crate::workbench_area::activate_terminal_page(window, cx);
    true
}

// ─── 视图 ────────────────────────────────────────────────────

/// 哪一颗窗口控制键正被悬停。
///
/// hover 态不走 `.hover(..)` 样式而走视图状态,是因为图标是 [`VectorIcon`] 自绘 ——
/// 它的颜色是 paint 期的常量,`text_color` 影响不到它。底色与图标色必须同帧翻转
/// (关闭键 hover 是**红底白叉**,只翻底色会变成红底深灰叉),所以两者共用这一份。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Control {
    Min,
    Max,
    Close,
}

pub struct TitleBar {
    store: Entity<AppStore>,
    /// 项目切换胶囊的下拉是否展开。
    switcher_open: bool,
    /// 下拉开着时焦点收在这儿(否则打字会落进终端)。
    focus: FocusHandle,
    /// 打开下拉前的焦点,关闭时**先还回去再跑动作**。
    ///
    /// 顺序是 P 批菜单基建的教训:切项目会重建终端视图并聚焦它,反过来的话
    /// 还原焦点会把光标从新终端上抢走。
    prev_focus: Option<FocusHandle>,
    hovered_control: Option<Control>,
    /// 全局状态灯被悬停(原版 `group-hover:scale-125`)。
    light_hovered: bool,
}

impl TitleBar {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            switcher_open: false,
            focus: cx.focus_handle(),
            prev_focus: None,
            hovered_control: None,
            light_hovered: false,
        }
    }

    fn open_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher_open {
            return;
        }
        self.prev_focus = window.focused(cx);
        window.focus(&self.focus);
        self.switcher_open = true;
        cx.notify();
    }

    /// 收起下拉并把焦点还给打开前那个元素(幂等)。
    fn dismiss_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.switcher_open {
            return;
        }
        self.switcher_open = false;
        if let Some(prev) = self.prev_focus.take() {
            window.focus(&prev);
        }
        cx.notify();
    }

    /// 一颗窗口控制键的外壳。`on_click` 由调用方按平台决定挂不挂。
    fn control_button(
        &self,
        which: Control,
        shapes: &'static [Shape],
        tip: &'static str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let hovered = self.hovered_control == Some(which);
        let danger = which == Control::Close;
        let ink = match (hovered, danger) {
            // 关闭键 hover:红底白叉(白色是字面量 —— 底色是系统红,不跟主题走)
            (true, true) => gpui::white(),
            (true, false) => ui::text_primary(),
            (false, _) => ui::text_secondary(),
        };
        let id: ElementId = match which {
            Control::Min => "titlebar-min".into(),
            Control::Max => "titlebar-max".into(),
            Control::Close => "titlebar-close".into(),
        };
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .w(px(BUTTON_WIDTH))
            .h_full()
            .flex_none()
            .when(hovered, |el| {
                el.bg(if danger {
                    rgb8(CLOSE_HOVER_BG.0, CLOSE_HOVER_BG.1, CLOSE_HOVER_BG.2)
                } else {
                    ui::border_default()
                })
            })
            .on_hover(cx.listener(move |this: &mut Self, hovered: &bool, _window, cx| {
                let next = if *hovered {
                    Some(which)
                } else if this.hovered_control == Some(which) {
                    None
                } else {
                    // 别人的「离开」不该把当前这颗的高亮抹掉
                    return;
                };
                if this.hovered_control != next {
                    this.hovered_control = next;
                    cx.notify();
                }
            }))
            .tooltip(move |window, cx| Tooltip::new(t("app", tip)).build(window, cx))
            .child(VectorIcon::new(shapes, px(10.0)).ink(ink))
    }

    /// 项目切换胶囊(触发按钮 + 展开时的下拉)。
    fn switcher(
        &self,
        project_name: String,
        active_kind: Option<AiProjectKind>,
        entries: Vec<AiProjectEntry>,
        active_project_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.switcher_open;
        // 当前项目自己的 AI 状态色点;没有 AI 会话时压暗,和全局灯一个口径
        let dot_color = active_kind.map(kind_color).unwrap_or_else(ui::text_muted);
        let dot_alpha = if active_kind.is_some() { 1.0 } else { 0.45 };

        let button = div()
            .id("titlebar-switcher")
            .flex()
            .items_center()
            .gap(px(6.0))
            .max_w(px(220.0))
            .h(px(22.0))
            .pl(px(8.0))
            .pr(px(6.0))
            .rounded_full()
            .border_1()
            .border_color(ui::border_default())
            .bg(ui::bg_elevated())
            .text_size(ui::font_px(12.0))
            .text_color(ui::text_primary())
            .cursor_pointer()
            .hover(|el| el.border_color(ui::accent()).bg(ui::border_subtle()))
            .tooltip(move |window, cx| {
                Tooltip::new(t("app", "titleBar.projectSwitcher")).build(window, cx)
            })
            .on_click(cx.listener(|this, _event, window, cx| {
                if this.switcher_open {
                    this.dismiss_switcher(window, cx);
                } else {
                    this.open_switcher(window, cx);
                }
            }))
            .child(
                div()
                    .w(px(6.0))
                    .h(px(6.0))
                    .flex_none()
                    .rounded_full()
                    .bg(ui::with_alpha(dot_color, dot_alpha)),
            )
            .child(div().flex_1().overflow_hidden().child(div().truncate().child(project_name)))
            .child(
                div()
                    .flex_none()
                    .child(
                        VectorIcon::new(ICON_CHEVRON_DOWN, px(9.0))
                            .ink(ui::text_muted())
                            // 展开时 `rotate(180deg)` = 半圈(`VectorIcon::rotation` 的单位是圈)
                            .rotation(if open { 0.5 } else { 0.0 }),
                    ),
            );

        let mut container = div().relative().flex_none().child(button);
        if open {
            container = container
                .child(self.switcher_backdrop(window, cx))
                .child(self.switcher_panel(entries, active_project_id, cx));
        }
        container.into_any_element()
    }

    /// 点外关闭用的全窗遮罩。照抄 [`crate::menu`] 的做法:`occlude` 让它吃掉这一下,
    /// 否则「关下拉」那一次点击会同时点到底下的终端/项目行。
    fn switcher_backdrop(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let size = window.viewport_size();
        deferred(
            anchored().position(point(px(0.0), px(0.0))).child(
                div()
                    .w(size.width)
                    .h(size.height)
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _event: &MouseDownEvent, window, cx| {
                            this.dismiss_switcher(window, cx);
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, _event: &MouseDownEvent, window, cx| {
                            this.dismiss_switcher(window, cx);
                        }),
                    ),
            ),
        )
        .into_any_element()
    }

    /// 下拉面板本体。**不裁剪、不限条数**(与托盘的 `trayMaxProjects` 不同),
    /// 靠 `max-w` + `truncate` 处理溢出。
    fn switcher_panel(
        &self,
        entries: Vec<AiProjectEntry>,
        active_project_id: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut panel = div()
            // 焦点收在面板上,免得下拉开着时打字落进终端。原版没有 Esc 关闭、
            // 也没有键盘导航(那是 `ProjectSwitcher` / audit #26 的事),所以这里
            // 只登记焦点、不挂按键。
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .min_w(px(220.0))
            .max_w(px(320.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(ui::border_default())
            .bg(ui::bg_elevated())
            .shadow_lg()
            .overflow_hidden()
            // 面板内的按下不算「点外」
            .occlude();

        if entries.is_empty() {
            panel = panel.child(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(t("app", "titleBar.noAiProjects")),
            );
        }
        for entry in entries {
            let is_active = active_project_id.as_deref() == Some(entry.id.as_str());
            let target = entry.id.clone();
            panel = panel.child(
                div()
                    .id(SharedString::from(format!("titlebar-proj-{}", entry.id)))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(12.0))
                    .py(px(6.0))
                    .text_size(ui::font_px(12.0))
                    .cursor_pointer()
                    .map(|el| {
                        if is_active {
                            el.bg(ui::accent_subtle()).text_color(ui::accent())
                        } else {
                            el.text_color(ui::text_primary())
                                .hover(|el| el.bg(ui::border_subtle()))
                        }
                    })
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        // 先收下拉(顺带把焦点还回去)再跑动作 —— 切项目会重建
                        // 终端视图并聚焦它,反过来会被还原焦点抢走光标
                        this.dismiss_switcher(window, cx);
                        let store = this.store.clone();
                        // 定位不到目标(pane 已安静)也要把项目切过去,不能没反应
                        if !focus_attention_target(&store, Some(&target), window, cx) {
                            store.update(cx, |store, cx| store.set_active_project(&target, cx));
                        }
                    }))
                    // 左:6px 状态点(**不压暗** —— 这一行必然有 AI 会话)
                    .child(
                        div()
                            .w(px(6.0))
                            .h(px(6.0))
                            .flex_none()
                            .rounded_full()
                            .bg(kind_color(entry.kind)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child(div().truncate().child(entry.name.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(ui::text_muted())
                            .child(t("app", entry.kind.tray_status_key())),
                    ),
            );
        }

        // `absolute top:100% left:0` 挂在胶囊下方;外面套 `anchored()`(不给
        // position = 以自己的布局位置为锚点)白拿贴边收拢,`deferred` 把它抬到
        // 常规内容之上(否则标题栏是根 flex-col 的**首个** child,会被下面的三栏盖住)
        deferred(
            div()
                .absolute()
                .left_0()
                .top(relative(1.0))
                .mt(px(6.0))
                .child(anchored().snap_to_window_with_margin(px(4.0)).child(panel)),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// 全局状态灯。点一下跳到「下一件该我做的事」(不限项目)。
    fn status_light(
        &self,
        light: TitleBarLight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let color = light_color(light);
        let alpha = if light == TitleBarLight::Idle { 0.45 } else { 1.0 };
        // 原版 `group-hover:scale-125`(8px → 10px)
        let size = if self.light_hovered { 10.0 } else { 8.0 };
        let tip = light.i18n_key();

        let dot = div()
            .w(px(size))
            .h(px(size))
            .rounded_full()
            .bg(ui::with_alpha(color, alpha));
        // `working` 档闪烁(`animate-blink`),相位来自低频泵
        // (`mt_ui::motion::pulse_phase`)—— 静态档连泵都不挂。
        // ⚠️ 还要过减弱动效的闸(`mt_ui::motion`):原版的通配规则把
        // `.animate-blink` 停在第一帧 —— 它**不在** reduce 的豁免名单里,
        // 装机版在用户机器上就是不闪的。
        let dot: AnyElement = if light == TitleBarLight::Working && mt_ui::motion::blinks() {
            let phase = blink_phase(mt_ui::motion::pulse_phase(
                std::time::Duration::from_millis(800),
                window,
                cx,
            ));
            let side = px(size - (size * 0.25) * phase);
            dot.w(side)
                .h(side)
                .opacity(1.0 - 0.8 * phase)
                .into_any_element()
        } else {
            dot.into_any_element()
        };

        div()
            .id("titlebar-light")
            .h_full()
            // 原版是品牌容器 `gap-1.5` + 按钮自己的 `px-1.5`
            .ml(px(6.0))
            .px(px(6.0))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .on_hover(cx.listener(|this: &mut Self, hovered: &bool, _window, cx| {
                if this.light_hovered != *hovered {
                    this.light_hovered = *hovered;
                    cx.notify();
                }
            }))
            .tooltip(move |window, cx| Tooltip::new(t("app", tip)).build(window, cx))
            .on_click(cx.listener(|this, _event, window, cx| {
                let store = this.store.clone();
                focus_attention_target(&store, None, window, cx);
            }))
            .child(dot)
            .into_any_element()
    }
}

impl Render for TitleBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (light, entries, active_project, active_project_id) = {
            let store = self.store.read(cx);
            // 状态灯与胶囊下拉**合成一次全 pane 遍历**(`title_bar_snapshot`)。
            // 拆成两个 getter 会各扫一遍 `pane_refs(None)`,而标题栏挂了
            // `window_control_area`、套不了 view 级缓存,那两遍是每帧都来的。
            // done 判据仍是 `aiDoneOrder`(不看窗口焦点),与托盘的
            // `unreadDonePaneIds` 口径**有意不同**。
            let (light, projects) = store.title_bar_snapshot();
            (
                light,
                projects.entries,
                store.active_project().map(|p| p.name.clone()),
                store.active_project_id.clone(),
            )
        };
        // 当前项目的 AI 状态档位;`None` = 当前项目没有 AI 会话
        let active_kind = active_project_id
            .as_deref()
            .and_then(|id| entries.iter().find(|e| e.id == id))
            .map(|e| e.kind);

        let maximized = window.is_maximized();
        let (max_shapes, max_tip) = max_button_face(maximized);
        let is_mac = cfg!(target_os = "macos");
        // Linux 的 `on_hit_test_window_control` 是空实现 → 三键必须靠 `on_click`;
        // Windows 上挂了会与系统动作**双双触发**(见模块注释)
        let click_fallback = cfg!(target_os = "linux");

        div()
            .w_full()
            .h(px(HEIGHT))
            .flex_none()
            .flex()
            .bg(ui::bg_surface())
            .border_b_1()
            .border_color(ui::border_subtle())
            // macOS 把左上角让给系统交通灯
            .when(is_mac, |el| el.child(div().w(px(MAC_TRAFFIC_LIGHT_WIDTH)).flex_none()))
            // 品牌段 —— 拖拽区之一。⚠️ 挂了 Drag 之后里面不能再放可点元素
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(12.0))
                    .pr(px(6.0))
                    .flex_none()
                    .window_control_area(WindowControlArea::Drag)
                    .child(VectorIcon::new(ICON_LOGO, px(14.0)).ink(ui::text_muted()))
                    .child(
                        div()
                            .text_size(ui::font_px(12.0))
                            .text_color(ui::text_secondary())
                            .child("Mini-Term"),
                    )
                    .child(
                        div()
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .child(format!("v{}", env!("CARGO_PKG_VERSION"))),
                    ),
            )
            // 项目切换胶囊。**没有项目时整块(含竖分隔线)都不渲染**
            .children(active_project.map(|name| {
                div()
                    .flex()
                    .items_center()
                    .flex_none()
                    // 原版这几件同在品牌容器的 `gap-1.5` 里,分隔线自己再带 `mx-1`
                    // —— 两侧各 10px。拆成两段之后靠 `gap` + `mx` 还原同一间距。
                    .gap(px(6.0))
                    // 竖分隔线:纯文字紧挨版本号会被误读成标题的一部分
                    .child(
                        div()
                            .mx(px(4.0))
                            .w(px(1.0))
                            .h(px(14.0))
                            .flex_none()
                            .bg(ui::border_default()),
                    )
                    .child(self.switcher(
                        name,
                        active_kind,
                        entries,
                        active_project_id.clone(),
                        window,
                        cx,
                    ))
            }))
            .child(self.status_light(light, window, cx))
            // 中段留白 —— 主要的拖拽区
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .window_control_area(WindowControlArea::Drag),
            )
            // 窗口控制 —— macOS 用系统交通灯,这里不画(两套并存会撞在一起)
            .when(!is_mac, |el| {
                el.child(
                    div()
                        .flex()
                        .flex_none()
                        .child(
                            self.control_button(
                                Control::Min,
                                ICON_MINIMIZE,
                                "titleBar.minimize",
                                cx,
                            )
                            .window_control_area(WindowControlArea::Min)
                            .when(click_fallback, |el| {
                                el.on_click(|_event, window, cx| minimize(window, cx))
                            }),
                        )
                        .child(
                            self.control_button(Control::Max, max_shapes, max_tip, cx)
                                .window_control_area(WindowControlArea::Max)
                                .when(click_fallback, |el| {
                                    el.on_click(|_event, window, cx| toggle_maximize(window, cx))
                                }),
                        )
                        .child(
                            self.control_button(Control::Close, ICON_CLOSE, "titleBar.close", cx)
                                .window_control_area(WindowControlArea::Close)
                                .when(click_fallback, |el| {
                                    el.on_click(|_event, window, cx| {
                                        request_close_window(window, cx)
                                    })
                                }),
                        ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 形状表的点必须全落在单位方框内 —— 越界会画到相邻按钮上。
    /// (mt-ui 与 M 批边条各有一份同款体检,这里是标题栏这五张表的。)
    #[test]
    fn 标题栏图标的点全在单位方框内() {
        let mut points = 0usize;
        for shapes in [
            ICON_MINIMIZE,
            ICON_MAXIMIZE,
            ICON_RESTORE,
            ICON_CLOSE,
            ICON_LOGO,
            ICON_CHEVRON_DOWN,
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
        assert!(points > 30, "形状表看起来没被遍历到(只有 {points} 个点)");
    }

    /// 形状表拍平成点列。`&'static [Shape]` 是 const,用 `ptr::eq` 比较不可靠
    /// (每个使用点都可能各自内联出一份),按几何数据比才是真的。
    fn dump(shapes: &[Shape]) -> Vec<Vec<(f32, f32)>> {
        shapes.iter().map(|s| s.geom.points().0).collect()
    }

    /// 最大化键两态:图形与 tooltip 必须一起换,只换一个就会出现
    /// 「画着还原、写着最大化」。
    #[test]
    fn 最大化键两态一起换() {
        let (shapes, tip) = max_button_face(false);
        assert_eq!(dump(shapes), dump(ICON_MAXIMIZE));
        assert_eq!(tip, "titleBar.maximize");

        let (shapes, tip) = max_button_face(true);
        assert_eq!(dump(shapes), dump(ICON_RESTORE));
        assert_eq!(tip, "titleBar.restore");

        // 还原态是「两笔」:后层只画露出来的两条边(4 个顶点的开放折线),
        // 画成完整方框会糊成一团;最大化态是单独一个方框。
        assert_eq!(ICON_RESTORE.len(), 2);
        assert_eq!(ICON_MAXIMIZE.len(), 1);
        assert_eq!(dump(ICON_RESTORE)[1].len(), 5);
        assert_ne!(dump(ICON_MAXIMIZE), dump(ICON_RESTORE));
    }

    #[test]
    fn 关窗风险只预览前五项并汇总剩余数量() {
        let items = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
        ]
        .map(str::to_string)
        .to_vec();
        let preview = close_risk_preview(&items);
        for name in &items[..CLOSE_RISK_PREVIEW_LIMIT] {
            assert!(preview.contains(name), "{preview}");
        }
        assert!(!preview.contains("foxtrot"), "{preview}");
        assert_eq!(preview.lines().count(), CLOSE_RISK_PREVIEW_LIMIT + 1);
        assert!(preview.contains('3'), "剩余数量未显示:{preview}");
    }

    /// 三键的 tooltip key 都在字典里(拼错的后果是空 tooltip,真机上很难发现)。
    #[test]
    fn 窗口控制键文案_key_齐全() {
        let mut keys = vec!["titleBar.minimize", "titleBar.close", "titleBar.projectSwitcher", "titleBar.noAiProjects"];
        keys.push(max_button_face(false).1);
        keys.push(max_button_face(true).1);
        for key in keys {
            for locale in mt_i18n::Locale::ALL {
                assert!(
                    mt_i18n::lookup(locale, "app", key).is_some(),
                    "字典缺条目 app.{key}({locale})"
                );
            }
        }
    }

    /// 闪烁相位:两端是 0(全亮全尺寸)、正中是 1(最暗最小),且全程落在 0..1。
    #[test]
    fn 闪烁相位在中点折返() {
        assert!(blink_phase(0.0).abs() < 1e-6);
        assert!((blink_phase(1.0)).abs() < 1e-6);
        assert!((blink_phase(0.5) - 1.0).abs() < 1e-6);
        // 单调:前半段升,后半段降
        assert!(blink_phase(0.25) < blink_phase(0.4));
        assert!(blink_phase(0.6) > blink_phase(0.75));
        for i in 0..=100 {
            let v = blink_phase(i as f32 / 100.0);
            assert!((0.0..=1.0).contains(&v), "相位越界 {v}");
        }
    }

    /// 五档灯色互不相同(除了 idle 用 `--text-muted`,其余四档各是一种语义色)——
    /// 撞色等于状态编码失效。
    #[test]
    fn 状态灯五档取色互不相同() {
        let colors = [
            light_color(TitleBarLight::Error),
            light_color(TitleBarLight::Attention),
            light_color(TitleBarLight::Working),
            light_color(TitleBarLight::Done),
            light_color(TitleBarLight::Idle),
        ];
        for (i, a) in colors.iter().enumerate() {
            for (j, b) in colors.iter().enumerate() {
                assert!(i == j || a != b, "第 {i} 档与第 {j} 档撞色");
            }
        }
        // 胶囊/下拉的四档色与状态灯同一张表
        assert_eq!(kind_color(AiProjectKind::Attention), colors[1]);
        assert_eq!(kind_color(AiProjectKind::Working), colors[2]);
        assert_eq!(kind_color(AiProjectKind::Done), colors[3]);
        assert_eq!(kind_color(AiProjectKind::Idle), colors[4]);
    }
}
