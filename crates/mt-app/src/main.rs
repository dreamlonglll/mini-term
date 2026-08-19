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
mod file_tree;
mod focus_nav;
mod fs_ops;
mod i18n;
mod menu;
mod modal;
mod notify;
mod overlay;
mod pane;
mod pane_actions;
mod persist;
mod project_list;
mod project_switcher;
mod prompt;
mod search_modal;
mod session_panel;
mod shell_ops;
mod store;
mod terminal_area;
mod theme;
mod tree;
mod ui;
mod usage_panel;

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use gpui::{
    App, AppContext, Application, Bounds, Context, Entity, InteractiveElement, IntoElement,
    KeyBinding, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div,
    prelude::FluentBuilder, px, size,
};
use gpui_component::notification::Notification;
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_component::tooltip::Tooltip;
use gpui_component::{Root, WindowExt as _};
use mt_ui::icons::{StatusDot, StatusKind};

use crate::ai::AiBridge;
use crate::file_tree::FileTree;
use crate::focus_nav::Direction;
use crate::i18n::t;
use crate::notify::ToastKind;
use crate::project_list::ProjectList;
use crate::session_panel::SessionPanel;
use crate::store::{AppStore, PendingAlert};
use crate::terminal_area::TerminalArea;
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

/// toast 的去重键类型(gpui-component 按 `TypeId + key` 唯一化通知)。
/// 完成与待确认各一个键空间 —— 旧版同样把两种 toast 分开计数。
struct CompletionToast;
struct AttentionToast;

/// 三栏默认宽度(像素),与 `src/App.tsx` 的 Allotment 默认值一致。
const DEFAULT_COLUMNS: [f64; 2] = [520.0, 1000.0];
const DEFAULT_MIDDLE: [f64; 2] = [320.0, 380.0];

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

struct Workspace {
    store: Entity<AppStore>,
    /// 右键菜单浮层。状态住在全局(任何视图都能 `menu::show`),这里只是把它
    /// **画出来**的那个位置 —— 与 `Root::render_dialog_layer` 同一种分工。
    menu_layer: Entity<menu::ContextMenu>,
    project_list: Entity<ProjectList>,
    file_tree: Entity<FileTree>,
    terminal_area: Entity<TerminalArea>,
    session_panel: Entity<SessionPanel>,
    /// 用量面板惰性创建:它一开就跑账本同步,没打开过就不该有这笔开销。
    usage_panel: Option<Entity<UsagePanel>>,
    columns_state: Entity<ResizableState>,
    middle_state: Entity<ResizableState>,
    /// 右侧 AI 历史抽屉是否展开(运行时态,不持久化 —— 与旧版一致)。
    sessions_open: bool,
    usage_open: bool,
    _ai_pump: Task<()>,
    _activation: Subscription,
}

impl Workspace {
    fn new(
        store: Entity<AppStore>,
        ai_events: futures::channel::mpsc::UnboundedReceiver<ai::AiEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&store, |_, _, cx| cx.notify()).detach();

        let project_list = cx.new(|cx| ProjectList::new(store.clone(), cx));
        let file_tree = cx.new(|cx| FileTree::new(store.clone(), cx));
        let terminal_area = cx.new(|cx| TerminalArea::new(store.clone(), cx));
        let session_panel = cx.new(|cx| SessionPanel::new(store.clone(), cx));
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

