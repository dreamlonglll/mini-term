use super::*;

// ---------------------------------------------------------------------------
// Worktree 管理
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeInfo {
    pub name: String,
    pub path: String,
    /// HEAD 所在分支;detached / 失效条目为 None
    pub branch: Option<String>,
    pub is_main: bool,
    /// 目录还在且元数据能通过校验;false = 可被 prune 的失效条目
    pub is_valid: bool,
    pub is_locked: bool,
}

/// worktree 路径的对外形态:去掉尾部分隔符、换成本机原生分隔符。
///
/// git2 的 workdir() 与 worktree 路径一律是 `D:/x/proj/` 这种正斜杠带尾杠的写法,而
/// 项目配置里的路径不带尾杠、Windows 上扫盘得到的是反斜杠。这串既拿来展示,也被当成
/// 终端 cwd、「设为项目」的项目路径落盘 —— 不归一的话同一目录会出现两种斜杠
/// (Git 面板两个入口开出的终端 cwd 一正一反,第四轮 E2E)。
pub(super) fn display_path(p: &Path) -> String {
    let raw = p.to_string_lossy();
    let trimmed = mt_core::path_key::trim_trailing_separators(&raw);
    let native = if cfg!(windows) {
        trimmed.replace('/', "\\")
    } else {
        trimmed.to_string()
    };
    // 盘符根(`D:`)要留住分隔符,否则是「D 盘的当前目录」
    if native.len() == 2 && native.ends_with(':') {
        format!("{native}{}", std::path::MAIN_SEPARATOR)
    } else {
        native
    }
}

pub(super) fn head_branch(repo: &Repository) -> Option<String> {
    repo.head().ok().and_then(|h| {
        if h.is_branch() {
            h.shorthand().map(|s| s.to_string())
        } else {
            None
        }
    })
}

/// 列出某仓库的主工作区 + 全部 linked worktree(含失效条目,供管理面板展示与清理)。
/// 从 worktree 路径调用同样可行:元数据都在主仓库 .git/worktrees 下。
pub fn list_worktrees(repo_path: &Path) -> Result<Vec<WorktreeInfo>> {
    let repo = Repository::open(repo_path)?;
    // 从 linked worktree 打开时回到主仓库:linked worktree 的 gitdir 形如
    // `<main>/.git/worktrees/<name>`,上溯两级即主仓库 .git(git2 0.19 未暴露 commondir)
    let main_repo = if repo.is_worktree() {
        let git_dir = repo.path().to_path_buf();
        let main_git = git_dir
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow!("无法定位主仓库"))?;
        Repository::open(main_git)?
    } else {
        repo
    };

    let mut out = Vec::new();
    if let Some(workdir) = main_repo.workdir() {
        let name = workdir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "main".to_string());
        out.push(WorktreeInfo {
            name,
            path: display_path(workdir),
            branch: head_branch(&main_repo),
            is_main: true,
            is_valid: true,
            is_locked: false,
        });
    }

    if let Ok(names) = main_repo.worktrees() {
        for wt_name in names.iter().flatten() {
            let wt = match main_repo.find_worktree(wt_name) {
                Ok(w) => w,
                Err(_) => continue,
            };
            let wt_path = wt.path().to_path_buf();
            let is_valid = wt_path.exists() && wt.validate().is_ok();
            let is_locked = matches!(wt.is_locked(), Ok(git2::WorktreeLockStatus::Locked(_)));
            let branch = if is_valid {
                Repository::open_from_worktree(&wt)
                    .ok()
                    .and_then(|r| head_branch(&r))
            } else {
                None
            };
            out.push(WorktreeInfo {
                name: wt_name.to_string(),
                path: display_path(&wt_path),
                branch,
                is_main: false,
                is_valid,
                is_locked,
            });
        }
    }

    Ok(out)
}

