//! 一个终端 pane 的运行时:PTY + VT 状态机 + 渲染 + 键盘。
//!
//! 从原来 `main.rs` 里那条端到端竖切抽出来,补上三件事:
//! 1. **pane 编号**(`pty_id`):既是 `MINITERM_PTY_ID`(hook 回报的定位键),
//!    也是 `mt-ai` 里的 `pane_id`,还是 store 里 `terminals` 表的键;
//! 2. **AI 感知旁路**:写入前 `observe_input`、读出后 `observe_output`;
//! 3. **退出上报**:子进程退出 → 发 [`PaneEvent::Exited`],由 store 落成 `error`
//!    状态(与旧版 `pty-exit` → `updatePaneStatusByPty('error')` 同语义)。
//!
//! # PTY 在后台起
//!
//! [`TerminalPane::new`] 只在主线程上建 emulator 与视图、分好 `pty_id`,**立即返回**;
//! 「定启动参数(远程预检 / 续接 cwd 反查)→ spawn → (SSH 远程项目)arm 自动填充」
//! 整段丢到 `background_executor`,完成后回主线程回填([`TerminalPane::finish_spawn`])。
//! 起 PTY 在 Windows 上要逐个 PATH 目录 × PATHEXT 去 stat 裸名、`is_dir(cwd)`、建
//! ConPTY、起子进程,本机一次十几毫秒、偶发几百毫秒,cwd 或 PATH 里挂着断开的网络盘
//! 时能冻结好几秒 —— 恢复六个 pane 就是整窗卡一下。
//!
//! 回填前的写入 / resize / 退出通知怎么攒、回填时按什么顺序交出去,全在
//! [`crate::pty_slot`](与 GPUI 无关,有单测)。
//!
//! # 渲染/键盘/IME 归 [`mt_ui::TerminalView`]
//!
//! 本模块**不**处理按键:`TerminalView` 自己 `track_focus` + `key_context("Terminal")`
//! + `on_key_down`,并按 `is_text_input_key` 分流(可打印键放行走 WM_CHAR/IME,
//! 其余键转义序列 + `stop_propagation`)。宿主再挂一份就会双份处理,中文输入法下
//! 一个字变两个。应用级快捷键仍然通:gpui 的按键派发是**先匹配 action 绑定、
//! 后跑 key 监听**(`Window::dispatch_key_event`),所以 `Workspace` 上绑的
//! Ctrl+Shift+T 之类根本轮不到终端;Ctrl+Shift+C/V 没有绑定,由 `TerminalView`
//! 自己消费,其余 Ctrl+Shift 组合它原样冒泡。
//!
//! # 重绘唤醒
//!
//! PTY reader 在**独立线程**上,gpui 的 `AsyncApp` 内部是 `Weak<AppCell>`(Rc),
//! 不能跨线程持有,所以 reader 线程没法直接 `notify`。走标准做法:reader 线程往
//! `futures::mpsc` 无界 channel 丢信号,主线程上 `cx.spawn` 起的前台任务 `await`
//! 它,醒来后 `cx.notify()`。
//!
//! 这是**事件驱动**,不是定时轮询 —— 空闲时一帧都不画。
//!
//! 醒来之后分两条路,**节奏不同**:
//!
//! | 走什么 | 归谁管 | 节拍 |
//! |--------|--------|------|
//! | `drain_term_events`(PtyWrite/DA/DSR 应答) | 本循环 | [`DRAIN_PERIOD`] 恒 16ms |
//! | `cx.notify()`(重绘) | [`crate::redraw`] | 默认前台 33ms / 后台 200ms(设置页可调),**全局共用一条** |
//!
//! 分开是因为两者的「晚一拍」代价完全不同:应答晚了对面的 TUI 干等,画面晚一拍
//! 没人看得出来。此前两件事绑在同一个 16ms 定时器上,于是每个 pane 各自按 62fps
//! 请求整窗重绘 —— N 个 pane 相位错开,等于每个 vsync 都撞上一次 dirty。
//!
//! # 同步输出的超时释放
//!
//! 程序用 `ESC[?2026h` 开一个同步块之后,字节只进解析器的缓冲、不上屏。vte 只记一个
//! 到期时刻,到期不会自己解开:TUI 崩了、ssh 断了,结束标记永远不来,shell 其实还活着,
//! 提示符与回显却全进了缓冲,画面看着就是死了,resize 也救不回来。
//!
//! 所以唤醒循环看到有同步块在攒就排一个**一次性**定时器(见
//! [`TerminalPane::arm_sync_release`]):不用常驻轮询,也不依赖下一次输出、不依赖有没有
//! 在画帧 —— 前台任务不看窗口脸色,后台与最小化时照样到点释放,重绘请求由节拍器攒着。
//!
//! # 本地 shell 退出 / 起不来之后
//!
//! pane 不自动关(画面留着可回看),底部另起一行「shell 已退出(退出码 N)· Enter 重新启动
//! [重新启动] [关闭]」([`TerminalPane::render_exit_bar`])。这一行**占自己的位置**、不盖在
//! 终端上 —— 整屏遮罩会挡住退出前的最后几行输出,而用户多半正要看那几行。重新启动复用
//! 远程重连那条 `AppStore::reset_pane_for_reconnect`(同一 shell 与 cwd,就地换一条新 PTY);
//! 远程 pane 不走这里,断线由 `terminal_area` 的整屏重连遮罩负责,行为不变。
//!
//! # 回看浮钮
//!
//! `display_offset > 0` 时终端右下角出一颗「↓」([`TerminalPane::render_scroll_bottom_button`]),
//! 回看期间来过新输出就点一颗小圆点。回看位置在 render 里现读:滚轮 / 翻页键是终端视图
//! 自己 `notify`,gpui 会把祖先 view(本 pane)一并标脏重画;新输出走节拍器 notify 本 pane。
//!
//! # 右键「清除缓冲」
//!
//! 要的效果是 Windows Terminal 的「清除缓冲区」:回看与当前屏都清掉、提示符重画在顶上。
//! WT 能干净做到是因为它另调了 `ConptyClearPseudoConsole` 让 conhost 也清掉自己那份
//! 屏幕 —— 这里没有这条通道,**只在本地清屏的话 conhost 不知道**,它以为屏上还是旧内容,
//! 之后只补画变了的格子,提示符就再也画不回来。所以分三档([`clear_buffer_plan`]):
//!
//! - **普通 shell**(认得出支持 Ctrl+L 的那几家,及远程项目的登录 shell):本地清回看,再给
//!   shell 发一个 Ctrl+L —— 清屏与重画提示符由 shell 自己做(正在输入的半行命令照样留着),
//!   conhost 与本地的账就对得上。shell 清屏会把屏上内容推进回看(xterm / alacritty 的 ED2
//!   语义),所以之后 [`CLEAR_REDRAW_WINDOW`] 之内来的输出再顺手清一次回看;
//! - **AI 会话 / 全屏程序 / 认不出的 shell(cmd 之类)**:只清回看,**一个字节都不发** ——
//!   Ctrl+L 在 Claude Code 这类 TUI 里是它自己的快捷键,在 cmd 里会变成命令行里的一个怪字符;
//! - **程序已经退出**:没人会重画了,本地连屏带回看一起清。

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use gpui::{
    AnyElement, App, AppContext, Bounds, ClipboardItem, Context, Div, Entity, EventEmitter,
    FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, ParentElement, Pixels, Point, Render, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Subscription, Task, Window, div, prelude::FluentBuilder,
    px,
};
use gpui_component::ActiveTheme as _;
use mt_config::{ShellConfig, SshConnection};
use mt_pty::{PtySession, PtySpawn};
use mt_terminal::alacritty_terminal::event::Event as TermEvent;
use mt_terminal::alacritty_terminal::grid::{Dimensions as _, Scroll};
use mt_terminal::alacritty_terminal::term::TermMode;
use mt_terminal::{SyncRelease, TermSize, TerminalEmulator};
use mt_ui::icons::ShellKind;
use mt_ui::terminal::{MouseMods, TerminalLink, prefers_local_handling};
use mt_ui::tooltip::TooltipExt as _;
use mt_ui::{
    CopiedTip, DwellConfig, FlashLine, PasteAction, SearchBarEvent, TerminalSearch,
    TerminalSearchBar, TerminalStyle, TerminalTheme, TerminalView,
};

use crate::ai::AiBridge;
use crate::clipboard::{self, ClipboardImage, PasteTarget, RemotePaste};
use crate::i18n::{t, tr};
use crate::markers::{self, MarkerBatch, MarkerSubmit};
use crate::menu::{self, MenuItem};
use crate::notify::ToastKind;
use crate::overlay;
use crate::popover_reveal::{Chrome, RevealState, ShadowMode, reveal_frame};
use crate::pty_slot::{PtyPhase, PtySlot};
use crate::redraw;
use crate::ssh_activity::SshActivityView;
use crate::store::{AppStore, TerminalZoom};
use crate::toast;

gpui::actions!(
    mini_term,
    [
        /// 终端字号放大一档(Ctrl+=)。绑在 `Terminal` context 上(见 `hotkeys.rs`),
        /// 只在终端有焦点时命中;落点见 [`zoom_terminal_font`]。
        TerminalZoomIn,
        /// 终端字号缩小一档(Ctrl+-)。
        TerminalZoomOut,
        /// 终端字号恢复默认(Ctrl+0)。
        TerminalZoomReset,
    ]
);

/// pane 发给上层的事件。
pub enum PaneEvent {
    /// 子进程退出(退出码取不到为 `None`)。
    Exited(Option<u32>),
    /// PTY 没能起来(预检失败 / spawn 失败)。pane 里画一行红字之外,store 据此把
    /// pane 落 error,页签与项目行才看得出是哪个终端坏了。已关闭的 pane 不发。
    SpawnFailed,
    /// 用户往这个 pane 里键入了东西 —— store 据此清掉 attention 黄灯
    /// (旧版 `clearPaneAttentionByPty`:键入即视为「已在处理待确认事项」)。
    UserInput,
    /// 用户往 AI 会话里提交了一行 → 打一批任务标记(⚑),锚点已经取好。
    ///
    /// 走事件而不是在 [`TerminalPane::write`] 里直接写 store:`write` 有一条
    /// 调用路径是 `AppStore::write_to_pane`(在 `store.update` 里调),那里再去
    /// `AppStore::global(cx).update` 就是同一实体的嵌套 update,gpui 直接 panic。
    AiMarks(MarkerBatch),
    /// shell 通过 OSC 0/2 设置的窗口标题(`None` = `ResetTitle`)——
    /// store 据此更新 pane 的副段标题(`AppStore::set_pane_osc_title_by_pty`)。
    ///
    /// **已按 [`OSC_TITLE_PERIOD`] 合并过**:发上来的是一个窗口内的最新值,
    /// 不是每次 OSC 序列都发一条。清洗与去重在 store 侧。
    Title(Option<String>),
    /// 启动恢复时后台反查到的会话 cwd(见 [`PreparedLaunch::resume_cwd`]),
    /// 回填完成后交还 store 随身份写回并落盘,下次重启免查。
    ResumeCwd(ResumeCwd),
}

/// 续接反查所得的会话启动目录,连同它属于哪个会话。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeCwd {
    pub session_id: String,
    pub cwd: String,
}

/// reader / watcher 线程 → 主线程的信号。
enum PaneSignal {
    Output,
    Exit(Option<u32>),
}

pub struct TerminalPane {
    /// 后端 pane 编号,见模块注释。
    pty_id: u32,
    emulator: Arc<TerminalEmulator>,
    /// PTY 会话。**建 pane 时还不存在**(后台在起),见模块注释「PTY 在后台起」与
    /// [`crate::pty_slot`]。
    pty: PtySlot<PtySession>,
    /// 后台起 PTY 的那条前台任务(等后台结果、回主线程回填)。实体释放时随字段一起
    /// 丢掉 —— 后台还没开跑就连 spawn 都不做;已经在跑的,产出的会话随之被丢弃
    /// (= kill)。`shutdown` 不动它,理由见那边。
    _spawn: Task<()>,
    /// 等「PTY 起没起来」定局的人(移动端发起会话的回执)。定局即逐个回 `true/false`,
    /// pane 没了则 sender 随之丢弃,接收端拿到 `Canceled` 按失败算。
    spawn_waiters: Vec<oneshot::Sender<bool>>,
    focus: FocusHandle,
    /// 渲染 + 键盘 + IME 全在这一层([`mt_ui::TerminalView`])。
    ///
    /// **宿主不再自己 `track_focus` / `key_context` / `on_key_down` / 左键聚焦** ——
    /// 留着会让按键被处理两遍,而且 IME 分流依赖「可打印键放行走 WM_CHAR」,
    /// 宿主抢先把字节写进 PTY 的话中文输入法下一个字会变两个。
    view: Entity<TerminalView>,
    /// 当前的渲染样式。留着是给字号/字族热更新做「值变了没」的比较
    /// (视图侧自己也比一次,这里比是为了省掉一次 entity update)。
    style: TerminalStyle,
    theme: TerminalTheme,
    ai: AiBridge,
    /// SSH 远程项目的 pane(见 [`Self::with_remote`])。退出 / 起不来之后的「重新启动」
    /// 只给本地 pane:远程断线有 `terminal_area` 的整屏重连遮罩,不叠第二套入口。
    remote: bool,
    /// 子进程已退出。
    exited: bool,
    /// 子进程的退出码(取不到为 `None`)。退出条上非 0 才显示。
    exit_code: Option<u32>,
    /// PTY 死掉(退出 / 起不来)的时刻;`None` = 还活着或还在起。
    /// 「Enter 重新启动」据此挡掉 [`RESTART_ENTER_GRACE`] 之内的回车。
    dead_since: Option<Instant>,
    /// 已经排过一次重新启动。重启要 defer 到本轮 effect 之后才做,这之前连按的 Enter /
    /// 连点的按钮不许再排第二次。
    restart_requested: bool,
    /// 上一帧在回看(`display_offset > 0`)。render 里刷新,唤醒循环据此判「回看期间有新输出」。
    scrolled_back: bool,
    /// 回看期间来过新输出 —— 「回到底部」浮钮上那颗小圆点。回到底部即清。
    unseen_output: bool,
    /// 右键「清除缓冲」给 shell 发了 Ctrl+L 之后,到这个时刻之前来的输出都要顺手清一次回看
    /// (shell 清屏会把屏上内容推进回看)。`None` = 没在清。见模块注释「右键「清除缓冲」」。
    scrollback_purge_until: Option<Instant>,
    /// PTY 起不来时的错误文本(直接显示给用户,不吞)。后台起完之前是 `None` ——
    /// 「还在起」与「起失败」要看 [`Self::spawn_settled`],不能拿它当判据。
    spawn_error: Option<String>,
    /// 「已复制」气泡的落点(**元素相对**坐标)。`None` = 不显示。
    /// 1s 后由自撤任务清掉,与旧版 `tipTimer` 同语义。
    copied_tip: Option<Point<Pixels>>,
    /// 气泡自撤任务的句柄。存着是为了「连着复制两次」时上一个计时器被丢弃 ——
    /// 否则第一次的计时器到点会把第二次刚弹出来的气泡提前抹掉。
    _tip_timer: Option<Task<()>>,
    /// 终端内查找引擎。与查找条、渲染层**共用同一份**(计数与高亮从此是同一份
    /// 状态),所以关键词/选项活得过查找条的一次次开关 —— 与原版
    /// `useTerminalSearchStore` 把关键词留在 store 里同语义。
    search: Rc<RefCell<TerminalSearch>>,
    /// 浮动查找条。`None` = 没打开。**逐 pane 一条**(原版是全局单例,见
    /// [`Self::open_search`] 的说明)。
    search_bar: Option<Entity<TerminalSearchBar>>,
    /// 正在播收起动画的查找条(已经关了,`search_bar` 是 `None`)。播完才丢;动画中途再打开时
    /// 原样接回来,从当前进度往回长。
    search_exit: Option<Entity<TerminalSearchBar>>,
    /// 查找条的开合动画 + 原点(标题栏「查找」按钮,探针报来),见 [`crate::popover_reveal`]。
    search_reveal: RevealState,
    /// 查找条事件订阅:gpui-fast 下查找条 notify 不会连带重绘同级 `TerminalView`,
    /// 关键词/导航变化时经本订阅显式标脏 `self.view`。
    _search_sub: Option<Subscription>,
    /// 监听 `self.view` 的重绘:gpui-fast 下 `TerminalView` 滚动/翻页时会拼接跳过
    /// `TerminalPane`,这里在 `scrolled_back` 状态翻转时标脏外壳以显隐「回到底部」浮钮。
    _view_sub: Subscription,
    /// 标记跳转后那 300ms 闪烁的撤销计时器。与 `_tip_timer` 同理必须存句柄:
    /// 连着跳两条时上一个计时器随之被丢弃,否则第一次的到点回调会把第二次刚
    /// 亮起来的那一行提前抹掉。
    _flash_timer: Option<Task<()>>,
    /// 等着定锚的 AI 任务标记正文 —— Enter 已经按下,锚点还在等 Ink 把光标
    /// 顶回块首。空 = 没有在等的。见 [`Self::arm_marks`]。
    pending_marks: Vec<MarkerSubmit>,
    /// 待定标记的定锚计时器。掉了任务就没了,必须存着。
    _marks_timer: Option<Task<()>>,
    /// 限流窗口里待推的 OSC 标题。
    ///
    /// 外层 `Option` = **有没有在途的窗口**(`Some` 即表示定时器已经排上了,
    /// 到点会来取这里的值),内层 `Option` = 标题本身(`None` = `ResetTitle`)。
    /// 两层分开是因为「要推一个 None 上去」与「没有要推的」是两件事。
    pending_osc_title: Option<Option<String>>,
    /// OSC 标题限流窗口的计时器。掉了任务就没了,必须存着。
    _osc_title_timer: Option<Task<()>>,
    /// grid 已经改了、还没下发给 PTY 的尺寸 `(rows, cols)`。见 [`PTY_RESIZE_SETTLE`]。
    pending_pty_size: Option<(u16, u16)>,
    /// PTY resize 的去抖计时器。新尺寸一到就换一个,旧的随之丢弃。
    _pty_resize_timer: Option<Task<()>>,
    /// 同步块释放定时器在途。见 [`Self::arm_sync_release`]。
    sync_release_armed: bool,
    /// 同步块的释放定时器。掉了任务就没了,必须存着。
    _sync_release_timer: Option<Task<()>>,
    /// 唤醒任务的句柄。掉了任务就没了,必须存着。
    _wake: Task<()>,
    /// SSH 交互浮层:记录模型 + 内容区右上角的悬浮框(入口按钮在终端组标题栏上,
    /// `terminal_area::render_leaf_controls` 读它的状态来画)。**恒挂在右上角那一列里**,
    /// 悬浮框关着时画空,理由见 [`crate::ssh_activity`] 视图模块注释。事件由
    /// `ssh_activity` 的泵直接投给它,不经本 pane 的 update。
    ssh_activity: Entity<SshActivityView>,
}

