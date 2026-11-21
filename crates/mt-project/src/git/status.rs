use super::*;

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

pub(super) fn map_status(status: Status, is_empty_repo: bool) -> Option<GitStatus> {
    if status.contains(Status::CONFLICTED) {
        return Some(GitStatus::Conflicted);
    }
    if status.contains(Status::INDEX_RENAMED) || status.contains(Status::WT_RENAMED) {
        return Some(GitStatus::Renamed);
    }
    if status.contains(Status::INDEX_NEW) {
        return Some(GitStatus::Added);
    }
    if status.contains(Status::INDEX_MODIFIED) || status.contains(Status::WT_MODIFIED) {
        return Some(GitStatus::Modified);
    }
    if status.contains(Status::INDEX_DELETED) || status.contains(Status::WT_DELETED) {
        return Some(GitStatus::Deleted);
    }
    if status.contains(Status::WT_NEW) {
        if is_empty_repo {
            return Some(GitStatus::Added);
        } else {
            return Some(GitStatus::Untracked);
        }
    }
    None
}

pub(super) fn status_label(status: &GitStatus) -> &'static str {
    match status {
        GitStatus::Modified => "M",
        GitStatus::Added => "A",
        GitStatus::Deleted => "D",
        GitStatus::Renamed => "R",
        GitStatus::Untracked => "?",
        GitStatus::Conflicted => "C",
    }
}

pub(super) fn map_staged_status(status: Status) -> Option<GitStatus> {
    if status.contains(Status::CONFLICTED) {
        return Some(GitStatus::Conflicted);
    }
    if status.contains(Status::INDEX_RENAMED) {
        return Some(GitStatus::Renamed);
    }
    if status.contains(Status::INDEX_NEW) {
        return Some(GitStatus::Added);
    }
    if status.contains(Status::INDEX_MODIFIED) {
        return Some(GitStatus::Modified);
    }
    if status.contains(Status::INDEX_DELETED) {
        return Some(GitStatus::Deleted);
    }
    None
}

pub(super) fn map_unstaged_status(status: Status, is_empty_repo: bool) -> Option<GitStatus> {
    if status.contains(Status::CONFLICTED) {
        return Some(GitStatus::Conflicted);
    }
    if status.contains(Status::WT_RENAMED) {
        return Some(GitStatus::Renamed);
    }
    if status.contains(Status::WT_MODIFIED) {
        return Some(GitStatus::Modified);
    }
    if status.contains(Status::WT_DELETED) {
        return Some(GitStatus::Deleted);
    }
    if status.contains(Status::WT_NEW) {
        if is_empty_repo {
            return Some(GitStatus::Added);
        } else {
            return Some(GitStatus::Untracked);
        }
    }
    None
}

/// 取状态的选项:含未跟踪(逐个文件)、不含忽略,**不让 libgit2 做重命名检测**。
///
/// 「更改」区要的是暂存区一侧的重命名(`git mv` / 暂存了「删旧 + 加新」之后显示一行 R,
/// 与 `git status` 一致),但 libgit2 status 自带的 `RENAMES_HEAD_TO_INDEX` 没有护栏:
/// 相似度配对是「目标数 × min(源数, 1000)」次比较,还被 `diff.renameLimit`(默认 1000)
/// 拦腰截断 —— 暂存 2000 个改名 1.24s、只配上 999 对。所以 status 本身不开,配对交给
/// [`pair_staged_renames`](有护栏,见 [`find_renames_guarded`])。
///
/// 工作区一侧一直不配:libgit2 的 `RENAMES_INDEX_TO_WORKDIR` 会把「工作区删掉的已跟踪
/// 文件」与**未跟踪文件**配成一对,`git status` 从不这么做 —— 未跟踪文件只是躺在那里,
/// 不该被认成谁的新名字。
pub(super) fn status_options() -> StatusOptions {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);
    opts
}

/// 一次 status 的一条原始结果(重命名已配好)。「更改」区与文件树两种视图都从它派生。
#[derive(Debug, Clone)]
pub(super) struct RawEntry {
    /// 仓库内相对路径(`/` 分隔);已暂存的重命名是**新**路径。
    pub(super) path: String,
    /// 已暂存重命名的旧路径。
    pub(super) renamed_from: Option<String>,
    pub(super) status: Status,
}

/// 一个仓库一次全量 status 的结果,见 [`shared_status`]。
#[derive(Debug)]
pub(super) struct RawStatus {
    pub(super) workdir: PathBuf,
    pub(super) is_empty_repo: bool,
    pub(super) entries: Vec<RawEntry>,
}

