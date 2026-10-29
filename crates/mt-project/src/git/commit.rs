//! 提交区的后端:提交 / 修改上次提交(amend)、提交消息历史、amend 前要看的 HEAD 信息、
//! 提交框占位符里的目标分支。
//!
//! 提交与 amend 走 git CLI(继承用户的 hooks / gpg 签名 / 身份配置,与
//! [`super::git_commit`] 同一条理由);其余全是只读查询,走 git2。

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, anyhow};
use git2::{BranchType, Oid, Repository, Sort};

use super::run_git_command_labeled;

/// 一次成功提交的结果(给「已提交 abc1234:标题」那条 toast 用)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    pub short_hash: String,
    pub summary: String,
}

/// 提交(`amend = true` 时是 `git commit --amend`)。
///
/// 消息走 `-m <msg>` **独立参数**:多行、以 `-` 开头都安全(`-m` 必带一个值,下一个参数
/// 一律当消息本身),错误信息里的操作名固定写 `commit`,不把整条消息灌进去。
/// amend 允许暂存区为空 —— 那就是只改消息。
///
/// **阻塞最多 60s**(pre-commit hook / gpg),必须在后台线程上调用。
pub fn git_commit_with(repo_path: &Path, message: &str, amend: bool) -> Result<CommitOutcome> {
    let mut args = vec!["commit"];
    if amend {
        args.push("--amend");
    }
    args.extend(["-m", message]);
    run_git_command_labeled(
        repo_path,
        &args,
        if amend { "commit --amend" } else { "commit" },
        Duration::from_secs(60),
        ",可能卡在 pre-commit hook 或 gpg 签名上",
    )?;
    head_outcome(repo_path)
}

fn head_outcome(repo_path: &Path) -> Result<CommitOutcome> {
    let repo = Repository::open(repo_path)?;
    let commit = repo.head()?.peel_to_commit()?;
    Ok(CommitOutcome {
        short_hash: short_hash(commit.id()),
        summary: commit.summary().unwrap_or_default().to_string(),
    })
}

fn short_hash(oid: Oid) -> String {
    oid.to_string().chars().take(7).collect()
}

/// 勾选「修改上次提交」时要的 HEAD 信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmendInfo {
    /// HEAD 的完整提交消息(输入框为空时预填)
    pub message: String,
    /// HEAD 已被某个远程跟踪分支包含 —— 改了之后要强制推送
    pub pushed: bool,
}

/// 空仓库(还没有任何提交)返回 `None`:没有「上次提交」可改。
pub fn amend_info(repo_path: &Path) -> Result<Option<AmendInfo>> {
    let repo = Repository::open(repo_path)?;
    let Ok(head) = repo.head() else {
        return Ok(None);
    };
    let commit = head.peel_to_commit()?;
    Ok(Some(AmendInfo {
        message: commit.message().unwrap_or_default().to_string(),
        pushed: is_pushed(&repo, commit.id()),
    }))
}

/// `oid` 是否已被任何远程跟踪分支(`refs/remotes/*`)包含。
///
/// 远程跟踪分支是本地最后一次 fetch / push 时的快照,不联网;多一个分支就多一次
/// 祖先判定,数量封顶避免远程分支成千上万的仓库卡住后台线程(超出部分不看,
/// 宁可漏报「已推送」也不让勾选框转半天)。
fn is_pushed(repo: &Repository, oid: Oid) -> bool {
    const MAX_REMOTE_BRANCHES: usize = 500;
    let Ok(branches) = repo.branches(Some(BranchType::Remote)) else {
        return false;
    };
    branches
        .flatten()
        .take(MAX_REMOTE_BRANCHES)
        .filter_map(|(branch, _)| branch.get().target())
        .any(|tip| tip == oid || repo.graph_descendant_of(tip, oid).unwrap_or(false))
}

/// 仓库里有没有任何提交(没有就谈不上 amend)。打不开仓库按「没有」算。
pub fn head_exists(repo_path: &Path) -> bool {
    Repository::open(repo_path).is_ok_and(|repo| repo.head().is_ok())
}

/// 提交框占位符里的「提交到 xxx」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitTarget {
    /// 在某个分支上(含还没有任何提交的新分支)
    Branch(String),
    /// 游离 HEAD,带短 hash
    Detached(String),
}

