//! 快速打开的文件索引:一次遍历收齐项目里的文件,紧凑存放,按查询打分取前 N 条。
//!
//! # 结构
//!
//! 参照 Winc(Apache-2.0)`crates/winc-editor/src/workspace/picker.rs` 的 `Files` / `Packed`:
//! 所有相对路径首尾相接存进**一个** `String`,另记每条的字节范围与文件名偏移 —— 二十万个文件
//! 只占三块内存,而不是四十万个各自分配的小 `String`,打分时顺序读内存也更快。排序口径
//! ([`path_order`])、相对路径的拼法([`relative_path`])与「同分按路径长短、再按字典序」的
//! 取前 N 条(`select_nth_unstable_by` + 只排前 N 条)同样移植自那里与
//! `crates/winc-editor/src/workspace/picker/scan.rs`。
//!
//! **没搬**的是 Winc 的增量更新(`patch` / `merge`,靠一路覆盖整个工作区的常驻目录监听)与按层
//! 收录:本仓没有覆盖整个项目的常驻监听(文件树只盯展开着的目录,Git 面板的工作区监听只在
//! git 仓库里、跟面板走),索引失效改由调用方按「过期 / 被标脏就整份重建」处理,见
//! `mt-app` 的 `quick_open` 模块注释。
//!
//! # 遍历与忽略规则
//!
//! 与全局搜索**同一把尺子**:直接用 [`crate::search`] 的 walker(`ignore` 的并行遍历,认
//! `.gitignore` / `.ignore` / 全局 excludes,不藏点文件,外加文件树那份黑名单目录与 worktree
//! 根下的 `.git` 指针文件)。全局搜索的「文件名」模式能搜到的文件,这里都列得出;反过来也一样。
//!
//! # 上限
//!
//! 文件数到 [`MAX_FILES`] 或遍历超过 [`BUILD_TIMEOUT`] 就收工,索引标成不完整
//! ([`Incomplete`]),界面据此显示「结果不完整」。并行遍历的到达顺序每次不同,所以截断时
//! **收进来的是哪一部分不固定**;没截断时结果与遍历顺序无关(收齐后按 [`path_order`] 排序)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::fuzzy::{self, PathRef, Scratch};

/// 索引收录的最大文件数(个,与 Winc 同值)。文件再多用户也无法逐个浏览,而且每次按键都要
/// 扫一遍整张表。
pub const MAX_FILES: usize = 200_000;

/// 建一次索引最多花多久。WSL(`\\wsl$`)与网络盘上每次列目录都是往返,大仓库可能慢到几十秒;
/// 到点就拿已经收到的那部分先用,索引标成不完整。
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(10);

/// 打分时每扫这么多条看一眼取消标志。
const CANCEL_CHECK_EVERY: usize = 4096;

/// 建索引的限额。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildLimits {
    pub max_files: usize,
    pub timeout: Duration,
}

impl Default for BuildLimits {
    fn default() -> Self {
        Self {
            max_files: MAX_FILES,
            timeout: BUILD_TIMEOUT,
        }
    }
}

/// 索引不完整的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Incomplete {
    /// 文件数到了上限,后面的没收。
    TooManyFiles,
    /// 遍历超时,只收了已经走到的那部分。
    TimedOut,
}

/// 一个项目的文件表。路径相对项目根、以 `/` 分隔,按 [`path_order`] 排好序。
#[derive(Debug)]
pub struct FileIndex {
    root: PathBuf,
    /// 所有相对路径首尾相接。
    text: String,
    /// 每条路径在 `text` 中的(起始,结束)字节偏移。
    spans: Vec<(u32, u32)>,
    /// 与 `spans` 一一对应:文件名在该路径内的起始字节偏移。
    names: Vec<u32>,
    incomplete: Option<Incomplete>,
}

/// 一次查询的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOutcome {
    /// 排好序的文件表下标,最多 `limit` 条。
    pub hits: Vec<usize>,
    /// 截断到 `limit` 之前一共命中了几个文件(空查询时是文件总数)。
    pub matched: usize,
}

