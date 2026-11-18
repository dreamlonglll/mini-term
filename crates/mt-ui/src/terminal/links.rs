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
//! 元素在链接旁边画一个气泡,原样显示它的目标(`TerminalElement::paint_link_bubble`)。
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
//!
//! # 不按修饰键的悬停提示
//!
//! 不按 Ctrl 时网址与路径看着就是普通文字,没人知道能点。所以鼠标**停**在一格上
//! [`LINK_HINT_DELAY`] 之后再探一次,底下是链接就给它画一条弱化的下划线、旁边出一个
//! 「Ctrl+单击打开」的气泡(文案由宿主注入,[`LinkHintLabel`])。光标形状不变 ——
//! 不按 Ctrl 的单击照旧是起选区,手形光标会让人以为点一下就开。
//!
//! 与上面「每帧现算」那条路的差别全在开销上:那条路只在按住 Ctrl 时付,这条路是
//! 鼠标在终端上**随便晃**都可能触发,所以收得更紧([`LinkHint`] 的状态机):
//!
//! - **停留才探**:跨格移动只换计时器,不探、不要帧;到点才在计时器回调里探一次
//!   (路径验盘就在这一下)。探不到链接什么都不画,也不要帧;
//! - **只在提示出现 / 消失时要帧**:提示挂着时移动、按下鼠标、离开元素才各要一帧把它
//!   撤掉;
//! - **内容变了才重探**:提示挂着期间来了新输出,paint 里按内容代数判断要不要重探
//!   (走同一份裁决缓存,不再碰盘);探出来已经不是同一个链接 —— 输出把别的文字顶到了
//!   鼠标底下 —— 就撤掉,不许跳到别的链接上。回看位置变了直接撤(滚动等于移开);
//! - **TUI 抓着鼠标、正在拖选、按着 Ctrl 时不做**:Ctrl 那条路自己会画,两条路叠在
//!   一起会出两条下划线;OSC 8 的「真实目标」气泡与提示合成**一个**气泡(目标在上、
//!   提示在最后一行),不叠两个。

use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui::{App, Bounds, Pixels, Point, SharedString, Size, Window, point};
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

/// 宿主问「这一格底下是什么链接」时拿到的答案(右键菜单的「打开链接 / 复制链接」)。
///
/// 与悬停、Ctrl+单击走的是同一个 [`probe`],识别口径一处。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalLink {
    /// 宿主裁决过、能在文件页打开的文件。
    File(FileLink),
    /// http / https 网址(文本网址或 OSC 8 的真实目标)。
    Url(String),
}

impl From<LinkTarget> for TerminalLink {
    fn from(target: LinkTarget) -> Self {
        match target {
            LinkTarget::File(link) => Self::File(link),
            LinkTarget::Url { url, .. } => Self::Url(url),
        }
    }
}

/// 不按修饰键悬停在链接上多久才出提示。比 gpui 自带 tooltip 的 1.2s 短:这是「这里能点」
/// 的提示,鼠标划过去时不该出,停下来看的时候要来得及。
pub const LINK_HINT_DELAY: Duration = Duration::from_millis(500);

/// 悬停提示气泡里那行字(「Ctrl+单击打开」)。宿主注入(mt-ui 不依赖 mt-i18n),
/// 每次画都现取一次,切语言立刻生效。
pub type LinkHintLabel = Rc<dyn Fn() -> SharedString>;

/// 提示挂着时探到的链接,连同探的那一刻的回看位置与内容代数。
#[derive(Clone, Debug, PartialEq, Eq)]
struct ShownHint {
    link: HoveredLink,
    display_offset: usize,
    content_generation: u64,
}

/// 鼠标移动一次之后提示该怎么变。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HintMove {
    /// 要(重新)起停留计时器,带上这一轮的代号。
    pub rearm: Option<u64>,
    /// 撤掉计时器(鼠标离开、按下修饰键之类,不再停留)。
    pub disarm: bool,
    /// 撤掉了一个挂着的提示 —— 要一帧把它擦掉。
    pub repaint: bool,
}

