use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use mt_git_helper::{
    DiffSide, Hello, Operation, PROTOCOL_VERSION, Reply, Request, Response, read_frame, write_frame,
};
use mt_project::git::GitStatus;

struct Fixture(PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "mini-term-helper-{tag}-{}-{now}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(mt_project::fs::strip_verbatim_prefix(
            root.canonicalize().unwrap(),
        ))
    }

    fn repo(&self, name: &str) -> PathBuf {
        let root = self.0.join(name);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "--template=", "."]);
        let hooks = root.join(".git/isolated-hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        for (key, value) in [
            ("user.name", "mini-term helper test"),
            ("user.email", "helper-test@example.com"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
            ("core.autocrlf", "false"),
            ("core.hooksPath", hooks.to_str().unwrap()),
        ] {
            git(&root, &["config", "--local", key, value]);
        }
        root
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=mini-term helper test",
            "-c",
            "user.email=helper-test@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .output()
        .expect("starting git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn path(root: &Path) -> String {
    root.to_str().unwrap().to_owned()
}

fn helper_command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mt-git-helper"));
    command
        .arg("--root")
        .arg(root)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("isolated-config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true");
    for key in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
    ] {
        command.env_remove(key);
    }
    command
}

struct Helper {
    child: Child,
    input: Option<ChildStdin>,
    output: ChildStdout,
    next_id: u64,
}

impl Helper {
    fn start(root: &Path) -> Self {
        let mut child = helper_command(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("starting real helper");
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let mut helper = Self {
            child,
            input: Some(input),
            output,
            next_id: 0,
        };
        let hello = read_frame::<Hello>(&mut helper.output).unwrap().unwrap();
        assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
        assert_eq!(hello.helper_version, env!("CARGO_PKG_VERSION"));
        helper
    }

    fn exchange(&mut self, request: &Request) -> Response {
        let input = self.input.as_mut().unwrap();
        write_frame(input, request).unwrap();
        input.flush().unwrap();
        let response = read_frame::<Response>(&mut self.output).unwrap().unwrap();
        assert_eq!(response.id, request.id);
        assert_eq!(response.protocol_version, PROTOCOL_VERSION);
        response
    }

    fn request(&mut self, operation: Operation) -> Response {
        let request = Request {
            protocol_version: PROTOCOL_VERSION,
            id: self.next_id,
            operation,
        };
        self.next_id += 1;
        self.exchange(&request)
    }

    fn success(&mut self, operation: Operation) -> Reply {
        self.request(operation).result.unwrap()
    }

    fn finish(mut self) {
        drop(self.input.take());
        assert!(read_frame::<Response>(&mut self.output).unwrap().is_none());
        assert!(self.child.wait().unwrap().success());
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        drop(self.input.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn requests(root: &Path, requests: &[Request]) -> Vec<Response> {
    let mut helper = Helper::start(root);
    let responses = requests
        .iter()
        .map(|request| helper.exchange(request))
        .collect();
    helper.finish();
    responses
}

fn operations(root: &Path, operations: Vec<Operation>) -> Vec<Reply> {
    let input: Vec<_> = operations
        .into_iter()
        .enumerate()
        .map(|(id, operation)| Request {
            protocol_version: PROTOCOL_VERSION,
            id: id as u64,
            operation,
        })
        .collect();
    requests(root, &input)
        .into_iter()
        .map(|response| response.result.unwrap())
        .collect()
}

fn diff(repo: &Path, name: &str, side: DiffSide) -> Operation {
    Operation::Diff {
        repo: path(repo),
        path: name.to_owned(),
        old_path: None,
        side,
    }
}

fn diff_contents(reply: &Reply) -> (&str, &str) {
    let Reply::Diff { diff } = reply else {
        panic!("expected diff, got {reply:?}")
    };
    (diff.old_content.as_str(), diff.new_content.as_str())
}

fn files(reply: &Reply) -> &[mt_project::git::ChangeFileStatus] {
    let Reply::Status { files } = reply else {
        panic!("expected status, got {reply:?}")
    };
    files
}

#[test]
fn real_partial_stage_diff_commit_and_error_recovery() {
    let fixture = Fixture::new("partial");
    let repo = fixture.repo("repo");
    std::fs::write(repo.join("file.txt"), "base\n").unwrap();
    let replies = operations(
        &repo,
        vec![
            Operation::Stage {
                repo: path(&repo),
                paths: vec!["file.txt".into()],
            },
            diff(&repo, "file.txt", DiffSide::Staged),
            Operation::Commit {
                repo: path(&repo),
                message: "initial helper commit".into(),
            },
        ],
    );
    assert_eq!(diff_contents(&replies[1]), ("", "base\n"));
    assert!(matches!(replies[2], Reply::Committed { .. }));
    std::fs::write(repo.join("file.txt"), "staged\n").unwrap();
    operations(
        &repo,
        vec![Operation::Stage {
            repo: path(&repo),
            paths: vec!["file.txt".into()],
        }],
    );
    std::fs::write(repo.join("file.txt"), "working\n").unwrap();
    let replies = operations(
        &repo,
        vec![
            Operation::Status { repo: path(&repo) },
            diff(&repo, "file.txt", DiffSide::Staged),
            diff(&repo, "file.txt", DiffSide::Unstaged),
            diff(&repo, "file.txt", DiffSide::Working),
            Operation::Commit {
                repo: path(&repo),
                message: "commit staged only\n\nbody with ' quote".into(),
            },
            Operation::Status { repo: path(&repo) },
        ],
    );
    let file = &files(&replies[0])[0];
    assert_eq!(file.staged_status, Some(GitStatus::Modified));
    assert_eq!(file.unstaged_status, Some(GitStatus::Modified));
    assert_eq!(diff_contents(&replies[1]), ("base\n", "staged\n"));
    assert_eq!(diff_contents(&replies[2]), ("staged\n", "working\n"));
    assert_eq!(diff_contents(&replies[3]), ("base\n", "working\n"));
    assert!(files(&replies[5])[0].staged_status.is_none());
    assert_eq!(git(&repo, &["show", "HEAD:file.txt"]), "staged");
    assert_eq!(
        std::fs::read_to_string(repo.join("file.txt")).unwrap(),
        "working\n"
    );
    let responses = requests(
        &repo,
        &[
            Request {
                protocol_version: PROTOCOL_VERSION,
                id: 7,
                operation: Operation::Commit {
                    repo: path(&repo),
                    message: "nothing staged".into(),
                },
            },
            Request {
                protocol_version: PROTOCOL_VERSION,
                id: 8,
                operation: Operation::Status { repo: path(&repo) },
            },
        ],
    );
    assert!(responses[0].result.is_err());
    assert!(responses[1].result.is_ok());
}

#[test]
fn discovery_preserves_multiple_repositories_and_ancestor_context() {
    let fixture = Fixture::new("discovery");
    let first = fixture.repo("one");
    let second = fixture.repo("nested/two");
    let replies = operations(&fixture.0, vec![Operation::Discover]);
    let Reply::Repositories { repos } = &replies[0] else {
        panic!("expected repos")
    };
    assert_eq!(repos.len(), 2);
    let first_canonical = first.canonicalize().unwrap();
    let second_canonical = second.canonicalize().unwrap();
    assert!(repos.iter().any(|repo| repo.path == first_canonical));
    assert!(repos.iter().any(|repo| repo.path == second_canonical));
    let child = first.join("child/deep");
    std::fs::create_dir_all(&child).unwrap();
    let replies = operations(
        &child,
        vec![
            Operation::Discover,
            Operation::Status { repo: path(&first) },
        ],
    );
    let Reply::Repositories { repos } = &replies[0] else {
        panic!("expected repos")
    };
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].path, first_canonical);
}

#[test]
fn version_mismatch_never_mutates_and_ids_are_opaque() {
    let fixture = Fixture::new("version");
    let repo = fixture.repo("repo");
    std::fs::write(repo.join("file"), "untracked\n").unwrap();
    let replies = requests(
        &repo,
        &[
            Request {
                protocol_version: PROTOCOL_VERSION + 1,
                id: u64::MAX,
                operation: Operation::StageAll { repo: path(&repo) },
            },
            Request {
                protocol_version: PROTOCOL_VERSION,
                id: u64::MAX,
                operation: Operation::Status { repo: path(&repo) },
            },
        ],
    );
    assert!(
        replies[0]
            .result
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("unsupported protocol version")
    );
    let status = files(replies[1].result.as_ref().unwrap());
    assert_eq!(status[0].staged_status, None);
    assert!(git(&repo, &["diff", "--cached", "--name-only"]).is_empty());
}

#[test]
fn unborn_unstage_and_empty_batches_keep_core_semantics() {
    let fixture = Fixture::new("unborn");
    let repo = fixture.repo("repo");
    std::fs::write(repo.join("one"), "one\n").unwrap();
    std::fs::write(repo.join("two"), "two\n").unwrap();
    let replies = operations(
        &repo,
        vec![
            Operation::Stage {
                repo: path(&repo),
                paths: Vec::new(),
            },
            Operation::Unstage {
                repo: path(&repo),
                paths: Vec::new(),
            },
            Operation::StageAll { repo: path(&repo) },
            Operation::Unstage {
                repo: path(&repo),
                paths: vec!["one".into()],
            },
            Operation::Status { repo: path(&repo) },
            Operation::UnstageAll { repo: path(&repo) },
            Operation::Status { repo: path(&repo) },
        ],
    );
    let status = files(&replies[4]);
    assert!(
        status
            .iter()
            .find(|file| file.path == "one")
            .unwrap()
            .staged_status
            .is_none()
    );
    assert!(
        status
            .iter()
            .find(|file| file.path == "two")
            .unwrap()
            .staged_status
            .is_some()
    );
    assert!(
        files(&replies[6])
            .iter()
            .all(|file| file.staged_status.is_none())
    );
}

#[test]
fn rename_diff_and_unstage_reuse_core_pairing() {
    let fixture = Fixture::new("rename");
    let repo = fixture.repo("repo");
    let body = "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\n";
    std::fs::write(repo.join("old.txt"), body).unwrap();
    operations(
        &repo,
        vec![
            Operation::StageAll { repo: path(&repo) },
            Operation::Commit {
                repo: path(&repo),
                message: "initial".into(),
            },
        ],
    );
    git(&repo, &["mv", "old.txt", "new.txt"]);
    let changed = body.replace("line 5", "LINE 5");
    std::fs::write(repo.join("new.txt"), &changed).unwrap();
    let replies = operations(
        &repo,
        vec![
            Operation::Stage {
                repo: path(&repo),
                paths: vec!["new.txt".into()],
            },
            Operation::Status { repo: path(&repo) },
            Operation::Diff {
                repo: path(&repo),
                path: "new.txt".into(),
                old_path: Some("old.txt".into()),
                side: DiffSide::Staged,
            },
            Operation::Unstage {
                repo: path(&repo),
                paths: vec!["new.txt".into()],
            },
            Operation::Status { repo: path(&repo) },
        ],
    );
    let status = files(&replies[1]);
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].old_path.as_deref(), Some("old.txt"));
    assert_eq!(status[0].staged_status, Some(GitStatus::Renamed));
    assert_eq!(diff_contents(&replies[2]), (body, changed.as_str()));
    assert!(
        files(&replies[4])
            .iter()
            .all(|file| file.staged_status.is_none())
    );
}