        Self {
            store,
            menu_layer: menu::layer(cx),
            project_list,
            file_tree,
            terminal_area,
            session_panel,
            usage_panel: None,
            columns_state,
            middle_state,
            sessions_open: false,
            usage_open: false,
            _ai_pump: ai_pump,
            _activation: activation,
        }
    }

    /// 兑现一次提醒:提示音 / 任务栏闪烁 / toast。
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

        let store = self.store.clone();
        let project_id = alert.project_id.clone();
        let key = SharedString::from(alert.project_id.clone());
        // 点 toast = 跳到那个项目的待办 pane(旧版 toast 的同一行为)
        let on_click = move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
            let target = store.read(cx).next_attention_target(Some(&project_id));
            store.update(cx, |store, cx| {
                store.set_active_project(&project_id, cx);
                if let Some((pid, pane_id)) = target {
                    store.activate_pane(&pid, &pane_id, window, cx);
                }
            });
        };
        // 旧版 toast 是「项目名一行 + 描述一行」(`ToastContainer.tsx`),
        // gpui-component 的通知只有一段正文,拼成一行。
        let note = match kind {
            ToastKind::Completion => {
                Notification::success(format!(
                    "{} · {}",
                    alert.project_name,
                    t("toast", "aiDone")
                ))
                .id1::<CompletionToast>(key)
                .on_click(on_click)
            }
            ToastKind::Attention => {
                Notification::warning(format!(
                    "{} · {}",
                    alert.project_name,
                    t("toast", "aiAttention")
                ))
                .id1::<AttentionToast>(key)
                .on_click(on_click)
            }
        };
        window.push_notification(note, cx);
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
        modal::open_terminal_settings(self.store.clone(), window, cx);
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
        self.toggle_sessions(cx);
    }

    /// 开合 AI 历史抽屉。可见性要透给面板 —— 收着的时候它不该去扫会话
    /// (WSL 那一路会冷启动整台 VM)。
    fn toggle_sessions(&mut self, cx: &mut Context<Self>) {
        self.sessions_open = !self.sessions_open;
        let open = self.sessions_open;
        self.session_panel
            .update(cx, |panel, cx| panel.set_visible(open, cx));
        cx.notify();
    }

    fn on_toggle_usage(&mut self, _: &ToggleUsage, window: &mut Window, cx: &mut Context<Self>) {
        if yields_to_overlay(window, cx) {
            return;
        }
        self.toggle_usage(cx);
    }

    fn toggle_usage(&mut self, cx: &mut Context<Self>) {
        self.usage_open = !self.usage_open;
        if self.usage_open && self.usage_panel.is_none() {
            let store = self.store.clone();
            let dir = app_data_dir();
            self.usage_panel = Some(cx.new(|cx| UsagePanel::new(store, dir, cx)));
        }
        cx.notify();
    }

    /// 跳到「下一件该我做的事」(旧版点标题栏状态灯的动作)。
    fn on_jump_attention(
        &mut self,
        _: &JumpAttention,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if yields_to_overlay(window, cx) {
            return;
        }
        let Some((project_id, pane_id)) = self.store.read(cx).next_attention_target(None) else {
            return;
        };
        self.store.update(cx, |store, cx| {
            store.set_active_project(&project_id, cx);
            store.activate_pane(&project_id, &pane_id, window, cx);
            store.clear_unread_done(cx);
        });
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
        let sessions_open = self.sessions_open;

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
            // 右侧 AI 历史:与旧版的「悬浮抽屉」不同,这里是第三栏 —— 分隔条拖拽
            // 与宽度持久化跟着 resizable 白拿,不必自己写一套拖拽
            .child(
                resizable_panel()
                    .visible(self.sessions_open)
                    .size(px(drawer_width as f32))
                    .size_range(px(240.0)..px(720.0))
                    .child(self.session_panel.clone()),
            )
            .on_resize(move |state, _window, cx| {
                let sizes: Vec<f64> = state.read(cx).sizes().iter().map(|p| f32::from(*p) as f64).collect();
                store_for_columns.update(cx, |store, cx| {
                    // 折叠/收起的那一栏**不写回**:gpui-component 的
                    // `ResizableState` 按 children 个数占位,不可见的面板既不
                    // 渲染也不上报自己的尺寸,`sizes[i]` 停在建组时的最小值上。
                    // 照抄回去的话「收起中间栏后拖一下右边的分隔条」就把中间栏
                    // 宽度和抽屉宽度双双抹成最小值,再展开时只剩一条缝。
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
                    if sessions_open && let Some(w) = sizes.get(2) {
                        store.set_right_drawer_width(*w, cx);
                    }
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
                    self.sessions_open,
                )
                .on_click(cx.listener(|this, _event, _window, cx| this.toggle_sessions(cx))),
            )
            .child(activity_bar::divider())
            .child(
                activity_bar::strip_button(
                    "toggle-usage",
                    activity_bar::STATS,
                    t("app", "activityBar.stats"),
                    self.usage_open,
                )
                .on_click(cx.listener(|this, _event, _window, cx| this.toggle_usage(cx))),
            )
            .child(
                activity_bar::strip_button(
                    "open-settings",
                    activity_bar::SETTINGS,
                    t("app", "activityBar.settings"),
                    false,
                )
                .on_click(cx.listener(|this, _event, window, cx| {
                    modal::open_terminal_settings(this.store.clone(), window, cx);
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
                                .text_size(px(12.0))
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
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.usage_open = false;
                                    cx.notify();
                                }))
                                .child("×"),
                        ),
                )
                .child(div().flex_1().overflow_hidden().child(panel))
        });

        div()
            .size_full()
            .relative()
            .flex()
            .bg(ui::bg_base())
            .text_color(ui::text_primary())
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
            .child(toggle_strip)
            .child(div().flex_1().h_full().child(columns_group))
            .children(usage_layer)
            // Modal 与通知层由 Root 持有,但要由应用视图**画出来**
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
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
        // 真正的主题在 store 装好之后按 config 装配(`apply_theme_from_config`):
        // 亮/暗/auto + 外置主题包 + 终端配色一次算全。这里先钉一个暗色兜底,
        // 免得从 init 到装配之间有一帧走 gpui-component 的默认亮色。
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);

        // 键位表的唯一事实来源是 `src/utils/hotkeys.ts`,逐条对齐。
        //
        // 应用级动作统一走 `Ctrl+Shift+*`(Windows Terminal / VS Code 终端的惯例)——
        // 裸 Ctrl+B/Ctrl+T/Ctrl+W/Ctrl+P 在 shell 行编辑里都有既定语义,占了就是吞键;
        // 只有确实不冲突的才用裸 Ctrl(Ctrl+Tab、Ctrl+1..9、Ctrl+,)。
        //
        // gpui 的按键派发**先匹配 action 绑定、后跑 key 监听**,所以这里绑上就等于
        // 拿到了原版 capture 阶段 `consume(e)` 的效果:终端看不到这些键。
        let mut bindings = vec![
            KeyBinding::new("ctrl-shift-t", NewTerminal, Some("Workspace")),
            KeyBinding::new("ctrl-shift-w", ClosePane, Some("Workspace")),
            KeyBinding::new("ctrl-shift-d", SplitRight, Some("Workspace")),
            KeyBinding::new("ctrl-shift-e", SplitDown, Some("Workspace")),
            KeyBinding::new("ctrl-shift-b", ToggleMiddleColumn, Some("Workspace")),
            KeyBinding::new("f2", RenamePane, Some("Workspace")),
            KeyBinding::new("ctrl-,", OpenTerminalSettings, Some("Workspace")),
            KeyBinding::new("ctrl-tab", NextPane, Some("Workspace")),
            KeyBinding::new("ctrl-shift-tab", PrevPane, Some("Workspace")),
            // 搜索三连(原版 hotkeys.ts 的 terminalSearch / globalSearch / switchProject)。
            // 键位与原版逐条对齐:Ctrl+F 在终端里本是 `\x06`,原版同样把它整个吃掉
            // (`consume(e)` 连 stopPropagation 一起),这里靠 action 绑定天然做到。
            KeyBinding::new("ctrl-f", TerminalSearch, Some("Workspace")),
            KeyBinding::new("ctrl-shift-f", GlobalSearch, Some("Workspace")),
            KeyBinding::new("ctrl-shift-p", SwitchProject, Some("Workspace")),
            // 项目切换器的方向键。谓词写成 `"ProjectSwitcher > Input"` 而不是
            // `"ProjectSwitcher"` —— 只有与 `Input` **同深度**才压得过组件库自带的
            // `up`/`down`(单行输入框那两个处理器直接 return,不 propagate,
            // 挂在容器上的 on_key_down 永远收不到)。详见 project_switcher 模块注释,
            // 机制有单测钉住。
            KeyBinding::new(
                "up",
                project_switcher::SwitcherPrev,
                Some("ProjectSwitcher > Input"),
            ),
            KeyBinding::new(
                "down",
                project_switcher::SwitcherNext,
                Some("ProjectSwitcher > Input"),
            ),
            // 原版没有的三条,保留
            KeyBinding::new("ctrl-shift-a", ToggleSessions, Some("Workspace")),
            KeyBinding::new("ctrl-shift-u", ToggleUsage, Some("Workspace")),
            KeyBinding::new("ctrl-shift-j", JumpAttention, Some("Workspace")),
            KeyBinding::new("alt-left", FocusLeft, Some("Workspace")),
            KeyBinding::new("alt-right", FocusRight, Some("Workspace")),
            KeyBinding::new("alt-up", FocusUp, Some("Workspace")),
            KeyBinding::new("alt-down", FocusDown, Some("Workspace")),
        ];
        // Ctrl+1..9:选中叶内第 N 个 tab(原版键位表里存的是占位串 '1…9',
        // 由 useGlobalHotkeys 单独判 `/^[1-9]$/`)
        for n in 1..=9usize {
            bindings.push(KeyBinding::new(
                &format!("ctrl-{n}"),
                SelectPane(n),
                Some("Workspace"),
            ));
        }
        cx.bind_keys(bindings);

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
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
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
