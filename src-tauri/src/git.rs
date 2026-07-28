use git2::{Repository, RepositoryOpenFlags, Status, StatusOptions};
use pathdiff::diff_paths;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Clone)]
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
// Task 2: get_git_status implementation
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
) -> Result<Vec<GitFileStatus>, String> {
    let is_empty_repo = repo.head().is_err();

    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);

    let statuses = repo.statuses(Some(&mut opts)).map_err(|e| e.to_string())?;

    let mut result = Vec::new();
    for entry in statuses.iter() {
        let raw_path = entry.path().unwrap_or("").to_string();
        let s = entry.status();

        let git_status = match map_status(s, is_empty_repo) {
            Some(gs) => gs,
            None => continue,
        };

        let label = status_label(&git_status).to_string();

        // Compute path relative to path_prefix (if given), else use raw_path
        let display_path = if let Some(prefix) = path_prefix {
            let repo_workdir = repo.workdir().unwrap_or_else(|| repo.path());
            let abs = repo_workdir.join(&raw_path);
            diff_paths(&abs, prefix)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|| raw_path.clone())
        } else {
            raw_path.clone()
        };

        // old_path for renames
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
    pub path: String,
    pub current_branch: Option<String>,
    /// 该条目是不是某个主仓库的 linked worktree(前端据此显示 ⎇ 标识)
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
    /// 全部父提交 hash（按 git 顺序：第 0 个是主线父）。前端据此绘制分支拓扑图。
    pub parent_hashes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CommitFileInfo {
    pub path: String,
    pub status: String,
    pub old_path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    pub is_remote: bool,
    pub commit_hash: String,
}

const MAX_DISCOVER_PARENTS: usize = 5;

fn discover_repo_limited(start: &Path) -> Option<Repository> {
    let mut ceiling = start.to_path_buf();
    for _ in 0..MAX_DISCOVER_PARENTS {
        match ceiling.parent() {
            Some(p) if p != ceiling => ceiling = p.to_path_buf(),
            _ => break,
        }
    }
    Repository::open_ext(start, RepositoryOpenFlags::empty(), &[&ceiling]).ok()
}

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
fn invalidate_repo_cache() {
    REPO_PATH_CACHE.lock().unwrap().clear();
}

fn find_repos_cached_paths(project_path: &Path) -> Vec<RepoPathEntry> {
    let key = project_path.to_path_buf();
    {
        let cache = REPO_PATH_CACHE.lock().unwrap();
        if let Some((ts, entries)) = cache.get(&key) {
            if ts.elapsed() < REPO_CACHE_TTL {
                return entries
                    .iter()
                    .map(|e| RepoPathEntry {
                        name: e.name.clone(),
                        path: e.path.clone(),
                        is_worktree: e.is_worktree,
                    })
                    .collect();
            }
        }
    }
    let entries = discover_repo_paths(project_path);
    {
        let mut cache = REPO_PATH_CACHE.lock().unwrap();
        cache.insert(
            key,
            (
                Instant::now(),
                entries
                    .iter()
                    .map(|e| RepoPathEntry {
                        name: e.name.clone(),
                        path: e.path.clone(),
                        is_worktree: e.is_worktree,
                    })
                    .collect(),
            ),
        );
    }
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
    entries
}

/// Scan project_path for git repositories.
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

#[tauri::command]
pub fn get_git_status(project_path: String) -> Result<Vec<GitFileStatus>, String> {
    let path = Path::new(&project_path);
    let repos = find_repos(path);

    if repos.is_empty() {
        return Ok(Vec::new());
    }

    let mut all = Vec::new();
    for (_, _, repo, _) in &repos {
        if let Ok(mut files) = collect_repo_status(repo, Some(path)) {
            all.append(&mut files);
        }
    }
    Ok(all)
}

#[tauri::command]
pub fn get_changes_status(repo_path: String) -> Result<Vec<ChangeFileStatus>, String> {
    let path = Path::new(&repo_path);
    let repo = Repository::open(path).map_err(|e| e.to_string())?;
    let is_empty_repo = repo.head().is_err();

    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);

    let statuses = repo.statuses(Some(&mut opts)).map_err(|e| e.to_string())?;
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

