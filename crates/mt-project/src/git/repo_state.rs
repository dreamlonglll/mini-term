//! 仓库级状态:进行中的合并 / 变基 / 拣选 / 还原(含继续与中止),以及 Git 面板
//! 自动刷新要用的 `.git` 目录布局、事件过滤与状态签名。
//!
//! # 进行中的操作
//!
//! 判据是 `repo.state()`(看 `MERGE_HEAD` / `rebase-merge/` / `CHERRY_PICK_HEAD` /
//! `REVERT_HEAD` / `sequencer/` 这些文件在不在),**不**是哪条命令的输出 —— 冲突可能
//! 来自本面板的合并、另一条线的拣选,也可能是用户在终端里自己敲的 `git rebase`,
//! 横幅都得认得出来。
//!
//! # 自动刷新的「防自激」
//!
//! 面板监听 `.git` 目录,变化后去抖刷新;刷新本身只读(git2 的 status 不带
//! `UPDATE_INDEX` 就不回写索引,单测钉住了这一条)。但别的程序(IDE 的 git 插件、
//! 终端里的 `git status`)会顺手刷新索引里的 stat 缓存 —— 索引文件变了、内容没变。
//! 所以刷新前先算一个 [`repo_signature`]:HEAD、全部引用、贮藏条数、仓库状态、索引条目
//! (路径 + blob + 暂存号)的摘要,没变就不刷。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use git2::{Repository, RepositoryState};

use super::stash::stash_entries;
use super::{NO_EDITOR, WORKTREE_OP_TIMEOUT, failure_text, run_git_output_env};

/// 进行中的操作种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepoOpKind {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl RepoOpKind {
    /// git 子命令名(`merge --abort` / `rebase --continue` 用)。
    fn command(self) -> &'static str {
        match self {
            RepoOpKind::Merge => "merge",
            RepoOpKind::Rebase => "rebase",
            RepoOpKind::CherryPick => "cherry-pick",
            RepoOpKind::Revert => "revert",
        }
    }
}

/// 仓库此刻的「进行中操作」快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RepoOpState {
    /// `None` = 干净(没有进行中的合并 / 变基 / 拣选 / 还原)。
    pub kind: Option<RepoOpKind>,
    /// 索引里还挂着冲突的文件数。
    pub conflicted: usize,
}

/// libgit2 的仓库状态 → 横幅关心的四种。bisect / `git am` 不归这条横幅管。
pub fn op_kind_of(state: RepositoryState) -> Option<RepoOpKind> {
    match state {
        RepositoryState::Merge => Some(RepoOpKind::Merge),
        RepositoryState::Rebase
        | RepositoryState::RebaseInteractive
        | RepositoryState::RebaseMerge
        | RepositoryState::ApplyMailboxOrRebase => Some(RepoOpKind::Rebase),
        RepositoryState::CherryPick | RepositoryState::CherryPickSequence => {
            Some(RepoOpKind::CherryPick)
        }
        RepositoryState::Revert | RepositoryState::RevertSequence => Some(RepoOpKind::Revert),
        RepositoryState::Clean | RepositoryState::Bisect | RepositoryState::ApplyMailbox => None,
    }
}

/// 索引里挂着冲突的文件数(一个路径的 base/ours/theirs 三条算一个)。
pub fn conflicted_file_count(repo: &Repository) -> usize {
    repo.index()
        .ok()
        .and_then(|index| index.conflicts().ok().map(|c| c.count()))
        .unwrap_or(0)
}

/// 读仓库的进行中操作与冲突数。纯 git2 只读。
pub fn repo_operation_state(repo_path: &Path) -> Result<RepoOpState> {
    let repo = Repository::open(repo_path)?;
    Ok(RepoOpState {
        kind: op_kind_of(repo.state()),
        conflicted: conflicted_file_count(&repo),
    })
}

/// 中止进行中的操作(`git <cmd> --abort`),工作区回到操作开始之前。
/// 已做的冲突解决会丢,UI 侧先确认。
pub fn abort_operation(repo_path: &Path, kind: RepoOpKind) -> Result<()> {
    let out = run_git_output_env(
        repo_path,
        &[kind.command(), "--abort"],
        &[],
        "abort",
        WORKTREE_OP_TIMEOUT,
        "",
    )?;
    if !out.success {
        bail!("{}", failure_text(&out));
    }
    Ok(())
}

