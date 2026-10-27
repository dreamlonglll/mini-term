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
//! 挨个去盘上验,第一个成立的才算链接。
//!
//! # 硬换行认不出来
//!
//! 只有终端自己折的行(写满一行自动换行)带 `WRAPLINE`。Claude Code / Codex 这类
//! TUI 按宽度**自己插换行符**排版,折开的路径在 grid 里就是两行互不相干的文本 ——
//! 这一类只认得出鼠标所在的那一截。

use std::ops::Range;

use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;

use crate::TerminalEmulator;

/// 往上 / 往下各最多拼多少段软换行。再长的「一行」多半是日志刷屏,不是路径。
const MAX_WRAPPED_ROWS: i32 = 16;
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
}

impl TerminalEmulator {
    /// `line`(grid 行号)所在的整条逻辑行。行号越界返回 `None`。
    pub fn logical_line_at(&self, line: i32) -> Option<LogicalLine> {
        let term = self.term.lock();
        let grid = term.grid();
        let top = -(grid.history_size() as i32);
        let bottom = grid.screen_lines() as i32 - 1;
        let columns = grid.columns();
        if columns == 0 || line < top || line > bottom {
            return None;
        }
        // 行尾那一格带 WRAPLINE = 这一行是被终端折过去的,下一行接着它
        let wraps = |l: i32| grid[Line(l)][Column(columns - 1)].flags.contains(Flags::WRAPLINE);
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
}

/// 一个「可能是文件」的片段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathCandidate {
    /// 路径正文,行号后缀已剥掉。可能是相对路径,也可能是别的系统的写法
    /// (WSL 里的 `/home/...`)—— 解释权在宿主。
    pub path: String,
    /// 1 起算的行号。写成 `:0` 的当没写。
    pub line: Option<u32>,
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
    let mut line = None;
    let mut column = None;
    let mut range_end = end;

    if let Some((hash, l, c)) = github_anchor(token) {
        path_end = start + hash;
        line = l;
        column = c;
    } else if let Some((stripped, l, c)) = split_colon_suffix(token) {
        path_end = start + stripped;
        line = l;
        column = c;
    }
    // 片段里没写行号:再往后看一眼 TypeScript 的 `(行,列)` 与 Python 的 `", line N`
    if path_end == end {
        if let Some((l, c, consumed)) = paren_suffix(&chars[end..]) {
            line = l;
            column = c;
            range_end = end + consumed;
        } else if let Some(l) = python_line_suffix(&chars[end..]) {
            line = l;
        }
    }
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
            column,
            range: start..start + anchorless.chars().count(),
        });
    }
    out
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

/// `20` 或 `20-35`(只取起点)。不是这两种形状返回 `None`。
fn number_or_range(seg: &[char]) -> Option<Option<u32>> {
    let digits = seg.iter().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let rest = &seg[digits..];
    let ok = rest.is_empty()
        || (rest[0] == '-' && rest.len() > 1 && rest[1..].iter().all(char::is_ascii_digit));
    ok.then(|| digits_value(&seg[..digits]))
}

/// 从尾部剥 `:行` / `:行:列`(各自允许写成 `起-止`,只取起点)。
///
/// 返回 `(路径结束下标, 行, 列)`;没有后缀返回 `None`。
/// `D:\a.ts` 的盘符冒号后面不是数字,不会被误剥。
fn split_colon_suffix(token: &[char]) -> Option<(usize, Option<u32>, Option<u32>)> {
    let mut end = token.len();
    let mut numbers: Vec<Option<u32>> = Vec::new();
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
    match numbers.as_slice() {
        [] => None,
        [line] => Some((end, *line, None)),
        [column, line, ..] => Some((end, *line, *column)),
    }
}