impl FileIndex {
    /// 遍历 `root` 建索引。**阻塞**(要把整棵目录树走一遍),调用方放后台线程。
    /// `cancel` 置位时尽快返回 `None`。
    pub fn build(root: &Path, limits: BuildLimits, cancel: &AtomicBool) -> Option<FileIndex> {
        let deadline = Instant::now() + limits.timeout;
        let found: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let count = AtomicUsize::new(0);
        let too_many = AtomicBool::new(false);
        let timed_out = AtomicBool::new(false);
        crate::search::build_walker(root, None).run(|| {
            let (found, count, too_many, timed_out) = (&found, &count, &too_many, &timed_out);
            Box::new(move |entry| {
                if cancel.load(Ordering::Relaxed) {
                    return ignore::WalkState::Quit;
                }
                if Instant::now() >= deadline {
                    timed_out.store(true, Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }
                // 读不了的条目(权限等)跳过,与全局搜索同口径
                let Ok(entry) = entry else {
                    return ignore::WalkState::Continue;
                };
                if entry.file_type().is_none_or(|ft| ft.is_dir()) {
                    return ignore::WalkState::Continue;
                }
                // 含非 UTF-8 段的路径跳过:界面画不出、拼回去也不一定是原路径
                let Some(rel) = entry.path().strip_prefix(root).ok().and_then(relative_path) else {
                    return ignore::WalkState::Continue;
                };
                if count.fetch_add(1, Ordering::Relaxed) >= limits.max_files {
                    too_many.store(true, Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }
                found.lock().push(rel);
                ignore::WalkState::Continue
            })
        });
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let incomplete = if too_many.load(Ordering::Relaxed) {
            Some(Incomplete::TooManyFiles)
        } else if timed_out.load(Ordering::Relaxed) {
            Some(Incomplete::TimedOut)
        } else {
            None
        };
        let mut paths = found.into_inner();
        paths.sort_unstable_by(|a, b| path_order(a, b));
        Some(Self::pack(root, &paths, incomplete))
    }

    /// 用现成的相对路径表(`/` 分隔)建索引,不碰盘。会按 [`path_order`] 排序并去重。
    pub fn from_relative_paths(root: &Path, paths: &[&str]) -> FileIndex {
        let mut paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        paths.sort_unstable_by(|a, b| path_order(a, b));
        paths.dedup();
        Self::pack(root, &paths, None)
    }

    fn pack(root: &Path, paths: &[String], incomplete: Option<Incomplete>) -> FileIndex {
        let bytes = paths.iter().map(String::len).sum();
        let mut text = String::with_capacity(bytes);
        let mut spans = Vec::with_capacity(paths.len());
        let mut names = Vec::with_capacity(paths.len());
        for path in paths {
            let start = text.len() as u32;
            text.push_str(path);
            spans.push((start, text.len() as u32));
            names.push(path.rfind('/').map_or(0, |i| i + 1) as u32);
        }
        FileIndex {
            root: root.to_path_buf(),
            text,
            spans,
            names,
            incomplete,
        }
    }

    /// 建索引时的项目根。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 文件数。
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// 索引不完整的原因;完整时为 `None`。
    pub fn incomplete(&self) -> Option<Incomplete> {
        self.incomplete
    }

    /// 第 `index` 个文件的相对路径(`/` 分隔)。
    pub fn path(&self, index: usize) -> Option<&str> {
        let &(start, end) = self.spans.get(index)?;
        Some(&self.text[start as usize..end as usize])
    }

    /// 第 `index` 个文件(相对路径 + 文件名段)。
    pub fn file(&self, index: usize) -> Option<PathRef<'_>> {
        let path = self.path(index)?;
        let name = self.names[index] as usize;
        Some(PathRef {
            path,
            name: &path[name..],
        })
    }

    /// 第 `index` 个文件的绝对路径:项目根拼上相对路径,分隔符换回本平台的。
    pub fn absolute(&self, index: usize) -> Option<PathBuf> {
        let path = self.path(index)?;
        Some(
            self.root
                .join(path.replace('/', std::path::MAIN_SEPARATOR_STR)),
        )
    }

    /// 相对路径 `relative`(`/` 分隔)在表中的下标。Windows 上大小写不同也认(同一目录里
    /// 不会并存只差大小写的两个文件)。
    pub fn position(&self, relative: &str) -> Option<usize> {
        let exact = self.spans.binary_search_by(|&(start, end)| {
            path_order(&self.text[start as usize..end as usize], relative)
        });
        if let Ok(index) = exact {
            return Some(index);
        }
        if cfg!(windows) {
            // 表按 `path_order` 排序,它的第一键就是小写比较,所以按小写比较二分同样成立
            return self
                .spans
                .binary_search_by(|&(start, end)| {
                    lower_order(&self.text[start as usize..end as usize], relative)
                })
                .ok();
        }
        None
    }

    /// 绝对路径 `path` 在表中的下标;不在项目根下或不在表中时为 `None`。
    pub fn position_of(&self, path: &Path) -> Option<usize> {
        self.position(&self.relative_of(path)?)
    }

    /// 绝对路径相对项目根的写法(`/` 分隔)。Windows 上项目根按 ASCII 大小写不敏感比较 ——
    /// 终端链接等入口给的路径大小写不一定与项目根的写法一致。
    fn relative_of(&self, path: &Path) -> Option<String> {
        if let Ok(rel) = path.strip_prefix(&self.root) {
            return relative_path(rel);
        }
        if !cfg!(windows) {
            return None;
        }
        let root = self.root.to_string_lossy().replace('\\', "/");
        let root = root.trim_end_matches('/');
        let path = path.to_string_lossy().replace('\\', "/");
        let head = path.get(..root.len())?;
        let rest = path[root.len()..].strip_prefix('/')?;
        (head.eq_ignore_ascii_case(root) && !rest.is_empty()).then(|| rest.to_string())
    }

    /// 按查询打分排序,返回最多 `limit` 条。`needle` 须经 [`fuzzy::normalize_query`] 归一化。
    ///
    /// `recent` 是最近打开的文件(表下标,新的在前):
    ///
    /// - 空查询:`recent` 依次排在最前,后面按表的顺序接其余文件 —— 刚打开面板、还没输入时
    ///   第一条就是上一个文件;
    /// - 有查询:分数高的在前;**同分**时最近打开过的在前(越近越前),再按路径短的在前、
    ///   路径字典序,使结果稳定。
    ///
    /// `cancel` 置位时中途放弃,返回 `None`(查询已经过期,结果没人要了)。
    pub fn search(
        &self,
        needle: &[char],
        recent: &[usize],
        limit: usize,
        cancel: Option<&AtomicBool>,
    ) -> Option<SearchOutcome> {
        // 同一个文件在 `recent` 里出现多次时只认最靠前那次;越界的下标丢掉
        let mut rank: HashMap<usize, usize> = HashMap::with_capacity(recent.len());
        let mut ordered_recent = Vec::with_capacity(recent.len());
        for &index in recent {
            if index < self.len() && !rank.contains_key(&index) {
                rank.insert(index, ordered_recent.len());
                ordered_recent.push(index);
            }
        }

        if needle.is_empty() {
            let mut hits: Vec<usize> = ordered_recent.iter().copied().take(limit).collect();
            if hits.len() < limit {
                let rest = (0..self.len()).filter(|i| !rank.contains_key(i));
                hits.extend(rest.take(limit - hits.len()));
            }
            return Some(SearchOutcome {
                hits,
                matched: self.len(),
            });
        }

        let mut scratch = Scratch::default();
        let mut scored: Vec<(u32, usize)> = Vec::new();
        for index in 0..self.len() {
            if index % CANCEL_CHECK_EVERY == 0
                && cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed))
            {
                return None;
            }
            if let Some(file) = self.file(index)
                && let Some(points) = fuzzy::score(needle, file, &mut scratch)
            {
                scored.push((points, index));
            }
        }
        let matched = scored.len();

