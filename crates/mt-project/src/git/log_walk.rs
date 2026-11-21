use super::*;

// ---------------------------------------------------------------------------
// 提交历史分页
// ---------------------------------------------------------------------------
//
// # 为什么不用 libgit2 的排序 revwalk
//
// 旧实现是 `revwalk.set_sorting(TIME | TOPOLOGICAL)`。libgit2 只要设了排序就进
// limited 模式(`revwalk.c` 的 `git_revwalk_sorting`:`sorting != NONE` 即
// `limited = 1`),产出第一个提交之前 `prepare_walk` 先跑 `limit_list` 把全部可达
// 提交走一遍、再 `sort_in_topological_order` 整体排序 —— 每页都是 O(全历史)。
// 十万提交的仓库一页几百毫秒,终端里每 commit 一次还要自动重载一遍。
//
// 不设排序(NONE)时 libgit2 按「弹出一个、把它的父按提交时间插回待走链表」走,
// 是增量的,但只是**按时间**:子提交时间早于父(时钟偏移)或同一秒的并行分支,
// 都会让父排到子前面。
//
// 旧的分页也有洞:续页从「上一页最后一个提交的父」重起一个 revwalk,只沿那一条
// 父链往下走,页边界时别的分支上还没产出的提交从此再也不会出现(合并历史里必现,
// 3000 提交的基准仓库里有 108 个永远翻不到);反过来也会把已加载的提交再带回来
// (UI 那边「重复 hash 必须丢掉」的去重就是为这个加的)。
//
// # 现在的做法
//
// 自己走:**已发现子图上的 Kahn 拓扑序,就绪集合里按提交时间取最新的**,同一秒按
// 发现先后(规则同 `git log --date-order`)。发现是懒的 —— 产出一个提交时才读它的
// 父,所以一页的代价是 O(本页 + frontier),与总历史长度无关。
//
// 续页的状态不靠服务端缓存,而是由已加载列表**原样还原**:frontier = 已加载提交
// 的父 − 已加载提交(见 [`GitLogCursor`])。任何未加载的可达提交都是某个 frontier
// 提交的祖先(沿任意一条从起点下来的路径,第一个未加载的提交必然是某个已加载
// 提交的父),从 frontier 接着走、跳过已加载的,所以**跨页不漏、不重**。
//
// # 拓扑序:遍历只对「已发现的子」负责,其余由 [`merge_log_page`] 兜底
//
// 「父永远在子之后」要求产出父之前确认它**所有**子都已产出。已发现的子提交能确认
// (上面的 Kahn 就是干这个的);但时钟偏移下,子提交可能还埋在某个时间更早的
// frontier 提交的祖先里,尚未发现 —— 不走全历史(或没有 commit-graph 的代次号)
// 就无从得知。这是增量遍历的理论下限,git 自己在没有 commit-graph 时做
// `--topo-order` 也是退回全量遍历。
//
// 于是分两层:遍历按上面的规则出页(页内也可能出现这种迟到的子);UI 合并新页时由
// [`merge_log_page`] 检查「新提交的父是否已排在它前面」,有就对已加载列表做一次
// 稳定的拓扑修正,把迟到的子提交挪到它最早那个父的正前方(典型的「慢时钟分支」
// 场景下,这正是 `git log --date-order` 全量排序给它的位置)。没有时钟偏移的仓库
// 这一步只是检查,一行不挪。

/// 续页游标:由已加载的提交还原出来的遍历现场。
///
/// 不在服务端缓存 revwalk(那要管仓库变更后的失效、多个面板各自的游标),而是
/// 由已加载列表还原 —— 历史区常驻一份 [`LoadedLog`],随每页增量推进(与整份重算逐字段
/// 相同),不必每页 O(已加载条数) 地重算。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitLogCursor {
    /// 已加载(已产出)的提交。续页再遇到一律跳过。
    pub(super) loaded: HashSet<Oid>,
    /// 已加载提交的父里还没加载的那些,按第一次出现的先后 —— 与一口气连续遍历时
    /// 它们被发现的次序相同,同一秒的并列就按这个先后排。
    pub(super) frontier: Vec<Oid>,
}