#[test]
fn repository_and_file_boundaries_reject_entire_batches() {
    let fixture = Fixture::new("scope");
    let repo = fixture.repo("allowed");
    let outside = fixture.repo("outside");
    std::fs::write(repo.join("good"), "good\n").unwrap();
    std::fs::create_dir(repo.join("subdir")).unwrap();
    let bad_paths = [
        "../outside/file",
        "/absolute",
        "dir/../good",
        "bad\0name",
        "",
        ".",
    ];
    let mut input: Vec<_> = bad_paths
        .iter()
        .enumerate()
        .map(|(id, bad)| Request {
            protocol_version: PROTOCOL_VERSION,
            id: id as u64,
            operation: Operation::Stage {
                repo: path(&repo),
                paths: vec!["good".into(), (*bad).into()],
            },
        })
        .collect();
    for operation in [
        Operation::StageAll {
            repo: path(&outside),
        },
        Operation::Status {
            repo: path(&repo.join("subdir")),
        },
        Operation::Diff {
            repo: path(&repo),
            path: "good".into(),
            old_path: Some("../outside/file".into()),
            side: DiffSide::Staged,
        },
        Operation::Status {
            repo: "relative".into(),
        },
    ] {
        input.push(Request {
            protocol_version: PROTOCOL_VERSION,
            id: input.len() as u64,
            operation,
        });
    }
    assert!(
        requests(&repo, &input)
            .iter()
            .all(|response| response.result.is_err())
    );
    assert!(git(&repo, &["diff", "--cached", "--name-only"]).is_empty());
    let nested = fixture.repo("allowed/nested");
    std::fs::write(nested.join("secret"), "not the selected repository\n").unwrap();
    let responses = requests(
        &repo,
        &[Request {
            protocol_version: PROTOCOL_VERSION,
            id: 1,
            operation: diff(&repo, "nested/secret", DiffSide::Working),
        }],
    );
    assert!(
        responses[0]
            .result
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("different repository")
    );
}

