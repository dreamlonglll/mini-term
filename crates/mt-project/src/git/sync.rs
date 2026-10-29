//! 同步:Fetch / 带模式的 Pull / 带选项的 Push / 待推送提交。
//!
//! 与 [`super::git_pull`] / [`super::git_push`](无参、30s)并存 —— 那两个另一条线
//! 的「提交并推送」可能还在用,这里的是新起的名字。
//!
//! 结果判定照旧只看结构化信号:HEAD 与远程跟踪引用前后的变化、`repo.state()`、
//! `git push --porcelain` 的标志位(机器可读格式,不随界面语言变)。
//! **全部阻塞**(网络操作最多 120s),调用方放后台执行器。

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use git2::{BranchType, Oid, Repository};

use super::branch::{
    NETWORK_HINT, NETWORK_TIMEOUT, NO_EDITOR, failure_text, head_branch_name, remote_names,
    run_checked,
};
use super::repo_state::{conflicted_file_count, op_kind_of};
use super::{GitCommitInfo, commit_info, run_git_output, run_git_output_env};

/// 全部远程跟踪引用 → 指向的提交。
fn remote_refs(repo: &Repository) -> HashMap<String, Oid> {
    let mut out = HashMap::new();
    if let Ok(refs) = repo.references_glob("refs/remotes/*") {
        for reference in refs.flatten() {
            if let (Some(name), Some(oid)) = (reference.name(), reference.target()) {
                out.insert(name.to_string(), oid);
            }
        }
    }
    out
}

/// 两份引用快照之间变了几条(新增 / 移动 / 消失都算)。
fn changed_refs(before: &HashMap<String, Oid>, after: &HashMap<String, Oid>) -> usize {
    let moved = after
        .iter()
        .filter(|(name, oid)| before.get(*name) != Some(*oid))
        .count();
    let gone = before
        .keys()
        .filter(|name| !after.contains_key(*name))
        .count();
    moved + gone
}

/// 从 `tip` 可达、从 `hide` 都不可达的提交数(最多数到 `cap`)。
fn count_between(repo: &Repository, tip: Oid, hide: &[Oid], cap: usize) -> usize {
    let Ok(mut walk) = repo.revwalk() else {
        return 0;
    };
    if walk.push(tip).is_err() {
        return 0;
    }
    for oid in hide {
        let _ = walk.hide(*oid);
    }
    walk.take(cap).filter(|r| r.is_ok()).count()
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

/// 一次 Fetch 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchOutcome {
    /// 有变化的远程跟踪引用条数。0 = 已是最新。
    pub updated_refs: usize,
}

/// `git fetch --all`(**不** prune:删远端分支的影响由用户自己决定要不要清)。
pub fn git_fetch_all(repo_path: &Path) -> Result<FetchOutcome> {
    let before = remote_refs(&Repository::open(repo_path)?);
    run_checked(
        repo_path,
        &["fetch", "--all"],
        "fetch",
        NETWORK_TIMEOUT,
        NETWORK_HINT,
    )?;
    let after = remote_refs(&Repository::open(repo_path)?);
    Ok(FetchOutcome {
        updated_refs: changed_refs(&before, &after),
    })
}

// ---------------------------------------------------------------------------
// Pull
// ---------------------------------------------------------------------------

/// 拉取方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PullMode {
    /// 合并(`--no-rebase`:压过用户 `pull.rebase=true` 的配置,按钮上写的是什么就做什么)。
    #[default]
    Merge,
    /// 变基(`--rebase`)。
    Rebase,
    /// 仅快进(`--ff-only`):分叉了就拒绝,什么都不改。
    FfOnly,
}

impl PullMode {
    fn args(self) -> &'static [&'static str] {
        match self {
            PullMode::Merge => &["pull", "--no-rebase", "--no-edit"],
            PullMode::Rebase => &["pull", "--rebase"],
            PullMode::FfOnly => &["pull", "--ff-only"],
        }
    }
}

/// 一次 Pull 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullOutcome {
    UpToDate,
    /// 拉下来 `commits` 个新提交(上游有、拉取前本地没有的)。
    Updated {
        commits: usize,
    },
    /// 合并 / 变基停在冲突上(`files` = 冲突文件数)。
    Conflicts {
        files: usize,
    },
}

/// 数提交的上限 —— 只是给摘要用的数字,不必为几万个提交走完整张图。
const COUNT_CAP: usize = 10_000;

