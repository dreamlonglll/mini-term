//! SSH 接管:本地 pane 临时把终端交给内置 SSH 会话,断开后回到本地。
//!
//! # 为什么要拍快照
//!
//! 接管期间本地 ConPTY 不杀、它的输出被宿主丢弃,SSH 会话的输出画进**同一个**
//! emulator。回到本地时 conhost 不会重画整屏 —— 它只按自己记着的屏幕模型发增量,
//! emulator 的可见屏幕与光标要是对不上,之后每一笔局部重绘都落在错的位置上(花屏)。
//! 所以接管要像全屏程序进出备用屏那样:开始时拍下本地屏幕,结束时原样写回;
//! 远端会话的内容推进回看,往上翻还看得到。
//!
//! # 拍得到什么、怎么还原
//!
//! | 状态 | 取法 | 还原 |
//! |---|---|---|
//! | 可见屏幕(逐格含属性) | `grid()` 逐行克隆 `Row`,只拍可见行 | 写回 `grid_mut()` |
//! | 光标(位置 / 属性模板 / 字符集 / 待折行) | `grid().cursor` | 直接赋值 |
//! | DECSC 保存的光标 | `grid().saved_cursor` | 直接赋值 |
//! | 模式位 | `mode()` 只读 | 字段私有,经 `Handler` 的 DECSET / DECRST / SM / RM 入口逐位调 |
//! | 备用屏 | `ALT_SCREEN` 位;`swap_alt()` 换到主屏再拍一份 | `swap_alt()` 换回去 |
//! | 滚动区域 | 私有、无读回口 → `Scratch::scroll_region` 跟着 DECSTBM 记 | `Handler::set_scrolling_region` |
//! | kitty 键盘协议标志 | `mode()` 里那五位 | `pop_keyboard_modes` + `push_keyboard_mode` |
//! | 调色板(OSC 4 / 10 / 11 / 12) | `colors()` | `set_color` / `reset_color` |
//! | 光标样式(DECSCUSR) | `cursor_style()` | `set_cursor_style` |
//! | win32-input-mode | emulator 自己的标志 | 直接写回 |
//!
//! 取不到、两头都按基线复位的:SO / SI 选中的字符集(`active_charset` 私有)→ G0;
//! 制表位(`tabs` 私有)→ 每 8 列一档,即 alacritty 新建 `Term` 的默认,经 TBC + HTS 立。
//! kitty 键盘协议只还原当前生效的那一层,不还原整个栈(栈私有)。本 crate 建 `Term` 时
//! 没开 `kitty_keyboard`,alacritty 对这组序列一律不理,这几位实际恒为零,这里照规矩写全。
//! 窗口标题(OSC 0 / 2)走事件队列,由宿主自己管。
//!
//! 「经 `Handler` 入口」与「把等价的转义序列喂进解析器」是同一件事,只是不绕 vte 一圈
//! —— 也不经 [`crate::width::WidthOverride`],所以不会误动 win32-input-mode 与滚动区域的跟踪值,
//! 那两样在这里直接写。
//!
//! # 尺寸变了
//!
//! 接管期间 pane 可能被拖大拖小。结束时把快照放进一张临时 `Grid`(拍下时的尺寸),
//! 调 alacritty 自己的 `Grid::resize` 换到当前尺寸 —— 主屏 reflow、备用屏不 reflow,
//! 光标与 DECSC 一并按它的规则挪,与 `Term::resize` 对两块屏的处理一致。主屏因变窄 /
//! 变矮挤出可见区的本地行(开头的空行不要)接在远端内容后面推进回看,与不接管时 resize
//! 把它们顶进回看是同一个结果。尺寸变过时滚动区域按 alacritty resize 的规矩复位成整屏,
//! 不再还原。
//!
//! # 解析器
//!
//! 两头各换一个全新的 vte 解析器:本地在接管那一刻可能停在半截序列 / 半个 UTF-8 上,
//! 不能让远端的第一个字节接着它解析;远端断开时同理。还没结束的同步块(DEC 2026)
//! 先放出来再换:开始时放出来的是本地的字节(进快照),结束时是远端的(进回看)。
//!
//! # 本地停在备用屏
//!
//! 远端画在主屏上(备用屏没有回看,远端内容就留不进历史),所以开始时先拍备用屏、
//! `swap_alt()` 换到主屏再拍主屏;结束时两块都写回,最后 `swap_alt()` 进备用屏。
//! 已知差异:alacritty 进备用屏时把主屏的 DECSC 改写成主屏光标(与 `CSI ? 1049 h`
//! 同一口径),改不回去 —— 那时主屏已经换进私有的 `inactive_grid` 了。程序经 1049
//! 进的备用屏,那两个值本来就相等。
//!
//! # 别的
//!
//! - kitty 图形不做特殊处理:快照里的格子带着锚,接管期间摆放被当成看不见清掉的话,
//!   回来就是图没了;不会出错。
//! - 选区作废;回看滚回底部;⚑ 的光标水位作废(两边的行号对不上)。