#[cfg(unix)]
#[test]
fn odd_posix_names_survive_staging_status_and_both_diff_sides() {
    let fixture = Fixture::new("odd-names");
    let repo = fixture.repo("repo");
    let names = [
        "space name.txt",
        "tab\tname.txt",
        "line\nname.txt",
        "-option.txt",
        ":(glob)*.txt",
        "literal\\name.txt",
    ];
    for name in names {
        std::fs::write(repo.join(name), "initial\n").unwrap();
    }
    operations(
        &repo,
        vec![
            Operation::Stage {
                repo: path(&repo),
                paths: names.iter().map(|name| (*name).into()).collect(),
            },
            Operation::Commit {
                repo: path(&repo),
                message: "odd names".into(),
            },
        ],
    );
    for name in names {
        std::fs::write(repo.join(name), "staged\n").unwrap();
    }
    operations(
        &repo,
        vec![Operation::Stage {
            repo: path(&repo),
            paths: names.iter().map(|name| (*name).into()).collect(),
        }],
    );
    for name in names {
        std::fs::write(repo.join(name), "working\n").unwrap();
    }
    let mut ops = vec![Operation::Status { repo: path(&repo) }];
    for name in names {
        ops.extend([
            diff(&repo, name, DiffSide::Staged),
            diff(&repo, name, DiffSide::Unstaged),
        ]);
    }
    let replies = operations(&repo, ops);
    let status = files(&replies[0]);
    assert_eq!(status.len(), names.len());
    for (index, name) in names.iter().enumerate() {
        let file = status.iter().find(|file| file.path == *name).unwrap();
        assert_eq!(file.staged_status, Some(GitStatus::Modified));
        assert_eq!(file.unstaged_status, Some(GitStatus::Modified));
        assert_eq!(
            diff_contents(&replies[1 + index * 2]),
            ("initial\n", "staged\n")
        );
        assert_eq!(
            diff_contents(&replies[2 + index * 2]),
            ("staged\n", "working\n")
        );
    }
}

#[cfg(unix)]
#[test]
fn canonical_root_aliases_work_but_file_symlink_escapes_do_not() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new("symlinks");
    let repo = fixture.repo("repo");
    let outside = fixture.0.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret"), "outside\n").unwrap();
    let alias = fixture.0.join("alias");
    symlink(&repo, &alias).unwrap();
    symlink(&outside, repo.join("escape-dir")).unwrap();
    symlink(outside.join("secret"), repo.join("escape-file")).unwrap();
    let replies = operations(
        &alias,
        vec![
            Operation::Discover,
            Operation::Status { repo: path(&alias) },
        ],
    );
    let Reply::Repositories { repos } = &replies[0] else {
        panic!("expected repos")
    };
    assert_eq!(repos[0].path, repo);
    let responses = requests(
        &repo,
        &[
            Request {
                protocol_version: PROTOCOL_VERSION,
                id: 1,
                operation: diff(&repo, "escape-dir/secret", DiffSide::Working),
            },
            Request {
                protocol_version: PROTOCOL_VERSION,
                id: 2,
                operation: diff(&repo, "escape-file", DiffSide::Working),
            },
            Request {
                protocol_version: PROTOCOL_VERSION,
                id: 3,
                operation: Operation::Stage {
                    repo: path(&repo),
                    paths: vec!["escape-dir/secret".into()],
                },
            },
            Request {
                protocol_version: PROTOCOL_VERSION,
                id: 4,
                operation: Operation::Stage {
                    repo: path(&repo),
                    paths: vec!["escape-file".into()],
                },
            },
        ],
    );
    assert!(
        responses[..3]
            .iter()
            .all(|response| response.result.is_err())
    );
    assert!(
        responses[3].result.is_ok(),
        "Git stages symlinks, not their targets"
    );
}

#[test]
fn foreign_index_lock_is_preserved_and_same_session_recovers() {
    let fixture = Fixture::new("foreign-lock");
    let repo = fixture.repo("repo");
    std::fs::write(repo.join("file.txt"), "base\n").unwrap();
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "initial"]);
    std::fs::write(repo.join("file.txt"), "changed\n").unwrap();
    let original_index = std::fs::read(repo.join(".git/index")).unwrap();
    let lock_path = repo.join(".git/index.lock");
    let mut lock = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .unwrap();
    let owner = b"owned by another git process\n";
    lock.write_all(owner).unwrap();
    lock.flush().unwrap();
    let mut helper = Helper::start(&repo);
    let failed = helper.request(Operation::Stage {
        repo: path(&repo),
        paths: vec!["file.txt".into()],
    });
    assert!(failed.result.is_err());
    assert_eq!(std::fs::read(&lock_path).unwrap(), owner);
    assert_eq!(
        std::fs::read(repo.join(".git/index")).unwrap(),
        original_index
    );
    let status = helper.success(Operation::Status { repo: path(&repo) });
    assert_eq!(files(&status).len(), 1);
    assert_eq!(files(&status)[0].staged_status, None);
    assert_eq!(files(&status)[0].unstaged_status, Some(GitStatus::Modified));
    assert_eq!(std::fs::read(&lock_path).unwrap(), owner);
    drop(lock);
    std::fs::remove_file(&lock_path).unwrap();
    assert!(matches!(
        helper.success(Operation::Stage {
            repo: path(&repo),
            paths: vec!["file.txt".into()],
        }),
        Reply::Updated
    ));
    let staged = helper.success(diff(&repo, "file.txt", DiffSide::Staged));
    assert_eq!(diff_contents(&staged), ("base\n", "changed\n"));
    let status = helper.success(Operation::Status { repo: path(&repo) });
    assert_eq!(files(&status)[0].staged_status, Some(GitStatus::Modified));
    assert_eq!(files(&status)[0].unstaged_status, None);
    assert!(!lock_path.exists());
    helper.finish();
}

#[test]
fn rejecting_commit_hook_preserves_head_and_index_then_same_session_retries() {
    let fixture = Fixture::new("reject-hook");
    let repo = fixture.repo("repo");
    std::fs::write(repo.join("file.txt"), "base\n").unwrap();
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "initial"]);
    std::fs::write(repo.join("file.txt"), "staged\n").unwrap();
    git(&repo, &["add", "file.txt"]);
    std::fs::write(repo.join("file.txt"), "working\n").unwrap();
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let index_tree = git(&repo, &["write-tree"]);
    let index_entries = git(&repo, &["ls-files", "--stage"]);
    let hook = repo.join(".git/isolated-hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'fixture rejects commit' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut helper = Helper::start(&repo);
    assert!(
        helper
            .request(Operation::Commit {
                repo: path(&repo),
                message: "rejected commit".into(),
            })
            .result
            .is_err()
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&repo, &["write-tree"]), index_tree);
    assert_eq!(git(&repo, &["ls-files", "--stage"]), index_entries);
    assert_eq!(
        std::fs::read_to_string(repo.join("file.txt")).unwrap(),
        "working\n"
    );
    let status = helper.success(Operation::Status { repo: path(&repo) });
    assert_eq!(files(&status).len(), 1);
    assert_eq!(files(&status)[0].staged_status, Some(GitStatus::Modified));
    assert_eq!(files(&status)[0].unstaged_status, Some(GitStatus::Modified));
    let staged = helper.success(diff(&repo, "file.txt", DiffSide::Staged));
    assert_eq!(diff_contents(&staged), ("base\n", "staged\n"));
    let unstaged = helper.success(diff(&repo, "file.txt", DiffSide::Unstaged));
    assert_eq!(diff_contents(&unstaged), ("staged\n", "working\n"));
    std::fs::remove_file(hook).unwrap();
    assert!(matches!(
        helper.success(Operation::Commit {
            repo: path(&repo),
            message: "accepted retry".into(),
        }),
        Reply::Committed { .. }
    ));
    assert_ne!(git(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&repo, &["show", "HEAD:file.txt"]), "staged");
    let status = helper.success(Operation::Status { repo: path(&repo) });
    assert_eq!(files(&status).len(), 1);
    assert_eq!(files(&status)[0].staged_status, None);
    assert_eq!(files(&status)[0].unstaged_status, Some(GitStatus::Modified));
    helper.finish();
}

