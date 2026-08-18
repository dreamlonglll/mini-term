//! mini-term 的 GPUI 应用壳。
//!
//! # 组件树
//!
//! ```text
//! Workspace(根视图,持有 AppStore 与三个栏目视图)
//!  └─ h_resizable "columns"                      ← 替 Allotment 外层
//!      ├─ panel(可折叠,宽度落 config.layoutSizes[0])
//!      │   └─ h_resizable "middle"               ← 替 Allotment 内层
//!      │       ├─ ProjectList                    ← 项目列表
//!      │       └─ FileTree                       ← 文件树
//!      └─ panel
//!          └─ TerminalArea                       ← SplitNode 树 → 嵌套 resizable
//!              └─ (leaf) tab 栏 + TerminalPane 实体
//! ```
//!
//! # 事件流
//!
//! ```text
//! 用户键入 → TerminalPane::write → AiPerception::observe_input → PtySession::write
//! PTY reader 线程 → TerminalEmulator::advance + observe_output → 唤醒 channel → 重绘
//! hook / 500ms 轮询 → StatusSink → channel → Workspace 的前台任务 → AppStore → notify
//! 布局/配置变化 → AppStore::save_config_soon(500ms 防抖)→ ConfigStore::save(带令牌)
//! ```
//!
//! 状态形状与操作语义对照 `src/store.ts`,见 [`store`] 与 [`tree`] 两个模块的注释。

mod ai;
mod file_tree;
mod pane;
mod persist;
mod project_list;
mod store;
mod terminal_area;
mod tree;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use gpui::{
    App, AppContext, Application, Bounds, Context, Entity, InteractiveElement, IntoElement,
    KeyBinding, ParentElement, Render, StatefulInteractiveElement, Styled, Task, TitlebarOptions,
    Window, WindowBounds, WindowOptions, actions, div, px, size,
};
use gpui_component::resizable::{ResizableState, h_resizable, resizable_panel};

use crate::ai::AiBridge;
use crate::file_tree::FileTree;
use crate::project_list::ProjectList;
use crate::store::AppStore;
use crate::terminal_area::TerminalArea;
use crate::tree::SplitDirection;

