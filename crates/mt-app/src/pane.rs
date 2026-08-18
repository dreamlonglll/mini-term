//! 一个终端 pane 的运行时:PTY + VT 状态机 + 渲染 + 键盘。
//!
//! 从原来 `main.rs` 里那条端到端竖切抽出来,补上三件事:
//! 1. **pane 编号**(`pty_id`):既是 `MINITERM_PTY_ID`(hook 回报的定位键),
//!    也是 `mt-ai` 里的 `pane_id`,还是 store 里 `terminals` 表的键;
//! 2. **AI 感知旁路**:写入前 `observe_input`、读出后 `observe_output`;
//! 3. **退出上报**:子进程退出 → 发 [`PaneEvent::Exited`],由 store 落成 `error`
//!    状态(与旧版 `pty-exit` → `updatePaneStatusByPty('error')` 同语义)。
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
    App, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, Styled, Task, Window, div,
    prelude::FluentBuilder, px,
};
use mt_pty::{PtySession, PtySpawn};
use mt_terminal::alacritty_terminal::event::Event as TermEvent;
use mt_terminal::alacritty_terminal::grid::Scroll;
use mt_terminal::{TermSize, TerminalEmulator};
use mt_ui::{TerminalElement, TerminalStyle, TerminalTheme, keystroke_to_bytes, paste_to_bytes};

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
    style: TerminalStyle,
    theme: TerminalTheme,
    ai: AiBridge,
    /// 子进程已退出。
    exited: bool,
    /// PTY 起不来时的错误文本(直接显示给用户,不吞)。
    spawn_error: Option<String>,
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

        let focus = cx.focus_handle();
        ai.add_pane(pty_id);

        Self {
            pty_id,
            emulator,
            pty,
            focus,
            style,
            theme,
            ai,
            exited: false,
            spawn_error,
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
                TermEvent::ColorRequest(index, format) => {
                    let rgb = self.theme_color_rgb(index);
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

    /// 调色板查询的应答:按当前主题回真值(ANSI 0..16 之外回默认前景)。
    fn theme_color_rgb(&self, index: usize) -> mt_terminal::alacritty_terminal::vte::ansi::Rgb {
        let hsla = self
            .theme
            .ansi
            .get(index)
            .copied()
            .unwrap_or(self.theme.foreground);
        let rgba = gpui::Rgba::from(hsla);
        mt_terminal::alacritty_terminal::vte::ansi::Rgb {
            r: (rgba.r * 255.0).round() as u8,
            g: (rgba.g * 255.0).round() as u8,
            b: (rgba.b * 255.0).round() as u8,
        }
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus);
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

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let mods = &keystroke.modifiers;

        // 应用层快捷键:Ctrl+Shift+C / Ctrl+Shift+V。
        if mods.control && mods.shift {
            match keystroke.key.as_str() {
                "c" => {
                    if let Some(text) = self.emulator.with_term(|t| t.selection_to_string())
                        && !text.is_empty()
                    {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                }
                "v" => self.paste(cx),
                _ => {}
            }
            return;
        }

        let Some(bytes) = keystroke_to_bytes(keystroke, self.emulator.mode()) else {
            return;
        };
        // 有键入就回到底部 —— 和所有终端一样。
        self.emulator
            .with_term_mut(|term| term.scroll_display(Scroll::Bottom));
        self.write(&bytes, cx);
        cx.notify();
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|it| it.text()) else {
            return;
        };
        let bytes = paste_to_bytes(&text, self.emulator.mode());
        self.write(&bytes, cx);
        cx.notify();
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(err) = self.spawn_error.clone() {
            return div()
                .size_full()
                .bg(self.theme.background)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(13.0))
                .text_color(crate::ui::color_error())
                .child(format!("终端启动失败:{err}"));
        }

        let element = {
            let this = cx.weak_entity();
            let this_for_input = this.clone();
            TerminalElement::new(
                ("terminal", self.pty_id),
                self.emulator.clone(),
                self.focus.clone(),
                self.style.clone(),
                self.theme.clone(),
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
            // alt screen 里滚轮 → 方向键。元素不持有 PTY,字节从这里回来。
            .on_input(move |bytes, _window, cx| {
                let bytes = bytes.to_vec();
                let _ = this_for_input.update(cx, |pane: &mut TerminalPane, cx| {
                    pane.write(&bytes, cx);
                });
            })
        };

        div()
            .size_full()
            .relative()
            .bg(self.theme.background)
            .track_focus(&self.focus)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, window, _cx| {
                    window.focus(&this.focus);
                }),
            )
            .child(element)
            // 子进程没了但 pane 留着(与旧版一致:画面可回看,不自动关)
            .when(self.exited, |el| {
                el.child(
                    div()
                        .absolute()
                        .bottom_2()
                        .right_3()
                        .text_size(px(12.0))
                        .text_color(crate::ui::color_error())
                        .child("shell 已退出"),
                )
            })
    }
}