#[test]
fn missing_repository_and_removed_workspace_are_errors_not_empty_success() {
    let fixture = Fixture::new("removed-workspace");
    let workspace = fixture.0.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let missing = workspace.join("missing-repository");
    let mut helper = Helper::start(&workspace);
    assert!(
        helper
            .request(Operation::Status {
                repo: path(&missing)
            })
            .result
            .is_err()
    );
    let Reply::Repositories { repos } = helper.success(Operation::Discover) else {
        panic!("expected repositories")
    };
    assert!(repos.is_empty(), "an existing non-Git workspace is valid");
    std::fs::remove_dir(&workspace).unwrap();
    assert!(helper.request(Operation::Discover).result.is_err());
    assert!(
        helper
            .request(Operation::Status {
                repo: path(&workspace)
            })
            .result
            .is_err()
    );
    std::fs::create_dir(&workspace).unwrap();
    let Reply::Repositories { repos } = helper.success(Operation::Discover) else {
        panic!("expected repositories")
    };
    assert!(repos.is_empty());
    helper.finish();
    let failed_start = helper_command(&missing).output().unwrap();
    assert!(!failed_start.status.success());
    assert!(
        failed_start.stdout.is_empty(),
        "failed startup must not greet"
    );

    let repo = fixture.repo("repo");
    let mut helper = Helper::start(&repo);
    assert!(matches!(
        helper.success(Operation::Discover),
        Reply::Repositories { .. }
    ));
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    assert!(
        helper
            .request(Operation::Status { repo: path(&repo) })
            .result
            .is_err()
    );
    assert!(
        helper
            .request(diff(&repo, "file.txt", DiffSide::Working))
            .result
            .is_err()
    );
    assert!(
        helper
            .request(Operation::StageAll { repo: path(&repo) })
            .result
            .is_err()
    );
    helper.finish();
}

fn assert_diff_marker(reply: &Reply, binary: bool, too_large: bool) {
    let Reply::Diff { diff } = reply else {
        panic!("expected diff, got {reply:?}")
    };
    assert_eq!(diff.is_binary, binary);
    assert_eq!(diff.too_large, too_large);
    assert!(diff.old_content.is_empty());
    assert!(diff.new_content.is_empty());
    assert!(diff.hunks.is_empty());
}

#[test]
fn binary_and_oversized_diff_markers_survive_worktree_index_and_head() {
    let fixture = Fixture::new("diff-markers");
    let repo = fixture.repo("repo");
    let cases: [(&str, Vec<u8>, bool, bool); 4] = [
        ("binary-nul.bin", b"\0binary\x01".to_vec(), true, false),
        ("invalid.bin", vec![0xff, 0xfe, 0x80], true, false),
        ("large.txt", vec![b'x'; 1_048_577], false, true),
        ("large.bin", vec![0; 1_048_577], false, true),
    ];
    let mut helper = Helper::start(&repo);
    for (name, bytes, binary, too_large) in &cases {
        std::fs::write(repo.join(name), bytes).unwrap();
        for side in [DiffSide::Unstaged, DiffSide::Working] {
            let reply = helper.success(diff(&repo, name, side));
            assert_diff_marker(&reply, *binary, *too_large);
        }
        helper.success(Operation::Stage {
            repo: path(&repo),
            paths: vec![(*name).into()],
        });
        let reply = helper.success(diff(&repo, name, DiffSide::Staged));
        assert_diff_marker(&reply, *binary, *too_large);
        std::fs::write(repo.join(name), "small text\n").unwrap();
        let reply = helper.success(diff(&repo, name, DiffSide::Unstaged));
        assert_diff_marker(&reply, *binary, *too_large);
        std::fs::write(repo.join(name), bytes).unwrap();
    }
    helper.success(Operation::Commit {
        repo: path(&repo),
        message: "binary and large blobs".into(),
    });
    for (name, _, binary, too_large) in &cases {
        std::fs::write(repo.join(name), "replacement text\n").unwrap();
        for side in [DiffSide::Working, DiffSide::Unstaged] {
            let reply = helper.success(diff(&repo, name, side));
            assert_diff_marker(&reply, *binary, *too_large);
        }
        helper.success(Operation::Stage {
            repo: path(&repo),
            paths: vec![(*name).into()],
        });
        let reply = helper.success(diff(&repo, name, DiffSide::Staged));
        assert_diff_marker(&reply, *binary, *too_large);
        std::fs::remove_file(repo.join(name)).unwrap();
        let reply = helper.success(diff(&repo, name, DiffSide::Working));
        assert_diff_marker(&reply, *binary, *too_large);
        helper.success(Operation::Stage {
            repo: path(&repo),
            paths: vec![(*name).into()],
        });
        let reply = helper.success(diff(&repo, name, DiffSide::Staged));
        assert_diff_marker(&reply, *binary, *too_large);
    }
    helper.finish();
}

#[test]
fn simultaneous_helpers_observe_external_worktree_index_and_head_changes() {
    let fixture = Fixture::new("multiple-helpers");
    let repo = fixture.repo("repo");
    std::fs::write(repo.join("file.txt"), "base\n").unwrap();
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "initial"]);
    let mut first = Helper::start(&repo);
    let mut second = Helper::start(&repo);
    for helper in [&mut first, &mut second] {
        let status = helper.success(Operation::Status { repo: path(&repo) });
        assert!(files(&status).is_empty());
    }
    std::fs::write(repo.join("file.txt"), "external worktree edit\n").unwrap();
    for helper in [&mut first, &mut second] {
        let status = helper.success(Operation::Status { repo: path(&repo) });
        assert_eq!(files(&status).len(), 1);
        assert_eq!(files(&status)[0].staged_status, None);
        assert_eq!(files(&status)[0].unstaged_status, Some(GitStatus::Modified));
        let reply = helper.success(diff(&repo, "file.txt", DiffSide::Working));
        assert_eq!(
            diff_contents(&reply),
            ("base\n", "external worktree edit\n")
        );
    }
    git(&repo, &["add", "file.txt"]);
    for helper in [&mut first, &mut second] {
        let status = helper.success(Operation::Status { repo: path(&repo) });
        assert_eq!(files(&status).len(), 1);
        assert_eq!(files(&status)[0].staged_status, Some(GitStatus::Modified));
        assert_eq!(files(&status)[0].unstaged_status, None);
        let reply = helper.success(diff(&repo, "file.txt", DiffSide::Staged));
        assert_eq!(
            diff_contents(&reply),
            ("base\n", "external worktree edit\n")
        );
    }
    git(&repo, &["commit", "-m", "external commit"]);
    for helper in [&mut first, &mut second] {
        let status = helper.success(Operation::Status { repo: path(&repo) });
        assert!(files(&status).is_empty());
        let reply = helper.success(diff(&repo, "file.txt", DiffSide::Working));
        assert_eq!(
            diff_contents(&reply),
            ("external worktree edit\n", "external worktree edit\n")
        );
    }
    first.finish();
    second.finish();
}

