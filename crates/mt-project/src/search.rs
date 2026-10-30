//! 项目内搜索(文件名 / 文件内容),可取消。
//!
//! # 与 Tauri 版的差别
//!
//! - 取消不再走「前端发 `cancel_search` 命令 → managed `SearchManager` 查 id」这条
//!   跨进程链路,而是 [`start_search`] 直接返回一个 [`SearchHandle`],谁拿着谁能取消。
//!   [`SearchManager`] 仍保留,但只做「同一项目同时只留一个搜索」这一件事,
//!   键从 `search_id` 换成项目根路径 —— id 本来就只是为了跨 IPC 对齐事件。
//! - 结果不再 `emit`,改为回调 sink 收 [`SearchEvent`]。
//! - **分批仍然保留**(50 条 / 100ms):现在不再是摊薄 IPC,而是别让 UI 线程
//!   被上万条命中逐条打断。
//!
//! # 遍历与限额
//!
//! 两段式:先用 `ignore` 的并行遍历把候选文件收齐、按 [`sort_key`] 排好序;再多线程
//! 逐个文件求命中,由调用线程**按候选顺序**往外吐([`scan_ordered`])——并行只改快慢,
//! 不改结果与顺序,同一次输入每次都一样。结果收满 [`MAX_RESULTS`] 条即收工:排在后面
//! 的文件怎么也挤不进前 N 条,不必再读。
//!
//! 内容搜索另有三道闸,挡的是「项目里躺着大日志 / 数据集 / 压缩过的 JS」这一类:
//! 跳过大于 [`MAX_CONTENT_FILE_BYTES`] 的文件、先读文件头判二进制再决定读不读全文
//! ([`read_text_file`])、超长命中行只留命中点附近一段([`clip_long_line`])。

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use parking_lot::Mutex;
use regex::Regex;
use serde::Serialize;

// ── 限额 ──

/// 结果条数上限。与 `mt-app` 搜索框的展示上限是同一个数(原版 `SearchModal.tsx` 里
/// 的字面量 1000):超出的部分界面反正不显示,后端收满就停,不再白读剩下的文件。
pub const MAX_RESULTS: usize = 1000;

/// 内容搜索跳过大于这个字节数的文件。
///
/// 大日志、数据集、打包产物动辄几十上百 MB,整份读进内存再逐行比对,内存尖峰与耗时
/// 几乎全压在这类文件上,而它们极少是用户想搜的东西。4 MB 装得下绝大多数手写源码与
/// 常见的 lock 文件。只作用于内容搜索——文件名搜索照样能按名字找到大文件。
pub const MAX_CONTENT_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// 二进制判定只看文件头这么多字节:出现 NUL 即判二进制(与 git 同一判据)。
const BINARY_SNIFF_BYTES: usize = 8192;

/// 命中行超过这么多 char 就只保留命中点附近的一段,见 [`clip_long_line`]。
pub const MAX_LINE_CHARS: usize = 400;

/// 截断窗口里首个命中点之前留多少 char 的上文。结果行一行只显示得下一百来个字符,
/// 上文留多了命中点反而被挤出可见区。
const LINE_CONTEXT_BEFORE: usize = 40;

/// 截断处补的省略标记。单个 char,平移区间时按 1 计。
const ELLIPSIS: char = '…';

// ── Data structures ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    FileName,
    FileContent,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResultItem {
    /// 相对项目根的路径。
    pub file_path: PathBuf,
    pub file_name: String,
    pub line_number: Option<u32>,
    /// 命中行。超过 [`MAX_LINE_CHARS`] 个 char 时只是命中点附近的一段,被截掉的一侧
    /// 补 `…`(见 [`clip_long_line`]),`match_ranges` 已随之平移。
    pub line_content: Option<String>,
    /// 命中区间,按 **char** 计(不是字节),上层直接拿去切片高亮。
    pub match_ranges: Vec<(usize, usize)>,
    /// 文件名模式下 `match_ranges` 落在哪一段:`true` 落在 `file_path`(按路径搜时),
    /// `false` 落在 `file_name`。内容模式恒为 `false`(区间落在 `line_content`)。
    /// 路径口径见 [`path_for_match`]——分隔符换成 `/` 不改变 char 数,上层拿
    /// `file_path.display()` 原样高亮即可。
    pub match_in_path: bool,
}

/// 搜索过程中回调给上层的事件。
#[derive(Debug, Clone)]
pub enum SearchEvent {
    /// 一批命中(50 条或 100ms 攒一批)。整次搜索里按「文件路径 → 行号」有序。
    Results(Vec<SearchResultItem>),
    /// 搜索结束。`cancelled=true` 表示是被取消的,结果不完整;`truncated=true` 表示
    /// 收满 [`MAX_RESULTS`] 条后提前收工、后面可能还有命中——此时 `total_count` 就是
    /// 上限,不再是完整命中数。
    Complete {
        total_count: u32,
        cancelled: bool,
        truncated: bool,
    },
}

/// 一次搜索的输入。
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub project_root: PathBuf,
    pub query: String,
    pub mode: SearchMode,
    pub use_regex: bool,
}

// ── 取消句柄 ──

/// 可取消句柄。克隆出去的副本共享同一个标志位,取消任意一个即取消整次搜索。
#[derive(Debug, Clone, Default)]
pub struct SearchHandle {
    cancel: Arc<AtomicBool>,
}

impl SearchHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取消搜索。worker 在下一个文件/下一行边界上退出,不保证立即返回。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// 是否指向同一次搜索(克隆体之间为真)。
    pub fn same(&self, other: &SearchHandle) -> bool {
        Arc::ptr_eq(&self.cancel, &other.cancel)
    }
}

// ── SearchManager ──

/// 「同一项目同时只跑一个搜索」的簿记。可选:调用方自己持有 [`SearchHandle`]
/// 也能达到同样效果,这个类型只是把这条规则收在一处。
#[derive(Default)]
pub struct SearchManager {
    // project_root → 该项目当前在跑的搜索
    active: Mutex<HashMap<PathBuf, SearchHandle>>,
}

impl SearchManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一次新搜索,并取消同一项目上此前那次。
    pub fn register(&self, project_root: &Path) -> SearchHandle {
        let handle = SearchHandle::new();
        let mut active = self.active.lock();
        if let Some(prev) = active.insert(project_root.to_path_buf(), handle.clone()) {
            prev.cancel();
        }
        handle
    }

    pub fn cancel(&self, project_root: &Path) {
        if let Some(handle) = self.active.lock().remove(project_root) {
            handle.cancel();
        }
    }

    /// 搜索结束后摘掉登记。带句柄比对:一次迟到的收尾不该把后来者的登记删掉。
    pub fn remove(&self, project_root: &Path, handle: &SearchHandle) {
        let mut active = self.active.lock();
        if active.get(project_root).is_some_and(|h| h.same(handle)) {
            active.remove(project_root);
        }
    }
}

// ── Helpers ──

fn is_binary(data: &[u8]) -> bool {
    data.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0)
}

/// 读一个文本文件:先只读文件头判二进制(是就到此为止,不必整份读进内存),不是再读完
/// 剩下的部分。二进制 / 非 UTF-8 / 读失败 / 遍历之后长过了上限,一律返回 `None`。
fn read_text_file(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let size_hint = file
        .metadata()
        .map_or(0, |m| m.len().min(MAX_CONTENT_FILE_BYTES)) as usize;
    // 多放行 1 字节:读满说明文件在遍历之后长过了上限(日志还在写),按超限跳过
    let mut reader = file.take(MAX_CONTENT_FILE_BYTES + 1);
    let mut buf = Vec::with_capacity(size_hint.min(BINARY_SNIFF_BYTES));
    (&mut reader)
        .take(BINARY_SNIFF_BYTES as u64)
        .read_to_end(&mut buf)
        .ok()?;
    if is_binary(&buf) {
        return None;
    }
    buf.reserve(size_hint.saturating_sub(buf.len()));
    reader.read_to_end(&mut buf).ok()?;
    if buf.len() as u64 > MAX_CONTENT_FILE_BYTES {
        return None;
    }
    String::from_utf8(buf).ok()
}

