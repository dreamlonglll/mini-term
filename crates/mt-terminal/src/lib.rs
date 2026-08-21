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
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 把 alacritty 整个重新导出。渲染层(`mt-ui`)要用 `Cell` / `Flags` / `Color` /
/// `TermMode` / `Selection` 这些类型,统一从这里取,避免各 crate 各自写一份
/// `alacritty_terminal` 依赖后版本漂移导致类型不互通。
pub use alacritty_terminal;

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
    /// 当前的回滚行数。**自己记一份**:`Term` 的 `config` 字段是私有的、
    /// alacritty 也没给读回口,而 [`Self::set_scrollback`] 要靠它做「值没变就不动」
    /// 的短路(`set_options` 会把整屏标脏并发一次 title 事件)。
    scrollback: AtomicUsize,
    /// 光标绝对行的最低水位。`Some` = 追踪中,见 [`Self::arm_cursor_floor`]。
    cursor_floor: Mutex<Option<CursorFloor>>,
}

/// 追踪中的光标水位。
struct CursorFloor {
    /// 到达过的最低绝对行。
    min: i32,
    /// 还允许逐字节采样多少字节,见 [`FLOOR_SAMPLE_BUDGET`]。
    budget: usize,
}

/// 一次追踪最多逐字节采样多少字节,之后退回整批推进(水位保留已经找到的值)。
///
/// 要抓的 erase 就在 AI 应答的**头一批**数据里,几 KB 就走完了;这个上限是给
/// 「在 AI 会话里 `cat` 了个大文件」这类爆发兜底 —— 逐字节推进会让 vte 的批量
/// 快路径失效,不封顶的话 reader 线程可能被拖出一次可见的卡顿。
const FLOOR_SAMPLE_BUDGET: usize = 256 * 1024;

/// 光标当前所在的 **grid 绝对行** = `cursor.line + history_size`。
///
/// 内容每被顶进历史一行,它的 `Line` 减 1、`history_size` 加 1,两者之和守恒 ——
/// 所以这个量**可以跨滚动比较**,而裸的 `cursor.point.line` 不行。
fn cursor_row(term: &Term<EventQueue>) -> i32 {
    term.grid().cursor.point.line.0 + term.history_size() as i32
}

impl TerminalEmulator {
    /// 用 alacritty 的默认回滚行数(10000 行)建一个。
    pub fn new(size: TermSize) -> Self {
        Self::with_scrollback(size, Config::default().scrolling_history)
    }

    /// 指定回滚行数(`config.terminalScrollback`)。
    ///
    /// **必须在建终端时就喂进去**:`scrolling_history` 决定 grid 的历史容量,
    /// 默认值(10000)与配置默认值撞上纯属巧合 —— 用户把它调到 5 万,
    /// 不喂的话新终端照样只留 1 万行。
    pub fn with_scrollback(size: TermSize, scrollback: usize) -> Self {
        let events = EventQueue::default();
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        let term = Term::new(config, &size, events.clone());
        Self {
            term: Arc::new(Mutex::new(term)),
            parser: Mutex::new(Processor::new()),
            events,
            scrollback: AtomicUsize::new(scrollback),
            cursor_floor: Mutex::new(None),
        }
    }

    /// 当前的回滚行数。
    pub fn scrollback(&self) -> usize {
        self.scrollback.load(Ordering::Relaxed)
    }

    /// 热改回滚行数(设置页改动那一刻)。
    ///
    /// 走 `Term::set_options`,它内部会 `grid.update_history` —— 调小时**当场**
    /// 裁掉多余历史并释放内存(与原版 `updateAllTerminalScrollback` 同效果)。
    /// 值没变就不动:`set_options` 会把整屏标脏并发一次 title 事件。
    pub fn set_scrollback(&self, scrollback: usize) {
        if self.scrollback.swap(scrollback, Ordering::Relaxed) == scrollback {
            return;
        }
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        self.term.lock().set_options(config);
    }

    /// 把刚从 PTY 读到的字节推进状态机。直接接 [`mt_pty::PtySession::spawn`]
    /// 的 `on_output` 回调。
    ///
    /// 追踪光标水位时**改成逐字节推进**(见 [`Self::arm_cursor_floor`]):要找的
    /// 那个位置只在整批数据的**中间态**里存在,喂完再读就已经被后续输出推走了。
    pub fn advance(&self, bytes: &[u8]) {
        let mut term = self.term.lock();
        let mut parser = self.parser.lock();
        let mut floor = self.cursor_floor.lock();
        let Some(floor) = floor.as_mut().filter(|f| f.budget > 0) else {
            parser.advance(&mut *term, bytes);
            return;
        };
        let (sampled, rest) = bytes.split_at(bytes.len().min(floor.budget));
        for byte in sampled {
            parser.advance(&mut *term, std::slice::from_ref(byte));
            floor.min = floor.min.min(cursor_row(&term));
        }
        floor.budget -= sampled.len();
        if !rest.is_empty() {
            parser.advance(&mut *term, rest);
        }
    }

