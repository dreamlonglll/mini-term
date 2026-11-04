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
//! 5. **图片协议**:alacritty_terminal **不支持**。Kitty 图形协议的静态 PNG 子集在
//!    [`kitty`] 模块,默认关闭(宿主经 [`TerminalEmulator::with_kitty_graphics`] 打开);
//!    Sixel 与 iTerm2 没有实现。
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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub mod kitty;
pub mod links;
pub mod width;

use width::{Scratch, WidthOverride};

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
    parser: Mutex<Parser>,
    events: EventQueue,
    /// 当前的回滚行数。**自己记一份**:`Term` 的 `config` 字段是私有的、
    /// alacritty 也没给读回口,而 [`Self::set_scrollback`] 要靠它做「值没变就不动」
    /// 的短路(`set_options` 会把整屏标脏并发一次 title 事件)。
    scrollback: AtomicUsize,
    /// 光标绝对行的最低水位。`Some` = 追踪中,见 [`Self::arm_cursor_floor`]。
    cursor_floor: Mutex<Option<CursorFloor>>,
    /// 内容代数,见 [`Self::generation`]。
    generation: AtomicU64,
    /// 光标的停稳追踪,见 [`Self::settled_cursor`]。
    cursor_settle: Mutex<CursorSettle>,
    /// Kitty 图形。`None` = 开关关着(缺省),字节不经过它。见 [`kitty`]。
    kitty: Option<kitty::Kitty>,
    /// ConPTY 的 win32-input-mode(`CSI ? 9001h/l`)是否已由应用开启。
    /// 键盘编码要据它决定发 WSI 记录还是传统字节,见 [`Self::win32_input_mode`]。
    win32_input: AtomicBool,
    /// 本次运行是否允许跟踪 WSI。`MT_DISABLE_WIN32_INPUT_MODE=1` 时整条路径
    /// 关闭,行为与加入该协议前逐字节一致。
    win32_input_allowed: bool,
}

/// 光标要在一格上停多久才算「停稳」。
///
/// 下限是「同步块写完 → 光标归位」那段空档:真 ConPTY 上实测 p50 约 50µs、
/// 最大约 400µs,这里留足负载下的余量。上限是 TUI 的重绘间隔:pi-tui 最快 16ms
/// 一帧,光标每帧都要离开输入位置再回来,超过它的话输入位置永远轮不到停稳。
const CURSOR_SETTLE: Duration = Duration::from_millis(8);

/// IME 该贴着的光标位置。行列口径与 `grid().cursor.point` 相同。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettledCursor {
    pub line: i32,
    pub column: usize,
    /// 光标最新的落点与这里不同,过一会儿读到的可能就变了 —— 要跟的人得再读一次。
    pub pending: bool,
}

/// 把 TUI 重绘途中的光标滤掉。
///
/// 自绘输入框的 TUI 每帧都让光标先走到改动内容的末尾,再回到输入位置。pi-tui 的
/// regular 模式还把这两步拆成两次 write,于是 reader 线程收到两批字节,其间光标
/// 就停在撑满整行的边框行尾。这段空档只有几十微秒,但 IME 恰好这时来取坐标,
/// 候选窗就飞到屏幕右边去了(issue #88)。
///
/// 判据是「停了多久」而不是认某个 TUI 的字节形态:途中的位置停不到
/// [`CURSOR_SETTLE`] 就不算数,真挪了位置的最多晚这么久生效。
#[derive(Debug)]
struct CursorSettle {
    /// 停稳过的最近一个位置。
    settled: (i32, usize),
    /// 最新的位置,以及它从什么时候开始待在这儿。
    latest: (i32, usize),
    latest_since: Instant,
}

impl CursorSettle {
    fn new(cell: (i32, usize), now: Instant) -> Self {
        Self {
            settled: cell,
            latest: cell,
            latest_since: now,
        }
    }

    /// 记一次光标位置。**挪走之前**先结算旧位置 —— 它可能已经停够了,而读的人
    /// 这期间一次都没来。
    fn observe(&mut self, cell: (i32, usize), now: Instant) {
        if cell == self.latest {
            return;
        }
        self.promote(now);
        self.latest = cell;
        self.latest_since = now;
    }

