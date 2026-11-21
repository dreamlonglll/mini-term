//! 弹窗宿主:全仓弹窗的开 / 关 / 栈 / 渲染(替 gpui-component `Root` 的弹窗栈)。
//!
//! # 为什么自建
//!
//! 上游 `Dialog`(0.6.2 `dialog.rs:506-731`)的进出场是整层连遮罩 opacity 从 0 淡入、面板从
//! 窗口顶边滑到位,关闭时 `Root` 当场出栈(`root.rs:325-371`)、没有退场 —— 正是用户口径禁止
//! 的「从 0 淡入 + 无退场」。退场那几帧上游给不了:builder 存在 `Root` 的私有字段里,出栈
//! 即没;上游元素的焦点句柄 / 层号也是 crate 私有,换不了宿主。于是整套搬过来:
//!
//! - 栈与焦点链在这里(每扇窗口一份,[`DialogHosts`] 全局);
//! - [`Dialog`] 是与上游同名同方法的构造器,调用点只改 `use`;
//! - 交互逐条照抄 gpui-base `dialog.rs` 的宿主(键位上下文 `"Dialog"`、Esc → `Cancel`、
//!   回车 → `Confirm`、遮罩按下即关、标题栏高度以内不算点遮罩、焦点陷阱)与 gpui-component
//!   `Dialog` 的面板版式(底色 / 边框 / `radius_lg` / 16px 内边距 / gap / `min_h_24` / 阴影 /
//!   宽度与 `max_h` 的算法 / 默认距顶视口 1/10 / 叠层每层下移 16px)。
//!
//! `gpui_component::Root` 仍是窗口根:Tab 焦点循环(它按 `focus_trap` 登记的陷阱绕圈)、
//! 通知层、tooltip 层都还靠它。
//!
//! # 动画
//!
//! - **遮罩**压暗渐变进出(半透明黑底,不是内容闪现);
//! - **面板**是与 SSH 浮层同一语言的裁剪窗展开([`crate::overlay_motion::reveal_box`]):
//!   按最终尺寸排好,框体与裁剪窗从面板顶边水平中点的小框长到全尺寸,`OVERLAY_IN`;收起
//!   反向 `OVERLAY_OUT`,播完才卸。面板内容全程不透明;
//! - 叠开的第二层同样播,关上层不影响下层;半路关掉从当前进度掉头;
//! - 「启用动画」总开关关掉时直接开 / 直接关([`Presence`] 自带)。
//!
//! ## 叠层压暗
//!
//! 上游只有**最上面那层**画遮罩色(下层弹窗被它一起压暗)。叠开第二层时遮罩从下层底下
//! 挪到两层之间 —— 直接挪就是下层面板「啪」地一暗。这里按各层在场度现算每层遮罩的
//! alpha([`dim_alphas`]):背景始终压在「最在场的那一层」的暗度上,下层面板随上层的在场度
//! 渐暗 / 渐亮,终态与上游逐值相同。
//!
//! # 退场期间
//!
//! - 不接鼠标:遮罩不再拦点击、不再是标题栏拖拽区,面板上盖一块拦鼠标的 hitbox;
//! - 不吃键盘:焦点在退场开始那一刻就还给打开前的句柄;
//! - **不算「有弹窗」**([`has_open`]),但**仍算「挂着」**([`is_mounted`]):HTML 预览的
//!   WebView 让位看后者,否则它会盖在正在收起的弹窗上;抢焦点类的判据看前者。

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Instant;

use gpui::{
    AnyElement, App, BoxShadow, ClickEvent, Edges, FocusHandle, Global, Hsla, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Pixels, Role, StatefulInteractiveElement,
    StyleRefinement, Styled, WeakFocusHandle, Window, WindowControlArea, WindowId, anchored,
    deferred, div, hsla, point, prelude::FluentBuilder, px,
};
use gpui_base::{ElementExt as _, FocusTrapElement as _, TextSelection, TextSelectionScopeId};
use gpui_component::dialog::{Cancel, Confirm, DialogTitle};
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::{ActiveTheme as _, StyledExt as _, TITLE_BAR_HEIGHT, v_flex, window_paddings};
use mt_ui::motion::{OVERLAY_IN, OVERLAY_OUT, animations_enabled};

use crate::overlay_motion::{Presence, chrome_style, reveal_box};

type Decision = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App) -> bool>;
type Closed = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type Builder = Rc<dyn Fn(Dialog, &mut Window, &mut App) -> Dialog>;

/// 叠层每层下移多少(上游 `layer_ix * 16`)。
const LAYER_STEP: f32 = 16.0;
/// 弹窗层的 `deferred` 优先级起点(上游 `10 + 层号`)。必须压不过右键菜单
/// (`menu::MENU_PRIORITY` = 100):弹窗里弹出来的菜单要画在弹窗上面。
const PRIORITY_BASE: usize = 10;
const PRIORITY_MAX: usize = 90;

// ─── 构造器 ──────────────────────────────────────────────────

/// 一个弹窗的内容与选项。与 `gpui_component::dialog::Dialog` 同名同方法(调用点只改 `use`),
/// 每帧由宿主新建一个交给 builder 填。
///
/// 方法集以全仓调用点为准:上游那几个这里**没有**的 —— `trigger`(本仓弹窗全是命令式打开)、
/// `content` / `button_props` / `width` / `max_w` / `overlay(false)`(没人用),要用时照上游补;
/// `close_button` 只留开关 —— 宿主不画上游那颗 ✕,[`crate::prompt::open_guarded`] 一律把它
/// 关掉,要 ✕ 的弹窗用 `prompt::dialog_title` 自绘(判据见那里的注释)。
pub struct Dialog {
    style: StyleRefinement,
    children: Vec<AnyElement>,
    title: Option<AnyElement>,
    footer: Option<AnyElement>,
    width: Pixels,
    margin_top: Option<Pixels>,
    close_button: bool,
    overlay_closable: bool,
    keyboard: bool,
    on_ok: Decision,
    on_cancel: Decision,
    on_close: Closed,
}

