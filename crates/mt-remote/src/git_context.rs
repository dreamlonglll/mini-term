//! Explicit, captured Git backend for both local and SSH projects. All business
//! calls block; construction, identity, cloning and close perform no network IO.
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use anyhow::{Result, bail};
use mt_config::SshConnection;
use mt_git_helper::{GitMutation as M, GitQuery as Q, Operation as O, Reply};
use mt_project::git::{self, *};
use serde::de::DeserializeOwned;

use crate::{RemoteGitWorkspace, connection_fingerprint, lock};

static REMOTES: LazyLock<Mutex<Vec<Weak<RemoteBackend>>>> = LazyLock::new(Default::default);

struct RemoteBackend {
    project_id: String,
    connection: SshConnection,
    fingerprint: u64,
    root: String,
    workspace: RemoteGitWorkspace,
}

#[derive(Clone)]
enum Backend {
    Local,
    Remote(Arc<RemoteBackend>),
    Broken(Arc<str>),
}

#[derive(Clone)]
pub struct GitContext {
    backend: Backend,
    path: String,
}

impl std::fmt::Debug for GitContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitContext")
            .field("key", &self.key())
            .field("path", &self.path)
            .finish()
    }
}
impl PartialEq for GitContext {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}
impl Eq for GitContext {}

impl GitContext {
    pub fn local(path: impl Into<String>) -> Self {
        Self {
            backend: Backend::Local,
            path: path.into(),
        }
    }

    pub fn remote(project_id: String, connection: SshConnection, root: String) -> Self {
        let fingerprint = connection_fingerprint(&connection);
        let mut remotes = lock(&REMOTES);
        remotes.retain(|remote| remote.strong_count() != 0);
        let backend = remotes
            .iter()
            .filter_map(Weak::upgrade)
            .find(|remote| {
                remote.project_id == project_id
                    && remote.connection.id == connection.id
                    && remote.fingerprint == fingerprint
                    && remote.root == root
            })
            .unwrap_or_else(|| {
                let backend = Arc::new(RemoteBackend {
                    workspace: RemoteGitWorkspace::new(
                        project_id.clone(),
                        connection.clone(),
                        root.clone(),
                    ),
                    project_id,
                    connection,
                    fingerprint,
                    root: root.clone(),
                });
                remotes.push(Arc::downgrade(&backend));
                backend
            });
        Self {
            backend: Backend::Remote(backend),
            path: root,
        }
    }