#[tauri::command]
pub fn discover_git_repos(project_path: String) -> Result<Vec<GitRepoInfo>, String> {
    let path = Path::new(&project_path);
    let repos = find_repos(path);
    Ok(repos
        .into_iter()
        .map(|(name, abs_path, repo, is_worktree)| {
            let current_branch = repo.head().ok().and_then(|h| {
                if h.is_branch() {
                    h.shorthand().map(|s| s.to_string())
                } else {
                    // detached HEAD — show short hash
                    h.target().map(|oid| {
                        let s = oid.to_string();
                        format!("({})", &s[..7.min(s.len())])
                    })
                }
            });
            GitRepoInfo {
                name,
                path: abs_path.to_string_lossy().to_string(),
                current_branch,
                is_worktree,
            }
        })
        .collect())
}

#[tauri::command]
pub fn get_git_log(
    repo_path: String,
    before_commit: Option<String>,
    limit: Option<usize>,
    branch: Option<String>,
) -> Result<Vec<GitCommitInfo>, String> {
    let path = Path::new(&repo_path);
    let repo = Repository::open(path).map_err(|e| e.to_string())?;
    let limit = limit.unwrap_or(30);

    let mut revwalk = repo.revwalk().map_err(|e| e.to_string())?;
    // 加 TOPOLOGICAL：保证父提交永远排在子提交之后，否则时钟偏移/rebase 后的仓库
    // 会出现父在子之前，前端拓扑图的连线就会断。
    revwalk
        .set_sorting(git2::Sort::TIME | git2::Sort::TOPOLOGICAL)
        .map_err(|e| e.to_string())?;

    if let Some(ref hash) = before_commit {
        let oid = git2::Oid::from_str(hash).map_err(|e| e.to_string())?;
        let commit = repo.find_commit(oid).map_err(|e| e.to_string())?;
        for parent_id in commit.parent_ids() {
            revwalk.push(parent_id).map_err(|e| e.to_string())?;
        }
    } else if let Some(ref b) = branch {
        // 先找本地 refs/heads/<b>,再找远程 refs/remotes/<b>
        // worktree 持有的分支也在 refs/heads/ 下(与主 repo 共享 refs 存储),天然支持
        let local_ref = format!("refs/heads/{}", b);
        let remote_ref = format!("refs/remotes/{}", b);
        let reference = repo
            .find_reference(&local_ref)
            .or_else(|_| repo.find_reference(&remote_ref))
            .map_err(|_| format!("未找到分支:{}", b))?;
        let oid = reference
            .target()
            .ok_or_else(|| format!("分支 {} 无有效 target", b))?;
        revwalk.push(oid).map_err(|e| e.to_string())?;
    } else {
        revwalk.push_head().map_err(|e| e.to_string())?;
    }

    let mut result = Vec::with_capacity(limit);
    for oid_result in revwalk {
        if result.len() >= limit {
            break;
        }
        let oid = oid_result.map_err(|e| e.to_string())?;
        let commit = repo.find_commit(oid).map_err(|e| e.to_string())?;
        let hash = oid.to_string();
        let short_hash = hash[..7.min(hash.len())].to_string();
        let message = commit.summary().unwrap_or("").to_string();
        let body = commit.body().map(|s| s.to_string());
        let author = commit.author().name().unwrap_or("unknown").to_string();
        let timestamp = commit.time().seconds();
        let parent_hashes = commit.parent_ids().map(|id| id.to_string()).collect();
        result.push(GitCommitInfo {
            hash,
            short_hash,
            message,
            body,
            author,
            timestamp,
            parent_hashes,
        });
    }

    Ok(result)
}