/// 搜索用几个线程:留一个核给 UI 与终端渲染,封顶 8(再多先撞磁盘瓶颈)。
fn worker_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .saturating_sub(1)
        .clamp(1, 8)
}

/// `max_filesize` 只给内容搜索用:文件名搜索要能按名字找到大文件。
fn build_walker(root: &Path, max_filesize: Option<u64>) -> ignore::WalkParallel {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .max_filesize(max_filesize)
        .threads(worker_threads());
    // 与文件树同一把尺子:黑名单目录 + worktree 根下的 `.git` 指针文件
    builder.filter_entry(|entry| {
        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        let name = entry.file_name().to_str().unwrap_or("");
        !crate::fs::is_always_hidden(name, is_dir)
    });
    builder.build_parallel()
}

/// 大小写不敏感子串搜索，直接返回【原始 text】的 char 区间（上层按 char 高亮）。
///
/// 逐字符做小写折叠，同时记录每个小写字符来自哪个原始字符；匹配在小写字符序列
/// 上按 char 进行，命中后回映射到原始 char 下标。这样即便 Unicode 大小写折叠改变
/// 长度（İ→i̇、ǅ→ǆ 等），结果也始终落在原始字符边界上——从根本上避免了旧实现里
/// 「按字节 +1 步进切多字节字符 panic」以及「在 to_lowercase() 串上算偏移却拿原串
/// 做 byte→char 映射导致越界 / 错位」两个问题。query_lower 由调用方预先小写化。
fn find_substring_char_ranges(text: &str, query_lower: &str) -> Vec<(usize, usize)> {
    if query_lower.is_empty() {
        return Vec::new();
    }
    // 纯 ASCII 行(压缩过的 JS、日志绝大多数如此)走零分配的快速路径:ASCII 字符的
    // 小写折叠一对一且仍是 ASCII,char 下标就是字节下标,结果与下面的通用路径逐位相同
    // ——通用路径要为每个 char 建两张表(12 字节/char),几 MB 的单行会凭空多出几十 MB。
    // 非 ASCII 的查询词不可能命中纯 ASCII 文本:ASCII 字符小写后还是 ASCII。
    if text.is_ascii() {
        if !query_lower.is_ascii() {
            return Vec::new();
        }
        return find_ascii_ranges(text.as_bytes(), query_lower.as_bytes());
    }
    let query_chars: Vec<char> = query_lower.chars().collect();
    // 小写字符序列 + 每个小写字符对应的原始字符下标
    let mut lower_chars: Vec<char> = Vec::new();
    let mut origin: Vec<usize> = Vec::new();
    for (orig_ci, ch) in text.chars().enumerate() {
        for lc in ch.to_lowercase() {
            lower_chars.push(lc);
            origin.push(orig_ci);
        }
    }
    let qn = query_chars.len();
    let mut result: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i + qn <= lower_chars.len() {
        if lower_chars[i..i + qn] == query_chars[..] {
            let start_char = origin[i];
            let end_char = origin[i + qn - 1] + 1; // 覆盖最后一个原始字符的完整宽度
            if result.last() != Some(&(start_char, end_char)) {
                result.push((start_char, end_char));
            }
            i += qn; // 非重叠匹配
        } else {
            i += 1;
        }
    }
    result
}

/// [`find_substring_char_ranges`] 的纯 ASCII 版:同样的非重叠从左到右匹配。
fn find_ascii_ranges(text: &[u8], query_lower: &[u8]) -> Vec<(usize, usize)> {
    let qn = query_lower.len();
    let mut result = Vec::new();
    let mut i = 0;
    while i + qn <= text.len() {
        let hit = text[i..i + qn]
            .iter()
            .zip(query_lower)
            .all(|(t, q)| t.to_ascii_lowercase() == *q);
        if hit {
            result.push((i, i + qn));
            i += qn;
        } else {
            i += 1;
        }
    }
    result
}

fn find_regex_matches(text: &str, re: &Regex) -> Vec<(usize, usize)> {
    re.find_iter(text).map(|m| (m.start(), m.end())).collect()
}

/// 把字节区间换算成 char 区间,非 ASCII 文本(CJK、emoji)才不会切错位置。
fn byte_ranges_to_char_ranges(text: &str, byte_ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    // 纯 ASCII:字节下标就是 char 下标,不必建映射表(超长行上这张表是 8 字节/字节)
    if byte_ranges.is_empty() || text.is_ascii() {
        return byte_ranges;
    }
    let mut byte_to_char = vec![0usize; text.len() + 1];
    for (ci, (bi, _)) in text.char_indices().enumerate() {
        byte_to_char[bi] = ci;
    }
    let total_chars = text.chars().count();
    byte_to_char[text.len()] = total_chars;
    byte_ranges
        .into_iter()
        .map(|(s, e)| (byte_to_char[s], byte_to_char[e]))
        .collect()
}

/// 超长命中行只留命中点附近一段。压缩过的 JS、单行 JSON 动辄几十万字符,整行塞进
/// 结果既占内存,又让结果行画出一长串文字元素。返回 `(显示文本, 区间)`,区间与显示
/// 文本同口径(char 下标):
///
/// - 窗口从首个命中点往前留 [`LINE_CONTEXT_BEFORE`] 个 char 起,共 [`MAX_LINE_CHARS`]
///   个 char;靠近行尾时整体前移补足。只在 char 边界上切,不会切断 UTF-8;
/// - 被截掉的一侧补一个 [`ELLIPSIS`];
/// - 区间先裁到窗口内(整段落在窗外的丢掉),再整体平移「−窗口起点 + 前缀省略号长度」,
///   上层拿它切 `line_content` 高亮,位置因此仍然正确。
fn clip_long_line(line: &str, ranges: Vec<(usize, usize)>) -> (String, Vec<(usize, usize)>) {
    // 字节数不超,char 数更不会超
    if line.len() <= MAX_LINE_CHARS {
        return (line.to_string(), ranges);
    }
    let total = line.chars().count();
    if total <= MAX_LINE_CHARS {
        return (line.to_string(), ranges);
    }
    let first = ranges.first().map_or(0, |r| r.0);
    let end = (first.saturating_sub(LINE_CONTEXT_BEFORE) + MAX_LINE_CHARS).min(total);
    let start = end - MAX_LINE_CHARS;

    // 窗口两端的 char 下标换成字节下标
    let (mut byte_start, mut byte_end) = (line.len(), line.len());
    for (ci, (bi, _)) in line.char_indices().enumerate() {
        if ci == start {
            byte_start = bi;
        }
        if ci == end {
            byte_end = bi;
            break;
        }
    }
    let lead = usize::from(start > 0);
    let mut text = String::with_capacity(byte_end - byte_start + 2 * ELLIPSIS.len_utf8());
    if start > 0 {
        text.push(ELLIPSIS);
    }
    text.push_str(&line[byte_start..byte_end]);
    if end < total {
        text.push(ELLIPSIS);
    }

    let ranges = ranges
        .into_iter()
        .filter_map(|(s, e)| {
            let (cs, ce) = (s.max(start), e.min(end));
            // 零宽命中(正则 `^` / `$` 之类)只要落在窗口内也保留
            if cs < ce || (s == e && (start..=end).contains(&s)) {
                Some((cs - start + lead, ce - start + lead))
            } else {
                None
            }
        })
        .collect();
    (text, ranges)
}

