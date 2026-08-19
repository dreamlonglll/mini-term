//! 一个终端 pane 的运行时:PTY + VT 状态机 + 渲染 + 键盘。
//!
//! 从原来 `main.rs` 里那条端到端竖切抽出来,补上三件事:
//! 1. **pane 编号**(`pty_id`):既是 `MINITERM_PTY_ID`(hook 回报的定位键),
//!    也是 `mt-ai` 里的 `pane_id`,还是 store 里 `terminals` 表的键;
//! 2. **AI 感知旁路**:写入前 `observe_input`、读出后 `observe_output`;
//! 3. **退出上报**:子进程退出 → 发 [`PaneEvent::Exited`],由 store 落成 `error`
//!    状态(与旧版 `pty-exit` → `updatePaneStatusByPty('error')` 同语义)。
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
//! 这是**事件驱动**,不是定时轮询 —— 空闲时一帧都不画。唯一的定时器是收到信号后
//! 的 16ms 节流:刷屏时 reader 每读一块就发一个信号,不节流会让重绘频率跟着 read
//! 次数走(远高于 60fps)。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    App, AppContext, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels, Point,
    Render, Styled, Task, Window, div, prelude::FluentBuilder, px,
};
use mt_pty::{PtySession, PtySpawn};
use mt_terminal::alacritty_terminal::event::Event as TermEvent;
use mt_terminal::alacritty_terminal::grid::{Dimensions as _, Scroll};
use mt_terminal::alacritty_terminal::term::TermMode;
use mt_terminal::{TermSize, TerminalEmulator};
use mt_ui::terminal::{MouseMods, prefers_local_handling};
use mt_ui::{
    CopiedTip, DwellConfig, FlashLine, PasteAction, TerminalSearch, TerminalSearchBar,
    TerminalStyle, TerminalTheme, TerminalView,
};

use crate::ai::AiBridge;
use crate::clipboard::{self, PasteTarget};
use crate::i18n::{t, tr};
use crate::markers::MarkerBatch;
use crate::menu::{self, MenuItem};
use crate::notify::ToastKind;
use crate::overlay;
use crate::store::AppStore;
use crate::toast;

/// pane 发给上层的事件。
pub enum PaneEvent {
    /// 子进程退出(退出码取不到为 `None`)。
    Exited(Option<u32>),
    /// 用户往这个 pane 里键入了东西 —— store 据此清掉 attention 黄灯
    /// (旧版 `clearPaneAttentionByPty`:键入即视为「已在处理待确认事项」)。
    UserInput,
    /// 用户往 AI 会话里提交了一行 → 打一批任务标记(⚑),锚点已经取好。
    ///
    /// 走事件而不是在 [`TerminalPane::write`] 里直接写 store:`write` 有一条
    /// 调用路径是 `AppStore::write_to_pane`(在 `store.update` 里调),那里再去
    /// `AppStore::global(cx).update` 就是同一实体的嵌套 update,gpui 直接 panic。
    AiMarks(MarkerBatch),
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
    pty: Option<PtySession>,
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
    /// 子进程已退出。
    exited: bool,
    /// PTY 起不来时的错误文本(直接显示给用户,不吞)。
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
    /// 标记跳转后那 300ms 闪烁的撤销计时器。与 `_tip_timer` 同理必须存句柄:
    /// 连着跳两条时上一个计时器随之被丢弃,否则第一次的到点回调会把第二次刚
    /// 亮起来的那一行提前抹掉。
    _flash_timer: Option<Task<()>>,
    /// 唤醒任务的句柄。掉了任务就没了,必须存着。
    _wake: Task<()>,
}

/// 标记跳转后整行闪烁的底色与时长(`terminalCache.ts:193-194` 的
/// `rgba(245, 197, 24, 0.33)` / `300ms`)。
///
/// 原版这两个值是写死的字面量、不走 CSS 变量,所以这里也不进 [`crate::ui`] 调色板。
const FLASH_COLOR: u32 = 0xf5_c5_18_54;
const FLASH_DURATION: Duration = Duration::from_millis(300);

impl EventEmitter<PaneEvent> for TerminalPane {}

