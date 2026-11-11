use super::*;
use crate::{GitMutation, GitQuery};

impl Server {
    /// Project roots need not themselves be repositories. A context may descend
    /// from the original root or a Git-registered linked worktree, never jump to
    /// an arbitrary absolute directory supplied by the client.
    pub(super) fn project_directory(&mut self, requested: &str) -> Result<PathBuf> {
        let path = Path::new(requested);
        ensure!(path.is_absolute(), "project path must be absolute");
        if self.allowed.is_none() {
            self.discover()?;
        }
        let path = path
            .canonicalize()
            .context("canonicalizing project directory")?;
        ensure!(path.is_dir(), "project path must be a directory");
        ensure!(
            path == self.root
                || path.starts_with(&self.root)
                || self
                    .allowed
                    .as_ref()
                    .is_some_and(|allowed| allowed.iter().any(|repo| path.starts_with(repo))),
            "project is outside the discovered allowlist"
        );
        Ok(path)
    }

    pub(super) fn register_worktrees(&mut self, repo: &Path) -> Result<Vec<git::WorktreeInfo>> {
        let worktrees = git::list_worktrees(repo)?;
        for worktree in &worktrees {
            let path = PathBuf::from(&worktree.path);
            self.known_worktrees.insert(path.clone());
            if worktree.is_valid {
                self.allowed
                    .get_or_insert_with(HashSet::new)
                    .insert(path.canonicalize()?);
            }
        }
        Ok(worktrees)
    }

    pub(super) fn resolve_file(
        &mut self,
        project: &Path,
        file: &str,
        reads_worktree: bool,
    ) -> Result<(PathBuf, String)> {
        validate_path(project, file, reads_worktree)?;
        let (repo, file) = git::resolve_repo_file(project, file)?;
        let repo = self.repository(&repo.to_string_lossy())?;
        validate_path(&repo, &file, reads_worktree)?;
        Ok((repo, file))
    }