use std::fmt;
use std::sync::atomic::Ordering;
use std::time::Instant;

use alacritty_terminal::grid::{Cursor, Dimensions, Grid, Row, Scroll};
use alacritty_terminal::index::Line;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::color::{COUNT, Colors};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{
    CharsetIndex, ClearMode, CursorStyle, Handler, KeyboardModes, NamedMode, NamedPrivateMode,
    Processor, TabulationClearMode,
};

use crate::{EventQueue, TermSize, TerminalEmulator, cursor_cell};

/// alacritty 新建 `Term` 时的制表位间隔(`term::INITIAL_TABSTOPS`,私有常量)。
const DEFAULT_TAB_INTERVAL: usize = 8;

/// 接管开始那一刻拍下的本地终端状态,只能交回 [`TerminalEmulator::end_takeover`]。
///
/// 只存可见行,不碰回看;一屏 200×50 也就几十 KB。
pub struct TakeoverSnapshot {
    /// 拍下时的尺寸。
    size: TermSize,
    /// 主屏的可见区与光标。
    primary: Screen,
    /// 本地停在备用屏时,备用屏的可见区与光标。
    alt: Option<Screen>,
    /// 全部模式位(含 `ALT_SCREEN` 与 kitty 键盘协议那几位)。
    mode: TermMode,
    win32_input: bool,
    colors: Colors,
    cursor_style: CursorStyle,
    /// DECSTBM 的跟踪值,`None` = 整屏。
    scroll_region: Option<(usize, Option<usize>)>,
}

impl fmt::Debug for TakeoverSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TakeoverSnapshot")
            .field("size", &self.size)
            .field("alt_screen", &self.alt.is_some())
            .field("mode", &self.mode)
            .field("win32_input", &self.win32_input)
            .finish_non_exhaustive()
    }
}

/// 一块屏(主屏或备用屏)的可见内容与两个光标。
struct Screen {
    /// 可见行,自上而下。
    rows: Vec<Row<Cell>>,
    cursor: Cursor<Cell>,
    saved_cursor: Cursor<Cell>,
}

impl Screen {
    fn capture(grid: &Grid<Cell>) -> Self {
        Self {
            rows: (0..grid.screen_lines())
                .map(|line| grid[Line(line as i32)].clone())
                .collect(),
            cursor: grid.cursor.clone(),
            saved_cursor: grid.saved_cursor.clone(),
        }
    }

    /// 原样写回 `grid` 的可见区。行数、列数必须已经与 `grid` 一致(见 [`fit`])。
    fn restore(self, grid: &mut Grid<Cell>) {
        debug_assert_eq!(self.rows.len(), grid.screen_lines());
        for (line, row) in self.rows.into_iter().enumerate() {
            grid[Line(line as i32)] = row;
        }
        grid.cursor = self.cursor;
        grid.saved_cursor = self.saved_cursor;
    }
}

/// [`fit`] 的结果。
struct Fitted {
    /// 挤出可见区的行,旧的在前。只有主屏 reflow 才会有。
    overflow: Vec<Row<Cell>>,
    screen: Screen,
}

/// 把 `from` 尺寸拍下的一块屏换到 `to`:放进临时 `Grid` 调 alacritty 自己的
/// `Grid::resize`(`reflow` 与 `Term::resize` 同口径:主屏 true、备用屏 false)。
fn fit(screen: Screen, from: TermSize, to: TermSize, reflow: bool) -> Fitted {
    if from == to {
        return Fitted {
            overflow: Vec::new(),
            screen,
        };
    }
    // 回看容量只要装得下挤出来的行:变窄时一行最多折成「原列数」行
    let history = if reflow {
        from.screen_lines.saturating_mul(from.columns)
    } else {
        0
    };
    let mut grid: Grid<Cell> = Grid::new(from.screen_lines, from.columns, history);
    screen.restore(&mut grid);
    grid.resize(reflow, to.screen_lines, to.columns);

    let mut overflow: Vec<Row<Cell>> = (1..=grid.history_size())
        .rev()
        .map(|back| grid[Line(-(back as i32))].clone())
        .collect();
    let blank = overflow.iter().take_while(|row| row.is_clear()).count();
    overflow.drain(..blank);
    Fitted {
        overflow,
        screen: Screen::capture(&grid),
    }
}