        let path = |i: usize| self.path(i).unwrap_or("");
        let recency = |i: usize| rank.get(&i).copied().unwrap_or(usize::MAX);
        let order = |a: &(u32, usize), b: &(u32, usize)| {
            b.0.cmp(&a.0)
                .then_with(|| recency(a.1).cmp(&recency(b.1)))
                .then_with(|| path(a.1).len().cmp(&path(b.1).len()))
                .then_with(|| path(a.1).cmp(path(b.1)))
        };
        // 只需前 `limit` 条:先用 O(n) 的选择算法选出,再只排这一小段。短查询(一个字母)在大
        // 项目里几乎每条路径都命中,全排序的耗时随文件数增长,每次按键都会有可感知的延迟
        if limit == 0 {
            scored.clear();
        } else if scored.len() > limit {
            scored.select_nth_unstable_by(limit - 1, order);
            scored.truncate(limit);
        }
        scored.sort_by(order);
        Some(SearchOutcome {
            hits: scored.into_iter().map(|(_, i)| i).collect(),
            matched,
        })
    }

    /// 查询在第 `index` 个文件的路径里命中的字符下标(对齐方式与打分时相同);
    /// 不命中、空查询或下标越界时为空。
    pub fn matched_positions(&self, needle: &[char], index: usize) -> Vec<usize> {
        self.file(index)
            .and_then(|file| fuzzy::matched_positions(needle, file))
            .unwrap_or_default()
    }
}

