//! 提交历史右键菜单背后的操作:检出 / 新建分支 / 拣选 / 还原 / 重置 / 撤销提交 /
//! 修改提交消息,以及菜单判定要用的「这个提交与 HEAD、远程的关系」。
//!
//! 改仓库状态的一律走 git CLI(继承用户的 hooks / 签名 / 配置),只读判定走 git2。
//! **结果判定不看 git 的提示文本**(用户的 git 可能是中文界面):只看退出码、
//! `repo.state()`、索引里有没有冲突条目这些结构化信号。
//!
//! 参数防线:提交号只收十六进制([`is_commit_hash`]),分支名先过 libgit2 的校验
//! ([`is_valid_branch_name`]),两者都挡掉 `-` 开头被 git 当成选项的输入。
//!
//! 全部是**阻塞**调用(最长 120s),调用方自己放到后台执行器上跑。

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use git2::{BranchType, Oid, Repository, RepositoryState};

use super::{run_git_command_labeled, run_git_output};

/// 纯本地、但会跑 hook 的操作(拣选 / 还原 / 改消息)的超时。口径同 `git_commit`。
const LOCAL_OP_TIMEOUT: Duration = Duration::from_secs(60);
/// 会改写工作区的操作(检出 / 重置)。大仓库 checkout 可能很慢,与 worktree add 同档。
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(120);

/// `git reset` 的四种模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    /// 只移动分支指针,暂存区与工作区原样 —— 被「撤掉」的提交的改动留在暂存区。
    Soft,
    /// 移动指针 + 重置暂存区,工作区原样 —— 改动回到「未暂存」。
    Mixed,
    /// 三者全部对齐目标提交,**未提交的改动全部丢失**。
    Hard,
    /// 像 Hard,但本地改动与目标冲突时拒绝执行(不丢改动)。
    Keep,
}

impl ResetMode {
    fn flag(self) -> &'static str {
        match self {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
            ResetMode::Keep => "--keep",
        }
    }

    /// 给 UI 拼摘要用的小写名(`soft` / `mixed` …),与 git 的叫法一致。
    pub fn name(self) -> &'static str {
        &self.flag()[2..]
    }
}

/// 拣选 / 还原的结果。冲突**不是错误**:仓库停在进行中状态,等用户在「更改」区解决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickOutcome {
    /// 已生成新提交;`new_head` 是之后的 HEAD。
    Done { new_head: String },
    /// 出现冲突,仓库停在 cherry-pick / revert 进行中。
    Conflicts,
}

/// 一个提交与 HEAD / 远程的关系。右键菜单据此决定哪些项可用。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommitRelation {
    /// 就是 HEAD 本身。
    pub is_head: bool,
    /// HEAD 能走到它(已在当前分支历史上;HEAD 自己也算)。
    pub in_head_history: bool,
    /// 已被当前分支的上游、或任一远程跟踪分支包含(改写它需要强制推送)。
    pub pushed: bool,
    /// 父提交个数(0 = 根提交,≥2 = 合并提交)。
    pub parent_count: usize,
    /// HEAD 所在的本地分支;detached / 空仓库为 `None`。
    pub head_branch: Option<String>,
    /// 仓库当前是不是停在合并 / 拣选 / 变基这类进行中状态(此时改写操作一律不给)。
    pub busy_state: bool,
}

