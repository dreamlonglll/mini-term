use super::*;

/// 冲突菜单「采用一侧」的文案 key(`gitCommit` 命名空间)。`deleted` = 那一侧是删除。
///
/// 变基时 git 的 ours / theirs 与直觉**相反**:ours 是被变基到的那一侧(上游),
/// theirs 才是正在重放的、用户自己的提交 —— 照「我方 / 对方」写,用户点「采用我方」
/// 恰好丢掉自己的改动。合并 / 拣选 / 还原时 ours 就是当前分支,维持原文案。
pub(super) fn take_side_key(
    side: ConflictSide,
    deleted: bool,
    op: Option<RepoOpKind>,
) -> &'static str {
    let rebasing = op == Some(RepoOpKind::Rebase);
    match (side, deleted, rebasing) {
        (ConflictSide::Ours, false, false) => "takeOurs",
        (ConflictSide::Ours, true, false) => "takeOursDeleted",
        (ConflictSide::Theirs, false, false) => "takeTheirs",
        (ConflictSide::Theirs, true, false) => "takeTheirsDeleted",
        (ConflictSide::Ours, false, true) => "takeOursRebase",
        (ConflictSide::Ours, true, true) => "takeOursRebaseDeleted",
        (ConflictSide::Theirs, false, true) => "takeTheirsRebase",
        (ConflictSide::Theirs, true, true) => "takeTheirsRebaseDeleted",
    }
}

/// 确认框里列出的文件(最多 5 个,多出的写「…等 N 个」)。
/// 部分暂存的文件(同时在已暂存与未暂存两组里,口径见 [`in_area`]),按列表原顺序。
pub(super) fn partially_staged_paths(changes: &[ChangeFileStatus]) -> Vec<String> {
    changes
        .iter()
        .filter(|c| in_area(c, Area::Staged) && in_area(c, Area::Unstaged))
        .map(|c| c.path.clone())
        .collect()
}

pub(super) fn preview_paths(paths: &[String]) -> Vec<String> {
    const LIMIT: usize = 5;
    let mut lines: Vec<String> = paths.iter().take(LIMIT).map(|p| format!("• {p}")).collect();
    if paths.len() > LIMIT {
        lines.push(tr!(
            "gitCommit",
            "andMore",
            count = (paths.len() - LIMIT).to_string()
        ));
    }
    lines
}

impl GitChanges {
    // ── 整文件操作 ──

    /// 仓库的写操作闸被别的区占着(面板在拉取、历史区在拣选…):行内 / 组操作与提交
    /// 按钮按它禁用。自己在跑的那一件由 `busy_ops` / `committing` 管。
    pub(super) fn gated(&self) -> bool {
        self.context
            .as_ref()
            .is_some_and(|repo| git_ui::gate::is_busy_key(&repo.repository_key()))
    }

    /// 整文件操作能不能点:自己没在跑、闸也没被别人占。render 里只问一次、传给每一行
    /// (问闸要拼一次比较键,不该每行一次)。
    pub(super) fn ops_enabled(&self) -> bool {
        self.ops_enabled_with(self.gated())
    }

    /// [`Self::ops_enabled`],闸的状态由调用方给(同一帧已经问过了)。
    pub(super) fn ops_enabled_with(&self, gated: bool) -> bool {
        self.busy_ops == 0 && !gated
    }

    /// 跑一件阻塞的 git 动作(暂存 / 取消暂存 / 丢弃 / 冲突处理),占闸、后台执行,完成后
    /// 重取列表。失败弹详情框(口径见模块注释);闸被占时推 toast、不执行。`reselect` 是
    /// 完成后要落回的位置。
    pub(super) fn run_op(
        &mut self,
        op: impl FnOnce(&GitContext) -> anyhow::Result<()> + Send + 'static,
        reselect: Option<(Area, usize)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.context.clone() else {
            return;
        };
        self.run_op_on(&repo, op, reselect, window, cx);
    }

