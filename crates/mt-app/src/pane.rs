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

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    App, AppContext, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Pixels, Point, Render, Styled, Task, Window, div,
    prelude::FluentBuilder, px,
};
use mt_pty::{PtySession, PtySpawn};
use mt_terminal::alacritty_terminal::event::Event as TermEvent;
use mt_terminal::{TermSize, TerminalEmulator};
use mt_ui::{CopiedTip, DwellConfig, TerminalStyle, TerminalTheme, TerminalView};

use crate::ai::AiBridge;

/// pane 发给上层的事件。
pub enum PaneEvent {
    /// 子进程退出(退出码取不到为 `None`)。
    Exited(Option<u32>),
    /// 用户往这个 pane 里键入了东西 —— store 据此清掉 attention 黄灯
    /// (旧版 `clearPaneAttentionByPty`:键入即视为「已在处理待确认事项」)。
    UserInput,
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
    /// 建视图时用过一次;留着是为了字号/字族热更新那一批(`view.set_style`)
    /// 能拿到「当前是什么」来做比较。
    #[allow(dead_code)]
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
    /// 唤醒任务的句柄。掉了任务就没了,必须存着。
    _wake: Task<()>,
}

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
        ai: AiBridge,
        cx: &mut Context<Self>,
    ) -> Self {
        // 首帧还没量过字体,先给个能跑的初值;真正的尺寸在元素 prepaint 里量出来
        // 之后通过 on_grid_resize 回来纠正。
        let emulator = Arc::new(TerminalEmulator::new(TermSize::new(
            spec.cols as usize,
            spec.rows as usize,
        )));

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
            _wake: wake,
        }
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

        if let Some(pty) = self.pty.as_ref()
            && let Err(err) = pty.write(bytes)
        {
            eprintln!("[pane {}] 写 PTY 失败: {err:#}", self.pty_id);
        }
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

    /// 丢弃组合中的预编辑串。切 tab / 关 pane 之前调,免得残影留在画面上。
    pub fn clear_preedit(&mut self, cx: &mut Context<Self>) {
        self.view.update(cx, |view, cx| view.clear_preedit(cx));
    }

    /// 关闭 pane:杀子进程 + 清掉 AI 感知里的一切痕迹。
    pub fn shutdown(&mut self) {
        if let Some(pty) = self.pty.as_mut()
            && let Err(err) = pty.kill()
        {
            eprintln!("[pane {}] kill 失败: {err:#}", self.pty_id);
        }
        self.pty = None;
        self.ai.remove_pane(self.pty_id);
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        // pane 实体被丢弃(项目移除 / 应用退出)时同样要回收 —— 否则后端留一个
        // 谁也看不见、谁也杀不掉的孤儿子进程。
        if self.pty.is_some() {
            self.shutdown();
        }
    }
}

impl Focusable for TerminalPane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalPane {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(err) = self.spawn_error.clone() {
            return div()
                .size_full()
                .bg(self.theme.background)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(13.0))
                .text_color(crate::ui::color_error())
                .child(format!("{}:{err}", crate::i18n::t("paneGroup", "startFailed")));
        }

        // 焦点 / key_context / 按键 / 左键聚焦全在 TerminalView 里,这里只剩一行。
        // `.bg()` 留在宿主:主题带背景图时终端背景是半透明的,画两层等于把透明度平方。
        div()
            .size_full()
            .relative()
            .bg(self.theme.background)
            .child(self.view.clone())
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
                        .text_size(px(12.0))
                        .text_color(crate::ui::color_error())
                        // 旧版没有这个角标(子进程退出后 pane 直接标红),
                        // `paneGroup.shellExited` 是 M 批往 TS 源头补的条目。
                        .child(crate::i18n::t("paneGroup", "shellExited")),
                )
            })
    }
}
