// release 构建在 Windows 上走 GUI 子系统:console 子系统的 exe 从快捷方式/Explorer 启动
// 会被 Windows 新开一个控制台窗口滚启动日志(装机版即此形态)。debug 不挂,保留 console
// 让 cargo run 的日志照常附着当前终端。GUI 子系统下 println!/eprintln! 本会被 std 静默
// 丢弃,`logfile::install`(main 第一行)把它们接到 `{数据目录}/mini-term.log`。
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

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
//!      │   │   └─ v_resizable "middle"               ← 替 Allotment 内层(vertical)
//!      │   │       ├─ ProjectList                    ← 项目列表(上)
//!      │   │       └─ FileTree                       ← 文件树(下)
//!      │   ├─ panel
//!      │   │   ├─ TerminalArea                       ← 活动面板的 SplitNode 树 → 嵌套 resizable
//!      │   │   │   └─ (leaf) tab 栏 + TerminalPane 实体
//!      │   │   └─ TerminalsPanel                     ← 项目级面板切换竖条(44px 图标列,可开合)
//!      │   └─ panel(可折叠)
//!      │       └─ SessionPanel                       ← AI 历史(右侧抽屉)
//!      ├─ UsagePanel(浮层)                           ← 用量统计
//!      ├─ dialog_host::render_layer                  ← 各类 Modal(自建弹窗外壳)
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
//! 配置变化 → AppStore::save_config_soon(500ms 防抖)→ ConfigStore::save(带令牌,写 config.db)
//! 布局变化 → AppStore::save_layout_soon(300ms 防抖)→ LayoutStore(写 layout.db)
//! ```
//!
//! 两条落盘路径是分开的:布局是交互频次的数据(拖分隔条/开关终端),配置是月级的,
//! 共用一个信封时改一次布局要把整份配置重写一遍。详见 `mt-layout` 与
//! `mt_config::db` 的模块注释。
//!
//! 状态形状与操作语义对照 `src/store.ts`,见 [`store`] 与 [`tree`] 两个模块的注释。

mod activity_bar;
mod branch_family;
mod clipboard;
mod command_library;
mod date_picker;
mod dialog_host;
mod dnd;
mod env_vars;
mod file_tree;
mod file_viewer;
mod first_run;
mod fold_motion;
// dev-only 度量工具(gpui-fast 内置帧剖析叠层)。默认不编译:整个模块连同
// `open_window` 之后那唯一一处调用都挂在 `frame-profiler` feature 下,
// 正式版零代码零依赖。用法见该模块的注释。
#[cfg(feature = "frame-profiler")]
mod frame_profiler;
mod frost;
mod fs_ops;
mod git;
mod height_reveal;
mod hook_hint;
mod hotkeys;
mod http;
mod i18n;
mod image_lightbox;
mod logfile;
mod menu;
mod mermaid_compat;
mod mobile_panel;
mod mobile_relay;
mod modal;
mod motion;
mod motion_kit;
mod native_view;
mod notify;
mod overlay;
mod overlay_motion;
mod pane;
mod pane_actions;
mod pane_preview;
mod popover_reveal;
mod pricing;
mod project_list;
mod project_switcher;
mod prompt;
mod quick_open;
mod redraw;
mod remote_directory_picker;
mod remote_notify;
mod remote_project;
mod search_modal;
mod secrets;
mod segment_slider;
mod session_panel;
mod settings;
mod ssh_activity;
mod ssh_assoc;
mod ssh_panel;
mod ssh_process;
mod ssh_quick_connect;
mod store;
mod syntax_languages;
mod tab_expansion;
mod terminal_area;
mod terminal_links;
mod terminals_panel;
mod theme;
mod title_bar;
mod toast;
mod tray;
mod ui;
mod update_check;
mod update_dialog;
mod usage_panel;
mod workbench_area;
mod workspace;

