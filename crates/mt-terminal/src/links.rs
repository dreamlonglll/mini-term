//! 终端输出里的链接识别 —— 按住 Ctrl(macOS 上 ⌘)悬停 / 单击打开文件或网址用。
//!
//! 全是纯逻辑,都不碰 gpui:
//!
//! - [`TerminalEmulator::logical_line_at`] 顺着软换行(`WRAPLINE`)把被终端折开的
//!   几段物理行拼回一整行,每个字符记住自己落在哪一格 —— 长路径被折成两行时照样
//!   认得出,下划线也能跨行画。
//! - [`path_candidates`] 在一行里找「鼠标所在那个字符」所属的路径片段,把
//!   `:行:列` / `(行,列)` / `#L行` / Python 的 `", line N` 这些行号写法一并拆出来。
//! - [`TerminalEmulator::url_at`] 回答鼠标底下有没有网址,见下面「网址」一节。
//!
//! 路径这边**只看形状**。存不存在、在不在项目里、相对路径对着哪个目录解析,全是宿主的事
//! (mt-app 的 `terminal_links`)—— 所以给出的是一串候选(最像的排在前面),宿主
//! 挨个去盘上验,第一个成立的才算链接。入口是 [`TerminalEmulator::path_candidates_at`]。
//!
//! # 网址
//!
//! 两个来源,**OSC 8 优先**:
//!
//! - **OSC 8**(`ESC]8;;URI ESC\ 文字 ESC]8;; ESC\`):目标是程序明说的,落在 cell 上
//!   ([`Cell::hyperlink`])。只开 http / https。别的 scheme(`file:`、`javascript:`……)
//!   不亮不开,也不再把它的显示文字当网址认 —— 文字与目标对不上正是伪装的样子;文字
//!   要是像项目里的文件路径,照旧走上面那条验盘的路(`ls --hyperlink` 这类程序给路径
//!   包的 `file:` 链接,不能因此把原有的文件链接弄没了)。
//! - **普通文本**([`url_candidate`]):认 `http://` / `https://` 开头、不含非 ASCII 字符的
//!   一段,边界口径见那边。与路径互不抢:路径识别遇到带 scheme 的片段一律不给候选
//!   (`file://` 除外,而网址这边不认 `file://`)。
//!
//! OSC 8 的显示文字可以写成任何样子,所以 [`UrlLink::from_osc8`] 为真时宿主必须把真实
//! 目标亮给人看。
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
//!
//! 网址复用同一套走法,但**没有盘可验**,接错就是开错页面,所以判据收得更紧(见
//! [`URL_WRAP`]):行尾要真贴着右边缘,下一行开头还得像网址的后半截。

use std::collections::VecDeque;
use std::ops::Range;

use alacritty_terminal::grid::{Dimensions as _, Grid};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};

use crate::TerminalEmulator;

/// 往上 / 往下各最多拼多少段软换行。再长的「一行」多半是日志刷屏,不是路径。
const MAX_WRAPPED_ROWS: i32 = 16;
/// 往上 / 往下各最多接几段 TUI 自己折开的行(硬换行)。
const MAX_HARD_WRAP_ROWS: usize = 2;
/// 网址的版本:登录链接这类长网址在 80 列里能折成好几行,而每一行都要过 [`URL_WRAP`]
/// 那道判据,多看几行不会多接错。
const MAX_URL_HARD_WRAP_ROWS: usize = 8;
/// 行尾离右边缘不超过这么多列,才算「写满了、被 TUI 折到下一行」。
///
/// 留的是 TUI 的右边距(Claude Code 一两列)与「宽字符放不下、行尾空一格」。
const HARD_WRAP_EDGE_SLACK: usize = 6;
/// 网址的版本:只留 TUI 右边距那一两列。接错没有盘上校验兜底,宁可少接。
const URL_HARD_WRAP_EDGE_SLACK: usize = 2;
/// 认作路径的片段最多多少个字符。超了直接不认,免得在一整屏 base64 上逐字扫。
const MAX_PATH_CHARS: usize = 1024;
/// 认作网址的片段最多多少个字符(OSC 8 给的目标同样受这个限)。
const MAX_URL_CHARS: usize = 2048;
/// alacritty 给没写 `id=` 的 OSC 8 链接自动生成的 id 的后缀(`cell.rs` 的 `HyperlinkInner`)。
const AUTO_LINK_ID_SUFFIX: &str = "_alacritty";

/// 硬换行怎么接:路径与网址同一套走法,只有判据不同。
struct HardWrapRule {
    /// 接缝两侧该是哪类字符。
    is_char: fn(char) -> bool,
    /// 行尾离右边缘不超过这么多列才算写满了。
    edge_slack: usize,
    /// 这一行开头(去掉缩进)像不像上一行被折下来的后半截。
    continues: fn(&LogicalLine) -> bool,
    /// 往上 / 往下各最多接几段。
    max_rows: usize,
}

const PATH_WRAP: HardWrapRule = HardWrapRule {
    is_char: is_path_char,
    edge_slack: HARD_WRAP_EDGE_SLACK,
    continues: LogicalLine::starts_with_path_char,
    max_rows: MAX_HARD_WRAP_ROWS,
};

