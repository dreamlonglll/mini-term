//! mini-term 的 GPUI 应用壳。
//!
//! # 组件树
//!
//! ```text
//! Root(gpui_component 的根,承载 Dialog / 通知层;Input 也要求它在场)
//!  └─ Workspace(持有 AppStore 与各栏视图)
//!      ├─ background_art(主题包背景图,窗口级,必须是第一个 child)
//!      ├─ ActivityBar(44px 窄边条)                   ← 替 ActivityBar.tsx
//!      ├─ h_resizable "columns"                      ← 替 Allotment 外层
//!      │   ├─ panel(可折叠,宽度落 config.layoutSizes[0])
//!      │   │   └─ h_resizable "middle"               ← 替 Allotment 内层
//!      │   │       ├─ ProjectList                    ← 项目列表
//!      │   │       └─ FileTree                       ← 文件树
//!      │   ├─ panel
//!      │   │   └─ TerminalArea                       ← SplitNode 树 → 嵌套 resizable
//!      │   │       └─ (leaf) tab 栏 + TerminalPane 实体
//!      │   └─ panel(可折叠)
//!      │       └─ SessionPanel                       ← AI 历史(右侧抽屉)
//!      ├─ UsagePanel(浮层)                           ← 用量统计
//!      ├─ Root::render_dialog_layer                  ← 各类 Modal
//!      └─ Root::render_notification_layer            ← 完成 / 待确认 toast
//! ```
//!
//! # 事件流
//!
//! ```text
//! 用户键入 → TerminalPane::write → AiPerception::observe_input → PtySession::write
//!                                └→ PaneEvent::UserInput → 清 attention 黄灯
//! PTY reader 线程 → TerminalEmulator::advance + observe_output → 唤醒 channel → 重绘
//! hook / 500ms 轮询 → StatusSink → channel → Workspace 的前台任务 → AppStore
//!                                                   └→ PendingAlert → 提示音/闪任务栏/toast
//! 布局/配置变化 → AppStore::save_config_soon(500ms 防抖)→ ConfigStore::save(带令牌)
//! ```
//!
//! 状态形状与操作语义对照 `src/store.ts`,见 [`store`] 与 [`tree`] 两个模块的注释。

mod activity_bar;
mod ai;
mod branch_family;
mod clipboard;
mod dnd;
mod file_tree;
mod file_viewer;
mod focus_nav;
mod fs_ops;
mod git_changes;
mod git_diff;
mod git_graph;
mod git_history;
mod git_panel;
mod git_watch;
mod git_worktree;
mod hotkeys;
mod i18n;
mod markers;
mod menu;
mod mobile_panel;
mod mobile_relay;
mod modal;
mod notify;
mod overlay;
mod pane;
mod pane_actions;
mod persist;
mod pricing;
mod project_list;
mod project_switcher;
mod project_tree;
mod prompt;
mod search_modal;
mod session_branch;
mod session_panel;
mod settings;
mod shell_ops;
mod store;
mod terminal_area;
mod theme;
mod title_bar;
mod toast;
mod tray;
mod tree;
mod ui;
mod usage_panel;

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use gpui::{
    AnimationExt as _, App, AppContext, Application, Bounds, Context, Entity, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div,
    prelude::FluentBuilder, px, size,
};
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_component::tooltip::Tooltip;
use gpui_component::{Root, WindowExt as _};
use mt_ui::icons::{StatusDot, StatusKind};

use crate::ai::AiBridge;
use crate::file_tree::FileTree;
use crate::focus_nav::Direction;
use crate::i18n::t;
use crate::project_list::ProjectList;
use crate::session_panel::SessionPanel;
use crate::store::{AppStore, DoneScope, PendingAlert};
use crate::terminal_area::TerminalArea;
use crate::title_bar::TitleBar;
use crate::tray::{Tray, TrayEvent};
use crate::tree::SplitDirection;
use crate::usage_panel::UsagePanel;

actions!(
    mini_term,
    [
        /// 新建终端标签(Ctrl+Shift+T)
        NewTerminal,
        /// 关闭当前**整组**(Ctrl+Shift+W)。
        ///
        /// 原版 `closePane` 调的是 `closeLeaf` —— 关的是当前分屏格里的全部 tab,
        /// 不是单个 tab(单个 tab 走 tab 上的 × )。
        ClosePane,
        /// 向右分屏(Ctrl+Shift+D)
        SplitRight,
        /// 向下分屏(Ctrl+Shift+E)
        SplitDown,
        /// 折叠/展开中间栏(Ctrl+Shift+B)
        ToggleMiddleColumn,
        /// 叶内切到下一个 tab(Ctrl+Tab)
        NextPane,
        /// 叶内切到上一个 tab(Ctrl+Shift+Tab)
        PrevPane,
        /// 重命名当前标签(F2)
        RenamePane,
        /// 终端配置(Ctrl+,)
        OpenTerminalSettings,
        /// 开合 AI 历史面板(Ctrl+Shift+A)
        ToggleSessions,
        /// 开合用量统计面板(Ctrl+Shift+U)
        ToggleUsage,
        /// 跳到下一件待办(Ctrl+Shift+J)
        JumpAttention,
        /// 焦点移到左侧分屏(Alt+←)
        FocusLeft,
        /// 焦点移到右侧分屏(Alt+→)
        FocusRight,
        /// 焦点移到上方分屏(Alt+↑)
        FocusUp,
        /// 焦点移到下方分屏(Alt+↓)
        FocusDown,
        /// 终端内查找(Ctrl+F)
        TerminalSearch,
        /// 全局搜索(Ctrl+Shift+F,toggle)
        GlobalSearch,
        /// 项目快速切换器(Ctrl+Shift+P)
        SwitchProject,
        /// 跳到上一个 AI 任务标记(Ctrl+Shift+↑)
        MarkerPrev,
        /// 跳到下一个 AI 任务标记(Ctrl+Shift+↓)
        MarkerNext,
    ]
);

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

/// 选中叶内第 N 个 tab(Ctrl+1..9,**1-based**;越界不动)。
///
/// 带数据的 action 必须走 `derive(Action)`(`actions!` 只生成单元结构),
/// `no_json` 让它不要求 serde/schemars —— 这个 action 只从代码里绑,不进键位 JSON。
#[derive(Clone, PartialEq, Default, Debug, gpui::Action)]
#[action(namespace = mini_term, no_json)]
pub struct SelectPane(pub usize);

/// 三栏默认宽度(像素),与 `src/App.tsx` 的 Allotment 默认值一致。
const DEFAULT_COLUMNS: [f64; 2] = [520.0, 1000.0];
const DEFAULT_MIDDLE: [f64; 2] = [320.0, 380.0];

/// 浮层退场后 DOM 还留多久(`src/hooks/useOverlayMotion.ts:19` 的 `OVERLAY_EXIT_MS`)。
const OVERLAY_EXIT_MS: u64 = 400;
/// `--motion-overlay-in` / `--motion-overlay-out` / `--motion-terminal-swap`
/// (`styles.css:67-78`)。
const MOTION_OVERLAY_IN_MS: u64 = 240;
const MOTION_OVERLAY_OUT_MS: u64 = 140;
const MOTION_PANEL_SWAP_MS: u64 = 200;

