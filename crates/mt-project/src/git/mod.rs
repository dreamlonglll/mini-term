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
//! 否则 30s/120s 的超时等待会卡住 UI 线程。
//!
//! 本文件之外的能力按主题拆成子模块(`git/<主题>.rs`,在这里 `mod` + `pub use`
//! 摊平,调用方照旧 `mt_project::git::xxx`)。子模块直接用本模块私有的 CLI 共用件:
//! 执行器 `run_git_command` / `run_git_command_labeled` / `run_git_output`、
//! 非零即错的 [`run_checked`] 与失败原文 [`failure_text`]、超时档位
//! ([`QUICK_OP_TIMEOUT`] 等四档)与 [`NO_EDITOR`] / [`NETWORK_HINT`]。

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

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

/// 取状态的选项:含未跟踪(逐个文件)、不含忽略,**暂存区一侧开重命名检测**。
///
/// 只开 HEAD → 暂存区这一侧(`git mv` / 暂存了「删旧 + 加新」之后显示一行 R,与
/// `git status` 一致)。工作区一侧不开:libgit2 的 `RENAMES_INDEX_TO_WORKDIR` 会把
/// 「工作区删掉的已跟踪文件」与**未跟踪文件**配成一对,`git status` 从不这么做 ——
/// 未跟踪文件只是躺在那里,不该被认成谁的新名字。
fn status_options() -> StatusOptions {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .renames_head_to_index(true);
    opts
}

/// 一条状态的路径:`(当前路径, 重命名前的路径)`。
///
/// ⚠️ 不能直接用 `StatusEntry::path()`:git2 取的是 `head_to_index` 的**旧**路径 ——
/// 开了重命名检测之后,已暂存的重命名会被报成旧名字。重命名要从 diff 的新旧两端各取。
fn entry_paths(entry: &git2::StatusEntry<'_>) -> (String, Option<String>) {
    let slash = |p: &Path| p.to_string_lossy().replace('\\', "/");
    if entry.status().contains(Status::INDEX_RENAMED)
        && let Some(delta) = entry.head_to_index()
        && let Some(new) = delta.new_file().path()
    {
        return (slash(new), delta.old_file().path().map(slash));
    }
    (entry.path().unwrap_or("").to_string(), None)
}

fn collect_repo_status(
    repo: &Repository,
    path_prefix: Option<&Path>,
) -> Result<Vec<GitFileStatus>> {
    let is_empty_repo = repo.head().is_err();

    let statuses = repo.statuses(Some(&mut status_options()))?;
    let repo_workdir = repo.workdir().unwrap_or_else(|| repo.path());
    // 项目经软链进入时 workdir 是解析过的真实路径,前缀也得换成真实路径才比得上
    let path_prefix = path_prefix.map(|prefix| status_prefix(repo_workdir, prefix));

    let mut result = Vec::new();
    for entry in statuses.iter() {
        // 重命名只报新路径(旧路径已不在工作区,文件树上没有它那一行)
        let (raw_path, renamed_from) = entry_paths(&entry);
        let s = entry.status();

        let git_status = match map_status(s, is_empty_repo) {
            Some(gs) => gs,
            None => continue,
        };

        let label = status_label(&git_status).to_string();

        // 相对 path_prefix 的展示路径;没给 prefix 就用仓库内相对路径
        let display_path = if let Some(prefix) = &path_prefix {
            let abs = repo_workdir.join(&raw_path);
            diff_paths(&abs, prefix)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|| raw_path.clone())
        } else {
            raw_path.clone()
        };

        // 重命名才有 old_path
        let old_path = renamed_from.filter(|_| matches!(git_status, GitStatus::Renamed));

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
    /// **作者时间**(秒),只给界面显示:与 `git log` 默认、IDEA 的 Log 一致,修改消息 /
    /// 拣选 / 变基之后行里的时间不会变成「刚刚」而与详情区对不上。提交历史的拓扑 / 分页
    /// 排序仍按提交者时间(`DateTopoWalk`),与这个字段无关。
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

/// `project_path` 下的文件 `abs_file` 在仓库工作区 `workdir` 里的相对路径(`/` 分隔);
/// 不在这个工作区里(或就是工作区根本身)为 `None`。
///
/// 先直接算;算出 `..` 开头时再把项目根 `canonicalize` 一遍重算:libgit2 发现仓库时对
/// 起点做过 realpath,Unix 上经软链进入的项目(macOS 的 `/var` → `/private/var` 也是)
/// workdir 是解析后的真实路径、项目路径不是,直接比出来是 `../<链接名>/…` —— diff 误显示
/// 为整文件新增、按块暂存找不到条目。只规范化**项目根**(它一定存在),文件那一段原样
/// 拼回去:文件本身可能已被删除,也不该顺着文件自己的软链跳走。
///
/// Windows 上 `canonicalize` 给的是 `\\?\` 形式,经 [`crate::fs::strip_verbatim_prefix`]
/// 剥回 `C:\…` / `\\server\share\…`(`\\?\UNC\wsl$\…` → `\\wsl$\…`)。第一步就算对时
/// (绝大多数情况,libgit2 在 Windows 上不解析联接点)根本走不到这里。
fn repo_relative_path(workdir: &Path, project_path: &Path, abs_file: &Path) -> Option<String> {
    let inside = |rel: PathBuf| -> Option<String> {
        let escapes = rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        });
        let text = rel.to_string_lossy().replace('\\', "/");
        (!escapes && !text.is_empty()).then_some(text)
    };
    if let Some(rel) = diff_paths(abs_file, workdir).and_then(inside) {
        return Some(rel);
    }
    let in_project = abs_file.strip_prefix(project_path).ok()?;
    let real_project = crate::fs::strip_verbatim_prefix(project_path.canonicalize().ok()?);
    diff_paths(real_project.join(in_project), workdir).and_then(inside)
}