impl TerminalPane {
    /// `user_env` 是项目级环境变量:走 [`mt_pty::PtyOptions::user_env`] 而不是
    /// `spec.env`,因为前者会被 `MINITERM_` 前缀过滤挡一道 —— 用户手改
    /// `config.json` 也覆盖不掉内部协议变量。
    pub fn new(
        pty_id: u32,
        spec: PtySpawn,
        user_env: Vec<(String, String)>,
        style: TerminalStyle,
        theme: TerminalTheme,
        dwell: DwellConfig,
        scrollback: usize,
        ai: AiBridge,
        cx: &mut Context<Self>,
    ) -> Self {
        // 首帧还没量过字体,先给个能跑的初值;真正的尺寸在元素 prepaint 里量出来
        // 之后通过 on_grid_resize 回来纠正。
        //
        // 回滚行数(`config.terminalScrollback`)必须在这一刻喂进 alacritty 的
        // `term::Config`:它决定 grid 的历史容量,建完再改只能靠 `set_options`。
        let emulator = Arc::new(TerminalEmulator::with_scrollback(
            TermSize::new(spec.cols as usize, spec.rows as usize),
            scrollback,
        ));

        let (tx, mut rx) = mpsc::unbounded::<PaneSignal>();
        let exit_tx = tx.clone();
        let options = mt_pty::PtyOptions::default()
            .with_user_env(user_env)
            .on_exit(move |code| {
                let _ = exit_tx.unbounded_send(PaneSignal::Exit(code));
            });

        let pty = {
            let emulator = emulator.clone();
            let ai = ai.clone();
            PtySession::spawn_with_options(spec, options, move |bytes| {
                // reader 线程:直接推进状态机,没有 IPC、没有批缓冲、没有序列化。
                emulator.advance(bytes);
                // AI 感知的输出旁路(命令 echo 回扫 + 输出活跃度)
                ai.perception().observe_output(pty_id, bytes);
                // Git 面板的输出旁路(外部跑了 git 命令 → 刷新变更与仓库元信息)。
                // **这条线程上不跑任何模式匹配**:总闸关着时只有一次原子读,
                // 开着时也只是把尾部字节塞进有界环形缓冲,5 条口径在主线程节拍上跑。
                // 详见 `git_watch` 模块注释(后续 Y 批的 git 着色与本条共用)。
                crate::git_watch::observe_output(pty_id, bytes);
                let _ = tx.unbounded_send(PaneSignal::Output);
            })
        };

        let (pty, spawn_error) = match pty {
            Ok(pty) => (Some(pty), None),
            Err(err) => {
                let msg = format!("{err:#}");
                eprintln!("[pane {pty_id}] PTY 启动失败: {msg}");
                (None, Some(msg))
            }
        };

        // WSL 启动器重写的一次性告知(`App.tsx:367-379`)。判定与重写早在
        // `mt_pty::launch::plan` 里做完了,结论挂在会话上 —— 这里只是**唯一的
        // 读取方**(此前全仓零调用,提示因此一直缺着)。
        //
        // 「一次性」= 每个新 PTY 各推一次,不去重(原版同款):同一个项目开两个
        // 终端就该看到两条,那正是「这两个都被改用 wsl.exe 启动了」的意思。
        if let Some(wsl) = pty.as_ref().and_then(|p| p.wsl_override()) {
            toast::push_wsl_override(&wsl.distro, &wsl.unix_path, cx);
        }

        let wake = cx.spawn(async move |this, cx| {
            while let Some(signal) = rx.next().await {
                let mut exit: Option<Option<u32>> = None;
                match signal {
                    PaneSignal::Output => {}
                    PaneSignal::Exit(code) => exit = Some(code),
                }
                // 把已经排队的信号一次抽干,避免一次读一个信号地重绘。
                while let Ok(extra) = rx.try_recv() {
                    if let PaneSignal::Exit(code) = extra {
                        exit = Some(code);
                    }
                }
                if this
                    .update(cx, |pane, cx| {
                        pane.drain_term_events(cx);
                        if let Some(code) = exit {
                            pane.exited = true;
                            cx.emit(PaneEvent::Exited(code));
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
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
                    // 否则 shell 换行位置与画面对不上。
                    let _ = this.update(cx, |pane: &mut TerminalPane, _cx| {
                        let Some(pty) = pane.pty.as_ref() else { return };
                        match pty.resize_if_changed(size.screen_lines as u16, size.columns as u16) {
                            // 只有**真实下发**的 resize 才开重绘冷却窗口:同尺寸的
                            // resize 不会引起 TUI 重绘,平白开冷却会漏掉真的 AI 活跃
                            Ok(true) => pane.ai.perception().note_resize(pane.pty_id),
                            Ok(false) => {}
                            Err(err) => eprintln!("[pane {}] resize 失败: {err:#}", pane.pty_id),
                        }
                    });
                })
                // **唯一**的写 PTY 通道:键盘 / 粘贴 / IME 提交 / 鼠标上报 /
                // alt screen 滚轮全走这里,`write()` 里的 AI 感知旁路一处不落。
                .on_input(move |bytes, _window, cx| {
                    let bytes = bytes.to_vec();
                    let _ = this_for_input.update(cx, |pane: &mut TerminalPane, cx| {
                        pane.write(&bytes, cx);
                    });
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

        Self {
            pty_id,
            emulator,
            pty,
            focus,
            view,
            style,
            theme,
            ai,
            exited: false,
            spawn_error,
            copied_tip: None,
            _tip_timer: None,
            search,
            search_bar: None,
            _flash_timer: None,
            _wake: wake,
        }
    }

    /// PTY 起不来时的错误原文;`None` = 起来了。
    ///
    /// 视图里已经把它画成一行红字(见 `Render` 实现),这个访问器是给**回执**用的:
    /// 移动端发起会话要区分「pane 建出来了」与「PTY 真的起来了」——
    /// [`Self::write`] 在没有 PTY 时是静默丢弃的,不看这一条就会把「终端起不来」
    /// 报成成功,手机侧只能干等 15s 超时。
    pub fn spawn_error(&self) -> Option<&str> {
        self.spawn_error.as_deref()
    }

    /// Ctrl+F。打开查找条,已经开着就把焦点送回输入框并全选。
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
    ///    关闭走 Esc / `✕`。这里照此:第二次按 Ctrl+F = 聚焦 + 全选。
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bar) = self.search_bar.clone() {
            bar.update(cx, |bar, cx| bar.focus_input(window, cx));
            return;
        }
        // 覆盖物栈里登记一条(按 pty_id 区分)。它**不挡**全局快捷键,
        // 只是防叠开 + 让「现在压着什么」有唯一真相,见 `overlay` 模块注释。
        if !overlay::push(overlay::terminal_search(self.pty_id)) {
            return;
        }
        let search = self.search.clone();
        let emulator = self.emulator.clone();
        let this = cx.weak_entity();
        let bar = cx.new(|cx| {
            TerminalSearchBar::new(search, emulator, window, cx).on_close(move |window, cx| {
                let _ = this.update(cx, |pane: &mut TerminalPane, cx| {
                    pane.dismiss_search(window, cx);
                });
            })
        });
        // 开引擎 + 按已有关键词搜一遍 + 聚焦全选
        bar.update(cx, |bar, cx| bar.open(window, cx));
        self.search_bar = Some(bar);
        cx.notify();
    }

    /// 收起查找条(Esc / `✕` 都走这里)。
    ///
    /// ⚠️ **焦点必须还给终端**:不还的话焦点停在已卸载的输入框上,用户接着敲的字
    /// 全部落空,还得先用鼠标点一下终端才能继续 —— 原版 `closeTerminalSearch()`
    /// 里那句 `term.focus()` 就是为这个。
    fn dismiss_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_bar.take().is_none() {
            return;
        }
        overlay::pop(overlay::terminal_search(self.pty_id));
        window.focus(&self.focus);
        cx.notify();
    }

    /// 往 PTY 写字节。
    ///
    /// **`observe_input` 必须在字节交给 PTY 之前调** —— 焦点冷却窗口要早于 TUI 对
    /// 焦点事件的重绘响应抵达,否则那波重绘会被当成 AI 活跃(与原 `write_pty` 同序)。
    pub fn write(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        // 行快照:↑ 历史召回 / Tab 补全会让 shell 整行改写,本地输入缓冲重建不出来,
        // 只能在回车前抓一份当前可见行补判(见 observe_input_with_line_snapshot)。
        let snapshot = if bytes.contains(&b'\r') {
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
        // AI 任务标记:**必须在这里取,不能挪到异步 tick 上**。`observe_input` 是
        // 同步的,回车那一刻 `pending_submits` 里已经有这条了;而此刻 PTY 还没回显
        // 换行(`pty.write` 在下面几行),光标仍停在用户输入的那一行上 —— 锚点直接
        // 取 `cursor.point.line` 即可,不需要原版 `registerMarker(-1)` 的减一
        // (`terminalCache.ts:558-559` 的 `-1` 正是为了补偿「回显已换行」)。
        // 一挪到 tick 上就得重新面对「减几行」这个问题。
        if let Some(batch) = self.take_marker_batch() {
            cx.emit(PaneEvent::AiMarks(batch));
        }

        if let Some(pty) = self.pty.as_ref()
            && let Err(err) = pty.write(bytes)
        {
            eprintln!("[pane {}] 写 PTY 失败: {err:#}", self.pty_id);
        }
    }

    /// 取走这一轮的用户提交并当场折算成锚点。`None` = 没有提交 / 不该打点。
    ///
    /// **alt screen 一律跳过**(照抄 `terminalCache.ts:554-557`):alt grid 的
    /// `max_scroll_limit` 是 0,没有回看缓冲,打了也无处可跳 —— 走 TUI 的 AI
    /// (Claude Code / Codex)基本全落在这个分支,这正是「⚑ 按钮平时不出现」的原因。
    /// 注意 `drain_submits` 是**取走即清**,所以这一句要放在闸门之后:
    /// 提前抽干等于把 alt screen 期间的提交默默吞掉,退出 TUI 后也补不回来。
    fn take_marker_batch(&self) -> Option<MarkerBatch> {
        if self.emulator.mode().contains(TermMode::ALT_SCREEN) {
            return None;
        }
        let submits: Vec<(String, i64)> = self
            .ai
            .perception()
            .drain_submits(self.pty_id)
            .into_iter()
            .map(|s| (s.line, s.ts))
            .collect();
        if submits.is_empty() {
            return None;
        }
        let (line, history) = self
            .emulator
            .with_term(|term| (term.grid().cursor.point.line.0, term.history_size() as i32));
        Some(MarkerBatch {
            submits,
            anchor: line + history,
            history,
            max_scrollback: self.emulator.scrollback() as i32,
        })
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

    /// alacritty 内部产生的事件。**`PtyWrite` 必须处理** —— DA/DSR/光标位置查询
    /// 这些是终端要回给程序的应答,吞掉会让 shell 与 TUI 程序卡在等回应上。
    fn drain_term_events(&mut self, cx: &mut App) {
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
                    let payload = format(mt_terminal::alacritty_terminal::event::WindowSize {
                        num_lines: size.screen_lines as u16,
                        num_cols: size.columns as u16,
                        cell_width: 1,
                        cell_height: 1,
                    });
                    self.write_raw(payload.as_bytes());
                }
                _ => {}
            }
        }
    }

    /// 不经 AI 输入旁路的写入(终端应答 / 内部序列)。
    fn write_raw(&self, bytes: &[u8]) {
        if let Some(pty) = self.pty.as_ref()
            && let Err(err) = pty.write(bytes)
        {
            eprintln!("[pane {}] 写 PTY 失败: {err:#}", self.pty_id);
        }
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus);
    }

    /// 当前有没有可复制的选区(空串不算 —— 选中一段空白后「复制」该是灰的)。
    fn has_selection(&self) -> bool {
        self.emulator
            .with_term(|term| term.selection_to_string())
            .is_some_and(|text| !text.is_empty())
    }

    /// 换终端配色(主题包切换 / 亮暗切换)。
    ///
    /// 宿主这份 `theme` 也要更新 —— `.bg()` 与 OSC 调色板应答用得着。
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
        self.view.update(cx, |view, cx| view.set_style(style, cx));
        cx.notify();
    }

