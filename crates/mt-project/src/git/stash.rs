//! 贮藏(git stash):列表 / 新建 / 应用 / 弹出 / 删除 / 清空 / 从贮藏新建分支 / 查看内容,
//! 以及「检出被本地改动挡住 → 贮藏后检出再恢复」(IDEA 的 Smart Checkout)。
//!
//! # 口径(与 `branch.rs` 同)
//!
//! - **改仓库状态的走 git CLI,只读查询走 git2**。CLI 继承用户的配置与过滤器
//!   (autocrlf / git-lfs),贮藏与恢复的语义也与用户在终端里敲的完全一致。
//! - **结果只看结构化信号**:有没有生成新贮藏看 `refs/stash` 前后变没变(「No local
//!   changes to save」是会被本地化的提示文本),冲突看索引里的冲突条目。stderr 只原样
//!   转交给用户看,不参与判断。
//!
//! # 贮藏提交的结构
//!
//! 一个贮藏是一个合并样的提交 W:第一个父 = 贮藏时的 HEAD,第二个父 = 暂存区快照,
//! 带 `--include-untracked` 时第三个父 U 存未跟踪文件。U 是**没有父提交的根提交**,
//! 所以「U 相对空树的 diff」正好是「这些文件整份新增」—— 查看内容时已跟踪的文件对 W
//! 取 diff(相对第一个父),未跟踪的对 U 取,两者都直接复用 [`get_commit_file_diff`]。
//!
//! # 下标为什么要带哈希核对
//!
//! `stash@{n}` 是**会漂移**的下标:列表弹出来之后用户在终端里又贮藏了一次,原先的
//! `stash@{1}` 就成了 `stash@{2}`,按旧下标删除 / 弹出会动到别的贮藏。所以改写类操作
//! 都收一个可选的 `expected_hash`,执行前核对该下标仍是那个提交,对不上就拒绝。
//!
//! 全部改写操作都是**阻塞**调用(最长 120s),调用方放后台执行器。

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use git2::{BranchType, Oid, Repository, RepositoryState, Status, StatusOptions};

use super::branch::head_branch_name;
use super::{
    CommitFileInfo, GitDiffResult, QUICK_OP_TIMEOUT, WORKTREE_OP_TIMEOUT, checkout_branch,
    failure_text, get_commit_file_diff, get_commit_files, invalidate_repo_cache, is_commit_hash,
    list_conflicts, run_checked, run_git_output, validate_branch_name,
};

// ---------------------------------------------------------------------------
// 列表
// ---------------------------------------------------------------------------

/// 一条贮藏。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashInfo {
    /// `stash@{n}` 里的 n(0 = 最新)。
    pub index: usize,
    /// `stash@{n}`。
    pub reference: String,
    /// 贮藏提交 W 的完整哈希。
    pub hash: String,
    /// 贮藏消息原文(`WIP on main: 1a2b3c4 init` / `On main: 我的说明`)。
    pub message: String,
    /// 去掉「`WIP on <分支>: `」/「`On <分支>: `」前缀后的部分;认不出格式时是原文。
    pub summary: String,
    /// git 自动生成的消息(贮藏时没给 `-m`)。此时 `summary` 是贮藏时 HEAD 的短哈希 + 标题。
    pub is_wip: bool,
    /// 贮藏时所在的分支;detached(`(no branch)`)或消息认不出时为 `None`。
    pub branch: Option<String>,
    /// 贮藏时间(W 的提交时间,秒)。
    pub timestamp: i64,
    /// 贮藏里带着未跟踪文件(有第三个父提交)。
    pub has_untracked: bool,
}

/// 拆贮藏消息 → `(分支, 摘要, 是否 git 自动生成的消息)`。
///
/// 格式是 git 写死的英文(reflog 消息不随界面语言本地化):`WIP on <分支>: <短哈希> <标题>`
/// 或 `On <分支>: <消息>`。分支名不可能含 `:`,按第一个冒号切不会切错;含空白的「分支」
/// 说明这其实是 `git stash store -m` 写进来的任意文本,原样当摘要。
fn parse_stash_message(message: &str) -> (Option<String>, String, bool) {
    let raw = || (None, message.trim().to_string(), false);
    let (rest, wip) = if let Some(rest) = message.strip_prefix("WIP on ") {
        (rest, true)
    } else if let Some(rest) = message.strip_prefix("On ") {
        (rest, false)
    } else {
        return raw();
    };
    let Some((branch, summary)) = rest.split_once(':') else {
        return raw();
    };
    let summary = summary.trim().to_string();
    if branch == "(no branch)" {
        return (None, summary, wip);
    }
    if branch.is_empty() || branch.contains(char::is_whitespace) {
        return raw();
    }
    (Some(branch.to_string()), summary, wip)
}

/// 全部贮藏,新的在前(`stash@{0}` 打头)。纯 git2 只读。
///
/// git2 的贮藏遍历要 `&mut Repository`(libgit2 的接口签名如此),实际只读 `refs/stash`
/// 的 reflog,不写任何东西。
pub fn list_stashes(repo_path: &Path) -> Result<Vec<StashInfo>> {
    let mut repo = Repository::open(repo_path)?;
    let mut raw: Vec<(usize, String, Oid)> = Vec::new();
    repo.stash_foreach(|index, message, oid| {
        raw.push((index, message.to_string(), *oid));
        true
    })?;
    let mut out = Vec::with_capacity(raw.len());
    for (index, message, oid) in raw {
        // 提交对象缺失(仓库坏了)的那条跳过,其余照常列出 —— 下标来自 reflog,不受影响
        let Ok(commit) = repo.find_commit(oid) else {
            continue;
        };
        let (branch, summary, is_wip) = parse_stash_message(&message);
        out.push(StashInfo {
            index,
            reference: format!("stash@{{{index}}}"),
            hash: oid.to_string(),
            message,
            summary,
            is_wip,
            branch,
            timestamp: commit.time().seconds(),
            has_untracked: commit.parent_count() >= 3,
        });
    }
    Ok(out)
}

/// 贮藏条数(= `git stash list` 的行数)。纯 git2 只读,只读 `refs/stash` 的 reflog 一个小
/// 文件,不解析提交 —— 仓库栏随分支快照一起读,有贮藏时在贮藏按钮旁显示数量。
pub fn stash_count(repo_path: &Path) -> Result<usize> {
    Ok(stash_entries(&Repository::open(repo_path)?))
}

/// [`stash_count`] 的本体(`.git` 监听的签名也算它:只删 `stash@{n>0}` 时 `refs/stash`
/// 不动,只有 reflog 少一条)。没有 `refs/stash` = 0(残留的 reflog 不算)。
pub(super) fn stash_entries(repo: &Repository) -> usize {
    if repo.find_reference("refs/stash").is_err() {
        return 0;
    }
    repo.reflog("refs/stash").map(|log| log.len()).unwrap_or(0)
}

