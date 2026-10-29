//! 暂存区读写(暂存 / 取消暂存)与 CLI 执行器(超时、进程树、管道)的单测。
//! 全部走临时仓库,不联网。

use super::test_support::{cleanup, commit_file, current, git, init_repo_with_file};
use super::*;

/// 暂存区里有改动的路径(`git diff --cached --name-only`),按 git 的顺序。
fn staged(root: &Path) -> Vec<String> {
    git(root, &["diff", "--cached", "--name-only"])
        .lines()
        .map(str::to_string)
        .collect()
}

/// 暂存区 stage 0 条目的内容(换行归一)。
fn index_blob(root: &Path, rel: &str) -> String {
    let repo = Repository::open(root).unwrap();
    let index = repo.index().unwrap();
    let entry = index
        .get_path(Path::new(rel), 0)
        .unwrap_or_else(|| panic!("{rel} 不在暂存区"));
    let blob = repo.find_blob(entry.id).unwrap();
    String::from_utf8_lossy(blob.content()).replace("\r\n", "\n")
}

fn state_of(root: &Path) -> git2::RepositoryState {
    Repository::open(root).unwrap().state()
}

fn cli(root: &Path, args: &[&str]) -> GitOutput {
    run_git_output(root, args, "test", Duration::from_secs(30), "").unwrap()
}

/// hooks 目录放在工作区外(放里面会被当成未跟踪文件),返回目录。
fn hooks_dir(root: &Path) -> PathBuf {
    let hooks = root.with_file_name(format!(
        "{}-hooks",
        root.file_name().unwrap().to_string_lossy()
    ));
    std::fs::create_dir_all(&hooks).unwrap();
    git(
        root,
        &["config", "core.hooksPath", &hooks.to_string_lossy()],
    );
    hooks
}