// 下沉到非 GPUI 应用域核心 crate(`mt-app-core`)的模块，在壳层原路径再导出以保持调用点稳定。
pub(crate) use mt_app_core::{
    ai, cli, file_ops, focus_nav, kitty_graphics, markers, osc7501, persist, project_tree,
    pty_slot, session_branch, shell_ops, ssh_conn, startup_trace, tree,
};

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext, Styled as _, TitlebarOptions, WindowOptions, actions, px};
use gpui_component::Root;

use crate::ai::AiBridge;
use crate::i18n::t;
pub(crate) use crate::i18n::tr;
use crate::store::AppStore;
use crate::workspace::{Workspace, restore_window_bounds};

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
        /// 叶内切到下一个 tab(Ctrl+Tab);组内只有一个 tab 时切到下一个分组
        NextPane,
        /// 叶内切到上一个 tab(Ctrl+Shift+Tab);组内只有一个 tab 时切到上一个分组
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
        /// 快速打开文件(Ctrl+Shift+O,toggle)
        QuickOpenFile,
        /// 项目快速切换器(Ctrl+Shift+P)
        SwitchProject,
        /// 跳到上一个 AI 任务标记(Ctrl+Shift+↑)
        MarkerPrev,
        /// 跳到下一个 AI 任务标记(Ctrl+Shift+↓)
        MarkerNext,
        /// 开合命令库浮层(Ctrl+Shift+K,issue #81)
        ToggleCommandLibrary,
        /// 终端页 ↔ 最近停过的文件页签来回切(Ctrl+`)
        ToggleWorkbenchPage,
        /// 关闭当前**单个**终端页签(Ctrl+Shift+X;整组仍是 Ctrl+Shift+W)
        CloseTerminalTab,
        /// 最大化 / 还原当前分组(Ctrl+Shift+M)
        ToggleMaximizePane,
        /// 当前页签在组内左移(Ctrl+Shift+PageUp)
        MoveTabLeft,
        /// 当前页签在组内右移(Ctrl+Shift+PageDown)
        MoveTabRight,
        /// 当前分屏朝左扩(Alt+Shift+←)
        ResizeSplitLeft,
        /// 当前分屏朝右扩(Alt+Shift+→)
        ResizeSplitRight,
        /// 当前分屏朝上扩(Alt+Shift+↑)
        ResizeSplitUp,
        /// 当前分屏朝下扩(Alt+Shift+↓)
        ResizeSplitDown,
        /// 重新打开已关闭的终端(Ctrl+Shift+Z)
        ReopenClosedTerminal,
    ]
);