    pub(super) fn query(&mut self, requested: String, query: GitQuery) -> Result<Reply> {
        match query {
            GitQuery::DiscoverGitRepos => {
                self.discover()?;
                let project = self.project_directory(&requested)?;
                let mut repos = git::discover_git_repos(&project)?;
                for repo in &mut repos {
                    repo.path = self.repository(&repo.path.to_string_lossy())?;
                }
                data(repos)
            }
            GitQuery::GetGitStatus => {
                let project = self.project_directory(&requested)?;
                // Core returns project-relative filenames, including nested repositories.
                data(git::get_git_status(&project)?)
            }
            GitQuery::GetGitDiffFrom {
                file_path,
                old_path,
                side,
            } => {
                let project = self.project_directory(&requested)?;
                let (repo, file) = self.resolve_file(&project, &file_path, true)?;
                let old = old_path
                    .map(|old| -> Result<String> {
                        let (old_repo, old) = self.resolve_file(&project, &old, true)?;
                        ensure!(
                            repo == old_repo,
                            "old file belongs to a different repository"
                        );
                        Ok(old)
                    })
                    .transpose()?;
                data(git::get_git_diff_from(&repo, &file, old.as_deref(), side)?)
            }
            GitQuery::GetFileLog {
                file_rel,
                after,
                limit,
            } => {
                let project = self.project_directory(&requested)?;
                let (repo, file) = self.resolve_file(&project, &file_rel, false)?;
                data(git::get_file_log(&repo, &file, after, limit)?)
            }
            GitQuery::ResolveRepoFile { file_rel } => {
                let project = self.project_directory(&requested)?;
                let resolved = self.resolve_file(&project, &file_rel, false)?;
                data(resolved)
            }
            GitQuery::IsInsideGitRepo { abs_file } => {
                let project = self.project_directory(&requested)?;
                let abs = Path::new(&abs_file);
                let relative = abs
                    .strip_prefix(&project)
                    .context("file is outside project")?;
                validate_path(&project, &relative.to_string_lossy(), false)?;
                data(git::is_inside_git_repo(&project, abs))
            }
            GitQuery::ExistingDirectories { paths } => {
                self.project_directory(&requested)?;
                // Validate every entry before inspecting any target. Prune callers
                // may query vanished registered worktrees, so canonicalize parents.
                let root = mt_project::fs::strip_verbatim_prefix(self.root.clone());
                for path in &paths {
                    let path = mt_project::fs::strip_verbatim_prefix(PathBuf::from(path));
                    ensure!(
                        path.is_absolute()
                            && !path.components().any(|c| matches!(c, Component::ParentDir)),
                        "invalid directory path"
                    );
                    ensure!(
                        path == root
                            || path.starts_with(&root)
                            || self.known_worktrees.iter().any(|known| {
                                mt_project::fs::strip_verbatim_prefix(known.clone()) == path
                            }),
                        "directory is outside the discovered allowlist"
                    );
                    let metadata = match std::fs::metadata(&path) {
                        Ok(metadata) => metadata,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(error).context("checking directory metadata"),
                    };
                    ensure!(metadata.is_dir(), "directory target is not a directory");
                    {
                        let canonical = path.canonicalize()?;
                        ensure!(
                            canonical.starts_with(&self.root)
                                || self
                                    .allowed
                                    .as_ref()
                                    .is_some_and(|allowed| allowed.contains(&canonical))
                                || self.known_worktrees.iter().any(|known| known
                                    .canonicalize()
                                    .ok()
                                    .as_ref()
                                    == Some(&canonical)),
                            "directory escapes the discovered allowlist"
                        );
                    }
                }
                let mut existing = Vec::new();
                for path in paths {
                    match std::fs::metadata(&path) {
                        Ok(metadata) => {
                            ensure!(metadata.is_dir(), "directory target is not a directory");
                            existing.push(path);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error).context("checking directory metadata"),
                    }
                }
                data(existing)
            }
            query => {
                let repo = self.repository(&requested)?;
                match query {
                    GitQuery::RepoSignature => data(git::repo_signature(&repo)?),
                    GitQuery::RepoOperationState => data(git::repo_operation_state(&repo)?),
                    GitQuery::ListConflicts => data(git::list_conflicts(&repo)?),
                    GitQuery::HasConflictMarkers { file } => {
                        validate_path(&repo, &file, true)?;
                        data(git::has_conflict_markers(&repo, &file)?)
                    }
                    GitQuery::AmendInfo => data(git::amend_info(&repo)?),
                    GitQuery::HeadExists => data(git::head_exists(&repo)),
                    GitQuery::CommitTarget => data(git::commit_target(&repo)?),
                    GitQuery::RecentCommitMessages { limit } => {
                        data(git::recent_commit_messages(&repo, limit)?)
                    }
                    GitQuery::GetRepoBranches => data(git::get_repo_branches(&repo)?),
                    GitQuery::GetGitLog {
                        after,
                        limit,
                        branch,
                    } => data(git::get_git_log(
                        &repo,
                        after.as_ref(),
                        limit,
                        branch.as_deref(),
                    )?),
                    GitQuery::GetGitLogFiltered {
                        after,
                        limit,
                        branch,
                        filter,
                    } => {
                        if let Some(path) = filter.path.as_deref() {
                            let path = path.trim().replace('\\', "/");
                            if !path.is_empty() && path != "." {
                                validate_path(&repo, &path, false)?;
                            }
                        }
                        data(git::get_git_log_filtered(
                            &repo,
                            after,
                            limit,
                            branch.as_deref(),
                            &filter,
                        )?)
                    }
                    GitQuery::CurrentHeadLabel => data(git::current_head_label(&repo)?),
                    GitQuery::ConfigUserEmail => data(git::config_user_email(&repo)),
                    GitQuery::ListTags => data(git::list_tags(&repo)?),
                    GitQuery::GetCommitLineStats { hash } => {
                        data(git::get_commit_line_stats(&repo, &hash)?)
                    }
                    GitQuery::GetBranchesContaining { hash } => {
                        data(git::get_branches_containing(&repo, &hash)?)
                    }
                    GitQuery::GetCombinedCommitFiles { hashes } => {
                        data(git::get_combined_commit_files(&repo, &hashes)?)
                    }
                    GitQuery::CommitRelation { hash } => data(git::commit_relation(&repo, &hash)?),
                    GitQuery::ResetDropsPushed { target } => {
                        data(git::reset_drops_pushed(&repo, &target)?)
                    }
                    GitQuery::ListRemoteNames => data(git::list_remote_names(&repo)?),
                    GitQuery::GetRevisionFileDiff { base, head } => {
                        for rev in base.iter().chain(head.iter()) {
                            validate_path(&repo, &rev.path, false)?;
                        }
                        data(git::get_revision_file_diff(
                            &repo,
                            base.as_ref(),
                            head.as_ref(),
                        )?)
                    }
                    GitQuery::LocalChangesConflictWith { branch } => {
                        data(git::local_changes_conflict_with(&repo, &branch)?)
                    }
                    GitQuery::PushPlan { branch } => {
                        data(git::push_plan(&repo, branch.as_deref())?)
                    }
                    GitQuery::OutgoingCommits { branch, limit } => {
                        data(git::outgoing_commits(&repo, branch.as_deref(), limit)?)
                    }
                    GitQuery::StashFiles { stash_hash } => {
                        data(git::stash_files(&repo, &stash_hash)?)
                    }
                    GitQuery::ListWorktrees => data(self.register_worktrees(&repo)?),
                    GitQuery::DiscoverGitRepos
                    | GitQuery::GetGitStatus
                    | GitQuery::GetGitDiffFrom { .. }
                    | GitQuery::GetFileLog { .. }
                    | GitQuery::ResolveRepoFile { .. }
                    | GitQuery::IsInsideGitRepo { .. }
                    | GitQuery::ExistingDirectories { .. } => {
                        unreachable!("project queries dispatched above")
                    }
                }
            }
        }
    }