/// 应用数据目录:`config.json`、`hook-server.json` 的落点。
///
/// **开发用逃生门 `MT_APP_DATA_DIR`**:装机版正在跑的时候直接 `cargo run` 会与它
/// 共用同一个目录 —— 配置被两边轮流改写,hook 端口文件更是直接互抢(装机版占了
/// 23456,新起的这个退到 23457 并把端口文件覆盖成自己的)。设了这个环境变量就整
/// 个隔离出去,与 Tauri 那边靠 `--config` 覆盖 identifier 是同一招。
pub fn app_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MT_APP_DATA_DIR") {
        return PathBuf::from(dir);
    }
    mt_config::app_data_dir().unwrap_or_else(|_| PathBuf::from("."))
}

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

/// 抽屉退场动画的驻留。原版 `useOverlayPresence` + `OVERLAY_EXIT_MS = 400`:
/// 关闭时 DOM 与**面板内容**都多留 400ms,否则「抽屉在滑出的同时内容先空掉」
/// (`RightDrawer.tsx:29-30` 原注释)。
struct DrawerExit {
    panel: DrawerPanel,
    _timer: Task<()>,
}

/// 右抽屉左缘拖拽的一次会话。
#[derive(Clone, Copy)]
struct DrawerDrag {
    /// 按下时的鼠标 x(窗口坐标)。
    start_x: gpui::Pixels,
    /// 按下时的宽度。
    start_width: f64,
    /// 当前宽度(已钳到 240..720)。
    width: f64,
}

struct Workspace {
    store: Entity<AppStore>,
    /// 右键菜单浮层。状态住在全局(任何视图都能 `menu::show`),这里只是把它
    /// **画出来**的那个位置 —— 与 `Root::render_dialog_layer` 同一种分工。
    menu_layer: Entity<menu::ContextMenu>,
    /// 自绘标题栏(无边框窗口的拖拽区 + 三键 + 项目胶囊 + 全局状态灯)。
    title_bar: Entity<TitleBar>,
    /// 自建 toast 层。与 [`Self::menu_layer`] 同一种分工:状态住在全局
    /// (AI 泵 / pane / store 三处都要往里推),这里只是把它**画出来**的位置。
    toast_layer: Entity<toast::ToastLayer>,
    project_list: Entity<ProjectList>,
    file_tree: Entity<FileTree>,
    terminal_area: Entity<TerminalArea>,
    session_panel: Entity<SessionPanel>,
    /// Git 面板(抽屉的第二块)。与会话面板一样常驻实体,靠
    /// [`GitPanel::set_visible`](git_panel::GitPanel::set_visible) 闸住扫盘与
    /// pty 输出旁路。
    git_panel: Entity<git_panel::GitPanel>,
    /// 用量面板惰性创建:它一开就跑账本同步,没打开过就不该有这笔开销。
    usage_panel: Option<Entity<UsagePanel>>,
    columns_state: Entity<ResizableState>,
    middle_state: Entity<ResizableState>,
    /// 右侧悬浮抽屉现在开着哪一块(运行时态,不持久化 —— 与旧版一致)。
    right_drawer: Option<DrawerPanel>,
    /// 正在播退场动画的那一块(见 [`DrawerExit`])。
    drawer_exit: Option<DrawerExit>,
    /// 抽屉左缘正在被拖。`Some` 期间宽度由本结构自持,松手才落盘。
    drawer_drag: Option<DrawerDrag>,
    usage_open: bool,
    /// 系统托盘(状态灯 + 项目菜单)。**drop 即摘图标**,所以必须由 Workspace
    /// 持有而不是丢进全局:窗口没了托盘也就该没了。
    tray: Tray,
    _ai_pump: Task<()>,
    /// 移动端中转桥(泵 + store 观察者 + 去抖同步靠它的生命周期保活,
    /// 与 [`Self::_ai_pump`] 同一种分工)。见 [`mobile_relay`]。
    _relay: Entity<mobile_relay::RelayBridge>,
    _tray_pump: Task<()>,
    _activation: Subscription,
}