    /// 换拖选停留自动复制时长(`config.selectionAutoCopySecs`)。
    pub fn set_selection_dwell(&mut self, dwell: DwellConfig, cx: &mut Context<Self>) {
        self.view
            .update(cx, |view, cx| view.set_selection_dwell(dwell, cx));
    }

    /// 换回滚行数。调小时 alacritty 当场裁历史并释放内存。
    ///
    /// **不碰视图**:grid 的容量变化不改任何渲染参数,下一帧照常读当前 grid。
    pub fn set_scrollback(&mut self, lines: usize) {
        self.emulator.set_scrollback(lines);
    }

    /// 丢弃组合中的预编辑串。切 tab / 关 pane 之前调,免得残影留在画面上。
    pub fn clear_preedit(&mut self, cx: &mut Context<Self>) {
        self.view.update(cx, |view, cx| view.clear_preedit(cx));
    }

    /// 关闭 pane:杀子进程 + 清掉 AI 感知里的一切痕迹 + 收掉查找条。
    pub fn shutdown(&mut self) {
        if let Some(pty) = self.pty.as_mut()
            && let Err(err) = pty.kill()
        {
            eprintln!("[pane {}] kill 失败: {err:#}", self.pty_id);
        }
        self.pty = None;
        self.ai.remove_pane(self.pty_id);
        self.close_search_state();
    }