fn fixed_time_commit(repo: &Path, message: &str) {
    let output = Command::new("git")
        .args(["commit", "--allow-empty", "-m", message])
        .current_dir(repo)
        .env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn desktop_history_serialized_cursors_cross_many_equal_time_merge_pages() {
    use mt_git_helper::HistoryPage;
    use mt_project::git::{self, GitLogCursor, LogFilter};

    let fixture = Fixture::new("desktop-history");
    let repo = fixture.repo("repo");
    fixed_time_commit(&repo, "keep root");
    let original_branch = git(&repo, &["branch", "--show-current"]);
    git(&repo, &["checkout", "-b", "side"]);
    for i in 0..4 {
        fixed_time_commit(&repo, &format!("keep side {i}"));
    }
    git(&repo, &["checkout", &original_branch]);
    for i in 0..6 {
        fixed_time_commit(&repo, &format!("main {i}"));
    }
    git(&repo, &["merge", "--no-ff", "--no-commit", "side"]);
    fixed_time_commit(&repo, "keep merge");
    git(&repo, &["tag", "history-tag"]);
    let mut helper = Helper::start(&repo);

    for filter in [
        None,
        Some(LogFilter {
            text: "keep".into(),
            ..Default::default()
        }),
    ] {
        let expected = match &filter {
            Some(filter) => {
                git::get_git_log_filtered(&repo, None, 100, None, filter)
                    .unwrap()
                    .commits
            }
            None => git::get_git_log(&repo, None, Some(100), None).unwrap(),
        };
        let mut after: Option<GitLogCursor> = None;
        let mut hashes = Vec::new();
        let mut pages = 0;
        loop {
            let page: HistoryPage = helper
                .success(Operation::Log {
                    repo: path(&repo),
                    after,
                    limit: 1,
                    branch: None,
                    filter: filter.clone(),
                })
                .decode()
                .unwrap();
            pages += 1;
            assert!(pages < 30, "continuation must converge");
            assert_eq!(page.tags.len(), 1);
            hashes.extend(page.commits.into_iter().map(|commit| commit.hash));
            match page.next {
                // Simulate clients storing opaque cursors between actions.
                Some(next) => {
                    after =
                        Some(serde_json::from_slice(&serde_json::to_vec(&next).unwrap()).unwrap())
                }
                None => break,
            }
        }
        assert!(pages > 3);
        assert_eq!(
            hashes,
            expected
                .into_iter()
                .map(|commit| commit.hash)
                .collect::<Vec<_>>()
        );
        let unique: std::collections::HashSet<_> = hashes.iter().collect();
        assert_eq!(unique.len(), hashes.len(), "pages must not repeat commits");
    }
    helper.finish();
}

#[test]
fn desktop_snapshots_hunks_commit_details_branches_and_stashes_use_real_core() {
    use mt_git_helper::{ChangesSnapshot, PushPreview};
    use mt_project::git::{self, CommitOutcome, CommitTarget, StashApplyOutcome, StashSaveOutcome};

    let fixture = Fixture::new("desktop-operations");
    let repo = fixture.repo("repo");
    std::fs::write(repo.join("file"), "base\n").unwrap();
    git(&repo, &["add", "file"]);
    fixed_time_commit(&repo, "initial");
    let mut helper = Helper::start(&repo);
    let panel: git::RepoPanelSnapshot = helper
        .success(Operation::PanelSnapshot {
            repo: path(&repo),
            recent_limit: 8,
        })
        .decode()
        .unwrap();
    assert_eq!(panel.branches.len(), 1);
    let snapshot: ChangesSnapshot = helper
        .success(Operation::ChangesSnapshot { repo: path(&repo) })
        .decode()
        .unwrap();
    assert!(snapshot.has_head);
    assert!(snapshot.files.is_empty());
    assert!(snapshot.amend.is_some());
    assert!(matches!(snapshot.target, CommitTarget::Branch(_)));
    std::fs::write(repo.join("file"), "edited\n").unwrap();
    let Reply::Diff { diff: patch } = helper.success(diff(&repo, "file", DiffSide::Unstaged))
    else {
        panic!("diff");
    };
    helper
        .success(Operation::StageHunk {
            repo: path(&repo),
            path: "file".into(),
            diff: patch,
            hunk: 0,
        })
        .decode::<()>()
        .unwrap();
    let Reply::Diff { diff: patch } = helper.success(diff(&repo, "file", DiffSide::Staged)) else {
        panic!("diff");
    };
    helper
        .success(Operation::UnstageHunk {
            repo: path(&repo),
            path: "file".into(),
            diff: patch,
            hunk: 0,
        })
        .decode::<()>()
        .unwrap();
    let Reply::Diff { diff: patch } = helper.success(diff(&repo, "file", DiffSide::Unstaged))
    else {
        panic!("diff");
    };
    helper
        .success(Operation::DiscardHunk {
            repo: path(&repo),
            path: "file".into(),
            diff: patch,
            hunk: 0,
        })
        .decode::<()>()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.join("file")).unwrap(),
        "base\n"
    );
    std::fs::write(repo.join("file"), "committed\n").unwrap();
    helper.success(Operation::StageAll { repo: path(&repo) });
    let outcome: CommitOutcome = helper
        .success(Operation::CommitWith {
            repo: path(&repo),
            message: "desktop commit".into(),
            amend: false,
        })
        .decode()
        .unwrap();
    assert_eq!(outcome.summary, "desktop commit");
    let hash = git(&repo, &["rev-parse", "HEAD"]);
    let detail: git::CommitDetail = helper
        .success(Operation::CommitDetail {
            repo: path(&repo),
            hash: hash.clone(),
        })
        .decode()
        .unwrap();
    assert_eq!(detail.hash, hash);
    let commit_files: Vec<git::CommitFileInfo> = helper
        .success(Operation::CommitFiles {
            repo: path(&repo),
            hash: hash.clone(),
        })
        .decode()
        .unwrap();
    assert_eq!(commit_files[0].path, "file");
    let commit_diff: git::GitDiffResult = helper
        .success(Operation::CommitDiff {
            repo: path(&repo),
            hash,
            path: "file".into(),
            old_path: None,
        })
        .decode()
        .unwrap();
    assert_eq!(commit_diff.new_content, "committed\n");
    helper
        .success(Operation::CreateBranch {
            repo: path(&repo),
            name: "topic".into(),
            start: None,
            checkout: false,
        })
        .decode::<()>()
        .unwrap();
    helper
        .success(Operation::RenameBranch {
            repo: path(&repo),
            old: "topic".into(),
            name: "renamed".into(),
        })
        .decode::<()>()
        .unwrap();
    let deleted: git::DeleteOutcome = helper
        .success(Operation::DeleteBranch {
            repo: path(&repo),
            name: "renamed".into(),
            force: false,
        })
        .decode()
        .unwrap();
    assert_eq!(deleted, git::DeleteOutcome::Deleted);
    let preview: PushPreview = helper
        .success(Operation::PushPreview {
            repo: path(&repo),
            branch: None,
            limit: 10,
        })
        .decode()
        .unwrap();
    assert!(matches!(preview.plan, git::PushPlan::NoRemote { .. }));
    std::fs::write(repo.join("file"), "stash me\n").unwrap();
    let saved: StashSaveOutcome = helper
        .success(Operation::SaveStash {
            repo: path(&repo),
            options: git::StashSaveOptions::default(),
        })
        .decode()
        .unwrap();
    assert!(matches!(saved, StashSaveOutcome::Saved { .. }));
    let stashes: Vec<git::StashInfo> = helper
        .success(Operation::ListStashes { repo: path(&repo) })
        .decode()
        .unwrap();
    assert_eq!(stashes.len(), 1);
    let applied: StashApplyOutcome = helper
        .success(Operation::ApplyStash {
            repo: path(&repo),
            index: 0,
            hash: stashes[0].hash.clone(),
            pop: false,
            reinstate: false,
        })
        .decode()
        .unwrap();
    assert_eq!(applied, StashApplyOutcome::Applied);
    helper
        .success(Operation::Discard {
            repo: path(&repo),
            paths: vec!["file".into()],
        })
        .decode::<()>()
        .unwrap();
    helper
        .success(Operation::DropStash {
            repo: path(&repo),
            index: 0,
            hash: stashes[0].hash.clone(),
        })
        .decode::<()>()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.join("file")).unwrap(),
        "committed\n"
    );
    assert!(
        helper
            .request(Operation::Pull {
                repo: path(&repo),
                mode: None
            })
            .result
            .is_err()
    );
    helper.finish();
}

