//! 「更改」列表 / 文件树状态的重命名口径,与提交历史的显示时间。
//!
//! 走 git CLI 造场景(`git mv`),断言里再拿 `git status --porcelain` 对账 —— 面板的
//! 结论必须与命令行看到的一致。

use super::test_support::{git, init_repo_with_file};
use super::*;

/// 十行左右的文本:重命名后改一行仍在 libgit2 的相似度阈值之上(太短的文件改一行就
/// 不算重命名了)。
const BODY: &str = "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\n";

/// 一行变更:`(路径, 重命名前的路径, 暂存侧, 未暂存侧)`。
type ChangeRow = (String, Option<String>, Option<GitStatus>, Option<GitStatus>);

fn changes_of(root: &Path) -> Vec<ChangeRow> {
    let mut list: Vec<_> = get_changes_status(root)
        .unwrap()
        .into_iter()
        .map(|c| (c.path, c.old_path, c.staged_status, c.unstaged_status))
        .collect();
    list.sort_by(|a, b| a.0.cmp(&b.0));
    list
}

/// `git mv` 之后:「更改」里是**一行** R(新路径 + 旧路径),不是 A + D 两行;
/// 文件树的状态里只有新路径(旧路径已不在工作区)。
#[test]
fn 已暂存的重命名是一行_r() {
    let root = init_repo_with_file("rename-row", "src/old.txt", BODY);
    git(&root, &["mv", "src/old.txt", "src/new.txt"]);

    assert_eq!(
        changes_of(&root),
        vec![(
            "src/new.txt".to_string(),
            Some("src/old.txt".to_string()),
            Some(GitStatus::Renamed),
            None
        )]
    );
    let tree: Vec<(String, Option<String>, String)> = get_git_status(&root)
        .unwrap()
        .into_iter()
        .map(|s| (s.path, s.old_path, s.status_label))
        .collect();
    assert_eq!(
        tree,
        vec![(
            "src/new.txt".to_string(),
            Some("src/old.txt".to_string()),
            "R".to_string()
        )]
    );
    std::fs::remove_dir_all(&root).ok();
}

/// 已暂存的重命名 + 改了一行:暂存侧 diff 拿 HEAD 的旧路径比暂存区的新路径,只显示
/// 改的那一行;不给旧路径时新路径在 HEAD 里查不到,整份都成了新增(旧行为)。
#[test]
fn 重命名的暂存侧_diff_比旧路径() {
    let root = init_repo_with_file("rename-diff", "old.txt", BODY);
    git(&root, &["mv", "old.txt", "new.txt"]);
    std::fs::write(root.join("new.txt"), BODY.replace("line 5", "LINE 5")).unwrap();
    git(&root, &["add", "new.txt"]);
    assert_eq!(
        changes_of(&root)[0],
        (
            "new.txt".to_string(),
            Some("old.txt".to_string()),
            Some(GitStatus::Renamed),
            None
        )
    );

    let diff = get_git_diff_from(&root, "new.txt", Some("old.txt"), DiffSide::Staged).unwrap();
    let changed: Vec<(String, String)> = diff
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter(|l| l.kind != "context")
        .map(|l| (l.kind.clone(), l.content.clone()))
        .collect();
    assert_eq!(
        changed,
        vec![
            ("delete".to_string(), "line 5".to_string()),
            ("add".to_string(), "LINE 5".to_string()),
        ]
    );
    let whole = get_git_diff(&root, "new.txt", DiffSide::Staged).unwrap();
    assert!(
        whole.hunks[0].lines.iter().all(|l| l.kind == "add"),
        "不给旧路径时整份算新增"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// 取消暂存一个已暂存的重命名:新旧两条路径一起撤 —— 回到「旧路径工作区已删除 +
/// 新路径未跟踪」,与 `git restore --staged` 两条路径的结果一致。只撤新路径的话会
/// 裂成「已暂存 D + 未跟踪」。
#[test]
fn 取消暂存重命名恢复成删除加未跟踪() {
    let root = init_repo_with_file("rename-unstage", "old.txt", BODY);
    git(&root, &["mv", "old.txt", "new.txt"]);
    git_unstage(&root, &["new.txt".to_string()]).unwrap();

    assert_eq!(
        changes_of(&root),
        vec![
            (
                "new.txt".to_string(),
                None,
                None,
                Some(GitStatus::Untracked)
            ),
            ("old.txt".to_string(), None, None, Some(GitStatus::Deleted)),
        ]
    );
    let porcelain = git(&root, &["status", "--porcelain"]);
    let mut lines: Vec<&str> = porcelain.lines().map(str::trim).collect();
    lines.sort();
    assert_eq!(lines, vec!["?? new.txt", "D old.txt"]);

    // 再整组暂存回去:又是一行 R
    git_stage(&root, &["new.txt".to_string(), "old.txt".to_string()]).unwrap();
    assert_eq!(changes_of(&root)[0].2, Some(GitStatus::Renamed));
    std::fs::remove_dir_all(&root).ok();
}

/// 工作区一侧不做重命名检测:删掉一个已跟踪文件、另放一个同内容的未跟踪文件,
/// `git status` 认的是「删除 + 未跟踪」,面板也一样(未跟踪文件不参与配对)。
#[test]
fn 未跟踪文件不参与重命名配对() {
    let root = init_repo_with_file("rename-untracked", "old.txt", BODY);
    std::fs::remove_file(root.join("old.txt")).unwrap();
    std::fs::write(root.join("copy.txt"), BODY).unwrap();
    assert_eq!(
        changes_of(&root),
        vec![
            (
                "copy.txt".to_string(),
                None,
                None,
                Some(GitStatus::Untracked)
            ),
            ("old.txt".to_string(), None, None, Some(GitStatus::Deleted)),
        ]
    );
    std::fs::remove_dir_all(&root).ok();
}

/// 提交历史行里的时间是**作者时间**(与详情区、`git log` 默认一致):修改消息 / 拣选
/// 之后提交者时间是「刚刚」,作者时间还是原来那天。
#[test]
fn 历史行显示作者时间() {
    let root = init_repo_with_file("author-time", "a.txt", "a\n");
    let repo = Repository::open(&root).unwrap();
    std::fs::write(root.join("a.txt"), "b\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("a.txt")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let author_at = 1_700_000_000;
    let author = git2::Signature::new(
        "mini-term",
        "test@example.com",
        &git2::Time::new(author_at, 0),
    )
    .unwrap();
    let committer = git2::Signature::new(
        "mini-term",
        "test@example.com",
        &git2::Time::new(author_at + 86_400 * 30, 0),
    )
    .unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    repo.commit(
        Some("HEAD"),
        &author,
        &committer,
        "amended",
        &tree,
        &[&parent],
    )
    .unwrap();

    let log = get_git_log(&root, None, Some(10), None).unwrap();
    assert_eq!(log[0].message, "amended");
    assert_eq!(log[0].timestamp, author_at);
    std::fs::remove_dir_all(&root).ok();
}
