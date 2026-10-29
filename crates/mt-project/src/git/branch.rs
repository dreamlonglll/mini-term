//! 分支:名称校验 / 检出 / 新建 / 重命名 / 删除 / 合并 / 变基 / 最近分支 / 上游领先落后。
//!
//! # 两条口径
//!
//! - **改仓库状态的走 git CLI,只读查询走 git2**。CLI 天然继承用户的 hooks、
//!   `merge.*` / `rebase.*` 配置与凭证;git2 读引用 / reflog / 提交图又快又不起进程。
//! - **结果只看结构化信号**:退出码、`repo.state()`、引用前后的变化、索引里的冲突条目。
//!   **不认 git 的提示文本** —— 用户的 git 可能是中文界面(`已经是最新的。`),拿
//!   `Already up to date` 去匹配在那台机器上永远不中。失败时的 stderr 只原样转交给
//!   用户看,不参与判断。
//!
//! # 分支名为什么先校验
//!
//! 分支名要拼进 git 命令行。以 `-` 开头的名字会被 git 当成选项(`-D`、`--force`),
//! 所以凡是用户输入的新名字一律先过 [`validate_branch_name`];已有分支的名字则先在
//! git2 里查到实体再交给 CLI —— git 本身不许建 `-` 开头的分支,查得到就安全。

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use git2::{BranchType, Oid, Repository};

use super::repo_state::conflicted_file_count;
use super::{GitOutput, invalidate_repo_cache, run_git_output, run_git_output_env};

/// 只动引用的操作(`branch` / `branch -m` / `branch -d`):毫秒级,30s 足够兜底。
const REF_TIMEOUT: Duration = Duration::from_secs(30);
/// 要改写工作区的操作(检出 / 合并 / 变基):大仓库一次 checkout 能到几十秒,
/// 还可能跑 post-checkout hook,给到 120s。
pub(super) const WORKTREE_TIMEOUT: Duration = Duration::from_secs(120);
/// 网络操作(删远程分支)的超时与提示,与 pull/push 同一口径。
pub(super) const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);
pub(super) const NETWORK_HINT: &str = ",可能在等待凭证或网络故障。请确认已配置凭证管理器或 SSH key";
/// 会拉起编辑器的命令(合并提交、`rebase` / `pull --rebase` 续跑)一律带上:
/// 沿用 git 生成的提交信息,不等一个永远不会有人操作的编辑器。
pub(super) const NO_EDITOR: &[(&str, &str)] = &[("GIT_EDITOR", "true")];

/// git 失败时给用户看的原文:优先 stderr,空的话退 stdout(`merge` 的冲突说明
/// 打在 stdout 上),两者都空才给一句兜底。
pub(super) fn failure_text(out: &GitOutput) -> String {
    let stderr = out.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    let stdout = out.stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_string();
    }
    "git 执行失败(没有输出)".to_string()
}