    pub fn broken(message: impl Into<String>) -> Self {
        Self {
            backend: Backend::Broken(Arc::from(message.into())),
            path: String::new(),
        }
    }
    pub fn with_path(&self, path: impl Into<String>) -> Self {
        Self {
            backend: self.backend.clone(),
            path: path.into(),
        }
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    /// Context identity binds attribution and permission to the source project/root.
    pub fn key(&self) -> String {
        match &self.backend {
            Backend::Remote(remote) => format!(
                "project|{}:{}|{}:{}|{}:{}|{:016x}|{}",
                remote.project_id.len(),
                remote.project_id,
                remote.root.len(),
                remote.root,
                remote.connection.id.len(),
                remote.connection.id,
                remote.fingerprint,
                self.repository_key()
            ),
            _ => self.repository_key(),
        }
    }
    /// Backend + repository identity used for shared mutation gates, not authorization.
    pub fn repository_key(&self) -> String {
        match &self.backend {
            Backend::Local => {
                #[cfg(windows)]
                let path = mt_core::path_key::windows_eq_key(&self.path);
                #[cfg(not(windows))]
                let path = mt_core::path_key::posix_trim_trailing(&self.path);
                format!("local|{}:{path}", path.len())
            }
            Backend::Remote(remote) => {
                let path = mt_core::path_key::posix_trim_trailing(&self.path);
                crate::git_service::repository_identity(&remote.connection, path)
            }
            Backend::Broken(message) => format!(
                "broken|{}:{message}|{}:{}",
                message.len(),
                self.path.len(),
                self.path
            ),
        }
    }
    pub fn is_remote(&self) -> bool {
        matches!(self.backend, Backend::Remote(_))
    }
    pub fn local_path(&self) -> Option<&Path> {
        matches!(self.backend, Backend::Local).then(|| Path::new(&self.path))
    }
    pub fn connection(&self) -> Option<&SshConnection> {
        match &self.backend {
            Backend::Remote(remote) => Some(&remote.connection),
            _ => None,
        }
    }
    pub fn connection_id(&self) -> Option<&str> {
        self.connection().map(|connection| connection.id.as_str())
    }
    pub fn project_id(&self) -> Option<&str> {
        match &self.backend {
            Backend::Remote(remote) => Some(&remote.project_id),
            _ => None,
        }
    }
    pub fn close(&self) {
        if let Backend::Remote(remote) = &self.backend {
            remote.workspace.close();
        }
    }

    pub fn write_outcome_unknown(&self) -> bool {
        matches!(&self.backend, Backend::Remote(remote) if remote.workspace.write_outcome_unknown())
    }

    pub fn unknown_write(&self) -> Option<crate::RemoteGitUnknownWrite> {
        match &self.backend {
            Backend::Remote(remote) => remote.workspace.unknown_write(),
            _ => None,
        }
    }
    pub fn write_running(&self) -> bool {
        matches!(&self.backend, Backend::Remote(remote) if remote.workspace.write_running())
    }
    pub fn recovery_snapshot(&self) -> Result<crate::RemoteGitRecoverySnapshot> {
        match &self.backend {
            Backend::Remote(remote) => remote
                .workspace
                .recovery_snapshot_blocking()
                .map_err(mt_git_helper::GitFailure::into_error),
            Backend::Broken(message) => bail!("{message}"),
            Backend::Local => bail!("remote_git_no_unknown_write"),
        }
    }
    pub fn acknowledge_recovery(&self, snapshot: &crate::RemoteGitRecoverySnapshot) -> Result<()> {
        match &self.backend {
            Backend::Remote(remote) => remote
                .workspace
                .acknowledge_recovery_blocking(snapshot)
                .map_err(mt_git_helper::GitFailure::into_error),
            Backend::Broken(message) => bail!("{message}"),
            Backend::Local => bail!("remote_git_no_unknown_write"),
        }
    }

    fn request(&self, operation: O) -> Result<Reply> {
        match &self.backend {
            Backend::Remote(remote) => remote
                .workspace
                .request_typed(operation)
                .map_err(mt_git_helper::GitFailure::into_error),
            Backend::Broken(message) => bail!("{message}"),
            Backend::Local => unreachable!("local calls dispatch directly to core"),
        }
    }
    fn data<T: DeserializeOwned + Send + 'static>(&self, operation: O) -> Result<T> {
        match &self.backend {
            Backend::Remote(remote) => remote
                .workspace
                .request_data(operation)
                .map_err(mt_git_helper::GitFailure::into_error),
            Backend::Broken(message) => bail!("{message}"),
            Backend::Local => unreachable!("local calls dispatch directly to core"),
        }
    }
    fn query<T: DeserializeOwned + Send + 'static>(&self, query: Q) -> Result<T> {
        self.data(O::Query {
            repo: self.path.clone(),
            query,
        })
    }
    fn mutate<T: DeserializeOwned + Send + 'static>(&self, mutation: M) -> Result<T> {
        self.data(O::Mutate {
            repo: self.path.clone(),
            mutation,
        })
    }

    pub fn head_exists(&self) -> bool {
        match &self.backend {
            Backend::Local => git::head_exists(Path::new(&self.path)),
            _ => self.query(Q::HeadExists).unwrap_or(false),
        }
    }
    pub fn config_user_email(&self) -> Option<String> {
        match &self.backend {
            Backend::Local => git::config_user_email(Path::new(&self.path)),
            _ => self.query(Q::ConfigUserEmail).ok().flatten(),
        }
    }
    pub fn is_inside_git_repo(&self, abs_file: &Path) -> bool {
        match &self.backend {
            Backend::Local => git::is_inside_git_repo(Path::new(&self.path), abs_file),
            _ => self
                .query(Q::IsInsideGitRepo {
                    abs_file: abs_file.to_string_lossy().into_owned(),
                })
                .unwrap_or(false),
        }
    }
    pub fn existing_directories(&self, paths: &[String]) -> Result<Vec<String>> {
        match &self.backend {
            Backend::Local => {
                let mut existing = Vec::new();
                for path in paths {
                    match std::fs::metadata(path) {
                        Ok(metadata) if metadata.is_dir() => existing.push(path.clone()),
                        Ok(_) => bail!("directory target is not a directory: {path}"),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok(existing)
            }
            _ => self.query(Q::ExistingDirectories {
                paths: paths.to_vec(),
            }),
        }
    }
    pub fn get_git_diff(&self, file_path: &str, side: git::DiffSide) -> Result<GitDiffResult> {
        self.get_git_diff_from(file_path, None, side)
    }
    pub fn get_changes_status(&self) -> Result<Vec<ChangeFileStatus>> {
        if let Backend::Local = &self.backend {
            return git::get_changes_status(Path::new(&self.path));
        }
        match self.request(O::Status {
            repo: self.path.clone(),
        })? {
            Reply::Status { files } => Ok(files),
            other => bail!("expected status reply, got {}", other.kind()),
        }
    }
    pub fn git_stage(&self, files: &[String]) -> Result<()> {
        self.update_files(files, true)
    }
    pub fn git_unstage(&self, files: &[String]) -> Result<()> {
        self.update_files(files, false)
    }
    fn update_files(&self, files: &[String], stage: bool) -> Result<()> {
        if let Backend::Local = &self.backend {
            return if stage {
                git::git_stage(Path::new(&self.path), files)
            } else {
                git::git_unstage(Path::new(&self.path), files)
            };
        }
        let repo = self.path.clone();
        let paths = files.to_vec();
        match self.request(if stage {
            O::Stage { repo, paths }
        } else {
            O::Unstage { repo, paths }
        })? {
            Reply::Updated => Ok(()),
            other => bail!("expected updated reply, got {}", other.kind()),
        }
    }
    pub fn git_stage_all(&self) -> Result<()> {
        self.update_all(true)
    }
    pub fn git_unstage_all(&self) -> Result<()> {
        self.update_all(false)
    }
    fn update_all(&self, stage: bool) -> Result<()> {
        if let Backend::Local = &self.backend {
            return if stage {
                git::git_stage_all(Path::new(&self.path))
            } else {
                git::git_unstage_all(Path::new(&self.path))
            };
        }
        let repo = self.path.clone();
        match self.request(if stage {
            O::StageAll { repo }
        } else {
            O::UnstageAll { repo }
        })? {
            Reply::Updated => Ok(()),
            other => bail!("expected updated reply, got {}", other.kind()),
        }
    }
    pub fn checkout_branch(&self, name: &str) -> Result<()> {
        if let Backend::Local = &self.backend {
            return git::checkout_branch(Path::new(&self.path), name);
        }
        let _: Option<RemoteCheckout> = self.data(O::Checkout {
            repo: self.path.clone(),
            branch: name.into(),
            remote: false,
        })?;
        Ok(())
    }
    pub fn checkout_remote_branch(&self, remote_branch: &str) -> Result<RemoteCheckout> {
        if let Backend::Local = &self.backend {
            return git::checkout_remote_branch(Path::new(&self.path), remote_branch);
        }
        self.data::<Option<RemoteCheckout>>(O::Checkout {
            repo: self.path.clone(),
            branch: remote_branch.into(),
            remote: true,
        })?
        .ok_or_else(|| anyhow::anyhow!("remote checkout returned no outcome"))
    }
}

// Each method preserves the original core signature after removing its first
// Path argument. Remote request allocation happens only in the remote branch.
macro_rules! methods {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $result:ty => $this:ident, $remote:expr;)*) => {
        impl GitContext { $(
            pub fn $name(&self, $($arg: $ty),*) -> Result<$result> {
                match &self.backend {
                    Backend::Local => git::$name(Path::new(&self.path), $($arg),*),
                    _ => { let $this = self; $remote }
                }
            }
        )* }
    }
}