/// `refs/stash` 此刻指向的提交(= `stash@{0}`);没有贮藏时为 `None`。
fn stash_tip(repo_path: &Path) -> Result<Option<Oid>> {
    let repo = Repository::open(repo_path)?;
    Ok(repo.refname_to_id("refs/stash").ok())
}

/// 核对 `stash@{index}` 存在、且(给了 `expected_hash` 时)仍是那个提交,返回 `stash@{index}`。
/// `expected_hash` 可以是短哈希(按前缀比)。
fn stash_ref(repo_path: &Path, index: usize, expected_hash: Option<&str>) -> Result<String> {
    let found = list_stashes(repo_path)?
        .into_iter()
        .find(|s| s.index == index)
        .ok_or_else(|| anyhow!("贮藏 stash@{{{index}}} 不存在,请刷新"))?;
    if let Some(expected) = expected_hash
        && !(is_commit_hash(expected) && found.hash.starts_with(&expected.to_ascii_lowercase()))
    {
        bail!("贮藏列表已变化(stash@{{{index}}} 已不是所选的那一个),请刷新后重试");
    }
    Ok(found.reference)
}

/// 索引里挂着冲突的文件路径。
fn conflict_paths(repo_path: &Path) -> Result<Vec<String>> {
    Ok(list_conflicts(repo_path)?
        .into_iter()
        .map(|c| c.path)
        .collect())
}

/// 应用 / 弹出 / 建分支之前:索引里还有冲突时 git 一律拒绝,而那时再看「索引里有冲突」
/// 会把旧冲突误判成这次的结果 —— 先在这里挡掉。
fn ensure_no_conflicts(repo_path: &Path) -> Result<()> {
    let count = list_conflicts(repo_path)?.len();
    if count > 0 {
        bail!("仍有 {count} 个冲突文件未解决,请先在更改区处理");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 新建
// ---------------------------------------------------------------------------

/// 新建贮藏的选项。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StashSaveOptions {
    /// 贮藏消息;`None` 或只有空白 = 用 git 的默认消息(`WIP on <分支>: …`)。
    pub message: Option<String>,
    /// `--keep-index`:已暂存的改动贮藏起来的同时**留在**暂存区与工作区。
    pub keep_index: bool,
    /// `--include-untracked`:未跟踪文件一并贮藏(并从工作区移走)。
    pub include_untracked: bool,
}

/// 新建贮藏的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StashSaveOutcome {
    /// 生成了新贮藏(新贮藏永远是 `stash@{0}`)。
    Saved { reference: String },
    /// 贮藏已经生成,但 git 退出码非零:典型是带 `--include-untracked` 时有未跟踪文件
    /// 被别的程序占用、清不掉(git 跳过了随后的 `reset --hard`,工作区没被清干净)。
    /// `detail` 是 git 的 stderr 原文。
    SavedWithWarnings { reference: String, detail: String },
    /// 没有可贮藏的改动(git 正常退出、什么都没做)。
    NothingToStash,
}

/// `git stash push [-m <消息>] [--keep-index] [--include-untracked]`。
///
/// 成败一律看 `refs/stash` 前后变没变,不看退出码、更不认 git 的提示文本(会被本地化):
/// - 变了 = 生成了新贮藏。退出码非零的话是 [`StashSaveOutcome::SavedWithWarnings`]
///   —— 带 `-u` 时有未跟踪文件被占用删不掉,git 已经存好贮藏、删掉了其余未跟踪文件,
///   只是随后的清理失败、退出码 1。报成失败的话用户会以为什么都没发生,再点一次就叠出
///   第二个贮藏;
/// - 没变且退出码为零 = 没有可贮藏的改动;没变且非零 = 真失败。
///
/// **阻塞最多 120s**。
pub fn stash_save(repo_path: &Path, options: &StashSaveOptions) -> Result<StashSaveOutcome> {
    let before = stash_tip(repo_path)?;
    let mut args = vec!["stash", "push"];
    if options.keep_index {
        args.push("--keep-index");
    }
    if options.include_untracked {
        args.push("--include-untracked");
    }
    let message = options
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    // `-m` 的值即使以 `-` 开头也只会被当成消息(parse-options 的 OPT_STRING 直接吃下一个参数)
    if let Some(message) = message {
        args.extend(["-m", message]);
    }
    // 操作名写死:args 里带着用户的消息文本,不该被拼进「超时 / 启动失败」的报错
    let out = run_git_output(repo_path, &args, "stash push", WORKTREE_OP_TIMEOUT, "")?;
    let after = stash_tip(repo_path)?;
    let created = after.is_some() && after != before;
    let reference = || "stash@{0}".to_string();
    Ok(match (created, out.success) {
        (true, true) => StashSaveOutcome::Saved {
            reference: reference(),
        },
        (true, false) => StashSaveOutcome::SavedWithWarnings {
            reference: reference(),
            detail: failure_text(&out),
        },
        (false, true) => StashSaveOutcome::NothingToStash,
        (false, false) => bail!("{}", failure_text(&out)),
    })
}

// ---------------------------------------------------------------------------
// 应用 / 弹出 / 删除 / 清空 / 建分支
// ---------------------------------------------------------------------------

/// 应用 / 弹出 / 从贮藏建分支的结果。冲突**不是错误**:改动已部分落到工作区,
/// 冲突文件在「更改」区里等用户解决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StashApplyOutcome {
    Applied,
    /// 出现冲突。**贮藏一律保留**:`apply` 本来就不删,`pop` / `stash branch` 遇到冲突
    /// 时 git 也不删 —— UI 要把这一点告诉用户(解决完冲突再手动删除)。
    Conflicts {
        files: Vec<String>,
    },
}

fn apply_like(
    repo_path: &Path,
    op: &'static str,
    index: usize,
    reinstate_index: bool,
    expected_hash: Option<&str>,
) -> Result<StashApplyOutcome> {
    ensure_no_conflicts(repo_path)?;
    let reference = stash_ref(repo_path, index, expected_hash)?;
    let mut args = vec!["stash", op];
    if reinstate_index {
        args.push("--index");
    }
    args.push(&reference);
    let out = run_git_output(
        repo_path,
        &args,
        &format!("stash {op}"),
        WORKTREE_OP_TIMEOUT,
        "",
    )?;
    if out.success {
        return Ok(StashApplyOutcome::Applied);
    }
    let files = conflict_paths(repo_path)?;
    if !files.is_empty() {
        return Ok(StashApplyOutcome::Conflicts { files });
    }
    bail!("{}", failure_text(&out));
}