    fn read(&mut self, now: Instant) -> SettledCursor {
        self.promote(now);
        SettledCursor {
            line: self.settled.0,
            column: self.settled.1,
            pending: self.settled != self.latest,
        }
    }

    fn promote(&mut self, now: Instant) {
        if now.saturating_duration_since(self.latest_since) >= CURSOR_SETTLE {
            self.settled = self.latest;
        }
    }
}

/// 光标此刻在 grid 的哪一格。
fn cursor_cell(term: &Term<EventQueue>) -> (i32, usize) {
    let point = term.grid().cursor.point;
    (point.line.0, point.column.0)
}

/// VT 解析器与它的随身暂存。
struct Parser {
    vte: Processor,
    /// [`WidthOverride`] 的随身状态(拼簇文本的暂存等),跟解析器同锁、跨批次复用。
    cluster: Scratch,
}

impl Parser {
    /// 把一批字节经 [`WidthOverride`] 推进 `term`。
    fn advance(
        &mut self,
        term: &mut Term<EventQueue>,
        bytes: &[u8],
        win32_input: Option<&AtomicBool>,
    ) {
        self.vte.advance(
            &mut WidthOverride::new(term, &mut self.cluster, win32_input),
            bytes,
        );
    }

    /// 同步块的到期时刻。`None` = 没有同步块在攒。
    fn sync_deadline(&self) -> Option<Instant> {
        self.vte.sync_timeout().sync_timeout()
    }

    /// 结束同步块:把攒下的字节同样经 [`WidthOverride`] 推进 `term`。
    fn stop_sync(&mut self, term: &mut Term<EventQueue>, win32_input: Option<&AtomicBool>) {
        self.vte.stop_sync(&mut WidthOverride::new(
            term,
            &mut self.cluster,
            win32_input,
        ));
    }
}