#[test]
fn every_desktop_operation_checks_repository_scope_before_core() {
    use mt_project::git::{ConflictSide, GitDiffResult, RepoOpKind};
    let fixture = Fixture::new("desktop-scope");
    let repo = fixture.repo("workspace");
    let outside_fixture = Fixture::new("desktop-outside");
    let outside = path(&outside_fixture.repo("outside"));
    let patch = GitDiffResult {
        old_content: String::new(),
        new_content: String::new(),
        hunks: Vec::new(),
        is_binary: false,
        too_large: false,
    };
    let mut helper = Helper::start(&repo);
    let operations = vec![
        Operation::PanelSnapshot {
            repo: outside.clone(),
            recent_limit: 1,
        },
        Operation::ChangesSnapshot {
            repo: outside.clone(),
        },
        Operation::Log {
            repo: outside.clone(),
            after: None,
            limit: 1,
            branch: None,
            filter: None,
        },
        Operation::CommitDetail {
            repo: outside.clone(),
            hash: "bad".into(),
        },
        Operation::CommitFiles {
            repo: outside.clone(),
            hash: "bad".into(),
        },
        Operation::CommitDiff {
            repo: outside.clone(),
            hash: "bad".into(),
            path: "file".into(),
            old_path: None,
        },
        Operation::CommitWith {
            repo: outside.clone(),
            message: "message".into(),
            amend: false,
        },
        Operation::Discard {
            repo: outside.clone(),
            paths: vec![],
        },
        Operation::StageHunk {
            repo: outside.clone(),
            path: "file".into(),
            diff: patch.clone(),
            hunk: 0,
        },
        Operation::UnstageHunk {
            repo: outside.clone(),
            path: "file".into(),
            diff: patch.clone(),
            hunk: 0,
        },
        Operation::DiscardHunk {
            repo: outside.clone(),
            path: "file".into(),
            diff: patch.clone(),
            hunk: 0,
        },
        Operation::Checkout {
            repo: outside.clone(),
            branch: "topic".into(),
            remote: false,
        },
        Operation::CreateBranch {
            repo: outside.clone(),
            name: "topic".into(),
            start: None,
            checkout: false,
        },
        Operation::RenameBranch {
            repo: outside.clone(),
            old: "topic".into(),
            name: "new".into(),
        },
        Operation::DeleteBranch {
            repo: outside.clone(),
            name: "topic".into(),
            force: false,
        },
        Operation::Fetch {
            repo: outside.clone(),
        },
        Operation::Pull {
            repo: outside.clone(),
            mode: None,
        },
        Operation::PushPreview {
            repo: outside.clone(),
            branch: None,
            limit: 1,
        },
        Operation::Push {
            repo: outside.clone(),
            options: Default::default(),
        },
        Operation::ListStashes {
            repo: outside.clone(),
        },
        Operation::SaveStash {
            repo: outside.clone(),
            options: Default::default(),
        },
        Operation::ApplyStash {
            repo: outside.clone(),
            index: 0,
            hash: "bad".into(),
            pop: false,
            reinstate: false,
        },
        Operation::DropStash {
            repo: outside.clone(),
            index: 0,
            hash: "bad".into(),
        },
        Operation::OperationContinue {
            repo: outside.clone(),
            kind: RepoOpKind::Merge,
        },
        Operation::OperationAbort {
            repo: outside.clone(),
            kind: RepoOpKind::Merge,
        },
        Operation::ResolveConflict {
            repo: outside.clone(),
            path: "file".into(),
            side: ConflictSide::Ours,
        },
        Operation::MarkResolved {
            repo: outside,
            path: "file".into(),
        },
    ];
    for operation in operations {
        let error = helper.request(operation).result.unwrap_err();
        assert!(error.to_string().contains("allowlist"), "{error}");
    }
    for bad in ["../outside/file", "/absolute", "", "nul\0byte"] {
        for operation in [
            Operation::Discard {
                repo: path(&repo),
                paths: vec![bad.into()],
            },
            Operation::StageHunk {
                repo: path(&repo),
                path: bad.into(),
                diff: patch.clone(),
                hunk: 0,
            },
            Operation::UnstageHunk {
                repo: path(&repo),
                path: bad.into(),
                diff: patch.clone(),
                hunk: 0,
            },
            Operation::DiscardHunk {
                repo: path(&repo),
                path: bad.into(),
                diff: patch.clone(),
                hunk: 0,
            },
            Operation::CommitDiff {
                repo: path(&repo),
                hash: "bad".into(),
                path: bad.into(),
                old_path: None,
            },
            Operation::CommitDiff {
                repo: path(&repo),
                hash: "bad".into(),
                path: "file".into(),
                old_path: Some(bad.into()),
            },
            Operation::ResolveConflict {
                repo: path(&repo),
                path: bad.into(),
                side: ConflictSide::Theirs,
            },
            Operation::MarkResolved {
                repo: path(&repo),
                path: bad.into(),
            },
        ] {
            assert!(
                helper
                    .request(operation)
                    .result
                    .unwrap_err()
                    .to_string()
                    .contains("file path")
            );
        }
    }
    helper.finish();
}

