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
use git2::{BranchType, Direction, Oid, Repository};

use super::branch::{
    NETWORK_HINT, NETWORK_TIMEOUT, NO_EDITOR, failure_text, head_branch_name, remote_names,
    run_checked,
};
use super::log_ops::is_commit_hash;
use super::repo_state::{conflicted_file_count, op_kind_of};
use super::{GIT_USAGE_ERROR, GitCommitInfo, commit_info, run_git_output, run_git_output_env};

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

/// 拉取方式(用户在面板里**手选**的那种;没手选时走 [`git_pull_by_config`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PullMode {
    /// 合并(`--no-rebase`)。
    #[default]
    Merge,
    /// 变基(`--rebase`)。
    Rebase,
    /// 仅快进(`--ff-only`):分叉了就拒绝,什么都不改。
    FfOnly,
}

impl PullMode {
    /// 用户 git 配置里的拉取方式(只用于显示,见 [`PullConfig`])。
    pub fn from_config(repo_path: &Path) -> PullMode {
        pull_config(repo_path).mode
    }

    fn args(self) -> &'static [&'static str] {
        match self {
            PullMode::Merge => &["pull", "--no-rebase", "--no-edit"],
            PullMode::Rebase => &["pull", "--rebase"],
            PullMode::FfOnly => &["pull", "--ff-only"],
        }
    }
}

/// 用户 git 配置里写的拉取方式。**只用于显示**(「跟随配置:变基」):真正拉取时
/// [`git_pull_by_config`] 不带方式参数,`interactive`、`merges` 这些细节全交给 git 自己读。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PullConfig {
    pub mode: PullMode,
    /// `rebase=merges`:变基时保留本地的合并提交(`--rebase=merges`)。
    pub rebase_merges: bool,
    /// 配置里写明了方式(`branch.<b>.rebase` / `pull.rebase` / `pull.ff` 任一)。
    /// 都没写时 git 2.33+ 遇到分叉会直接报「需要指定如何调和」,见 [`git_pull_by_config`]。
    pub explicit: bool,
}

/// 读当前分支的拉取配置:`branch.<b>.rebase` 压过 `pull.rebase`(与 git 一致);变基的
/// 取值 `true / merges / interactive`(及其缩写、布尔同义词)都算变基;不变基时
/// `pull.ff=only` → 仅快进,其余合并。读不到配置按合并算。
pub fn pull_config(repo_path: &Path) -> PullConfig {
    let Ok(repo) = Repository::open(repo_path) else {
        return PullConfig::default();
    };
    let Ok(config) = repo.config() else {
        return PullConfig::default();
    };
    let get = |key: &str| {
        config
            .get_string(key)
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
    };
    let branch_rebase = head_branch_name(&repo).and_then(|b| get(&format!("branch.{b}.rebase")));
    let pull_rebase = get("pull.rebase");
    let ff = get("pull.ff");
    let explicit = branch_rebase.is_some() || pull_rebase.is_some() || ff.is_some();
    let rebase = branch_rebase.or(pull_rebase).unwrap_or_default();
    let rebase_merges = matches!(rebase.as_str(), "merges" | "m");
    let rebasing = rebase_merges
        || matches!(
            rebase.as_str(),
            "true" | "yes" | "on" | "1" | "interactive" | "i"
        );
    let mode = if rebasing {
        PullMode::Rebase
    } else if ff.as_deref() == Some("only") {
        PullMode::FfOnly
    } else {
        PullMode::Merge
    };
    PullConfig {
        mode,
        rebase_merges,
        explicit,
    }
}

/// 「按配置拉取」的命令行:配置里写明了方式就一个方式参数都不带,完全交给 git(含
/// `branch.<b>.rebase`、`merges`、`interactive`);什么都没写时补 `--no-rebase` —— 那是
/// git 一直以来的默认行为,但 2.33+ 在分叉时不再默认、直接报「需要指定如何调和」。
fn config_pull_args(config: &PullConfig) -> &'static [&'static str] {
    if config.explicit {
        &["pull"]
    } else {
        &["pull", "--no-rebase"]
    }
}

