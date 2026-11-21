use super::*;

pub(super) struct CommitDraft {
    pub(super) input: Entity<TextareaState>,
    pub(super) revision: Rc<Cell<u64>>,
    pub(super) _subscription: Subscription,
}

#[derive(Clone)]
pub(super) struct SavedMessage {
    pub(super) id: u64,
    pub(super) message: String,
}
/// Both the text and its edit generation must match: changing away and back to
/// the submitted text still creates a new draft that a pending commit cannot erase.
pub(super) fn draft_matches(submitted: u64, current: u64, text: &str, now: &str) -> bool {
    submitted == current && text == now
}
/// 「已推送」判定的缓存键:引用那一半签名(HEAD + 全部引用)。只看 HEAD 不够 —— 推送 /
/// fetch 挪了远程跟踪分支,同一个 HEAD 的判定也会变。读不到为 `None`(当作变了)。
pub(super) fn amend_key_of(repo: &GitContext) -> Option<u64> {
    repo.repo_signature().ok().map(|sig| sig.refs)
}

/// 手上那份 amend 信息还作不作数:键读不到、或与取它时的键不同,都要重取。
pub(super) fn amend_stale(known: Option<u64>, now: Option<u64>) -> bool {
    now.is_none() || now != known
}

/// [`GitChanges::refresh_amend`] 的后台那一趟:键没变或本地读取失败返回 `None`(保留原状态)。
/// 否则返回 `Some((新键, 信息))`;远程读取失败仍清掉元数据,阻止未经核验的 amend。
pub(super) fn fetch_amend(
    repo: &GitContext,
    known: Option<u64>,
) -> Option<(Option<u64>, Option<AmendInfo>)> {
    let key = amend_key_of(repo);
    if !amend_stale(known, key) {
        return None;
    }
    let info = match repo.amend_info() {
        Ok(info) => info,
        Err(err) => {
            tracing::warn!(
                "[git] {}: amend metadata failed: {err:#}",
                git_ui::repo_label(repo)
            );
            if !repo.is_remote() {
                return None;
            }
            None
        }
    };
    Some((key, info))
}
pub(super) fn history_entries(
    list: Vec<CommitMessageEntry>,
    owner: Entity<GitChanges>,
    context: GitContext,
) -> Vec<menu::MenuEntry> {
    if list.is_empty() {
        return vec![
            MenuItem::new(t("gitCommit", "historyEmpty"))
                .disabled(true)
                .into(),
        ];
    }
    list.into_iter()
        .map(|entry| {
            let label = message_label(&entry.summary);
            let owner = owner.clone();
            let context = context.clone();
            let message = entry.message;
            MenuItem::new(label)
                .detail(format_date(entry.timestamp))
                .on_click(move |window, cx| {
                    owner.update(cx, |this, cx| {
                        this.replace_message_on(&context, message.clone(), window, cx);
                    });
                })
                .into()
        })
        .collect()
}

pub(super) fn message_label(message: &str) -> String {
    let title = message.lines().next().unwrap_or("");
    if title.chars().count() > 60 {
        title.chars().take(60).chain(['…']).collect()
    } else {
        title.to_string()
    }
}

/// 一段文本末尾的光标位置(行、列都从 0 数,列按字符)。
pub(super) fn text_end(text: &str) -> Position {
    let line = text.matches('\n').count();
    let last = text.rsplit('\n').next().unwrap_or_default();
    Position::new(line as u32, last.chars().count() as u32)
}

pub(super) fn format_date(timestamp: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(timestamp, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => timestamp.to_string(),
    }
}

impl GitChanges {
    // ── 提交 ──

    /// 提交按钮为什么点不了(`None` = 能点)。`stage_first` = 「全部暂存并提交」。
    pub(super) fn commit_blocker(&self, stage_first: bool, cx: &App) -> Option<String> {
        self.commit_blocker_with(stage_first, self.gated(), &self.model(cx), cx)
    }