/// 新建 worktree。`create_branch=true` 时以 `base`(缺省 HEAD)为起点建新分支,
/// 否则检出已有分支(该分支不能已被其他工作区持有,git 会给出明确报错)。
/// 大仓库的首次 checkout 可能较慢,超时给到 120s ——**阻塞调用线程**。
pub fn add_worktree(
    repo_path: &Path,
    worktree_path: &str,
    branch: &str,
    create_branch: bool,
    base: Option<&str>,
) -> Result<String> {
    let mut args: Vec<&str> = vec!["worktree", "add"];
    if create_branch {
        args.push("-b");
        args.push(branch);
        args.push(worktree_path);
        if let Some(b) = base
            && !b.is_empty()
        {
            args.push(b);
        }
    } else {
        args.push(worktree_path);
        args.push(branch);
    }
    let result = run_git_command(
        repo_path,
        &args,
        WORKTREE_OP_TIMEOUT,
        "，大仓库 checkout 可能较慢，请稍后刷新查看",
    );
    if result.is_ok() {
        invalidate_repo_cache();
    }
    result
}

/// [`remove_worktree`] 里要界面区别对待的两种失败(其余失败是 git 的原文)。
/// 调用方用 `err.downcast_ref::<WorktreeRemoveError>()` 认出来,换成说清原因与下一步的文案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeRemoveError {
    /// 目录里有东西被别的进程占着(终端停在里面、编辑器独占开着文件……),预检拦下,
    /// **git 一步都没做**:登记与目录都还在。`busy` 是查到的第一处占用。
    InUse { path: String, busy: String },
    /// git 摘掉了登记、目录却没删干净(预检之后才被占上),收尾补删也没成功:
    /// 登记已无、目录残留。`detail` 是补删失败的原因。
    Leftover { path: String, detail: String },
}

impl std::fmt::Display for WorktreeRemoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InUse { path, busy } => {
                write!(
                    f,
                    "worktree 目录正被占用,未做任何改动: {path}(占用处: {busy})"
                )
            }
            Self::Leftover { path, detail } => {
                write!(f, "worktree 登记已删除,但目录未能删除: {path}: {detail}")
            }
        }
    }
}

impl std::error::Error for WorktreeRemoveError {}

/// 删除前等占用放开的宽限:调用方刚关掉停在 worktree 里的终端时,shell 进程还在退出,
/// 当前目录句柄要等它真正退出才放掉。
pub(super) const REMOVE_IN_USE_GRACE: Duration = Duration::from_secs(3);
/// git 删到一半之后,补删残留目录的宽限(占用方可能正在退出)。
pub(super) const REMOVE_LEFTOVER_GRACE: Duration = Duration::from_secs(2);
/// 两档宽限内的轮询间隔。
pub(super) const REMOVE_POLL: Duration = Duration::from_millis(100);

/// 删除 worktree(工作目录 + 主仓库里的元数据)。
/// 有未提交改动 / 已锁定时 git 会拒绝,`force=true` 对应 `--force` 强制删除。
///
/// # 不留半删状态
///
/// `git worktree remove` 先删目录,**删目录失败也照样摘掉登记**(git 源码注释原话
/// 「there's no going back from here」)。Windows 上目录里只要有一样东西被别的进程以
/// 不许删除的方式占着 —— 终端的当前目录停在里面、编辑器独占开着文件 —— 就删不掉,
/// 落下「登记没了、目录还在、界面只报 Permission denied」的半删状态(第四轮 E2E A14)。
/// 所以分两步兜:
///
/// 1. 调 git 之前先预检占用([`wait_until_deletable`]),给 [`REMOVE_IN_USE_GRACE`]
///    的宽限;过了还占着就返回 [`WorktreeRemoveError::InUse`],git 一步不做;
/// 2. git 仍然失败、且**这一次**把登记摘掉了而目录还在(预检之后才被占上):把剩下的
///    目录删掉收尾,删得掉按成功算;删不掉返回 [`WorktreeRemoveError::Leftover`]。
///
/// **阻塞调用线程**(预检要遍历整个目录,另加最多两档宽限)。
pub fn remove_worktree(repo_path: &Path, worktree_path: &str, force: bool) -> Result<String> {
    let dir = Path::new(worktree_path);
    if dir.is_dir()
        && let Err(busy) = wait_until_deletable(dir, REMOVE_IN_USE_GRACE)
    {
        return Err(WorktreeRemoveError::InUse {
            path: worktree_path.to_string(),
            busy: busy.to_string_lossy().to_string(),
        }
        .into());
    }
    let was_registered = worktree_registered(repo_path, dir);
    let mut args: Vec<&str> = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(worktree_path);
    let result = run_git_command(repo_path, &args, LOCAL_OP_TIMEOUT, "");
    match result {
        Ok(out) => {
            invalidate_repo_cache();
            Ok(out)
        }
        // 只收拾「这一次」git 摘掉了登记的情形:本来就不是登记在册的 worktree 时
        // git 一开头就拒绝,目录不归这里删
        Err(err) if was_registered && dir.exists() && !worktree_registered(repo_path, dir) => {
            invalidate_repo_cache();
            match remove_leftover_dir(dir, REMOVE_LEFTOVER_GRACE) {
                Ok(()) => Ok(String::new()),
                Err(detail) => {
                    tracing::warn!("[git] worktree 删到一半: {err:#}");
                    Err(WorktreeRemoveError::Leftover {
                        path: worktree_path.to_string(),
                        detail,
                    }
                    .into())
                }
            }
        }
        Err(err) => Err(err),
    }
}