/// 把若干行(旧的在前)接到回看最新的一端:借可见区中转,一次写满一屏再整屏上滚。
/// 可见区随后会被整屏覆盖,中转留下的东西不要紧。
fn push_into_history(term: &mut Term<EventQueue>, mut rows: Vec<Row<Cell>>) {
    let lines = term.screen_lines();
    let region = Line(0)..Line(lines as i32);
    let grid = term.grid_mut();
    while !rows.is_empty() {
        let count = rows.len().min(lines);
        for (line, row) in rows.drain(..count).enumerate() {
            grid[Line(line as i32)] = row;
        }
        grid.scroll_up(&region, count);
    }
}

/// 由 [`apply_modes`] 逐位管的 DEC 私有模式(开关各自独立的那些)。
const PRIVATE_MODES: [(TermMode, NamedPrivateMode); 8] = [
    (TermMode::SHOW_CURSOR, NamedPrivateMode::ShowCursor),
    (TermMode::APP_CURSOR, NamedPrivateMode::CursorKeys),
    (TermMode::BRACKETED_PASTE, NamedPrivateMode::BracketedPaste),
    (TermMode::FOCUS_IN_OUT, NamedPrivateMode::ReportFocusInOut),
    (
        TermMode::ALTERNATE_SCROLL,
        NamedPrivateMode::AlternateScroll,
    ),
    (TermMode::LINE_WRAP, NamedPrivateMode::LineWrap),
    (TermMode::ORIGIN, NamedPrivateMode::Origin),
    (TermMode::URGENCY_HINTS, NamedPrivateMode::UrgencyHints),
];

/// 鼠标上报三选一。
const MOUSE_REPORTS: [(TermMode, NamedPrivateMode); 3] = [
    (
        TermMode::MOUSE_REPORT_CLICK,
        NamedPrivateMode::ReportMouseClicks,
    ),
    (
        TermMode::MOUSE_DRAG,
        NamedPrivateMode::ReportCellMouseMotion,
    ),
    (
        TermMode::MOUSE_MOTION,
        NamedPrivateMode::ReportAllMouseMotion,
    ),
];

/// 鼠标编码二选一。
const MOUSE_ENCODINGS: [(TermMode, NamedPrivateMode); 2] = [
    (TermMode::SGR_MOUSE, NamedPrivateMode::SgrMouse),
    (TermMode::UTF8_MOUSE, NamedPrivateMode::Utf8Mouse),
];

/// SM / RM 管的两个 ANSI 模式。
const ANSI_MODES: [(TermMode, NamedMode); 2] = [
    (TermMode::INSERT, NamedMode::Insert),
    (TermMode::LINE_FEED_NEW_LINE, NamedMode::LineFeedNewLine),
];

/// kitty 键盘协议:`TermMode` 里的位 ↔ `KeyboardModes` 里的位。
const KEYBOARD_MODES: [(TermMode, KeyboardModes); 5] = [
    (
        TermMode::DISAMBIGUATE_ESC_CODES,
        KeyboardModes::DISAMBIGUATE_ESC_CODES,
    ),
    (
        TermMode::REPORT_EVENT_TYPES,
        KeyboardModes::REPORT_EVENT_TYPES,
    ),
    (
        TermMode::REPORT_ALTERNATE_KEYS,
        KeyboardModes::REPORT_ALTERNATE_KEYS,
    ),
    (
        TermMode::REPORT_ALL_KEYS_AS_ESC,
        KeyboardModes::REPORT_ALL_KEYS_AS_ESC,
    ),
    (
        TermMode::REPORT_ASSOCIATED_TEXT,
        KeyboardModes::REPORT_ASSOCIATED_TEXT,
    ),
];

