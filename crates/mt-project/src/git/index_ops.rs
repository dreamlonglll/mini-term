use super::*;

// ---------------------------------------------------------------------------
// 暂存 / 取消暂存(整文件)
//
// 一律走 git CLI:
// - 暂存要过 clean 过滤器(git-lfs、`.gitattributes` 里的 `filter=`),libgit2 不跑外部
//   过滤驱动,LFS 文件会以原始大文件入库;
// - 取消暂存用**带路径**的 `git reset -- <paths>`:不带路径的 reset(libgit2 的
//   `reset(HEAD, Mixed)` 也一样)会顺手清掉 MERGE_HEAD / rebase-merge / sequencer,
//   解决完冲突点一下「全部取消暂存」,合并提交就静默成了单亲、变基现场被毁;
// - 路径一律 `--literal-pathspecs` + `--`:文件名里的 `*` `?` `[ab]` 不许被当通配
//   (libgit2 的 `reset_default` 就把它们当 glob),`-` 开头的不许被当选项;
// - 一批超过 [`PATHS_PER_CALL`] 条先试逐路径直写暂存区的快路径(`bulk_index`):pathspec
//   是「条目数 × 路径数」,2 万条要近 10s。快路径拿不准的整批照旧走 pathspec。
// ---------------------------------------------------------------------------

/// 暂存类操作的超时:本身毫秒级,但 clean 过滤器(git-lfs 给大文件算哈希)可能很慢,
/// 与改写工作区的操作同档。
pub(super) const INDEX_OP_TIMEOUT: Duration = WORKTREE_OP_TIMEOUT;

/// 一次带太多路径会撞上 Windows 命令行 32K 的上限,分批跑(见 [`run_literal_paths`])。
pub(super) const PATHS_PER_CALL: usize = 64;

/// `git --literal-pathspecs <head_args> -- <files>`,按批跑。
pub(super) fn run_with_literal_paths(
    repo_path: &Path,
    head_args: &[&str],
    files: &[String],
    op_label: &str,
) -> Result<()> {
    let files: Vec<&str> = files.iter().map(String::as_str).collect();
    run_literal_paths(repo_path, head_args, &files, op_label, INDEX_OP_TIMEOUT)
}

/// `git --literal-pathspecs <head_args> -- <files>`。暂存 / 取消暂存 / 丢弃共用。
///
/// 一批装得下([`PATHS_PER_CALL`] 以内)就照常带在命令行上。再多的话,旧做法是每 64 个
/// 路径起一个进程(1000 个文件 16 个进程、约 0.7s),现在改从 stdin 喂一次跑完
/// (`--pathspec-from-file=- --pathspec-file-nul`,git ≥ 2.25;NUL 分隔、逐字读入,
/// `--literal-pathspecs` 照样管着它们);git 太老不认这个选项(用法错误,退出码 129,
/// 那时什么都还没做)才退回按批起进程。
pub(super) fn run_literal_paths(
    repo_path: &Path,
    head_args: &[&str],
    files: &[&str],
    op_label: &str,
    timeout: Duration,
) -> Result<()> {
    if files.len() > PATHS_PER_CALL
        && run_paths_via_stdin(repo_path, head_args, files, op_label, timeout)?
    {
        return Ok(());
    }
    for chunk in files.chunks(PATHS_PER_CALL) {
        let mut args = vec!["--literal-pathspecs"];
        args.extend_from_slice(head_args);
        args.push("--");
        args.extend_from_slice(chunk);
        run_git_command_labeled(repo_path, &args, op_label, timeout, "")?;
    }
    Ok(())
}

