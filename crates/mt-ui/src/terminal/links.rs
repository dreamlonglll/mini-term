//! 终端里的链接:按住 Ctrl(macOS 上 ⌘)悬停出下划线与手形光标,单击交给宿主打开。
//! 两种目标 —— 文件(输出里的路径)与网址(http / https 文本、OSC 8 超链接)。
//!
//! 识别在 [`mt_terminal::links`];「是不是真文件、指向哪」由宿主回答
//! ([`FileLinks::new`] 的 `resolve`)。这里管渲染层那一截:鼠标对着哪一格、
//! 下划线画在哪几段、宿主的裁决缓存多久。
//!
//! # OSC 8 要亮出真实目标
//!
//! OSC 8 的显示文字可以写成任何样子(`https://bank.example` 底下是别的网址),所以悬停时
//! 元素在链接旁边画一个气泡,原样显示它的目标(`TerminalElement::paint_link_target`)。
//! 普通文本网址不画:显示的就是要打开的。气泡在 paint 里现画,不走 gpui 的 tooltip ——
//! 那个要等 1.2s、还挂在 div 上,这里只有一个大 hitbox;悬停态本来也只有 paint 才知道。
//!
//! # 悬停态每帧现算,不存坐标
//!
//! 悬停中的链接在 paint 阶段按**当下的**修饰键、鼠标位置与 grid 内容重新求一遍
//! (`TerminalElement::hovered_link`),而不是在鼠标事件里算好存着:AI 一边输出、
//! 内容一边上滚,存下来的行号下一帧就指向别的文字了。代价只在按住 Ctrl 且鼠标
//! 停在终端上时才付 —— 拼一条逻辑行、扫一遍字符;宿主的盘上校验由 [`LinkCache`] 挡着。
//!
//! 要一帧的时机只有三个:按住 Ctrl 跨格移动(元素的 mouse-move)、Ctrl 按下 / 松开
//! ([`super::TerminalView`] 的 modifiers-changed —— 只有焦点终端收得到,非焦点的
//! 分屏要动一下鼠标)、本来就有的输出重绘。

use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{App, Bounds, Pixels, Point, Size, Window, point};
use mt_terminal::TerminalEmulator;
use mt_terminal::links::{LineCell, LogicalLine, PathCandidate};

/// 一个确定能打开的文件链接。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileLink {
    /// 宿主解析好的路径(绝对路径,或宿主自己认得的远程路径)。
    pub path: PathBuf,
    /// 1 起算的行号 / 列号。
    pub line: Option<u32>,
    /// 范围写法的终点行(见 [`PathCandidate::end_line`])。
    pub end_line: Option<u32>,
    pub column: Option<u32>,
}

/// 宿主对一个候选的裁决:能打开就给出路径。在 paint 期调用,只该做几次 stat 这种
/// 量级的事;同一段文字的结果有缓存([`LinkCache`]),不会每帧都问。
pub type ResolveFileLink = Rc<dyn Fn(&PathCandidate, &App) -> Option<PathBuf>>;
/// Ctrl+单击链接时的出口。
pub type OpenFileLink = Rc<dyn Fn(&FileLink, &mut Window, &mut App)>;
/// Ctrl+单击网址时的出口。给到这里的只会是 http / https(见 `mt_terminal::links::is_web_url`)。
pub type OpenUrlLink = Rc<dyn Fn(&str, &mut App)>;

/// 宿主接上的「终端链接」能力(文件 + 网址)。不接 = 终端里 Ctrl+悬停什么都不发生。
#[derive(Clone)]
pub struct FileLinks {
    pub(crate) resolve: ResolveFileLink,
    pub(crate) open: OpenFileLink,
    pub(crate) open_url: OpenUrlLink,
}

