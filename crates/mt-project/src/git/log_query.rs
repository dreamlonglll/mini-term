//! 带条件的提交历史:文本 / 作者过滤(历史区过滤栏)与单文件历史(Show History),
//! 外加提交详情区要的完整元信息。
//!
//! # 分页怎么保证不漏不重
//!
//! 无过滤的历史靠「已加载列表还原游标」续页(见 `mod.rs` 的 [`GitLogCursor`] 注释),
//! 前提是**走过的提交 = 已加载的提交**。过滤之后这条不成立了:被跳过的提交也走过了,
//! 却不在列表里,从列表还原出来的 frontier 会把它们再走一遍。
//!
//! 所以这里的续页游标由后端**原样交回**([`LogPage::next`]):`loaded` = 至今走过的
//! 全部提交(含跳过的),`frontier` = 已发现未产出的那些,按发现先后。遍历本身仍是
//! 同一个 [`DateTopoWalk`](增量 Kahn + 按时间),只是产出时多一道 `keep` 判定。
//! 跳过的提交照常推进拓扑,所以跨页不漏不重的论证与无过滤时一字不差。
//!
//! # 为什么分批走([`SCAN_BUDGET`])
//!
//! 条件很少命中时,凑满一页可能要走遍整个历史。一次走完会把后台线程占满、UI 也没法
//! 半路换条件 —— 于是每次最多走 [`SCAN_BUDGET`] 个提交,没凑满也先交回(带游标),
//! UI 见「不满页但还有下文」就接着要。内存只有走过的提交号集合(20 字节一个),
//! 不会把整个历史的提交对象读进来。

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use git2::{Commit, Oid, Repository};
use pathdiff::diff_paths;
use serde::{Deserialize, Serialize};

use super::{
    CommitFileInfo, DateTopoWalk, GitCommitInfo, GitLogCursor, MAX_DISCOVER_PARENTS,
    discover_repo_for, get_commit_files, log_tip, nth_parent,
};

/// 每次调用最多走过的提交数。实测 git2 读一个提交 + 比对条件是几微秒级,
/// 两万个在百毫秒量级,足够让「换个条件」的响应跟得上手。
pub const SCAN_BUDGET: usize = 20_000;

/// 历史区过滤条件。两项同时给时取交集。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogFilter {
    /// 匹配提交标题 / 正文、哈希前缀、作者名 / 邮箱(忽略大小写)。空串 = 不按文本过滤。
    pub text: String,
    /// 只要这个邮箱的提交(「仅我的」= 仓库的 `user.email`)。比对忽略大小写。
    pub author_email: Option<String>,
}

impl LogFilter {
    /// 有没有任何实际生效的条件。
    pub fn is_active(&self) -> bool {
        !self.text.trim().is_empty() || self.author_email.is_some()
    }
}

/// 一页带条件的历史。
#[derive(Debug, Clone)]
pub struct LogPage {
    pub commits: Vec<GitCommitInfo>,
    /// 续页游标;`None` = 历史已经走完。
    pub next: Option<GitLogCursor>,
    /// 这一次走过的提交数(含没命中的),UI 显示「已扫描 N 个」用。
    pub scanned: usize,
}

/// 预处理好的匹配器:小写化只做一次。
struct Matcher {
    needle: Option<String>,
    /// 关键词本身是十六进制 → 也按哈希前缀匹配。
    hex: bool,
    email: Option<String>,
}

impl Matcher {
    fn new(filter: &LogFilter) -> Self {
        let needle = Some(filter.text.trim().to_lowercase()).filter(|n| !n.is_empty());
        let hex = needle
            .as_deref()
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_hexdigit()));
        Self {
            needle,
            hex,
            email: filter
                .author_email
                .as_deref()
                .map(|e| e.trim().to_lowercase()),
        }
    }

    fn matches(&self, commit: &Commit<'_>) -> bool {
        let author = commit.author();
        if let Some(email) = &self.email {
            let theirs = author.email().unwrap_or("").to_lowercase();
            if &theirs != email {
                return false;
            }
        }
        let Some(needle) = &self.needle else {
            return true;
        };
        if self.hex && commit.id().to_string().starts_with(needle.as_str()) {
            return true;
        }
        let hit = |s: Option<&str>| s.is_some_and(|s| s.to_lowercase().contains(needle.as_str()));
        hit(author.name()) || hit(author.email()) || hit(commit.message())
    }
}