/// 标记跳转后整行闪烁的底色与时长(`terminalCache.ts:193-194` 的
/// `rgba(245, 197, 24, 0.33)` / `300ms`)。
///
/// 原版这两个值是写死的字面量、不走 CSS 变量,所以这里也不进 [`crate::ui`] 调色板。
const FLASH_COLOR: u32 = 0xf5_c5_18_54;
const FLASH_DURATION: Duration = Duration::from_millis(300);

/// 按下 Enter 到给 AI 任务标记定锚之间的等待窗口。
///
/// 要等的是 Ink 那一次「erase 顶回块首 + 打 static 消息」的重绘,本机几十毫秒
/// 就到;放宽到 200ms 是给慢机器 / WSL 留余量。**放长不会变差**:窗口期取的是
/// 光标绝对行的**最小值**,而 AI 开始输出后光标只会往下走,后续重绘的块首也在
/// 已打出的消息**之下** —— 多等只是多采样几个更大的值。
const MARK_SETTLE_DELAY: Duration = Duration::from_millis(200);

/// 唤醒循环合并 PTY 读信号的窗口。刷屏时 reader 每读一块就发一个信号,不合并的话
/// 这条循环会跟着 read 次数空转。
///
/// ⚠️ 这**不是**重绘节拍 —— 那个在 [`crate::redraw`],默认前台 33ms / 后台 200ms。
/// 这一档管的是 [`TerminalPane::drain_term_events`]:终端要回给程序的应答
/// (PtyWrite / DA / DSR)走它,晚一拍对面的 TUI 就多等一拍,所以它跟着读节奏走、
/// **不随窗口前后台变**。
const DRAIN_PERIOD: Duration = Duration::from_millis(16);

/// OSC 0/2 标题推给 store 的**最小间隔**(每个 pane 各算各的)。
///
/// Claude Code 这类 CLI 会把 spinner 帧写进窗口标题,一秒能改好几次;每次都推
/// 就是每次一趟 store 写 + `cx.notify()` 整窗重绘。窗口内只留最新值 ——
/// 标题是「当前是什么」的显示量,中间帧丢掉没有任何损失。
///
/// 代价是首个标题最多晚 250ms 上屏,而那 250ms 里 shell 多半还在打 banner。
const OSC_TITLE_PERIOD: Duration = Duration::from_millis(250);

/// grid 尺寸停稳多久才把 resize 下发给 PTY(尾沿去抖)。
///
/// 逐列下发会让 Claude Code 的输出在 scrollback 里一叠叠重复:alacritty 缩窄列时
/// 按「底部锚定」重排,屏上每有一行折成两行,屏顶就有一行被挤进 scrollback;而
/// Claude Code 收到 resize 后原地(`ESC[H` + 逐行 `ESC[2K`)重画整屏、不碰
/// scrollback,被挤出去的那几行就与屏上重画的成了重复。拖分隔条每缩一列来一遍,
/// 就重复一份(实测 130→110→130 拖一趟,启动横幅 21 份)。停稳再发至多留一份,
/// 拖窄再拖回则一份都不留;单向拖窄残留的那一份终端侧无法无损消除 —— 不知道
/// 哪些被挤出的行 TUI 会重画,删了会丢 shell 的真输出。
///
/// 拖分屏 / 三栏分隔条另有一道闸:gpui 拖拽在途时一律不发,松手才发 —— 慢慢拖时
/// 两列之间隔得比这个窗口还久,光靠去抖仍会逐列漏发。这一档兜的是拖窗口边、
/// 开合侧栏这类没有 gpui 拖拽的来源。grid 本身照旧当帧重排,画面不等。
///
/// 攒下的那一次 resize 在 Windows 上由 mt-pty 逐格走到目标尺寸(不是一步跳过去):
/// ConPTY 的重排与路径有关,一步缩一大截会让 PSReadLine 的提示行错位,见
/// `mt_pty` 的 `resize_path`。连发在一瞬间完成,TUI 仍只按最终尺寸重画一次。
const PTY_RESIZE_SETTLE: Duration = Duration::from_millis(120);

/// 退出 / 起不来之后多久之内的 Enter 不算「重新启动」。
///
/// 防的是 `exit⏎⏎` 这种连按:第一下回车送走 `exit`,shell 退得比第二下回车快,不挡的话
/// 第二下就把刚退出的终端又拉起来了。500ms 盖得住手快的连按,又短到看见退出条之后再按
/// 不会觉得「没反应」。
const RESTART_ENTER_GRACE: Duration = Duration::from_millis(500);

/// 「清除缓冲」发出 Ctrl+L 之后,多久之内的输出算 shell 那次清屏重画(见模块注释「右键
/// 「清除缓冲」」)。要盖住远程 ssh 一来一回的延迟;放得太长则会把用户紧接着跑的命令里
/// 滚出屏幕的那截一并清掉 —— 闲着的提示符上按下去,这段时间里本来就只有那一次重画。
const CLEAR_REDRAW_WINDOW: Duration = Duration::from_millis(800);

/// 「回到底部」浮钮离终端本体右缘的距离:pane 内边距 8 + 滚动条 6 + 滚动条留白 1,
/// 再空出 9 —— 浮钮不压在滚动条那一竖条上(滑块拖到底时正好在这个高度)。
const SCROLL_BUTTON_RIGHT: f32 = 24.0;
/// 离终端本体下缘的距离:让开 pane 内边距 8,再空 8。
const SCROLL_BUTTON_BOTTOM: f32 = 16.0;