    /// [`Self::commit_blocker`],闸的状态与派生模型由调用方给(render 里同一帧已经
    /// 取过;已暂存 / 冲突 / 未暂存的数目都从模型读,不再每帧扫一遍 `changes`)。
    pub(super) fn commit_blocker_with(
        &self,
        stage_first: bool,
        gated: bool,
        model: &ListModel,
        cx: &App,
    ) -> Option<String> {
        if self.context.is_none() {
            return Some(t("gitCommit", "blockNoRepo").to_string());
        }
        if self.input_key != self.context.as_ref().map(GitContext::key)
            || !self.loaded_once
            || self.load_error.is_some()
            || self.metadata_warning.is_some()
        {
            return Some(t("gitChanges", "metadataUnavailable").to_string());
        }
        if self.amend
            && self.context.as_ref().is_some_and(GitContext::is_remote)
            && (self.amend_info.is_none() || self.amend_verifying)
        {
            return Some(t("gitChanges", "metadataUnavailable").to_string());
        }
        if self.committing {
            return Some(t("gitCommit", "blockBusy").to_string());
        }
        if gated {
            return Some(git_ui::gate_busy_text().to_string());
        }
        let conflicts = model.members(Area::Conflicts).len();
        if conflicts > 0 {
            return Some(tr!(
                "gitCommit",
                "blockConflicts",
                count = conflicts.to_string()
            ));
        }
        if self.commit_input.read(cx).value().trim().is_empty() {
            return Some(t("gitCommit", "blockNoMessage").to_string());
        }
        if self.amend {
            return (!self.has_head).then(|| t("gitCommit", "amendNoHead").to_string());
        }
        if stage_first {
            return model
                .members(Area::Unstaged)
                .is_empty()
                .then(|| t("gitCommit", "blockNothingToStage").to_string());
        }
        if model.members(Area::Staged).is_empty() {
            return Some(if model.members(Area::Unstaged).is_empty() {
                t("gitCommit", "blockNothingStaged").to_string()
            } else {
                t("gitCommit", "blockNothingStagedHint").to_string()
            });
        }
        None
    }