/// 比较两条路径在文件表中的先后顺序:先比逐字符转小写后的路径(`a.txt` 和 `Cargo.toml` 按字母
/// 排在一起),只差大小写时再按原样比较,因此是全序,二分查找能找到确切的那一项。
///
/// 移植自 Winc(Apache-2.0)`crates/winc-editor/src/workspace/picker/scan.rs` 的 `path_order`。
pub fn path_order(a: &str, b: &str) -> std::cmp::Ordering {
    lower_order(a, b).then_with(|| a.cmp(b))
}

/// [`path_order`] 的第一键:逐字符转小写后比较。
///
/// 建表时要比较几百万次,所以先逐字节比较 ASCII 的小写形式,碰到非 ASCII 的字节时两边都正好在
/// 字符边界上,从这里起改为逐字符比较,结果与全程逐字符比较相同。用 `char::to_lowercase` 而
/// 不用 `str::to_lowercase`:后者会把词尾的 Σ 转成 ς,得到的顺序不一致。
fn lower_order(a: &str, b: &str) -> std::cmp::Ordering {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    for (i, (&p, &q)) in x.iter().zip(y).enumerate() {
        if !p.is_ascii() || !q.is_ascii() {
            return a[i..]
                .chars()
                .flat_map(char::to_lowercase)
                .cmp(b[i..].chars().flat_map(char::to_lowercase));
        }
        let (p, q) = (p.to_ascii_lowercase(), q.to_ascii_lowercase());
        if p != q {
            return p.cmp(&q);
        }
    }
    x.len().cmp(&y.len())
}