/// 提交号形态校验:4~64 位十六进制(短哈希到 SHA-256 全长)。挡住 `-` 开头被当选项、
/// 以及 `HEAD~1` / 分支名这类不是提交号的东西混进 CLI 参数。
pub fn is_commit_hash(s: &str) -> bool {
    (4..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 分支名是否合法(libgit2 的 `git_branch_name_is_valid`:拒绝 `-` 开头、`HEAD`、
/// 以及 `check-ref-format` 不认的写法)。UI 拿它做即时校验,后端执行前再查一遍。
pub fn is_valid_branch_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('-') && git2::Branch::name_is_valid(name).unwrap_or(false)
}

fn check_hash(hash: &str) -> Result<()> {
    if is_commit_hash(hash) {
        Ok(())
    } else {
        bail!("不是有效的提交哈希:{hash}")
    }
}

/// 把(可能是短的)提交号解析成完整的 commit oid。
fn resolve_commit(repo: &Repository, hash: &str) -> Result<Oid> {
    check_hash(hash)?;
    let object = repo
        .revparse_single(hash)
        .map_err(|_| anyhow!("找不到提交:{hash}"))?;
    Ok(object
        .peel_to_commit()
        .map_err(|_| anyhow!("{hash} 不是提交"))?
        .id())
}

fn head_oid(repo: &Repository) -> Option<Oid> {
    repo.head().ok().and_then(|h| h.target())
}

/// `tip` 包含 `commit`(相等或是它的后代)。
fn contains(repo: &Repository, tip: Oid, commit: Oid) -> bool {
    tip == commit || repo.graph_descendant_of(tip, commit).unwrap_or(false)
}

/// 仓库是否停在某个进行中的操作上(合并 / 拣选 / 还原 / 变基 / bisect …)。
fn in_progress(repo: &Repository) -> bool {
    repo.state() != RepositoryState::Clean
}

/// 菜单判定:这个提交与 HEAD / 远程的关系。
///
/// 「已推送」= 当前分支的上游包含它;没有上游时退到任一远程跟踪分支包含它
/// (detached HEAD、刚从别处拉来的分支都走这条)。
pub fn commit_relation(repo_path: &Path, hash: &str) -> Result<CommitRelation> {
    let repo = Repository::open(repo_path)?;
    let oid = resolve_commit(&repo, hash)?;
    let commit = repo.find_commit(oid)?;
    let head = head_oid(&repo);
    let head_branch = repo
        .head()
        .ok()
        .filter(|h| h.is_branch())
        .and_then(|h| h.shorthand().map(str::to_string));
    Ok(CommitRelation {
        is_head: head == Some(oid),
        in_head_history: head.is_some_and(|h| contains(&repo, h, oid)),
        pushed: pushed_in(&repo, oid, head_branch.as_deref()),
        parent_count: commit.parent_count(),
        head_branch,
        busy_state: in_progress(&repo),
    })
}

/// 该提交是否已被推送(见 [`commit_relation`] 的口径)。
pub fn is_commit_pushed(repo_path: &Path, hash: &str) -> Result<bool> {
    let repo = Repository::open(repo_path)?;
    let oid = resolve_commit(&repo, hash)?;
    let head_branch = repo
        .head()
        .ok()
        .filter(|h| h.is_branch())
        .and_then(|h| h.shorthand().map(str::to_string));
    Ok(pushed_in(&repo, oid, head_branch.as_deref()))
}

/// 该提交是否在 HEAD 的历史上(HEAD 自己也算)。空仓库恒 `false`。
pub fn is_ancestor_of_head(repo_path: &Path, hash: &str) -> Result<bool> {
    let repo = Repository::open(repo_path)?;
    let oid = resolve_commit(&repo, hash)?;
    Ok(head_oid(&repo).is_some_and(|h| contains(&repo, h, oid)))
}

fn pushed_in(repo: &Repository, oid: Oid, head_branch: Option<&str>) -> bool {
    // 先看当前分支的上游:绝大多数情况一次就有结论
    if let Some(name) = head_branch
        && let Ok(branch) = repo.find_branch(name, BranchType::Local)
        && let Ok(upstream) = branch.upstream()
        && let Some(tip) = upstream.get().target()
        && contains(repo, tip, oid)
    {
        return true;
    }
    let Ok(remotes) = repo.branches(Some(BranchType::Remote)) else {
        return false;
    };
    remotes.flatten().any(|(branch, _)| {
        // `origin/HEAD` 是符号引用,target() 为 None,天然跳过
        branch
            .get()
            .target()
            .is_some_and(|tip| contains(repo, tip, oid))
    })
}

/// 跑一条会改仓库状态的 git 命令,失败时把 stderr 原样交回(工作区脏、hook 拒绝…)。
fn run_mutation(repo_path: &Path, args: &[&str], label: &str, timeout: Duration) -> Result<()> {
    run_git_command_labeled(repo_path, args, label, timeout, "").map(|_| ())
}

/// 检出某个提交(进入分离头指针状态)。工作区有会被覆盖的改动时 git 拒绝执行,
/// 错误里是 git 的原文。**阻塞最多 120s**。
pub fn checkout_commit(repo_path: &Path, hash: &str) -> Result<()> {
    check_hash(hash)?;
    run_mutation(
        repo_path,
        &[
            "-c",
            "advice.detachedHead=false",
            "checkout",
            "--detach",
            hash,
        ],
        "checkout --detach",
        CHECKOUT_TIMEOUT,
    )
}

/// 在某个提交上新建分支;`checkout` 为真时建完顺手切过去(`git checkout -b`)。
/// **阻塞最多 120s**。
pub fn create_branch_at(repo_path: &Path, name: &str, hash: &str, checkout: bool) -> Result<()> {
    if !is_valid_branch_name(name) {
        bail!("不是合法的分支名:{name}");
    }
    check_hash(hash)?;
    if checkout {
        run_mutation(
            repo_path,
            &["checkout", "-b", name, hash],
            "checkout -b",
            CHECKOUT_TIMEOUT,
        )
    } else {
        run_mutation(
            repo_path,
            &["branch", name, hash],
            "branch",
            LOCAL_OP_TIMEOUT,
        )
    }
}

/// 拣选 / 还原的公共实现:合并提交带 `-m 1`(以第一个父提交为主线)。
fn pick_like(repo_path: &Path, op: &'static str, hash: &str) -> Result<PickOutcome> {
    let parents = {
        let repo = Repository::open(repo_path)?;
        if in_progress(&repo) {
            bail!("仓库正处于未完成的合并 / 拣选 / 变基中,请先完成或中止");
        }
        let oid = resolve_commit(&repo, hash)?;
        repo.find_commit(oid)?.parent_count()
    };
    let mut args = vec!["-c", "core.editor=true", op];
    if op == "revert" {
        args.push("--no-edit");
    }
    if parents > 1 {
        args.extend(["-m", "1"]);
    }
    args.push(hash);
    let output = run_git_output(repo_path, &args, op, LOCAL_OP_TIMEOUT, "")?;

    // 结果判定只看结构化信号:退出码 → 仓库状态 → 索引冲突
    let repo = Repository::open(repo_path)?;
    if output.success {
        let new_head = head_oid(&repo).map(|o| o.to_string()).unwrap_or_default();
        return Ok(PickOutcome::Done { new_head });
    }
    if in_progress(&repo) {
        if repo.index().map(|i| i.has_conflicts()).unwrap_or(false) {
            return Ok(PickOutcome::Conflicts);
        }
        // 停在进行中却没有冲突 = 结果是空提交(改动早已在当前分支上)。
        // 不留半截状态给用户:中止回到原样,把 git 的原话交出去
        let _ = run_git_output(repo_path, &[op, "--abort"], op, LOCAL_OP_TIMEOUT, "");
    }
    bail!("{}", output.stderr.trim());
}

/// 拣选一个提交到当前分支。合并提交以第一个父为主线(`-m 1`)。**阻塞最多 60s**。
pub fn cherry_pick(repo_path: &Path, hash: &str) -> Result<PickOutcome> {
    pick_like(repo_path, "cherry-pick", hash)
}

/// 还原一个提交(生成一个反向提交,`--no-edit`)。合并提交以第一个父为主线。
/// **阻塞最多 60s**。
pub fn revert_commit(repo_path: &Path, hash: &str) -> Result<PickOutcome> {
    pick_like(repo_path, "revert", hash)
}

/// 把当前分支(或 detached HEAD)重置到某个提交。**阻塞最多 120s**。
pub fn reset_to(repo_path: &Path, hash: &str, mode: ResetMode) -> Result<()> {
    check_hash(hash)?;
    run_mutation(
        repo_path,
        &["reset", mode.flag(), hash],
        "reset",
        CHECKOUT_TIMEOUT,
    )
}

/// 核对 HEAD 仍是调用方看到的那个提交(菜单弹出到点击之间仓库可能被终端改过)。
fn expect_head<'r>(repo: &'r Repository, expected: &str) -> Result<git2::Commit<'r>> {
    check_hash(expected)?;
    let head = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .ok_or_else(|| anyhow!("当前没有 HEAD 提交"))?;
    if !head
        .id()
        .to_string()
        .starts_with(&expected.to_ascii_lowercase())
    {
        bail!("HEAD 已经变化,请刷新后重试");
    }
    Ok(head)
}