/// 选中叶内第 N 个 tab(Ctrl+1..9,**1-based**;越界不动)。
///
/// 带数据的 action 必须走 `derive(Action)`(`actions!` 只生成单元结构),
/// `no_json` 让它不要求 serde/schemars —— 这个 action 只从代码里绑,不进键位 JSON。
#[derive(Clone, PartialEq, Default, Debug, gpui::Action)]
#[action(namespace = mini_term, no_json)]
pub struct SelectPane(pub usize);
/// 应用数据目录:`config.db`(配置本体)、`layout.db`(界面布局)、
/// `config.json`(给 sidecar 读的 SSH 投影)、`hook-server.json` 的落点。
///
/// **开发用逃生门 `MT_APP_DATA_DIR`**:装机版正在跑的时候直接 `cargo run` 会与它
/// 共用同一个目录 —— 配置被两边轮流改写,hook 端口文件更是直接互抢(装机版占了
/// 23456,新起的这个退到 23457 并把端口文件覆盖成自己的)。设了这个环境变量就整
/// 个隔离出去,与 Tauri 那边靠 `--config` 覆盖 identifier 是同一招。
///
/// 判据本体在 [`mt_config::active_data_dir`](mt_config::paths::active_data_dir)
/// —— themes/ 也走同一口径(`mt_config::themes_dir`),这里只是它的「不返错」版本,
/// 两处各判一次环境变量的旧写法已收掉。
pub fn app_data_dir() -> PathBuf {
    mt_config::active_data_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 全局 panic 兜底:在默认 hook 之前补一行带**线程名**的 stderr,再附一份 backtrace。
///
/// 倒下的多半不是主线程 —— PTY reader、hook HTTP、500ms 轮询、mt-relay 的 tokio
/// 任务都在各自线程里跑,默认 hook 只打消息与位置,事后从用户贴来的日志里认不出
/// 是哪条线路。原 hook 链式调用在后,它自己的 backtrace 行为(RUST_BACKTRACE)不变。
///
/// backtrace **不看 RUST_BACKTRACE、无条件抓**(`force_capture`):装机版用户不会去设
/// 环境变量,而 panic 往往复现不了第二次。按完整格式(`{:#}`)打,每帧带绝对地址:
/// - 安装目录里有同版本的 `mini_term.pdb`(release 资产里单独下载)时,帧直接解析成
///   函数名 + file:line(release 编了行号表,见根 Cargo.toml 的 `debug`);
/// - 没有 PDB 时帧是 `<unknown>`,拿同一行里的模块基址换算成 RVA(地址 − 基址),
///   事后对着 PDB 离线解析。基址每次启动随 ASLR 变,所以必须和帧地址记在一起。
///
/// 抓栈 + 符号化在 panic 线程上同步做,有 PDB 时首次要加载它(一百多 MB),慢一点
/// 无妨:panic 本身就是稀有事件。
///
/// release 的 Windows GUI 子系统下 stderr 由 [`logfile::install`] 接到
/// `mini-term.log`(早于本钩子安装),装机版的 panic 于是也留档。
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        eprintln!(
            "[panic] thread={} at {}: {}",
            name,
            location,
            info.payload_as_str().unwrap_or("<non-string payload>")
        );
        let backtrace = std::backtrace::Backtrace::force_capture();
        eprintln!(
            "[panic] backtrace (thread={name}, module base {}):\n{backtrace:#}",
            module_base()
        );
        default_hook(info);
    }));
}

/// 主程序模块的加载基址(十六进制),给 backtrace 的离线符号化换算 RVA 用。
#[cfg(windows)]
fn module_base() -> String {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    // SAFETY: 传 None 取的是当前进程 exe 自身的模块句柄,不增引用计数、无需释放。
    match unsafe { GetModuleHandleW(None) } {
        Ok(module) => format!("{:#x}", module.0 as usize),
        Err(_) => "<unknown>".to_string(),
    }
}

/// 非 Windows:地址解析靠符号表(release 已剥)或 core dump,不在这条链路上。
#[cfg(not(windows))]
fn module_base() -> String {
    "<n/a>".to_string()
}