// Oids have no serde representation in git2. Keep frontier order (date-topology
// tie-breaking depends on it), while sorting the unordered loaded set on wire.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GitLogCursorWire {
    loaded: Vec<String>,
    frontier: Vec<String>,
}

impl Serialize for GitLogCursor {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut loaded: Vec<_> = self.loaded.iter().map(ToString::to_string).collect();
        loaded.sort_unstable();
        GitLogCursorWire {
            loaded,
            frontier: self.frontier.iter().map(ToString::to_string).collect(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for GitLogCursor {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let wire = GitLogCursorWire::deserialize(deserializer)?;
        let parse = |field: &str, values: Vec<String>| -> std::result::Result<Vec<Oid>, D::Error> {
            values.into_iter().enumerate().map(|(index, value)| {
                if value.len() != 40 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(serde::de::Error::custom(format!(
                        "GitLogCursor.{field}[{index}] must be a full 40-character hexadecimal Oid"
                    )));
                }
                Oid::from_str(&value).map_err(|error| serde::de::Error::custom(format!(
                    "invalid GitLogCursor.{field}[{index}]: {error}"
                )))
            }).collect()
        };
        Ok(Self {
            loaded: parse("loaded", wire.loaded)?.into_iter().collect(),
            frontier: parse("frontier", wire.frontier)?,
        })
    }
}

impl GitLogCursor {
    /// 按已加载列表(UI 手上那份,顺序即展示顺序)还原游标。
    pub fn from_loaded(loaded: &[GitCommitInfo]) -> Self {
        let set: HashSet<Oid> = loaded
            .iter()
            .filter_map(|c| Oid::from_str(&c.hash).ok())
            .collect();
        let mut queued = HashSet::new();
        let mut frontier = Vec::new();
        for commit in loaded {
            for parent in &commit.parent_hashes {
                if let Ok(oid) = Oid::from_str(parent)
                    && !set.contains(&oid)
                    && queued.insert(oid)
                {
                    frontier.push(oid);
                }
            }
        }
        Self {
            loaded: set,
            frontier,
        }
    }

