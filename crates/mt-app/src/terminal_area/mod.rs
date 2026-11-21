//! 终端区:SplitNode 树 → 嵌套 resizable + 每个叶子一条 tab 栏。
//!
//! 对应 `src/components/TerminalArea.tsx` + `SplitLayout.tsx` + `PaneGroup.tsx`。
//!
//! - split 节点 → [`gpui_component::resizable`](gpui_component::resizable)
//!   的 `h_resizable` / `v_resizable`(替 Allotment),每个节点一份
//!   `ResizableState`,按节点 id 缓存,拖动后把比例写回 store 并落盘;
//! - leaf 节点 → tab 栏 + 当前激活 pane 的 [`crate::pane::TerminalPane`] 实体。
//!   同一个叶子里的多个 pane 就是「终端标签」,与旧版一致(项目级 tab 层早已删除)。
//!
//! # 分屏比例的跨重启恢复
//!
//! `ResizablePanel` 只吃**像素**初值(`ResizableState` 内部一律按像素算,百分比
//! 没有入口),而布局树与磁盘格式存的是百分比。于是渲染时自上而下带一个「本节点
//! 可用尺寸」参数,逐层按百分比换算成像素喂给 `.size()`:
//!
//! ```text
//! 终端区 bounds(canvas 量出来,跨帧保留)
//!   └─ Split(h, [30,70]) → 子 0 宽 = 可用宽 × 0.30,子 1 宽 = 可用宽 × 0.70
//!        └─ Split(v, [50,50]) → 各自再按自己那块可用高度分
//! ```
//!
//! 初值只在该节点的 `ResizableState` 第一次落地时起作用;用户拖过之后
//! `panel.size` 变成 `Some`,我们喂的初值自动让位,不会与拖动打架。
//!
//! 正因为「只认第一帧」,**首帧必须已经量到真实尺寸**:canvas 是在本帧 prepaint
//! 才回填 `area_size` 的,元素树早在那之前就构造完了。拿兜底尺寸铺出去的话,
//! `ResizableState` 会把按 1200×800 算出来的像素当成自己的初值锁死,窗口比它宽时
//! 多出来的空间被各面板**等分**(每个 panel 都是 `flex_grow: 1`),20/80 的分屏就
//! 会恢复成 35/65。于是首帧只放量尺的 canvas,下一帧再铺分屏树 —— 代价是一帧空白。

use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    AnimationExt as _, AnyElement, App, AppContext, Bounds, ClickEvent, Context, Entity,
    FocusHandle, Hsla, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseUpEvent, ParentElement, Pixels, Render, ScrollHandle, SharedString, Size,
    StatefulInteractiveElement, Styled, Subscription, Task, Window, anchored, canvas, deferred,
    div, point, prelude::FluentBuilder, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use mt_ui::icons::{AiVendor, BrandIcon, Geom, Ink, Shape, ShellIcon, ShellKind, VectorIcon};
use mt_ui::tooltip::TooltipExt as _;

use crate::branch_family;
use crate::command_library::{self, CommandPopover, PopoverEvent};
use crate::focus_nav::{self, Direction, PaneRect};
use crate::i18n::{t, tr};
use crate::markers;
use crate::menu::{self, MenuEntry, MenuItem};
use crate::modal;
use crate::overlay;
use crate::pane_actions;
use crate::pane_preview;
use crate::popover_reveal::{Chrome, RevealState, ShadowMode, anchor_probe, reveal_frame};
use crate::session_branch::{BranchMenuSegment, branch_menu_segment};
use crate::ssh_activity::{ButtonState, SshActivityView};
use crate::store::AppStore;
use crate::tree::{DropZone, PaneState, SplitDirection, SplitNode, StatusLight};
use crate::ui;

/// tab chip 位移补间与关分屏后邻居扩张(FLIP 一族),见该模块注释。
mod fx;

/// 终端区还没量出尺寸时的兜底(首帧)。比例照样对,只是绝对值不准。
const FALLBACK_AREA: Size<Pixels> = Size {
    width: px(1200.0),
    height: px(800.0),
};

mod dnd;
mod icons;
mod layout;
mod leaf;
mod motion;
mod popovers;
mod preview;
mod tab_bar;
mod tab_menu;

use dnd::*;
use icons::*;
use layout::*;
use motion::*;
use popovers::*;
use tab_bar::*;
pub(crate) use tab_menu::{reopen_closed_menu_item, tab_menu};