/// `dir` 是不是 `repo_path` 所属仓库登记在册的 linked worktree。列不出来时按「不是」
/// 算 —— 调用方据此决定要不要替 git 收尾删目录,宁可不删。
pub(super) fn worktree_registered(repo_path: &Path, dir: &Path) -> bool {
    let key = mt_core::path_key::windows_eq_key(&dir.to_string_lossy());
    list_worktrees(repo_path).is_ok_and(|list| {
        list.iter()
            .any(|wt| !wt.is_main && mt_core::path_key::windows_eq_key(&wt.path) == key)
    })
}

/// 等 `dir` 整棵子树都能删:有占用就盯着那一处等它放开,放开了再整棵复查,直到全部
/// 可删(`Ok`)或宽限用完(`Err(仍被占用的那一处)`)。非 Windows 上删除不受打开的
/// 句柄影响,直接 `Ok`。
pub(super) fn wait_until_deletable(
    dir: &Path,
    grace: Duration,
) -> std::result::Result<(), PathBuf> {
    let deadline = Instant::now() + grace;
    loop {
        let Some(busy) = first_locked_entry(dir) else {
            return Ok(());
        };
        // 盯着已知的那一处等(只开一个句柄,便宜);它放开了才值得再走一遍整棵树
        loop {
            if Instant::now() >= deadline {
                return Err(busy);
            }
            std::thread::sleep(REMOVE_POLL);
            if !is_locked_against_delete(&busy) {
                break;
            }
        }
    }
}

/// 子树里第一处删不掉的(目录自身算在内)。只往真目录里走:符号链接 / 目录联接
/// 不跟进去 —— git 删的是链接本身,链接那头的东西与这次删除无关。
pub(super) fn first_locked_entry(root: &Path) -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if is_locked_against_delete(&path) {
            return Some(path);
        }
        let is_real_dir = std::fs::symlink_metadata(&path)
            .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink());
        if !is_real_dir {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&path) {
            stack.extend(entries.flatten().map(|entry| entry.path()));
        }
    }
    None
}

/// 这一项现在删得掉吗(Windows)。以 DELETE 访问权、三种共享全开的方式试开一下:
/// 只和「没给 FILE_SHARE_DELETE」的既有句柄冲突(报共享冲突)—— 正好是会让删除失败的
/// 那一类:进程的当前目录、独占打开的文件。本应用自己的目录监听句柄给了
/// FILE_SHARE_DELETE,不算占用(它不挡删除)。其余错误(不存在、无权限)不归
/// 「占用」管,交给 git 自己报。
#[cfg(windows)]
pub(super) fn is_locked_against_delete(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    // winnt.h / winbase.h 的原值(windows-sys 没开 Storage_FileSystem 这组 feature)
    const DELETE: u32 = 0x0001_0000;
    const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x1 | 0x2 | 0x4;
    // 开目录必须带;链接只开它本身,不跟到目标
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    match std::fs::OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ_WRITE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(_) => false,
        Err(err) => err.raw_os_error() == Some(ERROR_SHARING_VIOLATION),
    }
}