/// 当前 HEAD 指向哪儿。空仓库也能给出分支名(HEAD 仍是指向 `refs/heads/<name>` 的符号引用)。
pub fn commit_target(repo_path: &Path) -> Result<CommitTarget> {
    let repo = Repository::open(repo_path)?;
    let head_ref = repo.find_reference("HEAD")?;
    if let Some(target) = head_ref.symbolic_target() {
        let name = target.strip_prefix("refs/heads/").unwrap_or(target);
        return Ok(CommitTarget::Branch(name.to_string()));
    }
    let oid = head_ref
        .target()
        .ok_or_else(|| anyhow!("HEAD 既不是符号引用也没有指向提交"))?;
    Ok(CommitTarget::Detached(short_hash(oid)))
}

/// 提交消息历史里的一条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMessageEntry {
    /// 完整消息(选中后整段填进输入框)
    pub message: String,
    /// 首行(菜单里显示)
    pub summary: String,
    /// 提交时间(秒)
    pub timestamp: i64,
}

/// 当前用户在本仓库最近的 `limit` 条提交消息(从 HEAD 往回走,按时间倒序,同文去重)。
///
/// 「当前用户」按 `user.email` 认(大小写不敏感);读不到就不过滤。最多翻
/// `SCAN_LIMIT` 个提交 —— 别人提交占多数的大仓库里,凑不满也不再往下翻。
pub fn recent_commit_messages(repo_path: &Path, limit: usize) -> Result<Vec<CommitMessageEntry>> {
    const SCAN_LIMIT: usize = 5000;
    let repo = Repository::open(repo_path)?;
    if repo.head().is_err() {
        return Ok(Vec::new());
    }
    let email = repo
        .config()
        .ok()
        .and_then(|c| c.get_string("user.email").ok())
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty());

    let mut walk = repo.revwalk()?;
    walk.push_head()?;
    walk.set_sorting(Sort::TIME)?;

    let mut out: Vec<CommitMessageEntry> = Vec::new();
    for oid in walk.take(SCAN_LIMIT) {
        if out.len() >= limit {
            break;
        }
        let Ok(commit) = repo.find_commit(oid?) else {
            continue;
        };
        if let Some(email) = &email {
            let author = commit.author();
            let mine = author
                .email()
                .is_some_and(|e| e.trim().eq_ignore_ascii_case(email));
            if !mine {
                continue;
            }
        }
        let message = commit.message().unwrap_or_default().trim_end().to_string();
        if message.is_empty() || out.iter().any(|e| e.message == message) {
            continue;
        }
        out.push(CommitMessageEntry {
            summary: message.lines().next().unwrap_or_default().to_string(),
            message,
            timestamp: commit.time().seconds(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::init_repo_with_file;
    use super::super::{get_changes_status, git_stage};
    use super::*;

    fn commit_all(root: &Path, message: &str) -> CommitOutcome {
        let paths: Vec<String> = get_changes_status(root)
            .unwrap()
            .into_iter()
            .map(|c| c.path)
            .collect();
        git_stage(root, &paths).unwrap();
        git_commit_with(root, message, false).unwrap()
    }

    /// 多行、以 `-` 开头的消息原样落进提交;结果带短 hash 与标题。
    #[test]
    fn 提交消息多行与横杠开头都安全() {
        let root = init_repo_with_file("commit-msg", "a.txt", "a\n");
        std::fs::write(root.join("a.txt"), "b\n").unwrap();
        let outcome = commit_all(&root, "-n 不是参数\n\n正文第二段");
        assert_eq!(outcome.summary, "-n 不是参数");
        assert_eq!(outcome.short_hash.len(), 7);
        let repo = Repository::open(&root).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(
            head.message().unwrap().trim_end(),
            "-n 不是参数\n\n正文第二段"
        );
        assert!(head.id().to_string().starts_with(&outcome.short_hash));
        std::fs::remove_dir_all(&root).ok();
    }

    /// amend:暂存区为空也能只改消息;提交数不变,父提交不变。
    #[test]
    fn amend_只改消息() {
        let root = init_repo_with_file("commit-amend", "a.txt", "a\n");
        std::fs::write(root.join("a.txt"), "b\n").unwrap();
        commit_all(&root, "第一版消息");
        let repo = Repository::open(&root).unwrap();
        let before = repo.head().unwrap().peel_to_commit().unwrap();
        let parent = before.parent_id(0).unwrap();

        let info = amend_info(&root).unwrap().unwrap();
        assert_eq!(info.message.trim_end(), "第一版消息");
        assert!(!info.pushed, "没有远程跟踪分支");

        let outcome = git_commit_with(&root, "改过的消息", true).unwrap();
        assert_eq!(outcome.summary, "改过的消息");
        let after = repo.head().unwrap().peel_to_commit().unwrap();
        assert_ne!(after.id(), before.id());
        assert_eq!(after.parent_id(0).unwrap(), parent, "amend 不新增提交");
        assert_eq!(after.tree_id(), before.tree_id(), "暂存区为空 = 只改消息");
        std::fs::remove_dir_all(&root).ok();
    }

    /// 「已推送」= 被任一远程跟踪分支包含(含远程分支领先的情况);不联网,手写远程引用。
    #[test]
    fn amend_已推送判定() {
        let root = init_repo_with_file("commit-pushed", "a.txt", "a\n");
        let repo = Repository::open(&root).unwrap();
        let first = repo.head().unwrap().target().unwrap();
        repo.remote("origin", "https://example.invalid/repo.git")
            .unwrap();
        repo.reference("refs/remotes/origin/main", first, true, "test")
            .unwrap();
        assert!(
            amend_info(&root).unwrap().unwrap().pushed,
            "HEAD 就是远程分支的头"
        );

        // 本地新提交还没推:不算已推送
        std::fs::write(root.join("a.txt"), "b\n").unwrap();
        commit_all(&root, "本地新提交");
        assert!(!amend_info(&root).unwrap().unwrap().pushed);

        // 远程分支走到了本地 HEAD 之后:HEAD 被包含,算已推送
        let head = repo.head().unwrap().target().unwrap();
        repo.reference("refs/remotes/origin/feature", head, true, "test")
            .unwrap();
        assert!(amend_info(&root).unwrap().unwrap().pushed);
        std::fs::remove_dir_all(&root).ok();
    }

    /// 空仓库:没有可改的上次提交;占位符仍能给出分支名。
    #[test]
    fn 空仓库没有上次提交() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mini-term-git-empty-{ts}"));
        std::fs::create_dir_all(&root).unwrap();
        let repo = Repository::init(&root).unwrap();
        repo.set_head("refs/heads/trunk").unwrap();
        assert_eq!(amend_info(&root).unwrap(), None);
        assert_eq!(
            commit_target(&root).unwrap(),
            CommitTarget::Branch("trunk".to_string())
        );
        assert!(recent_commit_messages(&root, 20).unwrap().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 游离_head_的提交目标() {
        let root = init_repo_with_file("commit-detached", "a.txt", "a\n");
        let repo = Repository::open(&root).unwrap();
        let head = repo.head().unwrap().target().unwrap();
        repo.set_head_detached(head).unwrap();
        assert_eq!(
            commit_target(&root).unwrap(),
            CommitTarget::Detached(short_hash(head))
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 消息历史:只要当前用户的、新的在前、同文去重、按 limit 截断。
    #[test]
    fn 消息历史只列当前用户() {
        let root = init_repo_with_file("commit-history", "a.txt", "a\n");
        let repo = Repository::open(&root).unwrap();
        let other = git2::Signature::new(
            "someone",
            "other@example.com",
            &git2::Time::new(2_000_000_000, 0),
        )
        .unwrap();
        let mine = |secs: i64| {
            git2::Signature::new("mini-term", "TEST@example.com", &git2::Time::new(secs, 0))
                .unwrap()
        };
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let mut parent = repo.head().unwrap().peel_to_commit().unwrap();
        for (sig, msg) in [
            (mine(1_900_000_001), "feat: 一\n\n正文"),
            (other, "别人的提交"),
            (mine(1_900_000_003), "fix: 二"),
            (mine(1_900_000_004), "fix: 二"),
            (mine(1_900_000_005), "docs: 三"),
        ] {
            let oid = repo
                .commit(Some("HEAD"), &sig, &sig, msg, &tree, &[&parent])
                .unwrap();
            parent = repo.find_commit(oid).unwrap();
        }
        let list = recent_commit_messages(&root, 20).unwrap();
        let summaries: Vec<&str> = list.iter().map(|e| e.summary.as_str()).collect();
        // 「init」是脚手架用同一邮箱提交的,也算当前用户
        assert_eq!(summaries, vec!["docs: 三", "fix: 二", "feat: 一", "init"]);
        assert_eq!(list[2].message, "feat: 一\n\n正文");
        assert_eq!(recent_commit_messages(&root, 2).unwrap().len(), 2);
        std::fs::remove_dir_all(&root).ok();
    }
}