const URL_WRAP: HardWrapRule = HardWrapRule {
    is_char: is_url_char,
    edge_slack: URL_HARD_WRAP_EDGE_SLACK,
    continues: LogicalLine::looks_like_url_tail,
    max_rows: MAX_URL_HARD_WRAP_ROWS,
};

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

    /// 内容写到了最后一个物理行的右边缘附近、收尾是接得上的字符 —— 像是被 TUI 折下去了。
    fn runs_to_edge(&self, columns: usize, rule: &HardWrapRule) -> bool {
        let Some((_, last)) = self.content_bounds() else {
            return false;
        };
        let cell = self.cells[last];
        let end = cell.column + if cell.wide { 2 } else { 1 };
        self.cells.last().is_some_and(|tail| tail.line == cell.line)
            && end + rule.edge_slack >= columns
            && (rule.is_char)(self.chars[last])
    }

    /// 去掉缩进之后以路径字符开头 —— 可能是上一行被 TUI 折下来的后半截。
    fn starts_with_path_char(&self) -> bool {
        self.content_bounds()
            .is_some_and(|(first, _)| is_path_char(self.chars[first]))
    }

    /// 去掉缩进之后开头那个词像网址的后半截。
    ///
    /// 散文在词边界被折行时下一行开头是普通单词(`and`),网址没有盘可验,接上去就是
    /// 开错页面,所以光「是网址字符」不够:要带数字或网址的结构字符;或者它独占这一行
    /// (长网址最后一截经常是几个字母)。自带 scheme 的是下一条网址,不是后半截。
    fn looks_like_url_tail(&self) -> bool {
        let Some((first, _)) = self.content_bounds() else {
            return false;
        };
        let end = (first..self.chars.len())
            .find(|&i| !is_url_char(self.chars[i]))
            .unwrap_or(self.chars.len());
        let word = &self.chars[first..end];
        if scheme_len_at(word, 0).is_some() {
            return false;
        }
        let trimmed = word.len() - trailing_url_punct(word);
        if trimmed == 0 {
            return false;
        }
        let alone = trimmed == word.len() && self.chars[end..].iter().all(|c| c.is_whitespace());
        alone
            || word[..trimmed].iter().any(|c| {
                c.is_ascii_digit()
                    || matches!(
                        c,
                        '/' | '?' | '=' | '&' | '%' | '#' | '-' | '_' | '.' | '~' | '+' | ':' | '@'
                    )
            })
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
        let current = logical_line(grid, line)?;
        let index = current.index_at(line, column)?;
        // 鼠标不在路径字符上就不必往上下找了。反过来不成立:`:20` 这种只剩行号的
        // 后半截自己认不出候选,接上前半截才是完整的 `…lib.rs:20`
        if !current.chars.get(index).is_some_and(|&c| is_path_char(c)) {
            return Some((current, Vec::new()));
        }
        let plain = path_candidates(&current.chars, index);

        let (before, after) = hard_wrap_neighbors(grid, &current, &PATH_WRAP)?;
        drop(term);
        let Some(joined) = join_hard_wrapped(&current, &before, &after, index) else {
            return Some((current, plain));
        };

        // 接上的只留真跨过接缝的;没跨的与「没接」那份重复
        let mut out: Vec<PathCandidate> = path_candidates(&joined.line.chars, joined.shift(index))
            .into_iter()
            .filter(|cand| {
                cand.range.start < joined.current.start || cand.range.end > joined.current.end
            })
            .collect();
        for cand in plain {
            let shifted = PathCandidate {
                range: joined.shift(cand.range.start)..joined.shift(cand.range.end),
                ..cand
            };
            if !out.contains(&shifted) {
                out.push(shifted);
            }
        }
        Some((joined.line, out))
    }

    /// 鼠标对着 grid 的 `(line, column)` 时底下的网址(口径见模块注释「网址」)。
    ///
    /// `None` 不等于这里没有链接:OSC 8 的非 http(s) 目标、以及普通的文件路径都归
    /// [`Self::path_candidates_at`] 那条路接着认。
    pub fn url_at(&self, line: i32, column: usize) -> Option<UrlLink> {
        let term = self.term.lock();
        let grid = term.grid();
        if let Some(link) = hyperlink_at(grid, line, column) {
            if !is_web_url(link.uri()) {
                return None;
            }
            return Some(UrlLink {
                url: link.uri().to_string(),
                cells: hyperlink_cells(grid, line, column, &link),
                from_osc8: true,
            });
        }
        let current = logical_line(grid, line)?;
        let index = current.index_at(line, column)?;
        if !current.chars.get(index).is_some_and(|&c| is_url_char(c)) {
            return None;
        }
        let (before, after) = hard_wrap_neighbors(grid, &current, &URL_WRAP)?;
        drop(term);
        // 接得上就先按接好的认:网址贴着接缝时这才是完整的。接缝附近没有网址
        // (上一行末尾只是个碰巧贴边的词)时落回当前行自己
        join_hard_wrapped(&current, &before, &after, index)
            .and_then(|joined| plain_url_link(&joined.line, joined.shift(index)))
            .or_else(|| plain_url_link(&current, index))
    }
}

/// 顺着 TUI 硬换行找 `current` 上下接得上的行,上面的由近及远。
fn hard_wrap_neighbors(
    grid: &Grid<Cell>,
    current: &LogicalLine,
    rule: &HardWrapRule,
) -> Option<(Vec<LogicalLine>, Vec<LogicalLine>)> {
    let columns = grid.columns();
    // 往上:当前行开头接得上,上一行写满且以接得上的字符收尾
    let mut before: Vec<LogicalLine> = Vec::new();
    let mut head = current.top_line()?;
    let mut head_continues = (rule.continues)(current);
    while head_continues && before.len() < rule.max_rows {
        let Some(prev) = logical_line(grid, head - 1) else {
            break;
        };
        if !prev.runs_to_edge(columns, rule) {
            break;
        }
        head = prev.top_line()?;
        head_continues = (rule.continues)(&prev);
        before.push(prev);
    }
    // 往下:当前行写满且以接得上的字符收尾,下一行开头接得上
    let mut after: Vec<LogicalLine> = Vec::new();
    let mut tail = current.bottom_line()?;
    let mut tail_continues = current.runs_to_edge(columns, rule);
    while tail_continues && after.len() < rule.max_rows {
        let Some(next) = logical_line(grid, tail + 1) else {
            break;
        };
        if !(rule.continues)(&next) {
            break;
        }
        tail = next.bottom_line()?;
        tail_continues = next.runs_to_edge(columns, rule);
        after.push(next);
    }
    Some((before, after))
}

/// 接好的一整条,以及当前行落在里面的哪一截。
struct HardWrapJoin {
    line: LogicalLine,
    /// 当前行在 `line` 里占的下标区间。
    current: Range<usize>,
    /// 当前行从第几个字符开始接进来的(前面的缩进被剥掉了)。
    from: usize,
}