/// 路径从 stdin 喂给 git 跑一次。`Ok(false)` = 这个 git 不认 `--pathspec-from-file`
/// (调用方退回按批);其余失败是 `Err`(git 原文)。
pub(super) fn run_paths_via_stdin(
    repo_path: &Path,
    head_args: &[&str],
    files: &[&str],
    op_label: &str,
    timeout: Duration,
) -> Result<bool> {
    let mut args = vec!["--literal-pathspecs"];
    args.extend_from_slice(head_args);
    args.extend(["--pathspec-from-file=-", "--pathspec-file-nul"]);
    let input = nul_separated(files);
    ensure_repo_dir(repo_path)?;
    let out = spawn_git_with_input(repo_path, &args, &[], op_label, timeout, "", Some(input))?;
    if out.success {
        return Ok(true);
    }
    if out.code == Some(GIT_USAGE_ERROR) {
        return Ok(false);
    }
    bail!("{}", failure_text(&out));
}

/// 路径逐条接上 NUL,喂给 `-z` / `--pathspec-file-nul` 一类从 stdin 读路径的 git 命令。
pub(super) fn nul_separated<S: AsRef<str>>(paths: &[S]) -> Vec<u8> {
    let mut input = Vec::with_capacity(paths.iter().map(|p| p.as_ref().len() + 1).sum());
    for path in paths {
        input.extend_from_slice(path.as_ref().as_bytes());
        input.push(0);
    }
    input
}

/// 暂存 / 取消暂存实际走了哪条路(单测据此确认分派)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IndexRoute {
    /// `--literal-pathspecs` 的 `add -A` / `reset`([`run_literal_paths`])
    Pathspec,
    /// 逐路径直写暂存区(`bulk_index`)
    Bulk,
}

/// 还没有任何提交(HEAD 指向未出生的分支)。这时没有 HEAD 可以 reset 回去,
/// 取消暂存 = 从暂存区摘掉。
pub(super) fn is_unborn(repo_path: &Path) -> Result<bool> {
    Ok(Repository::open(repo_path)?.head().is_err())
}

/// 暂存这些文件(`git add -A -- <paths>`:工作区里已删除的暂存为删除)。
/// **阻塞**(起 git 进程、跑 clean 过滤器),调用方放后台执行器。
pub fn git_stage(repo_path: &Path, files: &[String]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    stage_paths(repo_path, files).map(|_| ())
}

/// [`git_stage`] 的本体。一批装得下([`PATHS_PER_CALL`] 以内)照旧走 pathspec;更多的先试
/// 快路径,它不收(或它的 git 失败、暂存区原样未动)再整批走 pathspec。
pub(super) fn stage_paths(repo_path: &Path, files: &[String]) -> Result<IndexRoute> {
    if files.len() > PATHS_PER_CALL && bulk_index::stage(repo_path, files)? {
        return Ok(IndexRoute::Bulk);
    }
    run_with_literal_paths(repo_path, &["add", "-A"], files, "add")?;
    Ok(IndexRoute::Pathspec)
}

/// 取消暂存这些文件(`git reset -q -- <paths>`,只动暂存区、不碰仓库状态)。
///
/// 已暂存的重命名连旧路径一起取消(见 [`with_rename_sources`]):结果与对两条路径各
/// `git restore --staged` 一样 —— 旧路径回到「工作区已删除」、新路径回到「未跟踪」。
pub fn git_unstage(repo_path: &Path, files: &[String]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    if is_unborn(repo_path)? {
        let repo = Repository::open(repo_path)?;
        let mut index = repo.index()?;
        for file in files {
            index.remove_path(Path::new(file))?;
        }
        index.write()?;
        return Ok(());
    }
    let files = with_rename_sources(repo_path, files)?;
    unstage_paths(repo_path, &files).map(|_| ())
}

/// [`git_unstage`] 在 HEAD 已出生时的本体,分派口径同 [`stage_paths`]。
pub(super) fn unstage_paths(repo_path: &Path, files: &[String]) -> Result<IndexRoute> {
    if files.len() > PATHS_PER_CALL && bulk_index::unstage(repo_path, files)? {
        return Ok(IndexRoute::Bulk);
    }
    run_with_literal_paths(repo_path, &["reset", "-q"], files, "reset")?;
    Ok(IndexRoute::Pathspec)
}