thread_local! {
    /// 退出条「关闭」上一次被点的时刻,连点闸用(见 [`TerminalPane::request_close`])。
    static LAST_EXIT_BAR_CLOSE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// 这次写入是不是「Enter 重新启动」:PTY 已死、过了 [`RESTART_ENTER_GRACE`]、
/// 写的正好是一个回车。
///
/// 只认单独一个 `\r`(Enter 在传统编码与 win32-input-mode 下都是它;Shift+Enter /
/// Ctrl+J 另有编码,不算)。粘贴、IME 上屏、别的键都照旧往 PTY 写。
fn enter_restarts(bytes: &[u8], dead_since: Option<Instant>, now: Instant) -> bool {
    bytes == b"\r"
        && dead_since.is_some_and(|at| now.saturating_duration_since(at) >= RESTART_ENTER_GRACE)
}

/// 退出码的显示形态。Windows 的崩溃码是 NTSTATUS(`0xC0000005` 访问违例、`0xC000013A`
/// 关窗 / Ctrl+Break),十进制是一串看不出名堂的十位数,大于 0xFFFF 的一并给出十六进制。
fn format_exit_code(code: u32) -> String {
    if code > 0xFFFF {
        format!("{code} (0x{code:08X})")
    } else {
        code.to_string()
    }
}

/// 退出条上的提示:退出码 0 / 取不到是「shell 已退出」,非 0 带上退出码。
fn exit_message(code: Option<u32>) -> String {
    match code {
        Some(code) if code != 0 => {
            tr!(
                "paneGroup",
                "shellExitedCode",
                code = format_exit_code(code)
            )
        }
        _ => t("paneGroup", "shellExited").to_string(),
    }
}

/// 终端字号缩放(Ctrl+= / Ctrl+- / Ctrl+0 与 Ctrl+滚轮的共同落点)。
///
/// 改的是**全局**终端字号 —— 与设置页那条滑杆同一个 setter(全部终端即时生效 + 防抖落盘),
/// 范围同一把尺子(`store::TERMINAL_FONT_SIZE_MIN / MAX`)。
///
/// 两个入口都不借着本 pane:键位走元素的 `on_action`(普通闭包,不是 `cx.listener`),
/// 滚轮走视图的 `on_zoom`(元素的鼠标监听里调)。setter 会回头 `set_style` 每一个终端
/// —— 包括这一个,所以落点不能在本 pane 的监听器里,否则就是同一实体的嵌套 update。
fn zoom_terminal_font(zoom: TerminalZoom, cx: &mut App) {
    AppStore::global(cx).update(cx, |store, cx| store.zoom_terminal_font(zoom, cx));
}

impl EventEmitter<PaneEvent> for TerminalPane {}

/// 起 pane 的「启动预案」:在**后台线程**上兑现成真正要 spawn 的东西。
///
/// 为什么是闭包而不是现成的 [`PtySpawn`]:有两条支路光是定 spec 本身就要碰盘 ——
/// SSH 远程项目的预检(PATH 里找 ssh 客户端、复制私钥并起 `icacls` 收紧权限、
/// 解开已存密码)与启动恢复的续接 cwd 反查(翻 `~/.claude/projects`)。它们与
/// spawn 进同一个后台任务,按「先定 spec、再 spawn」的顺序跑,主线程一步都不等。
///
/// 返回 `Err(文本)` = 预检失败(断链 / 本机缺 ssh 客户端):**不 spawn**,直接把
/// 这条错误画在 pane 里 —— 与装机版 `create_pty` 返回 `Err` 后前端落 `spawnErrors`
/// 同样效果,不留半开的会话。
pub type PaneLaunch = Box<dyn FnOnce() -> Result<PreparedLaunch, String> + Send>;

/// [`PaneLaunch`] 兑现后的产物。
pub struct PreparedLaunch {
    pub spec: PtySpawn,
    /// SSH 登录密码(远程项目)。spawn 成功后**在同一个后台任务里立刻**注册
    /// autofill,不等回主线程。
    ///
    /// ⚠️ 装机版是在 `openpty` 之后、`spawn_command` 之前 arm 的(那里 PTY 与
    /// reader 是两步)。GPUI 侧 `PtySession::spawn` 一步就把 reader 线程起了,
    /// 只能事后 arm —— 窗口是「spawn 返回」到「下一行」的几微秒,而 ssh 的密码
    /// 提示要等 TCP 连接 + 版本协商 + 密钥交换(最快也几十毫秒),够不着。
    /// **不能**挪到主线程回填时再 arm:那要排到主线程下一次空闲,窗口一下子放大
    /// 到不可控。真要彻底消除得给 `mt_pty::PtyOptions` 加一个 autofill 字段,
    /// 那是改 mt-pty 公开 API,本批不做(记档见 BB-a 报告)。
    pub ssh_password: Option<String>,
    /// 续接反查所得的会话 cwd(只有启动恢复的续接 pane 才有)。回填后经
    /// [`PaneEvent::ResumeCwd`] 交还 store 写回。
    pub resume_cwd: Option<ResumeCwd>,
}

/// 后台任务交回主线程的结果。
struct SpawnOutcome {
    session: anyhow::Result<PtySession>,
    resume_cwd: Option<ResumeCwd>,
}

/// 后台线程上的那一段:兑现启动预案 → spawn →(远程)紧贴 spawn arm 自动填充。
fn spawn_in_background<F>(
    launch: PaneLaunch,
    options: mt_pty::PtyOptions,
    on_output: F,
) -> SpawnOutcome
where
    F: FnMut(&[u8]) + Send + 'static,
{
    let prepared = match launch() {
        Ok(prepared) => prepared,
        // 预检失败:根本不 spawn。`options` / `on_output` 随之丢弃,它们手里的
        // 信号 sender 一没,pane 的唤醒循环自然收摊(与此前起失败同)。
        Err(err) => {
            return SpawnOutcome {
                session: Err(anyhow::anyhow!(err)),
                resume_cwd: None,
            };
        }
    };
    let session = PtySession::spawn_with_options(prepared.spec, options, on_output);
    // SSH 远程 pane:密码自动填充**紧贴 spawn** 注册(见 `PreparedLaunch` 的字段
    // 注释)。`disarm_on_input = true`:远程项目 pane 起来之后不再写任何命令,首个
    // `write` 即用户交互 —— 一打字就解除,避免 SSH 登录密码被灌进后续 `su` /
    // `mysql -p` / `passwd` 的提示里。回填前用户抢先敲的字节在队列里,冲刷时照样
    // 经 `write` 解除,口径不变。
    if let (Ok(session), Some(password)) = (session.as_ref(), prepared.ssh_password) {
        session.arm_ssh_autofill(password, true);
    }
    SpawnOutcome {
        session,
        resume_cwd: prepared.resume_cwd,
    }
}

impl TerminalPane {
    /// 建 emulator 与视图、登记 AI 感知后**立即返回**;PTY 由 `launch` 在后台起,
    /// 完成后回主线程回填(见模块注释「PTY 在后台起」)。
    ///
    /// `user_env` 是用户配的环境变量(全局 + 项目级,已合并):走
    /// [`mt_pty::PtyOptions::user_env`] 而不是 `spec.env`,
    /// 因为前者会被 `MINITERM_` 前缀过滤挡一道 —— 用户手改配置
    /// (现在是 `config.db`)也覆盖不掉内部协议变量。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pty_id: u32,
        launch: PaneLaunch,
        user_env: Vec<(String, String)>,
        style: TerminalStyle,
        theme: TerminalTheme,
        dwell: DwellConfig,
        scrollback: usize,
        ai: AiBridge,
        cx: &mut Context<Self>,
    ) -> Self {
        // 首帧还没量过字体,先给个能跑的初值;真正的尺寸在元素 prepaint 里量出来
        // 之后通过 on_grid_resize 回来纠正(PTY 还没回填时记在 `PtySlot` 上,
        // 回填时补下发)。
        //
        // 回滚行数(`config.terminalScrollback`)必须在这一刻喂进 alacritty 的
        // `term::Config`:它决定 grid 的历史容量,建完再改只能靠 `set_options`。
        let emulator = TerminalEmulator::with_scrollback(
            TermSize::new(
                mt_pty::INITIAL_PTY_COLS as usize,
                mt_pty::INITIAL_PTY_ROWS as usize,
            ),
            scrollback,
        );
        let emulator = Arc::new(emulator.with_kitty_graphics(crate::kitty_graphics::active()));

        let (tx, mut rx) = mpsc::unbounded::<PaneSignal>();
        let exit_tx = tx.clone();
        let options = mt_pty::PtyOptions::default()
            .with_user_env(user_env)
            .on_exit(move |code| {
                let _ = exit_tx.unbounded_send(PaneSignal::Exit(code));
            });
        let on_output = {
            let emulator = emulator.clone();
            let ai = ai.clone();
            move |bytes: &[u8]| {
                // reader 线程:直接推进状态机,没有 IPC、没有批缓冲、没有序列化。
                emulator.advance(bytes);
                // AI 感知的输出旁路(命令 echo 回扫 + 输出活跃度)
                ai.perception().observe_output(pty_id, bytes);
                // Git 面板的输出旁路(外部跑了 git 命令 → 刷新变更与仓库元信息)。
                // **这条线程上不跑任何模式匹配**:总闸关着时只有一次原子读,
                // 开着时也只是把尾部字节塞进有界环形缓冲,5 条口径在主线程节拍上跑。
                // 详见 `git_watch` 模块注释(后续 Y 批的 git 着色与本条共用)。
                crate::git::watch::observe_output(pty_id, bytes);
                let _ = tx.unbounded_send(PaneSignal::Output);
            }
        };

        // 起 PTY:整段在后台跑,回主线程回填。
        //
        // 回填前 pane 没了有两种形态,都不留孤儿进程:
        // - **实体已释放**(或应用在退出):`this.update` 返回 Err,闭包连同捕获的
        //   outcome 一起被丢弃,`PtySession::drop` 当场杀子进程;实体释放时这条任务
        //   本身也随 `_spawn` 字段一起被丢掉,后台还没开跑的话连 spawn 都不做;
        // - **只是关掉了**(`shutdown`,实体还在):`PtySlot` 已是 Closed,
        //   `finish_spawn` 里 `backfill` 把会话退回来丢弃。
        let spawn = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { spawn_in_background(launch, options, on_output) })
                .await;
            let _ = this.update(cx, |pane: &mut TerminalPane, cx| {
                pane.finish_spawn(outcome, cx)
            });
        });

        let wake = cx.spawn(async move |this, cx| {
            while let Some(signal) = rx.next().await {
                let mut exit: Option<Option<u32>> = None;
                let mut output = false;
                match signal {
                    PaneSignal::Output => output = true,
                    PaneSignal::Exit(code) => exit = Some(code),
                }
                // 把已经排队的信号一次抽干,避免一次读一个信号地重绘。
                while let Ok(extra) = rx.try_recv() {
                    match extra {
                        PaneSignal::Output => output = true,
                        PaneSignal::Exit(code) => exit = Some(code),
                    }
                }
                if this
                    .update(cx, |pane, cx| {
                        pane.drain_term_events(cx);
                        pane.arm_sync_release(cx);
                        if output {
                            pane.note_output();
                            pane.purge_cleared_scrollback();
                        }
                        // 子进程快到回填之前就退了:先挂着,回填完成后再报
                        // (`PtySlot::note_exit`)—— 退出永远排在回填之后
                        if let Some(code) = exit.and_then(|code| pane.pty.note_exit(code)) {
                            pane.emit_exit(code, cx);
                        }
                    })
                    .is_err()
                {
                    return;
                }
                // 重绘交给全局节拍器:多个 pane 一起刷屏也只出一帧,窗口在后台
                // 时还会自动降到 5fps。**这里不再自己 `notify`** —— 缘由见
                // `crate::redraw` 的模块注释。
                if exit.is_none() {
                    cx.update(|cx| redraw::request(this.clone(), cx));
                }
                cx.background_executor().timer(DRAIN_PERIOD).await;
            }
        });

        // 焦点句柄由宿主持有(切 tab / 点分屏要 `window.focus(&handle)`),
        // 但 `track_focus` 由 TerminalView 自己调 —— 见 view.rs 的接线说明。
        let focus = cx.focus_handle();

        // 查找引擎常驻(关键词要活过一次次开关),一开始是关着的 —— 关着时
        // 渲染层不跑重搜、不画高亮,零开销。
        let search = Rc::new(RefCell::new(TerminalSearch::new()));
        search.borrow_mut().set_enabled(false);

        let view = {
            let this = cx.weak_entity();
            let this_for_input = this.clone();
            let this_for_tip = this.clone();
            let tip_duration = dwell.tip_duration;
            cx.new(|vcx| {
                TerminalView::new(
                    ("terminal", pty_id),
                    emulator.clone(),
                    focus.clone(),
                    style.clone(),
                    theme.clone(),
                    vcx,
                )
                // 查找命中的底色/描边由渲染层自己画,宿主只管开关引擎
                .search(search.clone())
                .on_grid_resize(move |size: TermSize, _window, cx| {
                    // grid 尺寸是渲染侧量出来的(可用像素 ÷ cell 尺寸),PTY 必须跟着改,
                    // 否则 shell 换行位置与画面对不上。grid 当帧就改,PTY 停稳再改
                    // (见 `PTY_RESIZE_SETTLE`)。
                    let _ = this.update(cx, |pane: &mut TerminalPane, cx| {
                        pane.schedule_pty_resize(size.screen_lines as u16, size.columns as u16, cx);
                    });
                })
                // **唯一**的写 PTY 通道:键盘 / 粘贴 / IME 提交 / 鼠标上报 /
                // alt screen 滚轮全走这里,`write()` 里的 AI 感知旁路一处不落。
                .on_input(move |bytes, window, cx| {
                    let bytes = bytes.to_vec();
                    let _ = this_for_input.update(cx, |pane: &mut TerminalPane, cx| {
                        // 本地 shell 退出后的 Enter = 重新启动(判据见 `enter_restarts`);
                        // 别的键照旧往(已死的)PTY 写,与此前一致
                        if !pane.remote && enter_restarts(&bytes, pane.dead_since, Instant::now()) {
                            pane.request_restart(window, cx);
                            return;
                        }
                        pane.write(&bytes, cx);
                    });
                })
                // Ctrl+滚轮缩放全局终端字号(键盘那三条在 render 的 `on_action` 上)
                .on_zoom(|step, _window, cx| {
                    let zoom = if step > 0 {
                        TerminalZoom::In
                    } else {
                        TerminalZoom::Out
                    };
                    zoom_terminal_font(zoom, cx);
                })
                // 拖选停留自动复制(`config.selectionAutoCopySecs`)。剪贴板由
                // mt-ui 写,宿主只负责那颗「已复制」气泡:origin 是**元素相对**
                // 坐标(mt-ui 已按容器宽度贴边收拢),分屏右侧也不会算歪。
                // 长文本粘贴转文件(audit #30)。视图把控制权交出来,
                // 阈值/落盘/路径映射全在 [`resolve_paste`] 里 —— 那需要 AppConfig,
                // mt-ui 不该知道它。
                .on_paste(move |_window, cx| resolve_paste(pty_id, cx))
                // 「智能 Ctrl+C / Ctrl+V」的开关**每次按键现问 store**:
                // 设置页一改立刻生效,不必再造一条「配置变了挨个终端下发」的链路
                // (字号/主题那几条都得那么做,这条不用)。
                .smart_copy_paste(|cx: &gpui::App| {
                    AppStore::global(cx).read(cx).config().smart_copy_paste
                })
                // Ctrl(⌘)+悬停 / 单击输出里的文件路径 → 文件页新开页签;网址 → 默认
                // 浏览器。视图只认形状,「指向哪个文件、文件页开不开得了」全在
                // `terminal_links`
                .file_links(
                    move |cand, cx| crate::terminal_links::resolve(pty_id, cand, cx),
                    crate::terminal_links::open,
                    crate::terminal_links::open_url,
                )
                // 不按修饰键停在链接上:弱化下划线 +「Ctrl+单击打开」(文案由宿主注入,
                // mt-ui 不依赖 mt-i18n;每次画都现取,切语言立刻生效)
                .link_hover_hint(|| SharedString::from(link_hover_hint_label()))
                .selection_dwell(dwell)
                .on_selection_copied(move |_text, origin, _window, cx| {
                    let _ = this_for_tip.update(cx, |pane: &mut TerminalPane, cx| {
                        pane.copied_tip = Some(origin);
                        cx.notify();
                        // 1s 后自撤(旧版 tipTimer 就是这么做的);句柄存回字段,
                        // 连着复制两次时上一个计时器随之被丢弃
                        pane._tip_timer = Some(cx.spawn(async move |pane, cx| {
                            cx.background_executor().timer(tip_duration).await;
                            let _ = pane.update(cx, |pane: &mut TerminalPane, cx| {
                                pane.copied_tip = None;
                                cx.notify();
                            });
                        }));
                    });
                })
            })
        };

        ai.add_pane(pty_id);

        let view_sub = cx.observe(&view, |this: &mut Self, _, cx| {
            let scrolled = this
                .emulator
                .with_term(|term| term.grid().display_offset() > 0);
            if scrolled != this.scrolled_back {
                cx.notify();
            }
        });

        let ssh_activity = {
            let focus = focus.clone();
            let mono_font = style.font_family.clone();
            cx.new(|_| SshActivityView::new(pty_id, focus, mono_font))
        };

        Self {
            pty_id,
            emulator,
            pty: PtySlot::starting(),
            _spawn: spawn,
            spawn_waiters: Vec::new(),
            focus,
            view,
            style,
            theme,
            ai,
            remote: false,
            exited: false,
            exit_code: None,
            dead_since: None,
            restart_requested: false,
            scrolled_back: false,
            unseen_output: false,
            scrollback_purge_until: None,
            spawn_error: None,
            copied_tip: None,
            _tip_timer: None,
            search,
            search_bar: None,
            search_exit: None,
            search_reveal: RevealState::new(),
            _search_sub: None,
            _view_sub: view_sub,
            _flash_timer: None,
            pending_marks: Vec::new(),
            _marks_timer: None,
            pending_osc_title: None,
            _osc_title_timer: None,
            pending_pty_size: None,
            _pty_resize_timer: None,
            sync_release_armed: false,
            _sync_release_timer: None,
            _wake: wake,
            ssh_activity,
        }
    }

    /// SSH 交互浮层的视图(`ssh_activity` 的事件泵经它投递)。
    pub fn ssh_activity(&self) -> &Entity<SshActivityView> {
        &self.ssh_activity
    }

    /// 清掉 SSH 交互记录(关 pane / 重启换 PTY,见 `AppStore::dispose_terminal`)。
    pub fn clear_ssh_activity(&mut self, cx: &mut Context<Self>) {
        self.ssh_activity.update(cx, |view, cx| view.clear(cx));
    }

    /// 标记这是 SSH 远程项目的 pane(store 建 pane 时调,见 `start_pty_with`)。
    /// 远程 pane 退出后不出底部「重新启动」条、Enter 也不重启 —— 断线重连归
    /// `terminal_area` 的整屏遮罩,行为与此前一致。
    pub fn with_remote(mut self, remote: bool) -> Self {
        self.remote = remote;
        self
    }

    /// 有同步块在攒就排一个到期时刻的定时器(缘由见模块注释「同步输出的超时释放」)。
    ///
    /// **已经排着就不动**:定时器醒来会重新读当前的到期时刻 —— 被新的 BSU 续过期就接着睡,
    /// 结束标记到过了就收摊,所以旧定时器不可能把续过期的那一帧提前放出来,连着来的同步
    /// 块也不必一帧换一个任务。
    ///
    /// 没有漏排的窗口:reader 线程是先把字节推进状态机、再发唤醒信号的,而这里与定时器
    /// 收摊都在主线程上 —— 定时器看到没有同步块才落下 `sync_release_armed`,这之后开的同步块
    /// 的信号还在队列里,等它收完摊才轮到这里。
    fn arm_sync_release(&mut self, cx: &mut Context<Self>) {
        if self.sync_release_armed {
            return;
        }
        let Some(mut deadline) = self.emulator.sync_deadline() else {
            return;
        };
        self.sync_release_armed = true;
        // 不在任务里清自己的句柄(从运行中的任务里丢掉自己的 `Task` 是在给自己拆脚手架),
        // 下次排定时器时句柄自然被顶掉
        self._sync_release_timer = Some(cx.spawn(async move |pane, cx| {
            loop {
                cx.background_executor()
                    .timer(deadline.saturating_duration_since(Instant::now()))
                    .await;
                let outcome = pane.update(cx, |pane: &mut TerminalPane, cx| {
                    let outcome = pane.emulator.release_expired_sync();
                    if !matches!(outcome, SyncRelease::Pending(_)) {
                        pane.sync_release_armed = false;
                    }
                    if outcome == SyncRelease::Released {
                        // 攒在块里的查询这时才跑,应答不能等下一次输出
                        pane.drain_term_events(cx);
                    }
                    outcome
                });
                match outcome {
                    Ok(SyncRelease::Pending(next)) => deadline = next,
                    Ok(SyncRelease::Released) => {
                        // 走节拍器,不在 `pane.update` 里调:它当场 flush 时要 update 这个 pane
                        cx.update(|cx| redraw::request(pane.clone(), cx));
                        return;
                    }
                    Ok(SyncRelease::Idle) | Err(_) => return,
                }
            }
        }));
    }

    /// grid 尺寸变了:停稳后再下发给 PTY,时机见 [`PTY_RESIZE_SETTLE`]。
    fn schedule_pty_resize(&mut self, rows: u16, cols: u16, cx: &mut Context<Self>) {
        // 会话还在后台起:`PtySlot` 只记最新一次、回填时补下发,本来就不会逐列下发;
        // 直接交给它,回填拿到的就是此刻的真实尺寸
        if self.pty.phase() != PtyPhase::Running {
            self.pending_pty_size = None;
            self._pty_resize_timer = None;
            self.apply_pty_resize(rows, cols);
            return;
        }
        self.pending_pty_size = Some((rows, cols));
        // 换掉旧计时器 = 重新计时(尾沿去抖)
        self._pty_resize_timer = Some(cx.spawn(async move |pane, cx| {
            loop {
                cx.background_executor().timer(PTY_RESIZE_SETTLE).await;
                let settled = pane.update(cx, |pane: &mut TerminalPane, cx| {
                    // 分隔条还按着:等松手
                    if cx.has_active_drag() {
                        return false;
                    }
                    if let Some((rows, cols)) = pane.pending_pty_size.take() {
                        pane.apply_pty_resize(rows, cols);
                    }
                    true
                });
                // 已下发,或 pane 已释放
                if settled.unwrap_or(true) {
                    return;
                }
            }
        }));
    }

    /// 真正下发 resize。只有**真实下发**的 resize 才开 AI 感知的重绘冷却窗口:
    /// 同尺寸的 resize 不会引起 TUI 重绘,平白开冷却会漏掉真的 AI 活跃。
    fn apply_pty_resize(&mut self, rows: u16, cols: u16) {
        // 子进程已经退出:PTY 那头没人收 resize。退出条占掉两行时 grid 照改(内容挤进回滚),
        // 不必再去碰 ConPTY,也不必给已撤出轮询的 pane 开 AI 重绘冷却
        if self.exited {
            return;
        }
        match self.pty.resize(rows, cols) {
            Ok(true) => self.ai.perception().note_resize(self.pty_id),
            Ok(false) => {}
            Err(err) => tracing::warn!("[pane {}] resize 失败: {err:#}", self.pty_id),
        }
    }

    /// 后台起 PTY 的结果回到主线程(只由 `new` 里那条前台任务调用)。
    ///
    /// 成功:回填(补 resize → 按序冲刷空窗期的写入,见 [`PtySlot::backfill`])→
    /// WSL 提示 → 答复等回执的人 → 交还续接反查所得的 cwd → 补报空窗期挂着的退出。
    /// 失败:落 `spawn_error` 画一行红字(与此前同步起失败同一效果),并发
    /// [`PaneEvent::SpawnFailed`] 让页签亮红叉。
    /// 回填前 pane 已关闭(`shutdown` 过):会话退回来当场丢弃(= kill),什么都不报。
    fn finish_spawn(&mut self, outcome: SpawnOutcome, cx: &mut Context<Self>) {
        let SpawnOutcome {
            session,
            resume_cwd,
        } = outcome;
        let exit = match session {
            Ok(session) => {
                let wsl = session.wsl_override().cloned();
                let filled = match self.pty.backfill(session) {
                    Ok(filled) => filled,
                    Err(session) => {
                        // `PtySession::drop`:先 kill 子进程,再把 master 丢到后台销毁
                        drop(session);
                        return;
                    }
                };
                match filled.resized {
                    Some(Ok(true)) => self.ai.perception().note_resize(self.pty_id),
                    Some(Err(err)) => tracing::warn!("[pane {}] resize 失败: {err:#}", self.pty_id),
                    Some(Ok(false)) | None => {}
                }
                for err in &filled.write_errors {
                    tracing::error!("[pane {}] 写 PTY 失败: {err:#}", self.pty_id);
                }
                // WSL 启动器重写的一次性告知(`App.tsx:367-379`)。判定与重写早在
                // `mt_pty::launch::plan` 里做完了,结论挂在会话上 —— 这里只是**唯一的
                // 读取方**(此前全仓零调用,提示因此一直缺着)。
                //
                // 「一次性」= 每个新 PTY 各推一次,不去重(原版同款):同一个项目开两个
                // 终端就该看到两条,那正是「这两个都被改用 wsl.exe 启动了」的意思。
                if let Some(wsl) = wsl {
                    toast::push_wsl_override(&wsl.distro, &wsl.unix_path, cx);
                }
                self.settle_spawn_waiters(true);
                filled.exit
            }
            Err(err) => {
                let msg = format!("{err:#}");
                tracing::error!("[pane {}] PTY 启动失败: {msg}", self.pty_id);
                // 已关闭的 pane 不必再画错误
                if !self.pty.fail() {
                    return;
                }
                self.spawn_error = Some(msg);
                self.dead_since = Some(Instant::now());
                self.settle_spawn_waiters(false);
                cx.emit(PaneEvent::SpawnFailed);
                None
            }
        };
        // 反查结果与起没起来无关(挪后台之前也是起失败照样写回),交还即可
        if let Some(found) = resume_cwd {
            cx.emit(PaneEvent::ResumeCwd(found));
        }
        if let Some(code) = exit {
            self.emit_exit(code, cx);
        }
        cx.notify();
    }

    /// 子进程退出:退出条 + 上报。只从「回填之后」的路径进来(见 `PtySlot::note_exit`)。
    fn emit_exit(&mut self, code: Option<u32>, cx: &mut Context<Self>) {
        self.exited = true;
        self.exit_code = code;
        self.dead_since = Some(Instant::now());
        cx.emit(PaneEvent::Exited(code));
        // 退出是一次性事件:不进节拍器,当场画完收工
        cx.notify();
    }

    /// 唤醒循环收到了新输出。回看中(上一帧 `display_offset > 0`)就给「回到底部」浮钮
    /// 点上小圆点。不在这里读 grid:上一帧的回看状态够用,省一次抢 term 锁。
    fn note_output(&mut self) {
        if self.scrolled_back {
            self.unseen_output = true;
        }
    }

    /// 「清除缓冲」发出 Ctrl+L 之后的收尾:窗口期里来的输出(shell 的清屏重画)把屏上
    /// 旧内容推进了回看,再清一次。过了窗口期就收手,之后的输出照常进回看。
    fn purge_cleared_scrollback(&mut self) {
        let Some(until) = self.scrollback_purge_until else {
            return;
        };
        if Instant::now() > until {
            self.scrollback_purge_until = None;
            return;
        }
        self.emulator.clear_scrollback();
    }

    /// 右键「清除缓冲」。三档口径见模块注释「右键「清除缓冲」」与 [`clear_buffer_plan`]。
    ///
    /// AI 会话的判据取 pane **当下**的状态(口径同 [`crate::pane_actions::is_ai_alive`]),
    /// shell 认的是配置里那条 shell 的可执行文件(认不出再看名字)。
    fn clear_buffer(&mut self, cx: &mut Context<Self>) {
        let (ai_alive, shell) = {
            let store = AppStore::global(cx).read(cx);
            let pane = store
                .pane_of_pty(self.pty_id)
                .and_then(|(pid, pane_id)| store.project_state(&pid)?.pane(&pane_id).cloned());
            let ai_alive = pane
                .as_ref()
                .is_some_and(|p| crate::pane_actions::is_ai_alive(p.status));
            let shell = pane.map_or(ShellKind::Unknown, |p| {
                shell_kind_of(&store.config().available_shells, &p.shell_name)
            });
            (ai_alive, shell)
        };
        let plan = clear_buffer_plan(
            self.dead_since.is_some(),
            ai_alive,
            self.in_alt_screen(),
            self.remote,
            shell,
        );
        match plan {
            ClearBufferPlan::LocalAll => self.emulator.clear_all(),
            ClearBufferPlan::ScrollbackOnly => self.emulator.clear_scrollback(),
            ClearBufferPlan::ShellRedraw => {
                self.emulator.clear_scrollback();
                self.scrollback_purge_until = Some(Instant::now() + CLEAR_REDRAW_WINDOW);
                // 当作用户按了一下 Ctrl+L:走 `write`,AI 输入旁路照常看得到(它不认这个字节)
                self.write(b"\x0c", cx);
            }
        }
        // 回看没了,滚动条与「回到底部」浮钮都要跟着变
        self.unseen_output = false;
        self.view.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// 本帧的回看状态 `(在回看, 回看期间有新输出)`,顺手刷新唤醒循环要看的那一位。
    ///
    /// 回看位置在 render 里现读:滚轮 / 翻页键改的是终端视图自己,它 `notify` 时 gpui 会把
    /// 祖先 view(本 pane)一并标脏重画;新输出由节拍器 notify 本 pane —— 两条路都会走到
    /// 这里。alt screen 的 grid 没有回看缓冲,`display_offset` 恒 0,浮钮自然不出。
    fn sync_scrollback_state(&mut self) -> (bool, bool) {
        let offset = self.emulator.with_term(|term| term.grid().display_offset());
        self.scrolled_back = offset > 0;
        if !self.scrolled_back {
            self.unseen_output = false;
        }
        (self.scrolled_back, self.unseen_output)
    }

    /// 节拍器通知本 pane 有新输出需要重绘。
    ///
    /// gpui-fast 下子视图 `TerminalView` 默认保留复用,只 notify 外壳 `TerminalPane`
    /// 不会重绘 `Arc<TerminalEmulator>` 里的新 grid —— 必须直接标脏 `self.view`;
    /// 回看浮钮状态未变时不惊动外壳,让 gpui-fast 直接拼接 `TerminalView`。
    pub(crate) fn notify_redraw(&mut self, cx: &mut Context<Self>) {
        self.view.update(cx, |_, cx| cx.notify());
        let scrolled = self
            .emulator
            .with_term(|term| term.grid().display_offset() > 0);
        if scrolled != self.scrolled_back || (scrolled && self.unseen_output) {
            cx.notify();
        }
    }

    /// 「回到底部」浮钮:滚回底部、熄掉小圆点、把键盘还给终端(点浮钮不该让人再去点一下
    /// 终端才能接着打字)。
    fn scroll_to_bottom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.emulator
            .with_term_mut(|term| term.scroll_display(Scroll::Bottom));
        self.unseen_output = false;
        window.focus(&self.focus, cx);
        self.view.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// 「重新启动」(按钮 / 退出后的 Enter):在**同一个 pane** 上按原 shell 与 cwd 起一条
    /// 新 PTY。
    ///
    /// 动作复用远程重连那一条 [`AppStore::reset_pane_for_reconnect`](它本身不分本地 /
    /// 远程):回收旧 PTY(连同标记 / 退出登记)、就地起新的、pane 回落 idle。旧回滚随旧
    /// 终端一起销毁,理由见那边的注释。重启后焦点送进新终端。
    ///
    /// 必须 `window.defer`:调用方是本 pane 的监听器 / 输入回调,而
    /// `reset_pane_for_reconnect` 里的 `dispose_terminal` 要 update 这个 pane ——
    /// 同一实体的嵌套 update 当场 panic。
    fn request_restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.restart_requested || self.dead_since.is_none() {
            return;
        }
        self.restart_requested = true;
        let pty_id = self.pty_id;
        window.defer(cx, move |window, cx| {
            let store = AppStore::global(cx);
            let Some((project_id, pane_id)) = store.read(cx).pane_of_pty(pty_id) else {
                return;
            };
            store.update(cx, |store, cx| {
                if store
                    .reset_pane_for_reconnect(&project_id, &pane_id, cx)
                    .is_some()
                {
                    store.focus_pane(&project_id, &pane_id, window, cx);
                }
            });
        });
    }

    /// 退出条上的「关闭」:走关终端的唯一入口 [`crate::pane_actions::close_pane`] ——
    /// PTY 已死,免确认直接关。
    ///
    /// 连点闸:关掉之后补位上来的若也是个已退出的终端,它的「关闭」正好落在同一处,
    /// 连点的第二下会把它连同回滚一起关掉。这颗按钮不在 `TerminalArea` 里(那边的闸记在
    /// 它自己身上),所以单记一个进程内的时刻,口径同一个
    /// [`crate::pane_actions::swallows_close_click`]。
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        let last = LAST_EXIT_BAR_CLOSE.with(|at| at.replace(Some(now)));
        let store = AppStore::global(cx);
        if crate::pane_actions::swallows_close_click(store.read(cx).config(), true, last, now) {
            return;
        }
        let Some((project_id, pane_id)) = store.read(cx).pane_of_pty(self.pty_id) else {
            return;
        };
        crate::pane_actions::close_pane(store, project_id, pane_id, window, cx);
    }

    /// PTY 起没起来,定局后答复(`true` = 起来了)。
    ///
    /// 给**回执**用的:移动端发起会话要区分「pane 建出来了」与「PTY 真的起来了」——
    /// [`Self::write`] 在起失败后是静默丢弃的,不看这一条就会把「终端起不来」报成
    /// 成功,手机侧只能干等 15s 超时。PTY 在后台起,pane 建完那一刻还没有结论,
    /// 所以这是个等待口而不是同步查询:已定局则接收端立刻就绪;还在起就排队,
    /// 定局时统一答复;pane 在定局前没了,sender 随之丢弃,接收端拿到 `Canceled`
    /// —— 调用方按失败算。
    pub fn spawn_settled(&mut self) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        match self.pty.phase() {
            PtyPhase::Starting => self.spawn_waiters.push(tx),
            phase => {
                let _ = tx.send(phase == PtyPhase::Running);
            }
        }
        rx
    }

    fn settle_spawn_waiters(&mut self, alive: bool) {
        for tx in self.spawn_waiters.drain(..) {
            let _ = tx.send(alive);
        }
    }

    /// grid 的只读句柄。给悬停缩略图([`crate::pane_preview`])用 ——
    /// [`mt_ui::MiniTerminalElement`] 只读不写、不 resize、不接输入。
    ///
    /// ⚠️ 别拿它去建第二个 [`mt_ui::TerminalElement`]:那个件会在 prepaint 里
    /// 按自己的可用像素 `resize` emulator,等于让缩略图去改真终端的行列。
    pub fn emulator(&self) -> Arc<TerminalEmulator> {
        self.emulator.clone()
    }

    /// 当前终端配色。缩略图要用同一份,否则浮层里的画面配色与切过去看到的不一致。
    pub fn theme(&self) -> &TerminalTheme {
        &self.theme
    }

    /// Ctrl+F。打开查找条:终端里有单行、非空白的选区就用它预填并当场按它查找,没有就
    /// 留着上次的词;输入框全选,直接打字即覆盖。已经开着(焦点在终端上)时同样预填 / 全选,
    /// 并把焦点送回输入框。预填口径见 [`mt_ui::terminal::search_bar::search_prefill`]。
    ///
    /// # 与原版的两处口径差
    ///
    /// 1. **逐 pane 一条,不是全局单例**。原版 `TerminalSearchBar` 是 portal 到
    ///    body 的单例,靠 rAF 每帧量目标 pane 的矩形贴过去,换 pane 就把上一条挪走。
    ///    GPUI 侧查找条是终端容器里的 `absolute` 子元素,分屏/拖分隔条/切 tab 全由
    ///    布局自动跟随 —— 单例反而要额外簿记「现在贴着谁」。代价:两个分屏可以各开
    ///    一条(各搜各的),原版做不到。
    /// 2. **不是 toggle**。原版 `openTerminalSearch()` 只开不关(再按一次是「回到
    ///    查找条接着改关键词」,焦点在输入框里时那一下压根到不了全局 handler),
    ///    关闭走 Esc / `✕`。这里照此:第二次按 Ctrl+F = 选区预填 + 聚焦 + 全选。
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bar) = self.search_bar.clone() {
            bar.update(cx, |bar, cx| bar.open(window, cx));
            return;
        }
        // 覆盖物栈里登记一条(按 pty_id 区分)。它**不挡**全局快捷键,
        // 只是防叠开 + 让「现在压着什么」有唯一真相,见 `overlay` 模块注释。
        if !overlay::push(overlay::terminal_search(self.pty_id)) {
            return;
        }
        // 收起动画播到一半又打开:接着用那一条,从当前进度往回长
        let bar = match self.search_exit.take() {
            Some(bar) => bar,
            None => {
                let search = self.search.clone();
                let emulator = self.emulator.clone();
                let this = cx.weak_entity();
                cx.new(|cx| {
                    // 文案由宿主注入(mt-ui 不依赖 mt-i18n):查找条每帧调一次,切语言立刻生效
                    let labels = crate::i18n::terminal_search_labels;
                    TerminalSearchBar::new(search, emulator, labels, window, cx).on_close(
                        move |window, cx| {
                            let _ = this.update(cx, |pane: &mut TerminalPane, cx| {
                                pane.dismiss_search(window, cx);
                            });
                        },
                    )
                })
            }
        };
        // 选区预填 + 开引擎 + 按关键词搜一遍 + 聚焦全选
        bar.update(cx, |bar, cx| bar.open(window, cx));
        self._search_sub = Some(cx.subscribe(
            &bar,
            |this: &mut Self, _, _event: &SearchBarEvent, cx| {
                this.view.update(cx, |_, cx| cx.notify());
                let scrolled = this
                    .emulator
                    .with_term(|term| term.grid().display_offset() > 0);
                if scrolled != this.scrolled_back {
                    cx.notify();
                }
            },
        ));
        self.search_bar = Some(bar);
        // 开合动画(点按钮与 Ctrl+F 都走这里)
        self.search_reveal.show(cx);
        self.view.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// 标题栏「查找」按钮上的探针报来按钮窗口坐标(查找条开合动画的原点)。返回「变了、且
    /// 查找条开着或在动画中」—— 只有那时才要再画一帧。
    pub fn note_search_anchor(&mut self, bounds: Bounds<Pixels>) -> bool {
        self.search_reveal.note_anchor(bounds)
    }

    /// 收起查找条(Esc / `✕` 都走这里)。
    ///
    /// ⚠️ **焦点必须还给终端**:不还的话焦点停在已卸载的输入框上,用户接着敲的字
    /// 全部落空,还得先用鼠标点一下终端才能继续 —— 原版 `closeTerminalSearch()`
    /// 里那句 `term.focus()` 就是为这个。
    ///
    /// 收起播动画:查找条挪进 `search_exit` 留在树上,播完才丢(见 render)。焦点**一开始就**
    /// 还给终端,正在收的查找条不再接按键,也不再接鼠标(`crate::popover_reveal` 收起时盖住内容)。
    fn dismiss_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = self.search_bar.take() else {
            return;
        };
        overlay::pop(overlay::terminal_search(self.pty_id));
        self.search_exit = Some(bar);
        self.search_reveal.hide(cx);
        window.focus(&self.focus, cx);
        self.view.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// 往 PTY 写字节。
    ///
    /// **`observe_input` 必须在字节交给 PTY 之前调** —— 焦点冷却窗口要早于 TUI 对
    /// 焦点事件的重绘响应抵达,否则那波重绘会被当成 AI 活跃(与原 `write_pty` 同序)。
    ///
    /// PTY 还在后台起时字节进队列,回填时按到达顺序冲刷(旁路照旧**当场**跑,
    /// AI 输入识别看到的时刻不变);起失败 / 已关闭时静默丢弃(与此前没有 PTY 时同)。
    pub fn write(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        self.observe_user_write(bytes, cx);
        if let Err(err) = self.pty.write(bytes) {
            tracing::error!("[pane {}] 写 PTY 失败: {err:#}", self.pty_id);
        }
    }

    /// 终端右键「SSH 连接」:写入 `ssh …\r` 命令行,带密码时**写完再**注册自动填充
    /// (`disarm_on_input = true`,时序论证见
    /// [`mt_pty::PtySession::write_then_arm_ssh_autofill`])。输入旁路与 [`write`](Self::write) 一致。
    ///
    /// PTY 还在后台起时,「写入 + 注册」作为**一个**队列元素排队,回填冲刷时整体交给
    /// 那个原子方法,前后的写入插不进它中间(见 [`crate::pty_slot`])。
    /// 起失败 / 已关闭时静默不做。
    pub fn write_ssh_command(
        &mut self,
        line: &[u8],
        password: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(password) = password else {
            self.write(line, cx);
            return;
        };
        self.observe_user_write(line, cx);
        if let Err(err) = self.pty.write_then_arm_ssh_autofill(line, password) {
            tracing::error!("[pane {}] 写 PTY 失败: {err:#}", self.pty_id);
        }
    }

    /// 用户写入交给 PTY **之前**的旁路:AI 输入识别、`UserInput` 事件、AI 任务标记。
    fn observe_user_write(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        // 行快照:↑ 历史召回 / Tab 补全会让 shell 整行改写,本地输入缓冲重建不出来,
        // 只能在回车前抓一份当前可见行补判(见 observe_input_with_line_snapshot)。
        let snapshot = if self.ai.perception().wants_line_snapshot(bytes) {
            self.current_line()
        } else {
            None
        };
        self.ai.perception().observe_input_with_line_snapshot(
            self.pty_id,
            bytes,
            snapshot.as_deref(),
        );
        cx.emit(PaneEvent::UserInput);
        // AI 任务标记:**正文必须在这里取**(`observe_input` 是同步的,回车那一刻
        // `pending_submits` 里已经有这条了,而 `drain_submits` 取走即清);
        // **锚点则必须延后**,理由见 [`mt_terminal::TerminalEmulator::arm_cursor_floor`]。
        if let Some(submits) = self.take_submits() {
            self.arm_marks(submits, cx);
        }
    }

    /// 取走这一轮的用户提交。`None` = 没有提交 / 不该打点。
    ///
    /// **alt screen 一律跳过**(照抄 `terminalCache.ts:554-557`):alt grid 的
    /// `max_scroll_limit` 是 0,没有回看缓冲,打了也无处可跳 —— 走 alt screen 的
    /// AI(Codex 这类 ratatui 应用)全落在这个分支。注意 `drain_submits` 是
    /// **取走即清**,所以这一句要放在闸门之后:提前抽干等于把 alt screen 期间的
    /// 提交默默吞掉,退出 TUI 后也补不回来。
    fn take_submits(&self) -> Option<Vec<MarkerSubmit>> {
        if self.emulator.mode().contains(TermMode::ALT_SCREEN) {
            return None;
        }
        let submits: Vec<MarkerSubmit> = self
            .ai
            .perception()
            .drain_submits(self.pty_id)
            .into_iter()
            .map(|s| MarkerSubmit {
                line: s.line,
                ts: s.ts,
                // 从屏幕上猜来的正文要验明正身之后才示人,见 markers 模块注释
                // 的「第四个破绽」
                confirmed: !s.from_snapshot,
            })
            .collect();
        (!submits.is_empty()).then_some(submits)
    }

    /// 收下一批提交,武装光标水位追踪,到点再定锚。
    ///
    /// 为什么不当场定锚见 [`mt_terminal::TerminalEmulator::arm_cursor_floor`]:
    /// Ink 应用等待输入时光标停在渲染块**下方**,提交那一下才会把光标顶回块首 ——
    /// 而块首正是 `> 用户输入` 这条消息落地的行。
    fn arm_marks(&mut self, submits: Vec<MarkerSubmit>, cx: &mut Context<Self>) {
        // 窗口里又按了一次 Enter:先把上一批按现有水位结清,两批的先后顺序不能乱
        self.settle_marks(cx);
        self.pending_marks = submits;
        self.emulator.arm_cursor_floor();
        self._marks_timer = Some(cx.spawn(async move |pane, cx| {
            cx.background_executor().timer(MARK_SETTLE_DELAY).await;
            let _ = pane.update(cx, |pane: &mut TerminalPane, cx| pane.settle_marks(cx));
        }));
    }

    /// 定锚并把这批标记发出去。没有待定的就是空操作(计时器到点 / 又一次 Enter
    /// 抢先结算 / pane 关掉,三条路都可能重入)。
    fn settle_marks(&mut self, cx: &mut Context<Self>) {
        self._marks_timer = None;
        let floor = self.emulator.take_cursor_floor();
        let submits = std::mem::take(&mut self.pending_marks);
        if submits.is_empty() {
            return;
        }
        // 窗口期里切进了 TUI:`history_size` 读的是备用 grid(恒为 0),锚点无从
        // 谈起 —— 与 `take_submits` 的闸门同口径,整批丢掉
        if self.emulator.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        let Some(floor) = floor else {
            return;
        };
        let history = self.emulator.with_term(|term| term.history_size() as i32);
        // 等了 MARK_SETTLE_DELAY 之后再取,是因为 `> 用户输入` 那条 static 消息要在
        // erase 顶回块首之后才打出来。但**它未必真的打出来了**:AI 正忙时这一句是
        // 被排进队列的,那 200ms 里水位只落得到还在重绘的动态区上 —— 拿那一行的指纹
        // 当锚点,下一次校验必然对不上,这条标记就凭空消失了。所以定不住就先挂起,
        // 等 `relocate_pending` 补,见 [`crate::markers`] 模块注释第三个破绽。
        let text = self.emulator.line_text(floor);
        let anchor = {
            // **只拿确凿的正文去判定**。从屏幕上猜来的那些不许参与:候选取自光标
            // 所在行,而水位在 agent 还没重绘时也落在那一行 —— 让它走定锚判定就是
            // 拿输入框证明输入框。整批都是猜的就一律挂起,等 relocate 在别处找到
            // 才算数(见 markers 模块注释的「第四个破绽」)
            let heads: Vec<&str> = submits
                .iter()
                .filter(|s| s.confirmed)
                .map(|s| s.line.as_str())
                .collect();
            if heads.is_empty() {
                markers::MarkerAnchor::Pending { from: floor }
            } else {
                markers::settle_anchor(floor, text.as_deref(), &heads)
            }
        };
        cx.emit(PaneEvent::AiMarks(MarkerBatch {
            submits,
            anchor,
            history,
            max_scrollback: self.emulator.scrollback() as i32,
        }));
    }

    /// 现在读得到主屏的行吗 —— [`Self::line_fingerprint`] 的前置闸门。
    ///
    /// ⚠️ **alt screen 期间必须返回 false**:那时 `line_text` 读的是**备用 grid**,
    /// 主屏攒下的锚点一行都读不到、指纹全变 `None`,[`crate::markers::prune_stale`]
    /// 会把整份标记误杀。与 [`Self::scrollback_state`] 里那句 `(0, 0)` 是同一个坑、
    /// 同一条处置:**TUI 期间干脆不校验**(主屏内容在 TUI 期间原封不动,退出后
    /// 指纹照样对得上)。
    pub fn can_probe_lines(&self) -> bool {
        !self.emulator.mode().contains(TermMode::ALT_SCREEN)
    }

    /// 某个锚点当前指向那一行的内容指纹。`None` = 那一行已不在缓冲区里。
    ///
    /// 定锚(上面)与校验([`crate::markers::prune_stale`])共用这一个口,取法不会
    /// 走岔。为什么需要它见 [`crate::markers`] 模块注释的「第二个破绽」。
    ///
    /// 调用前先过 [`Self::can_probe_lines`]。
    /// 某个绝对行当前的文本 —— [`crate::markers::relocate_pending`] 回扫用的探针。
    ///
    /// 与 [`Self::line_fingerprint`] 同一个读回口,只是补锚要拿原文做匹配、不是比指纹。
    /// 调用前先过 [`Self::can_probe_lines`]。
    pub fn line_text(&self, row: i32) -> Option<String> {
        self.emulator.line_text(row)
    }

    /// 回扫的边界:`(最底下那一行的绝对行号, 可视区行数)`。
    ///
    /// 底行是 `history + screen_lines - 1` ——
    /// [`mt_terminal::TerminalEmulator::line_text`] 的合法区间是
    /// `[0, history + screen_lines)`,再往下就是 `None`。可视区行数给
    /// [`crate::markers::relocate_pending`] 决定「推进起点时留多少行不算已扫」。
    ///
    /// 两个量**一次持锁取齐**:分两次读的话中间可能滚过一批输出,底行与屏高对不上。
    pub fn scan_bounds(&self) -> (i32, i32) {
        self.emulator.with_term(|term| {
            let viewport = term.screen_lines() as i32;
            ((term.history_size() as i32 + viewport) - 1, viewport)
        })
    }

    pub fn line_fingerprint(&self, anchor: i32) -> Option<u64> {
        self.emulator
            .line_text(anchor)
            .map(|text| crate::markers::fingerprint_line(&text))
    }

    /// 当前的 `(history_size, max_scroll_limit)` —— store 侧剪枝的判据。
    ///
    /// alt screen 期间 `history_size` 读的是**备用 grid**(恒为 0),那会让剪枝
    /// 误判,所以这里直接如实回报 `(0, 0)`:[`crate::markers::is_saturated`] 对
    /// `max <= 0` 不判废,等于「TUI 期间不剪枝」——正是我们要的(主屏 scrollback
    /// 在 TUI 期间原封不动,退出后标记照样有效)。
    pub fn scrollback_state(&self) -> (i32, i32) {
        if self.emulator.mode().contains(TermMode::ALT_SCREEN) {
            return (0, 0);
        }
        let history = self.emulator.with_term(|term| term.history_size() as i32);
        (history, self.emulator.scrollback() as i32)
    }

    /// 跳到某条标记:把那一行滚到**视口顶部**并闪 300ms。
    ///
    /// 与终端查找的 `scroll_to_current`(「已在视口里就一动不动,否则滚到视口中间」)
    /// **语义不同**:原版 `scrollToMarker` 调的是 `term.scrollToLine(marker.line)`,
    /// 贴视口顶部且**无条件滚动**(哪怕这一行已经在视口里)。别照抄查找那一份。
    ///
    /// alt screen 期间不动:`scroll_display` 作用在当前 grid 上,TUI 里滚它既没有
    /// 回看缓冲、画面也不是主屏,纯属乱动。返回 `false` = 这次没跳(调用方据此
    /// **不推进游标** —— 连按方向键不该在跳不动的时候空走格子)。
    pub fn scroll_to_marker(&mut self, anchor: i32, cx: &mut Context<Self>) -> bool {
        if self.emulator.mode().contains(TermMode::ALT_SCREEN) {
            return false;
        }
        let line = self.emulator.with_term_mut(|term| {
            let history = term.history_size() as i32;
            let line = crate::markers::marker_line(anchor, history);
            let offset = term.grid().display_offset() as i32;
            let delta = scroll_delta_to_top(line, offset, history);
            if delta != 0 {
                term.scroll_display(Scroll::Delta(delta));
            }
            line
        });
        self.flash_line(line, cx);
        // 闪烁那一行没变时 `set_flash` 不 notify(同一条标记 300ms 内连跳两次),
        // 回看位置却可能刚被滚过 —— 标脏子视图与外壳
        self.view.update(cx, |_, cx| cx.notify());
        cx.notify();
        true
    }

    /// 让某一行整行闪一下,到点自己撤掉(原版是 300ms 后 `decoration.dispose()`)。
    fn flash_line(&mut self, line: i32, cx: &mut Context<Self>) {
        let flash = FlashLine {
            line,
            color: gpui::rgba(FLASH_COLOR).into(),
        };
        self.view.update(cx, |view, cx| view.set_flash(Some(flash), cx));
        self._flash_timer = Some(cx.spawn(async move |pane, cx| {
            cx.background_executor().timer(FLASH_DURATION).await;
            let _ = pane.update(cx, |pane: &mut TerminalPane, cx| {
                pane.view.update(cx, |view, cx| view.set_flash(None, cx));
            });
        }));
    }

    /// 光标所在的可见行文本(取不到返回 `None`)。
    fn current_line(&self) -> Option<String> {
        let row = self
            .emulator
            .with_term(|term| term.grid().cursor.point.line.0);
        if row < 0 {
            return None;
        }
        self.emulator.visible_lines().get(row as usize).cloned()
    }

    /// 把 shell 报上来的窗口标题排进限流窗口(见 [`OSC_TITLE_PERIOD`])。
    ///
    /// 窗口内只留**最新**值:第一条标题起一个 250ms 的定时器,窗口期内再来的
    /// 标题只覆盖 `pending_osc_title`、**不重排定时器**(所以是固定窗口节流,
    /// 不是每来一条就顺延的去抖 —— 后者在 spinner 一直改标题时永远不会到点)。
    ///
    /// 到点由任务 `take()` 走 pending 并 `cx.emit`,**不在任务里清自己的句柄** ——
    /// 从运行中的任务里丢掉自己的 `Task` 是在给自己拆脚手架;下次排窗口时
    /// 句柄自然被新任务顶掉。
    fn note_osc_title(&mut self, title: Option<String>, cx: &mut Context<Self>) {
        let armed = self.pending_osc_title.is_some();
        self.pending_osc_title = Some(title);
        if armed {
            return;
        }
        self._osc_title_timer = Some(cx.spawn(async move |pane, cx| {
            cx.background_executor().timer(OSC_TITLE_PERIOD).await;
            let _ = pane.update(cx, |pane: &mut TerminalPane, cx| {
                if let Some(title) = pane.pending_osc_title.take() {
                    cx.emit(PaneEvent::Title(title));
                }
            });
        }));
    }

    /// alacritty 内部产生的事件。**`PtyWrite` 必须处理** —— DA/DSR/光标位置查询
    /// 这些是终端要回给程序的应答,吞掉会让 shell 与 TUI 程序卡在等回应上。
    ///
    /// ⚠️ 标题这两条**不只**来自 OSC 0/2:`Term::set_options`(改回滚行数时)
    /// 会把**当前**标题原样再发一次(有标题发 `Title`、没有发 `ResetTitle`,
    /// 见 alacritty_terminal 0.26 `term/mod.rs:505`)。所以它是幂等的 ——
    /// 改一次设置不会把已经收到的标题抹掉,store 侧「值没变就不动」还会把
    /// 这次重发整个吃掉。
    fn drain_term_events(&mut self, cx: &mut Context<Self>) {
        for event in self.emulator.events().drain() {
            match event {
                // 这些是终端自己的应答,不是用户键入:直接写,不走 AI 输入旁路
                TermEvent::PtyWrite(text) => self.write_raw(text.as_bytes()),
                TermEvent::ClipboardStore(_, text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                TermEvent::ClipboardLoad(_, format) => {
                    let text = cx.read_from_clipboard().and_then(|it| it.text());
                    let payload = format(text.as_deref().unwrap_or(""));
                    self.write_raw(payload.as_bytes());
                }
                // index 不止 0..16:256/257/258 是前景/背景/光标,而且 OSC 4 改过的
                // 调色板要优先于主题 —— 两件事都在 `terminal_color_rgb` 里。
                TermEvent::ColorRequest(index, format) => {
                    let rgb = mt_ui::terminal_color_rgb(&self.emulator, &self.theme, index);
                    self.write_raw(format(rgb).as_bytes());
                }
                TermEvent::TextAreaSizeRequest(format) => {
                    let size = self.emulator.term_size();
                    // 开着 Kitty 图形才报真实像素:关着时维持原样,与现在字节级一致
                    let (cell_width, cell_height) = self
                        .emulator
                        .kitty()
                        .map_or((1, 1), |kitty| kitty.reported_cell_pixels());
                    let payload = format(mt_terminal::alacritty_terminal::event::WindowSize {
                        num_lines: size.screen_lines as u16,
                        num_cols: size.columns as u16,
                        cell_width,
                        cell_height,
                    });
                    self.write_raw(payload.as_bytes());
                }
                // 页签副段的数据源。两条都走同一个限流窗口,顺序即最终值:
                // 窗口内先 Title 后 ResetTitle,推上去的就是 `None`。
                TermEvent::Title(title) => self.note_osc_title(Some(title), cx),
                TermEvent::ResetTitle => self.note_osc_title(None, cx),
                _ => {}
            }
        }
    }

    /// 不经 AI 输入旁路的写入(终端应答 / 内部序列)。
    ///
    /// 走 [`mt_pty::PtySession::write_reply`] 而不是 `write`:应答不是用户按键,
    /// 不能把 SSH 密码自动填充解掉(「SSH 连接」菜单路径里,本地 shell 与 ConPTY
    /// 在 ssh 起来前后都可能发 DA / DSR 查询),也不经过 mt-pty 的输入观察器。
    ///
    /// 回填前也可能走到这里:reader 线程在后台 spawn 一返回就开始交输出,唤醒循环
    /// 可能抢在回填之前处理到 shell 的开场查询 —— 应答与用户输入进同一条队列,
    /// 相对顺序不变。
    fn write_raw(&mut self, bytes: &[u8]) {
        if let Err(err) = self.pty.write_reply(bytes) {
            tracing::error!("[pane {}] 写 PTY 失败: {err:#}", self.pty_id);
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }

    /// 终端此刻是不是在 alt screen(vim / less / htop / Codex 这类全屏程序)。
    /// 右键「SSH 连接」据此决定要不要换新终端,见 [`ssh_connect_target`]。
    pub fn in_alt_screen(&self) -> bool {
        self.emulator.mode().contains(TermMode::ALT_SCREEN)
    }

    /// 当前有没有可复制的选区(空串不算 —— 选中一段空白后「复制」该是灰的)。
    fn has_selection(&self) -> bool {
        self.emulator
            .with_term(|term| term.selection_to_string())
            .is_some_and(|text| !text.is_empty())
    }

    /// 换终端配色(主题包切换 / 亮暗切换)。
    ///
    /// 宿主这份 `theme` 也要更新 —— OSC 调色板应答用得着(宿主已不再 `.bg()`,
    /// 终端区着色由 TerminalArea 根容器单层承担)。
    pub fn set_theme(&mut self, theme: TerminalTheme, cx: &mut Context<Self>) {
        if self.theme == theme {
            return;
        }
        self.theme = theme.clone();
        self.view.update(cx, |view, cx| view.set_theme(theme, cx));
        cx.notify();
    }

    /// 换字号 / 字族(设置页「字体」页的落点)。
    ///
    /// cell 尺寸随之变化,下一帧渲染层会连带 resize grid 与 PTY ——
    /// 与原版改 `term.options.fontSize` 后 fit addon 重排是同一条链路。
    pub fn set_style(&mut self, style: TerminalStyle, cx: &mut Context<Self>) {
        if self.style == style {
            return;
        }
        self.style = style.clone();
        let mono_font = style.font_family.clone();
        self.view.update(cx, |view, cx| view.set_style(style, cx));
        // SSH 交互浮层的命令与输出用同一个等宽字族
        self.ssh_activity
            .update(cx, |view, cx| view.set_mono_font(mono_font, cx));
        cx.notify();
    }

    /// 换拖选停留自动复制时长(`config.selectionAutoCopySecs`)。
    pub fn set_selection_dwell(&mut self, dwell: DwellConfig, cx: &mut Context<Self>) {
        self.view
            .update(cx, |view, cx| view.set_selection_dwell(dwell, cx));
    }

    /// 换回滚行数。调小时 alacritty 当场裁历史并释放内存。
    ///
    /// 不改任何渲染参数,但裁掉历史会改滚动条的长度与位置 —— pane 套着 view 级
    /// 缓存(`terminal_area::cached_terminal`),不 notify 就要等下一次输出才重画。
    pub fn set_scrollback(&mut self, lines: usize, cx: &mut Context<Self>) {
        self.emulator.set_scrollback(lines);
        self.view.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// 丢弃组合中的预编辑串。切 tab / 关 pane 之前调,免得残影留在画面上。
    pub fn clear_preedit(&mut self, cx: &mut Context<Self>) {
        self.view.update(cx, |view, cx| view.clear_preedit(cx));
    }

    /// 关闭 pane:杀子进程 + 清掉 AI 感知里的一切痕迹 + 收掉查找条。
    ///
    /// PTY 还在后台起时:槽位落成 Closed,排队的写入作废;后台之后交回来的会话由
    /// `finish_spawn` 退回丢弃(= kill),不会双开也不会漏关。**这里不丢 `_spawn`**
    /// —— 关 pane 可能正好发生在那条任务自己回填时派发的事件里,从运行中的任务里
    /// 丢掉自己的句柄是在给自己拆脚手架;实体释放时它随字段一起走。
    pub fn shutdown(&mut self) {
        if let Some(mut pty) = self.pty.close()
            && let Err(err) = pty.kill()
        {
            tracing::warn!("[pane {}] kill 失败: {err:#}", self.pty_id);
        }
        self.settle_spawn_waiters(false);
        self.ai.remove_pane(self.pty_id);
        // SSH 交互浮层的 nonce(store 的 dispose 路径也注销一次;实体直接被丢弃的
        // 路径 —— 项目移除 / 应用退出 —— 只走到这里)。幂等
        crate::ssh_activity::forget_pty(self.pty_id);
        self.close_search_state();
    }

    /// 丢掉查找状态(关键词一并清掉)。**不碰焦点** —— 这条路上终端马上就没了,
    /// 与原版 `closeTerminalSearchFor(ptyId)` 同语义(它同样不去 focus 已死的终端)。
    fn close_search_state(&mut self) {
        self.search.borrow_mut().clear();
        if self.search_bar.take().is_some() {
            overlay::pop(overlay::terminal_search(self.pty_id));
        }
        // 终端都没了:不播收起动画,直接卸
        self.search_exit = None;
        self.search_reveal.hide_now();
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        // pane 实体被丢弃(项目移除 / 应用退出)时同样要回收 —— 否则后端留一个
        // 谁也看不见、谁也杀不掉的孤儿子进程。还在后台起的也算:`_spawn` 随字段
        // 一起丢弃,后台已产出的会话随之被丢弃(= kill)。
        if self.pty.is_live() {
            self.shutdown();
        }
        // shutdown 走过就已经摘干净了;这一条兜住「PTY 起失败的 pane 被丢弃」
        // ——覆盖物栈里留一条死登记,那个 pty_id 复用之后查找条就再也开不出来。
        self.close_search_state();
    }
}

impl Focusable for TerminalPane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// 一次粘贴要用到的壳侧上下文(阈值配置 + pane 归属 + 远程连接)。
///
/// 图片与长文本两条路线都要它,且都在**读到剪贴板之前**就得备好 ——
/// 所以单独取一次,不跟着某一条分支走。
struct PasteContext {
    /// 这一次粘贴要不要考虑长文本转文件:总开关 + 「只在 AI 会话里」子开关 + pane 当下
    /// 状态合成的结论([`clipboard::long_paste_wanted`]),阈值另判。**图片不看它**:终端
    /// 本来就粘不了图,不转文件就只剩 `Alt+V`(装机版同款口径)。
    enabled: bool,
    line_threshold: u32,
    char_threshold: u32,
    target: PasteTarget,
    /// toast 的归属项目;取不到就是空串(`push_message` 能吃)。
    project_id: String,
    project_name: String,
    /// 远程 pane 的上传素材:连接 + 远程项目路径。断链时为 `None`。
    remote: Option<(mt_config::SshConnection, String)>,
    remote_paste_dir: String,
    /// pane 里正跑着会自己读剪贴板的 agent(Claude Code):有图就发 `Alt+V`
    /// 交给它,不落盘。判据见 [`clipboard::agent_takes_clipboard_image`]。
    agent_takes_image: bool,
}

/// 取一次粘贴上下文。失败提示的标题行在这里定死,不会为空。
fn paste_context(pty_id: u32, cx: &gpui::App) -> PasteContext {
    let store = AppStore::global(cx);
    let s = store.read(cx);
    let cfg = s.config();
    let owner = s.pane_of_pty(pty_id);
    // 失败提示的标题行:项目名 →(取不到)pane 标签 →(还取不到)pty 编号。
    // 规格把「原版本地失败时拿到 undefined 项目名」记成隐性缺陷并要求补兜底,
    // 这一串就是那个兜底 —— 标题行永远不为空。
    let project_name = owner
        .as_ref()
        .and_then(|(pid, _)| s.project(pid))
        .map(|p| p.name.clone())
        .or_else(|| {
            owner.as_ref().and_then(|(pid, pane_id)| {
                s.project_state(pid)
                    .and_then(|st| st.pane(pane_id))
                    .map(|p| p.label().to_string())
            })
        })
        .unwrap_or_else(|| format!("pane {pty_id}"));
    let remote = owner.as_ref().and_then(|(pid, _)| {
        let project = s.project(pid)?;
        let conn = s.remote_connection_of(pid)?;
        Some((conn, project.path.clone()))
    });
    let target = clipboard::resolve_paste_target(s, pty_id);
    // 状态与 agent 名都取 pane 当下那份:hook 上报优先、输入检测兜底(`ai_agent`),
    // 与 tab 上品牌图标同一口径。长文本转存看的也是这一份状态。
    let pane = owner
        .as_ref()
        .and_then(|(pid, pane_id)| s.project_state(pid)?.pane(pane_id));
    let agent_takes_image = pane
        .is_some_and(|p| clipboard::agent_takes_clipboard_image(p.status, p.ai_agent(), target));
    let enabled = clipboard::long_paste_wanted(
        cfg.long_paste_to_file,
        cfg.long_paste_ai_only,
        pane.map(|p| p.status),
    );
    PasteContext {
        enabled,
        line_threshold: cfg.long_paste_line_threshold,
        char_threshold: cfg.long_paste_char_threshold,
        target,
        project_id: owner.map(|(pid, _)| pid).unwrap_or_default(),
        project_name,
        remote,
        remote_paste_dir: cfg.remote_paste_dir.clone(),
        agent_takes_image,
    }
}

/// 一次粘贴该往终端里写什么(`terminalCache.ts::pasteToTerminalInner`)。
///
/// ```text
/// 剪贴板有图 && pane 里正跑着 Claude Code(本地/WSL)→ 不落盘,发 Alt+V 让它自己取
/// 剪贴板有图 → 落盘 → 写 "{映射后的路径}";远程 pane 交给后台上传
///   └─ 有图但读不出 → 发 Alt+V,让终端里的 AI 工具自己读剪贴板
/// 否则取文本 → 空则什么都不做
/// 开关开着 && (pane 在 AI 会话里 || 关了「只在 AI 会话里」) && 命中阈值
///   ├─ 远程 pane 且连接在场 → 交给后台任务(转存 + SFTP 上传 + 写远端路径),
///   │                        当场返回 None(语义 = 宿主已接管)
///   ├─ 本地/WSL 转存成功    → 写 "{映射后的路径}"(裸写,不走 bracketed paste)
///   └─ 转存失败             → 弹一条 paste-error toast,**继续往下粘原文**(老行为)
/// 否则 → 按 bracketed paste 粘原文
/// ```
///
/// # 与原版的一处偏差
///
/// **本地转存失败也弹 toast**。原版 `notifyPasteFailure` 开头就
/// `if (target.kind !== 'ssh') return`,本地写盘失败只有 console.error ——
/// 规格把这条记成原版的隐性缺陷并建议「补一个兜底项目名」,这里照办:
/// 项目名取该 pane 所属项目,取不到就退回 pane 的显示名。
///
/// # 为什么是自由函数而不是 `TerminalPane` 的方法
///
/// 钩子在 `TerminalView` 被可变借用时调用;方法版会诱使人写
/// `self.view.update(...)`,那就是同一实体的嵌套 update(gpui 当场 panic)。
/// 自由函数只拿 `pty_id` + `&mut App`,连碰到视图的机会都没有。
fn resolve_paste(pty_id: u32, cx: &mut gpui::App) -> PasteAction {
    let ctx = paste_context(pty_id, cx);
    // 剪贴板**只读一次**:判图与取文本看同一份快照,免得用户在两次读之间换了
    // 内容,出现「判定是图、粘出来是文本」。
    let item = cx.read_from_clipboard();

    // pane 里正跑着 Claude Code:有图就把 Alt+V 交给它,由它自己读剪贴板插
    // `[Image #N]` 芯片 —— 模型直接看到图,比粘一条本机路径强。**排在落盘之前**,
    // 否则临时目录里会多一个谁也不引用的文件。SSH pane 不进这里
    // (`agent_takes_clipboard_image` 已挡),仍走下面的上传路线。
    if ctx.agent_takes_image && clipboard::clipboard_has_image(item.as_ref()) {
        return PasteAction::Raw(clipboard::ALT_V.to_string());
    }

    // 图片先判 —— 截图工具放进剪贴板的只有位图,没有文本可粘,这一支不判阈值
    // 也不看 `enabled`。
    match clipboard::read_clipboard_image(item.as_ref()) {
        ClipboardImage::Saved(path) => return paste_image(pty_id, path, ctx, cx),
        // 有图却读不出(BI_BITFIELDS 之类):退 Alt+V 让 AI 工具自己去读。
        // **绝不能**掉进下面的文本分支 —— 图文混排时会把 alt 文字当正文粘。
        ClipboardImage::Unreadable => return PasteAction::Raw(clipboard::ALT_V.to_string()),
        ClipboardImage::None => {}
    }

    let Some(text) = item.and_then(|it| it.text()) else {
        return PasteAction::None;
    };
    if text.is_empty() {
        return PasteAction::None;
    }

    let PasteContext {
        enabled,
        line_threshold,
        char_threshold,
        target,
        project_id,
        project_name,
        remote,
        remote_paste_dir,
        agent_takes_image: _,
    } = ctx;

    // SSH 远程 pane:转存 + SFTP 上传是异步的,交给后台任务,钩子当场返回
    // `None`(语义 = 宿主已接管)。断链(连接被删)时 `remote` 为 None ——
    // 没有上传通道,退回粘原文,与 mt-ssh 进 crates 之前的行为一致。
    if enabled
        && target == PasteTarget::Ssh
        && clipboard::is_long_text(&text, line_threshold, char_threshold)
        && let Some((conn, project_path)) = remote
    {
        clipboard::spawn_remote_paste(
            pty_id,
            RemotePaste::Text(text),
            conn,
            project_path,
            project_id,
            project_name,
            remote_paste_dir,
            cx,
        );
        return PasteAction::None;
    }

    if enabled
        && target != PasteTarget::Ssh
        && clipboard::is_long_text(&text, line_threshold, char_threshold)
    {
        match clipboard::save_clipboard_text(&text) {
            Ok(path) => {
                let mapped = clipboard::map_pasted_path(&path, target);
                return PasteAction::Raw(clipboard::quote_path(&mapped));
            }
            Err(detail) => {
                tracing::warn!("[pane {pty_id}] 粘贴内容转存失败: {detail}");
                toast::push_message(
                    ToastKind::PasteError,
                    project_id,
                    project_name,
                    tr!("terminal", "pasteUploadFailed", detail = detail),
                    cx,
                );
                // 提示完继续往下粘原文 —— 与原版一致(就是长了点,比什么都没有强)
            }
        }
    }
    PasteAction::Text(text)
}

/// 已落盘的剪贴板图片该怎么粘(`pasteToTerminalInner` 的图片分支)。
///
/// 本地 / WSL 直接粘映射后的路径;远程 pane 交给后台 SFTP 上传。
///
/// # 远程断链为什么什么都不粘
///
/// 图片没有「原文」可退,而 [`ALT_V`](clipboard::ALT_V) 对远程也没用 ——
/// 那头的 agent 读的是**远端**剪贴板。只剩「提示用户」这一条(装机版同款)。
fn paste_image(
    pty_id: u32,
    local: std::path::PathBuf,
    ctx: PasteContext,
    cx: &mut gpui::App,
) -> PasteAction {
    if ctx.target == PasteTarget::Ssh {
        let Some((conn, project_path)) = ctx.remote else {
            tracing::warn!("[pane {pty_id}] 远程连接不在场,剪贴板图片未粘贴");
            toast::push_message(
                ToastKind::PasteError,
                ctx.project_id,
                ctx.project_name,
                tr!("terminal", "pasteImageNoRemote").to_string(),
                cx,
            );
            return PasteAction::None;
        };
        clipboard::spawn_remote_paste(
            pty_id,
            RemotePaste::File(local),
            conn,
            project_path,
            ctx.project_id,
            ctx.project_name,
            ctx.remote_paste_dir,
            cx,
        );
        return PasteAction::None;
    }

    let mapped = clipboard::map_pasted_path(&local, ctx.target);
    PasteAction::Raw(clipboard::quote_path(&mapped))
}

/// 按 pty 编号取「分支那一段」的菜单项(含前导分隔线)。
///
/// 显隐口径与 tab 右键**逐字相同**(`branch_menu_segment` 一处判据),
/// 项的实现也是同一份(`branch_family` 的三个构造器)——
/// 「用户在哪儿右键都找得到同一个入口」是这条功能的设计前提。
///
/// # 为什么是自由函数
///
/// 与 [`resolve_paste`] 同一条理由:它在 `TerminalPane` 被可变借用时调用,
/// 方法版会诱使人写 `self.view.update(...)` 那种同实体嵌套 update。
fn branch_entries_for_pty(pty_id: u32, cx: &mut gpui::App) -> Vec<menu::MenuEntry> {
    let store = AppStore::global(cx);
    let Some((project_id, pane_id)) = store.read(cx).pane_of_pty(pty_id) else {
        return Vec::new();
    };
    let (segment, project_path) = {
        let s = store.read(cx);
        let segment = s
            .project_state(&project_id)
            .and_then(|st| st.pane(&pane_id))
            .map(|p| {
                crate::session_branch::branch_menu_segment(
                    p.ai_session.as_ref(),
                    p.detected_agent.as_deref(),
                )
            })
            .unwrap_or(crate::session_branch::BranchMenuSegment::None);
        let path = s.project(&project_id).map(|p| p.path.clone()).unwrap_or_default();
        (segment, path)
    };
    crate::branch_family::branch_menu_entries(&store, &project_id, &pane_id, project_path, &segment)
}

// ─── 终端右键菜单的其余几项(链接 / 清除缓冲 / 保存缓冲) ───

/// 悬停提示里那行字:「Ctrl+单击打开」,macOS 上是 ⌘。修饰键与 Ctrl+单击那条路认的
/// `Modifiers::secondary()` 同一口径。
fn link_hover_hint_label() -> String {
    let modifier = if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl"
    };
    tr!("terminal", "linkHoverHint", modifier = modifier)
}

/// 右键位置命中链接时,菜单最前面那一段(不含分隔线)。
///
/// 「打开」与 Ctrl+单击是同一个出口(`terminal_links::open` / `open_url`):文件在文件页
/// 开页签、网址交给默认浏览器。「复制」:网址复制网址本身(OSC 8 则是真实目标,不是显示
/// 文字);文件复制宿主解析好的路径(本地是绝对路径,远程项目是远程那边的绝对路径)。
fn link_menu_entries(link: TerminalLink) -> Vec<menu::MenuEntry> {
    match link {
        TerminalLink::Url(url) => {
            let copied = url.clone();
            vec![
                menu::item(t("terminal", "openLink"), move |_window, cx| {
                    crate::terminal_links::open_url(&url, cx)
                }),
                menu::item(t("terminal", "copyLink"), move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()))
                }),
            ]
        }
        TerminalLink::File(file) => {
            let path = file.path.to_string_lossy().into_owned();
            vec![
                menu::item(t("terminal", "openFileLink"), move |window, cx| {
                    crate::terminal_links::open(&file, window, cx)
                }),
                menu::item(t("terminal", "copyFilePath"), move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(path.clone()))
                }),
            ]
        }
    }
}