#[test]
fn desktop_sync_and_conflict_replies_preserve_real_outcomes() {
    use mt_git_helper::{ChangesSnapshot, PushPreview};
    use mt_project::git::{self, ConflictSide, RepoOpKind};
    let fixture = Fixture::new("desktop-sync");
    let repo = fixture.repo("workspace");
    std::fs::write(repo.join("file"), "base\n").unwrap();
    git(&repo, &["add", "file"]);
    fixed_time_commit(&repo, "base");
    let original = git(&repo, &["branch", "--show-current"]);
    let remote = fixture.0.join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "--bare", "--template=", "."]);
    git(&repo, &["remote", "add", "origin", &path(&remote)]);
    let mut helper = Helper::start(&repo);
    let preview: PushPreview = helper
        .success(Operation::PushPreview {
            repo: path(&repo),
            branch: None,
            limit: 10,
        })
        .decode()
        .unwrap();
    assert!(matches!(preview.plan, git::PushPlan::Ready(_)));
    let pushed: git::PushOutcome = helper
        .success(Operation::Push {
            repo: path(&repo),
            options: Default::default(),
        })
        .decode()
        .unwrap();
    assert!(matches!(pushed, git::PushOutcome::Pushed(_)));
    let _: git::FetchOutcome = helper
        .success(Operation::Fetch { repo: path(&repo) })
        .decode()
        .unwrap();
    let pulled: git::PullOutcome = helper
        .success(Operation::Pull {
            repo: path(&repo),
            mode: Some(git::PullMode::FfOnly),
        })
        .decode()
        .unwrap();
    assert_eq!(pulled, git::PullOutcome::UpToDate);

    helper
        .success(Operation::CreateBranch {
            repo: path(&repo),
            name: "side".into(),
            start: None,
            checkout: true,
        })
        .decode::<()>()
        .unwrap();
    std::fs::write(repo.join("file"), "theirs\n").unwrap();
    git(&repo, &["add", "file"]);
    fixed_time_commit(&repo, "side change");
    let checkout: Option<git::RemoteCheckout> = helper
        .success(Operation::Checkout {
            repo: path(&repo),
            branch: original,
            remote: false,
        })
        .decode()
        .unwrap();
    assert!(checkout.is_none());
    std::fs::write(repo.join("file"), "ours\n").unwrap();
    git(&repo, &["add", "file"]);
    fixed_time_commit(&repo, "main change");
    for finish in [false, true] {
        let merged = Command::new("git")
            .args(["merge", "--no-edit", "side"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(!merged.status.success());
        let snapshot: ChangesSnapshot = helper
            .success(Operation::ChangesSnapshot { repo: path(&repo) })
            .decode()
            .unwrap();
        assert_eq!(snapshot.operation.kind, Some(RepoOpKind::Merge));
        assert_eq!(snapshot.conflicts.len(), 1);
        assert!(
            helper
                .request(Operation::OperationContinue {
                    repo: path(&repo),
                    kind: RepoOpKind::Merge
                })
                .result
                .is_err()
        );
        if !finish {
            helper
                .success(Operation::OperationAbort {
                    repo: path(&repo),
                    kind: RepoOpKind::Merge,
                })
                .decode::<()>()
                .unwrap();
        } else {
            helper
                .success(Operation::ResolveConflict {
                    repo: path(&repo),
                    path: "file".into(),
                    side: ConflictSide::Theirs,
                })
                .decode::<()>()
                .unwrap();
            let state: git::RepoOpState = helper
                .success(Operation::OperationContinue {
                    repo: path(&repo),
                    kind: RepoOpKind::Merge,
                })
                .decode()
                .unwrap();
            assert_eq!(state.kind, None);
            assert_eq!(
                std::fs::read_to_string(repo.join("file")).unwrap(),
                "theirs\n"
            );
        }
    }
    helper.finish();
}

#[test]
fn stale_hunk_failure_retains_type_and_never_stages_new_content() {
    use mt_git_helper::GitFailure;
    let fixture = Fixture::new("typed-stale");
    let repo = fixture.repo("repo");
    std::fs::write(repo.join("file"), "base\n").unwrap();
    git(&repo, &["add", "file"]);
    fixed_time_commit(&repo, "base");
    std::fs::write(repo.join("file"), "displayed\n").unwrap();
    let mut helper = Helper::start(&repo);
    let Reply::Diff { diff: displayed } = helper.success(diff(&repo, "file", DiffSide::Unstaged))
    else {
        panic!("diff");
    };
    std::fs::write(repo.join("file"), "new external edit\n").unwrap();
    let failure = helper
        .request(Operation::StageHunk {
            repo: path(&repo),
            path: "file".into(),
            diff: displayed,
            hunk: 0,
        })
        .result
        .unwrap_err();
    assert_eq!(failure, GitFailure::StaleDiff);
    assert!(failure.into_error().is::<mt_project::git::StaleDiff>());
    assert!(git(&repo, &["diff", "--cached", "--name-only"]).is_empty());
    assert_eq!(
        std::fs::read_to_string(repo.join("file")).unwrap(),
        "new external edit\n"
    );
    helper.finish();
}

#[test]
fn linked_worktree_scope_supports_cross_root_actions_and_rejects_unregistered_targets() {
    use mt_git_helper::{GitMutation as M, GitQuery as Q};
    use mt_project::git::{BranchInfo, WorktreeInfo};
    let fixture = Fixture::new("linked-scope");
    let main = fixture.repo("main");
    std::fs::write(main.join("file"), "base\n").unwrap();
    git(&main, &["add", "file"]);
    fixed_time_commit(&main, "base");
    let linked = fixture.0.join("linked");
    git(&main, &["worktree", "add", "-b", "linked", &path(&linked)]);
    let mut helper = Helper::start(&linked);
    let trees: Vec<WorktreeInfo> = helper
        .success(Operation::Query {
            repo: path(&linked),
            query: Q::ListWorktrees,
        })
        .decode()
        .unwrap();
    assert!(
        trees
            .iter()
            .any(|tree| tree.is_main && Path::new(&tree.path) == main)
    );
    let branches: Vec<BranchInfo> = helper
        .success(Operation::Query {
            repo: path(&main),
            query: Q::GetRepoBranches,
        })
        .decode()
        .unwrap();
    assert!(branches.iter().any(|branch| branch.name == "linked"));
    let sibling = fixture.0.join("new-sibling");
    helper
        .success(Operation::Mutate {
            repo: path(&main),
            mutation: M::AddWorktree {
                worktree_path: path(&sibling),
                branch: "new-sibling".into(),
                create_branch: true,
                base: None,
            },
        })
        .decode::<String>()
        .unwrap();
    let _: Vec<BranchInfo> = helper
        .success(Operation::Query {
            repo: path(&sibling),
            query: Q::GetRepoBranches,
        })
        .decode()
        .unwrap();
    let unrelated_fixture = Fixture::new("unregistered");
    let unrelated = unrelated_fixture.repo("repo");
    let failure = helper
        .request(Operation::Mutate {
            repo: path(&main),
            mutation: M::RemoveWorktree {
                worktree_path: path(&unrelated),
                force: true,
            },
        })
        .result
        .unwrap_err();
    assert!(failure.to_string().contains("registered linked worktree"));
    assert!(unrelated.join(".git").is_dir());
    helper
        .success(Operation::Mutate {
            repo: path(&main),
            mutation: M::RemoveWorktree {
                worktree_path: path(&sibling),
                force: false,
            },
        })
        .decode::<String>()
        .unwrap();
    assert!(!sibling.exists());
    let directories: Vec<String> = helper
        .success(Operation::Query {
            repo: path(&main),
            query: Q::ExistingDirectories {
                paths: vec![path(&linked), path(&sibling)],
            },
        })
        .decode()
        .unwrap();
    assert_eq!(directories, vec![path(&linked)]);
    helper.finish();
}

#[test]
fn advanced_history_guards_reword_reset_tags_and_project_relative_file_history() {
    use mt_git_helper::{GitMutation as M, GitQuery as Q};
    use mt_project::git::{FileLogPage, ResetMode, TagInfo};
    let fixture = Fixture::new("advanced-history");
    let repo = fixture.repo("repo");
    std::fs::create_dir(repo.join("sub")).unwrap();
    std::fs::write(repo.join("sub/file"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    fixed_time_commit(&repo, "first");
    let first = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("sub/file"), "second\n").unwrap();
    git(&repo, &["add", "."]);
    fixed_time_commit(&repo, "second");
    let second = git(&repo, &["rev-parse", "HEAD"]);
    let mut helper = Helper::start(&repo.join("sub"));
    let page: FileLogPage = helper
        .success(Operation::Query {
            repo: path(&repo.join("sub")),
            query: Q::GetFileLog {
                file_rel: "file".into(),
                after: None,
                limit: 1,
            },
        })
        .decode()
        .unwrap();
    assert_eq!(page.path_in_repo, "sub/file");
    assert_eq!(page.entries[0].commit.hash, second);
    let next: FileLogPage = helper
        .success(Operation::Query {
            repo: path(&repo.join("sub")),
            query: Q::GetFileLog {
                file_rel: "file".into(),
                after: page.next,
                limit: 1,
            },
        })
        .decode()
        .unwrap();
    assert_eq!(next.entries[0].commit.hash, first);
    assert!(
        helper
            .request(Operation::Mutate {
                repo: path(&repo),
                mutation: M::RewordHead {
                    expected_head: first.clone(),
                    message: "must not apply".into(),
                }
            })
            .result
            .is_err()
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), second);
    let rewritten: String = helper
        .success(Operation::Mutate {
            repo: path(&repo),
            mutation: M::RewordHead {
                expected_head: second,
                message: "rewritten message".into(),
            },
        })
        .decode()
        .unwrap();
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]),
        "rewritten message"
    );
    helper
        .success(Operation::Mutate {
            repo: path(&repo),
            mutation: M::CreateTag {
                name: "annotated".into(),
                hash: rewritten.clone(),
                message: Some("tag body".into()),
            },
        })
        .decode::<()>()
        .unwrap();
    let tags: Vec<TagInfo> = helper
        .success(Operation::Query {
            repo: path(&repo),
            query: Q::ListTags,
        })
        .decode()
        .unwrap();
    assert_eq!(tags[0].target_hash, rewritten);
    assert_eq!(git(&repo, &["cat-file", "-t", "annotated"]), "tag");
    helper
        .success(Operation::Mutate {
            repo: path(&repo),
            mutation: M::ResetTo {
                hash: first.clone(),
                mode: ResetMode::Soft,
            },
        })
        .decode::<()>()
        .unwrap();
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), first);
    assert_eq!(git(&repo, &["show", ":sub/file"]), "second");
    helper
        .success(Operation::Mutate {
            repo: path(&repo),
            mutation: M::DeleteTag {
                name: "annotated".into(),
            },
        })
        .decode::<()>()
        .unwrap();
    let tags: Vec<TagInfo> = helper
        .success(Operation::Query {
            repo: path(&repo),
            query: Q::ListTags,
        })
        .decode()
        .unwrap();
    assert!(tags.is_empty());
    helper.finish();
}

