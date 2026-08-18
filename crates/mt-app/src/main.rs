//! mini-term 的 GPUI 应用壳。
//!
//! 现阶段是一条**端到端的最小竖切**:窗口里起一个真 PTY,字节喂进
//! [`mt_terminal::TerminalEmulator`],由 [`mt_ui::TerminalElement`] 画出来,
//! 键盘/粘贴写回 PTY,窗口尺寸变化同步 grid 与 PTY。
//!
//! 全局状态(对应现有 `src/store.ts`)、三栏布局、Tab 与 SplitNode 树还没动工 ——
//! 先把「PTY → VT → 渲染 → 键入」这条链跑通,后面才有地基可搭。
//!
//! # 重绘唤醒
//!
//! PTY reader 在**独立线程**上,gpui 的 `AsyncApp` 内部是 `Weak<AppCell>`(Rc),
//! 不能跨线程持有,所以 reader 线程没法直接去 `notify`。这里走标准做法:
//! reader 线程往一个 `futures::mpsc` 无界 channel 里丢一个信号,主线程上
//! 由 `cx.spawn` 起的前台任务 `await` 这个 channel,醒来后 `cx.notify()`。
//!
//! 这是**事件驱动**,不是定时轮询 —— 空闲时一帧都不画。唯一的定时器是收到信号
//! 后的 16ms 节流:刷屏时 reader 每读一块就发一个信号,不节流会让重绘频率跟着
//! read 的次数走(远高于 60fps);节流之后重绘上限就是 ~60fps,而这期间 reader
//! 线程照旧把字节喂进 grid,一帧画的是攒够的结果。

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    App, AppContext, Application, Bounds, ClipboardItem, Context, Entity, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, Styled,
    Task, Window, WindowBounds, WindowOptions, div, prelude::FluentBuilder, px, size,
};
use mt_pty::{PtySession, PtySpawn};
use mt_terminal::alacritty_terminal::event::Event as TermEvent;
use mt_terminal::alacritty_terminal::grid::Scroll;
use mt_terminal::{TermSize, TerminalEmulator};
use mt_ui::{TerminalElement, TerminalStyle, TerminalTheme, keystroke_to_bytes, paste_to_bytes};

/// 一个终端 pane:PTY + VT 状态 + 焦点。将来会挂在 SplitNode 树的叶子上。
struct TerminalPane {
    emulator: Arc<TerminalEmulator>,
    pty: Option<PtySession>,
    focus: FocusHandle,
    style: TerminalStyle,
    theme: TerminalTheme,
    /// 子进程已退出。
    exited: bool,
    /// 唤醒任务的句柄。掉了任务就没了,必须存着。
    _wake: Task<()>,
}

impl TerminalPane {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let style = TerminalStyle::default();
        // 首帧还没量过字体,先给个能跑的初值;真正的尺寸在元素 prepaint 里量出来
        // 之后通过 on_grid_resize 回来纠正。
        let emulator = Arc::new(TerminalEmulator::new(TermSize::new(80, 24)));

        let (tx, mut rx) = mpsc::unbounded::<()>();
        let pty = {
            let emulator = emulator.clone();
            PtySession::spawn(default_shell(), move |bytes| {
                // reader 线程:直接推进状态机,没有 IPC、没有批缓冲、没有序列化。
                emulator.advance(bytes);
                let _ = tx.unbounded_send(());
            })
        };

        let pty = match pty {
            Ok(pty) => Some(pty),
            Err(err) => {
                eprintln!("PTY 启动失败: {err:#}");
                None
            }
        };

