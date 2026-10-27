//! 终端里的文件链接:按住 Ctrl(macOS 上 ⌘)悬停出下划线与手形光标,单击交给宿主打开。
//!
//! 识别在 [`mt_terminal::links`](只看形状);「是不是真文件、指向哪」由宿主回答
//! ([`FileLinks::new`] 的 `resolve`)。这里管渲染层那一截:鼠标对着哪一格、
//! 下划线画在哪几段、宿主的裁决缓存多久。
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

use gpui::{App, Window};
use mt_terminal::TerminalEmulator;
use mt_terminal::links::{LogicalLine, PathCandidate, path_candidates};

/// 一个确定能打开的文件链接。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileLink {
    /// 宿主解析好的路径(绝对路径,或宿主自己认得的远程路径)。
    pub path: PathBuf,
    /// 1 起算的行号 / 列号。
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// 宿主对一个候选的裁决:能打开就给出路径。在 paint 期调用,只该做几次 stat 这种
/// 量级的事;同一段文字的结果有缓存([`LinkCache`]),不会每帧都问。
pub type ResolveFileLink = Rc<dyn Fn(&PathCandidate, &App) -> Option<PathBuf>>;
/// Ctrl+单击链接时的出口。
pub type OpenFileLink = Rc<dyn Fn(&FileLink, &mut Window, &mut App)>;

/// 宿主接上的「文件链接」能力。不接 = 终端里 Ctrl+悬停什么都不发生。
#[derive(Clone)]
pub struct FileLinks {
    pub(crate) resolve: ResolveFileLink,
    pub(crate) open: OpenFileLink,
}

impl FileLinks {
    pub fn new(
        resolve: impl Fn(&PathCandidate, &App) -> Option<PathBuf> + 'static,
        open: impl Fn(&FileLink, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            resolve: Rc::new(resolve),
            open: Rc::new(open),
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

/// 鼠标底下的那个链接,连同它在 grid 上占的几段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HoveredLink {
    pub link: FileLink,
    pub spans: Vec<LinkSpan>,
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

/// 鼠标对着 grid 的 `(line, column)` 时,底下有没有能打开的文件。
pub(crate) fn probe(
    emulator: &TerminalEmulator,
    links: &FileLinks,
    line: i32,
    column: usize,
    cache: &mut LinkCache,
    cx: &App,
) -> Option<HoveredLink> {
    let logical = emulator.logical_line_at(line)?;
    let index = logical.index_at(line, column)?;
    path_candidates(&logical.chars, index)
        .into_iter()
        .find_map(|cand| {
            let path = cache.resolve(&cand, links, cx)?;
            Some(HoveredLink {
                link: FileLink {
                    path,
                    line: cand.line,
                    column: cand.column,
                },
                spans: spans(&logical, cand.range),
            })
        })
}

/// 字符区间 → 逐行的列区间。宽字符多盖一列;软换行折开的链接落成多段。
pub(crate) fn spans(line: &LogicalLine, range: Range<usize>) -> Vec<LinkSpan> {
    let mut out: Vec<LinkSpan> = Vec::new();
    for cell in line.cells.get(range).unwrap_or_default() {
        let end = cell.column + if cell.wide { 2 } else { 1 };
        match out.last_mut() {
            Some(span) if span.line == cell.line => span.end = span.end.max(end),
            _ => out.push(LinkSpan {
                line: cell.line,
                start: cell.column,
                end,
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mt_terminal::links::LineCell;

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
                LinkSpan { line: -1, start: 8, end: 10 },
                LinkSpan { line: 0, start: 0, end: 2 },
            ]
        );
    }

    #[test]
    fn 宽字符多盖一列() {
        let line = LogicalLine {
            chars: "改a".chars().collect(),
            cells: vec![cell(0, 0, true), cell(0, 2, false)],
        };
        assert_eq!(spans(&line, 0..2), vec![LinkSpan { line: 0, start: 0, end: 3 }]);
        assert!(spans(&line, 5..9).is_empty(), "越界区间不 panic");
    }
}
