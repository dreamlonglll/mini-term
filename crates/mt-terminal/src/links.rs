//! 终端输出里的文件路径识别 —— 按住 Ctrl(macOS 上 ⌘)悬停 / 单击打开文件用。
//!
//! 两件纯逻辑,都不碰 gpui:
//!
//! - [`TerminalEmulator::logical_line_at`] 顺着软换行(`WRAPLINE`)把被终端折开的
//!   几段物理行拼回一整行,每个字符记住自己落在哪一格 —— 长路径被折成两行时照样
//!   认得出,下划线也能跨行画。
//! - [`path_candidates`] 在一行里找「鼠标所在那个字符」所属的路径片段,把
//!   `:行:列` / `(行,列)` / `#L行` / Python 的 `", line N` 这些行号写法一并拆出来。
//!
//! 这里**只看形状**。存不存在、在不在项目里、相对路径对着哪个目录解析,全是宿主的事
//! (mt-app 的 `terminal_links`)—— 所以给出的是一串候选(最像的排在前面),宿主
//! 挨个去盘上验,第一个成立的才算链接。入口是 [`TerminalEmulator::path_candidates_at`]。
//!
//! # 硬换行:TUI 自己折开的路径
//!
//! 只有终端自己折的行(写满一行自动换行)带 `WRAPLINE`。Claude Code / Codex 这类
//! TUI 按宽度**自己插换行符**排版(Ink 的 wrap 遇到超长「词」直接在列宽处硬断),
//! 折开的路径在 grid 里就是两行互不相干的文本:
//!
//! ```text
//! ● …宽屏终端要更长的行:D:\Git\mini-term\crates   ← 写到右边缘
//!   \mt-core\src\lib.rs                          ← 下一行缩进两格接着写
//! ```
//!
//! 判据只看形状:上一行的内容写到右边缘附近([`HARD_WRAP_EDGE_SLACK`])、收尾是路径
//! 字符,下一行(去掉缩进)以路径字符开头,就把两行接起来再认一遍。接上的候选排前面,
//! 没接的照样垫底 —— 散文在词边界被折开(`…the file` / `src/a.ts`)时接出来的
//! `filesrc/a.ts` 盘上不存在,宿主自然落到没接的那个。一行一个路径的短列表不贴右边缘,
//! 不会被接。

use std::ops::Range;

use alacritty_terminal::grid::{Dimensions as _, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};

use crate::TerminalEmulator;

/// 往上 / 往下各最多拼多少段软换行。再长的「一行」多半是日志刷屏,不是路径。
const MAX_WRAPPED_ROWS: i32 = 16;
/// 往上 / 往下各最多接几段 TUI 自己折开的行(硬换行)。
const MAX_HARD_WRAP_ROWS: usize = 2;
/// 行尾离右边缘不超过这么多列,才算「写满了、被 TUI 折到下一行」。
///
/// 留的是 TUI 的右边距(Claude Code 一两列)与「宽字符放不下、行尾空一格」。
const HARD_WRAP_EDGE_SLACK: usize = 6;
/// 认作路径的片段最多多少个字符。超了直接不认,免得在一整屏 base64 上逐字扫。
const MAX_PATH_CHARS: usize = 1024;

/// 逻辑行里一个字符落在哪一格。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineCell {
    /// grid 行号(alacritty 的 `Line`:0 = 屏幕首行,负数 = 回看缓冲)。
    pub line: i32,
    pub column: usize,
    /// 宽字符(占两列)。下划线要多盖一列。
    pub wide: bool,
}

/// 软换行拼回来的一整行。`chars[i]` 画在 `cells[i]` 那一格。
///
/// 宽字符右半边的 spacer 格不进来 —— 与 [`TerminalEmulator::line_text`] 同一口径。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogicalLine {
    pub chars: Vec<char>,
    pub cells: Vec<LineCell>,
}

impl LogicalLine {
    /// 鼠标落在 `(line, column)` 时对着第几个字符。点在宽字符右半格算它本身。
    pub fn index_at(&self, line: i32, column: usize) -> Option<usize> {
        self.cells.iter().position(|cell| {
            cell.line == line && (cell.column == column || (cell.wide && cell.column + 1 == column))
        })
    }

    /// 第一个 / 最后一个非空白字符的下标。全是空白返回 `None`。
    fn content_bounds(&self) -> Option<(usize, usize)> {
        let first = self.chars.iter().position(|c| !c.is_whitespace())?;
        let last = self.chars.iter().rposition(|c| !c.is_whitespace())?;
        Some((first, last))
    }