/// `files` 里已暂存重命名的新路径,补上各自的旧路径(列表里已有的不重复)。
///
/// 只 reset 新路径的话,旧路径那条「已暂存删除」还挂在暂存区,一行 R 就裂成
/// 「已暂存 D + 未跟踪」。只比 HEAD 与暂存区(`StatusShow::Index`),不扫工作区;重命名
/// 配对与「更改」区同一套(有护栏,见 [`pair_staged_renames`])。
pub(super) fn with_rename_sources(repo_path: &Path, files: &[String]) -> Result<Vec<String>> {
    let repo = Repository::open(repo_path)?;
    let entries = raw_entries(&repo, git2::StatusShow::Index)?;
    let wanted: HashSet<&str> = files.iter().map(String::as_str).collect();
    let mut out = files.to_vec();
    for entry in entries {
        if let Some(old) = entry.renamed_from
            && wanted.contains(entry.path.as_str())
            && !out.contains(&old)
        {
            out.push(old);
        }
    }
    Ok(out)
}

/// 暂存全部改动(`git add -A`,含未跟踪文件与删除)。
pub fn git_stage_all(repo_path: &Path) -> Result<()> {
    run_git_command(repo_path, &["add", "-A"], INDEX_OP_TIMEOUT, "").map(|_| ())
}

/// 取消暂存全部(`git reset -q -- .`):带路径的 reset 只把暂存区对回 HEAD,
/// 进行中的合并 / 变基 / 拣选 / 还原原样保留。
pub fn git_unstage_all(repo_path: &Path) -> Result<()> {
    if is_unborn(repo_path)? {
        let repo = Repository::open(repo_path)?;
        let mut index = repo.index()?;
        index.clear()?;
        index.write()?;
        return Ok(());
    }
    // 在仓库根上跑(执行器要求 repo_path 下有 .git),`.` 就是整个工作区
    run_git_command(
        repo_path,
        &["--literal-pathspecs", "reset", "-q", "--", "."],
        INDEX_OP_TIMEOUT,
        "",
    )
    .map(|_| ())
}

/// 走 git CLI 而非 git2:提交要继承用户的 hooks / gpg 签名 / user.name 配置。
///
/// **阻塞最多 [`LOCAL_OP_TIMEOUT`]**(会同步跑 pre-commit hook),必须在后台线程上调用。
pub fn git_commit(repo_path: &Path, message: &str) -> Result<String> {
    run_git_command_labeled(
        repo_path,
        &["commit", "-m", message],
        "commit",
        LOCAL_OP_TIMEOUT,
        "，可能卡在 pre-commit hook 或 gpg 签名上",
    )
}

pub fn git_discard_file(repo_path: &Path, files: &[String]) -> Result<()> {
    let repo = Repository::open(repo_path)?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow!("bare repo"))?
        .to_path_buf();

    for file in files {
        let abs_path = workdir.join(file);

        // 检查是否 untracked (WT_NEW)
        // 注意:StatusOptions::new() 默认不含未跟踪文件,必须显式开 include_untracked,
        // 否则新增文件永远查不到 WT_NEW,会被误当作已跟踪文件走 checkout_head(对其无效),
        // 表现为「丢弃新增文件没有反应」。
        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .pathspec(file);
        let statuses = repo.statuses(Some(&mut opts))?;
        let is_untracked = statuses.iter().any(|e| e.status().contains(Status::WT_NEW));

        if is_untracked {
            // untracked: 直接删除文件
            if abs_path.exists() {
                std::fs::remove_file(&abs_path)?;
            }
        } else {
            // tracked: 先 unstage（如果在暂存区），再 checkout HEAD 版本
            let head = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
            if let Some(ref commit) = head {
                // unstage
                let _ = repo.reset_default(Some(commit.as_object()), [file.as_str()]);
            }
            // checkout from HEAD
            repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force().path(file)))?;
        }
    }
    Ok(())
}