    /// 开始追踪光标绝对行的**最低水位**,起点是此刻的光标位置。
    ///
    /// # 这是给 AI 任务标记(⚑)定锚用的
    ///
    /// Claude Code 这类 Ink 应用走 `log-update`:每帧输出 `eraseLines(n)` +
    /// 块内容 + `\n`,所以**等待输入时光标恒定停在渲染块的下一行**,而用户键入的
    /// 文字在块里面 —— 拿按 Enter 那一刻的 `cursor.point.line` 当锚点必然偏下,
    /// 偏多少还随窗口宽度折行、提示行在不在而变(实测 1~3 行都出现过)。
    ///
    /// 但提交那一下 Ink 会先发 `eraseLines` 把光标**顶回块首**,再把
    /// `> 用户输入` 这条 static 消息打在块首 —— 于是「窗口期内光标到达过的最靠上
    /// 的位置」正好就是那条消息落地的行。这样取锚点**不含任何魔数**,Claude Code
    /// 改 UI 布局也不会失效。
    ///
    /// 不做 erase 的行式 CLI 光标只会往下走,水位停在起点 = 退化成原来的行为。
    pub fn arm_cursor_floor(&self) {
        let term = self.term.lock();
        let min = cursor_row(&term);
        *self.cursor_floor.lock() = Some(CursorFloor {
            min,
            budget: FLOOR_SAMPLE_BUDGET,
        });
    }