impl HardWrapJoin {
    /// 当前行的字符下标 → `line` 里的下标。
    fn shift(&self, index: usize) -> usize {
        self.current.start + index - self.from
    }
}

/// 把 [`hard_wrap_neighbors`] 找到的行拼成一条。没东西可接、或鼠标正落在拼接时要剥掉
/// 的空白上,返回 `None`。
///
/// 接缝两侧的空白都去掉 —— 上一行的行尾空白、下一行 TUI 补的对齐缩进。
fn join_hard_wrapped(
    current: &LogicalLine,
    before: &[LogicalLine],
    after: &[LogicalLine],
    index: usize,
) -> Option<HardWrapJoin> {
    if before.is_empty() && after.is_empty() {
        return None;
    }
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
        return None;
    }
    let offset = joined.chars.len();
    joined.push_slice(current, cur_from..cur_to);
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
    Some(HardWrapJoin {
        line: joined,
        current: offset..cur_end,
        from: cur_from,
    })
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

// ── 网址 ───────────────────────────────────────────────────────────────────────

/// 鼠标底下的网址。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UrlLink {
    /// 要交给浏览器的目标。普通文本就是那段文字本身,OSC 8 是程序给的 URI,原样不动。
    pub url: String,
    /// 下划线该盖住的格子,按阅读顺序。
    pub cells: Vec<LineCell>,
    /// 来自 OSC 8:显示的文字可以与目标毫不相干,宿主必须把 [`Self::url`] 亮给人看。
    pub from_osc8: bool,
}

/// 能出现在普通文本网址里的字符:可见 ASCII,去掉 RFC 3986 不允许裸写、终端里又常
/// 贴着网址的几个(引号、尖括号、反引号、反斜杠、竖线、`^`)。
///
/// 只认 ASCII 是刻意的:中文标点、汉字、Claude Code 的 `│` 边框紧贴在网址后面是常态,
/// 而真要带非 ASCII 字符的网址在终端里本来就该是编码过的。
fn is_url_char(c: char) -> bool {
    c.is_ascii_graphic() && !matches!(c, '"' | '<' | '>' | '\\' | '^' | '`' | '|')
}

/// `chars[at..]` 以 `http://` / `https://` 开头(不分大小写)时返回这段的长度。
fn scheme_len_at(chars: &[char], at: usize) -> Option<usize> {
    const HTTP: [char; 4] = ['h', 't', 't', 'p'];
    const SEPARATOR: [char; 3] = [':', '/', '/'];
    let rest = chars.get(at..)?;
    if rest.len() < HTTP.len()
        || !rest
            .iter()
            .zip(HTTP)
            .all(|(c, want)| c.eq_ignore_ascii_case(&want))
    {
        return None;
    }
    let tls = rest
        .get(HTTP.len())
        .is_some_and(|c| c.eq_ignore_ascii_case(&'s'));
    let prefix = HTTP.len() + usize::from(tls);
    rest.get(prefix..prefix + SEPARATOR.len())
        .is_some_and(|sep| sep == SEPARATOR)
        .then_some(prefix + SEPARATOR.len())
}

/// `at` 处真是一个 scheme 的开头:前一个字符不是 scheme 的合法字符。
/// `xhttp://`、`git+https://` 是别的 scheme,不算。
fn at_scheme_start(chars: &[char], at: usize) -> bool {
    at == 0 || !(chars[at - 1].is_ascii_alphanumeric() || matches!(chars[at - 1], '+' | '-' | '.'))
}

/// 从 `start`(scheme 之后)往后走到网址的原始结尾,不超过 `limit`。
///
/// 遇到没有配对的右括号就停:`(见 https://a.com)`、`[t](https://a.com)` 里的 `)` 不属于
/// 网址,而 `wiki/Foo_(bar)` 的成对括号属于。分隔符后紧跟新的 scheme 也停 —— 两条网址
/// 连写;`?to=https://…` 这种查询参数里的网址前面是 `=`,不在此列。
fn url_end(chars: &[char], start: usize, limit: usize) -> usize {
    let mut depth = [0usize; 3];
    let mut i = start;
    while i < limit {
        let c = chars[i];
        if matches!(c, ',' | ';') && scheme_len_at(&chars[..limit], i + 1).is_some() {
            break;
        }
        match c {
            '(' => depth[0] += 1,
            '[' => depth[1] += 1,
            '{' => depth[2] += 1,
            ')' | ']' | '}' => {
                let open = match c {
                    ')' => 0,
                    ']' => 1,
                    _ => 2,
                };
                if depth[open] == 0 {
                    break;
                }
                depth[open] -= 1;
            }
            _ => {}
        }
        i += 1;
    }
    i
}

/// 网址尾巴上有多少个字符不算网址:句末标点、Markdown 的粗体星号、没配对的单引号、
/// 半边左括号。没配对的右括号在 [`url_end`] 就停了,`"` `>` 根本不是网址字符。
fn trailing_url_punct(text: &[char]) -> usize {
    let mut end = text.len();
    while end > 0 {
        let strip = match text[end - 1] {
            '.' | ',' | ';' | ':' | '!' | '?' | '*' | '(' | '[' | '{' => true,
            '\'' => text[..end].iter().filter(|&&c| c == '\'').count() % 2 == 1,
            _ => false,
        };
        if !strip {
            break;
        }
        end -= 1;
    }
    text.len() - end
}