/// 继续进行中的操作,返回继续之后的状态(变基 / 拣选序列可能在下一个提交上又停住)。
///
/// - 合并:`git commit --no-edit`(沿用 git 生成的 `MERGE_MSG`);
/// - 变基 / 拣选 / 还原:`git <cmd> --continue`,带 `GIT_EDITOR=true` 不拉编辑器。
///
/// 还有冲突时直接拒绝(git 也会拒,但这里给的原因更明确)。续跑后又停在新的冲突上
/// 不算失败,返回 `Ok`(状态里带着新的冲突数);其余非零退出是 `Err`(git 原文)。
pub fn continue_operation(repo_path: &Path, kind: RepoOpKind) -> Result<RepoOpState> {
    let before = repo_operation_state(repo_path)?;
    if before.conflicted > 0 {
        bail!(
            "仍有 {} 个冲突文件未解决(解决后请先暂存)",
            before.conflicted
        );
    }
    let args: &[&str] = match kind {
        RepoOpKind::Merge => &["commit", "--no-edit"],
        _ => &[kind.command(), "--continue"],
    };
    let out = run_git_output_env(
        repo_path,
        args,
        NO_EDITOR,
        "continue",
        WORKTREE_OP_TIMEOUT,
        "",
    )?;
    let after = repo_operation_state(repo_path)?;
    if !out.success && !(after.kind.is_some() && after.conflicted > 0) {
        bail!("{}", failure_text(&out));
    }
    Ok(after)
}

// ---------------------------------------------------------------------------
// `.git` 目录监听:布局 / 事件过滤 / 签名
// ---------------------------------------------------------------------------

/// 仓库的 git 目录布局。普通仓库两者相同(`<work>/.git`);linked worktree 的
/// `git_dir` 是 `<main>/.git/worktrees/<name>`(HEAD / index / MERGE_HEAD 在这),
/// `common_dir` 是主仓库的 `.git`(refs / packed-refs 在这)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDirLayout {
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
}

/// 求仓库的 git 目录布局。`commondir` 文件是 linked worktree 才有的(内容通常是
/// 相对路径 `../..`),git2 0.19 没暴露 commondir,自己读。
pub fn git_dir_layout(repo_path: &Path) -> Result<GitDirLayout> {
    let repo = Repository::open(repo_path)?;
    let git_dir = trim_dir(repo.path());
    let common_dir = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) if !text.trim().is_empty() => {
            let raw = PathBuf::from(text.trim());
            let joined = if raw.is_absolute() {
                raw
            } else {
                git_dir.join(raw)
            };
            // 相对路径里的 `..` 要解掉:监听的 key 与事件路径都按字面比
            std::fs::canonicalize(&joined)
                .map(crate::fs::strip_verbatim_prefix)
                .unwrap_or(joined)
        }
        _ => git_dir.clone(),
    };
    Ok(GitDirLayout {
        git_dir,
        common_dir,
    })
}

/// libgit2 给的目录带尾部分隔符(`D:/x/.git/`),去掉以免与拼出来的路径对不上。
fn trim_dir(path: &Path) -> PathBuf {
    PathBuf::from(mt_core::path_key::trim_trailing_separators(
        &path.to_string_lossy(),
    ))
}

/// 一次最多监听多少个目录(`refs/` 下按分支前缀分了上百个子目录的仓库也不至于
/// 把监听器塞爆)。超出的子目录里的变化收不到,手动 ↻ 仍可兜底。
pub const GIT_WATCH_DIR_LIMIT: usize = 256;

/// 贮藏的 reflog(`<common>/logs/refs/stash`)。删 `stash@{n>0}` 只改它、不动
/// `refs/stash`,监听要单认这一个文件。
fn stash_reflog(layout: &GitDirLayout) -> PathBuf {
    layout.common_dir.join("logs").join("refs").join("stash")
}

/// 要挂监听的目录(监听器是非递归的,`refs/` 要逐层列出来)。
///
/// git_dir 与 common_dir 各一条;`refs/` 下全部子目录(heads / remotes/<r> / tags,
/// 以及 `feature/` 这类分支前缀目录)。`objects/`、`hooks/` 不挂 —— 那是噪声;`logs/`
/// 只挂 `logs/refs` 这一层(为了贮藏的 reflog,见 [`is_relevant_git_change`]),其下
/// 分支的 reflog 照旧不认。
pub fn git_watch_dirs(layout: &GitDirLayout) -> Vec<PathBuf> {
    let mut dirs = vec![layout.git_dir.clone()];
    if layout.common_dir != layout.git_dir {
        dirs.push(layout.common_dir.clone());
    }
    if let Some(logs) = stash_reflog(layout).parent().filter(|dir| dir.is_dir()) {
        dirs.push(logs.to_path_buf());
    }
    let mut stack = vec![layout.common_dir.join("refs")];
    while let Some(dir) = stack.pop() {
        if dirs.len() >= GIT_WATCH_DIR_LIMIT {
            break;
        }
        if !dir.is_dir() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut children: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect();
            children.sort();
            stack.extend(children.into_iter().rev());
        }
        dirs.push(dir);
    }
    dirs
}