pub(super) fn slash_path(p: &Path) -> String {
    let text = p.to_string_lossy();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

/// 跑一次 status(`show` 决定比哪两侧),再配暂存区一侧的重命名。
pub(super) fn raw_entries(repo: &Repository, show: git2::StatusShow) -> Result<Vec<RawEntry>> {
    let mut opts = status_options();
    opts.show(show);
    let statuses = repo.statuses(Some(&mut opts))?;
    let mut entries = Vec::with_capacity(statuses.len());
    let (mut deleted, mut added) = (false, false);
    for entry in statuses.iter() {
        let status = entry.status();
        deleted |= status.contains(Status::INDEX_DELETED);
        added |= status.contains(Status::INDEX_NEW);
        entries.push(RawEntry {
            path: entry.path().unwrap_or("").to_string(),
            renamed_from: None,
            status,
        });
    }
    drop(statuses);
    // 只有「已暂存删除」与「已暂存新增」都在时才可能有重命名(绝大多数刷新到这里就完了)
    if deleted && added {
        pair_staged_renames(repo, &mut entries)?;
    }
    Ok(entries)
}

/// 把 status 里一对「已暂存删除(旧路径)+ 已暂存新增(新路径)」并成一条已暂存重命名,
/// 结果与 libgit2 status 开 `RENAMES_HEAD_TO_INDEX` 时的形态一致:
///
/// - 新路径那条:`INDEX_NEW` 换成 `INDEX_RENAMED`(内容也变了再加 `INDEX_MODIFIED`),
///   工作区一侧的位原样保留,记下旧路径;
/// - 旧路径那条:去掉 `INDEX_DELETED`;剩下的位(旧路径又冒出一个未跟踪文件)单独成行,
///   什么都不剩就整条去掉。
///
/// 配对来自一份 HEAD → 暂存区的树 diff([`find_renames_guarded`],选项与 libgit2 status
/// 自己做配对时相同)。这份 diff 与 status 不是同一刻读的:对不上(期间暂存区又变了)的
/// 配对直接跳过,只合并两头都还在 status 里的。
pub(super) fn pair_staged_renames(repo: &Repository, entries: &mut Vec<RawEntry>) -> Result<()> {
    let head_tree = match repo.head() {
        Ok(head) => Some(head.peel_to_tree()?),
        Err(_) => None,
    };
    let mut diff_opts = git2::DiffOptions::new();
    diff_opts.include_typechange(true);
    let mut diff = repo.diff_tree_to_index(head_tree.as_ref(), None, Some(&mut diff_opts))?;
    // 与 libgit2 status 同一份配对选项:只带 FOR_UNTRACKED(其余旗标不设 = 按 `diff.renames`
    // 配置决定认不认重命名),阈值、`diff.renameLimit` 都取默认
    let mut find = git2::DiffFindOptions::new();
    find.for_untracked(true);
    find_renames_guarded(&mut diff, find)?;

    let pos: HashMap<String, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.path.clone(), i))
        .collect();
    let mut emptied = false;
    for delta in diff.deltas() {
        if delta.status() != git2::Delta::Renamed {
            continue;
        }
        let (Some(old), Some(new)) = (delta.old_file().path(), delta.new_file().path()) else {
            continue;
        };
        let (old, new) = (slash_path(old), slash_path(new));
        let (Some(&src), Some(&dst)) = (pos.get(&old), pos.get(&new)) else {
            continue;
        };
        if !entries[src].status.contains(Status::INDEX_DELETED)
            || !entries[dst].status.contains(Status::INDEX_NEW)
        {
            continue;
        }
        let mut status = (entries[dst].status - Status::INDEX_NEW) | Status::INDEX_RENAMED;
        if delta.old_file().id() != delta.new_file().id() {
            status |= Status::INDEX_MODIFIED;
        }
        entries[dst].status = status;
        entries[dst].renamed_from = Some(old);
        entries[src].status -= Status::INDEX_DELETED;
        emptied |= entries[src].status.is_empty();
    }
    if emptied {
        entries.retain(|e| !e.status.is_empty());
    }
    Ok(())
}

/// libgit2 相似度配对的上限(删除数 × 新增数)。实测(每个文件约 60 行 / 2.5 KB,
/// HEAD → 暂存区):100×100 31ms、200×200 68ms、300×300 123ms、500×500 271ms,
/// 2000×2000(还被 `diff.renameLimit` 截成 2000×1000)1.33s。status 每次刷新都要算,
/// 取 200×200 那一档。
pub(super) const MAX_INEXACT_RENAME_PAIRS: u64 = 40_000;