    pub(super) fn mutate(&mut self, requested: String, mutation: GitMutation) -> Result<Reply> {
        let repo = self.repository(&requested)?;
        match mutation {
            GitMutation::CheckoutCommit { hash } => data(git::checkout_commit(&repo, &hash)?),
            GitMutation::CreateBranchAt {
                name,
                hash,
                checkout,
            } => data(git::create_branch_at(&repo, &name, &hash, checkout)?),
            GitMutation::CherryPick { hash } => data(git::cherry_pick(&repo, &hash)?),
            GitMutation::RevertCommit { hash } => data(git::revert_commit(&repo, &hash)?),
            GitMutation::ResetTo { hash, mode } => data(git::reset_to(&repo, &hash, mode)?),
            GitMutation::UndoLastCommit { expected_head } => {
                data(git::undo_last_commit(&repo, &expected_head)?)
            }
            GitMutation::RewordHead {
                expected_head,
                message,
            } => data(git::reword_head(&repo, &expected_head, &message)?),
            GitMutation::CreateTag {
                name,
                hash,
                message,
            } => data(git::create_tag(&repo, &name, &hash, message.as_deref())?),
            GitMutation::DeleteTag { name } => data(git::delete_tag(&repo, &name)?),
            GitMutation::PushTag { remote, name } => data(git::push_tag(&repo, &remote, &name)?),
            GitMutation::DeleteRemoteBranch { remote_branch } => {
                data(git::delete_remote_branch(&repo, &remote_branch)?)
            }
            GitMutation::MergeBranch { name } => data(git::merge_branch(&repo, &name)?),
            GitMutation::RebaseOnto { onto } => data(git::rebase_onto(&repo, &onto)?),
            GitMutation::StashApply {
                index,
                reinstate_index,
                expected_hash,
            } => data(git::stash_apply(
                &repo,
                index,
                reinstate_index,
                expected_hash.as_deref(),
            )?),
            GitMutation::StashPop {
                index,
                reinstate_index,
                expected_hash,
            } => data(git::stash_pop(
                &repo,
                index,
                reinstate_index,
                expected_hash.as_deref(),
            )?),
            GitMutation::StashDrop {
                index,
                expected_hash,
            } => data(git::stash_drop(&repo, index, expected_hash.as_deref())?),
            GitMutation::StashClear => data(git::stash_clear(&repo)?),
            GitMutation::StashBranch {
                name,
                index,
                expected_hash,
            } => data(git::stash_branch(
                &repo,
                &name,
                index,
                expected_hash.as_deref(),
            )?),
            GitMutation::CheckoutWithStash { branch } => {
                data(git::checkout_with_stash(&repo, &branch)?)
            }
            GitMutation::AddWorktree {
                worktree_path,
                branch,
                create_branch,
                base,
            } => {
                let destination = Path::new(&worktree_path);
                ensure!(
                    destination.is_absolute()
                        && !worktree_path.contains('\0')
                        && !destination
                            .components()
                            .any(|c| matches!(c, Component::ParentDir)),
                    "worktree destination must be absolute without parent traversal"
                );
                ensure!(
                    !branch.starts_with('-') && base.as_deref().is_none_or(|b| !b.starts_with('-')),
                    "worktree branch/base must not be an option"
                );
                let output = git::add_worktree(
                    &repo,
                    &worktree_path,
                    &branch,
                    create_branch,
                    base.as_deref(),
                );
                // Even a failed command can have registered a worktree before a
                // checkout/filter failure. Refresh scope from actual Git state.
                let registered = self.register_worktrees(&repo);
                let output = output?;
                registered?;
                data(output)
            }
            GitMutation::RemoveWorktree {
                worktree_path,
                force,
            } => {
                let worktrees = self.register_worktrees(&repo)?;
                let target = Path::new(&worktree_path);
                let entry = worktrees
                    .iter()
                    .find(|entry| {
                        Path::new(&entry.path) == target
                            || target
                                .canonicalize()
                                .ok()
                                .zip(Path::new(&entry.path).canonicalize().ok())
                                .is_some_and(|(a, b)| a == b)
                    })
                    .filter(|entry| !entry.is_main)
                    .context("removal target is not a registered linked worktree")?;
                let registered_path = entry.path.clone();
                let target = Path::new(&registered_path);
                let output = git::remove_worktree(&repo, &registered_path, force);
                // Preserve known directory names for the following prune/project
                // cleanup query, but revoke repository operations immediately.
                if let Some(allowed) = &mut self.allowed {
                    allowed.remove(target);
                    if let Ok(canonical) = target.canonicalize() {
                        allowed.remove(&canonical);
                    }
                }
                self.register_worktrees(&repo)?;
                data(output?)
            }
            GitMutation::PruneWorktrees => {
                self.register_worktrees(&repo)?;
                let output = git::prune_worktrees(&repo)?;
                self.discover()?;
                data(output)
            }
        }
    }
}
