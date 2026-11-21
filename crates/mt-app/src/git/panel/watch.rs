use super::*;

/// 工作区监听清单不是逐个非递归挂全的时候记一行(见 `git_dir_watch` 模块注释「递归子树」)。
pub(super) fn log_worktree_plan(plan: &WorktreeWatchDirs) {
    let limit = mt_project::git::WORKTREE_WATCH_DIR_LIMIT;
    if !plan.trees.is_empty() {
        tracing::info!(
            "[git] 工作区目录多于 {limit} 个,改为递归监听 {} 棵子树",
            plan.trees.len()
        );
    } else if plan.truncated {
        tracing::info!(
            "[git] 工作区目录多于 {limit} 个,只监听浅层的那些,抽屉开着时深处每 {}s 兜底重取一次",
            PARTIAL_POLL.as_secs()
        );
    }
}

/// 一轮整面板重取的来由(见 [`GitPanel::reload_views`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Round {
    /// 手动 ↻:按钮转圈、历史标「刷新中…」、文件树无条件重取
    Manual,
    /// 面板 / 弹窗里的写操作做完
    Write,
    /// `.git` 监听判出引用变了:历史静默重载(没变不重画)
    Watch,
}

/// pty 嗅探命中之后走哪条路(见模块注释「自动刷新」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PtyRoute {
    /// `.git` 监听挂在选中的仓库上:只叫它比一次签名,没变不刷
    Signature,
    /// 监听没挂上(列目录失败 / 正在换目标):回落原版的两个 500ms 去抖
    Debounce,
}

pub(super) fn pty_route(watching_selected: bool) -> PtyRoute {
    if watching_selected {
        PtyRoute::Signature
    } else {
        PtyRoute::Debounce
    }
}

/// 两份签名的差别该刷什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SigVerdict {
    /// 没变
    Unchanged,
    /// 只有索引那半变了:只重取「更改」与冲突数
    IndexOnly,
    /// 引用变了(或有一边读不到):整轮重取
    Refs,
}

pub(super) fn sig_verdict(known: Option<RepoSignature>, new: Option<RepoSignature>) -> SigVerdict {
    match (known, new) {
        (Some(old), Some(new)) if old == new => SigVerdict::Unchanged,
        (Some(old), Some(new)) if old.refs == new.refs => SigVerdict::IndexOnly,
        _ => SigVerdict::Refs,
    }
}

/// 一次检查读到的状态怎么处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CheckFate {
    /// 读得不比手上的新(手上那份是之后读的,或之后已经发过整轮重取):丢
    Covered,
    /// 分支快照在途:它落地时签名会换,先压着,落地后再比
    Defer,
    /// 当场比
    Compare,
}

/// `observed` = 这次检查开始读的时刻;`known` = 手上签名的读取时刻与最近一轮整轮重取的发出
/// 时刻里晚的那个。
pub(super) fn check_fate(
    observed: Instant,
    known: Option<Instant>,
    snapshot_in_flight: bool,
) -> CheckFate {
    if known.is_some_and(|known| observed <= known) {
        CheckFate::Covered
    } else if snapshot_in_flight {
        CheckFate::Defer
    } else {
        CheckFate::Compare
    }
}

/// 分支快照落地时,签名与冲突数要不要沿用手上那份:手上那份是这趟快照发出**之后**才读的。
pub(super) fn keep_newer_observation(
    observed_at: Option<Instant>,
    snapshot_issued: Instant,
) -> bool {
    observed_at.is_some_and(|at| at > snapshot_issued)
}

/// 面板自己写完索引后读的那份能不能直接记作「已知」:两边都读得到、引用那半一致(只是
/// 索引变了),且不比手上的旧。
pub(super) fn absorbs_own_write(
    known: Option<RepoSignature>,
    known_at: Option<Instant>,
    observed: &RepoObservation,
) -> bool {
    let (Some(known), Some(new)) = (known, observed.signature) else {
        return false;
    };
    known.refs == new.refs && known_at.is_none_or(|at| at <= observed.at)
}