pub struct TerminalArea {
    store: Entity<AppStore>,
    /// 活动布局树的**跨帧快照**。`render` 要边读这棵树边改视图状态
    /// (`render_node` 是 `&mut self` + `&mut Context`),所以树必须以拥有权的
    /// 形态拿在手上、不能挂在 `store` 的借用上;而每帧深拷贝一棵树是纯浪费。
    /// 折中:与 store 里那棵**逐字段比**,真变了才拷一次,平时只递一次
    /// 引用计数。维护点只有 `render` 开头那一段。
    layout_snapshot: Option<std::rc::Rc<SplitNode>>,
    /// 每个 split 节点一份分隔条状态(跨帧保留,否则每帧都重置回均分)。
    split_states: HashMap<String, Entity<ResizableState>>,
    /// 终端区自身的可用尺寸(canvas 量出来,用于把百分比换算成像素初值)。
    area_size: Size<Pixels>,
    /// 是否已经量到过真实尺寸。没量到之前不铺分屏树(见模块注释)。
    measured: bool,
    /// 每个 pane 在屏幕上的矩形 —— 方向导航按几何最近邻挑目标。
    pane_rects: HashMap<String, PaneRect>,
    /// AI 任务标记浮层开在哪个 pane 上(`None` = 没开)。存 `(pane_id, pty_id)`:
    /// 前者用来实现原版那条「activePane 的 ptyId 一变就无条件关」。
    marker_open: Option<(String, u32)>,
    /// 浮层的焦点句柄。收着焦点才有人接 Esc(与 `menu.rs` 同一套路)。
    marker_focus: FocusHandle,
    /// 开浮层之前焦点在谁身上,关的时候还回去。
    ///
    /// 不还的话焦点停在已经不画了的浮层上,用户接着敲的字全部落空,还得先用鼠标
    /// 点一下终端才能继续 —— 与 `pane.rs::dismiss_search` 那句 `window.focus` 同一条红线
    /// (原版这个浮层压根不收焦点,所以没有这个问题)。
    marker_prev_focus: Option<FocusHandle>,
    /// 标记浮层**画在**哪个 pane 上:开着,或者收起动画还没播完(`marker_open` 已经是
    /// `None`)。播完 / pane 没了才清 —— 与命令库浮层的 `cmd_shown` 同一套。
    marker_shown: Option<(String, u32)>,
    /// 标记浮层的开合动画(见 [`crate::popover_reveal`]),原点取 [`Self::marker_anchors`]。
    marker_reveal: RevealState,
    /// 各叶子「⚑ N」按钮的窗口坐标,按该叶子 active pane 的 id 索引(探针报来),
    /// 理由同 [`Self::cmd_anchors`]。
    marker_anchors: HashMap<String, Bounds<Pixels>>,
    /// 命令库浮层(issue #81)本体。**懒建、常驻**:分组折叠态要跨次打开记住,
    /// 而且它自带一个 `InputState`,每次开都重建等于每次都丢焦点句柄。
    cmd_popover: Option<Entity<CommandPopover>>,
    /// 命令库浮层开在哪个 pane 上:`(project_id, pane_id, pty_id)`。`None` = 没开。
    /// 存活判据与 marker 浮层同一条([`marker_popover_alive`]):切 tab / 关 pane
    /// 就无条件关。
    cmd_open: Option<(String, String, u32)>,
    /// 命令库浮层**画在**哪个 pane 上:开着,或者收起动画还没播完(`cmd_open` 已经是 `None`)。
    /// 播完 / pane 没了才清。
    cmd_shown: Option<(String, String, u32)>,
    /// 命令库浮层的开合动画(见 [`crate::popover_reveal`])。原点不用它自带的锚点,用
    /// [`Self::cmd_anchors`] 里目标 pane 那一条。
    cmd_reveal: RevealState,
    /// 各叶子「命令库」按钮的窗口坐标,按该叶子 active pane 的 id 索引(探针报来)。
    /// 浮层开在哪个 pane 上,原点就取那个 pane 所在叶子的按钮 —— 分屏时每个叶子各有一颗,
    /// 不能共用一个锚点。条目只有几十字节,不按帧回收(与 `pane_enter` 同一档)。
    cmd_anchors: HashMap<String, Bounds<Pixels>>,
    /// 开命令库浮层前焦点在谁身上(理由同 [`Self::marker_prev_focus`])。
    cmd_prev_focus: Option<FocusHandle>,
    /// 对浮层事件的订阅(Close / Edit / NewGroup / RenameGroup)。
    _cmd_sub: Option<Subscription>,
    /// 正拖着文件悬停在哪个 pane 上(文件树的 `DragFilePath` 与系统的
    /// `ExternalPaths` 共用)。`on_drop` 不带位置,高亮只能从这里来。
    file_drop_pane: Option<String>,
    /// 每个叶子的进场动画(`.pane-enter`),按 `项目\u{1}叶子` 索引。
    ///
    /// 键里带项目 id 是为了**切项目不重播**:原版切项目是 `display:none`
    /// 留着不卸载,CSS 动画自然不会重来(`PaneGroup.tsx:391` 的注释原文)。
    /// 这张表也照此**不按帧回收** —— 跑完的条目只剩一个 `Instant`,与
    /// `split_states` 同属「关掉的项目会留下几十字节」那一档,不值得为它
    /// 每帧去遍历全部项目的布局树。
    pane_enter: HashMap<String, mt_ui::motion::Transition>,
    /// 每个 tab 一个焦点句柄(原版 tab 上的 `tabIndex` + `role="tab"`)。
    /// 拿到焦点后 Enter/Space 才能激活它。
    tab_focus: HashMap<String, FocusHandle>,
    /// 鼠标停在哪个 tab 上(缩略图计时要它)。
    hovered_tab: Option<String>,
    /// 上一下单击切过来的 tab:紧跟着的那一下凑成的双击不改名。
    tab_click: SwitchClickGuard,
    /// 每个 tab 的屏幕矩形(每个 tab 挂一片只量不画的 `canvas`)。
    ///
    /// 两个消费方:① 悬停缩略图的锚点;② tab 栏拖拽插入位要 tab 中线
    /// (原版是 `bar.querySelectorAll('[data-pane-tab]')` 逐个 `getBoundingClientRect`,
    /// 这张表就是它的等价物)。
    tab_rects: HashMap<String, Bounds<Pixels>>,
    /// 缩略图开在哪个 tab 上(key = pane id)+ 弹出那一刻的矩形 + 开合 / 换卡动画
    /// (见 [`pane_preview::HoverCard`]):首次弹出从 tab 下缘展开,卡还在屏幕上时扫到
    /// 相邻 tab 直接换卡,收起播完再卸。
    tab_card: pane_preview::HoverCard<String>,
    /// 250ms 计时 + 开着之后的 500ms 续活节拍。**丢掉句柄等于 `clearTimeout`**。
    _tab_preview_task: Option<Task<()>>,

    // ─── pane 拖拽(v0.14.0 / 原版 PR #49)───────────────────────
    //
    // 三份状态都只在拖拽期间有值,`render` 开头与 `cx.has_active_drag()` 对账后
    // 统一清 —— 与 `file_drop_pane` 同一套(拖拽被中断时 gpui 会自己清 active_drag
    // 并重画,残留状态自动失效,不必到处补清理)。
    /// 正被拖着的 pane id —— 源 tab 变淡靠它(原版 `el.style.opacity = '0.4'`)。
    pane_drag: Option<String>,
    /// 终端区落点预览:`(leaf_id, 档位)`。**`on_drop` 不带位置,这是唯一通道**。
    pane_drop: Option<(String, DropZone)>,
    /// tab 栏插入位:`(leaf_id, 插入下标, 指示线相对 tab 栏左缘的 x)`。
    tab_drop: Option<(String, usize, f32)>,