    /// 内容写到了最后一个物理行的右边缘附近、收尾是路径字符 —— 像是被 TUI 折下去了。
    fn runs_to_edge(&self, columns: usize) -> bool {
        let Some((_, last)) = self.content_bounds() else {
            return false;
        };
        let cell = self.cells[last];
        let end = cell.column + if cell.wide { 2 } else { 1 };
        self.cells.last().is_some_and(|tail| tail.line == cell.line)
            && end + HARD_WRAP_EDGE_SLACK >= columns
            && is_path_char(self.chars[last])
    }

    /// 去掉缩进之后以路径字符开头 —— 可能是上一行被 TUI 折下来的后半截。
    fn starts_with_path_char(&self) -> bool {
        self.content_bounds()
            .is_some_and(|(first, _)| is_path_char(self.chars[first]))
    }

    fn top_line(&self) -> Option<i32> {
        self.cells.first().map(|cell| cell.line)
    }

    fn bottom_line(&self) -> Option<i32> {
        self.cells.last().map(|cell| cell.line)
    }

    fn push_slice(&mut self, other: &LogicalLine, range: Range<usize>) {
        self.chars.extend_from_slice(&other.chars[range.clone()]);
        self.cells.extend_from_slice(&other.cells[range]);
    }
}

impl TerminalEmulator {
    /// `line`(grid 行号)所在的整条逻辑行。行号越界返回 `None`。
    pub fn logical_line_at(&self, line: i32) -> Option<LogicalLine> {
        logical_line(self.term.lock().grid(), line)
    }

    /// 鼠标对着 grid 的 `(line, column)` 时的路径候选,连同候选下标所指的那条行。
    ///
    /// 先按终端软换行拼整行;路径贴着行首 / 行尾时,再试着接上 TUI 自己折开的上下行
    /// (见模块注释「硬换行」)。接上的候选排前面,没接的垫底 —— 接错了由宿主验盘淘汰。
    /// 整个过程只拿一次 grid 锁。
    pub fn path_candidates_at(
        &self,
        line: i32,
        column: usize,
    ) -> Option<(LogicalLine, Vec<PathCandidate>)> {
        let term = self.term.lock();
        let grid = term.grid();
        let columns = grid.columns();
        let current = logical_line(grid, line)?;
        let index = current.index_at(line, column)?;
        // 鼠标不在路径字符上就不必往上下找了。反过来不成立:`:20` 这种只剩行号的
        // 后半截自己认不出候选,接上前半截才是完整的 `…lib.rs:20`
        if !current.chars.get(index).is_some_and(|&c| is_path_char(c)) {
            return Some((current, Vec::new()));
        }
        let plain = path_candidates(&current.chars, index);

        // 往上:当前行以路径字符开头,上一行写满且以路径字符收尾
        let mut before: Vec<LogicalLine> = Vec::new();
        let mut head = current.top_line()?;
        let mut head_continues = current.starts_with_path_char();
        while head_continues && before.len() < MAX_HARD_WRAP_ROWS {
            let Some(prev) = logical_line(grid, head - 1) else {
                break;
            };
            if !prev.runs_to_edge(columns) {
                break;
            }
            head = prev.top_line()?;
            head_continues = prev.starts_with_path_char();
            before.push(prev);
        }
        // 往下:当前行写满且以路径字符收尾,下一行以路径字符开头
        let mut after: Vec<LogicalLine> = Vec::new();
        let mut tail = current.bottom_line()?;
        let mut tail_continues = current.runs_to_edge(columns);
        while tail_continues && after.len() < MAX_HARD_WRAP_ROWS {
            let Some(next) = logical_line(grid, tail + 1) else {
                break;
            };
            if !next.starts_with_path_char() {
                break;
            }
            tail = next.bottom_line()?;
            tail_continues = next.runs_to_edge(columns);
            after.push(next);
        }
        drop(term);
        if before.is_empty() && after.is_empty() {
            return Some((current, plain));
        }

        // 拼:接缝两侧的空白都去掉 —— 上一行的行尾空白、下一行 TUI 补的对齐缩进
        let mut joined = LogicalLine::default();
        for prev in before.iter().rev() {
            let (first, last) = prev.content_bounds()?;
            joined.push_slice(prev, first..last + 1);
        }
        let (cur_first, cur_last) = current.content_bounds()?;
        let cur_from = if before.is_empty() { 0 } else { cur_first };
        let cur_to = if after.is_empty() {
            current.chars.len()
        } else {
            cur_last + 1
        };
        if !(cur_from..cur_to).contains(&index) {
            return Some((current, plain));
        }
        let offset = joined.chars.len();
        joined.push_slice(&current, cur_from..cur_to);
        let cur_end = joined.chars.len();
        for (i, next) in after.iter().enumerate() {
            let (first, last) = next.content_bounds()?;
            let to = if i + 1 == after.len() {
                next.chars.len()
            } else {
                last + 1
            };
            joined.push_slice(next, first..to);
        }

        let shift = |i: usize| offset + i - cur_from;
        // 接上的只留真跨过接缝的;没跨的与「没接」那份重复
        let mut out: Vec<PathCandidate> = path_candidates(&joined.chars, shift(index))
            .into_iter()
            .filter(|cand| cand.range.start < offset || cand.range.end > cur_end)
            .collect();
        for cand in plain {
            let shifted = PathCandidate {
                range: shift(cand.range.start)..shift(cand.range.end),
                ..cand
            };
            if !out.contains(&shifted) {
                out.push(shifted);
            }
        }
        Some((joined, out))
    }
}