/// 把相对路径转成以 `/` 分隔的字符串;含非 UTF-8 的部分或路径为空时返回 `None`,调用方据此
/// 跳过该文件。
///
/// 移植自 Winc(Apache-2.0)`crates/winc-editor/src/workspace/picker/scan.rs` 的 `relative_path`。
fn relative_path(relative: &Path) -> Option<String> {
    let mut out = String::new();
    for part in relative.components() {
        let text = part.as_os_str().to_str()?;
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(text);
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quick_open::fuzzy::normalize_query;

    fn index(paths: &[&str]) -> FileIndex {
        FileIndex::from_relative_paths(Path::new("/tmp/mt-quick-open"), paths)
    }

    fn search<'a>(idx: &'a FileIndex, query: &str) -> Vec<&'a str> {
        idx.search(&normalize_query(query), &[], 200, None)
            .unwrap()
            .hits
            .into_iter()
            .map(|i| idx.path(i).unwrap())
            .collect()
    }

    fn first<'a>(idx: &'a FileIndex, query: &str) -> &'a str {
        search(idx, query)[0]
    }

    // ── 排序口径(与 Winc 的测试同名同义) ──

    #[test]
    fn 文件名命中优先于目录命中() {
        let idx = index(&["src/bcd/other.rs", "src/abc.rs"]);
        assert_eq!(search(&idx, "abc"), vec!["src/abc.rs"]);
    }

    #[test]
    fn 连续命中优先于散落命中() {
        let idx = index(&["src/m_a_i_n.rs", "src/main.rs"]);
        assert_eq!(first(&idx, "main"), "src/main.rs");
    }

    #[test]
    fn 词首命中优先于词中命中() {
        let idx = index(&["src/xxmxxaxxiyxxn.rs", "src/my_awesome_thing.rs"]);
        assert_eq!(first(&idx, "mat"), "src/my_awesome_thing.rs");
    }

    #[test]
    fn 驼峰的大写字母算词首() {
        let idx = index(&["src/PickerBar.rs", "src/pxxb.rs"]);
        assert_eq!(first(&idx, "pb"), "src/PickerBar.rs");
    }

    #[test]
    fn 同分时短路径在前() {
        let idx = index(&["a/very/deep/nested/path/to/file.rs", "file.rs"]);
        assert_eq!(first(&idx, "file"), "file.rs");
    }

    /// 目录和文件同名(`button/button.ts`):命中落进文件名的那条排前面;但向文件名一侧靠拢时
    /// 不能拆散开头连续的命中。
    #[test]
    fn 查询也是目录名时文件名那条在前() {
        let idx = index(&["components/button/index.ts", "components/button/button.ts"]);
        assert_eq!(first(&idx, "button"), "components/button/button.ts");
        let idx = index(&["src/lib/rs.c", "lib/x/s/r/c.rs"]);
        assert_eq!(first(&idx, "src"), "src/lib/rs.c");
    }

    /// 中文目录名一个字占三个字节。若按字节计算文件名起点,起点会偏进目录部分,目录里的 `a`
    /// 与文件名里的 `a` 就无法区分。
    #[test]
    fn 文件名加分按字符计不按字节计() {
        let idx = index(&["很长的中文目录名/b/a.rs", "很长的中文目录名/a/b.rs"]);
        assert_eq!(first(&idx, "a"), "很长的中文目录名/b/a.rs");
    }

    #[test]
    fn 中文路径能匹配() {
        let idx = index(&["文档/设计稿说明.md", "src/main.rs"]);
        assert_eq!(search(&idx, "设计"), vec!["文档/设计稿说明.md"]);
        assert_eq!(search(&idx, "文档说明"), vec!["文档/设计稿说明.md"]);
    }

    /// 空白不参与匹配(可以用空格分开路径的几段),反斜杠当 `/`。
    #[test]
    fn 查询忽略空白并接受反斜杠() {
        let idx = index(&["src/account/zebra.rs", "src/other.rs"]);
        for query in [
            "account zebra",
            "account/zebra",
            "account\\zebra",
            "src acc zeb",
        ] {
            assert_eq!(search(&idx, query), vec!["src/account/zebra.rs"], "{query}");
            let hit = idx.position("src/account/zebra.rs").unwrap();
            assert!(
                !idx.matched_positions(&normalize_query(query), hit)
                    .is_empty()
            );
        }
        assert!(search(&idx, "zebra account").is_empty(), "顺序颠倒不算命中");
    }

    // ── 最近打开 ──

    /// 空查询:最近打开的依次排最前(去重、越界丢掉),其余按表顺序接上。
    #[test]
    fn 空查询最近打开的排最前() {
        let idx = index(&["a.rs", "b.rs", "c.rs", "d.rs"]);
        let c = idx.position("c.rs").unwrap();
        let a = idx.position("a.rs").unwrap();
        let outcome = idx.search(&[], &[c, a, c, 99], 200, None).unwrap();
        let paths: Vec<&str> = outcome.hits.iter().map(|&i| idx.path(i).unwrap()).collect();
        assert_eq!(paths, vec!["c.rs", "a.rs", "b.rs", "d.rs"]);
        assert_eq!(outcome.matched, 4);
        // 上限照样生效
        let outcome = idx.search(&[], &[c, a], 1, None).unwrap();
        assert_eq!(outcome.hits, vec![c]);
    }

    /// 有查询时最近打开只是**同分**加权:分数更高的照样在前,同分时越近越前。
    #[test]
    fn 有查询时最近打开的同分靠前() {
        let idx = index(&["x/util.rs", "y/util.rs", "z/util.rs"]);
        let z = idx.position("z/util.rs").unwrap();
        let y = idx.position("y/util.rs").unwrap();
        let needle = normalize_query("util");
        let plain = idx.search(&needle, &[], 200, None).unwrap().hits;
        let paths: Vec<&str> = plain.iter().map(|&i| idx.path(i).unwrap()).collect();
        assert_eq!(
            paths,
            ["x/util.rs", "y/util.rs", "z/util.rs"],
            "没有最近记录时同分按字典序"
        );

        let hits = idx.search(&needle, &[z, y], 200, None).unwrap().hits;
        let paths: Vec<&str> = hits.iter().map(|&i| idx.path(i).unwrap()).collect();
        assert_eq!(paths, ["z/util.rs", "y/util.rs", "x/util.rs"]);

        // 分数不同的不受影响:`main.rs` 对 `main` 的分数远高于散落命中的那条
        let idx = index(&["m_a_i_n.rs", "src/main.rs"]);
        let scattered = idx.position("m_a_i_n.rs").unwrap();
        let hits = idx
            .search(&normalize_query("main"), &[scattered], 200, None)
            .unwrap()
            .hits;
        assert_eq!(idx.path(hits[0]), Some("src/main.rs"));
    }

    // ── 上限与计数 ──

    #[test]
    fn 结果封顶且命中数是截断前的() {
        let names: Vec<String> = (0..250).map(|i| format!("f{i}.rs")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let idx = index(&refs);
        let outcome = idx.search(&normalize_query("rs"), &[], 200, None).unwrap();
        assert_eq!(outcome.hits.len(), 200);
        assert_eq!(outcome.matched, 250);
        let outcome = idx.search(&normalize_query("zzz"), &[], 200, None).unwrap();
        assert!(outcome.hits.is_empty());
        assert_eq!(outcome.matched, 0);
        // 空查询的命中数就是文件总数
        assert_eq!(idx.search(&[], &[], 200, None).unwrap().matched, 250);
    }

    /// 截断时取的是全排序后的前 N 条,与不截断时的前 N 条一致。
    #[test]
    fn 选择算法取前_n_条与全排序一致() {
        let names: Vec<String> = (0..500)
            .map(|i| format!("dir{}/file_{i}_{}.rs", i % 7, i % 13))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let idx = index(&refs);
        let needle = normalize_query("f1r");
        let all = idx.search(&needle, &[], usize::MAX, None).unwrap().hits;
        let capped = idx.search(&needle, &[], 20, None).unwrap().hits;
        assert_eq!(&all[..20], &capped[..]);
    }

    #[test]
    fn 取消后中途放弃() {
        let idx = index(&["a.rs", "b.rs"]);
        let cancel = AtomicBool::new(true);
        assert!(
            idx.search(&normalize_query("a"), &[], 200, Some(&cancel))
                .is_none()
        );
        // 空查询不扫表,不看取消标志
        assert!(idx.search(&[], &[], 200, Some(&cancel)).is_some());
    }

    // ── 表本身 ──

    #[test]
    fn 按小写排序且能按路径定位() {
        let idx = index(&[
            "b.rs",
            "Cargo.toml",
            "a.txt",
            "src/Z.rs",
            "src/y.rs",
            "README",
        ]);
        let paths: Vec<&str> = (0..idx.len()).map(|i| idx.path(i).unwrap()).collect();
        assert_eq!(
            paths,
            [
                "a.txt",
                "b.rs",
                "Cargo.toml",
                "README",
                "src/y.rs",
                "src/Z.rs"
            ]
        );
        for (i, path) in paths.iter().enumerate() {
            assert_eq!(idx.position(path), Some(i));
            assert_eq!(idx.position_of(&idx.absolute(i).unwrap()), Some(i));
        }
        assert_eq!(idx.position("src/x.rs"), None);
        assert_eq!(idx.position_of(Path::new("/elsewhere/a.txt")), None);
        assert_eq!(idx.file(4).unwrap().name, "y.rs");
    }

    #[cfg(windows)]
    #[test]
    fn windows_上大小写不同也能定位() {
        let idx = FileIndex::from_relative_paths(Path::new(r"D:\Repo"), &["src/Main.rs"]);
        assert_eq!(idx.position("SRC/main.RS"), Some(0));
        assert_eq!(idx.position_of(Path::new(r"d:\repo\SRC\main.rs")), Some(0));
        assert_eq!(idx.position_of(Path::new(r"D:\Repo")), None);
        assert_eq!(
            idx.position_of(Path::new(r"D:\Repository\src\Main.rs")),
            None
        );
    }

    #[test]
    fn 路径顺序等价于逐字符小写比较() {
        fn reference(a: &str, b: &str) -> std::cmp::Ordering {
            a.chars()
                .flat_map(char::to_lowercase)
                .cmp(b.chars().flat_map(char::to_lowercase))
                .then_with(|| a.cmp(b))
        }
        let samples = [
            "",
            "a",
            "A",
            "a.txt",
            "Cargo.toml",
            "src/a",
            "src/A",
            "src/ä",
            "src/Ä",
            "Σ",
            "ς",
            "文档/说明",
            "文档/b",
            "İ",
            "i",
        ];
        for a in samples {
            for b in samples {
                assert_eq!(path_order(a, b), reference(a, b), "{a:?} {b:?}");
            }
        }
    }

    // ── 遍历 ──

    fn temp_project(tag: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mini-term-quick-open-{tag}-{ts}"));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    fn all_paths(idx: &FileIndex) -> Vec<&str> {
        (0..idx.len()).map(|i| idx.path(i).unwrap()).collect()
    }

    /// 忽略规则与全局搜索同一把尺子:认 `.gitignore`(在 git 仓库里)、黑名单目录、不藏点文件;
    /// 路径相对项目根、以 `/` 分隔。
    #[test]
    fn 遍历遵守与全局搜索相同的忽略规则() {
        let root = temp_project("ignore");
        // `.gitignore` 只在 git 仓库里才认(`require_git` 默认开),与全局搜索的测试同法
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "dist/\n*.log\n").unwrap();
        write(&root, "src/main.rs");
        write(&root, "src/deep/a/b.rs");
        write(&root, "README.md");
        write(&root, ".env.example");
        write(&root, "dist/bundle.js");
        write(&root, "debug.log");
        write(&root, "node_modules/x/index.js");
        write(&root, "target/debug/out.o");
        write(&root, "中文/说明.md");

        let cancel = AtomicBool::new(false);
        let idx = FileIndex::build(&root, BuildLimits::default(), &cancel).unwrap();
        let paths = all_paths(&idx);
        assert_eq!(
            paths,
            vec![
                ".env.example",
                ".gitignore",
                "README.md",
                "src/deep/a/b.rs",
                "src/main.rs",
                "中文/说明.md",
            ]
        );
        assert_eq!(idx.incomplete(), None);
        let main = idx.position("src/main.rs").unwrap();
        assert_eq!(idx.absolute(main), Some(root.join("src").join("main.rs")));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 文件数超限时截断并标成不完整() {
        let root = temp_project("cap");
        for i in 0..30 {
            write(&root, &format!("d{}/f{i}.txt", i % 3));
        }
        let cancel = AtomicBool::new(false);
        let limits = BuildLimits {
            max_files: 10,
            timeout: BUILD_TIMEOUT,
        };
        let idx = FileIndex::build(&root, limits, &cancel).unwrap();
        assert_eq!(idx.len(), 10);
        assert_eq!(idx.incomplete(), Some(Incomplete::TooManyFiles));

        let all = FileIndex::build(&root, BuildLimits::default(), &cancel).unwrap();
        assert_eq!(all.len(), 30);
        assert_eq!(all.incomplete(), None);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 超时时截断并标成不完整() {
        let root = temp_project("timeout");
        write(&root, "a.txt");
        let cancel = AtomicBool::new(false);
        let limits = BuildLimits {
            max_files: MAX_FILES,
            timeout: Duration::ZERO,
        };
        let idx = FileIndex::build(&root, limits, &cancel).unwrap();
        assert_eq!(idx.incomplete(), Some(Incomplete::TimedOut));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 取消时不出索引() {
        let root = temp_project("cancel");
        write(&root, "a.txt");
        let cancel = AtomicBool::new(true);
        assert!(FileIndex::build(&root, BuildLimits::default(), &cancel).is_none());
        std::fs::remove_dir_all(&root).ok();
    }
}
