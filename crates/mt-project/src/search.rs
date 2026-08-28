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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use parking_lot::Mutex;
use regex::Regex;
use serde::Serialize;

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
    /// 一批命中(50 条或 100ms 攒一批)。
    Results(Vec<SearchResultItem>),
    /// 搜索结束。`cancelled=true` 表示是被取消的,结果不完整。
    Complete { total_count: u32, cancelled: bool },
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
    data.iter().take(8192).any(|&b| b == 0)
}

fn build_walker(root: &Path) -> ignore::Walk {
    let mut builder = ignore::WalkBuilder::new(root);
    builder.hidden(false);
    builder.filter_entry(|entry| {
        if entry.file_type().is_some_and(|ft| ft.is_dir()) {
            let name = entry.file_name().to_str().unwrap_or("");
            !crate::fs::ALWAYS_IGNORE.contains(&name)
        } else {
            true
        }
    });
    builder.build()
}

/// 大小写不敏感子串搜索，直接返回【原始 text】的 char 区间（上层按 char 高亮）。
///
/// 逐字符做小写折叠，同时记录每个小写字符来自哪个原始字符；匹配在小写字符序列
/// 上按 char 进行，命中后回映射到原始 char 下标。这样即便 Unicode 大小写折叠改变
/// 长度（İ→i̇、ǅ→ǆ 等），结果也始终落在原始字符边界上——从根本上避免了旧实现里
/// 「按字节 +1 步进切多字节字符 panic」以及「在 to_lowercase() 串上算偏移却拿原串
/// 做 byte→char 映射导致越界 / 错位」两个问题。query_lower 由调用方预先小写化。
fn find_substring_char_ranges(text: &str, query_lower: &str) -> Vec<(usize, usize)> {
    let query_chars: Vec<char> = query_lower.chars().collect();
    if query_chars.is_empty() {
        return Vec::new();
    }
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

fn find_regex_matches(text: &str, re: &Regex) -> Vec<(usize, usize)> {
    re.find_iter(text).map(|m| (m.start(), m.end())).collect()
}

/// 把字节区间换算成 char 区间,非 ASCII 文本(CJK、emoji)才不会切错位置。
fn byte_ranges_to_char_ranges(text: &str, byte_ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    if byte_ranges.is_empty() {
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

    fn finish(mut self, cancelled: bool) {
        self.flush();
        (self.sink)(SearchEvent::Complete {
            total_count: self.total_count,
            cancelled,
        });
    }
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

fn search_filenames<F: Fn(SearchEvent)>(
    root: &Path,
    query: &str,
    use_regex: bool,
    cancel: &SearchHandle,
    batcher: &mut ResultBatcher<F>,
) -> Result<()> {
    let re = if use_regex {
        Some(compile_regex(query)?)
    } else {
        None
    };
    let match_in_path = wants_path_match(query, use_regex);
    let query_lower = if match_in_path {
        normalize_path_query(query)
    } else {
        query.to_lowercase()
    };

    for entry in build_walker(root) {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().is_none_or(|ft| ft.is_dir()) {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().to_string();
        let rel_path = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_path_buf();

        let haystack = if match_in_path {
            path_for_match(&rel_path)
        } else {
            file_name.clone()
        };
        let char_ranges = if let Some(ref re) = re {
            byte_ranges_to_char_ranges(&haystack, find_regex_matches(&haystack, re))
        } else {
            find_substring_char_ranges(&haystack, &query_lower)
        };

        if !char_ranges.is_empty() {
            batcher.push(SearchResultItem {
                file_path: rel_path,
                file_name,
                line_number: None,
                line_content: None,
                match_ranges: char_ranges,
                match_in_path,
            });
        }
    }
    Ok(())
}

fn search_contents<F: Fn(SearchEvent)>(
    root: &Path,
    query: &str,
    use_regex: bool,
    cancel: &SearchHandle,
    batcher: &mut ResultBatcher<F>,
) -> Result<()> {
    let re = if use_regex {
        Some(compile_regex(query)?)
    } else {
        None
    };
    let query_lower = query.to_lowercase();

    for entry in build_walker(root) {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().is_none_or(|ft| ft.is_dir()) {
            continue;
        }

        let path = entry.path();
        let content = match std::fs::read(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if is_binary(&content) {
            continue;
        }
        let text = match String::from_utf8(content) {
            Ok(t) => t,
            Err(_) => continue,
        };

        let file_name = entry.file_name().to_string_lossy().to_string();
        let rel_path = path.strip_prefix(root).unwrap_or(path).to_path_buf();

        for (line_idx, line) in text.lines().enumerate() {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let char_ranges = if let Some(ref re) = re {
                byte_ranges_to_char_ranges(line, find_regex_matches(line, re))
            } else {
                find_substring_char_ranges(line, &query_lower)
            };
            if !char_ranges.is_empty() {
                batcher.push(SearchResultItem {
                    file_path: rel_path.clone(),
                    file_name: file_name.clone(),
                    line_number: Some((line_idx + 1) as u32),
                    line_content: Some(line.to_string()),
                    match_ranges: char_ranges,
                    match_in_path: false,
                });
            }
        }
    }
    Ok(())
}

fn compile_regex(query: &str) -> Result<Regex> {
    Regex::new(query).map_err(|e| anyhow::anyhow!("Invalid regex: {}", e))
}

// ── 入口 ──

/// 在**当前线程**上跑完一次搜索。想放到自己的执行器上(GPUI 的
/// `background_executor`)就用它;想要「起一个后台线程就不管了」用 [`start_search`]。
///
/// sink 会先收到若干 [`SearchEvent::Results`],最后必定收到一条
/// [`SearchEvent::Complete`] —— 即便中途 panic 也不例外。
pub fn run_search<F>(req: SearchRequest, cancel: SearchHandle, sink: F)
where
    F: Fn(SearchEvent),
{
    let mut batcher = ResultBatcher::new(sink);
    // 用 catch_unwind 兜底:即便搜索体内将来再出现 panic,也不会跳过下面的 finish(),
    // 否则上层永远收不到 Complete、搜索框卡死在 loading。
    // AssertUnwindSafe 是因为 batcher 跨越捕获边界。
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match req.mode {
        SearchMode::FileName => search_filenames(
            &req.project_root,
            &req.query,
            req.use_regex,
            &cancel,
            &mut batcher,
        ),
        SearchMode::FileContent => search_contents(
            &req.project_root,
            &req.query,
            req.use_regex,
            &cancel,
            &mut batcher,
        ),
    }));
    if outcome.is_err() {
        eprintln!("[search] worker panicked while searching {:?}", req.query);
    }
    batcher.finish(cancel.is_cancelled());
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
        let (tx, rx) = channel();
        run_search(
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
        );
        let mut items = Vec::new();
        let mut total = 0;
        let mut cancelled = false;
        for ev in rx {
            match ev {
                SearchEvent::Results(mut batch) => items.append(&mut batch),
                SearchEvent::Complete {
                    total_count,
                    cancelled: c,
                } => {
                    total = total_count;
                    cancelled = c;
                }
            }
        }
        (items, total, cancelled)
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
                cancelled: true
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
