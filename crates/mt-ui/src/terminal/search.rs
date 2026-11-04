//! 终端内查找(Ctrl+F)—— 搜索引擎。
//!
//! 对应旧版 `src/utils/terminalSearch.ts` + xterm.js 的 `@xterm/addon-search`:
//! 三种匹配模式(字面 / 区分大小写 / 正则)× 整词开关、全 buffer 命中枚举与计数、
//! 环形的上一个/下一个、跳转时把命中滚进视口。
//!
//! # 建在 alacritty 的哪套设施上
//!
//! `alacritty_terminal::term::search` 提供的可用面只有三样:
//!
//! | API | 用途 |
//! |---|---|
//! | [`RegexSearch::new`] | 编译正则(内部是 4 个 lazy DFA:左右方向 × 找头/找尾) |
//! | [`RegexIter`] | 在 `[start, end]` 区间内**顺序枚举**命中,一次一个 `RangeInclusive<Point>` |
//! | `Term::search_next` | 从某点找下一个命中(**本模块没用**,理由见下) |
//!
//! `Term::search_next` 看着最顺手,但它只回一条命中、给不出总数,而查找条要显示
//! 「n/总数」。所以这里统一走 [`RegexIter`] 把区间内的命中**一次枚举完**,
//! next/prev 退化成对 `Vec` 的环形推进 —— 计数、跳转、高亮三件事共用一份结果集,
//! 不会出现「计数说 12 条、跳转却跳到第 13 条」这种两套口径打架的经典 bug。
//!
//! ## 大小写:必须绕开 alacritty 的 smart case
//!
//! [`RegexSearch::new`] 里写死了一句
//! `SyntaxConfig::new().case_insensitive(!has_uppercase)` —— 即 **smart case**:
//! 关键词里有大写就区分大小写,没有就不区分。而查找条上的 `Aa` 是一个**显式开关**,
//! 用户按下去就得区分,不管关键词长什么样。
//!
//! 绕法是在模式串前面加内联标志:`(?i)` / `(?-i)`。内联标志优先级高于
//! `SyntaxConfig` 里的默认值,于是 smart case 被彻底架空,两个方向都能钉死。
//!
//! ## 整词:自己判边界,不用 `\b`
//!
//! 正则的 `\b` 在 regex-automata 的 DFA 引擎上要额外开
//! `Config::unicode_word_boundary`,alacritty 没开;而且 `\b` 与「字面模式下要先转义」
//! 叠在一起容易出边界怪事。所以整词走**命中两侧邻格的字符判定**
//! ([`whole_word_ok`]),与 xterm SearchAddon 的 `_isWholeWord` 同口径:
//! 左右邻格要么不存在(行首/行尾),要么不是词字符。
//!
//! # 重搜策略(为什么不是每帧全量重搜)
//!
//! 一次全 buffer 扫描是 O(scrollback × 列数)。本项目 scrollback 默认一万行,
//! 用户还能开到十万 —— 每帧扫一遍是不可能的(实测 5 万行 × 120 列搜一个稀有词
//! 全扫一次 55ms,期间 grid 锁一直攥着:UI 线程与 PTY reader 线程一起停)。
//! 四道闸按顺序拦:
//!
//! 1. **脏标记**:关键词 / 选项变了 → 立刻**全扫**(用户在等结果,不能拖)。
//! 2. **去抖**:内容变化引起的重搜最快 200ms 一次(与 xterm SearchAddon
//!    `_updateMatches` 的 200ms 完全一致);上一次扫描越贵,间隔按比例放大
//!    (封顶 2s,见 `TerminalSearch::debounce`)。被挡下、且内容确实动过的那次
//!    由宿主排一发延后重绘兜底([`TerminalSearch::take_trailing_rescan`]),输出停在
//!    去抖窗口里也不会让结果永远停在旧样子。
//! 3. **变化检测**:去抖到期后先比内容代数(`TerminalEmulator::generation`,
//!    每批输出 +1)与屏幕内容哈希([`content_fingerprint`]),都没变就直接跳过 ——
//!    空闲时(最常见)一次扫描都不会发生。代数补的是指纹的盲区:周期性输出恰好
//!    滚过整数个周期时屏幕逐字相同、历史满了总行数也不变,光看指纹会漏掉新内容。
//! 4. **增量重扫**:内容变化引起的重扫**只扫变了的行**,见下一节。
//!
//! 指纹**不含 display_offset**,用户滚动回看不会白白触发重搜。
//!
//! # 增量重扫与它为什么是对的
//!
//! alacritty 的正则迭代在「非折行的行尾」一定会重置 DFA(`regex_search_internal`
//! 的 linebreak 分支),命中不跨逻辑行(= 被 WRAPLINE 连起来的若干物理行);
//! 整词判定只看同一行的邻格。所以**一条逻辑行上的命中只取决于这条逻辑行自己的
//! 内容**,与它上面扫过什么无关 —— 从逻辑行首单独扫它,和全扫走到这里得到的
//! 结果逐条相同。增量重扫就建在这条性质上:
//!
//! - 每次扫描都给整个 grid 记一份**逐行内容哈希**(`ScanSnapshot`:字符 + 影响
//!   匹配的格标志,含行尾 WRAPLINE);
//! - 下次先算出新 grid 的逐行哈希,按「新第 i 行 = 旧第 i + dropped 行」对齐
//!   (`dropped` = 回滚区满了之后顶部被挤掉的行数,见 `plan_rescan`),从顶部往下
//!   **逐行核对**到第一处不一致为止;
//! - 核对一致的那一段整段沿用旧命中(行号整体平移),从第一处不一致所在逻辑行的
//!   行首一直重扫到底部;顶部挤掉过行时,首条逻辑行可能被截了头,也单独重扫。
//!
//! 对不齐的一律退回全扫:列数 / 屏幕行数变了(resize 会 reflow,行结构整个变了)、
//! 进出 alt screen、历史变少(清屏 `ESC[3J` / RIS / 回滚上限调小)、上次结果已经
//! 封顶(被截掉的那部分命中不在手上)、限定了 `max_scan_lines`、找不到对齐点。
//! 就算对齐点找错了(内容高度重复时可能),核对是逐行比内容的 —— 错的对齐只会让
//! 核对早早失配、重扫范围变大,不会留下错的命中。
//!
//! 全扫的结果封顶时连快照都不抓:下一次反正只能再全扫,这份要逐格哈希整个回滚区的
//! 快照抓了也用不上,而那段时间 grid 锁一直攥着。结果掉回上限以下的那一轮才抓,再下一轮
//! 恢复增量。
//!
//! 命中条数上限 [`SearchLimits::max_matches`] 默认 1000,同样照抄 xterm 的
//! `_highlightLimit` —— `grep -o` 式的关键词(比如一个空格)不会把内存与绘制打爆。
//!
//! **取舍留档**:第 1 条(打字重搜)刻意**不去抖** —— 与旧版一样,每敲一个字母
//! 都是一次全 buffer 扫描。scrollback 开到十万行时这一下是有感的;真嫌慢的话
//! 调 [`SearchLimits::max_scan_lines`] 换成「只搜最近 N 行」,而不是给打字加延迟
//! (加了延迟就会出现「已经不打字了计数还在跳」,比慢半拍更难受)。
//!
//! # 与宿主的接法
//!
//! 引擎实例由宿主持有一份 `Rc<RefCell<TerminalSearch>>`,**同时**交给
//! [`TerminalView`](super::TerminalView)(渲染高亮)和
//! [`TerminalSearchBar`](super::TerminalSearchBar)(改关键词/翻页)。
//! 两边共用同一份状态,计数与高亮天然同步,不需要任何回调对账。
//! 完整接线清单见 [`super::search_bar`] 的模块注释。
//!
//! 渲染层每帧(prepaint)调 [`TerminalSearch::frame_sync`],按返回值办两件事:
//! 结果变了就再要一帧(查找条的计数在同一帧的 render 里已经读过、是扫描前的),
//! 被去抖挡下就排一发兜底重扫。两件都不能省 —— pane 套着 view 级缓存,
//! 窗口别处的重绘不会顺手替它补上(见 [`FrameSync`])。

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::{Duration, Instant};

use alacritty_terminal::grid::{Dimensions as _, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::search::{RegexIter, RegexSearch};
use alacritty_terminal::term::{Term, TermMode};
use mt_terminal::TerminalEmulator;

// ---------------------------------------------------------------------------
// 选项与纯函数(全部可单测)
// ---------------------------------------------------------------------------

/// 查找条上三个开关的状态。与旧版 `SearchState` 的同名字段一一对应。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SearchOptions {
    /// `Aa`:区分大小写。关掉时**一律**不区分(不是 smart case)。
    pub case_sensitive: bool,
    /// `.*`:把关键词当正则,而不是字面量。
    pub regex: bool,
    /// `ab`:整词匹配。
    pub whole_word: bool,
}

/// 翻页方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchDirection {
    Next,
    Previous,
}

/// 扫描规模的封顶参数。默认值照抄 xterm SearchAddon。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchLimits {
    /// 最多收多少条命中。超过就停止扫描(计数会停在这个数上)。
    pub max_matches: usize,
    /// 内容变化引起的重搜的最小间隔。
    pub debounce: Duration,
    /// 只扫最近 N 行(含屏幕)。`None` = 整个 buffer。
    ///
    /// 十万行 scrollback + 复杂正则时可以用它换响应速度,代价是搜不到更早的历史。
    pub max_scan_lines: Option<usize>,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            max_matches: 1000,
            debounce: Duration::from_millis(200),
            max_scan_lines: None,
        }
    }
}

/// 渲染层一帧 sync 完该做的事([`TerminalSearch::frame_sync`] 的返回值)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameSync {
    /// 结果集(命中 / 当前命中)这一帧变了,宿主要**再画一帧**。
    ///
    /// 内容变化引起的重扫发生在终端元素的 prepaint 里,而查找条的「n/总数」早在
    /// 同一帧的 render 阶段就读过了 —— 这一帧画出去的计数是扫描**之前**的。
    /// 不再要一帧的话,计数要等别的事碰巧重画这个 pane 才跟上;pane 套着 view 级
    /// 缓存时,窗口别处的重绘碰不到它(输出停了,计数就一直停在旧值)。
    pub repaint: bool,
    /// 被去抖挡下、内容确实动过:这么久之后要再 sync 一次(兜底重扫,
    /// 见 [`TerminalSearch::take_trailing_rescan`])。
    pub rescan_after: Option<Duration>,
}

/// 自适应去抖:内容变化引起的重搜间隔至少是上一次扫描耗时的这么多倍 ——
/// 把「持锁扫描」压在 UI 线程时间的 1/8 以内。
const ADAPTIVE_DEBOUNCE_FACTOR: u32 = 8;

/// 自适应去抖的上限。再长命中高亮就会明显跟不上滚动的输出了。
const ADAPTIVE_DEBOUNCE_CAP: Duration = Duration::from_secs(2);

/// 一条命中:grid 坐标上的闭区间。
///
/// `Point::line` 是**grid 绝对行号**(0 = 屏幕第一行,负数 = 回看缓冲),
/// 与 display_offset 无关 —— 但新输出把内容顶进 scrollback 时行号会整体减小,
/// 所以命中集合在内容变化后必须重建,不能跨帧长留。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub start: Point,
    pub end: Point,
}

/// 一个格子的高亮档位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HighlightKind {
    /// 普通命中。
    Match,
    /// 当前命中(「n/总数」里的那个 n)。
    Current,
}

impl HighlightKind {
    /// 进行签名用的判别码(0 留给「没有高亮」)。
    pub fn code(self) -> u8 {
        match self {
            HighlightKind::Match => 1,
            HighlightKind::Current => 2,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(HighlightKind::Match),
            2 => Some(HighlightKind::Current),
            _ => None,
        }
    }
}

