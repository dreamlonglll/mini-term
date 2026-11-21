use std::sync::Arc;

use futures::StreamExt;
use gpui::StyledImage as _;
use gpui::{
    AnyView, App, AppContext, Bounds, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, SharedString, Size, StatefulInteractiveElement, StyleRefinement,
    Styled, Subscription, Task, Window, WindowBounds, div, point, prelude::FluentBuilder, px, size,
};
use gpui_component::ActiveTheme as _;
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use gpui_component::WindowExt as _;
use mt_ui::tooltip::TooltipExt as _;

use crate::file_tree::FileTree;
use crate::focus_nav::Direction;
use crate::git::{history as git_history, panel as git_panel};
use crate::i18n::{t, tr};
use crate::project_list::ProjectList;
use crate::session_panel::SessionPanel;
use crate::store::{AppStore, DoneScope, PendingAlert, StoreEvent};
use crate::terminal_area::TerminalArea;
use crate::title_bar::TitleBar;
use crate::tray::{Tray, TrayEvent};
use crate::tree::SplitDirection;
use crate::usage_panel::UsagePanel;
use crate::workbench_area::WorkbenchArea;
use crate::{
    ClosePane, CloseTerminalTab, FocusDown, FocusLeft, FocusRight, FocusUp, GlobalSearch,
    JumpAttention, MarkerNext, MarkerPrev, MoveTabLeft, MoveTabRight, NewTerminal, NextPane,
    OpenTerminalSettings, PrevPane, QuickOpenFile, RenamePane, ReopenClosedTerminal,
    ResizeSplitDown, ResizeSplitLeft, ResizeSplitRight, ResizeSplitUp, SelectPane, SplitDown,
    SplitRight, SwitchProject, TerminalSearch, ToggleCommandLibrary, ToggleMaximizePane,
    ToggleMiddleColumn, ToggleSessions, ToggleUsage, ToggleWorkbenchPage, activity_bar, ai,
    app_data_dir, dialog_host, file_tree, frost, hook_hint, menu, mobile_panel, mobile_relay,
    modal, motion, motion_kit, native_view, notify, overlay, overlay_motion, pane_actions,
    project_list, project_switcher, prompt, quick_open, redraw, remote_notify, search_modal,
    settings, ssh_panel, store, terminals_panel, toast, tray, ui, update_check, update_dialog,
    workbench_area,
};

mod actions;
mod render;

/// 这次全局动作要不要让路。对应原版 `useGlobalHotkeys` 里连着的那两道闸:
///
/// ```text
/// if (isTypingTarget(e.target)) return;                    // ① 焦点在输入框里
/// if (overlayOpen && id !== 'openSettings' && id !== 'globalSearch') return;  // ②
/// ```
///
/// ① 用 `gpui_component` 的 `has_focused_input`(它按 `Input` 元素的
/// 聚焦/失焦维护 `Root::focused_input`,语义等价于原版那个「是不是 input /
/// textarea / contenteditable」的判定;终端**不是** `Input`,所以在终端里敲字
/// 照样能用快捷键 —— 与原版排除 `xterm-helper-textarea` 同效)。
/// 注意它**优先于**白名单:原版在输入框里连 openSettings / globalSearch 也让路。
///
/// ② 判据统一在 [`overlay`]。白名单那两条(`OpenTerminalSettings` /
/// `GlobalSearch`)的处理器里只保留 ①,不加 ②。
fn yields_to_typing(window: &mut Window, cx: &mut App) -> bool {
    window.has_focused_input(cx)
}

fn yields_to_overlay(window: &mut Window, cx: &mut App) -> bool {
    yields_to_typing(window, cx) || !overlay::allows(overlay::Yield::ToOverlay)
}

/// 一条终端 / 导航快捷键此刻落在哪一页(见 [`Workspace::hotkey_page`])。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HotkeyPage {
    /// 让路:覆盖物压着,或焦点在别处的输入框里
    Yield,
    /// 停在终端页:走原来那条路
    Terminal,
    /// 停在文件 / diff 页签上:交给工作区页签(`workbench_area` 模块注释)
    Document,
}
/// 三栏默认宽度(像素),与 `src/App.tsx` 的 Allotment 默认值一致。
const DEFAULT_COLUMNS: [f64; 2] = [520.0, 1000.0];
const DEFAULT_MIDDLE: [f64; 2] = [320.0, 380.0];

/// 右抽屉里当前是哪一块。对应 `store.ts:685` 的
/// `rightDrawer: 'sessions' | 'git' | null` —— **运行时态,互斥单抽屉,
/// 不持久化开合**(每次启动收起)。落盘的只有宽度。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DrawerPanel {
    Sessions,
    Git,
}

impl DrawerPanel {
    fn key(self) -> &'static str {
        match self {
            DrawerPanel::Sessions => "sessions",
            DrawerPanel::Git => "git",
        }
    }
}

/// 中栏开合时的流内同步推拉过渡。
///
/// 展开/折叠途中,中栏放进 `body` 的流内定高容器(宽随进度在 `0..=target_w` 平滑变化),
/// 内层保持定宽 `target_w` 贴右缘(`right_0`)随容器推入/收回 —— 右侧工作区同步丝滑让位/
/// 填满,全程零白底空洞、零遮盖终端文字、零外置主题包半透明闪色,且落定切回 `h_resizable`
/// 零像素宽差。期间经 `mt_ui::terminal::set_grid_resize_frozen(true)` 暂停终端逐帧重排,
/// 动画结束解冻后一次性完成唯一一次 `emulator.resize` 与去抖 PTY resize。
/// ⚠️ 项目列表、文件树套着 [`cached_panel`]:动画层只改容器裁切宽度,**不许**改 opacity。
///
/// 半路掉头(收到一半又展开、展开到一半又收)从眼下的位置接着走:进度存成
/// `ValueTween`,新一场从 [`motion_kit::reversed_progress`] 起步(推导见
/// [`middle_visible_width`] 的单测),不从头播。
struct MiddleMotion {
    entering: bool,
    progress: mt_ui::motion::ValueTween,
}

/// 中栏最小/最大宽度,与 `h_resizable` 的 `size_range` 保持唯一真相源。
const MIDDLE_MIN_WIDTH: f32 = 180.0;
const MIDDLE_MAX_WIDTH: f32 = 700.0;
/// 右侧工作区最少留出的安全宽度(与 `gpui-base` `PANEL_MIN_SIZE` 一致)。
const MIDDLE_RIGHT_RESERVE: f32 = 100.0;

/// 中栏最终目标宽度(像素):流内推拉动画与落定后的 `h_resizable` 第 0 栏共用此值,
/// 保证动画末帧与静止态严格 0px 宽差。
fn middle_target_width(saved: f64, viewport_w: f32) -> f32 {
    let max_fit = (viewport_w - activity_bar::WIDTH - MIDDLE_RIGHT_RESERVE)
        .clamp(MIDDLE_MIN_WIDTH, MIDDLE_MAX_WIDTH);
    (saved as f32).clamp(MIDDLE_MIN_WIDTH, max_fit)
}

/// 中栏流内推拉容器这一帧的可视宽度(像素):展开从 `0` 长到 `target_w`,折叠从 `target_w`
/// 收回 `0`。**纯函数**,端点精确且半路掉头严格连续。
fn middle_visible_width(target_w: f32, entering: bool, progress: f32) -> f32 {
    let w = target_w.max(0.0);
    let p = progress.clamp(0.0, 1.0);
    if entering {
        if p >= 1.0 { w } else { w * p }
    } else if p >= 1.0 {
        0.0
    } else {
        w * (1.0 - p)
    }
}