#[cfg(not(windows))]
pub(super) fn is_locked_against_delete(_path: &Path) -> bool {
    false
}

/// git 删到一半留下的目录:在宽限内反复删,删掉(或已经没了)返回 `Ok`,
/// 宽限用完返回最后一次的错误原文。
pub(super) fn remove_leftover_dir(dir: &Path, grace: Duration) -> std::result::Result<(), String> {
    let deadline = Instant::now() + grace;
    loop {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) if Instant::now() >= deadline => return Err(err.to_string()),
            Err(_) => std::thread::sleep(REMOVE_POLL),
        }
    }
}

/// 清理失效的 worktree 元数据(目录已被手动删除的条目)。
pub fn prune_worktrees(repo_path: &Path) -> Result<String> {
    let result = run_git_command(repo_path, &["worktree", "prune"], QUICK_OP_TIMEOUT, "");
    if result.is_ok() {
        invalidate_repo_cache();
    }
    result
}

/// 批量判断路径是否 linked worktree,是则返回其分支名(项目列表 ⎇ 徽章用)。
/// UNC 路径(WSL 项目)直接跳过:git2 对网络路径的探测慢且徽章意义不大。
pub fn get_worktree_branches(paths: &[PathBuf]) -> Vec<Option<String>> {
    paths
        .iter()
        .map(|p| {
            // 必须按字符串判 UNC:Path::starts_with 是按「路径分量」比的,
            // 传 `\\` 永远不会命中。
            if p.to_string_lossy().starts_with(r"\\") {
                return None;
            }
            let repo = Repository::open(p).ok()?;
            if !repo.is_worktree() {
                return None;
            }
            head_branch(&repo)
        })
        .collect()
}

/// 某个本地主仓库的 linked worktree 快照（用于自动发现新增 worktree 与挂 `.git/worktrees` 监听）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRepoWorktrees {
    /// 主仓库的 `.git` 目录（已去尾斜杠、统一本机分隔符，供 `FsWatcher` 挂非递归监听）。
    pub git_dir: PathBuf,
    /// `<git_dir>/worktrees` 此刻是否存在（在后台判好，避免主线程做同步 stat）。
    pub worktrees_dir_exists: bool,
    /// 主仓库下全部有效 linked worktree 的规范化路径（排除主仓库自身与失效条目，去重）。
    pub linked_paths: Vec<String>,
    /// 是否存在刚建好 / 仍在 `initializing` 锁中、尚未通过校验的条目（调用方可做短重试）。
    pub has_pending_invalid: bool,
}

/// 检查一条刚创建但尚未通过 `validate()` 的 worktree 元数据是否处于初始化中：
/// git 在 `git worktree add` checkout 期间会先写 `locked` 文件内容 `"initializing"`，
/// 或是元目录刚建好不到几秒。只对这类条目置 `has_pending_invalid`，避免对陈旧失效条目空重试。
fn is_initializing_worktree(git_dir: &Path, wt_name: &str, wt: &git2::Worktree) -> bool {
    if let Ok(git2::WorktreeLockStatus::Locked(Some(reason))) = wt.is_locked()
        && reason.trim().eq_ignore_ascii_case("initializing")
    {
        return true;
    }
    let meta_dir = git_dir.join("worktrees").join(wt_name);
    std::fs::metadata(&meta_dir)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
        .is_some_and(|age| age < Duration::from_secs(5))
}