/// 把 `stash@{index}` 的改动应用到工作区,贮藏保留。`reinstate_index` = `--index`:
/// 连贮藏时的暂存状态一起恢复(暂存区的改动贴不上时 git 拒绝,原文在错误里,
/// 不带它重来即可)。**阻塞最多 120s**。
pub fn stash_apply(
    repo_path: &Path,
    index: usize,
    reinstate_index: bool,
    expected_hash: Option<&str>,
) -> Result<StashApplyOutcome> {
    apply_like(repo_path, "apply", index, reinstate_index, expected_hash)
}

/// 同 [`stash_apply`],成功后删掉这个贮藏;**出现冲突时贮藏保留**。**阻塞最多 120s**。
pub fn stash_pop(
    repo_path: &Path,
    index: usize,
    reinstate_index: bool,
    expected_hash: Option<&str>,
) -> Result<StashApplyOutcome> {
    apply_like(repo_path, "pop", index, reinstate_index, expected_hash)
}

/// 删除 `stash@{index}`(`git stash drop`)。删掉的贮藏只能靠 reflog / fsck 找回,
/// UI 侧先确认。
pub fn stash_drop(repo_path: &Path, index: usize, expected_hash: Option<&str>) -> Result<()> {
    let reference = stash_ref(repo_path, index, expected_hash)?;
    run_checked(
        repo_path,
        &["stash", "drop", &reference],
        "stash drop",
        QUICK_OP_TIMEOUT,
        "",
    )?;
    Ok(())
}

/// 删除全部贮藏(`git stash clear`)。**不可恢复**,UI 侧二次确认。
pub fn stash_clear(repo_path: &Path) -> Result<()> {
    run_checked(
        repo_path,
        &["stash", "clear"],
        "stash clear",
        QUICK_OP_TIMEOUT,
        "",
    )?;
    Ok(())
}

/// 从贮藏新建分支(`git stash branch <name> stash@{n}`):在贮藏时的那个提交上新建
/// 并检出 `name`,再带 `--index` 应用贮藏,成功则删掉它。工作区改动挡住检出时是 `Err`
/// (git 原文)。**阻塞最多 120s**。
pub fn stash_branch(
    repo_path: &Path,
    name: &str,
    index: usize,
    expected_hash: Option<&str>,
) -> Result<StashApplyOutcome> {
    validate_branch_name(name).map_err(|issue| anyhow!("分支名不合法({issue:?}):{name}"))?;
    {
        let repo = Repository::open(repo_path)?;
        if repo.find_branch(name, BranchType::Local).is_ok() {
            bail!("已存在同名分支:{name}");
        }
    }
    ensure_no_conflicts(repo_path)?;
    let reference = stash_ref(repo_path, index, expected_hash)?;
    let out = run_git_output(
        repo_path,
        &["stash", "branch", name, &reference],
        "stash branch",
        WORKTREE_OP_TIMEOUT,
        "",
    )?;
    // 分支可能已经建好并检出(随后的应用才失败),仓库栏的分支名要重新发现
    invalidate_repo_cache();
    if out.success {
        return Ok(StashApplyOutcome::Applied);
    }
    let files = conflict_paths(repo_path)?;
    if !files.is_empty() {
        return Ok(StashApplyOutcome::Conflicts { files });
    }
    bail!("{}", failure_text(&out));
}

// ---------------------------------------------------------------------------
// 查看内容
// ---------------------------------------------------------------------------

/// 贮藏里的一个文件。
#[derive(Debug, Clone)]
pub struct StashFileInfo {
    pub file: CommitFileInfo,
    /// 来自第三个父提交的未跟踪文件(状态恒为 `added`)。
    pub untracked: bool,
    /// 取这个文件 diff 要用的提交:已跟踪 = 贮藏提交 W(相对贮藏时的 HEAD),
    /// 未跟踪 = 第三个父 U(根提交,相对空树 = 整份新增)。直接交给
    /// [`get_commit_file_diff`] / 提交 diff 面板即可。
    pub diff_hash: String,
}

/// 贮藏的改动文件:先已跟踪的(相对贮藏时的 HEAD,含已暂存与未暂存),再未跟踪的。
/// `stash_hash` 可以是短哈希。纯 git2 只读。
pub fn stash_files(repo_path: &Path, stash_hash: &str) -> Result<Vec<StashFileInfo>> {
    if !is_commit_hash(stash_hash) {
        bail!("不是有效的贮藏哈希:{stash_hash}");
    }
    let (full, untracked) = {
        let repo = Repository::open(repo_path)?;
        let commit = repo
            .revparse_single(stash_hash)
            .map_err(|_| anyhow!("找不到贮藏:{stash_hash}"))?
            .peel_to_commit()?;
        let untracked = if commit.parent_count() >= 3 {
            Some(commit.parent_id(2)?.to_string())
        } else {
            None
        };
        (commit.id().to_string(), untracked)
    };
    let mut out: Vec<StashFileInfo> = get_commit_files(repo_path, &full)?
        .into_iter()
        .map(|file| StashFileInfo {
            file,
            untracked: false,
            diff_hash: full.clone(),
        })
        .collect();
    if let Some(untracked) = untracked {
        out.extend(
            get_commit_files(repo_path, &untracked)?
                .into_iter()
                .map(|file| StashFileInfo {
                    file,
                    untracked: true,
                    diff_hash: untracked.clone(),
                }),
        );
    }
    Ok(out)
}

/// 贮藏里一个文件的 diff(见 [`StashFileInfo::diff_hash`])。
pub fn stash_file_diff(repo_path: &Path, file: &StashFileInfo) -> Result<GitDiffResult> {
    get_commit_file_diff(
        repo_path,
        &file.diff_hash,
        &file.file.path,
        file.file.old_path.as_deref(),
    )
}

// ---------------------------------------------------------------------------
// Smart checkout:贮藏后检出再恢复
// ---------------------------------------------------------------------------

/// 「贮藏后检出」的结果。除 [`SmartCheckoutOutcome::CheckoutCancelled`] 外,只要返回
/// `Ok`,HEAD 就已经在目标分支上了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmartCheckoutOutcome {
    /// 没有需要贮藏的本地改动(或本来就在该分支上),直接检出了。
    CheckedOut,
    /// 贮藏 → 检出 → 恢复一路顺利,自动贮藏已删除。
    Restored,
    /// 已检出,但恢复本地改动时出现冲突:冲突文件在「更改」区等用户解决,
    /// **自动贮藏保留**(在 `stash@{0}`),解决完再手动删除。
    RestoreConflicts { files: Vec<String> },
    /// 已检出,但恢复本地改动失败(非冲突:比如未跟踪文件与目标分支上的文件同名),
    /// **自动贮藏保留**;`detail` 是 git 的原文。
    RestoreFailed { detail: String },
    /// **没有检出**:自动贮藏已生成(在 `stash@{0}`),但工作区没清干净(有未跟踪文件被
    /// 占用删不掉),带着残留检出只会更乱,于是停在原分支。改动都在那个贮藏里;`detail`
    /// 是 git 的原文。
    CheckoutCancelled { detail: String },
}