/// 只做精确配对(blob 相同)的上限。精确配对也是逐对比较(只比 oid,2000×2000 约 12ms),
/// 二次方增长,5000×5000 约 75ms;再多就整个不配了(删除 / 新增照原样分两行列出)。
pub(super) const MAX_EXACT_RENAME_PAIRS: u64 = 25_000_000;

/// 重命名配对的档位,由删除数 × 新增数决定(见两个上限常量)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RenameTier {
    /// 没有可配的(一侧为零)。
    Nothing,
    /// libgit2 的相似度配对,与以往一样(改名又改了内容也认得出)。
    Similar,
    /// 只认内容完全相同的(纯改名);改名又改了内容的照「删除 + 新增」列。
    ExactOnly,
    /// 不配对。
    Skip,
}

pub(super) fn rename_tier(sources: u64, targets: u64) -> RenameTier {
    match sources.saturating_mul(targets) {
        0 => RenameTier::Nothing,
        n if n <= MAX_INEXACT_RENAME_PAIRS => RenameTier::Similar,
        n if n <= MAX_EXACT_RENAME_PAIRS => RenameTier::ExactOnly,
        _ => RenameTier::Skip,
    }
}

/// 带护栏的 `find_similar`:先数一遍候选(删除 / 新增,类型变更两边都算),按
/// [`rename_tier`] 决定怎么配。
///
/// libgit2 的配对没有 git CLI 那种「先精确配、候选太多就跳过模糊」的捷径,而是
/// 「目标 × min(源, `diff.renameLimit`)」次相似度比较一把算完 —— 候选一多就是秒级,
/// 还会被 renameLimit 截断(超出的源根本不看,纯改名也只配上一半)。精确档把
/// renameLimit 放到候选数,保证纯改名全部配上。提交 diff([`first_parent_diff`])与
/// 「更改」区([`pair_staged_renames`])共用。
pub(super) fn find_renames_guarded(
    diff: &mut git2::Diff<'_>,
    mut find: git2::DiffFindOptions,
) -> Result<()> {
    let (mut sources, mut targets) = (0u64, 0u64);
    for delta in diff.deltas() {
        match delta.status() {
            git2::Delta::Deleted => sources += 1,
            git2::Delta::Added => targets += 1,
            git2::Delta::Typechange => {
                sources += 1;
                targets += 1;
            }
            _ => {}
        }
    }
    match rename_tier(sources, targets) {
        RenameTier::Nothing | RenameTier::Skip => return Ok(()),
        RenameTier::Similar => {}
        RenameTier::ExactOnly => {
            let limit = usize::try_from(sources.max(targets)).unwrap_or(usize::MAX);
            find.exact_match_only(true).rename_limit(limit);
        }
    }
    diff.find_similar(Some(&mut find))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// status 的单飞:同一仓库的并发请求合并成一趟
// ---------------------------------------------------------------------------
//
// 「更改」区([`get_changes_status`])与文件树([`get_git_status`])对同一次变化各要一份
// status,4 万文件 + 2 万未跟踪的仓库一份就是 120~190ms。两边共用一趟:
//
// - **只拿「请求之后才开始」的那一趟**。在途的那一趟若早于本次请求开始,它可能没看到
//   请求之前刚发生的改动 —— 本次请求排到下一趟,下一趟由这期间到达的全部请求共享。
// - **不按时间复用**(没有 TTL):一趟算完就不再交给后来的请求。
// - **开算前留一个很短的集合窗口**([`STATUS_GATHER`]):同一刻发出的几路请求(一次变化
//   同时叫醒「更改」区与文件树)前后差几毫秒才落到后台线程上,先到的那一路若立刻开算,
//   后到的只能等它算完再自己算一趟(反倒比各算各的更慢)。窗口里到达的请求仍然早于
//   计算开始,搭同一趟不违反上一条。
//
// 于是并发的 N 个请求最多触发两趟(在途的一趟 + 合并后的一趟),顺序的请求一趟不省。

/// status 开算前的集合窗口(见上面的说明)。比一次 status 的耗时(大仓库上百毫秒)小两个
/// 数量级,单独一路请求只多等这么一下。
pub(super) const STATUS_GATHER: Duration = Duration::from_millis(5);

/// 单飞的现场。代次:`started` = 最近一趟开始(领跑)时的代次号,`finished` = 最近一趟
/// 算完的代次号。请求进来时要的是此刻之后才开始计算的那一趟:领跑者还在集合窗口里
/// (`gathering`)就是它这一趟,否则是下一趟。
pub(super) struct FlightState<T> {
    running: bool,
    gathering: bool,
    started: u64,
    finished: u64,
    /// 正在等的请求数。算完时没人等就不留结果(不把整份 status 攥在内存里)。
    waiters: usize,
    /// 第 `finished` 趟的结果(有人等时才留)。
    last: Option<T>,
}

pub(super) struct SingleFlight<T> {
    state: Mutex<FlightState<T>>,
    done: parking_lot::Condvar,
    /// 领跑者开算前等多久,让同一刻发出的请求搭上同一趟;零 = 不等。
    gather: Duration,
}

impl<T> SingleFlight<T> {
    pub(super) fn with_gather(gather: Duration) -> Self {
        Self {
            state: Mutex::new(FlightState {
                running: false,
                gathering: false,
                started: 0,
                finished: 0,
                waiters: 0,
                last: None,
            }),
            done: parking_lot::Condvar::new(),
            gather,
        }
    }
}

impl<T> Default for SingleFlight<T> {
    fn default() -> Self {
        Self::with_gather(Duration::ZERO)
    }
}

/// 领跑者收尾:记下结果、放行等着的请求。放在 `Drop` 里 —— 计算中途 panic 也得把
/// `running` 放掉,否则后来的请求永远等下去(那时不记结果,等着的请求里会有一个接着领跑)。
pub(super) struct FlightLeader<'a, T> {
    flight: &'a SingleFlight<T>,
    generation: u64,
    value: Option<T>,
}