/// 仓库列表画出来取决于的那几项(`GitRepoInfo` 没有 `PartialEq`)。
pub(super) fn repo_list_key(repos: &[GitRepoInfo]) -> Vec<(String, PathBuf, Option<String>, bool)> {
    repos
        .iter()
        .map(|r| {
            (
                r.name.clone(),
                r.path.clone(),
                r.current_branch.clone(),
                r.is_worktree,
            )
        })
        .collect()
}

/// 发现仓库回来之后要不要再取一次分支快照:选中的仓库换了,或者还一份都没取过(切项目
/// 吃了仓库缓存之后的第一轮)且没有在途的那一趟。
///
/// 选中仓库没变时不重取:「刷新」本来就同时发了一趟分支快照,这里再发会把它作废(令牌
/// 前移),横幅 / 计数要等扫盘(深 5 层)做完再从头取一遍 —— 外部 `git merge --abort`
/// 后冲突横幅拖到约 3.3s 才消失(E2E r4 A11)。
///
/// `loading` 是「有快照在途」(`snapshot_in_flight`),不是刷新按钮转不转圈 —— 自动刷新
/// 不转圈,但同样在途。
pub(super) fn branches_stale_after_discovery(
    before: &str,
    after: &str,
    empty: bool,
    loading: bool,
) -> bool {
    before != after || (empty && !loading)
}

/// detached HEAD 的显示名是带括号的短 hash(`"(1a2b3c4)"`)。
pub(super) fn is_detached_label(label: &str) -> bool {
    label.starts_with('(') && label.ends_with(')')
}

/// detached 显示名里的短 hash(剥掉两侧括号)。文案自己带括号(「分离 HEAD(…)」),
/// 直接套显示名会成双层括号;不是 detached 显示名的原样返回。
pub(crate) fn detached_hash(label: &str) -> &str {
    label
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(label)
}

/// 仓库栏写按钮(Fetch / 拉取 / 推送 / 贮藏)的提示该写哪一种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteTip {
    /// 正在跑的就是它:「Fetch 中…」
    Running,
    /// 有别的写操作在跑 —— 面板自己的(推送在后台跑着),或别的区占着写操作闸:
    /// 按钮置灰,提示写「另一项 Git 操作正在进行」,与提交按钮同一句
    Waiting,
    /// 闲着:常规说明
    Normal,
}

/// `op` = 这颗按钮对应的写操作(贮藏按钮没有自己的进行中态,给 `None`)。
pub(super) fn write_tip(busy: Option<GitBusy>, gated: bool, op: Option<GitBusy>) -> WriteTip {
    if busy.is_some() && busy == op {
        WriteTip::Running
    } else if busy.is_some() || gated {
        WriteTip::Waiting
    } else {
        WriteTip::Normal
    }
}

/// 进行中操作的名词(「合并」「变基」…),拼进横幅与提示。
pub(crate) fn op_noun(kind: RepoOpKind) -> &'static str {
    t(
        "gitBranch",
        match kind {
            RepoOpKind::Merge => "kindMerge",
            RepoOpKind::Rebase => "kindRebase",
            RepoOpKind::CherryPick => "kindCherryPick",
            RepoOpKind::Revert => "kindRevert",
        },
    )
}

/// 拉取方式的名字(tooltip 与菜单共用)。
pub(super) fn pull_mode_name(mode: PullMode) -> &'static str {
    t(
        "gitBranch",
        match mode {
            PullMode::Merge => "modeMerge",
            PullMode::Rebase => "modeRebase",
            PullMode::FfOnly => "modeFfOnly",
        },
    )
}

/// 用户 git 配置里那种拉取方式的名字:`rebase=merges` 单独写成「变基(保留合并)」。
pub(super) fn config_pull_name(config: &PullConfig) -> &'static str {
    if config.mode == PullMode::Rebase && config.rebase_merges {
        t("gitBranch", "modeRebaseMerges")
    } else {
        pull_mode_name(config.mode)
    }
}