/// 把模式位逐位调成 `target`。`mode` 是私有字段,只能经 `Handler` 的入口改。
///
/// 不管的:`ALT_SCREEN`(`swap_alt`)、kitty 键盘协议(有栈)、`VI`(alacritty 自己的
/// 界面模式,接管不碰)。开 `ORIGIN` 会把光标挪回原点,调用方事后要把光标放回去。
fn apply_modes(term: &mut Term<EventQueue>, target: TermMode) {
    let current = *term.mode();
    for (bit, mode) in PRIVATE_MODES {
        match (current.contains(bit), target.contains(bit)) {
            (false, true) => term.set_private_mode(mode.into()),
            (true, false) => term.unset_private_mode(mode.into()),
            _ => {}
        }
    }
    // 同组互斥:先把开着的全关,再开目标那一位(开的时候 alacritty 会顺手清掉同组其余位)
    for group in [&MOUSE_REPORTS[..], &MOUSE_ENCODINGS[..]] {
        let mask = group
            .iter()
            .fold(TermMode::empty(), |mask, (bit, _)| mask | *bit);
        if current & mask == target & mask {
            continue;
        }
        for &(bit, mode) in group {
            if current.contains(bit) {
                term.unset_private_mode(mode.into());
            }
        }
        for &(bit, mode) in group {
            if target.contains(bit) {
                term.set_private_mode(mode.into());
            }
        }
    }
    match (
        current.contains(TermMode::APP_KEYPAD),
        target.contains(TermMode::APP_KEYPAD),
    ) {
        (false, true) => term.set_keypad_application_mode(),
        (true, false) => term.unset_keypad_application_mode(),
        _ => {}
    }
    for (bit, mode) in ANSI_MODES {
        match (current.contains(bit), target.contains(bit)) {
            (false, true) => term.set_mode(mode.into()),
            (true, false) => term.unset_mode(mode.into()),
            _ => {}
        }
    }
}

/// 模式位里 kitty 键盘协议当前生效的那一层。
fn keyboard_modes(mode: TermMode) -> KeyboardModes {
    KEYBOARD_MODES
        .iter()
        .filter(|(bit, _)| mode.contains(*bit))
        .fold(KeyboardModes::empty(), |modes, (_, flag)| modes | *flag)
}

/// 调色板逐项调成 `target`(`None` = 回到主题色)。
fn apply_colors(term: &mut Term<EventQueue>, target: &Colors) {
    let current = *term.colors();
    for index in 0..COUNT {
        match target[index] {
            Some(rgb) if current[index] != Some(rgb) => term.set_color(index, rgb),
            None if current[index].is_some() => term.reset_color(index),
            _ => {}
        }
    }
}

/// 制表位复位成每 8 列一档。`tabs` 是私有字段,只能经 TBC + HTS 逐个立 ——
/// 会挪光标,调用方事后要把光标放回去。
fn reset_tab_stops(term: &mut Term<EventQueue>) {
    term.clear_tabs(TabulationClearMode::All);
    for column in (0..term.columns()).step_by(DEFAULT_TAB_INTERVAL) {
        term.goto(0, column);
        term.set_horizontal_tabstop();
    }
}

impl TerminalEmulator {
    /// 接管开始:拍下本地终端(见 [`TakeoverSnapshot`] 与模块注释),再把会影响远端的
    /// 状态复位到基线 —— 模式位回到 alacritty 新建 `Term` 时的默认、kitty 键盘协议清零、
    /// win32-input-mode 关掉、滚动区域整屏、制表位默认、调色板与光标样式回到主题、
    /// SGR 与字符集复位、DECSC 清空。
    ///
    /// 光标**留在原地**:远端会话接着本地屏幕往下写,就像在 shell 里敲了 `ssh`。
    /// 本地停在备用屏时远端改画在主屏上(见模块注释)。
    ///
    /// 代数 +1;选区作废、回看滚回底部、⚑ 的光标水位作废。
    pub fn begin_takeover(&self) -> TakeoverSnapshot {
        let win32_input = self.win32_input_allowed.then_some(&self.win32_input);
        let mut term = self.term.lock();
        let scroll_region = {
            let mut parser = self.parser.lock();
            // 本地还没结束的同步块先放出来:那是本地屏幕的一部分,要进快照
            if parser.sync_deadline().is_some() {
                parser.stop_sync(&mut term, win32_input);
            }
            parser.vte = Processor::new();
            parser.cluster.scroll_region.take()
        };

        let mode = *term.mode();
        let alt = if mode.contains(TermMode::ALT_SCREEN) {
            let alt = Screen::capture(term.grid());
            // 离开备用屏不清它,只是把主屏换回来(主屏的本地内容随后一并拍下)
            term.swap_alt();
            Some(alt)
        } else {
            None
        };
        let snapshot = TakeoverSnapshot {
            size: TermSize::new(term.columns(), term.screen_lines()),
            primary: Screen::capture(term.grid()),
            alt,
            mode,
            win32_input: self.win32_input.swap(false, Ordering::Relaxed),
            colors: *term.colors(),
            cursor_style: term.cursor_style(),
            scroll_region,
        };

        // ── 远端从新建 Term 的基线起步(这几步会挪光标,光标放在最后写)
        apply_modes(&mut term, TermMode::default());
        term.pop_keyboard_modes(u16::MAX);
        term.set_scrolling_region(1, None);
        reset_tab_stops(&mut term);
        apply_colors(&mut term, &Colors::default());
        term.set_cursor_style(None);
        term.set_active_charset(CharsetIndex::G0);
        let local = &snapshot.primary.cursor;
        let grid = term.grid_mut();
        grid.cursor = Cursor {
            point: local.point,
            input_needs_wrap: local.input_needs_wrap,
            ..Cursor::default()
        };
        grid.saved_cursor = Cursor::default();
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        self.finish_takeover_transition(&term);
        snapshot
    }