    // ─── 方向性换场(push 过渡,见文件头「换场」注释)──────────────
    /// 叶内切 tab 的换场记录(key = leaf id)。在场期间旧 pane 还要画
    /// (出场层),计时到点摘除。
    tab_swaps: HashMap<String, TabSwap>,
    /// 整块(面板切换 / 最大化开合)的换场记录。
    area_swap: Option<AreaSwap>,
    /// 上一次画到「每个叶子」时的激活 pane + 它在组里的下标 + 那一轮 render 的代次 ——
    /// 检测切 tab 用;下标给「关掉激活 tab 后新激活的从哪一侧推入」定方向。**不按帧回收**
    /// (与 `pane_enter` 同理:切面板再回来不该被误判成一次切换);隔了几轮才再画到的
    /// 叶子只对齐不起过渡(见 [`leaf_drawn_recently`])。
    last_leaf_active: HashMap<String, LeafActive>,
    /// render 的代次(每次 `render` 加一)。[`LeafActive::drawn_gen`] 拿它判「上一轮画没画到」。
    render_gen: u64,
    /// 上一帧的整块内容键:(项目, 面板, 最大化叶)。项目一换只记不播。
    last_area_key: Option<(String, String, Option<String>)>,
    /// 换场动画序号:同一处连续快速切换时,上一场动画的 id 必须让位。
    swap_seq: u64,
    /// 终端区在窗口里的原点(量尺 canvas 顺手记的)。`pane_rects` 是窗口坐标,
    /// Reveal 的起点矩形要换算成相对终端区的坐标。
    area_origin: gpui::Point<Pixels>,
    /// RevealBack 在飞期间,整树里这一组(叶子 id)整个挖空 —— 整组(tab 栏 +
    /// 终端实体)在飞行层上,同帧双挂会撞元素 id、焦点句柄与 `tab_rects`。
    /// 只在渲染进场内容时置位。
    suppress_leaf: Option<String>,
    /// 当前是否正在渲染 `AreaExitSrc::Panel` 出场层。置位期间跳过 `pane_enter`、
    /// `fx::leaf_slot`、`note_leaf_tab_swap` 与 `pane_rects`/`tab_rects` 写入,
    /// 防止退场中的旧面板污染活态布局表与点击区。
    rendering_exit: bool,
    /// 上一次点 ×(tab 上的与控制条的)的时刻,判连点用
    /// (见 [`pane_actions::swallows_close_click`])。
    last_close_click: Option<std::time::Instant>,
    /// 每个分组页签区的横向滚动状态(key = leaf id)。跨帧保留 —— 句柄一重建,
    /// 用户滚到的位置就丢了;回收与 `split_states` 同一时机(布局变了的那一帧)。
    tab_strips: HashMap<String, TabStripScroll>,
    /// tab chip 位移补间与关分屏后邻居扩张的共用状态(见 [`fx`])。包装元素在自己的
    /// prepaint 里读写它,所以是 `Rc<RefCell>`;每帧 render 开头 [`fx::LayoutFx::begin_frame`]。
    fx: fx::SharedFx,
    /// 键盘焦点的兜底落点:项目里一个终端都不剩时焦点落到这里,不落空。挂在终端区根上一个
    /// 零尺寸的元素上 —— 点不到(不抢鼠标点击的焦点),但在 Workspace 的派发路径里,
    /// 全局快捷键照样收得到。见 [`Self::restore_lost_focus`]。
    focus_sink: FocusHandle,
}

/// 键盘焦点落空之后交给谁(Workspace 的「焦点落空」兜底转过来)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FocusHandoff {
    /// 交给这个 pane 的终端:活动项目里该操作的那个(`AppStore::active_pane_id`)。
    Pane(String),
    /// 落到终端区的兜底焦点:项目里一个终端都不剩,或接手的 pane 还没有终端实体。
    Sink,
    /// 不动:不在终端页(文件页自己管焦点),或有弹窗 / 菜单压着(焦点归它们)。
    Leave,
}

/// 焦点落空的去处。`pane`:活动项目里该操作的 pane 以及它有没有终端实体。
pub fn lost_focus_handoff(
    terminal_page: bool,
    overlay_open: bool,
    pane: Option<(String, bool)>,
) -> FocusHandoff {
    if !terminal_page || overlay_open {
        return FocusHandoff::Leave;
    }
    match pane {
        Some((pane_id, true)) => FocusHandoff::Pane(pane_id),
        _ => FocusHandoff::Sink,
    }
}

impl TerminalArea {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        Self {
            store,
            layout_snapshot: None,
            split_states: HashMap::new(),
            area_size: FALLBACK_AREA,
            measured: false,
            pane_rects: HashMap::new(),
            marker_open: None,
            marker_focus: cx.focus_handle(),
            marker_prev_focus: None,
            marker_shown: None,
            marker_reveal: RevealState::new(),
            marker_anchors: HashMap::new(),
            cmd_popover: None,
            cmd_open: None,
            cmd_shown: None,
            cmd_reveal: RevealState::new(),
            cmd_anchors: HashMap::new(),
            cmd_prev_focus: None,
            _cmd_sub: None,
            file_drop_pane: None,
            pane_enter: HashMap::new(),
            tab_focus: HashMap::new(),
            hovered_tab: None,
            tab_click: SwitchClickGuard::default(),
            tab_rects: HashMap::new(),
            tab_card: pane_preview::HoverCard::default(),
            _tab_preview_task: None,
            pane_drag: None,
            pane_drop: None,
            tab_drop: None,
            tab_swaps: HashMap::new(),
            area_swap: None,
            last_leaf_active: HashMap::new(),
            render_gen: 0,
            last_area_key: None,
            swap_seq: 0,
            area_origin: gpui::Point::default(),
            suppress_leaf: None,
            rendering_exit: false,
            last_close_click: None,
            tab_strips: HashMap::new(),
            fx: Rc::default(),
            focus_sink: cx.focus_handle(),
        }
    }

    /// 键盘焦点落空(拿着焦点的终端 / 页签被关掉、弹窗把焦点还给了已经关掉的终端)之后接回来。
    /// Workspace 的 `on_focus_lost` 转过来,终端页与覆盖物两条判据由它带进来,去处见
    /// [`lost_focus_handoff`]:活动项目里该操作的那个终端,一个都不剩就落到 [`Self::focus_sink`]
    /// —— 不能落空:落空时按键从窗口根派发,Workspace 那一层的快捷键一个都收不到。
    pub fn restore_lost_focus(
        &mut self,
        terminal_page: bool,
        overlay_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (project_id, pane) = {
            let store = self.store.read(cx);
            let project_id = store.active_project_id.clone();
            let pane = project_id.as_deref().and_then(|project_id| {
                let pane_id = store.active_pane_id(project_id)?;
                let has_terminal = store
                    .project_state(project_id)
                    .and_then(|s| s.pane(&pane_id))
                    .and_then(|p| p.pty_id)
                    .is_some_and(|pty_id| store.terminal(pty_id).is_some());
                Some((pane_id, has_terminal))
            });
            (project_id, pane)
        };
        match lost_focus_handoff(terminal_page, overlay_open, pane) {
            FocusHandoff::Pane(pane_id) => {
                let Some(project_id) = project_id else { return };
                self.store.update(cx, |store, cx| {
                    store.focus_pane(&project_id, &pane_id, window, cx)
                });
            }
            FocusHandoff::Sink => window.focus(&self.focus_sink, cx),
            FocusHandoff::Leave => {}
        }
    }

    /// 兜底焦点挂在这个零尺寸元素上(见 [`Self::focus_sink`])。终端区的每条渲染出口都要带上,
    /// 否则落到兜底上的焦点这一帧不在树上,等于还是落空。
    fn focus_sink_anchor(&self) -> impl IntoElement + use<> {
        div().absolute().size_0().track_focus(&self.focus_sink)
    }

    /// 这次点 × 要不要当连点吞掉。吞掉的点击也要记时刻,连点才只算一次。
    /// `target_dead`:点的终端(整组则组内全部)已退出,见 [`pane_actions::swallows_close_click`]。
    fn swallows_close_click(&mut self, target_dead: bool, cx: &App) -> bool {
        let now = std::time::Instant::now();
        let last = self.last_close_click.replace(now);
        pane_actions::swallows_close_click(self.store.read(cx).config(), target_dead, last, now)
    }
    fn animations_enabled(&self, _cx: &App) -> bool {
        mt_ui::motion::animations_enabled()
    }

    /// 折叠条的落点:这一组接管铺满,原先铺满的那组缩回折叠区。
    ///
    /// 顺序是**先换铺满再激活**:`activate_pane` 末尾会把焦点交给终端,那时候
    /// 布局状态得已经是新的,否则焦点会落在这一帧还没画出来的那格上。
    /// [`AppStore::toggle_maximized_leaf`] 在这里恒等于「设成最大化」——
    /// 折叠条上的叶子按定义就不是当前铺满的那一个。
    fn take_over_maximized(
        &mut self,
        project_id: &str,
        pane_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_tab_preview(cx);
        self.hovered_tab = None;
        self.store.update(cx, |store, cx| {
            store.toggle_maximized_leaf(project_id, pane_id, cx);
            store.activate_pane(project_id, pane_id, window, cx);
        });
    }
}