/// 鼠标对着 `chars[index]` 时,它所在的网址占哪几个字符。不在网址上是 `None`。
///
/// 边界:scheme 只认 http / https;非 ASCII 字符一律终止(中文标点紧贴在网址后面时只取到
/// 标点之前);尾部的 `.,;:!?` 与没配对的 `)]}>'"` 不算;成对括号保留(`wiki/Foo_(bar)`)。
/// 整段连续的网址字符超过 [`MAX_URL_CHARS`] 直接不认,免得在一整屏 base64 上逐字扫。
pub fn url_candidate(chars: &[char], index: usize) -> Option<Range<usize>> {
    if !chars.get(index).is_some_and(|&c| is_url_char(c)) {
        return None;
    }
    let mut run_start = index;
    while run_start > 0 && is_url_char(chars[run_start - 1]) {
        run_start -= 1;
        if index - run_start > MAX_URL_CHARS {
            return None;
        }
    }
    let mut run_end = index + 1;
    while run_end < chars.len() && is_url_char(chars[run_end]) {
        run_end += 1;
        if run_end - run_start > MAX_URL_CHARS {
            return None;
        }
    }
    // 从左往右切:查询参数里还带着网址时,鼠标在后半截也得是外面那条
    let mut pos = run_start;
    while pos < run_end {
        let start = (pos..run_end).find(|&i| {
            scheme_len_at(&chars[..run_end], i).is_some() && at_scheme_start(chars, i)
        })?;
        if start > index {
            return None;
        }
        let authority = start + scheme_len_at(&chars[..run_end], start)?;
        let end = url_end(chars, authority, run_end);
        if index < end {
            let end = end - trailing_url_punct(&chars[authority..end]);
            let has_host = end > authority
                && (chars[authority].is_ascii_alphanumeric() || chars[authority] == '[');
            return (has_host && index < end).then_some(start..end);
        }
        pos = end;
    }
    None
}

/// `line` 里鼠标对着第 `index` 个字符时的普通文本网址。
fn plain_url_link(line: &LogicalLine, index: usize) -> Option<UrlLink> {
    let range = url_candidate(&line.chars, index)?;
    Some(UrlLink {
        url: line.chars[range.clone()].iter().collect(),
        cells: line.cells[range].to_vec(),
        from_osc8: false,
    })
}

/// OSC 8 的目标能不能交给浏览器:只认 http / https,不超长,带了主机;不含空白、控制
/// 字符、不可见的格式字符(方向覆盖、零宽字符 —— 它们能让气泡里显示的与实际打开的
/// 不是一回事),ASCII 部分还要过 [`is_url_char`]。
pub fn is_web_url(url: &str) -> bool {
    let chars: Vec<char> = url.chars().collect();
    let Some(authority) = scheme_len_at(&chars, 0) else {
        return false;
    };
    chars.len() <= MAX_URL_CHARS
        && chars
            .get(authority)
            .is_some_and(|&c| c.is_alphanumeric() || c == '[')
        && chars.iter().all(|&c| {
            if c.is_ascii() {
                is_url_char(c)
            } else {
                !c.is_whitespace() && !c.is_control() && !is_invisible_format(c)
            }
        })
}

/// 不可见的 Unicode 格式字符。
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FEFF}'
    )
}

/// `(line, column)` 那一格的 OSC 8 链接。越界或没有都是 `None`。
fn hyperlink_at(grid: &Grid<Cell>, line: i32, column: usize) -> Option<Hyperlink> {
    let top = -(grid.history_size() as i32);
    let bottom = grid.screen_lines() as i32 - 1;
    if line < top || line > bottom || column >= grid.columns() {
        return None;
    }
    grid[Line(line)][Column(column)].hyperlink()
}

/// 一条 OSC 8 链接占的所有格子,按阅读顺序。上下各最多看 [`MAX_WRAPPED_ROWS`] 行。
///
/// - **写了 `id=`**:同 id 同 URI 的格子不管隔多远都是一条(OSC 8 规范的本意,程序分段
///   输出、中间隔着别的内容时就靠它);
/// - **没写**:alacritty 给每次 OSC 8 各发一个 id,TUI 自己换行时一条链接的每段 id
///   都不同,只能看位置 —— 同一 URI 的格子连续、跨行时只隔着行尾与行首的空白才算一条。
fn hyperlink_cells(grid: &Grid<Cell>, line: i32, column: usize, link: &Hyperlink) -> Vec<LineCell> {
    let columns = grid.columns();
    let top = (line - MAX_WRAPPED_ROWS).max(-(grid.history_size() as i32));
    let bottom = (line + MAX_WRAPPED_ROWS).min(grid.screen_lines() as i32 - 1);
    let auto_id = link.id().ends_with(AUTO_LINK_ID_SUFFIX);
    let same = |cell: &Cell| {
        cell.hyperlink().is_some_and(|other| {
            if auto_id {
                other.uri() == link.uri()
            } else {
                other == *link
            }
        })
    };
    let rows: VecDeque<(i32, Range<usize>)> = if auto_id {
        adjacent_runs(grid, (line, column), top..=bottom, &same)
    } else {
        (top..=bottom).map(|l| (l, 0..columns)).collect()
    };
    let mut out = Vec::new();
    for (l, range) in rows {
        let row = &grid[Line(l)];
        for col in range {
            let cell = &row[Column(col)];
            let spacer = cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER);
            if !spacer && same(cell) {
                out.push(LineCell {
                    line: l,
                    column: col,
                    wide: cell.flags.contains(Flags::WIDE_CHAR),
                });
            }
        }
    }
    out
}