/// 从游标(或起点)接着走一批,只产出 `keep` 认可的提交,交回结果与新游标。
fn walk_page(
    repo: &Repository,
    after: Option<GitLogCursor>,
    tip: Option<Oid>,
    limit: usize,
    budget: usize,
    keep: impl FnMut(&Commit<'_>) -> bool,
) -> LogPage {
    let resuming = after.is_some();
    let mut cursor = after.unwrap_or_default();
    let (commits, scanned, walked, frontier) = {
        let mut walk = DateTopoWalk::new(repo, &cursor);
        if resuming {
            for &oid in &cursor.frontier {
                walk.discover(oid);
            }
        } else if let Some(tip) = tip {
            walk.discover(tip);
        }
        let (commits, scanned) = walk.take_matching(limit, budget, keep);
        // 走过的 = 本次发现过、已不在待产出表里的(含读不到的坏对象 —— 一并记上,
        // 下次不再白读);frontier = 还在待产出表里的,按发现先后
        let walked: Vec<Oid> = walk
            .discovered
            .iter()
            .filter(|oid| !walk.pending.contains_key(oid))
            .copied()
            .collect();
        let mut frontier: Vec<(u64, Oid)> = walk
            .pending
            .iter()
            .map(|(oid, (_, seq))| (*seq, *oid))
            .collect();
        frontier.sort_unstable();
        (
            commits,
            scanned,
            walked,
            frontier.into_iter().map(|(_, oid)| oid).collect::<Vec<_>>(),
        )
    };
    cursor.loaded.extend(walked);
    cursor.frontier = frontier;
    LogPage {
        commits,
        next: (!cursor.frontier.is_empty()).then_some(cursor),
        scanned,
    }
}

/// 带条件取一页历史。
///
/// - `after = None`:首页,从 `branch`(`None` = HEAD)起走;
/// - `after = Some(游标)`:续页,游标必须是上一页 [`LogPage::next`] 交回的那个
///   (**不是** [`GitLogCursor::from_loaded`] 还原的 —— 见模块注释)。
///
/// 返回的 `commits` 可能不满 `limit`(本批走满了 [`SCAN_BUDGET`]),只要 `next`
/// 还在就说明后面还有。空仓库返回空页。
pub fn get_git_log_filtered(
    repo_path: &Path,
    after: Option<GitLogCursor>,
    limit: usize,
    branch: Option<&str>,
    filter: &LogFilter,
) -> Result<LogPage> {
    let repo = Repository::open(repo_path)?;
    let tip = if after.is_none() {
        match log_tip(&repo, branch)? {
            Some(tip) => Some(tip),
            None => {
                return Ok(LogPage {
                    commits: Vec::new(),
                    next: None,
                    scanned: 0,
                });
            }
        }
    } else {
        None
    };
    let matcher = Matcher::new(filter);
    Ok(walk_page(&repo, after, tip, limit, SCAN_BUDGET, |c| {
        matcher.matches(c)
    }))
}

/// 仓库配置里的 `user.email`(「仅我的」用)。没配返回 `None`。
pub fn config_user_email(repo_path: &Path) -> Option<String> {
    let repo = Repository::open(repo_path).ok()?;
    let config = repo.config().ok()?;
    config
        .get_string("user.email")
        .ok()
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
}

// ─── 单文件历史 ───────────────────────────────────────────────

/// 文件在某次提交里的变动类型(相对第一个父提交)。字符串口径与
/// [`CommitFileInfo::status`] 一致。
fn file_change(commit: &Commit<'_>, path: &Path) -> Option<&'static str> {
    let tree = commit.tree().ok()?;
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    // 整棵树没变(空提交 / 只改了别处又改回来的合并)直接不算
    if parent_tree.as_ref().is_some_and(|p| p.id() == tree.id()) {
        return None;
    }
    let new = tree.get_path(path).ok().map(|e| e.id());
    let old = parent_tree
        .as_ref()
        .and_then(|t| t.get_path(path).ok().map(|e| e.id()));
    match (old, new) {
        (None, None) => None,
        (None, Some(_)) => Some("added"),
        (Some(_), None) => Some("deleted"),
        (Some(a), Some(b)) if a != b => Some("modified"),
        _ => None,
    }
}

/// 单文件历史的一条:提交 + 该文件在这次提交里的变动类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileLogEntry {
    pub commit: GitCommitInfo,
    /// `added` / `modified` / `deleted`。
    pub status: String,
}

/// 一页单文件历史。
#[derive(Debug, Clone)]
pub struct FileLogPage {
    /// 文件所属仓库的工作区根。取 diff 要用它。
    pub repo_root: PathBuf,
    /// 文件相对仓库根的路径(`/` 分隔)。
    pub path_in_repo: String,
    pub entries: Vec<FileLogEntry>,
    pub next: Option<GitLogCursor>,
    pub scanned: usize,
}

/// 把「项目根 + 相对路径」换算成「所属仓库根 + 仓库内路径」。项目根可以是
/// 仓库里的子目录(monorepo 子项目),也可以有嵌套子仓库 —— 取离文件最近的那个。
pub fn resolve_repo_file(project_path: &Path, file_rel: &str) -> Result<(PathBuf, String)> {
    let abs_file = project_path.join(file_rel);
    let repo = discover_repo_for(project_path, &abs_file)
        .ok_or_else(|| anyhow!("{} 不在 git 仓库中", abs_file.display()))?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow!("bare repository not supported"))?
        .to_path_buf();
    let rel = diff_paths(&abs_file, &workdir)
        .ok_or_else(|| anyhow!("file is outside repository working directory"))?;
    let rel = rel.to_string_lossy().replace('\\', "/");
    if rel.is_empty() || rel.starts_with("../") {
        return Err(anyhow!("file is outside repository working directory"));
    }
    Ok((workdir, rel))
}

