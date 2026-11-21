use super::test_support::init_repo_with_file;
use super::worktree::*;
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
    let result = get_git_diff(&project, "src/pages/task/my/my.vue", DiffSide::Unstaged).unwrap();
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