/// 「清除缓冲」怎么清,口径见模块注释「右键「清除缓冲」」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClearBufferPlan {
    /// 程序已经退出:本地连屏带回看清掉。
    LocalAll,
    /// 只清回看,一个字节都不发。
    ScrollbackOnly,
    /// 清回看 + 给 shell 发 Ctrl+L,让它自己清屏、重画提示符。
    ShellRedraw,
}

/// 这一下「清除缓冲」走哪一档。
///
/// 发 Ctrl+L 的前提是**确定**收的人是认它的 shell:AI 会话里(Claude Code 等 TUI 自己绑了
/// Ctrl+L)、全屏程序里(vim 里是重画、less 里是别的意思)一律不发;cmd 与认不出的 shell
/// 也不发 —— cmd 的行编辑不认 Ctrl+L,发过去只会在命令行里多一个控制字符,下一条命令就
/// 带着它跑。远程项目的 pane 是 ssh,收的是服务器上的登录 shell(bash / zsh 一类),发。
fn clear_buffer_plan(
    dead: bool,
    ai_alive: bool,
    alt_screen: bool,
    remote: bool,
    shell: ShellKind,
) -> ClearBufferPlan {
    if dead {
        return ClearBufferPlan::LocalAll;
    }
    if ai_alive || alt_screen {
        return ClearBufferPlan::ScrollbackOnly;
    }
    let redraws = remote
        || matches!(
            shell,
            ShellKind::Pwsh
                | ShellKind::WindowsPowerShell
                | ShellKind::Bash
                | ShellKind::Zsh
                | ShellKind::Fish
                | ShellKind::Nu
                | ShellKind::Wsl
        );
    if redraws {
        ClearBufferPlan::ShellRedraw
    } else {
        ClearBufferPlan::ScrollbackOnly
    }
}

