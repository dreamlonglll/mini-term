use super::*;

impl GitChanges {
    /// 文件行右键菜单(`GitChanges.tsx:242-256`),多选时作用于整组。
    pub(super) fn file_menu(
        &self,
        index: usize,
        area: Area,
        cx: &mut Context<Self>,
    ) -> Vec<menu::MenuEntry> {
        let this = cx.entity();
        let context = self.context.clone();
        let paths = self.targets(area, index, cx);
        let count = paths.len();
        let many = count > 1;
        let enabled = self.ops_enabled();
        let counted = |single: &'static str, multi: &'static str| -> String {
            if many {
                tr!("gitCommit", multi, count = count.to_string())
            } else {
                single.to_string()
            }
        };
        let diff_files = self.model(cx);
        let diff_order = diff_files.order(area);
        let captured_files = self.diff_files(diff_order, area);
        let captured_current = diff_order.iter().position(|&i| i == index).unwrap_or(0);
        let view_diff = {
            let this = this.clone();
            let context = context.clone();
            let path = self.changes.get(index).map(|file| file.path.clone());
            menu::item(t("gitChanges", "contextViewDiff"), move |window, cx| {
                this.update(cx, |this, cx| {
                    if let Some(context) = context.clone() {
                        if this.context.as_ref() == Some(&context)
                            && let Some(index) = this.changes.iter().position(|file| {
                                Some(&file.path) == path.as_ref() && in_area(file, area)
                            })
                        {
                            this.view_diff(index, area, window, cx);
                            return;
                        }
                        git_diff::open_file_diff_with(
                            this.store.clone(),
                            context,
                            captured_files.clone(),
                            captured_current,
                            git_diff::FileDiffHooks::default(),
                            window,
                            cx,
                        );
                    }
                });
            })
        };

        let mut entries = Vec::new();
        match area {
            Area::Conflicts => {
                let enabled =
                    enabled && self.metadata_warning.is_none() && self.load_error.is_none();
                // 逐段解决:一次一个文件;删除 / 修改冲突、没有成对标记、二进制、编码认不出、
                // 过大时禁用并在副标题写明原因(状态随列表一起取,见 `load_snapshot`)
                let path = self
                    .changes
                    .get(index)
                    .map(|file| file.path.clone())
                    .unwrap_or_default();
                let available = if many {
                    Err(t("gitCommit", "segments.blockedMany"))
                } else {
                    self.segment_availability(&path)
                };
                let segments_this = this.clone();
                let segments_context = context.clone();
                let mut segments = MenuItem::new(t("gitCommit", "segments.menu"))
                    .disabled(!enabled || available.is_err())
                    .on_click(move |window, cx| {
                        let path = path.clone();
                        segments_this.update(cx, |this, cx| {
                            if this.context == segments_context {
                                this.open_segments(path, window, cx);
                            } else if let Some(context) = segments_context.as_ref() {
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
                if let Err(reason) = available {
                    segments = segments.detail(reason);
                }
                entries.push(segments.into());
                entries.push(menu::separator());
                let info = (!many)
                    .then(|| self.changes.get(index))
                    .flatten()
                    .and_then(|file| self.conflicts.iter().find(|c| c.path == file.path));
                for side in [ConflictSide::Ours, ConflictSide::Theirs] {
                    let deleted = info.is_some_and(|c| match side {
                        ConflictSide::Ours => !c.ours,
                        ConflictSide::Theirs => !c.theirs,
                    });
                    let label = t("gitCommit", take_side_key(side, deleted, self.op_kind));
                    let (this, paths) = (this.clone(), paths.clone());
                    let context = context.clone();
                    let mut item =
                        MenuItem::new(label)
                            .disabled(!enabled)
                            .on_click(move |window, cx| {
                                let paths = paths.clone();
                                this.update(cx, |this, cx| {
                                    if let Some(context) = context.clone() {
                                        this.run_op_on(
                                            &context,
                                            move |repo| {
                                                for path in &paths {
                                                    repo.resolve_conflict_with(path, side)?;
                                                }
                                                Ok(())
                                            },
                                            None,
                                            window,
                                            cx,
                                        );
                                    }
                                });
                            });
                    if deleted {
                        item = item.danger();
                    }
                    entries.push(item.into());
                }
                let (this2, paths2) = (this.clone(), paths.clone());
                let context = context.clone();
                entries.push(
                    MenuItem::new(t("gitCommit", "markResolved"))
                        .disabled(!enabled)
                        .on_click(move |window, cx| {
                            let paths = paths2.clone();
                            this2.update(cx, |this, cx| {
                                if this.context == context {
                                    this.mark_resolved(paths, window, cx);
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
                        .into(),
                );
                entries.push(menu::separator());
                entries.push(view_diff);
            }
            Area::Staged => {
                entries.push(view_diff);
                entries.push(menu::separator());
                let (this, paths) = (this.clone(), paths.clone());
                let context = context.clone();
                entries.push(
                    MenuItem::new(counted(t("panels", "unstage"), "unstageSelected"))
                        .disabled(!enabled)
                        .on_click(move |window, cx| {
                            let paths = paths.clone();
                            this.update(cx, |this, cx| {
                                if let Some(context) = context.as_ref() {
                                    this.run_op_on(
                                        context,
                                        move |repo| repo.git_unstage(&paths),
                                        None,
                                        window,
                                        cx,
                                    );
                                }
                            });
                        })
                        .into(),
                );
            }
            Area::Unstaged | Area::Untracked => {
                entries.push(view_diff);
                entries.push(menu::separator());
                {
                    let (this, paths) = (this.clone(), paths.clone());
                    let context = context.clone();
                    entries.push(
                        MenuItem::new(counted(t("panels", "stage"), "stageSelected"))
                            .disabled(!enabled)
                            .on_click(move |window, cx| {
                                let paths = paths.clone();
                                this.update(cx, |this, cx| {
                                    if let Some(context) = context.as_ref() {
                                        this.run_op_on(
                                            context,
                                            move |repo| repo.git_stage(&paths),
                                            None,
                                            window,
                                            cx,
                                        );
                                    }
                                });
                            })
                            .into(),
                    );
                }
                entries.push(menu::separator());
                let discard_label = if area == Area::Untracked {
                    counted(t("gitCommit", "deleteUntracked"), "deleteSelected")
                } else {
                    counted(t("gitChanges", "contextDiscard"), "discardSelected")
                };
                entries.push(
                    MenuItem::new(discard_label)
                        .danger()
                        .disabled(!enabled)
                        .shortcut("Delete")
                        .on_click(move |window, cx| {
                            let paths = paths.clone();
                            this.update(cx, |this, cx| {
                                if this.context == context {
                                    this.discard(area, paths, window, cx);
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
                        .into(),
                );
            }
        }
        // 单文件历史(IDEA Show History)。未跟踪文件还没有历史,多选时没有「那一个文件」
        if !many
            && area != Area::Untracked
            && let Some(file) = self.changes.get(index)
            && let Some(repo) = self.context.clone()
        {
            let (store, repo, rel) = (self.store.clone(), repo, file.path.clone());
            entries.push(menu::separator());
            entries.push(menu::item(
                t("gitLog", "fileHistory.menu"),
                move |window, cx| {
                    crate::git::file_history::open(
                        store.clone(),
                        repo.clone(),
                        rel.clone(),
                        window,
                        cx,
                    );
                },
            ));
        }
        entries
    }
}