#[tauri::command]
pub fn get_repo_branches(repo_path: String) -> Result<Vec<BranchInfo>, String> {
    let path = Path::new(&repo_path);
    let repo = Repository::open(path).map_err(|e| e.to_string())?;

    let head_target = repo.head().ok().and_then(|h| h.target());

    let mut branches = Vec::new();

    // Local branches
    for branch_result in repo
        .branches(Some(git2::BranchType::Local))
        .map_err(|e| e.to_string())?
    {
        let (branch, _) = branch_result.map_err(|e| e.to_string())?;
        let name = branch
            .name()
            .map_err(|e| e.to_string())?
            .unwrap_or("")
            .to_string();
        if let Some(target) = branch.get().target() {
            branches.push(BranchInfo {
                name,
                is_head: head_target == Some(target),
                is_remote: false,
                commit_hash: target.to_string(),
            });
        }
    }

    // Remote branches
    for branch_result in repo
        .branches(Some(git2::BranchType::Remote))
        .map_err(|e| e.to_string())?
    {
        let (branch, _) = branch_result.map_err(|e| e.to_string())?;
        let name = branch
            .name()
            .map_err(|e| e.to_string())?
            .unwrap_or("")
            .to_string();
        // Skip HEAD pointer like origin/HEAD
        if name.ends_with("/HEAD") {
            continue;
        }
        if let Some(target) = branch.get().target() {
            branches.push(BranchInfo {
                name,
                is_head: false,
                is_remote: true,
                commit_hash: target.to_string(),
            });
        }
    }

    Ok(branches)
}

