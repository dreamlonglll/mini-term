use super::*;

pub(super) fn remote_operation_label(operation: mt_remote::RemoteGitOperationKind) -> &'static str {
    match operation {
        mt_remote::RemoteGitOperationKind::Stage => t("gitHistory", "operationStage"),
        mt_remote::RemoteGitOperationKind::Unstage => t("gitHistory", "operationUnstage"),
        mt_remote::RemoteGitOperationKind::StageAll => t("gitHistory", "operationStageAll"),
        mt_remote::RemoteGitOperationKind::UnstageAll => t("gitHistory", "operationUnstageAll"),
        mt_remote::RemoteGitOperationKind::Commit => t("gitHistory", "operationCommit"),
        mt_remote::RemoteGitOperationKind::Amend => t("gitHistory", "operationAmend"),
        mt_remote::RemoteGitOperationKind::Discard => t("gitHistory", "operationDiscard"),
        mt_remote::RemoteGitOperationKind::StageHunks => t("gitHistory", "operationStageHunks"),
        mt_remote::RemoteGitOperationKind::UnstageHunks => t("gitHistory", "operationUnstageHunks"),
        mt_remote::RemoteGitOperationKind::DiscardHunks => t("gitHistory", "operationDiscardHunks"),
        mt_remote::RemoteGitOperationKind::Checkout => t("gitHistory", "operationCheckout"),
        mt_remote::RemoteGitOperationKind::CreateBranch => t("gitHistory", "operationCreateBranch"),
        mt_remote::RemoteGitOperationKind::RenameBranch => t("gitHistory", "operationRenameBranch"),
        mt_remote::RemoteGitOperationKind::DeleteBranch => t("gitHistory", "operationDeleteBranch"),
        mt_remote::RemoteGitOperationKind::Fetch => t("gitHistory", "operationFetch"),
        mt_remote::RemoteGitOperationKind::Pull => t("gitHistory", "operationPull"),
        mt_remote::RemoteGitOperationKind::Push => t("gitHistory", "operationPush"),
        mt_remote::RemoteGitOperationKind::SaveStash => t("gitHistory", "operationSaveStash"),
        mt_remote::RemoteGitOperationKind::ApplyStash => t("gitHistory", "operationApplyStash"),
        mt_remote::RemoteGitOperationKind::PopStash => t("gitHistory", "operationPopStash"),
        mt_remote::RemoteGitOperationKind::DropStash => t("gitHistory", "operationDropStash"),
        mt_remote::RemoteGitOperationKind::ClearStashes => t("gitHistory", "operationClearStashes"),
        mt_remote::RemoteGitOperationKind::StashBranch => t("gitHistory", "operationStashBranch"),
        mt_remote::RemoteGitOperationKind::Continue => t("gitHistory", "operationContinue"),
        mt_remote::RemoteGitOperationKind::Abort => t("gitHistory", "operationAbort"),
        mt_remote::RemoteGitOperationKind::ResolveConflict => {
            t("gitHistory", "operationResolveConflict")
        }
        mt_remote::RemoteGitOperationKind::MarkResolved => t("gitHistory", "operationMarkResolved"),
        mt_remote::RemoteGitOperationKind::CherryPick => t("gitHistory", "operationCherryPick"),
        mt_remote::RemoteGitOperationKind::Revert => t("gitHistory", "operationRevert"),
        mt_remote::RemoteGitOperationKind::Reset => t("gitHistory", "operationReset"),
        mt_remote::RemoteGitOperationKind::UndoCommit => t("gitHistory", "operationUndoCommit"),
        mt_remote::RemoteGitOperationKind::RewordCommit => t("gitHistory", "operationRewordCommit"),
        mt_remote::RemoteGitOperationKind::CreateTag => t("gitHistory", "operationCreateTag"),
        mt_remote::RemoteGitOperationKind::DeleteTag => t("gitHistory", "operationDeleteTag"),
        mt_remote::RemoteGitOperationKind::PushTag => t("gitHistory", "operationPushTag"),
        mt_remote::RemoteGitOperationKind::DeleteRemoteBranch => {
            t("gitHistory", "operationDeleteRemoteBranch")
        }
        mt_remote::RemoteGitOperationKind::Merge => t("gitHistory", "operationMerge"),
        mt_remote::RemoteGitOperationKind::Rebase => t("gitHistory", "operationRebase"),
        mt_remote::RemoteGitOperationKind::AddWorktree => t("gitHistory", "operationAddWorktree"),
        mt_remote::RemoteGitOperationKind::RemoveWorktree => {
            t("gitHistory", "operationRemoveWorktree")
        }
        mt_remote::RemoteGitOperationKind::PruneWorktrees => {
            t("gitHistory", "operationPruneWorktrees")
        }
    }
}