/// 一行之内的一段高亮(闭区间列号)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub kind: HighlightKind,
}

/// 供渲染层按行查询的命中索引。
///
/// 渲染层是**逐格**问「这一格要不要高亮」的,一屏上万次;直接对着
/// `Vec<SearchMatch>`(最多 1000 条)线性扫是一千万次比较。所以这里按行拍平成
/// `line → 段`,渲染层换行时取一次 [`Self::row`],行内再线性扫(通常 0~2 段)。
#[derive(Clone, Debug, Default)]
pub struct SearchHighlights {
    rows: HashMap<i32, Vec<HighlightSpan>>,
    revision: u64,
    matches: usize,
}

const NO_SPANS: &[HighlightSpan] = &[];

impl SearchHighlights {
    /// 结果集版本号。每次命中集合或当前命中变化都会 +1。
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 命中总条数(不是段数:跨行的命中会拆成多段)。
    pub fn matches(&self) -> usize {
        self.matches
    }

    /// 某一 grid 行上的全部高亮段。没有就是空切片。
    pub fn row(&self, line: i32) -> &[HighlightSpan] {
        self.rows.get(&line).map(|v| v.as_slice()).unwrap_or(NO_SPANS)
    }

    /// 单格查询(诊断 / 测试用;渲染热路径请走 [`Self::row`])。
    pub fn kind_at(&self, line: i32, column: usize) -> Option<HighlightKind> {
        self.row(line)
            .iter()
            .find(|s| column >= s.start && column <= s.end)
            .map(|s| s.kind)
    }
}

/// 正则元字符表。与 `regex_syntax::is_meta_character` 逐字一致 ——
/// 字面模式的转义必须与真正的正则语法同源,少一个就是「搜 `a.b` 却匹配上 `axb`」。
fn is_meta_character(c: char) -> bool {
    matches!(
        c,
        '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '#'
            | '&' | '-' | '~'
    )
}