/// pane 用的是哪家 shell:先认配置里那条 shell 的可执行文件(`pwsh.exe`、`cmd.exe`……,
/// 比用户起的名字可靠),认不出再看名字。配置里已经没有这条了也按名字认。
fn shell_kind_of(shells: &[ShellConfig], shell_name: &str) -> ShellKind {
    let by_command = shells
        .iter()
        .find(|s| s.name == shell_name)
        .map_or(ShellKind::Unknown, |s| {
            ShellKind::from_shell_name(&s.command)
        });
    if by_command == ShellKind::Unknown {
        ShellKind::from_shell_name(shell_name)
    } else {
        by_command
    }
}

/// 「将缓冲保存为文件…」的建议文件名:`mini-term-年月日-时分秒.txt`。不带页签名 ——
/// 那是用户起的名字,什么字符都可能有,文件名里放不下的还得一个个替换。
fn buffer_file_name(now: chrono::NaiveDateTime) -> String {
    format!("mini-term-{}.txt", now.format("%Y%m%d-%H%M%S"))
}

/// 右键「将缓冲保存为文件…」:回看 + 当前屏的纯文本(口径见 `mt_terminal::buffer`)写到用户
/// 在系统保存对话框里选的位置。
///
/// 文本在**点下去那一刻**取(就是用户眼前这份),对话框开着期间的新输出不算;写盘丢后台。
/// 对话框的起始目录:本地项目是项目根,远程项目(项目路径在服务器上)交给系统默认。
/// 失败给 toast;取消什么都不做。
fn save_buffer_to_file(
    pty_id: u32,
    emulator: Arc<TerminalEmulator>,
    window: &mut Window,
    cx: &mut App,
) {
    let text = emulator.buffer_text();
    let (project_id, project_name, directory) = {
        let store = AppStore::global(cx).read(cx);
        let project = store
            .pane_of_pty(pty_id)
            .and_then(|(pid, _)| store.project(&pid));
        let directory = project
            .filter(|p| p.ssh_connection_id.is_none())
            .map(|p| p.path.clone())
            .unwrap_or_default();
        (
            project.map(|p| p.id.clone()).unwrap_or_default(),
            project.map(|p| p.name.clone()).unwrap_or_default(),
            directory,
        )
    };
    let name = buffer_file_name(chrono::Local::now().naive_local());
    let prompt = cx.prompt_for_new_path(Path::new(&directory), Some(&name));
    window
        .spawn(cx, async move |cx| {
            let path = match prompt.await {
                Ok(Ok(Some(path))) => path,
                // 取消 / 对话框那头没了
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(err)) => {
                    let detail = format!("{err:#}");
                    let _ = cx.update(|_window, cx| {
                        toast_save_failure(project_id, project_name, &detail, cx)
                    });
                    return;
                }
            };
            let written = cx
                .background_executor()
                .spawn(async move { std::fs::write(&path, text) })
                .await;
            if let Err(err) = written {
                let detail = err.to_string();
                let _ = cx.update(|_window, cx| {
                    toast_save_failure(project_id, project_name, &detail, cx)
                });
            }
        })
        .detach();
}