/// 点击次数(键盘触发的「点击」按一次算)。
pub(crate) fn click_count(event: &ClickEvent) -> usize {
    match event {
        ClickEvent::Mouse(e) => e.up.click_count,
        ClickEvent::Keyboard(_) => 1,
        ClickEvent::Touch(e) => e.tap_count,
    }
}

/// 「单击切换、双击改名」的条目(终端 tab、终端面板竖条)上一次点击该做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwitchClick {
    Switch,
    Rename,
    /// 双击的后一下,而前一下是把它切过来的那一下
    Ignore,
}

/// 记着上一次单击切过来的是哪个条目,好认出「切过去后顺手又点了一下」。
///
/// gpui 的双击是两次单击按时间窗合成的(`click_count` 依次 1、2),第一下已经把条目
/// 切过来了:切完紧接着再点一下标题,不该被当成双击弹出改名。只有前一下落在**本来
/// 就激活**的条目上,这一对点击才算双击改名。
#[derive(Default)]
pub(crate) struct SwitchClickGuard {
    switched_to: Option<String>,
}

impl SwitchClickGuard {
    /// 点在 `id` 上(`was_active` = 点之前它就是激活的那个)。
    pub(crate) fn click(&mut self, id: &str, was_active: bool, count: usize) -> SwitchClick {
        if count >= 2 {
            return if self.switched_to.as_deref() == Some(id) {
                SwitchClick::Ignore
            } else {
                SwitchClick::Rename
            };
        }
        self.switched_to = (!was_active).then(|| id.to_string());
        SwitchClick::Switch
    }
}

/// 点击位置(弹菜单要它)。键盘触发的「点击」没有坐标,退回当前鼠标位置 ——
/// 菜单总得有个锚点,而这一条在真机上走不到(那个 `+` 没有键盘入口)。
pub(crate) fn click_position(event: &ClickEvent, window: &Window) -> gpui::Point<gpui::Pixels> {
    match event {
        ClickEvent::Mouse(e) => e.up.position,
        ClickEvent::Keyboard(_) => window.mouse_position(),
        ClickEvent::Touch(e) => e.position,
    }
}

/// 布局从 `prev` 变到 `now`,叶子是不是**只少不多**:关掉了一个分屏(或把独占一组的
/// tab 并进了别的组)。有增有减的(拖出去分屏)与没增没减的(切 tab、改比例)都不算 ——
/// 关分屏后邻居扩张(见 [`fx`])只认这一种。
fn leaves_only_shrank(prev: &SplitNode, now: &SplitNode) -> bool {
    let (prev, now) = (prev.leaves(), now.leaves());
    now.len() < prev.len()
        && now
            .iter()
            .all(|leaf| prev.iter().any(|old| old.id() == leaf.id()))
}

/// 把 `snapshot` 对齐到 store 里那棵活树,返回**这一帧是不是换了棵树**。
///
/// [`TerminalArea::layout_snapshot`] 存在的理由见那个字段的注释。这里是它唯一的
/// 维护点,拆成自由函数是为了能不带 `Window` 单测 —— 失效判据错了的后果是
/// 「画的是上一帧的分屏树」,那种 bug 在真机上极难认。
///
/// 判据是 `SplitNode` 的 `PartialEq`(逐字段走查,零分配,首个不同处即短路),
/// **不是**指针相等:store 那边是就地改树(`tree.rs` 模块注释里那条「不重建」
/// 的取舍),指针从头到尾都不会变。
fn sync_layout_snapshot(
    snapshot: &mut Option<std::rc::Rc<SplitNode>>,
    live: Option<&SplitNode>,
) -> bool {
    match live {
        Some(live) => {
            let stale = snapshot.as_deref() != Some(live);
            if stale {
                *snapshot = Some(std::rc::Rc::new(live.clone()));
            }
            stale
        }
        // 没有活动布局(切到空项目 / 关光了终端)—— 快照跟着清掉,
        // 「本来就是空的」不算换过
        None => snapshot.take().is_some(),
    }
}