/// [`TerminalEmulator::release_expired_sync`] 的结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncRelease {
    /// 没有同步块在攒:结束标记已经到了,或者早就释放过了。
    Idle,
    /// 还没到期(可能刚被新的 BSU 续过期),到这个时刻再来问。
    Pending(Instant),
    /// 到期了,攒下的字节已经落进 grid。
    Released,
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
        let cursor_settle = CursorSettle::new(cursor_cell(&term), Instant::now());
        Self {
            term: Arc::new(Mutex::new(term)),
            parser: Mutex::new(Parser {
                vte: Processor::new(),
                cluster: Scratch::default(),
            }),
            events,
            scrollback: AtomicUsize::new(scrollback),
            cursor_floor: Mutex::new(None),
            generation: AtomicU64::new(0),
            cursor_settle: Mutex::new(cursor_settle),
            kitty: None,
            win32_input: AtomicBool::new(false),
            win32_input_allowed: cfg!(windows)
                && !matches!(
                    std::env::var("MT_DISABLE_WIN32_INPUT_MODE").as_deref(),
                    Ok("1")
                ),
        }
    }

    /// 打开 Kitty 图形。开不开由宿主判(环境变量开关 + ConPTY 后端),`false` 原样返回。
    pub fn with_kitty_graphics(mut self, enabled: bool) -> Self {
        if enabled {
            let kitty = kitty::Kitty::new(self.events.clone());
            self.parser.get_mut().cluster.set_kitty(kitty.shared());
            self.kitty = Some(kitty);
        }
        self
    }

    /// Kitty 图形状态。`None` = 没开。
    pub fn kitty(&self) -> Option<&kitty::Kitty> {
        self.kitty.as_ref()
    }

    /// 内容代数:每推进一批字节、每次 resize、每次真改了回滚行数都 +1。
    ///
    /// **在 term 锁里改**,所以持锁([`Self::with_term`])读到的值与 grid 当时的
    /// 状态是一致的。终端查找靠它判断「上次扫描之后有没有新输出」—— 光看内容
    /// 指纹判不出「周期性输出恰好滚过整数个周期、屏幕逐字相同」这种变化,
    /// 而那时回看缓冲里的内容已经变了。
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
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
        let mut term = self.term.lock();
        term.set_options(config);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// 把刚从 PTY 读到的字节推进状态机。直接接 mt-pty 里 `PtySession::spawn`
    /// 的 `on_output` 回调。
    ///
    /// 追踪光标水位时**改成逐字节推进**(见 [`Self::arm_cursor_floor`]):要找的
    /// 那个位置只在整批数据的**中间态**里存在,喂完再读就已经被后续输出推走了。
    ///
    /// 字节不是直接喂 `Term`,中间隔着 [`WidthOverride`]:emoji 序列整簇落一格,
    /// 理由与手法见 [`width`] 模块注释。
    ///
    /// 开了 Kitty 图形时先在锁外过一遍预解析器(含 PNG 解码),见 [`kitty`]。
    pub fn advance(&self, bytes: &[u8]) {
        let mut kitty = self.kitty.as_ref().map(kitty::Kitty::pre_parser);
        let bytes = match kitty.as_mut() {
            Some(pre) => pre.filter(bytes),
            None => bytes,
        };
        // 这一批全是图片 payload:不用去抢 term 锁
        if self.kitty.is_some() && bytes.is_empty() {
            return;
        }
        let mut term = self.term.lock();
        // 持着 term 锁记账:读的人持锁看到的代数与内容必然同步
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.feed(&mut term, bytes);
        self.cursor_settle
            .lock()
            .observe(cursor_cell(&term), Instant::now());
    }

    fn feed(&self, term: &mut Term<EventQueue>, bytes: &[u8]) {
        let win32_input = self.win32_input_allowed.then_some(&self.win32_input);
        let mut parser = self.parser.lock();
        let mut floor = self.cursor_floor.lock();
        let Some(floor) = floor.as_mut().filter(|f| f.budget > 0) else {
            parser.advance(term, bytes, win32_input);
            return;
        };
        let (sampled, rest) = bytes.split_at(bytes.len().min(floor.budget));
        for byte in sampled {
            parser.advance(term, std::slice::from_ref(byte), win32_input);
            floor.min = floor.min.min(cursor_row(term));
        }
        floor.budget -= sampled.len();
        if !rest.is_empty() {
            parser.advance(term, rest, win32_input);
        }
    }

    /// 同步块(`ESC[?2026h`)的到期时刻。`None` = 没有同步块在攒。
    ///
    /// 只拿解析器这一把锁:持着它不会再去等 term,与 term→parser 的锁序不冲突。
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.lock().sync_deadline()
    }

    /// 同步块到期了就把攒下的字节放出来。宿主的定时器到点时调。
    ///
    /// vte 进了同步块之后字节只进它自己的缓冲,等结束标记、缓冲满 2 MiB 或宿主调
    /// `stop_sync`。deadline(150ms)只是个记录:**到期不会自解**,新来的字节也解不开。
    /// alacritty 自己的事件循环是按 deadline 等 poll 超时,这里的 reader 线程阻塞在 read
    /// 上等不了,所以交给宿主排定时器(mt-app 的 `TerminalPane::arm_sync_release`)。
    ///
    /// deadline **在这里重新读**:同步块进行中再来一个 BSU 会续期,旧定时器到点时拿到
    /// [`SyncRelease::Pending`] 接着排,不能提前放行。
    ///
    /// 两把锁与 [`Self::advance`] 同序(term→parser),释放与后续输出不会交错。
    pub fn release_expired_sync(&self) -> SyncRelease {
        self.release_expired_sync_at(Instant::now())
    }

    /// [`Self::release_expired_sync`] 的可注入时钟版本,单测用。
    fn release_expired_sync_at(&self, now: Instant) -> SyncRelease {
        let win32_input = self.win32_input_allowed.then_some(&self.win32_input);
        let mut term = self.term.lock();
        let mut parser = self.parser.lock();
        let Some(deadline) = parser.sync_deadline() else {
            return SyncRelease::Idle;
        };
        if now < deadline {
            return SyncRelease::Pending(deadline);
        }
        // 持着 term 锁记账,口径同 `advance`
        self.generation.fetch_add(1, Ordering::Relaxed);
        parser.stop_sync(&mut term, win32_input);
        // 块里的中间态已经看不到了(结束标记那条路也一样),水位只能记下放完之后的光标
        if let Some(floor) = self.cursor_floor.lock().as_mut().filter(|f| f.budget > 0) {
            floor.min = floor.min.min(cursor_row(&term));
        }
        self.cursor_settle
            .lock()
            .observe(cursor_cell(&term), Instant::now());
        SyncRelease::Released
    }

    /// IME 该贴着的光标位置:滤掉了 TUI 重绘途中的落点,见 [`CursorSettle`]。
    ///
    /// **现读,不走帧**:IME 取坐标的时机由系统决定,不会等下一帧。
    pub fn settled_cursor(&self) -> SettledCursor {
        self.cursor_settle.lock().read(Instant::now())
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
        let mut term = self.term.lock();
        term.resize(size);
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.cursor_settle
            .lock()
            .observe(cursor_cell(&term), Instant::now());
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

    /// Windows 本地 ConPTY 的 win32-input-mode 是否开启:应用发过
    /// `CSI ? 9001h`,之后没有 `CSI ? 9001l`,且本次运行没被
    /// `MT_DISABLE_WIN32_INPUT_MODE=1` 关掉。非 Windows 恒为 false。
    ///
    /// 键盘编码据它决定是否把 `Shift+Enter` / `Ctrl+J` 发成 WSI 记录;其余按键
    /// 始终走 legacy 字节。
    pub fn win32_input_mode(&self) -> bool {
        self.win32_input_allowed && self.win32_input.load(Ordering::Relaxed)
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

    const INPUT: (i32, usize) = (5, 2);
    const BORDER_END: (i32, usize) = (4, 149);

    fn settled_at(settle: &mut CursorSettle, now: Instant) -> ((i32, usize), bool) {
        let cursor = settle.read(now);
        ((cursor.line, cursor.column), cursor.pending)
    }

    /// **issue #88 的回归**:pi-tui 写完边框后光标在行尾停几十微秒才归位。
    /// IME 恰好在这段空档里取坐标,也必须拿到输入位置。
    #[test]
    fn 重绘途中读到的还是停稳的位置() {
        let t0 = Instant::now();
        let mut settle = CursorSettle::new(INPUT, t0);
        let render = t0 + Duration::from_millis(16);
        settle.observe(BORDER_END, render);
        let in_gap = render + Duration::from_micros(20);
        assert_eq!(settled_at(&mut settle, in_gap), (INPUT, true));
        settle.observe(INPUT, render + Duration::from_micros(50));
        assert_eq!(
            settled_at(&mut settle, in_gap + Duration::from_micros(40)),
            (INPUT, false)
        );
    }

    /// TUI 按最快节奏持续重绘:光标每帧都跑一趟,读到的永远是输入位置。
    #[test]
    fn 持续重绘时输入位置不被途中落点顶掉() {
        let t0 = Instant::now();
        let mut settle = CursorSettle::new(INPUT, t0);
        for frame in 1..=120u32 {
            let render = t0 + Duration::from_millis(16) * frame;
            settle.observe(BORDER_END, render);
            assert_eq!(settled_at(&mut settle, render).0, INPUT);
            settle.observe(INPUT, render + Duration::from_micros(50));
            let after = render + Duration::from_millis(10);
            assert_eq!(settled_at(&mut settle, after), (INPUT, false));
        }
    }

    /// 真挪了位置(输入框被流式正文往下推)最多晚一个停稳窗口生效。
    #[test]
    fn 真挪了位置停够之后生效() {
        let t0 = Instant::now();
        let mut settle = CursorSettle::new(INPUT, t0);
        let moved = (6, 2);
        settle.observe(moved, t0);
        assert_eq!(
            settled_at(&mut settle, t0 + Duration::from_millis(1)),
            (INPUT, true)
        );
        assert_eq!(settled_at(&mut settle, t0 + CURSOR_SETTLE), (moved, false));
    }

    /// 没人来读的时候光标停够又挪走了:旧位置也要结算,不能退回更早的那个。
    #[test]
    fn 挪走之前先结算旧位置() {
        let t0 = Instant::now();
        let mut settle = CursorSettle::new(INPUT, t0);
        let moved = (6, 2);
        settle.observe(moved, t0);
        let render = t0 + Duration::from_millis(16);
        settle.observe(BORDER_END, render);
        assert_eq!(settled_at(&mut settle, render), (moved, true));
    }

    /// 光标一直在动就一直不算停稳,结论留在上一个停稳的位置,并如实报 pending。
    #[test]
    fn 光标一直在动时不产生新结论() {
        let t0 = Instant::now();
        let mut settle = CursorSettle::new(INPUT, t0);
        for step in 0..100u32 {
            let now = t0 + Duration::from_millis(1) * step;
            settle.observe((0, step as usize + 10), now);
            assert_eq!(settled_at(&mut settle, now), (INPUT, true));
        }
    }

    /// 接线:`advance` 与 `resize` 都要报告光标。只断言「等够之后」这一头 ——
    /// 另一头受机器负载影响,交给上面注入时钟的用例。
    #[test]
    fn 字节与_resize_挪动的光标都能读到() {
        let e = TerminalEmulator::new(TermSize::new(40, 10));
        let wait = || std::thread::sleep(CURSOR_SETTLE * 3);

        e.advance(b"abc");
        wait();
        let cursor = e.settled_cursor();
        assert_eq!((cursor.line, cursor.column, cursor.pending), (0, 3, false));

        e.advance(b"\x1b[10;1H");
        wait();
        assert_eq!(e.settled_cursor().line, 9);

        // 行数变少,光标被挤回视口内
        e.resize(TermSize::new(40, 5));
        wait();
        let cursor = e.settled_cursor();
        assert_eq!(cursor.line, e.with_term(|t| t.grid().cursor.point.line.0));
        assert!(cursor.line < 5);
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

    /// 上一条的**对照组:AI 正忙时提交**。
    ///
    /// 那一句被排进队列,根本没打成 static 消息 —— 这 200ms 里的 erase 只是动态块
    /// 自己在重绘。水位照样落地,但落到的是**块首这一行动态内容**,读回来不是用户
    /// 正文,而且下一帧就变。这正是「AI 忙的时候追加的那句,标记下拉里根本没有」
    /// 的物理成因:拿它的指纹当锚点,下一次校验必然失配、条目被剪光。
    ///
    /// mt-app 的 `markers::settle_anchor` 据此判「挂起」而不是硬定一个锚。
    #[test]
    fn ai忙时水位落在动态区而不是用户正文() {
        let e = TerminalEmulator::new(TermSize::new(40, 10));
        // line0 是已经落地的 static,line1..3 是还在重绘的动态块,光标停在块下方
        e.advance(b"> previous question\r\nbox-top\r\nThinking...\r\nbox-bottom\r\n");
        e.arm_cursor_floor();

        // 用户追加一句:agent 只是把动态块重绘一遍(队列计数进去了),没有 static 落地
        let mut batch = Vec::new();
        for i in 0..4 {
            batch.extend_from_slice(b"\x1b[2K");
            if i < 3 {
                batch.extend_from_slice(b"\x1b[1A");
            }
        }
        batch.extend_from_slice(b"\rbox-top\r\nThinking... (1 queued)\r\nbox-bottom\r\n");
        e.advance(&batch);

        let anchor = e.take_cursor_floor().expect("水位照样落地 —— 问题不在这");
        assert_eq!(
            e.line_text(anchor).as_deref(),
            Some("box-top"),
            "落的是动态块块首,不是用户正文 —— 指纹方案的支点在这里塌了"
        );

        // 下一帧重绘,同一行的内容说变就变
        e.advance(b"\x1b[3A\x1b[2K\rbox-top-v2\r\n");
        assert_eq!(e.line_text(anchor).as_deref(), Some("box-top-v2"));
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

    /// 内容代数:推进字节 / resize / 真改回滚行数都要 +1;只读访问与同值
    /// set_scrollback 不动它(查找引擎拿它判「有没有新输出」)。
    #[test]
    fn 内容代数随改动推进() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(20, 4), 10);
        let g0 = e.generation();
        e.advance(b"x");
        let g1 = e.generation();
        assert!(g1 > g0, "推进字节必须 +1");
        let _ = e.visible_lines();
        e.with_term(|_| ());
        assert_eq!(e.generation(), g1, "只读访问不动代数");
        e.resize(TermSize::new(30, 4));
        let g2 = e.generation();
        assert!(g2 > g1, "resize 会 reflow,必须 +1");
        e.set_scrollback(10);
        assert_eq!(e.generation(), g2, "同值不重设,代数不动");
        e.set_scrollback(3);
        assert!(e.generation() > g2, "调小回滚会裁历史,必须 +1");
    }

    // ── 同步输出(DEC 2026)的超时释放 ──────────────────────────────────────

    const BSU: &[u8] = b"\x1b[?2026h";
    const ESU: &[u8] = b"\x1b[?2026l";

    /// 远远越过任何 deadline 的时刻:注入时钟,测试不必真睡 150ms。
    fn far_future() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// 定时器在所有 deadline 之后醒来。
    fn force_release(e: &TerminalEmulator) -> SyncRelease {
        e.release_expired_sync_at(far_future())
    }

    /// 钉住宿主为什么要接这一套:deadline 只是个记录,到期后 vte 照旧往缓冲里攒,
    /// 新来的字节解不开它。哪天上游自己处理了,这条会红,到时候再看宿主这边要不要留。
    #[test]
    fn vte_到期也不会自己释放() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        e.advance(BSU);
        e.advance(b"frozen");
        let deadline = e.sync_deadline().expect("BSU 之后必须有 deadline");
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()) * 2);
        e.advance(b" more");
        e.resize(TermSize::new(50, 6));
        assert_eq!(e.visible_lines()[0], "", "到期 + 新输出 + resize 都解不开");
    }

    /// 核心用例:TUI 崩了、ssh 断了,结束标记永远不来。到期释放后攒的内容上屏,
    /// 之后的输出也不再进缓冲。
    #[test]
    fn 漏发结束标记时到期释放() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        e.advance(BSU);
        e.advance(b"frozen");
        assert_eq!(e.visible_lines()[0], "", "同步块里的字节不上屏");
        assert!(e.sync_deadline().is_some());

        assert_eq!(force_release(&e), SyncRelease::Released);
        assert_eq!(e.visible_lines()[0], "frozen");
        assert_eq!(e.sync_deadline(), None);

        e.advance(b" after");
        assert_eq!(e.visible_lines()[0], "frozen after", "释放后直接上屏");
        assert_eq!(e.sync_deadline(), None);
    }

    /// 不注入时钟走一遍:真等到 deadline,用的是宿主调的那个入口。
    #[test]
    fn 真实时钟到点释放() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        e.advance(BSU);
        e.advance(b"late");
        let deadline = e.sync_deadline().expect("BSU 之后必须有 deadline");
        std::thread::sleep(
            deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1),
        );
        assert_eq!(e.release_expired_sync(), SyncRelease::Released);
        assert_eq!(e.visible_lines()[0], "late");
    }

    /// 释放必须走 [`WidthOverride`]。绕开的话 👍🏽 按字符算 4 列,后面的 x 落到第 4 列;
    /// 单码点仍是 1 列(`mt_terminal::width` 的铁律)。
    #[test]
    fn 释放走列宽封装() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        e.advance(BSU);
        e.advance("👍🏽x\r\n●y".as_bytes());
        assert_eq!(force_release(&e), SyncRelease::Released);
        let rows = e.visible_columns();
        assert_eq!(rows[0][..2], [(0, '👍'), (2, 'x')]);
        assert_eq!(rows[1][..2], [(0, '●'), (1, 'y')]);
    }

    /// 正常的 BSU→ESU 不受影响:定时器在两批之间醒来也不能拆帧。
    #[test]
    fn 正常结束不被拆帧() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        e.advance(BSU);
        e.advance(b"frame-");
        let deadline = e.sync_deadline().expect("BSU 之后必须有 deadline");
        assert_eq!(
            e.release_expired_sync_at(deadline - Duration::from_millis(1)),
            SyncRelease::Pending(deadline)
        );
        assert_eq!(e.visible_lines()[0], "", "没到期不许动");

        e.advance(b"done");
        e.advance(ESU);
        assert_eq!(e.visible_lines()[0], "frame-done", "整帧一起上屏");
        assert_eq!(e.sync_deadline(), None);
        assert_eq!(
            force_release(&e),
            SyncRelease::Idle,
            "结束标记到过了,定时器醒来什么都不做"
        );
    }

    /// 没有同步块时问一次不留痕迹:`stop_sync` 会无条件走一遍 `unset_private_mode`,
    /// 不能让它在正常输出之间空跑。
    #[test]
    fn 没有同步块时不做事也不动代数() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        e.advance(b"plain");
        let generation = e.generation();
        assert_eq!(force_release(&e), SyncRelease::Idle);
        assert_eq!(e.generation(), generation);
        assert_eq!(e.visible_lines()[0], "plain");
    }

    /// 同步块进行中又来一个 BSU = 续期。旧定时器到点只能拿到新 deadline 去续排,
    /// 不能按旧 deadline 把这一帧提前放出来。
    #[test]
    fn 续期后旧定时器不提前释放() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        e.advance(BSU);
        e.advance(b"a");
        let old = e.sync_deadline().expect("BSU 之后必须有 deadline");
        // 留出时钟分辨率,新 deadline 才严格晚于旧的
        std::thread::sleep(Duration::from_millis(5));
        e.advance(BSU);
        e.advance(b"b");
        let new = e.sync_deadline().expect("续期后还在同步块里");
        assert!(new > old, "BSU 必须续期");

        assert_eq!(e.release_expired_sync_at(old), SyncRelease::Pending(new));
        assert_eq!(e.visible_lines()[0], "", "旧 deadline 到点不许放行");

        assert_eq!(e.release_expired_sync_at(new), SyncRelease::Released);
        assert_eq!(e.visible_lines()[0], "ab");
    }

    /// 一批里上一帧结束、下一帧开头:上一帧当场上屏,下一帧照样受超时保护。
    #[test]
    fn 结束标记后紧跟新同步块时后者照常到期() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        let mut batch = BSU.to_vec();
        batch.extend_from_slice(b"one ");
        batch.extend_from_slice(ESU);
        batch.extend_from_slice(BSU);
        batch.extend_from_slice(b"two");
        e.advance(&batch);
        assert_eq!(e.visible_lines()[0], "one", "上一帧已经结束");
        assert!(e.sync_deadline().is_some(), "下一帧还在攒");
        assert_eq!(force_release(&e), SyncRelease::Released);
        assert_eq!(e.visible_lines()[0], "one two");
    }

    /// 释放时切在多字节字符 / 转义序列半中间:解析器状态要接得上,不丢不错位。
    #[test]
    fn 释放切在字符与序列半中间也接得上() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        let wide = "中".as_bytes();
        e.advance(BSU);
        e.advance(&wide[..1]);
        assert_eq!(force_release(&e), SyncRelease::Released);
        e.advance(&wide[1..]);
        assert_eq!(e.visible_lines()[0], "中", "UTF-8 切半");

        e.advance(BSU);
        e.advance(b"\x1b[3;");
        assert_eq!(force_release(&e), SyncRelease::Released);
        e.advance(b"5Hx");
        assert!(e.visible_columns()[2].contains(&(4, 'x')), "CUP 切半");
    }

    /// 释放是内容变化,代数要推进(查找引擎靠它判「有没有新输出」);攒在块里的
    /// 查询这时才跑,应答要进事件队列。
    #[test]
    fn 释放推进代数并吐出块内应答() {
        let is_reply = |event: &Event| matches!(event, Event::PtyWrite(_));
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        e.advance(BSU);
        e.advance(b"\x1b[6n");
        assert!(!e.events().drain().iter().any(is_reply), "块里的查询还没跑");

        let generation = e.generation();
        let deadline = e.sync_deadline().expect("BSU 之后必须有 deadline");
        e.release_expired_sync_at(deadline - Duration::from_millis(1));
        assert_eq!(e.generation(), generation, "没释放不动代数");

        assert_eq!(force_release(&e), SyncRelease::Released);
        assert!(e.generation() > generation, "释放了必须 +1");
        assert!(e.events().drain().iter().any(is_reply), "应答要交给宿主");
    }

    /// 光标是块里的字节挪的:IME 的停稳追踪与 ⚑ 的水位都要记上释放这一下。
    #[test]
    fn 释放挪动的光标记得上() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        e.advance(BSU);
        e.advance(b"abc");
        assert_eq!(force_release(&e), SyncRelease::Released);
        std::thread::sleep(CURSOR_SETTLE * 3);
        let cursor = e.settled_cursor();
        assert_eq!((cursor.line, cursor.column, cursor.pending), (0, 3, false));

        let e = TerminalEmulator::new(TermSize::new(40, 5));
        e.advance(b"a\r\nb\r\nc");
        e.arm_cursor_floor();
        e.advance(BSU);
        e.advance(b"\x1b[H");
        assert_eq!(force_release(&e), SyncRelease::Released);
        assert_eq!(e.take_cursor_floor(), Some(0), "释放后光标顶回首行");
    }

    /// reader 线程一边喂、定时器一边释放:逐字节喂(切开 BSU、多字节字符、换行),
    /// 每行都要不多不少地落进 grid。靠的是两边同走 term→parser 两把锁。
    #[test]
    fn 释放与后续输出并发不丢不重() {
        use std::sync::atomic::AtomicBool;

        const LINES: usize = 1500;
        let e = Arc::new(TerminalEmulator::with_scrollback(
            TermSize::new(40, 5),
            LINES * 2,
        ));
        let done = Arc::new(AtomicBool::new(false));
        let releaser = {
            let (e, done) = (e.clone(), done.clone());
            std::thread::spawn(move || {
                let future = far_future();
                let mut released = 0usize;
                while !done.load(Ordering::Relaxed) {
                    if e.release_expired_sync_at(future) == SyncRelease::Released {
                        released += 1;
                    }
                }
                released
            })
        };
        // 等释放线程真跑起来:第一个同步块被它放行之后才开始喂
        e.advance(BSU);
        let give_up = Instant::now() + Duration::from_secs(30);
        while e.sync_deadline().is_some() {
            assert!(Instant::now() < give_up, "同步块一直没被放行");
            std::thread::yield_now();
        }
        for i in 0..LINES {
            let mut batch = BSU.to_vec();
            batch.extend_from_slice(format!("第{i}行\r\n").as_bytes());
            for byte in &batch {
                e.advance(std::slice::from_ref(byte));
            }
        }
        done.store(true, Ordering::Relaxed);
        let released = releaser.join().expect("释放线程不该 panic");
        // 最后一批可能还攒着
        force_release(&e);
        assert!(released > 0, "这条用例要真撞上释放才有意义");

        assert_eq!(e.history_lines(), LINES + 1 - 5);
        for i in 0..LINES {
            assert_eq!(
                e.line_text(i as i32).as_deref(),
                Some(format!("第{i}行").as_str()),
                "第 {i} 行丢了或重了"
            );
        }
        assert_eq!(e.line_text(LINES as i32).as_deref(), Some(""));
    }

    fn emulator_with_wsi() -> TerminalEmulator {
        let mut e = TerminalEmulator::new(TermSize::new(20, 4));
        // 绕过 cfg(windows)/环境变量门控，单独验证模式跟踪本身。
        e.win32_input_allowed = true;
        e
    }

    /// WSI 开关完全跟随应用的 `CSI ? 9001h/l`。
    #[test]
    fn win32_input_mode_follows_private_mode() {
        let e = emulator_with_wsi();
        assert!(!e.win32_input_mode());
        e.advance(b"\x1b[?9001h");
        assert!(e.win32_input_mode());
        e.advance(b"\x1b[?9001l");
        assert!(!e.win32_input_mode());
    }

    /// 序列被 PTY 读取切成两批时也必须识别（解析器状态机跨 chunk）。
    #[test]
    fn win32_input_mode_survives_chunk_split() {
        let e = emulator_with_wsi();
        e.advance(b"\x1b[?90");
        assert!(!e.win32_input_mode());
        e.advance(b"01h");
        assert!(e.win32_input_mode());
        e.advance(b"\x1b[?900");
        e.advance(b"1l");
        assert!(!e.win32_input_mode());
    }

    /// 普通私有模式（光标、鼠标、括号粘贴）不能被误当成 WSI。
    #[test]
    fn other_private_modes_do_not_toggle_win32_input() {
        let e = emulator_with_wsi();
        e.advance(b"\x1b[?1h\x1b[?25l\x1b[?1000h\x1b[?2004h");
        assert!(!e.win32_input_mode());
    }

    /// RIS（`ESC c`）必须把 WSI 状态一并清掉。
    #[test]
    fn reset_clears_win32_input_mode() {
        let e = emulator_with_wsi();
        e.advance(b"\x1b[?9001h");
        assert!(e.win32_input_mode());
        e.advance(b"\x1bc");
        assert!(!e.win32_input_mode());
    }

    #[cfg(not(windows))]
    #[test]
    fn win32_input_mode_disabled_off_windows() {
        let e = TerminalEmulator::new(TermSize::new(20, 4));
        e.advance(b"\x1b[?9001h");
        assert!(!e.win32_input_mode());
    }
}