impl Workspace {
    fn new(
        store: Entity<AppStore>,
        ai_events: futures::channel::mpsc::UnboundedReceiver<ai::AiEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // store 的每一次 notify 都顺带刷一遍托盘。**推送时机就这一处** ——
        // 原版在七个调用点上手动 `queueMicrotask(syncTrayStatus)`(状态变化 /
        // 关项目 / 改布局 / 清未读 / 焦点变化 / 托盘配置变化 …),而这些在 GPUI
        // 侧无一例外都以 `cx.notify()` 收尾,挂观察者等于把七处一次覆盖全。
        // 代价是会被无关变化(改个字号)带着跑一遍,由 [`Tray::push`] 的签名去重挡住。
        cx.observe(&store, |this, _, cx| {
            this.sync_tray(cx);
            cx.notify();
        })
        .detach();

        let title_bar = cx.new(|cx| TitleBar::new(store.clone(), cx));
        let project_list = cx.new(|cx| ProjectList::new(store.clone(), cx));
        let file_tree = cx.new(|cx| FileTree::new(store.clone(), cx));
        let terminal_area = cx.new(|cx| TerminalArea::new(store.clone(), cx));
        let session_panel = cx.new(|cx| SessionPanel::new(store.clone(), cx));
        let git_panel = cx.new(|cx| git_panel::GitPanel::new(store.clone(), window, cx));
        let columns_state = cx.new(|_| ResizableState::default());
        let middle_state = cx.new(|_| ResizableState::default());

        // 窗口聚焦状态:聚焦时完成的任务用户正看着,不计入「未读完成」
        let store_for_focus = store.clone();
        let activation = cx.observe_window_activation(window, move |_, window, cx| {
            let active = window.is_window_active();
            store_for_focus.update(cx, |store, cx| store.set_window_focused(active, cx));
        });

        // AI 状态泵:后台线程(hook server / 500ms 轮询)→ channel → 这里改 store。
        // 提醒(提示音/闪任务栏/toast)要碰 Window,所以走 spawn_in 拿到窗口上下文。
        let ai_store = store.clone();
        let mut ai_events = ai_events;
        let ai_pump = cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = ai_events.next().await {
                let Ok(alert) = ai_store.update(cx, |store, cx| store.apply_ai_event(event, cx))
                else {
                    return;
                };
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

        // 恢复出来的布局已经把 PTY 补齐了,键盘焦点也该落到当前 pane 上 ——
        // 否则用户得先点一下终端才能打字。
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

        let mut workspace = Self {
            store,
            menu_layer: menu::layer(cx),
            toast_layer: toast::layer(cx),
            title_bar,
            project_list,
            file_tree,
            terminal_area,
            session_panel,
            git_panel,
            usage_panel: None,
            columns_state,
            middle_state,
            right_drawer: None,
            drawer_exit: None,
            drawer_drag: None,
            usage_open: false,
            tray,
            _ai_pump: ai_pump,
            _relay: relay,
            _tray_pump: tray_pump,
            _activation: activation,
        };
        // 开机第一帧就把灯点上:观察者只在 store **变化**时才响,而恢复出来的
        // 布局里本来就可能有跑着的 AI 会话。
        workspace.sync_tray(cx);
        workspace
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
                if !self.store.read(cx).config().tray_click_focus.unwrap_or(true) {
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
        true
    }

    /// 兑现一次提醒:提示音 / 任务栏闪烁 / toast。
    ///
    /// toast 走自建的 [`toast`] 层。gpui-component 的 `Notification` 有四条
    /// **结构性**缺口(没有悬停暂停、上限写死 10 条、× 只在 hover 时显形且图标走
    /// `IconName` 渲染成空白、去重是「替换」而原版是「忽略」),外加右上角 448px
    /// 的位置尺寸 —— 都不是宿主能绕过去的,见 `toast.rs` 模块注释。跳转与去重
    /// 语义一并搬进那一层,这里只剩「推一条」。
    fn deliver_alert(
        &mut self,
        alert: PendingAlert,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if alert.plan.sound {
            notify::play_sound(alert.sound_path.as_deref());
        }
        if alert.plan.flash {
            notify::flash_taskbar(window);
        }
        let Some(kind) = alert.plan.toast else { return };
        toast::push_alert(kind, alert.project_id, alert.project_name, cx);
    }

    /// 当前该操作哪个 pane:焦点 pane,没有就落到布局里第一个激活 pane。
    fn target_pane(&self, cx: &App) -> Option<(String, String)> {
        let store = self.store.read(cx);
        let project_id = store.active_project_id.clone()?;
        let pane_id = store.active_pane_id(&project_id)?;
        Some((project_id, pane_id))
    }

    fn on_new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        if yields_to_overlay(window, cx) {
            return;
        }
        let Some(project_id) = self.store.read(cx).active_project_id.clone() else {
            return;
        };
        let anchor = self.target_pane(cx).map(|(_, pane)| pane);
        self.store.update(cx, |store, cx| {
            store.new_terminal(&project_id, None, anchor, window, cx);
        });
    }

    /// Ctrl+Shift+W = 关**整组**(原版 `closePane` 调的是 `closeLeaf`),
    /// 不是关当前这一个 tab —— 单个 tab 走 tab 上的 ×。
    ///
    /// 走 [`pane_actions::close_leaf_of_pane`] 而不是直接调 store:关闭前要盘点
    /// 组里活着的 AI 会话并确认(三条关闭路径共用同一个入口)。
    fn on_close_pane(&mut self, _: &ClosePane, window: &mut Window, cx: &mut Context<Self>) {
        if yields_to_overlay(window, cx) {
            return;
        }
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        pane_actions::close_leaf_of_pane(self.store.clone(), project_id, pane_id, window, cx);
    }

    fn on_next_pane(&mut self, _: &NextPane, window: &mut Window, cx: &mut Context<Self>) {
        if yields_to_overlay(window, cx) {
            return;
        }
        self.cycle_pane(1, window, cx);
    }

    fn on_prev_pane(&mut self, _: &PrevPane, window: &mut Window, cx: &mut Context<Self>) {
        if yields_to_overlay(window, cx) {
            return;
        }
        self.cycle_pane(-1, window, cx);
    }

    fn cycle_pane(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        self.store.update(cx, |store, cx| {
            store.cycle_pane(&project_id, &pane_id, delta, window, cx)
        });
    }

    fn on_select_pane(&mut self, action: &SelectPane, window: &mut Window, cx: &mut Context<Self>) {
        if yields_to_overlay(window, cx) {
            return;
        }
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        let index = action.0;
        self.store.update(cx, |store, cx| {
            store.select_pane_by_index(&project_id, &pane_id, index, window, cx)
        });
    }

    fn on_split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        if yields_to_overlay(window, cx) {
            return;
        }
        self.split(SplitDirection::Horizontal, window, cx);
    }