actions!(
    mini_term,
    [
        /// 新建终端标签(Ctrl+Shift+T)
        NewTerminal,
        /// 关闭当前 pane(Ctrl+Shift+W)
        ClosePane,
        /// 向右分屏(Ctrl+Shift+D)
        SplitRight,
        /// 向下分屏(Ctrl+Shift+E)
        SplitDown,
        /// 折叠/展开中间栏(Ctrl+B)
        ToggleMiddleColumn,
    ]
);

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
    project_list: Entity<ProjectList>,
    file_tree: Entity<FileTree>,
    terminal_area: Entity<TerminalArea>,
    columns_state: Entity<ResizableState>,
    middle_state: Entity<ResizableState>,
    _ai_pump: Task<()>,
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
        let columns_state = cx.new(|_| ResizableState::default());
        let middle_state = cx.new(|_| ResizableState::default());

        // AI 状态泵:后台线程(hook server / 500ms 轮询)→ channel → 这里改 store。
        let ai_store = store.clone();
        let mut ai_events = ai_events;
        let ai_pump = cx.spawn(async move |_this, cx| {
            while let Some(event) = ai_events.next().await {
                if ai_store
                    .update(cx, |store, cx| store.apply_ai_event(event, cx))
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
            project_list,
            file_tree,
            terminal_area,
            columns_state,
            middle_state,
            _ai_pump: ai_pump,
        }
    }

    /// 当前该操作哪个 pane:焦点 pane,没有就落到布局里第一个激活 pane。
    fn target_pane(&self, cx: &App) -> Option<(String, String)> {
        let store = self.store.read(cx);
        let project_id = store.active_project_id.clone()?;
        let layout = store.active_layout()?;
        let pane_id = store
            .focused_pane_id
            .clone()
            .filter(|id| layout.pane(id).is_some())
            .or_else(|| layout.first_active_pane().map(|p| p.id.clone()))?;
        Some((project_id, pane_id))
    }

    fn on_new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.store.read(cx).active_project_id.clone() else {
            return;
        };
        let anchor = self.target_pane(cx).map(|(_, pane)| pane);
        self.store.update(cx, |store, cx| {
            store.new_terminal(&project_id, None, anchor, window, cx);
        });
    }

    fn on_close_pane(&mut self, _: &ClosePane, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        self.store
            .update(cx, |store, cx| store.close_pane(&project_id, &pane_id, cx));
    }

    fn on_split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        self.split(SplitDirection::Horizontal, window, cx);
    }

    fn on_split_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
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
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.store.update(cx, |store, cx| store.toggle_middle_column(cx));
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (columns, middle, middle_visible) = {
            let config = self.store.read(cx).config();
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
            (columns, middle, config.middle_column_visible)
        };

        let store_for_columns = self.store.clone();
        let store_for_middle = self.store.clone();

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
                store_for_columns.update(cx, |store, cx| store.set_layout_sizes(sizes, cx));
            });

        // 中间栏的折叠开关。Ctrl+B 也能切,但只有快捷键的话这个功能等于不存在 ——
        // 折叠后左边留一条窄边条,是唯一能把它请回来的可见入口。
        let toggle_strip = div()
            .id("toggle-middle")
            .flex_none()
            .w(px(14.0))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(ui::bg_surface())
            .border_r_1()
            .border_color(ui::border_subtle())
            .text_size(px(10.0))
            .text_color(ui::text_muted())
            .cursor_pointer()
            .hover(|el| el.text_color(ui::accent()))
            .on_click(cx.listener(|this, _event, _window, cx| {
                this.store.update(cx, |store, cx| store.toggle_middle_column(cx));
            }))
            .child(if middle_visible { "‹" } else { "›" });

        div()
            .size_full()
            .flex()
            .bg(ui::bg_base())
            .text_color(ui::text_primary())
            .key_context("Workspace")
            .on_action(cx.listener(Self::on_new_terminal))
            .on_action(cx.listener(Self::on_close_pane))
            .on_action(cx.listener(Self::on_split_right))
            .on_action(cx.listener(Self::on_split_down))
            .on_action(cx.listener(Self::on_toggle_middle))
            .child(toggle_strip)
            .child(div().flex_1().h_full().child(columns_group))
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);

        cx.bind_keys([
            KeyBinding::new("ctrl-shift-t", NewTerminal, Some("Workspace")),
            KeyBinding::new("ctrl-shift-w", ClosePane, Some("Workspace")),
            KeyBinding::new("ctrl-shift-d", SplitRight, Some("Workspace")),
            KeyBinding::new("ctrl-shift-e", SplitDown, Some("Workspace")),
            KeyBinding::new("ctrl-b", ToggleMiddleColumn, Some("Workspace")),
        ]);

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
        // hook 开关取自配置(与装机版同一字段);start_hook_server 的数据目录统一
        // 走 mt_config::app_data_dir(),端口文件与装机版落在同一处。
        let hook_enabled = config_store.read().hook_enabled;
        let (ai_bridge, ai_events) = AiBridge::new(hook_enabled);
        let ai_for_quit = ai_bridge.clone();

        AppStore::set_global(cx.new(|cx| AppStore::new(config_store, ai_bridge, cx)), cx);
        // 往后所有视图都从 Global 取这一份 store(等价于 zustand 的 useAppStore)
        let store = AppStore::global(cx);

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
                    title: Some("mini-term".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Workspace::new(store, ai_events, window, cx)),
        );
        if let Err(err) = window {
            eprintln!("打开窗口失败: {err:#}");
            return;
        }
        cx.activate(true);
    });
}