/// git 目录(及 common 目录)下**直接**子项里,变化值得刷新的那几个名字。
const RELEVANT_TOP_LEVEL: &[&str] = &[
    "HEAD",
    "index",
    "packed-refs",
    "FETCH_HEAD",
    "ORIG_HEAD",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "REBASE_HEAD",
    "rebase-merge",
    "rebase-apply",
    "sequencer",
    "refs",
];

/// 一条文件变化值不值得刷新面板。
///
/// - `*.lock` 一律忽略(git 写文件的中间态,随后的改名才是真变化);
/// - `refs/` 下的任何变化都算;
/// - `logs/` 下只认贮藏的 reflog(`logs/refs/stash`):删 `stash@{n>0}` 时只有它变,
///   分支的 reflog 总与 `refs/` 一起变,不必再认;
/// - git 目录 / common 目录的直接子项只认 [`RELEVANT_TOP_LEVEL`] 那几个 ——
///   Windows 的非递归监听在子目录内容变化时还会报一条「子目录被修改」,
///   `objects` / `logs` 这些就是这么混进来的。
pub fn is_relevant_git_change(layout: &GitDirLayout, path: &Path) -> bool {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy()) else {
        return false;
    };
    if name.ends_with(".lock") {
        return false;
    }
    if path == stash_reflog(layout) {
        return true;
    }
    [&layout.common_dir, &layout.git_dir].iter().any(|base| {
        let Ok(rel) = path.strip_prefix(base) else {
            return false;
        };
        let mut parts = rel.components();
        let first = parts.next().map(|c| c.as_os_str().to_string_lossy());
        match first {
            Some(first) if first == "refs" => true,
            Some(first) => parts.next().is_none() && RELEVANT_TOP_LEVEL.contains(&first.as_ref()),
            None => false,
        }
    })
}

/// 仓库「面板可见状态」的摘要,分两半:
///
/// - `refs`:HEAD、全部引用、贮藏条数、进行中的操作 —— 变了就要重载分支、计数与提交历史;
/// - `index`:索引条目(路径 + blob + 暂存号,**不含** stat 缓存)—— 只变它的话
///   (终端里 `git add`、解决了一个冲突)只需重取「更改」区与冲突数,历史不必重载。
///
/// 两半都没变 = 面板没什么可刷的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoSignature {
    pub refs: u64,
    pub index: u64,
}

