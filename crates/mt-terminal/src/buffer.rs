//! 整个缓冲(回看 + 当前屏)的读出与清除 —— 终端右键「全选 / 将缓冲保存为文件 /
//! 清除缓冲」的底层。
//!
//! # 读出口径
//!
//! 与 [`TerminalEmulator::line_text`] 同一套取字规则(跳过宽字符右半边的 spacer 格),
//! 再多两条:
//!
//! - **软换行合并回一行**:终端自己折的行(行尾格带 `WRAPLINE`)接着下一行写,不插
//!   换行符 —— 存出来的文本里,一条长命令 / 长路径还是一行,不随当时的窗口宽度断开;
//! - **只裁逻辑行行尾的空白**:折行处的空格是正文(`foo bar` 正好折在空格上),不能裁。
//!
//! 末尾的空行(屏幕下半截还没写到的那些)一并去掉,文件以一个换行收尾。
//!
//! # 清除口径
//!
//! [`TerminalEmulator::clear_scrollback`] 只清回看,与程序发 `ESC[3J` 同效果,但**不经
//! 解析器**:reader 线程可能正停在一条转义序列的半中间,往解析器里插字节会把那条
//! 序列拆坏。直接在 term 锁里调 `Handler::clear_screen(Saved)`,与解析器状态无关。
//!
//! alt screen(vim / less / Codex)期间,主屏的回看挂在 alacritty 的**非活动** grid 上,
//! `clear_screen(Saved)` 只看活动 grid、碰不到它;唯一的公开入口是 `set_options` ——
//! 它按配置的回滚行数收缩**主屏那份** grid 的历史(alt screen 期间正是非活动的那份)。
//! 于是先把回滚行数设成 0 再设回原值:历史被收光,容量照旧。`swap_alt` 那条路不能走,
//! 切回 alt 时它会把 TUI 的整屏内容重置掉。

use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{ClearMode, Handler as _};
use std::sync::atomic::Ordering;

use crate::TerminalEmulator;

/// 缓冲里的一个物理行:正文(已跳过 spacer 格)+ 它是不是被终端折到下一行的。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferRow {
    pub text: String,
    /// 行尾格带 `WRAPLINE`:下一行是这一行的接续。
    pub wrapped: bool,
}

impl TerminalEmulator {
    /// 整个缓冲(回看 + 当前屏,当前活动的那块屏)的纯文本,口径见模块注释。
    ///
    /// 整段持一次 term 锁:一万行回看 × 两百列也就几毫秒,期间 reader 线程等着 ——
    /// 分两次取会让中间滚过的输出把回看与屏幕错开一截。
    pub fn buffer_text(&self) -> String {
        let rows = {
            let term = self.term.lock();
            let grid = term.grid();
            let columns = term.columns();
            let mut rows = Vec::with_capacity(term.total_lines());
            for line in term.topmost_line().0..=term.bottommost_line().0 {
                let row = &grid[Line(line)];
                let mut text = String::with_capacity(columns);
                for col in 0..columns {
                    let cell = &row[Column(col)];
                    if cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                    {
                        continue;
                    }
                    text.push(cell.c);
                    if let Some(zerowidth) = cell.zerowidth() {
                        text.extend(zerowidth.iter());
                    }
                }
                let wrapped =
                    columns > 0 && row[Column(columns - 1)].flags.contains(Flags::WRAPLINE);
                rows.push(BufferRow { text, wrapped });
            }
            rows
        };
        join_buffer_rows(&rows)
    }

    /// 缓冲里有没有任何可见内容(非空白字符)。
    ///
    /// 从屏幕底部往上扫、碰到第一个非空白格就停 —— 提示符就在屏幕上,平常扫一两行就
    /// 有结论;真正的空缓冲本来也没几行可扫。
    pub fn has_buffer_content(&self) -> bool {
        let term = self.term.lock();
        let grid = term.grid();
        let columns = term.columns();
        (term.topmost_line().0..=term.bottommost_line().0)
            .rev()
            .any(|line| {
                let row = &grid[Line(line)];
                (0..columns).any(|col| {
                    let c = row[Column(col)].c;
                    c != ' ' && c != '\t' && c != '\0'
                })
            })
    }

    /// 清掉回看(主屏的历史),当前屏不动。口径见模块注释「清除口径」。
    ///
    /// 内容代数照常 +1:查找要据此重扫,AI 任务标记的指纹校验会把指进历史里的锚点剪掉。
    pub fn clear_scrollback(&self) {
        let mut term = self.term.lock();
        self.generation.fetch_add(1, Ordering::Relaxed);
        if term.mode().contains(TermMode::ALT_SCREEN) {
            let scrollback = self.scrollback();
            term.set_options(crate::term_config(0));
            term.set_options(crate::term_config(scrollback));
        } else {
            term.clear_screen(ClearMode::Saved);
        }
    }