impl Render for TerminalArea {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 活动布局的快照。**每帧一次整棵树深拷贝**是 render 的固定开销
        // (每个 pane 五到七次堆分配 × 30fps),改成「与上一帧逐字段比,真变了
        // 才拷一次」并把结果收进 `Rc`:比较是零分配的走查且首个不同处即短路,
        // 拿到手的 `layout` 是一次引用计数,树本身不动。
        //
        // ⚠️ 为什么不干脆按引用用(消掉这一份拥有权):`store` 是
        // `self.store.read(cx)` 借出来的,借的是 `*cx`;而下面 `render_node` /
        // `render_maximized` / `render_marker_popover` 全都要 `&mut self` **加**
        // `&mut Context<Self>`。只要 `layout` 还挂在 store 的借用上,那三处就一个
        // 都调不了 —— 这不是语句顺序能绕开的,是「渲染要边读树边改视图状态」
        // 这件事本身的形状。`Rc` 快照是同一目标的等价落点:热路径上不再有深拷贝。
        //
        // 上一帧的那份先留一个引用计数:关分屏后邻居扩张要比对「哪些叶子没了」
        self.fx.borrow_mut().begin_frame();
        self.render_gen += 1;
        // 整块换场播完即自动摘除出场层
        if self
            .area_swap
            .as_ref()
            .is_some_and(|swap| !swap.transition.running())
        {
            self.area_swap = None;
        }
        let prev_layout = self.layout_snapshot.clone();
        let layout_changed = {
            let store = self.store.read(cx);
            sync_layout_snapshot(&mut self.layout_snapshot, store.active_layout())
        };
        if layout_changed {
            let live_nodes = self.store.read(cx).live_node_ids();
            self.split_states.retain(|id, _| live_nodes.contains(id));
            self.tab_strips.retain(|id, _| live_nodes.contains(id));
            self.pane_enter.retain(|key, _| {
                key.split_once('\u{1}')
                    .is_some_and(|(_, leaf)| live_nodes.contains(leaf))
            });
        }
        // 拖拽结束后借这一帧清掉落点残留(**不 notify**,正在渲染)。
        // 高亮另外还与 `has_active_drag` 与门,见 `render_leaf`。
        if !cx.has_active_drag() {
            self.file_drop_pane = None;
            self.pane_drag = None;
            self.pane_drop = None;
            self.tab_drop = None;
        }

        // 切走项目 / 关光了终端 → 浮层无处可挂。下面两条早退路径压根走不到浮层
        // 组装那一步,不在这里收掉的话覆盖物栈里会留一条永远摘不掉的登记。
        if self.marker_shown.is_some() && self.store.read(cx).active_layout().is_none() {
            self.drop_marker_popover(window, cx);
        }
        if self.cmd_shown.is_some() && self.store.read(cx).active_layout().is_none() {
            self.drop_command_popover(window, cx);
        }

        let store = self.store.read(cx);
        let Some(project) = store.active_project() else {
            // 一个项目都没有 = 首启,换成引导页(audit #30 的 FirstRunGuide):
            // 原版判据就是 `config.projects.length === 0`(`App.tsx:534`),
            // 没有任何「首启标记」字段,添完项目自然消失。
            //
            // 「有项目但没选中」不走引导页 —— 原版那种情形是**整块空白**
            // (FirstRunGuide 不显示、每个 TerminalArea 都 display:none),
            // 下面这句是 GPUI 侧原有的兜底提示,保持不动。
            if store.config().projects.is_empty() {
                return crate::first_run::guide(self.store.clone()).child(self.focus_sink_anchor());
            }
            return div()
                .size_full()
                .bg(ui::bg_terminal())
                .flex()
                .items_center()
                .justify_center()
                .text_color(ui::text_muted())
                .text_size(ui::font_px(13.0))
                .child(t("app", "emptyState"))
                .child(self.focus_sink_anchor());
        };
        let project_id = project.id.clone();
        let project_name = project.name.clone();
        // 上面那段已经把快照对齐到 `store.active_layout()` 了,这里只是取一份
        // 引用计数 —— 与旧写法的 `.cloned()` 语义等价,少一整棵树的堆分配
        let layout = self.layout_snapshot.clone();