impl FileLinks {
    pub fn new(
        resolve: impl Fn(&PathCandidate, &App) -> Option<PathBuf> + 'static,
        open: impl Fn(&FileLink, &mut Window, &mut App) + 'static,
        open_url: impl Fn(&str, &mut App) + 'static,
    ) -> Self {
        Self {
            resolve: Rc::new(resolve),
            open: Rc::new(open),
            open_url: Rc::new(open_url),
        }
    }
}

/// 下划线的一段:一个 grid 行上的列区间(左闭右开)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LinkSpan {
    pub line: i32,
    pub start: usize,
    pub end: usize,
}

/// 链接指向什么。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LinkTarget {
    File(FileLink),
    Url {
        url: String,
        /// 显示的文字不一定就是目标(OSC 8),悬停时要把目标亮出来。
        reveal: bool,
    },
}

/// 鼠标底下的那个链接,连同它在 grid 上占的几段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HoveredLink {
    pub target: LinkTarget,
    pub spans: Vec<LinkSpan>,
}

impl HoveredLink {
    /// 悬停时要亮给人看的真实目标。
    pub fn revealed_target(&self) -> Option<&str> {
        match &self.target {
            LinkTarget::Url { url, reveal: true } => Some(url),
            _ => None,
        }
    }
}

/// 宿主裁决的缓存,键是候选的路径原文。
///
/// 鼠标停在一个片段上不动时每帧都会重新识别(见模块注释),没有这层就是每帧一次
/// 盘上校验。松开 Ctrl 就整表清空 —— 下次按下时刚生成的文件也认得出来。
#[derive(Default)]
pub(crate) struct LinkCache {
    entries: Vec<(String, Option<PathBuf>)>,
}

/// 一次按住 Ctrl 期间最多记多少个片段的裁决。鼠标扫过一屏也就几十个。
const LINK_CACHE_CAP: usize = 64;

