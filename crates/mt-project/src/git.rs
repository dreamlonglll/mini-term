//! Git:状态 / diff / log / 分支 / 暂存 / 提交 / worktree。
//!
//! 从 `src-tauri/src/git.rs` 移入,逐函数照搬,只做三件事:去 `#[tauri::command]`、
//! 路径参数 `String` → `&Path`/`&str`、错误 `Result<T, String>` → `anyhow::Result<T>`
//! (面向用户的中文错误文案原样保留)。
//!
//! `git2` 仍带 `vendored-openssl` feature —— 换 GPUI 不改变这条,
//! Windows MSVC 上的坑与依据见 `spec/backend/rust-crypto-on-windows-msvc.md`。
//!
//! 网络类操作(pull/push/worktree add/remove/prune)走 git CLI 而非 git2:
//! 凭证管理器 / SSH agent / hooks 这些都由 CLI 天然继承。原实现靠
//! `#[tauri::command(async)]` 把阻塞等待挪出主线程,现在这一层不做线程调度,
//! **调用方必须自己放到后台执行器上跑**(GPUI 的 `background_executor`),
//! 否则 30s/120s 的 `recv_timeout` 会卡住 UI 线程。

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use git2::{Commit, Oid, Repository, RepositoryOpenFlags, Status, StatusOptions};
use parking_lot::Mutex;
use pathdiff::diff_paths;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum GitStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GitFileStatus {
    pub path: String,
    pub old_path: Option<String>,
    pub status: GitStatus,
    pub status_label: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ChangeFileStatus {
    pub path: String,
    pub old_path: Option<String>,
    pub staged_status: Option<GitStatus>,
    pub unstaged_status: Option<GitStatus>,
    pub status_label: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DiffLine {
    pub kind: String,
    pub content: String,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffResult {
    pub old_content: String,
    pub new_content: String,
    pub hunks: Vec<DiffHunk>,
    pub is_binary: bool,
    pub too_large: bool,
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

fn map_status(status: Status, is_empty_repo: bool) -> Option<GitStatus> {
    if status.contains(Status::CONFLICTED) {
        return Some(GitStatus::Conflicted);
    }
    if status.contains(Status::INDEX_RENAMED) || status.contains(Status::WT_RENAMED) {
        return Some(GitStatus::Renamed);
    }
    if status.contains(Status::INDEX_NEW) {
        return Some(GitStatus::Added);
    }
    if status.contains(Status::INDEX_MODIFIED) || status.contains(Status::WT_MODIFIED) {
        return Some(GitStatus::Modified);
    }
    if status.contains(Status::INDEX_DELETED) || status.contains(Status::WT_DELETED) {
        return Some(GitStatus::Deleted);
    }
    if status.contains(Status::WT_NEW) {
        if is_empty_repo {
            return Some(GitStatus::Added);
        } else {
            return Some(GitStatus::Untracked);
        }
    }
    None
}

fn status_label(status: &GitStatus) -> &'static str {
    match status {
        GitStatus::Modified => "M",
        GitStatus::Added => "A",
        GitStatus::Deleted => "D",
        GitStatus::Renamed => "R",
        GitStatus::Untracked => "?",
        GitStatus::Conflicted => "C",
    }
}

fn map_staged_status(status: Status) -> Option<GitStatus> {
    if status.contains(Status::CONFLICTED) {
        return Some(GitStatus::Conflicted);
    }
    if status.contains(Status::INDEX_RENAMED) {
        return Some(GitStatus::Renamed);
    }
    if status.contains(Status::INDEX_NEW) {
        return Some(GitStatus::Added);
    }
    if status.contains(Status::INDEX_MODIFIED) {
        return Some(GitStatus::Modified);
    }
    if status.contains(Status::INDEX_DELETED) {
        return Some(GitStatus::Deleted);
    }
    None
}

fn map_unstaged_status(status: Status, is_empty_repo: bool) -> Option<GitStatus> {
    if status.contains(Status::CONFLICTED) {
        return Some(GitStatus::Conflicted);
    }
    if status.contains(Status::WT_RENAMED) {
        return Some(GitStatus::Renamed);
    }
    if status.contains(Status::WT_MODIFIED) {
        return Some(GitStatus::Modified);
    }
    if status.contains(Status::WT_DELETED) {
        return Some(GitStatus::Deleted);
    }
    if status.contains(Status::WT_NEW) {
        if is_empty_repo {
            return Some(GitStatus::Added);
        } else {
            return Some(GitStatus::Untracked);
        }
    }
    None
}

fn collect_repo_status(
    repo: &Repository,
    path_prefix: Option<&Path>,
) -> Result<Vec<GitFileStatus>> {
    let is_empty_repo = repo.head().is_err();

    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);

    let statuses = repo.statuses(Some(&mut opts))?;

    let mut result = Vec::new();
    for entry in statuses.iter() {
        let raw_path = entry.path().unwrap_or("").to_string();
        let s = entry.status();

        let git_status = match map_status(s, is_empty_repo) {
            Some(gs) => gs,
            None => continue,
        };

        let label = status_label(&git_status).to_string();

        // 相对 path_prefix 的展示路径;没给 prefix 就用仓库内相对路径
        let display_path = if let Some(prefix) = path_prefix {
            let repo_workdir = repo.workdir().unwrap_or_else(|| repo.path());
            let abs = repo_workdir.join(&raw_path);
            diff_paths(&abs, prefix)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|| raw_path.clone())
        } else {
            raw_path.clone()
        };

        // 重命名才有 old_path
        let old_path = if matches!(git_status, GitStatus::Renamed) {
            entry.head_to_index().and_then(|d| {
                d.old_file()
                    .path()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
            })
        } else {
            None
        };

        result.push(GitFileStatus {
            path: display_path,
            old_path,
            status: git_status,
            status_label: label,
        });
    }

    Ok(result)
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GitRepoInfo {
    pub name: String,
    pub path: PathBuf,
    pub current_branch: Option<String>,
    /// 该条目是不是某个主仓库的 linked worktree(UI 据此显示 ⎇ 标识)
    pub is_worktree: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GitCommitInfo {
    pub hash: String,
    pub short_hash: String,
    pub message: String,
    pub body: Option<String>,
    pub author: String,
    pub timestamp: i64,
    /// 全部父提交 hash（按 git 顺序：第 0 个是主线父）。据此绘制分支拓扑图。
    pub parent_hashes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CommitFileInfo {
    pub path: String,
    pub status: String,
    pub old_path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    pub is_remote: bool,
    pub commit_hash: String,
    /// 本地分支关联的远程分支短名(`origin/main`)。远程分支、没配上游、
    /// 上游的远程跟踪引用已不存在(远端删了又 prune 过)时都是 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
}

/// 仓库发现允许越过**项目根**往上爬的层数(项目是 monorepo 子目录时仓库在项目根之上)。
const MAX_DISCOVER_PARENTS: usize = 5;

/// 为 `project_path` 下的文件 `abs_file` 找它所属的仓库。
///
/// 起点是文件所在目录、向上搜;搜索上界锚在 **项目根再往上 [`MAX_DISCOVER_PARENTS`] 级**。
/// 两个坑都踩过,别改回去:
///
/// 1. **上界不能按文件算。** 旧实现拿文件往上数 5 级当 ceiling,文件嵌得越深、ceiling 就
///    越可能落在仓库根**以内**——`src/pages/task/my/my.vue` 往上 5 级正好是仓库根,diff
///    直接报「找不到仓库」(issue #58);凡是仓库根以下 ≥4 层目录的文件全中招。锚在项目根
///    上,能不能找到就与文件深度无关了。
/// 2. **libgit2 的 ceiling 是排他的**:走到 ceiling 目录本身即停、不检查它(`repository.c`
///    的 `path.ptr[ceiling_offset] == 0` 即 break)。要让「往上 N 级」都被查到,ceiling 得取
///    第 N+1 级。
///
/// 起点从文件所在目录出发而不是直接开项目根,是为了让嵌套子仓库里的文件找到离它最近的
/// 那个仓库(文件树的「查看变更」标签就是按最近仓库算的)。目录已随文件一起被删掉时回退
/// 到最近一个还存在的祖先——libgit2 会对起点做 realpath,不存在直接报错。
fn discover_repo_for(project_path: &Path, abs_file: &Path) -> Option<Repository> {
    let start = abs_file
        .ancestors()
        .skip(1)
        .find(|p| p.is_dir())
        .unwrap_or(project_path);
    open_repo_within(start, project_path)
}

/// 项目根所属的仓库:从项目根本身向上找,上界同样是项目根再往上 [`MAX_DISCOVER_PARENTS`] 级。
fn discover_repo_limited(project_path: &Path) -> Option<Repository> {
    open_repo_within(project_path, project_path)
}

/// 从 `start` 向上找仓库,最多找到 `anchor` 往上 [`MAX_DISCOVER_PARENTS`] 级为止
/// (libgit2 的 ceiling 排他,故取第 N+1 级)。
fn open_repo_within(start: &Path, anchor: &Path) -> Option<Repository> {
    let ceiling = nth_parent(anchor, MAX_DISCOVER_PARENTS + 1);
    Repository::open_ext(start, RepositoryOpenFlags::empty(), &[&ceiling]).ok()
}

/// `path` 往上 `n` 级的祖先;不够 `n` 级就停在根(或相对路径的顶层)。
fn nth_parent(path: &Path, n: usize) -> PathBuf {
    let mut cur = path;
    for _ in 0..n {
        match cur.parent() {
            Some(p) if !p.as_os_str().is_empty() => cur = p,
            _ => break,
        }
    }
    cur.to_path_buf()
}

#[derive(Clone)]
struct RepoPathEntry {
    name: String,
    path: PathBuf,
    is_worktree: bool,
}

static REPO_PATH_CACHE: std::sync::LazyLock<
    Mutex<HashMap<PathBuf, (Instant, Vec<RepoPathEntry>)>>,
> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

const REPO_CACHE_TTL: Duration = Duration::from_secs(30);

/// worktree 增删/清理之后调用:仓库集合已变,让所有项目的发现缓存立即失效,
/// 否则 History/Changes 面板要等 TTL 过期才能看到新条目。
pub fn invalidate_repo_cache() {
    REPO_PATH_CACHE.lock().clear();
}

fn find_repos_cached_paths(project_path: &Path) -> Vec<RepoPathEntry> {
    let key = project_path.to_path_buf();
    {
        let cache = REPO_PATH_CACHE.lock();
        if let Some((ts, entries)) = cache.get(&key) {
            if ts.elapsed() < REPO_CACHE_TTL {
                return entries.clone();
            }
        }
    }
    let entries = discover_repo_paths(project_path);
    REPO_PATH_CACHE
        .lock()
        .insert(key, (Instant::now(), entries.clone()));
    entries
}

fn discover_repo_paths(project_path: &Path) -> Vec<RepoPathEntry> {
    let mut entries = Vec::new();

    if let Some(repo) = discover_repo_limited(project_path) {
        if let Some(workdir) = repo.workdir() {
            let repo_root = workdir.to_path_buf();
            let name = repo_root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "root".to_string());
            // 每个项目只展示自己工作区的仓库,不再把关联 worktree 注入为独立条目——
            // worktree 通过「设为项目」拥有自己的 Git 面板,这里再列一遍就是重复。
            entries.push(RepoPathEntry {
                name,
                path: repo_root,
                is_worktree: repo.is_worktree(),
            });
            return entries;
        }
    }

    const MAX_DEPTH: u32 = 5;
    const SKIP_DIRS: &[&str] = &[
        ".git",
        "node_modules",
        "target",
        ".next",
        "dist",
        "__pycache__",
        ".superpowers",
    ];
    fn scan(dir: &Path, depth: u32, entries: &mut Vec<RepoPathEntry>) {
        if depth > MAX_DEPTH {
            return;
        }
        let dir_entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in dir_entries.flatten() {
            let sub = entry.path();
            if !sub.is_dir() {
                continue;
            }
            let dir_name = entry.file_name();
            let dir_name_str = dir_name.to_string_lossy();
            if SKIP_DIRS.contains(&dir_name_str.as_ref()) {
                continue;
            }
            if let Ok(repo) = Repository::open(&sub) {
                if let Some(workdir) = repo.workdir() {
                    if workdir.canonicalize().ok() == sub.canonicalize().ok() {
                        let name = sub
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        // 物理上在项目目录内的 worktree 才会走到这里(作为子目录仓库),
                        // 项目目录外的关联 worktree 不再注入
                        entries.push(RepoPathEntry {
                            name,
                            path: sub,
                            is_worktree: repo.is_worktree(),
                        });
                        continue;
                    }
                }
            }
            scan(&sub, depth + 1, entries);
        }
    }
    scan(project_path, 1, &mut entries);
    // `read_dir` 给的是文件系统顺序 —— Windows 的 NTFS 恰好按名字,但 ext4 是
    // 哈希序,同一个目录两次枚举都可能不一样,仓库下拉里的条目会莫名换位置。
    // 按路径排一遍:顺序确定,且同名仓库(monorepo 里的一堆 `api`)按父目录
    // 挨在一起,配上 UI 那行父级路径正好一眼对比。
    entries.sort_by_key(|e| e.path.to_string_lossy().to_lowercase());
    entries
}

/// 扫描 project_path 下的 git 仓库。
/// 只收集项目自身 / 子目录下物理可见的仓库;项目目录外的关联 worktree 不注入——
/// 它们经「设为项目」成为独立项目后,有自己的 History / Changes 面板。
fn find_repos(project_path: &Path) -> Vec<(String, PathBuf, Repository, bool)> {
    let cached_paths = find_repos_cached_paths(project_path);
    let mut repos = Vec::new();
    for entry in cached_paths {
        if let Ok(repo) = Repository::open(&entry.path) {
            repos.push((entry.name, entry.path, repo, entry.is_worktree));
        }
    }
    repos
}

pub fn get_git_status(project_path: &Path) -> Result<Vec<GitFileStatus>> {
    let repos = find_repos(project_path);

    if repos.is_empty() {
        return Ok(Vec::new());
    }

    let mut all = Vec::new();
    for (_, _, repo, _) in &repos {
        if let Ok(mut files) = collect_repo_status(repo, Some(project_path)) {
            all.append(&mut files);
        }
    }
    Ok(all)
}

pub fn get_changes_status(repo_path: &Path) -> Result<Vec<ChangeFileStatus>> {
    let repo = Repository::open(repo_path)?;
    let is_empty_repo = repo.head().is_err();

    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);

    let statuses = repo.statuses(Some(&mut opts))?;
    let mut result = Vec::new();

    for entry in statuses.iter() {
        let raw_path = entry.path().unwrap_or("").to_string();
        let s = entry.status();

        let staged = map_staged_status(s);
        let unstaged = map_unstaged_status(s, is_empty_repo);

        if staged.is_none() && unstaged.is_none() {
            continue;
        }

        let label = staged
            .as_ref()
            .or(unstaged.as_ref())
            .map(status_label)
            .unwrap_or("")
            .to_string();

        let old_path = if s.contains(Status::INDEX_RENAMED) || s.contains(Status::WT_RENAMED) {
            entry.head_to_index().and_then(|d| {
                d.old_file()
                    .path()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
            })
        } else {
            None
        };

        result.push(ChangeFileStatus {
            path: raw_path,
            old_path,
            staged_status: staged,
            unstaged_status: unstaged,
            status_label: label,
        });
    }

    Ok(result)
}

pub fn discover_git_repos(project_path: &Path) -> Result<Vec<GitRepoInfo>> {
    let repos = find_repos(project_path);
    Ok(repos
        .into_iter()
        .map(|(name, abs_path, repo, is_worktree)| {
            let current_branch = repo.head().ok().and_then(|h| {
                if h.is_branch() {
                    h.shorthand().map(|s| s.to_string())
                } else {
                    // detached HEAD — 显示短 hash
                    h.target().map(|oid| {
                        let s = oid.to_string();
                        format!("({})", &s[..7.min(s.len())])
                    })
                }
            });
            GitRepoInfo {
                name,
                path: abs_path,
                current_branch,
                is_worktree,
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// 提交历史分页
// ---------------------------------------------------------------------------
//
// # 为什么不用 libgit2 的排序 revwalk
//
// 旧实现是 `revwalk.set_sorting(TIME | TOPOLOGICAL)`。libgit2 只要设了排序就进
// limited 模式(`revwalk.c` 的 `git_revwalk_sorting`:`sorting != NONE` 即
// `limited = 1`),产出第一个提交之前 `prepare_walk` 先跑 `limit_list` 把全部可达
// 提交走一遍、再 `sort_in_topological_order` 整体排序 —— 每页都是 O(全历史)。
// 十万提交的仓库一页几百毫秒,终端里每 commit 一次还要自动重载一遍。
//
// 不设排序(NONE)时 libgit2 按「弹出一个、把它的父按提交时间插回待走链表」走,
// 是增量的,但只是**按时间**:子提交时间早于父(时钟偏移)或同一秒的并行分支,
// 都会让父排到子前面。
//
// 旧的分页也有洞:续页从「上一页最后一个提交的父」重起一个 revwalk,只沿那一条
// 父链往下走,页边界时别的分支上还没产出的提交从此再也不会出现(合并历史里必现,
// 3000 提交的基准仓库里有 108 个永远翻不到);反过来也会把已加载的提交再带回来
// (UI 那边「重复 hash 必须丢掉」的去重就是为这个加的)。
//
// # 现在的做法
//
// 自己走:**已发现子图上的 Kahn 拓扑序,就绪集合里按提交时间取最新的**,同一秒按
// 发现先后(规则同 `git log --date-order`)。发现是懒的 —— 产出一个提交时才读它的
// 父,所以一页的代价是 O(本页 + frontier),与总历史长度无关。
//
// 续页的状态不靠服务端缓存,而是由已加载列表**原样还原**:frontier = 已加载提交
// 的父 − 已加载提交(见 [`GitLogCursor`])。任何未加载的可达提交都是某个 frontier
// 提交的祖先(沿任意一条从起点下来的路径,第一个未加载的提交必然是某个已加载
// 提交的父),从 frontier 接着走、跳过已加载的,所以**跨页不漏、不重**。
//
// # 拓扑序:遍历只对「已发现的子」负责,其余由 [`merge_log_page`] 兜底
//
// 「父永远在子之后」要求产出父之前确认它**所有**子都已产出。已发现的子提交能确认
// (上面的 Kahn 就是干这个的);但时钟偏移下,子提交可能还埋在某个时间更早的
// frontier 提交的祖先里,尚未发现 —— 不走全历史(或没有 commit-graph 的代次号)
// 就无从得知。这是增量遍历的理论下限,git 自己在没有 commit-graph 时做
// `--topo-order` 也是退回全量遍历。
//
// 于是分两层:遍历按上面的规则出页(页内也可能出现这种迟到的子);UI 合并新页时由
// [`merge_log_page`] 检查「新提交的父是否已排在它前面」,有就对已加载列表做一次
// 稳定的拓扑修正,把迟到的子提交挪到它最早那个父的正前方(典型的「慢时钟分支」
// 场景下,这正是 `git log --date-order` 全量排序给它的位置)。没有时钟偏移的仓库
// 这一步只是检查,一行不挪。

/// 续页游标:由已加载的提交还原出来的遍历现场。
///
/// 不在服务端缓存 revwalk(那要管仓库变更后的失效、多个面板各自的游标),而是
/// 每次从已加载列表重算 —— O(已加载条数) 的哈希,远小于一次全历史遍历。
#[derive(Debug, Clone, Default)]
pub struct GitLogCursor {
    /// 已加载(已产出)的提交。续页再遇到一律跳过。
    loaded: HashSet<Oid>,
    /// 已加载提交的父里还没加载的那些,按第一次出现的先后 —— 与一口气连续遍历时
    /// 它们被发现的次序相同,同一秒的并列就按这个先后排。
    frontier: Vec<Oid>,
}

impl GitLogCursor {
    /// 按已加载列表(UI 手上那份,顺序即展示顺序)还原游标。
    pub fn from_loaded(loaded: &[GitCommitInfo]) -> Self {
        let set: HashSet<Oid> = loaded
            .iter()
            .filter_map(|c| Oid::from_str(&c.hash).ok())
            .collect();
        let mut queued = HashSet::new();
        let mut frontier = Vec::new();
        for commit in loaded {
            for parent in &commit.parent_hashes {
                if let Ok(oid) = Oid::from_str(parent)
                    && !set.contains(&oid)
                    && queued.insert(oid)
                {
                    frontier.push(oid);
                }
            }
        }
        Self {
            loaded: set,
            frontier,
        }
    }
}

/// 取一页提交历史。
///
/// - `after = None`:首页,从 `branch`(`None` = HEAD)起走;
/// - `after = Some(游标)`:续页,只由游标决定从哪接着走,`branch` 不再解析 ——
///   分支在两页之间被删 / 被移动都不影响翻页(要看新内容由调用方整体重载)。
///
/// 空仓库(HEAD 未出生)返回空列表而不是错误。
pub fn get_git_log(
    repo_path: &Path,
    after: Option<&GitLogCursor>,
    limit: Option<usize>,
    branch: Option<&str>,
) -> Result<Vec<GitCommitInfo>> {
    let repo = Repository::open(repo_path)?;
    let limit = limit.unwrap_or(30);
    let empty = GitLogCursor::default();
    let mut walk = DateTopoWalk::new(&repo, after.unwrap_or(&empty));

    if let Some(cursor) = after {
        for &oid in &cursor.frontier {
            walk.discover(oid);
        }
    } else {
        let tip = match branch {
            Some(b) => {
                // 先找本地 refs/heads/<b>,再找远程 refs/remotes/<b>
                // worktree 持有的分支也在 refs/heads/ 下(与主 repo 共享 refs 存储),天然支持
                let local_ref = format!("refs/heads/{}", b);
                let remote_ref = format!("refs/remotes/{}", b);
                let reference = repo
                    .find_reference(&local_ref)
                    .or_else(|_| repo.find_reference(&remote_ref))
                    .map_err(|_| anyhow!("未找到分支:{}", b))?;
                reference
                    .peel_to_commit()
                    .map_err(|_| anyhow!("分支 {} 无有效 target", b))?
                    .id()
            }
            None => match repo.head() {
                Ok(head) => head.peel_to_commit()?.id(),
                // 空仓库:HEAD 指向还没出生的分支,没有历史可列
                Err(e) if e.code() == git2::ErrorCode::UnbornBranch => return Ok(Vec::new()),
                Err(e) => return Err(e.into()),
            },
        };
        walk.discover(tip);
    }

    Ok(walk.take(limit))
}

/// 一次取页的遍历现场。见上面「现在的做法」。
struct DateTopoWalk<'r> {
    repo: &'r Repository,
    /// 上一批已加载的提交(续页时);首页是空集。
    loaded: &'r HashSet<Oid>,
    /// 本次发现过的提交(含读不到的),再遇到不重复入队。
    discovered: HashSet<Oid>,
    /// 已发现、未产出的提交,连同它的发现序号。
    pending: HashMap<Oid, (Commit<'r>, u64)>,
    /// 每个提交还有几个「已发现、未产出」的子提交;归零才轮得到它。
    waiting_children: HashMap<Oid, u32>,
    /// 就绪的提交:时间新的先出,同一秒按发现先后。可能有过期条目,弹出时复核。
    ready: BinaryHeap<(i64, Reverse<u64>, Oid)>,
    next_seq: u64,
}

impl<'r> DateTopoWalk<'r> {
    fn new(repo: &'r Repository, cursor: &'r GitLogCursor) -> Self {
        Self {
            repo,
            loaded: &cursor.loaded,
            discovered: HashSet::new(),
            pending: HashMap::new(),
            waiting_children: HashMap::new(),
            ready: BinaryHeap::new(),
            next_seq: 0,
        }
    }

    fn waiting(&self, oid: &Oid) -> u32 {
        self.waiting_children.get(oid).copied().unwrap_or(0)
    }

    /// 把一个提交纳入已发现子图。已加载 / 已发现过的直接跳过;读不到的
    /// (浅克隆边界、对象缺失)当它不存在,不让整页失败。
    fn discover(&mut self, oid: Oid) {
        if self.loaded.contains(&oid) || !self.discovered.insert(oid) {
            return;
        }
        let Ok(commit) = self.repo.find_commit(oid) else {
            return;
        };
        for parent in commit.parent_ids() {
            *self.waiting_children.entry(parent).or_default() += 1;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let time = commit.time().seconds();
        self.pending.insert(oid, (commit, seq));
        if self.waiting(&oid) == 0 {
            self.ready.push((time, Reverse(seq), oid));
        }
    }

    fn take(&mut self, limit: usize) -> Vec<GitCommitInfo> {
        let mut out = Vec::with_capacity(limit.min(256));
        while out.len() < limit {
            let Some((_, _, oid)) = self.ready.pop() else {
                break;
            };
            // 过期条目:已产出过(重复入堆),或入堆之后又发现了它的子提交(时钟偏移)
            if self.waiting(&oid) > 0 {
                continue;
            }
            let Some((commit, _)) = self.pending.remove(&oid) else {
                continue;
            };
            for parent in commit.parent_ids() {
                if let Some(n) = self.waiting_children.get_mut(&parent) {
                    *n = n.saturating_sub(1);
                }
                match self.pending.get(&parent) {
                    // 已发现的父:最后一个子也产出了才就绪
                    Some((parent_commit, seq)) => {
                        if self.waiting(&parent) == 0 {
                            let time = parent_commit.time().seconds();
                            self.ready.push((time, Reverse(*seq), parent));
                        }
                    }
                    None => self.discover(parent),
                }
            }
            out.push(commit_info(&commit));
        }
        out
    }
}

fn commit_info(commit: &Commit<'_>) -> GitCommitInfo {
    let hash = commit.id().to_string();
    let short_hash = hash[..7.min(hash.len())].to_string();
    GitCommitInfo {
        short_hash,
        message: commit.summary().unwrap_or("").to_string(),
        body: commit.body().map(|s| s.to_string()),
        author: commit.author().name().unwrap_or("unknown").to_string(),
        timestamp: commit.time().seconds(),
        parent_hashes: commit.parent_ids().map(|id| id.to_string()).collect(),
        hash,
    }
}

/// 把新取到的一页并进已加载列表,返回实际新增的条数。
///
/// 1. **去重**:按 hash 丢掉已有的(游标续页本身不会带回已加载的提交,这里是防线);
/// 2. **跨页拓扑修正**:新提交里若有「父已经排在它前面」的(时钟偏移下迟到的子
///    提交,见 `get_git_log` 上方的说明),对整个列表做一次稳定的拓扑重排,保证
///    父永远在子之后 —— 拓扑图的连线依赖这一点。没有这种情况时一行不挪。
pub fn merge_log_page(loaded: &mut Vec<GitCommitInfo>, page: Vec<GitCommitInfo>) -> usize {
    let before = loaded.len();
    let mut known: HashSet<String> = loaded.iter().map(|c| c.hash.clone()).collect();
    for commit in page {
        if known.insert(commit.hash.clone()) {
            loaded.push(commit);
        }
    }
    let added = loaded.len() - before;
    if added > 0 && !new_tail_is_topo_ordered(loaded, before) {
        stable_topo_reorder(loaded);
    }
    added
}

/// 新并入的 `loaded[from..]` 有没有「父排在自己前面」的。旧的部分此前已经是拓扑序,
/// 而新提交全排在旧提交之后,所以只可能是新提交的父在前 —— 只查它们就够。
fn new_tail_is_topo_ordered(loaded: &[GitCommitInfo], from: usize) -> bool {
    let pos: HashMap<&str, usize> = loaded
        .iter()
        .enumerate()
        .map(|(i, c)| (c.hash.as_str(), i))
        .collect();
    loaded[from..].iter().enumerate().all(|(k, commit)| {
        commit
            .parent_hashes
            .iter()
            .all(|p| pos.get(p.as_str()).is_none_or(|&pi| pi > from + k))
    })
}

/// 稳定的拓扑重排(子在父前)。
///
/// 每个提交的排序键 = 它自己与它全部已加载祖先里**最靠前的位置**:迟到的子提交
/// 因此被拉到它最早那个父的位置上,再用 Kahn(同键按原位置)排出一个合法拓扑序。
/// 原本就合法的部分键就是自己的位置,次序原样不动。
fn stable_topo_reorder(loaded: &mut Vec<GitCommitInfo>) {
    let n = loaded.len();
    let pos: HashMap<&str, usize> = loaded
        .iter()
        .enumerate()
        .map(|(i, c)| (c.hash.as_str(), i))
        .collect();
    let parents: Vec<Vec<usize>> = loaded
        .iter()
        .map(|c| {
            c.parent_hashes
                .iter()
                .filter_map(|p| pos.get(p.as_str()).copied())
                .collect()
        })
        .collect();

    // 子先父后的 Kahn:入度 = 已加载的子提交个数,就绪集合里按 (rank, 原位置) 取最小
    let kahn = |rank: &dyn Fn(usize) -> usize| -> Vec<usize> {
        let mut indegree = vec![0u32; n];
        for &p in parents.iter().flatten() {
            indegree[p] += 1;
        }
        let mut ready: BinaryHeap<Reverse<(usize, usize)>> = (0..n)
            .filter(|&i| indegree[i] == 0)
            .map(|i| Reverse((rank(i), i)))
            .collect();
        let mut order = Vec::with_capacity(n);
        while let Some(Reverse((_, i))) = ready.pop() {
            order.push(i);
            for &p in &parents[i] {
                indegree[p] -= 1;
                if indegree[p] == 0 {
                    ready.push(Reverse((rank(p), p)));
                }
            }
        }
        order
    };

    // 先按原位置求一个合法拓扑序,倒过来就是「父先于子」,正好用来自底向上算键
    let by_index = kahn(&|i| i);
    let mut key = vec![usize::MAX; n];
    for &i in by_index.iter().rev() {
        key[i] = parents[i].iter().map(|&p| key[p]).fold(i, usize::min);
    }
    let mut order = kahn(&|i| key[i]);
    if order.len() < n {
        // 提交图不会有环;真遇上坏数据也别把提交弄丢,排不进去的按原位置垫在后面
        let mut placed = vec![false; n];
        for &i in &order {
            placed[i] = true;
        }
        order.extend((0..n).filter(|&i| !placed[i]));
    }

    let mut slots: Vec<Option<GitCommitInfo>> =
        std::mem::take(loaded).into_iter().map(Some).collect();
    loaded.extend(order.into_iter().filter_map(|i| slots[i].take()));
}

pub fn get_repo_branches(repo_path: &Path) -> Result<Vec<BranchInfo>> {
    let repo = Repository::open(repo_path)?;

    let head_target = repo.head().ok().and_then(|h| h.target());

    let mut branches = Vec::new();

    // 本地分支
    for branch_result in repo.branches(Some(git2::BranchType::Local))? {
        let (branch, _) = branch_result?;
        let name = branch.name()?.unwrap_or("").to_string();
        if let Some(target) = branch.get().target() {
            // 上游:`branch.<name>.remote/merge` 经 fetch refspec 换算成远程跟踪引用;
            // 没配或引用不在都是 Err,一律当没有上游
            let upstream = branch
                .upstream()
                .ok()
                .and_then(|u| u.name().ok().flatten().map(str::to_string));
            branches.push(BranchInfo {
                name,
                is_head: head_target == Some(target),
                is_remote: false,
                commit_hash: target.to_string(),
                upstream,
            });
        }
    }

    // 远程分支
    for branch_result in repo.branches(Some(git2::BranchType::Remote))? {
        let (branch, _) = branch_result?;
        let name = branch.name()?.unwrap_or("").to_string();
        // 跳过 origin/HEAD 这类指针
        if name.ends_with("/HEAD") {
            continue;
        }
        if let Some(target) = branch.get().target() {
            branches.push(BranchInfo {
                name,
                is_head: false,
                is_remote: true,
                commit_hash: target.to_string(),
                upstream: None,
            });
        }
    }

    Ok(branches)
}

pub fn get_commit_files(repo_path: &Path, commit_hash: &str) -> Result<Vec<CommitFileInfo>> {
    let repo = Repository::open(repo_path)?;
    let oid = git2::Oid::from_str(commit_hash)?;
    let commit = repo.find_commit(oid)?;
    let tree = commit.tree()?;

    let parent_tree = if commit.parent_count() > 0 {
        Some(commit.parent(0)?.tree()?)
    } else {
        None
    };

    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;

    let mut files = Vec::new();
    for delta in diff.deltas() {
        let status = match delta.status() {
            git2::Delta::Added => "added",
            git2::Delta::Deleted => "deleted",
            git2::Delta::Modified => "modified",
            git2::Delta::Renamed => "renamed",
            _ => "modified",
        };
        let path = delta
            .new_file()
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let old_path = if delta.status() == git2::Delta::Renamed {
            delta
                .old_file()
                .path()
                .map(|p| p.to_string_lossy().to_string())
        } else {
            None
        };
        files.push(CommitFileInfo {
            path,
            status: status.to_string(),
            old_path,
        });
    }
    Ok(files)
}

pub fn get_commit_file_diff(
    repo_path: &Path,
    commit_hash: &str,
    file_path: &str,
    old_file_path: Option<&str>,
) -> Result<GitDiffResult> {
    let repo = Repository::open(repo_path)?;
    let oid = git2::Oid::from_str(commit_hash)?;
    let commit = repo.find_commit(oid)?;
    let tree = commit.tree()?;

    let parent_tree = if commit.parent_count() > 0 {
        Some(commit.parent(0)?.tree()?)
    } else {
        None
    };

    let new_content = match tree.get_path(Path::new(file_path)) {
        Ok(entry) => {
            let obj = entry.to_object(&repo)?;
            let blob = obj.as_blob().ok_or_else(|| anyhow!("not a blob"))?;
            if blob.is_binary() {
                return Ok(binary_diff_result());
            }
            if blob.content().len() > MAX_DIFF_BYTES {
                return Ok(too_large_diff_result());
            }
            std::str::from_utf8(blob.content())
                .map_err(|_| anyhow!("binary"))?
                .to_string()
        }
        Err(_) => String::new(),
    };

    let old_lookup_path = old_file_path.unwrap_or(file_path);
    let old_content = if let Some(ref pt) = parent_tree {
        match pt.get_path(Path::new(old_lookup_path)) {
            Ok(entry) => {
                let obj = entry.to_object(&repo)?;
                let blob = obj.as_blob().ok_or_else(|| anyhow!("not a blob"))?;
                if blob.is_binary() {
                    return Ok(binary_diff_result());
                }
                std::str::from_utf8(blob.content())
                    .map_err(|_| anyhow!("binary"))?
                    .to_string()
            }
            Err(_) => String::new(),
        }
    } else {
        String::new()
    };

    Ok(diff_two_texts(old_content, new_content))
}

// ---------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------

/// 内置 diff 视图的上限,与 fs 侧的文件查看上限同为 1MB。
const MAX_DIFF_BYTES: usize = 1_048_576;

/// LCS 的 O(m·n) DP 表在超大文件上会吃爆内存,越过这条线就退化成「整块替换」。
///
/// ⚠️ 这条线量的是**剥掉公共前后缀之后**的中段(见 [`lcs_workload`])。按整文件
/// 量的话,一个 4000 行的文件只改一行也会越线 —— 退化出来的「整块替换」把全文
/// 标成一删一增,等于没 diff。
const MAX_LCS_CELLS: u64 = 10_000_000;

fn binary_diff_result() -> GitDiffResult {
    GitDiffResult {
        old_content: String::new(),
        new_content: String::new(),
        hunks: Vec::new(),
        is_binary: true,
        too_large: false,
    }
}

fn too_large_diff_result() -> GitDiffResult {
    GitDiffResult {
        old_content: String::new(),
        new_content: String::new(),
        hunks: Vec::new(),
        is_binary: false,
        too_large: true,
    }
}

/// 两段文本 → 完整 diff 结果(超大文件自动退化为整块替换)。
fn diff_two_texts(old_content: String, new_content: String) -> GitDiffResult {
    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();

    let hunks = if lcs_workload(&old_lines, &new_lines) > MAX_LCS_CELLS {
        full_replace_diff(&old_content, &new_content)
    } else {
        build_hunks(&old_lines, &new_lines)
    };

    GitDiffResult {
        old_content,
        new_content,
        hunks,
        is_binary: false,
        too_large: false,
    }
}

fn get_head_content(repo: &Repository, rel_path: &str) -> Result<Option<String>> {
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok(None), // 空仓库
    };
    let tree = head.peel_to_tree()?;
    let entry = match tree.get_path(Path::new(rel_path)) {
        Ok(e) => e,
        Err(_) => return Ok(Some(String::new())), // 文件还没进 HEAD
    };
    let obj = entry.to_object(repo)?;
    let blob = obj.as_blob().ok_or_else(|| anyhow!("not a blob"))?;

    if blob.is_binary() {
        bail!("binary");
    }
    let content = std::str::from_utf8(blob.content())
        .map_err(|_| anyhow!("binary"))?
        .to_string();
    Ok(Some(content))
}

/// 两段行序列的公共前缀 / 后缀行数(前缀优先,后缀在剩下的范围里数)。
fn common_affix(old_lines: &[&str], new_lines: &[&str]) -> (usize, usize) {
    let max = old_lines.len().min(new_lines.len());
    let mut head = 0;
    while head < max && old_lines[head] == new_lines[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < max - head
        && old_lines[old_lines.len() - 1 - tail] == new_lines[new_lines.len() - 1 - tail]
    {
        tail += 1;
    }
    (head, tail)
}

/// 剥掉公共前后缀之后,LCS 还要开多大的 DP 表(格子数)。
///
/// 「大文件」与「大改动」不是一回事:1 万行的文件改一行,中段只剩几行,
/// DP 表小到可以忽略;整份重写才是真的要退化。
fn lcs_workload(old_lines: &[&str], new_lines: &[&str]) -> u64 {
    let (head, tail) = common_affix(old_lines, new_lines);
    (old_lines.len() - head - tail) as u64 * (new_lines.len() - head - tail) as u64
}

/// 编辑序列: `('=', old_i, new_j)` | `('-', old_i, _)` | `('+', _, new_j)`。
///
/// 两头对得上的行直接抄成 `=`,只有中段跑 LCS —— DP 表按 [`lcs_workload`] 算。
fn edit_script(old_lines: &[&str], new_lines: &[&str]) -> Vec<(char, usize, usize)> {
    let (head, tail) = common_affix(old_lines, new_lines);
    let mut flat: Vec<(char, usize, usize)> = (0..head).map(|i| ('=', i, i)).collect();

    let a = &old_lines[head..old_lines.len() - tail];
    let b = &new_lines[head..new_lines.len() - tail];
    let (m, n) = (a.len(), b.len());

    // LCS DP table
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            if a[i] == b[j] {
                dp[i][j] = dp[i + 1][j + 1] + 1;
            } else {
                dp[i][j] = dp[i + 1][j].max(dp[i][j + 1]);
            }
        }
    }

    // 回溯。下标要加回 head —— 外面拿它当行号用
    //
    // ⚠️ 平局时**先走删除**(`>` 而不是 `>=`)。换行改动的 LCS 是平的,
    // 取 `>=` 会先吐 `+` 再吐 `-` —— 一行改写显示成「先增后删」,与 git 的习惯
    // 相反,并排视图的配对(`mt-app::git_diff::pair_rows` 只认「delete 段紧跟
    // add 段」)也因此永远对不上,左右两侧各占一行错开显示。
    let mut i = 0;
    let mut j = 0;
    while i < m || j < n {
        if i < m && j < n && a[i] == b[j] {
            flat.push(('=', head + i, head + j));
            i += 1;
            j += 1;
        } else if j < n && (i >= m || dp[i][j + 1] > dp[i + 1][j]) {
            flat.push(('+', head + i, head + j));
            j += 1;
        } else {
            flat.push(('-', head + i, head + j));
            i += 1;
        }
    }

    for k in 0..tail {
        flat.push((
            '=',
            old_lines.len() - tail + k,
            new_lines.len() - tail + k,
        ));
    }
    flat
}

/// 基于 LCS 的行 diff,上下文 3 行。
fn build_hunks(old_lines: &[&str], new_lines: &[&str]) -> Vec<DiffHunk> {
    let flat = edit_script(old_lines, new_lines);

    // 按上下文 3 行分组成 hunk
    const CONTEXT: usize = 3;
    let mut hunks: Vec<DiffHunk> = Vec::new();

    let changed_indices: Vec<usize> = flat
        .iter()
        .enumerate()
        .filter(|(_, (k, _, _))| *k != '=')
        .map(|(idx, _)| idx)
        .collect();

    if changed_indices.is_empty() {
        return hunks;
    }

    // 把变更下标按「带上下文后是否相接」合并成区间
    let mut groups: Vec<(usize, usize)> = Vec::new(); // (start, end) in flat[]
    let start = changed_indices[0].saturating_sub(CONTEXT);
    let end = (changed_indices[0] + CONTEXT + 1).min(flat.len());
    groups.push((start, end));

    for &idx in &changed_indices[1..] {
        let last = groups.last_mut().unwrap();
        let expanded_start = idx.saturating_sub(CONTEXT);
        let expanded_end = (idx + CONTEXT + 1).min(flat.len());
        if expanded_start <= last.1 {
            last.1 = last.1.max(expanded_end);
        } else {
            groups.push((expanded_start, expanded_end));
        }
    }

    for (grp_start, grp_end) in groups {
        let slice = &flat[grp_start..grp_end];
        let mut lines_out: Vec<DiffLine> = Vec::new();
        let mut old_start = 0u32;
        let mut new_start = 0u32;
        let mut old_count = 0u32;
        let mut new_count = 0u32;
        let mut first = true;

        for (k, oi, ni) in slice {
            let old_lineno = (*oi as u32) + 1;
            let new_lineno = (*ni as u32) + 1;
            match k {
                '=' => {
                    if first {
                        old_start = old_lineno;
                        new_start = new_lineno;
                        first = false;
                    }
                    lines_out.push(DiffLine {
                        kind: "context".to_string(),
                        content: old_lines[*oi].to_string(),
                        old_lineno: Some(old_lineno),
                        new_lineno: Some(new_lineno),
                    });
                    old_count += 1;
                    new_count += 1;
                }
                '-' => {
                    if first {
                        old_start = old_lineno;
                        // new_start 可能是紧随其后的插入位置,这里取近似值
                        new_start = (*ni as u32) + 1;
                        first = false;
                    }
                    lines_out.push(DiffLine {
                        kind: "delete".to_string(),
                        content: old_lines[*oi].to_string(),
                        old_lineno: Some(old_lineno),
                        new_lineno: None,
                    });
                    old_count += 1;
                }
                '+' => {
                    if first {
                        old_start = (*oi as u32) + 1;
                        new_start = new_lineno;
                        first = false;
                    }
                    lines_out.push(DiffLine {
                        kind: "add".to_string(),
                        content: new_lines[*ni].to_string(),
                        old_lineno: None,
                        new_lineno: Some(new_lineno),
                    });
                    new_count += 1;
                }
                _ => {}
            }
        }

        hunks.push(DiffHunk {
            old_start,
            old_lines: old_count,
            new_start,
            new_lines: new_count,
            lines: lines_out,
        });
    }

    hunks
}

fn full_replace_diff(old_content: &str, new_content: &str) -> Vec<DiffHunk> {
    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();
    let mut lines_out: Vec<DiffLine> = Vec::new();

    for (i, l) in old_lines.iter().enumerate() {
        lines_out.push(DiffLine {
            kind: "delete".to_string(),
            content: l.to_string(),
            old_lineno: Some((i as u32) + 1),
            new_lineno: None,
        });
    }
    for (i, l) in new_lines.iter().enumerate() {
        lines_out.push(DiffLine {
            kind: "add".to_string(),
            content: l.to_string(),
            old_lineno: None,
            new_lineno: Some((i as u32) + 1),
        });
    }

    if lines_out.is_empty() {
        return Vec::new();
    }

    vec![DiffHunk {
        old_start: 1,
        old_lines: old_lines.len() as u32,
        new_start: 1,
        new_lines: new_lines.len() as u32,
        lines: lines_out,
    }]
}

/// 工作区/暂存区文件相对 HEAD 的 diff。`file_path` 是相对 `project_path` 的路径。
pub fn get_git_diff(
    project_path: &Path,
    file_path: &str,
    staged: Option<bool>,
) -> Result<GitDiffResult> {
    let abs_file = project_path.join(file_path);

    let repo = discover_repo_for(project_path, &abs_file).ok_or_else(|| {
        anyhow!(
            "no git repository found for {} (searched up to {} parents above {})",
            abs_file.display(),
            MAX_DISCOVER_PARENTS,
            project_path.display()
        )
    })?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow!("bare repository not supported"))?;

    let rel_path = diff_paths(&abs_file, workdir)
        .ok_or_else(|| anyhow!("file is outside repository working directory"))?;
    let rel_str = rel_path.to_string_lossy().replace('\\', "/");

    let is_staged = staged.unwrap_or(false);

    // 新内容:暂存区取 index,否则取工作区
    let new_content = if is_staged {
        let index = repo.index()?;
        match index.get_path(Path::new(&rel_str), 0) {
            Some(entry) => {
                let blob = repo.find_blob(entry.id)?;
                if blob.is_binary() {
                    return Ok(binary_diff_result());
                }
                if blob.content().len() > MAX_DIFF_BYTES {
                    return Ok(too_large_diff_result());
                }
                std::str::from_utf8(blob.content())
                    .map_err(|_| anyhow!("binary"))?
                    .to_string()
            }
            None => String::new(),
        }
    } else {
        let new_bytes = std::fs::read(&abs_file)?;
        if new_bytes.len() > MAX_DIFF_BYTES {
            return Ok(too_large_diff_result());
        }
        match std::str::from_utf8(&new_bytes) {
            Ok(s) => s.to_string(),
            Err(_) => return Ok(binary_diff_result()),
        }
    };

    let old_content = get_head_content(&repo, &rel_str)?.unwrap_or_default();

    Ok(diff_two_texts(old_content, new_content))
}

// ---------------------------------------------------------------------------
// git CLI(网络与 worktree)
// ---------------------------------------------------------------------------

/// 在 Windows GUI 应用下 spawn console 子进程(比如 git.exe)默认会弹出 conhost
/// 黑框,并且窗口创建/焦点切换会让 UI 感知卡顿。这里统一给 `Command` 加
/// CREATE_NO_WINDOW 抑制掉控制台分配。
fn hide_console_window(_cmd: &mut std::process::Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        _cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

/// 通用 git CLI 执行器(pull/push/worktree 系列共用):
/// - 校验 `repo_path` 是目录并且包含 `.git`(避免在任意目录上跑 git)
/// - 在独立线程里 spawn git 进程,通过 mpsc 回传 output
/// - `recv_timeout` 到达上限后立即返回超时错误(子进程会被 drop,
///   虽然不保证立刻 kill,但调用线程不再被阻塞)
///
/// **会阻塞当前线程最多 `timeout`**,调用方自行放到后台执行器上跑。
fn run_git_command(
    repo_path: &Path,
    args: &[&str],
    timeout: Duration,
    timeout_hint: &str,
) -> Result<String> {
    let op = args.join(" ");
    run_git_command_labeled(repo_path, args, &op, timeout, timeout_hint)
}

/// 同 `run_git_command`,但错误信息里的操作名由 `op_label` 指定而非拼 `args`。
///
/// 给 `git commit -m <message>` 这类**参数里带用户文本**的调用用:直接 join 会把
/// 整条提交信息(可能几十行)灌进「启动失败 / 超时」的错误里。
fn run_git_command_labeled(
    repo_path: &Path,
    args: &[&str],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<String> {
    if !repo_path.is_dir() {
        bail!("不是有效目录:{}", repo_path.display());
    }
    // worktree 目录下 `.git` 是文件而非目录,exists() 两者皆真
    if !repo_path.join(".git").exists() {
        bail!("不是 git 仓库(缺少 .git):{}", repo_path.display());
    }

    let op = op_label.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    let repo_path_owned = repo_path.to_path_buf();
    let args_owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    std::thread::spawn(move || {
        let mut cmd = std::process::Command::new("git");
        cmd.args(&args_owned)
            .current_dir(&repo_path_owned)
            .stdin(std::process::Stdio::null());
        hide_console_window(&mut cmd);
        let result = cmd.output();
        // 忽略发送失败:调用方超时后接收端已被 drop
        let _ = tx.send(result);
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => {
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).to_string())
            } else {
                bail!("{}", String::from_utf8_lossy(&output.stderr));
            }
        }
        Ok(Err(e)) => bail!("启动 git {} 失败:{}", op, e),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            bail!("git {} 超时({}s){}", op, timeout.as_secs(), timeout_hint)
        }
        Err(e) => bail!("git {} 通信错误:{}", op, e),
    }
}

fn run_git_network_command(repo_path: &Path, op: &'static str) -> Result<String> {
    run_git_command(
        repo_path,
        &[op],
        Duration::from_secs(30),
        ",可能在等待凭证或网络故障。请确认已配置凭证管理器或 SSH key",
    )
}

/// **阻塞最多 30s**,必须在后台线程上调用。
pub fn git_pull(repo_path: &Path) -> Result<String> {
    run_git_network_command(repo_path, "pull")
}

/// **阻塞最多 30s**,必须在后台线程上调用。
pub fn git_push(repo_path: &Path) -> Result<String> {
    run_git_network_command(repo_path, "push")
}

pub fn git_stage(repo_path: &Path, files: &[String]) -> Result<()> {
    let repo = Repository::open(repo_path)?;
    let mut index = repo.index()?;
    for file in files {
        let path = Path::new(file);
        let abs_path = repo
            .workdir()
            .ok_or_else(|| anyhow!("bare repo"))?
            .join(path);
        if abs_path.exists() {
            index.add_path(path)?;
        } else {
            // 文件已删除，需要从 index 移除
            index.remove_path(path)?;
        }
    }
    index.write()?;
    Ok(())
}

pub fn git_unstage(repo_path: &Path, files: &[String]) -> Result<()> {
    let repo = Repository::open(repo_path)?;

    let head = match repo.head() {
        Ok(h) => Some(h.peel_to_commit()?),
        Err(_) => None, // 空仓库,没有 HEAD
    };

    if let Some(ref commit) = head {
        for file in files {
            repo.reset_default(Some(commit.as_object()), [file.as_str()])?;
        }
    } else {
        // 空仓库:批量从 index 移除，最后一次 write
        let mut index = repo.index()?;
        for file in files {
            index.remove_path(Path::new(file))?;
        }
        index.write()?;
    }
    Ok(())
}

pub fn git_stage_all(repo_path: &Path) -> Result<()> {
    let repo = Repository::open(repo_path)?;
    let mut index = repo.index()?;
    index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)?;

    // 处理已删除的文件：遍历 index，移除工作区中不存在的文件
    let workdir = repo.workdir().ok_or_else(|| anyhow!("bare repo"))?;
    let entries: Vec<String> = index
        .iter()
        .filter_map(|e| {
            let path = String::from_utf8_lossy(&e.path).to_string();
            if !workdir.join(&path).exists() {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    for path in entries {
        index.remove_path(Path::new(&path))?;
    }

    index.write()?;
    Ok(())
}

pub fn git_unstage_all(repo_path: &Path) -> Result<()> {
    let repo = Repository::open(repo_path)?;

    match repo.head() {
        Ok(head) => {
            let commit = head.peel_to_commit()?;
            repo.reset(commit.as_object(), git2::ResetType::Mixed, None)?;
        }
        Err(_) => {
            // 空仓库:清空整个 index
            let mut index = repo.index()?;
            index.clear()?;
            index.write()?;
        }
    }
    Ok(())
}

/// 走 git CLI 而非 git2:提交要继承用户的 hooks / gpg 签名 / user.name 配置。
///
/// **阻塞最多 60s**,必须在后台线程上调用。取值取 pull/push(30s)与 worktree add
/// (120s)的中段:提交本身是本地操作、毫秒级,但会同步跑 pre-commit hook(格式化 /
/// lint / 测试),30s 对大仓库的 hook 偏紧;真卡死的形态是 hook 死循环或 gpg 等口令
/// 输入(stdin 已置 null,不会永久挂起但仍可能拖很久),给 60s 兜底足够。
pub fn git_commit(repo_path: &Path, message: &str) -> Result<String> {
    run_git_command_labeled(
        repo_path,
        &["commit", "-m", message],
        "commit",
        Duration::from_secs(60),
        ",可能卡在 pre-commit hook 或 gpg 签名上",
    )
}

pub fn git_discard_file(repo_path: &Path, files: &[String]) -> Result<()> {
    let repo = Repository::open(repo_path)?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow!("bare repo"))?
        .to_path_buf();

    for file in files {
        let abs_path = workdir.join(file);

        // 检查是否 untracked (WT_NEW)
        // 注意:StatusOptions::new() 默认不含未跟踪文件,必须显式开 include_untracked,
        // 否则新增文件永远查不到 WT_NEW,会被误当作已跟踪文件走 checkout_head(对其无效),
        // 表现为「丢弃新增文件没有反应」。
        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .pathspec(file);
        let statuses = repo.statuses(Some(&mut opts))?;
        let is_untracked = statuses.iter().any(|e| e.status().contains(Status::WT_NEW));

        if is_untracked {
            // untracked: 直接删除文件
            if abs_path.exists() {
                std::fs::remove_file(&abs_path)?;
            }
        } else {
            // tracked: 先 unstage（如果在暂存区），再 checkout HEAD 版本
            let head = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
            if let Some(ref commit) = head {
                // unstage
                let _ = repo.reset_default(Some(commit.as_object()), [file.as_str()]);
            }
            // checkout from HEAD
            repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force().path(file)))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Worktree 管理
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeInfo {
    pub name: String,
    pub path: String,
    /// HEAD 所在分支;detached / 失效条目为 None
    pub branch: Option<String>,
    pub is_main: bool,
    /// 目录还在且元数据能通过校验;false = 可被 prune 的失效条目
    pub is_valid: bool,
    pub is_locked: bool,
}

/// 去掉路径尾部分隔符:git2 的 workdir() 带尾杠,而项目配置里的路径不带,
/// 统一后才能做「该 worktree 是否已是项目」的对比。
fn display_path(p: &Path) -> String {
    mt_core::path_key::trim_trailing_separators(&p.to_string_lossy()).to_string()
}

fn head_branch(repo: &Repository) -> Option<String> {
    repo.head().ok().and_then(|h| {
        if h.is_branch() {
            h.shorthand().map(|s| s.to_string())
        } else {
            None
        }
    })
}

/// 列出某仓库的主工作区 + 全部 linked worktree(含失效条目,供管理面板展示与清理)。
/// 从 worktree 路径调用同样可行:元数据都在主仓库 .git/worktrees 下。
pub fn list_worktrees(repo_path: &Path) -> Result<Vec<WorktreeInfo>> {
    let repo = Repository::open(repo_path)?;
    // 从 linked worktree 打开时回到主仓库:linked worktree 的 gitdir 形如
    // `<main>/.git/worktrees/<name>`,上溯两级即主仓库 .git(git2 0.19 未暴露 commondir)
    let main_repo = if repo.is_worktree() {
        let git_dir = repo.path().to_path_buf();
        let main_git = git_dir
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow!("无法定位主仓库"))?;
        Repository::open(main_git)?
    } else {
        repo
    };

    let mut out = Vec::new();
    if let Some(workdir) = main_repo.workdir() {
        let name = workdir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "main".to_string());
        out.push(WorktreeInfo {
            name,
            path: display_path(workdir),
            branch: head_branch(&main_repo),
            is_main: true,
            is_valid: true,
            is_locked: false,
        });
    }

    if let Ok(names) = main_repo.worktrees() {
        for wt_name in names.iter().flatten() {
            let wt = match main_repo.find_worktree(wt_name) {
                Ok(w) => w,
                Err(_) => continue,
            };
            let wt_path = wt.path().to_path_buf();
            let is_valid = wt_path.exists() && wt.validate().is_ok();
            let is_locked = matches!(wt.is_locked(), Ok(git2::WorktreeLockStatus::Locked(_)));
            let branch = if is_valid {
                Repository::open_from_worktree(&wt)
                    .ok()
                    .and_then(|r| head_branch(&r))
            } else {
                None
            };
            out.push(WorktreeInfo {
                name: wt_name.to_string(),
                path: display_path(&wt_path),
                branch,
                is_main: false,
                is_valid,
                is_locked,
            });
        }
    }

    Ok(out)
}

/// 新建 worktree。`create_branch=true` 时以 `base`(缺省 HEAD)为起点建新分支,
/// 否则检出已有分支(该分支不能已被其他工作区持有,git 会给出明确报错)。
/// 大仓库的首次 checkout 可能较慢,超时给到 120s ——**阻塞调用线程**。
pub fn add_worktree(
    repo_path: &Path,
    worktree_path: &str,
    branch: &str,
    create_branch: bool,
    base: Option<&str>,
) -> Result<String> {
    let mut args: Vec<&str> = vec!["worktree", "add"];
    if create_branch {
        args.push("-b");
        args.push(branch);
        args.push(worktree_path);
        if let Some(b) = base {
            if !b.is_empty() {
                args.push(b);
            }
        }
    } else {
        args.push(worktree_path);
        args.push(branch);
    }
    let result = run_git_command(
        repo_path,
        &args,
        Duration::from_secs(120),
        ",大仓库 checkout 可能较慢,请稍后刷新查看",
    );
    if result.is_ok() {
        invalidate_repo_cache();
    }
    result
}

/// 删除 worktree(工作目录 + 主仓库里的元数据)。
/// 有未提交改动 / 已锁定时 git 会拒绝,`force=true` 对应 `--force` 强制删除。
pub fn remove_worktree(repo_path: &Path, worktree_path: &str, force: bool) -> Result<String> {
    let mut args: Vec<&str> = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(worktree_path);
    let result = run_git_command(repo_path, &args, Duration::from_secs(60), "");
    if result.is_ok() {
        invalidate_repo_cache();
    }
    result
}

/// 清理失效的 worktree 元数据(目录已被手动删除的条目)。
pub fn prune_worktrees(repo_path: &Path) -> Result<String> {
    let result = run_git_command(
        repo_path,
        &["worktree", "prune"],
        Duration::from_secs(30),
        "",
    );
    if result.is_ok() {
        invalidate_repo_cache();
    }
    result
}

/// 批量判断路径是否 linked worktree,是则返回其分支名(项目列表 ⎇ 徽章用)。
/// UNC 路径(WSL 项目)直接跳过:git2 对网络路径的探测慢且徽章意义不大。
pub fn get_worktree_branches(paths: &[PathBuf]) -> Vec<Option<String>> {
    paths
        .iter()
        .map(|p| {
            // 必须按字符串判 UNC:Path::starts_with 是按「路径分量」比的,
            // 传 `\\` 永远不会命中。
            if p.to_string_lossy().starts_with(r"\\") {
                return None;
            }
            let repo = Repository::open(p).ok()?;
            if !repo.is_worktree() {
                return None;
            }
            head_branch(&repo)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_labels_are_single_letters() {
        assert_eq!(status_label(&GitStatus::Modified), "M");
        assert_eq!(status_label(&GitStatus::Added), "A");
        assert_eq!(status_label(&GitStatus::Deleted), "D");
        assert_eq!(status_label(&GitStatus::Renamed), "R");
        assert_eq!(status_label(&GitStatus::Untracked), "?");
        assert_eq!(status_label(&GitStatus::Conflicted), "C");
    }

    #[test]
    fn map_status_prefers_conflict_then_rename() {
        assert_eq!(
            map_status(Status::CONFLICTED | Status::WT_MODIFIED, false),
            Some(GitStatus::Conflicted)
        );
        assert_eq!(
            map_status(Status::INDEX_RENAMED | Status::INDEX_MODIFIED, false),
            Some(GitStatus::Renamed)
        );
        assert_eq!(map_status(Status::empty(), false), None);
    }

    /// 空仓库里的新文件算 Added 而非 Untracked —— 首次提交面板才不会全是 `?`
    #[test]
    fn map_status_new_file_depends_on_empty_repo() {
        assert_eq!(
            map_status(Status::WT_NEW, false),
            Some(GitStatus::Untracked)
        );
        assert_eq!(map_status(Status::WT_NEW, true), Some(GitStatus::Added));
        assert_eq!(
            map_unstaged_status(Status::WT_NEW, true),
            Some(GitStatus::Added)
        );
    }

    /// 暂存/未暂存两侧各看各的位:同一文件可以同时有两种状态
    #[test]
    fn staged_and_unstaged_split() {
        let s = Status::INDEX_MODIFIED | Status::WT_DELETED;
        assert_eq!(map_staged_status(s), Some(GitStatus::Modified));
        assert_eq!(map_unstaged_status(s, false), Some(GitStatus::Deleted));
        assert_eq!(map_staged_status(Status::WT_MODIFIED), None);
    }

    #[test]
    fn build_hunks_no_change_yields_nothing() {
        assert!(build_hunks(&["a", "b"], &["a", "b"]).is_empty());
    }

    #[test]
    fn build_hunks_single_line_replacement() {
        let hunks = build_hunks(&["a", "b", "c"], &["a", "x", "c"]);
        assert_eq!(hunks.len(), 1, "相邻改动应合并成一个 hunk");
        let kinds: Vec<&str> = hunks[0].lines.iter().map(|l| l.kind.as_str()).collect();
        assert!(kinds.contains(&"delete") && kinds.contains(&"add"));
        // 上下文行(a / c)也在 hunk 里
        assert_eq!(kinds.iter().filter(|k| **k == "context").count(), 2);
        let deleted: Vec<&str> = hunks[0]
            .lines
            .iter()
            .filter(|l| l.kind == "delete")
            .map(|l| l.content.as_str())
            .collect();
        assert_eq!(deleted, vec!["b"]);
    }

    #[test]
    fn full_replace_diff_lists_all_lines() {
        let hunks = full_replace_diff("a\nb\n", "x\n");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_lines, 2);
        assert_eq!(hunks[0].new_lines, 1);
        assert!(full_replace_diff("", "").is_empty());
    }

    #[test]
    fn diff_two_texts_falls_back_to_full_replace_on_huge_input() {
        // 越过 LCS 上限时不应该去开 O(m·n) 的 DP 表
        let huge_old = "x\n".repeat(4000);
        let huge_new = "y\n".repeat(4000);
        let result = diff_two_texts(huge_old, huge_new);
        assert_eq!(result.hunks.len(), 1);
        assert_eq!(result.hunks[0].old_start, 1);
        assert_eq!(result.hunks[0].old_lines, 4000);
    }

    #[test]
    fn common_affix_counts_both_ends() {
        assert_eq!(common_affix(&["a", "b", "c"], &["a", "x", "c"]), (1, 1));
        // 前缀吃完就没有后缀可数(不能把同一行数两遍)
        assert_eq!(common_affix(&["a", "b"], &["a", "b", "c"]), (2, 0));
        assert_eq!(common_affix(&["a"], &["b"]), (0, 0));
        assert_eq!(common_affix(&[], &["a"]), (0, 0));
    }

    /// **大文件 ≠ 大改动**:上万行的文件只改一行,剥完公共前后缀中段只剩一两行,
    /// 不该被 `MAX_LCS_CELLS` 判去「整块替换」(那等于没 diff)。
    #[test]
    fn huge_file_with_one_changed_line_still_gets_a_real_diff() {
        let mut old: Vec<String> = (0..5000).map(|i| format!("line {i}")).collect();
        let mut new = old.clone();
        new[2500] = "line 2500 changed".to_string();
        old.push(String::new());
        new.push(String::new());

        let result = diff_two_texts(old.join("\n"), new.join("\n"));
        assert_eq!(result.hunks.len(), 1, "只改一行应该只出一个 hunk");
        let hunk = &result.hunks[0];
        // 一删一增 + 上下 3 行上下文
        assert_eq!(hunk.lines.iter().filter(|l| l.kind == "delete").count(), 1);
        assert_eq!(hunk.lines.iter().filter(|l| l.kind == "add").count(), 1);
        assert_eq!(hunk.lines.iter().filter(|l| l.kind == "context").count(), 6);
        assert_eq!(hunk.old_start, 2498);
    }

    /// 一行改写要吐成「先删后增」——并排视图靠这个顺序把两行配成一行(顺序反了
    /// 左右就各占一行错开),而且与 git 的习惯一致。
    #[test]
    fn replacement_emits_delete_before_add() {
        let hunks = build_hunks(&["a", "b", "c"], &["a", "x", "c"]);
        let kinds: Vec<&str> = hunks[0].lines.iter().map(|l| l.kind.as_str()).collect();
        assert_eq!(kinds, vec!["context", "delete", "add", "context"]);
    }

    /// 剥前后缀不能把行号剥歪:`=` 行的下标必须还是原文里的下标。
    #[test]
    fn edit_script_keeps_absolute_line_indices() {
        let flat = edit_script(&["a", "b", "c", "d"], &["a", "x", "c", "d"]);
        assert_eq!(
            flat,
            vec![
                ('=', 0, 0),
                ('-', 1, 1),
                ('+', 2, 1),
                ('=', 2, 2),
                ('=', 3, 3),
            ]
        );
    }

    #[test]
    fn display_path_trims_trailing_separators() {
        assert_eq!(display_path(Path::new(r"C:\proj\")), r"C:\proj");
        assert_eq!(display_path(Path::new("/home/u/proj/")), "/home/u/proj");
    }

    #[test]
    fn get_worktree_branches_skips_unc_paths() {
        let out = get_worktree_branches(&[
            PathBuf::from(r"\\wsl$\Ubuntu\home\u\proj"),
            PathBuf::from("/definitely/not/a/repo"),
        ]);
        assert_eq!(out, vec![None, None]);
    }

    #[test]
    fn run_git_command_rejects_non_repo_dir() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mini-term-git-not-a-repo-{ts}"));
        std::fs::create_dir_all(&dir).unwrap();
        let err = run_git_command(&dir, &["status"], Duration::from_secs(5), "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("不是 git 仓库"), "实际错误: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// git_commit 改走 run_git_command 后,那两道目录/仓库前置校验必须还在
    /// (不能因为换了执行路径就变成「先 spawn 再说」)。
    #[test]
    fn git_commit_keeps_repo_guards() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mini-term-git-commit-guard-{ts}"));
        std::fs::create_dir_all(&dir).unwrap();
        let err = git_commit(&dir, "msg").unwrap_err().to_string();
        assert!(err.contains("不是 git 仓库"), "实际错误: {err}");
        std::fs::remove_dir_all(&dir).ok();

        let missing = std::env::temp_dir().join(format!("mini-term-git-commit-absent-{ts}"));
        let err = git_commit(&missing, "msg").unwrap_err().to_string();
        assert!(err.contains("不是有效目录"), "实际错误: {err}");
    }

    /// 用 git2 搭一个只有一次提交、只含 `rel` 一个文件的仓库,返回仓库根。
    ///
    /// 对根做 canonicalize(Windows 顺带剥掉 `\\?\` 前缀):libgit2 会对路径做 realpath,
    /// macOS 的 `/var` → `/private/var`、GitHub Windows runner 临时目录的 8.3 短路径
    /// (`RUNNER~1` → `runneradmin`)都会让「文件相对 workdir 的路径」算歪。
    fn init_repo_with_file(tag: &str, rel: &str, content: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mini-term-git-{tag}-{ts}"));
        std::fs::create_dir_all(&root).unwrap();
        let root = crate::fs::strip_verbatim_prefix(std::fs::canonicalize(&root).unwrap());
        let repo = Repository::init(&root).unwrap();
        let abs = root.join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(rel)).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("mini-term", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        root
    }

    fn diff_lines(result: &GitDiffResult) -> Vec<(String, String)> {
        result
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .map(|l| (l.kind.clone(), l.content.clone()))
            .collect()
    }

    fn old_to_new() -> Vec<(String, String)> {
        vec![
            ("delete".to_string(), "old".to_string()),
            ("add".to_string(), "new".to_string()),
        ]
    }

    /// issue #58:仓库根以下 ≥4 层目录的文件,旧实现按文件往上数 5 级当 ceiling 会停在
    /// 仓库根以内(而 libgit2 的 ceiling 排他),diff 直接报「找不到仓库」。
    /// 能不能找到仓库必须与文件深度无关。
    #[test]
    fn diff_finds_repo_for_deeply_nested_file() {
        let rel = "src/pages/task/my/my.vue";
        let root = init_repo_with_file("deep", rel, "old\n");
        std::fs::write(root.join(rel), "new\n").unwrap();
        let result = get_git_diff(&root, rel, Some(false)).unwrap();
        assert_eq!(diff_lines(&result), old_to_new());
        std::fs::remove_dir_all(&root).ok();

        // 比 MAX_DISCOVER_PARENTS 深得多也一样
        let rel = "a/b/c/d/e/f/g/h/i/j.txt";
        let root = init_repo_with_file("deeper", rel, "old\n");
        std::fs::write(root.join(rel), "new\n").unwrap();
        let result = get_git_diff(&root, rel, Some(false)).unwrap();
        assert_eq!(diff_lines(&result), old_to_new());
        std::fs::remove_dir_all(&root).ok();
    }

    /// 项目是 monorepo 的子目录(仓库在项目根之上):文件树的「查看变更」传的是项目根,
    /// 仍要能找到上面的仓库,且文件相对 workdir 的路径算对(HEAD 内容取得到)。
    #[test]
    fn diff_finds_repo_above_project_root() {
        let rel_in_repo = "packages/app/src/pages/task/my/my.vue";
        let root = init_repo_with_file("monorepo", rel_in_repo, "old\n");
        std::fs::write(root.join(rel_in_repo), "new\n").unwrap();
        let project = root.join("packages").join("app");
        let result = get_git_diff(&project, "src/pages/task/my/my.vue", Some(false)).unwrap();
        assert_eq!(diff_lines(&result), old_to_new());
        std::fs::remove_dir_all(&root).ok();
    }

    /// 文件连同所在目录一起被删、删除已暂存:起点目录不存在时得回退到还存在的祖先,
    /// 否则 libgit2 对起点做 realpath 就报错了。
    #[test]
    fn diff_of_staged_deletion_survives_missing_directory() {
        let rel = "src/pages/task/my/my.vue";
        let root = init_repo_with_file("deleted", rel, "old\n");
        std::fs::remove_dir_all(root.join("src")).unwrap();
        {
            let repo = Repository::open(&root).unwrap();
            let mut index = repo.index().unwrap();
            index.remove_path(Path::new(rel)).unwrap();
            index.write().unwrap();
        }
        let result = get_git_diff(&root, rel, Some(true)).unwrap();
        assert_eq!(
            diff_lines(&result),
            vec![("delete".to_string(), "old".to_string())]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn nth_parent_stops_at_root() {
        let p = Path::new("/a/b/c");
        assert_eq!(nth_parent(p, 1), PathBuf::from("/a/b"));
        assert_eq!(nth_parent(p, 3), PathBuf::from("/"));
        assert_eq!(nth_parent(p, 10), PathBuf::from("/"));
    }

    /// 分支列表带上游:本地分支配了 `branch.<name>.remote/merge` 且远程跟踪引用在场时
    /// 给出 `origin/<name>`;没配上游、以及远程分支自己,都是 `None`。
    /// 不联网:远程只登记 URL,远程跟踪引用直接手写。
    #[test]
    fn branches_carry_upstream() {
        let root = init_repo_with_file("upstream", "a.txt", "a\n");
        let before = get_repo_branches(&root).unwrap();
        assert_eq!(before.len(), 1, "{before:?}");
        let local = &before[0];
        assert!(local.is_head && !local.is_remote);
        assert_eq!(local.upstream, None, "还没配上游");
        let name = local.name.clone();

        {
            let repo = Repository::open(&root).unwrap();
            repo.remote("origin", "https://example.invalid/repo.git")
                .unwrap();
            let head = repo.head().unwrap().target().unwrap();
            repo.reference(&format!("refs/remotes/origin/{name}"), head, true, "test")
                .unwrap();
            let mut branch = repo.find_branch(&name, git2::BranchType::Local).unwrap();
            branch
                .set_upstream(Some(&format!("origin/{name}")))
                .unwrap();
        }

        let after = get_repo_branches(&root).unwrap();
        let local = after
            .iter()
            .find(|b| !b.is_remote && b.name == name)
            .expect("本地分支还在");
        assert_eq!(
            local.upstream.as_deref(),
            Some(format!("origin/{name}").as_str())
        );
        let remote = after
            .iter()
            .find(|b| b.is_remote)
            .expect("远程跟踪分支要列出来");
        assert_eq!(remote.name, format!("origin/{name}"));
        assert_eq!(remote.upstream, None, "远程分支自己没有上游");
        assert_eq!(remote.commit_hash, local.commit_hash);
        std::fs::remove_dir_all(&root).ok();
    }

    // ── 提交历史分页 ──────────────────────────────────────────

    /// 空仓库(没有任何提交)。提交历史的用例用 [`LogRepo`] 直接造提交对象,
    /// 提交时间逐个指定 —— 时钟偏移就是这么造出来的。
    struct LogRepo {
        root: PathBuf,
        repo: Repository,
        tree: Oid,
    }

    impl LogRepo {
        fn new(tag: &str) -> Self {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("mini-term-gitlog-{tag}-{ts}"));
            std::fs::create_dir_all(&root).unwrap();
            let repo = Repository::init(&root).unwrap();
            let tree = repo.treebuilder(None).unwrap().write().unwrap();
            Self { root, repo, tree }
        }

        /// 提交时间 = `time`(秒),父按给定顺序(第 0 个是主线父)。不动任何引用。
        fn commit(&self, msg: &str, time: i64, parents: &[Oid]) -> Oid {
            let sig =
                git2::Signature::new("tester", "t@example.com", &git2::Time::new(time, 0)).unwrap();
            let tree = self.repo.find_tree(self.tree).unwrap();
            let parents: Vec<_> = parents
                .iter()
                .map(|p| self.repo.find_commit(*p).unwrap())
                .collect();
            let parent_refs: Vec<_> = parents.iter().collect();
            self.repo
                .commit(None, &sig, &sig, msg, &tree, &parent_refs)
                .unwrap()
        }

        /// `refs/heads/<name>` 指向 `oid`;`head` 为真时 HEAD 也指过去。
        fn branch(&self, name: &str, oid: Oid, head: bool) {
            let full = format!("refs/heads/{name}");
            self.repo.reference(&full, oid, true, "test").unwrap();
            if head {
                self.repo.set_head(&full).unwrap();
            }
        }

        fn msg_of(&self, hash: &str) -> String {
            let oid = Oid::from_str(hash).unwrap();
            self.repo
                .find_commit(oid)
                .unwrap()
                .summary()
                .unwrap()
                .to_string()
        }

        /// 从 `tip` 可达的全部提交(不管顺序)。
        fn reachable(&self, tip: Oid) -> HashSet<String> {
            let mut seen = HashSet::new();
            let mut stack = vec![tip];
            while let Some(oid) = stack.pop() {
                if seen.insert(oid.to_string()) {
                    stack.extend(self.repo.find_commit(oid).unwrap().parent_ids());
                }
            }
            seen
        }
    }

    impl Drop for LogRepo {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    /// 列表里每个提交的父(在列表里的话)都排在它后面。
    fn assert_parents_after_children(list: &[GitCommitInfo], ctx: &str) {
        let pos: HashMap<&str, usize> = list
            .iter()
            .enumerate()
            .map(|(i, c)| (c.hash.as_str(), i))
            .collect();
        for (i, c) in list.iter().enumerate() {
            for p in &c.parent_hashes {
                if let Some(&pi) = pos.get(p.as_str()) {
                    assert!(
                        pi > i,
                        "{ctx}: 父 {p} 在第 {pi} 行,排到了子 {} (第 {i} 行)前面",
                        c.hash
                    );
                }
            }
        }
    }

    /// 按 UI 的用法一页页翻到底:首页带 `branch`,续页用已加载列表还原游标。
    /// 每页都查一遍:后端不回已加载的提交(不重)、合并后父在子后。
    fn load_all(root: &Path, branch: Option<&str>, page: usize) -> Vec<GitCommitInfo> {
        let mut loaded = Vec::new();
        let first = get_git_log(root, None, Some(page), branch).unwrap();
        assert!(first.len() <= page);
        merge_log_page(&mut loaded, first);
        assert_parents_after_children(&loaded, "首页");
        for round in 0.. {
            assert!(round < 10_000, "翻页不收敛");
            let cursor = GitLogCursor::from_loaded(&loaded);
            let next = get_git_log(root, Some(&cursor), Some(page), None).unwrap();
            if next.is_empty() {
                break;
            }
            assert!(next.len() <= page);
            let before: HashSet<String> = loaded.iter().map(|c| c.hash.clone()).collect();
            for c in &next {
                assert!(!before.contains(&c.hash), "续页带回了已加载的 {}", c.hash);
            }
            let added = merge_log_page(&mut loaded, next.clone());
            assert_eq!(added, next.len(), "页内不该有重复");
            assert_parents_after_children(&loaded, &format!("第 {round} 次续页后"));
        }
        loaded
    }

    fn hashes(list: &[GitCommitInfo]) -> HashSet<String> {
        list.iter().map(|c| c.hash.clone()).collect()
    }

    /// 旧实现的洞:续页只从「上一页最后一个提交的父」往下走。合并提交两侧分支
    /// 交替出现时,页边界上另一侧还没产出的提交就丢了。游标续页必须一个不漏。
    #[test]
    fn 合并历史跨页不漏不重() {
        let r = LogRepo::new("merge");
        let base = r.commit("base", 100, &[]);
        let l1 = r.commit("L1", 110, &[base]);
        let r1 = r.commit("R1", 120, &[base]);
        let l2 = r.commit("L2", 130, &[l1]);
        let r2 = r.commit("R2", 140, &[r1]);
        let m = r.commit("M", 150, &[l2, r2]);
        r.branch("main", m, true);

        for page in 1..=7 {
            let all = load_all(&r.root, None, page);
            assert_eq!(all.len(), 6, "页大小 {page}");
            assert_eq!(hashes(&all), r.reachable(m), "页大小 {page}");
            let msgs: Vec<_> = all.iter().map(|c| r.msg_of(&c.hash)).collect();
            // 没有时钟偏移:就是按时间倒序
            assert_eq!(msgs, ["M", "R2", "L2", "R1", "L1", "base"], "页大小 {page}");
        }
    }

    /// 时钟偏移且子提交已被发现:分支提交比它的父早,但它是经由合并提交先被
    /// 发现的 —— 父要等它产出才轮得到,页流本身就是拓扑序,不需要事后修正。
    #[test]
    fn 时钟偏移_已发现的子提交挡住父() {
        let r = LogRepo::new("skew-known");
        let f = r.commit("F", 1000, &[]);
        let m1 = r.commit("m1", 1050, &[f]);
        let b1 = r.commit("B1", 500, &[f]);
        let m = r.commit("M", 1100, &[m1, b1]);
        r.branch("main", m, true);

        let page = get_git_log(&r.root, None, Some(10), None).unwrap();
        let msgs: Vec<_> = page.iter().map(|c| r.msg_of(&c.hash)).collect();
        assert_eq!(msgs, ["M", "m1", "B1", "F"]);
        assert_parents_after_children(&page, "单页");
    }

    /// 时钟偏移且子提交埋在更深处:整条分支都比分叉点早 500 秒,只有分支尖端
    /// 挂在合并提交上被发现,中间那几个要等时间走到 500 才被读到 —— 那时分叉点 F
    /// 早已产出。合并页时必须把迟到的这串提到 F 前面(与 `git log --date-order`
    /// 全量排出来的位置一致),其余行不动。
    #[test]
    fn 时钟偏移_迟到的子提交在合并时提到父前() {
        let r = LogRepo::new("skew-late");
        let d = r.commit("D", 980, &[]);
        let e = r.commit("E", 990, &[d]);
        let f = r.commit("F", 1000, &[e]);
        let m1 = r.commit("m1", 1010, &[f]);
        let m2 = r.commit("m2", 1020, &[m1]);
        let b1 = r.commit("B1", 500, &[f]);
        let b2 = r.commit("B2", 510, &[b1]);
        let b3 = r.commit("B3", 520, &[b2]);
        let m = r.commit("M", 1100, &[m2, b3]);
        r.branch("main", m, true);

        let expected = ["M", "m2", "m1", "B3", "B2", "B1", "F", "E", "D"];
        for page in 1..=10 {
            let all = load_all(&r.root, None, page);
            assert_eq!(hashes(&all), r.reachable(m), "页大小 {page}");
            let msgs: Vec<_> = all.iter().map(|c| r.msg_of(&c.hash)).collect();
            assert_eq!(msgs, expected, "页大小 {page}");
        }
    }

    /// 同一秒的并行分支(脚本批量提交、rebase 常见):时间比不出先后,
    /// 拓扑约束仍要成立,也不能漏。
    #[test]
    fn 同一秒的并行分支() {
        let r = LogRepo::new("same-second");
        let base = r.commit("base", 100, &[]);
        let mut left = base;
        let mut right = base;
        for i in 0..5 {
            left = r.commit(&format!("L{i}"), 100, &[left]);
            right = r.commit(&format!("R{i}"), 100, &[right]);
        }
        let m = r.commit("M", 100, &[left, right]);
        r.branch("main", m, true);
        for page in 1..=4 {
            let all = load_all(&r.root, None, page);
            assert_eq!(hashes(&all), r.reachable(m), "页大小 {page}");
            assert_eq!(r.msg_of(&all[0].hash), "M");
            assert_eq!(r.msg_of(&all.last().unwrap().hash), "base");
        }
    }

    /// 随机 DAG + 随机提交时间(大量时钟偏移):任意页大小翻到底都是全集、无重复、
    /// 每次合并后父都在子后。伪随机数用固定种子的 LCG,失败可复现。
    #[test]
    fn 随机_dag_翻页性质() {
        for seed in [1u64, 7, 42] {
            let mut state = seed;
            let mut rand = move |n: u64| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) % n
            };
            let r = LogRepo::new(&format!("random-{seed}"));
            let mut commits: Vec<Oid> = Vec::new();
            for i in 0..120 {
                let mut parents = Vec::new();
                if !commits.is_empty() {
                    // 主线父偏向最近的提交,偶尔带第二个父(合并)
                    let len = commits.len() as u64;
                    parents.push(commits[(len - 1 - rand(len.min(6))) as usize]);
                    if rand(4) == 0 {
                        let other = commits[rand(len) as usize];
                        if !parents.contains(&other) {
                            parents.push(other);
                        }
                    }
                }
                // 时间整体随序号增长,但抖动远大于步长 —— 父比子新的边比比皆是
                let time = 10_000 + i as i64 * 10 + rand(400) as i64 - 200;
                commits.push(r.commit(&format!("c{i}"), time, &parents));
            }
            // 把所有尖端汇进一个合并链,保证全部提交从 HEAD 可达
            let mut tip = *commits.last().unwrap();
            for (k, &c) in commits.iter().enumerate() {
                if !r.reachable(tip).contains(&c.to_string()) {
                    tip = r.commit(&format!("join{k}"), 20_000 + k as i64, &[tip, c]);
                }
            }
            r.branch("main", tip, true);
            let expected = r.reachable(tip);
            for page in [1, 2, 3, 5, 8, 13, 30, 500] {
                let all = load_all(&r.root, None, page);
                assert_eq!(all.len(), expected.len(), "seed {seed} 页大小 {page}");
                assert_eq!(hashes(&all), expected, "seed {seed} 页大小 {page}");
            }
        }
    }

    /// 起点:不给分支走 HEAD;给本地分支名走那条分支;远程分支名也认;
    /// 不存在的分支报错。续页不再解析分支(分支删了照样能翻)。
    #[test]
    fn 起点_head_与指定分支() {
        let r = LogRepo::new("branches");
        let base = r.commit("base", 100, &[]);
        let main1 = r.commit("main1", 110, &[base]);
        let main2 = r.commit("main2", 120, &[main1]);
        let dev1 = r.commit("dev1", 130, &[base]);
        let dev2 = r.commit("dev2", 140, &[dev1]);
        r.branch("main", main2, true);
        r.branch("dev", dev2, false);
        r.repo
            .reference("refs/remotes/origin/dev", dev1, true, "test")
            .unwrap();

        let msgs = |list: &[GitCommitInfo]| -> Vec<String> {
            list.iter().map(|c| r.msg_of(&c.hash)).collect()
        };
        assert_eq!(
            msgs(&load_all(&r.root, None, 2)),
            ["main2", "main1", "base"]
        );
        assert_eq!(
            msgs(&load_all(&r.root, Some("dev"), 2)),
            ["dev2", "dev1", "base"]
        );
        assert_eq!(
            msgs(&load_all(&r.root, Some("origin/dev"), 2)),
            ["dev1", "base"]
        );
        let err = get_git_log(&r.root, None, Some(10), Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("未找到分支"), "实际错误: {err}");

        // 首页之后把 dev 删掉:续页只看游标,照样翻到底
        let first = get_git_log(&r.root, None, Some(1), Some("dev")).unwrap();
        r.repo
            .find_reference("refs/heads/dev")
            .unwrap()
            .delete()
            .unwrap();
        let cursor = GitLogCursor::from_loaded(&first);
        let rest = get_git_log(&r.root, Some(&cursor), Some(10), Some("dev")).unwrap();
        assert_eq!(msgs(&rest), ["dev1", "base"]);
    }

    /// 空仓库列空而不是报错;单提交仓库一页就到底,续页为空。
    #[test]
    fn 空仓库与单提交仓库() {
        let r = LogRepo::new("empty");
        assert!(
            get_git_log(&r.root, None, Some(30), None)
                .unwrap()
                .is_empty()
        );
        assert!(load_all(&r.root, None, 30).is_empty());

        let only = r.commit("only", 100, &[]);
        r.branch("main", only, true);
        let page = get_git_log(&r.root, None, Some(30), None).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].hash, only.to_string());
        assert!(page[0].parent_hashes.is_empty());
        let cursor = GitLogCursor::from_loaded(&page);
        assert!(
            get_git_log(&r.root, Some(&cursor), Some(30), None)
                .unwrap()
                .is_empty()
        );
    }

    /// 合并一页:重复的丢掉、返回真正新增的条数;没有跨页的父子倒挂时一行不挪。
    #[test]
    fn 合并页去重且顺序合法时不重排() {
        let commit = |hash: &str, parents: &[&str]| GitCommitInfo {
            hash: hash.into(),
            short_hash: hash.into(),
            message: String::new(),
            body: None,
            author: String::new(),
            timestamp: 0,
            parent_hashes: parents.iter().map(|p| p.to_string()).collect(),
        };
        let mut loaded = vec![commit("c", &["b"]), commit("x", &["b"])];
        let added = merge_log_page(&mut loaded, vec![commit("x", &["b"]), commit("b", &["a"])]);
        assert_eq!(added, 1);
        let order: Vec<_> = loaded.iter().map(|c| c.hash.as_str()).collect();
        assert_eq!(order, ["c", "x", "b"]);
        assert_eq!(merge_log_page(&mut loaded, vec![commit("c", &["b"])]), 0);

        // 迟到的子 y(父是已经排在前面的 b):提到 b 前,其它行相对次序不变
        let added = merge_log_page(&mut loaded, vec![commit("a", &[]), commit("y", &["b"])]);
        assert_eq!(added, 2);
        let order: Vec<_> = loaded.iter().map(|c| c.hash.as_str()).collect();
        assert_eq!(order, ["c", "x", "y", "b", "a"]);
    }
}