fn toast_save_failure(project_id: String, project_name: String, detail: &str, cx: &mut App) {
    toast::push_message(
        ToastKind::Error,
        project_id,
        project_name,
        tr!("terminal", "saveBufferFailed", detail = detail),
        cx,
    );
}

// ─── 终端右键「SSH 连接」的连接动作(`TerminalInstance.tsx:60-82`) ───
//
// 菜单项本身(悬停展开的快速连接面板)在 [`crate::ssh_quick_connect`]。

/// 把一条连接拼成 `ssh` 命令行(`buildSshCommand`)。
///
/// `identity_path` 是解析后的私钥路径(可能是 `prepare_ssh_key` 收紧权限后的
/// 临时副本),未配置私钥时传 `None`。
///
/// **反斜杠一律换成正斜杠**:Nushell / bash 会把双引号里的 `\` 当转义符从而报错,
/// 而 Windows OpenSSH 接受正斜杠路径 —— 正斜杠在所有 shell 里都安全(原版原话)。
fn build_ssh_command(conn: &SshConnection, identity_path: Option<&str>) -> String {
    let mut parts = vec!["ssh".to_string()];
    if conn.port != 0 && conn.port != 22 {
        parts.push("-p".into());
        parts.push(conn.port.to_string());
    }
    if let Some(identity) = identity_path.map(str::trim).filter(|p| !p.is_empty()) {
        parts.push("-i".into());
        parts.push(format!("\"{}\"", identity.replace('\\', "/")));
    }
    parts.push(format!("{}@{}", conn.user, conn.host));
    parts.join(" ")
}