methods! {
    discover_git_repos() -> Vec<GitRepoInfo> => s, s.query(Q::DiscoverGitRepos);
    get_git_status() -> Vec<GitFileStatus> => s, s.query(Q::GetGitStatus);
    repo_signature() -> RepoSignature => s, s.query(Q::RepoSignature);
    repo_operation_state() -> RepoOpState => s, s.query(Q::RepoOperationState);
    repo_panel_snapshot(recent_limit: usize) -> RepoPanelSnapshot => s, s.data(O::PanelSnapshot { repo: s.path.clone(), recent_limit });
    git_fetch_all() -> FetchOutcome => s, s.data(O::Fetch { repo: s.path.clone() });
    git_pull_with(mode: PullMode) -> PullOutcome => s, s.data(O::Pull { repo: s.path.clone(), mode: Some(mode) });
    git_pull_by_config() -> PullOutcome => s, s.data(O::Pull { repo: s.path.clone(), mode: None });
    continue_operation(kind: RepoOpKind) -> RepoOpState => s, s.data(O::OperationContinue { repo: s.path.clone(), kind });
    abort_operation(kind: RepoOpKind) -> () => s, s.data(O::OperationAbort { repo: s.path.clone(), kind });
    list_conflicts() -> Vec<ConflictInfo> => s, s.query(Q::ListConflicts);
    resolve_conflict_with(file: &str, side: ConflictSide) -> () => s, s.data(O::ResolveConflict { repo: s.path.clone(), path: file.into(), side });
    mark_conflict_resolved(file: &str) -> () => s, s.data(O::MarkResolved { repo: s.path.clone(), path: file.into() });
    has_conflict_markers(file: &str) -> bool => s, s.query(Q::HasConflictMarkers { file: file.into() });
    discard_unstaged(files: &[String]) -> () => s, s.data(O::Discard { repo: s.path.clone(), paths: files.to_vec() });
    git_commit_with(message: &str, amend: bool) -> CommitOutcome => s, s.data(O::CommitWith { repo: s.path.clone(), message: message.into(), amend });
    amend_info() -> Option<AmendInfo> => s, s.query(Q::AmendInfo);
    commit_target() -> CommitTarget => s, s.query(Q::CommitTarget);
    recent_commit_messages(limit: usize) -> Vec<CommitMessageEntry> => s, s.query(Q::RecentCommitMessages { limit });
    get_repo_branches() -> Vec<BranchInfo> => s, s.query(Q::GetRepoBranches);
    get_git_log(after: Option<&GitLogCursor>, limit: Option<usize>, branch: Option<&str>) -> Vec<GitCommitInfo> => s, s.query(Q::GetGitLog { after: after.cloned(), limit, branch: branch.map(Into::into) });
    get_git_log_filtered(after: Option<GitLogCursor>, limit: usize, branch: Option<&str>, filter: &LogFilter) -> LogPage => s, s.query(Q::GetGitLogFiltered { after, limit, branch: branch.map(Into::into), filter: filter.clone() });
    current_head_label() -> Option<String> => s, s.query(Q::CurrentHeadLabel);
    list_tags() -> Vec<TagInfo> => s, s.query(Q::ListTags);
    get_commit_detail(hash: &str) -> CommitDetail => s, s.data(O::CommitDetail { repo: s.path.clone(), hash: hash.into() });
    get_commit_line_stats(hash: &str) -> Option<CommitLineStats> => s, s.query(Q::GetCommitLineStats { hash: hash.into() });
    get_branches_containing(hash: &str) -> Vec<ContainingBranch> => s, s.query(Q::GetBranchesContaining { hash: hash.into() });
    get_combined_commit_files(hashes: &[String]) -> Vec<CombinedFileChange> => s, s.query(Q::GetCombinedCommitFiles { hashes: hashes.to_vec() });
    commit_relation(hash: &str) -> CommitRelation => s, s.query(Q::CommitRelation { hash: hash.into() });
    reset_drops_pushed(target: &str) -> bool => s, s.query(Q::ResetDropsPushed { target: target.into() });
    checkout_commit(hash: &str) -> () => s, s.mutate(M::CheckoutCommit { hash: hash.into() });
    create_branch_at(name: &str, hash: &str, checkout: bool) -> () => s, s.mutate(M::CreateBranchAt { name: name.into(), hash: hash.into(), checkout });
    cherry_pick(hash: &str) -> PickOutcome => s, s.mutate(M::CherryPick { hash: hash.into() });
    revert_commit(hash: &str) -> PickOutcome => s, s.mutate(M::RevertCommit { hash: hash.into() });
    reset_to(hash: &str, mode: ResetMode) -> () => s, s.mutate(M::ResetTo { hash: hash.into(), mode });
    undo_last_commit(expected_head: &str) -> String => s, s.mutate(M::UndoLastCommit { expected_head: expected_head.into() });
    reword_head(expected_head: &str, message: &str) -> String => s, s.mutate(M::RewordHead { expected_head: expected_head.into(), message: message.into() });
    create_tag(name: &str, hash: &str, message: Option<&str>) -> () => s, s.mutate(M::CreateTag { name: name.into(), hash: hash.into(), message: message.map(Into::into) });
    delete_tag(name: &str) -> () => s, s.mutate(M::DeleteTag { name: name.into() });
    push_tag(remote: &str, name: &str) -> () => s, s.mutate(M::PushTag { remote: remote.into(), name: name.into() });
    list_remote_names() -> Vec<String> => s, s.query(Q::ListRemoteNames);
    get_git_diff_from(file_path: &str, old_path: Option<&str>, side: git::DiffSide) -> GitDiffResult => s, s.query(Q::GetGitDiffFrom { file_path: file_path.into(), old_path: old_path.map(Into::into), side });
    get_commit_files(commit_hash: &str) -> Vec<CommitFileInfo> => s, s.data(O::CommitFiles { repo: s.path.clone(), hash: commit_hash.into() });
    get_commit_file_diff(commit_hash: &str, file_path: &str, old_file_path: Option<&str>) -> GitDiffResult => s, s.data(O::CommitDiff { repo: s.path.clone(), hash: commit_hash.into(), path: file_path.into(), old_path: old_file_path.map(Into::into) });
    get_revision_file_diff(base: Option<&FileRevision>, head: Option<&FileRevision>) -> GitDiffResult => s, s.query(Q::GetRevisionFileDiff { base: base.cloned(), head: head.cloned() });
    stage_hunk(file_path: &str, diff: &GitDiffResult, hunk_index: usize) -> () => s, s.data(O::StageHunk { repo: s.path.clone(), path: file_path.into(), diff: diff.clone(), hunk: hunk_index });
    unstage_hunk(file_path: &str, diff: &GitDiffResult, hunk_index: usize) -> () => s, s.data(O::UnstageHunk { repo: s.path.clone(), path: file_path.into(), diff: diff.clone(), hunk: hunk_index });
    discard_hunk(file_path: &str, diff: &GitDiffResult, hunk_index: usize) -> () => s, s.data(O::DiscardHunk { repo: s.path.clone(), path: file_path.into(), diff: diff.clone(), hunk: hunk_index });
    local_changes_conflict_with(branch: &str) -> Vec<String> => s, s.query(Q::LocalChangesConflictWith { branch: branch.into() });
    create_branch(name: &str, start: Option<&str>, checkout: bool) -> () => s, s.data(O::CreateBranch { repo: s.path.clone(), name: name.into(), start: start.map(Into::into), checkout });
    rename_branch(old: &str, new: &str) -> () => s, s.data(O::RenameBranch { repo: s.path.clone(), old: old.into(), name: new.into() });
    delete_branch(name: &str, force: bool) -> DeleteOutcome => s, s.data(O::DeleteBranch { repo: s.path.clone(), name: name.into(), force });
    delete_remote_branch(remote_branch: &str) -> () => s, s.mutate(M::DeleteRemoteBranch { remote_branch: remote_branch.into() });
    merge_branch(name: &str) -> MergeOutcome => s, s.mutate(M::MergeBranch { name: name.into() });
    rebase_onto(onto: &str) -> RebaseOutcome => s, s.mutate(M::RebaseOnto { onto: onto.into() });
    push_plan(branch: Option<&str>) -> PushPlan => s, s.query(Q::PushPlan { branch: branch.map(Into::into) });
    outgoing_commits(branch: Option<&str>, limit: usize) -> OutgoingCommits => s, s.query(Q::OutgoingCommits { branch: branch.map(Into::into), limit });
    git_push_with(options: &PushOptions) -> PushOutcome => s, s.data(O::Push { repo: s.path.clone(), options: options.clone() });
    list_stashes() -> Vec<StashInfo> => s, s.data(O::ListStashes { repo: s.path.clone() });
    stash_files(stash_hash: &str) -> Vec<StashFileInfo> => s, s.query(Q::StashFiles { stash_hash: stash_hash.into() });
    stash_save(options: &StashSaveOptions) -> StashSaveOutcome => s, s.data(O::SaveStash { repo: s.path.clone(), options: options.clone() });
    stash_apply(index: usize, reinstate_index: bool, expected_hash: Option<&str>) -> StashApplyOutcome => s, s.mutate(M::StashApply { index, reinstate_index, expected_hash: expected_hash.map(Into::into) });
    stash_pop(index: usize, reinstate_index: bool, expected_hash: Option<&str>) -> StashApplyOutcome => s, s.mutate(M::StashPop { index, reinstate_index, expected_hash: expected_hash.map(Into::into) });
    stash_drop(index: usize, expected_hash: Option<&str>) -> () => s, s.mutate(M::StashDrop { index, expected_hash: expected_hash.map(Into::into) });
    stash_clear() -> () => s, s.mutate(M::StashClear);
    stash_branch(name: &str, index: usize, expected_hash: Option<&str>) -> StashApplyOutcome => s, s.mutate(M::StashBranch { name: name.into(), index, expected_hash: expected_hash.map(Into::into) });
    checkout_with_stash(branch: &str) -> SmartCheckoutOutcome => s, s.mutate(M::CheckoutWithStash { branch: branch.into() });
    get_file_log(file_rel: &str, after: Option<GitLogCursor>, limit: usize) -> FileLogPage => s, s.query(Q::GetFileLog { file_rel: file_rel.into(), after, limit });
    resolve_repo_file(file_rel: &str) -> (PathBuf, String) => s, s.query(Q::ResolveRepoFile { file_rel: file_rel.into() });
    list_worktrees() -> Vec<WorktreeInfo> => s, s.query(Q::ListWorktrees);
    add_worktree(worktree_path: &str, branch: &str, create_branch: bool, base: Option<&str>) -> String => s, s.mutate(M::AddWorktree { worktree_path: worktree_path.into(), branch: branch.into(), create_branch, base: base.map(Into::into) });
    remove_worktree(worktree_path: &str, force: bool) -> String => s, s.mutate(M::RemoveWorktree { worktree_path: worktree_path.into(), force });
    prune_worktrees() -> String => s, s.mutate(M::PruneWorktrees);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> SshConnection {
        SshConnection {
            id: "git-context-identity-test".into(),
            name: "test".into(),
            host: "one.invalid".into(),
            port: 22,
            user: "tester".into(),
            password: None,
            identity_file: None,
            group: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn remote_identity_is_host_scoped_and_construction_reuses_workspace() {
        let a = GitContext::remote("project".into(), connection(), "/Project".into());
        let same = GitContext::remote("project".into(), connection(), "/Project".into());
        let (Backend::Remote(a_backend), Backend::Remote(same_backend)) =
            (&a.backend, &same.backend)
        else {
            panic!("remote");
        };
        assert!(Arc::ptr_eq(a_backend, same_backend));
        assert_eq!(a, same);
        assert_ne!(a.with_path("/Project/Repo"), a.with_path("/Project/repo"));
        assert_eq!(a.with_path("/Project/Repo"), a.with_path("/Project/Repo/"));
        let mut other = connection();
        other.host = "two.invalid".into();
        assert_ne!(
            a,
            GitContext::remote("project".into(), other, "/Project".into())
        );
        assert!(a.local_path().is_none());
        assert_eq!(a.connection_id(), Some("git-context-identity-test"));
        let other_root = GitContext::remote("project".into(), connection(), "/OtherProject".into());
        assert_ne!(
            a.with_path("/Shared/repo"),
            other_root.with_path("/Shared/repo")
        );
        assert_eq!(
            a.with_path("/Shared/repo").repository_key(),
            other_root.with_path("/Shared/repo").repository_key()
        );
        let other_project = GitContext::remote("other".into(), connection(), "/Project".into());
        assert_ne!(a, other_project);
        assert_eq!(a.repository_key(), other_project.repository_key());
        let mut credentials = connection();
        credentials.password = Some("rotated".into());
        let credentials = GitContext::remote("project".into(), credentials, "/Project".into());
        assert_ne!(a, credentials);
        assert_eq!(a.repository_key(), credentials.repository_key());
        assert_eq!(a.project_id(), Some("project"));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_identity_preserves_case_and_literal_backslashes() {
        assert_ne!(GitContext::local("/Repo"), GitContext::local("/repo"));
        assert_ne!(
            GitContext::local("/repo\\name"),
            GitContext::local("/repo/name")
        );
        assert_eq!(GitContext::local("/repo/"), GitContext::local("/repo"));
    }

    #[test]
    fn broken_context_never_falls_back_to_local_io() {
        let broken = GitContext::broken("missing SSH connection").with_path("/");
        assert!(broken.local_path().is_none());
        assert!(
            broken
                .get_changes_status()
                .unwrap_err()
                .to_string()
                .contains("missing SSH connection")
        );
        assert!(
            broken
                .git_stage_all()
                .unwrap_err()
                .to_string()
                .contains("missing SSH connection")
        );
        assert!(broken.recovery_snapshot().is_err());
    }

    #[test]
    fn local_directory_probe_does_not_treat_files_as_missing() {
        let root =
            std::env::temp_dir().join(format!("mt-context-directories-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("file");
        std::fs::write(&file, "not a directory").unwrap();
        let context = GitContext::local(root.to_string_lossy());
        assert!(
            context
                .existing_directories(&[file.to_string_lossy().into_owned()])
                .is_err()
        );
        assert!(
            context
                .existing_directories(&[root.join("missing").to_string_lossy().into_owned()])
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