    /// 接管结束:远端停在备用屏就先退出,把主屏整屏推进回看(远端会话的内容留在历史里),
    /// 再把快照写回可见区 —— 尺寸变过就按 alacritty 的 resize 规则把快照调到当前尺寸
    /// (见模块注释);光标、DECSC、模式位、滚动区域、kitty 键盘标志、调色板、光标样式、
    /// win32-input-mode 一并还原。
    ///
    /// 代数 +1;选区作废、回看滚回底部、⚑ 的光标水位作废。
    pub fn end_takeover(&self, snapshot: TakeoverSnapshot) {
        let win32_input = self.win32_input_allowed.then_some(&self.win32_input);
        let mut term = self.term.lock();
        let size = TermSize::new(term.columns(), term.screen_lines());
        let same_size = size == snapshot.size;
        {
            let mut parser = self.parser.lock();
            // 远端最后一个同步块放出来再收场:那是远端会话的尾巴,要进回看
            if parser.sync_deadline().is_some() {
                parser.stop_sync(&mut term, win32_input);
            }
            parser.vte = Processor::new();
            parser.cluster.scroll_region = snapshot.scroll_region.filter(|_| same_size);
        }

        // ── 远端收场:停在备用屏先退出,主屏整屏推进回看(清出来的空行是默认底色)
        if term.mode().contains(TermMode::ALT_SCREEN) {
            term.swap_alt();
        }
        term.grid_mut().cursor.template = Cell::default();
        term.clear_screen(ClearMode::All);

        // ── 终端状态还原(这几步会挪光标,光标随屏幕最后写)
        term.set_scrolling_region(1, None);
        if let Some((top, bottom)) = snapshot.scroll_region.filter(|_| same_size) {
            term.set_scrolling_region(top, bottom);
        }
        reset_tab_stops(&mut term);
        apply_modes(&mut term, snapshot.mode);
        apply_colors(&mut term, &snapshot.colors);
        term.set_cursor_style(Some(snapshot.cursor_style));
        term.set_active_charset(CharsetIndex::G0);

        // ── 屏幕写回:主屏挤出来的本地行先推进回看;本地原先在备用屏的,再进备用屏写一份
        let primary = fit(snapshot.primary, snapshot.size, size, true);
        push_into_history(&mut term, primary.overflow);
        primary.screen.restore(term.grid_mut());
        if let Some(alt) = snapshot.alt {
            term.swap_alt();
            fit(alt, snapshot.size, size, false)
                .screen
                .restore(term.grid_mut());
        }
        // kitty 键盘协议有栈,只还原当前生效那一层;放在 swap_alt 之后(它会换栈)
        term.pop_keyboard_modes(u16::MAX);
        let keyboard = keyboard_modes(snapshot.mode);
        if !keyboard.is_empty() {
            term.push_keyboard_mode(keyboard);
        }
        self.win32_input
            .store(snapshot.win32_input, Ordering::Relaxed);
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        self.finish_takeover_transition(&term);
    }