/// 探测单个本地路径是否为主 Git 仓库并返回其有效 linked worktree 清单。
///
/// - UNC 路径（WSL / 网络共享）、非仓库目录或**本身就是 linked worktree** 的路径返回 `None`
///   （不把兄弟 worktree 误挂到某个 worktree 之下）；
/// - `Repository::open` 不向上爬父目录，只认项目根自身是仓库的情况（零递归扫盘开销）。
pub fn inspect_local_main_repo_worktrees(repo_path: &Path) -> Option<LocalRepoWorktrees> {
    if repo_path.to_string_lossy().starts_with(r"\\") {
        return None;
    }
    let repo = Repository::open(repo_path).ok()?;
    if repo.is_worktree() {
        return None;
    }
    let git_dir = PathBuf::from(display_path(repo.path()));
    let worktrees_dir_exists = git_dir.join("worktrees").is_dir();
    let main_key = mt_core::path_key::windows_eq_key(&repo_path.to_string_lossy());
    let workdir_key = repo
        .workdir()
        .map(|w| mt_core::path_key::windows_eq_key(&w.to_string_lossy()));

    let mut seen_keys = HashSet::new();
    let mut linked_paths = Vec::new();
    let mut has_pending_invalid = false;

    if let Ok(names) = repo.worktrees() {
        for wt_name in names.iter().flatten() {
            let Ok(wt) = repo.find_worktree(wt_name) else {
                continue;
            };
            let wt_path = wt.path().to_path_buf();
            if wt_path.is_dir() && wt.validate().is_ok() {
                let norm = display_path(&wt_path);
                if norm.is_empty() {
                    continue;
                }
                let key = mt_core::path_key::windows_eq_key(&norm);
                if key == main_key || workdir_key.as_deref() == Some(key.as_str()) {
                    continue;
                }
                if seen_keys.insert(key) {
                    linked_paths.push(norm);
                }
            } else if is_initializing_worktree(&git_dir, wt_name, &wt) {
                has_pending_invalid = true;
            }
        }
    }

    Some(LocalRepoWorktrees {
        git_dir,
        worktrees_dir_exists,
        linked_paths,
        has_pending_invalid,
    })
}

/// 批量探测本地主仓库的 linked worktree 清单（供后台线程一次性跑完，避免逐项切线程）。
pub fn discover_local_repo_worktrees(paths: &[PathBuf]) -> Vec<Option<LocalRepoWorktrees>> {
    paths
        .iter()
        .map(|p| inspect_local_main_repo_worktrees(p))
        .collect()
}

#[cfg(test)]
mod local_discover_tests {
    use super::*;
    use crate::git::test_support::init_repo_with_file;

    #[test]
    fn 主仓库探测返回有效_worktree_而子_worktree_与非仓库返回_none() {
        let repo = init_repo_with_file("local-wt-discover", "a.txt", "hello\n");

        // 还没有 worktree 时：识别为主仓库、linked_paths 为空
        let before = inspect_local_main_repo_worktrees(&repo).expect("应识别为主仓库");
        assert!(before.linked_paths.is_empty());
        assert!(!before.worktrees_dir_exists);

        // 新增一条 linked worktree
        let wt_dir = repo.parent().unwrap().join(format!(
            "{}-wt-feat",
            repo.file_name().unwrap().to_string_lossy()
        ));
        add_worktree(&repo, &wt_dir.to_string_lossy(), "feat/auto", true, None)
            .expect("add_worktree");

        let after = inspect_local_main_repo_worktrees(&repo).expect("应识别为主仓库");
        assert!(after.worktrees_dir_exists);
        assert_eq!(after.linked_paths.len(), 1);
        assert_eq!(
            mt_core::path_key::windows_eq_key(&after.linked_paths[0]),
            mt_core::path_key::windows_eq_key(&wt_dir.to_string_lossy())
        );

        // 对 linked worktree 自身调用必须返回 None（不把兄弟 worktree 误挂到它下面）
        assert!(inspect_local_main_repo_worktrees(&wt_dir).is_none());
        // 非 git 目录与 UNC 路径返回 None
        assert!(inspect_local_main_repo_worktrees(repo.parent().unwrap()).is_none());
        assert!(inspect_local_main_repo_worktrees(Path::new(r"\\wsl$\Ubuntu\repo")).is_none());

        // 删除目录制造失效 worktree：linked_paths 不再包含它
        let _ = std::fs::remove_dir_all(&wt_dir);
        let stale = inspect_local_main_repo_worktrees(&repo).expect("主仓库仍在");
        assert!(stale.linked_paths.is_empty());

        let _ = std::fs::remove_dir_all(&repo);
    }
}
