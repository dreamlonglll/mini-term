//! VT 状态机 + grid 模型。**不含 UI**,不依赖 gpui —— 渲染在 `mt-ui` / `mt-app`。
//!
//! 这里是 xterm.js 的替代品。分工:
//!
//! ```text
//! mt-pty        字节进出子进程          (无解析)
//! mt-terminal   字节 → grid 状态        (本 crate,无 UI)
//! mt-ui         grid 状态 → GPUI 元素   (无业务)
//! ```
//!
//! # xterm.js 白送、这里必须自己补的东西
//!
//! 按迁移优先级排列,每一条都是独立可验的:
//!
//! 1. **grid → 字形绘制**(`mt-ui`):含全角/组合字符的列宽判定。这是整个改造
//!    风险最高的一点,`project_renderer_alignment` 记的那套「双终端对照页 +
//!    截图逐列测量」诊断手法可以直接复用来验收。
//! 2. **鼠标选择与复制**:`alacritty_terminal::selection::Selection` 已提供
//!    语义(Simple / Block / Semantic / Lines),需要接上鼠标事件与剪贴板。
//! 3. **IME 组合输入**:GPUI 侧的 `InputHandler`,预编辑文本要浮在光标处。
//! 4. **链接检测 / 搜索**:alacritty 有 `RegexSearch`,但 hint 的 UI 要自己做。
//! 5. **图片协议(Sixel / Kitty)**:alacritty_terminal **不支持**,当前 xterm.js
//!    侧若有依赖需要单独评估,不要默认它会跟着过来。
//!
//! # 背景图与半透明
//!
//! 渲染 cell 背景时,**背景色等于默认背景的格子不要发 quad**,让下层的背景图
//! 直接透出。这比 xterm.js 的 `allowTransparency: true` 干净,也没有它在 WebGL
//! renderer 下的性能代价。注意透明叠层的 GPU overdraw —— 参见
//! `docs/gpui-migration.md` 里从 oxideterm 补丁清单反推出来的坑位表。

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use parking_lot::Mutex;
use std::sync::Arc;

/// 终端尺寸。alacritty_terminal 只要求实现 `Dimensions`,不提供现成类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermSize {
    pub columns: usize,
    pub screen_lines: usize,
}

impl TermSize {
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        Self {
            columns,
            screen_lines,
        }
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// alacritty 内部事件(标题变更、响铃、剪贴板请求、PTY 写回等)的出口。
///
/// 注意 `send_event` 会在 **reader 线程**上被调用,所以这里只做入队,
/// 真正的处理交给 UI 线程去 drain。
#[derive(Clone, Default)]
pub struct EventQueue {
    inner: Arc<Mutex<Vec<Event>>>,
}

impl EventQueue {
    pub fn drain(&self) -> Vec<Event> {
        std::mem::take(&mut *self.inner.lock())
    }
}

impl EventListener for EventQueue {
    fn send_event(&self, event: Event) {
        self.inner.lock().push(event);
    }
}

/// 一个终端的完整状态:VT 解析器 + grid。
///
/// 线程模型:`advance` 由 PTY reader 线程调用,渲染由 UI 线程读 `term()`。
/// 两侧共用同一把锁 —— 这是 GPUI 架构下唯一需要的同步原语,取代了原来
/// 「有界 channel + 双水位背压 + 前端水位回调」那整条链路。
pub struct TerminalEmulator {
    term: Arc<Mutex<Term<EventQueue>>>,
    parser: Mutex<Processor>,
    events: EventQueue,
}

impl TerminalEmulator {
    pub fn new(size: TermSize) -> Self {
        let events = EventQueue::default();
        let term = Term::new(Config::default(), &size, events.clone());
        Self {
            term: Arc::new(Mutex::new(term)),
            parser: Mutex::new(Processor::new()),
            events,
        }
    }

    /// 把刚从 PTY 读到的字节推进状态机。直接接 [`mt_pty::PtySession::spawn`]
    /// 的 `on_output` 回调。
    pub fn advance(&self, bytes: &[u8]) {
        let mut term = self.term.lock();
        self.parser.lock().advance(&mut *term, bytes);
    }

    pub fn resize(&self, size: TermSize) {
        self.term.lock().resize(size);
    }

    /// 供渲染侧读取 grid。持锁期间 reader 线程会被挡住 —— 这正是我们要的背压。
    pub fn term(&self) -> &Arc<Mutex<Term<EventQueue>>> {
        &self.term
    }

    pub fn events(&self) -> &EventQueue {
        &self.events
    }
}