/// 从 `(line, column)` 那一段往上下走,跨行时只容忍行尾与行首的空白。
fn adjacent_runs(
    grid: &Grid<Cell>,
    (line, column): (i32, usize),
    rows: std::ops::RangeInclusive<i32>,
    same: &dyn Fn(&Cell) -> bool,
) -> VecDeque<(i32, Range<usize>)> {
    let columns = grid.columns();
    // 空格且不属于任何链接。宽字符的 spacer 也是空格,但它是内容
    let blank = |cell: &Cell| {
        cell.c == ' ' && cell.hyperlink().is_none() && !cell.flags.contains(Flags::WIDE_CHAR_SPACER)
    };
    let run_at = |l: i32, col: usize| {
        let row = &grid[Line(l)];
        let mut start = col;
        while start > 0 && same(&row[Column(start - 1)]) {
            start -= 1;
        }
        let mut end = col + 1;
        while end < columns && same(&row[Column(end)]) {
            end += 1;
        }
        start..end
    };
    let first = run_at(line, column);
    let (mut up, mut up_start) = (line, first.start);
    let (mut down, mut down_end) = (line, first.end);
    let mut out = VecDeque::from([(line, first)]);
    while up > *rows.start() && (0..up_start).all(|c| blank(&grid[Line(up)][Column(c)])) {
        let prev = &grid[Line(up - 1)];
        let Some(last) = (0..columns).rev().find(|&c| !blank(&prev[Column(c)])) else {
            break;
        };
        if !same(&prev[Column(last)]) {
            break;
        }
        let run = run_at(up - 1, last);
        up -= 1;
        up_start = run.start;
        out.push_front((up, run));
    }
    while down < *rows.end() && (down_end..columns).all(|c| blank(&grid[Line(down)][Column(c)])) {
        let next = &grid[Line(down + 1)];
        let Some(first) = (0..columns).find(|&c| !blank(&next[Column(c)])) else {
            break;
        };
        if !same(&next[Column(first)]) {
            break;
        }
        let run = run_at(down + 1, first);
        down += 1;
        down_end = run.end;
        out.push_back((down, run));
    }
    out
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

    // ── 普通文本里的网址 ───────────────────────────────────────────────────────

    /// 鼠标对着 `needle` 第一个字符时认出的网址。
    fn url_in(text: &str, needle: &str) -> Option<String> {
        let chars = chars(text);
        let byte = text.find(needle).expect("needle 不在文本里");
        let index = text[..byte].chars().count();
        url_candidate(&chars, index).map(|range| chars[range].iter().collect())
    }

    #[test]
    fn 网址的基本形状() {
        for url in [
            "https://example.com",
            "http://example.com/a/b.html?x=1&y=2#frag",
            "https://example.com:8443/a_b-c~d%20e",
            "http://localhost:3000/x",
            "http://127.0.0.1:8080",
            "http://[::1]:8080/a",
            "HTTPS://EXAMPLE.COM/A",
            "https://user:pw@example.com/",
        ] {
            let text = format!("see {url} now");
            assert_eq!(url_in(&text, url).as_deref(), Some(url));
            // 鼠标在网址尾部同样认得出整条
            let last = url.chars().last().unwrap().to_string();
            let tail = text.rfind(&format!("{last} now")).unwrap();
            let chars = chars(&text);
            let index = text[..tail].chars().count();
            let range = url_candidate(&chars, index).expect("网址尾部");
            assert_eq!(chars[range].iter().collect::<String>(), url);
        }
    }

    #[test]
    fn 只认_http_与_https() {
        for text in [
            "ftp://example.com/a",
            "file:///D:/proj/a.ts",
            "javascript:alert(1)",
            "ssh://git@github.com/a/b",
            "git+https://github.com/a/b",
            "xhttps://example.com",
            "http-https://example.com",
            "www.example.com",
        ] {
            let chars = chars(text);
            assert_eq!(url_candidate(&chars, 0), None, "{text}");
            assert_eq!(url_candidate(&chars, chars.len() - 1), None, "{text}");
        }
    }

    #[test]
    fn 没有主机的不算网址() {
        assert_eq!(url_in("https://", "h"), None);
        assert_eq!(url_in("https://.", "h"), None);
        assert_eq!(url_in("https:///etc/passwd", "h"), None);
        assert_eq!(url_in("https://?x=1", "h"), None);
        assert_eq!(url_in("https:/a.com", "h"), None);
    }

    #[test]
    fn 尾部标点不算网址() {
        for tail in [
            ".", ",", ";", ":", "!", "?", "...", "?!", ",.", "**", "):", ");",
        ] {
            let text = format!("(see https://example.com/a{tail}");
            assert_eq!(
                url_in(&text, "https").as_deref(),
                Some("https://example.com/a"),
                "{tail}"
            );
        }
        // 鼠标停在尾部标点上不亮
        assert_eq!(
            url_in("https://example.com.", "."),
            Some("https://example.com".into())
        );
        let text = chars("https://example.com.");
        assert_eq!(url_candidate(&text, text.len() - 1), None);
        // 中段的标点属于网址
        assert_eq!(
            url_in("https://example.com/a.b,c;d:e!f?g", "h").as_deref(),
            Some("https://example.com/a.b,c;d:e!f?g")
        );
    }

    #[test]
    fn 没配对的括号与引号不算网址() {
        for (text, expected) in [
            ("(https://a.com)", "https://a.com"),
            ("[https://a.com]", "https://a.com"),
            ("{https://a.com}", "https://a.com"),
            ("<https://a.com>", "https://a.com"),
            ("'https://a.com'", "https://a.com"),
            ("\"https://a.com\"", "https://a.com"),
            ("`https://a.com`", "https://a.com"),
            ("**https://a.com**", "https://a.com"),
            ("[文档](https://a.com/x)", "https://a.com/x"),
            ("(见 https://a.com/x).", "https://a.com/x"),
            ("'https://a.com/x',", "https://a.com/x"),
            ("https://a.com/x)y", "https://a.com/x"),
            ("https://a.com/x(", "https://a.com/x"),
            ("│ https://a.com │", "https://a.com"),
        ] {
            assert_eq!(url_in(text, "https").as_deref(), Some(expected), "{text}");
        }
    }

    #[test]
    fn 成对的括号保留() {
        for url in [
            "https://en.wikipedia.org/wiki/Foo_(bar)",
            "https://a.com/x[1]/y",
            "https://a.com/{id}",
            "https://a.com/'x'",
        ] {
            assert_eq!(url_in(url, "https").as_deref(), Some(url));
        }
        // 外面还包着一层时只剥外层
        assert_eq!(
            url_in("(https://en.wikipedia.org/wiki/Foo_(bar))", "https").as_deref(),
            Some("https://en.wikipedia.org/wiki/Foo_(bar)")
        );
        assert_eq!(
            url_in("[w](https://en.wikipedia.org/wiki/Foo_(bar)).", "https").as_deref(),
            Some("https://en.wikipedia.org/wiki/Foo_(bar)")
        );
    }

    #[test]
    fn 非_ascii_字符终止网址() {
        // U+FF0C 全角逗号。写成转义免得被编辑器换成半角,那样这条就测不到了
        const TEXT: &str = "访问https://a.com\u{FF0C}然后";
        assert_eq!(url_in(TEXT, "https").as_deref(), Some("https://a.com"));
        assert_eq!(url_in(TEXT, ".com").as_deref(), Some("https://a.com"));
        assert_eq!(url_in(TEXT, "访问"), None);
        assert_eq!(url_in(TEXT, "然后"), None);
        assert_eq!(
            url_in("https://a.com/路径/x", "https").as_deref(),
            Some("https://a.com/")
        );
        assert_eq!(url_in("https://a.com/路径/x", "x"), None);
        assert_eq!(
            url_in("见「https://a.com」。", "https").as_deref(),
            Some("https://a.com")
        );
    }

    #[test]
    fn 两条网址连写与查询参数里的网址() {
        let text = "https://a.com,https://b.com;https://c.com";
        assert_eq!(url_in(text, "a.com").as_deref(), Some("https://a.com"));
        assert_eq!(url_in(text, "b.com").as_deref(), Some("https://b.com"));
        assert_eq!(url_in(text, "c.com").as_deref(), Some("https://c.com"));
        // 查询参数里的网址是外面那条的一部分,鼠标停在哪半截都一样
        let text = "https://a.com/go?to=https://b.com/x";
        assert_eq!(url_in(text, "a.com").as_deref(), Some(text));
        assert_eq!(url_in(text, "b.com").as_deref(), Some(text));
        // 右括号收尾之后紧跟的是另一条
        let text = "[a](https://a.com),[b](https://b.com)";
        assert_eq!(url_in(text, "a.com").as_deref(), Some("https://a.com"));
        assert_eq!(url_in(text, "b.com").as_deref(), Some("https://b.com"));
    }

    #[test]
    fn 网址长度有上限() {
        let long = |n: usize| format!("https://a.com/{}", "x".repeat(n));
        let head = "https://a.com/".len();
        let fits = long(MAX_URL_CHARS - head);
        assert_eq!(url_in(&fits, "https").as_deref(), Some(fits.as_str()));
        let over = long(MAX_URL_CHARS - head + 1);
        assert_eq!(url_in(&over, "https"), None);
        assert_eq!(url_in(&over, "xxx"), None);
    }

    /// `(line, column)` 底下的网址文本。
    fn url_at(e: &TerminalEmulator, line: i32, column: usize) -> Option<String> {
        e.url_at(line, column).map(|link| link.url)
    }

    #[test]
    fn 软换行折开的网址拼回来() {
        let e = TerminalEmulator::new(TermSize::new(10, 4));
        e.advance("见 https://a.com/bc/de 后".as_bytes());
        // 第一行:见(0-1) 空格(2) https://(3..) 写到第 9 列折行
        for (line, column) in [(0, 3), (0, 9), (1, 0), (1, 7)] {
            let link = e.url_at(line, column).expect("鼠标在网址上");
            assert_eq!(link.url, "https://a.com/bc/de", "({line},{column})");
            assert!(!link.from_osc8);
            assert_eq!(link.cells.first().map(|c| (c.line, c.column)), Some((0, 3)));
            assert_eq!(
                link.cells.last().map(|c| (c.line, c.column)),
                Some((2, 1)),
                "下划线要盖到最后一段"
            );
        }
        assert_eq!(url_at(&e, 0, 0), None, "宽字符不是网址");
        assert_eq!(url_at(&e, 2, 3), None, "宽字符不是网址");
    }

    /// 路径与网址的候选互不抢:网址里没有路径候选,路径上没有网址,贴在一起也各认各的。
    #[test]
    fn 路径与网址互不抢() {
        let e = TerminalEmulator::new(TermSize::new(60, 4));
        e.advance(b"src/a.ts:12,https://a.com/x.ts:9 file:///D:/b.ts:3");
        assert_eq!(paths_at(&e, 0, 2), ["src/a.ts"]);
        assert_eq!(url_at(&e, 0, 2), None);
        assert_eq!(url_at(&e, 0, 14).as_deref(), Some("https://a.com/x.ts:9"));
        assert!(paths_at(&e, 0, 14).is_empty(), "网址不归路径认");
        assert_eq!(url_at(&e, 0, 44), None, "file:// 不是网址");
        assert_eq!(paths_at(&e, 0, 44), ["D:/b.ts"]);
    }

    /// Claude Code 的排版:长网址写到右边缘被硬断,下一行缩进两格接着写。
    #[test]
    fn tui_硬换行折开的网址接回来() {
        let e = TerminalEmulator::new(TermSize::new(24, 6));
        e.advance(b"  https://a.com/docs/xy\r\n  z/overview?a=1 ok\r\n");
        let want = "https://a.com/docs/xyz/overview?a=1";
        for (line, column) in [(0, 4), (0, 22), (1, 2), (1, 10)] {
            let link = e.url_at(line, column).expect("鼠标在网址上");
            assert_eq!(link.url, want, "({line},{column})");
            assert_eq!(link.cells[0].line, 0);
            assert_eq!(link.cells.last().map(|c| c.line), Some(1), "下划线跨两行");
        }
        assert_eq!(url_at(&e, 1, 17), None, "后面的散文不是网址");
    }

    /// 登录链接这类长网址折成好几行:路径那边只接上下各两段,网址要走完整条。
    #[test]
    fn 硬换行折成好几行的长网址整条接回来() {
        let e = TerminalEmulator::new(TermSize::new(20, 8));
        let rows = [
            "  https://a.com/auth",
            "  ?client=abcdefghi",
            "  jklmnopqrstuvwxyz",
            "  0123456789abcdefg",
            "  &state=xyz end",
        ];
        e.advance(rows.join("\r\n").as_bytes());
        let want =
            "https://a.com/auth?client=abcdefghijklmnopqrstuvwxyz0123456789abcdefg&state=xyz";
        for (line, column) in [(0, 5), (1, 5), (2, 5), (3, 5), (4, 5)] {
            let link = e.url_at(line, column).expect("鼠标在网址上");
            assert_eq!(link.url, want, "({line},{column})");
        }
        assert_eq!(url_at(&e, 4, 14), None, "后面的散文不是网址");
    }

    /// 长网址最后一截只有几个字母时靠「独占一行」认。
    #[test]
    fn 硬换行的最后一截独占一行时接回来() {
        let e = TerminalEmulator::new(TermSize::new(20, 4));
        e.advance(b"https://a.com/abcdef\r\n  ghij\r\n");
        assert_eq!(
            url_at(&e, 1, 3).as_deref(),
            Some("https://a.com/abcdefghij")
        );
    }

    /// 散文在词边界折行:没有盘可验,接错就是开错页面。这些一律不接。
    #[test]
    fn 散文折行不接到网址上() {
        // 下一行开头是普通单词
        let e = TerminalEmulator::new(TermSize::new(20, 4));
        e.advance(b"https://a.com/abcdef\r\nand then more\r\n");
        assert_eq!(url_at(&e, 0, 3).as_deref(), Some("https://a.com/abcdef"));
        assert_eq!(url_at(&e, 1, 0), None);

        // 句子的最后一个词独占一行,带着句号
        let e = TerminalEmulator::new(TermSize::new(20, 4));
        e.advance(b"https://a.com/abcdef\r\ninformation.\r\n");
        assert_eq!(url_at(&e, 0, 3).as_deref(), Some("https://a.com/abcdef"));

        // 行尾离右边缘超过两列
        let e = TerminalEmulator::new(TermSize::new(24, 4));
        e.advance(b"https://a.com/abcde\r\nfoo/bar\r\n");
        assert_eq!(url_at(&e, 0, 3).as_deref(), Some("https://a.com/abcde"));
        assert_eq!(url_at(&e, 1, 0), None);

        // 一行一条网址的列表
        let e = TerminalEmulator::new(TermSize::new(20, 4));
        e.advance(b"https://a.com/abcdef\r\nhttps://b.com/x\r\n");
        assert_eq!(url_at(&e, 0, 3).as_deref(), Some("https://a.com/abcdef"));
        assert_eq!(url_at(&e, 1, 3).as_deref(), Some("https://b.com/x"));

        // 上一行只是碰巧贴边的词,不影响当前行自己的网址
        let e = TerminalEmulator::new(TermSize::new(20, 4));
        e.advance(b"aaaa bbbb cccc dddd\r\nhttps://b.com/x\r\n");
        assert_eq!(url_at(&e, 1, 3).as_deref(), Some("https://b.com/x"));
    }

    // ── OSC 8 ─────────────────────────────────────────────────────────────────

    /// `ESC]8;params;uri ST 文字 ESC]8;; ST`
    fn osc8(params: &str, uri: &str, text: &str) -> Vec<u8> {
        format!("\x1b]8;{params};{uri}\x1b\\{text}\x1b]8;;\x1b\\").into_bytes()
    }

    #[test]
    fn osc8_经解析落到_cell_上() {
        let e = TerminalEmulator::new(TermSize::new(40, 4));
        e.advance(b"a ");
        e.advance(&osc8("id=x", "https://real.example/p", "text"));
        e.advance(b" b");
        e.with_term(|term| {
            let row = &term.grid()[Line(0)];
            assert!(row[Column(0)].hyperlink().is_none(), "链接外");
            let link = row[Column(2)].hyperlink().expect("链接落到了 cell 上");
            assert_eq!(link.uri(), "https://real.example/p");
            assert_eq!(link.id(), "x");
            assert_eq!(row[Column(5)].hyperlink().as_ref(), Some(&link));
            assert!(row[Column(6)].hyperlink().is_none(), "结束序列之后没有链接");
        });
        // BEL 结尾、URI 里带 `;` 也认
        let e = TerminalEmulator::new(TermSize::new(40, 4));
        e.advance(b"\x1b]8;;https://a.com/x;y=1\x07t\x1b]8;;\x07");
        assert_eq!(url_at(&e, 0, 0).as_deref(), Some("https://a.com/x;y=1"));
    }

    /// 经过同步块也一样:释放走的是同一条解析路径。
    #[test]
    fn osc8_在同步块里也落到_cell_上() {
        let e = TerminalEmulator::new(TermSize::new(40, 4));
        e.advance(b"\x1b[?2026h");
        e.advance(&osc8("", "https://a.com", "text"));
        assert_eq!(url_at(&e, 0, 0), None, "还没上屏");
        e.advance(b"\x1b[?2026l");
        assert_eq!(url_at(&e, 0, 0).as_deref(), Some("https://a.com"));
    }

    #[test]
    fn osc8_显示文字可以与目标毫不相干() {
        let e = TerminalEmulator::new(TermSize::new(40, 4));
        e.advance(&osc8("", "https://evil.example", "https://bank.example"));
        let link = e.url_at(0, 3).expect("链接上");
        assert_eq!(link.url, "https://evil.example", "以 URI 为准,不是显示文字");
        assert!(link.from_osc8, "宿主要把真实目标亮出来");
        assert_eq!(link.cells.len(), "https://bank.example".len());
    }

    #[test]
    fn osc8_只开_http_与_https() {
        for uri in [
            "file:///D:/proj/a.ts",
            "javascript:alert(1)",
            "vscode://file/a.ts",
            "mailto:a@b.c",
            "https://",
            "https://a.com/a b",
            "https://a.com/\u{202E}moc.live",
            "https://a.com/\u{200B}x",
            "https://a.com/\\x",
        ] {
            let e = TerminalEmulator::new(TermSize::new(40, 4));
            e.advance(&osc8("", uri, "https://shown.example"));
            assert_eq!(url_at(&e, 0, 3), None, "{uri:?}");
        }
        assert!(
            is_web_url("HTTP://A.COM/路径"),
            "非 ASCII 字符交给浏览器编码"
        );
        assert!(!is_web_url("https://a.com/\tx"), "解析器会吞掉,判据也得挡");
        let long = format!("https://a.com/{}", "x".repeat(MAX_URL_CHARS));
        assert!(!is_web_url(&long));
    }

    /// `file:` 链接包着的路径文字不能因此认不出:`ls --hyperlink` 这类程序给路径包的
    /// 就是它。走的是验盘的那条路,不是打开它的 URI。
    #[test]
    fn osc8_的_file_链接不挡路径识别() {
        let e = TerminalEmulator::new(TermSize::new(40, 4));
        e.advance(&osc8("", "file:///D:/elsewhere", "src/a.ts:7"));
        assert_eq!(url_at(&e, 0, 2), None);
        let (_, cands) = e.path_candidates_at(0, 2).unwrap();
        assert_eq!(cands[0].path, "src/a.ts");
    }

    #[test]
    fn osc8_整段一起高亮() {
        // 软换行折开,中间还夹着宽字符
        let e = TerminalEmulator::new(TermSize::new(10, 4));
        e.advance(b"ab");
        e.advance(&osc8("", "https://a.com", "12345678中9"));
        e.advance(b"z");
        for (line, column) in [(0, 2), (0, 9), (1, 0), (1, 1), (1, 2)] {
            let link = e.url_at(line, column).expect("链接上");
            let cells: Vec<(i32, usize)> = link.cells.iter().map(|c| (c.line, c.column)).collect();
            assert_eq!(
                cells,
                [
                    (0, 2),
                    (0, 3),
                    (0, 4),
                    (0, 5),
                    (0, 6),
                    (0, 7),
                    (0, 8),
                    (0, 9),
                    (1, 0),
                    (1, 2)
                ],
                "鼠标在 ({line},{column})"
            );
            assert!(link.cells[8].wide);
        }
        assert_eq!(url_at(&e, 0, 1), None);
        assert_eq!(url_at(&e, 1, 3), None);

        // 宽字符放不下时行尾留一格占位:占位格不进下划线,鼠标停在上面照样是这条链接
        let e = TerminalEmulator::new(TermSize::new(10, 4));
        e.advance(b"abc");
        e.advance(&osc8("", "https://a.com", "123456中"));
        let link = e.url_at(0, 9).expect("占位格也属于这条链接");
        let cells: Vec<(i32, usize)> = link.cells.iter().map(|c| (c.line, c.column)).collect();
        assert_eq!(
            cells,
            [(0, 3), (0, 4), (0, 5), (0, 6), (0, 7), (0, 8), (1, 0)]
        );
    }

    /// TUI 自己换行:每行各发一次 OSC 8、没写 id,alacritty 给的 id 各不相同。
    /// 同一 URI、只隔着行尾行首的空白,还是一条。
    #[test]
    fn osc8_tui_分行输出时跨行高亮() {
        let e = TerminalEmulator::new(TermSize::new(20, 6));
        e.advance(b"see ");
        e.advance(&osc8("", "https://a.com/long", "https://a.c"));
        e.advance(b"\r\n  ");
        e.advance(&osc8("", "https://a.com/long", "om/long"));
        e.advance(b"\r\n");
        for (line, column) in [(0, 5), (1, 3)] {
            let link = e.url_at(line, column).expect("链接上");
            assert_eq!(
                link.cells.len(),
                "https://a.com/long".len(),
                "({line},{column})"
            );
            assert_eq!(link.cells[0].line, 0);
            assert_eq!(link.cells.last().map(|c| c.line), Some(1));
        }
    }

    /// 同一 URI 但隔着别的内容、又没写 id:是两条,只亮鼠标底下那条。
    #[test]
    fn osc8_隔着内容的两段不连起来() {
        let e = TerminalEmulator::new(TermSize::new(40, 4));
        e.advance(&osc8("", "https://a.com", "one"));
        e.advance(b" mid ");
        e.advance(&osc8("", "https://a.com", "two"));
        let link = e.url_at(0, 1).unwrap();
        assert_eq!(link.cells.len(), 3);
        assert_eq!(link.cells[0].column, 0);
        let link = e.url_at(0, 9).unwrap();
        assert_eq!(link.cells.len(), 3);
        assert_eq!(link.cells[0].column, 8);
    }

    /// 写了同一个 id:OSC 8 规范说它们就是一条,隔着内容也一起亮。
    /// id 相同但 URI 不同不算(规范以 id + URI 为准)。
    #[test]
    fn osc8_同_id_同_uri_隔着内容也是一条() {
        let e = TerminalEmulator::new(TermSize::new(40, 4));
        e.advance(&osc8("id=x", "https://a.com", "one"));
        e.advance(b" mid ");
        e.advance(&osc8("id=x", "https://a.com", "two"));
        e.advance(b" ");
        e.advance(&osc8("id=x", "https://b.com", "other"));
        let link = e.url_at(0, 1).unwrap();
        let columns: Vec<usize> = link.cells.iter().map(|c| c.column).collect();
        assert_eq!(columns, [0, 1, 2, 8, 9, 10]);
        assert_eq!(e.url_at(0, 13).unwrap().url, "https://b.com");
        assert_eq!(e.url_at(0, 13).unwrap().cells.len(), 5);
    }

    /// 回看缓冲里的链接照样能认,行号越界 / 列号越界都不 panic。
    #[test]
    fn osc8_在回看缓冲里也认得出() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(40, 3), 100);
        e.advance(&osc8("", "https://a.com", "old"));
        e.advance(b"\r\n1\r\n2\r\n3\r\n4\r\n");
        let top = -(e.history_lines() as i32);
        assert_eq!(url_at(&e, top, 1).as_deref(), Some("https://a.com"));
        assert_eq!(url_at(&e, top - 1, 1), None);
        assert_eq!(url_at(&e, 3, 1), None);
        assert_eq!(url_at(&e, 0, 40), None);
    }
}
