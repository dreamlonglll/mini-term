use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use mt_project::git;

use crate::{
    ChangesSnapshot, Hello, HistoryPage, Operation, PROTOCOL_VERSION, PushPreview, Reply, Request,
    Response, encode_frame, read_frame, write_frame,
};

mod advanced;

/// Serve one persistent protocol stream. The greeting and every response are
/// flushed. A malformed frame terminates the stream; operation errors do not.
/// Root must be an existing absolute directory. Repository discovery retains
/// mt-project's ancestor/depth/worktree rules, including repositories above root.
pub fn serve(root: &Path, mut reader: impl Read, mut writer: impl Write) -> Result<()> {
    ensure!(root.is_absolute(), "--root must be an absolute directory");
    let root = root.canonicalize().context("canonicalizing --root")?;
    ensure!(root.is_dir(), "--root must be a directory");
    let mut server = Server {
        root,
        allowed: None,
        known_worktrees: HashSet::new(),
        last_completed_write: None,
    };
    write_frame(
        &mut writer,
        &Hello {
            protocol_version: PROTOCOL_VERSION,
            helper_version: env!("CARGO_PKG_VERSION").to_owned(),
        },
    )?;
    writer.flush()?;
    while let Some(request) = read_frame::<Request>(&mut reader).context("reading request frame")? {
        // Version is checked before even lazy discovery, and certainly before
        // any Git operation. IDs are opaque and echoed, not replay protection.
        let is_write = request.operation.is_write();
        let id = request.id;
        let result = if request.protocol_version != PROTOCOL_VERSION {
            Err(crate::GitFailure::from(format!(
                "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
                request.protocol_version
            )))
        } else {
            let result = server.dispatch(request.operation);
            if is_write {
                server.last_completed_write = Some(id);
            }
            result.map_err(|error| {
                if is_write && error.is::<ReplyEncodingFailure>() {
                    crate::GitFailure::outcome_unknown(error)
                } else {
                    crate::GitFailure::from_error(error)
                }
            })
        };
        let response = Response {
            protocol_version: PROTOCOL_VERSION,
            id: request.id,
            result,
        };
        writer.write_all(&response_frame(&response, is_write)?)?;
        writer.flush()?;
    }
    Ok(())
}

fn response_frame(response: &Response, is_write: bool) -> std::io::Result<Vec<u8>> {
    match encode_frame(response) {
        Ok(frame) => Ok(frame),
        Err(error) => encode_frame(&Response {
            protocol_version: PROTOCOL_VERSION,
            id: response.id,
            result: Err(if is_write {
                crate::GitFailure::outcome_unknown(format_args!(
                    "cannot encode bounded response: {error}"
                ))
            } else {
                crate::GitFailure::from(format!("cannot encode bounded response: {error}"))
            }),
        }),
    }
}

struct Server {
    root: PathBuf,
    allowed: Option<HashSet<PathBuf>>,
    known_worktrees: HashSet<PathBuf>,
    last_completed_write: Option<u64>,
}

impl Server {
    fn discover(&mut self) -> Result<Vec<git::GitRepoInfo>> {
        let mut repos = git::discover_git_repos(&self.root)?;
        let mut allowed = HashSet::with_capacity(repos.len());
        for repo in &mut repos {
            repo.path = repo
                .path
                .canonicalize()
                .context("canonicalizing discovered repository")?;
            allowed.insert(repo.path.clone());
        }
        repos.retain(|repo| {
            // Deduplicate aliases while retaining core discovery order.
            allowed.remove(&repo.path)
        });
        self.allowed = Some(repos.iter().map(|repo| repo.path.clone()).collect());
        // Git, not the requested path, authorizes sibling linked worktrees.
        for repo in &repos {
            self.register_worktrees(&repo.path)?;
        }
        Ok(repos)
    }

    fn repository(&mut self, requested: &str) -> Result<PathBuf> {
        let requested = Path::new(requested);
        ensure!(requested.is_absolute(), "repository path must be absolute");
        if self.allowed.is_none() {
            self.discover()?;
        }
        let canonical = requested
            .canonicalize()
            .context("canonicalizing repository path")?;
        ensure!(
            self.allowed
                .as_ref()
                .is_some_and(|allowed| allowed.contains(&canonical)),
            "repository is outside the discovered allowlist"
        );
        Ok(canonical)
    }