    /// Advance a remote/stateless continuation without losing previously loaded
    /// commits or the ordering of the still-pending frontier.
    pub fn advance(&self, commits: &[GitCommitInfo]) -> Self {
        let page = Self::from_loaded(commits);
        let mut loaded = self.loaded.clone();
        loaded.extend(page.loaded);
        let mut queued = HashSet::new();
        let frontier = self
            .frontier
            .iter()
            .copied()
            .chain(page.frontier)
            .filter(|oid| !loaded.contains(oid) && queued.insert(*oid))
            .collect();
        Self { loaded, frontier }
    }
}

/// 取一页提交历史。
///
/// - `after = None`:首页,从 `branch`(`None` = HEAD)起走;
/// - `after = Some(游标)`:续页,只由游标决定从哪接着走,`branch` 不再解析 ——
///   分支在两页之间被删 / 被移动都不影响翻页(要看新内容由调用方整体重载)。
///
/// 空仓库(HEAD 未出生)返回空列表而不是错误。
pub fn get_git_log(
    repo_path: &Path,
    after: Option<&GitLogCursor>,
    limit: Option<usize>,
    branch: Option<&str>,
) -> Result<Vec<GitCommitInfo>> {
    let repo = Repository::open(repo_path)?;
    let limit = limit.unwrap_or(30);
    let empty = GitLogCursor::default();
    let mut walk = DateTopoWalk::new(&repo, after.unwrap_or(&empty));

    if let Some(cursor) = after {
        for &oid in &cursor.frontier {
            walk.discover(oid);
        }
    } else {
        match log_tip(&repo, branch)? {
            Some(tip) => walk.discover(tip),
            None => return Ok(Vec::new()),
        }
    }

    Ok(walk.take(limit))
}

/// 首页的起点:`branch`(`None` = HEAD)指向的提交。空仓库(HEAD 未出生)是 `None`。
pub(super) fn log_tip(repo: &Repository, branch: Option<&str>) -> Result<Option<Oid>> {
    match branch {
        Some(b) => {
            // 先找本地 refs/heads/<b>,再找远程 refs/remotes/<b>
            // worktree 持有的分支也在 refs/heads/ 下(与主 repo 共享 refs 存储),天然支持
            let local_ref = format!("refs/heads/{}", b);
            let remote_ref = format!("refs/remotes/{}", b);
            let reference = repo
                .find_reference(&local_ref)
                .or_else(|_| repo.find_reference(&remote_ref))
                .map_err(|_| anyhow!("未找到分支:{}", b))?;
            Ok(Some(
                reference
                    .peel_to_commit()
                    .map_err(|_| anyhow!("分支 {} 无有效 target", b))?
                    .id(),
            ))
        }
        None => match repo.head() {
            Ok(head) => Ok(Some(head.peel_to_commit()?.id())),
            // 空仓库:HEAD 指向还没出生的分支,没有历史可列
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch => Ok(None),
            Err(e) => Err(e.into()),
        },
    }
}

/// 一次取页的遍历现场。见上面「现在的做法」。
pub(super) struct DateTopoWalk<'r> {
    repo: &'r Repository,
    /// 上一批已加载的提交(续页时);首页是空集。
    loaded: &'r HashSet<Oid>,
    /// 本次发现过的提交(含读不到的),再遇到不重复入队。
    pub(super) discovered: HashSet<Oid>,
    /// 已发现、未产出的提交,连同它的发现序号。
    pub(super) pending: HashMap<Oid, (Commit<'r>, u64)>,
    /// 每个提交还有几个「已发现、未产出」的子提交;归零才轮得到它。
    waiting_children: HashMap<Oid, u32>,
    /// 就绪的提交:时间新的先出,同一秒按发现先后。可能有过期条目,弹出时复核。
    ready: BinaryHeap<(i64, Reverse<u64>, Oid)>,
    next_seq: u64,
}

impl<'r> DateTopoWalk<'r> {
    pub(super) fn new(repo: &'r Repository, cursor: &'r GitLogCursor) -> Self {
        Self {
            repo,
            loaded: &cursor.loaded,
            discovered: HashSet::new(),
            pending: HashMap::new(),
            waiting_children: HashMap::new(),
            ready: BinaryHeap::new(),
            next_seq: 0,
        }
    }

    fn waiting(&self, oid: &Oid) -> u32 {
        self.waiting_children.get(oid).copied().unwrap_or(0)
    }