/// 恢复自动贮藏的三种结局。
enum Restore {
    Done,
    Conflicts(Vec<String>),
    Failed(String),
}

/// 工作区干净(没有已跟踪改动、没有未跟踪文件;子模块不算)。
fn worktree_clean(repo_path: &Path) -> Result<bool> {
    let repo = Repository::open(repo_path)?;
    let mut opts = StatusOptions::new();
    opts.include_untracked(true).exclude_submodules(true);
    Ok(repo.statuses(Some(&mut opts))?.is_empty())
}

/// 把自动贮藏弹回来。起不来 / 超时这类错误也收成 [`Restore::Failed`] —— 调用方那时
/// 已经换过分支,不能再整个报成「检出失败」。
fn restore_auto_stash(repo_path: &Path, stash_hash: &str) -> Restore {
    try_restore(repo_path, stash_hash).unwrap_or_else(|err| Restore::Failed(format!("{err:#}")))
}

fn try_restore(repo_path: &Path, stash_hash: &str) -> Result<Restore> {
    let present = |repo_path: &Path| -> Result<Option<usize>> {
        Ok(list_stashes(repo_path)?
            .into_iter()
            .find(|s| s.hash == stash_hash)
            .map(|s| s.index))
    };
    let index = present(repo_path)?.ok_or_else(|| anyhow!("找不到自动贮藏 {stash_hash}"))?;
    let reference = format!("stash@{{{index}}}");
    let pop = |with_index: bool| {
        let mut args = vec!["stash", "pop"];
        if with_index {
            args.push("--index");
        }
        args.push(&reference);
        run_git_output(repo_path, &args, "stash pop", WORKTREE_OP_TIMEOUT, "")
    };

    // 先连暂存状态一起恢复
    let first = pop(true)?;
    if first.success {
        return Ok(Restore::Done);
    }
    let files = conflict_paths(repo_path)?;
    if !files.is_empty() {
        return Ok(Restore::Conflicts(files));
    }
    // `--index` 被拒(暂存区的改动在新分支上贴不上)时 git 还没动工作区:贮藏还在、
    // 工作区仍是刚检出的干净状态 —— 退一步不恢复暂存状态,改动全部回到「未暂存」。
    // 不满足这两条说明 git 已经动过手(比如已跟踪的改动落下了、未跟踪的没落下),
    // 再来一次只会叠出更乱的状态,原文交给用户
    if present(repo_path)?.is_none() || !worktree_clean(repo_path)? {
        return Ok(Restore::Failed(failure_text(&first)));
    }
    let second = pop(false)?;
    if second.success {
        return Ok(Restore::Done);
    }
    let files = conflict_paths(repo_path)?;
    if !files.is_empty() {
        return Ok(Restore::Conflicts(files));
    }
    Ok(Restore::Failed(failure_text(&second)))
}

/// 贮藏后检出本地分支 `branch`:`stash push --include-untracked` → 检出 → `stash pop`
/// (先带 `--index`,暂存区贴不上再退回不带)。
///
/// - 没有可贮藏的改动:直接检出,[`SmartCheckoutOutcome::CheckedOut`];
/// - 贮藏生成了但工作区没清干净:不检出,[`SmartCheckoutOutcome::CheckoutCancelled`];
/// - **检出失败**:把贮藏原样恢复回去,返回 `Err`(检出的 git 原文;恢复也不顺时附上说明);
/// - 恢复时冲突 / 失败:HEAD 已在目标分支,贮藏保留,见各变体的说明。
///
/// 检出「失败」但 HEAD 其实已经切过去(post-checkout hook 非零退出会让 `git checkout`
/// 整体非零,比如装了 LFS 配置却没装 git-lfs 的仓库)按检出成功处理 —— 恢复照样在目标
/// 分支上进行,报成「检出失败」反而误导。**阻塞,最长约 6 分钟**(三条 git 命令各 120s 上限)。
pub fn checkout_with_stash(repo_path: &Path, branch: &str) -> Result<SmartCheckoutOutcome> {
    {
        let repo = Repository::open(repo_path)?;
        repo.find_branch(branch, BranchType::Local)
            .map_err(|_| anyhow!("未找到本地分支:{branch}"))?;
        if head_branch_name(&repo).as_deref() == Some(branch) {
            return Ok(SmartCheckoutOutcome::CheckedOut);
        }
        if repo.state() != RepositoryState::Clean {
            bail!("仓库正处于未完成的合并 / 变基 / 拣选中,请先完成或中止");
        }
    }
    ensure_no_conflicts(repo_path)?;

    let saved = stash_save(
        repo_path,
        &StashSaveOptions {
            message: Some(format!("mini-term: 检出 {branch} 前自动贮藏")),
            keep_index: false,
            include_untracked: true,
        },
    )?;
    match saved {
        StashSaveOutcome::NothingToStash => {
            checkout_branch(repo_path, branch)?;
            return Ok(SmartCheckoutOutcome::CheckedOut);
        }
        // 贮藏生成了、工作区却没清干净(被占用的未跟踪文件删不掉,git 也就跳过了随后的
        // `reset --hard`):带着残留检出,要么被它们挡住、要么切过去再恢复时与自己撞车,
        // 两头都更乱。停在原分支,告诉用户改动都在贮藏里
        StashSaveOutcome::SavedWithWarnings { detail, .. } => {
            return Ok(SmartCheckoutOutcome::CheckoutCancelled { detail });
        }
        StashSaveOutcome::Saved { .. } => {}
    }
    let stash_hash = stash_tip(repo_path)?
        .ok_or_else(|| anyhow!("自动贮藏后找不到 refs/stash"))?
        .to_string();

    let checkout = checkout_branch(repo_path, branch);
    let landed = checkout.is_ok()
        || Repository::open(repo_path)
            .ok()
            .and_then(|repo| head_branch_name(&repo))
            .as_deref()
            == Some(branch);
    let restore = restore_auto_stash(repo_path, &stash_hash);

    if !landed {
        let err = checkout.err().map(|e| format!("{e:#}")).unwrap_or_default();
        match restore {
            Restore::Done => bail!("{err}"),
            Restore::Conflicts(files) => bail!(
                "{err}\n\n恢复本地改动时出现冲突({} 个文件),请在更改区解决;贮藏已保留",
                files.len()
            ),
            Restore::Failed(detail) => {
                bail!("{err}\n\n恢复本地改动失败,贮藏已保留(git stash list 可见):{detail}")
            }
        }
    }
    Ok(match restore {
        Restore::Done => SmartCheckoutOutcome::Restored,
        Restore::Conflicts(files) => SmartCheckoutOutcome::RestoreConflicts { files },
        Restore::Failed(detail) => SmartCheckoutOutcome::RestoreFailed { detail },
    })
}