fn write_hook(hooks: &Path, name: &str, body: &str) {
    let hook = hooks.join(name);
    std::fs::write(&hook, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

// ── 全部取消暂存:不许动仓库状态 ─────────────────────────────

/// 解决完冲突点「全部取消暂存」:libgit2 的 `reset(HEAD, Mixed)` 会顺手清掉 MERGE_HEAD,
/// 之后的「继续」就成了普通提交 —— 合并静默变成单亲。
#[test]
fn 全部取消暂存保留合并现场() {
    let root = init_repo_with_file("unstage-all-merge", "a.txt", "base\n");
    let main = current(&root);
    git(&root, &["checkout", "-q", "-b", "other"]);
    commit_file(&root, "a.txt", "theirs\n", "theirs");
    git(&root, &["checkout", "-q", &main]);
    commit_file(&root, "a.txt", "ours\n", "ours");
    assert!(
        !cli(&root, &["merge", "--no-edit", "other"]).success,
        "应当冲突"
    );
    std::fs::write(root.join("a.txt"), "merged\n").unwrap();
    git(&root, &["add", "a.txt"]);

    git_unstage_all(&root).unwrap();
    assert_eq!(
        state_of(&root),
        git2::RepositoryState::Merge,
        "合并现场还在"
    );
    assert!(root.join(".git").join("MERGE_HEAD").exists());
    assert!(staged(&root).is_empty(), "暂存区确实回到了 HEAD");

    git_stage(&root, &["a.txt".to_string()]).unwrap();
    continue_operation(&root, RepoOpKind::Merge).unwrap();
    let repo = Repository::open(&root).unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.parent_count(), 2, "合并提交必须是双亲");
    drop(head);
    drop(repo);
    cleanup(&[&root]);
}

/// 变基停在冲突上、拣选一串提交停在第一个上:`rebase-merge/` 与 `sequencer/` 同样不能被清。
#[test]
fn 全部取消暂存保留变基与拣选现场() {
    let root = init_repo_with_file("unstage-all-rebase", "a.txt", "base\n");
    let main = current(&root);
    git(&root, &["checkout", "-q", "-b", "topic"]);
    commit_file(&root, "a.txt", "topic\n", "topic edits a");
    commit_file(&root, "t.txt", "t\n", "topic adds t");
    git(&root, &["checkout", "-q", &main]);
    commit_file(&root, "a.txt", "main\n", "main edits a");

    git(&root, &["checkout", "-q", "topic"]);
    assert!(!cli(&root, &["rebase", &main]).success, "变基应当冲突");
    std::fs::write(root.join("a.txt"), "resolved\n").unwrap();
    git(&root, &["add", "a.txt"]);
    git_unstage_all(&root).unwrap();
    assert_eq!(
        op_kind_of(state_of(&root)),
        Some(RepoOpKind::Rebase),
        "变基现场还在"
    );
    assert!(root.join(".git").join("rebase-merge").exists());
    git(&root, &["rebase", "--abort"]);

    git(&root, &["checkout", "-q", &main]);
    assert!(
        !cli(&root, &["cherry-pick", "topic~1", "topic"]).success,
        "拣选应当停在第一个提交的冲突上"
    );
    assert!(root.join(".git").join("sequencer").exists());
    std::fs::write(root.join("a.txt"), "picked\n").unwrap();
    git(&root, &["add", "a.txt"]);
    git_unstage_all(&root).unwrap();
    assert_eq!(op_kind_of(state_of(&root)), Some(RepoOpKind::CherryPick));
    assert!(
        root.join(".git").join("sequencer").exists(),
        "还没拣的那些提交不能丢"
    );
    cleanup(&[&root]);
}

// ── 取消暂存 / 暂存:路径按字面量 ────────────────────────────

/// 文件名里的通配符按字面量:取消暂存 `[ab].txt` 不能连带 a.txt、b.txt。
/// `-` 开头的文件名不能被当成选项。
#[test]
fn 取消暂存按字面路径() {
    let root = init_repo_with_file("unstage-literal", "a.txt", "a\n");
    for name in ["b.txt", "[ab].txt", "-x.txt"] {
        std::fs::write(root.join(name), "x\n").unwrap();
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "files"]);
    for name in ["a.txt", "b.txt", "[ab].txt", "-x.txt"] {
        std::fs::write(root.join(name), "changed\n").unwrap();
    }
    git(&root, &["add", "-A"]);
    assert_eq!(staged(&root).len(), 4);

    git_unstage(&root, &["[ab].txt".to_string(), "-x.txt".to_string()]).unwrap();
    assert_eq!(staged(&root), vec!["a.txt", "b.txt"]);
    cleanup(&[&root]);
}

/// 还没有任何提交的仓库:取消暂存 = 从暂存区摘掉(没有 HEAD 可以 reset 回去)。
#[test]
fn 空仓库取消暂存() {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("mini-term-git-unborn-{ts}"));
    std::fs::create_dir_all(&root).unwrap();
    let root = crate::fs::strip_verbatim_prefix(std::fs::canonicalize(&root).unwrap());
    Repository::init(&root).unwrap();
    std::fs::write(root.join("f.txt"), "f\n").unwrap();
    std::fs::write(root.join("g.txt"), "g\n").unwrap();
    git_stage(&root, &["f.txt".to_string(), "g.txt".to_string()]).unwrap();
    git_unstage(&root, &["f.txt".to_string()]).unwrap();
    let names = |root: &Path| -> Vec<String> {
        let repo = Repository::open(root).unwrap();
        let index = repo.index().unwrap();
        index
            .iter()
            .map(|e| String::from_utf8_lossy(&e.path).into_owned())
            .collect()
    };
    assert_eq!(names(&root), vec!["g.txt"]);
    git_unstage_all(&root).unwrap();
    assert!(names(&root).is_empty());
    cleanup(&[&root]);
}

