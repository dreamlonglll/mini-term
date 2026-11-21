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
use mt_core::path::canonicalize;
use parking_lot::Mutex;
use pathdiff::diff_paths;
use serde::{Deserialize, Serialize};

use crate::encoding::{Decoded, TextEncoding};

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
    /// 有一侧不是 UTF-8 时解出来的编码(新的一侧优先;两侧都是 UTF-8 时为 `None`)。
    /// 有值时 diff 照常显示,但**按块暂存 / 取消暂存 / 丢弃一律拒绝**:块操作是按 UTF-8
    /// 文本拼接再写回字节的,非 UTF-8 文件拼回去就写坏了(见 [`hunk`] 的 `pick_hunk`)。
    /// 跨端保留编码安全信息;缺失字段不能被当成 UTF-8。
    #[serde(deserialize_with = "serde::Deserialize::deserialize")]
    pub encoding: Option<TextEncoding>,
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

mod cli;
mod diff_engine;
mod discover;
mod index_ops;
mod log_walk;
mod status;
mod worktree;
use cli::*;
pub use cli::{git_pull, git_push};
use diff_engine::*;
pub use diff_engine::{DiffSide, diff_two_texts, get_git_diff, get_git_diff_from};
use discover::*;
pub use discover::{discover_git_repos, enclosing_repo, invalidate_repo_cache};
use index_ops::*;
pub use index_ops::{
    git_commit, git_discard_file, git_stage, git_stage_all, git_unstage, git_unstage_all,
};
use log_walk::*;
pub use log_walk::{
    GitLogCursor, get_commit_file_diff, get_commit_files, get_git_log, get_repo_branches,
    merge_log_page,
};
use status::*;
pub use status::{get_changes_status, get_git_status};
pub use worktree::{
    WorktreeInfo, WorktreeRemoveError, add_worktree, get_worktree_branches, list_worktrees,
    prune_worktrees, remove_worktree,
};

// 编辑器 gutter 变更标记 / 注解(blame)的后端见 blame / line_diff 的模块注释
mod blame;
mod branch;
mod bulk_index;
mod combined;
mod commit;
mod commit_extras;
mod conflict;
mod conflict_blocks;
mod hunk;
mod line_diff;
mod log_index;
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
pub use commit_extras::*;
pub use conflict::*;
pub use conflict_blocks::*;
pub use hunk::*;
pub use line_diff::*;
pub use log_index::*;
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
#[path = "perf_guard_tests.rs"]
mod perf_guard_tests;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