    pub(super) fn run_op_on(
        &mut self,
        repo: &GitContext,
        op: impl FnOnce(&GitContext) -> anyhow::Result<()> + Send + 'static,
        reselect: Option<(Area, usize)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected = self.context.as_ref() == Some(repo);
        let context = repo.clone();
        let store = self.store.clone();
        let this = cx.entity().downgrade();
        let started = git_ui::spawn_gated(
            repo,
            move |repo| {
                let result = op(repo);
                if let Err(err) = &result {
                    tracing::warn!(
                        "[git] {}: working tree operation failed: {err:#}",
                        git_ui::repo_label(repo)
                    );
                }
                // 写完当场读一份签名与冲突数(成败都读:失败也可能改了一半),**先于**下面那次
                // 重取列表 —— 面板拿它当「已知」,不能比列表更新(见 `IndexWritten`)
                let observed = RepoObservation {
                    at: Instant::now(),
                    signature: repo.repo_signature().ok(),
                    op: repo.repo_operation_state().ok(),
                };
                Ok((result, observed))
            },
            move |outcome, window, cx| {
                // 外层只是包装,实际恒为 `Ok`
                let (result, observed) = match outcome {
                    Ok((result, observed)) => (result, Some(observed)),
                    Err(err) => (Err(err), None),
                };
                match &result {
                    Ok(()) => {
                        let current = git_ui::repo_is_current(&store, &context, cx)
                            && this.upgrade().is_some_and(|this| {
                                this.read(cx).context.as_ref() == Some(&context)
                            });
                        if !current {
                            git_ui::toast_for_repo(
                                &store,
                                &context,
                                t("gitChanges", "operationSucceeded").to_string(),
                                Tone::Info,
                                cx,
                            );
                        }
                    }
                    Err(err) => git_ui::show_git_error(
                        t("gitChanges", "operationFailedTitle"),
                        git_ui::repo_text(
                            &store,
                            &context,
                            git_ui::error_text_for_store(err, &store, cx),
                            cx,
                        ),
                        window,
                        cx,
                    ),
                }
                let _ = this.update(cx, |this: &mut Self, cx| {
                    if this.context.as_ref() != Some(&context) {
                        return;
                    }
                    this.busy_ops = this.busy_ops.saturating_sub(1);
                    if result.is_ok() {
                        this.reselect = reselect;
                    }
                    this.load(cx);
                    // 丢弃改了工作区、解决冲突改了状态:文件树的 git 标记跟着重算
                    git_watch::request_refresh(git_watch::Subscriber::FileTree);
                    if let Some(observed) = observed {
                        cx.emit(GitChangesEvent::IndexWritten(observed));
                    }
                });
            },
            window,
            cx,
        );
        if !started {
            git_ui::toast_for_repo(
                &self.store,
                repo,
                git_ui::gate_busy_text().to_string(),
                Tone::Attention,
                cx,
            );
            return;
        }
        if selected {
            self.busy_ops += 1;
            cx.notify();
        }
    }

    /// 写操作闸被占:推一条 toast(列表与按钮已画成禁用,走到这里多半是键盘快捷键)。
    pub(super) fn notify_gate_busy(&self, cx: &mut Context<Self>) {
        git_ui::toast(
            &self.store,
            git_ui::gate_busy_text().to_string(),
            Tone::Attention,
            cx,
        );
    }

    pub(super) fn stage(
        &mut self,
        paths: Vec<String>,
        reselect: Option<(Area, usize)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        self.run_op(move |repo| repo.git_stage(&paths), reselect, window, cx);
    }

    pub(super) fn unstage(
        &mut self,
        paths: Vec<String>,
        reselect: Option<(Area, usize)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        self.run_op(move |repo| repo.git_unstage(&paths), reselect, window, cx);
    }

    /// 某个分组的全部路径。
    pub(super) fn area_paths(&self, area: Area, cx: &App) -> Vec<String> {
        self.model(cx)
            .members(area)
            .iter()
            .map(|&i| self.changes[i].path.clone())
            .collect()
    }

    /// 分组标题上的「全部暂存」:**只暂存这一组**。原先未跟踪组的按钮也调
    /// `git_stage_all`(`add_all("*")`),会把已跟踪的改动一起暂存。
    pub(super) fn stage_group(&mut self, area: Area, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.area_paths(area, cx);
        self.stage(paths, None, window, cx);
    }