/// 按指定方式拉取当前分支。
pub fn git_pull_with(repo_path: &Path, mode: PullMode) -> Result<PullOutcome> {
    let repo = Repository::open(repo_path)?;
    let branch = head_branch_name(&repo).ok_or_else(|| anyhow!("分离 HEAD 状态下无法拉取"))?;
    let before = repo
        .head()?
        .target()
        .ok_or_else(|| anyhow!("当前没有提交"))?;
    drop(repo);
    let out = run_git_output_env(
        repo_path,
        mode.args(),
        NO_EDITOR,
        "pull",
        NETWORK_TIMEOUT,
        NETWORK_HINT,
    )?;
    let repo = Repository::open(repo_path)?;
    if !out.success {
        if op_kind_of(repo.state()).is_some() {
            return Ok(PullOutcome::Conflicts {
                files: conflicted_file_count(&repo),
            });
        }
        bail!("{}", failure_text(&out));
    }
    let after = repo.head()?.target();
    if after == Some(before) {
        return Ok(PullOutcome::UpToDate);
    }
    // 「拉了几个」按上游算:上游有、拉取前本地没有的。变基模式下 HEAD 上还有改写过的
    // 本地提交,拿 HEAD 数会把它们也算进来
    let upstream = repo
        .find_branch(&branch, BranchType::Local)
        .ok()
        .and_then(|b| b.upstream().ok())
        .and_then(|u| u.get().target());
    let tip = upstream.or(after).unwrap_or(before);
    Ok(PullOutcome::Updated {
        commits: count_between(&repo, tip, &[before], COUNT_CAP),
    })
}

// ---------------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------------

/// 推送目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTarget {
    /// 要推的本地分支。
    pub branch: String,
    pub remote: String,
    /// 远端的分支名(有上游时取上游的,可能与本地不同名;没上游时同名)。
    pub remote_branch: String,
    /// 已配上游。`false` = 这次推送会顺带 `-u` 设上游。
    pub has_upstream: bool,
}

/// 能不能推、推到哪。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushPlan {
    /// 分离 HEAD:没有分支可推。
    Detached,
    /// 一个远程都没配。
    NoRemote {
        branch: String,
    },
    Ready(PushTarget),
}

/// 没上游时推到哪个远程:只有一个就是它,有 `origin` 就 `origin`,否则取第一个。
fn default_remote(remotes: &[String]) -> Option<String> {
    if remotes.len() == 1 {
        return remotes.first().cloned();
    }
    remotes
        .iter()
        .find(|r| *r == "origin")
        .or_else(|| remotes.first())
        .cloned()
}

/// 算推送计划。`branch = None` 表示当前分支。
///
/// 上游按配置读(`branch.<b>.remote` + `branch.<b>.merge`),不要求远程跟踪引用在场 ——
/// 「远端分支被删了又 prune 过」时上游配置还在,推送会把它重新建出来。
pub fn push_plan(repo_path: &Path, branch: Option<&str>) -> Result<PushPlan> {
    let repo = Repository::open(repo_path)?;
    let branch = match branch {
        Some(b) => {
            repo.find_branch(b, BranchType::Local)
                .map_err(|_| anyhow!("未找到本地分支:{b}"))?;
            b.to_string()
        }
        None => match head_branch_name(&repo) {
            Some(b) => b,
            None => return Ok(PushPlan::Detached),
        },
    };
    let config = repo.config()?;
    let remote = config.get_string(&format!("branch.{branch}.remote")).ok();
    let merge = config.get_string(&format!("branch.{branch}.merge")).ok();
    let remotes = remote_names(&repo);
    if let (Some(remote), Some(merge)) = (remote, merge)
        && remotes.contains(&remote)
    {
        let remote_branch = merge
            .strip_prefix("refs/heads/")
            .unwrap_or(&merge)
            .to_string();
        return Ok(PushPlan::Ready(PushTarget {
            branch,
            remote,
            remote_branch,
            has_upstream: true,
        }));
    }
    match default_remote(&remotes) {
        Some(remote) => Ok(PushPlan::Ready(PushTarget {
            remote_branch: branch.clone(),
            branch,
            remote,
            has_upstream: false,
        })),
        None => Ok(PushPlan::NoRemote { branch }),
    }
}

/// 待推送的提交。
#[derive(Debug, Clone)]
pub struct OutgoingCommits {
    /// 新的在前。
    pub commits: Vec<GitCommitInfo>,
    /// 超出 `limit` 被截掉了。
    pub truncated: bool,
}