#[tauri::command]
pub fn get_commit_files(
    repo_path: String,
    commit_hash: String,
) -> Result<Vec<CommitFileInfo>, String> {
    let repo = Repository::open(Path::new(&repo_path)).map_err(|e| e.to_string())?;
    let oid = git2::Oid::from_str(&commit_hash).map_err(|e| e.to_string())?;
    let commit = repo.find_commit(oid).map_err(|e| e.to_string())?;
    let tree = commit.tree().map_err(|e| e.to_string())?;

    let parent_tree = if commit.parent_count() > 0 {
        Some(
            commit
                .parent(0)
                .map_err(|e| e.to_string())?
                .tree()
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };

    let diff = repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)
        .map_err(|e| e.to_string())?;

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

#[tauri::command]
pub fn get_commit_file_diff(
    repo_path: String,
    commit_hash: String,
    file_path: String,
    old_file_path: Option<String>,
) -> Result<GitDiffResult, String> {
    let repo = Repository::open(Path::new(&repo_path)).map_err(|e| e.to_string())?;
    let oid = git2::Oid::from_str(&commit_hash).map_err(|e| e.to_string())?;
    let commit = repo.find_commit(oid).map_err(|e| e.to_string())?;
    let tree = commit.tree().map_err(|e| e.to_string())?;

    let parent_tree = if commit.parent_count() > 0 {
        Some(
            commit
                .parent(0)
                .map_err(|e| e.to_string())?
                .tree()
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };

    let new_content = match tree.get_path(Path::new(&file_path)) {
        Ok(entry) => {
            let obj = entry.to_object(&repo).map_err(|e| e.to_string())?;
            let blob = obj.as_blob().ok_or("not a blob")?;
            if blob.is_binary() {
                return Ok(GitDiffResult {
                    old_content: String::new(),
                    new_content: String::new(),
                    hunks: Vec::new(),
                    is_binary: true,
                    too_large: false,
                });
            }
            if blob.content().len() > 1_048_576 {
                return Ok(GitDiffResult {
                    old_content: String::new(),
                    new_content: String::new(),
                    hunks: Vec::new(),
                    is_binary: false,
                    too_large: true,
                });
            }
            std::str::from_utf8(blob.content())
                .map_err(|_| "binary".to_string())?
                .to_string()
        }
        Err(_) => String::new(),
    };

    let old_lookup_path = old_file_path.as_deref().unwrap_or(&file_path);
    let old_content = if let Some(ref pt) = parent_tree {
        match pt.get_path(Path::new(old_lookup_path)) {
            Ok(entry) => {
                let obj = entry.to_object(&repo).map_err(|e| e.to_string())?;
                let blob = obj.as_blob().ok_or("not a blob")?;
                if blob.is_binary() {
                    return Ok(GitDiffResult {
                        old_content: String::new(),
                        new_content: String::new(),
                        hunks: Vec::new(),
                        is_binary: true,
                        too_large: false,
                    });
                }
                std::str::from_utf8(blob.content())
                    .map_err(|_| "binary".to_string())?
                    .to_string()
            }
            Err(_) => String::new(),
        }
    } else {
        String::new()
    };

    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();

    let ol = old_lines.len() as u64;
    let nl = new_lines.len() as u64;

    let hunks = if ol * nl > 10_000_000 {
        full_replace_diff(&old_content, &new_content)
    } else {
        build_hunks(&old_lines, &new_lines)
    };

    Ok(GitDiffResult {
        old_content,
        new_content,
        hunks,
        is_binary: false,
        too_large: false,
    })
}

// ---------------------------------------------------------------------------
// get_git_diff implementation
// ---------------------------------------------------------------------------

fn get_head_content(repo: &Repository, rel_path: &str) -> Result<Option<String>, String> {
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok(None), // empty repo
    };
    let tree = head.peel_to_tree().map_err(|e| e.to_string())?;
    let entry = match tree.get_path(Path::new(rel_path)) {
        Ok(e) => e,
        Err(_) => return Ok(Some(String::new())), // file not yet in HEAD
    };
    let obj = entry.to_object(repo).map_err(|e| e.to_string())?;
    let blob = obj.as_blob().ok_or("not a blob")?;

    if blob.is_binary() {
        return Err("binary".to_string());
    }
    let content = std::str::from_utf8(blob.content())
        .map_err(|_| "binary".to_string())?
        .to_string();
    Ok(Some(content))
}

// LCS-based diff producing DiffHunks (context = 3 lines)
fn build_hunks(old_lines: &[&str], new_lines: &[&str]) -> Vec<DiffHunk> {
    let m = old_lines.len();
    let n = new_lines.len();

    // LCS DP table
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            if old_lines[i] == new_lines[j] {
                dp[i][j] = dp[i + 1][j + 1] + 1;
            } else {
                dp[i][j] = dp[i + 1][j].max(dp[i][j + 1]);
            }
        }
    }

    // Produce flat edit list: ('=', old_i, new_j) | ('-', old_i, _) | ('+', _, new_j)
    let mut flat: Vec<(char, usize, usize)> = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < m || j < n {
        if i < m && j < n && old_lines[i] == new_lines[j] {
            flat.push(('=', i, j));
            i += 1;
            j += 1;
        } else if j < n && (i >= m || dp[i][j + 1] >= dp[i + 1][j]) {
            flat.push(('+', i, j));
            j += 1;
        } else {
            flat.push(('-', i, j));
            i += 1;
        }
    }

    // Group into hunks (context = 3 lines)
    const CONTEXT: usize = 3;
    let mut hunks: Vec<DiffHunk> = Vec::new();

    // Find ranges of non-equal edits, expand with context
    let changed_indices: Vec<usize> = flat
        .iter()
        .enumerate()
        .filter(|(_, (k, _, _))| *k != '=')
        .map(|(idx, _)| idx)
        .collect();

    if changed_indices.is_empty() {
        return hunks;
    }

    // Group changed indices into contiguous ranges (with context)
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
                        // new_start might be the next insert; approximate
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