/// 跑一条 git 命令,非零退出即把原文装进错误。
pub(super) fn run_checked(
    repo_path: &Path,
    args: &[&str],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<GitOutput> {
    let out = run_git_output(repo_path, args, op_label, timeout, timeout_hint)?;
    if !out.success {
        bail!("{}", failure_text(&out));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 分支名校验
// ---------------------------------------------------------------------------

/// 分支名哪里不合法。UI 按它给出对应的就地提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchNameIssue {
    /// 空串。
    Empty,
    /// 含空白(空格 / 制表 / 换行)。
    Whitespace,
    /// 含 git 不许的字符:控制字符与 `~ ^ : ? * [ \`。
    IllegalChar,
    /// 以 `-` 开头(会被当成命令行选项)。
    LeadingDash,
    /// `HEAD` / `@` 这类保留名。
    Reserved,
    /// 其余格式问题:`..`、`//`、`@{`、以 `/` 开头或结尾、以 `.` 结尾、
    /// 某一段以 `.` 开头或以 `.lock` 结尾。
    BadFormat,
}

/// 按 `git check-ref-format --branch` 的规则校验分支名(调用方先 trim)。
///
/// 规则逐条手写是为了给出**具体**的提示;最后再交 libgit2 的引用名校验兜一遍底,
/// 手写规则漏掉的情形也挡得住。
pub fn validate_branch_name(name: &str) -> std::result::Result<(), BranchNameIssue> {
    if name.is_empty() {
        return Err(BranchNameIssue::Empty);
    }
    if name.chars().any(char::is_whitespace) {
        return Err(BranchNameIssue::Whitespace);
    }
    if name
        .chars()
        .any(|c| c.is_control() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return Err(BranchNameIssue::IllegalChar);
    }
    if name.starts_with('-') {
        return Err(BranchNameIssue::LeadingDash);
    }
    if name == "HEAD" || name == "@" {
        return Err(BranchNameIssue::Reserved);
    }
    let bad_shape = name.contains("..")
        || name.contains("//")
        || name.contains("@{")
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name
            .split('/')
            .any(|part| part.starts_with('.') || part.ends_with(".lock"));
    if bad_shape || !git2::Reference::is_valid_name(&format!("refs/heads/{name}")) {
        return Err(BranchNameIssue::BadFormat);
    }
    Ok(())
}

fn ensure_valid_name(name: &str) -> Result<()> {
    validate_branch_name(name).map_err(|issue| anyhow!("分支名不合法({issue:?}):{name}"))
}

/// 本地分支必须存在(顺带挡住 `-` 开头被当成选项)。
fn ensure_local_branch(repo: &Repository, name: &str) -> Result<()> {
    repo.find_branch(name, BranchType::Local)
        .map(|_| ())
        .map_err(|_| anyhow!("未找到本地分支:{name}"))
}

/// HEAD 所在的本地分支名;detached / 空仓库为 `None`。
pub(super) fn head_branch_name(repo: &Repository) -> Option<String> {
    let head = repo.head().ok()?;
    if !head.is_branch() {
        return None;
    }
    head.shorthand().map(str::to_string)
}

/// HEAD 的显示名:分支名;detached 时是带括号的短 hash `"(1a2b3c4)"`(与
/// [`super::discover_git_repos`] 的 `current_branch` 同一口径);空仓库为 `None`。
///
/// Git 面板每次重载分支时拿它刷新仓库栏 —— 仓库列表那份要等下一次重新发现仓库。
pub fn current_head_label(repo_path: &Path) -> Result<Option<String>> {
    let repo = Repository::open(repo_path)?;
    let Ok(head) = repo.head() else {
        return Ok(None);
    };
    if head.is_branch() {
        return Ok(head.shorthand().map(str::to_string));
    }
    Ok(head.target().map(|oid| {
        let s = oid.to_string();
        format!("({})", &s[..7.min(s.len())])
    }))
}

fn head_oid(repo: &Repository) -> Option<Oid> {
    repo.head().ok().and_then(|h| h.target())
}

/// 分支名(本地优先,其次远程跟踪)指向的提交。
fn branch_tip(repo: &Repository, name: &str) -> Option<Oid> {
    repo.find_branch(name, BranchType::Local)
        .or_else(|_| repo.find_branch(name, BranchType::Remote))
        .ok()
        .and_then(|b| b.get().target())
}

// ---------------------------------------------------------------------------
// 远程
// ---------------------------------------------------------------------------

/// 仓库配置的全部远程名(`origin` / `upstream`…)。
pub fn list_remotes(repo_path: &Path) -> Result<Vec<String>> {
    let repo = Repository::open(repo_path)?;
    Ok(remote_names(&repo))
}

pub(super) fn remote_names(repo: &Repository) -> Vec<String> {
    repo.remotes()
        .map(|names| names.iter().flatten().map(str::to_string).collect())
        .unwrap_or_default()
}

/// 把远程分支短名 `origin/feature/x` 拆成 `(远程名, 分支名)`。
///
/// 远程名自己也可以带 `/`,所以不能按第一个 `/` 硬切:在已知远程里找**最长**的
/// 那个前缀。一个都对不上(远程已被删、名字不规范)时返回 `None`。
pub fn split_remote_branch(remotes: &[String], name: &str) -> Option<(String, String)> {
    remotes
        .iter()
        .filter_map(|remote| {
            let rest = name.strip_prefix(remote.as_str())?.strip_prefix('/')?;
            (!rest.is_empty()).then(|| (remote.clone(), rest.to_string()))
        })
        .max_by_key(|(remote, _)| remote.len())
}

// ---------------------------------------------------------------------------
// 检出 / 新建 / 重命名 / 删除
// ---------------------------------------------------------------------------

/// 检出本地分支。工作区改动与目标分支冲突时 git 拒绝,错误里是它的 stderr 原文。
///
/// **阻塞最多 120s**,调用方放后台执行器。
pub fn checkout_branch(repo_path: &Path, name: &str) -> Result<()> {
    let repo = Repository::open(repo_path)?;
    ensure_local_branch(&repo, name)?;
    // 末尾的 `--` 让 git 只把 name 当分支解析,不会因为恰好有同名文件而去「检出文件」
    run_checked(
        repo_path,
        &["checkout", name, "--"],
        "checkout",
        WORKTREE_TIMEOUT,
        "",
    )?;
    invalidate_repo_cache();
    Ok(())
}

/// 检出远程分支的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteCheckout {
    /// 本地没有同名分支:新建并跟踪该远程分支。
    Created { local: String },
    /// 本地已有同名分支:直接检出它(**不**与远程同步,UI 要提示这一点)。
    Existing { local: String },
}

/// 检出远程分支 `origin/foo`:本地无 `foo` 则 `checkout -b foo --track origin/foo`,
/// 有则检出本地那个。
pub fn checkout_remote_branch(repo_path: &Path, remote_branch: &str) -> Result<RemoteCheckout> {
    let repo = Repository::open(repo_path)?;
    repo.find_branch(remote_branch, BranchType::Remote)
        .map_err(|_| anyhow!("未找到远程分支:{remote_branch}"))?;
    let (_, local) = split_remote_branch(&remote_names(&repo), remote_branch)
        .ok_or_else(|| anyhow!("认不出远程分支所属的远程:{remote_branch}"))?;
    if repo.find_branch(&local, BranchType::Local).is_ok() {
        drop(repo);
        checkout_branch(repo_path, &local)?;
        return Ok(RemoteCheckout::Existing { local });
    }
    ensure_valid_name(&local)?;
    run_checked(
        repo_path,
        &["checkout", "-b", &local, "--track", remote_branch],
        "checkout -b",
        WORKTREE_TIMEOUT,
        "",
    )?;
    invalidate_repo_cache();
    Ok(RemoteCheckout::Created { local })
}

/// 新建分支。`start` 是起点(本地或远程分支名;`None` = HEAD),`checkout` 为真时
/// 建完顺手检出。起点是远程分支时 git 按默认的 `branch.autoSetupMerge` 自动设上游。
pub fn create_branch(
    repo_path: &Path,
    name: &str,
    start: Option<&str>,
    checkout: bool,
) -> Result<()> {
    ensure_valid_name(name)?;
    let repo = Repository::open(repo_path)?;
    if repo.find_branch(name, BranchType::Local).is_ok() {
        bail!("已存在同名分支:{name}");
    }
    if let Some(start) = start
        && branch_tip(&repo, start).is_none()
    {
        bail!("未找到起点分支:{start}");
    }
    drop(repo);
    let mut args: Vec<&str> = if checkout {
        vec!["checkout", "-b", name]
    } else {
        vec!["branch", name]
    };
    args.extend(start);
    let timeout = if checkout {
        WORKTREE_TIMEOUT
    } else {
        REF_TIMEOUT
    };
    run_checked(repo_path, &args, "branch", timeout, "")?;
    invalidate_repo_cache();
    Ok(())
}

/// 重命名本地分支(`git branch -m`)。当前分支也可以改,上游配置随之搬过去。
pub fn rename_branch(repo_path: &Path, old: &str, new: &str) -> Result<()> {
    ensure_valid_name(new)?;
    let repo = Repository::open(repo_path)?;
    ensure_local_branch(&repo, old)?;
    if repo.find_branch(new, BranchType::Local).is_ok() {
        bail!("已存在同名分支:{new}");
    }
    drop(repo);
    run_checked(
        repo_path,
        &["branch", "-m", old, new],
        "branch -m",
        REF_TIMEOUT,
        "",
    )?;
    invalidate_repo_cache();
    Ok(())
}

/// 删除本地分支的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    /// `-d` 被拒:分支上有既没合进 HEAD、也没合进上游的提交。UI 二次确认后带
    /// `force = true` 重来(`-D`)。
    NotMerged,
}