/// 撤销最近一次提交:`reset --soft` 到它的父提交,改动回到暂存区。
/// 只接受「它就是 HEAD、非合并、有父提交」。返回新的 HEAD。**阻塞最多 120s**。
pub fn undo_last_commit(repo_path: &Path, expected_head: &str) -> Result<String> {
    let parent = {
        let repo = Repository::open(repo_path)?;
        if in_progress(&repo) {
            bail!("仓库正处于未完成的合并 / 拣选 / 变基中,请先完成或中止");
        }
        let head = expect_head(&repo, expected_head)?;
        match head.parent_count() {
            0 => bail!("这是仓库的第一个提交,没有可回退的父提交"),
            1 => head.parent_id(0)?.to_string(),
            _ => bail!("合并提交不能撤销"),
        }
    };
    reset_to(repo_path, &parent, ResetMode::Soft)?;
    Ok(parent)
}

/// 修改 HEAD 的提交消息。`--only` 不带路径 = **不把暂存区的改动带进去**,
/// 只换消息;`--allow-empty` 让空提交(只改过消息的那种)也能改。返回新的 HEAD。
/// **阻塞最多 60s**(会跑 commit-msg / pre-commit hook)。
pub fn reword_head(repo_path: &Path, expected_head: &str, message: &str) -> Result<String> {
    if message.trim().is_empty() {
        bail!("提交消息不能为空");
    }
    {
        let repo = Repository::open(repo_path)?;
        if in_progress(&repo) {
            bail!("仓库正处于未完成的合并 / 拣选 / 变基中,请先完成或中止");
        }
        expect_head(&repo, expected_head)?;
    }
    run_git_command_labeled(
        repo_path,
        &[
            "-c",
            "core.editor=true",
            "commit",
            "--amend",
            "--only",
            "--allow-empty",
            "-m",
            message,
        ],
        "commit --amend",
        LOCAL_OP_TIMEOUT,
        ",可能卡在 commit-msg hook 或 gpg 签名上",
    )?;
    let repo = Repository::open(repo_path)?;
    head_oid(&repo)
        .map(|o| o.to_string())
        .ok_or_else(|| anyhow!("修改后找不到 HEAD"))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use git2::{Oid, Repository, RepositoryState};

    use super::super::test_support::init_repo_with_file;
    use super::*;

    /// 在 HEAD 上提交一个文件改动(git2 直接造,快),返回新提交号。
    fn commit_file(root: &Path, rel: &str, content: &str, msg: &str) -> String {
        let repo = Repository::open(root).unwrap();
        std::fs::write(root.join(rel), content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(rel)).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("mini-term", "test@example.com").unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &[&parent])
            .unwrap()
            .to_string()
    }

    fn head(root: &Path) -> String {
        let repo = Repository::open(root).unwrap();
        repo.head().unwrap().target().unwrap().to_string()
    }

    /// 读工作区文件。换行归一:测试机的全局 `core.autocrlf` 会让 CLI 检出成 CRLF。
    fn read(root: &Path, rel: &str) -> String {
        std::fs::read_to_string(root.join(rel))
            .unwrap()
            .replace("\r\n", "\n")
    }

    /// 在当前 HEAD 上开一条分支(不切过去)。
    fn branch(root: &Path, name: &str, at: &str) {
        let repo = Repository::open(root).unwrap();
        let commit = repo.find_commit(Oid::from_str(at).unwrap()).unwrap();
        repo.branch(name, &commit, true).unwrap();
    }

    fn switch(root: &Path, name: &str) {
        let repo = Repository::open(root).unwrap();
        let refname = format!("refs/heads/{name}");
        let obj = repo.revparse_single(&refname).unwrap();
        repo.checkout_tree(&obj, Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        repo.set_head(&refname).unwrap();
    }

    fn cleanup(root: PathBuf) {
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn 提交号与分支名校验() {
        assert!(is_commit_hash("abcd"));
        assert!(is_commit_hash(&"a".repeat(40)));
        assert!(!is_commit_hash("abc"), "太短");
        assert!(!is_commit_hash("-abcd"), "选项形态");
        assert!(!is_commit_hash("HEAD~1"));
        assert!(!is_commit_hash("ghij"));

        assert!(is_valid_branch_name("feature/x-1"));
        assert!(!is_valid_branch_name(""));
        assert!(!is_valid_branch_name("-f"), "`-` 开头会被当成选项");
        assert!(!is_valid_branch_name("HEAD"));
        assert!(!is_valid_branch_name("a..b"));
        assert!(!is_valid_branch_name("with space"));
        assert!(!is_valid_branch_name("end."));
    }

    #[test]
    fn 检出提交进入分离头指针() {
        let root = init_repo_with_file("log-checkout", "a.txt", "1\n");
        let first = head(&root);
        commit_file(&root, "a.txt", "2\n", "second");
        checkout_commit(&root, &first).unwrap();
        let repo = Repository::open(&root).unwrap();
        assert!(repo.head_detached().unwrap());
        assert_eq!(head(&root), first);
        assert_eq!(read(&root, "a.txt"), "1\n");
        // 选项形态的「哈希」在进 CLI 之前就被挡掉
        assert!(checkout_commit(&root, "--orphan").is_err());
        drop(repo);
        cleanup(root);
    }

    #[test]
    fn 检出时工作区冲突给出_git_原文() {
        let root = init_repo_with_file("log-checkout-dirty", "a.txt", "1\n");
        let first = head(&root);
        commit_file(&root, "a.txt", "2\n", "second");
        std::fs::write(root.join("a.txt"), "dirty\n").unwrap();
        let err = checkout_commit(&root, &first).unwrap_err().to_string();
        assert!(!err.trim().is_empty(), "失败要带 git 的 stderr");
        // 没动:还在原分支、改动还在
        assert!(!Repository::open(&root).unwrap().head_detached().unwrap());
        assert_eq!(read(&root, "a.txt"), "dirty\n");
        cleanup(root);
    }

    #[test]
    fn 在提交上建分支_可选检出() {
        let root = init_repo_with_file("log-branch-at", "a.txt", "1\n");
        let first = head(&root);
        let second = commit_file(&root, "a.txt", "2\n", "second");

        create_branch_at(&root, "old", &first, false).unwrap();
        let repo = Repository::open(&root).unwrap();
        let old = repo.find_branch("old", BranchType::Local).unwrap();
        assert_eq!(old.get().target().unwrap().to_string(), first);
        assert_eq!(head(&root), second, "不检出时 HEAD 不动");

        create_branch_at(&root, "topic/x", &first, true).unwrap();
        let repo = Repository::open(&root).unwrap();
        assert_eq!(repo.head().unwrap().shorthand(), Some("topic/x"));
        assert_eq!(read(&root, "a.txt"), "1\n");

        assert!(create_branch_at(&root, "-D", &first, false).is_err());
        assert!(create_branch_at(&root, "a b", &first, false).is_err());
        // 重名:git 拒绝,错误可见
        assert!(create_branch_at(&root, "old", &second, false).is_err());
        cleanup(root);
    }

    #[test]
    fn 拣选_成功与冲突() {
        let root = init_repo_with_file("log-pick", "a.txt", "base\n");
        let base = head(&root);
        let main = Repository::open(&root)
            .unwrap()
            .head()
            .unwrap()
            .shorthand()
            .unwrap()
            .to_string();
        branch(&root, "side", &base);
        switch(&root, "side");
        std::fs::write(root.join("b.txt"), "side\n").unwrap();
        let side_b = commit_file(&root, "b.txt", "side\n", "add b");
        let side_a = commit_file(&root, "a.txt", "side\n", "edit a on side");
        switch(&root, &main);
        commit_file(&root, "a.txt", "main\n", "edit a on main");

        // 不冲突:生成新提交,HEAD 前移
        let before = head(&root);
        match cherry_pick(&root, &side_b).unwrap() {
            PickOutcome::Done { new_head } => {
                assert_ne!(new_head, before);
                assert_eq!(new_head, head(&root));
            }
            other => panic!("应当成功:{other:?}"),
        }
        assert_eq!(read(&root, "b.txt"), "side\n");

        // 冲突:不是错误,仓库停在 cherry-pick 中
        assert_eq!(cherry_pick(&root, &side_a).unwrap(), PickOutcome::Conflicts);
        let repo = Repository::open(&root).unwrap();
        assert_eq!(repo.state(), RepositoryState::CherryPick);
        // 进行中时再拣选直接拒绝
        assert!(cherry_pick(&root, &side_b).is_err());
        drop(repo);
        cleanup(root);
    }

    #[test]
    fn 拣选_结果为空时中止并报错() {
        let root = init_repo_with_file("log-pick-empty", "a.txt", "base\n");
        let base = head(&root);
        let main = Repository::open(&root)
            .unwrap()
            .head()
            .unwrap()
            .shorthand()
            .unwrap()
            .to_string();
        branch(&root, "side", &base);
        switch(&root, "side");
        let side = commit_file(&root, "a.txt", "same\n", "same change on side");
        switch(&root, &main);
        commit_file(&root, "a.txt", "same\n", "same change on main");

        assert!(cherry_pick(&root, &side).is_err());
        let repo = Repository::open(&root).unwrap();
        assert_eq!(repo.state(), RepositoryState::Clean, "不留半截状态");
        drop(repo);
        cleanup(root);
    }

    #[test]
    fn 还原_普通提交与合并提交() {
        let root = init_repo_with_file("log-revert", "a.txt", "1\n");
        let base = head(&root);
        let second = commit_file(&root, "a.txt", "2\n", "second");
        match revert_commit(&root, &second).unwrap() {
            PickOutcome::Done { new_head } => assert_eq!(new_head, head(&root)),
            other => panic!("应当成功:{other:?}"),
        }
        assert_eq!(read(&root, "a.txt"), "1\n");

        // 合并提交:-m 1,撤掉侧枝带进来的改动
        let main = Repository::open(&root)
            .unwrap()
            .head()
            .unwrap()
            .shorthand()
            .unwrap()
            .to_string();
        branch(&root, "side", &base);
        switch(&root, "side");
        std::fs::write(root.join("s.txt"), "s\n").unwrap();
        let side = commit_file(&root, "s.txt", "s\n", "side file");
        switch(&root, &main);
        let merge = {
            let repo = Repository::open(&root).unwrap();
            let ours = repo.head().unwrap().peel_to_commit().unwrap();
            let theirs = repo.find_commit(Oid::from_str(&side).unwrap()).unwrap();
            let mut index = repo.merge_commits(&ours, &theirs, None).unwrap();
            let tree = repo.find_tree(index.write_tree_to(&repo).unwrap()).unwrap();
            let sig = git2::Signature::now("mini-term", "test@example.com").unwrap();
            let oid = repo
                .commit(
                    Some("HEAD"),
                    &sig,
                    &sig,
                    "merge side",
                    &tree,
                    &[&ours, &theirs],
                )
                .unwrap();
            repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
                .unwrap();
            oid.to_string()
        };
        assert!(root.join("s.txt").exists());
        assert!(matches!(
            revert_commit(&root, &merge).unwrap(),
            PickOutcome::Done { .. }
        ));
        assert!(!root.join("s.txt").exists(), "-m 1 还原掉侧枝的文件");
        cleanup(root);
    }

    #[test]
    fn 重置四种模式() {
        let root = init_repo_with_file("log-reset", "a.txt", "1\n");
        let first = head(&root);
        let second = commit_file(&root, "a.txt", "2\n", "second");

        // soft:改动回到暂存区
        reset_to(&root, &first, ResetMode::Soft).unwrap();
        assert_eq!(head(&root), first);
        let repo = Repository::open(&root).unwrap();
        let staged = repo
            .statuses(None)
            .unwrap()
            .iter()
            .any(|e| e.status().contains(git2::Status::INDEX_MODIFIED));
        assert!(staged, "soft 之后改动在暂存区");
        assert_eq!(read(&root, "a.txt"), "2\n");

        // mixed:暂存区重置,工作区保留
        reset_to(&root, &second, ResetMode::Soft).unwrap();
        reset_to(&root, &first, ResetMode::Mixed).unwrap();
        let repo = Repository::open(&root).unwrap();
        let statuses = repo.statuses(None).unwrap();
        let st = statuses.iter().next().unwrap().status();
        assert!(st.contains(git2::Status::WT_MODIFIED));
        assert!(!st.contains(git2::Status::INDEX_MODIFIED));
        drop(statuses);

        // keep:本地改动与目标冲突 → 拒绝,不丢改动
        assert!(reset_to(&root, &second, ResetMode::Keep).is_err());
        assert_eq!(read(&root, "a.txt"), "2\n");

        // hard:全部对齐
        reset_to(&root, &first, ResetMode::Hard).unwrap();
        assert_eq!(read(&root, "a.txt"), "1\n");
        // keep:没有本地改动时与 hard 同效
        reset_to(&root, &second, ResetMode::Keep).unwrap();
        assert_eq!(head(&root), second);
        assert_eq!(read(&root, "a.txt"), "2\n");
        assert_eq!(ResetMode::Keep.name(), "keep");
        drop(repo);
        cleanup(root);
    }

    #[test]
    fn 撤销提交_改动回到暂存区() {
        let root = init_repo_with_file("log-undo", "a.txt", "1\n");
        let first = head(&root);
        let second = commit_file(&root, "a.txt", "2\n", "second");
        // 不是 HEAD 的拒绝
        assert!(undo_last_commit(&root, &first).is_err());
        assert_eq!(undo_last_commit(&root, &second).unwrap(), first);
        assert_eq!(head(&root), first);
        let repo = Repository::open(&root).unwrap();
        let staged = repo
            .statuses(None)
            .unwrap()
            .iter()
            .any(|e| e.status().contains(git2::Status::INDEX_MODIFIED));
        assert!(staged);
        // 根提交没有父
        assert!(undo_last_commit(&root, &first).is_err());
        drop(repo);
        cleanup(root);
    }

    #[test]
    fn 修改消息不带入暂存区改动() {
        let root = init_repo_with_file("log-reword", "a.txt", "1\n");
        let old = commit_file(&root, "a.txt", "2\n", "old message");
        // 暂存一个新改动:改消息时不能被带进提交
        std::fs::write(root.join("a.txt"), "3\n").unwrap();
        {
            let repo = Repository::open(&root).unwrap();
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("a.txt")).unwrap();
            index.write().unwrap();
        }
        let new = reword_head(&root, &old, "new title\n\nnew body").unwrap();
        assert_ne!(new, old);
        let repo = Repository::open(&root).unwrap();
        let commit = repo.find_commit(Oid::from_str(&new).unwrap()).unwrap();
        assert_eq!(commit.summary(), Some("new title"));
        assert_eq!(commit.body(), Some("new body"));
        // 提交里的内容还是 2,暂存区的 3 原样留着
        let blob = commit
            .tree()
            .unwrap()
            .get_path(Path::new("a.txt"))
            .unwrap()
            .to_object(&repo)
            .unwrap();
        assert_eq!(blob.as_blob().unwrap().content(), b"2\n");
        let staged = repo
            .statuses(None)
            .unwrap()
            .iter()
            .any(|e| e.status().contains(git2::Status::INDEX_MODIFIED));
        assert!(staged, "暂存区的改动还在");

        // HEAD 已变 / 空消息都拒绝
        assert!(reword_head(&root, &old, "again").is_err());
        assert!(reword_head(&root, &new, "  \n").is_err());
        drop(blob);
        drop(commit);
        drop(repo);
        cleanup(root);
    }

    #[test]
    fn 关系判定_历史与推送() {
        let root = init_repo_with_file("log-relation", "a.txt", "1\n");
        let first = head(&root);
        let second = commit_file(&root, "a.txt", "2\n", "second");
        {
            // 远程跟踪分支停在 first:first 已推送,second 没有
            let repo = Repository::open(&root).unwrap();
            repo.reference(
                "refs/remotes/origin/main",
                Oid::from_str(&first).unwrap(),
                true,
                "test",
            )
            .unwrap();
        }
        let rel = commit_relation(&root, &second).unwrap();
        assert!(rel.is_head && rel.in_head_history && !rel.pushed);
        assert_eq!(rel.parent_count, 1);
        assert!(rel.head_branch.is_some());
        assert!(!rel.busy_state);
        let rel = commit_relation(&root, &first).unwrap();
        assert!(!rel.is_head && rel.in_head_history && rel.pushed);
        assert_eq!(rel.parent_count, 0);
        assert!(is_commit_pushed(&root, &first).unwrap());
        assert!(!is_commit_pushed(&root, &second).unwrap());

        // 不在当前分支历史上的提交
        let base = first.clone();
        branch(&root, "side", &base);
        switch(&root, "side");
        let side = commit_file(&root, "a.txt", "side\n", "side");
        let main_branch = rel.head_branch.clone().unwrap();
        switch(&root, &main_branch);
        assert!(!is_ancestor_of_head(&root, &side).unwrap());
        assert!(is_ancestor_of_head(&root, &first).unwrap());
        // 短哈希也认
        assert!(is_ancestor_of_head(&root, &first[..8]).unwrap());
        cleanup(root);
    }
}