impl LinkCache {
    fn resolve(&mut self, cand: &PathCandidate, links: &FileLinks, cx: &App) -> Option<PathBuf> {
        if let Some((_, hit)) = self.entries.iter().find(|(key, _)| *key == cand.path) {
            return hit.clone();
        }
        let resolved = (links.resolve)(cand, cx);
        if self.entries.len() >= LINK_CACHE_CAP {
            self.entries.remove(0);
        }
        self.entries.push((cand.path.clone(), resolved.clone()));
        resolved
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// 鼠标对着 grid 的 `(line, column)` 时,底下有没有能打开的东西。
pub(crate) fn probe(
    emulator: &TerminalEmulator,
    links: &FileLinks,
    line: i32,
    column: usize,
    cache: &mut LinkCache,
    cx: &App,
) -> Option<HoveredLink> {
    // 网址在前:它不用问宿主、不碰盘;鼠标不在网址上时路径照常认,两边的候选互不抢
    if let Some(url) = emulator.url_at(line, column) {
        return Some(HoveredLink {
            spans: cell_spans(&url.cells),
            target: LinkTarget::Url {
                url: url.url,
                reveal: url.from_osc8,
            },
        });
    }
    // 连 TUI 自己折开的上下行一起认(硬换行),见 `mt_terminal::links` 模块注释
    let (logical, candidates) = emulator.path_candidates_at(line, column)?;
    candidates.into_iter().find_map(|cand| {
        let path = cache.resolve(&cand, links, cx)?;
        Some(HoveredLink {
            target: LinkTarget::File(FileLink {
                path,
                line: cand.line,
                end_line: cand.end_line,
                column: cand.column,
            }),
            spans: spans(&logical, cand.range),
        })
    })
}

/// 字符区间 → 逐行的列区间。宽字符多盖一列;软换行折开的链接落成多段。
pub(crate) fn spans(line: &LogicalLine, range: Range<usize>) -> Vec<LinkSpan> {
    cell_spans(line.cells.get(range).unwrap_or_default())
}

/// 一串格子 → 逐行的列区间。同一行上不挨着的(同 id 的 OSC 8 分段)各成一段。
fn cell_spans(cells: &[LineCell]) -> Vec<LinkSpan> {
    let mut out: Vec<LinkSpan> = Vec::new();
    for cell in cells {
        let end = cell.column + if cell.wide { 2 } else { 1 };
        match out.last_mut() {
            Some(span) if span.line == cell.line && cell.column <= span.end => {
                span.end = span.end.max(end)
            }
            _ => out.push(LinkSpan {
                line: cell.line,
                start: cell.column,
                end,
            }),
        }
    }
    out
}

/// 把真实目标折成气泡里的几行:每行不超过 `columns` 列,至多 `max_rows` 行。原样显示,
/// 一个字符都不改。
///
/// 超长放不下时留头留尾、中间用 `…` 代替:只截尾巴的话,`https://好网站.com:填满气泡…@坏网站.com`
/// 这种把真实主机藏在后面的写法就看不出来了。列宽不查表:ASCII 一列,其余一律按两列算
/// —— 宁可早折,不让字符跑出气泡。
pub(crate) fn wrap_link_target(target: &str, columns: usize, max_rows: usize) -> Vec<String> {
    let width = |c: char| if c.is_ascii() { 1 } else { 2 };
    let columns = columns.max(2);
    let max_rows = max_rows.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    let mut used = 0;
    for c in target.chars() {
        if used + width(c) > columns {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push(c);
        used += width(c);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    if rows.len() <= max_rows {
        return rows;
    }
    // 最后一行的位置留给尾巴:倒着从原文取,放得下多少取多少
    let mut tail: Vec<char> = Vec::new();
    let mut used = 1; // `…` 占一列
    for c in target.chars().rev() {
        if used + width(c) > columns {
            break;
        }
        tail.push(c);
        used += width(c);
    }
    rows.truncate(max_rows - 1);
    rows.push(std::iter::once('…').chain(tail.into_iter().rev()).collect());
    rows
}

/// 气泡贴在链接上方,左边对齐链接起点;上面放不下就挪到链接下方,都放不下贴着元素顶边。
/// 横向收在元素之内。
pub(crate) fn link_target_origin(
    link_left: Pixels,
    link_top: Pixels,
    link_bottom: Pixels,
    tip: Size<Pixels>,
    bounds: Bounds<Pixels>,
) -> Point<Pixels> {
    let right = bounds.origin.x + bounds.size.width;
    let bottom = bounds.origin.y + bounds.size.height;
    let x = if link_left + tip.width > right {
        (right - tip.width).max(bounds.origin.x)
    } else {
        link_left
    };
    let above = link_top - tip.height;
    let y = if above >= bounds.origin.y {
        above
    } else if link_bottom + tip.height <= bottom {
        link_bottom
    } else {
        bounds.origin.y
    };
    point(x, y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{px, size};

    fn cell(line: i32, column: usize, wide: bool) -> LineCell {
        LineCell { line, column, wide }
    }

    #[test]
    fn 软换行折开的链接落成逐行两段() {
        let line = LogicalLine {
            chars: "abcdef".chars().collect(),
            cells: vec![
                cell(-1, 7, false),
                cell(-1, 8, false),
                cell(-1, 9, false),
                cell(0, 0, false),
                cell(0, 1, false),
                cell(0, 2, false),
            ],
        };
        assert_eq!(
            spans(&line, 1..5),
            vec![
                LinkSpan {
                    line: -1,
                    start: 8,
                    end: 10
                },
                LinkSpan {
                    line: 0,
                    start: 0,
                    end: 2
                },
            ]
        );
    }

    #[test]
    fn 宽字符多盖一列() {
        let line = LogicalLine {
            chars: "改a".chars().collect(),
            cells: vec![cell(0, 0, true), cell(0, 2, false)],
        };
        assert_eq!(
            spans(&line, 0..2),
            vec![LinkSpan {
                line: 0,
                start: 0,
                end: 3
            }]
        );
        assert!(spans(&line, 5..9).is_empty(), "越界区间不 panic");
    }

    #[test]
    fn 同一行不挨着的格子各成一段() {
        assert_eq!(
            cell_spans(&[
                cell(0, 0, false),
                cell(0, 1, false),
                cell(0, 8, false),
                cell(0, 9, true),
                cell(1, 0, false),
            ]),
            vec![
                LinkSpan {
                    line: 0,
                    start: 0,
                    end: 2
                },
                LinkSpan {
                    line: 0,
                    start: 8,
                    end: 11
                },
                LinkSpan {
                    line: 1,
                    start: 0,
                    end: 1
                },
            ]
        );
    }

    /// 只有 OSC 8 的显示文字可能与目标不符,普通网址与文件不画气泡。
    #[test]
    fn 只有_osc8_要亮出真实目标() {
        let url = |reveal| HoveredLink {
            target: LinkTarget::Url {
                url: "https://a.com".into(),
                reveal,
            },
            spans: Vec::new(),
        };
        assert_eq!(url(true).revealed_target(), Some("https://a.com"));
        assert_eq!(url(false).revealed_target(), None);
        let file = HoveredLink {
            target: LinkTarget::File(FileLink {
                path: PathBuf::from("a.ts"),
                line: None,
                end_line: None,
                column: None,
            }),
            spans: Vec::new(),
        };
        assert_eq!(file.revealed_target(), None);
    }

    #[test]
    fn 目标放得下时原样折行() {
        assert_eq!(wrap_link_target("https://a.com", 40, 4), ["https://a.com"]);
        assert_eq!(
            wrap_link_target("https://a.com/x", 5, 4),
            ["https", "://a.", "com/x"],
            "正好写满一行不多折"
        );
        // 宽字符按两列算,放不下就折到下一行
        assert_eq!(wrap_link_target("ab中", 3, 4), ["ab", "中"]);
        assert!(wrap_link_target("", 10, 4).is_empty());
    }

    /// 放不下时头尾都要留着 —— 只留头的话真实主机可以藏在后面。
    #[test]
    fn 目标超长时留头留尾() {
        let url = format!("https://good.example:{}@evil.example/", "x".repeat(200));
        let rows = wrap_link_target(&url, 30, 3);
        assert_eq!(rows.len(), 3);
        assert!(rows[0].starts_with("https://good.example:"));
        let last = rows.last().unwrap();
        assert!(last.starts_with('…'));
        assert!(last.ends_with("@evil.example/"), "{last}");
        assert!(rows.iter().all(|row| row.chars().count() <= 30));

        // 只有一行的位置:头让位给尾巴
        assert_eq!(wrap_link_target("abcdefghij", 5, 1), ["…ghij"]);
        // 窄到荒唐也不死循环、不丢字
        assert_eq!(wrap_link_target("abc", 0, 9).concat(), "abc");
    }

    #[test]
    fn 气泡落点() {
        let bounds = Bounds::new(point(px(100.0), px(50.0)), size(px(400.0), px(300.0)));
        let tip = size(px(120.0), px(30.0));
        // 上方放得下:贴在链接上方,左对齐
        assert_eq!(
            link_target_origin(px(150.0), px(200.0), px(220.0), tip, bounds),
            point(px(150.0), px(170.0))
        );
        // 顶到元素上沿:挪到链接下方
        assert_eq!(
            link_target_origin(px(150.0), px(60.0), px(80.0), tip, bounds),
            point(px(150.0), px(80.0))
        );
        // 横向超出右沿:收回来
        assert_eq!(
            link_target_origin(px(450.0), px(200.0), px(220.0), tip, bounds),
            point(px(380.0), px(170.0))
        );
        // 比元素还宽:贴左沿;上下都放不下:贴顶
        let huge = size(px(900.0), px(290.0));
        assert_eq!(
            link_target_origin(px(150.0), px(200.0), px(220.0), huge, bounds),
            point(px(100.0), px(50.0))
        );
    }
}