/// 抽屉段控件的排列次序(会话在左、Git 在右):换面板时往右边那项切,新面板从右进。
fn drawer_panel_order(old: DrawerPanel, new: DrawerPanel) -> f32 {
    let index = |panel: DrawerPanel| match panel {
        DrawerPanel::Sessions => 0,
        DrawerPanel::Git => 1,
    };
    (index(new) - index(old)) as f32
}

/// 抽屉段控件滑块停在哪(容器宽的分数)。
fn drawer_tab_left(panel: DrawerPanel) -> f32 {
    match panel {
        DrawerPanel::Sessions => 0.0,
        DrawerPanel::Git => 0.5,
    }
}

/// 右抽屉(连同左侧展开着的提交详情栏)这一帧的 `right`(像素):
/// 全开(`progress = 1.0`)严丝合缝贴在右缘 0;全收(`0.0`)把「抽屉 + 当前露出的详情栏」
/// 整块送出视口右缘,不会在关闭时把详情栏卡在右缘外面。
fn drawer_slide_right(drawer_width: f32, detail_visible_width: f32, progress: f32) -> f32 {
    let p = progress.clamp(0.0, 1.0);
    if p >= 1.0 {
        0.0
    } else {
        -(drawer_width + detail_visible_width.max(0.0)) * (1.0 - p)
    }
}

/// 提交详情栏这一帧里层的 `left`(像素):收起(`0.0`)整栏藏在抽屉左缘后面,全开(`1.0`)落到 0。
fn detail_slide_left(width: f32, progress: f32) -> f32 {
    let p = progress.clamp(0.0, 1.0);
    if p >= 1.0 { 0.0 } else { width * (1.0 - p) }
}

/// 右抽屉(或它左侧的提交详情栏)左缘拖拽的一次会话。
#[derive(Clone, Copy)]
struct DrawerDrag {
    /// 按下时的鼠标 x(窗口坐标)。
    start_x: gpui::Pixels,
    /// 按下时的宽度。
    start_width: f64,
    /// 当前宽度(已钳到可拖范围:抽屉 240..720,详情栏见 `store::GIT_DETAIL_*_WIDTH`)。
    width: f64,
}

/// 提交详情栏左边至少给终端留多宽(Activity Bar 44 + 一截能看清的终端)。窗口窄时
/// 详情栏先让,但不窄于 [`GIT_DETAIL_FLOOR_W`]。
const GIT_DETAIL_TERMINAL_ROOM: f32 = 200.0;
/// 窗口再窄,详情栏也不窄于这个宽(宁可压住终端左段,也别挤成一条缝)。
const GIT_DETAIL_FLOOR_W: f32 = 260.0;
/// 抽屉挤占页签区时,页签区至少留这么宽;窗口太窄、抽屉连同详情栏放不下时,多出来的
/// 那截照旧盖在页签区上(与详情栏「宁可压住,也别挤成一条缝」同一取舍)。
const SQUEEZE_MIN_DOC_W: f32 = 320.0;

/// 提交详情栏的宽:想要的宽(拖过 / 落盘的)先给窗口左侧留出 [`GIT_DETAIL_TERMINAL_ROOM`],
/// 但不窄于 [`GIT_DETAIL_FLOOR_W`]。
fn detail_layer_width(wanted: f32, viewport_w: f32, drawer_w: f32) -> f32 {
    wanted
        .min(viewport_w - drawer_w - GIT_DETAIL_TERMINAL_ROOM)
        .max(GIT_DETAIL_FLOOR_W)
}

/// 右抽屉(连同 Git 抽屉左侧的提交详情栏)从页签区右侧挤占多宽。
///
/// - 停在**终端页**、或抽屉没开着(含退场动画驻留的那 400ms)→ 0:抽屉照旧悬浮,
///   终端宽度一像素不动(每次宽度变化都是一次 PTY resize,见 `terminal_area` 模块注释);
/// - 停在**文件 / diff 页签**上 → 抽屉占多宽就让出多宽(`occupied` = 抽屉宽 + 详情栏宽),
///   页签区跟着变窄,可以边看 diff 边在更改列表里点;但页签区至少留 [`SQUEEZE_MIN_DOC_W`]
///   (`panel_w` 是页签区所在那一栏的宽),放不下的那截照旧盖在上面。
fn drawer_squeeze(terminal_page: bool, drawer_open: bool, occupied: f32, panel_w: f32) -> f32 {
    if terminal_page || !drawer_open {
        return 0.0;
    }
    occupied.min(panel_w - SQUEEZE_MIN_DOC_W).max(0.0)
}

/// 上次退出时的窗口几何 → 本次开窗参数。
///
/// 存过的框必须**与某块屏幕有交集**才用:外接显示器拔掉后原样还原,窗口会开在
/// 一个看不见的坐标上,用户只能靠任务栏右键「移动」把它捞回来。用交集而不是
/// 「原点落在屏内」判 —— 窗口探出屏幕一半是用户自己拖的,不该被"纠正"。
///
/// GPUI 版此前每次都是写死的居中 1280×800(Tauri 版靠 `tauri-plugin-window-state`
/// 存 `.window-state.json`,迁移时丢了这块;那个文件的格式与本库不兼容,不迁)。
pub(crate) fn restore_window_bounds(
    saved: Option<mt_layout::WindowGeometry>,
    cx: &App,
) -> WindowBounds {
    let default_bounds =
        || WindowBounds::Windowed(Bounds::centered(None, size(px(1280.0), px(800.0)), cx));
    let Some(geo) = saved else {
        return default_bounds();
    };
    let bounds = Bounds {
        origin: point(px(geo.x as f32), px(geo.y as f32)),
        size: size(px(geo.width as f32), px(geo.height as f32)),
    };
    if !cx.displays().iter().any(|d| d.bounds().intersects(&bounds)) {
        return default_bounds();
    }
    match geo.mode {
        mt_layout::WindowMode::Windowed => WindowBounds::Windowed(bounds),
        mt_layout::WindowMode::Maximized => WindowBounds::Maximized(bounds),
        mt_layout::WindowMode::Fullscreen => WindowBounds::Fullscreen(bounds),
    }
}

/// 当前窗口几何(供落盘)。取 `window_bounds()` 而不是 `bounds()` —— 前者在
/// 最大化/全屏时给的是**还原尺寸**,正是下次开窗该用的那个值(gpui 对这个方法的
/// 原注释就是 "how a window should be opened after it has been closed")。
fn current_window_geometry(window: &Window) -> mt_layout::WindowGeometry {
    let (mode, bounds) = match window.window_bounds() {
        WindowBounds::Windowed(b) => (mt_layout::WindowMode::Windowed, b),
        WindowBounds::Maximized(b) => (mt_layout::WindowMode::Maximized, b),
        WindowBounds::Fullscreen(b) => (mt_layout::WindowMode::Fullscreen, b),
    };
    mt_layout::WindowGeometry {
        mode,
        x: bounds.origin.x.to_f64(),
        y: bounds.origin.y.to_f64(),
        width: bounds.size.width.to_f64(),
        height: bounds.size.height.to_f64(),
    }
}