/// 暂存要过 clean 过滤器(git-lfs 就是一个 clean 过滤器):libgit2 不跑外部过滤驱动,
/// LFS 文件会以原始大文件入库。按文件暂存与全部暂存都一样。
#[test]
fn 暂存走_clean_过滤器() {
    let root = init_repo_with_file("stage-filter", "a.txt", "a\n");
    std::fs::write(root.join(".gitattributes"), "*.dat filter=upper\n").unwrap();
    git(&root, &["config", "filter.upper.clean", "tr a-z A-Z"]);
    std::fs::write(root.join("x.dat"), "hello\n").unwrap();
    git_stage(&root, &["x.dat".to_string()]).unwrap();
    assert_eq!(index_blob(&root, "x.dat"), "HELLO\n");

    std::fs::write(root.join("y.dat"), "world\n").unwrap();
    git_stage_all(&root).unwrap();
    assert_eq!(index_blob(&root, "y.dat"), "WORLD\n");
    cleanup(&[&root]);
}

/// 暂存:通配符与 `-` 开头的名字按字面量;工作区里删掉的文件暂存的是删除;
/// 全部暂存把删除也带上。
#[test]
fn 暂存按字面路径并带上删除() {
    let root = init_repo_with_file("stage-literal", "a.txt", "a\n");
    for name in ["b.txt", "[ab].txt", "-d.txt", "gone.txt"] {
        std::fs::write(root.join(name), "x\n").unwrap();
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "files"]);
    for name in ["a.txt", "b.txt", "[ab].txt"] {
        std::fs::write(root.join(name), "changed\n").unwrap();
    }
    std::fs::remove_file(root.join("-d.txt")).unwrap();
    std::fs::remove_file(root.join("gone.txt")).unwrap();

    git_stage(&root, &["[ab].txt".to_string(), "-d.txt".to_string()]).unwrap();
    assert_eq!(staged(&root), vec!["-d.txt", "[ab].txt"]);
    git_stage_all(&root).unwrap();
    assert_eq!(
        staged(&root),
        vec!["-d.txt", "[ab].txt", "a.txt", "b.txt", "gone.txt"]
    );
    let repo = Repository::open(&root).unwrap();
    assert!(
        repo.index()
            .unwrap()
            .get_path(Path::new("gone.txt"), 0)
            .is_none(),
        "删除已暂存"
    );
    drop(repo);
    cleanup(&[&root]);
}

// ── 执行器:超时 / 进程树 / 管道 ─────────────────────────────

/// 超时要连孙进程一起杀:Git for Windows 的 `git.exe` 只是个启动器,真正干活的 git、
/// hook 的 sh 与 sleep 都是它的后代。只杀启动器的话它们照跑,`commit -a` 在跑
/// pre-commit 期间攥着的 index.lock 也留着,之后的每条 git 命令都撞锁。
#[test]
fn 超时终止整棵进程树并清理_index_lock() {
    let root = init_repo_with_file("timeout-tree", "a.txt", "a\n");
    let hooks = hooks_dir(&root);
    let marker = hooks.join("survived");
    let marker_sh = marker.to_string_lossy().replace('\\', "/");
    write_hook(
        &hooks,
        "pre-commit",
        &format!("sleep 4\necho survived > \"{marker_sh}\""),
    );
    std::fs::write(root.join("a.txt"), "changed\n").unwrap();

    let err = match run_git_output(
        &root,
        &["commit", "-a", "-m", "x"],
        "commit",
        Duration::from_secs(2),
        "",
    ) {
        Ok(out) => panic!("应当超时: success={} {}", out.success, out.stderr),
        Err(err) => format!("{err:#}"),
    };
    assert!(err.contains("超时"), "{err}");
    assert!(
        !root.join(".git").join("index.lock").exists(),
        "本次留下的 index.lock 要清掉: {err}"
    );
    // hook 若没被一起终止,4s 后会写出标记文件
    std::thread::sleep(Duration::from_secs(5));
    assert!(!marker.exists(), "hook 里的 sleep 应随进程树一起被终止");
    assert!(
        cli(&root, &["status", "--porcelain"]).success,
        "仓库没被锁住"
    );
    cleanup(&[&root, &hooks]);
}