/// `line` 所在的整条逻辑行(顺着 `WRAPLINE` 往上下拼)。行号越界返回 `None`。
fn logical_line(grid: &Grid<Cell>, line: i32) -> Option<LogicalLine> {
    let top = -(grid.history_size() as i32);
    let bottom = grid.screen_lines() as i32 - 1;
    let columns = grid.columns();
    if columns == 0 || line < top || line > bottom {
        return None;
    }
    // 行尾那一格带 WRAPLINE = 这一行是被终端折过去的,下一行接着它
    let wraps = |l: i32| {
        grid[Line(l)][Column(columns - 1)]
            .flags
            .contains(Flags::WRAPLINE)
    };
    let mut start = line;
    while start > top && line - start < MAX_WRAPPED_ROWS && wraps(start - 1) {
        start -= 1;
    }
    let mut end = line;
    while end < bottom && end - line < MAX_WRAPPED_ROWS && wraps(end) {
        end += 1;
    }

    let mut out = LogicalLine::default();
    for l in start..=end {
        let row = &grid[Line(l)];
        for col in 0..columns {
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            out.chars.push(cell.c);
            out.cells.push(LineCell {
                line: l,
                column: col,
                wide: cell.flags.contains(Flags::WIDE_CHAR),
            });
        }
    }
    Some(out)
}

/// 一个「可能是文件」的片段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathCandidate {
    /// 路径正文,行号后缀已剥掉。可能是相对路径,也可能是别的系统的写法
    /// (WSL 里的 `/home/...`)—— 解释权在宿主。
    pub path: String,
    /// 1 起算的行号。写成 `:0` 的当没写。
    pub line: Option<u32>,
    /// 范围写法(`:20-35`、`#L5-L9`)的终点行,宿主拿它选中这几行。没写范围、
    /// 或终点比起点小时为 `None`。
    pub end_line: Option<u32>,
    pub column: Option<u32>,
    /// 下划线该盖住哪些字符(含行号后缀),是 [`LogicalLine::chars`] 的下标。
    pub range: Range<usize>,
}

/// 能出现在路径里的字符。
///
/// 用白名单而不是黑名单:终端里贴着路径的东西五花八门 —— 引号、反引号、括号、
/// Claude Code 的 `│` 边框与 `⎿` 引线、中文的「，。：」—— 白名单天然把它们
/// 全挡在外面。代价是带空格 / 逗号 / 括号的路径认不全,与 VS Code 终端同一取舍。
///
/// `[` `]` 收进来是为了 Next.js 的 `app/[slug]/page.tsx`;Markdown 链接
/// `[a.ts](a.ts)` 那种包在外面的方括号由 [`trim_edges`] 剥掉。
fn is_path_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(
            c,
            '/' | '\\' | '.' | '_' | '-' | '~' | ':' | '+' | '@' | '#' | '[' | ']'
        )
}