/// 仓库里的路径相对项目根的展示形式要用的「项目根」:与 workdir 有前缀关系就用原样;
/// 对不上(项目经软链进入、workdir 是解析过的真实路径)时换成规范化后的项目根。
fn status_prefix(workdir: &Path, project_path: &Path) -> PathBuf {
    let related = |p: &Path| workdir.starts_with(p) || p.starts_with(workdir);
    if related(project_path) {
        return project_path.to_path_buf();
    }
    project_path
        .canonicalize()
        .map(crate::fs::strip_verbatim_prefix)
        .ok()
        .filter(|real| related(real))
        .unwrap_or_else(|| project_path.to_path_buf())
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

/// 「更改」列表的数据:每个文件的暂存 / 未暂存两侧状态。已暂存的重命名是一行
/// (`path` = 新路径、`old_path` = 旧路径、`staged_status = Renamed`),口径见 [`status_options`]。
pub fn get_changes_status(repo_path: &Path) -> Result<Vec<ChangeFileStatus>> {
    let repo = Repository::open(repo_path)?;
    let is_empty_repo = repo.head().is_err();

    let statuses = repo.statuses(Some(&mut status_options()))?;
    let mut result = Vec::new();

    for entry in statuses.iter() {
        let (raw_path, old_path) = entry_paths(&entry);
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
        match log_tip(&repo, branch)? {
            Some(tip) => walk.discover(tip),
            None => return Ok(Vec::new()),
        }
    }

    Ok(walk.take(limit))
}

/// 首页的起点:`branch`(`None` = HEAD)指向的提交。空仓库(HEAD 未出生)是 `None`。
fn log_tip(repo: &Repository, branch: Option<&str>) -> Result<Option<Oid>> {
    match branch {
        Some(b) => {
            // 先找本地 refs/heads/<b>,再找远程 refs/remotes/<b>
            // worktree 持有的分支也在 refs/heads/ 下(与主 repo 共享 refs 存储),天然支持
            let local_ref = format!("refs/heads/{}", b);
            let remote_ref = format!("refs/remotes/{}", b);
            let reference = repo
                .find_reference(&local_ref)
                .or_else(|_| repo.find_reference(&remote_ref))
                .map_err(|_| anyhow!("未找到分支:{}", b))?;
            Ok(Some(
                reference
                    .peel_to_commit()
                    .map_err(|_| anyhow!("分支 {} 无有效 target", b))?
                    .id(),
            ))
        }
        None => match repo.head() {
            Ok(head) => Ok(Some(head.peel_to_commit()?.id())),
            // 空仓库:HEAD 指向还没出生的分支,没有历史可列
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch => Ok(None),
            Err(e) => Err(e.into()),
        },
    }
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
        self.take_matching(limit, usize::MAX, |_| true).0
    }

    /// 同 [`Self::take`],但只把 `keep` 认可的提交放进结果(过滤 / 文件历史用)。
    /// 不认可的照常推进拓扑 —— 父照样发现、照样就绪,只是不产出。
    ///
    /// `budget` 是本次最多**走过**几个提交(产出 + 跳过):条件很少命中时一页可能
    /// 要走遍大半个历史,分批走、每批把现场交回调用方,UI 才能半路换条件、不被
    /// 一次全量遍历拖住。返回 `(结果, 走过的个数)`。
    fn take_matching(
        &mut self,
        limit: usize,
        budget: usize,
        mut keep: impl FnMut(&Commit<'r>) -> bool,
    ) -> (Vec<GitCommitInfo>, usize) {
        let mut out = Vec::with_capacity(limit.min(256));
        let mut walked = 0usize;
        while out.len() < limit && walked < budget {
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
            walked += 1;
            if keep(&commit) {
                out.push(commit_info(&commit));
            }
        }
        (out, walked)
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
        // 显示用作者时间(见字段注释);排序在 `DateTopoWalk` 里另按提交者时间
        timestamp: commit.author().when().seconds(),
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
                // 按「HEAD 这个符号引用指着谁」判,不按提交比:新建的分支与 main 同一提交时
                // 两条都会被认成当前分支。游离 HEAD 不指向任何分支,一条都不是
                is_head: branch.is_head(),
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
    commit_files_in(&repo, &commit)
}

/// [`get_commit_files`] 的本体:拿已打开的仓库与提交算(合并查看多个提交时逐个调,
/// 不必每个提交都重开一次仓库)。相对**第一个父**比(合并提交也只比第一父),根提交
/// 对空树比。
pub(crate) fn commit_files_in(
    repo: &Repository,
    commit: &Commit<'_>,
) -> Result<Vec<CommitFileInfo>> {
    let tree = commit.tree()?;

    let parent_tree = if commit.parent_count() > 0 {
        Some(commit.parent(0)?.tree()?)
    } else {
        None
    };

    let mut diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;
    // 树对树的 diff 默认不做相似度配对:纯重命名会拆成一删一增,下游「仅重命名」的
    // 分支永远走不到。按 `git show` 的默认口径配对(只认重命名、不认复制,相似度 50%)
    let mut find = git2::DiffFindOptions::new();
    find.renames(true);
    diff.find_similar(Some(&mut find))?;

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

    let old_lookup_path = old_file_path.unwrap_or(file_path);
    diff_tree_texts(
        &repo,
        parent_tree.as_ref().map(|t| (t, old_lookup_path)),
        Some((&tree, file_path)),
    )
}

/// 树里一个文件的文本内容(diff 的一侧)。
enum TreeText {
    Text(String),
    Binary,
    TooLarge,
}

/// 读 `tree` 里 `path` 这个文件的文本。树里没有这个路径(新增 / 删除的那一侧)当空文本;
/// 不是 blob、不是 UTF-8 报错(与提交 diff 一直以来的口径一致)。`limit_size` 只给新的
/// 那一侧开:超过 [`MAX_DIFF_BYTES`] 的整份不做 diff。
fn read_tree_text(
    repo: &Repository,
    tree: &git2::Tree<'_>,
    path: &str,
    limit_size: bool,
) -> Result<TreeText> {
    let entry = match tree.get_path(Path::new(path)) {
        Ok(entry) => entry,
        Err(_) => return Ok(TreeText::Text(String::new())),
    };
    let obj = entry.to_object(repo)?;
    let blob = obj.as_blob().ok_or_else(|| anyhow!("not a blob"))?;
    if blob.is_binary() {
        return Ok(TreeText::Binary);
    }
    if limit_size && blob.content().len() > MAX_DIFF_BYTES {
        return Ok(TreeText::TooLarge);
    }
    let text = std::str::from_utf8(blob.content()).map_err(|_| anyhow!("binary"))?;
    Ok(TreeText::Text(text.to_string()))
}

/// 两棵树里各一个文件 → diff。`None` 的一侧当空文本(文件新增 / 删除)。
///
/// 判定顺序照旧:先读新的一侧(二进制 / 过大当场返回),再读旧的一侧(二进制返回)。
/// 单提交 diff([`get_commit_file_diff`])与合并查看的两版本 diff
/// ([`get_revision_file_diff`])共用这一份,口径不分叉。
pub(crate) fn diff_tree_texts(
    repo: &Repository,
    old: Option<(&git2::Tree<'_>, &str)>,
    new: Option<(&git2::Tree<'_>, &str)>,
) -> Result<GitDiffResult> {
    let new_content = match new {
        Some((tree, path)) => match read_tree_text(repo, tree, path, true)? {
            TreeText::Text(text) => text,
            TreeText::Binary => return Ok(binary_diff_result()),
            TreeText::TooLarge => return Ok(too_large_diff_result()),
        },
        None => String::new(),
    };
    let old_content = match old {
        Some((tree, path)) => match read_tree_text(repo, tree, path, false)? {
            TreeText::Text(text) => text,
            TreeText::Binary => return Ok(binary_diff_result()),
            TreeText::TooLarge => return Ok(too_large_diff_result()),
        },
        None => String::new(),
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
        flat.push(('=', old_lines.len() - tail + k, new_lines.len() - tail + k));
    }
    flat
}

/// 基于 LCS 的行 diff,上下文 3 行。改动块先按 git 的口径滑到位(见 `slide` 模块注释),
/// 行号才与 `git diff` 对得上。
fn build_hunks(old_lines: &[&str], new_lines: &[&str]) -> Vec<DiffHunk> {
    let flat = slide::compact_ops(&edit_script(old_lines, new_lines), old_lines, new_lines);

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

/// 单文件 diff 比的是哪两侧(与 `git diff` 家族同口径)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSide {
    /// HEAD → 暂存区(`git diff --cached`,「已暂存」分组)
    Staged,
    /// 暂存区 → 工作区(`git diff`,「未暂存」/「未跟踪」分组)。部分暂存的文件
    /// 只显示**还没暂存**的那部分 —— 按块暂存 / 丢弃都以这一侧为准
    Unstaged,
    /// HEAD → 工作区(`git diff HEAD`,文件树「查看变更」:相对上次提交的全部改动)
    Working,
}

/// 单文件 diff,两侧由 [`DiffSide`] 决定。`file_path` 是相对 `project_path` 的路径。
pub fn get_git_diff(project_path: &Path, file_path: &str, side: DiffSide) -> Result<GitDiffResult> {
    get_git_diff_from(project_path, file_path, None, side)
}

/// 同 [`get_git_diff`],外加重命名前的路径 `old_path`(同样相对 `project_path`)。
///
/// 已暂存的重命名(`git mv a b`)在 HEAD 里只有旧路径 `a`:拿新路径 `b` 去 HEAD 里查
/// 查不到,整份内容就成了「新增」。给了 `old_path` 时,取 HEAD 那一侧(`Staged` /
/// `Working` 的旧侧)改按旧路径取 —— 与 `git diff --cached -M` 一样只显示真正改了的行。
/// 暂存区 / 工作区那一侧照旧按新路径取;`None` 与 [`get_git_diff`] 完全相同。
pub fn get_git_diff_from(
    project_path: &Path,
    file_path: &str,
    old_path: Option<&str>,
    side: DiffSide,
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

    let rel_str = repo_relative_path(workdir, project_path, &abs_file)
        .ok_or_else(|| anyhow!("file is outside repository working directory"))?;
    // HEAD 里按哪个路径取:重命名取旧路径(算不出来就退回新路径,至少不报错)
    let head_rel = old_path
        .and_then(|old| repo_relative_path(workdir, project_path, &project_path.join(old)))
        .unwrap_or_else(|| rel_str.clone());

    // 新侧:暂存区取 index,否则取工作区(已删除的文件 = 空)
    let new_content = match side {
        DiffSide::Staged => match index_text(&repo, &rel_str)? {
            Some(SideText::Text(s)) => s,
            Some(SideText::Binary) => return Ok(binary_diff_result()),
            Some(SideText::TooLarge) => return Ok(too_large_diff_result()),
            None => String::new(),
        },
        DiffSide::Unstaged | DiffSide::Working => {
            if !abs_file.exists() {
                String::new()
            } else {
                let new_bytes = std::fs::read(&abs_file)?;
                if new_bytes.len() > MAX_DIFF_BYTES {
                    return Ok(too_large_diff_result());
                }
                match String::from_utf8(new_bytes) {
                    Ok(s) => s,
                    Err(_) => return Ok(binary_diff_result()),
                }
            }
        }
    };

    // 旧侧:未暂存比暂存区,其余比 HEAD
    let old_content = match side {
        DiffSide::Unstaged => match index_text(&repo, &rel_str)? {
            Some(SideText::Text(s)) => s,
            Some(SideText::Binary) => return Ok(binary_diff_result()),
            Some(SideText::TooLarge) => return Ok(too_large_diff_result()),
            // 冲突中的文件没有 stage 0 条目 —— 退回 HEAD(ours),不能当成新增
            None if is_conflicted(&repo, &rel_str)? => {
                get_head_content(&repo, &rel_str)?.unwrap_or_default()
            }
            // 不在暂存区 = 未跟踪 / 已暂存删除,全部算新增
            None => String::new(),
        },
        DiffSide::Staged | DiffSide::Working => {
            get_head_content(&repo, &head_rel)?.unwrap_or_default()
        }
    };

    Ok(diff_two_texts(old_content, new_content))
}

/// index / blob 一侧的取值结果。
enum SideText {
    Text(String),
    Binary,
    TooLarge,
}

/// 暂存区 stage 0 条目的内容。`None` = 该路径不在暂存区(未跟踪 / 已暂存删除 / 冲突中)。
fn index_text(repo: &Repository, rel_path: &str) -> Result<Option<SideText>> {
    let index = repo.index()?;
    let Some(entry) = index.get_path(Path::new(rel_path), 0) else {
        return Ok(None);
    };
    let blob = repo.find_blob(entry.id)?;
    if blob.is_binary() {
        return Ok(Some(SideText::Binary));
    }
    if blob.content().len() > MAX_DIFF_BYTES {
        return Ok(Some(SideText::TooLarge));
    }
    Ok(Some(match std::str::from_utf8(blob.content()) {
        Ok(s) => SideText::Text(s.to_string()),
        Err(_) => SideText::Binary,
    }))
}

/// 该路径在暂存区里有没有冲突条目(stage 1~3)。
fn is_conflicted(repo: &Repository, rel_path: &str) -> Result<bool> {
    let index = repo.index()?;
    let path = Path::new(rel_path);
    Ok((1..=3).any(|stage| index.get_path(path, stage).is_some()))
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

// ── 超时档位 ──────────────────────────────────────────────────
//
// 全部 git CLI 调用按「最慢会慢在哪」分四档,子模块一律取这里的名字,不再各写各的秒数。

/// 只动引用 / 只读的快操作(建删改名分支、删贮藏、删标签、冲突取一侧、blame、
/// worktree prune):本身毫秒级,30s 只是兜底。
const QUICK_OP_TIMEOUT: Duration = Duration::from_secs(30);
/// 纯本地、但会同步跑 hook / gpg 签名的操作(提交、拣选、还原、改消息、附注标签、
/// 删 worktree):pre-commit hook 可能跑格式化 / lint / 测试,30s 对大仓库偏紧;真卡死的
/// 形态是 hook 死循环或 gpg 等口令(stdin 已置 null,不会永久挂起),60s 兜底。
const LOCAL_OP_TIMEOUT: Duration = Duration::from_secs(60);
/// 改写工作区 / 暂存区的操作(检出、合并、变基、重置、贮藏、继续 / 中止、暂存类、
/// 建 worktree):大仓库一次 checkout 能到几十秒,还可能跑 post-checkout hook 与
/// clean 过滤器(git-lfs 给大文件算哈希),给到 120s。
const WORKTREE_OP_TIMEOUT: Duration = Duration::from_secs(120);
/// 网络操作(fetch / pull / push / 删远程分支 / 推标签):120s,超时提示用 [`NETWORK_HINT`]。
const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);
/// 网络操作超时时补在错误后面的提示。各处超时提示一律以全角「，」开头、不带句末标点
/// (句号由 [`timeout_message`] 统一补),整句全角标点、不与半角混排。
const NETWORK_HINT: &str = "，可能在等待凭证或网络故障，请确认已配置凭证管理器或 SSH key";

/// git 命令超时的说明:「git push 超时（120s），可能在等待凭证…。」,删过残留的
/// `index.lock` 时另起一行写明。展示给用户看的整句,标点全用全角(E2E r4 A10:原先
/// 「超时(120s),可能…。请确认…SSH key」半角括号 / 逗号混排、句末无句号)。
fn timeout_message(op_label: &str, secs: u64, hint: &str, removed_lock: Option<&Path>) -> String {
    let mut message = format!("git {op_label} 超时（{secs}s）{hint}。");
    if let Some(lock) = removed_lock {
        message.push_str(&format!(
            "\n已终止 git 及其子进程，并删除了它留下的 {}",
            lock.display()
        ));
    }
    message
}

/// 会拉起编辑器的命令(合并提交、`rebase` / `pull --rebase` 续跑、标签)一律带上:
/// 沿用 git 生成的提交信息,不等一个永远不会有人操作的编辑器。
const NO_EDITOR: &[(&str, &str)] = &[("GIT_EDITOR", "true")];

/// git 失败时给用户看的原文:**stderr 优先,空了退 stdout**,两者都空才给一句兜底。
///
/// 不把两者拼在一起:git 的报错本来就写在 stderr,stdout 上多半是进度 / porcelain 这类
/// 与失败无关的输出,拼上只会把原因埋进噪声。但有几类命令把说明写在 stdout 上(`merge`
/// 的冲突说明、`commit` 的「没有可提交的内容」),stderr 为空时只取 stderr 等于什么都
/// 没告诉用户,所以退一步取 stdout。
fn failure_text(out: &GitOutput) -> String {
    let stderr = out.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    let stdout = out.stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_string();
    }
    "git 执行失败(没有输出)".to_string()
}

/// 跑一条 git 命令,非零退出即把原文([`failure_text`])装进错误。
fn run_checked(
    repo_path: &Path,
    args: &[&str],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<GitOutput> {
    run_checked_env(repo_path, args, &[], op_label, timeout, timeout_hint)
}

/// 同 [`run_checked`],另给子进程补几条环境变量(见 [`run_git_output_env`])。
fn run_checked_env(
    repo_path: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<GitOutput> {
    let out = run_git_output_env(repo_path, args, envs, op_label, timeout, timeout_hint)?;
    if !out.success {
        bail!("{}", failure_text(&out));
    }
    Ok(out)
}

/// 执行器的前置校验:`repo_path` 是目录,且下面有 `.git`(避免在任意目录上跑 git)。
/// worktree 目录下 `.git` 是文件而非目录,`exists()` 两者皆真。
fn ensure_repo_dir(repo_path: &Path) -> Result<()> {
    if !repo_path.is_dir() {
        bail!("不是有效目录:{}", repo_path.display());
    }
    if !repo_path.join(".git").exists() {
        bail!("不是 git 仓库(缺少 .git):{}", repo_path.display());
    }
    Ok(())
}

/// 通用 git CLI 执行器(pull/push/worktree 系列共用):
/// - 校验 `repo_path` 是目录并且包含 `.git`([`ensure_repo_dir`])
/// - `GIT_TERMINAL_PROMPT=0`:没有终端可问,要账号密码时立刻失败,不干等到超时
///   (凭证管理器的 GUI 弹窗不受影响)
/// - 到达上限即 kill 子进程并返回超时错误;stdout/stderr 由独立线程读,
///   孙进程(ssh / 凭证助手)攥着管道不放也不会把调用线程拖住
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
    run_checked(repo_path, args, op_label, timeout, timeout_hint).map(|out| out.stdout)
}

/// 同 `run_git_command_labeled`,但退出码非零也返回 `Ok`,交调用方按 stdout/stderr
/// 自己判(merge 冲突、`branch -d` 未合并这类要和「真失败」分开处理的场合)。
fn run_git_output(
    repo_path: &Path,
    args: &[&str],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<GitOutput> {
    run_git_output_env(repo_path, args, &[], op_label, timeout, timeout_hint)
}

/// 同 [`run_git_output`],另给子进程补几条环境变量。
///
/// 给 `rebase --continue` / `cherry-pick --continue` 这类**会拉起编辑器**的续跑用:
/// 传 `GIT_EDITOR=true` 让它直接沿用现成的提交信息。不用 `-c core.editor=true` ——
/// 用户环境里设了 `GIT_EDITOR` 时它压过 `core.editor`,编辑器照样被拉起来。
fn run_git_output_env(
    repo_path: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<GitOutput> {
    ensure_repo_dir(repo_path)?;
    spawn_git(repo_path, args, envs, op_label, timeout, timeout_hint)
}

/// 同 `run_git_command_labeled`,但把 `input` 整段喂给子进程的 stdin(写完即关管道)。
/// 给 `git blame --contents -` 这类要从 stdin 读内容的调用用;其余调用的 stdin
/// 仍是 null(见 `spawn_git`)。
fn run_git_command_with_input(
    repo_path: &Path,
    args: &[&str],
    op_label: &str,
    input: Vec<u8>,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<String> {
    ensure_repo_dir(repo_path)?;
    let output = spawn_git_with_input(
        repo_path,
        args,
        &[],
        op_label,
        timeout,
        timeout_hint,
        Some(input),
    )?;
    if output.success {
        Ok(output.stdout)
    } else {
        bail!("{}", failure_text(&output));
    }
}

/// 一次 git CLI 调用的结果。
struct GitOutput {
    success: bool,
    /// 退出码;Unix 上被信号杀掉时为 `None`。[`GIT_USAGE_ERROR`] = 不认识的选项。
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// git 的「用法错误」退出码(`usage_with_options` 一律 `exit(129)`):老版本不认某个
/// 新选项时就是它。判断「这个 git 不支持某选项」只看它,不看会被本地化的提示文本。
const GIT_USAGE_ERROR: i32 = 129;

/// 管道读完后等孙进程松手的上限。git 本身已退出,剩下的只是 ssh / 凭证助手
/// 这类可能继承了管道句柄的后代 —— 等不到就按已读到的算。
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// 判断 index.lock 是不是「本次启动之后才出现」时对启动时刻的放宽量:文件时间戳取自
/// 粗粒度的系统时钟(Windows 上一拍约 15ms),可能比启动前读到的精确时刻还早一点。
const LOCK_MTIME_SLACK: Duration = Duration::from_millis(100);

/// 边读边攒的管道输出。读线程把读到的每一块追加进共享缓冲,读到 EOF 再发信号;
/// 等不到 EOF(孙进程攥着管道不放)时取走已经攒下的那部分 —— 旧实现读完才整块交出,
/// 宽限期一到已读到的输出就整块丢了。
struct PipeDrain {
    buf: std::sync::Arc<Mutex<Vec<u8>>>,
    done: std::sync::mpsc::Receiver<()>,
}

impl PipeDrain {
    fn start(pipe: Option<Box<dyn std::io::Read + Send>>) -> Self {
        use std::io::Read as _;
        let buf = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (tx, done) = std::sync::mpsc::channel();
        let sink = buf.clone();
        std::thread::spawn(move || {
            if let Some(mut pipe) = pipe {
                let mut chunk = [0u8; 8192];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => sink.lock().extend_from_slice(&chunk[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            }
            // 忽略发送失败:超时路径上接收端已被 drop
            let _ = tx.send(());
        });
        Self { buf, done }
    }

    /// 最多等到 `deadline` 读线程收尾,然后取走已读到的内容。
    fn collect(self, deadline: Instant) -> String {
        let _ = self
            .done
            .recv_timeout(deadline.saturating_duration_since(Instant::now()));
        let bytes = std::mem::take(&mut *self.buf.lock());
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// 子进程连同它的全部后代。超时要整棵终止:Git for Windows 的 `git.exe` 只是个启动器,
/// 真正干活的 git、hook 的 sh、ssh / 凭证助手都是它的后代 —— 只杀启动器,它们照跑,
/// checkout / merge / `commit -a` 的 pre-commit 期间攥着的 index.lock 也一直不放。
///
/// - Windows:spawn 后把子进程放进一个 Job Object,之后它起的后代自动入 Job,超时
///   `TerminateJobObject` 一把收掉。**不**设 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`:那样
///   正常结束时一关句柄就连带杀掉合法地留在后台的后代(凭证缓存守护进程之类)。
///   spawn 与入 Job 之间有极短的窗口,git 在那之前还没起任何子进程。
/// - Unix:spawn 时 `process_group(0)` 让 git 自成一个进程组(pgid = pid),超时对整组
///   发 SIGKILL(本 crate 不依赖 libc,借系统的 `kill` 命令)。
struct ProcessTree {
    #[cfg(windows)]
    job: Option<job_object::Job>,
}

impl ProcessTree {
    /// spawn 之前:Unix 上让子进程自成进程组。
    fn prepare(_cmd: &mut std::process::Command) {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            _cmd.process_group(0);
        }
    }

    /// spawn 之后:Windows 上把子进程挂进 Job。挂不上(极少见)就只能退回只杀 git 本身。
    fn attach(_child: &std::process::Child) -> Self {
        Self {
            #[cfg(windows)]
            job: job_object::Job::attach(_child),
        }
    }

    /// 终止整棵进程树并等 git 本身退出。
    fn kill(&self, child: &mut std::process::Child) {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
        #[cfg(unix)]
        {
            use std::process::{Command, Stdio};
            // process_group(0) 之后 pgid 就是 git 的 pid;负号 = 发给整个进程组
            let group = format!("-{}", child.id());
            let _ = Command::new("kill")
                .args(["-KILL", "--", group.as_str()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        // 兜底(没挂进 Job / 系统里没有 kill 命令):至少把 git 自己杀掉
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(windows)]
mod job_object {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
    };

    /// 一个匿名 Job Object 的句柄,Drop 时关闭(不连带终止里面的进程)。
    pub(super) struct Job(HANDLE);

    impl Job {
        pub(super) fn attach(child: &std::process::Child) -> Option<Job> {
            // SAFETY:两个参数都允许为空(默认安全属性、匿名);返回的句柄归 Job 所有
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle == 0 {
                return None;
            }
            let job = Job(handle);
            // SAFETY:child 存活期间它的进程句柄一直有效
            let ok = unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) };
            (ok != 0).then_some(job)
        }

        pub(super) fn terminate(&self) {
            // SAFETY:句柄在 Job 存活期间有效
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY:句柄只在这里关一次
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// 超时被杀的 git 留下的 `<gitdir>/index.lock`:只认**本次启动之后**才出现的那个(更早的
/// 属于别的 git 进程,不归这里管)。gitdir 走 git2 求 —— linked worktree 的 `.git` 是个
/// 文件,锁在 `<主仓库>/.git/worktrees/<名字>/` 下。返回删掉的路径。
fn remove_fresh_index_lock(repo_path: &Path, started: SystemTime) -> Option<PathBuf> {
    let lock = Repository::open(repo_path).ok()?.path().join("index.lock");
    let since = started.checked_sub(LOCK_MTIME_SLACK).unwrap_or(started);
    if std::fs::metadata(&lock).ok()?.modified().ok()? < since {
        return None;
    }
    // Windows 上被终止的进程要过一会儿才真正放开文件句柄:删不掉就稍等再试
    for _ in 0..20 {
        match std::fs::remove_file(&lock) {
            Ok(()) => return Some(lock),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    None
}

/// 起 git 子进程并限时等待;只有「起不来 / 超时」是 `Err`。stdin 是 null。
fn spawn_git(
    repo_path: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<GitOutput> {
    spawn_git_with_input(repo_path, args, envs, op_label, timeout, timeout_hint, None)
}

/// [`spawn_git`] 的本体。`input` 为 `Some` 时 stdin 接管道,由独立线程整段写完后
/// 关闭(内容大于管道缓冲时,git 一边读一边已经开始往 stdout 写,同一线程先写后读会
/// 互相等死);`None` 时 stdin 是 null,与此前的行为一致。
fn spawn_git_with_input(
    repo_path: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
    input: Option<Vec<u8>>,
) -> Result<GitOutput> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    // 超时后据此认出「本次留下的」index.lock(见 remove_fresh_index_lock)
    let started = SystemTime::now();
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .envs(envs.iter().copied())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console_window(&mut cmd);
    ProcessTree::prepare(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("启动 git {} 失败:{}", op_label, e))?;
    let tree = ProcessTree::attach(&child);

    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        std::thread::spawn(move || {
            // 写失败(git 提前退出、管道已断)不在这里报:退出码与 stderr 会说明原因
            let _ = stdin.write_all(&bytes);
            // drop 即关管道,git 才读得到 EOF
        });
    }

    let stdout = PipeDrain::start(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = PipeDrain::start(child.stderr.take().map(|p| Box::new(p) as _));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                tree.kill(&mut child);
                let lock = remove_fresh_index_lock(repo_path, started);
                bail!(
                    "{}",
                    timeout_message(op_label, timeout.as_secs(), timeout_hint, lock.as_deref())
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(e) => bail!("git {} 等待失败:{}", op_label, e),
        }
    };

    // 两根管道共用一个宽限期:git 已退出,只剩后代可能还攥着管道
    let grace = Instant::now() + PIPE_DRAIN_GRACE;
    Ok(GitOutput {
        success: status.success(),
        code: status.code(),
        stdout: stdout.collect(grace),
        stderr: stderr.collect(grace),
    })
}

fn run_git_network_command(repo_path: &Path, op: &'static str) -> Result<String> {
    run_git_command(repo_path, &[op], NETWORK_TIMEOUT, NETWORK_HINT)
}

/// 无参 `git pull`(旧接口;面板走 `sync` 里的 `git_pull_with` / `git_pull_by_config`)。
/// **阻塞最多 [`NETWORK_TIMEOUT`]**,必须在后台线程上调用。
pub fn git_pull(repo_path: &Path) -> Result<String> {
    run_git_network_command(repo_path, "pull")
}

/// 无参 `git push`(旧接口;面板走 `sync` 里的 `git_push_with`)。
/// **阻塞最多 [`NETWORK_TIMEOUT`]**,必须在后台线程上调用。
pub fn git_push(repo_path: &Path) -> Result<String> {
    run_git_network_command(repo_path, "push")
}

// ---------------------------------------------------------------------------
// 暂存 / 取消暂存(整文件)
//
// 一律走 git CLI:
// - 暂存要过 clean 过滤器(git-lfs、`.gitattributes` 里的 `filter=`),libgit2 不跑外部
//   过滤驱动,LFS 文件会以原始大文件入库;
// - 取消暂存用**带路径**的 `git reset -- <paths>`:不带路径的 reset(libgit2 的
//   `reset(HEAD, Mixed)` 也一样)会顺手清掉 MERGE_HEAD / rebase-merge / sequencer,
//   解决完冲突点一下「全部取消暂存」,合并提交就静默成了单亲、变基现场被毁;
// - 路径一律 `--literal-pathspecs` + `--`:文件名里的 `*` `?` `[ab]` 不许被当通配
//   (libgit2 的 `reset_default` 就把它们当 glob),`-` 开头的不许被当选项。
// ---------------------------------------------------------------------------

/// 暂存类操作的超时:本身毫秒级,但 clean 过滤器(git-lfs 给大文件算哈希)可能很慢,
/// 与改写工作区的操作同档。
const INDEX_OP_TIMEOUT: Duration = WORKTREE_OP_TIMEOUT;

/// 一次带太多路径会撞上 Windows 命令行 32K 的上限,分批跑(与 `conflict.rs` 同口径)。
const PATHS_PER_CALL: usize = 64;

/// `git --literal-pathspecs <head_args> -- <files>`,按批跑。
fn run_with_literal_paths(
    repo_path: &Path,
    head_args: &[&str],
    files: &[String],
    op_label: &str,
) -> Result<()> {
    for chunk in files.chunks(PATHS_PER_CALL) {
        let mut args = vec!["--literal-pathspecs"];
        args.extend_from_slice(head_args);
        args.push("--");
        args.extend(chunk.iter().map(String::as_str));
        run_git_command_labeled(repo_path, &args, op_label, INDEX_OP_TIMEOUT, "")?;
    }
    Ok(())
}

/// 还没有任何提交(HEAD 指向未出生的分支)。这时没有 HEAD 可以 reset 回去,
/// 取消暂存 = 从暂存区摘掉。
fn is_unborn(repo_path: &Path) -> Result<bool> {
    Ok(Repository::open(repo_path)?.head().is_err())
}

/// 暂存这些文件(`git add -A -- <paths>`:工作区里已删除的暂存为删除)。
/// **阻塞**(起 git 进程、跑 clean 过滤器),调用方放后台执行器。
pub fn git_stage(repo_path: &Path, files: &[String]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    run_with_literal_paths(repo_path, &["add", "-A"], files, "add")
}

/// 取消暂存这些文件(`git reset -q -- <paths>`,只动暂存区、不碰仓库状态)。
///
/// 已暂存的重命名连旧路径一起取消(见 [`with_rename_sources`]):结果与对两条路径各
/// `git restore --staged` 一样 —— 旧路径回到「工作区已删除」、新路径回到「未跟踪」。
pub fn git_unstage(repo_path: &Path, files: &[String]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    if is_unborn(repo_path)? {
        let repo = Repository::open(repo_path)?;
        let mut index = repo.index()?;
        for file in files {
            index.remove_path(Path::new(file))?;
        }
        index.write()?;
        return Ok(());
    }
    let files = with_rename_sources(repo_path, files)?;
    run_with_literal_paths(repo_path, &["reset", "-q"], &files, "reset")
}

/// `files` 里已暂存重命名的新路径,补上各自的旧路径(列表里已有的不重复)。
///
/// 只 reset 新路径的话,旧路径那条「已暂存删除」还挂在暂存区,一行 R 就裂成
/// 「已暂存 D + 未跟踪」。只比 HEAD 与暂存区(`StatusShow::Index`),不扫工作区。
fn with_rename_sources(repo_path: &Path, files: &[String]) -> Result<Vec<String>> {
    let repo = Repository::open(repo_path)?;
    let mut opts = StatusOptions::new();
    opts.show(git2::StatusShow::Index)
        .renames_head_to_index(true);
    let statuses = repo.statuses(Some(&mut opts))?;
    let wanted: HashSet<&str> = files.iter().map(String::as_str).collect();
    let mut out = files.to_vec();
    for entry in statuses.iter() {
        if let (new, Some(old)) = entry_paths(&entry)
            && wanted.contains(new.as_str())
            && !out.contains(&old)
        {
            out.push(old);
        }
    }
    Ok(out)
}

/// 暂存全部改动(`git add -A`,含未跟踪文件与删除)。
pub fn git_stage_all(repo_path: &Path) -> Result<()> {
    run_git_command(repo_path, &["add", "-A"], INDEX_OP_TIMEOUT, "").map(|_| ())
}

/// 取消暂存全部(`git reset -q -- .`):带路径的 reset 只把暂存区对回 HEAD,
/// 进行中的合并 / 变基 / 拣选 / 还原原样保留。
pub fn git_unstage_all(repo_path: &Path) -> Result<()> {
    if is_unborn(repo_path)? {
        let repo = Repository::open(repo_path)?;
        let mut index = repo.index()?;
        index.clear()?;
        index.write()?;
        return Ok(());
    }
    // 在仓库根上跑(执行器要求 repo_path 下有 .git),`.` 就是整个工作区
    run_git_command(
        repo_path,
        &["--literal-pathspecs", "reset", "-q", "--", "."],
        INDEX_OP_TIMEOUT,
        "",
    )
    .map(|_| ())
}

/// 走 git CLI 而非 git2:提交要继承用户的 hooks / gpg 签名 / user.name 配置。
///
/// **阻塞最多 [`LOCAL_OP_TIMEOUT`]**(会同步跑 pre-commit hook),必须在后台线程上调用。
pub fn git_commit(repo_path: &Path, message: &str) -> Result<String> {
    run_git_command_labeled(
        repo_path,
        &["commit", "-m", message],
        "commit",
        LOCAL_OP_TIMEOUT,
        "，可能卡在 pre-commit hook 或 gpg 签名上",
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

/// worktree 路径的对外形态:去掉尾部分隔符、换成本机原生分隔符。
///
/// git2 的 workdir() 与 worktree 路径一律是 `D:/x/proj/` 这种正斜杠带尾杠的写法,而
/// 项目配置里的路径不带尾杠、Windows 上扫盘得到的是反斜杠。这串既拿来展示,也被当成
/// 终端 cwd、「设为项目」的项目路径落盘 —— 不归一的话同一目录会出现两种斜杠
/// (Git 面板两个入口开出的终端 cwd 一正一反,第四轮 E2E)。
fn display_path(p: &Path) -> String {
    let raw = p.to_string_lossy();
    let trimmed = mt_core::path_key::trim_trailing_separators(&raw);
    let native = if cfg!(windows) {
        trimmed.replace('/', "\\")
    } else {
        trimmed.to_string()
    };
    // 盘符根(`D:`)要留住分隔符,否则是「D 盘的当前目录」
    if native.len() == 2 && native.ends_with(':') {
        format!("{native}{}", std::path::MAIN_SEPARATOR)
    } else {
        native
    }
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
        WORKTREE_OP_TIMEOUT,
        "，大仓库 checkout 可能较慢，请稍后刷新查看",
    );
    if result.is_ok() {
        invalidate_repo_cache();
    }
    result
}

/// [`remove_worktree`] 里要界面区别对待的两种失败(其余失败是 git 的原文)。
/// 调用方用 `err.downcast_ref::<WorktreeRemoveError>()` 认出来,换成说清原因与下一步的文案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeRemoveError {
    /// 目录里有东西被别的进程占着(终端停在里面、编辑器独占开着文件……),预检拦下,
    /// **git 一步都没做**:登记与目录都还在。`busy` 是查到的第一处占用。
    InUse { path: String, busy: String },
    /// git 摘掉了登记、目录却没删干净(预检之后才被占上),收尾补删也没成功:
    /// 登记已无、目录残留。`detail` 是补删失败的原因。
    Leftover { path: String, detail: String },
}

impl std::fmt::Display for WorktreeRemoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InUse { path, busy } => {
                write!(
                    f,
                    "worktree 目录正被占用,未做任何改动: {path}(占用处: {busy})"
                )
            }
            Self::Leftover { path, detail } => {
                write!(f, "worktree 登记已删除,但目录未能删除: {path}: {detail}")
            }
        }
    }
}

impl std::error::Error for WorktreeRemoveError {}

/// 删除前等占用放开的宽限:调用方刚关掉停在 worktree 里的终端时,shell 进程还在退出,
/// 当前目录句柄要等它真正退出才放掉。
const REMOVE_IN_USE_GRACE: Duration = Duration::from_secs(3);
/// git 删到一半之后,补删残留目录的宽限(占用方可能正在退出)。
const REMOVE_LEFTOVER_GRACE: Duration = Duration::from_secs(2);
/// 两档宽限内的轮询间隔。
const REMOVE_POLL: Duration = Duration::from_millis(100);

/// 删除 worktree(工作目录 + 主仓库里的元数据)。
/// 有未提交改动 / 已锁定时 git 会拒绝,`force=true` 对应 `--force` 强制删除。
///
/// # 不留半删状态
///
/// `git worktree remove` 先删目录,**删目录失败也照样摘掉登记**(git 源码注释原话
/// 「there's no going back from here」)。Windows 上目录里只要有一样东西被别的进程以
/// 不许删除的方式占着 —— 终端的当前目录停在里面、编辑器独占开着文件 —— 就删不掉,
/// 落下「登记没了、目录还在、界面只报 Permission denied」的半删状态(第四轮 E2E A14)。
/// 所以分两步兜:
///
/// 1. 调 git 之前先预检占用([`wait_until_deletable`]),给 [`REMOVE_IN_USE_GRACE`]
///    的宽限;过了还占着就返回 [`WorktreeRemoveError::InUse`],git 一步不做;
/// 2. git 仍然失败、且**这一次**把登记摘掉了而目录还在(预检之后才被占上):把剩下的
///    目录删掉收尾,删得掉按成功算;删不掉返回 [`WorktreeRemoveError::Leftover`]。
///
/// **阻塞调用线程**(预检要遍历整个目录,另加最多两档宽限)。
pub fn remove_worktree(repo_path: &Path, worktree_path: &str, force: bool) -> Result<String> {
    let dir = Path::new(worktree_path);
    if dir.is_dir()
        && let Err(busy) = wait_until_deletable(dir, REMOVE_IN_USE_GRACE)
    {
        return Err(WorktreeRemoveError::InUse {
            path: worktree_path.to_string(),
            busy: busy.to_string_lossy().to_string(),
        }
        .into());
    }
    let was_registered = worktree_registered(repo_path, dir);
    let mut args: Vec<&str> = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(worktree_path);
    let result = run_git_command(repo_path, &args, LOCAL_OP_TIMEOUT, "");
    match result {
        Ok(out) => {
            invalidate_repo_cache();
            Ok(out)
        }
        // 只收拾「这一次」git 摘掉了登记的情形:本来就不是登记在册的 worktree 时
        // git 一开头就拒绝,目录不归这里删
        Err(err) if was_registered && dir.exists() && !worktree_registered(repo_path, dir) => {
            invalidate_repo_cache();
            match remove_leftover_dir(dir, REMOVE_LEFTOVER_GRACE) {
                Ok(()) => Ok(String::new()),
                Err(detail) => {
                    eprintln!("[git] worktree 删到一半: {err:#}");
                    Err(WorktreeRemoveError::Leftover {
                        path: worktree_path.to_string(),
                        detail,
                    }
                    .into())
                }
            }
        }
        Err(err) => Err(err),
    }
}

/// `dir` 是不是 `repo_path` 所属仓库登记在册的 linked worktree。列不出来时按「不是」
/// 算 —— 调用方据此决定要不要替 git 收尾删目录,宁可不删。
fn worktree_registered(repo_path: &Path, dir: &Path) -> bool {
    let key = mt_core::path_key::windows_eq_key(&dir.to_string_lossy());
    list_worktrees(repo_path).is_ok_and(|list| {
        list.iter()
            .any(|wt| !wt.is_main && mt_core::path_key::windows_eq_key(&wt.path) == key)
    })
}

/// 等 `dir` 整棵子树都能删:有占用就盯着那一处等它放开,放开了再整棵复查,直到全部
/// 可删(`Ok`)或宽限用完(`Err(仍被占用的那一处)`)。非 Windows 上删除不受打开的
/// 句柄影响,直接 `Ok`。
fn wait_until_deletable(dir: &Path, grace: Duration) -> std::result::Result<(), PathBuf> {
    let deadline = Instant::now() + grace;
    loop {
        let Some(busy) = first_locked_entry(dir) else {
            return Ok(());
        };
        // 盯着已知的那一处等(只开一个句柄,便宜);它放开了才值得再走一遍整棵树
        loop {
            if Instant::now() >= deadline {
                return Err(busy);
            }
            std::thread::sleep(REMOVE_POLL);
            if !is_locked_against_delete(&busy) {
                break;
            }
        }
    }
}

/// 子树里第一处删不掉的(目录自身算在内)。只往真目录里走:符号链接 / 目录联接
/// 不跟进去 —— git 删的是链接本身,链接那头的东西与这次删除无关。
fn first_locked_entry(root: &Path) -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if is_locked_against_delete(&path) {
            return Some(path);
        }
        let is_real_dir = std::fs::symlink_metadata(&path)
            .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink());
        if !is_real_dir {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&path) {
            stack.extend(entries.flatten().map(|entry| entry.path()));
        }
    }
    None
}

/// 这一项现在删得掉吗(Windows)。以 DELETE 访问权、三种共享全开的方式试开一下:
/// 只和「没给 FILE_SHARE_DELETE」的既有句柄冲突(报共享冲突)—— 正好是会让删除失败的
/// 那一类:进程的当前目录、独占打开的文件。本应用自己的目录监听句柄给了
/// FILE_SHARE_DELETE,不算占用(它不挡删除)。其余错误(不存在、无权限)不归
/// 「占用」管,交给 git 自己报。
#[cfg(windows)]
fn is_locked_against_delete(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    // winnt.h / winbase.h 的原值(windows-sys 没开 Storage_FileSystem 这组 feature)
    const DELETE: u32 = 0x0001_0000;
    const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x1 | 0x2 | 0x4;
    // 开目录必须带;链接只开它本身,不跟到目标
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    match std::fs::OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ_WRITE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(_) => false,
        Err(err) => err.raw_os_error() == Some(ERROR_SHARING_VIOLATION),
    }
}

#[cfg(not(windows))]
fn is_locked_against_delete(_path: &Path) -> bool {
    false
}

/// git 删到一半留下的目录:在宽限内反复删,删掉(或已经没了)返回 `Ok`,
/// 宽限用完返回最后一次的错误原文。
fn remove_leftover_dir(dir: &Path, grace: Duration) -> std::result::Result<(), String> {
    let deadline = Instant::now() + grace;
    loop {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) if Instant::now() >= deadline => return Err(err.to_string()),
            Err(_) => std::thread::sleep(REMOVE_POLL),
        }
    }
}

/// 清理失效的 worktree 元数据(目录已被手动删除的条目)。
pub fn prune_worktrees(repo_path: &Path) -> Result<String> {
    let result = run_git_command(repo_path, &["worktree", "prune"], QUICK_OP_TIMEOUT, "");
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

// 编辑器 gutter 变更标记 / 注解(blame)的后端见 blame / line_diff 的模块注释
mod blame;
mod branch;
mod combined;
mod commit;
mod conflict;
mod hunk;
mod line_diff;
mod log_ops;
mod log_query;
mod repo_state;
mod slide;
mod stash;
mod sync;
mod tags;
pub use blame::*;
pub use branch::*;
pub use combined::*;
pub use commit::*;
pub use conflict::*;
pub use hunk::*;
pub use line_diff::*;
pub use log_ops::*;
pub use log_query::*;
pub use repo_state::*;
pub use stash::*;
pub use sync::*;
pub use tags::*;

#[cfg(test)]
mod test_support;

#[cfg(test)]
#[path = "index_exec_tests.rs"]
mod index_exec_tests;

#[cfg(test)]
#[path = "changes_tests.rs"]
mod changes_tests;

#[cfg(test)]
mod tests {
    use super::test_support::init_repo_with_file;
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
        #[cfg(not(windows))]
        assert_eq!(display_path(Path::new("/home/u/proj/")), "/home/u/proj");
    }

    /// libgit2 给的 `D:/x/proj/` 在 Windows 上换成原生反斜杠(展示、终端 cwd、
    /// 「设为项目」都用这一串);盘符根留住分隔符。
    #[cfg(windows)]
    #[test]
    fn display_path_uses_native_separators_on_windows() {
        assert_eq!(
            display_path(Path::new("D:/Git/repo-wt/")),
            r"D:\Git\repo-wt"
        );
        assert_eq!(
            display_path(Path::new("//wsl.localhost/Ubuntu/home/u/p/")),
            r"\\wsl.localhost\Ubuntu\home\u\p"
        );
        assert_eq!(display_path(Path::new("D:/")), r"D:\");
    }

    /// 一个主仓库 + 一个 linked worktree(`<主仓库>-wt`,分支 `wt-branch`),
    /// worktree 里带一个子目录。返回 `(主仓库, worktree)`。
    fn repo_with_worktree(tag: &str) -> (PathBuf, PathBuf) {
        use super::test_support::git;
        let main = init_repo_with_file(tag, "a.txt", "a\n");
        let wt = main.with_file_name(format!(
            "{}-wt",
            main.file_name().unwrap().to_string_lossy()
        ));
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "wt-branch",
                &wt.to_string_lossy(),
            ],
        );
        std::fs::create_dir_all(wt.join("sub").join("deep")).unwrap();
        (main, wt)
    }

    /// 正常删除:登记与目录一起没了,`list_worktrees` 只剩主仓库。
    #[test]
    fn remove_worktree_removes_registration_and_dir() {
        let (main, wt) = repo_with_worktree("wt-remove");
        let wt_str = display_path(&wt);
        assert!(worktree_registered(&main, &wt));
        remove_worktree(&main, &wt_str, true).unwrap();
        assert!(!wt.exists(), "目录应已删除");
        assert!(!worktree_registered(&main, &wt), "登记应已摘除");
        super::test_support::cleanup(&[&main]);
    }

    /// 被占用时(这里用一个当前目录停在 worktree 子目录里的进程,与「终端开在里面」
    /// 同形)预检拦下:不调 git,登记与目录都还在,错误能认出是 `InUse`;
    /// 占用放开后再删就成功(第四轮 E2E A14 的半删状态)。
    #[cfg(windows)]
    #[test]
    fn remove_worktree_in_use_leaves_everything_intact() {
        let (main, wt) = repo_with_worktree("wt-busy");
        let wt_str = display_path(&wt);
        // 直接起 ping(不经 cmd):经 cmd 的话杀掉 cmd 后孙进程 ping 还攥着当前目录
        let mut holder = std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .current_dir(wt.join("sub").join("deep"))
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("起不了占位进程");
        // 进程起来、当前目录句柄挂上之前预检可能还看不到占用 —— 等它真被认出来
        let deadline = Instant::now() + Duration::from_secs(10);
        while first_locked_entry(&wt).is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            first_locked_entry(&wt).is_some(),
            "当前目录停在里面的进程应被认作占用"
        );

        let err = remove_worktree(&main, &wt_str, true).unwrap_err();
        match err.downcast_ref::<WorktreeRemoveError>() {
            Some(WorktreeRemoveError::InUse { path, busy }) => {
                assert_eq!(path, &wt_str);
                assert!(busy.contains("sub"), "占用处应指到子目录: {busy}");
            }
            other => panic!("应为 InUse,实际 {other:?} / {err:#}"),
        }
        assert!(wt.join("a.txt").exists(), "git 一步都不该做:文件还在");
        assert!(worktree_registered(&main, &wt), "登记还在");

        holder.kill().ok();
        holder.wait().ok();
        remove_worktree(&main, &wt_str, true).unwrap();
        assert!(!wt.exists());
        assert!(!worktree_registered(&main, &wt));
        super::test_support::cleanup(&[&main]);
    }

    /// 本应用自己的目录监听句柄(给了 FILE_SHARE_DELETE)不挡删除,不能算占用;
    /// 独占打开(不给 FILE_SHARE_DELETE)的文件才算。
    #[cfg(windows)]
    #[test]
    fn share_delete_handles_are_not_in_use() {
        use std::os::windows::fs::OpenOptionsExt;
        let (main, wt) = repo_with_worktree("wt-share");
        let shared = std::fs::File::open(wt.join("a.txt")).unwrap();
        assert_eq!(first_locked_entry(&wt), None, "Rust 默认共享模式含 DELETE");
        drop(shared);
        let exclusive = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1)
            .open(wt.join("a.txt"))
            .unwrap();
        assert_eq!(first_locked_entry(&wt), Some(wt.join("a.txt")));
        drop(exclusive);
        assert_eq!(first_locked_entry(&wt), None);
        super::test_support::cleanup(&[&main, &wt]);
    }

    /// git 删到一半(登记已摘、目录残留)时的收尾:目录删得掉就删。这里直接摆出
    /// 「登记没了、目录还在」的局面,验证收尾那一步本身。
    #[test]
    fn remove_leftover_dir_finishes_or_reports() {
        let root = std::env::temp_dir().join(format!(
            "mini-term-wt-leftover-{}",
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("x.txt"), "x").unwrap();
        assert_eq!(
            remove_leftover_dir(&root, Duration::from_millis(200)),
            Ok(())
        );
        assert!(!root.exists());
        // 已经没了也算收尾成功
        assert_eq!(
            remove_leftover_dir(&root, Duration::from_millis(200)),
            Ok(())
        );
    }

    /// 两种失败的原文要带上路径(日志里看得出是哪个目录)。
    #[test]
    fn worktree_remove_error_display_mentions_path() {
        let in_use = WorktreeRemoveError::InUse {
            path: "D:/repo-wt".into(),
            busy: "D:/repo-wt/sub".into(),
        };
        assert!(in_use.to_string().contains("D:/repo-wt/sub"));
        let leftover = WorktreeRemoveError::Leftover {
            path: "D:/repo-wt".into(),
            detail: "拒绝访问".into(),
        };
        let text = leftover.to_string();
        assert!(text.contains("D:/repo-wt") && text.contains("拒绝访问"));
        // 走 anyhow 之后还认得出来
        let err: anyhow::Error = leftover.clone().into();
        assert_eq!(err.downcast_ref::<WorktreeRemoveError>(), Some(&leftover));
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
        let result = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
        assert_eq!(diff_lines(&result), old_to_new());
        std::fs::remove_dir_all(&root).ok();

        // 比 MAX_DISCOVER_PARENTS 深得多也一样
        let rel = "a/b/c/d/e/f/g/h/i/j.txt";
        let root = init_repo_with_file("deeper", rel, "old\n");
        std::fs::write(root.join(rel), "new\n").unwrap();
        let result = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
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
        let result =
            get_git_diff(&project, "src/pages/task/my/my.vue", DiffSide::Unstaged).unwrap();
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
        let result = get_git_diff(&root, rel, DiffSide::Staged).unwrap();
        assert_eq!(
            diff_lines(&result),
            vec![("delete".to_string(), "old".to_string())]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    fn changed_lines(result: &GitDiffResult) -> Vec<(String, String)> {
        diff_lines(result)
            .into_iter()
            .filter(|(kind, _)| kind != "context")
            .collect()
    }

    fn pair(kind: &str, content: &str) -> (String, String) {
        (kind.to_string(), content.to_string())
    }

    /// 部分暂存:首行改动已暂存、末行改动未暂存。三侧各看各的 ——
    /// 「未暂存」只剩末行(按块暂存/丢弃的前提),「已暂存」只有首行,「工作区」两处都在。
    #[test]
    fn diff_sides_split_partially_staged_file() {
        let rel = "a.txt";
        let root = init_repo_with_file("sides", rel, "a\nb\nc\nd\ne\nf\ng\nh\n");
        std::fs::write(root.join(rel), "A\nb\nc\nd\ne\nf\ng\nh\n").unwrap();
        git_stage(&root, &[rel.to_string()]).unwrap();
        std::fs::write(root.join(rel), "A\nb\nc\nd\ne\nf\ng\nH\n").unwrap();

        let unstaged = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
        assert_eq!(
            changed_lines(&unstaged),
            vec![pair("delete", "h"), pair("add", "H")]
        );
        let staged = get_git_diff(&root, rel, DiffSide::Staged).unwrap();
        assert_eq!(
            changed_lines(&staged),
            vec![pair("delete", "a"), pair("add", "A")]
        );
        let working = get_git_diff(&root, rel, DiffSide::Working).unwrap();
        assert_eq!(
            changed_lines(&working),
            vec![
                pair("delete", "a"),
                pair("add", "A"),
                pair("delete", "h"),
                pair("add", "H"),
            ]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 工作区删除(未暂存):旧实现读工作区文件直接报「找不到文件」,现在新侧按空算。
    #[test]
    fn unstaged_diff_of_deleted_file_is_full_delete() {
        let rel = "gone.txt";
        let root = init_repo_with_file("wt-delete", rel, "x\ny\n");
        std::fs::remove_file(root.join(rel)).unwrap();
        let result = get_git_diff(&root, rel, DiffSide::Unstaged).unwrap();
        assert_eq!(
            changed_lines(&result),
            vec![pair("delete", "x"), pair("delete", "y")]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 未跟踪文件不在暂存区:未暂存一侧整份算新增。
    #[test]
    fn unstaged_diff_of_untracked_file_is_full_add() {
        let root = init_repo_with_file("untracked", "a.txt", "a\n");
        std::fs::write(root.join("new.txt"), "n1\nn2\n").unwrap();
        let result = get_git_diff(&root, "new.txt", DiffSide::Unstaged).unwrap();
        assert_eq!(
            changed_lines(&result),
            vec![pair("add", "n1"), pair("add", "n2")]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 退出码非零不算 `Err`:由调用方按 stderr 自己判(merge 冲突、未合并分支删除等)。
    #[test]
    fn run_git_output_reports_failure_without_err() {
        let root = init_repo_with_file("output", "a.txt", "a\n");
        let out = run_git_output(
            &root,
            &["rev-parse", "--verify", "refs/heads/definitely-missing"],
            "rev-parse",
            Duration::from_secs(20),
            "",
        )
        .unwrap();
        assert!(!out.success);
        assert!(!out.stderr.trim().is_empty());
        let ok = run_git_output(
            &root,
            &["rev-parse", "--abbrev-ref", "HEAD"],
            "rev-parse",
            Duration::from_secs(20),
            "",
        )
        .unwrap();
        assert!(ok.success, "stderr: {}", ok.stderr);
        assert!(!ok.stdout.trim().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    /// 超时说明整句全角标点、句末有句号(E2E r4 A10:「超时(120s),…。请确认…SSH key」
    /// 半角括号 / 逗号混排、句末无句号);删过 index.lock 时另起一行。
    #[test]
    fn 超时说明全角标点() {
        let push = timeout_message("push", 120, NETWORK_HINT, None);
        assert_eq!(
            push,
            "git push 超时（120s），可能在等待凭证或网络故障，请确认已配置凭证管理器或 SSH key。"
        );
        let bare = timeout_message("commit", 60, "", None);
        assert_eq!(bare, "git commit 超时（60s）。");
        let locked = timeout_message("add", 30, "", Some(Path::new("/r/.git/index.lock")));
        assert!(
            locked.starts_with("git add 超时（30s）。\n已终止 git 及其子进程，"),
            "{locked}"
        );
        for text in [&push, &bare, &locked] {
            // 写给人看的部分不出现半角括号与逗号(git 原文与路径不在其中)
            let prose = text.split("index.lock").next().unwrap();
            assert!(
                !prose.contains('(') && !prose.contains(')') && !prose.contains(','),
                "{text}"
            );
        }
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

    /// 提交的改动文件按 `git show` 口径配对重命名:纯重命名是一条 renamed(带旧路径、
    /// diff 无改动行),改名又小改也是 renamed(diff 只有改的那行);内容毫不相干的
    /// 一删一增仍是两条。
    #[test]
    fn 提交里的重命名配对成一条() {
        use super::test_support::git;

        let body = "1\n2\n3\n4\n5\n6\n7\n8\n";
        let root = init_repo_with_file("commit-rename", "old.txt", body);
        std::fs::write(root.join("gone.txt"), "x\n").unwrap();
        git(&root, &["add", "gone.txt"]);
        git(&root, &["commit", "-q", "-m", "add gone"]);

        let commit_of = |root: &Path| git(root, &["rev-parse", "HEAD"]);
        let shape = |files: &[CommitFileInfo]| -> Vec<(String, String, Option<String>)> {
            files
                .iter()
                .map(|f| (f.path.clone(), f.status.clone(), f.old_path.clone()))
                .collect()
        };

        // 纯重命名
        git(&root, &["mv", "old.txt", "new.txt"]);
        git(&root, &["commit", "-q", "-m", "pure rename"]);
        let pure = commit_of(&root);
        let files = get_commit_files(&root, &pure).unwrap();
        assert_eq!(
            shape(&files),
            vec![("new.txt".into(), "renamed".into(), Some("old.txt".into()))]
        );
        let diff = get_commit_file_diff(&root, &pure, "new.txt", Some("old.txt")).unwrap();
        assert!(diff.hunks.is_empty(), "{:?}", diff.hunks);

        // 改名又改一行:仍配成重命名,diff 只剩那一行
        git(&root, &["mv", "new.txt", "moved.txt"]);
        std::fs::write(root.join("moved.txt"), body.replace("5\n", "five\n")).unwrap();
        git(&root, &["add", "moved.txt"]);
        git(&root, &["commit", "-q", "-m", "rename and edit"]);
        let edited = commit_of(&root);
        let files = get_commit_files(&root, &edited).unwrap();
        assert_eq!(
            shape(&files),
            vec![("moved.txt".into(), "renamed".into(), Some("new.txt".into()))]
        );
        let diff = get_commit_file_diff(&root, &edited, "moved.txt", Some("new.txt")).unwrap();
        let changed: Vec<(String, String)> = diff_lines(&diff)
            .into_iter()
            .filter(|(kind, _)| kind != "context")
            .collect();
        assert_eq!(
            changed,
            vec![
                ("delete".to_string(), "5".to_string()),
                ("add".to_string(), "five".to_string()),
            ]
        );

        // 内容不相干的一删一增不配对
        git(&root, &["rm", "-q", "gone.txt"]);
        std::fs::write(root.join("fresh.txt"), "完全不同的内容\n").unwrap();
        git(&root, &["add", "fresh.txt"]);
        git(&root, &["commit", "-q", "-m", "delete and add"]);
        let files = get_commit_files(&root, &commit_of(&root)).unwrap();
        let mut got = shape(&files);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("fresh.txt".into(), "added".into(), None),
                ("gone.txt".into(), "deleted".into(), None),
            ]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 当前分支按名字认:新建的分支与原分支指着同一提交时,只有 HEAD 指着的那条是
    /// `is_head`(按提交比的话两条都是,历史行会把原分支画成当前分支);游离 HEAD 时
    /// 一条都不是。
    #[test]
    fn 当前分支按名字认_同一提交的其它分支不算() {
        let root = init_repo_with_file("is-head", "a.txt", "a\n");
        let repo = Repository::open(&root).unwrap();
        let first = repo
            .head()
            .unwrap()
            .shorthand()
            .expect("初始分支")
            .to_string();
        let tip = {
            let commit = repo.head().unwrap().peel_to_commit().unwrap();
            repo.branch("feature", &commit, false).unwrap();
            commit.id()
        };
        // 先只建不切:HEAD 还在初始分支上
        let current = |root: &Path| -> Vec<String> {
            get_repo_branches(root)
                .unwrap()
                .into_iter()
                .filter(|b| b.is_head)
                .map(|b| b.name)
                .collect()
        };
        assert_eq!(current(&root), vec![first.clone()]);

        // 切到新分支:两条仍指着同一提交,当前分支跟着 HEAD 换过去
        repo.set_head("refs/heads/feature").unwrap();
        assert_eq!(current(&root), vec!["feature".to_string()]);
        let all = get_repo_branches(&root).unwrap();
        assert!(
            all.iter()
                .filter(|b| !b.is_remote)
                .all(|b| b.commit_hash == tip.to_string()),
            "{all:?}"
        );

        // 游离 HEAD:即便与两条分支同一提交,也没有当前分支
        repo.set_head_detached(tip).unwrap();
        assert!(current(&root).is_empty());
        drop(repo);
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