/// git 已经退出、孙进程还攥着管道:宽限期到了也要把已经读到的输出交出来,不能整块丢掉。
#[test]
fn 孙进程攥着管道时保留已读到的输出() {
    let root = init_repo_with_file("drain-partial", "a.txt", "a\n");
    let out = run_git_output(
        &root,
        &[
            "-c",
            "alias.mt-hold=!echo hello-from-git; sleep 5 &",
            "mt-hold",
        ],
        "alias",
        Duration::from_secs(20),
        "",
    )
    .unwrap();
    assert!(out.success, "{}", out.stderr);
    assert!(
        out.stdout.contains("hello-from-git"),
        "stdout: {:?}",
        out.stdout
    );
    // 别急着删目录:后台那个 sleep 的工作目录还在里面(Windows 上删不掉),等它退
    std::thread::sleep(Duration::from_secs(4));
    cleanup(&[&root]);
}

// ── 文件路径 → 仓库内路径 ───────────────────────────────────

fn changed_lines(result: &GitDiffResult) -> Vec<(String, String)> {
    result
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter(|l| l.kind != "context")
        .map(|l| (l.kind.clone(), l.content.clone()))
        .collect()
}

/// 经软链 / 联接点进入的项目:libgit2 的 workdir 是解析过的真实路径,项目路径没解析,
/// 直接 `diff_paths` 得出 `../<链接名>/…`,diff 误显示为整文件新增、按块暂存找不到条目、
/// 文件历史报「不在仓库里」、文件树状态全对不上。
fn assert_linked_project_resolves(link: &Path) {
    let rel = "src/a.txt";
    let diff = get_git_diff(link, rel, DiffSide::Unstaged).unwrap();
    assert_eq!(
        changed_lines(&diff),
        vec![
            ("delete".to_string(), "old".to_string()),
            ("add".to_string(), "new".to_string())
        ],
        "不能误判成整文件新增"
    );
    stage_hunk(link, rel, &diff, 0).unwrap();
    let (_, in_repo) = resolve_repo_file(link, rel).unwrap();
    assert_eq!(in_repo, rel);
    assert!(
        matches!(
            head_file(link, &link.join(rel)).unwrap(),
            HeadFile::Text { .. }
        ),
        "HEAD 版本取得到"
    );
    let status = get_git_status(link).unwrap();
    assert!(
        status.iter().any(|s| s.path == rel),
        "文件树状态的路径相对项目根: {status:?}"
    );
}

#[cfg(unix)]
#[test]
fn 软链目录下的项目路径换算正确() {
    let root = init_repo_with_file("symlink-real", "src/a.txt", "old\n");
    std::fs::write(root.join("src/a.txt"), "new\n").unwrap();
    let link = root.with_file_name(format!(
        "{}-link",
        root.file_name().unwrap().to_string_lossy()
    ));
    std::os::unix::fs::symlink(&root, &link).unwrap();
    assert_linked_project_resolves(&link);
    std::fs::remove_file(&link).ok();
    cleanup(&[&root]);
}

/// Windows 上用目录联接点(不需要管理员权限)走同一套断言:libgit2 在 Windows 上
/// 不解析联接点时这条本来就过,留着防止换算逻辑在 Windows 上反而算歪。
#[cfg(windows)]
#[test]
fn 联接点目录下的项目路径换算正确() {
    let root = init_repo_with_file("junction-real", "src/a.txt", "old\n");
    std::fs::write(root.join("src/a.txt"), "new\n").unwrap();
    let link = root.with_file_name(format!(
        "{}-link",
        root.file_name().unwrap().to_string_lossy()
    ));
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&root)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "建联接点失败");
    assert_linked_project_resolves(&link);
    std::fs::remove_dir(&link).ok();
    cleanup(&[&root]);
}