/// 检出 `branch`(本地优先,其次远程跟踪分支)会被哪些本地改动挡住:「HEAD 与目标之间
/// 有差异的路径」∩「本地有改动的路径(含未跟踪文件,不含忽略的)」,按路径排序。
/// 纯 git2 只读,检出前给 UI 提示「贮藏后检出」用。
///
/// 比 git 自己的判定**略保守**:本地改动恰好与目标分支内容相同时 git 放行,这里仍会
/// 列出来;目录与文件互相顶替(本地未跟踪 `foo/x`、目标里 `foo` 是文件)这里不认。
pub fn local_changes_conflict_with(repo_path: &Path, branch: &str) -> Result<Vec<String>> {
    let repo = Repository::open(repo_path)?;
    let target = repo
        .find_branch(branch, BranchType::Local)
        .or_else(|_| repo.find_branch(branch, BranchType::Remote))
        .map_err(|_| anyhow!("未找到分支:{branch}"))?;
    let target_tree = target.get().peel_to_tree()?;
    // 空仓库没有 HEAD:相对空树比,目标里的每个文件都算「有差异」
    let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
    let diff = repo.diff_tree_to_tree(head_tree.as_ref(), Some(&target_tree), None)?;
    let mut touched: HashSet<String> = HashSet::new();
    for delta in diff.deltas() {
        for file in [delta.old_file(), delta.new_file()] {
            if let Some(path) = file.path() {
                touched.insert(path.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    if touched.is_empty() {
        return Ok(Vec::new());
    }
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .exclude_submodules(true);
    let statuses = repo.statuses(Some(&mut opts))?;
    let mut out: Vec<String> = statuses
        .iter()
        .filter(|e| e.status() != Status::CURRENT && !e.status().is_ignored())
        .filter_map(|e| e.path().map(str::to_string))
        .filter(|path| touched.contains(path))
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use super::super::test_support::{cleanup, commit_file, current, git, init_repo_with_file};
    use super::*;

    fn opts(message: Option<&str>, keep_index: bool, include_untracked: bool) -> StashSaveOptions {
        StashSaveOptions {
            message: message.map(str::to_string),
            keep_index,
            include_untracked,
        }
    }

    /// 读文件并归一换行(被测函数走的 git 读机器的 autocrlf,检出可能是 CRLF)。
    fn read(root: &Path, rel: &str) -> String {
        std::fs::read_to_string(root.join(rel))
            .unwrap()
            .replace("\r\n", "\n")
    }

    fn staged_names(root: &Path) -> String {
        git(root, &["diff", "--cached", "--name-only"])
    }

    fn saved() -> StashSaveOutcome {
        StashSaveOutcome::Saved {
            reference: "stash@{0}".into(),
        }
    }

    #[test]
    fn 解析贮藏消息() {
        assert_eq!(
            parse_stash_message("WIP on main: 1a2b3c4 init"),
            (Some("main".into()), "1a2b3c4 init".into(), true)
        );
        assert_eq!(
            parse_stash_message("On feature/x: 修登录"),
            (Some("feature/x".into()), "修登录".into(), false)
        );
        assert_eq!(
            parse_stash_message("WIP on (no branch): 1a2b3c4 init"),
            (None, "1a2b3c4 init".into(), true),
            "detached 时没有分支"
        );
        assert_eq!(
            parse_stash_message("On main:"),
            (Some("main".into()), String::new(), false)
        );
        // `git stash store -m` 写进来的任意文本:原样当摘要
        assert_eq!(
            parse_stash_message("autostash"),
            (None, "autostash".into(), false)
        );
        assert_eq!(
            parse_stash_message("On my way: x"),
            (None, "On my way: x".into(), false)
        );
    }

    /// 没改动 / 只有未跟踪文件却不含未跟踪 → NothingToStash;含未跟踪与默认消息各贮藏一次,
    /// 列表新的在前、字段齐全。
    #[test]
    fn 新建贮藏与列表() {
        let root = init_repo_with_file("stash-save", "a.txt", "a\n");
        let main = current(&root);
        assert!(list_stashes(&root).unwrap().is_empty());
        assert_eq!(stash_count(&root).unwrap(), 0);
        assert_eq!(
            stash_save(&root, &opts(None, false, true)).unwrap(),
            StashSaveOutcome::NothingToStash
        );

        std::fs::write(root.join("new.txt"), "n\n").unwrap();
        assert_eq!(
            stash_save(&root, &opts(None, false, false)).unwrap(),
            StashSaveOutcome::NothingToStash,
            "只有未跟踪文件、又不含未跟踪"
        );
        assert!(root.join("new.txt").exists());
        assert_eq!(
            stash_save(&root, &opts(Some("  带未跟踪  "), false, true)).unwrap(),
            saved()
        );
        assert!(!root.join("new.txt").exists(), "未跟踪文件被收走");

        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        assert_eq!(
            stash_save(&root, &opts(Some("   "), false, false)).unwrap(),
            saved(),
            "只有空白的消息 = 用默认消息"
        );
        assert_eq!(read(&root, "a.txt"), "a\n");

        let list = list_stashes(&root).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(stash_count(&root).unwrap(), 2, "条数与列表一致");
        let (newest, older) = (&list[0], &list[1]);
        assert_eq!((newest.index, newest.reference.as_str()), (0, "stash@{0}"));
        assert!(newest.is_wip);
        assert!(newest.summary.ends_with("init"), "{newest:?}");
        assert_eq!(newest.branch.as_deref(), Some(main.as_str()));
        assert!(!newest.has_untracked);
        assert_eq!(newest.hash.len(), 40);
        assert!(newest.timestamp > 0);
        assert_eq!((older.index, older.reference.as_str()), (1, "stash@{1}"));
        assert_eq!(older.summary, "带未跟踪");
        assert!(!older.is_wip);
        assert!(older.has_untracked);
        assert_eq!(older.message, format!("On {main}: 带未跟踪"));
        cleanup(&[&root]);
    }

    /// `--keep-index`:已暂存的改动留在暂存区与工作区,未暂存的被收走。
    #[test]
    fn 保留暂存区() {
        let root = init_repo_with_file("stash-keep", "a.txt", "a\n");
        commit_file(&root, "b.txt", "b\n", "b");
        std::fs::write(root.join("a.txt"), "staged\n").unwrap();
        git(&root, &["add", "a.txt"]);
        std::fs::write(root.join("b.txt"), "unstaged\n").unwrap();

        assert_eq!(
            stash_save(&root, &opts(None, true, false)).unwrap(),
            saved()
        );
        assert_eq!(read(&root, "a.txt"), "staged\n");
        assert_eq!(staged_names(&root), "a.txt");
        assert_eq!(read(&root, "b.txt"), "b\n");
        cleanup(&[&root]);
    }

    /// apply 保留贮藏、不带 `--index` 时暂存状态不恢复;pop 带 `--index` 连暂存状态一起
    /// 恢复并删掉贮藏。
    #[test]
    fn 应用与弹出() {
        let root = init_repo_with_file("stash-apply", "a.txt", "a\n");
        commit_file(&root, "b.txt", "b\n", "b");
        std::fs::write(root.join("a.txt"), "staged\n").unwrap();
        git(&root, &["add", "a.txt"]);
        std::fs::write(root.join("b.txt"), "unstaged\n").unwrap();
        assert_eq!(
            stash_save(&root, &opts(None, false, false)).unwrap(),
            saved()
        );
        let hash = list_stashes(&root).unwrap()[0].hash.clone();

        assert_eq!(
            stash_apply(&root, 0, false, Some(&hash)).unwrap(),
            StashApplyOutcome::Applied
        );
        assert_eq!(read(&root, "a.txt"), "staged\n");
        assert_eq!(read(&root, "b.txt"), "unstaged\n");
        assert_eq!(staged_names(&root), "", "不带 --index:暂存状态不恢复");
        assert_eq!(list_stashes(&root).unwrap().len(), 1, "apply 不删贮藏");

        git(&root, &["reset", "-q", "--hard"]);
        assert_eq!(
            stash_pop(&root, 0, true, Some(&hash)).unwrap(),
            StashApplyOutcome::Applied
        );
        assert_eq!(staged_names(&root), "a.txt", "带 --index:暂存状态也恢复");
        assert_eq!(read(&root, "b.txt"), "unstaged\n");
        assert!(list_stashes(&root).unwrap().is_empty(), "pop 成功后删贮藏");
        cleanup(&[&root]);
    }

    /// pop 遇到冲突:返回冲突文件、贮藏保留;冲突没解决时再应用直接拒绝。
    #[test]
    fn 弹出冲突时贮藏保留() {
        let root = init_repo_with_file("stash-conflict", "a.txt", "a\n");
        std::fs::write(root.join("a.txt"), "stashed\n").unwrap();
        assert_eq!(
            stash_save(&root, &opts(None, false, false)).unwrap(),
            saved()
        );
        commit_file(&root, "a.txt", "committed\n", "move on");

        assert_eq!(
            stash_pop(&root, 0, false, None).unwrap(),
            StashApplyOutcome::Conflicts {
                files: vec!["a.txt".into()]
            }
        );
        assert_eq!(list_stashes(&root).unwrap().len(), 1, "冲突时 pop 不删贮藏");
        assert!(
            stash_apply(&root, 0, false, None).is_err(),
            "还有冲突没解决时不许再应用"
        );
        cleanup(&[&root]);
    }

    /// 删除按下标 + 哈希核对(对不上就拒,短哈希按前缀认);越界下标报错;清空删光。
    #[test]
    fn 删除与清空() {
        let root = init_repo_with_file("stash-drop", "a.txt", "a\n");
        for (content, message) in [("1\n", "one"), ("2\n", "two"), ("3\n", "three")] {
            std::fs::write(root.join("a.txt"), content).unwrap();
            assert_eq!(
                stash_save(&root, &opts(Some(message), false, false)).unwrap(),
                saved()
            );
        }
        let list = list_stashes(&root).unwrap();
        let summaries: Vec<&str> = list.iter().map(|s| s.summary.as_str()).collect();
        assert_eq!(summaries, vec!["three", "two", "one"]);
        let two = list[1].clone();

        assert!(
            stash_drop(&root, 0, Some(&two.hash)).is_err(),
            "下标漂移(stash@{{0}} 已不是 two)要拒"
        );
        assert!(stash_drop(&root, 0, Some("-x")).is_err());
        assert_eq!(list_stashes(&root).unwrap().len(), 3);
        stash_drop(&root, 1, Some(&two.hash[..10])).unwrap();
        let summaries: Vec<String> = list_stashes(&root)
            .unwrap()
            .into_iter()
            .map(|s| s.summary)
            .collect();
        assert_eq!(summaries, vec!["three", "one"]);
        assert_eq!(stash_count(&root).unwrap(), 2, "删中间那条只少一条 reflog");
        assert!(stash_drop(&root, 5, None).is_err(), "越界下标");

        stash_clear(&root).unwrap();
        assert!(list_stashes(&root).unwrap().is_empty());
        assert_eq!(stash_count(&root).unwrap(), 0);
        cleanup(&[&root]);
    }

    /// 从贮藏新建分支:在贮藏时的提交上建分支并检出、应用后删贮藏;非法名与重名先挡下。
    #[test]
    fn 从贮藏新建分支() {
        let root = init_repo_with_file("stash-branch", "a.txt", "a\n");
        let main = current(&root);
        std::fs::write(root.join("a.txt"), "wip\n").unwrap();
        assert_eq!(
            stash_save(&root, &opts(Some("wip"), false, false)).unwrap(),
            saved()
        );
        // 当前分支往前走了一步:直接在 main 上应用会冲突,从贮藏建分支不会
        commit_file(&root, "a.txt", "moved\n", "main moves");

        assert!(
            stash_branch(&root, "-x", 0, None).is_err(),
            "选项样的名字要拒"
        );
        assert!(stash_branch(&root, &main, 0, None).is_err(), "同名分支要拒");
        assert_eq!(
            stash_branch(&root, "from-stash", 0, None).unwrap(),
            StashApplyOutcome::Applied
        );
        assert_eq!(current(&root), "from-stash");
        assert_eq!(read(&root, "a.txt"), "wip\n");
        assert!(list_stashes(&root).unwrap().is_empty());
        cleanup(&[&root]);
    }

    /// 内容:已跟踪的(修改 / 删除 / 已暂存的新增)在前、未跟踪的在后并带标记;
    /// 已跟踪的 diff 相对贮藏时的 HEAD,未跟踪的是整份新增。
    #[test]
    fn 查看贮藏内容() {
        let root = init_repo_with_file("stash-files", "a.txt", "a\n");
        commit_file(&root, "b.txt", "b\n", "b");
        std::fs::write(root.join("a.txt"), "a2\n").unwrap();
        std::fs::write(root.join("c.txt"), "c\n").unwrap();
        git(&root, &["add", "c.txt"]);
        std::fs::remove_file(root.join("b.txt")).unwrap();
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::write(root.join("dir/u.txt"), "u1\nu2\n").unwrap();
        assert_eq!(
            stash_save(&root, &opts(Some("mix"), false, true)).unwrap(),
            saved()
        );
        let stash = list_stashes(&root).unwrap().remove(0);

        let files = stash_files(&root, &stash.hash[..12]).unwrap();
        let shape: Vec<(&str, &str, bool)> = files
            .iter()
            .map(|f| (f.file.path.as_str(), f.file.status.as_str(), f.untracked))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("a.txt", "modified", false),
                ("b.txt", "deleted", false),
                ("c.txt", "added", false),
                ("dir/u.txt", "added", true),
            ]
        );
        assert_eq!(files[0].diff_hash, stash.hash);
        assert_ne!(files[3].diff_hash, stash.hash);

        let tracked = stash_file_diff(&root, &files[0]).unwrap();
        assert_eq!(tracked.old_content.replace("\r\n", "\n"), "a\n");
        assert_eq!(tracked.new_content.replace("\r\n", "\n"), "a2\n");
        let untracked = stash_file_diff(&root, &files[3]).unwrap();
        assert_eq!(untracked.old_content, "");
        assert_eq!(untracked.new_content.replace("\r\n", "\n"), "u1\nu2\n");
        let kinds: Vec<&str> = untracked
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .map(|l| l.kind.as_str())
            .collect();
        assert_eq!(kinds, vec!["add", "add"], "未跟踪文件整份新增");

        assert!(stash_files(&root, "-x").is_err());
        assert!(stash_files(&root, "deadbeef").is_err());
        cleanup(&[&root]);
    }

    /// 贮藏里的纯重命名(`git mv` 后贮藏)配成一条 renamed、带旧路径,diff 无改动行
    /// —— 不拆成一删一增,「仅重命名」的提示才走得到。
    #[test]
    fn 贮藏里的纯重命名识别为重命名() {
        let root = init_repo_with_file("stash-rename", "old.txt", "1\n2\n3\n4\n5\n");
        git(&root, &["mv", "old.txt", "new.txt"]);
        assert_eq!(
            stash_save(&root, &opts(None, false, false)).unwrap(),
            saved()
        );
        let stash = list_stashes(&root).unwrap().remove(0);

        let files = stash_files(&root, &stash.hash).unwrap();
        let shape: Vec<(&str, &str, Option<&str>)> = files
            .iter()
            .map(|f| {
                (
                    f.file.path.as_str(),
                    f.file.status.as_str(),
                    f.file.old_path.as_deref(),
                )
            })
            .collect();
        assert_eq!(shape, vec![("new.txt", "renamed", Some("old.txt"))]);
        let diff = stash_file_diff(&root, &files[0]).unwrap();
        assert!(diff.hunks.is_empty(), "{:?}", diff.hunks);
        cleanup(&[&root]);
    }

    /// other 分支改了 a.txt 第一行、新增了 added.txt;停在 main 上。返回 `(仓库, main)`。
    fn two_branch_repo(tag: &str) -> (PathBuf, String) {
        let root = init_repo_with_file(tag, "a.txt", "1\n2\n3\n4\n5\n");
        let main = current(&root);
        git(&root, &["checkout", "-q", "-b", "other"]);
        commit_file(&root, "a.txt", "one\n2\n3\n4\n5\n", "other edits line 1");
        commit_file(&root, "added.txt", "from other\n", "other adds a file");
        git(&root, &["checkout", "-q", &main]);
        (root, main)
    }

    /// 冲突路径 = 目标分支上有差异 ∩ 本地有改动;无关的本地改动不算;认不出的分支报错。
    #[test]
    fn 检出前的冲突预判() {
        let (root, _) = two_branch_repo("smart-overlap");
        std::fs::write(root.join("z.txt"), "unrelated\n").unwrap();
        assert!(
            local_changes_conflict_with(&root, "other")
                .unwrap()
                .is_empty()
        );
        std::fs::write(root.join("a.txt"), "x\n2\n3\n4\n5\n").unwrap();
        std::fs::write(root.join("added.txt"), "mine\n").unwrap();
        assert_eq!(
            local_changes_conflict_with(&root, "other").unwrap(),
            vec!["a.txt", "added.txt"]
        );
        assert!(local_changes_conflict_with(&root, "nope").is_err());
        cleanup(&[&root]);
    }

    /// 干净工作区:直接检出;已在目标分支上什么都不做;分支不存在报错。
    #[test]
    fn 贮藏后检出_没有改动() {
        let (root, _) = two_branch_repo("smart-clean");
        assert_eq!(
            checkout_with_stash(&root, "other").unwrap(),
            SmartCheckoutOutcome::CheckedOut
        );
        assert_eq!(current(&root), "other");
        assert_eq!(
            checkout_with_stash(&root, "other").unwrap(),
            SmartCheckoutOutcome::CheckedOut
        );
        assert!(checkout_with_stash(&root, "nope").is_err());
        assert!(list_stashes(&root).unwrap().is_empty());
        cleanup(&[&root]);
    }

    /// 本地改动挡住普通检出,但与目标分支的改动不相交:贮藏后检出合并成功,
    /// 暂存状态与未跟踪文件都回来,自动贮藏删掉。
    #[test]
    fn 贮藏后检出_顺利恢复() {
        let (root, _) = two_branch_repo("smart-merge");
        std::fs::write(root.join("a.txt"), "1\n2\n3\n4\nfive\n").unwrap();
        std::fs::write(root.join("staged.txt"), "s\n").unwrap();
        git(&root, &["add", "staged.txt"]);
        std::fs::write(root.join("notes.txt"), "n\n").unwrap();
        assert_eq!(
            local_changes_conflict_with(&root, "other").unwrap(),
            vec!["a.txt"]
        );
        assert!(checkout_branch(&root, "other").is_err(), "普通检出被挡住");

        assert_eq!(
            checkout_with_stash(&root, "other").unwrap(),
            SmartCheckoutOutcome::Restored
        );
        assert_eq!(current(&root), "other");
        assert_eq!(read(&root, "a.txt"), "one\n2\n3\n4\nfive\n");
        assert_eq!(read(&root, "notes.txt"), "n\n");
        assert_eq!(staged_names(&root), "staged.txt", "暂存状态也恢复了");
        assert!(list_stashes(&root).unwrap().is_empty());
        cleanup(&[&root]);
    }

    /// 本地改动与目标分支改了同一行:已检出,恢复冲突,自动贮藏保留。
    #[test]
    fn 贮藏后检出_恢复冲突() {
        let (root, main) = two_branch_repo("smart-conflict");
        std::fs::write(root.join("a.txt"), "uno\n2\n3\n4\n5\n").unwrap();
        assert_eq!(
            checkout_with_stash(&root, "other").unwrap(),
            SmartCheckoutOutcome::RestoreConflicts {
                files: vec!["a.txt".into()]
            }
        );
        assert_eq!(current(&root), "other");
        let list = list_stashes(&root).unwrap();
        assert_eq!(list.len(), 1, "冲突时自动贮藏保留");
        assert_eq!(list[0].branch.as_deref(), Some(main.as_str()));
        assert!(list[0].summary.contains("other"), "{:?}", list[0]);
        cleanup(&[&root]);
    }

    /// 未跟踪文件与目标分支上的文件同名:已检出,恢复失败(非冲突),自动贮藏保留。
    #[test]
    fn 贮藏后检出_未跟踪文件撞名() {
        let (root, _) = two_branch_repo("smart-untracked");
        std::fs::write(root.join("added.txt"), "mine\n").unwrap();
        assert!(checkout_branch(&root, "other").is_err(), "普通检出被挡住");
        let out = checkout_with_stash(&root, "other").unwrap();
        assert!(
            matches!(out, SmartCheckoutOutcome::RestoreFailed { .. }),
            "{out:?}"
        );
        assert_eq!(current(&root), "other");
        assert_eq!(read(&root, "added.txt"), "from other\n");
        assert_eq!(list_stashes(&root).unwrap().len(), 1, "自动贮藏保留");
        cleanup(&[&root]);
    }

    /// post-checkout hook 非零退出让 `git checkout` 报失败,但 HEAD 已切过去:
    /// 按检出成功处理,改动照样恢复。
    #[test]
    fn 贮藏后检出_hook_失败但已切过去() {
        let (root, main) = two_branch_repo("smart-hook");
        // hooks 目录放在工作区外:放里面会被当成未跟踪文件一起贮藏走
        let hooks = root.with_file_name(format!(
            "{}-hooks",
            root.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("post-checkout");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // 钉死 hooks 目录:机器的全局 core.hooksPath 不该让这条用例失效
        git(
            &root,
            &["config", "core.hooksPath", &hooks.to_string_lossy()],
        );
        // 先确认这条路径真走到了:普通检出报失败,HEAD 却已经切过去
        git(&root, &["branch", "probe"]);
        assert!(
            checkout_branch(&root, "probe").is_err(),
            "hook 让检出报失败"
        );
        assert_eq!(current(&root), "probe", "但 HEAD 已经切过去");
        let _ = checkout_branch(&root, &main);
        assert_eq!(current(&root), main);
        std::fs::write(root.join("a.txt"), "1\n2\n3\n4\nfive\n").unwrap();

        assert_eq!(
            checkout_with_stash(&root, "other").unwrap(),
            SmartCheckoutOutcome::Restored
        );
        assert_eq!(current(&root), "other");
        assert_eq!(read(&root, "a.txt"), "one\n2\n3\n4\nfive\n");
        assert!(list_stashes(&root).unwrap().is_empty());
        cleanup(&[&root, &hooks]);
    }

    /// 以「读写都共享、唯独不共享删除」打开:编辑器 / 杀毒软件开着文件时的样子 ——
    /// git 读得到内容(贮藏得进去),删不掉(清理工作区失败)。
    #[cfg(windows)]
    fn hold_open(path: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(path)
            .unwrap()
    }

    /// `stash push -u` 删不掉被占用的未跟踪文件:贮藏已经生成、其余未跟踪文件已删,
    /// 只是退出码 1。按 `refs/stash` 前后变化判成「已贮藏但有警告」,不能报成失败
    /// (用户会以为什么都没发生,再点一次就叠出第二个贮藏)。
    #[cfg(windows)]
    #[test]
    fn 贮藏时未跟踪文件被占用_贮藏照样生成并带警告() {
        let root = init_repo_with_file("stash-locked", "a.txt", "a\n");
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        std::fs::write(root.join("locked.txt"), "l\n").unwrap();
        std::fs::write(root.join("free.txt"), "f\n").unwrap();
        let handle = hold_open(&root.join("locked.txt"));
        let out = stash_save(&root, &opts(Some("busy"), false, true));
        drop(handle);
        match out.unwrap() {
            StashSaveOutcome::SavedWithWarnings { reference, detail } => {
                assert_eq!(reference, "stash@{0}");
                assert!(detail.contains("locked.txt"), "{detail}");
            }
            other => panic!("应为已贮藏但有警告: {other:?}"),
        }
        let list = list_stashes(&root).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].summary, "busy");
        assert!(!root.join("free.txt").exists(), "其余未跟踪文件照常收走");
        cleanup(&[&root]);
    }

    /// 贮藏后检出遇到同样的情况:工作区没清干净就**不检出**,明确告诉用户改动在贮藏里。
    #[cfg(windows)]
    #[test]
    fn 贮藏后检出_未跟踪文件被占用就不检出() {
        let (root, main) = two_branch_repo("smart-locked");
        std::fs::write(root.join("a.txt"), "x\n2\n3\n4\n5\n").unwrap();
        std::fs::write(root.join("locked.txt"), "l\n").unwrap();
        let handle = hold_open(&root.join("locked.txt"));
        let out = checkout_with_stash(&root, "other");
        drop(handle);
        match out.unwrap() {
            SmartCheckoutOutcome::CheckoutCancelled { detail } => {
                assert!(detail.contains("locked.txt"), "{detail}")
            }
            other => panic!("应为检出已取消: {other:?}"),
        }
        assert_eq!(current(&root), main, "停在原分支");
        assert_eq!(list_stashes(&root).unwrap().len(), 1, "自动贮藏保留");
        cleanup(&[&root]);
    }

    /// `.git` 下每个文件的 (修改时间, 长度)。
    fn snapshot(git_dir: &Path) -> BTreeMap<PathBuf, (SystemTime, u64)> {
        let mut out = BTreeMap::new();
        let mut stack = vec![git_dir.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                let meta = entry.metadata().unwrap();
                if meta.is_dir() {
                    stack.push(path);
                } else {
                    out.insert(path, (meta.modified().unwrap(), meta.len()));
                }
            }
        }
        out
    }

    /// 只读查询(列表 / 内容 / 冲突预判)不往 `.git` 里写任何东西 —— 面板刷新会调它们,
    /// 写了就会被 `.git` 监听收到、自激刷新。
    #[test]
    fn 只读查询不写_git_目录() {
        let (root, _) = two_branch_repo("stash-readonly");
        std::fs::write(root.join("a.txt"), "x\n2\n3\n4\n5\n").unwrap();
        std::fs::write(root.join("u.txt"), "u\n").unwrap();
        assert_eq!(
            stash_save(&root, &opts(None, false, true)).unwrap(),
            saved()
        );
        // 再改一次(同长度),让索引里的 stat 缓存过期
        std::fs::write(root.join("a.txt"), "y\n2\n3\n4\n5\n").unwrap();
        let git_dir = root.join(".git");
        std::thread::sleep(Duration::from_millis(50));
        let before = snapshot(&git_dir);

        let list = list_stashes(&root).unwrap();
        let files = stash_files(&root, &list[0].hash).unwrap();
        for file in &files {
            stash_file_diff(&root, file).unwrap();
        }
        local_changes_conflict_with(&root, "other").unwrap();
        assert_eq!(stash_count(&root).unwrap(), 1);

        assert_eq!(
            snapshot(&git_dir),
            before,
            "只读查询不许改动 .git 下任何文件"
        );
        cleanup(&[&root]);
    }
}