    fn on_split_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
        if yields_to_overlay(window, cx) {
            return;
        }
        self.split(SplitDirection::Vertical, window, cx);
    }

    fn split(&mut self, direction: SplitDirection, window: &mut Window, cx: &mut Context<Self>) {
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        self.store.update(cx, |store, cx| {
            store.split_pane(&project_id, &pane_id, direction, window, cx);
        });
    }

    fn on_toggle_middle(
        &mut self,
        _: &ToggleMiddleColumn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if yields_to_overlay(window, cx) {
            return;
        }
        self.store.update(cx, |store, cx| store.toggle_middle_column(cx));
    }

    fn on_rename_pane(&mut self, _: &RenamePane, window: &mut Window, cx: &mut Context<Self>) {
        if yields_to_overlay(window, cx) {
            return;
        }
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        let current = self
            .store
            .read(cx)
            .active_layout()
            .and_then(|l| l.pane(&pane_id))
            .map(|p| p.label().to_string())
            .unwrap_or_default();
        modal::open_rename_pane(self.store.clone(), project_id, pane_id, current, window, cx);
    }

    fn on_open_settings(
        &mut self,
        _: &OpenTerminalSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 白名单动作:覆盖物压着照样开(设置面板本身就是弹窗),但**焦点在输入框
        // 里时仍然让路** —— 原版 isTypingTarget 那道闸排在白名单之前
        if yields_to_typing(window, cx) {
            return;
        }
        settings::open_settings(self.store.clone(), None, window, cx);
    }

    fn on_toggle_sessions(
        &mut self,
        _: &ToggleSessions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if yields_to_overlay(window, cx) {
            return;
        }
        self.toggle_drawer(DrawerPanel::Sessions, cx);
    }

    /// 边条两颗按钮用的开关:相同则收起,否则换过去
    /// (`store.ts:686` 的 `toggleRightDrawer`)。
    fn toggle_drawer(&mut self, panel: DrawerPanel, cx: &mut Context<Self>) {
        let next = if self.right_drawer == Some(panel) {
            None
        } else {
            Some(panel)
        };
        self.set_drawer(next, cx);
    }

    /// 抽屉内 segmented 切换用的:直接换过去,**不做「再点一次关闭」**
    /// (`store.ts:687` 原注释)。
    fn open_drawer(&mut self, panel: DrawerPanel, cx: &mut Context<Self>) {
        self.set_drawer(Some(panel), cx);
    }

    /// 换抽屉。可见性要透给两个面板 —— 收着的时候会话面板不该去扫会话
    /// (WSL 那一路会冷启动整台 VM),Git 面板不该去 `discover_git_repos`(扫盘)。
    fn set_drawer(&mut self, next: Option<DrawerPanel>, cx: &mut Context<Self>) {
        if self.right_drawer == next {
            return;
        }
        let prev = self.right_drawer;
        self.right_drawer = next;
        self.session_panel.update(cx, |panel, cx| {
            panel.set_visible(next == Some(DrawerPanel::Sessions), cx)
        });
        self.git_panel.update(cx, |panel, cx| {
            panel.set_visible(next == Some(DrawerPanel::Git), cx)
        });

        // 整块收起时留 400ms 给退场动画(面板实体必须还在树上,否则「抽屉在
        // 滑出的同时内容先空掉」)。换面板不进退场:那是 panel-swap 的活。
        self.drawer_exit = match (prev, next) {
            (Some(panel), None) => Some(DrawerExit {
                panel,
                _timer: cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(OVERLAY_EXIT_MS))
                        .await;
                    let _ = this.update(cx, |this: &mut Self, cx| {
                        this.drawer_exit = None;
                        cx.notify();
                    });
                }),
            }),
            _ => None,
        };
        cx.notify();
    }

    fn on_toggle_usage(&mut self, _: &ToggleUsage, window: &mut Window, cx: &mut Context<Self>) {
        if yields_to_overlay(window, cx) {
            return;
        }
        self.toggle_usage(window, cx);
    }

    /// 开合用量面板。可见性要透给面板 —— 它常驻不销毁,自动刷新定时器只能靠
    /// 这个开关闸住(不然关掉之后还在每 5s 扫会话文件)。
    fn toggle_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.usage_open = !self.usage_open;
        if self.usage_open && self.usage_panel.is_none() {
            let store = self.store.clone();
            let dir = app_data_dir();
            self.usage_panel = Some(cx.new(|cx| UsagePanel::new(store, dir, window, cx)));
        }
        let open = self.usage_open;
        if let Some(panel) = self.usage_panel.as_ref() {
            panel.update(cx, |panel, cx| panel.set_visible(open, cx));
        }
        cx.notify();
    }

    /// 跳到「下一件该我做的事」(旧版点标题栏状态灯的动作)。
    ///
    /// 落点算法与托盘左键**共用** [`Self::focus_attention_target`](原版也是同一个
    /// `focusAttentionTarget`);这里多一句清未读 —— 点状态灯就是「我看过了」。
    fn on_jump_attention(
        &mut self,
        _: &JumpAttention,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if yields_to_overlay(window, cx) {
            return;
        }
        if self.focus_attention_target(None, window, cx) {
            self.store.update(cx, |store, cx| store.clear_unread_done(cx));
        }
    }

    fn on_focus_left(&mut self, _: &FocusLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_adjacent(Direction::Left, window, cx);
    }
    fn on_focus_right(&mut self, _: &FocusRight, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_adjacent(Direction::Right, window, cx);
    }
    fn on_focus_up(&mut self, _: &FocusUp, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_adjacent(Direction::Up, window, cx);
    }
    fn on_focus_down(&mut self, _: &FocusDown, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_adjacent(Direction::Down, window, cx);
    }

    fn focus_adjacent(&mut self, dir: Direction, window: &mut Window, cx: &mut Context<Self>) {
        // 这四条只从快捷键进来,守卫放这一处就够(Alt+方向键在文本输入框里
        // 还是「按词移动」,让路给弹窗是必须的)
        if yields_to_overlay(window, cx) {
            return;
        }
        self.terminal_area
            .update(cx, |area, cx| area.focus_adjacent(dir, window, cx));
    }

    /// Ctrl+F:在**当前焦点 pane** 上开查找条。
    ///
    /// 与原版有一处差:原版在「当前 pane 还没有 ptyId」时**不拦**这次按键
    /// (让 Ctrl+F 原样落进终端,发 `\x06`),而 gpui 的 action 一旦绑上就必然吞掉
    /// 按键、没有「退回按键」的通路。取舍是:PTY 还没起来的空 pane 上按 Ctrl+F
    /// 什么也不发生 —— 那个 pane 本来也没有终端能收这个字节。
    fn on_terminal_search(&mut self, _: &TerminalSearch, window: &mut Window, cx: &mut Context<Self>) {
        if yields_to_overlay(window, cx) {
            return;
        }
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        let pane = {
            let store = self.store.read(cx);
            store
                .project_state(&project_id)
                .and_then(|state| state.layout.as_ref())
                .and_then(|layout| layout.pane(&pane_id))
                .and_then(|pane| pane.pty_id)
                .and_then(|pty_id| store.terminal(pty_id).cloned())
        };
        let Some(pane) = pane else { return };
        pane.update(cx, |pane, cx| pane.open_search(window, cx));
    }

    /// Ctrl+Shift+F:开合全局搜索。
    ///
    /// **两道闸都不加**,与原版有一处有意的偏差:原版把 globalSearch 放进白名单
    /// 时写着「它是 toggle,弹窗开着时按第二次才能关掉」,可搜索框一打开焦点就在
    /// 它自己的输入框里,`isTypingTarget` 那道闸先一步把这条挡掉了 —— 注释里的
    /// toggle 实际做不到。这里让它真的 toggle(按注释的意图,不是按它的 bug)。
    fn on_global_search(&mut self, _: &GlobalSearch, window: &mut Window, cx: &mut Context<Self>) {
        search_modal::toggle(self.store.clone(), window, cx);
    }

    /// Ctrl+Shift+↑:跳到上一个 AI 任务标记。首次按跳**最新一条**,
    /// 之后每按一次往上一格,到顶停住(非环形,见 [`markers::next_index`])。
    ///
    /// ⚠️ **加了 `yields_to_overlay`,与原版有意不同**:原版这两条不走
    /// `useGlobalHotkeys`,自己挂 capture 阶段的 window 监听
    /// (`useMarkerHotkeys.ts:59`),因此绕过了「焦点在输入框里」与「弹窗压着」
    /// 两道闸。方向键在输入框里有明确语义,在设置对话框里按 Ctrl+Shift+↑ 去跳终端
    /// 是意外行为 —— 这里让它与其余全局动作同口径。
    fn on_marker_prev(&mut self, _: &MarkerPrev, window: &mut Window, cx: &mut Context<Self>) {
        if yields_to_overlay(window, cx) {
            return;
        }
        self.store.update(cx, |store, cx| store.step_marker(-1, cx));
    }

    /// Ctrl+Shift+↓:跳到下一个 AI 任务标记。首次按跳**最早一条**。
    /// 让路口径见 [`Self::on_marker_prev`]。
    fn on_marker_next(&mut self, _: &MarkerNext, window: &mut Window, cx: &mut Context<Self>) {
        if yields_to_overlay(window, cx) {
            return;
        }
        self.store.update(cx, |store, cx| store.step_marker(1, cx));
    }

    /// 抽屉标题条(`RightDrawer.tsx:80-124`):h-9 的段控件 + ✕。
    ///
    /// 段控件的选中态底块是一个**滑动的绝对定位块**(`absolute inset-y-0 left-0
    /// w-1/2`,`transform: translateX(0% | 100%)`,`--motion-tab-indicator` 0.22s)。
    /// gpui 没有 transform,这里用 `left` 百分比 + `with_animation` 做等效补间:
    /// 换面板时 id 里带目标面板名 → 动画重播,底块从 0% 滑到 50%(或反过来)。
    fn render_drawer_header(
        &self,
        panel: DrawerPanel,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let to_git = panel == DrawerPanel::Git;
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
                    .id(SharedString::from(format!("drawer-tab-ind-{}", panel.key())))
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .w(gpui::relative(0.5))
                    .bg(ui::accent_subtle())
                    .with_animation(
                        SharedString::from(format!("drawer-tab-slide-{}", panel.key())),
                        gpui::Animation::new(std::time::Duration::from_millis(220))
                            .with_easing(ui::cubic_bezier(0.16, 1.0, 0.3, 1.0)),
                        move |el, delta| {
                            // 起点是另一半,终点是自己那一半
                            let from = if to_git { 0.0 } else { 0.5 };
                            let to = if to_git { 0.5 } else { 0.0 };
                            el.left(gpui::relative(from + (to - from) * delta))
                        },
                    ),
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
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.open_drawer(tab, cx)
                    })),
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
                    .tooltip(move |window, cx| {
                        Tooltip::new(t("app", "activityBar.closeDrawer")).build(window, cx)
                    })
                    .child("✕")
                    .on_click(cx.listener(|this, _event, _window, cx| this.set_drawer(None, cx))),
            )
    }

    /// Ctrl+Shift+P:项目快速切换器。
    fn on_switch_project(
        &mut self,
        _: &SwitchProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if yields_to_overlay(window, cx) {
            return;
        }
        project_switcher::open(self.store.clone(), window, cx);
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (columns, middle, middle_visible, drawer_width, unread, global_status, background) = {
            let store = self.store.read(cx);
            let config = store.config();
            let columns = config
                .layout_sizes
                .clone()
                .filter(|s| s.len() == 2)
                .unwrap_or_else(|| DEFAULT_COLUMNS.to_vec());
            let middle = config
                .middle_column_sizes
                .clone()
                .filter(|s| s.len() == 2)
                .unwrap_or_else(|| DEFAULT_MIDDLE.to_vec());
            (
                columns,
                middle,
                config.middle_column_visible,
                store.right_drawer_width(),
                store.unread_done_count(),
                store.global_ai_status(),
                store.background_art().cloned(),
            )
        };

        let store_for_columns = self.store.clone();
        let store_for_middle = self.store.clone();
        // 拖拽期间宽度自持,松手才落盘(与原版 `RightDrawer` 的 `onResizeEnd` 同)
        let drawer_width = self.drawer_drag.map(|d| d.width).unwrap_or(drawer_width);

        let middle_group = h_resizable("middle-column")
            .with_state(&self.middle_state)
            .child(
                resizable_panel()
                    .size(px(middle[0] as f32))
                    .size_range(px(100.0)..px(600.0))
                    .child(self.project_list.clone()),
            )
            .child(
                resizable_panel()
                    .size(px(middle[1] as f32))
                    .size_range(px(120.0)..px(800.0))
                    .child(self.file_tree.clone()),
            )
            .on_resize(move |state, _window, cx| {
                let sizes: Vec<f64> = state.read(cx).sizes().iter().map(|p| f32::from(*p) as f64).collect();
                store_for_middle.update(cx, |store, cx| store.set_middle_column_sizes(sizes, cx));
            });

        let columns_group = h_resizable("columns")
            .with_state(&self.columns_state)
            .child(
                resizable_panel()
                    .visible(middle_visible)
                    .size(px(columns[0] as f32))
                    .size_range(px(180.0)..px(700.0))
                    .child(middle_group),
            )
            .child(resizable_panel().child(self.terminal_area.clone()))
            .on_resize(move |state, _window, cx| {
                let sizes: Vec<f64> = state.read(cx).sizes().iter().map(|p| f32::from(*p) as f64).collect();
                store_for_columns.update(cx, |store, cx| {
                    // 折叠/收起的那一栏**不写回**:gpui-component 的
                    // `ResizableState` 按 children 个数占位,不可见的面板既不
                    // 渲染也不上报自己的尺寸,`sizes[i]` 停在建组时的最小值上。
                    // 照抄回去的话「收起中间栏后拖一下右边的分隔条」就把中间栏
                    // 宽度抹成最小值,再展开时只剩一条缝。
                    let mut columns = store
                        .config()
                        .layout_sizes
                        .clone()
                        .filter(|s| s.len() == 2)
                        .unwrap_or_else(|| DEFAULT_COLUMNS.to_vec());
                    if middle_visible && let Some(w) = sizes.first() {
                        columns[0] = *w;
                    }
                    if let Some(w) = sizes.get(1) {
                        columns[1] = *w;
                    }
                    // layoutSizes 恒为两项 —— 磁盘格式与装机版共用,不许长出第三项
                    store.set_layout_sizes(columns, cx);
                });
            });

        // 左侧窄边条(ActivityBar):折叠中间栏 / AI 历史 / 用量统计 / 设置。
        //
        // 尺寸与配色照抄 `src/components/ActivityBar.tsx`(44px 宽、32px 方按钮、
        // 18px 图标、激活态左侧 accent 竖条);图形是原版那几条 SVG path 的
        // 逐点搬运,见 [`activity_bar`] 模块注释(以及「为什么不用 IconName」)。
        // SSH / 移动端 / Git / 更新提醒四个入口 GPUI 侧还没有功能,**不放占位**。
        let toggle_strip = div()
            .flex_none()
            .w(px(activity_bar::WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(4.0))
            .py(px(8.0))
            .bg(ui::bg_surface())
            .border_r_1()
            .border_color(ui::border_subtle())
            .child(
                activity_bar::strip_button(
                    "toggle-middle",
                    activity_bar::PANEL,
                    if middle_visible {
                        t("app", "activityBar.collapse")
                    } else {
                        t("app", "activityBar.expand")
                    },
                    middle_visible,
                )
                // 全局 AI 状态徽标挂在这颗按钮上(中间栏承载项目列表)。
                // 口径与原版一致:只反映 AI 状态,**error 不往上冒** ——
                // 某个 shell `exit 1` 不该让整条边栏亮红点、盖住真在跑的 AI。
                .when(global_status != crate::tree::PaneStatus::Idle, |el| {
                    el.child(
                        div()
                            .absolute()
                            .top(px(-1.0))
                            .right(px(-1.0))
                            .w(px(8.0))
                            .h(px(8.0))
                            .rounded_full()
                            .border_1()
                            .border_color(ui::bg_surface())
                            .bg(ui::status_color(global_status)),
                    )
                })
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.store.update(cx, |store, cx| store.toggle_middle_column(cx));
                })),
            )
            .child(
                activity_bar::strip_button(
                    "toggle-sessions",
                    activity_bar::SESSIONS,
                    t("app", "activityBar.sessions"),
                    self.right_drawer == Some(DrawerPanel::Sessions),
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.toggle_drawer(DrawerPanel::Sessions, cx)
                })),
            )
            // Git 变更抽屉。位置照原版:紧跟 Sessions、排在分隔线之前
            // (`ActivityBar.tsx:143-150`)。
            .child(
                activity_bar::strip_button(
                    "toggle-git",
                    activity_bar::GIT,
                    t("app", "activityBar.git"),
                    self.right_drawer == Some(DrawerPanel::Git),
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.toggle_drawer(DrawerPanel::Git, cx)
                })),
            )
            .child(activity_bar::divider())
            .child(
                activity_bar::strip_button(
                    "toggle-usage",
                    activity_bar::STATS,
                    t("app", "activityBar.stats"),
                    self.usage_open,
                )
                .on_click(cx.listener(|this, _event, window, cx| this.toggle_usage(window, cx))),
            )
            // 「移动端」面板。位置照原版排在「设置」之前(`ActivityBar.tsx:167-170`)
            .child(
                activity_bar::strip_button(
                    "open-mobile-relay",
                    activity_bar::MOBILE,
                    t("app", "activityBar.mobile"),
                    false,
                )
                .on_click(cx.listener(|_this, _event, window, cx| {
                    mobile_panel::open(window, cx);
                })),
            )
            .child(
                activity_bar::strip_button(
                    "open-settings",
                    activity_bar::SETTINGS,
                    t("app", "activityBar.settings"),
                    false,
                )
                .on_click(cx.listener(|this, _event, window, cx| {
                    settings::open_settings(this.store.clone(), None, window, cx);
                })),
            )
            // 未读完成计数:点一下跳到最先完成的那个 pane(旧版托盘绿灯的入口;
            // 原版边栏没有这颗按钮,所以借状态灯的「实心圆 + 勾」当图形)
            .when(unread > 0, |el| {
                el.child(
                    div()
                        .id("jump-attention")
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(32.0))
                        .h(px(32.0))
                        .flex_none()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .hover(|el| el.bg(ui::border_subtle()))
                        .tooltip(move |window, cx| {
                            Tooltip::new(t("app", "titleBar.status.done")).build(window, cx)
                        })
                        .child(
                            StatusDot::new("strip-unread-done", StatusKind::AiIdle)
                                .size(px(14.0))
                                .color(ui::color_success())
                                .contrast(ui::bg_surface()),
                        )
                        .on_click(cx.listener(|this, _event, window, cx| {
                            this.on_jump_attention(&JumpAttention, window, cx);
                        })),
                )
            });

        // 右侧悬浮抽屉(Sessions ⇄ Git):**absolute 悬浮层**,贴右边缘盖在终端之上。
        //
        // 原版就是这个形态(`RightDrawer.tsx:67`:`absolute top-0 right-0 h-full
        // z-[45]`),GPUI 侧此前借 resizable 的第三栏实现,代价是**开合会改终端
        // 宽度、连带触发一次 PTY resize**(刷屏进程正在跑时肉眼可见地重排)。
        // 改成悬浮层之后终端尺寸不动,PTY 也就不再收到 SIGWINCH。
        //
        // 层级对照:原版 `z-45` 压过 allotment 分隔条(35)、低于弹窗(50)——
        // 这里 `.children(...)` 的顺序等价:抽屉排在三栏之后、弹窗/菜单层之前。
        // 抽屉**不进 [`overlay`] 栈**(原版 `RightDrawer` 同样没压栈),
        // 所以它开着时全局快捷键照常生效。
        //
        // 动画三条(`styles.css:284-313`):
        // - 进场 `drawerSlideIn` 240ms:整层从 `translateX(100%)` 滑进来 ——
        //   gpui 没有 transform,改成把 `right` 从 `-width` 补到 0(等效);
        // - 退场 `drawerSlideOut` 140ms:反过来,期间**面板实体仍留在树上**
        //   (`drawer_exit` 驻留 400ms),否则内容会先空掉;
        // - 换面板 `panelSwapIn` 200ms:内容层的 `ElementId` 带面板名,
        //   换面板即换 id → 动画重播(等价于原版 `key={panel}` 的重建)。
        //
        // ⚠️ 这三条在原版被**显式豁免** `prefers-reduced-motion`
        // (`styles.css:424-451`),所以 GPUI 侧不加任何减弱动效判定,始终播放。
        let exiting = self.drawer_exit.as_ref().map(|e| e.panel);
        let drawer_layer = self.right_drawer.or(exiting).map(|panel| {
            let leaving = self.right_drawer.is_none();
            let width = drawer_width as f32;
            let content: gpui::AnyElement = match panel {
                DrawerPanel::Sessions => self.session_panel.clone().into_any_element(),
                DrawerPanel::Git => self.git_panel.clone().into_any_element(),
            };
            div()
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .w(px(width))
                .occlude()
                .flex()
                .flex_col()
                .bg(ui::bg_overlay())
                .border_l_1()
                .border_color(ui::border_default())
                // `--shadow-overlay`(`RightDrawer.tsx:67`);gpui 侧用同一档
                // 阴影,与 `menu.rs` 的浮层一致
                .shadow_lg()
                .child(self.render_drawer_header(panel, cx))
                .child(
                    div()
                        // `key={panel}` 的对应物:换面板时这层换 id → 重建 → 动画重播
                        .id(SharedString::from(format!("drawer-body-{}", panel.key())))
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_hidden()
                        .child(content)
                        .with_animation(
                            SharedString::from(format!("panel-swap-{}", panel.key())),
                            gpui::Animation::new(std::time::Duration::from_millis(
                                MOTION_PANEL_SWAP_MS,
                            ))
                            .with_easing(ui::cubic_bezier(0.16, 1.0, 0.3, 1.0)),
                            // `panelSwapIn`:opacity 0→1 且 translateX(10px)→0
                            |el, delta| el.opacity(delta).ml(px(10.0 * (1.0 - delta))),
                        ),
                )
                // 左缘拖拽手柄:抽屉贴右边缘,把左缘往左拖 = 变宽,
                // 所以位移取 `start_x - 当前 x`(照抄 `RightDrawer.tsx:48`)
                .child(
                    div()
                        .id("drawer-resize-handle")
                        .absolute()
                        // 原版用 `-translate-x-1/2` 骑在边缘上;gpui 侧整条留在
                        // 抽屉内(压着左边框那 6px),免得被父级裁掉
                        .left_0()
                        .top_0()
                        .h_full()
                        .w(px(6.0))
                        .cursor_col_resize()
                        .hover(|el| el.bg(ui::with_alpha(ui::accent(), 0.4)))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this: &mut Self, event: &gpui::MouseDownEvent, _window, cx| {
                                cx.stop_propagation();
                                this.drawer_drag = Some(DrawerDrag {
                                    start_x: event.position.x,
                                    start_width: drawer_width,
                                    width: drawer_width,
                                });
                            }),
                        ),
                )
                .with_animation(
                    SharedString::from(if leaving {
                        "drawer-slide-out"
                    } else {
                        "drawer-slide-in"
                    }),
                    gpui::Animation::new(std::time::Duration::from_millis(if leaving {
                        MOTION_OVERLAY_OUT_MS
                    } else {
                        MOTION_OVERLAY_IN_MS
                    }))
                    .with_easing(if leaving {
                        ui::cubic_bezier(0.4, 0.0, 0.9, 0.6)
                    } else {
                        ui::cubic_bezier(0.16, 1.0, 0.3, 1.0)
                    }),
                    move |el, delta| {
                        let offset = if leaving { delta } else { 1.0 - delta };
                        el.right(px(-width * offset))
                    },
                )
        });

        let usage_layer = self.usage_panel.clone().filter(|_| self.usage_open).map(|panel| {
            div()
                .absolute()
                .top(px(24.0))
                .left(px(60.0))
                .right(px(60.0))
                .bottom(px(24.0))
                .occlude()
                .rounded(px(6.0))
                .border_1()
                .border_color(ui::border_default())
                .overflow_hidden()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(12.0))
                        .py(px(6.0))
                        .bg(ui::bg_elevated())
                        .child(
                            div()
                                .text_size(crate::ui::font_px(12.0))
                                .text_color(ui::text_primary())
                                .child(t("usageStats", "title")),
                        )
                        .child(
                            div()
                                .id("usage-close")
                                .px(px(6.0))
                                .text_color(ui::text_muted())
                                .cursor_pointer()
                                .hover(|el| el.text_color(ui::color_error()))
                                // 走 toggle 而不是直接改标志位 —— 可见性要透给
                                // 面板,否则关掉之后自动刷新定时器还在每 5s 跑
                                .on_click(cx.listener(|this, _event, window, cx| {
                                    if this.usage_open {
                                        this.toggle_usage(window, cx);
                                    }
                                }))
                                .child("×"),
                        ),
                )
                .child(div().flex_1().overflow_hidden().child(panel))
        });

        // 三栏 + 悬浮抽屉/浮层的那一层。标题栏之下的**全部**内容都在这里 ——
        // 原版同款(`App.tsx:478` 的 `flex-1 overflow-hidden flex`),抽屉与用量
        // 面板的 `absolute` 于是不会盖到标题栏上。
        let body = div()
            .flex_1()
            .overflow_hidden()
            .relative()
            .flex()
            .child(toggle_strip)
            .child(div().flex_1().h_full().child(columns_group))
            .children(drawer_layer)
            .children(usage_layer)
            // 自建 toast 层。挂在 `body`(它是 `relative`)里而不是根上 ——
            // 原版 `.toast-stack` 贴的是视口右下角(`fixed right:16 bottom:16`),
            // 标题栏在上面本来就碰不到它;挂进 body 后底边就是窗口底边,等价。
            // 排在抽屉与用量面板**之后** = 画在它们之上,对应原版 `z-index:70`
            // (浮层 50 / 分隔条 35)。
            //
            // S 批记档的「gpui-component 是右上角起堆」这条差异到此为止:自建层
            // 按原版右下角起堆。`render_notification_layer` 留着不动(组件库内部
            // 别处可能还用),只是 mt-app 不再往它里面推东西。
            .child(self.toast_layer.clone())
            .children(Root::render_notification_layer(window, cx));

        div()
            .size_full()
            .relative()
            .flex()
            // 标题栏是根 flex-col 的**首个** child(`App.tsx:474-478`)。
            // ⚠️ 它**不受配置加载失败门控** —— 配置读不出来时用户也得有地方把
            // 窗口关掉(原版那句原注释)。GPUI 侧配置目录不可用时压根开不出窗口
            // (`main()` 里直接 return),这条门控在这边只剩语义上的对齐。
            .flex_col()
            .bg(ui::bg_base())
            .text_color(ui::text_primary())
            // 界面字族(`config.uiFontFamily`)。gpui 的 `font_family` 会**继承**给
            // 所有没自己设过字族的子元素 —— 等价于原版把它写进 `--app-font-family`
            // 这个 CSS 变量,一处替换全局跟随。字号那一路走 `ui::font_px`。
            .when_some(ui::ui_font_family(), |el, family| el.font_family(family))
            // 主题包背景图:**窗口级**铺一张,与原版挂 `#root` 同位置 ——
            // 三栏都透着同一张图(面板底色带 surface_opacity、终端「默认背景不发
            // quad」,两条一起让图透上来)。
            //
            // ⚠️ 与 `TerminalView::set_background_art` 的逐终端那一路**二选一**:
            // 同时开等于同一块像素画两遍图、两层纱罩把 dim 平方。逐终端那路
            // 从没接过线(`pane.rs` 不调 `set_background_art`),这里是唯一一处。
            .when_some(background, |el, art| {
                el.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(mt_ui::background_art(art)),
                )
            })
            .key_context("Workspace")
            .on_action(cx.listener(Self::on_new_terminal))
            .on_action(cx.listener(Self::on_close_pane))
            .on_action(cx.listener(Self::on_split_right))
            .on_action(cx.listener(Self::on_split_down))
            .on_action(cx.listener(Self::on_next_pane))
            .on_action(cx.listener(Self::on_prev_pane))
            .on_action(cx.listener(Self::on_select_pane))
            .on_action(cx.listener(Self::on_toggle_middle))
            .on_action(cx.listener(Self::on_rename_pane))
            .on_action(cx.listener(Self::on_open_settings))
            .on_action(cx.listener(Self::on_toggle_sessions))
            .on_action(cx.listener(Self::on_toggle_usage))
            .on_action(cx.listener(Self::on_jump_attention))
            .on_action(cx.listener(Self::on_focus_left))
            .on_action(cx.listener(Self::on_focus_right))
            .on_action(cx.listener(Self::on_focus_up))
            .on_action(cx.listener(Self::on_focus_down))
            .on_action(cx.listener(Self::on_terminal_search))
            .on_action(cx.listener(Self::on_global_search))
            .on_action(cx.listener(Self::on_switch_project))
            .on_action(cx.listener(Self::on_marker_prev))
            .on_action(cx.listener(Self::on_marker_next))
            // 拖拽期间鼠标可能划出手柄(甚至划过终端),所以移动/松手挂在**根**上
            // —— 等价于原版往 document 上挂 mousemove/mouseup
            .when(self.drawer_drag.is_some(), |el| {
                el.on_mouse_move(cx.listener(|this: &mut Self, event: &gpui::MouseMoveEvent, _window, cx| {
                    if let Some(drag) = this.drawer_drag.as_mut() {
                        let delta = f32::from(drag.start_x - event.position.x) as f64;
                        drag.width = (drag.start_width + delta).clamp(240.0, 720.0);
                        cx.notify();
                    }
                }))
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(|this: &mut Self, _event: &gpui::MouseUpEvent, _window, cx| {
                        let Some(drag) = this.drawer_drag.take() else {
                            return;
                        };
                        this.store
                            .update(cx, |store, cx| store.set_right_drawer_width(drag.width, cx));
                        cx.notify();
                    }),
                )
            })
            .child(self.title_bar.clone())
            .child(body)
            // Modal 由 Root 持有,但要由应用视图**画出来**。
            // (它自己走 `anchored()` 定在视口中央,挂哪一层都一样;
            //  通知层不同 —— 见 `body` 那边的注释。)
            .children(Root::render_dialog_layer(window, cx))
            // 右键菜单层。零尺寸的绝对定位壳子 —— 菜单自己走 anchored(窗口坐标)
            // + deferred,不参与这里的 flex 布局,收着的时候一个像素都不占。
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w_0()
                    .h_0()
                    .child(self.menu_layer.clone()),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);
        // 右键菜单层的状态是全局的(项目列表 / 文件树 / tab / 终端四处都要弹),
        // 必须早于任何视图建出来 —— 视图的右键回调里直接取它。
        menu::init(cx);
        // toast 层同理,而且要更早一档:启动补 PTY(`hydrate_project`)就可能推
        // 一条 WSL 提示,那发生在窗口打开**之前**。
        toast::init(cx);
        // 粘贴转存的临时文件清理(24h),启动时跑一次。丢后台线程:它要 stat
        // 整个目录,不该占住首帧(装机版是在 Rust 侧 setup 里同步跑的)。
        cx.background_executor()
            .spawn(async { clipboard::cleanup_old_files() })
            .detach();
        // 真正的主题在 store 装好之后按 config 装配(`apply_theme_from_config`):
        // 亮/暗/auto + 外置主题包 + 终端配色一次算全。这里先钉一个暗色兜底,
        // 免得从 init 到装配之间有一帧走 gpui-component 的默认亮色。
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);

        // 键位表的唯一事实来源在 [`hotkeys`](crate::hotkeys) —— 它同时喂给
        // `bind_keys` 与设置面板的「快捷键」页,重演原版 `src/utils/hotkeys.ts`
        // 的结构(此前这里是一串裸 `KeyBinding::new`,与设置页各写各的会漂移)。
        hotkeys::bind_keys(cx);

        let config_store = if std::env::var_os("MT_APP_DATA_DIR").is_some() {
            // 隔离模式:配置也落在覆盖目录里,不碰装机版那份
            Arc::new(mt_config::ConfigStore::at(
                app_data_dir().join("config.json"),
            ))
        } else {
            match mt_config::ConfigStore::open() {
                Ok(store) => Arc::new(store),
                Err(err) => {
                    eprintln!("[app] 配置目录不可用: {err:#}");
                    return;
                }
            }
        };
        // 界面语言必须在**任何视图建出来之前**定下来:`t()` 读的是进程级全局量,
        // 晚一步的话首帧会以默认中文画出来再被刷成英文(闪一下)。
        // 首启没有 config.locale 时按系统语言探测,探测结果不落盘 —— 与 TS 侧
        // `detectInitialLang()` 一致,用户没显式选过就一直跟随系统。
        let startup_config = config_store.read();
        i18n::install(startup_config.locale.as_deref());

        // hook 开关取自配置(与装机版同一字段);start_hook_server 的数据目录统一
        // 走 mt_config::app_data_dir(),端口文件与装机版落在同一处。
        let hook_enabled = startup_config.hook_enabled;
        let (ai_bridge, ai_events) = AiBridge::new(hook_enabled);
        let ai_for_quit = ai_bridge.clone();

        AppStore::set_global(cx.new(|cx| AppStore::new(config_store, ai_bridge, cx)), cx);
        // 往后所有视图都从 Global 取这一份 store(等价于 zustand 的 useAppStore)
        let store = AppStore::global(cx);

        // 界面字号 / 字族同样要在**任何视图建出来之前**定下来:`ui::font_px` 读的是
        // 进程级快照,晚一步首帧会按默认 13px 画出来再被刷一遍(闪一下)。
        store.read(cx).apply_ui_font();

        // 主题必须在**起 PTY 之前**装配:新建终端拿的是 store 里那份终端配色,
        // 晚一步的话首批终端会以默认配色建出来,再被热更新刷一遍(闪一下)。
        // 窗口还没开,`window` 传 None —— Theme::change 只是少一次 refresh。
        store.update(cx, |store, cx| store.apply_theme_from_config(None, cx));

        // 启动即把当前项目的终端补起来(布局是从 config.json 恢复的,PTY 当然没了)
        let active = store.read(cx).active_project_id.clone();
        if let Some(project_id) = active {
            store.update(cx, |store, cx| store.hydrate_project(&project_id, cx));
        }

        // 退出前把配置刷下去(不等 500ms 防抖),顺手收掉 hook server 的端口文件
        let store_for_quit = store.clone();
        cx.on_app_quit(move |cx| {
            store_for_quit.update(cx, |store, _| store.save_config_now());
            ai_for_quit.shutdown();
            async {}
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    // 与装机版一致(`App.tsx` 的 `setTitle(\`Mini-Term v${ver}\`)`)——
                    // 窗口虽已无边框,任务栏悬停预览与 Alt+Tab 仍读这个标题
                    title: Some(format!("Mini-Term v{}", env!("CARGO_PKG_VERSION")).into()),
                    // **自绘标题栏的总开关**(Windows / macOS 都认)。Windows 侧映射成
                    // `hide_title_bar`,驱动 `WM_NCCALCSIZE` 吃掉系统 caption 高度、
                    // 并让 `WM_NCHITTEST` 去问 [`title_bar`] 登记的
                    // `WindowControlArea` hitbox。
                    //
                    // ⚠️ 不要碰 `WindowOptions::window_decorations` —— 那是 Wayland 专用
                    // (字段注释原文 "Wayland only"),Windows 上 `window_decorations()`
                    // 恒返回 `Server`。
                    appears_transparent: true,
                    // macOS 的交通灯落点(标题栏 32px 高,9,9 让三颗灯居中偏上)。
                    // 本仓主力 Windows,这行留着不亏 —— 那边三键根本不渲染。
                    traffic_light_position: Some(gpui::point(px(9.0), px(9.0))),
                }),
                ..Default::default()
            },
            |window, cx| {
                // 关窗确认(audit #30)。Windows 上标题栏 ✕ / Alt+F4 / 任务栏右键
                // 关闭全都走系统 `WM_CLOSE` → gpui 的 `handle_close_msg` → 这个回调,
                // 返回 false 就把这条消息吞掉。判定与 Linux 降级路径的 ✕ 共用同一道闸
                // (`title_bar::allow_close`),口径于是只有一份。
                //
                // ⚠️ 必须**同步**返回 bool,而确认框是异步的 —— 套路见 `title_bar`
                // 的「关窗确认」段注释。
                window.on_window_should_close(cx, title_bar::allow_close);
                // 窗口的第一层必须是 gpui_component::Root:Dialog / 通知 / Input
                // 的焦点登记都挂在它身上(Root::update 取不到就直接 panic)。
                let workspace = cx.new(|cx| Workspace::new(store, ai_events, window, cx));
                cx.new(|cx| Root::new(workspace, window, cx))
            },
        );
        if let Err(err) = window {
            eprintln!("打开窗口失败: {err:#}");
            return;
        }
        cx.activate(true);
    });
}