pub(crate) struct Workspace {
    store: Entity<AppStore>,
    /// 右键菜单浮层。状态住在全局(任何视图都能 `menu::show`),这里只是把它
    /// **画出来**的那个位置 —— 与弹窗层(`dialog_host::render_layer`)同一种分工。
    menu_layer: Entity<menu::ContextMenu>,
    /// 自绘标题栏(无边框窗口的拖拽区 + 三键 + 项目胶囊 + 全局状态灯)。
    title_bar: Entity<TitleBar>,
    /// 自建 toast 层。与 [`Self::menu_layer`] 同一种分工:状态住在全局
    /// (AI 泵 / pane / store 三处都要往里推),这里只是把它**画出来**的位置。
    toast_layer: Entity<toast::ToastLayer>,
    project_list: Entity<ProjectList>,
    file_tree: Entity<FileTree>,
    terminal_area: Entity<TerminalArea>,
    /// 常驻终端页与运行时文件页签的宿主。文件页不进入终端布局持久化。
    workbench_area: Entity<WorkbenchArea>,
    /// 终端区右缘的「项目级终端面板」切换竖条。显隐住在 store
    /// (`terminals_panel_visible`,落 layout.db),收起时整个不进元素树。
    terminals_panel: Entity<terminals_panel::TerminalsPanel>,
    session_panel: Entity<SessionPanel>,
    /// Git 面板(抽屉的第二块)。与会话面板一样常驻实体,靠
    /// [`GitPanel::set_visible`](git_panel::GitPanel::set_visible) 闸住扫盘与
    /// pty 输出旁路。
    git_panel: Entity<git_panel::GitPanel>,
    /// 用量面板惰性创建:它一开就跑账本同步,没打开过就不该有这笔开销。
    usage_panel: Option<Entity<UsagePanel>>,
    columns_state: Entity<ResizableState>,
    middle_state: Entity<ResizableState>,
    /// 上一帧的视口尺寸,给上面两个分栏状态判「该不该重新播种」。
    /// 见 [`Workspace::reseed_resizables_on_viewport_change`]。
    last_viewport: Size<Pixels>,
    /// 右侧悬浮抽屉现在开着哪一块(运行时态,不持久化 —— 与旧版一致)。
    right_drawer: Option<DrawerPanel>,
    /// 右抽屉的开合在场度(进场 `OVERLAY_IN` / 退场 `OVERLAY_OUT`,支持半路掉头与关动画即卸)。
    drawer_presence: overlay_motion::Presence,
    /// 最近打开过的那块抽屉面板:退场动画播完前继续渲染它,播完清掉。
    drawer_last_panel: Option<DrawerPanel>,
    /// 抽屉**换面板**的整幅 push(会话在左、Git 在右,见 [`drawer_panel_order`])。开抽屉
    /// 不播(那有整层滑入),所以由 [`Self::set_drawer`] 显式驱动,不在 render 里比对。
    drawer_swap: motion_kit::KeyedSwap<DrawerPanel>,
    /// 抽屉段控件滑动选中块的位置(容器宽的分数:0 = 会话那一半,0.5 = Git 那一半)。
    /// 只在换面板时补间;开抽屉直接落位(见 [`drawer_tab_left`])。
    drawer_tab: mt_ui::motion::ValueTween,
    /// 提交详情栏的开合在场度(进出场统一,支持半路掉头与关动画即卸)。
    detail_presence: overlay_motion::Presence,
    /// 当前或正在退场的提交详情栏(实体 + 栏宽),退场播完自动释放。
    detail_last: Option<(Entity<git_history::CommitDetailPane>, f32)>,
    /// 左侧边条上独立隔离的 8px 闪烁圆点视图(AI 状态角标 / 更新提示蓝点),
    /// 避免 `PulsePump` 每 100ms 标脏整棵 `Workspace`。
    ai_status_dot: Entity<activity_bar::CornerDotView>,
    update_dot: Entity<activity_bar::CornerDotView>,
    /// 中栏开合的内容滑入 / 滑出(见 [`MiddleMotion`])。
    middle_motion: Option<MiddleMotion>,
    /// 上一帧中栏开着没有(`None` = 还没画过:首帧只记下,不播)。
    middle_seen: Option<bool>,
    /// 抽屉左缘正在被拖。`Some` 期间宽度由本结构自持,松手才落盘。
    drawer_drag: Option<DrawerDrag>,
    /// Git 抽屉左侧「提交详情」栏的左缘正在被拖(同上,松手才落盘)。
    detail_drag: Option<DrawerDrag>,
    usage_open: bool,
    /// 左侧 Activity Bar 整组按钮共用的 VS Code 式悬停会话。
    activity_bar_hover: activity_bar::HoverSession,
    /// 新会话第一条标签的 500ms 计时。drop 句柄立即取消;状态机的 generation
    /// 再挡住已经进入回调队列的旧任务。
    activity_bar_hover_task: Option<Task<()>>,
    /// 弹窗毛玻璃背板的快照(见 [`frost`] 模块注释)。弹窗/用量面板从无到有的
    /// 第一帧抓一次,期间沿用,全关即弃 —— 开着时再抓会把弹窗自己抓进去。
    /// 弃的时候连图集纹理一起摘(`drop_image`),光清字段每开一次弹窗漏一张。
    frost: Option<std::sync::Arc<gpui::RenderImage>>,
    /// 后台模糊任务(抓帧在 UI 线程、模糊在后台,见 [`frost::finish`])。
    /// drop 即取消 —— 弹窗在模糊完成前就关掉时,结果直接作废。
    frost_task: Option<Task<()>>,
    /// 启动自检发现的新版本(`None` = 没有 / 还没查完 / 查失败)。
    ///
    /// 与原版 `App.tsx:89` 的 `updateInfo` 同一份状态:只在进程内活着,不落盘。
    /// 边条按钮亮不亮还要过「忽略此版本」那道闸(`update_check::should_announce`,
    /// 每帧现判 —— 在弹窗里忽略、在「关于」里不再忽略,按钮即刻跟着变)。放 `Arc` 里:
    /// 带着发布说明正文,每帧交给按钮闭包时不整份复制。
    update_release: Option<Arc<crate::update_check::ReleaseInfo>>,
    /// 上一次收到关闭快捷键的时刻,判按住不放的连发用
    /// (见 [`pane_actions::swallows_close_hotkey`])。
    last_close_hotkey: Option<std::time::Instant>,
    /// 系统托盘(状态灯 + 项目菜单)。**drop 即摘图标**,所以必须由 Workspace
    /// 持有而不是丢进全局:窗口没了托盘也就该没了。
    tray: Tray,
    /// 启动版本自检的那条任务(丢了句柄它就被取消)。
    _update_check: Task<()>,
    _ai_pump: Task<()>,
    /// 移动端中转桥(泵 + store 观察者 + 去抖同步靠它的生命周期保活,
    /// 与 [`Self::_ai_pump`] 同一种分工)。见 [`mobile_relay`]。
    _relay: Entity<mobile_relay::RelayBridge>,
    _tray_pump: Task<()>,
    _activation: Subscription,
    /// 窗口大小/位置的观察者 —— 拖动缩放期间每帧回调,由 store 那边的防抖收口。
    _window_bounds: Subscription,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        // 兜底:窗口在侧栏动画中途关闭时解除终端 grid 尺寸冻结闸
        mt_ui::terminal::set_grid_resize_frozen(false);
    }
}