fn main() {
    // 卸载器调起的无窗口模式(`--unregister-hooks`,见 `cli` 模块注释):排在一切之前,
    // 做完即退 —— 不接管日志(那会在 AppData 下建数据目录)、不预载 ConPTY、不碰 gpui。
    if let Some(code) = cli::run_if_requested() {
        std::process::exit(code);
    }
    // 装机版没有控制台,stderr 先接到 `mini-term.log`(见 `logfile` 模块注释)。
    // 必须排在埋点之前:`startup_trace::init` 自己就要打第一条 `run() enter`。
    // 有控制台时它是空操作,dev 实例的日志照旧附着当前终端。
    logfile::install();
    // 紧随 stderr 重定向之后安装全局 tracing 订阅器:日志统一带级别 / 目标 / 时间戳
    // 写入 stderr(装机版自动落进 `mini-term.log`),支持 `RUST_LOG` 覆写。
    logfile::init_tracing();
    // 启动链路埋点的 T0。**紧随日志接管之后** —— 往后每个 `startup_trace::mark`
    // 打的都是相对这一刻的偏移(装机版 `lib.rs::run()` 同位置);上面那一步只是
    // 打开一个文件,毫秒级,不影响埋点的可比性。
    startup_trace::init();
    // 紧随其后装 panic 兜底:再往后的任何一行倒下都得留下可定位的一行日志。
    install_panic_hook();
    // 便携 ConPTY 预载(exe 旁 `portable-conpty\` 里的 Windows Terminal conpty.dll +
    // OpenConsole.exe;`MT_DISABLE_PORTABLE_CONPTY=1` 跳过,见 `mt_pty::conpty`)。
    //
    // ⚠️ 必须早于任何 PTY spawn:portable-pty 第一次 openpty 时按裸名解析
    // `conpty.dll`,那一刻已加载的模块才算数,之后再预载不影响已解析的函数表。
    // 放在这里 —— 主线程上同步做完、在 `application()` 建出平台层**之前** —— 就天然
    // 满足:PTY 一律在 background executor 上起(`pane` 模块注释「PTY 在后台起」),
    // 而 executor 的线程与任务都要等平台层建出来、`app.run` 的回调跑起来之后才有;
    // 最早的一批(`hydrate_project`)还排在首帧之后。
    // 线程创建本身是 happens-before 边,这里的 LoadLibrary 对它们一定可见。
    // 代价:读三个文件的 PE 头 + 一次 LoadLibrary,实测不到 1ms(下面的埋点可量)。
    mt_pty::conpty::initialize_default();
    startup_trace::mark("conpty bootstrap done");
    // 组件库的图标资产源。gpui 的 `svg()` 一律经 `AssetSource` 取字节,没挂资产源时
    // 上游组件里每一枚 `Icon::new(IconName::..)` 都画成**空白**(只在日志里留一行,
    // 编译期与运行期界面上都毫无提示)—— 0.5.1 时代 crate 包里根本不带 svg,于是
    // 全仓有十来处「只能自绘」的记档按着这条前提写;0.6.2 把图标拆进了
    // `gpui-kit-assets` 并给出现成的 `AssetSource`,前提不再成立。
    //
    // 用 `Assets`(组件库默认那 101 枚,约 44 KB)而不是 `AllAssets`(Lucide 全集
    // 1830 枚,约 731 KB):组件库自己只按名字取这 101 枚,全集是给应用画自己的
    // 图标用的,本仓的图标基建是 `mt_ui::icons` 那套自绘矢量(要多色,svg 单色
    // alpha 掩膜画不了),用不上。
    let app = gpui_platform::application().with_assets(gpui_kit_assets::Assets);
    app.run(|cx: &mut App| {
        startup_trace::mark("setup enter");
        gpui_component::init(cx);
        // 文件编辑器的补充语言包(C# 等五种补高亮查询 + PHP / Kotlin / Lua … 新增)。
        // 只是往注册表放字符串,毫秒级;必须早于任何文件页签建出来。
        syntax_languages::register();
        // 预览器里的图片要靠它取字节:gpui 默认装 `NullHttpClient`(什么都发不出),
        // 而富文本渲染器把图片一律画成 `img(SharedUri)` —— 本地图片走 `file://`
        // (md / html 源里的相对路径在渲染前被改写成绝对 file URL),网络图片走共享异步
        // `reqwest::Client`。细节与两条硬约束见 `file_viewer::PreviewHttpClient`。
        cx.set_http_client(Arc::new(file_viewer::PreviewHttpClient));
        // 右键菜单层的状态是全局的(项目列表 / 文件树 / tab / 终端四处都要弹),
        // 必须早于任何视图建出来 —— 视图的右键回调里直接取它。
        menu::init(cx);
        // toast 层同理:任何视图建出来之前就位。启动补 PTY(`hydrate_project`)
        // 可能推一条 WSL 提示 —— 它现在排在首帧之后,但 toast 层早就位一档不亏。
        toast::init(cx);
        // 粘贴转存的临时文件清理(24h),启动时跑一次。丢后台线程:它要 stat
        // 整个目录,不该占住首帧(装机版是在 Rust 侧 setup 里同步跑的)。
        cx.background_executor()
            .spawn(async { clipboard::cleanup_old_files() })
            .detach();
        // SSH 临时私钥副本的清理(装机版 `lib.rs::setup` 里的
        // `ssh::cleanup_ssh_temp_keys()`)。远程 pane 起 ssh 前会把私钥复制成
        // 权限收紧的临时文件(`mt_pty::prepare_ssh_key`),上一轮的副本必须在
        // 启动时清掉 —— 否则那个目录随连接次数无界增长,且留着别人机器上的
        // 私钥明文副本。同样丢后台:它要遍历目录。
        cx.background_executor()
            .spawn(async { mt_pty::cleanup_ssh_temp_keys() })
            .detach();
        // 已注册用户的启动期自愈,每家一条。装机版 `lib.rs::setup` 里原本也是
        // 一个后台线程按序跑这几条,GPUI 迁移时 hook_registry 逐字搬进了 mt-ai、
        // 这行调用漏搬,claude/grok 两条自 2026-08-20(b52a654 删 src-tauri)起
        // 一直没执行过 —— 补回来:
        // - claude: 补齐新版本新增的 hook 事件(事件集没长过就直接 return,不写盘)
        // - grok:   兼职把 `{grok_home}/hooks/` 里的 hook 二进制副本刷成当前版本
        //           (mini-term 升级后旧副本会滞留),故不设「没变化」短路
        // - codex:  把 config.toml 的 feature 键迁到现行名字(codex_hooks -> hooks),
        //           面板判「已注册」只看 hooks.json,界面上没有线索提示用户重点
        //           一次注册;键已是新名字就不落盘
        // - omp / pi:扩展文件与当前模板不同就整份重写(模板随版本演进)
        // - agy:    `~/.gemini/config/hooks.json` 里我们那一组缺事件、或命令里的 hook
        //           绝对路径与当前不一致(装机目录变了)时重写那一组;一致就不落盘
        // 每条都只在**已注册过**时才动手 —— 没开过这功能的用户一律不碰他的配置。
        // 都要读写用户主目录下的文件,故丢后台不挡启动。
        cx.background_executor()
            .spawn(async {
                mt_ai::hook_registry::sync_claude_hooks_if_registered();
                mt_ai::hook_registry::sync_grok_hooks_if_registered();
                mt_ai::hook_registry::sync_codex_hooks_feature_if_registered();
                mt_ai::hook_registry::sync_omp_hooks_if_registered();
                mt_ai::hook_registry::sync_pi_hooks_if_registered();
                mt_ai::hook_registry::sync_agy_hooks_if_registered();
            })
            .detach();
        // 真正的主题在 store 装好之后按 config 装配(`apply_theme_from_config`):
        // 亮/暗/auto + 外置主题包 + 终端配色一次算全。这里先钉一个暗色兜底,
        // 免得从 init 到装配之间有一帧走 gpui-component 的默认亮色。
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);

        // 「减少动画」也必须在**任何视图建出来之前**定下来:动画消费方读的是
        // 进程级闸(`mt_ui::motion`),晚一步的话首帧会按「允许动画」画出来 ——
        // 状态灯闪一下再停,正是这条设置想避免的东西。
        //
        // ⚠️ 顺序:必须排在上面那句 `gpui_component::init` **之后** —— 它还要
        // 接管 gpui 自己那道「一刀切停掉所有 `with_animation`」的闸,而接管的
        // 前提是组件库已经把系统值写过一遍(见 `motion::gate_values` 的注释)。
        motion::install(cx);

        // 终端内置的 Nerd Font 图标字体(issue #86):终端字体缺私用区图标时拿它补,
        // 不再画成方块。要早于任何终端出第一帧;失败只在日志里喊一声,图标退回旧样子。
        // 为什么不是往回退表里加个名字,见 `mt_ui::terminal::symbols` 的模块注释。
        mt_ui::terminal::install_symbol_font(cx);
        startup_trace::mark("symbol font registered");

        // 键位表的唯一事实来源在 [`hotkeys`](crate::hotkeys) —— 它同时喂给
        // `bind_keys` 与设置面板的「快捷键」页,重演原版 `src/utils/hotkeys.ts`
        // 的结构(此前这里是一串裸 `KeyBinding::new`,与设置页各写各的会漂移)。
        hotkeys::bind_keys(cx);
        startup_trace::mark("setup: init + cleanups done");

        let config_store = if std::env::var_os("MT_APP_DATA_DIR").is_some() {
            // 隔离模式:配置也落在覆盖目录里,不碰装机版那份
            Arc::new(mt_config::ConfigStore::at(
                app_data_dir().join("config.json"),
            ))
        } else {
            match mt_config::ConfigStore::open() {
                Ok(store) => Arc::new(store),
                Err(err) => {
                    tracing::error!("[app] 配置目录不可用: {err:#}");
                    return;
                }
            }
        };
        // 配置在首帧前只读**一遍**:这一次严格加载的结果同时交给界面语言、hook 开关
        // 与 `AppStore`。此前是先 `read()` 整读一遍只为取这两个字段,`AppStore::new`
        // 里再 `load()` 整读一遍。这一代库备份也不在这里同步做了 —— 挪成配置写线程
        // 的第一件活,「备份先于本次运行的任何写入」由单写者线程的顺序保证(见
        // `store::config_writer` 模块注释)。
        let loaded = config_store.load_without_backup();
        startup_trace::mark("setup: read_config done");
        // 加载失败时 `AppStore` 按默认配置以只读模式跑,这两个字段同样取默认值
        // (与此前 `read()` 失败时回落默认配置的结果一致)
        let (locale, hook_enabled) = match &loaded {
            Ok(loaded) => (loaded.config.locale.clone(), loaded.config.hook_enabled),
            Err(_) => {
                let fallback = mt_config::AppConfig::default();
                (fallback.locale, fallback.hook_enabled)
            }
        };
        // 界面语言必须在**任何视图建出来之前**定下来:`t()` 读的是进程级全局量,
        // 晚一步的话首帧会以默认中文画出来再被刷成英文(闪一下)。
        // 首启没有 config.locale 时按系统语言探测,探测结果不落盘 —— 与 TS 侧
        // `detectInitialLang()` 一致,用户没显式选过就一直跟随系统。
        i18n::install(locale.as_deref());
        i18n::install_encoding_region();

        // hook 开关取自配置(与装机版同一字段);start_hook_server 的数据目录统一
        // 走 mt_config::app_data_dir(),端口文件与装机版落在同一处。
        let (ai_bridge, ai_events) = AiBridge::new(hook_enabled);
        let ai_for_quit = ai_bridge.clone();

        AppStore::set_global(
            cx.new(|cx| AppStore::new(config_store, loaded, ai_bridge, cx)),
            cx,
        );
        // 往后所有视图都从 Global 取这一份 store(等价于 zustand 的 useAppStore)
        let store = AppStore::global(cx);

        // SSH 交互浮层的抄送监听(127.0.0.1 随机端口,不看 hook 开关)。要赶在任何 PTY
        // 起来之前:`MINITERM_SSH_FEED` 是起 PTY 时按 pane 注入的(`store::panes`)。
        // 事件泵要拿 store,所以排在 `set_global` 之后。绑不上只记日志,功能整体关闭。
        ssh_activity::start(cx);

        // 用户改过的快捷键压在默认键位之上(默认键位上面 `bind_keys` 已绑好;机制见
        // `hotkeys::apply_overrides`)。必须赶在窗口开出来之前:首帧的菜单提示、首启引导的
        // 键帽读的就是此刻生效的那份。读不懂 / 认不出的条目丢弃,只记日志
        let hotkey_overrides = store.read(cx).config().hotkey_overrides.clone();
        for issue in hotkeys::apply_overrides(&hotkey_overrides, cx) {
            tracing::warn!("[hotkeys] {issue}");
        }

        // 界面字号 / 字族同样要在**任何视图建出来之前**定下来:`ui::font_px` 读的是
        // 进程级快照,晚一步首帧会按默认 13px 画出来再被刷一遍(闪一下)。
        store.read(cx).apply_ui_font();

        // 终端重绘帧率(设置页「性能」那两项)同理要赶在第一次 PTY 输出起泵之前
        let (foreground_fps, background_fps) = {
            let store = store.read(cx);
            (store.foreground_fps(), store.background_fps())
        };
        redraw::set_frame_rates(foreground_fps, background_fps, cx);

        // 主题必须在**起 PTY 之前**装配:新建终端拿的是 store 里那份终端配色,
        // 晚一步的话首批终端会以默认配色建出来,再被热更新刷一遍(闪一下)。
        // 窗口还没开,`window` 传 None —— Theme::change 只是少一次 refresh。
        store.update(cx, |store, cx| store.apply_theme_from_config(None, cx));

        // 当前项目的终端要补起来(布局是从 layout.db 恢复的,PTY 当然没了),但
        // **不在这里补** —— 挪到首帧呈现之后(见下方 `open_window` 之后那段),窗口
        // 先出来,终端随后贴上。当初挪它是因为每个 pane 的 openpty + 进程 spawn 都在
        // 主线程串行走(实测恢复 6 个 pane 约 110ms,慢机器 / 网络盘上是几百毫秒到
        // 几秒);现在 spawn 与续接 cwd 反查已进后台(见 `pane` 模块注释「PTY 在后台
        // 起」),主线程上只剩建视图,这个时机照旧保留。
        let active = store.read(cx).active_project_id.clone();
        startup_trace::mark("setup: config applied (layout restored)");

        // 退出前把配置刷下去(不等 500ms 防抖),顺手收掉 hook server 的端口文件
        let store_for_quit = store.clone();
        cx.on_app_quit(move |cx| {
            store_for_quit.update(cx, |store, _| store.save_config_now());
            // 终端里的内置 SSH 会话(远程项目 pane / 右键「SSH 连接」接管)发关闭序列,不留连接
            store_for_quit.update(cx, |store, cx| store.close_ssh_sessions(cx));
            ai_for_quit.shutdown();
            // SSH 会话池优雅断开(对齐装机版 `RunEvent::Exit` 里的那一调)。
            // 池没建过时是 no-op,不会为此现起 tokio 运行时。
            mt_remote::shutdown_on_exit();
            async {}
        })
        .detach();

        // macOS 的 NSApplication **不随最后一个窗口关闭而退出**(Windows / Linux 的
        // gpui 会结束事件循环),于是关窗后进程还活着、Dock 里还挂着图标,而点它没有
        // 任何反应:本仓没注册 `on_reopen`,`menu.rs` 又是右键菜单不是菜单栏,没有
        // 任何入口能把窗口叫回来 —— 应用变成一个点不开也退不掉的僵尸。
        //
        // 收敛成「关窗即退出」而不是重开窗口:`Workspace::new` 吃掉的 `ai_events` 是
        // 一次性的 `UnboundedReceiver`,重建窗口得先把 AI 事件泵与 Workspace 的生命
        // 周期解耦;而 `title_bar::finish_close` 的注释(「那条管进程退出,这条管窗口
        // 关闭」)与关窗确认框的措辞(「关掉会丢失这些 AI 会话」)本来就是
        // 「关窗 = 关应用」的语义。
        //
        // 只在 macOS 挂:另外两家 gpui 自己就会退,多这一道只会在主力平台上引入变数。
        #[cfg(target_os = "macos")]
        cx.on_window_closed(|cx, _closed| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        // 上次退出时的窗口大小/位置/最大化态(存在 layout.db)。没存过 / 存的框
        // 已经不在任何一块屏幕上 → 回落默认居中 1280×800。
        let window_bounds = restore_window_bounds(store.read(cx).window_geometry(), cx);
        let store_for_window = store.clone();
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(window_bounds),
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
                let workspace =
                    cx.new(|cx| Workspace::new(store_for_window, ai_events, window, cx));
                // Root 自己那层整窗底色(`tokens.background`)压在 HTML 预览 WebView 的
                // 洞底下、绕不开,关掉它;窗口底色改由 Workspace 根上的
                // `native_view::root_bg` 画(同一个 bg_base,不透明,观感不变)
                cx.new(|cx| Root::new(workspace, window, cx).bg(gpui::transparent_black()))
            },
        );
        let window = match window {
            Ok(window) => window,
            Err(err) => {
                tracing::error!("打开窗口失败: {err:#}");
                return;
            }
        };
        cx.activate(true);
        // 装机版最后一个节点是前端的 `show() call (main UI first frame done)`;
        // GPUI 侧窗口一建出来元素树就已经构造完(`Workspace::new` 是同步的),
        // 差的只有 GPU 那一帧,于是收在这里。
        startup_trace::mark("setup exit (window opened)");

        // 项目里的 SKILL.md 对齐当前模板 / 令牌 / CLI 位置:后台跑一次,只进日志,
        // 不拦启动(dev 实例由注册器自己跳过)。
        store.read(cx).reconcile_ssh_skills_in_background(cx);

        // 帧剖析叠层(度量工具)。挂在窗口建好之后:叠层状态住在 `Window` 上,
        // 不是 App 级的。`MT_FRAME_OVERLAY` 没设时这一行什么都不做。
        #[cfg(feature = "frame-profiler")]
        frame_profiler::install(&window, cx);

        // 启动补 PTY,排在**首帧呈现之后**。
        //
        // gpui 的 `on_request_frame` 每帧的顺序是「先跑 `on_next_frame` 回调,再
        // draw + present」;`open_window` 在 Windows 上虽已同步 draw 过一次,却要等
        // 事件循环里第一个 request_frame 才 present。所以只挂一层回调会在首帧
        // present **之前**执行,等于没挪;套两层才落在第一帧呈现之后、第二帧开画
        // 之前。外层顺手 `refresh()` 保证第二帧一定会来(Windows 有 vsync 线程
        // 常驻驱动,另外两家不一定)。
        //
        // 代价是 PTY 晚一个 vsync 起步(十几毫秒),换来的是窗口不再陪 spawn 干等。
        // (spawn 本身已在后台跑,这里只是把「建视图 + 派发后台任务」排到首帧之后。)
        if let Some(project_id) = active {
            let store = store.clone();
            let _ = window.update(cx, |_, window, _| {
                window.on_next_frame(move |window, _| {
                    window.refresh();
                    window.on_next_frame(move |window, cx| {
                        store.update(cx, |store, cx| {
                            store.hydrate_project(&project_id, cx);
                            // `Workspace::new` 那一下只记住了 focused_pane_id,当时
                            // 还没有 PTY 实体可聚焦;真正的键盘焦点在这里落到终端上,
                            // 否则用户得先点一下才能打字。
                            if let Some(pane_id) = store.active_pane_id(&project_id) {
                                store.focus_pane(&project_id, &pane_id, window, cx);
                            }
                        });
                        // PTY 此刻只是派发给了后台,真正起好在各 pane 回填时
                        startup_trace::mark("hydrate: PTY spawns dispatched (after first frame)");
                    });
                });
            });
        }

        // 持久化降级的告知:配置加载失败 → 本次只读;布局库不可用 → 布局不落盘。
        // 此前两者都只在 stderr 留一行,用户照常改了一整天、重启后全丢才发现。
        // 同样排在**首帧呈现之后**(两层 `on_next_frame`,理由同上):弹窗挂在
        // Root 上,首帧之前开的话会跟着窗口一起闪出来。只有一个「知道了」。
        if let Some(message) = store.read(cx).read_only_state().dialog_message() {
            let _ = window.update(cx, |_, window, _| {
                window.on_next_frame(move |window, _| {
                    window.refresh();
                    window.on_next_frame(move |window, cx| {
                        prompt::show_alert(t("app", "storageIssueTitle"), message, window, cx);
                    });
                });
            });
        }
    });
}