    pub(super) fn unstage_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_op(|repo| repo.git_unstage_all(), None, window, cx);
    }

    /// 丢弃(先确认)。未暂存组只丢未暂存的部分(已暂存的保留),未跟踪组删除文件。
    pub(super) fn discard(
        &mut self,
        area: Area,
        paths: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() || !matches!(area, Area::Unstaged | Area::Untracked) {
            return;
        }
        let count = paths.len().to_string();
        let message = if area == Area::Untracked {
            tr!("gitCommit", "discardUntrackedConfirm", count = count)
        } else {
            tr!("gitCommit", "discardUnstagedConfirm", count = count)
        };
        let this = cx.entity();
        let Some(repo) = self.context.clone() else {
            return;
        };
        Confirm::new(t("gitChanges", "discardTitle"), message)
            .detail(preview_paths(&paths))
            .ok_text(t("gitChanges", "discardOk"))
            .cancel_text(t("gitChanges", "discardCancel"))
            .danger()
            .open(
                move |window, cx| {
                    let paths = paths.clone();
                    this.update(cx, |this, cx| {
                        let reselect = (this.context.as_ref() == Some(&repo))
                            .then(|| this.selection_anchor_pos(area, cx).map(|pos| (area, pos)))
                            .flatten();
                        this.run_op_on(
                            &repo,
                            move |repo| repo.discard_unstaged(&paths),
                            reselect,
                            window,
                            cx,
                        );
                    });
                },
                window,
                cx,
            );
    }

    /// 标记为已解决:先在后台看一眼工作区里还有没有 `<<<<<<<`,有就先确认。
    pub(super) fn mark_resolved(
        &mut self,
        paths: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        if self.metadata_warning.is_some() || self.load_error.is_some() {
            return;
        }
        let Some(repo) = self.context.clone() else {
            return;
        };
        let context = repo.clone();
        cx.spawn_in(window, async move |this, cx| {
            let checked = paths.clone();
            let marked = mt_remote::run_git_on(&repo.clone(), move || {
                checked
                    .into_iter()
                    .filter_map(|path| match repo.has_conflict_markers(&path) {
                        Ok(true) => Some(Ok(path)),
                        Ok(false) => None,
                        Err(err) => Some(Err(err)),
                    })
                    .collect::<anyhow::Result<Vec<_>>>()
            })
            .await;
            let _ = this.update_in(cx, |this: &mut Self, window, cx| {
                let marked = match marked {
                    Ok(marked) => marked,
                    Err(err) => {
                        git_ui::show_git_error(
                            t("gitChanges", "operationFailedTitle"),
                            git_ui::repo_text(
                                &this.store,
                                &context,
                                git_ui::error_text_for_store(&err, &this.store, cx),
                                cx,
                            ),
                            window,
                            cx,
                        );
                        return;
                    }
                };
                let resolve =
                    move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                        // Metadata may have failed while checking markers or
                        // while the confirmation was open. Never use that stale
                        // permission to resolve conflicts on the current repo.
                        if this.context.as_ref() == Some(&context)
                            && (this.metadata_warning.is_some() || this.load_error.is_some())
                        {
                            git_ui::toast_for_repo(
                                &this.store,
                                &context,
                                t("gitChanges", "metadataUnavailable").to_string(),
                                Tone::Attention,
                                cx,
                            );
                            return;
                        }
                        this.run_op_on(
                            &context,
                            move |repo| {
                                for path in &paths {
                                    repo.mark_conflict_resolved(path)?;
                                }
                                Ok(())
                            },
                            None,
                            window,
                            cx,
                        );
                    };
                if marked.is_empty() {
                    resolve(this, window, cx);
                    return;
                }
                let entity = cx.entity();
                let resolve = Rc::new(std::cell::RefCell::new(Some(resolve)));
                Confirm::new(
                    t("gitCommit", "markResolvedTitle"),
                    tr!(
                        "gitCommit",
                        "markResolvedConfirm",
                        count = marked.len().to_string()
                    ),
                )
                .detail(preview_paths(&marked))
                .ok_text(t("gitCommit", "markResolvedOk"))
                .open(
                    move |window, cx| {
                        if let Some(resolve) = resolve.borrow_mut().take() {
                            entity.update(cx, |this, cx| resolve(this, window, cx));
                        }
                    },
                    window,
                    cx,
                );
            });
        })
        .detach();
    }

    /// 冲突文件能不能逐段解决:能就是 `Ok(段数)`,不能就是禁用说明(菜单项的副标题、
    /// 行内按钮的 tooltip)。随列表一起取的状态里找不到它(刚换仓库、读冲突细节失败)
    /// 按「已不在冲突中,请刷新」算。
    pub(super) fn segment_availability(&self, path: &str) -> Result<usize, &'static str> {
        let status = self
            .conflicts
            .iter()
            .position(|c| c.path == path)
            .and_then(|i| self.segments.get(i).copied());
        match status {
            Some(SegmentStatus::Ready { blocks }) => Ok(blocks),
            Some(SegmentStatus::Blocked(blocker)) => {
                Err(t("gitCommit", git_conflict::blocker_key(blocker)))
            }
            None => Err(t("gitCommit", "segments.blockedNotConflicted")),
        }
    }

    /// 打开某个冲突文件的逐段解决弹窗(见 `git_conflict`)。写完(成败都算)重取列表、
    /// 重算文件树的 git 标记,并把写完当场读的签名交给面板 —— 与 [`Self::run_op`] 同一口径。
    pub(super) fn open_segments(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.segment_availability(&path).is_err()
            || self.metadata_warning.is_some()
            || self.load_error.is_some()
        {
            return;
        }
        let Some(context) = self.context.clone() else {
            return;
        };
        let written_context = context.clone();
        let weak = cx.entity().downgrade();
        let on_written: git_conflict::OnWritten =
            Rc::new(move |observed: Option<RepoObservation>, cx: &mut App| {
                let _ = weak.update(cx, |this, cx| {
                    if this.context.as_ref() != Some(&written_context) {
                        return;
                    }
                    this.load(cx);
                    git_watch::request_refresh(git_watch::Subscriber::FileTree);
                    if let Some(observed) = observed {
                        cx.emit(GitChangesEvent::IndexWritten(observed));
                    }
                });
            });
        git_conflict::open(
            self.store.clone(),
            context,
            path,
            self.op_kind,
            on_written,
            window,
            cx,
        );
    }

    /// diff 页签里能前后换的文件序列:某个分组的显示顺序(`order`,取自派生模型)。
    pub(super) fn diff_files(&self, order: &[usize], area: Area) -> Vec<git_diff::DiffFile> {
        order.iter().map(|&i| self.diff_file(i, area)).collect()
    }

    pub(super) fn diff_file(&self, index: usize, area: Area) -> git_diff::DiffFile {
        let file = &self.changes[index];
        let status = status_in_area(file, area);
        // 只有「两侧都是普通文本、能写回」的状态才给块操作:未跟踪 / 冲突 /
        // 重命名 / 删除都只支持整文件操作
        let hunk_actions = match area {
            Area::Staged => matches!(status, Some(GitStatus::Modified | GitStatus::Added)),
            Area::Unstaged => matches!(status, Some(GitStatus::Modified)),
            Area::Untracked | Area::Conflicts => false,
        };
        git_diff::DiffFile {
            path: file.path.clone(),
            // 已暂存的重命名:HEAD 那一侧按旧路径取(暂存侧才比 HEAD)
            old_path: file
                .old_path
                .clone()
                .filter(|_| matches!(status, Some(GitStatus::Renamed))),
            side: if area == Area::Staged {
                git_diff::DiffSide::Staged
            } else {
                git_diff::DiffSide::Unstaged
            },
            status_label: status_label_for(status).to_string(),
            hunk_actions,
        }
    }

    /// 打开某一行的 diff 页签(已开着就切过去),文件序列 = 它所在分组的显示顺序(页签里
    /// Alt+←/→ 前后换)。焦点随之交给 diff 页签(口径见 `git_diff::tab` 模块注释「焦点」)。
    /// 按块操作之后序列跟着重载过的列表走(取消暂存最后一块,文件就离开了「已暂存」)。
    pub(super) fn view_diff(
        &self,
        index: usize,
        area: Area,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.context.clone() else {
            return;
        };
        let hooks = self.diff_hooks(&repo, area, cx);
        let model = self.model(cx);
        let order = model.order(area);
        let files = self.diff_files(order, area);
        let current = order.iter().position(|&i| i == index).unwrap_or(0);
        git_diff::open_file_diff_with(self.store.clone(), repo, files, current, hooks, window, cx);
    }

    /// Both consumers stay attached to the captured backend, never its path alone.
    pub(super) fn diff_hooks(
        &self,
        context: &GitContext,
        area: Area,
        cx: &mut Context<Self>,
    ) -> git_diff::FileDiffHooks {
        let context = context.clone();
        let weak = cx.entity().downgrade();
        let reload_files: git_diff::ReloadFiles = {
            let context = context.clone();
            Rc::new(
                move |done: git_diff::FilesDone, cx: &mut App| match weak.upgrade() {
                    Some(this) if this.read(cx).context.as_ref() == Some(&context) => {
                        this.update(cx, |this, cx| {
                            this.diff_waiters.push((area, done));
                            this.load(cx);
                        });
                    }
                    _ => done(None, cx),
                },
            )
        };
        let weak = cx.entity().downgrade();
        let on_switch: git_diff::OnSwitch =
            Rc::new(move |file: &git_diff::DiffFile, cx: &mut App| {
                let _ = weak.update(cx, |this, cx| {
                    if this.context.as_ref() == Some(&context) {
                        this.follow_diff(area, &file.path, cx);
                    }
                });
            });
        git_diff::FileDiffHooks {
            on_switch: Some(on_switch),
            reload_files: Some(reload_files),
            ..git_diff::FileDiffHooks::default()
        }
    }

    /// diff 页签里换了文件:列表的选中项跟过去(并滚进视野),回到列表时停在看到的那个。
    pub(super) fn follow_diff(&mut self, area: Area, path: &str, cx: &mut Context<Self>) {
        let Some(index) = self
            .changes
            .iter()
            .position(|c| c.path == path && in_area(c, area))
        else {
            return;
        };
        self.select_single(area, index);
        self.reveal_cursor = true;
        cx.notify();
    }
}