    fn dispatch(&mut self, operation: Operation) -> Result<Reply> {
        ensure!(
            self.root.is_dir(),
            "workspace root is no longer an existing directory"
        );
        match operation {
            Operation::Barrier { completed_id } => {
                ensure!(
                    self.last_completed_write == Some(completed_id),
                    "no matching completed write on this helper stream"
                );
                Ok(Reply::Completed { completed_id })
            }
            Operation::Discover => Ok(Reply::Repositories {
                repos: self.discover()?,
            }),
            Operation::Status { repo } => {
                let repo = self.repository(&repo)?;
                Ok(Reply::Status {
                    files: git::get_changes_status(&repo)?,
                })
            }
            Operation::Query { repo, query } => self.query(repo, query),
            Operation::Mutate { repo, mutation } => self.mutate(repo, mutation),
            Operation::Diff {
                repo,
                path,
                old_path,
                side,
            } => {
                let repo = self.repository(&repo)?;
                validate_path(&repo, &path, true)?;
                if let Some(old_path) = &old_path {
                    validate_path(&repo, old_path, true)?;
                }
                validate_file_repository(&repo, &path)?;
                Ok(Reply::Diff {
                    diff: git::get_git_diff_from(&repo, &path, old_path.as_deref(), side.into())?,
                })
            }
            Operation::Stage { repo, paths } => {
                let repo = self.repository(&repo)?;
                validate_paths(&repo, &paths)?;
                git::git_stage(&repo, &paths)?;
                Ok(Reply::Updated)
            }
            Operation::Unstage { repo, paths } => {
                let repo = self.repository(&repo)?;
                validate_paths(&repo, &paths)?;
                git::git_unstage(&repo, &paths)?;
                Ok(Reply::Updated)
            }
            Operation::StageAll { repo } => {
                let repo = self.repository(&repo)?;
                git::git_stage_all(&repo)?;
                Ok(Reply::Updated)
            }
            Operation::UnstageAll { repo } => {
                let repo = self.repository(&repo)?;
                git::git_unstage_all(&repo)?;
                Ok(Reply::Updated)
            }
            Operation::Commit { repo, message } => {
                let repo = self.repository(&repo)?;
                Ok(Reply::Committed {
                    output: git::git_commit(&repo, &message)?,
                })
            }
            Operation::PanelSnapshot { repo, recent_limit } => {
                let repo = self.repository(&repo)?;
                data(git::repo_panel_snapshot(&repo, recent_limit)?)
            }
            Operation::ChangesSnapshot { repo } => {
                let repo = self.repository(&repo)?;
                data(ChangesSnapshot {
                    files: git::get_changes_status(&repo)?,
                    conflicts: git::list_conflicts(&repo)?,
                    operation: git::repo_operation_state(&repo)?,
                    target: git::commit_target(&repo)?,
                    has_head: git::head_exists(&repo),
                    amend: git::amend_info(&repo)?,
                })
            }
            Operation::Log {
                repo,
                after,
                limit,
                branch,
                filter,
            } => {
                let repo = self.repository(&repo)?;
                ensure!(limit > 0, "history page limit must be greater than zero");
                let (commits, next, scanned) = if let Some(filter) = filter {
                    if let Some(path) = filter.path.as_deref() {
                        // LogFilter interprets both separators, including on POSIX.
                        let path = path.trim().replace('\\', "/");
                        if !path.is_empty() && path != "." {
                            validate_path(&repo, &path, false)?;
                        }
                    }
                    let page =
                        git::get_git_log_filtered(&repo, after, limit, branch.as_deref(), &filter)?;
                    (page.commits, page.next, page.scanned)
                } else {
                    let commits =
                        git::get_git_log(&repo, after.as_ref(), Some(limit), branch.as_deref())?;
                    let cursor = match after {
                        Some(after) => after.advance(&commits),
                        None => git::GitLogCursor::from_loaded(&commits),
                    };
                    let has_next = !commits.is_empty()
                        && !git::get_git_log(&repo, Some(&cursor), Some(1), None)?.is_empty();
                    let scanned = commits.len();
                    (commits, has_next.then_some(cursor), scanned)
                };
                data(HistoryPage {
                    commits,
                    next,
                    scanned,
                    tags: git::list_tags(&repo)?,
                })
            }
            Operation::CommitDetail { repo, hash } => {
                let repo = self.repository(&repo)?;
                data(git::get_commit_detail(&repo, &hash)?)
            }
            Operation::CommitFiles { repo, hash } => {
                let repo = self.repository(&repo)?;
                data(git::get_commit_files(&repo, &hash)?)
            }
            Operation::CommitDiff {
                repo,
                hash,
                path,
                old_path,
            } => {
                let repo = self.repository(&repo)?;
                validate_path(&repo, &path, false)?;
                if let Some(old_path) = &old_path {
                    validate_path(&repo, old_path, false)?;
                }
                data(git::get_commit_file_diff(
                    &repo,
                    &hash,
                    &path,
                    old_path.as_deref(),
                )?)
            }
            Operation::CommitWith {
                repo,
                message,
                amend,
            } => {
                let repo = self.repository(&repo)?;
                data(git::git_commit_with(&repo, &message, amend)?)
            }
            Operation::Discard { repo, paths } => {
                let repo = self.repository(&repo)?;
                validate_paths(&repo, &paths)?;
                git::discard_unstaged(&repo, &paths)?;
                data(())
            }
            Operation::StageHunk {
                repo,
                path,
                diff,
                hunk,
            } => {
                let project = self.project_directory(&repo)?;
                let (repo, path) = self.resolve_file(&project, &path, true)?;
                git::stage_hunk(&repo, &path, &diff, hunk)?;
                data(())
            }
            Operation::UnstageHunk {
                repo,
                path,
                diff,
                hunk,
            } => {
                let project = self.project_directory(&repo)?;
                let (repo, path) = self.resolve_file(&project, &path, true)?;
                git::unstage_hunk(&repo, &path, &diff, hunk)?;
                data(())
            }
            Operation::DiscardHunk {
                repo,
                path,
                diff,
                hunk,
            } => {
                let project = self.project_directory(&repo)?;
                let (repo, path) = self.resolve_file(&project, &path, true)?;
                git::discard_hunk(&repo, &path, &diff, hunk)?;
                data(())
            }
            Operation::Checkout {
                repo,
                branch,
                remote,
            } => {
                let repo = self.repository(&repo)?;
                let outcome = if remote {
                    Some(git::checkout_remote_branch(&repo, &branch)?)
                } else {
                    git::checkout_branch(&repo, &branch)?;
                    None
                };
                data(outcome)
            }
            Operation::CreateBranch {
                repo,
                name,
                start,
                checkout,
            } => {
                let repo = self.repository(&repo)?;
                git::create_branch(&repo, &name, start.as_deref(), checkout)?;
                data(())
            }
            Operation::RenameBranch { repo, old, name } => {
                let repo = self.repository(&repo)?;
                git::rename_branch(&repo, &old, &name)?;
                data(())
            }
            Operation::DeleteBranch { repo, name, force } => {
                let repo = self.repository(&repo)?;
                data(git::delete_branch(&repo, &name, force)?)
            }
            Operation::Fetch { repo } => {
                let repo = self.repository(&repo)?;
                data(git::git_fetch_all(&repo)?)
            }
            Operation::Pull { repo, mode } => {
                let repo = self.repository(&repo)?;
                data(match mode {
                    Some(mode) => git::git_pull_with(&repo, mode)?,
                    None => git::git_pull_by_config(&repo)?,
                })
            }
            Operation::PushPreview {
                repo,
                branch,
                limit,
            } => {
                let repo = self.repository(&repo)?;
                data(PushPreview {
                    plan: git::push_plan(&repo, branch.as_deref())?,
                    outgoing: git::outgoing_commits(&repo, branch.as_deref(), limit)?,
                })
            }
            Operation::Push { repo, options } => {
                let repo = self.repository(&repo)?;
                data(git::git_push_with(&repo, &options)?)
            }
            Operation::ListStashes { repo } => {
                let repo = self.repository(&repo)?;
                data(git::list_stashes(&repo)?)
            }
            Operation::SaveStash { repo, options } => {
                let repo = self.repository(&repo)?;
                data(git::stash_save(&repo, &options)?)
            }
            Operation::ApplyStash {
                repo,
                index,
                hash,
                pop,
                reinstate,
            } => {
                let repo = self.repository(&repo)?;
                data(if pop {
                    git::stash_pop(&repo, index, reinstate, Some(&hash))?
                } else {
                    git::stash_apply(&repo, index, reinstate, Some(&hash))?
                })
            }
            Operation::DropStash { repo, index, hash } => {
                let repo = self.repository(&repo)?;
                git::stash_drop(&repo, index, Some(&hash))?;
                data(())
            }
            Operation::OperationContinue { repo, kind } => {
                let repo = self.repository(&repo)?;
                data(git::continue_operation(&repo, kind)?)
            }
            Operation::OperationAbort { repo, kind } => {
                let repo = self.repository(&repo)?;
                git::abort_operation(&repo, kind)?;
                data(())
            }
            Operation::ResolveConflict { repo, path, side } => {
                let repo = self.repository(&repo)?;
                validate_path(&repo, &path, false)?;
                git::resolve_conflict_with(&repo, &path, side)?;
                data(())
            }
            Operation::MarkResolved { repo, path } => {
                let repo = self.repository(&repo)?;
                validate_path(&repo, &path, false)?;
                git::mark_conflict_resolved(&repo, &path)?;
                data(())
            }
        }
    }
}