/// 待推送的提交:分支有、上游没有的;没上游时取「不在任何远程分支上」的。
/// `branch = None` 表示当前分支;detached / 空仓库返回空列表。
///
/// 遍历不设排序(libgit2 一设排序就先走完全部可达提交再排,见 `get_git_log`
/// 上方的说明),按提交时间增量出,够列表用。
pub fn outgoing_commits(
    repo_path: &Path,
    branch: Option<&str>,
    limit: usize,
) -> Result<OutgoingCommits> {
    let repo = Repository::open(repo_path)?;
    let name = match branch {
        Some(b) => Some(b.to_string()),
        None => head_branch_name(&repo),
    };
    let empty = OutgoingCommits {
        commits: Vec::new(),
        truncated: false,
    };
    let Some(name) = name else {
        return Ok(empty);
    };
    let Ok(local) = repo.find_branch(&name, BranchType::Local) else {
        return Ok(empty);
    };
    let Some(tip) = local.get().target() else {
        return Ok(empty);
    };
    let hide: Vec<Oid> = match local.upstream().ok().and_then(|u| u.get().target()) {
        Some(upstream) => vec![upstream],
        None => remote_refs(&repo).into_values().collect(),
    };
    let mut walk = repo.revwalk()?;
    walk.push(tip)?;
    for oid in hide {
        let _ = walk.hide(oid);
    }
    let mut commits = Vec::new();
    let mut truncated = false;
    for oid in walk {
        let oid = oid?;
        if commits.len() >= limit {
            truncated = true;
            break;
        }
        commits.push(commit_info(&repo.find_commit(oid)?));
    }
    Ok(OutgoingCommits { commits, truncated })
}

/// 推送选项。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushOptions {
    /// `None` = 当前分支。
    pub branch: Option<String>,
    /// `--force-with-lease`:远端有本地没见过的新提交时拒绝,否则覆盖远端历史。
    pub force_with_lease: bool,
}

/// 一次推送的结果。网络 / 认证这类失败是 `Err`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    Pushed(PushTarget),
    /// 被远端以「不是快进」拒绝(`[rejected]`:non-fast-forward / fetch first /
    /// stale info)—— 应当先拉取。`detail` 是 git 的输出原文。
    Rejected {
        detail: String,
    },
}

/// `git push --porcelain` 的输出里有没有「被拒(要先拉取)」的那种行。
///
/// porcelain 格式是机器可读的(`<标志>\t<from>:<to>\t<摘要>`),标志 `!` = 失败,
/// 摘要 `[rejected]` 只出现在非快进 / 需先 fetch / lease 过期三种情形;
/// 钩子拒绝是 `[remote rejected]`,不算(拉取解决不了)。这些串不随界面语言翻译。
pub fn push_was_rejected(porcelain: &str) -> bool {
    porcelain
        .lines()
        .any(|line| line.starts_with('!') && line.contains("[rejected]"))
}