/// GitHub 风格的 `#L12` / `#L12C5` / `#L12-L20`。返回 `(# 的下标, 行, 列)`。
fn github_anchor(token: &[char]) -> Option<(usize, Option<u32>, Option<u32>)> {
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
    // 范围的终点(`-L20` / `-20` / `-L20C3`)只认形状,不取值
    if !tail.is_empty()
        && !(tail[0] == '-'
            && tail.len() > 1
            && tail[1..].iter().all(|c| c.is_ascii_digit() || matches!(c, 'L' | 'C')))
    {
        return None;
    }
    Some((hash, line, column))
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
        assert_eq!((cand.path.as_str(), cand.line, cand.column), ("src/main.rs", Some(10), Some(5)));

        let cand = first_at("./main.go:12:3: undefined: x", "main").unwrap();
        assert_eq!((cand.path.as_str(), cand.line, cand.column), ("./main.go", Some(12), Some(3)));

        let cand = first_at("改动见 `src/a.ts:20-35`", "src").unwrap();
        assert_eq!((cand.path.as_str(), cand.line), ("src/a.ts", Some(20)));

        // 行号写 0 当没写
        let cand = first_at("a.ts:0", "a.ts").unwrap();
        assert_eq!((cand.path.as_str(), cand.line), ("a.ts", None));
    }

    #[test]
    fn tsc_括号行列号() {
        let text = "src/a.ts(20,5): error TS2322";
        let cand = first_at(text, "src").unwrap();
        assert_eq!((cand.path.as_str(), cand.line, cand.column), ("src/a.ts", Some(20), Some(5)));
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
        assert_eq!((cand.path.as_str(), cand.line), (r"C:\proj\app.py", Some(42)));
        assert_eq!(covered(text, &cand), r"C:\proj\app.py");
    }

    #[test]
    fn github_锚点与标题锚点() {
        let cand = first_at("docs/a.md#L12C3", "docs").unwrap();
        assert_eq!((cand.path.as_str(), cand.line, cand.column), ("docs/a.md", Some(12), Some(3)));
        let cand = first_at("src/x.rs#L5-L9", "src").unwrap();
        assert_eq!((cand.path.as_str(), cand.line), ("src/x.rs", Some(5)));

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
        assert_eq!(first_at("修改了 src/a.ts。", "src").unwrap().path, "src/a.ts");
        assert_eq!(first_at("见：src/a.ts，然后", "src").unwrap().path, "src/a.ts");
        assert_eq!(first_at("│ src/a.ts │", "src").unwrap().path, "src/a.ts");
        assert_eq!(first_at("see src/a.ts.", "src").unwrap().path, "src/a.ts");
        assert_eq!(first_at("**README.md**", "README").unwrap().path, "README.md");
        assert_eq!(first_at("[src/a.ts](src/a.ts)", "src").unwrap().path, "src/a.ts");
        assert_eq!(first_at("(see src/a.ts)", "src").unwrap().path, "src/a.ts");
    }

    #[test]
    fn nextjs_动态路由的方括号保留() {
        assert_eq!(
            first_at("app/[slug]/page.tsx", "page").unwrap().path,
            "app/[slug]/page.tsx"
        );
        assert_eq!(first_at("pages/[id].tsx", "pages").unwrap().path, "pages/[id].tsx");
    }

    #[test]
    fn 网址不认_file_协议剥成本地路径() {
        assert!(first_at("https://example.com/a.ts", "example").is_none());
        let cand = first_at("file:///D:/proj/a.ts:3", "proj").unwrap();
        assert_eq!((cand.path.as_str(), cand.line), ("D:/proj/a.ts", Some(3)));
        assert_eq!(first_at("file:///home/u/a.rs", "home").unwrap().path, "/home/u/a.rs");
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
        assert_eq!(line.cells[0], LineCell { line: 0, column: 0, wide: false });
        assert_eq!(line.cells[10], LineCell { line: 1, column: 0, wide: false });

        // 鼠标在第二段上照样拿到整条路径
        let index = line.index_at(1, 2).unwrap();
        let cand = path_candidates(&line.chars, index).into_iter().next().unwrap();
        assert_eq!((cand.path.as_str(), cand.line), ("src/abc/def.ts", Some(7)));

        // 没被折的行只有它自己
        e.advance(b"\r\nx");
        let short = e.logical_line_at(2).unwrap();
        assert_eq!(short.cells.len(), 10);
        assert!(short.cells.iter().all(|c| c.line == 2));
        assert!(e.logical_line_at(4).is_none(), "越过屏底");
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