    /// 清掉回看与当前屏,光标回左上角 —— `ESC[H ESC[2J ESC[3J` 的本地等价物。
    ///
    /// 给**已经没有程序在跑**的终端用(shell 退出后的回看):活着的 shell 不知道屏幕被
    /// 清了,不会重画提示符,活终端走 [`Self::clear_scrollback`] + 让 shell 自己清屏。
    /// 先清屏再清回看:主屏的 `ED2` 会把屏上内容推进回看(xterm 同款),顺序反了就白清。
    pub fn clear_all(&self) {
        let mut term = self.term.lock();
        self.generation.fetch_add(1, Ordering::Relaxed);
        term.goto(0, 0);
        term.clear_screen(ClearMode::All);
        term.clear_screen(ClearMode::Saved);
    }
}

/// 物理行 → 逻辑行文本。软换行接回一行,只裁逻辑行尾的空白,去掉末尾的空行。
///
/// 非空时以一个换行收尾;整个缓冲都是空白时返回空串。
pub fn join_buffer_rows(rows: &[BufferRow]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for row in rows {
        current.push_str(&row.text);
        if !row.wrapped {
            let trimmed = current.trim_end().len();
            current.truncate(trimmed);
            lines.push(std::mem::take(&mut current));
        }
    }
    // 最后一行还挂着「折到下一行」(写满屏幕最后一格):照样收尾
    if !current.is_empty() {
        let trimmed = current.trim_end().len();
        current.truncate(trimmed);
        lines.push(current);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TermSize;

    fn row(text: &str, wrapped: bool) -> BufferRow {
        BufferRow {
            text: text.to_string(),
            wrapped,
        }
    }

    #[test]
    fn 软换行接回一行且折行处的空格保留() {
        let rows = [
            row("echo foo ", true),
            row("bar      ", false),
            row("next     ", false),
        ];
        assert_eq!(join_buffer_rows(&rows), "echo foo bar\nnext\n");
    }

    #[test]
    fn 行尾空白裁掉_末尾空行去掉_中间空行保留() {
        let rows = [
            row("a   ", false),
            row("    ", false),
            row("b\t  ", false),
            row("    ", false),
            row("    ", false),
        ];
        assert_eq!(join_buffer_rows(&rows), "a\n\nb\n");
    }

    #[test]
    fn 全是空白时是空串() {
        assert_eq!(join_buffer_rows(&[row("   ", false), row("", false)]), "");
        assert_eq!(join_buffer_rows(&[]), "");
    }

    #[test]
    fn 最后一行写满屏幕也照样收尾() {
        assert_eq!(join_buffer_rows(&[row("abc", true)]), "abc\n");
    }

    fn emulator(columns: usize, lines: usize) -> TerminalEmulator {
        TerminalEmulator::with_scrollback(TermSize::new(columns, lines), 100)
    }

    /// 回看 + 当前屏都在;终端自己折的长行合并回一行,宽字符不重复。
    #[test]
    fn 缓冲文本含回看并合并软换行() {
        let e = emulator(10, 3);
        e.advance(b"one\r\ntwo\r\nthree\r\n");
        e.advance("0123456789abc 中文\r\n".as_bytes());
        assert!(e.history_lines() > 0, "前几行要已经进了回看");
        assert_eq!(e.buffer_text(), "one\ntwo\nthree\n0123456789abc 中文\n");
        assert!(e.has_buffer_content());
    }

    #[test]
    fn 空终端没有内容() {
        let e = emulator(10, 3);
        assert!(!e.has_buffer_content());
        assert_eq!(e.buffer_text(), "");
    }

    /// 只清回看:屏幕上的内容原样留着。
    #[test]
    fn 清回看不动当前屏() {
        let e = emulator(10, 3);
        for i in 0..10 {
            e.advance(format!("l{i}\r\n").as_bytes());
        }
        e.advance(b"$ ");
        assert!(e.history_lines() > 0);
        let screen = e.visible_lines();
        let before = e.generation();
        e.clear_scrollback();
        assert_eq!(e.history_lines(), 0);
        assert_eq!(e.visible_lines(), screen);
        assert!(e.generation() > before, "查找与标记要据代数重扫");
        // 回滚容量照旧:之后的输出照样进回看
        for i in 0..10 {
            e.advance(format!("m{i}\r\n").as_bytes());
        }
        assert!(e.history_lines() > 0);
    }

    /// alt screen 期间清的是**主屏**的回看,TUI 的画面一格不动;退出 TUI 后回看是空的。
    #[test]
    fn alt_screen_里清的是主屏回看且不动_tui_画面() {
        let e = emulator(10, 3);
        for i in 0..10 {
            e.advance(format!("l{i}\r\n").as_bytes());
        }
        e.advance(b"\x1b[?1049h\x1b[Hvim-ui");
        let tui = e.visible_lines();
        e.clear_scrollback();
        assert_eq!(e.visible_lines(), tui, "TUI 画面不许被重置");
        e.advance(b"\x1b[?1049l");
        assert_eq!(e.history_lines(), 0);
        assert_eq!(e.scrollback(), 100, "回滚容量要还原");
    }

    #[test]
    fn 全部清除连屏带回看() {
        let e = emulator(10, 3);
        for i in 0..10 {
            e.advance(format!("l{i}\r\n").as_bytes());
        }
        e.clear_all();
        assert_eq!(e.history_lines(), 0);
        assert!(!e.has_buffer_content());
        let cursor = e.with_term(|t| t.grid().cursor.point);
        assert_eq!((cursor.line.0, cursor.column.0), (0, 0));
    }
}