impl<T> Drop for FlightLeader<'_, T> {
    fn drop(&mut self) {
        let mut st = self.flight.state.lock();
        st.running = false;
        st.gathering = false;
        if let Some(value) = self.value.take() {
            st.finished = self.generation;
            st.last = (st.waiters > 0).then_some(value);
        }
        self.flight.done.notify_all();
    }
}

impl<T: Clone> SingleFlight<T> {
    /// 拿一份「本次调用之后才开始计算」的结果:没有在途的就自己领跑(先等一个集合窗口);
    /// 领跑者还在集合就搭它这一趟;已经开算了就等它算完,再由等着的请求之一领跑下一趟、
    /// 其余共享。
    pub(super) fn run(&self, compute: impl FnOnce() -> T) -> T {
        let mut st = self.state.lock();
        let need = if st.running && st.gathering {
            st.started
        } else {
            st.started + 1
        };
        loop {
            if st.finished >= need
                && let Some(value) = &st.last
            {
                return value.clone();
            }
            if !st.running {
                break;
            }
            st.waiters += 1;
            self.done.wait(&mut st);
            st.waiters -= 1;
        }
        st.running = true;
        st.started += 1;
        let mut leader = FlightLeader {
            flight: self,
            generation: st.started,
            value: None,
        };
        if !self.gather.is_zero() {
            st.gathering = true;
            drop(st);
            std::thread::sleep(self.gather);
            // 窗口关上之后才开算:之后到的请求一律排下一趟
            self.state.lock().gathering = false;
        } else {
            drop(st);
        }
        let value = compute();
        leader.value = Some(value.clone());
        drop(leader);
        value
    }

    #[cfg(test)]
    pub(super) fn waiting(&self) -> usize {
        self.state.lock().waiters
    }

    #[cfg(test)]
    pub(super) fn is_gathering(&self) -> bool {
        self.state.lock().gathering
    }
}

pub(super) type SharedStatus = std::result::Result<std::sync::Arc<RawStatus>, String>;

pub(super) static STATUS_FLIGHTS: std::sync::LazyLock<
    Mutex<HashMap<String, std::sync::Arc<SingleFlight<SharedStatus>>>>,
> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// 一个仓库的全量 status(按仓库单飞,见上面的说明)。`repo_path` 是仓库根;同一仓库的
/// 不同写法(libgit2 给的 `D:/x/proj/` 与配置里的反斜杠写法、Windows 上的大小写)按同一个
/// 键认。只做字符串归一、不碰文件系统(WSL 项目的 UNC 路径规范化一次就要走一趟网络);
/// 经软链换了写法的认不到一起,只是少共享一趟,结果不受影响。
pub(super) fn shared_status(repo_path: &Path) -> Result<std::sync::Arc<RawStatus>> {
    let raw = repo_path.to_string_lossy();
    let raw = mt_core::path_key::strip_verbatim_prefix(&raw);
    let key = if cfg!(windows) {
        mt_core::path_key::windows_eq_key(&raw)
    } else {
        mt_core::path_key::trim_trailing_separators(&raw).to_string()
    };
    let flight = STATUS_FLIGHTS
        .lock()
        .entry(key)
        .or_insert_with(|| std::sync::Arc::new(SingleFlight::with_gather(STATUS_GATHER)))
        .clone();
    flight
        .run(|| {
            compute_raw_status(repo_path)
                .map(std::sync::Arc::new)
                .map_err(|e| format!("{e:#}"))
        })
        .map_err(anyhow::Error::msg)
}

