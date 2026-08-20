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
//! 用户还能开到十万 —— 每帧扫一遍是不可能的。三道闸按顺序拦:
//!
//! 1. **脏标记**:关键词 / 选项变了 → 立刻重搜(用户在等结果,不能拖)。
//! 2. **去抖**:内容变化引起的重搜最快 200ms 一次(与 xterm SearchAddon
//!    `_updateMatches` 的 200ms 完全一致)。
//! 3. **内容指纹**:去抖到期后先算一遍屏幕内容的哈希([`content_fingerprint`]),
//!    与上次相同就直接跳过 —— 空闲时(最常见)一次扫描都不会发生。
//!
//! 指纹只哈屏幕区(不含 scrollback)+ 总行数:任何新输出都必然先经过屏幕,
//! 所以「内容变了而指纹不变」需要恰好滚过整数屏且内容逐字相同,可以忽略。
//! 指纹**不含 display_offset**,用户滚动回看不会白白触发重搜。
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

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::{Duration, Instant};

use alacritty_terminal::grid::{Dimensions as _, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::search::{RegexIter, RegexSearch};
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
            self.commit(Vec::new(), None);
        }
    }

    pub fn limits(&self) -> SearchLimits {
        self.limits
    }

    pub fn set_limits(&mut self, limits: SearchLimits) {
        self.limits = limits;
        self.dirty = true;
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
        self.commit(Vec::new(), None);
    }

    // -- 扫描 ---------------------------------------------------------------

    /// 每帧调一次(渲染层已经替宿主调了)。必要时重搜,返回结果集是否变化。
    pub fn sync(&mut self, emulator: &TerminalEmulator) -> bool {
        self.sync_at(emulator, Instant::now())
    }

    /// [`Self::sync`] 的可注入时钟版本,单测用。
    pub fn sync_at(&mut self, emulator: &TerminalEmulator, now: Instant) -> bool {
        if !self.is_active() {
            return if self.matches.is_empty() {
                false
            } else {
                self.commit(Vec::new(), None);
                true
            };
        }
        if self.dirty {
            return self.scan(emulator, now);
        }
        // 去抖:内容变化引起的重搜最快 200ms 一次
        let due = self
            .last_scan
            .map(|t| now.saturating_duration_since(t) >= self.limits.debounce)
            .unwrap_or(true);
        if !due {
            return false;
        }
        let fingerprint = emulator.with_term(content_fingerprint);
        if fingerprint == self.last_fingerprint {
            // 内容一个字没变:把计时重置掉,下一轮 200ms 后再问一次
            self.last_scan = Some(now);
            return false;
        }
        self.scan(emulator, now)
    }

    /// 无条件立刻重搜(关键词 / 选项刚改完时用)。返回结果集是否变化。
    pub fn refresh(&mut self, emulator: &TerminalEmulator) -> bool {
        self.dirty = true;
        self.sync_at(emulator, Instant::now())
    }

    fn scan(&mut self, emulator: &TerminalEmulator, now: Instant) -> bool {
        self.dirty = false;
        self.last_scan = Some(now);

        if !self.ensure_compiled() {
            self.last_fingerprint = emulator.with_term(content_fingerprint);
            let changed = !self.matches.is_empty();
            self.commit(Vec::new(), None);
            return changed;
        }

        // 借出 DFA:`with_term` 的闭包要 `&mut RegexSearch`,而 self 的其它字段
        // 之后还要写 —— 先把需要的标量拷出来,别让闭包捕获整个 self。
        let options = self.options;
        let limits = self.limits;
        let previous = self.current_match().map(|m| m.start);

        let Some((_, dfa)) = self.compiled.as_mut() else {
            return false;
        };
        let (found, fingerprint, columns, anchor) = emulator.with_term(|term| {
            let found = collect_matches(term, dfa, options, &limits);
            let anchor = -(term.grid().display_offset() as i32);
            (
                found,
                content_fingerprint(term),
                term.columns(),
                anchor,
            )
        });

        self.last_fingerprint = fingerprint;
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
        if out.len() >= limits.max_matches {
            break;
        }
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
}