/// 鼠标对着 `chars[index]` 时,它所在的那段文字可能指向哪些文件。最像的排最前,
/// 不像路径就是空的。
pub fn path_candidates(chars: &[char], index: usize) -> Vec<PathCandidate> {
    if !chars.get(index).is_some_and(|&c| is_path_char(c)) {
        return Vec::new();
    }
    let mut start = index;
    while start > 0 && is_path_char(chars[start - 1]) {
        start -= 1;
        if index - start > MAX_PATH_CHARS {
            return Vec::new();
        }
    }
    let mut end = index + 1;
    while end < chars.len() && is_path_char(chars[end]) {
        end += 1;
        if end - start > MAX_PATH_CHARS {
            return Vec::new();
        }
    }
    let (mut start, end) = trim_edges(chars, start, end);
    if start >= end {
        return Vec::new();
    }

    // 网址不归这里管;`file://` 剥成本地路径(`file:///D:/a` 连第三道斜杠一起剥)
    let token = &chars[start..end];
    if let Some(scheme_end) = find_seq(token, &[':', '/', '/']) {
        let scheme: String = token[..scheme_end].iter().collect();
        if !scheme.eq_ignore_ascii_case("file") {
            return Vec::new();
        }
        start += scheme_end + 3;
        let rest = &chars[start..end];
        if rest.len() >= 3 && rest[0] == '/' && rest[1].is_ascii_alphabetic() && rest[2] == ':' {
            start += 1;
        }
        if start >= end {
            return Vec::new();
        }
    }

    let token = &chars[start..end];
    let mut path_end = end;
    let mut loc = Location::default();
    let mut range_end = end;

    if let Some((hash, l)) = github_anchor(token) {
        path_end = start + hash;
        loc = l;
    } else if let Some((stripped, l)) = split_colon_suffix(token) {
        path_end = start + stripped;
        loc = l;
    }
    // 片段里没写行号:再往后看一眼 TypeScript 的 `(行,列)` 与 Python 的 `", line N`
    if path_end == end {
        if let Some((l, c, consumed)) = paren_suffix(&chars[end..]) {
            loc.line = l;
            loc.column = c;
            range_end = end + consumed;
        } else if let Some(l) = python_line_suffix(&chars[end..]) {
            loc.line = l;
        }
    }
    let Location {
        line,
        end_line,
        column,
    } = loc;
    if index >= range_end || index < start {
        return Vec::new();
    }

    let path: String = chars[start..path_end].iter().collect();
    if !path.chars().any(char::is_alphabetic) {
        return Vec::new();
    }

    let mut out = vec![PathCandidate {
        path: path.clone(),
        line,
        end_line,
        column,
        range: start..range_end,
    }];
    // 次选一:Claude Code 的 `@src/a.ts` 文件提及写法 —— 先按字面找,找不到再去掉 `@`
    if let Some(rest) = path.strip_prefix('@')
        && rest.chars().any(char::is_alphabetic)
    {
        out.push(PathCandidate {
            path: rest.to_string(),
            line,
            end_line,
            column,
            range: start + 1..range_end,
        });
    }
    // 次选二:Markdown 的标题锚点 `doc.md#install`(`#L12` 已在上面当行号拆过)
    if let Some(hash) = path.rfind('#')
        && hash > 0
    {
        let anchorless = &path[..hash];
        out.push(PathCandidate {
            path: anchorless.to_string(),
            line,
            end_line,
            column,
            range: start..start + anchorless.chars().count(),
        });
    }
    out
}

/// 从路径后缀里拆出来的位置。
#[derive(Clone, Copy, Debug, Default)]
struct Location {
    line: Option<u32>,
    end_line: Option<u32>,
    column: Option<u32>,
}

/// 剥掉粘在片段两头、不属于路径的标点:句末的 `.` / `:`、Markdown 方括号、
/// 半边括号。
fn trim_edges(chars: &[char], mut start: usize, mut end: usize) -> (usize, usize) {
    while start < end {
        let first = chars[start];
        let last = chars[end - 1];
        let opens = chars[start..end].iter().filter(|&&c| c == '[').count();
        let closes = chars[start..end].iter().filter(|&&c| c == ']').count();
        if matches!(last, '.' | ':') {
            end -= 1;
        } else if first == ':' {
            start += 1;
        } else if first == '[' && last == ']' && end - start >= 2 {
            start += 1;
            end -= 1;
        } else if last == ']' && closes > opens {
            end -= 1;
        } else if first == '[' && opens > closes {
            start += 1;
        } else {
            break;
        }
    }
    (start, end)
}