/// 删除本地分支。`force = false` 走 `-d`,未合并时返回 [`DeleteOutcome::NotMerged`]
/// 而不是错误;别的失败(被别的 worktree 检出等)照常是 `Err`(git 原文)。
pub fn delete_branch(repo_path: &Path, name: &str, force: bool) -> Result<DeleteOutcome> {
    let repo = Repository::open(repo_path)?;
    ensure_local_branch(&repo, name)?;
    if head_branch_name(&repo).as_deref() == Some(name) {
        bail!("不能删除当前分支:{name}");
    }
    drop(repo);
    let flag = if force { "-D" } else { "-d" };
    let out = run_git_output(
        repo_path,
        &["branch", flag, name],
        "branch -d",
        REF_TIMEOUT,
        "",
    )?;
    if out.success {
        invalidate_repo_cache();
        return Ok(DeleteOutcome::Deleted);
    }
    // 拒绝的原因不看文案,看事实:分支还在、而且确实没被合进 HEAD / 上游
    let repo = Repository::open(repo_path)?;
    if !force && repo.find_branch(name, BranchType::Local).is_ok() && !is_merged(&repo, name) {
        return Ok(DeleteOutcome::NotMerged);
    }
    bail!("{}", failure_text(&out));
}

/// 分支是否已合进 HEAD 或它自己的上游(`git branch -d` 的放行口径)。
fn is_merged(repo: &Repository, name: &str) -> bool {
    let Ok(branch) = repo.find_branch(name, BranchType::Local) else {
        return true;
    };
    let Some(tip) = branch.get().target() else {
        return true;
    };
    let upstream = branch.upstream().ok().and_then(|u| u.get().target());
    [head_oid(repo), upstream]
        .into_iter()
        .flatten()
        .any(|base| base == tip || repo.graph_descendant_of(base, tip).unwrap_or(false))
}