    /// 提交。`open_push` = 「提交并推送」:提交成功后让容器打开推送对话框(见模块注释);
    /// `stage_first` = 先暂存全部已跟踪改动(未暂存组,不含未跟踪文件)。
    ///
    /// 「全部暂存并提交」遇到**部分暂存**的文件先确认:按整文件 `git add` 会把用户按块 /
    /// 按行挑好的暂存内容连同剩下的改动一起暂存,挑的那一步就白做了,而且提交以后才看得出来。
    /// 没有部分暂存的文件时不打扰,直接提交。
    pub(super) fn commit(
        &mut self,
        open_push: bool,
        stage_first: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_input(window, cx);
        if self.commit_blocker(stage_first, cx).is_some() {
            return;
        }
        let Some(context) = self.context.clone() else {
            return;
        };
        if stage_first {
            let partial = partially_staged_paths(&self.changes);
            if !partial.is_empty() {
                let this = cx.entity();
                Confirm::new(
                    t("gitCommit", "stageAllAndCommit"),
                    tr!(
                        "gitCommit",
                        "stageAllPartialConfirm",
                        count = partial.len().to_string()
                    ),
                )
                .detail(preview_paths(&partial))
                .ok_text(t("gitCommit", "stageAllPartialOk"))
                .open(
                    move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.commit_confirmed(&context, open_push, true, window, cx)
                        });
                    },
                    window,
                    cx,
                );
                return;
            }
        }
        self.commit_confirmed(&context, open_push, stage_first, window, cx);
    }

    /// Confirmation is bound to its original backend; switching never submits
    /// another repository's draft or reuses its partially staged file list.
    pub(super) fn commit_confirmed(
        &mut self,
        context: &GitContext,
        open_push: bool,
        stage_first: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.context.as_ref() != Some(context)
            || !git_ui::repo_is_current(&self.store, context, cx)
        {
            git_ui::toast_for_repo(
                &self.store,
                context,
                t("gitChanges", "contextChanged").to_string(),
                Tone::Attention,
                cx,
            );
            return;
        }
        self.sync_input(window, cx);
        if self.commit_blocker(stage_first, cx).is_some() {
            return;
        }
        let repo = context.clone();
        let context = context.clone();
        let message = self.commit_input.read(cx).value().trim().to_string();
        let amend = self.amend;
        let stage_paths = if stage_first {
            self.area_paths(Area::Unstaged, cx)
        } else {
            Vec::new()
        };
        let input = self.commit_input.clone();
        let submitted_text = input.read(cx).value().to_string();
        let revision = self.input_revision.clone();
        let submitted_revision = revision.get();
        let store = self.store.clone();
        let this = cx.entity().downgrade();
        // git_commit 走 git CLI(60s 超时兜底 GPG/pre-commit hook 挂起)——
        // 即便如此也不上主线程跑,hook 慢的仓库会把 UI 卡满整个超时窗口
        let started = git_ui::spawn_gated(
            &repo,
            move |repo| {
                if !stage_paths.is_empty() {
                    repo.git_stage(&stage_paths)?;
                }
                repo.git_commit_with(&message, amend)
            },
            move |result, window, cx| {
                let succeeded = result.is_ok();
                if let Ok(outcome) = &result {
                    if draft_matches(
                        submitted_revision,
                        revision.get(),
                        &submitted_text,
                        input.read(cx).value().as_ref(),
                    ) {
                        input.update(cx, |state, cx| state.set_value("", window, cx));
                    }
                    git_ui::toast_for_repo(
                        &store,
                        &context,
                        tr!(
                            "gitCommit",
                            "committedToast",
                            hash = outcome.short_hash.clone(),
                            title = outcome.summary.clone()
                        ),
                        Tone::Info,
                        cx,
                    );
                }
                let _ = this.update(cx, |this: &mut Self, cx| {
                    if let Err(err) = &result {
                        this.present_commit_failure(&context, err, window, cx);
                    } else {
                        this.amend_drafts.remove(&context.key());
                    }
                    let current = this.context.as_ref() == Some(&context);
                    if current {
                        this.committing = false;
                        if result.is_ok() {
                            this.amend = false;
                            this.amend_info = None;
                            this.amend_prefill = None;
                            this.amend_key = None;
                            this.commit_error = None;
                            this.load(cx);
                            git_watch::request_refresh(git_watch::Subscriber::FileTree);
                            cx.emit(GitChangesEvent::Committed {
                                open_push,
                                context: context.clone(),
                            });
                        }
                        cx.notify();
                    } else if succeeded && open_push {
                        cx.emit(GitChangesEvent::OpenPush(context));
                    }
                });
            },
            window,
            cx,
        );
        if !started {
            self.notify_gate_busy(cx);
            return;
        }
        self.committing = true;
        self.commit_error = None;
        cx.notify();
    }

    pub(super) fn on_commit_action(
        &mut self,
        _: &GitCommitMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit(false, false, window, cx);
    }

    pub(super) fn on_commit_push_action(
        &mut self,
        _: &GitCommitAndPush,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit(true, false, window, cx);
    }

    pub(super) fn present_commit_failure(
        &mut self,
        context: &GitContext,
        err: &anyhow::Error,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.context.as_ref() == Some(context)
            && git_ui::repo_is_current(&self.store, context, cx)
        {
            self.commit_error = Some(tr!(
                "gitChanges",
                "commitFailed",
                detail = git_ui::repo_text(
                    &self.store,
                    context,
                    git_ui::error_text_for_store(err, &self.store, cx),
                    cx
                )
            ));
            cx.notify();
        } else {
            git_ui::show_git_error(
                t("gitChanges", "operationFailedTitle"),
                git_ui::repo_text(
                    &self.store,
                    context,
                    git_ui::error_text_for_store(err, &self.store, cx),
                    cx,
                ),
                window,
                cx,
            );
        }
    }

    /// Restore undo results to their captured repository, preserving user text.
    /// Conflicting messages remain available in the message-history menu.
    pub fn restore_message_on(
        &mut self,
        context: &GitContext,
        message: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = context.key();
        let message = message.trim_end().to_string();
        if self.context.as_ref() == Some(context) {
            self.sync_input(window, cx);
            if self.amend {
                self.toggle_amend(window, cx);
            }
            if self.commit_input.read(cx).value().trim().is_empty() {
                self.commit_input
                    .update(cx, |state, cx| state.set_value(message, window, cx));
            } else {
                self.queue_pending_message(key, message);
            }
        } else {
            let prefill = self
                .amend_drafts
                .remove(&key)
                .and_then(|(_, prefill)| prefill);
            let input = if self.input_key.as_deref() == Some(key.as_str()) {
                Some(self.commit_input.clone())
            } else {
                self.drafts.get(&key).map(|draft| draft.input.clone())
            };
            if let Some(input) = input {
                let current = input.read(cx).value().to_string();
                if current.trim().is_empty() || prefill.as_deref() == Some(current.as_str()) {
                    input.update(cx, |state, cx| state.set_value(message, window, cx));
                } else {
                    self.queue_pending_message(key, message);
                }
            } else {
                self.queue_pending_message(key, message);
            }
        }
        cx.notify();
    }

    pub(super) fn queue_pending_message(&mut self, key: String, message: String) {
        let entry = self.saved_message(message);
        self.pending_messages.entry(key).or_default().push(entry);
    }

    pub(super) fn saved_message(&mut self, message: String) -> SavedMessage {
        let id = self.next_message_id;
        self.next_message_id = self
            .next_message_id
            .checked_add(1)
            .expect("message identity overflow");
        SavedMessage { id, message }
    }

    pub(super) fn replace_message_on(
        &mut self,
        context: &GitContext,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = context.key();
        let draft = if self.input_key.as_deref() == Some(key.as_str()) {
            Some((self.commit_input.clone(), self.input_revision.clone()))
        } else {
            self.drafts
                .get(&key)
                .map(|draft| (draft.input.clone(), draft.revision.clone()))
        };
        let Some((input, revision)) = draft else {
            return false;
        };
        let current = input.read(cx).value().to_string();
        if !current.trim().is_empty() && current != message {
            let entry = self.saved_message(current);
            let saved = self.saved_messages.entry(key).or_default();
            saved.push(entry);
            if saved.len() > HISTORY_LIMIT {
                saved.remove(0);
            }
        }
        let end = text_end(&message);
        let current = self.context.as_ref() == Some(context)
            && git_ui::repo_is_current(&self.store, context, cx);
        revision.set(revision.get().wrapping_add(1));
        input.update(cx, |state, cx| {
            state.set_value(message, window, cx);
            if current {
                state.set_cursor_position(end, window, cx);
            }
        });
        cx.notify();
        true
    }

    pub(super) fn toggle_amend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if (!self.has_head && !self.amend) || self.committing {
            return;
        }
        self.amend = !self.amend;
        self.amend_request += 1;
        self.amend_flight.reset();
        self.amend_verifying =
            self.amend && self.context.as_ref().is_some_and(GitContext::is_remote);
        if !self.amend {
            // 预填的消息一个字没动过 —— 那是 HEAD 的旧消息,不是用户写的,收起时带走
            let current = self.commit_input.read(cx).value().to_string();
            if self.amend_prefill.take().is_some_and(|p| p == current) {
                self.commit_input
                    .update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.amend_info = None;
            self.amend_key = None;
            cx.notify();
            return;
        }
        let req = self.amend_request;
        let Some(repo) = self.context.clone() else {
            return;
        };
        let input = self.commit_input.clone();
        let context = repo.clone();
        cx.spawn_in(window, async move |this, cx| {
            let (key, result) = mt_remote::run_git_on(&repo.clone(), move || {
                (amend_key_of(&repo), repo.amend_info())
            })
            .await;
            let _ = this.update_in(cx, |this: &mut Self, window, cx| {
                if let Err(err) = &result
                    && (this.context.as_ref() != Some(&context)
                        || !git_ui::repo_is_current(&this.store, &context, cx))
                {
                    git_ui::show_git_error(
                        t("gitChanges", "operationFailedTitle"),
                        git_ui::repo_text(
                            &this.store,
                            &context,
                            git_ui::error_text_for_store(err, &this.store, cx),
                            cx,
                        ),
                        window,
                        cx,
                    );
                }
                if this.amend_request != req || !this.amend {
                    return;
                }
                this.amend_verifying = false;
                match result {
                    Ok(Some(info)) => {
                        if input.read(cx).value().trim().is_empty() {
                            let message = info.message.trim_end().to_string();
                            input.update(cx, |state, cx| {
                                state.set_value(message.clone(), window, cx)
                            });
                            this.amend_prefill = Some(message);
                        }
                        this.amend_info = Some(info);
                        this.amend_key = key;
                    }
                    // 空仓库:没有上次提交可改
                    Ok(None) => {
                        this.has_head = false;
                        if !context.is_remote() {
                            this.amend = false;
                            this.amend_info = None;
                            this.amend_key = None;
                        }
                    }
                    Err(err) => {
                        tracing::warn!("[git] 读取上次提交失败: {err:#}");
                        if !context.is_remote() {
                            this.amend = false;
                            this.amend_info = None;
                            this.amend_key = None;
                        }
                        this.commit_error = Some(tr!(
                            "gitCommit",
                            "amendInfoFailed",
                            detail = git_ui::repo_text(
                                &this.store,
                                &context,
                                git_ui::error_text_for_store(&err, &this.store, cx),
                                cx
                            )
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn restore_pending_message(
        &mut self,
        context: &GitContext,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = context.key();
        let Some(entry) = self
            .pending_messages
            .get(&key)
            .and_then(|messages| messages.iter().find(|entry| entry.id == id))
            .cloned()
        else {
            return;
        };
        if self.replace_message_on(context, entry.message, window, cx)
            && let Some(messages) = self.pending_messages.get_mut(&key)
        {
            messages.retain(|entry| entry.id != id);
        }
        cx.notify();
    }

    /// 提交消息历史:后台取最近 20 条,取回来在按钮处弹菜单,选中把整段消息填进输入框。
    pub(super) fn show_history(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_input(window, cx);
        if self.history_loading || self.context.is_none() {
            return;
        }
        self.history_loading = true;
        cx.notify();
        let Some(repo) = self.context.clone() else {
            return;
        };
        let context = repo.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = mt_remote::run_git_on(&repo.clone(), move || {
                repo.recent_commit_messages(HISTORY_LIMIT)
            })
            .await;
            let _ = this.update_in(cx, |this: &mut Self, window, cx| {
                let same_context = this.context.as_ref() == Some(&context);
                let current = same_context && git_ui::repo_is_current(&this.store, &context, cx);
                if same_context {
                    this.history_loading = false;
                    cx.notify();
                }
                let list = match result {
                    Ok(list) => list,
                    Err(err) => {
                        tracing::warn!("[git] 读取提交消息历史失败: {err:#}");
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
                        if current {
                            let entries = this.message_history_entries(Vec::new(), &context, cx);
                            menu::show(position, entries, window, cx);
                        }
                        return;
                    }
                };
                if !current {
                    return;
                }
                let entries = this.message_history_entries(list, &context, cx);
                menu::show(position, entries, window, cx);
            });
        })
        .detach();
    }

    pub(super) fn message_history_entries(
        &self,
        recent: Vec<CommitMessageEntry>,
        context: &GitContext,
        cx: &mut Context<Self>,
    ) -> Vec<menu::MenuEntry> {
        let key = context.key();
        let mut entries = Vec::new();
        for (messages, heading, pending) in [
            (self.pending_messages.get(&key), "historyPending", true),
            (self.saved_messages.get(&key), "historyDrafts", false),
        ] {
            let Some(messages) = messages.filter(|messages| !messages.is_empty()) else {
                continue;
            };
            if !entries.is_empty() {
                entries.push(menu::separator());
            }
            entries.push(menu::MenuEntry::Header(t("gitCommit", heading).into()));
            for entry in messages.iter().rev() {
                let owner = cx.entity();
                let context = context.clone();
                let entry = entry.clone();
                entries.push(
                    MenuItem::new(message_label(&entry.message))
                        .detail(t(
                            "gitCommit",
                            if pending {
                                "undoMessagePending"
                            } else {
                                "savedDraft"
                            },
                        ))
                        .on_click(move |window, cx| {
                            owner.update(cx, |this, cx| {
                                if pending {
                                    this.restore_pending_message(&context, entry.id, window, cx);
                                } else {
                                    this.replace_message_on(
                                        &context,
                                        entry.message.clone(),
                                        window,
                                        cx,
                                    );
                                }
                            });
                        })
                        .into(),
                );
            }
        }
        if !entries.is_empty() {
            entries.push(menu::separator());
        }
        entries.push(menu::MenuEntry::Header(
            t("gitCommit", "historyRecent").into(),
        ));
        entries.extend(history_entries(recent, cx.entity(), context.clone()));
        entries
    }

    pub(super) fn sync_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.context.as_ref().map(GitContext::key);
        if self.input_key == key {
            if self.amend
                && let Some(info) = self.amend_info.as_ref()
                && let Some(prefill) = self.amend_prefill.as_ref()
                && self.commit_input.read(cx).value().as_ref() == prefill
            {
                let message = info.message.trim_end().to_string();
                if &message != prefill {
                    self.commit_input
                        .update(cx, |state, cx| state.set_value(message.clone(), window, cx));
                    self.amend_prefill = Some(message);
                }
            }
            return;
        }
        let old = CommitDraft {
            input: self.commit_input.clone(),
            revision: self.input_revision.clone(),
            _subscription: self
                .input_subscription
                .take()
                .expect("active draft subscription"),
        };
        if let Some(old_key) = self.input_key.take() {
            self.drafts.insert(old_key, old);
        } else {
            self.blank_draft = Some(old);
        }
        let saved = match &key {
            Some(key) => self.drafts.remove(key),
            None => self.blank_draft.take(),
        };
        let draft = saved.unwrap_or_else(|| {
            let input = cx.new(|cx| TextareaState::new(window, cx).auto_grow(3, 8));
            let revision = Rc::new(Cell::new(0u64));
            let changed = revision.clone();
            let subscription = cx.subscribe(&input, move |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    changed.set(changed.get().wrapping_add(1));
                    cx.notify();
                }
            });
            CommitDraft {
                input,
                revision,
                _subscription: subscription,
            }
        });
        self.commit_input = draft.input;
        self.input_revision = draft.revision;
        self.input_subscription = Some(draft._subscription);
        self.input_key = key.clone();
        if let Some(key) = key {
            if let Some((amend, prefill)) = self.amend_drafts.remove(&key) {
                self.amend = amend;
                self.amend_prefill = prefill;
                self.amend_info = None;
                self.amend_key = None;
                self.refresh_amend(cx);
            }
            if self.commit_input.read(cx).value().trim().is_empty()
                && let Some(messages) = self.pending_messages.get_mut(&key)
                && !messages.is_empty()
            {
                let message = messages.remove(0);
                self.commit_input
                    .update(cx, |state, cx| state.set_value(message.message, window, cx));
            }
        }
        self.placeholder = None;
    }

    /// 占位符跟着目标分支走(「提交到 main(Ctrl+Enter)」)。只在文本变了时才改 ——
    /// 切语言 / 换分支都会让文本变,每帧比一次字符串即可。
    pub(super) fn sync_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shortcut = menu::hotkey_label(true, false, false, "Enter");
        let text = match &self.target {
            Some(CommitTarget::Branch(name)) => tr!(
                "gitCommit",
                "placeholderBranch",
                branch = name.clone(),
                shortcut = shortcut
            ),
            Some(CommitTarget::Detached(hash)) => tr!(
                "gitCommit",
                "placeholderDetached",
                hash = hash.clone(),
                shortcut = shortcut
            ),
            None => t("panels", "commitPlaceholder").to_string(),
        };
        if self.placeholder.as_deref() == Some(text.as_str()) {
            return;
        }
        self.placeholder = Some(text.clone());
        self.commit_input
            .update(cx, |state, cx| state.set_placeholder(text, window, cx));
    }
    /// 提交区:输入框 + 工具条(Amend / 消息历史 / 标题字数)+ 软提示 + 拆分式提交按钮
    /// + 就地错误。
    pub(super) fn render_commit_area(
        &self,
        model: &ListModel,
        gated: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let message = self.commit_input.read(cx).value().to_string();
        let hints = message_hints(&message);
        let blocker = self.commit_blocker_with(false, gated, model, cx);
        let can_commit = blocker.is_none();
        let stage_first_ok = self.commit_blocker_with(true, gated, model, cx).is_none();
        let staged_count = model.members(Area::Staged).len();

        let commit_label = if self.committing {
            t("gitChanges", "committing").to_string()
        } else if self.amend {
            t("gitCommit", "amendCommit").to_string()
        } else {
            tr!("panels", "commit", count = staged_count.to_string())
        };

        // ── 工具条 ──
        let amend_enabled = (self.has_head || self.amend) && !self.committing;
        let amend_toggle = git_ui::check_row(
            "git-commit-amend",
            self.amend,
            t("gitCommit", "amend"),
            false,
        )
        .when(amend_enabled, |el| {
            el.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.toggle_amend(window, cx);
            }))
        })
        .when(!amend_enabled, |el| el.opacity(0.5).cursor_default())
        .tip(if self.has_head {
            t("gitCommit", "amendTip")
        } else {
            t("gitCommit", "amendNoHead")
        });
        let history_enabled = !self.history_loading && self.context.is_some();
        let history = icon_button(
            "git-commit-history",
            ICON_HISTORY,
            t("gitCommit", "messageHistory"),
            history_enabled,
        )
        .when(history_enabled, |el| {
            el.on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                this.show_history(event.position(), window, cx);
            }))
        });
        let counter = (hints.title_chars >= TITLE_COUNT_FROM).then(|| {
            mono(div())
                .text_size(ui::font_px(11.0))
                .text_color(if hints.title_too_long {
                    ui::color_error()
                } else {
                    ui::color_warning()
                })
                .child(format!("{}/{TITLE_SOFT_LIMIT}", hints.title_chars))
        });
        let toolbar = div()
            .mt(px(8.0))
            .flex()
            .min_w_0()
            .items_center()
            .gap(px(8.0))
            .child(amend_toggle)
            .child(history)
            .when(
                self.context.as_ref().is_some_and(|context| {
                    self.pending_messages
                        .get(&context.key())
                        .is_some_and(|messages| !messages.is_empty())
                }),
                |el| {
                    el.child(
                        git_ui::hint_line(t("gitCommit", "undoMessagePending"))
                            .min_w_0()
                            .flex_1(),
                    )
                },
            )
            .child(div().flex_1())
            .children(counter);

        // ── 警告与软提示 ──
        let pushed_warning = (self.amend && self.amend_info.as_ref().is_some_and(|i| i.pushed))
            .then(|| {
                git_ui::notice_line(t("gitCommit", "amendPushedWarning"), ui::color_warning())
                    .mt(px(6.0))
            });
        let mut soft_hints: Vec<String> = Vec::new();
        if hints.title_too_long {
            soft_hints.push(tr!(
                "gitCommit",
                "hintTitleTooLong",
                count = hints.title_chars.to_string()
            ));
        }
        if hints.missing_blank_line {
            soft_hints.push(t("gitCommit", "hintBlankLine").to_string());
        }

        // ── 提交按钮(主按钮 + ▾) ──
        let main_button = div()
            .id("git-commit-button")
            .flex_1()
            .h(px(30.0))
            .flex()
            .items_center()
            .justify_center()
            .text_size(ui::font_px(13.0))
            .when(can_commit, |el| {
                el.bg(ui::accent())
                    .text_color(ui::bg_base())
                    .cursor_pointer()
                    .hover(|el| el.opacity(0.9))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.commit(false, false, window, cx)
                    }))
            })
            .when(!can_commit, |el| {
                el.bg(ui::bg_elevated()).text_color(ui::text_muted())
            })
            .when_some(blocker.clone(), |el, reason| el.tip(reason))
            .child(commit_label);
        let menu_enabled = !self.committing;
        let split = div()
            .id("git-commit-more")
            .flex_none()
            .w(px(28.0))
            .h(px(30.0))
            .flex()
            .items_center()
            .justify_center()
            .border_l_1()
            .border_color(ui::with_alpha(ui::bg_base(), 0.35))
            .when(can_commit, |el| el.bg(ui::accent()))
            .when(!can_commit, |el| el.bg(ui::bg_elevated()))
            .child(git_ui::icon(
                ICON_CHEVRON_DOWN,
                git_ui::ICON_SIZE,
                if can_commit {
                    ui::bg_base()
                } else {
                    ui::text_muted()
                },
            ))
            .tip(t("gitCommit", "moreCommitActions"))
            .when(menu_enabled, |el| {
                el.cursor_pointer()
                    .hover(|el| el.opacity(0.9))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        let entries = this.commit_menu(can_commit, stage_first_ok, cx);
                        let position = this.commit_menu_anchor(event.position(), window);
                        menu::show(position, entries, window, cx);
                    }))
            });
        let buttons_bounds = self.commit_buttons.clone();
        let buttons = div()
            .relative()
            .mt(px(8.0))
            .flex()
            .rounded(px(4.0))
            .overflow_hidden()
            .child(main_button)
            .child(split)
            // 记下这一行在窗口里的位置(▾ 菜单的锚点,见 `commit_menu_anchor`)
            .child(
                canvas(
                    move |bounds: Bounds<Pixels>, _window, _cx| buttons_bounds.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );

        // 错误原文:与弹窗里的 git 失败原文同一套红框淡底,右侧一颗「复制」
        let error = self.commit_error.clone().map(|err| {
            let copy = err.clone();
            div()
                .mt(px(8.0))
                .flex()
                .items_start()
                .gap(px(4.0))
                .pl(px(10.0))
                .pr(px(2.0))
                .py(px(2.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(ui::with_alpha(ui::color_error(), 0.35))
                .bg(ui::with_alpha(ui::color_error(), 0.06))
                .child(
                    div()
                        .id("git-commit-error")
                        .flex_1()
                        .min_w_0()
                        .max_h(px(160.0))
                        .py(px(4.0))
                        .overflow_y_scroll()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::color_error())
                        .child(err),
                )
                .child(
                    icon_button(
                        "git-commit-error-copy",
                        ICON_COPY,
                        t("gitCommit", "copyError"),
                        true,
                    )
                    .on_click(move |_: &ClickEvent, _window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                    }),
                )
        });

        let area_h = self.commit_area_h.clone();
        div()
            .relative()
            .flex_none()
            .border_t_1()
            .border_color(ui::border_subtle())
            .p(px(8.0))
            .child(Textarea::new(&self.commit_input))
            .child(toolbar)
            .children(pushed_warning)
            .children(
                soft_hints
                    .into_iter()
                    .map(|hint| git_ui::hint_line(hint).mt(px(4.0))),
            )
            .child(buttons)
            .children(error)
            // 量提交区整块的高度(面板按内容分高度时要,见 `natural_height`)
            .child(
                canvas(
                    move |bounds: Bounds<Pixels>, _window, _cx| {
                        area_h.set(f32::from(bounds.size.height));
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .into_any_element()
    }

    /// ▾ 菜单的左上角:贴着提交按钮那一行的下沿弹出(与整行左对齐);下面放不下就整块
    /// 弹到上沿之上。菜单基件只会把越出视口的面板**平移**回来 —— 在下沿弹、被推回来时
    /// 正好压住按钮下半截(E2E b/s60),所以朝上弹要在这里先算好。还没画过按钮(量不到)
    /// 时退回鼠标点。
    pub(super) fn commit_menu_anchor(
        &self,
        fallback: Point<Pixels>,
        window: &Window,
    ) -> Point<Pixels> {
        let Some(row) = self.commit_buttons.get() else {
            return fallback;
        };
        let gap = px(4.0);
        let estimate = ui::font_px(COMMIT_MENU_EST_H);
        if row.bottom() + gap + estimate <= window.viewport_size().height {
            point(row.left(), row.bottom() + gap)
        } else {
            point(
                row.left(),
                px(f32::from(row.top() - gap - estimate).max(0.0)),
            )
        }
    }

    /// ▾ 菜单:提交并推送…(提交后打开推送对话框)/ 全部暂存并提交 /(分隔)贮藏当前改动…。
    pub(super) fn commit_menu(
        &self,
        can_commit: bool,
        stage_first_ok: bool,
        cx: &mut Context<Self>,
    ) -> Vec<menu::MenuEntry> {
        let this = cx.entity();
        let context = self.context.clone();
        let push = {
            let this = this.clone();
            let context = context.clone();
            MenuItem::new(t("gitCommit", "commitAndPush"))
                .detail(t("gitCommit", "commitAndPushDetail"))
                .shortcut(menu::hotkey_label(true, false, true, "Enter"))
                .disabled(!can_commit)
                .on_click(move |window, cx| {
                    this.update(cx, |this, cx| {
                        if this.context == context {
                            this.commit(true, false, window, cx);
                        } else if let Some(context) = context.as_ref() {
                            git_ui::toast_for_repo(
                                &this.store,
                                context,
                                t("gitChanges", "contextChanged").to_string(),
                                Tone::Attention,
                                cx,
                            );
                        }
                    });
                })
        };
        let stage_all = {
            let this = this.clone();
            let context = context.clone();
            MenuItem::new(t("gitCommit", "stageAllAndCommit"))
                .detail(t("gitCommit", "stageAllAndCommitDetail"))
                .disabled(!stage_first_ok)
                .on_click(move |window, cx| {
                    this.update(cx, |this, cx| {
                        if this.context == context {
                            this.commit(false, true, window, cx);
                        } else if let Some(context) = context.as_ref() {
                            git_ui::toast_for_repo(
                                &this.store,
                                context,
                                t("gitChanges", "contextChanged").to_string(),
                                Tone::Attention,
                                cx,
                            );
                        }
                    });
                })
        };
        // 贮藏:有改动(含未跟踪)才可用;没有时禁用并写明原因
        let has_changes = !self.changes.is_empty();
        let mut stash = MenuItem::new(t("gitStash", "stashChanges"))
            .disabled(!has_changes)
            .on_click(move |window, cx| {
                this.update(cx, |this, cx| {
                    if this.context == context {
                        this.open_stash_save(window, cx);
                    } else if let Some(context) = context.as_ref() {
                        git_ui::toast_for_repo(
                            &this.store,
                            context,
                            t("gitChanges", "contextChanged").to_string(),
                            Tone::Attention,
                            cx,
                        );
                    }
                });
            });
        if !has_changes {
            stash = stash.detail(t("gitStash", "save.nothing"));
        }
        vec![
            push.into(),
            stage_all.into(),
            menu::separator(),
            stash.into(),
        ]
    }

    /// 「贮藏当前改动…」:开新建贮藏弹窗,成功后让容器整面板刷新(见 [`GitChangesEvent::Stashed`])。
    pub(super) fn open_stash_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.context.clone() else {
            return;
        };
        let this = cx.entity().downgrade();
        let context = repo.clone();
        let on_changed: Rc<dyn Fn(&mut App)> = Rc::new(move |cx: &mut App| {
            let _ = this.update(cx, |this, cx| {
                if this.context.as_ref() == Some(&context) {
                    cx.emit(GitChangesEvent::Stashed);
                }
            });
        });
        crate::git::stash::open_save(self.store.clone(), repo, on_changed, window, cx);
    }
}