    /// 丢掉查找状态(关键词一并清掉)。**不碰焦点** —— 这条路上终端马上就没了,
    /// 与原版 `closeTerminalSearchFor(ptyId)` 同语义(它同样不去 focus 已死的终端)。
    fn close_search_state(&mut self) {
        self.search.borrow_mut().clear();
        if self.search_bar.take().is_some() {
            overlay::pop(overlay::terminal_search(self.pty_id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// 目标偏移钳在 `[0, history]`:历史比锚点短(热改小了回滚行数)时不越界。
    #[test]
    fn 目标偏移钳在历史长度内() {
        assert_eq!(scroll_delta_to_top(-900, 0, 100), 100, "最多滚到历史顶端");
        assert_eq!(scroll_delta_to_top(-900, 0, 0), 0, "没有历史就不滚");
        // history 传了负数(不该发生)也不许算出负的目标偏移
        assert_eq!(scroll_delta_to_top(-900, 0, -3), 0);
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        // pane 实体被丢弃(项目移除 / 应用退出)时同样要回收 —— 否则后端留一个
        // 谁也看不见、谁也杀不掉的孤儿子进程。
        if self.pty.is_some() {
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

/// 一次粘贴该往终端里写什么(`terminalCache.ts::pasteToTerminalInner` 的文本那一半)。
///
/// ```text
/// 剪贴板取文本 → 空则什么都不做
/// 开关开着 && 不是远程 pane && 命中阈值
///   ├─ 转存成功 → 写 "{映射后的路径}"(裸写,不走 bracketed paste)
///   └─ 转存失败 → 弹一条 paste-error toast,**继续往下粘原文**(老行为)
/// 否则 → 按 bracketed paste 粘原文
/// ```
///
/// # 与原版的三处偏差(逐条有理由)
///
/// 1. **剪贴板图片不处理** —— 那是另一个缺口,见 [`crate::clipboard`] 模块注释;
/// 2. **远程(SSH)pane 一律粘原文** —— 上传通道还没有,见 [`PasteTarget::Ssh`];
/// 3. **本地转存失败也弹 toast**。原版 `notifyPasteFailure` 开头就
///    `if (target.kind !== 'ssh') return`,本地写盘失败只有 console.error ——
///    规格把这条记成原版的隐性缺陷并建议「补一个兜底项目名」,这里照办:
///    项目名取该 pane 所属项目,取不到就退回 pane 的显示名。
///
/// # 为什么是自由函数而不是 `TerminalPane` 的方法
///
/// 钩子在 `TerminalView` 被可变借用时调用;方法版会诱使人写
/// `self.view.update(...)`,那就是同一实体的嵌套 update(gpui 当场 panic)。
/// 自由函数只拿 `pty_id` + `&mut App`,连碰到视图的机会都没有。
fn resolve_paste(pty_id: u32, cx: &mut gpui::App) -> PasteAction {
    let Some(text) = cx.read_from_clipboard().and_then(|it| it.text()) else {
        return PasteAction::None;
    };
    if text.is_empty() {
        return PasteAction::None;
    }

    let store = AppStore::global(cx);
    let (enabled, line_threshold, char_threshold, target, project_id, project_name) = {
        let s = store.read(cx);
        let cfg = s.config();
        let owner = s.pane_of_pty(pty_id);
        // 失败提示的标题行:项目名 →(取不到)pane 标签 →(还取不到)pty 编号。
        // 规格把「原版本地失败时拿到 undefined 项目名」记成隐性缺陷并要求补兜底,
        // 这一串就是那个兜底 —— 标题行永远不为空。
        let name = owner
            .as_ref()
            .and_then(|(pid, _)| s.project(pid))
            .map(|p| p.name.clone())
            .or_else(|| {
                owner.as_ref().and_then(|(pid, pane_id)| {
                    s.project_state(pid)
                        .and_then(|st| st.layout.as_ref())
                        .and_then(|l| l.pane(pane_id))
                        .map(|p| p.label().to_string())
                })
            })
            .unwrap_or_else(|| format!("pane {pty_id}"));
        (
            cfg.long_paste_to_file,
            cfg.long_paste_line_threshold,
            cfg.long_paste_char_threshold,
            clipboard::resolve_paste_target(s, pty_id),
            owner.map(|(pid, _)| pid).unwrap_or_default(),
            name,
        )
    };

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
                eprintln!("[pane {pty_id}] 粘贴内容转存失败: {detail}");
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
            .and_then(|st| st.layout.as_ref())
            .and_then(|l| l.pane(&pane_id))
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

impl Render for TerminalPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(err) = self.spawn_error.clone() {
            return div()
                .size_full()
                .bg(self.theme.background)
                .flex()
                .items_center()
                .justify_center()
                .text_size(crate::ui::font_px(13.0))
                .text_color(crate::ui::color_error())
                .child(format!("{}:{err}", crate::i18n::t("paneGroup", "startFailed")));
        }

        // 焦点 / key_context / 按键 / 左键聚焦全在 TerminalView 里,这里只剩一行。
        // `.bg()` 留在宿主:主题带背景图时终端背景是半透明的,画两层等于把透明度平方。
        div()
            .size_full()
            .relative()
            .bg(self.theme.background)
            // 终端右键菜单(`TerminalInstance.tsx` 的 handleContextMenu):
            // 「复制 / 粘贴」+ 分支段。SSH 子菜单那一段的功能 GPUI 侧还没有,
            // 不放占位。
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
                    let has_selection = this.has_selection();
                    let view_copy = this.view.clone();
                    let view_paste = this.view.clone();
                    let focus = this.focus.clone();
                    let mut entries = vec![
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
                            window.focus(&focus);
                        }),
                    ];
                    // 会话分支入口:终端本体右键与 tab 右键**同权**(用户在哪儿
                    // 右键都找得到),显隐口径与项的实现都是同一份
                    entries.extend(branch_entries_for_pty(this.pty_id, cx));
                    menu::show(event.position, entries, window, cx);
                }),
            )
            .child(self.view.clone())
            // 终端内查找条:右上角,距顶 6px、距右 14px —— 与原版
            // `rect.top + 6` / `rect.right - w - 14` 同款(那边是 rAF 每帧算出来的
            // fixed 坐标,这里由布局白拿)
            .when_some(self.search_bar.clone(), |el, bar| {
                el.child(div().absolute().top(px(6.0)).right(px(14.0)).child(bar))
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
            // 子进程没了但 pane 留着(与旧版一致:画面可回看,不自动关)
            .when(self.exited, |el| {
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
    }
}