/// 删除远程分支(`git push <remote> --delete refs/heads/<branch>`)。成功后 git 会顺手
/// 删掉本地的远程跟踪引用。**网络操作,阻塞最多 120s**。
pub fn delete_remote_branch(repo_path: &Path, remote_branch: &str) -> Result<()> {
    let repo = Repository::open(repo_path)?;
    repo.find_branch(remote_branch, BranchType::Remote)
        .map_err(|_| anyhow!("未找到远程分支:{remote_branch}"))?;
    let (remote, branch) = split_remote_branch(&remote_names(&repo), remote_branch)
        .ok_or_else(|| anyhow!("认不出远程分支所属的远程:{remote_branch}"))?;
    drop(repo);
    // 写全 `refs/heads/`:远端恰好有同名 tag 时 `--delete foo` 有歧义
    let target = format!("refs/heads/{branch}");
    run_checked(
        repo_path,
        &["push", &remote, "--delete", &target],
        "push --delete",
        NETWORK_TIMEOUT,
        NETWORK_HINT,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 合并 / 变基
// ---------------------------------------------------------------------------

/// 合并的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcome {
    /// 目标已被当前分支包含,什么都没变。
    UpToDate,
    /// 快进:HEAD 直接挪到了目标上。
    FastForward,
    /// 生成了一个合并提交。
    Merged,
    /// 有冲突,仓库停在 MERGING 状态等用户处理(`files` = 冲突文件数)。
    Conflicts { files: usize },
}

/// 把 `name`(本地或远程分支)合并到当前分支(`git merge --no-edit`)。
///
/// 结果按 HEAD 前后的变化与 `repo.state()` 判,不看输出文案。工作区改动会被覆盖
/// 这类「还没开始就拒绝」的失败是 `Err`(git 原文)。**阻塞最多 120s**。
pub fn merge_branch(repo_path: &Path, name: &str) -> Result<MergeOutcome> {
    let repo = Repository::open(repo_path)?;
    let before = head_oid(&repo).ok_or_else(|| anyhow!("当前没有提交,无法合并"))?;
    let target = branch_tip(&repo, name).ok_or_else(|| anyhow!("未找到分支:{name}"))?;
    drop(repo);
    let out = run_git_output_env(
        repo_path,
        &["merge", "--no-edit", name],
        NO_EDITOR,
        "merge",
        WORKTREE_TIMEOUT,
        "",
    )?;
    let repo = Repository::open(repo_path)?;
    if !out.success {
        if repo.state() == git2::RepositoryState::Merge {
            return Ok(MergeOutcome::Conflicts {
                files: conflicted_file_count(&repo),
            });
        }
        bail!("{}", failure_text(&out));
    }
    let after = head_oid(&repo);
    Ok(if after == Some(before) {
        MergeOutcome::UpToDate
    } else if after == Some(target) {
        MergeOutcome::FastForward
    } else {
        MergeOutcome::Merged
    })
}

/// 变基的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// 当前分支已经在目标之上,什么都没变。
    UpToDate,
    Rebased,
    /// 停在某个提交上等用户处理冲突(`files` = 冲突文件数)。
    Conflicts {
        files: usize,
    },
}

/// 把当前分支变基到 `onto`(`git rebase <onto>`)。**改写当前分支上独有的提交**,
/// UI 侧先确认。**阻塞最多 120s**。
pub fn rebase_onto(repo_path: &Path, onto: &str) -> Result<RebaseOutcome> {
    let repo = Repository::open(repo_path)?;
    let before = head_oid(&repo).ok_or_else(|| anyhow!("当前没有提交,无法变基"))?;
    branch_tip(&repo, onto).ok_or_else(|| anyhow!("未找到分支:{onto}"))?;
    drop(repo);
    let out = run_git_output_env(
        repo_path,
        &["rebase", onto],
        NO_EDITOR,
        "rebase",
        WORKTREE_TIMEOUT,
        "",
    )?;
    let repo = Repository::open(repo_path)?;
    if !out.success {
        if super::repo_state::op_kind_of(repo.state()).is_some() {
            return Ok(RebaseOutcome::Conflicts {
                files: conflicted_file_count(&repo),
            });
        }
        bail!("{}", failure_text(&out));
    }
    Ok(if head_oid(&repo) == Some(before) {
        RebaseOutcome::UpToDate
    } else {
        RebaseOutcome::Rebased
    })
}

// ---------------------------------------------------------------------------
// 最近分支 / 上游领先落后
// ---------------------------------------------------------------------------

/// 解析一条 HEAD reflog 的检出记录 `checkout: moving from X to Y` → `(X, Y)`。
///
/// reflog 的消息是 git 内部写死的英文(不随界面语言本地化),`git switch` 写的也是
/// 这一句。分支名不含空格,按 ` to ` 切开不会切错。
pub fn parse_checkout_reflog(message: &str) -> Option<(&str, &str)> {
    let rest = message.trim().strip_prefix("checkout: moving from ")?;
    let (from, to) = rest.split_once(" to ")?;
    (!from.is_empty() && !to.is_empty()).then_some((from, to))
}