/// 算 [`RepoSignature`]。纯 git2 只读。
pub fn repo_signature(repo_path: &Path) -> Result<RepoSignature> {
    let repo = Repository::open(repo_path)?;
    let mut hasher = DefaultHasher::new();
    match repo.head() {
        Ok(head) => {
            head.name().hash(&mut hasher);
            head.target().map(|o| o.to_string()).hash(&mut hasher);
        }
        Err(err) => (err.code() as i32).hash(&mut hasher),
    }
    (repo.state() as i32).hash(&mut hasher);
    let mut refs: Vec<(String, String)> = Vec::new();
    for reference in repo.references()?.flatten() {
        let name = String::from_utf8_lossy(reference.name_bytes()).into_owned();
        let target = match reference.target() {
            Some(oid) => oid.to_string(),
            None => reference.symbolic_target().unwrap_or_default().to_string(),
        };
        refs.push((name, target));
    }
    refs.sort();
    refs.hash(&mut hasher);
    // 删 `stash@{n>0}` 时 `refs/stash` 不动、只少一条 reflog:贮藏条数单独算进来
    stash_entries(&repo).hash(&mut hasher);
    let refs = hasher.finish();

    let mut hasher = DefaultHasher::new();
    if let Ok(index) = repo.index() {
        for entry in index.iter() {
            entry.path.hash(&mut hasher);
            entry.id.as_bytes().hash(&mut hasher);
            // flags 的高位里有暂存号(冲突的 1/2/3);低位是路径长度,一并算进去无妨
            entry.flags.hash(&mut hasher);
        }
    }
    Ok(RepoSignature {
        refs,
        index: hasher.finish(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, SystemTime};

    use super::super::test_support::{
        cleanup, commit_file, current, git, init_repo_with_file, repo_with_origin,
    };
    use super::*;

    /// 造一个停在合并冲突上的仓库。
    fn merge_conflict_repo(tag: &str) -> PathBuf {
        let root = init_repo_with_file(tag, "a.txt", "a\n");
        let main = current(&root);
        git(&root, &["checkout", "-q", "-b", "other"]);
        commit_file(&root, "a.txt", "theirs\n", "theirs");
        git(&root, &["checkout", "-q", &main]);
        commit_file(&root, "a.txt", "ours\n", "ours");
        assert_eq!(
            super::super::merge_branch(&root, "other").unwrap(),
            super::super::MergeOutcome::Conflicts { files: 1 }
        );
        root
    }

    /// 合并冲突:检测到 Merge + 1 个冲突;有冲突时「继续」被拒;中止后回到干净。
    #[test]
    fn 检测与中止合并() {
        let root = merge_conflict_repo("state-merge");
        assert_eq!(
            repo_operation_state(&root).unwrap(),
            RepoOpState {
                kind: Some(RepoOpKind::Merge),
                conflicted: 1
            }
        );
        assert!(
            continue_operation(&root, RepoOpKind::Merge).is_err(),
            "有冲突不许继续"
        );
        abort_operation(&root, RepoOpKind::Merge).unwrap();
        assert_eq!(repo_operation_state(&root).unwrap(), RepoOpState::default());
        // 换行按机器的 autocrlf 检出,比内容前归一
        let content = std::fs::read_to_string(root.join("a.txt")).unwrap();
        assert_eq!(content.replace("\r\n", "\n"), "ours\n");
        cleanup(&[&root]);
    }

    /// 解决冲突并暂存后「继续」生成合并提交,仓库回到干净。
    #[test]
    fn 解决后继续合并() {
        let root = merge_conflict_repo("state-continue");
        std::fs::write(root.join("a.txt"), "resolved\n").unwrap();
        git(&root, &["add", "a.txt"]);
        let after = continue_operation(&root, RepoOpKind::Merge).unwrap();
        assert_eq!(after, RepoOpState::default());
        let repo = Repository::open(&root).unwrap();
        assert_eq!(
            repo.head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .parent_count(),
            2
        );
        cleanup(&[&root]);
    }

    /// 变基冲突:解决后 `--continue`(不拉编辑器)走完;中止路径另测一遍。
    #[test]
    fn 变基冲突继续与中止() {
        let root = init_repo_with_file("state-rebase", "a.txt", "a\n");
        let main = current(&root);
        git(&root, &["checkout", "-q", "-b", "topic"]);
        commit_file(&root, "a.txt", "topic\n", "topic");
        git(&root, &["checkout", "-q", &main]);
        commit_file(&root, "a.txt", "main\n", "main");
        git(&root, &["checkout", "-q", "topic"]);
        git(&root, &["branch", "topic-backup"]);

        assert!(matches!(
            super::super::rebase_onto(&root, &main).unwrap(),
            super::super::RebaseOutcome::Conflicts { files: 1 }
        ));
        let state = repo_operation_state(&root).unwrap();
        assert_eq!(state.kind, Some(RepoOpKind::Rebase));
        std::fs::write(root.join("a.txt"), "both\n").unwrap();
        git(&root, &["add", "a.txt"]);
        let after = continue_operation(&root, RepoOpKind::Rebase).unwrap();
        assert_eq!(after, RepoOpState::default(), "续跑应直接走完");
        assert_eq!(current(&root), "topic");

        // 再造一次冲突,这回中止
        git(&root, &["checkout", "-q", "-B", "topic2", "topic-backup"]);
        assert!(matches!(
            super::super::rebase_onto(&root, &main).unwrap(),
            super::super::RebaseOutcome::Conflicts { .. }
        ));
        abort_operation(&root, RepoOpKind::Rebase).unwrap();
        assert_eq!(repo_operation_state(&root).unwrap(), RepoOpState::default());
        assert_eq!(current(&root), "topic2");
        cleanup(&[&root]);
    }

    /// 拣选冲突也认得出来(另一条线的 cherry-pick 靠这条横幅收口)。
    #[test]
    fn 检测拣选冲突() {
        let root = init_repo_with_file("state-pick", "a.txt", "a\n");
        let main = current(&root);
        git(&root, &["checkout", "-q", "-b", "other"]);
        commit_file(&root, "a.txt", "theirs\n", "theirs");
        let pick = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["checkout", "-q", &main]);
        commit_file(&root, "a.txt", "ours\n", "ours");
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@e",
                "cherry-pick",
                &pick,
            ])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(!out.status.success(), "应当冲突");
        assert_eq!(
            repo_operation_state(&root).unwrap(),
            RepoOpState {
                kind: Some(RepoOpKind::CherryPick),
                conflicted: 1
            }
        );
        abort_operation(&root, RepoOpKind::CherryPick).unwrap();
        assert_eq!(repo_operation_state(&root).unwrap().kind, None);
        cleanup(&[&root]);
    }

    /// 普通仓库:git_dir 与 common_dir 相同;linked worktree:common_dir 回到主仓库。
    #[test]
    fn git_目录布局含_worktree() {
        let root = init_repo_with_file("state-layout", "a.txt", "a\n");
        let layout = git_dir_layout(&root).unwrap();
        assert_eq!(layout.git_dir, layout.common_dir);
        assert!(layout.git_dir.ends_with(".git"), "{layout:?}");

        let wt = root.with_file_name(format!(
            "{}-wt",
            root.file_name().unwrap().to_string_lossy()
        ));
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "wt-branch",
                &wt.to_string_lossy(),
            ],
        );
        let wt_layout = git_dir_layout(&wt).unwrap();
        assert_ne!(wt_layout.git_dir, wt_layout.common_dir);
        assert!(wt_layout.git_dir.join("HEAD").is_file());
        assert!(wt_layout.common_dir.join("refs").is_dir());
        assert_eq!(
            std::fs::canonicalize(&wt_layout.common_dir).unwrap(),
            std::fs::canonicalize(&layout.common_dir).unwrap()
        );

        // 监听目录:两个 git 目录 + refs 下逐层(含 feature/ 前缀目录)
        git(&root, &["branch", "feature/deep"]);
        let dirs = git_watch_dirs(&wt_layout);
        assert!(dirs.contains(&wt_layout.git_dir));
        assert!(dirs.contains(&wt_layout.common_dir));
        assert!(dirs.contains(&wt_layout.common_dir.join("refs").join("heads")));
        assert!(
            dirs.contains(
                &wt_layout
                    .common_dir
                    .join("refs")
                    .join("heads")
                    .join("feature")
            ),
            "{dirs:?}"
        );
        assert!(!dirs.iter().any(|d| d.ends_with("objects")));
        // logs 只挂 `logs/refs` 这一层(贮藏的 reflog 在它下面),分支 reflog 的子目录不挂
        let logs = wt_layout.common_dir.join("logs").join("refs");
        assert!(dirs.contains(&logs), "{dirs:?}");
        assert!(!dirs.contains(&logs.join("heads")));
        cleanup(&[&wt, &root]);
    }

    /// 事件过滤:refs 下全收、顶层只认那几个名字、`*.lock` 与 objects / logs 噪声全丢。
    #[test]
    fn 事件过滤() {
        let layout = GitDirLayout {
            git_dir: PathBuf::from("/r/.git/worktrees/w"),
            common_dir: PathBuf::from("/r/.git"),
        };
        let yes = [
            "/r/.git/worktrees/w/HEAD",
            "/r/.git/worktrees/w/index",
            "/r/.git/worktrees/w/MERGE_HEAD",
            "/r/.git/worktrees/w/rebase-merge",
            "/r/.git/packed-refs",
            "/r/.git/FETCH_HEAD",
            "/r/.git/refs/heads/main",
            "/r/.git/refs/remotes/origin/feature/x",
            "/r/.git/refs",
            "/r/.git/logs/refs/stash",
        ];
        for p in yes {
            assert!(is_relevant_git_change(&layout, Path::new(p)), "{p}");
        }
        let no = [
            "/r/.git/worktrees/w/index.lock",
            "/r/.git/refs/heads/main.lock",
            "/r/.git/objects",
            "/r/.git/logs",
            "/r/.git/worktrees/w/logs",
            "/r/.git/logs/refs/heads",
            "/r/.git/logs/refs/heads/main",
            "/r/.git/logs/refs/stash.lock",
            "/r/.git/logs/HEAD",
            "/r/.git/COMMIT_EDITMSG",
            "/r/.git/objects/ab/cdef",
            "/elsewhere/HEAD",
        ];
        for p in no {
            assert!(!is_relevant_git_change(&layout, Path::new(p)), "{p}");
        }
    }

    /// 签名:暂存 / 提交 / 建分支都会变;只刷新索引 stat 缓存(内容不变)不变。
    #[test]
    fn 签名只随可见状态变化() {
        let root = init_repo_with_file("state-sig", "a.txt", "a\n");
        let s0 = repo_signature(&root).unwrap();
        assert_eq!(repo_signature(&root).unwrap(), s0, "同一状态签名稳定");

        // 只让 git 刷新一遍索引的 stat 缓存:索引文件重写了,内容没变
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        git(&root, &["update-index", "--refresh", "-q"]);
        assert_eq!(repo_signature(&root).unwrap(), s0, "stat 刷新不算变化");

        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        assert_eq!(
            repo_signature(&root).unwrap(),
            s0,
            "未暂存的工作区改动不在签名里"
        );
        git(&root, &["add", "a.txt"]);
        let s1 = repo_signature(&root).unwrap();
        assert_ne!(s1.index, s0.index, "暂存改变了索引条目");
        assert_eq!(s1.refs, s0.refs, "暂存不动引用:历史不必重载");
        git(&root, &["commit", "-q", "-m", "c"]);
        let s2 = repo_signature(&root).unwrap();
        assert_ne!(s2.refs, s1.refs, "提交挪了分支头");
        git(&root, &["branch", "x"]);
        let s3 = repo_signature(&root).unwrap();
        assert_ne!(s3.refs, s2.refs, "新建分支");
        assert_eq!(s3.index, s2.index);
        cleanup(&[&root]);
    }

    /// 贮藏:删 `stash@{1}` 时 `refs/stash` 不动、只少一条 reflog —— 签名照样要变,
    /// 仓库栏的贮藏数才跟得上。
    #[test]
    fn 签名随贮藏条数变化() {
        let root = init_repo_with_file("state-sig-stash", "a.txt", "a\n");
        for content in ["1\n", "2\n"] {
            std::fs::write(root.join("a.txt"), content).unwrap();
            git(&root, &["stash", "push", "-q"]);
        }
        let tip = git(&root, &["rev-parse", "refs/stash"]);
        let s0 = repo_signature(&root).unwrap();
        git(&root, &["stash", "drop", "-q", "stash@{1}"]);
        assert_eq!(
            git(&root, &["rev-parse", "refs/stash"]),
            tip,
            "refs/stash 没动"
        );
        let s1 = repo_signature(&root).unwrap();
        assert_ne!(s1.refs, s0.refs, "少了一条贮藏");
        assert_eq!(s1.index, s0.index);
        cleanup(&[&root]);
    }

    /// `.git` 下(objects 除外)每个文件的 (修改时间, 长度)。
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

    /// **防自激的前提**:面板一轮刷新里调的全部只读函数都不往 `.git` 里写任何东西
    /// (包括「工作区改了但 stat 过期」时 —— 那正是会诱发索引回写的情形)。
    #[test]
    fn 刷新链路不写_git_目录() {
        let (work, bare, _) = repo_with_origin("state-readonly");
        commit_file(&work, "b.txt", "b\n", "ahead");
        // 改一个已跟踪文件(同长度),让索引里的 stat 缓存过期
        std::fs::write(work.join("a.txt"), "z\n").unwrap();
        std::fs::write(work.join("new.txt"), "n\n").unwrap();
        let git_dir = git_dir_layout(&work).unwrap().git_dir;
        std::thread::sleep(Duration::from_millis(50));
        let before = snapshot(&git_dir);

        super::super::get_changes_status(&work).unwrap();
        super::super::get_repo_branches(&work).unwrap();
        super::super::get_git_log(&work, None, Some(30), None).unwrap();
        super::super::discover_git_repos(&work).unwrap();
        super::super::ahead_behind(&work).unwrap();
        super::super::local_branch_tracking(&work).unwrap();
        super::super::recent_branches(&work, 5).unwrap();
        super::super::outgoing_commits(&work, None, 100).unwrap();
        super::super::push_plan(&work, None).unwrap();
        repo_operation_state(&work).unwrap();
        super::super::stash_count(&work).unwrap();
        repo_signature(&work).unwrap();
        git_watch_dirs(&git_dir_layout(&work).unwrap());

        assert_eq!(
            snapshot(&git_dir),
            before,
            "只读刷新不许改动 .git 下任何文件"
        );
        cleanup(&[&work, &bare]);
    }
}