        let wake = cx.spawn(async move |this, cx| {
            while rx.next().await.is_some() {
                // 把已经排队的信号一次抽干,避免一次读一个信号地重绘。
                while rx.try_recv().is_ok() {}
                if this
                    .update(cx, |pane, cx| {
                        pane.drain_term_events(cx);
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
            // channel 关闭 = reader 线程读到 EOF = 子进程没了。
            let _ = this.update(cx, |pane, cx| {
                pane.exited = true;
                cx.notify();
            });
        });

        let focus = cx.focus_handle();
        window.focus(&focus);

        Self {
            emulator,
            pty,
            focus,
            style,
            theme: TerminalTheme::default(),
            exited: false,
            _wake: wake,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        if let Some(pty) = self.pty.as_ref()
            && let Err(err) = pty.write(bytes)
        {
            eprintln!("写 PTY 失败: {err:#}");
        }
    }

    /// alacritty 内部产生的事件。**`PtyWrite` 必须处理** —— DA/DSR/光标位置查询
    /// 这些是终端要回给程序的应答,吞掉会让 shell 与 TUI 程序卡在等回应上。
    fn drain_term_events(&mut self, cx: &mut App) {
        for event in self.emulator.events().drain() {
            match event {
                TermEvent::PtyWrite(text) => self.write(text.as_bytes()),
                TermEvent::ClipboardStore(_, text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                TermEvent::ClipboardLoad(_, format) => {
                    let text = cx.read_from_clipboard().and_then(|it| it.text());
                    let payload = format(text.as_deref().unwrap_or(""));
                    self.write(payload.as_bytes());
                }
                TermEvent::ColorRequest(index, format) => {
                    // 调色板查询:先按主题里的 ANSI 值回。主题桥接上之后这里换成真值。
                    let rgb = mt_terminal::alacritty_terminal::vte::ansi::Rgb {
                        r: 0,
                        g: 0,
                        b: 0,
                    };
                    let _ = index;
                    self.write(format(rgb).as_bytes());
                }
                TermEvent::TextAreaSizeRequest(format) => {
                    let size = self.emulator.term_size();
                    let payload = format(mt_terminal::alacritty_terminal::event::WindowSize {
                        num_lines: size.screen_lines as u16,
                        num_cols: size.columns as u16,
                        cell_width: 1,
                        cell_height: 1,
                    });
                    self.write(payload.as_bytes());
                }
                _ => {}
            }
        }
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
        self.write(&bytes);
        cx.notify();
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|it| it.text()) else {
            return;
        };
        let bytes = paste_to_bytes(&text, self.emulator.mode());
        self.write(&bytes);
        cx.notify();
    }
}

impl Focusable for TerminalPane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let element = {
            let this = cx.weak_entity();
            let this_for_input = this.clone();
            TerminalElement::new(
                "terminal",
                self.emulator.clone(),
                self.focus.clone(),
                self.style.clone(),
                self.theme.clone(),
            )
            .on_grid_resize(move |size: TermSize, _window, cx| {
                // grid 尺寸是渲染侧量出来的(可用像素 ÷ cell 尺寸),
                // PTY 必须跟着改,否则 shell 换行位置与画面对不上。
                let _ = this.update(cx, |pane: &mut TerminalPane, _cx| {
                    if let Some(pty) = pane.pty.as_ref()
                        && let Err(err) =
                            pty.resize(size.screen_lines as u16, size.columns as u16)
                    {
                        eprintln!("resize PTY 失败: {err:#}");
                    }
                });
            })
            // alt screen 里滚轮 → 方向键。元素不持有 PTY,字节从这里回来。
            .on_input(move |bytes, _window, cx| {
                let bytes = bytes.to_vec();
                let _ = this_for_input.update(cx, |pane: &mut TerminalPane, _cx| {
                    pane.write(&bytes);
                });
            })
        };

        div()
            .size_full()
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
            .when(self.exited, |el| {
                el.child(
                    div()
                        .absolute()
                        .bottom_2()
                        .right_3()
                        .text_color(mt_ui::rgb8(0xff, 0x7b, 0x7b))
                        .text_size(px(12.0))
                        .child("shell 已退出"),
                )
            })
    }
}

/// 本机默认 shell。Windows 上优先 pwsh(PATH 里有就用),否则 powershell.exe。
fn default_shell() -> PtySpawn {
    let env = vec![
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
    ];

    #[cfg(windows)]
    let (program, args) = {
        let pwsh = which("pwsh.exe");
        if pwsh {
            ("pwsh.exe".to_string(), vec!["-NoLogo".to_string()])
        } else {
            ("powershell.exe".to_string(), vec!["-NoLogo".to_string()])
        }
    };

    #[cfg(not(windows))]
    let (program, args) = (
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string()),
        vec!["-l".to_string()],
    );

    PtySpawn {
        program,
        args,
        cwd: std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string()),
        env,
        rows: 24,
        cols: 80,
    }
}

#[cfg(windows)]
fn which(exe: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(exe).is_file())
        })
        .unwrap_or(false)
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.0), px(700.0)), cx);
        let window = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                let pane: Entity<TerminalPane> = cx.new(|cx| TerminalPane::new(window, cx));
                pane
            },
        );
        if let Err(err) = window {
            eprintln!("打开窗口失败: {err:#}");
            return;
        }
        cx.activate(true);
    });
}
