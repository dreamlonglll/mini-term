use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

// ── Data structures ──

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SearchMode {
    FileName,
    FileContent,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResultItem {
    pub file_path: String,
    pub file_name: String,
    pub line_number: Option<u32>,
    pub line_content: Option<String>,
    pub match_ranges: Vec<(usize, usize)>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchResultsPayload {
    search_id: String,
    items: Vec<SearchResultItem>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchCompletePayload {
    search_id: String,
    total_count: u32,
    cancelled: bool,
}

// ── SearchManager ──

#[derive(Clone)]
pub struct SearchManager {
    // search_id → (project_root, cancel_flag)
    active_searches: Arc<Mutex<HashMap<String, (String, Arc<AtomicBool>)>>>,
}

impl SearchManager {
    pub fn new() -> Self {
        Self {
            active_searches: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn register(&self, search_id: &str, project_root: &str) -> Arc<AtomicBool> {
        let mut searches = self.active_searches.lock().unwrap();
        // Cancel all existing searches for the same project
        let to_cancel: Vec<String> = searches
            .iter()
            .filter(|(_, (root, _))| root == project_root)
            .map(|(id, _)| id.clone())
            .collect();
        for id in to_cancel {
            if let Some((_, flag)) = searches.remove(&id) {
                flag.store(true, Ordering::Relaxed);
            }
        }
        let flag = Arc::new(AtomicBool::new(false));
        searches.insert(
            search_id.to_string(),
            (project_root.to_string(), flag.clone()),
        );
        flag
    }

    pub fn cancel(&self, search_id: &str) {
        let mut searches = self.active_searches.lock().unwrap();
        if let Some((_, flag)) = searches.remove(search_id) {
            flag.store(true, Ordering::Relaxed);
        }
    }

    pub fn remove(&self, search_id: &str) {
        self.active_searches.lock().unwrap().remove(search_id);
    }
}

// ── Helpers ──

fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8192).any(|&b| b == 0)
}

fn build_walker(root: &str) -> ignore::Walk {
    let mut builder = ignore::WalkBuilder::new(root);
    builder.hidden(false);
    builder.filter_entry(|entry| {
        if entry.file_type().map_or(false, |ft| ft.is_dir()) {
            let name = entry.file_name().to_str().unwrap_or("");
            !crate::fs::ALWAYS_IGNORE.contains(&name)
        } else {
            true
        }
    });
    builder.build()
}

/// 大小写不敏感子串搜索，直接返回【原始 text】的 char 区间（前端按 char 高亮）。
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

/// Convert byte-offset ranges to char-index ranges so the frontend HighlightText
/// (JS String.slice) works correctly on non-ASCII text (CJK, emoji, etc.).
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

struct ResultBatcher {
    buffer: Vec<SearchResultItem>,
    last_flush: Instant,
    app: AppHandle,
    search_id: String,
    total_count: u32,
}

impl ResultBatcher {
    fn new(app: AppHandle, search_id: String) -> Self {
        Self {
            buffer: Vec::new(),
            last_flush: Instant::now(),
            app,
            search_id,
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
        let _ = self.app.emit(
            "search-results",
            SearchResultsPayload {
                search_id: self.search_id.clone(),
                items,
            },
        );
        self.last_flush = Instant::now();
    }

    fn finish(mut self, cancelled: bool) {
        self.flush();
        let _ = self.app.emit(
            "search-complete",
            SearchCompletePayload {
                search_id: self.search_id.clone(),
                total_count: self.total_count,
                cancelled,
            },
        );
    }
}

// ── Search functions ──

fn search_filenames(
    root: &str,
    query: &str,
    use_regex: bool,
    cancel: &AtomicBool,
    batcher: &mut ResultBatcher,
) -> Result<(), String> {
    let re = if use_regex {
        Some(Regex::new(query).map_err(|e| format!("Invalid regex: {}", e))?)
    } else {
        None
    };
    let query_lower = query.to_lowercase();

    for entry in build_walker(root) {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().map_or(true, |ft| ft.is_dir()) {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().to_string();

        let char_ranges = if let Some(ref re) = re {
            byte_ranges_to_char_ranges(&file_name, find_regex_matches(&file_name, re))
        } else {
            find_substring_char_ranges(&file_name, &query_lower)
        };

        if !char_ranges.is_empty() {
            let rel_path = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .to_string();
            batcher.push(SearchResultItem {
                file_path: rel_path,
                file_name,
                line_number: None,
                line_content: None,
                match_ranges: char_ranges,
            });
        }
    }
    Ok(())
}

fn search_contents(
    root: &str,
    query: &str,
    use_regex: bool,
    cancel: &AtomicBool,
    batcher: &mut ResultBatcher,
) -> Result<(), String> {
    let re = if use_regex {
        Some(Regex::new(query).map_err(|e| format!("Invalid regex: {}", e))?)
    } else {
        None
    };
    let query_lower = query.to_lowercase();

    for entry in build_walker(root) {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().map_or(true, |ft| ft.is_dir()) {
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
        let rel_path = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        for (line_idx, line) in text.lines().enumerate() {
            if cancel.load(Ordering::Relaxed) {
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
                });
            }
        }
    }
    Ok(())
}

// ── Tauri commands ──

#[tauri::command]
pub fn start_search(
    app: AppHandle,
    state: tauri::State<'_, SearchManager>,
    project_root: String,
    query: String,
    mode: String,
    use_regex: bool,
    search_id: String,
) -> Result<(), String> {
    if query.is_empty() {
        return Err("Search query is empty".to_string());
    }
    if use_regex {
        Regex::new(&query).map_err(|e| format!("Invalid regex: {}", e))?;
    }

    let manager = state.inner().clone();
    let cancel = manager.register(&search_id, &project_root);
    let search_mode = match mode.as_str() {
        "content" => SearchMode::FileContent,
        _ => SearchMode::FileName,
    };

    let sid = search_id.clone();
    std::thread::spawn(move || {
        let mut batcher = ResultBatcher::new(app, sid.clone());
        // 用 catch_unwind 兜底：即便搜索体内将来再出现 panic，也不会跳过下面的
        // finish()/remove()，否则前端永远收不到 search-complete、搜索框卡死在 loading，
        // 且 active_searches 残留。AssertUnwindSafe 是因为 batcher/AppHandle 跨越捕获边界。
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match search_mode {
                SearchMode::FileName => {
                    search_filenames(&project_root, &query, use_regex, &cancel, &mut batcher)
                }
                SearchMode::FileContent => {
                    search_contents(&project_root, &query, use_regex, &cancel, &mut batcher)
                }
            }
        }));
        if outcome.is_err() {
            eprintln!("[search] worker panicked during search {}", sid);
        }
        let cancelled = cancel.load(Ordering::Relaxed);
        batcher.finish(cancelled);
        manager.remove(&sid);
    });

    Ok(())
}

#[tauri::command]
pub fn cancel_search(state: tauri::State<'_, SearchManager>, search_id: String) {
    state.cancel(&search_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_manager_register_and_cancel() {
        let mgr = SearchManager::new();
        let flag = mgr.register("s1", "/project");
        assert!(!flag.load(Ordering::Relaxed));
        mgr.cancel("s1");
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn search_manager_auto_cancels_same_project() {
        let mgr = SearchManager::new();
        let flag1 = mgr.register("s1", "/project");
        let _flag2 = mgr.register("s2", "/project");
        assert!(flag1.load(Ordering::Relaxed));
    }

    #[test]
    fn search_manager_different_projects_independent() {
        let mgr = SearchManager::new();
        let flag1 = mgr.register("s1", "/project-a");
        let _flag2 = mgr.register("s2", "/project-b");
        assert!(!flag1.load(Ordering::Relaxed));
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
}