/// 分支徽章的 tooltip:分支名 + 上游 + ↑↓ 的含义 + 操作提示,一行一条。
///
/// `viewing` = 正在查看的另一个分支(未检出)—— 此时计数不显示,第一行说明这一点。
pub(super) fn branch_tip_lines(
    branch: &str,
    tracking: Option<&BranchTracking>,
    detached: bool,
    viewing: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    if viewing {
        lines.push(tr!("gitBranch", "tipViewing", branch = branch));
    } else if detached {
        lines.push(tr!(
            "gitBranch",
            "tipDetached",
            commit = detached_hash(branch)
        ));
    } else {
        lines.push(branch.to_string());
        match tracking {
            Some(tracking) => {
                lines.push(tr!(
                    "gitBranch",
                    "tipUpstream",
                    upstream = tracking.upstream.as_str()
                ));
                if tracking.ahead == 0 && tracking.behind == 0 {
                    lines.push(t("gitBranch", "tipInSync").to_string());
                }
                if tracking.ahead > 0 {
                    lines.push(tr!("gitBranch", "tipAhead", count = tracking.ahead));
                }
                if tracking.behind > 0 {
                    lines.push(tr!("gitBranch", "tipBehind", count = tracking.behind));
                }
            }
            None => lines.push(t("gitBranch", "tipNoUpstream").to_string()),
        }
    }
    lines.push(t("gitBranch", "tipOpenBranches").to_string());
    lines
}

impl GitPanel {
    // ── `.git` 目录监听 ───────────────────────────────────

    /// 让监听目标跟上眼下的样子:抽屉开着 = 选中的仓库上挂 `.git` 与工作区两路;收着 = 只留
    /// 工作区那一路替文件树听 [`Self::hidden_repo`](见模块注释「抽屉与窗口」)。两路各自比,
    /// 没变的那一路不动(抽屉开合时工作区监听不必撤了重挂)。目录在后台列(要读盘)。
    pub(super) fn retarget_watch(&mut self, cx: &mut Context<Self>) {
        let local = self.local_project();
        let want_dir = self.repo_context().and_then(|context| {
            self.visible
                .then(|| {
                    context
                        .local_path()
                        .map(|path| path.to_string_lossy().into_owned())
                })
                .flatten()
        });
        let want_work = if self.visible {
            want_dir.clone()
        } else {
            self.hidden_repo.clone().filter(|_| local)
        };
        let dir_ok = want_dir.as_deref() == self.dir_watch.repo();
        let work_ok = want_work.as_deref() == self.work_watch.repo();
        if !(dir_ok && work_ok) {
            // 在途的那趟列目录作废:它可能正替刚撤掉的目标列
            self.watch_request += 1;
        }
        if !dir_ok {
            self.dir_watch.clear();
        }
        if !work_ok {
            self.work_watch.clear();
            self.index_lock.reset();
            self.partial_polled = None;
        }
        self.sync_feeder(cx);
        let dir_repo = want_dir.filter(|_| !dir_ok);
        let work_repo = want_work.filter(|_| !work_ok);
        if dir_repo.is_none() && work_repo.is_none() {
            // 同项目重新打开、两路都还挂着:当场比一次签名(见 `set_visible`)
            if std::mem::take(&mut self.catch_up) {
                self.check_git_dir_change(cx);
            }
            return;
        }
        let req = self.watch_request;
        // These targets were obtained only through local_path in the watcher gate.
        let dir_task = dir_repo.as_ref().map(PathBuf::from);
        let work_task = work_repo.as_ref().map(PathBuf::from);
        cx.spawn(async move |this, cx| {
            let (git_dirs, work_dirs) = cx
                .background_executor()
                .spawn(async move {
                    let git_dirs = dir_task.map(|repo| {
                        mt_project::git::git_dir_layout(&repo).map(|layout| {
                            let dirs = mt_project::git::git_watch_dirs(&layout);
                            (layout, dirs)
                        })
                    });
                    let work_dirs =
                        work_task.map(|repo| mt_project::git::worktree_watch_dirs(&repo));
                    (git_dirs, work_dirs)
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.watch_request != req {
                    return;
                }
                match (dir_repo, git_dirs) {
                    (Some(repo), Some(Ok((layout, dirs)))) => {
                        this.dir_watch.set_target(repo, layout, dirs);
                    }
                    // 监听挂不上不影响使用:手动 ↻ 与 pty 嗅探(回落去抖那条)照旧
                    (_, Some(Err(err))) => tracing::warn!("[git] 监听 .git 失败: {err:#}"),
                    _ => {}
                }
                match (work_repo, work_dirs) {
                    (Some(repo), Some(Ok(plan))) => {
                        log_worktree_plan(&plan);
                        this.work_watch.set_target(repo, plan);
                    }
                    (_, Some(Err(err))) => tracing::warn!("[git] 监听工作区失败: {err:#}"),
                    _ => {}
                }
                this.sync_feeder(cx);
                // 同项目重新打开:收着期间引用 / 索引变没变,当场比一次签名(见 `set_visible`)
                if std::mem::take(&mut this.catch_up) {
                    this.check_git_dir_change(cx);
                }
            });
        })
        .detach();
    }