/// 不带方式参数拉取也不能拉起编辑器:合并提交沿用 git 生成的消息,`rebase=interactive`
/// 的待办列表原样执行(`GIT_SEQUENCE_EDITOR=true` 等于直接保存退出)。
const PULL_NO_EDITOR: &[(&str, &str)] = &[("GIT_EDITOR", "true"), ("GIT_SEQUENCE_EDITOR", "true")];

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

/// 按用户手选的方式拉取当前分支。
pub fn git_pull_with(repo_path: &Path, mode: PullMode) -> Result<PullOutcome> {
    pull_run(repo_path, mode.args(), NO_EDITOR)
}

/// 用户本次会话没手选方式时的拉取:**不带方式参数**,完全按用户的 git 配置来(见
/// [`config_pull_args`])。旧实现把配置折算成 [`PullMode`] 再带参数,`pull.rebase=merges`
/// 被降成 `--rebase` 拍平了合并提交,`branch.<b>.rebase` 也压根没读。
pub fn git_pull_by_config(repo_path: &Path) -> Result<PullOutcome> {
    let config = pull_config(repo_path);
    pull_run(repo_path, config_pull_args(&config), PULL_NO_EDITOR)
}

fn pull_run(repo_path: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<PullOutcome> {
    let repo = Repository::open(repo_path)?;
    let branch = head_branch_name(&repo).ok_or_else(|| anyhow!("分离 HEAD 状态下无法拉取"))?;
    let before = repo
        .head()?
        .target()
        .ok_or_else(|| anyhow!("当前没有提交"))?;
    drop(repo);
    let out = run_git_output_env(repo_path, args, envs, "pull", NETWORK_TIMEOUT, NETWORK_HINT)?;
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
    /// 推到哪个远程:`branch.<b>.pushRemote` > `remote.pushDefault` > 上游所在的远程 >
    /// 默认远程(与 git 的取法一致;配置里写了不存在的远程就往下一档退)。
    pub remote: String,
    /// 远端的分支名。`push.default=upstream` 且推往上游所在的远程时取上游名;其余一律与
    /// 本地**同名**(git 的 `simple` / `current`;`matching` / `nothing` / 未设置也按 `simple`)。
    pub remote_branch: String,
    /// 上游的显示名(`origin/main`;`branch.<b>.remote=.` 的本地上游只有分支名)。
    /// `None` = 没配上游,这次推送顺带 `-u` 设上去(见 [`PushTarget::sets_upstream`])。
    pub upstream: Option<String>,
    /// 配了上游、但这次推的不是它(上游名与本地名不同,或推送远程另有配置)。推完上游
    /// 配置保持原样 —— 推送对话框要把这一点说出来。
    pub upstream_differs: bool,
    /// 算计划这一刻目标的远程跟踪引用(`refs/remotes/<远程>/<分支>`)指向的提交;
    /// 不在 = `None`。强推时当 lease 的期望值(见 [`PushOptions::expected_oid`])。
    pub tracking_oid: Option<String>,
}

impl PushTarget {
    /// 这次推送会不会顺带设上游(`-u`):只在**完全没有**上游时。已有上游(哪怕与这次的
    /// 目标不同)一律不动,不擅自改用户的配置。
    pub fn sets_upstream(&self) -> bool {
        self.upstream.is_none()
    }

    /// `origin/feature`。
    pub fn label(&self) -> String {
        format!("{}/{}", self.remote, self.remote_branch)
    }
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

/// 远程分支 `<remote>/<branch>` 在本地的远程跟踪引用名:按该远程的 fetch refspec 换算
/// (自定义过 refspec 的也认),换算不出来退回默认布局 `refs/remotes/<remote>/<branch>`。
fn tracking_ref_name(repo: &Repository, remote: &str, branch: &str) -> String {
    let src = format!("refs/heads/{branch}");
    if let Ok(found) = repo.find_remote(remote) {
        for spec in found.refspecs() {
            if spec.direction() != Direction::Fetch || !spec.src_matches(&src) {
                continue;
            }
            if let Some(name) = spec
                .transform(&src)
                .ok()
                .and_then(|b| b.as_str().map(str::to_string))
            {
                return name;
            }
        }
    }
    format!("refs/remotes/{remote}/{branch}")
}

/// 算推送计划。`branch = None` 表示当前分支。
///
/// 推到哪与 git 自己的口径一致(见 [`PushTarget`] 各字段)。旧实现一律拿
/// `branch.<b>.merge` 当目标名:从 `origin/main` 新建的分支(git 自动把上游设成
/// origin/main)「提交并推送」会执行 `push origin feature:main`,直接快进远端 main。
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
    plan_for(&repo, branch)
}

fn plan_for(repo: &Repository, branch: String) -> Result<PushPlan> {
    let config = repo.config()?;
    let get = |key: &str| {
        config
            .get_string(key)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let remotes = remote_names(repo);
    let known = |name: &String| remotes.contains(name);
    let upstream = match (
        get(&format!("branch.{branch}.remote")),
        get(&format!("branch.{branch}.merge")),
    ) {
        (Some(remote), Some(merge)) => {
            let name = merge
                .strip_prefix("refs/heads/")
                .unwrap_or(&merge)
                .to_string();
            Some((remote, name))
        }
        _ => None,
    };
    let remote = get(&format!("branch.{branch}.pushRemote"))
        .filter(known)
        .or_else(|| get("remote.pushDefault").filter(known))
        .or_else(|| upstream.as_ref().map(|(r, _)| r.clone()).filter(known))
        .or_else(|| default_remote(&remotes));
    let Some(remote) = remote else {
        return Ok(PushPlan::NoRemote { branch });
    };
    let push_default = get("push.default").map(|v| v.to_ascii_lowercase());
    // `tracking` 是 `upstream` 的旧名
    let to_upstream = matches!(push_default.as_deref(), Some("upstream" | "tracking"));
    let remote_branch = match &upstream {
        Some((up_remote, up_branch)) if to_upstream && *up_remote == remote => up_branch.clone(),
        _ => branch.clone(),
    };
    let upstream_differs = upstream
        .as_ref()
        .is_some_and(|(r, b)| *r != remote || *b != remote_branch);
    let tracking_oid = repo
        .refname_to_id(&tracking_ref_name(repo, &remote, &remote_branch))
        .ok()
        .map(|oid| oid.to_string());
    Ok(PushPlan::Ready(PushTarget {
        upstream: upstream.map(|(r, b)| if r == "." { b } else { format!("{r}/{b}") }),
        upstream_differs,
        tracking_oid,
        remote_branch,
        branch,
        remote,
    }))
}

/// 待推送的提交。
#[derive(Debug, Clone)]
pub struct OutgoingCommits {
    /// 新的在前。
    pub commits: Vec<GitCommitInfo>,
    /// 超出 `limit` 被截掉了。
    pub truncated: bool,
}

/// 待推送的提交:分支有、**推送目标**的远程分支没有的(与 [`push_plan`] 同一个目标,
/// 不是上游 —— 两者可以不同);目标在本地还没有远程跟踪引用时取「不在任何远程分支上」的。
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
    let target_tip = match plan_for(&repo, name)? {
        PushPlan::Ready(target) => target.tracking_oid.and_then(|o| Oid::from_str(&o).ok()),
        _ => None,
    };
    let hide: Vec<Oid> = match target_tip {
        Some(oid) => vec![oid],
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
    /// 强推(带 lease):远端已不是预览时的样子就拒绝,否则覆盖远端历史。
    pub force_with_lease: bool,
    /// 强推的期望值:推送对话框预览时目标远程跟踪引用指向的提交(见
    /// [`PushTarget::tracking_oid`])。`None` = 预览时目标远端分支还不存在 —— 这时用不着
    /// 强推,按普通推送走(远端其实已有且不是快进的话照常被拒)。
    pub expected_oid: Option<String>,
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

/// 按选项推送。完全没有上游时带 `-u` 顺带设上游;refspec 写全 `refs/heads/`,本地恰好有
/// 同名 tag 也不会推错。
///
/// 强推的 lease 写明期望值 `--force-with-lease=refs/heads/<目标>:<预览时的 oid>`,不用裸
/// `--force-with-lease`:后者取的是**推送这一刻**的远程跟踪引用 —— 对话框开着期间点过
/// Fetch 的话,别人刚推上去、本地还没整合的提交会被当成「见过的」覆盖掉。另带
/// `--force-if-includes` 多兜一层(按 git 文档,它只在不写期望值的 lease 上生效,写明期望值
/// 时由期望值本身把关);老 git(< 2.30)不认它,报用法错误时去掉重试一次。
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
    let lease = match (options.force_with_lease, options.expected_oid.as_deref()) {
        (true, Some(oid)) => {
            if !is_commit_hash(oid) || oid.len() < 40 {
                bail!("强推的期望值不是完整的提交哈希:{oid}");
            }
            Some(format!(
                "--force-with-lease=refs/heads/{}:{oid}",
                target.remote_branch
            ))
        }
        _ => None,
    };
    let push = |if_includes: bool| {
        let mut args: Vec<&str> = vec!["push", "--porcelain"];
        if target.sets_upstream() {
            args.push("-u");
        }
        if let Some(lease) = &lease {
            args.push(lease);
            if if_includes {
                args.push("--force-if-includes");
            }
        }
        args.push(&target.remote);
        args.push(&refspec);
        run_git_output(repo_path, &args, "push", NETWORK_TIMEOUT, NETWORK_HINT)
    };
    let mut out = push(true)?;
    if lease.is_some() && !out.success && out.code == Some(GIT_USAGE_ERROR) {
        out = push(false)?;
    }
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

    /// 没手选拉取方式时跟随用户配置:rebase 用户不该被 `--no-rebase` 压成合并。
    #[test]
    fn 拉取方式跟随_git_配置() {
        let root = init_repo_with_file("pull-mode", "a.txt", "a\n");
        assert_eq!(PullMode::from_config(&root), PullMode::Merge);
        let set = |key: &str, value: &str| {
            Repository::open(&root)
                .unwrap()
                .config()
                .unwrap()
                .set_str(key, value)
                .unwrap();
        };
        set("pull.ff", "only");
        assert_eq!(PullMode::from_config(&root), PullMode::FfOnly);
        set("pull.rebase", "true");
        assert_eq!(PullMode::from_config(&root), PullMode::Rebase);
        set("pull.rebase", "false");
        assert_eq!(PullMode::from_config(&root), PullMode::FfOnly);
        cleanup(&[&root]);
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
        let head = git(&work, &["rev-parse", "HEAD"]);
        assert_eq!(
            push_plan(&work, None).unwrap(),
            PushPlan::Ready(PushTarget {
                branch: branch.clone(),
                remote: "origin".into(),
                remote_branch: branch.clone(),
                upstream: Some(format!("origin/{branch}")),
                upstream_differs: false,
                tracking_oid: Some(head.clone()),
            })
        );
        git(&work, &["checkout", "-q", "-b", "fresh"]);
        let fresh = PushTarget {
            branch: "fresh".into(),
            remote: "origin".into(),
            remote_branch: "fresh".into(),
            upstream: None,
            upstream_differs: false,
            tracking_oid: None,
        };
        assert!(fresh.sets_upstream());
        assert_eq!(fresh.label(), "origin/fresh");
        assert_eq!(push_plan(&work, None).unwrap(), PushPlan::Ready(fresh));
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

        // fetch 过之后再预览、带 lease 强推(期望值 = 预览时的远程值):覆盖远端
        git(&work, &["fetch", "-q", "origin"]);
        let PushPlan::Ready(previewed) = push_plan(&work, None).unwrap() else {
            panic!("应当可推");
        };
        assert_eq!(
            previewed.tracking_oid,
            Some(git(&other, &["rev-parse", "HEAD"]))
        );
        let forced = git_push_with(
            &work,
            &PushOptions {
                branch: None,
                force_with_lease: true,
                expected_oid: previewed.tracking_oid,
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

    /// 从 `origin/main` 新建的分支(git 自动把上游设成 origin/main)「提交并推送」:
    /// 旧实现拿上游名当目标,执行 `push origin refs/heads/feature:refs/heads/main`,
    /// 直接把远端 main 快进了。与 git 的 `push.default=simple` 一致:推到同名分支,
    /// 且不擅自改用户的上游配置(不带 `-u`)。
    #[test]
    fn 从远程分支新建的分支推到同名分支且不改上游() {
        let (work, bare, branch) = repo_with_origin("sync-push-feature");
        let main_before = git(&bare, &["rev-parse", &branch]);
        git(
            &work,
            &[
                "checkout",
                "-q",
                "-b",
                "feature",
                &format!("origin/{branch}"),
            ],
        );
        let merge_key = "branch.feature.merge";
        assert_eq!(
            git(&work, &["config", merge_key]),
            format!("refs/heads/{branch}"),
            "前提:git 自动把上游设成了 origin/{branch}"
        );
        commit_file(&work, "f.txt", "f\n", "feature work");

        let PushPlan::Ready(target) = push_plan(&work, None).unwrap() else {
            panic!("应当可推");
        };
        assert_eq!(
            (target.remote.as_str(), target.remote_branch.as_str()),
            ("origin", "feature")
        );
        assert_eq!(target.upstream, Some(format!("origin/{branch}")));
        assert!(target.upstream_differs, "目标与上游不同,UI 要说明");
        assert!(!target.sets_upstream(), "已有上游,不带 -u");
        assert_eq!(target.tracking_oid, None, "远端还没有 feature");
        let out = outgoing_commits(&work, None, 100).unwrap();
        let titles: Vec<&str> = out.commits.iter().map(|c| c.message.as_str()).collect();
        assert_eq!(titles, vec!["feature work"]);

        let pushed = git_push_with(&work, &PushOptions::default()).unwrap();
        assert!(matches!(pushed, PushOutcome::Pushed(_)), "{pushed:?}");
        assert_eq!(
            git(&bare, &["rev-parse", &branch]),
            main_before,
            "远端 {branch} 不能被快进"
        );
        assert_eq!(
            git(&bare, &["rev-parse", "feature"]),
            git(&work, &["rev-parse", "HEAD"])
        );
        assert_eq!(
            git(&work, &["config", merge_key]),
            format!("refs/heads/{branch}"),
            "上游配置保持原样"
        );
        cleanup(&[&work, &bare]);
    }

    /// 强推的 lease 期望值取**预览时**的远程跟踪值:对话框开着期间点了 Fetch,远程跟踪
    /// 引用已经是别人的新提交、但本地没整合它 —— 裸 `--force-with-lease` 拿的是 Fetch 之后
    /// 的值,会把别人的提交覆盖掉。
    #[test]
    fn 强推的_lease_按预览时的远程值() {
        let (work, bare, branch) = repo_with_origin("sync-lease");
        let PushPlan::Ready(plan) = push_plan(&work, None).unwrap() else {
            panic!("应当可推");
        };
        let previewed = plan.tracking_oid.expect("预览时远程跟踪引用在场");
        assert_eq!(
            previewed,
            git(
                &work,
                &["rev-parse", &format!("refs/remotes/origin/{branch}")]
            )
        );
        let other = clone_of(&bare, &branch);
        commit_file(&other, "o.txt", "o\n", "theirs");
        git(&other, &["push", "-q", "origin", &branch]);
        let theirs = git(&other, &["rev-parse", "HEAD"]);
        git(&work, &["fetch", "-q", "origin"]);
        commit_file(&work, "w.txt", "w\n", "mine");

        let out = git_push_with(
            &work,
            &PushOptions {
                branch: None,
                force_with_lease: true,
                expected_oid: Some(previewed),
            },
        )
        .unwrap();
        assert!(matches!(out, PushOutcome::Rejected { .. }), "{out:?}");
        assert_eq!(
            git(&bare, &["rev-parse", &branch]),
            theirs,
            "别人的提交不能被覆盖"
        );
        // 期望值不是完整哈希(UI 传错 / 被篡改)直接拒绝,不拼进命令行
        let bad = PushOptions {
            branch: None,
            force_with_lease: true,
            expected_oid: Some("-x".into()),
        };
        assert!(git_push_with(&work, &bad).is_err());
        cleanup(&[&work, &bare, &other]);
    }

    /// 强推时目标远端分支不存在(预览时没有远程跟踪引用):用不着强推选项,按普通推送建出来。
    #[test]
    fn 强推到不存在的远端分支按普通推送() {
        let (work, bare, _) = repo_with_origin("sync-force-new");
        git(&work, &["checkout", "-q", "-b", "brand-new"]);
        commit_file(&work, "n.txt", "n\n", "new");
        let out = git_push_with(
            &work,
            &PushOptions {
                branch: None,
                force_with_lease: true,
                expected_oid: None,
            },
        )
        .unwrap();
        assert!(matches!(out, PushOutcome::Pushed(_)), "{out:?}");
        assert_eq!(
            git(&bare, &["rev-parse", "brand-new"]),
            git(&work, &["rev-parse", "HEAD"])
        );
        cleanup(&[&work, &bare]);
    }

    /// `push.default=upstream` 推到上游名;推送远程按 `branch.<b>.pushRemote` >
    /// `remote.pushDefault` > 上游所在的远程;推往别的远程时目标与上游不同。
    #[test]
    fn 推送目标跟随_push_配置() {
        let (work, bare, branch) = repo_with_origin("sync-push-config");
        git(
            &work,
            &[
                "checkout",
                "-q",
                "-b",
                "feature",
                &format!("origin/{branch}"),
            ],
        );
        git(&work, &["config", "push.default", "upstream"]);
        let PushPlan::Ready(target) = push_plan(&work, None).unwrap() else {
            panic!("应当可推");
        };
        assert_eq!(target.label(), format!("origin/{branch}"));
        assert!(!target.upstream_differs);

        let fork = bare.with_file_name(format!(
            "{}-fork.git",
            bare.file_name().unwrap().to_string_lossy()
        ));
        git(
            bare.parent().unwrap(),
            &["init", "-q", "--bare", &fork.to_string_lossy()],
        );
        git(&work, &["remote", "add", "fork", &fork.to_string_lossy()]);
        git(&work, &["config", "remote.pushDefault", "fork"]);
        let PushPlan::Ready(target) = push_plan(&work, None).unwrap() else {
            panic!("应当可推");
        };
        assert_eq!(target.label(), "fork/feature", "推往别的远程:按同名");
        assert!(target.upstream_differs);
        assert!(!target.sets_upstream());

        git(&work, &["config", "branch.feature.pushRemote", "origin"]);
        git(&work, &["config", "push.default", "simple"]);
        let PushPlan::Ready(target) = push_plan(&work, None).unwrap() else {
            panic!("应当可推");
        };
        assert_eq!(
            target.label(),
            "origin/feature",
            "分支级 pushRemote 压过 pushDefault"
        );
        // 配置里写了不存在的远程:退到下一档
        git(&work, &["config", "branch.feature.pushRemote", "gone"]);
        let PushPlan::Ready(target) = push_plan(&work, None).unwrap() else {
            panic!("应当可推");
        };
        assert_eq!(target.remote, "fork");
        cleanup(&[&work, &bare, &fork]);
    }

    /// 老 git 不认 `--force-if-includes` 时据以重试的信号:不认识的选项一律退出码 129
    /// (只看退出码,不看会被本地化的提示文本)。
    #[test]
    fn 不认识的选项退出码是_129() {
        let root = init_repo_with_file("exit-129", "a.txt", "a\n");
        let out = run_git_output(
            &root,
            &["push", "--mt-definitely-unknown-option"],
            "push",
            std::time::Duration::from_secs(20),
            "",
        )
        .unwrap();
        assert!(!out.success);
        assert_eq!(out.code, Some(GIT_USAGE_ERROR), "{}", out.stderr);
        cleanup(&[&root]);
    }

    /// 拉取方式的显示也读 `branch.<b>.rebase`(它压过 `pull.rebase`)。
    #[test]
    fn 拉取方式读分支级_rebase_配置() {
        let root = init_repo_with_file("pull-branch-rebase", "a.txt", "a\n");
        let branch = current(&root);
        let set = |key: &str, value: &str| {
            Repository::open(&root)
                .unwrap()
                .config()
                .unwrap()
                .set_str(key, value)
                .unwrap();
        };
        // 「什么都没配」测不了:Git for Windows 的安装器会往系统级配置里写 `pull.rebase=false`
        set(&format!("branch.{branch}.rebase"), "true");
        assert_eq!(PullMode::from_config(&root), PullMode::Rebase);
        set("pull.rebase", "true");
        set(&format!("branch.{branch}.rebase"), "false");
        assert_eq!(
            PullMode::from_config(&root),
            PullMode::Merge,
            "分支级配置压过 pull.rebase"
        );
        set(&format!("branch.{branch}.rebase"), "merges");
        let config = pull_config(&root);
        assert_eq!(config.mode, PullMode::Rebase);
        assert!(config.rebase_merges, "merges 要显示成「保留合并的变基」");
        assert!(config.explicit);
        cleanup(&[&root]);
    }

    /// 按配置拉取的命令行:配置写明了方式就一个方式参数都不带;什么都没写时补
    /// `--no-rebase`(git 2.33+ 分叉时不再默认合并,而是直接报错)。
    #[test]
    fn 按配置拉取的参数() {
        let explicit = PullConfig {
            explicit: true,
            ..PullConfig::default()
        };
        assert_eq!(config_pull_args(&explicit), ["pull"]);
        assert_eq!(
            config_pull_args(&PullConfig::default()),
            ["pull", "--no-rebase"]
        );
    }

    /// 没手选方式时按用户配置拉:`pull.rebase=merges` 要保留本地的合并提交,
    /// 旧实现把它降成 `--rebase`,合并被拍平。
    #[test]
    fn 跟随配置拉取不拍平合并提交() {
        let (work, bare, branch) = repo_with_origin("sync-pull-merges");
        let other = clone_of(&bare, &branch);
        commit_file(&other, "o.txt", "o\n", "theirs");
        git(&other, &["push", "-q", "origin", &branch]);

        git(&work, &["checkout", "-q", "-b", "side"]);
        commit_file(&work, "s.txt", "s\n", "side");
        git(&work, &["checkout", "-q", &branch]);
        commit_file(&work, "m.txt", "m\n", "main");
        git(&work, &["merge", "-q", "--no-ff", "--no-edit", "side"]);
        git(&work, &["config", "pull.rebase", "merges"]);

        let out = git_pull_by_config(&work).unwrap();
        assert!(matches!(out, PullOutcome::Updated { .. }), "{out:?}");
        assert_eq!(
            git(&work, &["rev-list", "--count", "--merges", "@{u}..HEAD"]),
            "1",
            "本地的合并提交要保留"
        );
        cleanup(&[&work, &bare, &other]);
    }
}