/// 不按修饰键的悬停提示的状态机(口径见模块注释「不按修饰键的悬停提示」)。
///
/// 纯数据,不碰 gpui:计时器由元素持有,这里只发代号,到点回来的那一发靠代号认赔。
#[derive(Default)]
pub(crate) struct LinkHint {
    /// 鼠标停着的屏幕格 `(列, 行)`。`None` = 不在停留(不在元素上 / 不该提示)。
    cell: Option<(usize, usize)>,
    /// 停留计时的代号。每换一格 / 撤销一次加一,到点回来的计时器拿它对账。
    generation: u64,
    /// 停够了、探到了的链接。
    shown: Option<ShownHint>,
}

impl LinkHint {
    /// 鼠标挪到了 `cell`(`None` = 不该提示的位置:离开元素、按着键、在滚动条上……)。
    ///
    /// 还在同一格不算移动:计时照走,提示照挂。
    pub fn on_move(&mut self, cell: Option<(usize, usize)>) -> HintMove {
        if cell == self.cell {
            return HintMove::default();
        }
        self.cell = cell;
        self.generation += 1;
        HintMove {
            rearm: cell.map(|_| self.generation),
            disarm: cell.is_none(),
            repaint: self.shown.take().is_some(),
        }
    }

    /// 无条件撤销(按下鼠标、鼠标离开窗口)。返回要不要一帧把挂着的提示擦掉。
    pub fn cancel(&mut self) -> bool {
        self.on_move(None).repaint
    }

    /// 停留计时到点:代号对得上、还没探过,就返回该探的那一格。
    pub fn elapsed(&self, generation: u64) -> Option<(usize, usize)> {
        (generation == self.generation && self.shown.is_none())
            .then_some(self.cell)
            .flatten()
    }

    /// 探到了链接,挂上提示。代号已经过期(探的途中鼠标又动了)就不挂。
    pub fn show(
        &mut self,
        generation: u64,
        link: HoveredLink,
        display_offset: usize,
        content_generation: u64,
    ) -> bool {
        if generation != self.generation || self.cell.is_none() {
            return false;
        }
        self.shown = Some(ShownHint {
            link,
            display_offset,
            content_generation,
        });
        true
    }