#[tauri::command]
pub fn get_git_diff(
    project_path: String,
    file_path: String,
    staged: Option<bool>,
) -> Result<GitDiffResult, String> {
    let project = Path::new(&project_path);
    let abs_file = project.join(&file_path);

    let repo = discover_repo_limited(&abs_file).ok_or_else(|| {
        format!(
            "no git repository found within {} parents of {}",
            MAX_DISCOVER_PARENTS,
            abs_file.display()
        )
    })?;
    let workdir = repo.workdir().ok_or("bare repository not supported")?;

    let rel_path =
        diff_paths(&abs_file, workdir).ok_or("file is outside repository working directory")?;
    let rel_str = rel_path.to_string_lossy().replace('\\', "/");

    let is_staged = staged.unwrap_or(false);

    // Read new content: from index (staged) or working tree (unstaged)
    let new_content = if is_staged {
        let index = repo.index().map_err(|e| e.to_string())?;
        match index.get_path(Path::new(&rel_str), 0) {
            Some(entry) => {
                let blob = repo.find_blob(entry.id).map_err(|e| e.to_string())?;
                if blob.is_binary() {
                    return Ok(GitDiffResult {
                        old_content: String::new(),
                        new_content: String::new(),
                        hunks: Vec::new(),
                        is_binary: true,
                        too_large: false,
                    });
                }
                if blob.content().len() > 1_048_576 {
                    return Ok(GitDiffResult {
                        old_content: String::new(),
                        new_content: String::new(),
                        hunks: Vec::new(),
                        is_binary: false,
                        too_large: true,
                    });
                }
                std::str::from_utf8(blob.content())
                    .map_err(|_| "binary".to_string())?
                    .to_string()
            }
            None => String::new(),
        }
    } else {
        let new_bytes = std::fs::read(&abs_file).map_err(|e| e.to_string())?;
        if new_bytes.len() > 1_048_576 {
            return Ok(GitDiffResult {
                old_content: String::new(),
                new_content: String::new(),
                hunks: Vec::new(),
                is_binary: false,
                too_large: true,
            });
        }
        match std::str::from_utf8(&new_bytes) {
            Ok(s) => s.to_string(),
            Err(_) => {
                return Ok(GitDiffResult {
                    old_content: String::new(),
                    new_content: String::new(),
                    hunks: Vec::new(),
                    is_binary: true,
                    too_large: false,
                })
            }
        }
    };

    let old_content = match get_head_content(&repo, &rel_str)? {
        None => String::new(),
        Some(s) => s,
    };

    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines_vec: Vec<&str> = new_content.lines().collect();

    let ol = old_lines.len() as u64;
    let nl = new_lines_vec.len() as u64;

    let hunks = if ol * nl > 10_000_000 {
        full_replace_diff(&old_content, &new_content)
    } else {
        build_hunks(&old_lines, &new_lines_vec)
    };

    Ok(GitDiffResult {
        old_content,
        new_content,
        hunks,
        is_binary: false,
        too_large: false,
    })
}

/// 在 Windows GUI 应用(windows_subsystem = "windows")下 spawn console 子进程
/// (比如 git.exe)默认会弹出 conhost 黑框,并且窗口创建/焦点切换会让 UI 感知卡顿。
/// 这里统一给 `Command` 加 CREATE_NO_WINDOW 抑制掉控制台分配。
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
///   虽然不保证立刻 kill,但主线程不再被阻塞)
fn run_git_command(
    repo_path: &str,
    args: &[&str],
    timeout: Duration,
    timeout_hint: &str,
) -> Result<String, String> {
    let repo = Path::new(repo_path);
    if !repo.is_dir() {
        return Err(format!("不是有效目录:{}", repo_path));
    }
    // worktree 目录下 `.git` 是文件而非目录,exists() 两者皆真
    if !repo.join(".git").exists() {
        return Err(format!("不是 git 仓库(缺少 .git):{}", repo_path));
    }

    let op = args.join(" ");
    let (tx, rx) = std::sync::mpsc::channel();
    let repo_path_owned = repo_path.to_string();
    let args_owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    std::thread::spawn(move || {
        let mut cmd = std::process::Command::new("git");
        cmd.args(&args_owned)
            .current_dir(&repo_path_owned)
            .stdin(std::process::Stdio::null());
        hide_console_window(&mut cmd);
        let result = cmd.output();
        // 忽略发送失败:主线程超时后接收端已被 drop
        let _ = tx.send(result);
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => {
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).to_string())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).to_string())
            }
        }
        Ok(Err(e)) => Err(format!("启动 git {} 失败:{}", op, e)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "git {} 超时({}s){}",
            op,
            timeout.as_secs(),
            timeout_hint
        )),
        Err(e) => Err(format!("git {} 通信错误:{}", op, e)),
    }
}

fn run_git_network_command(repo_path: &str, op: &'static str) -> Result<String, String> {
    run_git_command(
        repo_path,
        &[op],
        Duration::from_secs(30),
        ",可能在等待凭证或网络故障。请确认已配置凭证管理器或 SSH key",
    )
}