impl Dialog {
    fn new() -> Self {
        Self {
            style: StyleRefinement::default(),
            children: Vec::new(),
            title: None,
            footer: None,
            // 上游 `DialogProps::default` 的 448
            width: px(448.0),
            margin_top: None,
            close_button: true,
            overlay_closable: true,
            keyboard: true,
            on_ok: Rc::new(|_, _, _| true),
            on_cancel: Rc::new(|_, _, _| true),
            on_close: Rc::new(|_, _, _| {}),
        }
    }

    /// 标题行(上游 `DialogTitle`:`text_base` + 半粗 + 1.25 行高,左右与正文同一份内边距)。
    pub fn title(mut self, title: impl IntoElement) -> Self {
        self.title = Some(title.into_any_element());
        self
    }

    /// 页脚(按钮行)。
    pub fn footer(mut self, footer: impl IntoElement) -> Self {
        self.footer = Some(footer.into_any_element());
        self
    }

    /// 关窗回调:确定 / 取消 / Esc / 点遮罩四条路都调,程序化关闭([`close`])不调。
    pub fn on_close(
        mut self,
        on_close: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_close = Rc::new(on_close);
        self
    }

    /// 确定(回车 / 派发 `Confirm`)。返回 `true` 关窗,`false` 留着。
    pub fn on_ok(
        mut self,
        on_ok: impl Fn(&ClickEvent, &mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        self.on_ok = Rc::new(on_ok);
        self
    }

    /// 取消(Esc / 派发 `Cancel` / 点遮罩)。返回 `true` 关窗,`false` 留着。
    pub fn on_cancel(
        mut self,
        on_cancel: impl Fn(&ClickEvent, &mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        self.on_cancel = Rc::new(on_cancel);
        self
    }

    /// 见类型注释:只留开关,宿主不画上游那颗 ✕。
    pub fn close_button(mut self, close_button: bool) -> Self {
        self.close_button = close_button;
        self
    }

    /// 距顶多少;缺省视口高的 1/10。
    pub fn margin_top(mut self, margin_top: impl Into<Pixels>) -> Self {
        self.margin_top = Some(margin_top.into());
        self
    }

    /// 宽度,缺省 448。永远不比视口两侧各留一份 `lg` 间距之后还宽。
    pub fn w(mut self, width: impl Into<Pixels>) -> Self {
        self.width = width.into();
        self
    }

    /// 点遮罩关不关(缺省关,走 `on_cancel`)。
    pub fn overlay_closable(mut self, overlay_closable: bool) -> Self {
        self.overlay_closable = overlay_closable;
        self
    }

    /// Esc / 回车认不认(缺省认)。`false` 时不挂 `"Dialog"` 键位上下文,按钮派发的
    /// `Confirm` / `Cancel` 照样处理。
    pub fn keyboard(mut self, keyboard: bool) -> Self {
        self.keyboard = keyboard;
        self
    }
}

impl ParentElement for Dialog {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl Styled for Dialog {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl FluentBuilder for Dialog {}

// ─── 栈 ──────────────────────────────────────────────────────

struct Layer {
    id: u64,
    /// 覆盖物种类([`crate::overlay::kind`])。程序化关闭按它找层。
    kind: &'static str,
    focus: FocusHandle,
    /// 打开前的焦点,关闭时还回去。
    prev_focus: Option<WeakFocusHandle>,
    builder: Builder,
    presence: Presence,
    closing: bool,
    /// 位置档(每档下移 16px)。开着的层每帧按「下面还有几层开着」现算,退场中的冻住。
    slot: usize,
}

/// 关一层之后宿主外面要补的事(焦点)。
struct ClosePlan {
    is_top: bool,
    prev_focus: Option<WeakFocusHandle>,
    own_focus: FocusHandle,
}

#[derive(Default)]
struct HostState {
    layers: Vec<Layer>,
    next_id: u64,
    /// 毛玻璃快照的淡入淡出(见 [`frost_alpha`])。
    frost: Option<Presence>,
    /// 自绘模态(用量统计)的进出场,按名字分(见 [`shell_frame`])。
    shells: HashMap<&'static str, Presence>,
}

impl HostState {
    /// 退场播完的层出栈。
    fn prune(&mut self, now: Instant) {
        self.layers
            .retain(|layer| !layer.closing || layer.presence.mounted_at(now));
    }

    fn has_open(&self) -> bool {
        self.layers.iter().any(|layer| !layer.closing)
    }

    fn is_mounted(&self, now: Instant) -> bool {
        self.layers
            .iter()
            .any(|layer| !layer.closing || layer.presence.mounted_at(now))
    }

    fn is_layer_open(&self, id: u64) -> bool {
        self.layers
            .iter()
            .any(|layer| layer.id == id && !layer.closing)
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        kind: &'static str,
        focus: FocusHandle,
        prev_focus: Option<WeakFocusHandle>,
        builder: Builder,
        now: Instant,
        animate: bool,
    ) -> u64 {
        self.prune(now);
        // 同种类还在退场的那层当场撤掉:两份可能挂着同一个视图实体(全局搜索框是常驻
        // 实体),同一实体一帧不能挂两次
        self.layers
            .retain(|layer| !(layer.closing && layer.kind == kind));
        self.next_id += 1;
        let slot = self.layers.iter().filter(|layer| !layer.closing).count();
        let mut presence = Presence::hidden(OVERLAY_IN, OVERLAY_OUT);
        presence.retarget(1.0, now, animate);
        self.layers.push(Layer {
            id: self.next_id,
            kind,
            focus,
            prev_focus,
            builder,
            presence,
            closing: false,
            slot,
        });
        self.next_id
    }

    /// 开始收起一层。已经在收(或不存在)返回 `None`。
    ///
    /// 收的若不是最上面那层(确定回调里同步又开了一层,之后才返回 `true`):焦点归上面那层,
    /// 这里不还;上面那层记的「打开前焦点」若落在本层里(本层的输入框),改记本层的,
    /// 免得它关的时候把焦点还给一个已经不画的元素。
    fn begin_close(
        &mut self,
        id: u64,
        now: Instant,
        animate: bool,
        inside: impl Fn(&FocusHandle, &FocusHandle) -> bool,
    ) -> Option<ClosePlan> {
        let index = self
            .layers
            .iter()
            .position(|layer| layer.id == id && !layer.closing)?;
        let is_top = !self.layers[index + 1..].iter().any(|layer| !layer.closing);
        let layer = &mut self.layers[index];
        layer.closing = true;
        layer.presence.retarget(0.0, now, animate);
        let plan = ClosePlan {
            is_top,
            prev_focus: layer.prev_focus.clone(),
            own_focus: layer.focus.clone(),
        };
        if !is_top
            && let Some(upper) = self.layers[index + 1..]
                .iter_mut()
                .find(|layer| !layer.closing)
        {
            let lands_inside = upper
                .prev_focus
                .as_ref()
                .and_then(WeakFocusHandle::upgrade)
                .is_some_and(|focus| inside(&plan.own_focus, &focus));
            if lands_inside {
                upper.prev_focus = plan.prev_focus.clone();
            }
        }
        self.prune(now);
        Some(plan)
    }

    /// 每帧现算位置档(开着的层)。
    fn assign_slots(&mut self) {
        let open = self.layers.iter_mut().filter(|layer| !layer.closing);
        for (open_below, layer) in open.enumerate() {
            layer.slot = open_below;
        }
    }
}

/// 每扇窗口一份栈。
#[derive(Default)]
struct DialogHosts(HashMap<WindowId, HostState>);

impl Global for DialogHosts {}

fn with_host<R>(window: &Window, cx: &mut App, f: impl FnOnce(&mut HostState) -> R) -> R {
    let id = window.window_handle().window_id();
    f(cx.default_global::<DialogHosts>().0.entry(id).or_default())
}

fn read_host<R>(window: &Window, cx: &App, f: impl FnOnce(&HostState) -> R) -> Option<R> {
    let id = window.window_handle().window_id();
    cx.try_global::<DialogHosts>()?.0.get(&id).map(f)
}

// ─── 对外入口 ────────────────────────────────────────────────

/// 开一个弹窗(由 [`crate::prompt::open_guarded_with_close`] 调,别处别直接用 —— 防叠开与覆盖物
/// 栈登记在那边)。
///
/// 与上游 `Root::open_dialog` 同:记下打开前的焦点,弹窗一开就把焦点抢到自己身上(所以输入框
/// 的聚焦要排在打开之后,见 `prompt::autofocus`),清掉背景里残留的选区。
pub fn open(
    kind: &'static str,
    window: &mut Window,
    cx: &mut App,
    build: impl Fn(Dialog, &mut Window, &mut App) -> Dialog + 'static,
) {
    let prev_focus = window.focused(cx).map(|focus| focus.downgrade());
    let focus = cx.focus_handle();
    focus.focus(window, cx);
    let now = Instant::now();
    let animate = animations_enabled();
    with_host(window, cx, |host| {
        host.push(kind, focus, prev_focus, Rc::new(build), now, animate);
    });
    TextSelection::clear(window, cx);
    window.refresh();
}

/// 程序化关掉 `kind` 那一层(最上面开着的那个)。**不调 `on_close`**(与上游
/// `window.close_dialog` 同)。返回这次有没有真关。
///
/// 只该由 [`crate::prompt::close_guarded`] 调:它先判覆盖物栈顶、自己摘登记。
pub fn close(kind: &'static str, window: &mut Window, cx: &mut App) -> bool {
    let id = read_host(window, cx, |host| {
        host.layers
            .iter()
            .rev()
            .find(|layer| !layer.closing && layer.kind == kind)
            .map(|layer| layer.id)
    })
    .flatten();
    id.is_some_and(|id| close_layer(id, window, cx))
}

/// 收起一层:开始退场、焦点还给打开前的句柄(最上面那层才还)、清选区。
fn close_layer(id: u64, window: &mut Window, cx: &mut App) -> bool {
    let now = Instant::now();
    let animate = animations_enabled();
    let plan = {
        let window_ref: &Window = window;
        with_host(window_ref, cx, |host| {
            host.begin_close(id, now, animate, |own, focus| {
                own == focus || own.contains(focus, window_ref)
            })
        })
    };
    let Some(plan) = plan else {
        return false;
    };
    if plan.is_top {
        match plan.prev_focus.as_ref().and_then(WeakFocusHandle::upgrade) {
            Some(prev) => window.focus(&prev, cx),
            // 打开前那个元素已经没了:上游这时焦点留在已经不画的弹窗句柄上,等于没人接键。
            // 这里弹窗还要画一阵退场,焦点留着就会让正在收的输入框继续收字 —— 直接收掉
            None if plan.own_focus.contains_focused(window, cx) => window.blur(cx),
            None => {}
        }
    }
    TextSelection::clear(window, cx);
    window.refresh();
    true
}

/// 这扇窗口有没有**开着**的弹窗(退场中的不算)。抢焦点类的判据用它:弹窗收起那一刻焦点
/// 已经还回去了,上游这时弹窗也已出栈。
pub fn has_open(window: &Window, cx: &App) -> bool {
    read_host(window, cx, HostState::has_open).unwrap_or(false)
}

/// 这扇窗口有没有**还挂着**的弹窗(含退场中的)。HTML 预览的 WebView 让位用它 —— 退场期间
/// 照样让,否则 WebView 会盖在正在收起的弹窗上。
pub fn is_mounted(window: &Window, cx: &App) -> bool {
    let now = Instant::now();
    read_host(window, cx, |host| host.is_mounted(now)).unwrap_or(false)
}

fn layer_open(id: u64, window: &Window, cx: &App) -> bool {
    read_host(window, cx, |host| host.is_layer_open(id)).unwrap_or(false)
}

/// 背景(工作区 / 用量面板)的文字选区作用域。弹窗开着时背景挂在这个作用域里,
/// 窗口当前激活的是默认作用域(`Root` 每帧激活它自己那份,而它的弹窗栈此刻是空的)——
/// 背景里的可选文字于是不参与选区,拖选出弹窗也不会选到底下的字,与上游「模态把选区关在
/// 弹窗里」同一效果。最上面那层弹窗留在默认作用域里照常可选,下层与退场中的层也挂进来。
fn background_scope() -> TextSelectionScopeId {
    static SCOPE: OnceLock<TextSelectionScopeId> = OnceLock::new();
    *SCOPE.get_or_init(TextSelectionScopeId::new)
}

/// 弹窗开着时把 `el` 挂进背景选区作用域(见 [`background_scope`]),否则原样返回。
/// 作用域标记不占布局节点、不改元素 id,挂与不挂版式逐像素相同。
pub fn confine_background<E>(el: E, window: &Window, cx: &App) -> AnyElement
where
    E: ParentElement + IntoElement + 'static,
{
    if has_open(window, cx) {
        el.text_selection_scope(background_scope())
            .into_any_element()
    } else {
        el.into_any_element()
    }
}

// ─── 毛玻璃与自绘模态 ────────────────────────────────────────

/// 毛玻璃快照这一帧的不透明度。`target` = 该不该有玻璃(有弹窗 / 用量面板 / 图片浮层
/// 开着),`has_image` = 快照到了没有。
///
/// 玻璃跟着遮罩一起渐入渐出,不「啪」地出现:快照晚到(模糊丢在后台跑)的话从到货那一刻起
/// 淡入;全关时随遮罩淡出,播完调用方才把图丢掉。只在动画进行中要帧。
pub fn frost_alpha(target: bool, has_image: bool, window: &mut Window, cx: &mut App) -> f32 {
    let now = Instant::now();
    let presence = with_host(window, cx, |host| {
        let frost = host
            .frost
            .get_or_insert_with(|| Presence::hidden(OVERLAY_IN, OVERLAY_OUT));
        if !has_image {
            // 没图就没有可淡出的,下次到货从零淡入
            *frost = Presence::hidden(OVERLAY_IN, OVERLAY_OUT);
        } else if target != frost.target_open() {
            if target {
                frost.show(now);
            } else {
                frost.hide(now);
            }
        }
        *frost
    });
    presence.drive(window)
}

/// 自绘模态这一帧怎么画(见 [`shell_frame`])。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShellFrame {
    /// 在场度(已缓动)。遮罩暗度与面板裁剪窗都按它走。
    pub progress: f32,
    /// 目标是开着(退场中是 `false`:不拦点击、不接鼠标)。
    pub open: bool,
}

/// 不走 [`Dialog`] 的自绘模态(用量统计)用同一套外壳的进出场:`open` 是宿主的开关,
/// 变了就起一场。完全收起后返回 `None`(不画)。只在动画进行中要帧。
pub fn shell_frame(
    key: &'static str,
    open: bool,
    window: &mut Window,
    cx: &mut App,
) -> Option<ShellFrame> {
    let now = Instant::now();
    let presence = with_host(window, cx, |host| {
        let presence = host
            .shells
            .entry(key)
            .or_insert_with(|| Presence::hidden(OVERLAY_IN, OVERLAY_OUT));
        if open != presence.target_open() {
            if open {
                presence.show(now);
            } else {
                presence.hide(now);
            }
        }
        *presence
    });
    if !presence.mounted_at(now) {
        return None;
    }
    Some(ShellFrame {
        progress: presence.drive(window),
        open,
    })
}

// ─── 渲染 ────────────────────────────────────────────────────

/// 每层遮罩该压多暗(alpha)。`layers` 自下而上,每层给 `(在场度, 有没有遮罩)`;`base` 是主题
/// 遮罩色的 alpha(black/50)。
///
/// 内容自下而上是「背景,第 0 层面板,第 1 层面板 …」,第 j 层遮罩夹在第 j-1 层面板与第 j 层
/// 面板之间。目标暗度:某块内容被压到「它上面最在场的那一层」的暗度 `base × max(在场度)`。
/// 同色半透明叠加的合成暗度是 `1 - Π(1 - αᵢ)`,于是自上而下逐层反解 α。
///
/// 终态与上游一致:各层全开静止时只有最上面那层是 `base`,其余是 0。
fn dim_alphas(layers: &[(f32, bool)], base: f32) -> Vec<f32> {
    let n = layers.len();
    // darkness[m]:第 m 块内容(0 = 背景,m = 第 m-1 层面板)该有的暗度
    let mut darkness = vec![0.0f32; n + 1];
    let mut strongest = 0.0f32;
    for m in (0..n).rev() {
        let (presence, overlay) = layers[m];
        if overlay {
            strongest = strongest.max(presence.clamp(0.0, 1.0));
        }
        darkness[m] = base * strongest;
    }
    (0..n)
        .map(|j| {
            if !layers[j].1 {
                return 0.0;
            }
            let below = darkness[j];
            let above = darkness[j + 1];
            (1.0 - (1.0 - below) / (1.0 - above).max(f32::EPSILON)).clamp(0.0, 1.0)
        })
        .collect()
}

/// 上游面板的阴影(`shadow_xl` 的等价物,两层各 10% 黑)。
fn panel_shadows() -> Vec<BoxShadow> {
    vec![
        BoxShadow {
            color: hsla(0., 0., 0., 0.1),
            offset: point(px(0.), px(20.)),
            blur_radius: px(25.),
            spread_radius: px(-5.),
            inset: false,
        },
        BoxShadow {
            color: hsla(0., 0., 0., 0.1),
            offset: point(px(0.), px(8.)),
            blur_radius: px(10.),
            spread_radius: px(-6.),
            inset: false,
        },
    ]
}

/// 渲染时一层要的东西(从栈里拷出来,调 builder 时不借着全局)。
struct LayerView {
    id: u64,
    focus: FocusHandle,
    builder: Builder,
    progress: f32,
    closing: bool,
    slot: usize,
}

/// 弹窗层。由窗口根视图画(`main.rs`,替原来的 `Root::render_dialog_layer`)。
/// 退场播完的层在这里出栈;动画进行中顺手要下一帧(通知的是调用方视图)。
pub fn render_layer(window: &mut Window, cx: &mut App) -> Option<AnyElement> {
    let now = Instant::now();
    let (views, running) = with_host(window, cx, |host| {
        host.prune(now);
        host.assign_slots();
        let running = host
            .layers
            .iter()
            .any(|layer| layer.presence.running_at(now));
        let views = host
            .layers
            .iter()
            .map(|layer| LayerView {
                id: layer.id,
                focus: layer.focus.clone(),
                builder: layer.builder.clone(),
                progress: layer.presence.value_at(now),
                closing: layer.closing,
                slot: layer.slot,
            })
            .collect::<Vec<_>>();
        (views, running)
    });
    if views.is_empty() {
        return None;
    }
    if running {
        window.request_animation_frame();
    }
    let top = views.iter().rposition(|layer| !layer.closing);
    // builder 自下而上依次调(与上游同序);调的时候不借着全局,builder 里查宿主也不打架
    let dialogs = views
        .iter()
        .map(|layer| (layer.builder)(Dialog::new(), window, cx))
        .collect::<Vec<_>>();
    let overlay = cx.theme().overlay;
    // 本仓弹窗一律带遮罩(上游 `overlay(false)` 没人用,宿主没收这个开关)
    let dims = dim_alphas(
        &views
            .iter()
            .map(|layer| (layer.progress, true))
            .collect::<Vec<_>>(),
        overlay.a,
    );
    let layers = views
        .into_iter()
        .zip(dialogs)
        .zip(dims)
        .enumerate()
        .map(|(index, ((layer, dialog), dim))| {
            let topmost = Some(index) == top;
            let dim = Hsla { a: dim, ..overlay };
            render_one(index, layer, dialog, topmost, dim, window, cx)
        })
        .collect::<Vec<_>>();
    Some(div().children(layers).into_any_element())
}

/// 一层弹窗:遮罩 + 面板。结构照抄上游(`anchored` 定在窗口内边距处的全视口挡板 →
/// `deferred` 宿主 → 遮罩 + 面板),退场中只拿掉交互,元素 id 不变(面板里的滚动位置
/// 不在退场那几帧跳回顶上)。
#[allow(clippy::too_many_arguments)]
fn render_one(
    index: usize,
    layer: LayerView,
    dialog: Dialog,
    topmost: bool,
    dim: Hsla,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let Dialog {
        style,
        children,
        title,
        footer,
        width,
        margin_top,
        close_button,
        overlay_closable,
        keyboard,
        on_ok,
        on_cancel,
        on_close,
    } = dialog;
    debug_assert!(
        !close_button,
        "弹窗宿主不画上游那颗 ✕:`open_guarded` 一律关掉它,要 ✕ 用 `prompt::dialog_title`"
    );
    let id = layer.id;
    let interactive = !layer.closing;

    // ── 版式:逐项照抄上游 `Dialog::render` ──
    let paddings_of_window = window_paddings(window);
    let view_size = window.viewport_size()
        - gpui::size(
            paddings_of_window.left + paddings_of_window.right,
            paddings_of_window.top + paddings_of_window.bottom,
        );
    let (margin, background, border, radius) = {
        let theme = cx.theme();
        (
            theme.spacing_tokens().lg,
            theme.tokens.background,
            theme.border,
            theme.radius_lg,
        )
    };
    let y = margin_top.unwrap_or(view_size.height / 10.) + px(layer.slot as f32 * LAYER_STEP);
    let width = width.min((view_size.width - margin * 2.).max(px(0.)));
    let x = (view_size.width - width) / 2.;
    let max_height = (view_size.height - y - margin).max(px(0.));

    let base_size = window.text_style().font_size;
    let rem_size = window.rem_size();
    let mut paddings = Edges::all(px(16.));
    if let Some(pl) = style.padding.left {
        paddings.left = pl.to_pixels(base_size, rem_size);
    }
    if let Some(pr) = style.padding.right {
        paddings.right = pr.to_pixels(base_size, rem_size);
    }
    if let Some(pt) = style.padding.top {
        paddings.top = pt.to_pixels(base_size, rem_size);
    }
    if let Some(pb) = style.padding.bottom {
        paddings.bottom = pb.to_pixels(base_size, rem_size);
    }

    // 框体(底色 / 边框 / 圆角 / 阴影)由 reveal_box 按插值矩形画:取值与上游面板自己画的
    // 完全相同(调用方 refine 的覆盖照样生效),面板本体只留版式(边框宽度照占位)
    let chrome = chrome_style(|el| {
        el.bg(background)
            .border_1()
            .border_color(border)
            .rounded(radius)
            .refine_style(&style)
            .shadow(panel_shadows())
    });
    let popup = v_flex()
        .id(("mt-dialog-panel", id))
        .border_1()
        .min_h_24()
        .pt(paddings.top)
        .pb(paddings.bottom)
        .gap(paddings.top.max(px(8.)))
        .refine_style(&style)
        .px_0()
        .bg(gpui::transparent_black())
        .border_color(gpui::transparent_black())
        .shadow_none()
        // 下面这几条优先级最高,调用方覆盖不了(上游原注释)
        .absolute()
        .when(interactive, |el| el.occlude())
        .relative()
        .left(x)
        .top(y)
        .w(width)
        .max_h(max_height)
        .child(
            v_flex()
                .flex_1()
                .overflow_hidden()
                .gap_y_2()
                .when_some(title, |el, title| {
                    el.child(
                        DialogTitle::new()
                            .pl(paddings.left)
                            .pr(paddings.right)
                            .child(title),
                    )
                })
                .when(!children.is_empty(), |el| {
                    el.child(
                        div().flex_1().overflow_hidden().child(
                            v_flex()
                                .size_full()
                                .overflow_y_scrollbar()
                                .pl(paddings.left)
                                .pr(paddings.right)
                                .children(children),
                        ),
                    )
                }),
        )
        .when_some(footer, |el, footer| {
            el.child(div().pl(paddings.left).pr(paddings.right).child(footer))
        });
    let panel = reveal_box(popup, layer.progress)
        .chrome(chrome)
        .interactive(interactive);

    // ── 遮罩:按下即关(左键 + overlay_closable + 最上面那层 + on_cancel 放行);
    // 标题栏高度以内不算点遮罩;整块是标题栏拖拽区(上游同) ──
    let backdrop = {
        let on_cancel = on_cancel.clone();
        let on_close = on_close.clone();
        let closable = overlay_closable && topmost;
        div()
            .absolute()
            .inset_0()
            .when(dim.a > 0.0, |el| el.bg(dim))
            .when(interactive, move |el| {
                el.window_control_area(WindowControlArea::Drag)
                    .on_any_mouse_down(move |event, window, cx| {
                        if event.position.y < TITLE_BAR_HEIGHT {
                            return;
                        }
                        cx.stop_propagation();
                        if event.button != MouseButton::Left || !closable {
                            return;
                        }
                        let click = ClickEvent::default();
                        if on_cancel(&click, window, cx) {
                            close_layer(id, window, cx);
                            on_close(&click, window, cx);
                        }
                    })
            })
    };

    let host = div()
        .id(("mt-dialog-host", id))
        .absolute()
        .top_0()
        .left_0()
        .w(window.viewport_size().width)
        .h(window.viewport_size().height)
        .role(Role::Dialog)
        .when(interactive && keyboard, |el| el.key_context("Dialog"))
        .on_action({
            let on_cancel = on_cancel.clone();
            let on_close = on_close.clone();
            move |_: &Cancel, window, cx| {
                if !layer_open(id, window, cx) {
                    return;
                }
                let click = ClickEvent::default();
                if on_cancel(&click, window, cx) {
                    close_layer(id, window, cx);
                    on_close(&click, window, cx);
                }
            }
        })
        .on_action(move |_: &Confirm, window, cx| {
            if !layer_open(id, window, cx) {
                return;
            }
            let click = ClickEvent::default();
            if on_ok(&click, window, cx) {
                close_layer(id, window, cx);
                on_close(&click, window, cx);
            }
        })
        .child(backdrop)
        .child(panel)
        // Tab 不逃出弹窗:`Root` 的 Tab 处理按登记的陷阱绕圈(上游同一套)
        .focus_trap(("mt-dialog-trap", id), &layer.focus);
    // 只有最上面那层开着的弹窗留在默认选区作用域里(见 `background_scope`)
    let host = if topmost {
        host.into_any_element()
    } else {
        host.text_selection_scope(background_scope())
            .into_any_element()
    };

    let priority = (PRIORITY_BASE + index).min(PRIORITY_MAX);
    anchored()
        .position(point(paddings_of_window.left, paddings_of_window.top))
        .snap_to_window()
        .child(
            div()
                .id(("mt-dialog", id))
                // 全视口挡板:开着时底下的东西一概点不到;退场中撤掉,点击直接落到下面
                .when(interactive, |el| el.occlude())
                .w(view_size.width)
                .h(view_size.height)
                .child(
                    deferred(anchored().position(point(px(0.), px(0.))).child(host))
                        .with_priority(priority),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::time::Duration;

    use gpui::{AppContext as _, Context, Render, TestAppContext};

    /// 单层:全开时与上游一样压 base;叠两层全开时只有上面那层压(下层面板被它一起压暗)。
    #[test]
    fn 遮罩暗度_终态与上游一致() {
        assert_eq!(dim_alphas(&[(1.0, true)], 0.5), vec![0.5]);
        assert_eq!(dim_alphas(&[(1.0, true), (1.0, true)], 0.5), vec![0.0, 0.5]);
        assert_eq!(dim_alphas(&[], 0.5), Vec::<f32>::new());
        // 没有遮罩的层不压,下层照旧压
        assert_eq!(
            dim_alphas(&[(1.0, true), (1.0, false)], 0.5),
            vec![0.5, 0.0]
        );
        // 刚开始打开:还没压
        assert_eq!(dim_alphas(&[(0.0, true)], 0.5), vec![0.0]);
    }

    /// 叠开 / 收起上面那层的全程:背景的合成暗度钉在「最在场那层」上(不忽明忽暗),
    /// 下层面板的暗度随上层在场度单调走。
    #[test]
    fn 遮罩暗度_叠层过渡里背景不闪() {
        let composite = |alphas: &[f32]| 1.0 - alphas.iter().map(|a| 1.0 - a).product::<f32>();
        let mut prev_lower = -1.0;
        for step in 0..=20 {
            let p = step as f32 / 20.0;
            let alphas = dim_alphas(&[(1.0, true), (p, true)], 0.5);
            assert!(
                (composite(&alphas) - 0.5).abs() < 1e-5,
                "背景暗度在 {p} 处漂了"
            );
            // 下层面板只被上层遮罩压
            assert!((alphas[1] - 0.5 * p).abs() < 1e-5);
            assert!(alphas[1] >= prev_lower);
            prev_lower = alphas[1];
        }
        // 唯一一层收起:背景跟着变亮
        let alphas = dim_alphas(&[(0.4, true)], 0.5);
        assert!((composite(&alphas) - 0.2).abs() < 1e-5);
    }

    fn handle_pair(cx: &mut TestAppContext) -> (FocusHandle, FocusHandle) {
        cx.update(|cx| (cx.focus_handle(), cx.focus_handle()))
    }

    fn builder() -> Builder {
        Rc::new(|dialog, _, _| dialog)
    }

    /// 位置档:开着的按「下面还有几层开着」现算;退场中的冻在关闭那一刻,新开的一层顶它的位置
    /// (关了立刻又开时不会先低 16px、等旧的播完再跳上去)。
    #[gpui::test]
    fn 位置档_退场中的冻住_新层顶上它的位置(cx: &mut TestAppContext) {
        let (a, b) = handle_pair(cx);
        let (c, _) = handle_pair(cx);
        let t0 = Instant::now();
        let mut host = HostState::default();
        host.push("a", a, None, builder(), t0, true);
        let b_id = host.push("b", b, None, builder(), t0, true);
        host.assign_slots();
        assert_eq!(
            host.layers.iter().map(|l| l.slot).collect::<Vec<_>>(),
            vec![0, 1]
        );
        // 全开之后再关(刚开那一刻就关的话什么都还没露出来,直接出栈,见下一条用例)
        let t1 = t0 + OVERLAY_IN.duration;
        assert!(host.begin_close(b_id, t1, true, |_, _| false).is_some());
        assert!(host.has_open() && host.is_mounted(t1));
        host.push("c", c, None, builder(), t1, true);
        host.assign_slots();
        assert_eq!(
            host.layers.iter().map(|l| l.slot).collect::<Vec<_>>(),
            vec![0, 1, 1],
            "退场中的 b 冻在 1,新开的 c 也在 1"
        );
        // 退场播完出栈
        host.prune(t1 + OVERLAY_OUT.duration + Duration::from_millis(1));
        assert_eq!(host.layers.len(), 2);
    }

    /// 已经在收的层再关一次是空操作;同种类重开时还在退场的那层当场撤掉;总开关关掉时
    /// 关了就出栈。
    #[gpui::test]
    fn 重复关闭与同种类重开(cx: &mut TestAppContext) {
        let (a, b) = handle_pair(cx);
        let t0 = Instant::now();
        let mut host = HostState::default();
        let id = host.push("k", a, None, builder(), t0, true);
        let t1 = t0 + OVERLAY_IN.duration;
        assert!(host.begin_close(id, t1, true, |_, _| false).is_some());
        assert!(host.begin_close(id, t1, true, |_, _| false).is_none());
        assert!(!host.has_open() && host.is_mounted(t1));
        host.push("k", b, None, builder(), t1, true);
        assert_eq!(host.layers.len(), 1, "同种类的退场层当场撤掉");

        // 刚开那一刻就关:还什么都没露出来,不播退场,直接出栈
        let (d, _) = handle_pair(cx);
        let mut early = HostState::default();
        let id = early.push("k", d, None, builder(), t0, true);
        assert!(early.begin_close(id, t0, true, |_, _| false).is_some());
        assert!(early.layers.is_empty());

        let (c, _) = handle_pair(cx);
        let mut instant = HostState::default();
        let id = instant.push("k", c, None, builder(), t0, false);
        assert!(instant.begin_close(id, t0, false, |_, _| false).is_some());
        assert!(instant.layers.is_empty() && !instant.is_mounted(t0));
    }

    /// 收的不是最上面那层:焦点不归它还;上面那层记的焦点落在它里面的话改记它的。
    #[gpui::test]
    fn 收下层时焦点交给上层记着(cx: &mut TestAppContext) {
        let (before, lower) = handle_pair(cx);
        let (upper, _) = handle_pair(cx);
        let t0 = Instant::now();
        let mut host = HostState::default();
        let lower_id = host.push(
            "lower",
            lower.clone(),
            Some(before.downgrade()),
            builder(),
            t0,
            true,
        );
        host.push("upper", upper, Some(lower.downgrade()), builder(), t0, true);
        let plan = host
            .begin_close(lower_id, t0 + OVERLAY_IN.duration, true, |own, focus| {
                own == focus
            })
            .unwrap();
        assert!(!plan.is_top, "上面还有开着的层");
        let upper_prev = host.layers[1].prev_focus.as_ref().and_then(|w| w.upgrade());
        assert_eq!(upper_prev, Some(before));
    }

    // ── 交互:真窗口里走一遍(不依赖动画时长) ──

    struct Host;
    impl Render for Host {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().children(render_layer(window, cx))
        }
    }

    fn test_window(cx: &mut TestAppContext) -> &mut gpui::VisualTestContext {
        cx.update(gpui_component::init);
        let (_, cx) = cx.add_window_view(|window, cx| {
            let host = cx.new(|_| Host);
            gpui_component::Root::new(host, window, cx)
        });
        cx
    }

    #[derive(Default)]
    struct Calls {
        ok: Cell<usize>,
        cancel: Cell<usize>,
        close: Cell<usize>,
        ok_result: Cell<bool>,
    }

    fn open_counting(calls: &Rc<Calls>, kind: &'static str, window: &mut Window, cx: &mut App) {
        let calls = calls.clone();
        open(kind, window, cx, move |dialog, _, _| {
            let (ok, cancel, close) = (calls.clone(), calls.clone(), calls.clone());
            dialog
                .close_button(false)
                .child("body")
                .on_ok(move |_, _, _| {
                    ok.ok.set(ok.ok.get() + 1);
                    ok.ok_result.get()
                })
                .on_cancel(move |_, _, _| {
                    cancel.cancel.set(cancel.cancel.get() + 1);
                    true
                })
                .on_close(move |_, _, _| close.close.set(close.close.get() + 1))
        });
    }

    /// Esc(`Cancel`)关窗:on_cancel → 关 → on_close,焦点还给打开前的元素;
    /// 回车(`Confirm`)由 on_ok 的返回值决定关不关。
    #[gpui::test]
    fn 取消与确定的关窗路径与焦点还原(cx: &mut TestAppContext) {
        let cx = test_window(cx);
        let before = cx.update(|_, cx| cx.focus_handle());
        let calls = Rc::new(Calls::default());
        cx.update(|window, cx| {
            window.focus(&before, cx);
            open_counting(&calls, "k", window, cx);
            assert!(has_open(window, cx));
            assert!(!before.is_focused(window), "一开就把焦点抢到弹窗上");
        });
        cx.run_until_parked();

        // on_ok 返回 false:不关
        cx.dispatch_action(Confirm { secondary: false });
        cx.run_until_parked();
        assert_eq!(calls.ok.get(), 1);
        cx.update(|window, cx| assert!(has_open(window, cx), "on_ok 返回 false 不关"));

        cx.dispatch_action(Cancel);
        cx.run_until_parked();
        assert_eq!((calls.cancel.get(), calls.close.get()), (1, 1));
        cx.update(|window, cx| {
            assert!(!has_open(window, cx));
            assert!(before.is_focused(window), "焦点在退场开始时就还回去");
        });

        // 回车放行(on_ok 返回 true):关窗 + on_close
        let calls = Rc::new(Calls::default());
        calls.ok_result.set(true);
        cx.update(|window, cx| open_counting(&calls, "k", window, cx));
        cx.run_until_parked();
        cx.dispatch_action(Confirm { secondary: false });
        cx.run_until_parked();
        assert_eq!((calls.ok.get(), calls.close.get()), (1, 1));
        cx.update(|window, cx| {
            assert!(!has_open(window, cx));
            assert!(before.is_focused(window));
        });
    }

    /// 程序化关闭不调 on_close(prompt.rs 那条「第六条路」的语义)。
    #[gpui::test]
    fn 程序化关闭不调_on_close(cx: &mut TestAppContext) {
        let cx = test_window(cx);
        let calls = Rc::new(Calls::default());
        cx.update(|window, cx| open_counting(&calls, "k", window, cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(
                !close(crate::overlay::kind::ALERT, window, cx),
                "种类对不上不关"
            );
            assert!(close("k", window, cx));
            assert!(!close("k", window, cx), "已经在收的不再关一次");
            assert!(!has_open(window, cx));
        });
        assert_eq!(calls.close.get(), 0);
    }

    /// 叠两层:Esc 只关上面那层,焦点还给下层里打开它之前的那个元素;下层照旧开着。
    #[gpui::test]
    fn 叠层只关上面那层(cx: &mut TestAppContext) {
        let cx = test_window(cx);
        let lower = Rc::new(Calls::default());
        let upper = Rc::new(Calls::default());
        let lower_focus = Rc::new(RefCell::new(None));
        cx.update(|window, cx| {
            open_counting(&lower, "lower", window, cx);
            *lower_focus.borrow_mut() = window.focused(cx);
            open_counting(&upper, "upper", window, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(Cancel);
        cx.run_until_parked();
        assert_eq!((upper.cancel.get(), lower.cancel.get()), (1, 0));
        cx.update(|window, cx| {
            assert!(has_open(window, cx), "下层还开着");
            let focus = lower_focus.borrow().clone().unwrap();
            assert!(focus.is_focused(window), "焦点回到下层");
        });
    }

    /// 点遮罩:按下即关;标题栏高度以内不算点遮罩;`overlay_closable(false)` 点了不关。
    #[gpui::test]
    fn 点遮罩关窗与标题栏不算(cx: &mut TestAppContext) {
        let cx = test_window(cx);
        let calls = Rc::new(Calls::default());
        cx.update(|window, cx| open_counting(&calls, "k", window, cx));
        cx.run_until_parked();
        let viewport = cx.update(|window, _| window.viewport_size());
        cx.simulate_click(point(px(4.0), px(4.0)), Default::default());
        cx.run_until_parked();
        cx.update(|window, cx| assert!(has_open(window, cx), "标题栏高度以内不算点遮罩"));
        cx.simulate_click(
            point(px(4.0), viewport.height - px(4.0)),
            Default::default(),
        );
        cx.run_until_parked();
        assert_eq!((calls.cancel.get(), calls.close.get()), (1, 1));
        cx.update(|window, cx| assert!(!has_open(window, cx)));

        cx.update(|window, cx| {
            open("sticky", window, cx, |dialog, _, _| {
                dialog.close_button(false).overlay_closable(false)
            })
        });
        cx.run_until_parked();
        cx.simulate_click(
            point(px(4.0), viewport.height - px(4.0)),
            Default::default(),
        );
        cx.run_until_parked();
        cx.update(|window, cx| assert!(has_open(window, cx)));
    }

    /// `keyboard(false)`:不挂 `"Dialog"` 键位上下文,但按钮派发的动作照样处理。
    #[gpui::test]
    fn 关掉键盘时动作照样处理(cx: &mut TestAppContext) {
        let cx = test_window(cx);
        let closed = Rc::new(Cell::new(0));
        cx.update(|window, cx| {
            let closed = closed.clone();
            open("k", window, cx, move |dialog, _, _| {
                let closed = closed.clone();
                dialog
                    .close_button(false)
                    .keyboard(false)
                    .child("body")
                    .on_close(move |_, _, _| closed.set(closed.get() + 1))
            });
        });
        cx.run_until_parked();
        cx.dispatch_action(Cancel);
        cx.run_until_parked();
        assert_eq!(closed.get(), 1);
        cx.update(|window, cx| assert!(!has_open(window, cx)));
    }
}