    /// 接管两头的收尾:代数 +1(持着 term 锁记,口径同 `advance`)、⚑ 的光标水位作废、
    /// OSC 7501 扫描器复位、光标停稳追踪记一笔。
    fn finish_takeover_transition(&self, term: &Term<EventQueue>) {
        self.osc7501.reset();
        self.generation.fetch_add(1, Ordering::Relaxed);
        *self.cursor_floor.lock() = None;
        self.cursor_settle
            .lock()
            .observe(cursor_cell(term), Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::{Column, Point};

    fn emulator(columns: usize, lines: usize) -> TerminalEmulator {
        TerminalEmulator::with_scrollback(TermSize::new(columns, lines), 1000)
    }

    /// 活动屏的可见区逐格 + 两个光标 + 模式位:接管前后要逐一相等的东西。
    #[derive(Debug, PartialEq)]
    struct Picture {
        rows: Vec<Row<Cell>>,
        cursor: Cursor<Cell>,
        saved_cursor: Cursor<Cell>,
        mode: TermMode,
    }

    fn picture(e: &TerminalEmulator) -> Picture {
        e.with_term(|t| {
            let screen = Screen::capture(t.grid());
            Picture {
                rows: screen.rows,
                cursor: screen.cursor,
                saved_cursor: screen.saved_cursor,
                mode: *t.mode(),
            }
        })
    }

    /// 回看里的全部文本,旧的在前。
    fn history_text(e: &TerminalEmulator) -> Vec<String> {
        (0..e.history_lines() as i32)
            .filter_map(|line| e.line_text(line))
            .collect()
    }

    fn cursor_point(e: &TerminalEmulator) -> Point {
        e.with_term(|t| t.grid().cursor.point)
    }

    /// 核心用例:本地屏幕(颜色 / 粗体 / 底色 / 宽字符)、光标(位置 + 未用完的 SGR 模板)、
    /// DECSC、模式位、调色板、光标样式在「远端刷屏 → 进备用屏 → 开鼠标上报与 bracketed
    /// paste → 改滚动区域 / 制表位 / 插入模式」之后逐格还原;远端主屏的内容按序进了回看。
    #[test]
    fn 远端刷屏进备用屏改模式之后本地逐格还原() {
        let e = emulator(24, 6);
        e.advance("\x1b[31mred\x1b[0m plain\r\n\x1b[1;44mbold\x1b[0m\r\n中文ok".as_bytes());
        // DECSC 在 (2,4);光标停在 (1,8),模板是绿字下划线;应用光标键、隐藏光标、
        // 改了 2 号色、闪烁下划线光标
        e.advance(b"\x1b[3;5H\x1b7\x1b[2;9H\x1b[32;4m\x1b[?1h\x1b[?25l");
        e.advance(b"\x1b]4;2;rgb:11/22/33\x1b\\\x1b[3 q");
        let before = picture(&e);
        let color_before = e.with_term(|t| t.colors()[2]);
        let style_before = e.with_term(|t| t.cursor_style());
        assert!(color_before.is_some());

        let snapshot = e.begin_takeover();
        assert_eq!(e.mode(), TermMode::default(), "远端从新建 Term 的模式起步");
        e.with_term(|t| {
            let grid = t.grid();
            assert_eq!(grid.cursor.template, Cell::default(), "SGR 复位");
            assert_eq!(
                grid.cursor.point, before.cursor.point,
                "远端接着本地光标往下写"
            );
            assert_eq!(grid.saved_cursor, Cursor::default(), "DECSC 不漏给远端");
            assert_eq!(t.colors()[2], None, "调色板回到主题色");
            assert_eq!(t.cursor_style(), CursorStyle::default());
        });

        e.advance(b"\r\n");
        for i in 0..20 {
            e.advance(format!("\x1b[35mremote{i}\x1b[0m\r\n").as_bytes());
        }
        e.advance(b"\x1b[?1049h\x1b[2J\x1b[Hvim\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?1004h");
        e.advance(b"\x1b[4h\x1b[3;5r\x1b[3g\x1b[1;33m\x1b7\x1b[6 q");
        e.advance(b"\x1b]4;1;rgb:aa/bb/cc\x1b\\");
        assert!(
            e.mode()
                .contains(TermMode::ALT_SCREEN | TermMode::BRACKETED_PASTE)
        );

        e.end_takeover(snapshot);
        assert_eq!(picture(&e), before, "可见区、光标、DECSC、模式位逐一还原");
        e.with_term(|t| {
            assert_eq!(t.colors()[2], color_before, "本地改过的调色板回来了");
            assert_eq!(t.colors()[1], None, "远端改的调色板不留");
            assert_eq!(t.cursor_style(), style_before);
        });

        // 远端主屏刷过的每一行都按序进了回看;备用屏的内容随远端退出备用屏丢掉
        let history = history_text(&e);
        let remote: Vec<&str> = history
            .iter()
            .map(String::as_str)
            .filter(|line| line.starts_with("remote"))
            .collect();
        let expected: Vec<String> = (0..20).map(|i| format!("remote{i}")).collect();
        assert_eq!(remote, expected);
        assert!(!history.iter().any(|line| line == "vim"));

        // 滚动区域回到整屏:最后一行换行把整屏往上推
        let h = e.history_lines();
        e.advance(b"\x1b[6;1H\n");
        assert_eq!(e.history_lines(), h + 1);
        // 远端清掉的制表位复位成每 8 列一档
        e.advance(b"\r\t*");
        assert!(e.visible_columns()[5].contains(&(8, '*')));
    }

    /// win32-input-mode 往返:本地开着的,远端期间关掉、回来恢复;本地关着的,远端里
    /// 就算有人发了 `CSI ? 9001 h` 也不带回来。
    #[test]
    fn win32_input_mode_往返() {
        let mut e = TerminalEmulator::new(TermSize::new(20, 4));
        e.win32_input_allowed = true;
        e.advance(b"\x1b[?9001h");
        assert!(e.win32_input_mode());
        let snapshot = e.begin_takeover();
        assert!(!e.win32_input_mode(), "远端不认 WSI,按键要发传统字节");
        e.advance(b"remote\r\n");
        e.end_takeover(snapshot);
        assert!(e.win32_input_mode(), "回到本地 ConPTY 照旧发 WSI 记录");

        e.advance(b"\x1b[?9001l");
        let snapshot = e.begin_takeover();
        e.advance(b"\x1b[?9001h");
        assert!(e.win32_input_mode());
        e.end_takeover(snapshot);
        assert!(!e.win32_input_mode());
    }

    /// 接管期间被拖大:尺寸是当前的,本地内容在原位,光标原地。
    #[test]
    fn 接管期间变大之后尺寸正确且本地内容在原位() {
        let e = emulator(20, 5);
        e.advance(b"prompt-1\r\nprompt-2 $ ");
        let snapshot = e.begin_takeover();
        e.advance(b"\r\nremote\r\n");
        e.resize(TermSize::new(30, 8));
        e.advance(b"after resize\r\n");
        e.end_takeover(snapshot);

        assert_eq!(e.term_size(), TermSize::new(30, 8));
        let lines = e.visible_lines();
        assert_eq!(lines.len(), 8);
        assert_eq!(lines[..3], ["prompt-1", "prompt-2 $", ""]);
        assert_eq!(cursor_point(&e), Point::new(Line(1), Column(11)));
        let history = history_text(&e);
        assert!(history.iter().any(|line| line == "remote"));
        assert!(history.iter().any(|line| line == "after resize"));
    }

    /// 接管期间被拖小:按 alacritty 的 reflow 规则折行,挤出可见区的本地行接在远端内容
    /// 后面进回看,光标跟着内容走、落在新尺寸里。
    #[test]
    fn 接管期间变小之后按_reflow_还原且挤出的本地行进回看() {
        let e = emulator(20, 4);
        e.advance(b"l0\r\nl1\r\nabcdefghijklmnop\r\n$ ");
        let snapshot = e.begin_takeover();
        e.resize(TermSize::new(8, 3));
        e.advance(b"\r\nremote\r\n");
        e.end_takeover(snapshot);

        assert_eq!(e.term_size(), TermSize::new(8, 3));
        assert_eq!(e.visible_lines(), ["abcdefgh", "ijklmnop", "$"]);
        assert_eq!(cursor_point(&e), Point::new(Line(2), Column(2)));
        let history = history_text(&e);
        assert_eq!(history[history.len() - 2..], ["l0", "l1"]);
        assert!(history.iter().any(|line| line == "remote"));
    }

    /// 本地停在备用屏(比如开着 vim):远端画在主屏上;回来时备用屏原样回来,本地程序
    /// 退出备用屏后主屏是它进去之前的样子,远端内容在主屏回看里。
    #[test]
    fn 本地停在备用屏时两块屏都还原() {
        let e = emulator(20, 4);
        e.advance(b"shell$ vim\r\n");
        e.advance(b"\x1b[?1049h\x1b[H\x1b[44mTUI-1\x1b[0m\r\nTUI-2\x1b[?1000h\x1b[?1006h");
        let before = picture(&e);

        let snapshot = e.begin_takeover();
        assert_eq!(e.mode(), TermMode::default());
        assert_eq!(e.visible_lines()[0], "shell$ vim", "远端画在主屏上");
        e.advance(b"remote-1\r\nremote-2\r\n");
        e.end_takeover(snapshot);
        assert_eq!(picture(&e), before, "备用屏逐格回来,鼠标上报照旧开着");

        e.advance(b"\x1b[?1049l");
        assert_eq!(e.visible_lines()[0], "shell$ vim");
        assert_eq!(cursor_point(&e), Point::new(Line(1), Column(0)));
        assert!(history_text(&e).iter().any(|line| line == "remote-1"));
    }

    /// 本地设过的滚动区域原样回来;远端起步时是整屏。
    #[test]
    fn 滚动区域往返() {
        let e = emulator(10, 6);
        e.advance(b"L0\r\nL1\r\nL2\r\nL3\r\nL4\r\nL5");
        // 区域是第 2~4 行,光标停在区域底边
        e.advance(b"\x1b[2;4r\x1b[4;1H");
        let snapshot = e.begin_takeover();
        let h = e.history_lines();
        e.advance(b"\x1b[6;1H\n");
        assert_eq!(e.history_lines(), h + 1, "远端起步时区域是整屏");
        e.advance(b"\x1b[1;3r");
        e.end_takeover(snapshot);

        assert_eq!(e.visible_lines(), ["L0", "L1", "L2", "L3", "L4", "L5"]);
        let h = e.history_lines();
        e.advance(b"\nX");
        assert_eq!(
            e.visible_lines(),
            ["L0", "L2", "L3", "X", "L4", "L5"],
            "在区域底边换行只滚第 2~4 行"
        );
        assert_eq!(e.history_lines(), h);
    }

    /// 尺寸变过(接管前或接管期间)滚动区域按 alacritty resize 的规矩是整屏,不再还原。
    #[test]
    fn 尺寸变过时滚动区域是整屏() {
        // 接管前就 resize 过:alacritty 已把区域复位,跟踪值随之作废
        let e = emulator(10, 6);
        e.advance(b"\x1b[2;4r");
        e.resize(TermSize::new(12, 6));
        let snapshot = e.begin_takeover();
        e.end_takeover(snapshot);
        let h = e.history_lines();
        e.advance(b"L0\x1b[6;1H\n");
        assert_eq!(
            e.history_lines(),
            h + 1,
            "整屏区域:最后一行换行把整屏往上推"
        );

        // 接管期间 resize
        let e = emulator(10, 6);
        e.advance(b"\x1b[2;4r");
        let snapshot = e.begin_takeover();
        e.resize(TermSize::new(12, 7));
        e.end_takeover(snapshot);
        let h = e.history_lines();
        e.advance(b"L0\x1b[7;1H\n");
        assert_eq!(e.history_lines(), h + 1);
    }

    /// 两头没结束的同步块都先放出来:本地的进快照,远端的进回看;之后用的是全新的解析器。
    #[test]
    fn 两头没结束的同步块都先放出来() {
        let e = emulator(20, 4);
        e.advance(b"\x1b[?2026hlocal-frame");
        assert_eq!(e.visible_lines()[0], "");
        let snapshot = e.begin_takeover();
        assert_eq!(e.sync_deadline(), None);
        assert_eq!(e.visible_lines()[0], "local-frame", "本地的帧进了快照");
        e.advance(b"\r\n\x1b[?2026hremote-tail");
        e.end_takeover(snapshot);
        assert_eq!(e.sync_deadline(), None);
        assert_eq!(e.visible_lines()[0], "local-frame");
        assert!(history_text(&e).iter().any(|line| line == "remote-tail"));
    }

    /// 本地停在半截序列上:远端的第一个字节不接着它解析。
    #[test]
    fn 半截序列不串到远端() {
        let e = emulator(20, 4);
        e.advance(b"$ \x1b[3");
        let snapshot = e.begin_takeover();
        e.advance(b"1mX");
        assert_eq!(e.visible_lines()[0], "$ 1mX");
        assert_eq!(
            e.with_term(|t| t.grid()[Line(0)][Column(4)].fg),
            Cell::default().fg,
            "没被当成 SGR 31"
        );
        e.end_takeover(snapshot);
        assert_eq!(e.visible_lines()[0], "$");
    }

    /// 两头都推进代数(查找引擎靠它判「有没有新输出」),⚑ 的光标水位作废。
    #[test]
    fn 两头推进代数并作废光标水位() {
        let e = emulator(20, 4);
        e.advance(b"local");
        e.arm_cursor_floor();
        let g = e.generation();
        let snapshot = e.begin_takeover();
        assert!(e.generation() > g);
        assert_eq!(e.take_cursor_floor(), None);

        e.arm_cursor_floor();
        let g = e.generation();
        e.end_takeover(snapshot);
        assert!(e.generation() > g);
        assert_eq!(e.take_cursor_floor(), None);
    }

    /// 开着 kitty 图形、接管期间来了图形命令又 resize:不出岔子。
    #[test]
    fn 开着_kitty_图形也不出岔子() {
        let e = TerminalEmulator::new(TermSize::new(20, 4)).with_kitty_graphics(true);
        e.advance(b"local\r\n");
        let snapshot = e.begin_takeover();
        e.advance(b"\x1b_Ga=T,f=100;AAAA\x1b\\remote\r\n");
        e.resize(TermSize::new(10, 3));
        e.end_takeover(snapshot);
        assert_eq!(e.term_size(), TermSize::new(10, 3));
        assert_eq!(e.visible_lines()[0], "local");
        let images = e.with_term(|t| e.kitty().expect("开着").visible_images(t).len());
        assert_eq!(images, 0);
    }
}