    /// 挂着的提示在**这一帧**还算不算数(paint 里调)。
    ///
    /// 回看位置变了直接撤;内容代数变了用 `reprobe` 重探同一格,还是同一个目标才留着
    /// (段落可能换了行,照新的画),否则撤掉 —— 不许借着输出滚动跳到别的链接上。
    pub fn current(
        &mut self,
        display_offset: usize,
        content_generation: u64,
        reprobe: impl FnOnce((usize, usize)) -> Option<HoveredLink>,
    ) -> Option<HoveredLink> {
        let shown = self.shown.as_mut()?;
        if shown.display_offset != display_offset {
            self.shown = None;
            return None;
        }
        if shown.content_generation != content_generation {
            let cell = self.cell?;
            match reprobe(cell) {
                Some(link) if link.target == shown.link.target => {
                    shown.link = link;
                    shown.content_generation = content_generation;
                }
                _ => {
                    self.shown = None;
                    return None;
                }
            }
        }
        self.shown.as_ref().map(|shown| shown.link.clone())
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

    fn url_link(url: &str, column: usize) -> HoveredLink {
        HoveredLink {
            target: LinkTarget::Url {
                url: url.into(),
                reveal: false,
            },
            spans: vec![LinkSpan {
                line: 0,
                start: column,
                end: column + 5,
            }],
        }
    }

    /// 停留才探:同一格里晃不重新计时;换格换代号、旧代号到点作废;不该提示的位置撤计时器。
    #[test]
    fn 悬停提示停留才探且移动即取消() {
        let mut hint = LinkHint::default();
        let first = hint.on_move(Some((3, 1)));
        let ticket = first.rearm.expect("进了一格就该起计时");
        assert!(!first.repaint, "什么都没挂着,不要帧");
        assert_eq!(
            hint.on_move(Some((3, 1))),
            HintMove::default(),
            "同一格不算移动"
        );
        assert_eq!(hint.elapsed(ticket), Some((3, 1)));

        // 换了一格:旧计时器回来也认不上
        let second = hint.on_move(Some((4, 1))).rearm.unwrap();
        assert_ne!(second, ticket);
        assert_eq!(hint.elapsed(ticket), None);
        assert!(
            !hint.show(ticket, url_link("https://a.com", 4), 0, 1),
            "过期的探测结果不挂"
        );

        // 不该提示的位置(离开元素、按住 Ctrl、TUI 抓鼠标……):撤计时器
        let gone = hint.on_move(None);
        assert_eq!(gone.rearm, None);
        assert!(gone.disarm);
        assert_eq!(hint.elapsed(second), None);
    }

    /// 只在提示出现 / 消失时要帧:挂着时一动就撤并要一帧,没挂着时随便晃都不要。
    #[test]
    fn 悬停提示只在消失时要帧() {
        let mut hint = LinkHint::default();
        let ticket = hint.on_move(Some((3, 1))).rearm.unwrap();
        assert!(hint.show(ticket, url_link("https://a.com", 0), 0, 7));
        assert_eq!(hint.elapsed(ticket), None, "已经挂上了,不再探第二次");
        assert!(hint.on_move(Some((4, 1))).repaint);
        assert!(!hint.on_move(Some((5, 1))).repaint);

        let ticket = hint.on_move(Some((6, 1))).rearm.unwrap();
        hint.show(ticket, url_link("https://a.com", 0), 0, 7);
        assert!(hint.cancel(), "按下鼠标撤掉挂着的提示要一帧");
        assert!(!hint.cancel(), "撤过了就不再要");
    }

    /// 内容没变不重探;变了重探,还是同一个链接就跟着新位置画,换了链接或回看位置变了就撤。
    #[test]
    fn 悬停提示挂着时按内容代数重探() {
        let mut hint = LinkHint::default();
        let ticket = hint.on_move(Some((3, 1))).rearm.unwrap();
        hint.show(ticket, url_link("https://a.com", 0), 0, 7);

        let same = hint.current(0, 7, |_| panic!("内容没变不该重探"));
        assert_eq!(same, Some(url_link("https://a.com", 0)));

        // 新输出把同一个链接挪了位置:留着,按新的段画
        let moved = hint.current(0, 8, |cell| {
            assert_eq!(cell, (3, 1));
            Some(url_link("https://a.com", 2))
        });
        assert_eq!(moved, Some(url_link("https://a.com", 2)));
        assert_eq!(
            hint.current(0, 8, |_| None),
            Some(url_link("https://a.com", 2))
        );

        // 输出把别的链接顶到了鼠标底下:撤,不许跳过去
        assert_eq!(
            hint.current(0, 9, |_| Some(url_link("https://b.com", 2))),
            None
        );
        assert_eq!(hint.current(0, 9, |_| None), None, "撤了就是撤了");

        // 回看位置变了(滚轮):直接撤
        let ticket = hint.on_move(Some((4, 1))).rearm.unwrap();
        hint.show(ticket, url_link("https://a.com", 0), 0, 9);
        assert_eq!(hint.current(3, 9, |_| panic!("滚动不重探")), None);
    }

    #[test]
    fn 宿主拿到的链接只留目标() {
        let url = LinkTarget::Url {
            url: "https://a.com".into(),
            reveal: true,
        };
        assert_eq!(
            TerminalLink::from(url),
            TerminalLink::Url("https://a.com".into())
        );
        let file = FileLink {
            path: PathBuf::from("a.rs"),
            line: Some(3),
            end_line: None,
            column: None,
        };
        assert_eq!(
            TerminalLink::from(LinkTarget::File(file.clone())),
            TerminalLink::File(file)
        );
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