/// 文件是否落在某个 git 仓库里(文件树菜单「Git 历史」的显示条件)。
///
/// **只做 stat,不开仓库**:菜单是右键当场在 UI 线程上建的,libgit2 开仓库要读配置,
/// WSL 的 UNC 路径上会明显卡一下。判据与 [`resolve_repo_file`] 的发现范围一致:
/// 从文件所在目录往上,最多到项目根再往上 [`MAX_DISCOVER_PARENTS`] 级,
/// 途中任一层有 `.git`(目录或 worktree 的 `.git` 文件)即算。
pub fn is_inside_git_repo(project_path: &Path, abs_file: &Path) -> bool {
    let ceiling = nth_parent(project_path, MAX_DISCOVER_PARENTS);
    for dir in abs_file.ancestors().skip(1) {
        if dir.join(".git").exists() {
            return true;
        }
        if dir == ceiling || dir.as_os_str().is_empty() {
            break;
        }
    }
    false
}

/// 单文件历史(IDEA 的 Show History):触及该文件的提交,按与历史区相同的顺序分页。
///
/// 「触及」= 该提交相对**第一个父提交**改动了这个路径(根提交里存在也算)。
/// **不跟随重命名**:改名之前的历史停在「新增」那一条 —— 跟随要在每个「新增」点
/// 做一次带相似度检测的 diff,并且各条分支上可能是不同的旧名,超出这一版的范围。
///
/// `after` 的口径与 [`get_git_log_filtered`] 相同(用上一页交回的游标)。
pub fn get_file_log(
    project_path: &Path,
    file_rel: &str,
    after: Option<GitLogCursor>,
    limit: usize,
) -> Result<FileLogPage> {
    let (repo_root, path_in_repo) = resolve_repo_file(project_path, file_rel)?;
    let repo = Repository::open(&repo_root)?;
    let tip = if after.is_none() {
        log_tip(&repo, None)?
    } else {
        None
    };
    if after.is_none() && tip.is_none() {
        return Ok(FileLogPage {
            repo_root,
            path_in_repo,
            entries: Vec::new(),
            next: None,
            scanned: 0,
        });
    }
    let path = PathBuf::from(&path_in_repo);
    // keep 与产出一一对应(同一次循环里先判 keep 再 push),变动类型按顺序记一份
    let mut statuses: Vec<&'static str> = Vec::new();
    let page = walk_page(
        &repo,
        after,
        tip,
        limit,
        SCAN_BUDGET,
        |c| match file_change(c, &path) {
            Some(status) => {
                statuses.push(status);
                true
            }
            None => false,
        },
    );
    let entries = page
        .commits
        .into_iter()
        .zip(statuses)
        .map(|(commit, status)| FileLogEntry {
            commit,
            status: status.to_string(),
        })
        .collect();
    Ok(FileLogPage {
        repo_root,
        path_in_repo,
        entries,
        next: page.next,
        scanned: page.scanned,
    })
}