    /// 取走水位并停止追踪。`None` = 没在追踪(没武装过 / 已经取过一次)。
    pub fn take_cursor_floor(&self) -> Option<i32> {
        self.cursor_floor.lock().take().map(|f| f.min)
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

    /// 当前 grid 尺寸。渲染层每帧按可用像素算出目标尺寸,与这里比对后才决定
    /// 要不要 `resize` —— 免得每帧都去动 grid。
    pub fn term_size(&self) -> TermSize {
        let term = self.term.lock();
        TermSize::new(term.columns(), term.screen_lines())
    }

    /// 当前 VT 模式位(APP_CURSOR / BRACKETED_PASTE / ALT_SCREEN ...)。
    /// 键盘编码与粘贴编码都要看它。
    pub fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    /// 借出 grid 只读地做一件事。比裸拿 `term()` 少一次「忘了尽早放锁」的机会。
    pub fn with_term<R>(&self, f: impl FnOnce(&Term<EventQueue>) -> R) -> R {
        f(&self.term.lock())
    }

    /// 借出 grid 做一次可变操作(滚动回看、选择区、清屏……)。
    pub fn with_term_mut<R>(&self, f: impl FnOnce(&mut Term<EventQueue>) -> R) -> R {
        f(&mut self.term.lock())
    }

    /// 历史区里存着的行数(不含可视区)。回滚行数的验收口。
    pub fn history_lines(&self) -> usize {
        let term = self.term.lock();
        term.grid().total_lines().saturating_sub(term.grid().screen_lines())
    }

    /// 可视区逐行文本(行尾空格已裁掉)。
    ///
    /// 这是**给测试与诊断用的**读回口:PTY 里跑一条 `echo`,从这里断言回显成立,
    /// 整条链路不需要起 GPUI 窗口就能验。宽字符只在它自己的列上出现一次,
    /// WIDE_CHAR_SPACER 那一列直接跳过 —— 所以返回的字符串里
    /// 「字符数」不等于「列数」,列位置要用 [`Self::visible_columns`] 取。
    pub fn visible_lines(&self) -> Vec<String> {
        self.visible_columns()
            .into_iter()
            .map(|row| {
                let mut s: String = row.into_iter().map(|(_, c)| c).collect();
                while s.ends_with(' ') {
                    s.pop();
                }
                s
            })
            .collect()
    }

    /// **任意** grid 绝对行的内容指纹(行尾空格已裁)。`None` = 该行不在缓冲区里。
    ///
    /// 参数是 [`crate`] 里到处用的那个**绝对行**(`line + history_size` 守恒的那个),
    /// 不是 `Line`:调用方拿着的锚点就是绝对行,自己换算一次容易错。合法区间是
    /// `[0, history + screen_lines)` —— 越界返回 `None`,那正是「锚点已经不指向
    /// 任何东西」的信号。
    ///
    /// # 这是给 AI 任务标记(⚑)校验锚点用的
    ///
    /// 算术锚点只保证「内容被顶进 scrollback 时行号跟着走」,**保证不了内容还在**:
    /// Claude Code 的 `/new` 清屏是从屏幕顶部逐行 `ESC[2K` **原地擦**,不产生滚动,
    /// `history_size` 一动不动 —— 锚点算得出行号,那一行却已经被抹白、随后被新会话
    /// 的输出覆盖。列宽变化触发的 reflow 同理。指纹是唯一不依赖「是谁、发了什么
    /// 序列」的判据,详见 mt-app 的 `markers` 模块注释。
    ///
    /// 口径与 [`Self::visible_lines`] 一致(跳过宽字符 spacer、裁行尾空格),
    /// 这样定锚与校验两侧不会因为取法不同而假性失配。
    pub fn line_text(&self, abs_line: i32) -> Option<String> {
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::cell::Flags;

        let term = self.term.lock();
        let history = term.history_size() as i32;
        let line = abs_line - history;
        if line < -history || line >= term.screen_lines() as i32 {
            return None;
        }
        let row = &term.grid()[Line(line)];
        let mut s = String::new();
        for col in 0..term.columns() {
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            s.push(cell.c);
        }
        while s.ends_with(' ') {
            s.pop();
        }
        Some(s)
    }

    /// 可视区逐行的 `(列号, 字符)`。宽字符只出现一次,列号是它**起始**的那一列;
    /// 它占掉的第二列(WIDE_CHAR_SPACER)不出现。
    ///
    /// 中英混排的对齐断言就靠这个:`你好abc` 里 `a` 必须落在第 4 列而不是第 2 列。
    pub fn visible_columns(&self) -> Vec<Vec<(usize, char)>> {
        use alacritty_terminal::term::cell::Flags;

        let term = self.term.lock();
        let mut rows: Vec<Vec<(usize, char)>> = vec![Vec::new(); term.screen_lines()];
        let display_offset = term.grid().display_offset() as i32;
        for indexed in term.grid().display_iter() {
            if indexed
                .cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let row = (indexed.point.line.0 + display_offset) as usize;
            if let Some(cells) = rows.get_mut(row) {
                cells.push((indexed.point.column.0, indexed.cell.c));
            }
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 往终端里推 `n` 行文本。
    fn feed_lines(e: &TerminalEmulator, n: usize) {
        for i in 0..n {
            e.advance(format!("line{i}\r\n").as_bytes());
        }
    }

    /// 回滚行数**建终端时就要生效**:默认值(10000)与配置默认值撞上纯属巧合,
    /// 不喂进 `term::Config` 的话用户调高/调低都没效果。
    #[test]
    fn 回滚行数在建终端时生效() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(20, 4), 2);
        assert_eq!(e.scrollback(), 2);
        feed_lines(&e, 20);
        // 历史区被 scrolling_history 封顶
        assert_eq!(e.history_lines(), 2);
    }

    /// 热改回滚行数:**调小时当场裁掉多余历史**(设置页那一刻就释放内存,
    /// 与原版 `updateAllTerminalScrollback` 同效果)。
    #[test]
    fn 热改回滚行数当场裁历史() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(20, 4), 50);
        feed_lines(&e, 30);
        let before = e.history_lines();
        assert!(before > 3, "先攒出一段历史,实际 {before}");

        e.set_scrollback(3);
        assert_eq!(e.scrollback(), 3);
        assert_eq!(e.history_lines(), 3, "调小后历史必须当场被裁到新上限");