        let Some(layout) = layout else {
            let pid = project_id.clone();
            return div()
                .size_full()
                .bg(ui::bg_terminal())
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(14.0))
                .child(
                    div()
                        .text_color(ui::text_secondary())
                        .text_size(ui::font_px(13.0))
                        .child(tr!("terminalArea", "emptyTitle", project = project_name)),
                )
                .child(
                    div()
                        .id("empty-new-terminal")
                        .px(px(18.0))
                        .py(px(8.0))
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(ui::border_default())
                        .text_color(ui::text_muted())
                        .text_size(ui::font_px(13.0))
                        .cursor_pointer()
                        .hover(|el| el.border_color(ui::accent()).text_color(ui::accent()))
                        // 空态那颗按钮**也弹 shell 选择菜单**(`TerminalArea.tsx:32-46`
                        // 的 `handleNewTabClick`,与 tab 栏那颗「+」逐字同形、同一份
                        // `config.availableShells` 数据源、同一道 `<= 1` 闸)。
                        // 唯一差别是 anchor:空态没有「当前 pane」可挨着放,传 None
                        // (原版那边也是 `newTerminal(projectId)` 不带 targetPaneId)。
                        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                            let (shells, launchers) =
                                pane_actions::new_terminal_menu_data(this.store.read(cx), &pid);
                            if !pane_actions::should_show_new_terminal_menu(
                                shells.len(),
                                launchers.len(),
                            ) {
                                this.store.update(cx, |store, cx| {
                                    store.new_terminal(&pid, None, None, window, cx);
                                });
                                return;
                            }
                            let mut entries = pane_actions::new_terminal_menu_entries(
                                shells,
                                launchers,
                                {
                                    let store = this.store.clone();
                                    let pid = pid.clone();
                                    move |shell, window, cx| {
                                        let pid = pid.clone();
                                        store.update(cx, |store, cx| {
                                            store.new_terminal(&pid, Some(shell), None, window, cx);
                                        });
                                    }
                                },
                                {
                                    let store = this.store.clone();
                                    let pid = pid.clone();
                                    move |launcher, window, cx| {
                                        let pid = pid.clone();
                                        store.update(cx, |store, cx| {
                                            store.new_terminal_from_launcher(
                                                &pid, &launcher, None, window, cx,
                                            );
                                        });
                                    }
                                },
                            );
                            // 关光了终端才会停在这一页 —— 「误关了最后一个」正好在这里找回
                            entries.push(menu::separator());
                            entries.push(reopen_closed_menu_item(&this.store, &pid, cx));
                            menu::show(click_position(event, window), entries, window, cx);
                        }))
                        .child(format!("+ {}", t("terminalArea", "newTerminal"))),
                )
                // 键位提示是**独立一行**(原版 `TerminalArea.tsx:85-88`):
                // 「也可以按」+ 一颗键帽,不是塞进按钮文字里的括号。串从键位表取此刻生效的
                // 那份,改键位不会漏这里(与首启引导同一条路);用户把它解绑了,这一行就不画
                .when_some(
                    Some(crate::hotkeys::hotkey_label("newTerminal")).filter(|k| !k.is_empty()),
                    |el, keys| {
                        el.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .text_size(ui::font_px(11.0))
                                .text_color(ui::text_muted())
                                .child(t("terminalArea", "emptyHint"))
                                .child(ui::kbd(keys)),
                        )
                    },
                )
                // 关光了终端停在这一页:键盘焦点落在这里,新建终端 / 重开已关闭的终端这些
                // 快捷键照样响应
                .child(self.focus_sink_anchor());
        };

        // 双击最大化(v0.14.0):只渲染目标叶子,整树其余 pane 的**终端主体**
        // 不进元素树 —— 它们只留一条 26px 的折叠标题条码在底部
        // (见 [`Self::render_maximized`];v0.14.0 时是整组消失,连状态灯都没有)。
        //
        // ⚠️ **终端实体不受影响** —— `TerminalPane` 按 `pty_id` 挂在
        // `AppStore::terminals` 表里(旧版 `terminalCache` 的等价物),这里只是
        // 换个容器把同一个 `Entity` 画出来:PTY 不断、回滚缓冲不丢、光标不动。
        // 这就是原版 `getNodeKey` 那轮修复要保住的东西,GPUI 侧由「实体不在树里」
        // 这条结构天然成立(移动 / 重排 / 最大化都只动布局树的形状)。
        //
        // `maximized_pane_id()` 自带「布局是 split」这道闸;pane 被关掉后按 id 查不到
        // 叶子,自然回落整树(store 那边还会顺手把陈旧 id 清掉)。
        let maximized_leaf = self
            .store
            .read(cx)
            .maximized_pane_id(&project_id)
            .and_then(|id| layout.leaf_of_pane(id))
            .cloned();

        // ── 检测整块内容换了没(面板切换 / 最大化开合),起方向性 push ──
        // (见文件头「方向性换场」注释)
        let (active_panel_id, panel_order) = {
            let store = self.store.read(cx);
            let state = store.project_state(&project_id);
            (
                state
                    .and_then(|s| s.active_panel())
                    .map(|p| p.id.clone())
                    .unwrap_or_default(),
                state
                    .map(|s| s.panels.iter().map(|p| p.id.clone()).collect::<Vec<_>>())
                    .unwrap_or_default(),
            )
        };
        let area_key = (
            project_id.clone(),
            active_panel_id.clone(),
            maximized_leaf.as_ref().map(|l| l.id().to_string()),
        );
        // ── 关掉一个分屏:剩下的叶子从上一帧的格子扩张到新格子(见 [`fx`])────
        // 判据:同一项目 / 面板 / 最大化态(整块换场另有一套)、整树铺开(最大化时剩下的
        // 那一组本来就铺满)、叶子只少不多(拖出去分屏这类有增有减的不算)
        let closed_split = layout_changed
            && self.measured
            && maximized_leaf.is_none()
            // 整块换场还在飞时,上一帧量到的格子是推移途中的位置,不作数
            && self.area_swap.is_none()
            && self.last_area_key.as_ref() == Some(&area_key)
            && prev_layout
                .as_deref()
                .is_some_and(|prev| leaves_only_shrank(prev, &layout));
        self.fx.borrow_mut().arm_split_grow(closed_split);
        if self.last_area_key.as_ref() != Some(&area_key) {
            let prev = self.last_area_key.replace(area_key.clone());
            // 首帧加载、切项目或切面板时,目标布局里本来就存在的全部叶子都记为 settled:
            // 它们是既有的窗格而不是刚分屏出来的新窗格,避免首次切进已有项目时整排窗格横移 14px。
            let switched_context = prev
                .as_ref()
                .is_none_or(|(p_proj, p_panel, _)| p_proj != &project_id || p_panel != &active_panel_id);
            if switched_context {
                for leaf in layout.leaves() {
                    let key = format!("{project_id}\u{1}{}", leaf.id());
                    self.pane_enter
                        .entry(key)
                        .or_insert_with(|| mt_ui::motion::Transition::settled(mt_ui::motion::PANE_ENTER));
                }
            }
            // 项目一换只记不播:旧面板属于别的项目,出场层无从画起,
            // 换项目本来也不该有「翻页」感;「启用动画」关着时同样只记不播,并立刻清掉在飞的旧换场
            if let Some((prev_project, prev_panel, prev_max)) = prev
                && prev_project == project_id
                && self.animations_enabled(cx)
            {
                let panel_changed = prev_panel != active_panel_id;
                let swap: Option<(f32, AreaMotion, AreaExitSrc)> = if panel_changed {
                    let old_i = panel_order.iter().position(|id| *id == prev_panel);
                    let new_i = panel_order.iter().position(|id| *id == active_panel_id);
                    let dir = match (old_i, new_i) {
                        (Some(o), Some(n)) if n < o => -1.0,
                        _ => 1.0,
                    };
                    let exit = if panel_order.contains(&prev_panel) {
                        AreaExitSrc::Panel(prev_panel)
                    } else {
                        AreaExitSrc::None
                    };
                    Some((dir, AreaMotion::Push, exit))
                } else {
                    let max_leaf_exit = |old_leaf: &str| -> AreaExitSrc {
                        match layout.node(old_leaf) {
                            Some(SplitNode::Leaf { panes, .. }) if !panes.is_empty() => {
                                AreaExitSrc::MaxLeaf {
                                    leaf_id: old_leaf.to_string(),
                                    bar_count: restore_bar_count(prev_layout.as_deref(), &layout),
                                }
                            }
                            _ => AreaExitSrc::None,
                        }
                    };
                    match (&prev_max, &area_key.2) {
                        (Some(_), Some(new_leaf)) => {
                            if let Some(SplitNode::Leaf {
                                panes,
                                active_pane_id,
                                ..
                            }) = layout.node(new_leaf)
                            {
                                let active = panes
                                    .iter()
                                    .position(|p| &p.id == active_pane_id)
                                    .or_else(|| (!panes.is_empty()).then_some(0));
                                if let Some(i) = active {
                                    self.last_leaf_active.insert(
                                        new_leaf.clone(),
                                        LeafActive {
                                            pane_id: panes[i].id.clone(),
                                            index: i,
                                            drawn_gen: self.render_gen,
                                        },
                                    );
                                }
                            }
                            None
                        }
                        // 进最大化:起点取上一帧量到的**完整叶子矩形**(含 30px tab 栏),
                        // 与还原目标矩形 `leaf_rect_in` 严丝合缝对称,第 0 帧不裁 tab 栏
                        (None, Some(new_leaf)) => {
                            let from = self
                                .fx
                                .borrow()
                                .leaf_rect(new_leaf)
                                .map(|r| RevealFrom {
                                    left: r.left - f32::from(self.area_origin.x),
                                    top: r.top - f32::from(self.area_origin.y),
                                    width: r.width,
                                    height: r.height,
                                })
                                .or_else(|| {
                                    prev_layout.as_deref().and_then(|prev| {
                                        leaf_rect_in(
                                            prev,
                                            new_leaf,
                                            0.0,
                                            0.0,
                                            f32::from(self.area_size.width),
                                            f32::from(self.area_size.height),
                                        )
                                    })
                                });
                            match from {
                                Some(from) => {
                                    Some((1.0, AreaMotion::Reveal { from }, AreaExitSrc::None))
                                }
                                None => Some((1.0, AreaMotion::Drift, AreaExitSrc::None)),
                            }
                        }
                        // 还原:最大化的反向 —— 裁剪窗从整幅收回到该格还原后的完整目标矩形
                        (Some(old_leaf), None) => {
                            let to = leaf_rect_in(
                                &layout,
                                old_leaf,
                                0.0,
                                0.0,
                                f32::from(self.area_size.width),
                                f32::from(self.area_size.height),
                            );
                            let exit = max_leaf_exit(old_leaf);
                            match (to, &exit) {
                                (Some(to), AreaExitSrc::MaxLeaf { .. }) => {
                                    Some((-1.0, AreaMotion::RevealBack { to }, exit))
                                }
                                _ => Some((-1.0, AreaMotion::Drift, AreaExitSrc::None)),
                            }
                        }
                        (None, None) => None,
                    }
                };
                self.area_swap = swap.map(|(dir, motion, exit)| AreaSwap {
                    dir,
                    motion,
                    exit,
                    transition: mt_ui::motion::Transition::new(mt_ui::motion::AREA_SWAP),
                });
            } else {
                // 切项目或中途关闭动画总开关:立即清掉在飞的旧换场,不让旧飞行层残留在新项目里
                self.area_swap = None;
            }
        }

        // 关掉的 pane 的矩形残影一并清掉,免得方向导航挑到不存在的格子。
        // **最大化时只留被铺满那一组的矩形**:方向导航挑的是「屏幕上相邻的格子」,
        // 藏起来的那些格子不该被挑中(原版 `findAdjacentPtyId` 查的是 DOM,
        // 卸载掉的 PaneGroup 天然查不到,这里把那条性质补回来)。
        // 折叠标题条**只写 `tab_rects` 不写 `pane_rects`**,这条性质原样成立 ——
        // 条上没有终端主体,方向导航跳过去也没有格子可落。
        let alive: std::collections::HashSet<String> = match &maximized_leaf {
            Some(leaf) => leaf.panes().into_iter().map(|p| p.id.clone()).collect(),
            None => layout.panes().into_iter().map(|p| p.id.clone()).collect(),
        };
        self.pane_rects.retain(|id, _| alive.contains(id));
        // tab 焦点句柄与 tab 矩形按**整棵树**回收(切项目/关 tab 之后那些行不在了)。
        // ⚠️ 句柄必须跨帧稳定,不能每帧重建 —— 那样 Tab 过去的焦点每帧都会丢;
        // 折叠掉的组照样在画(标题条上那些 tab 要焦点、要缩略图锚点),
        // 更不跟着 `alive` 收窄。
        let in_layout: std::collections::HashSet<String> =
            layout.panes().into_iter().map(|p| p.id.clone()).collect();
        self.tab_focus.retain(|id, _| in_layout.contains(id));
        self.tab_rects.retain(|id, _| in_layout.contains(id));

        // push 过渡的出场层。先把要画的东西摘成 owned 再渲染(render_node /
        // 实体查表都要各自借 self);**先画出场层再画进场层** —— 两边都会往
        // pane_rects 等视图表里写,后画的(进场层 = 真正在台上的)覆盖为准。
        let exit_src = self
            .area_swap
            .as_ref()
            .map(|s| s.exit.clone())
            .unwrap_or(AreaExitSrc::None);
        let exit_content: Option<AnyElement> = match (exit_src, self.measured) {
            // 切面板:旧面板的整树。置 `rendering_exit` 隔离,不往 pane_rects/tab_rects/fx 写入
            (AreaExitSrc::Panel(panel_id), true) => {
                let old_layout = self
                    .store
                    .read(cx)
                    .project_state(&project_id)
                    .and_then(|st| st.panels.iter().find(|p| p.id == panel_id))
                    .map(|p| p.layout.clone());
                old_layout.map(|l| {
                    self.rendering_exit = true;
                    let el = self.render_node(
                        &l,
                        &project_id,
                        self.area_size,
                        SplitDirection::Horizontal,
                        window,
                        cx,
                    );
                    self.rendering_exit = false;
                    el
                })
            }
            // 还原飞行层:旧铺满的整组(tab 栏 + 终端主体,与 render_maximized 的 body
            // 同一个 render_leaf)+ 按旧折叠区同高占位 —— 终端主体与铺满态逐像素同高,
            // 还原第一帧不多一次 PTY resize(见 [`AreaExitSrc::MaxLeaf`])
            (AreaExitSrc::MaxLeaf { leaf_id, bar_count }, true) => {
                layout.node(&leaf_id).cloned().map(|leaf| {
                    let body = self.render_leaf(&leaf, &project_id, window, cx);
                    let zone_h = collapsed_zone_height(f32::from(self.area_size.height), bar_count);
                    div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex_1()
                                .min_h(px(0.0))
                                .bg(ui::bg_terminal())
                                .child(body),
                        )
                        .child(div().flex_none().h(px(zone_h)))
                        .into_any_element()
                })
            }
            _ => None,
        };
        let area_anim = self
            .area_swap
            .as_ref()
            .map(|s| (s.dir, s.motion, s.transition.drive(window)));

        // 首帧只量不画:百分比要按真实可用尺寸换算,而 ResizablePanel 只认第一帧的
        // 初值(见模块注释)。量到之后主动 notify 一次,下一帧把分屏树铺上去。
        //
        // RevealBack 在飞时,进场内容里把那一整组挖空(整组在飞行层上),
        // 只在这一次 render 调用期间置位 —— 出了这里谁也别受影响
        self.suppress_leaf = self
            .area_swap
            .as_ref()
            .and_then(|s| match (&s.motion, &s.exit) {
                (AreaMotion::RevealBack { .. }, AreaExitSrc::MaxLeaf { leaf_id, .. }) => {
                    Some(leaf_id.clone())
                }
                _ => None,
            });
        let content = self.measured.then(|| match &maximized_leaf {
            Some(leaf) => self.render_maximized(&layout, leaf, &project_id, window, cx),
            None => self.render_node(
                &layout,
                &project_id,
                self.area_size,
                SplitDirection::Horizontal,
                window,
                cx,
            ),
        });
        self.suppress_leaf = None;
        // 浮层在分屏树**之后**组装:它要读 render_node 刚更新过的 pane 矩形,
        // 而且要画在所有常规内容之上(deferred priority 1)
        let marker_popover = self.render_marker_popover(&layout, window, cx);
        let command_popover = self.render_command_popover(&layout, window, cx);
        let tab_preview = self.render_tab_preview(&layout, window, cx);
        let this = cx.entity();
        div()
            .size_full()
            .bg(ui::bg_terminal())
            .flex()
            .relative()
            .child(self.focus_sink_anchor())
            // Esc 中途取消拖拽**不在这里拦**:此前挂在这个根容器上的 `capture_key_down`
            // 只对 pane 拖拽有效(焦点在终端上时它才在派发路径上),从文件树起拖时
            // 焦点在文件树行上、这一层收不到。现已合并到 Workspace 根(`main.rs`),
            // 对所有内部拖拽一视同仁;本视图的 `pane_drag` / `pane_drop` / `tab_drop`
            // 残留由 `render` 开头那句 `has_active_drag` 对账清掉。
            .child(
                canvas(
                    move |bounds: Bounds<Pixels>, _window, cx| {
                        this.update(cx, |area: &mut TerminalArea, cx| {
                            if bounds.size.width > px(0.0) && bounds.size.height > px(0.0) {
                                let first = !area.measured;
                                area.area_size = bounds.size;
                                area.area_origin = bounds.origin;
                                area.measured = true;
                                // 只在第一次量到时唤起重画 —— 之后每帧都 notify
                                // 就是个死循环
                                if first {
                                    cx.notify();
                                }
                            }
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .children(content.map(|c| {
                // 整块换场:有在场记录才包动画层,否则零包装。进度由单一 `Transition` 驱动
                match area_anim {
                    // 整幅竖直 push:出场层(如有)与进场层首尾相接,出场层顶盖遮罩拦鼠标
                    Some((dir, AreaMotion::Push, progress)) => div()
                        .size_full()
                        .relative()
                        .overflow_hidden()
                        .children(exit_content.map(|old| {
                            div()
                                .absolute()
                                .top(gpui::relative(push_exit_x(dir, progress)))
                                .left_0()
                                .w_full()
                                .h_full()
                                .child(old)
                                .child(div().absolute().inset_0().occlude())
                        }))
                        .child(
                            div()
                                .absolute()
                                .top(gpui::relative(push_enter_x(dir, progress)))
                                .left_0()
                                .w_full()
                                .h_full()
                                .child(c),
                        )
                        .into_any_element(),
                    // 14px 竖直落位(算不出矩形时的兜底)
                    Some((dir, AreaMotion::Drift, progress)) => div()
                        .size_full()
                        .relative()
                        .overflow_hidden()
                        .child(
                            div()
                                .absolute()
                                .top(px(dir * swap_slide_offset(progress)))
                                .left_0()
                                .w_full()
                                .h_full()
                                .child(c),
                        )
                        .into_any_element(),
                    // 还原:单时钟 `ClipReveal` 裁剪窗从全屏收回到该格还原后的完整矩形,
                    // 飞行层内容左上角跟随裁剪窗左上角移动且不接鼠标(零 1px 抖动、零黑边)
                    Some((_dir, AreaMotion::RevealBack { to }, progress)) => {
                        let full = RevealFrom {
                            left: 0.0,
                            top: 0.0,
                            width: f32::from(self.area_size.width),
                            height: f32::from(self.area_size.height),
                        };
                        let clip = reveal_clip_rect(full, to, progress);
                        let flying = exit_content.map(|old| {
                            div().absolute().inset_0().child(clip_reveal(
                                div()
                                    .size_full()
                                    .relative()
                                    .child(old)
                                    .child(div().absolute().inset_0().occlude()),
                                clip,
                            ))
                        });
                        div()
                            .size_full()
                            .relative()
                            .child(div().absolute().inset_0().child(c))
                            .children(flying)
                            .into_any_element()
                    }
                    // 最大化:单时钟 `ClipReveal` 裁剪窗从原完整叶子矩形(含 tab 栏)展开到全屏,
                    // 内容左上角钉在裁剪窗左上角(右/下分屏展开直接显示该终端,零纯黑空白与抖动)
                    Some((_dir, AreaMotion::Reveal { from }, progress)) => {
                        let full = RevealFrom {
                            left: 0.0,
                            top: 0.0,
                            width: f32::from(self.area_size.width),
                            height: f32::from(self.area_size.height),
                        };
                        let clip = reveal_clip_rect(from, full, progress);
                        div()
                            .size_full()
                            .relative()
                            .child(clip_reveal(div().size_full().child(c), clip))
                            .into_any_element()
                    }
                    None => div().size_full().child(c).into_any_element(),
                }
            }))
            .children(marker_popover)
            .children(command_popover)
            // 非激活 tab 的悬停缩略图。卡不带 `.id()` → 无 hitbox → 不吃鼠标
            .children(tab_preview)
    }
}

#[cfg(test)]
mod tests;