#[derive(Debug)]
struct ReplyEncodingFailure(std::io::Error);
impl std::fmt::Display for ReplyEncodingFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "encoding Git reply data: {}", self.0)
    }
}
impl std::error::Error for ReplyEncodingFailure {}

fn data(value: impl serde::Serialize) -> Result<Reply> {
    let mut buffer = crate::FrameBuffer(Vec::new());
    serde_json::to_writer(&mut buffer, &value).map_err(|error| {
        ReplyEncodingFailure(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })?;
    crate::limits::validate_json(&buffer.0).map_err(|error| {
        ReplyEncodingFailure(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })?;
    let json = String::from_utf8(buffer.0).expect("JSON serializer emits UTF-8");
    let value = serde_json::value::RawValue::from_string(json).map_err(|error| {
        ReplyEncodingFailure(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })?;
    Ok(Reply::Data { value })
}

fn validate_file_repository(repo: &Path, path: &str) -> Result<()> {
    // Core working-tree diff/hunk operations discover from the file's directory.
    // Never silently switch to an undiscovered nested repository.
    let file = repo.join(path);
    let parent = file
        .ancestors()
        .skip(1)
        .find(|parent| parent.is_dir())
        .unwrap_or(repo);
    let enclosing = git::enclosing_repo(parent)
        .context("file is not in a discoverable repository")?
        .canonicalize()?;
    ensure!(enclosing == repo, "file belongs to a different repository");
    Ok(())
}

fn validate_paths(repo: &Path, paths: &[String]) -> Result<()> {
    // Validate the whole batch before dispatch so a bad final path cannot leave
    // earlier paths staged. Empty batches retain core's no-op behavior.
    for path in paths {
        validate_path(repo, path, false)?;
    }
    Ok(())
}

fn validate_path(repo: &Path, input: &str, reads_worktree: bool) -> Result<()> {
    ensure!(
        !input.is_empty() && !input.contains('\0'),
        "file path must be nonempty and contain no NUL"
    );
    let path = Path::new(input);
    ensure!(!path.is_absolute(), "file path must be repository-relative");
    // Inspect raw components too: Path::components normalizes some dots, but
    // parent traversal is forbidden even when it would remain within the root.
    ensure!(
        !path.components().any(|part| matches!(
            part,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )),
        "file path must not contain parent traversal or a root prefix"
    );
    ensure!(
        path.components()
            .any(|part| matches!(part, Component::Normal(_))),
        "file path must name a repository entry"
    );
    let absolute = repo.join(path);
    // Check existing parents without requiring the file (or deleted directories)
    // to exist. Backslashes on POSIX remain ordinary filename characters.
    for ancestor in absolute.ancestors().skip(1) {
        if ancestor == repo {
            break;
        }
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let canonical = ancestor.canonicalize().context("resolving file parent")?;
                ensure!(
                    canonical.starts_with(repo),
                    "file parent escapes repository through a symlink"
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("checking file parent"),
        }
    }
    // Git stages symlinks themselves; do not resolve the final component for
    // staging/unstaging. Core working-tree diff reads the target, so it needs an
    // additional containment check (including for Staged, for uniform safety).
    if reads_worktree {
        match absolute.canonicalize() {
            Ok(canonical) if !canonical.starts_with(repo) => {
                bail!("file target escapes repository through a symlink")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("resolving diff file"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unencodable_write_reply_is_unknown_but_read_failure_is_known() {
        let response = Response {
            protocol_version: PROTOCOL_VERSION,
            id: 7,
            result: Ok(Reply::Committed {
                output: "x".repeat(crate::MAX_FRAME_BYTES),
            }),
        };
        for is_write in [true, false] {
            let frame = response_frame(&response, is_write).unwrap();
            let decoded = read_frame::<Response>(&mut std::io::Cursor::new(frame))
                .unwrap()
                .unwrap();
            assert_eq!(decoded.id, response.id);
            assert_eq!(
                matches!(
                    decoded.result,
                    Err(crate::GitFailure::OutcomeUnknown { .. })
                ),
                is_write
            );
        }
    }
}