// ─── 提交详情 ─────────────────────────────────────────────────

/// 提交详情区要的全部信息(列表行里的 [`GitCommitInfo`] 不带邮箱、提交者与文件)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitDetail {
    pub hash: String,
    /// 完整提交消息(标题 + 正文),去掉末尾空行。
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub author_time: i64,
    pub committer_name: String,
    pub committer_email: String,
    pub commit_time: i64,
    pub parent_hashes: Vec<String>,
    /// 相对第一个父提交的改动文件(与提交 diff 弹窗同一份口径)。
    pub files: Vec<CommitFileInfo>,
}

/// 取一个提交的详情。
pub fn get_commit_detail(repo_path: &Path, hash: &str) -> Result<CommitDetail> {
    let files = get_commit_files(repo_path, hash)?;
    let repo = Repository::open(repo_path)?;
    let commit = repo.find_commit(Oid::from_str(hash)?)?;
    let author = commit.author();
    let committer = commit.committer();
    Ok(CommitDetail {
        hash: commit.id().to_string(),
        message: String::from_utf8_lossy(commit.message_bytes())
            .trim_end()
            .to_string(),
        author_name: author.name().unwrap_or("unknown").to_string(),
        author_email: author.email().unwrap_or("").to_string(),
        author_time: author.when().seconds(),
        committer_name: committer.name().unwrap_or("unknown").to_string(),
        committer_email: committer.email().unwrap_or("").to_string(),
        commit_time: commit.time().seconds(),
        parent_hashes: commit.parent_ids().map(|p| p.to_string()).collect(),
        files,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    use git2::{Oid, Repository};

    use super::super::test_support::init_repo_with_file;
    use super::*;

    /// 直接造提交对象的空仓库(同 `mod.rs` 测试里的 `LogRepo`,精简到这里用得上的)。
    struct Synth {
        root: PathBuf,
        repo: Repository,
    }

    impl Synth {
        fn new(tag: &str) -> Self {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("mini-term-logq-{tag}-{ts}"));
            std::fs::create_dir_all(&root).unwrap();
            let repo = Repository::init(&root).unwrap();
            Self { root, repo }
        }

        /// `files` 是这个提交的**完整**文件表(路径, 内容)。
        fn commit(
            &self,
            msg: &str,
            email: &str,
            time: i64,
            parents: &[Oid],
            files: &[(&str, &str)],
        ) -> Oid {
            let mut builder = self.repo.treebuilder(None).unwrap();
            for (path, content) in files {
                let blob = self.repo.blob(content.as_bytes()).unwrap();
                builder.insert(path, blob, 0o100644).unwrap();
            }
            let tree = self.repo.find_tree(builder.write().unwrap()).unwrap();
            let sig = git2::Signature::new("tester", email, &git2::Time::new(time, 0)).unwrap();
            let parents: Vec<_> = parents
                .iter()
                .map(|p| self.repo.find_commit(*p).unwrap())
                .collect();
            let refs: Vec<_> = parents.iter().collect();
            self.repo
                .commit(None, &sig, &sig, msg, &tree, &refs)
                .unwrap()
        }

        fn head_to(&self, oid: Oid) {
            self.repo
                .reference("refs/heads/main", oid, true, "test")
                .unwrap();
            self.repo.set_head("refs/heads/main").unwrap();
        }

        fn msg(&self, hash: &str) -> String {
            self.repo
                .find_commit(Oid::from_str(hash).unwrap())
                .unwrap()
                .summary()
                .unwrap()
                .to_string()
        }
    }

    impl Drop for Synth {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    /// 按 UI 的用法翻到底:首页 `after = None`,之后一律用上一页交回的游标;
    /// 每页都查「不回已出现过的提交」。
    fn load_all(root: &Path, filter: &LogFilter, page: usize) -> Vec<GitCommitInfo> {
        let mut out: Vec<GitCommitInfo> = Vec::new();
        let mut seen = HashSet::new();
        let mut after = None;
        let mut first = true;
        for round in 0.. {
            assert!(round < 10_000, "翻页不收敛");
            if !first && after.is_none() {
                break;
            }
            first = false;
            let got = get_git_log_filtered(root, after.take(), page, None, filter).unwrap();
            assert!(got.commits.len() <= page);
            for c in &got.commits {
                assert!(seen.insert(c.hash.clone()), "重复产出 {}", c.hash);
            }
            out.extend(got.commits);
            after = got.next;
        }
        out
    }

    fn msgs(s: &Synth, list: &[GitCommitInfo]) -> Vec<String> {
        list.iter().map(|c| s.msg(&c.hash)).collect()
    }

    /// 合并历史 + 时钟偏移下,带条件翻页的结果 = 不带条件全集里命中的那些,
    /// 且顺序与不带条件时一致(子序列)、任意页大小都不漏不重。
    #[test]
    fn 过滤分页_不漏不重且保持顺序() {
        let s = Synth::new("filter-paging");
        let a = "alice@example.com";
        let b = "BOB@example.com";
        let base = s.commit("base", a, 100, &[], &[]);
        let mut left = base;
        let mut right = base;
        for i in 0..8 {
            // 右枝时间整体比左枝早(时钟偏移),作者交替
            left = s.commit(
                &format!("L{i} fix"),
                if i % 2 == 0 { a } else { b },
                200 + i,
                &[left],
                &[],
            );
            right = s.commit(
                &format!("R{i} feat"),
                if i % 3 == 0 { b } else { a },
                150 - i,
                &[right],
                &[],
            );
        }
        let m = s.commit("merge", a, 300, &[left, right], &[]);
        s.head_to(m);

        let everything = load_all(&s.root, &LogFilter::default(), 1000);
        assert_eq!(everything.len(), 18);
        let order: Vec<String> = msgs(&s, &everything);

        for filter in [
            LogFilter {
                text: "FIX".into(),
                author_email: None,
            },
            LogFilter {
                text: String::new(),
                author_email: Some("bob@example.com".into()),
            },
            LogFilter {
                text: "feat".into(),
                author_email: Some(a.into()),
            },
        ] {
            let expected: Vec<String> = everything
                .iter()
                .filter(|c| {
                    let commit = s.repo.find_commit(Oid::from_str(&c.hash).unwrap()).unwrap();
                    Matcher::new(&filter).matches(&commit)
                })
                .map(|c| s.msg(&c.hash))
                .collect();
            assert!(!expected.is_empty());
            for page in [1, 2, 3, 5, 100] {
                let got = msgs(&s, &load_all(&s.root, &filter, page));
                assert_eq!(got, expected, "{filter:?} 页大小 {page}");
            }
            // 是全量顺序的子序列
            let mut it = order.iter();
            for m in &expected {
                assert!(it.any(|o| o == m), "{m} 顺序与全量不一致");
            }
        }
    }

    /// 走满一批还没凑满时先交回(带游标),接着要能把剩下的找全。
    #[test]
    fn 过滤分批扫描_预算用完先交回() {
        let s = Synth::new("filter-budget");
        let mut tip = s.commit("needle-0", "x@e.com", 1, &[], &[]);
        for i in 1..50 {
            let msg = if i % 10 == 0 {
                format!("needle-{i}")
            } else {
                format!("hay-{i}")
            };
            tip = s.commit(&msg, "x@e.com", 1 + i, &[tip], &[]);
        }
        s.head_to(tip);
        let repo = Repository::open(&s.root).unwrap();
        let tip_oid = log_tip(&repo, None).unwrap();
        let matcher = Matcher::new(&LogFilter {
            text: "needle".into(),
            author_email: None,
        });
        let mut found = Vec::new();
        let mut after = None;
        let mut rounds = 0;
        loop {
            let tip = if after.is_none() && rounds == 0 {
                tip_oid
            } else {
                None
            };
            let page = walk_page(&repo, after.take(), tip, 100, 7, |c| matcher.matches(c));
            assert!(page.scanned <= 7);
            found.extend(page.commits.iter().map(|c| s.msg(&c.hash)));
            rounds += 1;
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        assert_eq!(rounds, 50usize.div_ceil(7));
        assert_eq!(
            found,
            [
                "needle-40",
                "needle-30",
                "needle-20",
                "needle-10",
                "needle-0"
            ]
        );
    }

    #[test]
    fn 过滤_哈希前缀与大小写() {
        let s = Synth::new("filter-hash");
        let c1 = s.commit("Hello World", "Dev@Example.com", 1, &[], &[]);
        let c2 = s.commit("other", "x@y.z", 2, &[c1], &[]);
        s.head_to(c2);
        let prefix = c1.to_string()[..8].to_uppercase();
        let got = load_all(
            &s.root,
            &LogFilter {
                text: prefix,
                author_email: None,
            },
            30,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].hash, c1.to_string());
        // 作者邮箱的子串也算文本命中
        let got = load_all(
            &s.root,
            &LogFilter {
                text: "dev@example".into(),
                author_email: None,
            },
            30,
        );
        assert_eq!(msgs(&s, &got), ["Hello World"]);
        // 「仅我的」整串比对、忽略大小写
        let got = load_all(
            &s.root,
            &LogFilter {
                text: String::new(),
                author_email: Some("dev@example.COM".into()),
            },
            30,
        );
        assert_eq!(msgs(&s, &got), ["Hello World"]);
        assert!(!LogFilter::default().is_active());
        assert!(
            !LogFilter {
                text: "  ".into(),
                author_email: None
            }
            .is_active()
        );
    }

    #[test]
    fn 过滤_空仓库与无命中() {
        let s = Synth::new("filter-empty");
        let page = get_git_log_filtered(&s.root, None, 30, None, &LogFilter::default()).unwrap();
        assert!(page.commits.is_empty() && page.next.is_none());
        let c = s.commit("only", "a@b.c", 1, &[], &[]);
        s.head_to(c);
        let page = get_git_log_filtered(
            &s.root,
            None,
            30,
            None,
            &LogFilter {
                text: "zzz".into(),
                author_email: None,
            },
        )
        .unwrap();
        assert!(page.commits.is_empty());
        assert!(page.next.is_none(), "走完了就没有下文");
        assert_eq!(page.scanned, 1);
    }

    /// 文件历史:只列改动了该路径的提交(相对第一个父),变动类型对得上;
    /// 删除后重新加回来也接得上;合并提交带进来的改动也算;跨页不漏。
    #[test]
    fn 文件历史_触及判定与分页() {
        let s = Synth::new("file-log");
        let c1 = s.commit("add a", "a@b.c", 1, &[], &[("a.txt", "1"), ("b.txt", "1")]);
        let c2 = s.commit(
            "touch b",
            "a@b.c",
            2,
            &[c1],
            &[("a.txt", "1"), ("b.txt", "2")],
        );
        let c3 = s.commit(
            "edit a",
            "a@b.c",
            3,
            &[c2],
            &[("a.txt", "2"), ("b.txt", "2")],
        );
        let c4 = s.commit("rm a", "a@b.c", 4, &[c3], &[("b.txt", "2")]);
        let side = s.commit(
            "side a",
            "a@b.c",
            5,
            &[c4],
            &[("a.txt", "side"), ("b.txt", "2")],
        );
        let c5 = s.commit("b again", "a@b.c", 6, &[c4], &[("b.txt", "3")]);
        let m = s.commit(
            "merge",
            "a@b.c",
            7,
            &[c5, side],
            &[("a.txt", "side"), ("b.txt", "3")],
        );
        s.head_to(m);

        let mut all = Vec::new();
        let mut after = None;
        let mut first = true;
        while first || after.is_some() {
            first = false;
            let page = get_file_log(&s.root, "a.txt", after.take(), 2).unwrap();
            assert_eq!(page.path_in_repo, "a.txt");
            all.extend(page.entries);
            after = page.next;
        }
        let got: Vec<(String, String)> = all
            .iter()
            .map(|e| (s.msg(&e.commit.hash), e.status.clone()))
            .collect();
        let want = [
            ("merge", "added"),
            ("side a", "added"),
            ("rm a", "deleted"),
            ("edit a", "modified"),
            ("add a", "added"),
        ];
        let want: Vec<(String, String)> = want
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        assert_eq!(got, want);

        // 仓库里没有的文件:空历史而不是报错
        let page = get_file_log(&s.root, "nope.txt", None, 30).unwrap();
        assert!(page.entries.is_empty());
    }

    /// 项目根是仓库的子目录:相对路径按仓库根换算。
    #[test]
    fn 文件历史_项目是仓库子目录() {
        let root = init_repo_with_file("file-log-sub", "pkg/src/lib.rs", "fn a() {}\n");
        let project = root.join("pkg");
        let (repo_root, rel) = resolve_repo_file(&project, "src/lib.rs").unwrap();
        assert_eq!(rel, "pkg/src/lib.rs");
        assert_eq!(
            crate::fs::strip_verbatim_prefix(repo_root.canonicalize().unwrap()),
            root
        );
        let page = get_file_log(&project, "src/lib.rs", None, 30).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].status, "added");
        assert!(is_inside_git_repo(&project, &project.join("src/lib.rs")));
        // 把 .git 挪走就不算了(只看 stat,不开仓库)
        std::fs::rename(root.join(".git"), root.join("not-git")).unwrap();
        assert!(!is_inside_git_repo(&project, &project.join("src/lib.rs")));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn 提交详情带邮箱与文件() {
        let root = init_repo_with_file("detail", "a.txt", "1\n");
        let repo = Repository::open(&root).unwrap();
        let hash = repo.head().unwrap().target().unwrap().to_string();
        let detail = get_commit_detail(&root, &hash).unwrap();
        assert_eq!(detail.hash, hash);
        assert_eq!(detail.message, "init");
        assert_eq!(detail.author_email, "test@example.com");
        assert!(detail.parent_hashes.is_empty());
        assert_eq!(detail.files.len(), 1);
        assert_eq!(detail.files[0].path, "a.txt");
        assert_eq!(
            config_user_email(&root).as_deref(),
            Some("test@example.com")
        );
        drop(repo);
        std::fs::remove_dir_all(root).ok();
    }
}