pub(super) fn compute_raw_status(repo_path: &Path) -> Result<RawStatus> {
    let repo = Repository::open(repo_path)?;
    Ok(RawStatus {
        workdir: repo.workdir().unwrap_or_else(|| repo.path()).to_path_buf(),
        is_empty_repo: repo.head().is_err(),
        entries: raw_entries(&repo, git2::StatusShow::IndexAndWorkdir)?,
    })
}

/// 文件树的视图:路径相对项目根(`project_path` 可以是仓库里的子目录 —— 全量结果在
/// 「更改」区与文件树之间共享,这里只挑项目根以下的;重命名只报新路径,旧路径已不在
/// 工作区,文件树上没有它那一行)。
pub(super) fn file_tree_view(raw: &RawStatus, project_path: &Path) -> Vec<GitFileStatus> {
    // 项目经软链进入时 workdir 是解析过的真实路径,前缀也得换成真实路径才比得上
    let prefix = status_prefix(&raw.workdir, project_path);
    let mut result = Vec::new();
    for entry in &raw.entries {
        let Some(git_status) = map_status(entry.status, raw.is_empty_repo) else {
            continue;
        };
        let display_path = match diff_paths(raw.workdir.join(&entry.path), &prefix) {
            Some(rel) => {
                let rel = slash_path(&rel);
                // 在项目根之外(项目是 monorepo 的子目录):文件树上没有这一行
                if rel == ".." || rel.starts_with("../") {
                    continue;
                }
                rel
            }
            None => entry.path.clone(),
        };
        // 重命名才有 old_path
        let old_path = entry
            .renamed_from
            .clone()
            .filter(|_| matches!(git_status, GitStatus::Renamed));
        result.push(GitFileStatus {
            path: display_path,
            old_path,
            status_label: status_label(&git_status).to_string(),
            status: git_status,
        });
    }
    result
}

/// 「更改」区的视图:每个文件的暂存 / 未暂存两侧状态。
pub(super) fn changes_view(raw: &RawStatus) -> Vec<ChangeFileStatus> {
    let mut result = Vec::new();
    for entry in &raw.entries {
        let staged = map_staged_status(entry.status);
        let unstaged = map_unstaged_status(entry.status, raw.is_empty_repo);
        if staged.is_none() && unstaged.is_none() {
            continue;
        }
        let label = staged
            .as_ref()
            .or(unstaged.as_ref())
            .map(status_label)
            .unwrap_or("")
            .to_string();
        result.push(ChangeFileStatus {
            path: entry.path.clone(),
            old_path: entry.renamed_from.clone(),
            staged_status: staged,
            unstaged_status: unstaged,
            status_label: label,
        });
    }
    result
}
/// 文件树的 git 状态:项目根下每个仓库的状态,路径相对项目根。
///
/// 每个仓库的 status 与「更改」区([`get_changes_status`])按仓库单飞、共享一趟
/// ([`shared_status`]);项目是 monorepo 的子目录时也是整仓一趟,再按项目根筛。
/// 单个仓库读失败的跳过,不拖垮整份。
pub fn get_git_status(project_path: &Path) -> Result<Vec<GitFileStatus>> {
    let mut all = Vec::new();
    for entry in find_repos_cached_paths(project_path) {
        if let Ok(raw) = shared_status(&entry.path) {
            all.extend(file_tree_view(&raw, project_path));
        }
    }
    Ok(all)
}

/// 「更改」列表的数据:每个文件的暂存 / 未暂存两侧状态。已暂存的重命名是一行
/// (`path` = 新路径、`old_path` = 旧路径、`staged_status = Renamed`),口径见 [`status_options`]。
/// 与文件树共享同一趟 status([`shared_status`])。
pub fn get_changes_status(repo_path: &Path) -> Result<Vec<ChangeFileStatus>> {
    let raw = shared_status(repo_path)?;
    Ok(changes_view(&raw))
}