/// 编译好的匹配器,两种模式共用。返回的都是 char 区间。
enum Matcher {
    Regex(Regex),
    /// 已小写化的子串,大小写不敏感。
    Substring(String),
}

impl Matcher {
    fn char_ranges(&self, text: &str) -> Vec<(usize, usize)> {
        match self {
            Matcher::Regex(re) => byte_ranges_to_char_ranges(text, find_regex_matches(text, re)),
            Matcher::Substring(query_lower) => find_substring_char_ranges(text, query_lower),
        }
    }
}

// ── Result batching ──

struct ResultBatcher<F: Fn(SearchEvent)> {
    buffer: Vec<SearchResultItem>,
    last_flush: Instant,
    sink: F,
    total_count: u32,
}

impl<F: Fn(SearchEvent)> ResultBatcher<F> {
    fn new(sink: F) -> Self {
        Self {
            buffer: Vec::new(),
            last_flush: Instant::now(),
            sink,
            total_count: 0,
        }
    }

    fn push(&mut self, item: SearchResultItem) {
        self.total_count += 1;
        self.buffer.push(item);
        if self.buffer.len() >= 50 || self.last_flush.elapsed() >= Duration::from_millis(100) {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let items = std::mem::take(&mut self.buffer);
        (self.sink)(SearchEvent::Results(items));
        self.last_flush = Instant::now();
    }

    fn finish(mut self, cancelled: bool, truncated: bool) {
        self.flush();
        (self.sink)(SearchEvent::Complete {
            total_count: self.total_count,
            cancelled,
            truncated,
        });
    }
}

// ── 遍历与按序收集 ──

/// 遍历阶段收集到的一个候选文件。
struct Candidate {
    /// 排序键,见 [`sort_key`]。
    key: String,
    /// 相对项目根的路径。
    rel_path: PathBuf,
}

/// 结果排序键:相对路径逐段转大写、以 `\0` 相连。
///
/// 旧实现单线程顺序遍历,结果顺序 = 深度优先 + 同级按文件系统返回的顺序(NTFS 上即
/// 名字转大写后比较)。并行遍历的到达顺序每次都不一样,只能收齐后排序,这个键复现的
/// 正是那个顺序:`\0` 不会出现在文件名里且小于任何字符,整串比较因此等价于逐段比较
/// ——`a/x.txt` 排在 `a.txt` 之前,与深度优先先走完 `a/` 再到 `a.txt` 一致。只差
/// 大小写的同名项(Linux 上可以并存)再按原路径定序,保证是全序。
fn sort_key(rel_path: &Path) -> String {
    let mut key = String::new();
    for (i, part) in rel_path.components().enumerate() {
        if i > 0 {
            key.push('\0');
        }
        key.push_str(&part.as_os_str().to_string_lossy().to_uppercase());
    }
    key
}

fn file_name_of(rel_path: &Path) -> String {
    rel_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 第一段:并行遍历收齐候选文件(目录不算)并排好序。被取消时返回 `None`。
fn collect_candidates(
    root: &Path,
    max_filesize: Option<u64>,
    cancel: &SearchHandle,
) -> Option<Vec<Candidate>> {
    let found: Mutex<Vec<Candidate>> = Mutex::new(Vec::new());
    build_walker(root, max_filesize).run(|| {
        let found = &found;
        Box::new(move |entry| {
            if cancel.is_cancelled() {
                return ignore::WalkState::Quit;
            }
            // 读不了的条目(权限等)跳过,与旧的顺序遍历同口径
            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };
            if entry.file_type().is_none_or(|ft| ft.is_dir()) {
                return ignore::WalkState::Continue;
            }
            let rel_path = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_path_buf();
            let key = sort_key(&rel_path);
            found.lock().push(Candidate { key, rel_path });
            ignore::WalkState::Continue
        })
    });
    if cancel.is_cancelled() {
        return None;
    }
    let mut found = found.into_inner();
    found.sort_unstable_by(|a, b| a.key.cmp(&b.key).then_with(|| a.rel_path.cmp(&b.rel_path)));
    Some(found)
}

/// 第二段的协调者:收各候选文件的命中,**按候选下标顺序**往 batcher 吐;收满上限即
/// 收紧「截止下标」——下标不小于它的文件怎么也挤不进前 `cap` 条,不必再读。
struct InOrder<'a, F: Fn(SearchEvent)> {
    batcher: &'a mut ResultBatcher<F>,
    cap: usize,
    /// 下一个该吐出的候选下标。
    next: usize,
    /// 已经算完、但排在 `next` 之后的结果。
    pending: BTreeMap<usize, Vec<SearchResultItem>>,
    /// 截止下标(不含)。初值是候选总数,只会往前收。
    end: usize,
    /// 已吐出的条数。
    emitted: usize,
    /// 有命中因为上限被丢掉过。
    dropped: bool,
}

impl<'a, F: Fn(SearchEvent)> InOrder<'a, F> {
    fn new(batcher: &'a mut ResultBatcher<F>, cap: usize, total: usize) -> Self {
        Self {
            batcher,
            cap,
            next: 0,
            pending: BTreeMap::new(),
            end: total,
            emitted: 0,
            dropped: false,
        }
    }

    /// 收下第 `idx` 个候选的命中,吐出已经连成片的前缀,返回新的截止下标。
    fn accept(&mut self, idx: usize, items: Vec<SearchResultItem>) -> usize {
        // 截止收紧之前就已经在路上的结果
        if idx >= self.end {
            return self.end;
        }
        self.pending.insert(idx, items);
        while let Some(items) = self.pending.remove(&self.next) {
            self.next += 1;
            for item in items {
                if self.emitted >= self.cap {
                    self.dropped = true;
                    break;
                }
                self.batcher.push(item);
                self.emitted += 1;
            }
        }
        // 已吐出的,加上 pending 里按下标累加的条数:一旦够 cap,排在那之后的文件就
        // 不可能再进前 cap 条——中间还没算完的文件只会再往前面添命中,不会让它少
        let mut acc = self.emitted;
        if acc >= self.cap {
            self.end = self.end.min(self.next);
        } else {
            for (&j, items) in &self.pending {
                acc += items.len();
                if acc >= self.cap {
                    self.end = self.end.min(j + 1);
                    break;
                }
            }
        }
        let end = self.end;
        self.pending.retain(|&j, _| j < end);
        self.end
    }

    /// 截断了没有:丢过命中,或截止下标之后还有没看的文件(可能有命中)。
    fn truncated(&self, total: usize) -> bool {
        self.dropped || self.end < total
    }
}