/// 右键「SSH 连接」该把 `ssh …` 敲进哪个终端,见 [`ssh_connect_target`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SshConnectTarget {
    /// 右键的那个终端(原行为)。
    Current,
    /// 同一分组里新开一个终端页签(项目默认 shell)再连。
    NewTerminal,
    /// 该换新终端、但这是远程项目:不连,提示用户换个空闲的终端。
    Refuse,
}

/// 右键「SSH 连接」该落在哪个终端。
///
/// `ssh …\r` 是当作用户键入写进 PTY 的:终端里正跑着 Claude / Codex(`ai_alive`)时这一行
/// 会被当成一次提交,vim / less / htop 这类全屏程序(`alt_screen`)会把它当成一串按键 ——
/// 这两种情况改到新终端里连,其余照旧敲进当前终端。`force_new` 是快速连接面板里的
/// Ctrl+Enter / Ctrl+单击:python REPL、`tail -f` 这类既不是 AI 也不走 alt screen 的程序
/// 判不出来,由用户自己说「别动当前这个」。
///
/// 远程项目(`remote`)要换新终端时**不连**:那里的新终端是 ssh 启动器,起步时可能停在
/// 口令 / host key 确认上,预写的命令会被当口令吃掉 —— 与远程项目不出 AI 启动器
/// ([`crate::pane_actions::project_allows_launchers`])、启动恢复不给远程 pane 预写续接命令
/// 同一条理由。
pub(crate) fn ssh_connect_target(
    force_new: bool,
    ai_alive: bool,
    alt_screen: bool,
    remote: bool,
) -> SshConnectTarget {
    if !(force_new || ai_alive || alt_screen) {
        SshConnectTarget::Current
    } else if remote {
        SshConnectTarget::Refuse
    } else {
        SshConnectTarget::NewTerminal
    }
}

/// 快速连接面板的连接入口:按 [`ssh_connect_target`] 决定敲进当前终端、开新终端再连,
/// 还是提示后作罢。
///
/// AI 会话的判据取 pane **当下**的状态(hook 上报优先、输入检测兜底,口径同
/// [`crate::pane_actions::is_ai_alive`]);全屏程序看终端此刻是不是 alt screen。
///
/// 新终端走 `AppStore::new_terminal`:锚点是右键的那个 pane,于是落在同一分组、被激活并
/// 拿到焦点,shell 用项目默认的。随后照常 [`connect_ssh`] —— 那边写的是 pane 的写入队列,
/// PTY 还在后台起时整条 `ssh …\r`(连同密码自动填充的注册)排着,回填时才交给新 PTY;
/// PTY 起不来就随之丢弃。与 AI 启动器、启动恢复写续接命令同一条时序(见 [`crate::pty_slot`])。
pub(crate) fn connect_ssh_from_menu(
    pty_id: u32,
    conn: SshConnection,
    force_new: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let store = AppStore::global(cx);
    let (owner, ai_alive, remote, project_name) = {
        let s = store.read(cx);
        let owner = s.pane_of_pty(pty_id);
        let ai_alive = owner
            .as_ref()
            .and_then(|(pid, pane_id)| s.project_state(pid)?.pane(pane_id))
            .is_some_and(|p| crate::pane_actions::is_ai_alive(p.status));
        let project = owner.as_ref().and_then(|(pid, _)| s.project(pid));
        let remote = project.is_some_and(|p| p.ssh_connection_id.is_some());
        let name = project.map(|p| p.name.clone()).unwrap_or_default();
        (owner, ai_alive, remote, name)
    };
    // 定位不到 pane:照旧交给 `connect_ssh`(终端也不在的话它自己会作罢)
    let Some((project_id, pane_id)) = owner else {
        connect_ssh(pty_id, conn, window, cx);
        return;
    };
    let alt_screen = store
        .read(cx)
        .terminal(pty_id)
        .is_some_and(|t| t.read(cx).in_alt_screen());
    match ssh_connect_target(force_new, ai_alive, alt_screen, remote) {
        SshConnectTarget::Current => connect_ssh(pty_id, conn, window, cx),
        SshConnectTarget::NewTerminal => {
            let new_pty = store.update(cx, |store, cx| {
                let new_pane = store.new_terminal(&project_id, None, Some(pane_id), window, cx)?;
                store.project_state(&project_id)?.pane(&new_pane)?.pty_id
            });
            if let Some(new_pty) = new_pty {
                connect_ssh(new_pty, conn, window, cx);
            }
        }
        SshConnectTarget::Refuse => toast::push_message(
            ToastKind::Info,
            project_id,
            project_name,
            t("terminal", "sshNewTerminalRemote").to_string(),
            cx,
        ),
    }
}

/// 在指定终端里连 SSH:写入 `ssh` 命令并回车,有密码则**写完再**注册自动填充。
///
/// 私钥那一步(`mt_core::prepare_ssh_key`:复制成权限收紧的临时副本,绕开
/// OpenSSH 的 `UNPROTECTED PRIVATE KEY FILE` 拒绝)是**阻塞文件 IO**,丢后台;
/// 失败**回退原始路径**让 ssh 自己报错(原版 `console.error` 后照走)。
///
/// 自动填充的注册因此也挪进了后台任务、紧跟在那次命令写入之后
/// ([`TerminalPane::write_ssh_command`]):以 `disarm_on_input = true` 注册,用户
/// 此后一打字即解除 —— 公钥 / agent 先认证成功时不会有 SSH 密码提示,旧做法
/// (先注册、`false`)会让它一直待命,把密码灌进之后任何以 "password:" 结尾的输出。
pub(crate) fn connect_ssh(pty_id: u32, conn: SshConnection, window: &mut Window, cx: &mut App) {
    let Some(terminal) = AppStore::global(cx).read(cx).terminal(pty_id).cloned() else {
        return;
    };
    // 已存密码是 `mt-secret` 信封,交给 autofill 前在这里解开;解不开就提示并不填
    // (终端里照常出现密码提示,用户手输即可)。
    let password = match conn.password.as_deref().filter(|p| !p.is_empty()) {
        Some(stored) => match crate::secrets::reveal_password(stored) {
            Ok(password) => Some(password),
            Err(err) => {
                crate::secrets::toast_password_error(err, cx);
                None
            }
        },
        None => None,
    };
    let identity = conn
        .identity_file
        .clone()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());
    window
        .spawn(cx, async move |cx| {
            let identity = match identity {
                Some(path) => {
                    let source = path.clone();
                    let prepared = cx
                        .background_executor()
                        .spawn(async move { mt_core::prepare_ssh_key(&source) })
                        .await;
                    match prepared {
                        Ok(temp) => Some(temp),
                        Err(err) => {
                            tracing::warn!("[ssh] 准备私钥临时副本失败,回退原始路径: {err}");
                            Some(path)
                        }
                    }
                }
                None => None,
            };
            let command = build_ssh_command(&conn, identity.as_deref());
            let _ = cx.update(|window, cx| {
                let line = format!("{command}\r");
                terminal.update(cx, |pane, cx| {
                    pane.write_ssh_command(line.as_bytes(), password, cx)
                });
                // 写完把键盘还给终端(原版 `term.focus()`)
                terminal.update(cx, |pane, cx| pane.focus(window, cx));
            });
        })
        .detach();
}

/// 终端里的右键该弹**本地菜单**吗。
///
/// 判据只有一条,且必须与 mt-ui 的元素侧同源([`prefers_local_handling`]):
/// 应用开着鼠标上报时右键属于**应用**(vim 的右键菜单、tmux 的选择),本地菜单
/// 让位;按住 Shift 强制回本地 —— 这是终端界通行的「借回鼠标」手势。
///
/// 元素侧那份 `MouseDownEvent` 监听是 `window.on_mouse_event` 挂的、不吃
/// `stop_propagation`,所以两边**各判各的**,这里判错就会出现「菜单弹出来了、
/// 同时 vim 也收到了一次右键」。
fn allows_local_menu(mode: TermMode, shift: bool, alt: bool, control: bool) -> bool {
    prefers_local_handling(mode, MouseMods::new(shift, alt, control))
}

/// 把 grid 绝对行 `line` 滚到**视口顶部**所需的 `Scroll::Delta`。
///
/// `display_offset` 是「往回看多少行」,屏幕行 `row = line + display_offset`,
/// 要 `row == 0` 即 `display_offset == -line`。目标偏移钳在 `[0, history]` 内
/// (grid 自己也会钳一次,先钳是为了让 `delta == 0` 的短路判得准)。
fn scroll_delta_to_top(line: i32, display_offset: i32, history: i32) -> i32 {
    (-line).clamp(0, history.max(0)) - display_offset
}

impl TerminalPane {
    /// PTY 起不来:正中一行红字(原因不吞)。本地 pane 再给「重新启动 / 关闭」与 Enter;
    /// 远程 pane 与此前一致只有那行字。
    fn render_spawn_error(&mut self, err: String, cx: &mut Context<Self>) -> Div {
        // 不刷底色:着色由 TerminalArea 根容器那层 bg_terminal 承担(单层规则,
        // 见 terminal_area.rs pane 组容器处的说明)
        let root = div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.0))
            .child(
                div()
                    .text_size(crate::ui::font_px(13.0))
                    .text_color(crate::ui::color_error())
                    .child(format!("{}:{err}", t("paneGroup", "startFailed"))),
            );
        if self.remote {
            return root;
        }
        // 终端视图这一帧不在树上,焦点句柄改由这个容器认领:点页签 / 切过来时焦点照样
        // 送到这里,Enter 才有地方落
        root.track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let keystroke = &event.keystroke;
                if keystroke.key == "enter"
                    && !keystroke.modifiers.modified()
                    && enter_restarts(b"\r", this.dead_since, Instant::now())
                {
                    this.request_restart(window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .text_size(crate::ui::font_px(12.0))
                            .text_color(crate::ui::text_muted())
                            .child(t("paneGroup", "restartHint")),
                    )
                    .child(self.render_dead_actions(cx)),
            )
    }

    /// 本地 shell 退出后的底部条:「shell 已退出(退出码 N)· Enter 重新启动」+
    /// 「重新启动 / 关闭」。
    ///
    /// **占自己的一行,不盖在终端上**:整屏遮罩(远程断线那种)会挡住退出前的输出,而用户
    /// 往往正是要回看那几行(报错、最后一屏)。代价是 grid 少两行,挤出去的内容照常进回滚,
    /// 一行都不丢。不刷底色(单层规则),只用一条上边线和终端隔开。
    fn render_exit_bar(&mut self, cx: &mut Context<Self>) -> Div {
        // 与状态灯同一口径:干净退出是灰的,非 0 / 取不到退出码是红的
        let color = if self.exit_code == Some(0) {
            crate::ui::text_secondary()
        } else {
            crate::ui::color_error()
        };
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(12.0))
            .px(px(12.0))
            .py(px(6.0))
            .border_t_1()
            .border_color(crate::ui::border_subtle())
            .text_size(crate::ui::font_px(12.0))
            .child(div().text_color(color).child(exit_message(self.exit_code)))
            .child(
                div()
                    .text_color(crate::ui::text_muted())
                    .child(t("paneGroup", "restartHint")),
            )
            .child(div().flex_1())
            .child(self.render_dead_actions(cx))
    }

    /// 「重新启动」「关闭」两颗按钮(退出条与起不来两处共用)。
    fn render_dead_actions(&mut self, cx: &mut Context<Self>) -> Div {
        let id = self.pty_id;
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                crate::ui::ghost_button(
                    SharedString::from(format!("pane-restart-{id}")),
                    t("paneGroup", "restart"),
                )
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.request_restart(window, cx);
                })),
            )
            .child(
                crate::ui::ghost_button(
                    SharedString::from(format!("pane-close-{id}")),
                    t("paneGroup", "close"),
                )
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.request_close(window, cx);
                })),
            )
    }

    /// 回看时右下角的「↓」浮钮,回看期间来过新输出就在它右上角点一颗 accent 小圆点。
    ///
    /// 位置避让:滚动条在终端本体最右那 7px([`SCROLL_BUTTON_RIGHT`]);查找条在右上角;
    /// 退出条在本体**下面**另占一行,碰不到。「已复制」气泡跟着选区走、1 秒自撤,
    /// 画在它上面。
    fn render_scroll_bottom_button(
        &mut self,
        unseen: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(SharedString::from(format!("scroll-bottom-{}", self.pty_id)))
            .absolute()
            .right(px(SCROLL_BUTTON_RIGHT))
            .bottom(px(SCROLL_BUTTON_BOTTOM))
            .w(px(28.0))
            .h(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .border_1()
            .border_color(crate::ui::border_default())
            .bg(crate::ui::bg_elevated())
            .text_size(crate::ui::font_px(14.0))
            .text_color(crate::ui::text_secondary())
            .cursor_pointer()
            // 自己吃鼠标:底下终端的按下监听不该把这一下当成起选区 / 鼠标上报
            .occlude()
            .hover(|el| {
                el.border_color(crate::ui::accent())
                    .text_color(crate::ui::accent())
            })
            .tip(tr!(
                "terminal",
                "scrollToBottom",
                shortcut = crate::hotkeys::hotkey_tip("scrollBottom")
            ))
            .on_click(cx.listener(|this, _event, window, cx| this.scroll_to_bottom(window, cx)))
            .child("↓")
            .when(unseen, |el| {
                el.child(
                    div()
                        .absolute()
                        .top(px(-1.0))
                        .right(px(-1.0))
                        .w(px(8.0))
                        .h(px(8.0))
                        .rounded_full()
                        .bg(crate::ui::accent()),
                )
            })
    }
}

impl TerminalPane {
    /// 终端右键菜单的全部条目,按段排(段间分隔线):
    ///
    /// 1. **链接**(右键位置命中链接时才有):打开 / 复制。命中判定与 Ctrl+悬停是同一套
    ///    ([`TerminalView::link_at`]);
    /// 2. 复制 / 粘贴 / 全选;
    /// 3. 查找…(与 Ctrl+F 同一个 [`Self::open_search`]);
    /// 4. 清除缓冲 / 将缓冲保存为文件…;
    /// 5. 会话分支段(有才出)与 SSH 连接(恒在)—— 与此前一致。
    ///
    /// 置灰:没有选区时「复制」灰(原版 `disabled: !hasSelection`);缓冲里一个可见字都
    /// 没有时「全选」「保存」灰。各项动作都在菜单关掉之后跑,不借着本 pane,所以一律经
    /// 弱引用 / 视图句柄回来,不会撞上同一实体的嵌套 update。
    fn context_menu_entries(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> Vec<menu::MenuEntry> {
        let has_selection = self.has_selection();
        let has_content = self.emulator.has_buffer_content();
        let link = self.view.read(cx).link_at(position, cx);
        let this = cx.weak_entity();
        let mut entries = Vec::new();

        if let Some(link) = link {
            entries.extend(link_menu_entries(link));
            entries.push(menu::separator());
        }

        let view_copy = self.view.clone();
        let view_paste = self.view.clone();
        let view_select = self.view.clone();
        let focus_paste = self.focus.clone();
        let focus_select = self.focus.clone();
        entries.extend([
            MenuItem::new(t("terminal", "copy"))
                // 没有选区时置灰(原版 `disabled: !hasSelection`)
                .disabled(!has_selection)
                .on_click(move |_window, cx| {
                    view_copy.update(cx, |view, cx| {
                        view.copy_selection(cx);
                    });
                })
                .into(),
            // 走 `request_paste` 而不是 `paste`:长文本转文件挂在
            // 宿主钩子上,直接调 `paste` 会绕过它(Ctrl+Shift+V 与
            // 智能 Ctrl+V 同理,那两条在 mt-ui 侧已经改过来了)
            menu::item(t("terminal", "paste"), move |window, cx| {
                view_paste.update(cx, |view, cx| view.request_paste(window, cx));
                // 粘完把键盘还给终端(原版 `term.focus()`)
                window.focus(&focus_paste, cx);
            }),
            // 回看 + 当前屏整个选上;键盘还给终端,接着按 Ctrl+Shift+C 就能复制
            MenuItem::new(t("terminal", "selectAll"))
                .disabled(!has_content)
                .on_click(move |window, cx| {
                    view_select.update(cx, |view, cx| view.select_all(cx));
                    window.focus(&focus_select, cx);
                })
                .into(),
            menu::separator(),
        ]);

        let this_find = this.clone();
        entries.push(
            MenuItem::new(t("terminal", "find"))
                .shortcut(crate::hotkeys::hotkey_label("terminalSearch"))
                .on_click(move |window, cx| {
                    let _ = this_find.update(cx, |pane, cx| pane.open_search(window, cx));
                })
                .into(),
        );
        entries.push(menu::separator());

        let this_clear = this.clone();
        let pty_id = self.pty_id;
        let emulator = self.emulator.clone();
        entries.extend([
            menu::item(t("terminal", "clearBuffer"), move |_window, cx| {
                let _ = this_clear.update(cx, |pane, cx| pane.clear_buffer(cx));
            }),
            MenuItem::new(t("terminal", "saveBuffer"))
                .disabled(!has_content)
                .on_click(move |window, cx| {
                    save_buffer_to_file(pty_id, emulator.clone(), window, cx);
                })
                .into(),
        ]);

        // 会话分支入口:终端本体右键与 tab 右键**同权**(用户在哪儿
        // 右键都找得到),显隐口径与项的实现都是同一份
        entries.extend(branch_entries_for_pty(self.pty_id, cx));
        // SSH 段:**恒在**,悬停展开快速连接面板(一条连接都没有时
        // 面板是带「新建连接」的空态卡,见 `ssh_quick_connect`)
        entries.push(menu::separator());
        entries.push(crate::ssh_quick_connect::menu_entry(
            AppStore::global(cx),
            self.pty_id,
        ));
        entries
    }
}

impl TerminalPane {
    /// 右上角那一列里的查找条(开着,或者收起动画还没播完)。
    ///
    /// 开合动画走 [`crate::popover_reveal`]。查找条按自己的自然尺寸排版,外壳从第一帧起就占满最终尺寸,
    /// 下面的 SSH 悬浮框不会跟着动画跳。动画进行中框体(底色 / 边框 / 阴影)由宿主按插值矩形画,
    /// 取值与 [`TerminalSearchBar`] 自带的那套一致(主题 `popover` 底 / `border` 边 / `rounded_md`
    /// / `shadow_lg`),查找条本体只露出裁剪窗里的那一块;全开静止时直接画查找条本体,
    /// 外观与不加动画时一字不差 —— 所以不必改 mt-ui 的查找条。
    fn render_search_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let bar = self
            .search_bar
            .clone()
            .or_else(|| self.search_exit.clone())?;
        let Some(state) = self.search_reveal.frame(window) else {
            if self.search_bar.is_some() {
                return Some(bar.into_any_element());
            }
            // 收起动画播完了
            self.search_exit = None;
            return None;
        };
        if state.settled {
            return Some(bar.into_any_element());
        }
        let theme = cx.theme();
        let chrome = Chrome {
            bg: theme.popover,
            hover_bg: None,
            border_color: theme.border,
            border_width: px(1.0),
            radius: gpui::rems(0.375).into(),
            shadow: ShadowMode::Always,
            occlude: false,
        };
        Some(reveal_frame(bar, state, Some(chrome)).into_any_element())
    }
}