#[test]
fn completion_receipt_belongs_to_original_helper_and_exact_write() {
    let fixture = Fixture::new("completion-receipt");
    let repo = fixture.repo("repo");
    let mut helper = Helper::start(&repo);
    assert!(
        helper
            .request(Operation::Barrier { completed_id: 0 })
            .result
            .is_err()
    );
    let write = helper.request(Operation::StageAll { repo: path(&repo) });
    write.result.unwrap();
    let id = write.id;
    helper.success(Operation::Status { repo: path(&repo) });
    assert!(
        matches!(helper.success(Operation::Barrier { completed_id: id }), Reply::Completed { completed_id } if completed_id == id)
    );
    assert!(
        helper
            .request(Operation::Barrier {
                completed_id: id + 1
            })
            .result
            .is_err()
    );
    let mut other = Helper::start(&repo);
    assert!(
        other
            .request(Operation::Barrier { completed_id: id })
            .result
            .is_err()
    );
    other.finish();
    helper.finish();
}

#[test]
fn existing_directories_omits_only_missing_and_rejects_files() {
    use mt_git_helper::GitQuery;
    let fixture = Fixture::new("directory-errors");
    let repo = fixture.repo("repo");
    let mut helper = Helper::start(&repo);
    let existing: Vec<String> = helper
        .success(Operation::Query {
            repo: path(&repo),
            query: GitQuery::ExistingDirectories {
                paths: vec![path(&repo), path(&repo.join("missing"))],
            },
        })
        .decode()
        .unwrap();
    assert_eq!(existing, vec![path(&repo)]);
    let canonical = repo.canonicalize().unwrap();
    let aliases: Vec<String> = helper
        .success(Operation::Query {
            repo: path(&canonical),
            query: GitQuery::ExistingDirectories {
                paths: vec![path(&canonical), path(&canonical.join("missing"))],
            },
        })
        .decode()
        .unwrap();
    assert_eq!(aliases, vec![path(&canonical)]);
    let unrelated = Fixture::new("directory-outside-scope");
    assert!(
        helper
            .request(Operation::Query {
                repo: path(&repo),
                query: GitQuery::ExistingDirectories {
                    paths: vec![path(&unrelated.0.join("missing"))],
                },
            })
            .result
            .is_err()
    );
    std::fs::write(repo.join("file"), "not directory").unwrap();
    assert!(
        helper
            .request(Operation::Query {
                repo: path(&repo),
                query: GitQuery::ExistingDirectories {
                    paths: vec![path(&repo.join("file"))]
                }
            })
            .result
            .is_err()
    );
    helper.finish();
}
