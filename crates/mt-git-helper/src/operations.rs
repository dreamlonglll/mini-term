use mt_project::git;
use serde::{Deserialize, Serialize};

/// Reads missing from the original CLI protocol. Paths are interpreted only on
/// the helper host; callers never inspect them with the desktop filesystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum GitQuery {
    DiscoverGitRepos,
    GetGitStatus,
    RepoSignature,
    RepoOperationState,
    ListConflicts,
    HasConflictMarkers {
        file: String,
    },
    AmendInfo,
    HeadExists,
    CommitTarget,
    RecentCommitMessages {
        limit: usize,
    },
    GetRepoBranches,
    GetGitLog {
        after: Option<git::GitLogCursor>,
        limit: Option<usize>,
        branch: Option<String>,
    },
    GetGitLogFiltered {
        after: Option<git::GitLogCursor>,
        limit: usize,
        branch: Option<String>,
        filter: git::LogFilter,
    },
    CurrentHeadLabel,
    ConfigUserEmail,
    ListTags,
    GetCommitLineStats {
        hash: String,
    },
    GetBranchesContaining {
        hash: String,
    },
    GetCombinedCommitFiles {
        hashes: Vec<String>,
    },
    CommitRelation {
        hash: String,
    },
    ResetDropsPushed {
        target: String,
    },
    ListRemoteNames,
    GetGitDiffFrom {
        file_path: String,
        old_path: Option<String>,
        side: git::DiffSide,
    },
    GetRevisionFileDiff {
        base: Option<git::FileRevision>,
        head: Option<git::FileRevision>,
    },
    LocalChangesConflictWith {
        branch: String,
    },
    PushPlan {
        branch: Option<String>,
    },
    OutgoingCommits {
        branch: Option<String>,
        limit: usize,
    },
    StashFiles {
        stash_hash: String,
    },
    GetFileLog {
        file_rel: String,
        after: Option<git::GitLogCursor>,
        limit: usize,
    },
    ResolveRepoFile {
        file_rel: String,
    },
    IsInsideGitRepo {
        abs_file: String,
    },
    ListWorktrees,
    ExistingDirectories {
        paths: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum GitMutation {
    CheckoutCommit {
        hash: String,
    },
    CreateBranchAt {
        name: String,
        hash: String,
        checkout: bool,
    },
    CherryPick {
        hash: String,
    },
    RevertCommit {
        hash: String,
    },
    ResetTo {
        hash: String,
        mode: git::ResetMode,
    },
    UndoLastCommit {
        expected_head: String,
    },
    RewordHead {
        expected_head: String,
        message: String,
    },
    CreateTag {
        name: String,
        hash: String,
        message: Option<String>,
    },
    DeleteTag {
        name: String,
    },
    PushTag {
        remote: String,
        name: String,
    },
    DeleteRemoteBranch {
        remote_branch: String,
    },
    MergeBranch {
        name: String,
    },
    RebaseOnto {
        onto: String,
    },
    StashApply {
        index: usize,
        reinstate_index: bool,
        expected_hash: Option<String>,
    },
    StashPop {
        index: usize,
        reinstate_index: bool,
        expected_hash: Option<String>,
    },
    StashDrop {
        index: usize,
        expected_hash: Option<String>,
    },
    StashClear,
    StashBranch {
        name: String,
        index: usize,
        expected_hash: Option<String>,
    },
    CheckoutWithStash {
        branch: String,
    },
    AddWorktree {
        worktree_path: String,
        branch: String,
        create_branch: bool,
        base: Option<String>,
    },
    RemoveWorktree {
        worktree_path: String,
        force: bool,
    },
    PruneWorktrees,
}