        // 调大不会凭空长出历史,但上限跟着变
        e.set_scrollback(40);
        assert_eq!(e.scrollback(), 40);
        assert_eq!(e.history_lines(), 3);
    }

    /// 没武装时什么都不追踪 —— `advance` 走整批快路径。
    #[test]
    fn 未武装时取不到光标水位() {
        let e = TerminalEmulator::new(TermSize::new(40, 10));
        feed_lines(&e, 3);
        assert_eq!(e.take_cursor_floor(), None);
    }

    /// 光标只往下走时,水位停在武装那一刻的位置(不做 erase 的行式 CLI 就是这样,
    /// 等价于原来「按 Enter 当场取光标行」的行为)。
    #[test]
    fn 光标只下行时水位停在起点() {
        let e = TerminalEmulator::new(TermSize::new(40, 10));
        e.advance(b"a\r\nb\r\nc"); // 光标落在第 2 行
        e.arm_cursor_floor();
        e.advance(b"\r\nd\r\ne");
        assert_eq!(e.take_cursor_floor(), Some(2));
        assert_eq!(e.take_cursor_floor(), None, "取走即停止追踪");
    }

    /// 核心用例:Ink 的 `eraseLines` 把光标顶回块首那一瞬只存在于**整批数据的
    /// 中间态**里 —— 逐字节采样才抓得住。这里模拟提交一条消息的完整重绘。
    #[test]
    fn 水位抓得住整批数据里的中间态() {
        let e = TerminalEmulator::new(TermSize::new(40, 10));
        // 第 0~2 行是「渲染块」,光标停在块下方的第 3 行(log-update 尾部那个 \n)
        e.advance(b"box-top\r\n> hi\r\nbox-bottom\r\n");
        e.arm_cursor_floor();

        // 提交:一整批里 erase 顶回块首(第 0 行)、打 static、再画新块。
        // log-update 的 `previousLineCount` 数的是 `块内容 + '\n'` 切出来的段数,
        // 所以 3 行的块要擦 4 段 = 上移 3 次,正好从第 3 行回到第 0 行。
        let mut batch = Vec::new();
        for i in 0..4 {
            batch.extend_from_slice(b"\x1b[2K");
            if i < 3 {
                batch.extend_from_slice(b"\x1b[1A");
            }
        }
        batch.extend_from_slice(b"\r> hi\r\n\r\nthinking...\r\n");
        e.advance(&batch);

        assert_eq!(
            e.take_cursor_floor(),
            Some(0),
            "锚点必须落在 static 消息所在的块首行,而不是武装时的第 3 行"
        );
    }

    /// **整个指纹方案的支点**:定锚定出来的那一行,`line_text` 读回来必须正好是
    /// `> 用户输入` 那条 static 消息 —— 不是它上面的空行、也不是它下面被 log-update
    /// 反复重绘的动态区。落到动态区的话指纹每帧都变,标记会被校验全部剪光。
    #[test]
    fn 定锚那一行读回的是用户提交那条消息() {
        let e = TerminalEmulator::new(TermSize::new(40, 10));
        e.advance(b"box-top\r\n> hi\r\nbox-bottom\r\n");
        e.arm_cursor_floor();

        let mut batch = Vec::new();
        for i in 0..4 {
            batch.extend_from_slice(b"\x1b[2K");
            if i < 3 {
                batch.extend_from_slice(b"\x1b[1A");
            }
        }
        batch.extend_from_slice(b"\r> hi\r\n\r\nthinking...\r\n");
        e.advance(&batch);

        let anchor = e.take_cursor_floor().expect("水位必须落地");
        assert_eq!(
            e.line_text(anchor).as_deref(),
            Some("> hi"),
            "指纹取的是这一行 —— 取空行或动态区都会让校验失灵"
        );

        // 动态区继续重绘,static 那一行必须岿然不动(指纹稳定的前提)
        e.advance(b"\x1b[1A\x1b[2K\rdone.\r\n");
        assert_eq!(e.line_text(anchor).as_deref(), Some("> hi"));
    }

    /// 追踪期走的是逐字节推进,**多字节字符不能被切坏** —— 中文提交后 AI 正在
    /// 吐中文正文,那 200ms 的输出全程都在这条路上。
    #[test]
    fn 逐字节推进不切坏多字节字符() {
        let e = TerminalEmulator::new(TermSize::new(40, 6));
        e.arm_cursor_floor();
        e.advance("中文字符 · émoji 🎉".as_bytes());
        assert_eq!(e.visible_lines()[0], "中文字符 · émoji 🎉");
        e.take_cursor_floor();
    }

    /// 预算用尽后退回整批推进,但**已经找到的水位不丢** —— 爆发输出只让采样停掉,
    /// 不该把定好的锚点冲掉。
    #[test]
    fn 采样预算用尽后水位保留() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(40, 6), 10_000);
        e.advance(b"a\r\nb\r\nc");
        e.arm_cursor_floor();
        e.advance(b"\x1b[1A"); // 水位落到第 1 行
        // 灌爆预算,期间光标一路下行
        let flood = vec![b'x'; FLOOR_SAMPLE_BUDGET + 4096];
        e.advance(&flood);
        assert_eq!(e.take_cursor_floor(), Some(1));
    }

    /// 水位是**绝对行**,顶进历史后照样可比 —— erase 之后又滚出去若干行时不能错。
    #[test]
    fn 水位跨滚动仍然可比() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(40, 4), 100);
        feed_lines(&e, 4); // history 1,光标在第 3 行 → 绝对行 4
        e.arm_cursor_floor();
        // 先顶回一行(绝对行 3),再吐够把它挤进历史的量
        e.advance(b"\x1b[1A");
        feed_lines(&e, 20);
        assert_eq!(e.take_cursor_floor(), Some(3));
    }

    /// 绝对行读回口:视口、scrollback 都读得到,越界给 `None`。
    #[test]
    fn 按绝对行读回文本() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(40, 10), 10_000);
        feed_lines(&e, 30); // history 21,视口是 line21..line29 + 光标那一空行
        assert_eq!(e.line_text(0).as_deref(), Some("line0"), "scrollback 最顶上那行");
        assert_eq!(e.line_text(24).as_deref(), Some("line24"), "视口里的行");
        assert_eq!(e.line_text(30).as_deref(), Some(""), "光标停的空行");
        assert_eq!(e.line_text(31), None, "越过视口底部");
        assert_eq!(e.line_text(-1), None, "越过缓冲区顶端");
    }

    /// 回归:**Claude Code 的清屏不产生滚动** —— 锚点算术照样成立,内容却没了。
    ///
    /// 这是「AI 任务标记跳到不相干的行」那个 bug 的机制,判据只能是内容指纹
    /// (见 mt-app 的 `markers` 模块注释)。对照组 `ESC[2J` 走 `clear_viewport`
    /// → `scroll_up`,内容进 scrollback、锚点完好 —— 两条路必须区分得开。
    #[test]
    fn 就地清屏抹掉锚点行而历史不动() {
        // Claude Code 2.1.x: ESC[H + (ESC[2K + ESC[1B) x viewportRows + ESC[H
        let mut 清屏 = b"\x1b[H".to_vec();
        for _ in 0..10 {
            清屏.extend_from_slice(b"\x1b[2K\x1b[1B");
        }
        清屏.extend_from_slice(b"\x1b[H");

        let e = TerminalEmulator::with_scrollback(TermSize::new(40, 10), 10_000);
        feed_lines(&e, 30);
        let h0 = e.history_lines();
        assert_eq!(e.line_text(24).as_deref(), Some("line24"));

        e.advance(&清屏);
        assert_eq!(e.history_lines(), h0, "逐行 2K 不产生滚动,history 必须纹丝不动");
        assert_eq!(
            e.line_text(24).as_deref(),
            Some(""),
            "算术还指得到这一行,内容却被抹白了 —— 只有指纹测得到"
        );
        assert_eq!(e.line_text(0).as_deref(), Some("line0"), "scrollback 不受影响");

        // 对照组:ESC[2J 是滚上去,不是抹掉
        let e2 = TerminalEmulator::with_scrollback(TermSize::new(40, 10), 10_000);
        feed_lines(&e2, 30);
        e2.advance(b"\x1b[2J\x1b[0f");
        assert!(e2.history_lines() > h0, "2J 把整屏顶进了 scrollback");
        assert_eq!(e2.line_text(24).as_deref(), Some("line24"), "内容跟着锚点走,完好");
    }

    /// `ESC[3J` / RIS 把历史清零 → 锚点整体越界,读回口如实交回 `None`。
    #[test]
    fn 清历史后锚点越界() {
        for seq in [&b"\x1b[3J"[..], &b"\x1bc"[..]] {
            let e = TerminalEmulator::with_scrollback(TermSize::new(40, 10), 10_000);
            feed_lines(&e, 30);
            assert_eq!(e.line_text(24).as_deref(), Some("line24"));
            e.advance(seq);
            assert_eq!(e.history_lines(), 0, "历史被清空");
            assert_eq!(e.line_text(24), None, "锚点越界,判废");
        }
    }

    /// 值没变就不动 —— `set_options` 会把整屏标脏并发一次 title 事件。
    #[test]
    fn 回滚行数没变时不重设() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(20, 4), 10);
        let _ = e.events().drain();
        e.set_scrollback(10);
        assert!(e.events().drain().is_empty(), "同值 set 不该发事件");
        e.set_scrollback(11);
        assert!(!e.events().drain().is_empty(), "值变了才走 set_options");
    }
}