fn find_seq(haystack: &[char], needle: &[char]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 连续数字 → 数值。`0` 当没写(行号从 1 起)。
fn digits_value(digits: &[char]) -> Option<u32> {
    let text: String = digits.iter().collect();
    text.parse::<u32>().ok().filter(|&n| n > 0)
}

/// 范围的终点:比起点小(或起点没写)的当没写。
fn range_end(start: Option<u32>, end: Option<u32>) -> Option<u32> {
    end.filter(|&end| start.is_some_and(|start| end >= start))
}

/// `20` 或 `20-35`,返回 `(起点, 终点)`(终点口径见 [`range_end`])。
/// 不是这两种形状返回 `None`。
fn number_or_range(seg: &[char]) -> Option<(Option<u32>, Option<u32>)> {
    let digits = seg.iter().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let start = digits_value(&seg[..digits]);
    let rest = &seg[digits..];
    if rest.is_empty() {
        return Some((start, None));
    }
    let ok = rest[0] == '-' && rest.len() > 1 && rest[1..].iter().all(char::is_ascii_digit);
    ok.then(|| (start, range_end(start, digits_value(&rest[1..]))))
}

/// 从尾部剥 `:行` / `:行:列`(各自允许写成 `起-止`:行的终点留作选区,列的终点不要)。
///
/// 返回 `(路径结束下标, 位置)`;没有后缀返回 `None`。
/// `D:\a.ts` 的盘符冒号后面不是数字,不会被误剥。
fn split_colon_suffix(token: &[char]) -> Option<(usize, Location)> {
    let mut end = token.len();
    let mut numbers: Vec<(Option<u32>, Option<u32>)> = Vec::new();
    while numbers.len() < 2 {
        let Some(colon) = token[..end].iter().rposition(|&c| c == ':') else {
            break;
        };
        if colon == 0 {
            break;
        }
        let Some(n) = number_or_range(&token[colon + 1..end]) else {
            break;
        };
        numbers.push(n);
        end = colon;
    }
    let ((line, end_line), column) = match numbers.as_slice() {
        [] => return None,
        [line] => (*line, None),
        [column, line, ..] => (*line, column.0),
    };
    Some((
        end,
        Location {
            line,
            end_line,
            column,
        },
    ))
}

/// GitHub 风格的 `#L12` / `#L12C5` / `#L12-L20`。返回 `(# 的下标, 位置)`。
fn github_anchor(token: &[char]) -> Option<(usize, Location)> {
    let hash = token.iter().rposition(|&c| c == '#')?;
    if hash == 0 || token.get(hash + 1) != Some(&'L') {
        return None;
    }
    let rest = &token[hash + 2..];
    let line_digits = rest.iter().take_while(|c| c.is_ascii_digit()).count();
    if line_digits == 0 {
        return None;
    }
    let line = digits_value(&rest[..line_digits]);
    let mut tail = &rest[line_digits..];
    let mut column = None;
    if tail.first() == Some(&'C') {
        let col_digits = tail[1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if col_digits == 0 {
            return None;
        }
        column = digits_value(&tail[1..1 + col_digits]);
        tail = &tail[1 + col_digits..];
    }
    // 范围的终点(`-L20` / `-20` / `-L20C3`):形状照旧宽松地认,终点行留作选区,
    // 终点的列不要
    if !tail.is_empty()
        && !(tail[0] == '-'
            && tail.len() > 1
            && tail[1..]
                .iter()
                .all(|c| c.is_ascii_digit() || matches!(c, 'L' | 'C')))
    {
        return None;
    }
    let end_line = tail.get(1..).and_then(|end| {
        let end = end.strip_prefix(&['L']).unwrap_or(end);
        let digits = end.iter().take_while(|c| c.is_ascii_digit()).count();
        range_end(line, digits_value(&end[..digits]))
    });
    Some((
        hash,
        Location {
            line,
            end_line,
            column,
        },
    ))
}

/// 紧跟在路径后面的 `(行)` / `(行,列)` / `(行, 列)`(tsc / MSBuild 的写法)。
/// 返回 `(行, 列, 吃掉的字符数)`。
fn paren_suffix(after: &[char]) -> Option<(Option<u32>, Option<u32>, usize)> {
    if after.first() != Some(&'(') {
        return None;
    }
    let close = after.iter().take(24).position(|&c| c == ')')?;
    let inner = &after[1..close];
    let mut parts = inner.split(|&c| c == ',');
    let line_part = parts.next()?;
    if line_part.is_empty() || !line_part.iter().all(char::is_ascii_digit) {
        return None;
    }
    let line = digits_value(line_part);
    let column = match parts.next() {
        None => None,
        Some(part) => {
            let trimmed: Vec<char> = part.iter().copied().skip_while(|c| *c == ' ').collect();
            if trimmed.is_empty() || !trimmed.iter().all(char::is_ascii_digit) {
                return None;
            }
            digits_value(&trimmed)
        }
    };
    if parts.next().is_some() {
        return None;
    }
    Some((line, column, close + 1))
}

/// Python 回溯的 `File "x.py", line 20` —— 路径在引号里,行号在引号外。
fn python_line_suffix(after: &[char]) -> Option<Option<u32>> {
    const MARK: [char; 7] = [',', ' ', 'l', 'i', 'n', 'e', ' '];
    if !matches!(after.first(), Some('"' | '\'')) || after.get(1..8) != Some(&MARK[..]) {
        return None;
    }
    let digits = after[8..].iter().take_while(|c| c.is_ascii_digit()).count();
    (digits > 0).then(|| digits_value(&after[8..8 + digits]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TermSize;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    /// 鼠标对着 `needle` 的第一个字符时的首选候选。
    fn first_at(text: &str, needle: &str) -> Option<PathCandidate> {
        let chars = chars(text);
        let byte = text.find(needle).expect("needle 不在文本里");
        let index = text[..byte].chars().count();
        path_candidates(&chars, index).into_iter().next()
    }

    fn covered(text: &str, cand: &PathCandidate) -> String {
        chars(text)[cand.range.clone()].iter().collect()
    }

    #[test]
    fn windows_绝对路径带行号() {
        let text = r"见 D:\Git\bhyt\onlinev2-gui\src\utils\units.ts:20 这一行";
        let cand = first_at(text, "units").unwrap();
        assert_eq!(cand.path, r"D:\Git\bhyt\onlinev2-gui\src\utils\units.ts");
        assert_eq!(cand.line, Some(20));
        assert_eq!(cand.column, None);
        assert_eq!(
            covered(text, &cand),
            r"D:\Git\bhyt\onlinev2-gui\src\utils\units.ts:20",
            "下划线要连行号一起盖住"
        );
        // 鼠标停在盘符上也认得出同一个
        assert_eq!(first_at(text, "D:").unwrap(), cand);
    }

    #[test]
    fn 行列号与范围写法() {
        let cand = first_at("  --> src/main.rs:10:5", "src").unwrap();
        assert_eq!(
            (cand.path.as_str(), cand.line, cand.column),
            ("src/main.rs", Some(10), Some(5))
        );

        let cand = first_at("./main.go:12:3: undefined: x", "main").unwrap();
        assert_eq!(
            (cand.path.as_str(), cand.line, cand.column),
            ("./main.go", Some(12), Some(3))
        );

        let cand = first_at("改动见 `src/a.ts:20-35`", "src").unwrap();
        assert_eq!(
            (cand.path.as_str(), cand.line, cand.end_line),
            ("src/a.ts", Some(20), Some(35))
        );

        // 单行没有终点;从第 1 行起的范围照样认
        assert_eq!(first_at("a.ts:20", "a.ts").unwrap().end_line, None);
        let cand = first_at("D:\\proj\\a.rs:1-222", "proj").unwrap();
        assert_eq!((cand.line, cand.end_line), (Some(1), Some(222)));

        // 行范围 + 列:终点留给行,列的范围只取起点
        let cand = first_at("a.ts:10-20:5-9", "a.ts").unwrap();
        assert_eq!(
            (cand.line, cand.end_line, cand.column),
            (Some(10), Some(20), Some(5))
        );

        // 终点比起点小的当没写范围(起点照用)
        let cand = first_at("a.ts:30-20", "a.ts").unwrap();
        assert_eq!((cand.line, cand.end_line), (Some(30), None));

        // 行号写 0 当没写
        let cand = first_at("a.ts:0", "a.ts").unwrap();
        assert_eq!((cand.path.as_str(), cand.line), ("a.ts", None));
        let cand = first_at("a.ts:0-5", "a.ts").unwrap();
        assert_eq!(
            (cand.line, cand.end_line),
            (None, None),
            "起点没写就没有范围"
        );
    }

    #[test]
    fn tsc_括号行列号() {
        let text = "src/a.ts(20,5): error TS2322";
        let cand = first_at(text, "src").unwrap();
        assert_eq!(
            (cand.path.as_str(), cand.line, cand.column),
            ("src/a.ts", Some(20), Some(5))
        );
        assert_eq!(covered(text, &cand), "src/a.ts(20,5)");

        let cand = first_at("x.cs(7, 12)", "x.cs").unwrap();
        assert_eq!((cand.line, cand.column), (Some(7), Some(12)));
    }

    /// Claude Code 的工具调用 `Read(src/a.ts)`:括号里不是数字,不当行号吃
    #[test]
    fn 工具调用括号里的路径() {
        let text = "● Read(src/utils/units.ts)";
        let cand = first_at(text, "src").unwrap();
        assert_eq!(cand.path, "src/utils/units.ts");
        assert_eq!(cand.line, None);
        assert_eq!(covered(text, &cand), "src/utils/units.ts");
    }

    #[test]
    fn python_回溯行号在引号外() {
        let text = r#"  File "C:\proj\app.py", line 42, in <module>"#;
        let cand = first_at(text, "proj").unwrap();
        assert_eq!(
            (cand.path.as_str(), cand.line),
            (r"C:\proj\app.py", Some(42))
        );
        assert_eq!(covered(text, &cand), r"C:\proj\app.py");
    }

    #[test]
    fn github_锚点与标题锚点() {
        let cand = first_at("docs/a.md#L12C3", "docs").unwrap();
        assert_eq!(
            (cand.path.as_str(), cand.line, cand.column),
            ("docs/a.md", Some(12), Some(3))
        );
        let cand = first_at("src/x.rs#L5-L9", "src").unwrap();
        assert_eq!(
            (cand.path.as_str(), cand.line, cand.end_line),
            ("src/x.rs", Some(5), Some(9))
        );
        assert_eq!(first_at("x.rs#L5-9", "x.rs").unwrap().end_line, Some(9));
        assert_eq!(
            first_at("x.rs#L5C2-L9C4", "x.rs").unwrap().end_line,
            Some(9)
        );
        assert_eq!(first_at("x.rs#L12", "x.rs").unwrap().end_line, None);

        let text = "docs/guide.md#install";
        let all = path_candidates(&chars(text), 0);
        let paths: Vec<_> = all.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["docs/guide.md#install", "docs/guide.md"]);
        assert_eq!(covered(text, &all[1]), "docs/guide.md");
    }

    #[test]
    fn at_提及先字面后去前缀() {
        let all = path_candidates(&chars("@src/a.ts"), 3);
        let paths: Vec<_> = all.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["@src/a.ts", "src/a.ts"]);
        assert_eq!(all[1].range, 1..9);
    }

    #[test]
    fn 剥掉两头的标点与框线() {
        assert_eq!(
            first_at("修改了 src/a.ts。", "src").unwrap().path,
            "src/a.ts"
        );
        assert_eq!(
            first_at("见：src/a.ts，然后", "src").unwrap().path,
            "src/a.ts"
        );
        assert_eq!(first_at("│ src/a.ts │", "src").unwrap().path, "src/a.ts");
        assert_eq!(first_at("see src/a.ts.", "src").unwrap().path, "src/a.ts");
        assert_eq!(
            first_at("**README.md**", "README").unwrap().path,
            "README.md"
        );
        assert_eq!(
            first_at("[src/a.ts](src/a.ts)", "src").unwrap().path,
            "src/a.ts"
        );
        assert_eq!(first_at("(see src/a.ts)", "src").unwrap().path, "src/a.ts");
    }

    #[test]
    fn nextjs_动态路由的方括号保留() {
        assert_eq!(
            first_at("app/[slug]/page.tsx", "page").unwrap().path,
            "app/[slug]/page.tsx"
        );
        assert_eq!(
            first_at("pages/[id].tsx", "pages").unwrap().path,
            "pages/[id].tsx"
        );
    }

    #[test]
    fn 网址不认_file_协议剥成本地路径() {
        assert!(first_at("https://example.com/a.ts", "example").is_none());
        let cand = first_at("file:///D:/proj/a.ts:3", "proj").unwrap();
        assert_eq!((cand.path.as_str(), cand.line), ("D:/proj/a.ts", Some(3)));
        assert_eq!(
            first_at("file:///home/u/a.rs", "home").unwrap().path,
            "/home/u/a.rs"
        );
    }

    #[test]
    fn 不像路径的不给候选() {
        assert!(path_candidates(&chars("a b"), 1).is_empty(), "空白");
        assert!(first_at("共 1234 行", "1234").is_none(), "纯数字");
        assert!(first_at("cd ..", "..").is_none(), "只有点");
        // 句末的点不算路径的一部分,停在那颗点上不该亮
        assert!(path_candidates(&chars("a.ts."), 4).is_empty());
    }

    #[test]
    fn 逻辑行把软换行拼回来() {
        let e = TerminalEmulator::new(TermSize::new(10, 4));
        e.advance(b"src/abc/def.ts:7");
        let line = e.logical_line_at(1).expect("第二段物理行");
        let text: String = line.chars.iter().collect();
        assert!(text.starts_with("src/abc/def.ts:7"), "实际 {text:?}");
        assert_eq!(
            line.cells[0],
            LineCell {
                line: 0,
                column: 0,
                wide: false
            }
        );
        assert_eq!(
            line.cells[10],
            LineCell {
                line: 1,
                column: 0,
                wide: false
            }
        );

        // 鼠标在第二段上照样拿到整条路径
        let index = line.index_at(1, 2).unwrap();
        let cand = path_candidates(&line.chars, index)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!((cand.path.as_str(), cand.line), ("src/abc/def.ts", Some(7)));

        // 没被折的行只有它自己
        e.advance(b"\r\nx");
        let short = e.logical_line_at(2).unwrap();
        assert_eq!(short.cells.len(), 10);
        assert!(short.cells.iter().all(|c| c.line == 2));
        assert!(e.logical_line_at(4).is_none(), "越过屏底");
    }

    fn paths_at(e: &TerminalEmulator, line: i32, column: usize) -> Vec<String> {
        e.path_candidates_at(line, column)
            .map(|(_, cands)| cands.into_iter().map(|c| c.path).collect())
            .unwrap_or_default()
    }

    /// Claude Code 的排版:路径写到右边缘被硬断,下一行缩进两格接着写(用户截图那种)。
    /// 鼠标停在上下哪一截都认出整条路径,下划线跨两行。
    #[test]
    fn tui_硬换行折开的路径接回来() {
        let e = TerminalEmulator::new(TermSize::new(24, 4));
        // 「  see：」占 7 列,D 落在第 7 列,行尾 crates 写到第 21 列
        e.advance("  see：D:\\proj\\crates\r\n  \\src\\lib.rs:7".as_bytes());
        for (line, column) in [(1, 4), (0, 9)] {
            let (joined, cands) = e.path_candidates_at(line, column).unwrap();
            let first = &cands[0];
            assert_eq!(
                (first.path.as_str(), first.line),
                (r"D:\proj\crates\src\lib.rs", Some(7)),
                "鼠标在 ({line},{column})"
            );
            let covered = &joined.cells[first.range.clone()];
            assert_eq!(covered.first().map(|c| (c.line, c.column)), Some((0, 7)));
            assert_eq!(
                covered.last().map(|c| c.line),
                Some(1),
                "下划线要盖到第二行"
            );
        }
        // 没接的那一截垫底(接错时宿主落到它)
        assert!(paths_at(&e, 1, 4).contains(&r"\src\lib.rs".to_string()));
    }

    /// 盘符刚好断在行尾(截图第二行:`D:\` / `Git\...`),以及只剩行号的后半截。
    #[test]
    fn 断在盘符后与断在行号前() {
        let e = TerminalEmulator::new(TermSize::new(16, 6));
        e.advance("  abc def D:\\\r\n  Git\\a.rs\r\n".as_bytes());
        assert_eq!(paths_at(&e, 1, 4)[0], r"D:\Git\a.rs");

        e.advance("  abcdefg x.rs\r\n  :20 tail".as_bytes());
        let (_, cands) = e.path_candidates_at(3, 2).unwrap();
        assert_eq!((cands[0].path.as_str(), cands[0].line), ("x.rs", Some(20)));
    }

    /// 一行一个路径的短列表不贴右边缘,不接。
    #[test]
    fn 短行不接() {
        let e = TerminalEmulator::new(TermSize::new(40, 4));
        e.advance(b"  src/a.ts\r\n  src/b.ts");
        assert_eq!(paths_at(&e, 0, 3), ["src/a.ts"]);
        assert_eq!(paths_at(&e, 1, 3), ["src/b.ts"]);
    }

    /// 散文在词边界被折开:接出来的排前面,没接的垫底,宿主验盘时落到后者。
    #[test]
    fn 词边界折行时没接的候选垫底() {
        let e = TerminalEmulator::new(TermSize::new(20, 4));
        e.advance(b"aaaa bbbb cccc file\r\n  src/a.ts ok");
        assert_eq!(paths_at(&e, 1, 3), ["filesrc/a.ts", "src/a.ts"]);
    }

    #[test]
    fn 宽字符右半格算它本身() {
        let e = TerminalEmulator::new(TermSize::new(20, 2));
        e.advance("改 a.ts".as_bytes());
        let line = e.logical_line_at(0).unwrap();
        assert_eq!(line.chars[0], '改');
        assert!(line.cells[0].wide);
        assert_eq!(line.index_at(0, 1), Some(0), "spacer 格归到宽字符");
        // 宽字符之后的列号照实记(空格在第 2 列,a 在第 3 列)
        assert_eq!(line.index_at(0, 3), Some(2));
        assert_eq!(line.chars[2], 'a');
    }
}