    /// 抽屉收着时替文件树挂工作区监听的仓库(见模块注释「抽屉与窗口」):面板对当前项目选过
    /// 的那个(没换过项目就是选中的,换过就查 [`REPO_CACHE`]);都没有就在后台找项目根所属
    /// 的仓库 —— 只往上找,不像 `discover_git_repos` 那样扫盘。父目录型项目在抽屉打开过之前
    /// 找不到,不替它挂。抽屉开着时什么都不做。
    pub(super) fn refresh_hidden_repo(&mut self, cx: &mut Context<Self>) {
        self.hidden_request += 1;
        if self.visible {
            return;
        }
        let project = self
            .project_context
            .clone()
            .filter(|context| context.local_path().is_some());
        let Some(project) = project else {
            self.hidden_repo = None;
            self.retarget_watch(cx);
            return;
        };
        let known = if !self.stale
            && self.project_context.as_ref() == Some(&project)
            && !self.selected_repo.is_empty()
        {
            Some(self.selected_repo.clone())
        } else {
            REPO_CACHE
                .with(|c| {
                    c.borrow()
                        .get(&project.key())
                        .map(|(_, selected)| selected.clone())
                })
                .filter(|selected| !selected.is_empty())
        };
        self.hidden_repo = known.clone();
        self.retarget_watch(cx);
        if known.is_some() {
            return;
        }
        let req = self.hidden_request;
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    project
                        .local_path()
                        .and_then(mt_project::git::enclosing_repo)
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.hidden_request != req || this.visible {
                    return;
                }
                this.hidden_repo = found.map(|path| path.to_string_lossy().to_string());
                this.retarget_watch(cx);
            });
        })
        .detach();
    }

    /// 抽屉收着、窗口可见、工作区监听挂着时,起一个等唤醒口的任务替文件树取批次(见
    /// [`Self::feed_file_tree`]);别的情形撤掉。窗口最小化时它停着,监听照记,恢复时先把攒下
    /// 的那批取掉 —— 那批开头的一声叫在了已经撤掉的口上。
    pub(super) fn sync_feeder(&mut self, cx: &mut Context<Self>) {
        let want = !self.visible && self.window_visible && self.work_watch.repo().is_some();
        if !want {
            if self._feeder.take().is_some() {
                self.work_watch.set_waker(None);
            }
            return;
        }
        if self._feeder.is_some() {
            return;
        }
        let (tx, mut rx) = mpsc::unbounded();
        self.work_watch.set_waker(Some(tx));
        self._feeder = Some(cx.spawn(async move |this, cx| {
            loop {
                // 取到没有攒着的为止(没到点就睡到点);然后等下一批开头的那一声
                loop {
                    let Ok(wait) = this.update(cx, |this: &mut Self, cx| this.feed_file_tree(cx))
                    else {
                        return;
                    };
                    let Some(wait) = wait else {
                        break;
                    };
                    cx.background_executor().timer(wait).await;
                }
                if rx.next().await.is_none() {
                    return;
                }
            }
        }));
    }

    /// 抽屉收着时工作区监听攒下的一批:只把路径交给文件树(它自己按忽略规则过、只认没在
    /// 监听的那些 —— 折起来的目录的汇总字母靠它跟上),目录出现 / 消失顺带重列清单。不重取
    /// 「更改」,也就不看 `index.lock`。返回还要等多久再来取(`None` = 没有攒着的)。
    pub(super) fn feed_file_tree(&mut self, cx: &mut Context<Self>) -> Option<Duration> {
        if self.visible {
            // 抽屉开了:节拍接管
            return None;
        }
        let Some(batch) = self.work_watch.take_due() else {
            return self
                .work_watch
                .due_in()
                .map(|wait| wait.max(Duration::from_millis(10)));
        };
        git_watch::request_paths(
            git_watch::Subscriber::FileTree,
            &batch.paths,
            batch.overflow,
        );
        if batch.dirs_changed {
            self.replan_worktree(batch.last, cx);
        }
        None
    }

    /// 后台重列工作区监听清单(抽屉收着时,有目录出现 / 消失)。上一趟还没回来就把「要重列」
    /// 退回去,下一批再来。
    pub(super) fn replan_worktree(&mut self, last: Instant, cx: &mut Context<Self>) {
        let Some(repo) = self.work_watch.repo().map(str::to_string) else {
            return;
        };
        if self.work_checking {
            self.work_watch.requeue(WorktreeBatch {
                paths: Vec::new(),
                overflow: false,
                dirs_changed: true,
                last,
            });
            return;
        }
        let Some(path) = self
            .project_context
            .as_ref()
            .map(|context| context.with_path(repo.clone()))
            .and_then(|context| context.local_path().map(std::path::Path::to_path_buf))
        else {
            return;
        };
        self.work_checking = true;
        cx.spawn(async move |this, cx| {
            let plan = cx
                .background_executor()
                .spawn(async move { mt_project::git::worktree_watch_dirs(&path) })
                .await;
            let _ = this.update(cx, |this: &mut Self, _cx| {
                this.work_checking = false;
                if this.work_watch.repo() == Some(repo.as_str())
                    && let Ok(plan) = plan
                {
                    this.work_watch.update_dirs(plan);
                }
            });
        })
        .detach();
    }

    /// 工作区安静下来的一批变化(`batch`),或压着等 `index.lock` 的那次重取到点复查(`None`):
    /// 后台按忽略规则过一遍(全是被忽略的路径就不刷)、看一眼锁,有目录出现 / 消失就顺带重列
    /// 监听清单;要刷的话**只**重取「更改」列表 —— 历史与分支只随 `.git` 变化走,大仓库里改个
    /// 文件不至于把整个面板拖着重来。这批之后已经有一趟取数开跑的不再要;git 正写着索引时
    /// 「更改」的重取先压着、最多等一阵,交给文件树的路径不等(见模块注释「自动刷新」)。
    pub(super) fn check_worktree_change(
        &mut self,
        batch: Option<WorktreeBatch>,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.work_watch.repo().map(str::to_string) else {
            return;
        };
        if self.work_checking {
            // 上一批还没查完(节拍那边已经不在这时取,这里只是兜底):退回去下一拍再交
            if let Some(batch) = batch {
                self.work_watch.requeue(batch);
            }
            return;
        }
        let Some(path) = self
            .project_context
            .as_ref()
            .map(|context| context.with_path(repo.clone()))
            .and_then(|context| context.local_path().map(std::path::Path::to_path_buf))
        else {
            return;
        };
        self.work_checking = true;
        let git_dir = self
            .dir_watch
            .layout()
            .filter(|_| self.dir_watch.repo() == Some(repo.as_str()))
            .map(|layout| layout.git_dir);
        cx.spawn(async move |this, cx| {
            let (at, probe, checked) = cx
                .background_executor()
                .spawn(async move {
                    let at = Instant::now();
                    // git 正写着索引(长一点的变基 / 检出 / 贮藏)时工作区是中间态;锁在不在、
                    // 有多老,交给 `IndexLockGate` 定等不等
                    let probe = git_dir.as_deref().map(LockProbe::read).unwrap_or_default();
                    let checked = batch.map(|batch| {
                        let relevant = batch.overflow
                            || mt_project::git::any_unignored_change(&path, &batch.paths)
                                .unwrap_or(true);
                        let dirs = batch
                            .dirs_changed
                            .then(|| mt_project::git::worktree_watch_dirs(&path).ok())
                            .flatten();
                        (batch, relevant, dirs)
                    });
                    (at, probe, checked)
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.work_checking = false;
                if this.work_watch.repo() != Some(repo.as_str()) || this.selected_repo != repo {
                    return;
                }
                let mut last = None;
                if let Some((batch, relevant, dirs)) = checked {
                    if let Some(plan) = dirs {
                        this.work_watch.update_dirs(plan);
                    }
                    if relevant {
                        // 文件树只认它自己没在监听的那些路径(折起来的目录,汇总字母要跟上)。
                        // 不等锁:它取状态与索引锁无关,等了反倒让文件树也跟着卡住
                        git_watch::request_paths(
                            git_watch::Subscriber::FileTree,
                            &batch.paths,
                            batch.overflow,
                        );
                        last = Some(batch.last);
                    }
                }
                // 锁新就压着等它写完(到点由节拍复查),锁没了 / 太老了就取;这批之后已经有
                // 一趟开跑(`.git` 那一路刚整轮刷过之类)就不再要
                if let Some(last) = this.index_lock.offer(last, probe, at)
                    && !this.changes.read(cx).load_covers(last)
                {
                    this.changes.update(cx, |c, cx| c.load(cx));
                }
            });
        })
        .detach();
    }

    /// `.git` 下攒够一批安静下来的变化(或 pty 命中 / 重新打开 / 窗口恢复呈现叫的一次):
    /// 后台读一份 [`RepoObservation`],按「签名的先后」决定刷什么(见模块注释)。顺带重列
    /// 监听目录(新出现的分支前缀目录要补挂)。监听没挂上时什么都不做。
    pub(super) fn check_git_dir_change(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo_context() else {
            return;
        };
        let layout = if repo.local_path().is_some() {
            if self.dir_watch.repo() != Some(repo.path()) {
                return;
            }
            let Some(layout) = self.dir_watch.layout() else {
                return;
            };
            Some(layout)
        } else {
            None
        };
        if self.sig_checking {
            self.sig_recheck = true;
            return;
        }
        self.sig_checking = true;
        self.sig_request += 1;
        let req = self.sig_request;
        let task_repo = repo.clone();
        cx.spawn(async move |this, cx| {
            let executor = task_repo.clone();
            let (observed, dirs) = mt_remote::run_git_on(&executor, move || {
                let at = Instant::now();
                let observed = RepoObservation {
                    at,
                    signature: task_repo.repo_signature().ok(),
                    op: task_repo.repo_operation_state().ok(),
                };
                let dirs = layout.map(|layout| mt_project::git::git_watch_dirs(&layout));
                (observed, dirs)
            })
            .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.sig_request != req || this.repo_context().as_ref() != Some(&repo) {
                    return;
                }
                this.sig_checking = false;
                if repo.is_remote()
                    && (!this.visible || !this.window_visible || observed.signature.is_none())
                {
                    // Retain the last history snapshot during hidden or failed polls.
                    // Changes loading reports transport errors without resetting history.
                    this.sig_recheck = false;
                    return;
                }
                if std::mem::take(&mut this.sig_recheck) {
                    // 期间又有变化:这份签名可能已经旧了,下一拍重查(变化时刻按现在算)
                    this.dir_watch.poke();
                }
                if let Some(dirs) = dirs {
                    this.dir_watch.update_dirs(dirs);
                }
                let known = this.round_issued.max(this.sig_observed_at);
                match check_fate(observed.at, known, this.snapshot_in_flight) {
                    // 这份读得比手上的旧(或之后已整轮重取过):没什么可补的
                    CheckFate::Covered => this.settle_pty_hit(observed.at, cx),
                    // 分支快照还在路上:等它落地拿新签名再比
                    CheckFate::Defer => this.deferred_check = Some(observed),
                    CheckFate::Compare => this.apply_observation(observed, cx),
                }
            });
        })
        .detach();
    }

    /// 拿一份读到的状态与手上的签名比,决定刷什么(见模块注释「自动刷新」)。比手上的旧就丢。
    pub(super) fn apply_observation(&mut self, observed: RepoObservation, cx: &mut Context<Self>) {
        if self.sig_observed_at.is_some_and(|at| observed.at < at) {
            self.settle_pty_hit(observed.at, cx);
            return;
        }
        match sig_verdict(self.meta.signature, observed.signature) {
            // 没变:别的程序只是刷新了索引的 stat 缓存之类
            SigVerdict::Unchanged => {}
            // 只动了索引(终端里 `git add`、解决了一个冲突):重取「更改」区,冲突数(横幅)
            // 直接用读到的;文件树按签名核一遍(解决冲突后的 C 要消掉)。分支快照不必重取,
            // 理由见模块注释
            SigVerdict::IndexOnly => {
                self.meta.signature = observed.signature;
                self.sig_observed_at = Some(observed.at);
                if let Some(op) = observed.op
                    && op != self.meta.op
                {
                    self.meta.op = op;
                    cx.notify();
                }
                self.changes.update(cx, |c, cx| c.load(cx));
                git_watch::request_check(git_watch::Subscriber::FileTree);
            }
            SigVerdict::Refs => {
                self.meta.signature = observed.signature;
                self.sig_observed_at = Some(observed.at);
                self.reload_views(Round::Watch, cx);
            }
        }
        self.settle_pty_hit(observed.at, cx);
    }

    /// 走签名那条路的 pty 命中,在 `checked_at` 开始的那次检查之后收尾:签名没变、也没有
    /// 哪趟「更改」取数在命中之后开跑,就补一次只重取「更改」(子模块里的提交,见模块注释)。
    /// 命中晚于这次检查的留给下一次(poke 过,一定还有一次)。
    pub(super) fn settle_pty_hit(&mut self, checked_at: Instant, cx: &mut Context<Self>) {
        let Some(hit) = self.pty_hit_at.filter(|hit| *hit <= checked_at) else {
            return;
        };
        self.pty_hit_at = None;
        if !self.changes.read(cx).load_covers(hit) {
            self.changes.update(cx, |c, cx| c.load(cx));
        }
    }

    /// 「更改」区自己的暂存 / 丢弃 / 冲突处理写完了:写完当场读的那份签名与冲突数记作
    /// 「已知」,紧接着那次 `.git` 检查比出来没变就不再刷一遍(见模块注释「签名的先后」)。
    /// 引用那半与手上的对不上(期间别处动了引用)就不收,留给检查去判。
    pub(super) fn absorb_own_write(&mut self, observed: RepoObservation, cx: &mut Context<Self>) {
        if !absorbs_own_write(self.meta.signature, self.sig_observed_at, &observed) {
            return;
        }
        self.meta.signature = observed.signature;
        self.sig_observed_at = Some(observed.at);
        if let Some(op) = observed.op
            && op != self.meta.op
        {
            // 解决了一个冲突:横幅上的冲突数跟着变
            self.meta.op = op;
            cx.notify();
        }
    }
}