/// 把 pane 根容器的窗口坐标报给 SSH 交互视图(悬浮框的宽高按 pane 尺寸定)。
///
/// 一层 absolute 全尺寸的 canvas:不插 hitbox、不画东西。pane 套着 view 级缓存,命中缓存时
/// 不重跑 —— 但 pane 尺寸一变缓存就失效,正好是要重报的时候。报上去的值变了且悬浮框开着,
/// 就再要一帧让视图按新尺寸重画。
fn ssh_activity_probe(view: Entity<SshActivityView>) -> impl IntoElement {
    gpui::canvas(
        move |bounds, window, cx| {
            if view.update(cx, |view, _| view.note_pane_bounds(bounds)) {
                window.request_animation_frame();
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(err) = self.spawn_error.clone() {
            return self.render_spawn_error(err, cx).into_any_element();
        }

        let (scrolled_back, unseen) = self.sync_scrollback_state();
        let scroll_button = scrolled_back.then(|| self.render_scroll_bottom_button(unseen, cx));
        // 远程 pane 退出后不出这条(上面另有 terminal_area 的整屏重连遮罩),见 `remote`
        let exit_bar = (self.exited && !self.remote).then(|| self.render_exit_bar(cx));
        let ssh_has_records = self.ssh_activity.read(cx).has_records();
        let search_bar = self.render_search_bar(window, cx);

        // 焦点 / key_context / 按键 / 左键聚焦全在 TerminalView 里,这里只剩一行。
        // 宿主**不刷底色**:终端区着色只保留 TerminalArea 根容器一层 bg_terminal
        // (原版 `themePackManager.ts:294` 的单层口径)。背景图主题下终端背景是
        // 半透明的,区根/pane 组/宿主逐层重复刷等于透明度叠乘,图会被盖死 ——
        // 曾经三层 0.6 叠出 ≈0.94,真机实测背景图几乎不可见。
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            // Ctrl+= / Ctrl+- / Ctrl+0:绑在 `Terminal` context 上,终端有焦点才命中、
            // 一路冒泡到这里。普通闭包而不是 `cx.listener`,理由见 [`zoom_terminal_font`]
            .on_action(|_: &TerminalZoomIn, _window, cx| zoom_terminal_font(TerminalZoom::In, cx))
            .on_action(|_: &TerminalZoomOut, _window, cx| zoom_terminal_font(TerminalZoom::Out, cx))
            .on_action(|_: &TerminalZoomReset, _window, cx| {
                zoom_terminal_font(TerminalZoom::Reset, cx)
            })
            // 终端右键菜单(`TerminalInstance.tsx` 的 handleContextMenu 起家),
            // 条目见 [`TerminalPane::context_menu_entries`]。
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    let mods = event.modifiers;
                    if !allows_local_menu(
                        this.emulator.mode(),
                        mods.shift,
                        mods.alt,
                        mods.control,
                    ) {
                        return;
                    }
                    cx.stop_propagation();
                    let entries = this.context_menu_entries(event.position, cx);
                    menu::show(event.position, entries, window, cx);
                }),
            )
            // 终端本体。底下的退出条占自己的一行(不是盖在终端上的遮罩),
            // 本体跟着矮一截,退出前的输出一行都不会被挡住
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .relative()
                    // 终端内容内边距:GPUI 的 grid 逐格自绘、顶格铺满 bounds,不垫一层
                    // 会贴着 pane 边缘(原版 xterm 靠字形侧空隙 + 列取整余量,视觉上
                    // 不贴边);8px 与 Windows Terminal 默认同档。padding 挤掉的空间由
                    // resize 链自然吸收(cols/rows 按 view 的实际 bounds 算)。
                    .child(div().size_full().p(px(8.0)).child(self.view.clone()))
                    .children(scroll_button),
            )
            .children(exit_bar)
            // 右上角一列:终端内查找条 + SSH 交互悬浮框(入口按钮在终端组标题栏上)。距顶 6px、
            // 距右 14px —— 与原版查找条 `rect.top + 6` / `rect.right - w - 14` 同款(那边是 rAF
            // 每帧算出来的 fixed 坐标,这里由布局白拿)。竖排右对齐、间距 6px:查找条开着时
            // 悬浮框自然落到它下方,两者不重叠。SSH 视图恒挂(关着时画空),理由见它的模块注释
            .child(
                div()
                    .absolute()
                    .top(px(6.0))
                    .right(px(14.0))
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(px(6.0))
                    .children(search_bar)
                    .child(self.ssh_activity.clone()),
            )
            // 悬浮框的宽高按 pane 尺寸定:有记录时把 pane 根容器的窗口坐标报给 SSH 视图
            .when(ssh_has_records, |el| {
                el.child(ssh_activity_probe(self.ssh_activity.clone()))
            })
            // 「已复制」气泡:叠在终端之上,坐标是元素相对值
            .when_some(self.copied_tip, |el, origin| {
                el.child(
                    div().absolute().left(origin.x).top(origin.y).child(
                        CopiedTip::new(crate::i18n::t("terminal", "copied"))
                            .colors(crate::ui::bg_overlay(), crate::ui::text_primary()),
                    ),
                )
            })
            // 远程 pane 的子进程没了:与此前一致只留右下角一行小字(上面盖着
            // terminal_area 的整屏重连遮罩)。本地 pane 走上面那条退出条
            .when(self.exited && self.remote, |el| {
                el.child(
                    div()
                        .absolute()
                        .bottom_2()
                        .right_3()
                        .text_size(crate::ui::font_px(12.0))
                        .text_color(crate::ui::color_error())
                        // 旧版没有这个角标(子进程退出后 pane 直接标红),
                        // `paneGroup.shellExited` 是 M 批往 TS 源头补的条目。
                        .child(crate::i18n::t("paneGroup", "shellExited")),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(name: &str, group: Option<&str>) -> SshConnection {
        SshConnection {
            id: format!("id-{name}"),
            name: name.to_string(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            password: None,
            identity_file: None,
            group: group.map(str::to_string),
            extra: Default::default(),
        }
    }

    /// 命令行:默认端口不写 `-p`,私钥路径**反斜杠换正斜杠并加引号**。
    #[test]
    fn ssh命令行拼装() {
        let mut c = conn("x", None);
        assert_eq!(build_ssh_command(&c, None), "ssh u@h");
        c.port = 2222;
        assert_eq!(build_ssh_command(&c, None), "ssh -p 2222 u@h");
        // 端口 0(配置缺省)按默认端口处理
        c.port = 0;
        assert_eq!(build_ssh_command(&c, None), "ssh u@h");
        c.port = 22;
        assert_eq!(
            build_ssh_command(&c, Some(r"C:\keys\id_rsa")),
            "ssh -i \"C:/keys/id_rsa\" u@h",
            "双引号里的反斜杠会被 bash/Nushell 当转义符"
        );
        // 空白私钥路径等于没配
        assert_eq!(build_ssh_command(&c, Some("   ")), "ssh u@h");
    }

    /// 右键 SSH 连接:终端空着(既不在 AI 会话、也不在 alt screen、没要求新终端)才敲进
    /// 当前终端,否则开新终端。
    #[test]
    fn ssh_连接只在终端空着时敲进当前终端() {
        use SshConnectTarget as T;
        assert_eq!(ssh_connect_target(false, false, false, false), T::Current);
        assert_eq!(
            ssh_connect_target(false, true, false, false),
            T::NewTerminal,
            "Claude / Codex 正活着:敲进去就是一次提交"
        );
        assert_eq!(
            ssh_connect_target(false, false, true, false),
            T::NewTerminal,
            "vim / less 这类全屏程序会把它当一串按键"
        );
        assert_eq!(
            ssh_connect_target(true, false, false, false),
            T::NewTerminal,
            "Ctrl+Enter / Ctrl+单击:不管当前终端在跑什么"
        );
        assert_eq!(ssh_connect_target(true, true, true, false), T::NewTerminal);
    }

    /// 远程项目里不预写:要换新终端时一律作罢(新终端起步停在登录提示上会吃掉命令);
    /// 用不着换新终端时照旧敲进当前终端。
    #[test]
    fn 远程项目要换新终端时不连() {
        use SshConnectTarget as T;
        assert_eq!(ssh_connect_target(false, false, false, true), T::Current);
        for (force, ai, alt) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            assert_eq!(
                ssh_connect_target(force, ai, alt, true),
                T::Refuse,
                "force={force} ai={ai} alt={alt}"
            );
        }
    }

    /// 没开鼠标上报 = 本地菜单照弹(修饰键无关)。
    #[test]
    fn 未上报时右键弹本地菜单() {
        let mode = TermMode::empty();
        assert!(allows_local_menu(mode, false, false, false));
        assert!(allows_local_menu(mode, true, false, false));
    }

    /// 应用抓着鼠标时右键让位给应用;**按住 Shift 强制借回本地**。
    #[test]
    fn 上报模式下只有_shift_能弹() {
        for mode in [
            TermMode::MOUSE_REPORT_CLICK,
            TermMode::MOUSE_DRAG,
            TermMode::MOUSE_MOTION,
        ] {
            assert!(!allows_local_menu(mode, false, false, false), "{mode:?}");
            assert!(allows_local_menu(mode, true, false, false), "{mode:?}");
            // Alt / Ctrl 不是借回手势,不许放行
            assert!(!allows_local_menu(mode, false, true, false), "{mode:?}");
            assert!(!allows_local_menu(mode, false, false, true), "{mode:?}");
        }
    }

    /// 标记跳转把目标行顶到视口**第一行**(不是居中,也不是「已在视口就不动」)。
    #[test]
    fn 标记跳转把目标行滚到视口顶部() {
        // 回看缓冲里第 100 行(line = -100),当前贴底(offset = 0):往回滚 100
        assert_eq!(scroll_delta_to_top(-100, 0, 500), 100);
        // 已经滚到位就不动 —— 短路判据
        assert_eq!(scroll_delta_to_top(-100, 100, 500), 0);
        // 滚过头了就往回补
        assert_eq!(scroll_delta_to_top(-100, 300, 500), -200);
    }

    /// 屏幕内的行(line >= 0)目标偏移是 0:**无条件**滚回底部,
    /// 哪怕那一行本来就在视口里 —— 原版 `scrollToLine` 就是这个语义。
    #[test]
    fn 屏幕内的标记也照样滚() {
        assert_eq!(scroll_delta_to_top(5, 0, 500), 0, "已在底部,delta 为零");
        assert_eq!(scroll_delta_to_top(5, 42, 500), -42, "回看态下拉回底部");
    }

    /// Enter 重新启动:PTY 死了、过了 500ms、写的正好是一个回车,三条缺一不可。
    #[test]
    fn 退出后的回车过了宽限期才算重启() {
        let dead = Instant::now();
        // 还活着:回车照常进 PTY
        assert!(!enter_restarts(b"\r", None, dead));
        // `exit⏎⏎` 连按:第二下回车落在宽限期里,不算
        assert!(!enter_restarts(b"\r", Some(dead), dead));
        assert!(!enter_restarts(
            b"\r",
            Some(dead),
            dead + Duration::from_millis(499)
        ));
        assert!(enter_restarts(
            b"\r",
            Some(dead),
            dead + RESTART_ENTER_GRACE
        ));
        assert!(enter_restarts(
            b"\r",
            Some(dead),
            dead + Duration::from_secs(30)
        ));
        // 别的键 / 粘贴进来的多行 / Shift+Enter 的 win32 记录都不算
        let later = dead + Duration::from_secs(1);
        assert!(!enter_restarts(b"a", Some(dead), later));
        assert!(!enter_restarts(b"ls\r", Some(dead), later));
        assert!(!enter_restarts(b"\x1b[13;28;13;1;16;1_", Some(dead), later));
        // 时钟往回跳(理论上不该)也不 panic、不误判
        assert!(!enter_restarts(b"\r", Some(later), dead));
    }

    /// 退出码:小的照原样,Windows 的 NTSTATUS 崩溃码一并给出十六进制。
    #[test]
    fn 退出码的显示形态() {
        assert_eq!(format_exit_code(1), "1");
        assert_eq!(format_exit_code(130), "130");
        assert_eq!(format_exit_code(0xC000_013A), "3221225786 (0xC000013A)");
        // 非 0 进提示,0 / 取不到不带码
        assert!(exit_message(Some(2)).contains('2'));
        assert_eq!(exit_message(Some(0)), t("paneGroup", "shellExited"));
        assert_eq!(exit_message(None), t("paneGroup", "shellExited"));
    }

    /// 「清除缓冲」:程序没了本地全清;AI 会话 / 全屏程序只清回看不发字节;认得出的 shell
    /// 与远程项目发 Ctrl+L;cmd 与认不出的不发(Ctrl+L 会变成命令行里的怪字符)。
    #[test]
    fn 清除缓冲按场合分三档() {
        use ClearBufferPlan as P;
        // 已经退出:谁都不会重画了,哪怕退出前在 AI 会话里
        assert_eq!(
            clear_buffer_plan(true, true, false, false, ShellKind::Pwsh),
            P::LocalAll
        );
        assert_eq!(
            clear_buffer_plan(false, true, false, false, ShellKind::Pwsh),
            P::ScrollbackOnly,
            "Claude Code 这类 TUI 自己绑了 Ctrl+L"
        );
        assert_eq!(
            clear_buffer_plan(false, false, true, true, ShellKind::Bash),
            P::ScrollbackOnly,
            "vim / less 里 Ctrl+L 另有意思,远程也一样"
        );
        for shell in [
            ShellKind::Pwsh,
            ShellKind::WindowsPowerShell,
            ShellKind::Bash,
            ShellKind::Zsh,
            ShellKind::Fish,
            ShellKind::Nu,
            ShellKind::Wsl,
        ] {
            assert_eq!(
                clear_buffer_plan(false, false, false, false, shell),
                P::ShellRedraw,
                "{shell:?}"
            );
        }
        for shell in [ShellKind::Cmd, ShellKind::Unknown] {
            assert_eq!(
                clear_buffer_plan(false, false, false, false, shell),
                P::ScrollbackOnly,
                "{shell:?}"
            );
        }
        assert_eq!(
            clear_buffer_plan(false, false, false, true, ShellKind::Cmd),
            P::ShellRedraw,
            "远程 pane 收的是服务器上的登录 shell,本地配的 shell 名不作数"
        );
    }

    /// shell 先按配置里的可执行文件认(名字是用户起的),认不出再按名字。
    #[test]
    fn shell_按可执行文件认_认不出再看名字() {
        let shell = |name: &str, command: &str| ShellConfig {
            name: name.into(),
            command: command.into(),
            args: None,
        };
        let shells = [
            shell("我的终端", r"C:\Windows\System32\cmd.exe"),
            shell("PS7", r"C:\Program Files\PowerShell\7\pwsh.exe"),
            shell("bash", "/opt/custom/sh-wrapper"),
        ];
        assert_eq!(
            shell_kind_of(&shells, "我的终端"),
            ShellKind::Cmd,
            "名字不像 cmd 也认得出"
        );
        assert_eq!(shell_kind_of(&shells, "PS7"), ShellKind::Pwsh);
        assert_eq!(
            shell_kind_of(&shells, "bash"),
            ShellKind::Bash,
            "可执行文件认不出时看名字"
        );
        assert_eq!(
            shell_kind_of(&shells, "zsh"),
            ShellKind::Zsh,
            "配置里已经没有了也按名字认"
        );
        assert_eq!(shell_kind_of(&[], "神秘 shell"), ShellKind::Unknown);
    }

    #[test]
    fn 保存缓冲的建议文件名() {
        let at = chrono::NaiveDate::from_ymd_opt(2026, 11, 8)
            .unwrap()
            .and_hms_opt(9, 5, 3)
            .unwrap();
        assert_eq!(buffer_file_name(at), "mini-term-20261108-090503.txt");
    }

    #[test]
    fn 悬停提示文案带上平台的修饰键() {
        let label = link_hover_hint_label();
        let modifier = if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl"
        };
        assert!(label.starts_with(modifier), "{label}");
        assert!(!label.contains('{'), "占位符要替换掉: {label}");
    }

    /// 目标偏移钳在 `[0, history]`:历史比锚点短(热改小了回滚行数)时不越界。
    #[test]
    fn 目标偏移钳在历史长度内() {
        assert_eq!(scroll_delta_to_top(-900, 0, 100), 100, "最多滚到历史顶端");
        assert_eq!(scroll_delta_to_top(-900, 0, 0), 0, "没有历史就不滚");
        // history 传了负数(不该发生)也不许算出负的目标偏移
        assert_eq!(scroll_delta_to_top(-900, 0, -3), 0);
    }
}