/// 第二段:逐个候选求命中,按候选顺序交给 batcher,返回是否因上限截断。
///
/// `threads > 1` 时工作线程从一个原子下标上按候选顺序领活、并行算,本线程(调用
/// `run_search` 的那条,sink 也只在这条线程上调)当协调者按下标顺序收——结果顺序因此
/// 与线程调度无关。工作线程领到截止下标之外的活、或搜索被取消,就收工。
fn scan_ordered<F, P>(
    candidates: &[Candidate],
    threads: usize,
    cap: usize,
    cancel: &SearchHandle,
    batcher: &mut ResultBatcher<F>,
    per_file: P,
) -> bool
where
    F: Fn(SearchEvent),
    P: Fn(&Candidate) -> Vec<SearchResultItem> + Sync,
{
    let total = candidates.len();
    let mut order = InOrder::new(batcher, cap, total);
    if threads <= 1 {
        for (idx, candidate) in candidates.iter().enumerate() {
            if cancel.is_cancelled() || idx >= order.end {
                break;
            }
            order.accept(idx, per_file(candidate));
        }
        return order.truncated(total);
    }

    let next = AtomicUsize::new(0);
    let end = AtomicUsize::new(total);
    std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..threads.min(total) {
            let tx = tx.clone();
            let (next, end, per_file) = (&next, &end, &per_file);
            scope.spawn(move || {
                while !cancel.is_cancelled() {
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= end.load(Ordering::Relaxed) {
                        break;
                    }
                    if tx.send((idx, per_file(&candidates[idx]))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for (idx, items) in rx {
            if cancel.is_cancelled() {
                break;
            }
            end.store(order.accept(idx, items), Ordering::Relaxed);
        }
    });
    order.truncated(total)
}

// ── Search functions ──

/// 文件名模式的查询串里带路径分隔符,就说明用户要按路径找(issue #57:输入
/// `pages/task/my/my` 期望命中 `src/pages/task/my/my.vue`,而 `/` 不可能出现在
/// 文件名里,只匹配文件名永远是 0 结果)。
///
/// `/` 在任何模式下都算;`\` 只在非正则模式下算——正则里它是转义符(`\.vue`)。
/// 不带分隔符仍只匹配裸文件名:搜 `my` 时不该把 `my/` 目录下所有文件都冲出来。
fn wants_path_match(query: &str, use_regex: bool) -> bool {
    query.contains('/') || (!use_regex && query.contains('\\'))
}

/// 按路径搜时的被搜文本:相对项目根的路径,分隔符统一成 `/`。Windows 上 walker 给的是
/// `\`,用户照 issue 里的习惯敲 `/`;两者都是单个 char,换掉不影响区间下标。
fn path_for_match(rel_path: &Path) -> String {
    rel_path.to_string_lossy().replace('\\', "/")
}

/// 非正则路径查询的归一化:`\` 换成 `/`、去掉开头的 `/` 或 `./`(相对路径没有这两种
/// 前缀,留着必然搜不到),再小写。
fn normalize_path_query(query: &str) -> String {
    let q = query.replace('\\', "/");
    let q = q.strip_prefix("./").unwrap_or(&q);
    q.trim_start_matches('/').to_lowercase()
}

/// 返回是否因上限截断。
fn search_filenames<F: Fn(SearchEvent)>(
    root: &Path,
    query: &str,
    use_regex: bool,
    cap: usize,
    cancel: &SearchHandle,
    batcher: &mut ResultBatcher<F>,
) -> Result<bool> {
    let match_in_path = wants_path_match(query, use_regex);
    let matcher = if use_regex {
        Matcher::Regex(compile_regex(query)?)
    } else if match_in_path {
        Matcher::Substring(normalize_path_query(query))
    } else {
        Matcher::Substring(query.to_lowercase())
    };
    let Some(candidates) = collect_candidates(root, None, cancel) else {
        return Ok(false);
    };
    // 只比对名字,活太轻,多线程的调度开销反而更大:本线程按序扫
    Ok(scan_ordered(
        &candidates,
        1,
        cap,
        cancel,
        batcher,
        |candidate| {
            let file_name = file_name_of(&candidate.rel_path);
            let match_ranges = if match_in_path {
                matcher.char_ranges(&path_for_match(&candidate.rel_path))
            } else {
                matcher.char_ranges(&file_name)
            };
            if match_ranges.is_empty() {
                return Vec::new();
            }
            vec![SearchResultItem {
                file_path: candidate.rel_path.clone(),
                file_name,
                line_number: None,
                line_content: None,
                match_ranges,
                match_in_path,
            }]
        },
    ))
}

/// 返回是否因上限截断。
fn search_contents<F: Fn(SearchEvent)>(
    root: &Path,
    query: &str,
    use_regex: bool,
    cap: usize,
    cancel: &SearchHandle,
    batcher: &mut ResultBatcher<F>,
) -> Result<bool> {
    let matcher = if use_regex {
        Matcher::Regex(compile_regex(query)?)
    } else {
        Matcher::Substring(query.to_lowercase())
    };
    let Some(candidates) = collect_candidates(root, Some(MAX_CONTENT_FILE_BYTES), cancel) else {
        return Ok(false);
    };
    let per_file = |candidate: &Candidate| {
        let Some(text) = read_text_file(&root.join(&candidate.rel_path)) else {
            return Vec::new();
        };
        let file_name = file_name_of(&candidate.rel_path);
        let mut items = Vec::new();
        for (line_idx, line) in text.lines().enumerate() {
            if cancel.is_cancelled() {
                break;
            }
            let char_ranges = matcher.char_ranges(line);
            if char_ranges.is_empty() {
                continue;
            }
            let (line_content, match_ranges) = clip_long_line(line, char_ranges);
            items.push(SearchResultItem {
                file_path: candidate.rel_path.clone(),
                file_name: file_name.clone(),
                line_number: Some((line_idx + 1) as u32),
                line_content: Some(line_content),
                match_ranges,
                match_in_path: false,
            });
            // 同一文件的命中按行号有序,超出上限的那些怎么也排不进前 cap 条。多收 1 条
            // 是给协调者看的:它会丢掉这条并据此把本次搜索标成「已截断」
            if items.len() > cap {
                break;
            }
        }
        items
    };
    Ok(scan_ordered(
        &candidates,
        worker_threads(),
        cap,
        cancel,
        batcher,
        per_file,
    ))
}

fn compile_regex(query: &str) -> Result<Regex> {
    Regex::new(query).map_err(|e| anyhow::anyhow!("Invalid regex: {}", e))
}

// ── 入口 ──

/// 在**当前线程**上跑完一次搜索。想放到自己的执行器上(GPUI 的
/// `background_executor`)就用它;想要「起一个后台线程就不管了」用 [`start_search`]。
/// 遍历与读文件另起工作线程并行做,但 sink 只在当前线程上被调用。
///
/// sink 会先收到若干 [`SearchEvent::Results`](按文件路径、行号有序,最多
/// [`MAX_RESULTS`] 条),最后必定收到一条 [`SearchEvent::Complete`] —— 即便中途
/// panic 也不例外。
pub fn run_search<F>(req: SearchRequest, cancel: SearchHandle, sink: F)
where
    F: Fn(SearchEvent),
{
    run_search_capped(req, cancel, sink, MAX_RESULTS);
}

/// [`run_search`] 的本体,上限可调(测试拿小上限造「收满」)。
fn run_search_capped<F>(req: SearchRequest, cancel: SearchHandle, sink: F, cap: usize)
where
    F: Fn(SearchEvent),
{
    let mut batcher = ResultBatcher::new(sink);
    // 用 catch_unwind 兜底:即便搜索体内将来再出现 panic,也不会跳过下面的 finish(),
    // 否则上层永远收不到 Complete、搜索框卡死在 loading。工作线程里的 panic 由
    // `thread::scope` 在汇合时转抛到这里。
    // AssertUnwindSafe 是因为 batcher 跨越捕获边界。
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match req.mode {
        SearchMode::FileName => search_filenames(
            &req.project_root,
            &req.query,
            req.use_regex,
            cap,
            &cancel,
            &mut batcher,
        ),
        SearchMode::FileContent => search_contents(
            &req.project_root,
            &req.query,
            req.use_regex,
            cap,
            &cancel,
            &mut batcher,
        ),
    }));
    let truncated = match outcome {
        Ok(Ok(truncated)) => truncated,
        // 非法正则:start_search 在起线程前就拦下了,只有直接调 run_search 才会走到这
        Ok(Err(_)) => false,
        Err(_) => {
            eprintln!("[search] worker panicked while searching {:?}", req.query);
            false
        }
    };
    batcher.finish(cancel.is_cancelled(), truncated);
}

/// 起一个后台线程跑搜索,立刻返回可取消句柄。
///
/// 空 query 与非法正则在返回前就被拒绝(不会起线程,也就不会有 Complete 事件)。
pub fn start_search<F>(req: SearchRequest, sink: F) -> Result<SearchHandle>
where
    F: Fn(SearchEvent) + Send + 'static,
{
    if req.query.is_empty() {
        bail!("Search query is empty");
    }
    if req.use_regex {
        compile_regex(&req.query)?;
    }

    let handle = SearchHandle::new();
    let worker_handle = handle.clone();
    std::thread::spawn(move || run_search(req, worker_handle, sink));
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn search_manager_register_and_cancel() {
        let mgr = SearchManager::new();
        let handle = mgr.register(Path::new("/project"));
        assert!(!handle.is_cancelled());
        mgr.cancel(Path::new("/project"));
        assert!(handle.is_cancelled());
    }

    #[test]
    fn search_manager_auto_cancels_same_project() {
        let mgr = SearchManager::new();
        let h1 = mgr.register(Path::new("/project"));
        let _h2 = mgr.register(Path::new("/project"));
        assert!(h1.is_cancelled());
    }

    #[test]
    fn search_manager_different_projects_independent() {
        let mgr = SearchManager::new();
        let h1 = mgr.register(Path::new("/project-a"));
        let _h2 = mgr.register(Path::new("/project-b"));
        assert!(!h1.is_cancelled());
    }

    #[test]
    fn search_manager_remove_only_matching_handle() {
        let mgr = SearchManager::new();
        let root = Path::new("/project");
        let stale = mgr.register(root);
        let current = mgr.register(root);
        // 上一次搜索迟到的收尾不该把当前这次的登记摘掉
        mgr.remove(root, &stale);
        mgr.cancel(root);
        assert!(current.is_cancelled(), "当前搜索的登记应仍在");
    }

    #[test]
    fn is_binary_detects_null_bytes() {
        assert!(is_binary(&[0x48, 0x65, 0x00, 0x6c]));
        assert!(!is_binary(b"Hello world"));
        assert!(!is_binary(b""));
    }

    #[test]
    fn find_substring_case_insensitive() {
        // ASCII：char 区间与 byte 区间相同
        let matches = find_substring_char_ranges("Hello World hello", "hello");
        assert_eq!(matches, vec![(0, 5), (12, 17)]);
    }

    #[test]
    fn find_substring_no_match() {
        let matches = find_substring_char_ranges("foo bar", "baz");
        assert!(matches.is_empty());
    }

    #[test]
    fn find_substring_empty_query() {
        // 空 query 不应匹配（也防御性避免任何死循环）
        assert!(find_substring_char_ranges("anything", "").is_empty());
    }

    #[test]
    fn find_substring_cjk_no_panic() {
        // 旧实现按 +1 字节步进，搜中文相邻字符必 panic（not a char boundary）。
        // 现按字符返回原始 char 区间。
        let matches = find_substring_char_ranges("你你你", "你");
        assert_eq!(matches, vec![(0, 1), (1, 2), (2, 3)]);
    }

    #[test]
    fn find_substring_cjk_substring() {
        // “好” 在原始文本里是第 1 个字符（char 下标 1..2）
        let matches = find_substring_char_ranges("你好world", "好");
        assert_eq!(matches, vec![(1, 2)]);
    }

    #[test]
    fn find_substring_turkish_dotted_i_no_panic() {
        // İ (U+0130) 小写为 "i̇"（2 个 char），旧实现会让偏移越过原串长度而 panic。
        // 搜 "i" 应高亮整个原始 İ 字符（char 区间 0..1）。
        let matches = find_substring_char_ranges("İ", "i");
        assert_eq!(matches, vec![(0, 1)]);
    }

    #[test]
    fn find_substring_emoji_no_panic() {
        let matches = find_substring_char_ranges("a😀b😀c", "😀");
        assert_eq!(matches, vec![(1, 2), (3, 4)]);
    }

    /// 纯 ASCII 快速路径必须与通用路径逐位一致(含重叠候选、首尾、大小写混排)。
    #[test]
    fn find_substring_ascii_fast_path_matches_general_path() {
        // 通用路径的参照:在文本末尾拼一个非 ASCII 字符逼它走通用分支,再去掉尾部影响
        let general = |text: &str, q: &str| {
            let forced = format!("{text}é");
            find_substring_char_ranges(&forced, q)
        };
        for (text, q) in [
            ("aaaa", "aa"),
            ("AbAbAb", "bab"),
            ("xHELLOxhello", "hello"),
            ("abc", "abcd"),
            ("abc", "c"),
            ("K", "k"),
        ] {
            assert_eq!(
                find_substring_char_ranges(text, q),
                general(text, q),
                "{text:?} / {q:?}"
            );
        }
        // 非 ASCII 查询词不可能命中纯 ASCII 文本
        assert!(find_substring_char_ranges("abc", "é").is_empty());
    }

    #[test]
    fn find_regex_matches_basic() {
        let re = Regex::new(r"\d+").unwrap();
        let matches = find_regex_matches("abc123def456", &re);
        assert_eq!(matches, vec![(3, 6), (9, 12)]);
    }

    #[test]
    fn byte_to_char_ranges_ascii() {
        let ranges = byte_ranges_to_char_ranges("hello", vec![(0, 5)]);
        assert_eq!(ranges, vec![(0, 5)]);
    }

    #[test]
    fn byte_to_char_ranges_cjk() {
        // "你好world" — "你" = 3 bytes, "好" = 3 bytes, "world" = 5 bytes
        let text = "你好world";
        // byte offsets for "world": starts at byte 6, ends at byte 11
        let ranges = byte_ranges_to_char_ranges(text, vec![(6, 11)]);
        // char offsets for "world": starts at char 2, ends at char 7
        assert_eq!(ranges, vec![(2, 7)]);
    }

    // ── 长行截断 ──

    /// 与 `mt-app::ui::highlight_runs` 同口径地按 char 区间切出高亮文字。
    fn highlighted_slices(text: &str, ranges: &[(usize, usize)]) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        ranges
            .iter()
            .map(|&(s, e)| chars[s..e].iter().collect())
            .collect()
    }

    #[test]
    fn clip_long_line_keeps_short_lines_untouched() {
        let line = "a".repeat(MAX_LINE_CHARS);
        let (text, ranges) = clip_long_line(&line, vec![(3, 5)]);
        assert_eq!(text, line);
        assert_eq!(ranges, vec![(3, 5)]);
        // 字节数超了但 char 数没超(中文)也不截
        let cjk = "中".repeat(MAX_LINE_CHARS);
        let (text, ranges) = clip_long_line(&cjk, vec![(0, 1)]);
        assert_eq!(text, cjk);
        assert_eq!(ranges, vec![(0, 1)]);
    }

    #[test]
    fn clip_long_line_windows_around_first_match_and_shifts_ranges() {
        // 5000 字符长行,命中点在 3000;另有一处命中在窗外(4500)
        let mut line = "x".repeat(5000);
        line.replace_range(3000..3006, "needle");
        line.replace_range(4500..4506, "needle");
        let ranges = find_substring_char_ranges(&line, "needle");
        assert_eq!(ranges, vec![(3000, 3006), (4500, 4506)]);

        let (text, clipped) = clip_long_line(&line, ranges);
        assert_eq!(
            text.chars().count(),
            MAX_LINE_CHARS + 2,
            "两侧各补一个省略号"
        );
        assert!(text.starts_with(ELLIPSIS) && text.ends_with(ELLIPSIS));
        // 窗口从 3000-40 起;前缀省略号占 1 个 char
        assert_eq!(
            clipped,
            vec![(LINE_CONTEXT_BEFORE + 1, LINE_CONTEXT_BEFORE + 7)]
        );
        assert_eq!(highlighted_slices(&text, &clipped), ["needle"]);
    }

    #[test]
    fn clip_long_line_cjk_offsets_stay_on_char_boundaries() {
        // 全中文长行(3 字节/char):窗口按 char 切,不能切断 UTF-8,高亮仍落在「目标」上
        let mut chars: Vec<char> = "中文内容".chars().cycle().take(5000).collect();
        chars.splice(2500..2502, "目标".chars());
        chars.splice(2600..2602, "目标".chars());
        let line: String = chars.into_iter().collect();
        let ranges = find_substring_char_ranges(&line, "目标");
        assert_eq!(ranges, vec![(2500, 2502), (2600, 2602)]);

        let (text, clipped) = clip_long_line(&line, ranges);
        assert_eq!(text.chars().count(), MAX_LINE_CHARS + 2);
        assert_eq!(clipped.len(), 2, "两处命中都在窗口内");
        assert_eq!(highlighted_slices(&text, &clipped), ["目标", "目标"]);
        assert_eq!(clipped[1].0 - clipped[0].0, 100, "相对距离不变");
    }

    #[test]
    fn clip_long_line_near_edges() {
        // 命中在行首附近:窗口从 0 起,不补前缀省略号
        let mut line = "y".repeat(1000);
        line.replace_range(10..13, "abc");
        let (text, clipped) = clip_long_line(&line, vec![(10, 13)]);
        assert!(!text.starts_with(ELLIPSIS) && text.ends_with(ELLIPSIS));
        assert_eq!(clipped, vec![(10, 13)]);
        assert_eq!(highlighted_slices(&text, &clipped), ["abc"]);

        // 命中在行尾附近:窗口整体前移补足 400 个 char,不补后缀省略号
        let mut line = "y".repeat(1000);
        line.replace_range(995..998, "abc");
        let (text, clipped) = clip_long_line(&line, vec![(995, 998)]);
        assert!(text.starts_with(ELLIPSIS) && !text.ends_with(ELLIPSIS));
        assert_eq!(text.chars().count(), MAX_LINE_CHARS + 1);
        assert_eq!(highlighted_slices(&text, &clipped), ["abc"]);

        // 行尾的零宽命中(正则 `$`)保留在窗口末尾
        let (text, clipped) = clip_long_line(&line, vec![(1000, 1000)]);
        assert_eq!(clipped, vec![(MAX_LINE_CHARS + 1, MAX_LINE_CHARS + 1)]);
        assert_eq!(text.chars().count(), MAX_LINE_CHARS + 1);
    }

    #[test]
    fn clip_long_line_clips_match_longer_than_window() {
        // 正则一口吞掉整行(`.*`):区间裁到窗口,不越界
        let line = "z".repeat(2000);
        let (text, clipped) = clip_long_line(&line, vec![(0, 2000)]);
        assert_eq!(clipped, vec![(0, MAX_LINE_CHARS)]);
        assert_eq!(text.chars().count(), MAX_LINE_CHARS + 1);
    }

    // ── 按序收集 / 上限 / 取消(不碰文件系统) ──

    fn fake_candidates(n: usize) -> Vec<Candidate> {
        (0..n)
            .map(|i| {
                let rel_path = PathBuf::from(format!("f{i:05}.txt"));
                Candidate {
                    key: sort_key(&rel_path),
                    rel_path,
                }
            })
            .collect()
    }

    fn hits(candidate: &Candidate, n: usize) -> Vec<SearchResultItem> {
        (0..n)
            .map(|line| SearchResultItem {
                file_path: candidate.rel_path.clone(),
                file_name: file_name_of(&candidate.rel_path),
                line_number: Some(line as u32 + 1),
                line_content: Some(String::new()),
                match_ranges: vec![(0, 0)],
                match_in_path: false,
            })
            .collect()
    }

    /// 乱序到达、收满上限:吐出的必是按下标的前 cap 条,截止下标随之收紧。
    #[test]
    fn in_order_emits_prefix_and_tightens_end() {
        let candidates = fake_candidates(10);
        let got = std::cell::RefCell::new(Vec::new());
        let mut batcher = ResultBatcher::new(|ev| {
            if let SearchEvent::Results(items) = ev {
                got.borrow_mut().extend(items);
            }
        });
        let mut order = InOrder::new(&mut batcher, 5, 10);
        // 3 号先到且一家就够 5 条:3 号之后的文件再也不用看
        assert_eq!(order.accept(3, hits(&candidates[3], 5)), 4);
        // 截止之外的迟到结果直接丢
        assert_eq!(order.accept(7, hits(&candidates[7], 1)), 4);
        assert_eq!(order.accept(1, hits(&candidates[1], 2)), 4);
        assert_eq!(order.emitted, 0, "0 号没到,一条都不能先吐");
        order.accept(0, Vec::new());
        assert_eq!(order.emitted, 2);
        order.accept(2, hits(&candidates[2], 1));
        assert_eq!(order.emitted, 5);
        assert!(order.truncated(10), "3 号被截掉 2 条,且 4 号之后没看");
        drop(order);
        batcher.flush();
        drop(batcher);
        let got: Vec<(String, u32)> = got
            .into_inner()
            .into_iter()
            .map(|i| (i.file_name, i.line_number.unwrap()))
            .collect();
        let want: Vec<(String, u32)> = [(1, 1), (1, 2), (2, 1), (3, 1), (3, 2)]
            .into_iter()
            .map(|(f, l)| (format!("f{f:05}.txt"), l))
            .collect();
        assert_eq!(got, want);
    }

    /// 并行下收满上限要尽早停:前几个文件就凑够了,后面上千个文件不该再被读。
    #[test]
    fn scan_ordered_stops_early_once_cap_is_reached() {
        let candidates = fake_candidates(2000);
        let calls = AtomicUsize::new(0);
        let count = std::cell::Cell::new(0usize);
        let mut batcher = ResultBatcher::new(|ev| {
            if let SearchEvent::Results(items) = ev {
                count.set(count.get() + items.len());
            }
        });
        let truncated = scan_ordered(
            &candidates,
            4,
            10,
            &SearchHandle::new(),
            &mut batcher,
            |c| {
                calls.fetch_add(1, Ordering::Relaxed);
                // 模拟读文件的耗时:活太轻时工作线程会在协调者收紧截止下标之前跑完全程
                std::thread::sleep(Duration::from_millis(1));
                hits(c, 1)
            },
        );
        batcher.flush();
        assert!(truncated);
        assert_eq!(count.get(), 10);
        let calls = calls.load(Ordering::Relaxed);
        assert!(calls < 200, "收满后应尽快收工,实际读了 {calls} 个文件");
    }

    #[test]
    fn scan_ordered_stops_on_cancel() {
        let candidates = fake_candidates(2000);
        let cancel = SearchHandle::new();
        let calls = AtomicUsize::new(0);
        let mut batcher = ResultBatcher::new(|_| {});
        scan_ordered(&candidates, 4, usize::MAX, &cancel, &mut batcher, |c| {
            if calls.fetch_add(1, Ordering::Relaxed) == 20 {
                cancel.cancel();
            }
            hits(c, 1)
        });
        let calls = calls.load(Ordering::Relaxed);
        assert!(calls < 200, "取消后应尽快收工,实际读了 {calls} 个文件");
    }

    // ── 端到端 ──

    fn make_project(tag: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mini-term-search-{tag}-{ts}"));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::write(root.join("alpha.txt"), "hello 世界\nsecond line\n").unwrap();
        std::fs::write(root.join("sub").join("beta.txt"), "nothing here\n").unwrap();
        std::fs::write(root.join("node_modules").join("alpha.txt"), "hello\n").unwrap();
        root
    }

    fn collect(root: &Path, query: &str, mode: SearchMode) -> (Vec<SearchResultItem>, u32, bool) {
        collect_with(root, query, mode, false)
    }

    fn collect_with(
        root: &Path,
        query: &str,
        mode: SearchMode,
        use_regex: bool,
    ) -> (Vec<SearchResultItem>, u32, bool) {
        let (items, total, cancelled, _) =
            collect_capped(root, query, mode, use_regex, MAX_RESULTS);
        (items, total, cancelled)
    }

    /// 返回 `(命中, total_count, cancelled, truncated)`。
    fn collect_capped(
        root: &Path,
        query: &str,
        mode: SearchMode,
        use_regex: bool,
        cap: usize,
    ) -> (Vec<SearchResultItem>, u32, bool, bool) {
        let (tx, rx) = channel();
        run_search_capped(
            SearchRequest {
                project_root: root.to_path_buf(),
                query: query.to_string(),
                mode,
                use_regex,
            },
            SearchHandle::new(),
            move |ev| {
                let _ = tx.send(ev);
            },
            cap,
        );
        let mut items = Vec::new();
        let mut total = 0;
        let mut cancelled = false;
        let mut truncated = false;
        for ev in rx {
            match ev {
                SearchEvent::Results(mut batch) => items.append(&mut batch),
                SearchEvent::Complete {
                    total_count,
                    cancelled: c,
                    truncated: t,
                } => {
                    total = total_count;
                    cancelled = c;
                    truncated = t;
                }
            }
        }
        (items, total, cancelled, truncated)
    }

    #[test]
    fn run_search_filenames_skips_always_ignore_dirs() {
        let root = make_project("names");
        let (items, total, cancelled) = collect(&root, "alpha", SearchMode::FileName);
        assert!(!cancelled);
        assert_eq!(total, 1, "node_modules 下的同名文件不应被搜到");
        assert_eq!(items[0].file_name, "alpha.txt");
        assert_eq!(items[0].file_path, PathBuf::from("alpha.txt"));
        assert!(!items[0].match_in_path);
        std::fs::remove_dir_all(&root).ok();
    }

    /// issue #57 的现场:`src/pages/task/my/my.vue`,外加同目录另一个文件与
    /// 浅层同名文件,用来区分「按路径」与「按文件名」两种口径。
    fn make_nested_project(tag: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mini-term-search-{tag}-{ts}"));
        let deep = root.join("src").join("pages").join("task").join("my");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("my.vue"), "<template/>\n").unwrap();
        std::fs::write(deep.parent().unwrap().join("other.vue"), "<template/>\n").unwrap();
        std::fs::write(root.join("src").join("my.vue"), "<template/>\n").unwrap();
        root
    }

    #[test]
    fn filename_search_matches_path_when_query_has_separator() {
        let root = make_nested_project("path");
        let (items, total, _) = collect(&root, "pages/task/my/my", SearchMode::FileName);
        assert_eq!(total, 1);
        assert!(items[0].match_in_path);
        assert_eq!(items[0].file_name, "my.vue");
        assert_eq!(
            items[0].file_path,
            PathBuf::from("src/pages/task/my/my.vue")
        );
        // 区间落在路径上:"src/" 占 4 个 char,命中的是其后 16 个 char
        assert_eq!(items[0].match_ranges, vec![(4, 20)]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn filename_search_without_separator_stays_on_file_name() {
        let root = make_nested_project("name-only");
        let (items, total, _) = collect(&root, "my", SearchMode::FileName);
        // 两个 my.vue 命中;目录名 my 不算——other.vue 不该因为躺在 my/ 旁边被冲出来
        assert_eq!(total, 2);
        assert!(items.iter().all(|i| !i.match_in_path));
        assert!(items.iter().all(|i| i.file_name == "my.vue"));
        assert!(items.iter().all(|i| i.match_ranges == vec![(0, 2)]));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn filename_search_normalizes_backslash_and_leading_prefix() {
        let root = make_nested_project("normalize");
        // Windows 习惯的反斜杠
        let (items, total, _) = collect(&root, "pages\\task\\my\\", SearchMode::FileName);
        assert_eq!(total, 1);
        assert!(items[0].match_in_path);
        // 开头的 ./ 与 / 都去掉:相对路径没有这种前缀
        let (_, total, _) = collect(&root, "./src/my", SearchMode::FileName);
        assert_eq!(total, 1);
        let (_, total, _) = collect(&root, "/src/pages", SearchMode::FileName);
        assert_eq!(total, 2);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn filename_search_regex_with_separator_matches_path() {
        let root = make_nested_project("regex-path");
        let (items, total, _) =
            collect_with(&root, r"task/.*\.vue$", SearchMode::FileName, true);
        assert_eq!(total, 2);
        assert!(items.iter().all(|i| i.match_in_path));
        // 不带 / 的正则仍只看文件名:\ 是转义符,不算路径分隔符
        let (items, total, _) = collect_with(&root, r"^my\.vue$", SearchMode::FileName, true);
        assert_eq!(total, 2);
        assert!(items.iter().all(|i| !i.match_in_path));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wants_path_match_rules() {
        assert!(wants_path_match("pages/task", false));
        assert!(wants_path_match("pages/task", true));
        assert!(wants_path_match("pages\\task", false));
        assert!(!wants_path_match(r"my\.vue", true));
        assert!(!wants_path_match("my", false));
    }

    #[test]
    fn run_search_contents_reports_line_and_ranges() {
        let root = make_project("contents");
        let (items, total, _) = collect(&root, "世界", SearchMode::FileContent);
        assert_eq!(total, 1);
        assert_eq!(items[0].line_number, Some(1));
        assert_eq!(items[0].line_content.as_deref(), Some("hello 世界"));
        // char 区间:"hello " 占 6 个 char,"世界" 是第 6..8 个 char
        assert_eq!(items[0].match_ranges, vec![(6, 8)]);
        std::fs::remove_dir_all(&root).ok();
    }

    fn temp_project(tag: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mini-term-search-{tag}-{ts}"));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn content_search_skips_files_over_size_limit() {
        let root = temp_project("big");
        // 超限 1 字节的大文件:内容里有关键词也不读;文件名搜索照样能找到它
        let mut big = b"needle\n".to_vec();
        big.resize(MAX_CONTENT_FILE_BYTES as usize + 1, b'x');
        std::fs::write(root.join("big.log"), &big).unwrap();
        // 恰好卡在上限上的仍然搜
        let mut edge = b"needle\n".to_vec();
        edge.resize(MAX_CONTENT_FILE_BYTES as usize, b'x');
        std::fs::write(root.join("edge.log"), &edge).unwrap();
        std::fs::write(root.join("small.txt"), "a needle here\n").unwrap();

        let (items, total, _) = collect(&root, "needle", SearchMode::FileContent);
        assert_eq!(total, 2);
        let names: Vec<&str> = items.iter().map(|i| i.file_name.as_str()).collect();
        assert_eq!(names, ["edge.log", "small.txt"], "超限的 big.log 应被跳过");

        let (items, _, _) = collect(&root, "big", SearchMode::FileName);
        assert_eq!(items.len(), 1, "文件名搜索不受大小上限影响");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn content_search_sniffs_binary_from_header_only() {
        let root = temp_project("binary");
        // 文件头里有 NUL:二进制,跳过
        std::fs::write(root.join("bin.dat"), b"needle\x00\x01\x02 needle\n").unwrap();
        // NUL 在文件头 8KB 之后:按文本处理(与旧判据同口径)
        let mut late = b"needle first\n".to_vec();
        late.resize(BINARY_SNIFF_BYTES + 100, b'a');
        late.extend_from_slice(b"\x00\nneedle last\n");
        std::fs::write(root.join("late.txt"), &late).unwrap();

        assert!(read_text_file(&root.join("bin.dat")).is_none());
        assert!(read_text_file(&root.join("late.txt")).is_some());
        let (items, _, _) = collect(&root, "needle", SearchMode::FileContent);
        let got: Vec<(&str, Option<u32>)> = items
            .iter()
            .map(|i| (i.file_name.as_str(), i.line_number))
            .collect();
        assert_eq!(got, [("late.txt", Some(1)), ("late.txt", Some(3))]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn content_search_clips_long_line_with_correct_highlight() {
        let root = temp_project("longline");
        // 压缩 JS 式的 5000 字符单行,中文关键词埋在中间
        let mut line = "var a=1;".repeat(625);
        line.insert_str(3000, "查找目标");
        std::fs::write(root.join("app.min.js"), format!("{line}\n")).unwrap();

        let (items, total, _) = collect(&root, "查找目标", SearchMode::FileContent);
        assert_eq!(total, 1);
        let content = items[0].line_content.as_deref().unwrap();
        assert_eq!(content.chars().count(), MAX_LINE_CHARS + 2);
        assert_eq!(
            highlighted_slices(content, &items[0].match_ranges),
            ["查找目标"]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 同一目录树:并行遍历每次到达顺序不同,结果必须每次一样,且按「深度优先 +
    /// 同级按名字不分大小写」排好(与旧的顺序遍历在 NTFS 上的顺序一致)。
    fn make_ordered_project(tag: &str) -> PathBuf {
        let root = temp_project(tag);
        for dir in ["a", "B", "c/d", "c/E", "zz"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let files = [
            "a.txt",
            "a/x.txt",
            "a/Y.txt",
            "B/one.txt",
            "b.txt",
            "c/d/deep.txt",
            "c/E/e.txt",
            "c/f.txt",
            "zz/last.txt",
            "_under.txt",
            "Z.txt",
        ];
        for f in files {
            std::fs::write(root.join(f), "hit 1\nno\nhit 2\n").unwrap();
        }
        for i in 0..40 {
            std::fs::write(root.join("zz").join(format!("n{i:02}.txt")), "hit\n").unwrap();
        }
        root
    }

    #[test]
    fn parallel_results_are_sorted_and_stable() {
        let root = make_ordered_project("order");
        let run = || -> Vec<(String, Option<u32>)> {
            collect(&root, "hit", SearchMode::FileContent)
                .0
                .into_iter()
                .map(|i| {
                    (
                        i.file_path.to_string_lossy().replace('\\', "/"),
                        i.line_number,
                    )
                })
                .collect()
        };
        let first = run();
        for _ in 0..5 {
            assert_eq!(run(), first, "同一次输入每次结果必须一样");
        }
        let head: Vec<&str> = first
            .iter()
            .step_by(2)
            .take(11)
            .map(|(p, _)| p.as_str())
            .collect();
        assert_eq!(
            head,
            [
                "a/x.txt",
                "a/Y.txt",
                "a.txt",
                "B/one.txt",
                "b.txt",
                "c/d/deep.txt",
                "c/E/e.txt",
                "c/f.txt",
                "Z.txt",
                "zz/last.txt",
                "zz/n00.txt",
            ],
            "深度优先 + 同级按名字转大写比较;`_` 在字母之后"
        );
        assert_eq!(first[0].1, Some(1));
        assert_eq!(
            first[1],
            ("a/x.txt".to_string(), Some(3)),
            "同一文件内按行号"
        );
        assert_eq!(first.last().unwrap().0, "_under.txt");
        assert_eq!(first.len(), 11 * 2 + 40);

        // 文件名模式同一顺序
        let names: Vec<String> = collect(&root, ".txt", SearchMode::FileName)
            .0
            .into_iter()
            .map(|i| i.file_path.to_string_lossy().replace('\\', "/"))
            .collect();
        let mut dedup = first.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>();
        dedup.dedup();
        assert_eq!(names, dedup);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn results_capped_at_limit_keep_sorted_prefix() {
        let root = make_ordered_project("cap");
        let (all, total, _, truncated) =
            collect_capped(&root, "hit", SearchMode::FileContent, false, 1000);
        assert_eq!(total as usize, all.len());
        assert!(!truncated, "没到上限不算截断");

        for cap in [1, 7, 30] {
            let (items, total, _, truncated) =
                collect_capped(&root, "hit", SearchMode::FileContent, false, cap);
            assert!(truncated);
            assert_eq!(total as usize, cap);
            let got: Vec<_> = items
                .iter()
                .map(|i| (i.file_path.clone(), i.line_number))
                .collect();
            let want: Vec<_> = all[..cap]
                .iter()
                .map(|i| (i.file_path.clone(), i.line_number))
                .collect();
            assert_eq!(got, want, "上限 {cap}:必须恰是完整结果的前 {cap} 条");
        }
        // 恰好等于命中数:全收下,不算截断
        let (_, _, _, truncated) =
            collect_capped(&root, "hit", SearchMode::FileContent, false, all.len());
        assert!(!truncated);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn single_file_hits_capped_at_max_results() {
        let root = temp_project("many");
        let body = "match\n".repeat(MAX_RESULTS + 500);
        std::fs::write(root.join("many.txt"), body).unwrap();
        let (tx, rx) = channel();
        run_search(
            SearchRequest {
                project_root: root.clone(),
                query: "match".to_string(),
                mode: SearchMode::FileContent,
                use_regex: false,
            },
            SearchHandle::new(),
            move |ev| {
                let _ = tx.send(ev);
            },
        );
        let events: Vec<_> = rx.into_iter().collect();
        let shown: usize = events
            .iter()
            .map(|ev| match ev {
                SearchEvent::Results(items) => items.len(),
                SearchEvent::Complete { .. } => 0,
            })
            .sum();
        assert_eq!(shown, MAX_RESULTS);
        assert!(matches!(
            events.last(),
            Some(SearchEvent::Complete {
                total_count,
                cancelled: false,
                truncated: true,
            }) if *total_count as usize == MAX_RESULTS
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn cancelled_before_start_completes_immediately() {
        let root = make_project("cancel");
        let (tx, rx) = channel();
        let handle = SearchHandle::new();
        handle.cancel(); // 开跑前就取消
        run_search(
            SearchRequest {
                project_root: root.clone(),
                query: "alpha".to_string(),
                mode: SearchMode::FileName,
                use_regex: false,
            },
            handle,
            move |ev| {
                let _ = tx.send(ev);
            },
        );
        let events: Vec<_> = rx.into_iter().collect();
        assert_eq!(events.len(), 1, "只该有一条 Complete");
        assert!(matches!(
            events[0],
            SearchEvent::Complete {
                total_count: 0,
                cancelled: true,
                truncated: false,
            }
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn start_search_rejects_empty_query_and_bad_regex() {
        let req = |q: &str, re: bool| SearchRequest {
            project_root: std::env::temp_dir(),
            query: q.to_string(),
            mode: SearchMode::FileName,
            use_regex: re,
        };
        assert!(start_search(req("", false), |_| {}).is_err());
        let err = start_search(req("(unclosed", true), |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("Invalid regex"), "实际错误: {err}");
    }
}