/// 把字面关键词转义成等价的正则。
pub fn escape_literal(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for c in query.chars() {
        if is_meta_character(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// 关键词 + 选项 → 交给 [`RegexSearch::new`] 的模式串。
///
/// 前缀 `(?i)` / `(?-i)` 是**必须**的:不加就会落回 alacritty 的 smart case
/// (见模块注释)。整词不在这里做,见 [`whole_word_ok`]。
pub fn build_pattern(query: &str, options: SearchOptions) -> String {
    let body = if options.regex {
        query.to_string()
    } else {
        escape_literal(query)
    };
    let flag = if options.case_sensitive { "(?-i)" } else { "(?i)" };
    format!("{flag}{body}")
}

/// 词字符判定。字母 / 数字 / 下划线算词字符,其余(空格、标点、CJK 标点)不算。
///
/// xterm 用的是一张 ASCII 标点黑名单,对 CJK 的结论与这里一致(汉字算词字符);
/// 差别只在非 ASCII 标点上,这里更准。
pub fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// 整词判定:命中两侧的邻格都不能是词字符。`None` = 行首/行尾,算边界。
pub fn whole_word_ok(before: Option<char>, after: Option<char>) -> bool {
    !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
}

/// 环形推进当前命中序号。`count == 0` 时永远是 `None`。
///
/// 还没有当前命中时:向下取第一条,向上取最后一条 —— 与所有查找条一致。
pub fn advance_index(
    current: Option<usize>,
    count: usize,
    direction: SearchDirection,
) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match (current, direction) {
        (None, SearchDirection::Next) => 0,
        (None, SearchDirection::Previous) => count - 1,
        (Some(i), SearchDirection::Next) => (i + 1) % count,
        (Some(i), SearchDirection::Previous) => (i + count - 1) % count,
    })
}

/// 在**已排好序**的命中行号里,找第一条落在 `anchor` 行或其后的;
/// 都在 anchor 之前就绕回第一条。
///
/// 重搜之后重新挑当前命中用的:锚点取视口顶部,于是「改了个字母」之后高亮
/// 停在眼前而不是跳到十万行以外的第一条 —— 对应 xterm 的 `incremental: true`。
pub fn index_at_or_after(starts: &[i32], anchor: i32) -> Option<usize> {
    if starts.is_empty() {
        return None;
    }
    Some(
        starts
            .iter()
            .position(|line| *line >= anchor)
            .unwrap_or(0),
    )
}

// ---------------------------------------------------------------------------
// 引擎
// ---------------------------------------------------------------------------

/// 终端查找引擎。一个终端一份,由宿主用 `Rc<RefCell<_>>` 同时交给渲染层与查找条。
pub struct TerminalSearch {
    query: String,
    options: SearchOptions,
    limits: SearchLimits,
    /// 已编译的 DFA 组与它对应的模式串。模式串没变就复用 —— `RegexSearch::new`
    /// 要编 4 个 DFA,放在每次输入回调里跑会明显发涩。
    compiled: Option<(String, RegexSearch)>,
    error: Option<String>,
    matches: Vec<SearchMatch>,
    current: Option<usize>,
    highlights: Rc<SearchHighlights>,
    revision: u64,
    /// 关键词 / 选项变过,下一次 sync 必须无条件重搜。
    dirty: bool,
    last_scan: Option<Instant>,
    last_fingerprint: u64,
    /// 上次扫描时的内容代数(`TerminalEmulator::generation`)。
    last_generation: u64,
    /// 上次扫描时 grid 的逐行快照,增量重扫靠它。`None` = 下一次只能全扫。
    snapshot: Option<ScanSnapshot>,
    /// 上次扫描(全扫或增量)持锁的耗时 —— 自适应去抖按它放大间隔。
    last_cost: Duration,
    /// 最近一次 sync 因为没到去抖点被挡下(内容可能变了、还没扫)。
    deferred: bool,
    /// 兜底重绘已经排上了、还没到点(防止每帧都排一发)。
    trailing_armed: bool,
    /// 走了增量路径的扫描次数(诊断 / 单测确认增量真的被走到)。
    incremental_scans: u64,
    /// 上次扫描时的列数,拆行高亮段要用。
    columns: usize,
    /// 查找条是否开着。**关掉只停高亮与计数,关键词与选项原样留着** ——
    /// 旧版 `closeTerminalSearch` 也只清 ptyId / 计数,`query` 留在 store 里,
    /// 于是「排查同一个报错时连开几次 Ctrl+F」不用重打关键词。
    enabled: bool,
}

impl Default for TerminalSearch {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalSearch {
    pub fn new() -> Self {
        Self::with_limits(SearchLimits::default())
    }

    pub fn with_limits(limits: SearchLimits) -> Self {
        Self {
            query: String::new(),
            options: SearchOptions::default(),
            limits,
            compiled: None,
            error: None,
            matches: Vec::new(),
            current: None,
            highlights: Rc::new(SearchHighlights::default()),
            revision: 0,
            dirty: false,
            last_scan: None,
            last_fingerprint: 0,
            last_generation: 0,
            snapshot: None,
            last_cost: Duration::ZERO,
            deferred: false,
            trailing_armed: false,
            incremental_scans: 0,
            columns: 0,
            enabled: true,
        }
    }

    /// 查找条是否开着。见 [`Self::set_enabled`]。
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 开 / 关查找条。关掉时命中集合与高亮立刻清空,关键词与三个开关保留;
    /// 重新打开会强制重搜一遍(内容可能早变了)。
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        if enabled {
            self.dirty = true;
        } else {
            self.dirty = false;
            self.last_scan = None;
            self.snapshot = None;
            self.deferred = false;
            self.commit(Vec::new(), None);
        }
    }

    pub fn limits(&self) -> SearchLimits {
        self.limits
    }

    pub fn set_limits(&mut self, limits: SearchLimits) {
        self.limits = limits;
        self.dirty = true;
        self.snapshot = None;
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// 换关键词。返回是否真的变了 —— 没变就别去触发重搜。
    pub fn set_query(&mut self, query: impl Into<String>) -> bool {
        let query = query.into();
        if self.query == query {
            return false;
        }
        self.query = query;
        self.dirty = true;
        true
    }

    pub fn options(&self) -> SearchOptions {
        self.options
    }

    pub fn set_options(&mut self, options: SearchOptions) -> bool {
        if self.options == options {
            return false;
        }
        self.options = options;
        self.dirty = true;
        true
    }

    /// 正则语法错时的原始错误文本。字面模式下永远是 `None`。
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 查找条开着**且**关键词非空才真正在搜。关键词为空时既不高亮也不计数
    /// (与旧版 `resultCount < 0` 同义)。
    pub fn is_active(&self) -> bool {
        self.enabled && !self.query.is_empty()
    }

    pub fn matches(&self) -> &[SearchMatch] {
        &self.matches
    }

    pub fn count(&self) -> usize {
        self.matches.len()
    }

    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    pub fn current_match(&self) -> Option<SearchMatch> {
        self.current.and_then(|i| self.matches.get(i)).copied()
    }

    /// 结果集版本号,每次命中集合 / 当前命中变化 +1。
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// 供渲染层用的按行高亮索引。**每帧拿到的都是同一个 `Rc`**,
    /// 结果没变就连引用计数都不必动。
    pub fn highlights(&self) -> Rc<SearchHighlights> {
        self.highlights.clone()
    }

    /// 查找条上「n/总数」那一格的文案素材:1-based 序号(没有当前命中时是 0)。
    pub fn display_index(&self) -> usize {
        self.current.map(|i| i + 1).unwrap_or(0)
    }

    /// 彻底重置:清关键词、清结果、清高亮。选项(Aa/ab/.*)**保留**。
    ///
    /// 收起查找条请用 [`Self::set_enabled`] —— 那条会留住关键词。
    /// 这一条是给「pane 关了 / 换了终端」用的。
    pub fn clear(&mut self) {
        self.query.clear();
        self.compiled = None;
        self.error = None;
        self.dirty = false;
        self.last_scan = None;
        self.last_fingerprint = 0;
        self.snapshot = None;
        self.deferred = false;
        self.commit(Vec::new(), None);
    }

    // -- 扫描 ---------------------------------------------------------------

    /// 每帧调一次(渲染层已经替宿主调了)。必要时重搜,返回结果集是否变化。
    pub fn sync(&mut self, emulator: &TerminalEmulator) -> bool {
        self.sync_at(emulator, Instant::now())
    }

    /// 渲染层每帧(prepaint)的入口:[`Self::sync`] 一次,再把宿主要做的两件事
    /// 一并交代出来,见 [`FrameSync`]。
    pub fn frame_sync(&mut self, emulator: &TerminalEmulator, now: Instant) -> FrameSync {
        let repaint = self.sync_at(emulator, now);
        FrameSync {
            repaint,
            rescan_after: self.take_trailing_rescan(now),
        }
    }

    /// [`Self::sync`] 的可注入时钟版本,单测用。
    pub fn sync_at(&mut self, emulator: &TerminalEmulator, now: Instant) -> bool {
        if !self.is_active() {
            self.deferred = false;
            return if self.matches.is_empty() {
                false
            } else {
                self.commit(Vec::new(), None);
                true
            };
        }
        if self.dirty {
            return self.scan(emulator, now, false);
        }
        // 去抖:内容变化引起的重搜最快 200ms 一次(上次扫描贵的话更久)
        let due = self
            .last_scan
            .map(|t| now.saturating_duration_since(t) >= self.debounce())
            .unwrap_or(true);
        if !due {
            // 只有内容真动过(代数变了)才算欠一次重扫。不这么判的话,扫完紧跟着的
            // 那一帧(结果变了要求的重画、滚轮、键入回显)都会白排一发兜底重绘。
            // 代数是原子量,不拿 term 锁;所有改内容的路径(推进字节 / resize /
            // 改回滚行数)都会推进它,`with_term_mut` 只动回看位置与选区
            self.deferred = emulator.generation() != self.last_generation;
            return false;
        }
        self.deferred = false;
        let (fingerprint, generation) =
            emulator.with_term(|term| (content_fingerprint(term), emulator.generation()));
        if fingerprint == self.last_fingerprint && generation == self.last_generation {
            // 内容一个字没变:把计时重置掉,下一轮 200ms 后再问一次
            self.last_scan = Some(now);
            return false;
        }
        self.scan(emulator, now, true)
    }

    /// 无条件立刻重搜(关键词 / 选项刚改完时用)。返回结果集是否变化。
    pub fn refresh(&mut self, emulator: &TerminalEmulator) -> bool {
        self.dirty = true;
        self.sync_at(emulator, Instant::now())
    }

    /// 内容变化引起的重搜的实际间隔:至少 [`SearchLimits::debounce`];上一次扫描
    /// 越贵间隔越长(耗时 × [`ADAPTIVE_DEBOUNCE_FACTOR`],封顶
    /// [`ADAPTIVE_DEBOUNCE_CAP`])—— 退回全扫的那些场合(结果已封顶、刚 resize)
    /// 一次几十毫秒,输出不停时按 200ms 一轮追着扫就是把 UI 线程让出去一大半。
    fn debounce(&self) -> Duration {
        let adaptive = self
            .last_cost
            .saturating_mul(ADAPTIVE_DEBOUNCE_FACTOR)
            .min(ADAPTIVE_DEBOUNCE_CAP);
        self.limits.debounce.max(adaptive)
    }

    /// 最近一次 [`Self::sync`] 因为没到去抖点被挡下、而兜底重绘还没排:返回距
    /// 去抖点还有多久,并记下「已排」。宿主(渲染层)据此排一发延后重绘,到点再
    /// sync 一次 —— 否则输出恰好停在去抖窗口里时,最后那批内容要等到别的事碰巧
    /// 触发一帧才会被扫到(命中与计数一直停在旧样子)。
    ///
    /// 到点后宿主调 [`Self::trailing_rescan_fired`] 解除「已排」。
    pub fn take_trailing_rescan(&mut self, now: Instant) -> Option<Duration> {
        if !self.deferred || self.trailing_armed || !self.is_active() {
            return None;
        }
        let last = self.last_scan?;
        self.trailing_armed = true;
        Some((last + self.debounce()).saturating_duration_since(now))
    }

    /// 兜底重绘的定时器到点了。见 [`Self::take_trailing_rescan`]。
    pub fn trailing_rescan_fired(&mut self) {
        self.trailing_armed = false;
    }

    /// 扫一遍。`incremental` = 允许按上次的快照只扫变了的行(内容变化引起的
    /// 重搜);关键词 / 选项变了必须全扫。
    fn scan(&mut self, emulator: &TerminalEmulator, now: Instant, incremental: bool) -> bool {
        self.dirty = false;
        self.last_scan = Some(now);
        self.deferred = false;

        if !self.ensure_compiled() {
            let (fingerprint, generation) =
                emulator.with_term(|term| (content_fingerprint(term), emulator.generation()));
            self.last_fingerprint = fingerprint;
            self.last_generation = generation;
            self.snapshot = None;
            let changed = !self.matches.is_empty();
            self.commit(Vec::new(), None);
            return changed;
        }

        // 借出 DFA:`with_term` 的闭包要 `&mut RegexSearch`,而 self 的其它字段
        // 之后还要写 —— 先把需要的标量拷出来,别让闭包捕获整个 self。
        let options = self.options;
        let limits = self.limits;
        let previous = self.current_match().map(|m| m.start);
        // 增量的前提:上次是完整结果(没封顶 —— 截掉的那部分命中不在手上)、
        // 没限定扫描窗口(窗口下沿随输出移动,旧命中会从窗口顶部漏出去)
        let windowed = limits.max_scan_lines.is_some();
        let existing = self.snapshot.take();
        let can_increment = incremental && !windowed && self.matches.len() < limits.max_matches;
        let scrollback = emulator.scrollback();
        let old_matches = &self.matches;

        let Some((_, dfa)) = self.compiled.as_mut() else {
            return false;
        };
        let started = Instant::now();
        let (found, used_plan, snapshot, fingerprint, generation, columns, anchor) = emulator
            .with_term(|term| {
                let generation = emulator.generation();
                let fingerprint = content_fingerprint(term);
                // 内容没动过(只是关键词 / 选项变了)的快照原样留用:逐行哈希只描述内容、
                // 与关键词无关,打字时每敲一个字母都重算一遍是白花(5 万行十几毫秒)
                let (prior, reusable) = match existing {
                    Some(old) if old.generation == generation && old.fingerprint == fingerprint => {
                        (None, Some(old))
                    }
                    other => (other.filter(|_| can_increment), None),
                };
                // 能增量才要先抓新快照:计划靠它逐行核对。限定了扫描窗口就用不上快照,
                // 省掉逐行哈希
                let mut snapshot = if windowed {
                    None
                } else {
                    reusable.or_else(|| {
                        prior
                            .as_ref()
                            .map(|_| ScanSnapshot::capture(term, generation, fingerprint))
                    })
                };
                let plan = match (prior.as_ref(), snapshot.as_ref()) {
                    (Some(old), Some(new)) => {
                        plan_rescan(old, new, scrollback, |i| row_wrapped(term, new, i))
                    }
                    _ => None,
                };
                let used_plan = plan.is_some();
                let found = match plan {
                    Some(plan) => {
                        rescan_matches(term, dfa, options, limits.max_matches, &plan, old_matches)
                    }
                    None => collect_matches(term, dfa, options, &limits),
                };
                // 不能增量的路径收完命中才定:封顶了下一次反正只能全扫,这份要逐格哈希整个
                // 回滚区的快照抓了也用不上。还在同一次持锁里,内容与刚才收命中时是同一份
                if snapshot.is_none() && !windowed && found.len() < limits.max_matches {
                    snapshot = Some(ScanSnapshot::capture(term, generation, fingerprint));
                }
                let anchor = -(term.grid().display_offset() as i32);
                (
                    found,
                    used_plan,
                    snapshot,
                    fingerprint,
                    generation,
                    term.columns(),
                    anchor,
                )
            });
        self.last_cost = started.elapsed();
        if used_plan {
            self.incremental_scans += 1;
        }

        self.snapshot = snapshot;
        self.last_fingerprint = fingerprint;
        self.last_generation = generation;
        self.columns = columns;

        // 当前命中的接续:老位置还在就留着,否则挑视口顶部往下的第一条
        // (= xterm 的 incremental 语义)。
        let starts: Vec<i32> = found.iter().map(|m| m.start.line.0).collect();
        let next_current = previous
            .and_then(|p| found.iter().position(|m| m.start == p))
            .or_else(|| index_at_or_after(&starts, anchor));

        let changed = found != self.matches || next_current != self.current;
        self.commit(found, next_current);
        changed
    }

    /// 编译(必要时)。返回是否有可用的 DFA。
    fn ensure_compiled(&mut self) -> bool {
        let pattern = build_pattern(&self.query, self.options);
        if self.compiled.as_ref().is_some_and(|(p, _)| *p == pattern) {
            return true;
        }
        match RegexSearch::new(&pattern) {
            Ok(dfa) => {
                self.compiled = Some((pattern, dfa));
                self.error = None;
                true
            }
            Err(err) => {
                self.compiled = None;
                self.error = Some(err.to_string());
                false
            }
        }
    }

    /// 落地一份新结果集:重建高亮索引、推进版本号。
    fn commit(&mut self, matches: Vec<SearchMatch>, current: Option<usize>) {
        self.matches = matches;
        self.current = current.filter(|i| *i < self.matches.len());
        self.revision = self.revision.wrapping_add(1);
        self.highlights = Rc::new(build_highlights(
            &self.matches,
            self.current,
            self.columns,
            self.revision,
        ));
    }

    // -- 翻页 ---------------------------------------------------------------

    /// 下一个命中(环形)。会先把结果集刷新到最新,然后滚动到命中处。
    pub fn find_next(&mut self, emulator: &TerminalEmulator) -> Option<SearchMatch> {
        self.step(emulator, SearchDirection::Next)
    }

    /// 上一个命中(环形)。
    pub fn find_previous(&mut self, emulator: &TerminalEmulator) -> Option<SearchMatch> {
        self.step(emulator, SearchDirection::Previous)
    }

    fn step(
        &mut self,
        emulator: &TerminalEmulator,
        direction: SearchDirection,
    ) -> Option<SearchMatch> {
        self.sync(emulator);
        let next = advance_index(self.current, self.matches.len(), direction)?;
        self.set_current(next);
        self.scroll_to_current(emulator);
        self.current_match()
    }

    /// 直接指定当前命中(点计数、从外部跳转时用)。
    pub fn set_current(&mut self, index: usize) {
        if index >= self.matches.len() || self.current == Some(index) {
            return;
        }
        let matches = std::mem::take(&mut self.matches);
        self.commit(matches, Some(index));
    }

    /// 把当前命中滚进视口。**已经在视口里就一动不动** —— 与 xterm
    /// `_selectResult` 同款:只有滚出去了才滚回来,并把命中放在视口中间。
    pub fn scroll_to_current(&self, emulator: &TerminalEmulator) {
        let Some(m) = self.current_match() else {
            return;
        };
        emulator.with_term_mut(|term| {
            let screen_lines = term.screen_lines() as i32;
            let offset = term.grid().display_offset() as i32;
            let row = m.start.line.0 + offset;
            if row >= 0 && row < screen_lines {
                return;
            }
            let history = term.history_size() as i32;
            let target = (screen_lines / 2 - m.start.line.0).clamp(0, history);
            let delta = target - offset;
            if delta != 0 {
                term.scroll_display(Scroll::Delta(delta));
            }
        });
    }
}

// ---------------------------------------------------------------------------
// grid 侧的实现细节
// ---------------------------------------------------------------------------

/// 屏幕内容指纹。**不含 scrollback、不含 display_offset**:
/// 前者是为了 O(一屏) 的代价,后者是为了让「用户滚动回看」不触发重搜。
///
/// 总行数一起进哈希:内容相同但历史长度变了(刚开始积累 scrollback)也算变化。
pub fn content_fingerprint<T>(term: &Term<T>) -> u64 {
    let mut hasher = DefaultHasher::new();
    let columns = term.columns();
    let screen_lines = term.screen_lines();
    term.total_lines().hash(&mut hasher);
    columns.hash(&mut hasher);
    let grid = term.grid();
    for line in 0..screen_lines as i32 {
        let row = &grid[Line(line)];
        for col in 0..columns {
            row[Column(col)].c.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// 全 buffer(或最近 `max_scan_lines` 行)枚举命中,按 grid 顺序。
fn collect_matches<T>(
    term: &Term<T>,
    dfa: &mut RegexSearch,
    options: SearchOptions,
    limits: &SearchLimits,
) -> Vec<SearchMatch> {
    if term.columns() == 0 || term.screen_lines() == 0 {
        return Vec::new();
    }
    let topmost = term.topmost_line();
    let bottom = term.bottommost_line();
    let first = match limits.max_scan_lines {
        Some(n) => Line((bottom.0 - n as i32).max(topmost.0)),
        None => topmost,
    };
    let start = Point::new(first, Column(0));
    let end = Point::new(bottom, term.last_column());

    let mut out = Vec::new();
    collect_range(term, dfa, options, start, end, limits.max_matches, &mut out);
    out
}

/// 在 `[start, end]` 里顺序枚举命中追加进 `out`,`out` 攒够 `max` 条就停。
///
/// 增量重扫时 `start` 一定落在逻辑行首、`end` 一定落在非折行的行尾(或 grid 底部)
/// —— 这两处 alacritty 的迭代都从干净的 DFA 状态起步 / 收尾,所以区间内的结果与
/// 全扫走到这一段时逐条相同(见模块注释「增量重扫与它为什么是对的」)。
fn collect_range<T>(
    term: &Term<T>,
    dfa: &mut RegexSearch,
    options: SearchOptions,
    start: Point,
    end: Point,
    max: usize,
    out: &mut Vec<SearchMatch>,
) {
    if out.len() >= max {
        return;
    }
    for found in RegexIter::new(start, end, Direction::Right, term, dfa) {
        let candidate = SearchMatch {
            start: *found.start(),
            end: *found.end(),
        };
        if options.whole_word
            && !whole_word_ok(
                neighbor_before(term, candidate.start),
                neighbor_after(term, candidate.end),
            )
        {
            continue;
        }
        out.push(candidate);
        if out.len() >= max {
            break;
        }
    }
}

#[cfg(test)]
thread_local! {
    /// 这个线程上 [`ScanSnapshot::capture`] 被调了多少次(单测确认封顶路径真的没抓)。
    static SNAPSHOT_CAPTURES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// 一次扫描时 grid 的样子:逐行内容哈希 + 几个决定「行结构还能不能对上」的维度。
struct ScanSnapshot {
    /// 自 topmost 往下每一行的内容哈希(见 [`row_hash`])。下标 i ↔ `Line(i - history)`。
    rows: Vec<u64>,
    columns: usize,
    screen_lines: usize,
    history: usize,
    alt_screen: bool,
    /// 抓快照时的内容代数与屏幕指纹:两者都没变 = 内容没动过,快照可以原样留用。
    generation: u64,
    fingerprint: u64,
}

impl ScanSnapshot {
    fn capture<T>(term: &Term<T>, generation: u64, fingerprint: u64) -> Self {
        #[cfg(test)]
        SNAPSHOT_CAPTURES.with(|n| n.set(n.get() + 1));
        let grid = term.grid();
        let rows = (term.topmost_line().0..=term.bottommost_line().0)
            .map(|line| row_hash(&grid[Line(line)][..]))
            .collect();
        Self {
            rows,
            columns: term.columns(),
            screen_lines: term.screen_lines(),
            history: term.history_size(),
            alt_screen: term.mode().contains(TermMode::ALT_SCREEN),
            generation,
            fingerprint,
        }
    }

    /// 下标 → grid 行号。
    fn line(&self, index: usize) -> Line {
        Line(index as i32 - self.history as i32)
    }
}

/// 影响匹配的格标志:宽字符与它的占位格会被正则跳过、行尾 WRAPLINE 决定命中能不能
/// 跨到下一行。颜色 / 粗体之类变了不影响命中,不收 —— 免得一次重新着色让整段被当成变了。
const MATCH_FLAGS: u16 = Flags::WRAPLINE.bits()
    | Flags::WIDE_CHAR.bits()
    | Flags::WIDE_CHAR_SPACER.bits()
    | Flags::LEADING_WIDE_CHAR_SPACER.bits();

/// 一行的内容哈希:逐格的字符 + [`MATCH_FLAGS`]。
///
/// 用 128 位乘法折叠做混合(wyhash 的 mum 手法),分布足够好 —— 两行内容不同而
/// 哈希相同的概率在 2^-64 量级。四条互相独立的累加链交错吃格子(第 i 格进第
/// i % 4 条),最后按固定次序折叠:乘法的延迟被四路摊开,每次增量重扫都要把整个
/// 回看缓冲过一遍,这一步的快慢直接就是重扫的快慢。
fn row_hash(cells: &[Cell]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    #[inline(always)]
    fn mum(h: u64, v: u64) -> u64 {
        let x = u128::from(h ^ v) * u128::from(K);
        (x as u64) ^ ((x >> 64) as u64)
    }
    #[inline(always)]
    fn word(cell: &Cell) -> u64 {
        cell.c as u64 | (u64::from(cell.flags.bits() & MATCH_FLAGS) << 32)
    }
    let mut lanes: [u64; 4] = [
        0xA076_1D64_78BD_642F,
        0xE703_7ED1_A0B4_28DB,
        0x8EBC_6AF0_9C88_C6E3,
        0x5899_65CC_7537_4CC3,
    ];
    let (chunks, remainder) = cells.as_chunks::<4>();
    for chunk in chunks {
        for (lane, cell) in lanes.iter_mut().zip(chunk) {
            *lane = mum(*lane, word(cell));
        }
    }
    let mut h = mum(mum(mum(lanes[0], lanes[1]), lanes[2]), lanes[3]);
    for cell in remainder {
        h = mum(h, word(cell));
    }
    mum(h, cells.len() as u64)
}

/// 新 grid 第 `index` 行的行尾是不是折行(= 与下一行同属一条逻辑行)。
fn row_wrapped<T>(term: &Term<T>, snapshot: &ScanSnapshot, index: usize) -> bool {
    term.grid()[snapshot.line(index)][term.last_column()]
        .flags
        .contains(Flags::WRAPLINE)
}

/// 增量重扫的计划。行号都是 grid 行号。
#[derive(Debug, PartialEq, Eq)]
struct RescanPlan {
    /// 旧命中挪到新坐标:新行号 = 旧行号 - `shift`。
    shift: i32,
    /// 顶部被挤掉过行时,首条逻辑行可能被截了头,要从 topmost 重扫到这一行(含,
    /// 新坐标,非折行的行尾)。`None` = 首行原样沿用。
    head_end: Option<Line>,
    /// 沿用旧命中的区间,**旧**坐标 `[start, end)`,只看命中的起点行。
    keep_old: (i32, i32),
    /// 从这一行(新坐标,逻辑行首)一直重扫到底部。`None` = 底部没有变化。
    tail_start: Option<Line>,
}

/// 旧快照 → 新快照能不能增量、怎么增量。`None` = 退回全扫。
///
/// `scrollback` 是回滚上限:历史没到上限时顶部不会挤掉任何行(alacritty 只在历史
/// 满了之后才把最老的行转去复用),`dropped` 恒为 0;满了才去找对齐点。`wrapped`
/// 问的是**新** grid 某一行的行尾是否折行。
fn plan_rescan(
    old: &ScanSnapshot,
    new: &ScanSnapshot,
    scrollback: usize,
    wrapped: impl Fn(usize) -> bool,
) -> Option<RescanPlan> {
    // 列数 / 屏幕行数变了 = resize 过(reflow 把行结构整个改了);进出 alt screen =
    // 换了一块 grid;历史变少 = 清过历史或回滚上限调小。都没有对齐的意义
    if old.columns != new.columns
        || old.screen_lines != new.screen_lines
        || old.alt_screen != new.alt_screen
        || new.history < old.history
    {
        return None;
    }
    let dropped = if new.history < scrollback {
        0
    } else {
        find_dropped(old, new)?
    };
    // 新第 i 行 ↔ 旧第 i + dropped 行,自顶向下逐行核对到第一处不一致
    let mut verified = 0;
    while verified < new.rows.len()
        && verified + dropped < old.rows.len()
        && new.rows[verified] == old.rows[verified + dropped]
    {
        verified += 1;
    }
    // 顶部挤掉过行:首条逻辑行可能被截了头(它的前半截已经不在了),单独重扫;
    // 沿用区从它的下一行(逻辑行首)开始
    let (head_end, keep_start) = if dropped == 0 {
        (None, 0)
    } else {
        let end = (0..new.rows.len()).find(|&i| !wrapped(i))?;
        (Some(end), end + 1)
    };
    // 重扫区从第一处不一致所在逻辑行的行首开始。往上走过的行都在核对一致的范围里,
    // 新旧两边的折行标志相同(标志进了行哈希)
    let mut tail_start = verified;
    while tail_start > keep_start && wrapped(tail_start - 1) {
        tail_start -= 1;
    }
    if tail_start <= keep_start {
        return None;
    }
    let shift = (new.history - old.history + dropped) as i32;
    Some(RescanPlan {
        shift,
        head_end: head_end.map(|end| new.line(end)),
        keep_old: (
            old.line(keep_start + dropped).0,
            old.line(tail_start + dropped).0,
        ),
        tail_start: (tail_start < new.rows.len()).then(|| new.line(tail_start)),
    })
}

/// 历史满了之后顶部挤掉了多少行:拿旧历史最末几行当锚,在新 grid 里找它们挪到
/// 了哪儿。找不到(挤掉的比整个历史还多、或旧快照没有历史)返回 `None`。
///
/// 内容高度重复时锚可能对错位置 —— 没关系,[`plan_rescan`] 随后是逐行核对内容的,
/// 错位只会让核对早早失配、重扫范围变大。
fn find_dropped(old: &ScanSnapshot, new: &ScanSnapshot) -> Option<usize> {
    const ANCHOR_ROWS: usize = 8;
    let anchor = old.history.checked_sub(1)?;
    let window = ANCHOR_ROWS.min(anchor + 1);
    (0..=anchor).find(|&dropped| {
        // 锚窗里已经被挤掉的那几行(下标 < dropped)无从核对,跳过;锚行自己
        // (t = 0)一定在
        (0..window).all(|t| {
            let old_index = anchor - t;
            old_index < dropped || new.rows.get(old_index - dropped) == Some(&old.rows[old_index])
        })
    })
}

/// 按计划拼出新结果:重扫首条逻辑行 + 沿用中段旧命中(平移行号)+ 重扫尾段。
/// 三段在 grid 上首尾相接、互不重叠,按顺序拼起来就是 grid 顺序;攒够 `max`
/// 条即停,与全扫「从顶部数前 `max` 条」同口径。
fn rescan_matches<T>(
    term: &Term<T>,
    dfa: &mut RegexSearch,
    options: SearchOptions,
    max: usize,
    plan: &RescanPlan,
    old_matches: &[SearchMatch],
) -> Vec<SearchMatch> {
    let mut out = Vec::new();
    let last = term.last_column();
    if let Some(end) = plan.head_end {
        let start = Point::new(term.topmost_line(), Column(0));
        collect_range(
            term,
            dfa,
            options,
            start,
            Point::new(end, last),
            max,
            &mut out,
        );
    }
    let (keep_start, keep_end) = plan.keep_old;
    let shift = |p: Point| Point::new(Line(p.line.0 - plan.shift), p.column);
    out.extend(
        old_matches
            .iter()
            .filter(|m| m.start.line.0 >= keep_start && m.start.line.0 < keep_end)
            .map(|m| SearchMatch {
                start: shift(m.start),
                end: shift(m.end),
            })
            .take(max.saturating_sub(out.len())),
    );
    if let Some(start) = plan.tail_start {
        let end = Point::new(term.bottommost_line(), last);
        collect_range(
            term,
            dfa,
            options,
            Point::new(start, Column(0)),
            end,
            max,
            &mut out,
        );
    }
    out
}

/// 命中左边那一格的字符。行首返回 `None`。
///
/// 宽字符(CJK)占两列,它的第二列是 `WIDE_CHAR_SPACER`(字符是空格)——
/// 于是「汉字紧贴关键词」会被判成边界成立。与 xterm 的差异仅在此,记在这里。
fn neighbor_before<T>(term: &Term<T>, point: Point) -> Option<char> {
    if point.column.0 == 0 {
        return None;
    }
    Some(term.grid()[point.line][Column(point.column.0 - 1)].c)
}

/// 命中右边那一格的字符。行尾返回 `None`。
fn neighbor_after<T>(term: &Term<T>, point: Point) -> Option<char> {
    let next = point.column.0 + 1;
    if next >= term.columns() {
        return None;
    }
    Some(term.grid()[point.line][Column(next)].c)
}

/// 一条命中最多拆成多少行的高亮段。防住「正则匹配了半个 buffer」这种极端情况。
const MAX_SPAN_LINES: i32 = 512;

/// 命中集合 → 按行拍平的高亮索引。
pub fn build_highlights(
    matches: &[SearchMatch],
    current: Option<usize>,
    columns: usize,
    revision: u64,
) -> SearchHighlights {
    let mut rows: HashMap<i32, Vec<HighlightSpan>> = HashMap::new();
    let last_column = columns.saturating_sub(1);
    for (index, m) in matches.iter().enumerate() {
        let kind = if current == Some(index) {
            HighlightKind::Current
        } else {
            HighlightKind::Match
        };
        let first = m.start.line.0;
        let last = m.end.line.0.min(first + MAX_SPAN_LINES);
        if last < first {
            continue;
        }
        for line in first..=last {
            let start = if line == first { m.start.column.0 } else { 0 };
            let end = if line == m.end.line.0 {
                m.end.column.0
            } else {
                last_column
            };
            if end < start {
                continue;
            }
            rows.entry(line).or_default().push(HighlightSpan {
                start,
                end,
                kind,
            });
        }
    }
    // 行内按列排序:渲染层拿到的段落有序,合并/裁剪都不必再排
    for spans in rows.values_mut() {
        spans.sort_by_key(|s| s.start);
    }
    SearchHighlights {
        rows,
        revision,
        matches: matches.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mt_terminal::TermSize;

    // -- 纯函数 ---------------------------------------------------------

    #[test]
    fn 字面转义只动正则元字符() {
        assert_eq!(escape_literal("a.b"), "a\\.b");
        assert_eq!(escape_literal("1+1=2"), "1\\+1=2");
        assert_eq!(escape_literal("C:\\Users"), "C:\\\\Users");
        // 空格、冒号、斜杠不是元字符,转义了反而可能踩到「未知转义」
        assert_eq!(escape_literal("ls -la /tmp"), "ls \\-la /tmp");
        assert_eq!(escape_literal("你好"), "你好");
    }

    #[test]
    fn 模式串前缀钉死大小写而不是走_smart_case() {
        let insensitive = SearchOptions::default();
        let sensitive = SearchOptions {
            case_sensitive: true,
            ..Default::default()
        };
        // 关键词全小写:alacritty 的 smart case 会判成「不区分」,必须被 (?-i) 顶掉
        assert_eq!(build_pattern("abc", sensitive), "(?-i)abc");
        // 关键词带大写:smart case 会判成「区分」,必须被 (?i) 顶掉
        assert_eq!(build_pattern("Abc", insensitive), "(?i)Abc");
        // 正则模式不转义
        let re = SearchOptions {
            regex: true,
            ..Default::default()
        };
        assert_eq!(build_pattern("a.b", re), "(?i)a.b");
        assert_eq!(build_pattern("a.b", insensitive), "(?i)a\\.b");
    }

    #[test]
    fn 整词边界判定() {
        assert!(whole_word_ok(None, None), "整行就是这个词");
        assert!(whole_word_ok(Some(' '), Some(' ')));
        assert!(whole_word_ok(Some('('), Some(')')));
        assert!(!whole_word_ok(Some('x'), Some(' ')));
        assert!(!whole_word_ok(Some(' '), Some('9')));
        assert!(!whole_word_ok(Some('_'), None), "下划线算词字符");
        assert!(!whole_word_ok(Some('中'), Some(' ')), "汉字算词字符");
    }

    #[test]
    fn 环形推进() {
        use SearchDirection::*;
        assert_eq!(advance_index(None, 0, Next), None);
        assert_eq!(advance_index(Some(3), 0, Next), None);
        assert_eq!(advance_index(None, 5, Next), Some(0));
        assert_eq!(advance_index(None, 5, Previous), Some(4));
        assert_eq!(advance_index(Some(0), 3, Next), Some(1));
        assert_eq!(advance_index(Some(2), 3, Next), Some(0), "尾→头");
        assert_eq!(advance_index(Some(0), 3, Previous), Some(2), "头→尾");
    }

    #[test]
    fn 锚点挑选当前命中() {
        assert_eq!(index_at_or_after(&[], 0), None);
        let starts = [-40, -12, -3, 5];
        assert_eq!(index_at_or_after(&starts, -100), Some(0));
        assert_eq!(index_at_or_after(&starts, -12), Some(1), "锚点行本身要算上");
        assert_eq!(index_at_or_after(&starts, -11), Some(2));
        assert_eq!(index_at_or_after(&starts, 6), Some(0), "全在锚点之前就绕回头");
    }

    #[test]
    fn 高亮索引按行拍平() {
        let single = SearchMatch {
            start: Point::new(Line(2), Column(3)),
            end: Point::new(Line(2), Column(6)),
        };
        let wrapped = SearchMatch {
            start: Point::new(Line(4), Column(70)),
            end: Point::new(Line(6), Column(2)),
        };
        let h = build_highlights(&[single, wrapped], Some(1), 80, 7);
        assert_eq!(h.revision(), 7);
        assert_eq!(h.matches(), 2);
        assert_eq!(h.kind_at(2, 3), Some(HighlightKind::Match));
        assert_eq!(h.kind_at(2, 6), Some(HighlightKind::Match));
        assert_eq!(h.kind_at(2, 7), None);
        // 跨行的命中:首行到行尾、中间整行、末行到 end.column
        assert_eq!(h.kind_at(4, 79), Some(HighlightKind::Current));
        assert_eq!(h.kind_at(4, 69), None);
        assert_eq!(h.kind_at(5, 0), Some(HighlightKind::Current));
        assert_eq!(h.kind_at(5, 79), Some(HighlightKind::Current));
        assert_eq!(h.kind_at(6, 2), Some(HighlightKind::Current));
        assert_eq!(h.kind_at(6, 3), None);
        assert_eq!(h.row(3).len(), 0);
    }

    // -- 引擎(跑在真的 grid 上) ---------------------------------------

    fn emulator(text: &str) -> TerminalEmulator {
        let e = TerminalEmulator::new(TermSize::new(40, 6));
        e.advance(text.replace('\n', "\r\n").as_bytes());
        e
    }

    fn texts(search: &TerminalSearch, e: &TerminalEmulator) -> Vec<String> {
        search
            .matches()
            .iter()
            .map(|m| {
                e.with_term(|t| {
                    let mut s = String::new();
                    for col in m.start.column.0..=m.end.column.0 {
                        s.push(t.grid()[m.start.line][Column(col)].c);
                    }
                    s
                })
            })
            .collect()
    }

    #[test]
    fn 字面查找全_buffer_计数() {
        let e = emulator("cat dog\ncat cat\nbird\n");
        let mut s = TerminalSearch::new();
        s.set_query("cat");
        s.refresh(&e);
        assert_eq!(s.count(), 3);
        assert_eq!(texts(&s, &e), vec!["cat", "cat", "cat"]);
        // 命中按 grid 顺序:第一条在第 0 行,后两条在第 1 行
        assert_eq!(s.matches()[0].start.line, Line(0));
        assert_eq!(s.matches()[1].start.line, Line(1));
        assert_eq!(s.matches()[2].start.line, Line(1));
        assert!(s.matches()[1].start.column < s.matches()[2].start.column);
    }

    #[test]
    fn 大小写开关两个方向都钉死() {
        let e = emulator("Cat cat CAT\n");
        let mut s = TerminalSearch::new();
        // 关键词全小写 + 不区分 → 3 条
        s.set_query("cat");
        s.refresh(&e);
        assert_eq!(s.count(), 3);
        // 关键词全小写 + 区分 → 1 条(smart case 会误判成 3 条)
        s.set_options(SearchOptions {
            case_sensitive: true,
            ..Default::default()
        });
        s.refresh(&e);
        assert_eq!(s.count(), 1, "smart case 没被 (?-i) 顶掉");
        // 关键词带大写 + 不区分 → 3 条(smart case 会误判成 1 条)
        s.set_options(SearchOptions::default());
        s.set_query("Cat");
        s.refresh(&e);
        assert_eq!(s.count(), 3, "smart case 没被 (?i) 顶掉");
    }

    #[test]
    fn 整词匹配() {
        let e = emulator("cat concatenate cat_dog cat.\n");
        let mut s = TerminalSearch::new();
        s.set_query("cat");
        s.refresh(&e);
        assert_eq!(s.count(), 4, "不开整词:cat / concatenate / cat_dog / cat.");
        s.set_options(SearchOptions {
            whole_word: true,
            ..Default::default()
        });
        s.refresh(&e);
        // 行首那个 cat(后面是空格)与 `cat.`(前空格后句点)算整词;
        // concatenate 与 cat_dog 不算
        assert_eq!(s.count(), 2);
        assert_eq!(s.matches()[0].start.column, Column(0));
        assert_eq!(s.matches()[1].start.column, Column(24));
    }

    #[test]
    fn 字面模式里的元字符不当正则用() {
        let e = emulator("a.b axb\n");
        let mut s = TerminalSearch::new();
        s.set_query("a.b");
        s.refresh(&e);
        assert_eq!(s.count(), 1, "字面模式下 . 只能匹配点号本身");
        assert_eq!(s.matches()[0].start.column, Column(0));

        s.set_options(SearchOptions {
            regex: true,
            ..Default::default()
        });
        s.refresh(&e);
        assert_eq!(s.count(), 2, "正则模式下 . 是通配");
    }

    #[test]
    fn 正则语法错落到_error_且计数清零() {
        let e = emulator("hello\n");
        let mut s = TerminalSearch::new();
        s.set_options(SearchOptions {
            regex: true,
            ..Default::default()
        });
        s.set_query("hel(");
        s.refresh(&e);
        assert_eq!(s.count(), 0);
        assert!(s.error().is_some(), "非法正则必须有错误文本");
        // 改回合法就要自愈
        s.set_query("hel+o");
        s.refresh(&e);
        assert!(s.error().is_none());
        assert_eq!(s.count(), 1);
    }

    #[test]
    fn 环形翻页与当前命中() {
        let e = emulator("x x x\n");
        let mut s = TerminalSearch::new();
        s.set_query("x");
        s.refresh(&e);
        assert_eq!(s.count(), 3);
        // 刚搜完:当前命中落在视口顶部往下的第一条
        assert_eq!(s.current_index(), Some(0));
        assert_eq!(s.display_index(), 1);
        s.find_next(&e);
        assert_eq!(s.current_index(), Some(1));
        s.find_next(&e);
        assert_eq!(s.current_index(), Some(2));
        s.find_next(&e);
        assert_eq!(s.current_index(), Some(0), "尾部绕回头");
        s.find_previous(&e);
        assert_eq!(s.current_index(), Some(2), "头部绕回尾");
    }

    #[test]
    fn 命中集合覆盖_scrollback() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        // 30 行里只有第 0 行有 needle,它早被顶进回看缓冲
        e.advance(b"needle\r\n");
        for i in 0..30 {
            e.advance(format!("filler {i}\r\n").as_bytes());
        }
        let mut s = TerminalSearch::new();
        s.set_query("needle");
        s.refresh(&e);
        assert_eq!(s.count(), 1, "scrollback 里的命中必须搜得到");
        assert!(
            s.matches()[0].start.line.0 < 0,
            "命中应落在负行号(回看缓冲)"
        );
    }

    /// 整词判定要去读命中两侧的格子 —— 命中落在回看缓冲(负行号)时,
    /// grid 索引也必须照样走得通。这条专钉负行号的索引路径。
    #[test]
    fn 整词判定在_scrollback_里也成立() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        e.advance(b"the cat sat\r\n");
        e.advance(b"concatenate\r\n");
        for i in 0..30 {
            e.advance(format!("filler {i}\r\n").as_bytes());
        }
        let mut s = TerminalSearch::new();
        s.set_options(SearchOptions {
            whole_word: true,
            ..Default::default()
        });
        s.set_query("cat");
        s.refresh(&e);
        assert_eq!(s.count(), 1, "concatenate 里那个不算整词");
        assert!(s.matches()[0].start.line.0 < 0, "命中在回看缓冲里");
        assert_eq!(s.matches()[0].start.column, Column(4));
    }

    #[test]
    fn 跳转把命中滚进视口_已在视口内则不动() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        e.advance(b"needle\r\n");
        for i in 0..40 {
            e.advance(format!("filler {i}\r\n").as_bytes());
        }
        let mut s = TerminalSearch::new();
        s.set_query("needle");
        s.refresh(&e);
        assert_eq!(e.with_term(|t| t.grid().display_offset()), 0);
        s.find_next(&e);
        let offset = e.with_term(|t| t.grid().display_offset());
        assert!(offset > 0, "命中在回看缓冲里,必须滚上去");
        // 已经在视口里了:再跳同一条不该再滚
        s.scroll_to_current(&e);
        assert_eq!(e.with_term(|t| t.grid().display_offset()), offset);
    }

    #[test]
    fn 命中条数封顶() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        for _ in 0..20 {
            e.advance(b"aaaaaaaaaa\r\n");
        }
        let mut s = TerminalSearch::with_limits(SearchLimits {
            max_matches: 17,
            ..Default::default()
        });
        s.set_query("a");
        s.refresh(&e);
        assert_eq!(s.count(), 17, "超过上限就停,不把内存打爆");
    }

    #[test]
    fn 只扫最近_n_行() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        e.advance(b"needle\r\n");
        for i in 0..60 {
            e.advance(format!("filler {i}\r\n").as_bytes());
        }
        let mut s = TerminalSearch::with_limits(SearchLimits {
            max_scan_lines: Some(10),
            ..Default::default()
        });
        s.set_query("needle");
        s.refresh(&e);
        assert_eq!(s.count(), 0, "扫描窗口之外的命中不该被找到");

        let mut all = TerminalSearch::new();
        all.set_query("needle");
        all.refresh(&e);
        assert_eq!(all.count(), 1, "不设窗口就要能搜到");
    }

    #[test]
    fn 去抖与内容指纹挡住无谓重搜() {
        let e = emulator("cat\n");
        let mut s = TerminalSearch::new();
        s.set_query("cat");
        let t0 = Instant::now();
        assert!(s.sync_at(&e, t0), "首次(脏)必须扫");
        assert_eq!(s.count(), 1);

        // 没到去抖窗口:即使有新输出也先不扫
        e.advance(b"cat cat\r\n");
        assert!(!s.sync_at(&e, t0 + Duration::from_millis(50)));
        assert_eq!(s.count(), 1);

        // 过了窗口 + 指纹变了 → 扫
        assert!(s.sync_at(&e, t0 + Duration::from_millis(250)));
        assert_eq!(s.count(), 3);

        // 又过一个窗口但内容没动 → 指纹相同,直接跳过
        assert!(!s.sync_at(&e, t0 + Duration::from_millis(500)));

        // 关键词改了 → 脏标记无视去抖,立刻生效
        s.set_query("dog");
        assert!(s.sync_at(&e, t0 + Duration::from_millis(505)));
        assert_eq!(s.count(), 0);
    }

    #[test]
    fn 内容指纹不随回看滚动变化() {
        let e = TerminalEmulator::new(TermSize::new(40, 5));
        for i in 0..30 {
            e.advance(format!("line {i}\r\n").as_bytes());
        }
        let before = e.with_term(content_fingerprint);
        e.with_term_mut(|t| t.scroll_display(Scroll::Delta(10)));
        assert_eq!(
            e.with_term(content_fingerprint),
            before,
            "滚动回看不改内容,指纹必须稳住"
        );
        e.advance(b"new output\r\n");
        assert_ne!(
            e.with_term(content_fingerprint),
            before,
            "新输出必须让指纹变"
        );
    }

    #[test]
    fn 重搜后当前命中尽量原地不动() {
        let e = emulator("alpha\nbravo\ncharlie\n");
        let mut s = TerminalSearch::new();
        s.set_query("a");
        s.refresh(&e);
        s.find_next(&e);
        s.find_next(&e);
        let anchored = s.current_match().expect("应有当前命中");
        // 追加一行不含关键词的输出,行号整体不变(还没顶到 scrollback)
        e.advance(b"zzz\r\n");
        s.refresh(&e);
        assert_eq!(
            s.current_match(),
            Some(anchored),
            "命中还在原处时当前命中不该乱跳"
        );
    }

    #[test]
    fn 高亮随当前命中移动且版本号推进() {
        let e = emulator("x x\n");
        let mut s = TerminalSearch::new();
        s.set_query("x");
        s.refresh(&e);
        let v0 = s.revision();
        let h0 = s.highlights();
        assert_eq!(h0.kind_at(0, 0), Some(HighlightKind::Current));
        assert_eq!(h0.kind_at(0, 2), Some(HighlightKind::Match));
        s.find_next(&e);
        assert!(s.revision() > v0, "当前命中变了要推进版本号");
        let h1 = s.highlights();
        assert_eq!(h1.kind_at(0, 0), Some(HighlightKind::Match));
        assert_eq!(h1.kind_at(0, 2), Some(HighlightKind::Current));
    }

    #[test]
    fn 清空后不再高亮但保留选项() {
        let e = emulator("cat\n");
        let mut s = TerminalSearch::new();
        s.set_options(SearchOptions {
            whole_word: true,
            case_sensitive: true,
            regex: false,
        });
        s.set_query("cat");
        s.refresh(&e);
        assert_eq!(s.count(), 1);
        s.clear();
        assert!(!s.is_active());
        assert_eq!(s.count(), 0);
        assert!(s.highlights().is_empty());
        assert!(s.options().whole_word, "选项要留到下次打开");
        assert!(s.options().case_sensitive);
    }

    #[test]
    fn 收起查找条留住关键词_重开自动重搜() {
        let e = emulator("cat\n");
        let mut s = TerminalSearch::new();
        s.set_query("cat");
        s.refresh(&e);
        assert_eq!(s.count(), 1);

        // 收起:高亮与计数立刻清空,关键词留着
        s.set_enabled(false);
        assert!(!s.is_active());
        assert_eq!(s.count(), 0);
        assert!(s.highlights().is_empty());
        assert_eq!(s.query(), "cat", "关键词必须留到下次 Ctrl+F");
        // 关着的时候新输出不该让它偷偷复活
        e.advance(b"cat cat\r\n");
        assert!(!s.sync(&e));
        assert_eq!(s.count(), 0);

        // 重开:无视去抖立刻重搜,并且搜的是**现在**的内容
        s.set_enabled(true);
        assert!(s.sync(&e));
        assert_eq!(s.count(), 3);
    }

    #[test]
    fn 空关键词不搜也不留残留高亮() {
        let e = emulator("cat\n");
        let mut s = TerminalSearch::new();
        s.set_query("cat");
        s.refresh(&e);
        assert!(!s.highlights().is_empty());
        s.set_query("");
        assert!(s.sync(&e), "由有到无算变化");
        assert_eq!(s.count(), 0);
        assert!(s.highlights().is_empty());
        assert!(!s.sync(&e), "已经清干净了就不该再报变化");
    }

    // -- 增量重扫 -------------------------------------------------------

    /// 同一个 emulator 上拿全新引擎全扫一遍,作对照。
    fn fresh_matches(
        e: &TerminalEmulator,
        query: &str,
        options: SearchOptions,
    ) -> Vec<SearchMatch> {
        let mut s = TerminalSearch::new();
        s.set_options(options);
        s.set_query(query);
        s.refresh(e);
        s.matches().to_vec()
    }

    /// 内容变化触发的重搜(`now` 远超去抖点)。
    fn sync_later(s: &mut TerminalSearch, e: &TerminalEmulator, step: u64) {
        let base = Instant::now();
        s.sync_at(e, base + Duration::from_secs(10 * step));
    }

    /// 回滚区满了之后旧行被挤掉、行号整体平移:增量结果与全扫逐条相同,
    /// 且真的走了增量(没退回全扫)。
    #[test]
    fn 回滚区挤出后增量结果与全扫一致() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(30, 6), 40);
        for i in 0..80 {
            let tag = if i % 7 == 0 { "needle" } else { "filler" };
            e.advance(format!("{tag} {i}\r\n").as_bytes());
        }
        let mut s = TerminalSearch::new();
        s.set_query("needle");
        s.refresh(&e);
        let before = s.incremental_scans;
        for step in 1..=30u64 {
            let tag = if step % 3 == 0 { "needle" } else { "other" };
            e.advance(format!("{tag} new {step}\r\nplain\r\n").as_bytes());
            sync_later(&mut s, &e, step);
            assert_eq!(
                s.matches(),
                fresh_matches(&e, "needle", SearchOptions::default()).as_slice(),
                "第 {step} 步"
            );
        }
        assert!(
            s.incremental_scans - before >= 25,
            "挤出场景应该绝大多数走增量,实际 {}",
            s.incremental_scans - before
        );
    }

    /// 周期性输出恰好滚过整数个周期:屏幕逐字相同、历史已满、总行数不变 ——
    /// 光看指纹会漏掉。内容代数兜住它。
    #[test]
    fn 周期性输出不漏重扫() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(30, 6), 20);
        for i in 0..40 {
            e.advance(format!("filler {i}\r\n").as_bytes());
        }
        let mut s = TerminalSearch::new();
        s.set_query("needle");
        s.refresh(&e);
        for step in 1..=8u64 {
            // 周期 3 行、每轮正好 3 行:填满屏幕之后屏幕内容逐轮相同
            e.advance(b"needle\r\nx\r\ny\r\n");
            sync_later(&mut s, &e, step);
        }
        assert_eq!(
            s.matches(),
            fresh_matches(&e, "needle", SearchOptions::default()).as_slice()
        );
        assert_eq!(s.count(), 8);
    }

    /// resize(reflow)、清屏、清历史、进出 alt screen:都得与全扫一致。
    #[test]
    fn 重排与清屏后结果与全扫一致() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(24, 5), 30);
        for i in 0..20 {
            e.advance(format!("row {i} needle and some long text that wraps\r\n").as_bytes());
        }
        let mut s = TerminalSearch::new();
        s.set_query("needle");
        s.refresh(&e);
        let check = |s: &TerminalSearch, what: &str| {
            assert_eq!(
                s.matches(),
                fresh_matches(&e, "needle", SearchOptions::default()).as_slice(),
                "{what}"
            );
        };
        e.resize(TermSize::new(17, 5));
        sync_later(&mut s, &e, 1);
        check(&s, "列数变了(reflow)");
        e.resize(TermSize::new(17, 8));
        sync_later(&mut s, &e, 2);
        check(&s, "行数变了");
        e.advance(b"\x1b[2J\x1b[H");
        sync_later(&mut s, &e, 3);
        check(&s, "ESC[2J(整屏顶进历史)");
        e.advance(b"needle after clear\r\n\x1b[3J");
        sync_later(&mut s, &e, 4);
        check(&s, "ESC[3J(清历史)");
        e.advance(b"\x1b[?1049hneedle in alt\r\n");
        sync_later(&mut s, &e, 5);
        check(&s, "进 alt screen");
        e.advance(b"\x1b[?1049l");
        sync_later(&mut s, &e, 6);
        check(&s, "出 alt screen");
        // 就地逐行擦除(Claude Code 的清屏手法),不产生滚动
        e.advance(b"\x1b[H\x1b[2K\x1b[1B\x1b[2K\x1b[1B\x1b[2Kneedle here\r\n");
        sync_later(&mut s, &e, 7);
        check(&s, "原地擦除改写");
    }

    /// 极简的确定性伪随机(xorshift64*),免得为一条测试引依赖。
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// 一段随机输出:大多是若干行随机词(可能折行、可能不换行、带宽字符),
    /// 偶尔是清屏 / 清历史 / 原地改写 / 进出 alt screen。
    fn random_output(rng: &mut Rng) -> String {
        const WORDS: &[&str] = &[
            "ab",
            "a b",
            "axb",
            "needle",
            "need",
            "le",
            "xx",
            "xxy",
            "y",
            "中文字",
            "字",
            "ab_c",
            " ",
            "  ",
            "b",
            "AB",
            "aab",
        ];
        match rng.below(20) {
            0 => "\x1b[2J\x1b[H".into(),
            1 => "\x1b[3J".into(),
            2 => format!("\x1b[{};1H\x1b[2Kab needle 改写", 1 + rng.below(6)),
            3 => "\x1b[?1049hab needle\r\nxxy\x1b[?1049l".into(),
            4 => "\x1b[H\x1b[2K\x1b[1B\x1b[2K".into(),
            _ => {
                let mut out = String::new();
                for _ in 0..1 + rng.below(5) {
                    for _ in 0..rng.below(14) {
                        out.push_str(WORDS[rng.below(WORDS.len() as u64) as usize]);
                    }
                    if rng.below(8) != 0 {
                        out.push_str("\r\n");
                    }
                }
                out
            }
        }
    }

    /// 差分测试:随机输出流 + 偶发 resize,每一步都拿增量结果对照全新全扫。
    /// 覆盖挤出、折行、宽字符、清屏、清历史、alt screen、原地改写与命中封顶。
    #[test]
    fn 随机输出流下增量结果恒与全扫一致() {
        let queries: [(&str, SearchOptions, usize); 7] = [
            ("ab", SearchOptions::default(), 1000),
            (
                "needle",
                SearchOptions {
                    whole_word: true,
                    ..Default::default()
                },
                1000,
            ),
            (
                "a.b",
                SearchOptions {
                    regex: true,
                    ..Default::default()
                },
                1000,
            ),
            (
                "x+y",
                SearchOptions {
                    regex: true,
                    ..Default::default()
                },
                1000,
            ),
            ("中文字", SearchOptions::default(), 1000),
            (
                "AB",
                SearchOptions {
                    case_sensitive: true,
                    ..Default::default()
                },
                1000,
            ),
            // 小上限:经常封顶,走「封顶退回全扫」那条
            ("b", SearchOptions::default(), 25),
        ];
        let mut incremental = 0;
        let mut rescans = 0;
        for seed in 1..=24u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut cols = 16 + rng.below(20) as usize;
            let mut rows = 4 + rng.below(5) as usize;
            let e = TerminalEmulator::with_scrollback(
                TermSize::new(cols, rows),
                10 + rng.below(40) as usize,
            );
            let mut engines: Vec<TerminalSearch> = queries
                .iter()
                .map(|(q, options, max)| {
                    let mut s = TerminalSearch::with_limits(SearchLimits {
                        max_matches: *max,
                        ..Default::default()
                    });
                    s.set_options(*options);
                    s.set_query(*q);
                    s.refresh(&e);
                    s
                })
                .collect();
            for step in 1..=60u64 {
                if rng.below(15) == 0 {
                    cols = 12 + rng.below(24) as usize;
                    rows = 3 + rng.below(6) as usize;
                    e.resize(TermSize::new(cols, rows));
                } else {
                    e.advance(random_output(&mut rng).as_bytes());
                }
                for (s, (q, options, max)) in engines.iter_mut().zip(queries.iter()) {
                    let before = s.incremental_scans;
                    sync_later(s, &e, step);
                    incremental += s.incremental_scans - before;
                    rescans += 1;
                    let mut expected = fresh_matches(&e, q, *options);
                    expected.truncate(*max);
                    assert_eq!(
                        s.matches(),
                        expected.as_slice(),
                        "seed {seed} step {step} query {q:?}"
                    );
                }
            }
        }
        // 不只是「都退回了全扫所以一致」:增量路径必须被大量走到
        assert!(
            incremental * 3 > rescans,
            "增量路径走得太少:{incremental}/{rescans}"
        );
    }

    #[test]
    fn 行对齐计划的退回条件() {
        let snap = |rows: &[u64], history: usize| ScanSnapshot {
            rows: rows.to_vec(),
            columns: 10,
            screen_lines: 3,
            history,
            alt_screen: false,
            generation: 0,
            fingerprint: 0,
        };
        let no_wrap = |_: usize| false;
        let old = snap(&[1, 2, 3, 4, 5], 2);
        // 只是屏幕最后一行变了:保留前四行,从第五行重扫
        let plan = plan_rescan(&old, &snap(&[1, 2, 3, 4, 9], 2), 100, no_wrap).unwrap();
        assert_eq!(plan.shift, 0);
        assert_eq!(plan.head_end, None);
        assert_eq!(plan.keep_old, (-2, 2));
        assert_eq!(plan.tail_start, Some(Line(2)));
        // 历史长了 2 行(没满):不挤出,行号整体上移 2
        let plan = plan_rescan(&old, &snap(&[1, 2, 3, 4, 5, 6, 7], 4), 100, no_wrap).unwrap();
        assert_eq!(plan.shift, 2);
        assert_eq!(plan.tail_start, Some(Line(1)), "新第 5 行起是新内容");
        // 历史满了(上限 5)、顶部挤掉 2 行:找对齐点,首条逻辑行单独重扫
        let full = snap(&[1, 2, 3, 4, 5, 6, 7, 8], 5);
        let plan = plan_rescan(&full, &snap(&[3, 4, 5, 6, 7, 8, 9, 10], 5), 5, no_wrap).unwrap();
        assert_eq!(plan.shift, 2);
        assert_eq!(plan.head_end, Some(Line(-5)), "新 topmost 那条逻辑行重扫");
        assert_eq!(plan.keep_old, (-2, 3), "旧第 -2~2 行(新第 -4~0 行)沿用");
        assert_eq!(plan.tail_start, Some(Line(1)), "新第 1 行起是新内容");
        // 挤掉的比整个旧历史还多:找不到对齐点,退回全扫
        assert!(
            plan_rescan(
                &full,
                &snap(&[20, 21, 22, 23, 24, 25, 26, 27], 5),
                5,
                no_wrap
            )
            .is_none()
        );
        // 折行:不一致那一行所在的整条逻辑行都要重扫
        let wraps = |i: usize| i == 2;
        let plan = plan_rescan(&old, &snap(&[1, 2, 3, 8, 9], 2), 100, wraps).unwrap();
        assert_eq!(
            plan.tail_start,
            Some(Line(0)),
            "第 3 行折到第 4 行,从第 3 行重扫"
        );
        // 退回全扫的几种
        let mut resized = snap(&[1, 2, 3, 4, 5], 2);
        resized.columns = 11;
        assert!(
            plan_rescan(&old, &resized, 100, no_wrap).is_none(),
            "列数变了"
        );
        assert!(
            plan_rescan(&old, &snap(&[4, 5, 6], 0), 100, no_wrap).is_none(),
            "历史变少(清历史)"
        );
        let mut alt = snap(&[1, 2, 3, 4, 5], 2);
        alt.alt_screen = true;
        assert!(
            plan_rescan(&old, &alt, 100, no_wrap).is_none(),
            "进出 alt screen"
        );
        assert!(
            plan_rescan(&old, &snap(&[9, 9, 9, 9, 9], 2), 100, no_wrap).is_none(),
            "从第一行起就对不上:没有可沿用的"
        );
    }

    /// 没到去抖点被挡下的那次:兜底重绘只排一发,到点解除后才能再排。
    #[test]
    fn 被去抖挡下时排一发兜底重扫() {
        let e = emulator("cat\n");
        let mut s = TerminalSearch::new();
        s.set_query("cat");
        let t0 = Instant::now();
        s.sync_at(&e, t0);
        assert_eq!(s.take_trailing_rescan(t0), None, "刚扫完、没被挡过");
        e.advance(b"cat\r\n");
        assert!(!s.sync_at(&e, t0 + Duration::from_millis(50)), "没到去抖点");
        let delay = s.take_trailing_rescan(t0 + Duration::from_millis(50));
        assert_eq!(delay, Some(Duration::from_millis(150)), "距去抖点还剩多久");
        assert_eq!(
            s.take_trailing_rescan(t0 + Duration::from_millis(60)),
            None,
            "已排过"
        );
        s.trailing_rescan_fired();
        // 到点那一帧:扫到新内容,不再欠
        assert!(s.sync_at(&e, t0 + Duration::from_millis(200)));
        assert_eq!(s.count(), 2);
        assert_eq!(
            s.take_trailing_rescan(t0 + Duration::from_millis(200)),
            None
        );
    }

    /// 宿主帧时序的模拟,照 pane 套了 view 级缓存之后的真实情形:
    ///
    /// - 帧只在三种情况下发生:有输出(节拍器 notify pane)、兜底重扫到点、
    ///   上一帧要求重画(`FrameSync::repaint`)—— 窗口别处的重绘碰不到这个 pane;
    /// - 一帧之内查找条先 render(读计数),终端元素后 prepaint(`frame_sync`)。
    ///
    /// `bursts` 是 (毫秒, 输出);`honor_repaint = false` 模拟不理会重画请求的宿主。
    /// 返回输出停下、帧排空之后查找条上停住的计数,以及总帧数。
    fn simulate_host(
        s: &mut TerminalSearch,
        e: &TerminalEmulator,
        base: Instant,
        bursts: &[(u64, String)],
        honor_repaint: bool,
    ) -> (usize, usize) {
        use std::collections::BTreeMap;
        let at = |ms: u64| base + Duration::from_millis(ms);
        // 待画的帧:毫秒 → 这一帧是不是兜底重扫到点(同一毫秒的帧合并成一帧)
        let mut frames: BTreeMap<u64, bool> = BTreeMap::new();
        let mut bursts = bursts.iter().peekable();
        let mut shown = 0;
        let mut drawn = 0;
        loop {
            let next_frame = frames.keys().next().copied();
            let next_burst = bursts.peek().map(|(ms, _)| *ms);
            // 同一毫秒先落输出、再画帧
            let burst_first = match (next_burst, next_frame) {
                (None, None) => return (shown, drawn),
                (Some(b), Some(f)) => b <= f,
                (burst, _) => burst.is_some(),
            };
            if burst_first {
                let (ms, bytes) = bursts.next().unwrap();
                e.advance(bytes.as_bytes());
                frames.entry(*ms).or_insert(false);
                continue;
            }
            let ms = next_frame.unwrap();
            if frames.remove(&ms).unwrap() {
                s.trailing_rescan_fired();
            }
            drawn += 1;
            assert!(drawn < 500, "帧排不空:重画 / 兜底互相续命了");
            // render:查找条读计数
            shown = s.count();
            // prepaint:终端元素 sync
            let frame = s.frame_sync(e, at(ms));
            if frame.repaint && honor_repaint {
                frames.entry(ms + 16).or_insert(false);
            }
            if let Some(delay) = frame.rescan_after {
                *frames.entry(ms + delay.as_millis() as u64).or_insert(true) = true;
            }
        }
    }

    /// 回归(w5c 引入):回滚区满了之后命中被挤出顶部,输出停在去抖窗口里。兜底
    /// 重扫在 prepaint 里把命中剔掉了,但查找条那一帧已经 render 过、画的是扫描
    /// 前的数 —— pane 套着 view 级缓存,再没有帧来更新它,计数一直停在旧值,
    /// 滚一下滚轮才纠正。结果一变就再要一帧,停住的计数必须与全扫一致。
    #[test]
    fn 输出停下后查找条计数与全扫一致() {
        let setup = || {
            let e = TerminalEmulator::with_scrollback(TermSize::new(30, 6), 40);
            // 历史填满,命中散在回看缓冲里
            for i in 0..80 {
                let tag = if i % 7 == 0 { "needle" } else { "filler" };
                e.advance(format!("{tag} {i}\r\n").as_bytes());
            }
            e
        };
        // 每 30ms 两行不含关键词的输出,把命中一条条挤出顶部;最后一批(570ms)
        // 落在 400ms 那次扫描之后的去抖窗口里
        let bursts: Vec<(u64, String)> = (1..=19u64)
            .map(|k| (k * 30, format!("plain {k}\r\nplain {k}b\r\n")))
            .collect();

        let e = setup();
        let mut s = TerminalSearch::new();
        s.set_query("needle");
        let base = Instant::now();
        assert!(s.frame_sync(&e, base).repaint, "首扫(脏)");
        let before = s.count();
        let incremental = s.incremental_scans;
        let (shown, frames) = simulate_host(&mut s, &e, base, &bursts, true);
        let expected = fresh_matches(&e, "needle", SearchOptions::default());
        assert_eq!(s.matches(), expected.as_slice(), "引擎结果与全扫一致");
        assert!(expected.len() < before, "场景里必须真有命中被挤出顶部");
        assert!(
            s.incremental_scans > incremental,
            "历史满了的挤出场景要走增量重扫"
        );
        assert_eq!(shown, expected.len(), "查找条停住的计数与全扫一致");
        assert!(frames < 60, "重画请求不该让帧数失控:{frames}");
        // 帧排空之后引擎不再欠任何东西:再来一帧既不要重画也不排兜底
        assert_eq!(
            s.frame_sync(&e, base + Duration::from_secs(5)),
            FrameSync::default()
        );

        // 对照:不理会重画请求的宿主(= 修复前),计数停在兜底重扫之前的值
        let e = setup();
        let mut s = TerminalSearch::new();
        s.set_query("needle");
        s.frame_sync(&e, base);
        let (stale, _) = simulate_host(&mut s, &e, base, &bursts, false);
        assert_eq!(s.count(), expected.len(), "引擎本身早已扫对");
        assert_ne!(
            stale,
            s.count(),
            "对照组必须复现出陈旧计数,否则本场景没覆盖到回归"
        );
    }

    /// 结果变了要的那一帧紧跟在扫描之后(没到去抖点):内容没再动就什么都不欠,
    /// 不能再排一发兜底重扫 —— 否则每次结果变化都白多一帧。
    #[test]
    fn 重画帧内容未动不再排兜底重扫() {
        let e = emulator("cat\n");
        let mut s = TerminalSearch::new();
        s.set_query("cat");
        let t0 = Instant::now();
        assert!(s.frame_sync(&e, t0).repaint, "首扫结果变了,要一帧");
        let redraw = s.frame_sync(&e, t0 + Duration::from_millis(16));
        assert_eq!(redraw, FrameSync::default(), "内容没动:不重画、不欠重扫");
        // 滚动回看只动 display_offset,同样什么都不欠
        e.with_term_mut(|t| t.scroll_display(Scroll::Delta(1)));
        assert_eq!(
            s.frame_sync(&e, t0 + Duration::from_millis(40)),
            FrameSync::default()
        );
        // 真有新输出才欠:排到去抖点
        e.advance(b"cat\r\n");
        assert_eq!(
            s.frame_sync(&e, t0 + Duration::from_millis(50)),
            FrameSync {
                repaint: false,
                rescan_after: Some(Duration::from_millis(150)),
            }
        );
    }

    /// 只改关键词时内容没动:逐行快照原样留用,不在每次敲字时重算一遍。
    #[test]
    fn 改关键词不重算逐行快照() {
        let e = emulator("cat dog\ncat\n");
        let mut s = TerminalSearch::new();
        s.set_query("cat");
        s.refresh(&e);
        let rows_ptr = |s: &TerminalSearch| s.snapshot.as_ref().map(|snap| snap.rows.as_ptr());
        let generation = |s: &TerminalSearch| s.snapshot.as_ref().map(|snap| snap.generation);
        let first = rows_ptr(&s);
        let first_generation = generation(&s);
        assert!(first.is_some());
        s.set_query("dog");
        s.refresh(&e);
        assert_eq!(rows_ptr(&s), first, "内容没变,快照留用(同一块内存)");
        assert_eq!(s.count(), 1);
        e.advance(b"dog\r\n");
        s.set_query("do");
        s.refresh(&e);
        assert_ne!(
            generation(&s),
            first_generation,
            "内容变了,按新内容重抓快照"
        );
        assert_eq!(s.count(), 2);
    }

    /// 这个线程上到现在一共抓过几次逐行快照。
    fn captures() -> u64 {
        SNAPSHOT_CAPTURES.with(std::cell::Cell::get)
    }

    /// 限了条数的引擎,对着一屏命中远超上限的内容。
    fn capped_setup(max_matches: usize) -> (TerminalEmulator, TerminalSearch) {
        let e = TerminalEmulator::with_scrollback(TermSize::new(40, 5), 200);
        for _ in 0..50 {
            e.advance(b"needle needle\r\n");
        }
        let s = TerminalSearch::with_limits(SearchLimits {
            max_matches,
            ..Default::default()
        });
        (e, s)
    }

    /// 同样的上限下全扫一遍,作对照。
    fn fresh_capped(e: &TerminalEmulator, query: &str, max_matches: usize) -> Vec<SearchMatch> {
        let mut s = TerminalSearch::with_limits(SearchLimits {
            max_matches,
            ..Default::default()
        });
        s.set_query(query);
        s.refresh(e);
        s.matches().to_vec()
    }

    /// 结果封顶时下一次反正只能全扫,逐行快照抓了也用不上:不管是首次全扫就封顶,
    /// 还是输出不停、一轮轮地接着封顶,都不该逐格哈希整个回滚区。
    #[test]
    fn 封顶路径不抓快照() {
        let (e, mut s) = capped_setup(10);
        s.set_query("needle");

        let before = captures();
        s.refresh(&e);
        assert_eq!(s.count(), 10);
        assert_eq!(captures(), before, "首次全扫就封顶,不抓");
        assert!(s.snapshot.is_none());

        for step in 1..=5u64 {
            e.advance(b"needle more\r\n");
            sync_later(&mut s, &e, step);
            assert_eq!(s.matches(), fresh_capped(&e, "needle", 10).as_slice());
            assert_eq!(captures(), before, "第 {step} 轮仍封顶,不抓");
            assert!(s.snapshot.is_none());
        }
        assert_eq!(s.incremental_scans, 0, "封顶期间一次增量都走不了");
    }

    /// 结果掉回上限以下:这一轮结果完整了才抓快照,下一轮恢复增量,且与全扫逐条相同。
    #[test]
    fn 结果降到上限以下后恢复增量() {
        let (e, mut s) = capped_setup(10);
        s.set_query("needle");
        s.refresh(&e);
        assert_eq!(s.count(), 10);

        // 清屏 + 清历史,命中掉到上限以下
        e.advance(b"\x1b[2J\x1b[3J\x1b[Hneedle once\r\n");
        let before = captures();
        sync_later(&mut s, &e, 1);
        assert_eq!(s.count(), 1);
        assert_eq!(captures(), before + 1, "结果完整了就抓一份");
        assert_eq!(s.incremental_scans, 0, "手上没有旧快照,这一轮还是全扫");

        e.advance(b"plain\r\nneedle twice\r\n");
        sync_later(&mut s, &e, 2);
        assert_eq!(s.incremental_scans, 1, "恢复增量");
        assert_eq!(s.count(), 2);
        assert_eq!(s.matches(), fresh_capped(&e, "needle", 10).as_slice());
    }

    /// 增量那一路:没封顶时抓新快照照旧(计划要拿它逐行核对);这一轮涨到上限之后,
    /// 下一轮才回到不抓的路径。
    #[test]
    fn 涨到上限那一轮之后才不再抓() {
        let e = TerminalEmulator::with_scrollback(TermSize::new(40, 5), 200);
        e.advance(b"needle needle\r\n");
        let mut s = TerminalSearch::with_limits(SearchLimits {
            max_matches: 4,
            ..Default::default()
        });
        s.set_query("needle");
        s.refresh(&e);
        assert_eq!(s.count(), 2);
        assert!(s.snapshot.is_some());

        let before = captures();
        e.advance(b"needle needle needle\r\n");
        sync_later(&mut s, &e, 1);
        assert_eq!(s.count(), 4);
        assert_eq!(s.incremental_scans, 1, "上一轮结果完整,这一轮走增量");
        assert_eq!(captures(), before + 1, "增量计划要拿新快照核对");
        assert_eq!(s.matches(), fresh_capped(&e, "needle", 4).as_slice());

        let before = captures();
        e.advance(b"needle\r\n");
        sync_later(&mut s, &e, 2);
        assert_eq!(s.count(), 4);
        assert_eq!(captures(), before, "已经封顶,不再抓");
        assert!(s.snapshot.is_none());
    }

    /// 只改关键词时内容没动:旧快照留着,换成封顶的关键词也一样(不花钱);
    /// 内容一动,它就对不上了。
    #[test]
    fn 内容没动时封顶也留着旧快照() {
        let (e, mut s) = capped_setup(10);
        s.set_query("zzz");
        s.refresh(&e);
        assert_eq!(s.count(), 0);
        let rows = s.snapshot.as_ref().map(|snap| snap.rows.as_ptr());
        assert!(rows.is_some());

        let before = captures();
        s.set_query("needle");
        s.refresh(&e);
        assert_eq!(s.count(), 10);
        assert_eq!(captures(), before);
        assert_eq!(s.snapshot.as_ref().map(|snap| snap.rows.as_ptr()), rows);

        e.advance(b"more\r\n");
        sync_later(&mut s, &e, 1);
        assert_eq!(captures(), before);
        assert!(s.snapshot.is_none(), "内容变了,旧快照作废且不重抓");
    }

    #[test]
    fn 行哈希对字符_位置_折行标志都敏感() {
        let row = |text: &str| -> Vec<Cell> {
            text.chars()
                .map(|c| Cell {
                    c,
                    ..Default::default()
                })
                .collect()
        };
        let base = row("abcdefghij");
        assert_eq!(row_hash(&base), row_hash(&row("abcdefghij")));
        assert_ne!(row_hash(&base), row_hash(&row("abcdefghik")), "末字不同");
        assert_ne!(row_hash(&base), row_hash(&row("bacdefghij")), "换位");
        assert_ne!(row_hash(&base), row_hash(&row("abcdefghi")), "长度不同");
        let mut wrapped = base.clone();
        wrapped[9].flags.insert(Flags::WRAPLINE);
        assert_ne!(row_hash(&base), row_hash(&wrapped), "折行标志进哈希");
        let mut bold = base.clone();
        bold[3].flags.insert(Flags::BOLD);
        assert_eq!(row_hash(&base), row_hash(&bold), "粗体不影响命中,不进哈希");
    }

    #[test]
    fn 自适应去抖按上次耗时放大且封顶() {
        let mut s = TerminalSearch::new();
        assert_eq!(
            s.debounce(),
            Duration::from_millis(200),
            "便宜的扫描不改变节奏"
        );
        s.last_cost = Duration::from_millis(5);
        assert_eq!(s.debounce(), Duration::from_millis(200));
        s.last_cost = Duration::from_millis(60);
        assert_eq!(s.debounce(), Duration::from_millis(480));
        s.last_cost = Duration::from_secs(1);
        assert_eq!(s.debounce(), ADAPTIVE_DEBOUNCE_CAP);
    }
}