    /// 把一个提交纳入已发现子图。已加载 / 已发现过的直接跳过;读不到的
    /// (浅克隆边界、对象缺失)当它不存在,不让整页失败。
    pub(super) fn discover(&mut self, oid: Oid) {
        if self.loaded.contains(&oid) || !self.discovered.insert(oid) {
            return;
        }
        let Ok(commit) = self.repo.find_commit(oid) else {
            return;
        };
        for parent in commit.parent_ids() {
            *self.waiting_children.entry(parent).or_default() += 1;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let time = commit.time().seconds();
        self.pending.insert(oid, (commit, seq));
        if self.waiting(&oid) == 0 {
            self.ready.push((time, Reverse(seq), oid));
        }
    }

    fn take(&mut self, limit: usize) -> Vec<GitCommitInfo> {
        self.take_matching(limit, usize::MAX, |_| true).0
    }

    /// 同 [`Self::take`],但只把 `keep` 认可的提交放进结果(过滤 / 文件历史用)。
    /// 不认可的照常推进拓扑 —— 父照样发现、照样就绪,只是不产出。
    ///
    /// `budget` 是本次最多**走过**几个提交(产出 + 跳过):条件很少命中时一页可能
    /// 要走遍大半个历史,分批走、每批把现场交回调用方,UI 才能半路换条件、不被
    /// 一次全量遍历拖住。返回 `(结果, 走过的个数)`。
    pub(super) fn take_matching(
        &mut self,
        limit: usize,
        budget: usize,
        mut keep: impl FnMut(&Commit<'r>) -> bool,
    ) -> (Vec<GitCommitInfo>, usize) {
        let mut out = Vec::with_capacity(limit.min(256));
        let mut walked = 0usize;
        while out.len() < limit && walked < budget {
            let Some((_, _, oid)) = self.ready.pop() else {
                break;
            };
            // 过期条目:已产出过(重复入堆),或入堆之后又发现了它的子提交(时钟偏移)
            if self.waiting(&oid) > 0 {
                continue;
            }
            let Some((commit, _)) = self.pending.remove(&oid) else {
                continue;
            };
            for parent in commit.parent_ids() {
                if let Some(n) = self.waiting_children.get_mut(&parent) {
                    *n = n.saturating_sub(1);
                }
                match self.pending.get(&parent) {
                    // 已发现的父:最后一个子也产出了才就绪
                    Some((parent_commit, seq)) => {
                        if self.waiting(&parent) == 0 {
                            let time = parent_commit.time().seconds();
                            self.ready.push((time, Reverse(*seq), parent));
                        }
                    }
                    None => self.discover(parent),
                }
            }
            walked += 1;
            if keep(&commit) {
                out.push(commit_info(&commit));
            }
        }
        (out, walked)
    }
}

pub(super) fn commit_info(commit: &Commit<'_>) -> GitCommitInfo {
    let hash = commit.id().to_string();
    let short_hash = hash[..7.min(hash.len())].to_string();
    GitCommitInfo {
        short_hash,
        message: commit.summary().unwrap_or("").to_string(),
        body: commit.body().map(|s| s.to_string()),
        author: commit.author().name().unwrap_or("unknown").to_string(),
        // 显示用作者时间(见字段注释);排序在 `DateTopoWalk` 里另按提交者时间
        timestamp: commit.author().when().seconds(),
        parent_hashes: commit.parent_ids().map(|id| id.to_string()).collect(),
        hash,
    }
}

/// 把新取到的一页并进已加载列表,返回实际新增的条数。
///
/// 1. **去重**:按 hash 丢掉已有的(游标续页本身不会带回已加载的提交,这里是防线);
/// 2. **跨页拓扑修正**:新提交里若有「父已经排在它前面」的(时钟偏移下迟到的子
///    提交,见 `get_git_log` 上方的说明),对整个列表做一次稳定的拓扑重排,保证
///    父永远在子之后 —— 拓扑图的连线依赖这一点。没有这种情况时一行不挪。
///
/// 每次都按整份列表现建位置表,O(已加载条数)。一页页往下翻的调用方用常驻索引的
/// [`LoadedLog::merge`](语义相同,只做与新页成正比的活)。
pub fn merge_log_page(loaded: &mut Vec<GitCommitInfo>, page: Vec<GitCommitInfo>) -> usize {
    let before = loaded.len();
    let mut known: HashSet<String> = loaded.iter().map(|c| c.hash.clone()).collect();
    for commit in page {
        if known.insert(commit.hash.clone()) {
            loaded.push(commit);
        }
    }
    let added = loaded.len() - before;
    if added > 0 && !new_tail_is_topo_ordered(loaded, before) {
        stable_topo_reorder(loaded);
    }
    added
}

/// 新并入的 `loaded[from..]` 有没有「父排在自己前面」的。旧的部分此前已经是拓扑序,
/// 而新提交全排在旧提交之后,所以只可能是新提交的父在前 —— 只查它们就够。
pub(super) fn new_tail_is_topo_ordered(loaded: &[GitCommitInfo], from: usize) -> bool {
    let pos: HashMap<&str, usize> = loaded
        .iter()
        .enumerate()
        .map(|(i, c)| (c.hash.as_str(), i))
        .collect();
    loaded[from..].iter().enumerate().all(|(k, commit)| {
        commit
            .parent_hashes
            .iter()
            .all(|p| pos.get(p.as_str()).is_none_or(|&pi| pi > from + k))
    })
}

/// 稳定的拓扑重排(子在父前)。
///
/// 每个提交的排序键 = 它自己与它全部已加载祖先里**最靠前的位置**:迟到的子提交
/// 因此被拉到它最早那个父的位置上,再用 Kahn(同键按原位置)排出一个合法拓扑序。
/// 原本就合法的部分键就是自己的位置,次序原样不动。
pub(super) fn stable_topo_reorder(loaded: &mut Vec<GitCommitInfo>) {
    let n = loaded.len();
    let pos: HashMap<&str, usize> = loaded
        .iter()
        .enumerate()
        .map(|(i, c)| (c.hash.as_str(), i))
        .collect();
    let parents: Vec<Vec<usize>> = loaded
        .iter()
        .map(|c| {
            c.parent_hashes
                .iter()
                .filter_map(|p| pos.get(p.as_str()).copied())
                .collect()
        })
        .collect();

    // 子先父后的 Kahn:入度 = 已加载的子提交个数,就绪集合里按 (rank, 原位置) 取最小
    let kahn = |rank: &dyn Fn(usize) -> usize| -> Vec<usize> {
        let mut indegree = vec![0u32; n];
        for &p in parents.iter().flatten() {
            indegree[p] += 1;
        }
        let mut ready: BinaryHeap<Reverse<(usize, usize)>> = (0..n)
            .filter(|&i| indegree[i] == 0)
            .map(|i| Reverse((rank(i), i)))
            .collect();
        let mut order = Vec::with_capacity(n);
        while let Some(Reverse((_, i))) = ready.pop() {
            order.push(i);
            for &p in &parents[i] {
                indegree[p] -= 1;
                if indegree[p] == 0 {
                    ready.push(Reverse((rank(p), p)));
                }
            }
        }
        order
    };

    // 先按原位置求一个合法拓扑序,倒过来就是「父先于子」,正好用来自底向上算键
    let by_index = kahn(&|i| i);
    let mut key = vec![usize::MAX; n];
    for &i in by_index.iter().rev() {
        key[i] = parents[i].iter().map(|&p| key[p]).fold(i, usize::min);
    }
    let mut order = kahn(&|i| key[i]);
    if order.len() < n {
        // 提交图不会有环;真遇上坏数据也别把提交弄丢,排不进去的按原位置垫在后面
        let mut placed = vec![false; n];
        for &i in &order {
            placed[i] = true;
        }
        order.extend((0..n).filter(|&i| !placed[i]));
    }

    let mut slots: Vec<Option<GitCommitInfo>> =
        std::mem::take(loaded).into_iter().map(Some).collect();
    loaded.extend(order.into_iter().filter_map(|i| slots[i].take()));
}

pub fn get_repo_branches(repo_path: &Path) -> Result<Vec<BranchInfo>> {
    let repo = Repository::open(repo_path)?;
    repo_branches_in(&repo, &mut branch::UpstreamResolver::new(&repo))
}

/// [`get_repo_branches`] 的本体(拿已打开的仓库算,分支快照 [`repo_panel_snapshot`] 用)。
pub(super) fn repo_branches_in(
    repo: &Repository,
    upstreams: &mut branch::UpstreamResolver<'_>,
) -> Result<Vec<BranchInfo>> {
    let mut branches = Vec::new();

    // 本地分支
    for branch_result in repo.branches(Some(git2::BranchType::Local))? {
        let (branch, _) = branch_result?;
        let name = branch.name()?.unwrap_or("").to_string();
        if let Some(target) = branch.get().target() {
            // 上游:`branch.<name>.remote/merge` 经 fetch refspec 换算成远程跟踪引用;
            // 没配或引用不在都当没有上游(整批共用一份配置快照,见 `UpstreamResolver`)
            let upstream = upstreams.upstream(&name).map(|(short, _)| short);
            branches.push(BranchInfo {
                name,
                // 按「HEAD 这个符号引用指着谁」判,不按提交比:新建的分支与 main 同一提交时
                // 两条都会被认成当前分支。游离 HEAD 不指向任何分支,一条都不是
                is_head: branch.is_head(),
                is_remote: false,
                commit_hash: target.to_string(),
                upstream,
            });
        }
    }

    // 远程分支
    for branch_result in repo.branches(Some(git2::BranchType::Remote))? {
        let (branch, _) = branch_result?;
        let name = branch.name()?.unwrap_or("").to_string();
        // 跳过 origin/HEAD 这类指针
        if name.ends_with("/HEAD") {
            continue;
        }
        if let Some(target) = branch.get().target() {
            branches.push(BranchInfo {
                name,
                is_head: false,
                is_remote: true,
                commit_hash: target.to_string(),
                upstream: None,
            });
        }
    }

    Ok(branches)
}

pub fn get_commit_files(repo_path: &Path, commit_hash: &str) -> Result<Vec<CommitFileInfo>> {
    let repo = Repository::open(repo_path)?;
    let oid = git2::Oid::from_str(commit_hash)?;
    let commit = repo.find_commit(oid)?;
    commit_files_in(&repo, &commit)
}

/// [`get_commit_files`] 的本体:拿已打开的仓库与提交算(合并查看多个提交时逐个调,
/// 不必每个提交都重开一次仓库)。相对**第一个父**比(合并提交也只比第一父),根提交
/// 对空树比。
pub(crate) fn commit_files_in(
    repo: &Repository,
    commit: &Commit<'_>,
) -> Result<Vec<CommitFileInfo>> {
    let diff = first_parent_diff(repo, commit)?;
    let mut files = Vec::new();
    for delta in diff.deltas() {
        let status = match delta.status() {
            git2::Delta::Added => "added",
            git2::Delta::Deleted => "deleted",
            git2::Delta::Modified => "modified",
            git2::Delta::Renamed => "renamed",
            _ => "modified",
        };
        let path = delta
            .new_file()
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let old_path = if delta.status() == git2::Delta::Renamed {
            delta
                .old_file()
                .path()
                .map(|p| p.to_string_lossy().to_string())
        } else {
            None
        };
        files.push(CommitFileInfo {
            path,
            status: status.to_string(),
            old_path,
        });
    }
    Ok(files)
}

/// 提交相对**第一个父**的树对树 diff(根提交对空树比),已按 `git show` 的默认口径配对
/// 重命名。[`commit_files_in`] 与逐文件增删行数([`get_commit_line_stats`])共用这一份,
/// 两边的文件与顺序一一对应。
pub(crate) fn first_parent_diff<'r>(
    repo: &'r Repository,
    commit: &Commit<'_>,
) -> Result<git2::Diff<'r>> {
    let tree = commit.tree()?;
    let parent_tree = if commit.parent_count() > 0 {
        Some(commit.parent(0)?.tree()?)
    } else {
        None
    };
    let mut diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;
    // 树对树的 diff 默认不做相似度配对:纯重命名会拆成一删一增,下游「仅重命名」的
    // 分支永远走不到。按 `git show` 的默认口径配对(只认重命名、不认复制,相似度 50%);
    // 删除 × 新增太多时退到只认纯改名 / 不配(护栏见 [`find_renames_guarded`]),
    // 大批量搬目录的提交不再一点开就卡几秒
    let mut find = git2::DiffFindOptions::new();
    find.renames(true);
    find_renames_guarded(&mut diff, find)?;
    Ok(diff)
}

pub fn get_commit_file_diff(
    repo_path: &Path,
    commit_hash: &str,
    file_path: &str,
    old_file_path: Option<&str>,
) -> Result<GitDiffResult> {
    let repo = Repository::open(repo_path)?;
    let oid = git2::Oid::from_str(commit_hash)?;
    let commit = repo.find_commit(oid)?;
    let tree = commit.tree()?;

    let parent_tree = if commit.parent_count() > 0 {
        Some(commit.parent(0)?.tree()?)
    } else {
        None
    };

    let old_lookup_path = old_file_path.unwrap_or(file_path);
    diff_tree_texts(
        &repo,
        parent_tree.as_ref().map(|t| (t, old_lookup_path)),
        Some((&tree, file_path)),
    )
}

/// 树里一个文件的文本内容(diff 的一侧)。`Text` 的第二项见 [`decode_side`]。
pub(super) enum TreeText {
    Text(String, Option<TextEncoding>),
    Binary,
    TooLarge,
}

/// 读 `tree` 里 `path` 这个文件的文本。树里没有这个路径(新增 / 删除的那一侧)当空文本;
/// 不是 blob 报错。非 UTF-8 的按 [`decode_side`] 解(GBK 源码也看得了提交 diff),解不出来
/// 当二进制。超过 [`MAX_DIFF_BYTES`] 的整份不做 diff —— 先从对象头取大小再决定读不读,
/// 不把几百 MB 的 blob 整份读进内存。
pub(super) fn read_tree_text(
    repo: &Repository,
    tree: &git2::Tree<'_>,
    path: &str,
) -> Result<TreeText> {
    let entry = match tree.get_path(Path::new(path)) {
        Ok(entry) => entry,
        Err(_) => return Ok(TreeText::Text(String::new(), None)),
    };
    if blob_too_large(repo, entry.id())? {
        return Ok(TreeText::TooLarge);
    }
    let obj = entry.to_object(repo)?;
    let blob = obj.as_blob().ok_or_else(|| anyhow!("not a blob"))?;
    Ok(match decode_side(blob.content(), blob.is_binary()) {
        Some((text, encoding)) => TreeText::Text(text, encoding),
        None => TreeText::Binary,
    })
}

/// blob / 工作区文件的字节 → diff 一侧的文本,第二项是非 UTF-8 时解出来的编码
/// (`None` = 原样的 UTF-8,按块操作只认这种);整体 `None` = 二进制。`git_binary` 是
/// git 自己的判定(`Blob::is_binary`;工作区那一侧没有,传 `false`)。
///
/// - 合法 UTF-8 走原来那条路,一字不变(BOM 也原样留着,按块暂存拼回去才对得上);
/// - git 判成二进制的只放行带 UTF-16 BOM 的 —— git 一律把 UTF-16 当二进制;
/// - 其余非 UTF-8 交给 [`crate::encoding::decode`](GBK / Shift_JIS …),有 NUL 的仍是二进制。
pub(super) fn decode_side(
    bytes: &[u8],
    git_binary: bool,
) -> Option<(String, Option<TextEncoding>)> {
    if !git_binary {
        if let Ok(text) = std::str::from_utf8(bytes) {
            return Some((text.to_owned(), None));
        }
    } else if !crate::encoding::has_utf16_bom(bytes) {
        return None;
    }
    match crate::encoding::decode(bytes.to_vec()) {
        Decoded::Text { text, encoding } => Some((text, (!encoding.is_utf8()).then_some(encoding))),
        Decoded::Binary => None,
    }
}

/// diff 结果上记哪一侧的编码:新的一侧优先(用户眼前那份),两侧都是 UTF-8 时 `None`。
pub(super) fn diff_encoding(
    old: Option<TextEncoding>,
    new: Option<TextEncoding>,
) -> Option<TextEncoding> {
    new.or(old)
}

/// 两棵树里各一个文件 → diff。`None` 的一侧当空文本(文件新增 / 删除)。
///
/// 判定顺序:先读新的一侧(二进制 / 过大当场返回),再读旧的一侧(同样)。两侧套同一条
/// [`MAX_DIFF_BYTES`]:旧实现旧侧不限,删掉一个 200 MB 的文本文件再点开它,峰值内存是文件
/// 的三四倍、还要算上好几秒。大小在读内容之前判,所以超限的文件一律报「过大」(是不是
/// 二进制要读了内容才知道)。
/// 单提交 diff([`get_commit_file_diff`])与合并查看的两版本 diff
/// ([`get_revision_file_diff`])共用这一份,口径不分叉。
pub(crate) fn diff_tree_texts(
    repo: &Repository,
    old: Option<(&git2::Tree<'_>, &str)>,
    new: Option<(&git2::Tree<'_>, &str)>,
) -> Result<GitDiffResult> {
    let (new_content, new_encoding) = match new {
        Some((tree, path)) => match read_tree_text(repo, tree, path)? {
            TreeText::Text(text, encoding) => (text, encoding),
            TreeText::Binary => return Ok(binary_diff_result()),
            TreeText::TooLarge => return Ok(too_large_diff_result()),
        },
        None => (String::new(), None),
    };
    let (old_content, old_encoding) = match old {
        Some((tree, path)) => match read_tree_text(repo, tree, path)? {
            TreeText::Text(text, encoding) => (text, encoding),
            TreeText::Binary => return Ok(binary_diff_result()),
            TreeText::TooLarge => return Ok(too_large_diff_result()),
        },
        None => (String::new(), None),
    };
    let mut result = diff_two_texts(old_content, new_content);
    result.encoding = diff_encoding(old_encoding, new_encoding);
    Ok(result)
}
