use super::*;

impl GitPanel {
    /// 跑一个 git 写操作(见模块注释「写操作」)。
    ///
    /// `op` 占着仓库的写操作闸在后台执行器上跑(阻塞 CLI,见 [`git_ui::spawn_gated`]);
    /// `done` 拿到成功结果后在主线程上推摘要 / 接着开下一个确认框。失败统一弹详情框,
    /// 标题是「{动作}失败」。无论成败都整面板刷新([`Self::refresh_after_write`])—— 失败也
    /// 可能已经改了一半(合并停在冲突上之类)。闸被别的区占着时不执行,推一条 toast 说明。
    pub(crate) fn run_write_op<T: Send + 'static>(
        &mut self,
        busy: GitBusy,
        op: impl FnOnce(&GitContext) -> anyhow::Result<T> + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo_context() else {
            return;
        };
        self.run_write_op_on(&repo, busy, op, done, window, cx);
    }

    pub(crate) fn run_write_op_on<T: Send + 'static>(
        &mut self,
        repo: &GitContext,
        busy: GitBusy,
        op: impl FnOnce(&GitContext) -> anyhow::Result<T> + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = self.repo_context().as_ref() == Some(repo);
        if active && self.busy.is_some() {
            return;
        }
        let completed_repo = repo.clone();
        let this = cx.entity().downgrade();
        let started = git_ui::spawn_gated(
            repo,
            op,
            move |result, window, cx| {
                let _ = this.update(cx, |this: &mut Self, cx| {
                    let active = this.repo_context().as_ref() == Some(&completed_repo);
                    if active {
                        this.busy = None;
                    }
                    match result {
                        Ok(value) => {
                            this.feedback_context = Some(completed_repo.clone());
                            done(this, value, window, cx);
                            this.feedback_context = None;
                        }
                        Err(err) => {
                            tracing::warn!("[git] {}失败: {err:#}", busy.label());
                            git_ui::show_git_error(
                                tr!("gitBranch", "opFailed", op = busy.label()),
                                git_ui::repo_text(
                                    &this.store,
                                    &completed_repo,
                                    git_ui::error_text_for_store(&err, &this.store, cx),
                                    cx,
                                ),
                                window,
                                cx,
                            );
                        }
                    }
                    if active {
                        this.refresh_after_write(cx);
                        cx.notify();
                    }
                    this.feedback_context = None;
                });
            },
            window,
            cx,
        );
        if !started {
            self.notify_result(git_ui::gate_busy_text().to_string(), Tone::Attention, cx);
            return;
        }
        if active {
            self.busy = Some(busy);
            cx.notify();
        }
    }

    /// `git fetch --all`。
    pub(crate) fn fetch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_write_op(
            GitBusy::Fetch,
            GitContext::git_fetch_all,
            |this, outcome, _window, cx| {
                let message = if outcome.updated_refs == 0 {
                    t("gitBranch", "fetchUpToDate").to_string()
                } else {
                    tr!("gitBranch", "fetched", count = outcome.updated_refs)
                };
                this.notify_result(message, Tone::Info, cx);
            },
            window,
            cx,
        );
    }

    /// tooltip 里的拉取方式:手选过的照名字写;没手选时写「跟随配置:<配置里的方式>」。
    pub(super) fn pull_mode_label(&self) -> String {
        match PULL_MODE.with(Cell::get) {
            Some(mode) => pull_mode_name(mode).to_string(),
            None => tr!(
                "gitBranch",
                "modeFollowConfig",
                mode = config_pull_name(&self.meta.pull_config)
            ),
        }
    }

    /// 拉取。手选过方式就带对应参数;没手选时**不带方式参数**,完全交给用户的 git 配置
    /// (`branch.<b>.rebase`、`rebase=merges` / `interactive` 都由 git 自己读)——
    /// 折算成三种方式再带参数会把 `merges` 降成 `--rebase`、拍平合并提交。
    pub(super) fn pull(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = PULL_MODE.with(Cell::get);
        self.run_write_op(
            GitBusy::Pull,
            move |repo| match mode {
                Some(mode) => repo.git_pull_with(mode),
                None => repo.git_pull_by_config(),
            },
            |this, outcome, _window, cx| {
                let (message, tone) = match outcome {
                    PullOutcome::UpToDate => {
                        (t("gitBranch", "pullUpToDate").to_string(), Tone::Info)
                    }
                    PullOutcome::Updated { commits } => {
                        (tr!("gitBranch", "pulled", count = commits), Tone::Info)
                    }
                    PullOutcome::Conflicts { files } => (
                        tr!("gitBranch", "pullConflicts", count = files),
                        Tone::Attention,
                    ),
                };
                this.notify_result(message, tone, cx);
            },
            window,
            cx,
        );
    }

    /// 打开「推送」对话框,预选 `branch`(`None` = 当前分支)。仓库栏推送按钮、分支弹窗的
    /// 「推送」、「提交并推送」提交成功之后都走这里。
    pub(crate) fn open_push_dialog(
        &mut self,
        branch: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_repo.is_empty() || self.busy.is_some() {
            return;
        }
        let Some(repo) = self.repo_context() else {
            return;
        };
        crate::git::push::open(cx.entity(), repo, branch, window, cx);
    }

    /// 打开贮藏列表(仓库栏贮藏按钮)。有写操作在跑时不开 —— 贮藏的改写同样要抢
    /// `index.lock`。
    pub(crate) fn open_stash_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_repo.is_empty() || self.busy.is_some() {
            return;
        }
        let Some(repo) = self.repo_context() else {
            return;
        };
        let on_changed = self.stash_on_changed(false, cx);
        crate::git::stash::open_list(self.store.clone(), repo, on_changed, window, cx);
    }

    /// 检出 `branch` 会被本地改动挡住(分支弹窗检出前的预检,见 `git_branches::checkout`):
    /// 开「贮藏后检出」确认框。
    pub(crate) fn open_smart_checkout_on(
        &mut self,
        repo: &GitContext,
        branch: String,
        conflicts: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let repo = repo.clone();
        let on_changed = self.stash_on_changed_on(Some(repo.clone()), true, cx);
        crate::git::stash::open_smart_checkout(
            self.store.clone(),
            repo,
            branch,
            conflicts,
            on_changed,
            window,
            cx,
        );
    }

    /// 贮藏弹窗操作之后的回调:整面板刷新。`follow_head` = 顺带回到跟随 HEAD(检出类,
    /// 同普通检出)。弱引用:回调跟着弹窗走,面板没了就什么都不做。
    pub(super) fn stash_on_changed(
        &self,
        follow_head: bool,
        cx: &mut Context<Self>,
    ) -> Rc<dyn Fn(&mut App)> {
        self.stash_on_changed_on(self.repo_context(), follow_head, cx)
    }

    pub(super) fn stash_on_changed_on(
        &self,
        repo: Option<GitContext>,
        follow_head: bool,
        cx: &mut Context<Self>,
    ) -> Rc<dyn Fn(&mut App)> {
        let panel = cx.entity().downgrade();
        Rc::new(move |cx: &mut App| {
            let _ = panel.update(cx, |p, cx| {
                if p.repo_context() != repo {
                    return;
                }
                if follow_head {
                    p.set_view_branch(None, cx);
                }
                p.refresh_after_write(cx);
            });
        })
    }

    /// 推送对话框里推送进行中 / 结束时同步一下按钮状态。
    pub(crate) fn set_push_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy.then_some(GitBusy::Push);
        cx.notify();
    }

    /// 横幅「继续」。
    pub(super) fn continue_op(
        &mut self,
        kind: RepoOpKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_write_op(
            GitBusy::Continue,
            move |repo| repo.continue_operation(kind),
            move |this, state, _window, cx| {
                let noun = op_noun(kind);
                let (message, tone) = match state.kind {
                    None => (tr!("gitBranch", "opFinished", op = noun), Tone::Info),
                    Some(_) if state.conflicted > 0 => (
                        tr!(
                            "gitBranch",
                            "opStillConflict",
                            op = noun,
                            count = state.conflicted
                        ),
                        Tone::Attention,
                    ),
                    Some(_) => (tr!("gitBranch", "opStillRunning", op = noun), Tone::Info),
                };
                this.notify_result(message, tone, cx);
            },
            window,
            cx,
        );
    }

    /// 横幅「中止」:先确认(已做的冲突解决会丢)。
    pub(super) fn confirm_abort(
        &mut self,
        kind: RepoOpKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let noun = op_noun(kind);
        let Some(repo) = self.repo_context() else {
            return;
        };
        let this = cx.entity();
        Confirm::new(
            tr!("gitBranch", "abortTitle", op = noun),
            tr!("gitBranch", "abortMessage", op = noun),
        )
        .ok_text(t("gitBranch", "abort"))
        .danger()
        .open(
            move |window, cx| {
                this.update(cx, |this, cx| {
                    this.run_write_op_on(
                        &repo,
                        GitBusy::Abort,
                        move |repo| repo.abort_operation(kind),
                        move |this, (), _window, cx| {
                            this.notify_result(
                                tr!("gitBranch", "opAborted", op = op_noun(kind)),
                                Tone::Info,
                                cx,
                            );
                        },
                        window,
                        cx,
                    );
                });
            },
            window,
            cx,
        );
    }
}