// 必须用 `(async)`:Tauri 的同步 `#[tauri::command]` 会在主线程(WebView 事件循环)上执行,
// 而本函数内部 `recv_timeout(30s)` 是阻塞等待 —— 那会卡住整个 UI(间歇性卡顿的根因)。
// 标记 (async) 后 Tauri 通过 async_runtime::spawn 在 worker 线程运行,主线程不再被阻塞;
// 内部的「独立线程跑 git + mpsc + 30s 超时」逻辑保持不变,只是阻塞等待挪到了 worker 线程。
#[tauri::command(async)]
pub fn git_pull(repo_path: String) -> Result<String, String> {
    run_git_network_command(&repo_path, "pull")
}

#[tauri::command(async)]
pub fn git_push(repo_path: String) -> Result<String, String> {
    run_git_network_command(&repo_path, "push")
}

#[tauri::command]
pub fn git_stage(repo_path: String, files: Vec<String>) -> Result<(), String> {
    let repo = Repository::open(Path::new(&repo_path)).map_err(|e| e.to_string())?;
    let mut index = repo.index().map_err(|e| e.to_string())?;
    for file in &files {
        let path = Path::new(file);
        let abs_path = repo.workdir().ok_or("bare repo")?.join(path);
        if abs_path.exists() {
            index.add_path(path).map_err(|e| e.to_string())?;
        } else {
            // 文件已删除，需要从 index 移除
            index.remove_path(path).map_err(|e| e.to_string())?;
        }
    }
    index.write().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn git_unstage(repo_path: String, files: Vec<String>) -> Result<(), String> {
    let repo = Repository::open(Path::new(&repo_path)).map_err(|e| e.to_string())?;

    let head = match repo.head() {
        Ok(h) => Some(h.peel_to_commit().map_err(|e| e.to_string())?),
        Err(_) => None, // empty repo, no HEAD
    };

    if let Some(ref commit) = head {
        for file in &files {
            repo.reset_default(Some(commit.as_object()), [file.as_str()])
                .map_err(|e| e.to_string())?;
        }
    } else {
        // empty repo: 批量从 index 移除，最后一次 write
        let mut index = repo.index().map_err(|e| e.to_string())?;
        for file in &files {
            index
                .remove_path(Path::new(file))
                .map_err(|e| e.to_string())?;
        }
        index.write().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub fn git_stage_all(repo_path: String) -> Result<(), String> {
    let repo = Repository::open(Path::new(&repo_path)).map_err(|e| e.to_string())?;
    let mut index = repo.index().map_err(|e| e.to_string())?;
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .map_err(|e| e.to_string())?;

    // 处理已删除的文件：遍历 index，移除工作区中不存在的文件
    let workdir = repo.workdir().ok_or("bare repo")?;
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
        index
            .remove_path(Path::new(&path))
            .map_err(|e| e.to_string())?;
    }

    index.write().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn git_unstage_all(repo_path: String) -> Result<(), String> {
    let repo = Repository::open(Path::new(&repo_path)).map_err(|e| e.to_string())?;

    match repo.head() {
        Ok(head) => {
            let commit = head.peel_to_commit().map_err(|e| e.to_string())?;
            repo.reset(commit.as_object(), git2::ResetType::Mixed, None)
                .map_err(|e| e.to_string())?;
        }
        Err(_) => {
            // empty repo: 清空整个 index
            let mut index = repo.index().map_err(|e| e.to_string())?;
            index.clear().map_err(|e| e.to_string())?;
            index.write().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[tauri::command]
pub fn git_commit(repo_path: String, message: String) -> Result<String, String> {
    let repo = Path::new(&repo_path);
    if !repo.is_dir() {
        return Err(format!("不是有效目录:{}", repo_path));
    }
    if !repo.join(".git").exists() {
        return Err(format!("不是 git 仓库(缺少 .git):{}", repo_path));
    }

    let mut cmd = std::process::Command::new("git");
    cmd.args(["commit", "-m", &message])
        .current_dir(&repo_path)
        .stdin(std::process::Stdio::null());
    hide_console_window(&mut cmd);
    let output = cmd
        .output()
        .map_err(|e| format!("启动 git commit 失败:{}", e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

#[tauri::command]
pub fn git_discard_file(repo_path: String, files: Vec<String>) -> Result<(), String> {
    let repo = Repository::open(Path::new(&repo_path)).map_err(|e| e.to_string())?;
    let workdir = repo.workdir().ok_or("bare repo")?.to_path_buf();

    for file in &files {
        let abs_path = workdir.join(file);

        // 检查是否 untracked (WT_NEW)
        // 注意:StatusOptions::new() 默认不含未跟踪文件,必须显式开 include_untracked,
        // 否则新增文件永远查不到 WT_NEW,会被误当作已跟踪文件走 checkout_head(对其无效),
        // 表现为「丢弃新增文件没有反应」。
        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .pathspec(file);
        let statuses = repo.statuses(Some(&mut opts)).map_err(|e| e.to_string())?;
        let is_untracked = statuses.iter().any(|e| e.status().contains(Status::WT_NEW));

        if is_untracked {
            // untracked: 直接删除文件
            if abs_path.exists() {
                std::fs::remove_file(&abs_path).map_err(|e| e.to_string())?;
            }
        } else {
            // tracked: 先 unstage（如果在暂存区），再 checkout HEAD 版本
            let head = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
            if let Some(ref commit) = head {
                // unstage
                let _ = repo.reset_default(Some(commit.as_object()), [file.as_str()]);
            }
            // checkout from HEAD
            repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force().path(file)))
                .map_err(|e| e.to_string())?;
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
/// 统一后前端才能做「该 worktree 是否已是项目」的对比。
fn display_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.trim_end_matches(['/', '\\']).to_string()
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
#[tauri::command]
pub fn list_worktrees(repo_path: String) -> Result<Vec<WorktreeInfo>, String> {
    let repo = Repository::open(Path::new(&repo_path)).map_err(|e| e.to_string())?;
    // 从 linked worktree 打开时回到主仓库:linked worktree 的 gitdir 形如
    // `<main>/.git/worktrees/<name>`,上溯两级即主仓库 .git(git2 0.19 未暴露 commondir)
    let main_repo = if repo.is_worktree() {
        let git_dir = repo.path().to_path_buf();
        let main_git = git_dir
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| "无法定位主仓库".to_string())?;
        Repository::open(main_git).map_err(|e| e.to_string())?
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
            let is_locked = matches!(
                wt.is_locked(),
                Ok(git2::WorktreeLockStatus::Locked(_))
            );
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
/// 大仓库的首次 checkout 可能较慢,超时给到 120s。
#[tauri::command(async)]
pub fn add_worktree(
    repo_path: String,
    worktree_path: String,
    branch: String,
    create_branch: bool,
    base: Option<String>,
) -> Result<String, String> {
    let mut args: Vec<&str> = vec!["worktree", "add"];
    if create_branch {
        args.push("-b");
        args.push(&branch);
        args.push(&worktree_path);
        if let Some(ref b) = base {
            if !b.is_empty() {
                args.push(b);
            }
        }
    } else {
        args.push(&worktree_path);
        args.push(&branch);
    }
    let result = run_git_command(
        &repo_path,
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
#[tauri::command(async)]
pub fn remove_worktree(
    repo_path: String,
    worktree_path: String,
    force: bool,
) -> Result<String, String> {
    let mut args: Vec<&str> = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&worktree_path);
    let result = run_git_command(&repo_path, &args, Duration::from_secs(60), "");
    if result.is_ok() {
        invalidate_repo_cache();
    }
    result
}

/// 清理失效的 worktree 元数据(目录已被手动删除的条目)。
#[tauri::command(async)]
pub fn prune_worktrees(repo_path: String) -> Result<String, String> {
    let result = run_git_command(
        &repo_path,
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
#[tauri::command]
pub fn get_worktree_branches(paths: Vec<String>) -> Vec<Option<String>> {
    paths
        .into_iter()
        .map(|p| {
            if p.starts_with(r"\\") {
                return None;
            }
            let repo = Repository::open(Path::new(&p)).ok()?;
            if !repo.is_worktree() {
                return None;
            }
            head_branch(&repo)
        })
        .collect()
}