impl Workspace {
    pub(crate) fn new(
        store: Entity<AppStore>,
        ai_events: futures::channel::mpsc::UnboundedReceiver<ai::AiEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // 根视图的 render 直接读 store(三栏比例、徽标、未读数、背景……),
        // 任何变化都照旧重画。
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        // 子视图挂起的整窗覆盖物(如 Markdown 预览里的 ImageLightbox)改完覆盖物栈后
        // 推进 OverlayRevision,这里必须收到通知以同步抓拍 / 释放毛玻璃快照。
        overlay::observe_in(cx);
        // 托盘只在它读的数据变了时才重算快照。**推送时机就这一处** —— 原版在七个
        // 调用点上手动 `queueMicrotask(syncTrayStatus)`(状态变化 / 关项目 / 改布局 /
        // 清未读 / 焦点变化 / 托盘配置变化 …),对应的正是
        // [`StoreEvent::touches_tray`] 放行的那几类事件。此前挂在 notify 上,AI 工作时
        // 每个 pane 的 OSC 标题约 4Hz 就把快照白算一遍(再由 [`Tray::push`] 的签名
        // 去重挡掉);签名去重仍留着,挡的是「事件来了但灯色菜单没变」。
        cx.subscribe(&store, |this, _, event: &StoreEvent, cx| {
            if event.touches_tray() {
                this.sync_tray(cx);
            }
        })
        .detach();

        let title_bar = cx.new(|cx| TitleBar::new(store.clone(), cx));
        let project_list = cx.new(|cx| ProjectList::new(store.clone(), cx));
        // 列表之外的切项目入口(标题栏 / 托盘 / toast …)经它在列表里揭示新项目
        project_list::install(project_list.clone(), cx);
        let file_tree = cx.new(|cx| FileTree::new(store.clone(), cx));
        file_tree::install(file_tree.clone(), cx);
        let terminal_area = cx.new(|cx| TerminalArea::new(store.clone(), cx));
        // 键盘焦点落空(拿着焦点的终端 / 页签被关掉,弹窗把焦点还给已经关掉的终端)时接回来,
        // 见 [`Self::restore_lost_focus`]。监听跑在绘制期:那时改焦点不排下一帧、notify 也被
        // 吞掉(gpui `Window::draw` 焦点阶段),所以挪到本轮绘制之后再动手
        cx.on_focus_lost(window, |_: &mut Self, window, cx| {
            let lost = window.focused(cx).map(|handle| handle.downgrade());
            cx.defer_in(window, move |this, window, cx| {
                this.restore_lost_focus(lost, window, cx)
            });
        })
        .detach();
        let workbench_area =
            cx.new(|cx| WorkbenchArea::new(store.clone(), terminal_area.clone(), cx));
        workbench_area::install(workbench_area.clone(), cx);
        // 文件树高亮当前文件页签那一行:工作区换了活动页签要叫它重画(文件树先建,这里补订阅)
        file_tree.update(cx, |tree, cx| tree.observe_workbench(&workbench_area, cx));
        let terminals_panel = cx.new(|cx| terminals_panel::TerminalsPanel::new(store.clone(), cx));
        let session_panel = cx.new(|cx| SessionPanel::new(store.clone(), window, cx));
        let git_panel = cx.new(|cx| git_panel::GitPanel::new(store.clone(), window, cx));
        let columns_state = cx.new(|_| ResizableState::default());
        let middle_state = cx.new(|_| ResizableState::default());

        // 窗口聚焦状态:聚焦时完成的任务用户正看着,不计入「未读完成」
        let store_for_focus = store.clone();
        let activation = cx.observe_window_activation(window, move |_, window, cx| {
            let active = window.is_window_active();
            store_for_focus.update(cx, |store, cx| store.set_window_focused(active, cx));
            // 终端重绘的节拍跟着前后台切档:后台默认按 5fps 画就够了,挂着 AI 跑、
            // 人切去别的窗口时按满帧重绘整窗是纯浪费。见 `crate::redraw`。
            redraw::set_window_active(active, cx);
            // 顺手重探一次「减少动画」:用户多半是切到系统设置里改完再切回来的。
            // 变了才刷窗口(闪烁类动画的挂/摘只发生在 render 里)。
            if active && motion::refresh(cx) {
                cx.refresh_windows();
            }
        });

        // 窗口大小/位置/最大化态 → layout.db。这个回调在拖动缩放期间每帧都来,
        // 值没变直接被 `set_window_geometry` 挡掉,变了也只是标脏 + 300ms 防抖。
        let store_for_bounds = store.clone();
        let window_bounds = cx.observe_window_bounds(window, move |_, window, cx| {
            let geometry = current_window_geometry(window);
            store_for_bounds.update(cx, |store, cx| store.set_window_geometry(geometry, cx));
        });

        // AI 状态泵:后台线程(hook server / 500ms 轮询)→ channel → 这里改 store。
        // 提醒(提示音/闪任务栏/toast)要碰 Window,所以走 spawn_in 拿到窗口上下文。
        let ai_store = store.clone();
        let mut ai_events = ai_events;
        let ai_pump = cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = ai_events.next().await {
                let alert = ai_store.update(cx, |store, cx| store.apply_ai_event(event, cx));
                if let Some(alert) = alert
                    && this
                        .update_in(cx, |workspace, window, cx| {
                            workspace.deliver_alert(alert, window, cx)
                        })
                        .is_err()
                {
                    return;
                }
            }
        });

        // 移动端中转:建桥 + 登记全局 + 按配置建连一次。放在这里(而不是 `main`)
        // 是因为泵要 `spawn_in` 拿窗口 —— 移动端发起会话得建 pane、弹 toast。
        let relay = mobile_relay::install(store.clone(), window, cx);
        // 「注册 Hook 状态更准」的一次性提示:只订阅 store 事件,判据见 `hook_hint`
        hook_hint::install(store.clone(), cx);

        // 系统托盘:图标住在另一条线程上(自己的隐藏窗口 + 消息循环),
        // 交互经 channel 回到这里 —— 与 AI 状态泵同一套路数。
        let (tray, mut tray_events) = Tray::start(window);
        let tray_pump = cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = tray_events.next().await {
                if this
                    .update_in(cx, |workspace, window, cx| {
                        workspace.on_tray_event(event, window, cx)
                    })
                    .is_err()
                {
                    return;
                }
            }
        });

        // 把焦点记到当前 pane 上。此刻 PTY 还没补(启动补 PTY 排在首帧之后,见
        // `main` 末尾),`focus_pane` 只能先记下 `focused_pane_id`;终端实体建出来后
        // 由那一段再调一次 `focus_pane` 把键盘焦点真正落上去。
        let initial = {
            let s = store.read(cx);
            s.active_project_id.clone().zip(
                s.active_layout()
                    .and_then(|l| l.first_active_pane())
                    .map(|p| p.id.clone()),
            )
        };
        if let Some((project_id, pane_id)) = initial {
            store.update(cx, |store, cx| {
                store.focus_pane(&project_id, &pane_id, window, cx)
            });
        }

        // 启动版本自检(audit #30)。原版在拿到版本号之后立刻 `checkForUpdate(ver)`
        // 并 `.catch(() => {})`(`App.tsx:273-281`)—— 每次启动查一次、失败静默、
        // 没有开关(「忽略此版本」是后加的,在渲染端判,见 `update_release`)。HTTP 是
        // 阻塞的,丢后台执行器,回主线程只改一个字段(与 `pricing.rs` 拉价格表同一套路)。
        let update_check = cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async { crate::update_check::newer_release(env!("CARGO_PKG_VERSION")) })
                .await;
            let Some(release) = found else { return };
            let _ = this.update(cx, |workspace: &mut Self, cx| {
                workspace.update_release = Some(Arc::new(release));
                cx.notify();
            });
        });

        let ai_status_dot = cx.new(|_| {
            activity_bar::CornerDotView::new(ui::status_color(crate::tree::StatusLight::Idle), false)
        });
        let update_dot = cx.new(|_| activity_bar::CornerDotView::new(ui::accent(), false));

        let mut workspace = Self {
            store,
            menu_layer: menu::layer(cx),
            toast_layer: toast::layer(cx),
            title_bar,
            project_list,
            file_tree,
            terminal_area,
            workbench_area,
            terminals_panel,
            session_panel,
            git_panel,
            usage_panel: None,
            columns_state,
            middle_state,
            last_viewport: window.viewport_size(),
            right_drawer: None,
            drawer_presence: overlay_motion::Presence::hidden(
                mt_ui::motion::OVERLAY_IN,
                mt_ui::motion::OVERLAY_OUT,
            ),
            drawer_last_panel: None,
            drawer_swap: motion_kit::KeyedSwap::new(mt_ui::motion::PANEL_SWAP),
            drawer_tab: mt_ui::motion::ValueTween::settled(0.0, mt_ui::motion::TAB_INDICATOR),
            detail_presence: overlay_motion::Presence::hidden(
                mt_ui::motion::OVERLAY_IN,
                mt_ui::motion::OVERLAY_OUT,
            ),
            detail_last: None,
            ai_status_dot,
            update_dot,
            middle_motion: None,
            middle_seen: None,
            drawer_drag: None,
            detail_drag: None,
            usage_open: false,
            activity_bar_hover: activity_bar::HoverSession::default(),
            activity_bar_hover_task: None,
            frost: None,
            frost_task: None,
            update_release: None,
            last_close_hotkey: None,
            tray,
            _update_check: update_check,
            _ai_pump: ai_pump,
            _relay: relay,
            _tray_pump: tray_pump,
            _activation: activation,
            _window_bounds: window_bounds,
        };
        // 开机第一帧就把灯点上:观察者只在 store **变化**时才响,而恢复出来的
        // 布局里本来就可能有跑着的 AI 会话。
        workspace.sync_tray(cx);
        workspace
    }

    /// 一颗 Activity Bar 按钮的 enter / leave。热身前排一次 500ms 计时;
    /// 热身后只改状态并立即重画。
    fn on_activity_bar_item_hover(
        &mut self,
        key: &'static str,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        if hovered {
            match self.activity_bar_hover.enter(key) {
                activity_bar::HoverEnter::Unchanged => {}
                activity_bar::HoverEnter::ShowNow => {
                    self.activity_bar_hover_task = None;
                    cx.notify();
                }
                activity_bar::HoverEnter::Delay(generation) => {
                    // 覆盖句柄 = clearTimeout。generation 是旧回调已经排进队列时的
                    // 第二道闸,两条都保留才不会补弹过期标签。
                    self.activity_bar_hover_task = None;
                    self.activity_bar_hover_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(activity_bar::HOVER_SHOW_DELAY)
                            .await;
                        let _ = this.update(cx, |workspace: &mut Self, cx| {
                            if workspace
                                .activity_bar_hover
                                .on_delay_elapsed(generation, key)
                            {
                                workspace.activity_bar_hover_task = None;
                                cx.notify();
                            }
                        });
                    }));
                    cx.notify();
                }
            }
        } else if self.activity_bar_hover.leave(key) {
            self.activity_bar_hover_task = None;
            cx.notify();
        }
    }

    /// 给每颗 Activity Bar 按钮生成同款 hover 监听器。key 同时是按钮 id 与
    /// [`activity_bar::HoverSession`] 的稳定身份,集中在这里避免十处闭包漂移。
    fn activity_bar_item_hover_listener(
        key: &'static str,
        cx: &Context<Self>,
    ) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
        cx.listener(move |this, hovered: &bool, _window, cx| {
            this.on_activity_bar_item_hover(key, *hovered, cx);
        })
    }

    /// 只有离开**整条** Activity Bar 才降温;经过按钮间空隙只由按钮 leave
    /// 隐藏标签,保留 warmed 状态。
    fn on_activity_bar_hover(
        &mut self,
        hovered: &bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *hovered {
            return;
        }
        self.activity_bar_hover_task = None;
        if self.activity_bar_hover.reset() {
            cx.notify();
        }
    }

    /// 把 store 的当前状态压成一份托盘快照推下去(`store.ts::syncTrayStatus`)。
    ///
    /// done 判据取 [`DoneScope::Unread`] —— 与标题栏胶囊的 `All` **有意不同**:
    /// 托盘绿灯是「有你还没看过的回答」,窗口一聚焦就该灭;标题栏那颗灯不看焦点。
    fn sync_tray(&mut self, cx: &mut App) {
        let snapshot = {
            let store = self.store.read(cx);
            let config = store.config();
            tray::build_snapshot(
                config.tray_status_enabled.unwrap_or(true),
                store.window_focused(),
                &store.ai_projects(DoneScope::Unread),
                config.tray_max_projects.unwrap_or(5) as usize,
            )
        };
        self.tray.push(snapshot);
    }

    /// 托盘上的一次交互。
    ///
    /// 两条路的门控**有意不同**(`App.tsx:303-315`):左键受
    /// `trayClickFocus` 管辖(关掉时窗口已被托盘线程唤起,这里就什么都不做、
    /// 留在原地);右键菜单点项目**不受它管辖** —— 用户点的是具体项目,
    /// 那就是明确要求跳过去。
    fn on_tray_event(&mut self, event: TrayEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            TrayEvent::Clicked => {
                if !self
                    .store
                    .read(cx)
                    .config()
                    .tray_click_focus
                    .unwrap_or(true)
                {
                    return;
                }
                self.focus_attention_target(None, window, cx);
            }
            TrayEvent::ProjectClicked(project_id) => {
                // 菜单是上一次推送的快照,点下去时那个项目可能已经被删了
                if self.store.read(cx).project(&project_id).is_none() {
                    return;
                }
                // 那些 pane 也可能已经安静了 —— 定位不到目标也要把项目切过去,
                // 不能让这一下没反应
                if !self.focus_attention_target(Some(&project_id), window, cx) {
                    self.store
                        .update(cx, |store, cx| store.set_active_project(&project_id, cx));
                    project_list::reveal(&project_id, cx);
                }
            }
        }
    }

    /// 跳到「下一件该我做的事」(`utils/attentionJump.ts::focusAttentionTarget`)。
    /// 返回是否找到了目标 —— false = 全都闲着,调用方自己决定要不要兜底。
    fn focus_attention_target(
        &mut self,
        only_project: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((project_id, pane_id)) = self.store.read(cx).next_attention_target(only_project)
        else {
            return false;
        };
        self.store.update(cx, |store, cx| {
            store.set_active_project(&project_id, cx);
            store.activate_pane(&project_id, &pane_id, window, cx);
        });
        self.workbench_area
            .update(cx, |area, cx| area.activate_terminal(window, cx));
        // 跳过去的项目在「项目」块里也得看得见(折叠组里 / 可视区外)
        project_list::reveal(&project_id, cx);
        true
    }

    /// 兑现一次提醒:提示音 / 任务栏闪烁 / 远程通知 / toast。
    ///
    /// toast 走自建的 [`toast`] 层。当初记的是 gpui-component `Notification` 的
    /// 四条**结构性**缺口(没有悬停暂停、上限写死 10 条、× 只在 hover 时显形且
    /// 图标走 `IconName` 渲染成空白、去重是「替换」而原版是「忽略」)外加右上角
    /// 448px 的位置尺寸;**2026-09-19 对照 0.6.2 复核后只剩两条** —— 去重仍是
    /// 「替换」、× 仍靠 `group_hover`(悬停暂停 / 条数 / 位置尺寸都已可配,图标
    /// 那条随入口挂上 `gpui_kit_assets::Assets` 作废),逐条见 `toast.rs` 模块
    /// 注释。跳转与去重语义一并搬进那一层,这里只剩「推一条」。
    fn deliver_alert(&mut self, alert: PendingAlert, window: &mut Window, cx: &mut Context<Self>) {
        if alert.plan.sound {
            notify::play_sound(alert.sound_path.as_deref());
        }
        if alert.plan.flash {
            notify::flash_taskbar(window);
        }
        // 远程通知:几次 HTTPS 往返(每个渠道限时 10s),一律丢后台;失败只记日志
        if let Some(remote) = alert.remote {
            cx.background_executor()
                .spawn(async move { remote_notify::deliver(remote) })
                .detach();
        }
        let Some(kind) = alert.plan.toast else { return };
        toast::push_alert(kind, alert.project_id, alert.project_name, cx);
    }

    /// 当前该操作哪个 pane。键盘焦点在不在终端上(文件树、项目列表里按快捷键)都按同一条
    /// 回落链取:逻辑焦点 → 这个项目上次聚焦的 → 第一个分组的激活页签
    /// (`AppStore::active_pane_id`)。
    fn target_pane(&self, cx: &App) -> Option<(String, String)> {
        let store = self.store.read(cx);
        let project_id = store.active_project_id.clone()?;
        let pane_id = store.active_pane_id(&project_id)?;
        Some((project_id, pane_id))
    }

    fn terminal_page_active(&self, cx: &App) -> bool {
        self.workbench_area.read(cx).is_terminal_active(cx)
    }

    /// 键盘焦点落空之后接回来:落空时按键从窗口根派发,`Workspace` 这一层的快捷键一个都
    /// 收不到,打字也丢。去处(活动终端 / 终端区兜底 / 不动)见
    /// `TerminalArea::restore_lost_focus`。`lost` 是落空那一刻的焦点:推迟到绘制之后这段
    /// 时间里已经有别人接手(焦点换成了别的句柄)就不再插手。
    fn restore_lost_focus(
        &mut self,
        lost: Option<gpui::WeakFocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = window.focused(cx).map(|handle| handle.downgrade());
        if now.is_some() && now != lost {
            return;
        }
        let terminal_page = self.terminal_page_active(cx);
        let overlay_open = !overlay::allows(overlay::Yield::ToOverlay);
        self.terminal_area.update(cx, |area, cx| {
            area.restore_lost_focus(terminal_page, overlay_open, window, cx)
        });
    }

    /// Ctrl+Tab / Ctrl+Shift+Tab / Ctrl+` / Ctrl+Shift+T·W·D·E 这一刻落在哪一页。
    ///
    /// 终端页照旧走 [`yields_to_overlay`]。文件页的编辑器本身是 `Input`,套那道「焦点在
    /// 输入框里就让路」的闸,这几个键在文件页上就全废了 —— 改成只在焦点落在**别处**的输入框
    /// (Git 提交框、文件树改名框……)里时让路,覆盖物照旧让路。
    fn hotkey_page(&self, window: &mut Window, cx: &mut App) -> HotkeyPage {
        if self.terminal_page_active(cx) {
            return if yields_to_overlay(window, cx) {
                HotkeyPage::Yield
            } else {
                HotkeyPage::Terminal
            };
        }
        let typing_elsewhere = window.has_focused_input(cx)
            && !self
                .workbench_area
                .read(cx)
                .active_tab_contains_focus(window, cx);
        if typing_elsewhere || !overlay::allows(overlay::Yield::ToOverlay) {
            HotkeyPage::Yield
        } else {
            HotkeyPage::Document
        }
    }

    /// Ctrl+Shift+T / D / E 的前半截:停在页签上就先切回终端页,再照常新建 / 分屏。
    /// 返回 `false` = 这次让路。
    fn ensure_terminal_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match self.hotkey_page(window, cx) {
            HotkeyPage::Yield => false,
            HotkeyPage::Terminal => true,
            HotkeyPage::Document => {
                self.workbench_area
                    .update(cx, |area, cx| area.activate_terminal(window, cx));
                true
            }
        }
    }
    /// 中间栏的上下两块(项目列表 / 文件树)。`on_resize` 由调用方挂 —— 中栏收起的出场层
    /// 另画一份(那时栏里那份进的是不可见的 panel,不会被画),它不该回写尺寸。
    ///
    /// ⚠️ **只给上面那块播种,下面那块留空**(见 render 里那段注释)。
    fn middle_group(&self, middle: &[f64]) -> gpui_component::resizable::ResizablePanelGroup {
        v_resizable("middle-column")
            .with_state(&self.middle_state)
            .child(
                resizable_panel()
                    .size(px(middle[0] as f32))
                    .size_range(px(100.0)..Pixels::MAX)
                    // 两块都套 view 级缓存(判据见 [`cached_panel`]):内容全从
                    // store 来、各自 `cx.observe(&store)`,终端刷屏那一拍不该带着
                    // 它们重跑 render。占位样式与两者 render 根节点的
                    // `div().size_full()` 等价
                    .child(cached_panel(
                        &self.project_list,
                        StyleRefinement::default().size_full(),
                    )),
            )
            .child(
                resizable_panel()
                    .size_range(px(120.0)..Pixels::MAX)
                    .child(cached_panel(
                        &self.file_tree,
                        StyleRefinement::default().size_full(),
                    )),
            )
    }

    /// 提交详情栏此刻的宽:拖拽中按拖的,否则按落盘的;窗口窄时让一让(见
    /// [`detail_layer_width`])。
    fn detail_width(&self, drawer_width: f32, window: &Window, cx: &App) -> f32 {
        let wanted = self
            .detail_drag
            .map(|d| d.width)
            .unwrap_or_else(|| self.store.read(cx).git_detail_width()) as f32;
        detail_layer_width(
            wanted,
            f32::from(window.viewport_size().width),
            drawer_width,
        )
    }

    /// Git 抽屉左侧的「提交详情」栏(进出场统一由 [`Self::detail_presence`] 驱动,
    /// 支持半路掉头与关动画即卸)。
    ///
    /// - 进场/退场靠里层 `left = detail_slide_left(width, progress)` 从抽屉左缘滑出/收回,
    ///   零尺寸变化;
    /// - 底色用不透明的 `ui::bg_overlay()`(与抽屉本体同档),外置主题包背景图下不透叠影;
    /// - `native_view::occluder()` 挂在随 `left` 滑动的内层里,只抠当前实际盖住 WebView2 的区域;
    /// - `interactive = false`(栏自己退场或整个抽屉退场)时不画拖拽手柄,并在里层末尾盖透明
    ///   `.occlude()` 阻止点击与悬停穿透到旧内容。
    fn render_commit_detail_layer(
        &self,
        pane: Entity<git_history::CommitDetailPane>,
        drawer_width: f32,
        width: f32,
        progress: f32,
        interactive: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        div()
            .id("git-detail-layer")
            .absolute()
            .top_0()
            .right(px(drawer_width))
            .h_full()
            .w(px(width))
            .overflow_hidden()
            .child(
                div()
                    .id("git-detail-panel")
                    .absolute()
                    .top_0()
                    .left(px(detail_slide_left(width, progress)))
                    .h_full()
                    .w(px(width))
                    .when(interactive, |el| el.occlude())
                    .flex()
                    .flex_col()
                    .bg(ui::bg_overlay())
                    // 挂在滑动内层里:只把此刻真盖着 WebView2 的区域抠出来
                    .child(native_view::occluder())
                    .border_l_1()
                    .border_color(ui::border_default())
                    .child(pane)
                    .when(interactive, |el| {
                        // 左缘拖拽手柄(同抽屉那条:往左拖 = 变宽)
                        el.child(
                            div()
                                .id("git-detail-resize-handle")
                                .absolute()
                                .left_0()
                                .top_0()
                                .h_full()
                                .w(px(6.0))
                                .cursor_col_resize()
                                .hover(|el| el.bg(ui::with_alpha(ui::accent(), 0.4)))
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(
                                        move |this: &mut Self,
                                              event: &gpui::MouseDownEvent,
                                              _window,
                                              cx| {
                                            cx.stop_propagation();
                                            this.detail_drag = Some(DrawerDrag {
                                                start_x: event.position.x,
                                                start_width: width as f64,
                                                width: width as f64,
                                            });
                                        },
                                    ),
                                ),
                        )
                    })
                    .when(!interactive, |el| {
                        // 退场中的内容不接悬停与点击
                        el.child(div().absolute().inset_0().occlude())
                    }),
            )
    }

    /// 抽屉标题条(`RightDrawer.tsx:80-124`):h-9 的段控件 + ✕。
    ///
    /// 段控件的选中态底块是一个**滑动的绝对定位块**(`absolute inset-y-0 left-0
    /// w-1/2`,`transform: translateX(0% | 100%)`,`--motion-tab-indicator` 0.22s)。
    /// gpui 没有 transform,这里用 `left` 百分比做等效补间:位置是挂在 Workspace 上的
    /// [`Self::drawer_tab`],**只在换面板时**补(此前按目标面板名换 id 重播
    /// `with_animation`,抽屉关上超过驻留时间再打开,底块会从另一半滑过来)。
    fn render_drawer_header(
        &self,
        panel: DrawerPanel,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let now = std::time::Instant::now();
        let tab_left = self.drawer_tab.value_at(now);
        if self.drawer_tab.running_at(now) {
            window.request_animation_frame();
        }
        let mut seg = div()
            .relative()
            .flex()
            .flex_1()
            .h(px(24.0))
            .rounded(px(4.0))
            .overflow_hidden()
            .border_1()
            .border_color(ui::border_default())
            // 滑动选中块
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .w(gpui::relative(0.5))
                    .left(gpui::relative(tab_left))
                    .bg(ui::accent_subtle()),
            );
        for (tab, label) in [
            (DrawerPanel::Sessions, t("panels", "sessions")),
            (DrawerPanel::Git, t("panels", "git")),
        ] {
            let active = tab == panel;
            seg = seg.child(
                div()
                    .id(SharedString::from(format!("drawer-tab-{}", tab.key())))
                    .relative()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .px(px(8.0))
                    .cursor_pointer()
                    .text_size(ui::font_px(11.0))
                    .when(active, |el| el.text_color(ui::accent()))
                    .when(!active, |el| {
                        el.text_color(ui::text_muted())
                            .hover(|el| el.text_color(ui::text_primary()))
                    })
                    .child(label)
                    // 段控件走 open_drawer:**不做「再点一次关闭」**
                    .on_click(
                        cx.listener(move |this, _event, _window, cx| this.open_drawer(tab, cx)),
                    ),
            );
        }

        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(36.0))
            .flex_none()
            .px(px(6.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(seg)
            .child(
                div()
                    .id("drawer-close")
                    .w(px(24.0))
                    .h(px(24.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .flex_none()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .hover(|el| el.bg(ui::border_subtle()).text_color(ui::text_primary()))
                    .tip(t("app", "activityBar.closeDrawer"))
                    .child("✕")
                    .on_click(cx.listener(|this, _event, _window, cx| this.set_drawer(None, cx))),
            )
    }
    /// 视口尺寸一变,就把对应的 [`ResizableState`] 换成一个新的空实体 ——
    /// 下一帧那一组分栏会重新以**持久化的绝对像素**做种(`.size(px(...))`)。
    ///
    /// 为什么非这么干不可(gpui-component 0.5.1 的两条行为叠加):
    ///
    /// 1. `resizable_panel().size(..)` 传的是 **initial_size**,只在面板 state 还是
    ///    `None` 的那一帧读一次(`resizable/panel.rs` 里 `panel_state.size` 的
    ///    `match`:一旦是 `Some` 就改按它算 flex_basis)。此后我们每帧照传,全被忽略。
    /// 2. 组容器尺寸一变,`ResizableState::adjust_to_container_size()` 就把各面板
    ///    **按当前比例**重新分配(`resizable/mod.rs`)—— 绝对像素被换算成百分比再乘回去。
    ///
    /// 于是只要**容器尺寸在首帧之后还会变**,还原进去的绝对像素就被乘上一个
    /// 「新容器 / 旧容器」的系数。而这在 Windows 上是常态:窗口先按 windowed 尺寸
    /// 画出第一帧、`ShowWindowAsync(SW_MAXIMIZE)` 是之后才异步生效的
    /// (gpui `platform/windows/window.rs` 的 `set_window_placement`),用户拖窗口边框、
    /// 换显示器同理。实测:layout.db 里存的左栏 400px,1280 开窗时首帧正确还原成 400,
    /// 窗口一最大化(容器 1236 → 2516)当场变成 **814.24 = 400 × 2516/1236** ——
    /// 用户看到的就是「上次拖好的左栏宽度重启后没保住」。
    ///
    /// 顺带把「缩放窗口时左栏跟着按比例伸缩」改成了「左栏宽度不动、终端区吸收差额」,
    /// 与 VS Code / Zed 的侧栏一致;中栏上下比例同理。
    ///
    /// **不会与拖分隔条打架**:拖分隔条不改视口尺寸,压根走不到这里。宽高分开判,
    /// 免得只拉宽窗口却把中栏的上下比例也一起重播。
    fn reseed_resizables_on_viewport_change(&mut self, window: &Window, cx: &mut Context<Self>) {
        let viewport = window.viewport_size();
        if viewport == self.last_viewport {
            return;
        }
        if viewport.width != self.last_viewport.width {
            self.columns_state = cx.new(|_| ResizableState::default());
        }
        if viewport.height != self.last_viewport.height {
            self.middle_state = cx.new(|_| ResizableState::default());
        }
        self.last_viewport = viewport;
    }
}

/// 给一个「数据全从 store 来、自己 observe 了 store」的稳定面板套上 gpui 的
/// **view 级缓存**。
///
/// # 为什么非套不可
///
/// `gpui::AnyView` 的缓存**只对调过 `.cached(style)` 的 view 生效**
/// (`gpui-0.2.2/src/view.rs:170-182`:`cached_style` 是 `None` 时 `request_layout`
/// 无条件重跑 `render`;`prepaint` 走 :197 那条早退,压根到不了 :208-223 那段
/// 按 `dirty_views` 复用的逻辑)。本仓此前一处都没调过 —— 于是终端每一拍
/// (30fps,见 [`crate::redraw`])都拖着项目列表 / 文件树 / 面板竖条整套重跑 render。
///
/// 套上之后:`redraw` 里 notify 的是 `TerminalPane`,`mark_view_dirty` 只沿
/// **它那条 view 路径**往上标脏(`window.rs:1304-1318`),这几个面板不在路径上,
/// 整拍跳过 render/prepaint/paint 直接复用上一帧。
///
/// # 套之前必须逐个核实的三件事
///
/// 1. **render 读到的每一样东西都得能 notify 到这个 view**。缓存的失效条件只有
///    四条:自身进 `dirty_views`、bounds 变、content_mask 变、text_style 变
///    (外加 `Window::refresh()` 的全局绕过)。读了不 observe 的东西 = 画面冻结。
///    几条常见交互态 gpui 自己接好了,不必额外担心:hover 变化会
///    `cx.notify(current_view)`(`elements/div.rs:2068-2081`)、滚动同理
///    (:2417-2450)、tooltip 与拖拽期间走 `window.refresh()`
///    (:2616-2633 / `window.rs:3716-3727`)、`request_animation_frame`
///    notify 的也是当前 view(`window.rs:1654`)——`mt_ui::motion` 那套补间
///    因此照常跑。
/// 2. **别套在 paint 期登记了 `reuse_paint` 不搬的东西的 view 上**。`PaintIndex`
///    只覆盖 scene / mouse_listeners / input_handlers / cursor_styles /
///    tab_stops(`window.rs:2293-2329`),**`window_control_hitboxes` 不在里面**
///    且每帧 `Frame::clear` 清空 —— 用了 `.window_control_area(..)` 的 view
///    (标题栏)一旦命中缓存就会丢掉拖拽区与最小化/最大化/关闭三键。
/// 3. **祖先不能在动画里改 opacity**。opacity 是 paint 期烙进图元的
///    (`window.rs:2896` 一线),`reuse_paint` 原样重放,命中缓存就把上一帧的
///    透明度定死了。右侧抽屉那两块面板卡在这一条上。
///
/// # `style` 传什么
///
/// 命中缓存时 `request_layout` 返回的是一个**没有子节点**的占位节点,样式就是
/// 这里传的这一份(`view.rs:171-176`)。`StyleRefinement::default()` 的 size 是
/// `auto`,没有子节点即 0×0 —— 面板会整块塌掉。所以必须传一份与该面板
/// **render 根节点等价**的尺寸样式。
fn cached_panel<V: Render>(view: &Entity<V>, style: StyleRefinement) -> gpui::ViewElement<AnyView> {
    AnyView::from(view.clone()).cached(style)
}

/// 三栏(`columns_group`)在 body 横排里的宿主。
///
/// ⚠️ **`min_w(0)` 不能省**。flex 项的自动最小宽度是内容的 min-content,而叶子 tab 栏
/// 里的 tab 片是 `flex_none`(72~200px),tab 栏的 `overflow_x_scroll` 挡不住它往上报;
/// 一路上的 `size_full` / `min_w(0)` 只管各自那一层,这一层是唯一由 flex 定宽、又
/// 没钉最小宽度的。分屏把每个叶子的宽度砍半、tab 一个不少,几条 tab 就能让
/// min-content 超过窗口:宿主被撑宽,三栏的 `ResizableState` 按容器比例把中栏也放大
/// (「左栏被撑宽」),右侧面板竖条与新 pane 被挤出窗口右缘。
///
/// 在 gpui 测试平台上按同一嵌套实测(1280 宽窗口):分屏后左叶 5 个 200px 的 tab,
/// 不加时三栏被撑到 1496、中栏 300 → 352、竖条整条出界;单叶 12 个 tab 同样溢出。
/// 加上之后各块 bounds 与 tab 少时逐一吻合。pane 套不套 view 级缓存两种写法测出来
/// 完全相同 —— 与 `terminal_area::cached_terminal` 无关。
fn columns_host() -> gpui::Div {
    div().flex_1().min_w(px(0.0)).h_full()
}

/// 窗口级选区「抬起丢了」的兜底:左键没按着却还在拖选,当场收尾。
///
/// gpui-base 的 `TextSelectionLayer`(gpui-component `Root` 的首个子元素)在左键按下
/// 冒泡到根时就进拖选态,**只认冒泡到根的 MouseUp 收尾**
/// (`gpui-base-0.6.2/src/text_selection.rs:2589-2598` 开始、`:2741-2750` 收尾);期间
/// 每次 MouseMove / 滚轮都在延伸选区,且不看左键还按没按着(`:2724-2738`)。按下到了、
/// 抬起没到,光标划过会话详情 / md 预览就整段选成蓝底,直到下一次完整的点击。
/// 已知丢抬起的两条路:
///
/// 1. **单击标题栏拖拽区**:`WM_NCLBUTTONDOWN` 先被翻成 MouseDown 派发一遍
///    (`gpui-pre-windows-0.3.5/src/events.rs:1054-1099`),`HTCAPTION` 没人吞就交给
///    `DefWindowProc` 进系统移动循环,抬起被循环吃掉,`WM_NCLBUTTONUP` 根本不来。
///    标题栏自己另有一道(`title_bar::drag_area`);窗口边缘缩放按同一条路推断也会中。
/// 2. **MouseUp 半路被 `stop_propagation`**:自绘菜单项的 `on_click` 就是这样。
///
/// 所以「capture 相补一次 MouseUp 收尾」兜不住第 1 条 —— 那里压根没有 MouseUp。
/// 判据改看 MouseMove 自带的 `pressed_button`:正经拖选时左键一定按着(Windows 取
/// `MK_LBUTTON`),没按着还在拖选 = 抬起丢了。挂 **capture 相**,先于选区层的冒泡相
/// 处理器收尾,这一拍的移动就不会再延伸选区。`TextSelection::end` 不在拖选态时直接
/// 返回,每次移动只多一次表查;起点没落在文字上(标题栏、菜单)的,收尾时连锚点一起
/// 清掉(`text_selection.rs:1248-1260`)。
///
/// 触摸长按也会置拖选态,但它只来自 `PlatformInput::Touch`,三个桌面平台都不产生
/// (只有 web 后端有),不会被这里误收。
pub(crate) fn text_selection_release_guard() -> impl IntoElement {
    gpui::canvas(
        |_, _, _| {},
        |_, _, window, _| {
            window.on_mouse_event(|event: &gpui::MouseMoveEvent, phase, window, cx| {
                if phase.capture() && event.pressed_button != Some(gpui::MouseButton::Left) {
                    gpui_base::TextSelection::end(window, cx);
                }
            });
        },
    )
    .absolute()
    .inset_0()
}

#[cfg(test)]
mod tests;