/// 最近检出过的本地分支,新的在前,最多 `limit` 个。
///
/// 来源是 HEAD 的 reflog:每条检出记录的「目标」与「来源」都算一次访问(刚切走的
/// 那个分支正是最常要切回去的)。**排除当前分支**(它在列表里另有标记)、已被删除
/// 的分支与 detached 检出的提交号。
pub fn recent_branches(repo_path: &Path, limit: usize) -> Result<Vec<String>> {
    let repo = Repository::open(repo_path)?;
    let current = head_branch_name(&repo);
    let Ok(reflog) = repo.reflog("HEAD") else {
        return Ok(Vec::new());
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    // libgit2 的 reflog 下标 0 是最新的一条
    for entry in reflog.iter() {
        let Some((from, to)) = entry.message().and_then(parse_checkout_reflog) else {
            continue;
        };
        for name in [to, from] {
            if current.as_deref() == Some(name) || !seen.insert(name.to_string()) {
                continue;
            }
            if repo.find_branch(name, BranchType::Local).is_ok() {
                out.push(name.to_string());
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}

/// 本地分支相对上游的领先 / 落后。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchTracking {
    pub branch: String,
    /// 上游短名(`origin/main`)。
    pub upstream: String,
    /// 本地有、上游没有的提交数(待推送)。
    pub ahead: usize,
    /// 上游有、本地没有的提交数(待拉取)。
    pub behind: usize,
}

/// 逐个本地分支算领先 / 落后的上限。分支成百上千的仓库只算前这么多个,
/// 分支弹窗里其余的不显示计数 —— 每个都要走一次提交图。
const TRACKING_LIMIT: usize = 300;

fn tracking_of(repo: &Repository, branch: &git2::Branch<'_>) -> Option<BranchTracking> {
    let name = branch.name().ok().flatten()?.to_string();
    let local = branch.get().target()?;
    let upstream = branch.upstream().ok()?;
    let upstream_name = upstream.name().ok().flatten()?.to_string();
    let remote = upstream.get().target()?;
    let (ahead, behind) = repo.graph_ahead_behind(local, remote).ok()?;
    Some(BranchTracking {
        branch: name,
        upstream: upstream_name,
        ahead,
        behind,
    })
}

/// 全部配了上游(且远程跟踪引用在场)的本地分支的领先 / 落后。分支弹窗逐行用。
pub fn local_branch_tracking(repo_path: &Path) -> Result<Vec<BranchTracking>> {
    let repo = Repository::open(repo_path)?;
    let mut out = Vec::new();
    for item in repo.branches(Some(BranchType::Local))?.take(TRACKING_LIMIT) {
        let (branch, _) = item?;
        if let Some(tracking) = tracking_of(&repo, &branch) {
            out.push(tracking);
        }
    }
    Ok(out)
}

/// HEAD 所在分支相对上游的领先 / 落后(`git2::graph_ahead_behind`)。
/// detached HEAD、没配上游、上游引用不在时是 `None`。
pub fn ahead_behind(repo_path: &Path) -> Result<Option<BranchTracking>> {
    let repo = Repository::open(repo_path)?;
    let Some(name) = head_branch_name(&repo) else {
        return Ok(None);
    };
    let branch = repo.find_branch(&name, BranchType::Local)?;
    Ok(tracking_of(&repo, &branch))
}

// ---------------------------------------------------------------------------
// 单测共用的 git CLI 小件(sync / repo_state 的单测也用)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(super) mod test_cli {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::super::test_support::init_repo_with_file;

    /// 跑一条 git 命令,失败直接 panic(带上 stderr)。身份与签名用 `-c` 钉死,
    /// 不依赖跑测试那台机器的全局配置。
    ///
    /// ⚠️ **不**在这里钉 `core.autocrlf`:被测函数走的 git 读的是机器的全局配置,
    /// 两边换行口径不一致的话,一边 LF 检出、一边 CRLF 比对,干净的工作区会被认成
    /// 「有改动」挡住检出。读文件内容做断言时自己归一换行。
    pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=mini-term",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true")
            .output()
            .expect("git 起不来");
        assert!(
            out.status.success(),
            "git {args:?} 失败: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// 写文件并提交一次。
    pub(crate) fn commit_file(dir: &Path, rel: &str, content: &str, message: &str) {
        std::fs::write(dir.join(rel), content).unwrap();
        git(dir, &["add", rel]);
        git(dir, &["commit", "-q", "-m", message]);
    }

    /// 当前分支名。
    pub(crate) fn current(dir: &Path) -> String {
        git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
    }

    /// 带 `origin` 的仓库:工作区仓库 + 同级的裸仓库当远程,当前分支已推上去并设好上游。
    /// 返回 `(工作区, 裸仓库, 分支名)`。不联网。
    pub(crate) fn repo_with_origin(tag: &str) -> (PathBuf, PathBuf, String) {
        let work = init_repo_with_file(tag, "a.txt", "a\n");
        let bare = work.with_file_name(format!(
            "{}-origin.git",
            work.file_name().unwrap().to_string_lossy()
        ));
        let parent = work.parent().unwrap().to_path_buf();
        git(&parent, &["init", "-q", "--bare", &bare.to_string_lossy()]);
        git(&work, &["remote", "add", "origin", &bare.to_string_lossy()]);
        let branch = current(&work);
        git(&work, &["push", "-q", "-u", "origin", &branch]);
        (work, bare, branch)
    }

    /// 从裸仓库再克隆一份「别人的工作区」,用来往远程上推新提交。
    pub(crate) fn clone_of(bare: &Path, branch: &str) -> PathBuf {
        let dest = bare.with_file_name(format!(
            "{}-other",
            bare.file_name().unwrap().to_string_lossy()
        ));
        let parent = bare.parent().unwrap().to_path_buf();
        git(
            &parent,
            &[
                "clone",
                "-q",
                "-b",
                branch,
                &bare.to_string_lossy(),
                &dest.to_string_lossy(),
            ],
        );
        dest
    }

    pub(crate) fn cleanup(dirs: &[&Path]) {
        for dir in dirs {
            std::fs::remove_dir_all(dir).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::init_repo_with_file;
    use super::test_cli::{cleanup, clone_of, commit_file, current, git, repo_with_origin};
    use super::*;

    /// 校验:合法名放行,各类非法名给出各自的原因。
    #[test]
    fn 分支名校验分门别类() {
        for ok in [
            "main",
            "feature/x",
            "fix-1.2",
            "a/b/c",
            "中文分支",
            "v1.0_rc",
        ] {
            assert_eq!(validate_branch_name(ok), Ok(()), "{ok}");
        }
        let cases = [
            ("", BranchNameIssue::Empty),
            ("a b", BranchNameIssue::Whitespace),
            ("a\tb", BranchNameIssue::Whitespace),
            ("a~1", BranchNameIssue::IllegalChar),
            ("a^", BranchNameIssue::IllegalChar),
            ("a:b", BranchNameIssue::IllegalChar),
            ("a?", BranchNameIssue::IllegalChar),
            ("a*", BranchNameIssue::IllegalChar),
            ("a[b", BranchNameIssue::IllegalChar),
            ("a\\b", BranchNameIssue::IllegalChar),
            ("-D", BranchNameIssue::LeadingDash),
            ("--force", BranchNameIssue::LeadingDash),
            ("HEAD", BranchNameIssue::Reserved),
            ("@", BranchNameIssue::Reserved),
            ("a..b", BranchNameIssue::BadFormat),
            ("a//b", BranchNameIssue::BadFormat),
            ("a@{1}", BranchNameIssue::BadFormat),
            ("/a", BranchNameIssue::BadFormat),
            ("a/", BranchNameIssue::BadFormat),
            ("a.", BranchNameIssue::BadFormat),
            (".a", BranchNameIssue::BadFormat),
            ("x/.a", BranchNameIssue::BadFormat),
            ("a.lock", BranchNameIssue::BadFormat),
            ("x/a.lock/y", BranchNameIssue::BadFormat),
        ];
        for (name, issue) in cases {
            assert_eq!(validate_branch_name(name), Err(issue), "{name:?}");
        }
    }

    /// 远程分支按**已知远程的最长前缀**拆,远程名带 `/` 也拆得对。
    #[test]
    fn 远程分支按已知远程拆分() {
        let remotes = vec!["origin".to_string(), "team/fork".to_string()];
        assert_eq!(
            split_remote_branch(&remotes, "origin/feature/x"),
            Some(("origin".into(), "feature/x".into()))
        );
        assert_eq!(
            split_remote_branch(&remotes, "team/fork/main"),
            Some(("team/fork".into(), "main".into()))
        );
        // 前缀只是字符串像不算:originx 不是 origin
        assert_eq!(split_remote_branch(&remotes, "originx/main"), None);
        assert_eq!(split_remote_branch(&remotes, "origin/"), None);
        assert_eq!(split_remote_branch(&[], "origin/main"), None);
    }

    #[test]
    fn 解析检出_reflog() {
        assert_eq!(
            parse_checkout_reflog("checkout: moving from main to feature/x"),
            Some(("main", "feature/x"))
        );
        assert_eq!(
            parse_checkout_reflog("checkout: moving from 1a2b3c4 to main\n"),
            Some(("1a2b3c4", "main"))
        );
        assert_eq!(parse_checkout_reflog("commit: init"), None);
        assert_eq!(parse_checkout_reflog("checkout: moving from  to x"), None);
    }

    /// 新建(带 / 不带检出)、检出、重命名一条龙;非法名与重名在动 git 之前就挡下。
    #[test]
    fn 新建检出与重命名() {
        let root = init_repo_with_file("br-create", "a.txt", "a\n");
        let main = current(&root);

        create_branch(&root, "feature/x", None, false).unwrap();
        assert_eq!(current(&root), main, "不检出时 HEAD 不动");
        create_branch(&root, "dev", Some(&main), true).unwrap();
        assert_eq!(current(&root), "dev");
        assert_eq!(current_head_label(&root).unwrap().as_deref(), Some("dev"));

        assert!(
            create_branch(&root, "dev", None, false).is_err(),
            "重名要拒"
        );
        assert!(
            create_branch(&root, "-D", None, false).is_err(),
            "选项样的名字要拒"
        );
        assert!(
            create_branch(&root, "x", Some("nope"), false).is_err(),
            "起点不存在要拒"
        );

        checkout_branch(&root, "feature/x").unwrap();
        assert_eq!(current(&root), "feature/x");
        assert!(checkout_branch(&root, "nope").is_err());

        rename_branch(&root, "feature/x", "feature/y").unwrap();
        assert_eq!(current(&root), "feature/y", "改当前分支的名字,HEAD 跟着走");
        assert!(
            rename_branch(&root, "feature/y", "dev").is_err(),
            "改成已有的名字要拒"
        );
        assert!(rename_branch(&root, "feature/y", "a b").is_err());
        cleanup(&[&root]);
    }

    /// 工作区改动挡住检出时,错误带着 git 的原文,HEAD 不动。
    #[test]
    fn 脏工作区检出失败给原文() {
        let root = init_repo_with_file("br-dirty", "a.txt", "a\n");
        let main = current(&root);
        git(&root, &["checkout", "-q", "-b", "other"]);
        commit_file(&root, "a.txt", "other\n", "other");
        git(&root, &["checkout", "-q", &main]);
        std::fs::write(root.join("a.txt"), "local edit\n").unwrap();

        let err = checkout_branch(&root, "other").unwrap_err().to_string();
        assert!(err.contains("a.txt"), "错误里应有被挡住的文件名: {err}");
        assert_eq!(current(&root), main);
        cleanup(&[&root]);
    }

    /// `-d` 遇到未合并的分支返回 NotMerged(不是错误),`-D` 才真删;
    /// 已合并的直接删;当前分支不许删。
    #[test]
    fn 删除分支区分未合并() {
        let root = init_repo_with_file("br-delete", "a.txt", "a\n");
        let main = current(&root);
        git(&root, &["branch", "merged"]);
        git(&root, &["checkout", "-q", "-b", "wip"]);
        commit_file(&root, "b.txt", "b\n", "wip");
        git(&root, &["checkout", "-q", &main]);

        assert_eq!(
            delete_branch(&root, "merged", false).unwrap(),
            DeleteOutcome::Deleted
        );
        assert_eq!(
            delete_branch(&root, "wip", false).unwrap(),
            DeleteOutcome::NotMerged
        );
        assert!(
            Repository::open(&root)
                .unwrap()
                .find_branch("wip", BranchType::Local)
                .is_ok(),
            "未合并时不能被删"
        );
        assert_eq!(
            delete_branch(&root, "wip", true).unwrap(),
            DeleteOutcome::Deleted
        );
        assert!(delete_branch(&root, &main, true).is_err(), "当前分支不许删");
        assert!(delete_branch(&root, "nope", false).is_err());
        cleanup(&[&root]);
    }

    /// 合并四种结果:已是最新 / 快进 / 合并提交 / 冲突(冲突时仓库停在 MERGING)。
    #[test]
    fn 合并结果分四种() {
        let root = init_repo_with_file("br-merge", "a.txt", "a\n");
        let main = current(&root);

        // 快进
        git(&root, &["checkout", "-q", "-b", "ff"]);
        commit_file(&root, "b.txt", "b\n", "ff");
        git(&root, &["checkout", "-q", &main]);
        assert_eq!(
            merge_branch(&root, "ff").unwrap(),
            MergeOutcome::FastForward
        );
        // 已是最新
        assert_eq!(merge_branch(&root, "ff").unwrap(), MergeOutcome::UpToDate);

        // 合并提交:两边各有不相干的提交
        git(&root, &["checkout", "-q", "-b", "side"]);
        commit_file(&root, "c.txt", "c\n", "side");
        git(&root, &["checkout", "-q", &main]);
        commit_file(&root, "d.txt", "d\n", "main");
        assert_eq!(merge_branch(&root, "side").unwrap(), MergeOutcome::Merged);
        let repo = Repository::open(&root).unwrap();
        assert_eq!(
            repo.head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .parent_count(),
            2
        );
        drop(repo);

        // 冲突:两边改同一行
        git(&root, &["checkout", "-q", "-b", "clash"]);
        commit_file(&root, "a.txt", "theirs\n", "clash");
        git(&root, &["checkout", "-q", &main]);
        commit_file(&root, "a.txt", "ours\n", "ours");
        assert_eq!(
            merge_branch(&root, "clash").unwrap(),
            MergeOutcome::Conflicts { files: 1 }
        );
        assert_eq!(
            Repository::open(&root).unwrap().state(),
            git2::RepositoryState::Merge
        );
        cleanup(&[&root]);
    }

    /// 变基:已是最新 / 改写成功 / 冲突停住。
    #[test]
    fn 变基结果() {
        let root = init_repo_with_file("br-rebase", "a.txt", "a\n");
        let main = current(&root);
        git(&root, &["checkout", "-q", "-b", "topic"]);
        commit_file(&root, "t.txt", "t\n", "topic");
        assert_eq!(rebase_onto(&root, &main).unwrap(), RebaseOutcome::UpToDate);

        git(&root, &["checkout", "-q", &main]);
        commit_file(&root, "m.txt", "m\n", "main moves");
        git(&root, &["checkout", "-q", "topic"]);
        assert_eq!(rebase_onto(&root, &main).unwrap(), RebaseOutcome::Rebased);
        let repo = Repository::open(&root).unwrap();
        let main_tip = repo
            .find_branch(&main, BranchType::Local)
            .unwrap()
            .get()
            .target()
            .unwrap();
        let head = repo.head().unwrap().target().unwrap();
        assert!(
            repo.graph_descendant_of(head, main_tip).unwrap(),
            "变基后在 main 之上"
        );
        drop(repo);

        // 冲突
        git(&root, &["checkout", "-q", &main]);
        commit_file(&root, "a.txt", "main side\n", "main edits a");
        git(&root, &["checkout", "-q", "topic"]);
        commit_file(&root, "a.txt", "topic side\n", "topic edits a");
        assert_eq!(
            rebase_onto(&root, &main).unwrap(),
            RebaseOutcome::Conflicts { files: 1 }
        );
        assert!(
            super::super::repo_state::op_kind_of(Repository::open(&root).unwrap().state())
                .is_some()
        );
        cleanup(&[&root]);
    }

    /// 最近分支:新的在前、去重、排除当前分支与已删分支、截到上限。
    #[test]
    fn 最近分支来自_reflog() {
        let root = init_repo_with_file("br-recent", "a.txt", "a\n");
        let main = current(&root);
        for name in ["a", "b", "c", "gone"] {
            git(&root, &["branch", name]);
        }
        for name in ["a", "b", "gone", "c", "a"] {
            git(&root, &["checkout", "-q", name]);
        }
        git(&root, &["checkout", "-q", &main]);
        git(&root, &["branch", "-D", "gone"]);

        // 时间线(新→旧):a→main, c→a, gone→c, b→gone, a→b, main→a
        assert_eq!(recent_branches(&root, 5).unwrap(), vec!["a", "c", "b"]);
        assert_eq!(recent_branches(&root, 2).unwrap(), vec!["a", "c"]);
        cleanup(&[&root]);
    }

    /// 领先落后:本地多 1 个、远程多 2 个;没上游的分支不出现;detached 为 None。
    #[test]
    fn 领先落后计数() {
        let (work, bare, branch) = repo_with_origin("br-ab");
        let other = clone_of(&bare, &branch);
        commit_file(&other, "o1.txt", "1\n", "o1");
        commit_file(&other, "o2.txt", "2\n", "o2");
        git(&other, &["push", "-q", "origin", &branch]);

        commit_file(&work, "w.txt", "w\n", "w");
        git(&work, &["fetch", "-q", "origin"]);
        git(&work, &["branch", "lonely"]);

        let head = ahead_behind(&work).unwrap().expect("有上游");
        assert_eq!((head.ahead, head.behind), (1, 2));
        assert_eq!(head.upstream, format!("origin/{branch}"));

        let all = local_branch_tracking(&work).unwrap();
        assert_eq!(all.len(), 1, "没上游的 lonely 不在列: {all:?}");
        assert_eq!(all[0].branch, branch);

        let oid = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["checkout", "-q", &oid]);
        assert_eq!(ahead_behind(&work).unwrap(), None, "detached 不算");
        assert_eq!(
            current_head_label(&work).unwrap(),
            Some(format!("({})", &oid[..7])),
            "detached 的显示名是带括号的短 hash"
        );
        cleanup(&[&work, &bare, &other]);
    }

    /// 远程分支检出:本地没有就建跟踪分支,有就检出本地那个。
    #[test]
    fn 检出远程分支() {
        let (work, bare, branch) = repo_with_origin("br-remote-co");
        let other = clone_of(&bare, &branch);
        git(&other, &["checkout", "-q", "-b", "feature/r"]);
        commit_file(&other, "r.txt", "r\n", "r");
        git(&other, &["push", "-q", "origin", "feature/r"]);
        git(&work, &["fetch", "-q", "origin"]);

        assert_eq!(
            checkout_remote_branch(&work, "origin/feature/r").unwrap(),
            RemoteCheckout::Created {
                local: "feature/r".into()
            }
        );
        assert_eq!(current(&work), "feature/r");
        let tracking = ahead_behind(&work).unwrap().unwrap();
        assert_eq!(
            tracking.upstream, "origin/feature/r",
            "新建的分支要跟踪远程"
        );

        git(&work, &["checkout", "-q", &branch]);
        assert_eq!(
            checkout_remote_branch(&work, "origin/feature/r").unwrap(),
            RemoteCheckout::Existing {
                local: "feature/r".into()
            }
        );
        assert_eq!(current(&work), "feature/r");

        // 删远程分支:远端与本地的远程跟踪引用都没了
        delete_remote_branch(&work, "origin/feature/r").unwrap();
        let repo = Repository::open(&work).unwrap();
        assert!(
            repo.find_branch("origin/feature/r", BranchType::Remote)
                .is_err()
        );
        let refs = git(&bare, &["branch", "--list", "feature/r"]);
        assert!(refs.is_empty(), "远端的分支应已删除: {refs}");
        cleanup(&[&work, &bare, &other]);
    }
}