/// 按选项推送。没上游时带 `-u` 顺带设上游;refspec 写全 `refs/heads/`,本地恰好有
/// 同名 tag 也不会推错。
pub fn git_push_with(repo_path: &Path, options: &PushOptions) -> Result<PushOutcome> {
    let target = match push_plan(repo_path, options.branch.as_deref())? {
        PushPlan::Ready(target) => target,
        PushPlan::Detached => bail!("分离 HEAD 状态下无法推送"),
        PushPlan::NoRemote { .. } => bail!("没有配置远程仓库"),
    };
    let refspec = format!(
        "refs/heads/{}:refs/heads/{}",
        target.branch, target.remote_branch
    );
    let mut args: Vec<&str> = vec!["push", "--porcelain"];
    if !target.has_upstream {
        args.push("-u");
    }
    if options.force_with_lease {
        args.push("--force-with-lease");
    }
    args.push(&target.remote);
    args.push(&refspec);
    let out = run_git_output(repo_path, &args, "push", NETWORK_TIMEOUT, NETWORK_HINT)?;
    if out.success {
        return Ok(PushOutcome::Pushed(target));
    }
    if push_was_rejected(&out.stdout) {
        let detail = [out.stdout.trim(), out.stderr.trim()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(PushOutcome::Rejected { detail });
    }
    bail!("{}", failure_text(&out));
}

#[cfg(test)]
mod tests {
    use super::super::branch::test_cli::{
        cleanup, clone_of, commit_file, current, git, repo_with_origin,
    };
    use super::super::test_support::init_repo_with_file;
    use super::*;

    #[test]
    fn porcelain_被拒判定() {
        let rejected = "To ../o.git\n!\trefs/heads/main:refs/heads/main\t[rejected] (non-fast-forward)\nDone\n";
        assert!(push_was_rejected(rejected));
        let fetch_first = "!\trefs/heads/main:refs/heads/main\t[rejected] (fetch first)";
        assert!(push_was_rejected(fetch_first));
        // 钩子拒绝不是「先拉取」能解决的
        let hook =
            "!\trefs/heads/main:refs/heads/main\t[remote rejected] (pre-receive hook declined)";
        assert!(!push_was_rejected(hook));
        let ok = "To ../o.git\n \trefs/heads/main:refs/heads/main\t1a2b..3c4d\nDone\n";
        assert!(!push_was_rejected(ok));
    }

    #[test]
    fn 默认远程() {
        assert_eq!(default_remote(&[]), None);
        assert_eq!(default_remote(&["fork".into()]), Some("fork".into()));
        assert_eq!(
            default_remote(&["fork".into(), "origin".into()]),
            Some("origin".into())
        );
        assert_eq!(default_remote(&["b".into(), "a".into()]), Some("b".into()));
    }

    /// Fetch 数得出变了几条远程引用;再 fetch 一次就是 0。
    #[test]
    fn fetch_统计更新的远程引用() {
        let (work, bare, branch) = repo_with_origin("sync-fetch");
        let other = clone_of(&bare, &branch);
        commit_file(&other, "o.txt", "o\n", "o");
        git(&other, &["push", "-q", "origin", &branch]);
        git(
            &other,
            &[
                "push",
                "-q",
                "origin",
                &format!("{branch}:refs/heads/extra"),
            ],
        );

        assert_eq!(git_fetch_all(&work).unwrap().updated_refs, 2);
        assert_eq!(git_fetch_all(&work).unwrap().updated_refs, 0);
        cleanup(&[&work, &bare, &other]);
    }

    /// 三种拉取方式:已是最新 / 拉了 N 个 / 仅快进遇分叉报错 / 合并冲突停住。
    #[test]
    fn 拉取三种方式() {
        let (work, bare, branch) = repo_with_origin("sync-pull");
        assert_eq!(
            git_pull_with(&work, PullMode::Merge).unwrap(),
            PullOutcome::UpToDate
        );

        let other = clone_of(&bare, &branch);
        commit_file(&other, "o1.txt", "1\n", "o1");
        commit_file(&other, "o2.txt", "2\n", "o2");
        git(&other, &["push", "-q", "origin", &branch]);
        assert_eq!(
            git_pull_with(&work, PullMode::FfOnly).unwrap(),
            PullOutcome::Updated { commits: 2 }
        );

        // 两边分叉:仅快进拒绝,HEAD 不动
        commit_file(&other, "o3.txt", "3\n", "o3");
        git(&other, &["push", "-q", "origin", &branch]);
        commit_file(&work, "w.txt", "w\n", "w");
        let head = git(&work, &["rev-parse", "HEAD"]);
        assert!(git_pull_with(&work, PullMode::FfOnly).is_err());
        assert_eq!(git(&work, &["rev-parse", "HEAD"]), head);

        // 变基:拉到 1 个,本地提交被改写到上游之上
        assert_eq!(
            git_pull_with(&work, PullMode::Rebase).unwrap(),
            PullOutcome::Updated { commits: 1 }
        );
        assert_eq!(git(&work, &["rev-list", "--count", "@{u}..HEAD"]), "1");

        // 合并冲突
        git(&work, &["push", "-q", "origin", &branch]);
        git(&other, &["pull", "-q", "--no-rebase", "origin", &branch]);
        commit_file(&other, "a.txt", "theirs\n", "theirs");
        git(&other, &["push", "-q", "origin", &branch]);
        commit_file(&work, "a.txt", "ours\n", "ours");
        assert_eq!(
            git_pull_with(&work, PullMode::Merge).unwrap(),
            PullOutcome::Conflicts { files: 1 }
        );
        cleanup(&[&work, &bare, &other]);
    }

    /// 推送计划:有上游 / 没上游(取 origin、将设上游)/ 没远程 / detached。
    #[test]
    fn 推送计划() {
        let (work, bare, branch) = repo_with_origin("sync-plan");
        assert_eq!(
            push_plan(&work, None).unwrap(),
            PushPlan::Ready(PushTarget {
                branch: branch.clone(),
                remote: "origin".into(),
                remote_branch: branch.clone(),
                has_upstream: true,
            })
        );
        git(&work, &["checkout", "-q", "-b", "fresh"]);
        assert_eq!(
            push_plan(&work, None).unwrap(),
            PushPlan::Ready(PushTarget {
                branch: "fresh".into(),
                remote: "origin".into(),
                remote_branch: "fresh".into(),
                has_upstream: false,
            })
        );
        let oid = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["checkout", "-q", &oid]);
        assert_eq!(push_plan(&work, None).unwrap(), PushPlan::Detached);
        cleanup(&[&work, &bare]);

        let lonely = init_repo_with_file("sync-plan-none", "a.txt", "a\n");
        let name = current(&lonely);
        assert_eq!(
            push_plan(&lonely, None).unwrap(),
            PushPlan::NoRemote { branch: name }
        );
        cleanup(&[&lonely]);
    }

    /// 待推送:有上游时 = 上游之后的;没上游时 = 不在任何远程分支上的;截断标记。
    #[test]
    fn 待推送提交() {
        let (work, bare, _) = repo_with_origin("sync-outgoing");
        assert!(
            outgoing_commits(&work, None, 100)
                .unwrap()
                .commits
                .is_empty()
        );
        commit_file(&work, "1.txt", "1\n", "one");
        commit_file(&work, "2.txt", "2\n", "two");
        let out = outgoing_commits(&work, None, 100).unwrap();
        let titles: Vec<&str> = out.commits.iter().map(|c| c.message.as_str()).collect();
        assert_eq!(titles, vec!["two", "one"]);
        assert!(!out.truncated);
        let cut = outgoing_commits(&work, None, 1).unwrap();
        assert_eq!(cut.commits.len(), 1);
        assert!(cut.truncated);

        // 没上游的新分支:只列出不在任何远程分支上的那些
        git(&work, &["checkout", "-q", "-b", "fresh"]);
        commit_file(&work, "3.txt", "3\n", "three");
        let fresh = outgoing_commits(&work, None, 100).unwrap();
        let titles: Vec<&str> = fresh.commits.iter().map(|c| c.message.as_str()).collect();
        assert_eq!(titles, vec!["three", "two", "one"]);
        cleanup(&[&work, &bare]);
    }

    /// 推送:普通推送;没上游时设上游;远端领先时被拒(Rejected,不是 Err);
    /// `--force-with-lease` 在已 fetch 的前提下能覆盖。
    #[test]
    fn 推送与强推() {
        let (work, bare, branch) = repo_with_origin("sync-push");
        commit_file(&work, "1.txt", "1\n", "one");
        assert!(matches!(
            git_push_with(&work, &PushOptions::default()).unwrap(),
            PushOutcome::Pushed(_)
        ));
        assert!(
            outgoing_commits(&work, None, 100)
                .unwrap()
                .commits
                .is_empty()
        );

        // 新分支没上游:推完就有了
        git(&work, &["checkout", "-q", "-b", "fresh"]);
        commit_file(&work, "f.txt", "f\n", "fresh");
        git_push_with(&work, &PushOptions::default()).unwrap();
        assert_eq!(
            super::super::ahead_behind(&work).unwrap().unwrap().upstream,
            "origin/fresh"
        );
        git(&work, &["checkout", "-q", &branch]);

        // 远端领先 → 被拒
        let other = clone_of(&bare, &branch);
        commit_file(&other, "o.txt", "o\n", "other");
        git(&other, &["push", "-q", "origin", &branch]);
        commit_file(&work, "w.txt", "w\n", "mine");
        match git_push_with(&work, &PushOptions::default()).unwrap() {
            PushOutcome::Rejected { detail } => assert!(detail.contains("[rejected]"), "{detail}"),
            other => panic!("应被拒: {other:?}"),
        }

        // fetch 过之后带 lease 强推:覆盖远端
        git(&work, &["fetch", "-q", "origin"]);
        let forced = git_push_with(
            &work,
            &PushOptions {
                branch: None,
                force_with_lease: true,
            },
        )
        .unwrap();
        assert!(matches!(forced, PushOutcome::Pushed(_)), "{forced:?}");
        assert_eq!(
            git(&bare, &["rev-parse", &branch]),
            git(&work, &["rev-parse", "HEAD"])
        );
        cleanup(&[&work, &bare, &other]);
    }
}