pub(super) fn recovery_status_label(status: Option<&mt_project::git::GitStatus>) -> &'static str {
    match status {
        Some(mt_project::git::GitStatus::Modified) => t("gitCommit", "statusModified"),
        Some(mt_project::git::GitStatus::Added) => t("gitCommit", "statusAdded"),
        Some(mt_project::git::GitStatus::Deleted) => t("gitCommit", "statusDeleted"),
        Some(mt_project::git::GitStatus::Renamed) => t("gitCommit", "statusRenamed"),
        Some(mt_project::git::GitStatus::Untracked) => t("gitCommit", "statusUntracked"),
        Some(mt_project::git::GitStatus::Conflicted) => t("gitCommit", "statusConflicted"),
        None => t("gitHistory", "recoveryUnchanged"),
    }
}

impl GitPanel {
    pub(super) fn recovery_controls(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let state = self.store.read(cx);
        let id = state.active_project_id.as_deref()?;
        let context = crate::git::backend::recovery_context(state, id)?;
        let mut controls = div().flex().flex_col().gap(px(8.0)).p(px(10.0));
        let mut any = false;
        if let Some(unknown) = context.unknown_write() {
            any = true;
            controls = controls
                .child(ui::desc_text(tr!(
                    "gitHistory",
                    "verifyRepository",
                    repo = unknown.repo
                )))
                .child(
                    crate::prompt::footer_button(
                        "verify-remote-write",
                        t("gitHistory", "verifyWrite"),
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| this.verify_remote_write(window, cx)),
                    ),
                )
                .child(
                    crate::prompt::footer_button(
                        "abandon-remote-write",
                        t("gitHistory", "abandonAction"),
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| this.abandon_remote_write(window, cx)),
                    ),
                );
        }
        let blockers = if self.selected_repo.is_empty() {
            context.recovery_blockers_in_scope()
        } else {
            context.with_path(&self.selected_repo).recovery_blockers()
        };
        for blocker in blockers
            .into_iter()
            .filter(|blocker| blocker.owner_project_id != id)
        {
            any = true;
            let owner = blocker.owner_project_id;
            let name = state
                .project(&owner)
                .map(|project| project.name.clone())
                .unwrap_or_else(|| owner.clone());
            let store = self.store.clone();
            controls = controls
                .child(ui::desc_text(tr!(
                    "gitHistory",
                    "blockedByProject",
                    project = name,
                    repo = blocker.write.repo
                )))
                .child(
                    crate::prompt::footer_button(
                        "recovery-owner-project",
                        tr!("gitHistory", "goToRecoveryProject", project = name),
                    )
                    .on_click(move |_, _, cx| {
                        store.update(cx, |store, cx| store.set_active_project(&owner, cx))
                    }),
                );
        }
        any.then(|| controls.into_any_element())
    }

    pub(super) fn verify_remote_write(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.store.read(cx);
        let Some(id) = state.active_project_id.clone() else {
            return;
        };
        let Some(repo) = crate::git::backend::recovery_context(state, &id) else {
            return;
        };
        if !crate::git::backend::remote_git_enabled(state, &id) {
            let panel = cx.entity().downgrade();
            let consent = match repo.recovery_consent() {
                Ok(consent) => Rc::new(RefCell::new(Some(consent))),
                Err(error) => {
                    git_ui::show_git_error(
                        t("gitHistory", "verifyWrite"),
                        git_ui::error_text(&error),
                        window,
                        cx,
                    );
                    return;
                }
            };
            Confirm::new(
                t("gitHistory", "recoveryReadConsentTitle"),
                t("gitHistory", "recoveryReadConsentMessage"),
            )
            .detail(vec![git_ui::repo_label(&repo)])
            .ok_text(t("gitHistory", "recoveryReadConsentAction"))
            .open(
                move |window, cx| {
                    let Some(consent) = consent.borrow_mut().take() else {
                        return;
                    };
                    let _ = panel.update(cx, |panel, cx| {
                        panel.load_recovery_snapshot(repo.clone(), Some(consent), window, cx)
                    });
                },
                window,
                cx,
            );
        } else {
            self.load_recovery_snapshot(repo, None, window, cx);
        }
    }

    pub(super) fn abandon_remote_write(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.store.read(cx);
        let Some(id) = state.active_project_id.as_deref() else {
            return;
        };
        let Some(repo) = crate::git::backend::recovery_context(state, id) else {
            return;
        };
        let confirmation = match repo.prepare_offline_abandon() {
            Ok(confirmation) => Rc::new(RefCell::new(Some(confirmation))),
            Err(error) => {
                git_ui::show_git_error(
                    t("gitHistory", "abandonTitle"),
                    git_ui::error_text_for_store(&error, &self.store, cx),
                    window,
                    cx,
                );
                return;
            }
        };
        let panel = cx.entity().downgrade();
        let store = self.store.clone();
        Confirm::new(
            t("gitHistory", "abandonTitle"),
            t("gitHistory", "abandonWarning"),
        )
        .detail(vec![git_ui::repo_label(&repo)])
        .danger()
        .ok_text(t("gitHistory", "abandonAction"))
        .open(
            move |window, cx| {
                let Some(confirmation) = confirmation.borrow_mut().take() else {
                    return;
                };
                match repo.abandon_recovery(confirmation) {
                    Ok(()) => {
                        let _ = panel.update(cx, |panel, cx| {
                            git_ui::toast_for_repo(
                                &panel.store,
                                &repo,
                                t("gitHistory", "abandoned").to_string(),
                                Tone::Attention,
                                cx,
                            );
                            cx.notify();
                        });
                    }
                    Err(error) => git_ui::show_git_error(
                        t("gitHistory", "abandonTitle"),
                        git_ui::error_text_for_store(&error, &store, cx),
                        window,
                        cx,
                    ),
                }
            },
            window,
            cx,
        );
    }

    pub(super) fn load_recovery_snapshot(
        &mut self,
        repo: GitContext,
        consent: Option<mt_remote::RemoteGitRecoveryConsent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let captured = repo.clone();
        let store = self.store.clone();
        let panel = cx.entity().downgrade();
        let recovery_only = consent.is_some();
        window.spawn(cx, async move |cx| {
                let result = mt_remote::run_git_on(&captured, move || {
                    if let Some(consent) = consent { repo.recovery_snapshot_with_consent(consent) }
                    else { repo.recovery_snapshot() }
                }).await;
                let _ = cx.update(|window, cx| match result {
                    Ok(snapshot) => {
                        let format_time = |time: std::time::SystemTime| crate::git::history::format_absolute_time(time.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs().min(i64::MAX as u64) as i64);
                        let observed = format_time(snapshot.observed);
                        let occurred = format_time(snapshot.occurred);
                        let message = match (recovery_only, snapshot.completion_proven) {
                            (true, true) => t("gitHistory", "recoveryOnlyProven"),
                            (true, false) => t("gitHistory", "recoveryOnlyRisk"),
                            (false, true) => t("gitHistory", "recoveryProven"),
                            (false, false) => t("gitHistory", "recoveryRisk"),
                        };
                        let mut detail = vec![
                            git_ui::repo_label(&captured.with_path(snapshot.repo.clone())),
                            remote_operation_label(snapshot.operation).to_string(),
                            tr!("gitHistory", "recoveryOccurred", timestamp = occurred),
                            format!("HEAD: {}", snapshot.head.as_deref().unwrap_or("∅")),
                            tr!(
                                "gitHistory",
                                "recoverySignature",
                                refs = snapshot.signature.refs,
                                index = snapshot.signature.index
                            ),
                            tr!(
                                "gitHistory",
                                "recoveryOperation",
                                operation = snapshot.op.kind.map(|kind| match kind { RepoOpKind::Merge => t("gitBranch", "opMerge"), RepoOpKind::Rebase => t("gitBranch", "opRebase"), RepoOpKind::CherryPick => t("gitLog", "action.cherryPick"), RepoOpKind::Revert => t("gitLog", "action.revert") }).unwrap_or_else(|| t("gitHistory", "recoveryClean")),
                                conflicts = snapshot.op.conflicted
                            ),
                            tr!(
                                "gitHistory",
                                "recoveryFiles",
                                count = snapshot.files.len(),
                                timestamp = observed
                            ),
                        ];
                        detail.extend(snapshot.files.iter().take(20).map(|file| {
                            format!("{} · {} / {}", file.path,
                                recovery_status_label(file.staged_status.as_ref()),
                                recovery_status_label(file.unstaged_status.as_ref()))
                        }));
                        let context = captured.clone();
                        Confirm::new(t("gitHistory", "verifyWrite"), message)
                            .detail(detail)
                            .ok_text(t("gitHistory", if recovery_only { "acknowledgeReadOnlyRecovery" } else { "acknowledgeRecovery" }))
                            .open(
                                move |window, cx| {
                                    let context = context.clone();
                                    let executor = context.clone();
                                    let snapshot = snapshot.clone();
                                    let store = store.clone();
                                    let panel = panel.clone();
                                    window
                                        .spawn(cx, async move |cx| {
                                            let result =
                                                mt_remote::run_git_on(&executor, move || {
                                                    context.acknowledge_recovery(&snapshot)
                                                })
                                                .await;
                                            let _ = cx.update(|window, cx| match result {
                                                Ok(()) => {
                                                    git_ui::toast_for_repo(
                                                        &store,
                                                        &executor,
                                                        t("gitHistory", if recovery_only { "readOnlyVerified" } else { "writeVerified" })
                                                            .to_string(),
                                                        Tone::Info,
                                                        cx,
                                                    );
                                                    let _ = panel.update(cx, |panel, cx| {
                                                        if panel
                                                            .project_context
                                                            .as_ref()
                                                            .and_then(GitContext::project_id)
                                                            == executor.project_id()
                                                        {
                                                            if crate::git::backend::remote_git_enabled(panel.store.read(cx), executor.project_id().unwrap_or("")) { panel.refresh_after_write(cx); } else { cx.notify(); }
                                                        }
                                                    });
                                                }
                                                Err(err) => git_ui::show_git_error(
                                                    t("gitHistory", "verifyWrite"),
                                                    git_ui::error_text_for_store(&err, &store, cx),
                                                    window,
                                                    cx,
                                                ),
                                            });
                                        })
                                        .detach();
                                },
                                window,
                                cx,
                            );
                    }
                    Err(err) => git_ui::show_git_error(
                        t("gitHistory", "verifyWrite"),
                        git_ui::error_text_for_store(&err, &store, cx),
                        window,
                        cx,
                    ),
                });
            })
            .detach();
    }
}
