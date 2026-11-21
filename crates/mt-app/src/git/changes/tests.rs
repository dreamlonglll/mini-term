use super::*;

fn test_project(id: &str, path: &str) -> mt_config::ProjectConfig {
    mt_config::ProjectConfig {
        id: id.into(),
        name: id.into(),
        path: path.into(),
        ..Default::default()
    }
}

#[gpui::test]
async fn local_amend_toggle_is_immediate_and_failure_cancels_it(cx: &mut gpui::TestAppContext) {
    cx.skip_drawing();
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let store = cx.new(|cx| AppStore::git_test_store(mt_config::AppConfig::default(), cx));
    let window = cx.add_window(|window, cx| {
        let empty = cx.new(|_| gpui::Empty);
        gpui_component::Root::new(empty, window, cx)
    });
    let changes = cx
        .update_window(window.into(), |_, window, cx| {
            let changes = cx.new(|cx| GitChanges::new(store, window, cx));
            changes.update(cx, |this, cx| {
                this.context = Some(GitContext::local("/consumer-amend-missing"));
                this.sync_input(window, cx);
                this.loaded_once = true;
                this.has_head = true;
                this.commit_input.update(cx, |input, cx| {
                    input.set_value("keep user draft", window, cx)
                });
                this.toggle_amend(window, cx);
                assert!(this.amend);
                assert!(this.amend_info.is_none());
                assert!(!this.amend_verifying);
                assert!(this.commit_blocker(false, cx).is_none());
            });
            changes
        })
        .unwrap();
    cx.condition(&changes, |this, _| this.commit_error.is_some())
        .await;
    cx.update_window(window.into(), |_, window, cx| {
        changes.update(cx, |this, cx| {
            assert!(!this.amend);
            assert_eq!(
                this.commit_input.read(cx).value().as_ref(),
                "keep user draft"
            );
            let connection = serde_json::from_value(serde_json::json!({
                "id": "amend-guard", "name": "test", "host": "invalid.test",
                "port": 22, "user": "tester"
            }))
            .unwrap();
            this.context = Some(GitContext::remote(
                "remote".into(),
                connection,
                "/repo".into(),
            ));
            this.sync_input(window, cx);
            this.commit_input
                .update(cx, |input, cx| input.set_value("remote draft", window, cx));
            this.amend = true;
            let model = this.model(cx);
            assert!(this.commit_blocker_with(false, false, &model, cx).is_some());
            this.amend_info = Some(AmendInfo {
                message: "verified".into(),
                pushed: false,
            });
            this.amend_verifying = true;
            assert!(this.commit_blocker_with(false, false, &model, cx).is_some());
            this.amend_verifying = false;
            assert!(this.commit_blocker_with(false, false, &model, cx).is_none());
        });
    })
    .unwrap();
    drop(changes);
    cx.run_until_parked();
}

#[gpui::test]
async fn parent_commits_selected_child_repo_but_switching_blocks_submission(
    cx: &mut gpui::TestAppContext,
) {
    cx.skip_drawing();
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    cx.update(crate::toast::init);
    let root = std::env::temp_dir().join(format!(
        "mini-term-parent-commit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let repo = root.join("repo");
    let parent_id = format!("parent-{}", root.file_name().unwrap().to_string_lossy());
    let child_id = format!("child-{parent_id}");
    let other_id = format!("other-{parent_id}");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "t@e"]);
    std::fs::write(repo.join("file"), "first\n").unwrap();
    git(&["add", "file"]);
    let config = mt_config::AppConfig {
        projects: vec![
            test_project(&parent_id, root.to_str().unwrap()),
            test_project(&child_id, repo.to_str().unwrap()),
            test_project(&other_id, root.join("other").to_str().unwrap()),
        ],
        last_active_project_id: Some(parent_id.clone()),
        ..Default::default()
    };
    let store = cx.new(|cx| AppStore::git_test_store(config, cx));
    let context = cx.update(|cx| {
        crate::git::panel::set_test_selected_repo(
            store.read(cx),
            &parent_id,
            repo.to_str().unwrap(),
        );
        crate::git::backend::project_context(store.read(cx), &parent_id)
            .with_path(repo.to_string_lossy())
    });
    let window = cx.add_window(|window, cx| {
        let empty = cx.new(|_| gpui::Empty);
        gpui_component::Root::new(empty, window, cx)
    });
    let changes = cx
        .update_window(window.into(), |_, window, cx| {
            let changes = cx.new(|cx| GitChanges::new(store.clone(), window, cx));
            changes.update(cx, |this, cx| {
                this.context = Some(context.clone());
                this.sync_input(window, cx);
                this.apply_snapshot(load_snapshot(&context), false, cx);
                this.commit_input.update(cx, |input, cx| {
                    input.set_value("parent submission", window, cx)
                });
                assert!(git_ui::repo_is_current(&store, &context, cx));
                assert_eq!(
                    git_ui::repo_project_id(&store, &context, cx).as_deref(),
                    Some(parent_id.as_str())
                );
                assert!(this.commit_blocker(false, cx).is_none());
                this.commit(false, false, window, cx);
                assert!(
                    this.committing,
                    "parent source must submit its selected child repository"
                );
            });
            changes
        })
        .unwrap();
    cx.condition(&changes, |this, _| {
        !this.committing && this.loaded_once && this.changes.is_empty()
    })
    .await;
    assert_eq!(
        git(&["log", "-1", "--format=%s"]).trim(),
        "parent submission"
    );
    let committed = git(&["rev-parse", "HEAD"]);
    std::fs::write(repo.join("file"), "second\n").unwrap();
    git(&["add", "file"]);
    cx.update_window(window.into(), |_, window, cx| {
        store.update(cx, |state, _| {
            state.active_project_id = Some(other_id.clone())
        });
        changes.update(cx, |this, cx| {
            this.apply_snapshot(load_snapshot(&context), false, cx);
            this.commit_input.update(cx, |input, cx| {
                input.set_value("must remain draft", window, cx)
            });
            assert_eq!(this.context.as_ref(), Some(&context));
            assert!(!git_ui::repo_is_current(&store, &context, cx));
            assert!(
                this.commit_blocker(false, cx).is_none(),
                "only changed source blocks submission"
            );
            this.commit(false, false, window, cx);
            assert!(
                !this.committing,
                "hidden parent must not submit after switching projects"
            );
            assert_eq!(
                this.commit_input.read(cx).value().as_ref(),
                "must remain draft"
            );
        });
        crate::git::panel::set_test_selected_repo(store.read(cx), &parent_id, "");
    })
    .unwrap();
    assert_eq!(git(&["rev-parse", "HEAD"]), committed);
    assert_eq!(git(&["diff", "--cached", "--name-only"]).trim(), "file");
    drop(changes);
    cx.run_until_parked();
    std::fs::remove_dir_all(root).unwrap();
}

#[gpui::test]
async fn diff_reload_follows_last_staged_file_and_periodic_amend_stays_enabled(
    cx: &mut gpui::TestAppContext,
) {
    cx.skip_drawing();
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = std::env::temp_dir().join(format!(
        "mini-term-diff-consumer-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=t@e"])
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::write(root.join("file"), "base\n").unwrap();
    git(&["add", "file"]);
    git(&["commit", "-q", "-m", "first"]);
    std::fs::write(root.join("file"), "modified\n").unwrap();
    git(&["add", "file"]);
    let context = GitContext::local_for_project("source", root.to_string_lossy());
    let config = mt_config::AppConfig {
        projects: vec![test_project("source", root.to_str().unwrap())],
        last_active_project_id: Some("source".into()),
        ..Default::default()
    };
    let store = cx.new(|cx| AppStore::git_test_store(config, cx));
    let window = cx.add_window(|window, cx| {
        let empty = cx.new(|_| gpui::Empty);
        gpui_component::Root::new(empty, window, cx)
    });
    let (changes, hooks) = cx
        .update_window(window.into(), |_, window, cx| {
            let changes = cx.new(|cx| GitChanges::new(store, window, cx));
            let hooks = changes.update(cx, |this, cx| {
                this.context = Some(context.clone());
                this.sync_input(window, cx);
                this.apply_snapshot(load_snapshot(&context), false, cx);
                this.diff_hooks(&context, Area::Staged, cx)
            });
            (changes, hooks)
        })
        .unwrap();
    let file = cx.update(|cx| changes.read(cx).diff_file(0, Area::Staged));
    cx.update(|cx| hooks.on_switch.as_ref().unwrap()(&file, cx));
    cx.update(|cx| {
        assert_eq!(
            changes.read(cx).cursor.as_ref(),
            Some(&(Area::Staged, "file".into()))
        )
    });
    // A real unstage of the final staged file must return an empty new sequence.
    context.git_unstage(&["file".into()]).unwrap();
    let landed = Rc::new(RefCell::new(None));
    let result = landed.clone();
    cx.update(|cx| {
        hooks.reload_files.as_ref().unwrap()(
            Box::new(move |files, _| {
                *result.borrow_mut() = Some(files);
            }),
            cx,
        )
    });
    cx.condition(&changes, |_, _| landed.borrow().is_some())
        .await;
    assert!(
        landed
            .borrow()
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_empty()
    );
    cx.update_window(window.into(), |_, window, cx| {
        changes.update(cx, |this, cx| {
            this.amend = true;
            this.amend_info = context.amend_info().unwrap();
            this.amend_key = None;
            this.commit_input
                .update(cx, |input, cx| input.set_value("amended", window, cx));
            assert!(this.commit_blocker(false, cx).is_none());
            this.refresh_amend(cx);
            assert!(
                !this.amend_verifying,
                "periodic verification must not flash disabled"
            );
            assert!(this.commit_blocker(false, cx).is_none());
        });
    })
    .unwrap();
    cx.condition(&changes, |this, _| this.amend_key.is_some())
        .await;
    // A periodic local metadata failure must preserve the user's amend intent
    // and the last verified metadata, unlike a failure while first toggling it.
    std::fs::remove_dir_all(root.join(".git")).unwrap();
    let landed = Rc::new(Cell::new(false));
    let signal = landed.clone();
    let subscription = cx.update(|cx| cx.observe(&changes, move |_, _| signal.set(true)));
    cx.update(|cx| {
        changes.update(cx, |this, cx| {
            this.amend_prefill = Some("first".into());
            this.refresh_amend(cx);
        });
    });
    cx.condition(&changes, |_, _| landed.get()).await;
    cx.update(|cx| {
        let this = changes.read(cx);
        assert!(this.amend);
        assert_eq!(this.amend_prefill.as_deref(), Some("first"));
        assert_eq!(this.amend_info.as_ref().unwrap().message.trim(), "first");
        assert!(this.amend_key.is_some());
        assert!(this.commit_error.is_none());
        assert_eq!(this.commit_input.read(cx).value().as_ref(), "amended");
        assert!(this.commit_blocker(false, cx).is_none());
    });
    drop(subscription);
    drop(hooks);
    drop(changes);
    cx.run_until_parked();
    std::fs::remove_dir_all(root).unwrap();
}

#[gpui::test]
async fn undo_drafts_amend_and_subscriptions_follow_captured_repository(
    cx: &mut gpui::TestAppContext,
) {
    cx.skip_drawing();
    // run_git_on uses real blocking I/O: explicitly permit external wakes
    // and await its UI landing before the scheduler/test context is dropped.
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let store = cx.new(|cx| AppStore::git_test_store(mt_config::AppConfig::default(), cx));
    let window = cx.add_window(|window, cx| {
        let empty = cx.new(|_| gpui::Empty);
        gpui_component::Root::new(empty, window, cx)
    });
    let (changes, a_input, b_input, b_revision, blank) = cx
        .update_window(window.into(), |_, window, cx| {
            let changes = cx.new(|cx| GitChanges::new(store, window, cx));
            let a = GitContext::local("/consumer-draft-a");
            let b = GitContext::local("/consumer-draft-b");
            let (a_input, b_input, b_revision, blank) = changes.update(cx, |this, cx| {
                this.context = Some(a.clone());
                this.sync_input(window, cx);
                let a_input = this.commit_input.clone();
                let a_revision = this.input_revision.clone();
                a_input.update(cx, |input, cx| input.set_value("A user draft", window, cx));
                this.context = Some(b.clone());
                this.sync_input(window, cx);
                this.commit_input
                    .update(cx, |input, cx| input.set_value("B user draft", window, cx));
                let b_input = this.commit_input.clone();
                let b_revision = this.input_revision.clone();
                this.restore_message_on(&a, "undone A", window, cx);
                assert_eq!(this.commit_input.read(cx).value().as_ref(), "B user draft");
                assert_eq!(a_input.read(cx).value().as_ref(), "A user draft");
                let pending = this.pending_messages[&a.key()][0].clone();
                assert_eq!(pending.message, "undone A");
                this.restore_pending_message(&a, pending.id, window, cx);
                assert_eq!(a_input.read(cx).value().as_ref(), "undone A");
                assert!(this.pending_messages[&a.key()].is_empty());
                assert_eq!(this.saved_messages[&a.key()][0].message, "A user draft");
                assert_eq!(this.commit_input.read(cx).value().as_ref(), "B user draft");
                // Stable ids prevent a stale menu from consuming the next pending item.
                this.queue_pending_message(a.key(), "second undone A".into());
                this.restore_pending_message(&a, pending.id, window, cx);
                assert_eq!(this.pending_messages[&a.key()].len(), 1);
                assert_eq!(
                    this.pending_messages[&a.key()][0].message,
                    "second undone A"
                );
                let groups = this.message_history_entries(
                    vec![CommitMessageEntry {
                        message: "recent commit".into(),
                        summary: "recent commit".into(),
                        timestamp: 0,
                    }],
                    &a,
                    cx,
                );
                assert_eq!(
                    groups
                        .iter()
                        .filter(|entry| matches!(entry, menu::MenuEntry::Header(_)))
                        .count(),
                    3,
                    "pending, saved drafts and recent commits all remain available"
                );
                this.context = Some(a.clone());
                this.sync_input(window, cx);
                assert_eq!(this.commit_input.entity_id(), a_input.entity_id());
                assert!(Rc::ptr_eq(&this.input_revision, &a_revision));
                this.has_head = true;
                this.amend = true;
                this.amend_prefill = Some("old HEAD message".into());
                this.commit_input.update(cx, |input, cx| {
                    input.set_value("old HEAD message", window, cx)
                });
                // Actual switch consumer saves intent and resets verification.
                this.set_repo(None, cx);
                this.sync_input(window, cx);
                let blank = this.commit_input.entity_id();
                this.context = Some(a.clone());
                this.sync_input(window, cx);
                assert!(
                    this.amend,
                    "amend must never become ordinary submit on return"
                );
                assert!(this.amend_info.is_none());
                assert!(
                    !this.amend_verifying,
                    "local draft restoration is nonblocking"
                );
                this.loaded_once = true;
                this.has_head = true;
                this.amend_info = Some(AmendInfo {
                    message: "verified".into(),
                    pushed: false,
                });
                assert!(
                    this.commit_blocker(false, cx).is_none(),
                    "local amend does not wait for metadata"
                );
                this.amend_verifying = false;
                assert!(
                    this.commit_blocker(false, cx).is_none(),
                    "all other amend requirements met"
                );
                this.amend_info = None;
                (a_input, b_input, b_revision, blank)
            });
            (changes, a_input, b_input, b_revision, blank)
        })
        .unwrap();
    let landed = Rc::new(Cell::new(false));
    let signal = landed.clone();
    let subscription = cx.update(|cx| cx.observe(&changes, move |_, _| signal.set(true)));
    cx.condition(&changes, |_, _| landed.get()).await;
    drop(subscription);
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        changes.update(cx, |this, cx| {
            assert!(
                this.amend,
                "failed periodic local verification preserves amend"
            );
            assert!(
                this.amend_info.is_none(),
                "missing fixture repository cannot verify HEAD"
            );
            assert!(this.commit_error.is_none());
            this.amend_prefill = Some("old HEAD message".into());
            assert!(this.commit_blocker(false, cx).is_none());
            this.amend_info = Some(AmendInfo {
                message: "new HEAD message".into(),
                pushed: false,
            });
            let a = this.context.clone().unwrap();
            this.sync_input(window, cx);
            assert_eq!(
                this.commit_input.read(cx).value().as_ref(),
                "new HEAD message"
            );
            for _ in 0..8 {
                this.context = None;
                this.sync_input(window, cx);
                assert_eq!(this.commit_input.entity_id(), blank);
                this.context = Some(a.clone());
                this.sync_input(window, cx);
                assert_eq!(this.commit_input.entity_id(), a_input.entity_id());
            }
            assert_eq!(this.drafts.len(), 1, "only inactive B draft is retained");
            assert!(this.input_subscription.is_some());
            assert!(this.blank_draft.is_some());
            // Prove the inactive subscription is live before dropping it.
            b_input.update(cx, |_, cx| cx.emit(InputEvent::Change));
        });
    })
    .unwrap();
    cx.run_until_parked();
    let live_revision = b_revision.get();
    assert!(
        live_revision > 0,
        "live subscription must receive a drained event"
    );
    cx.update(|cx| changes.update(cx, |this, _| this.drafts.clear()));
    assert_eq!(Rc::strong_count(&b_revision), 1);
    cx.update(|cx| b_input.update(cx, |_, cx| cx.emit(InputEvent::Change)));
    cx.run_until_parked();
    assert_eq!(
        b_revision.get(),
        live_revision,
        "released subscription cannot receive events"
    );
    drop(changes);
    cx.run_until_parked();
}

#[gpui::test]
fn commit_failure_is_inline_only_until_source_context_changes(cx: &mut gpui::TestAppContext) {
    cx.skip_drawing();
    cx.update(gpui_component::init);
    let config = mt_config::AppConfig {
        projects: vec![
            test_project("source", "/consumer-original-repo"),
            test_project("other", "/consumer-another-repo"),
        ],
        last_active_project_id: Some("source".into()),
        ..Default::default()
    };
    let store = cx.new(|cx| AppStore::git_test_store(config, cx));
    let window = cx.add_window(|window, cx| {
        let empty = cx.new(|_| gpui::Empty);
        gpui_component::Root::new(empty, window, cx)
    });
    cx.update_window(window.into(), |_, window, cx| {
        let changes = cx.new(|cx| GitChanges::new(store.clone(), window, cx));
        let source = GitContext::local("/consumer-original-repo");
        changes.update(cx, |this, cx| {
            this.context = Some(source.clone());
            this.sync_input(window, cx);
            this.commit_input.update(cx, |input, cx| {
                input.set_value("keep this draft", window, cx)
            });
            let error = anyhow::anyhow!("original pre-commit failure");
            this.present_commit_failure(&source, &error, window, cx);
            assert!(
                this.commit_error
                    .as_deref()
                    .unwrap()
                    .contains("original pre-commit failure")
            );
            assert!(!crate::prompt::is_open(crate::prompt::kind::GIT_ERROR));
            store.update(cx, |state, _| {
                state.active_project_id = Some("other".into())
            });
            this.commit_error = None;
            this.present_commit_failure(&source, &error, window, cx);
            assert!(
                this.commit_error.is_none(),
                "hidden stale A context must not swallow failure"
            );
            assert!(crate::prompt::is_open(crate::prompt::kind::GIT_ERROR));
            crate::prompt::close_guarded(crate::prompt::kind::GIT_ERROR, window, cx);
            this.context = Some(GitContext::local("/consumer-another-repo"));
            this.commit_error = None;
            this.present_commit_failure(&source, &error, window, cx);
            assert!(
                this.commit_error.is_none(),
                "do not put A failure on B inline form"
            );
            assert!(crate::prompt::is_open(crate::prompt::kind::GIT_ERROR));
            crate::prompt::close_guarded(crate::prompt::kind::GIT_ERROR, window, cx);
            assert_eq!(
                this.commit_input.read(cx).value().as_ref(),
                "keep this draft"
            );
        });
    })
    .unwrap();
}
#[test]
fn auxiliary_metadata_failure_keeps_consumer_file_groups() {
    let files = vec![
        change("staged", Some(GitStatus::Modified), None),
        change(
            "conflict",
            Some(GitStatus::Conflicted),
            Some(GitStatus::Conflicted),
        ),
    ];
    let snap = snapshot_with_metadata(
        files,
        || anyhow::bail!("conflict metadata unavailable"),
        |_| unreachable!("no conflict metadata"),
        || anyhow::bail!("operation metadata unavailable"),
        || anyhow::bail!("commit metadata unavailable"),
        || true,
    )
    .unwrap();
    assert_eq!(group(&snap.changes, Area::Staged), ["staged"]);
    assert_eq!(group(&snap.changes, Area::Conflicts), ["conflict"]);
    let warning = snap.metadata_warning.as_deref().unwrap();
    for detail in ["conflict metadata", "operation metadata", "commit metadata"] {
        assert!(warning.contains(detail));
    }
    assert!(load_snapshot(&GitContext::broken("status unavailable")).is_err());
}

#[gpui::test]
fn conflict_status_batch_blocks_only_the_unreadable_file(cx: &mut gpui::TestAppContext) {
    use mt_project::git::SegmentBlocker;
    cx.skip_drawing();
    cx.update(gpui_component::init);
    let infos = vec![
        ConflictInfo {
            path: "unreadable".into(),
            ours: true,
            theirs: true,
        },
        ConflictInfo {
            path: "ready".into(),
            ours: true,
            theirs: true,
        },
    ];
    let files = infos
        .iter()
        .map(|info| {
            change(
                &info.path,
                Some(GitStatus::Conflicted),
                Some(GitStatus::Conflicted),
            )
        })
        .collect::<Vec<_>>();
    let calls = Cell::new(0);
    let snap = snapshot_with_metadata(
        files.clone(),
        || Ok(infos.clone()),
        |batch| {
            calls.set(calls.get() + 1);
            assert_eq!(batch.len(), 2);
            assert_eq!(batch[0].path, "unreadable");
            assert_eq!(batch[1].path, "ready");
            Ok(vec![
                SegmentStatus::Blocked(SegmentBlocker::Unreadable),
                SegmentStatus::Ready { blocks: 1 },
            ])
        },
        || Ok(None),
        || Ok(CommitTarget::Branch("main".into())),
        || true,
    )
    .unwrap();
    assert_eq!(calls.get(), 1, "all conflicts share one status request");
    assert!(
        snap.metadata_warning.is_none(),
        "file blockers must not disable the group"
    );
    let store = cx.new(|cx| AppStore::git_test_store(mt_config::AppConfig::default(), cx));
    let window = cx.add_window(|window, cx| {
        let empty = cx.new(|_| gpui::Empty);
        gpui_component::Root::new(empty, window, cx)
    });
    cx.update_window(window.into(), |_, window, cx| {
        let changes = cx.new(|cx| GitChanges::new(store, window, cx));
        changes.update(cx, |this, cx| {
            this.apply_snapshot(Ok(snap), false, cx);
            assert!(this.segment_availability("unreadable").is_err());
            assert_eq!(this.segment_availability("ready"), Ok(1));
            assert!(this.metadata_warning.is_none());
        });
    })
    .unwrap();
    let snap = snapshot_with_metadata(
        files,
        || Ok(infos),
        |_| anyhow::bail!("conflict index unavailable"),
        || Ok(None),
        || Ok(CommitTarget::Branch("main".into())),
        || true,
    )
    .unwrap();
    assert!(snap.metadata_warning.is_some());
    assert_eq!(
        snap.segments,
        vec![SegmentStatus::Blocked(SegmentBlocker::Unreadable); 2]
    );
}

#[test]
fn pending_commit_cannot_clear_a_revised_draft() {
    assert!(draft_matches(3, 3, "submitted", "submitted"));
    assert!(!draft_matches(3, 4, "submitted", "new draft"));
    assert!(!draft_matches(3, 5, "submitted", "submitted"));
    assert!(!draft_matches(3, 3, "submitted", "another repository"));
}

#[cfg(unix)]
#[gpui::test]
async fn slow_hook_commit_keeps_captured_backend_and_failure_feedback(
    cx: &mut gpui::TestAppContext,
) {
    use std::os::unix::fs::PermissionsExt;
    cx.skip_drawing();
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    cx.update(crate::toast::init);
    let root = std::env::temp_dir().join(format!(
        "mini-term-commit-consumer-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let a = root.join("a");
    let b = root.join("b");
    for path in [&a, &b] {
        std::fs::create_dir_all(path).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Test"],
            vec!["config", "user.email", "t@e"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(path)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(path.join("file"), "staged\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["add", "file"])
                .current_dir(path)
                .status()
                .unwrap()
                .success()
        );
    }
    let context = GitContext::local_for_project("a", a.to_string_lossy());
    let other = GitContext::local_for_project("b", b.to_string_lossy());
    let hook = a.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\ntouch .git/hook-pending\nwhile [ ! -f .git/hook-release ]; do sleep 0.02; done\necho hook-rejected-original-A >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = mt_config::AppConfig {
        projects: vec![
            test_project("a", a.to_str().unwrap()),
            test_project("b", b.to_str().unwrap()),
        ],
        last_active_project_id: Some("a".into()),
        ..Default::default()
    };
    let store = cx.new(|cx| AppStore::git_test_store(config, cx));
    let window = cx.add_window(|window, cx| {
        let empty = cx.new(|_| gpui::Empty);
        gpui_component::Root::new(empty, window, cx)
    });
    let changes = cx
        .update_window(window.into(), |_, window, cx| {
            let changes = cx.new(|cx| GitChanges::new(store.clone(), window, cx));
            changes.update(cx, |this, cx| {
                this.context = Some(context.clone());
                this.sync_input(window, cx);
                this.apply_snapshot(load_snapshot(&context), false, cx);
                this.commit_input
                    .update(cx, |input, cx| input.set_value("submitted A", window, cx));
                assert!(this.commit_blocker(false, cx).is_none());
                this.commit(false, false, window, cx);
                assert!(this.committing);
            });
            changes
        })
        .unwrap();
    // The UI executor must remain free while the actual commit hook waits.
    let pending_path = a.join(".git/hook-pending");
    let until = Instant::now() + Duration::from_secs(10);
    while !pending_path.exists() && Instant::now() < until {
        cx.background_executor
            .timer(Duration::from_millis(10))
            .await;
    }
    assert!(
        pending_path.exists(),
        "actual GitChanges commit did not enter hook"
    );
    cx.update_window(window.into(), |_, window, cx| {
        changes.update(cx, |this, cx| {
            this.commit_input
                .update(cx, |input, cx| input.set_value("new A", window, cx));
            // Hidden drawer: store changes, while GitChanges intentionally stays on A.
            store.update(cx, |state, _| state.active_project_id = Some("b".into()));
            assert_eq!(this.context.as_ref(), Some(&context));
            assert!(!git_ui::repo_is_current(&store, &context, cx));
        });
    })
    .unwrap();
    std::fs::write(a.join(".git/hook-release"), "").unwrap();
    cx.condition(&changes, |this, _| !this.committing).await;
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        changes.update(cx, |this, cx| {
            assert!(
                this.commit_error.is_none(),
                "hidden A failure must be visible, not inline"
            );
            assert!(crate::prompt::is_open(crate::prompt::kind::GIT_ERROR));
            crate::prompt::close_guarded(crate::prompt::kind::GIT_ERROR, window, cx);
            assert_eq!(this.commit_input.read(cx).value().as_ref(), "new A");
            this.context = Some(other.clone());
            this.sync_input(window, cx);
            this.commit_input
                .update(cx, |input, cx| input.set_value("draft B", window, cx));
            // An old stage-all confirmation cannot commit B after switching.
            this.loaded_once = true;
            this.changes = vec![change(
                "file",
                Some(GitStatus::Added),
                Some(GitStatus::Modified),
            )];
            this.invalidate_model();
            this.commit_confirmed(&context, false, true, window, cx);
            assert!(!this.committing);
            assert_eq!(this.commit_input.read(cx).value().as_ref(), "draft B");
        });
    })
    .unwrap();
    assert!(
        !context.head_exists(),
        "rejected A commit must not move HEAD"
    );
    assert!(
        !other.head_exists(),
        "captured A confirmation must not submit B"
    );
    drop(changes);
    cx.run_until_parked();
    std::fs::remove_dir_all(root).unwrap();
}

/// 变基中「采用 ours / theirs」换成写清方向的文案(ours = 变基目标,theirs = 你的
/// 改动);合并 / 拣选 / 还原 / 没有进行中的操作时维持「我方 / 对方」。
#[test]
fn 变基时采用一侧的文案写清方向() {
    use ConflictSide::{Ours, Theirs};
    for op in [
        None,
        Some(RepoOpKind::Merge),
        Some(RepoOpKind::CherryPick),
        Some(RepoOpKind::Revert),
    ] {
        assert_eq!(take_side_key(Ours, false, op), "takeOurs");
        assert_eq!(take_side_key(Theirs, true, op), "takeTheirsDeleted");
    }
    let rebase = Some(RepoOpKind::Rebase);
    assert_eq!(take_side_key(Ours, false, rebase), "takeOursRebase");
    assert_eq!(take_side_key(Ours, true, rebase), "takeOursRebaseDeleted");
    assert_eq!(take_side_key(Theirs, false, rebase), "takeTheirsRebase");
    assert_eq!(
        take_side_key(Theirs, true, rebase),
        "takeTheirsRebaseDeleted"
    );
    // 文案里真写着方向:theirs 那条点明是「你的改动」
    let theirs = mt_i18n::t_in(mt_i18n::Locale::Zh, "gitCommit", "takeTheirsRebase");
    assert!(theirs.contains("你的改动"), "{theirs}");
}

/// amend 的「已推送」判定按引用签名缓存:键没变才沿用,读不到当作变了。
#[test]
fn amend_信息按引用签名缓存() {
    assert!(!amend_stale(Some(7), Some(7)), "HEAD 与远程跟踪分支都没动");
    assert!(amend_stale(Some(7), Some(8)), "提交 / 推送 / fetch 之后");
    assert!(amend_stale(None, Some(7)), "还没取过");
    assert!(amend_stale(Some(7), None), "签名读不到时宁可重取");
    assert!(amend_stale(None, None));
}

/// 真仓库上走一遍:第一次取到信息,键不变时不重取,提交之后重取。
#[test]
fn amend_信息键不变不重取() {
    let root = std::env::temp_dir().join(format!(
        "mini-term-amend-key-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@e"])
            .args(args)
            .current_dir(&root)
            .output()
            .expect("git 起不来");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::write(root.join("a.txt"), "a\n").unwrap();
    git(&["add", "a.txt"]);
    git(&["commit", "-q", "-m", "first"]);

    let context = GitContext::local(root.to_string_lossy().into_owned());
    let (key, info) = fetch_amend(&context, None).expect("还没取过:要取");
    assert!(key.is_some());
    assert_eq!(
        info.map(|i| i.message.trim().to_string()).as_deref(),
        Some("first")
    );
    assert!(fetch_amend(&context, key).is_none(), "键没变:不重取");

    std::fs::write(root.join("a.txt"), "b\n").unwrap();
    git(&["commit", "-q", "-am", "second"]);
    let (new_key, info) = fetch_amend(&context, key).expect("HEAD 动了:重取");
    assert_ne!(new_key, key);
    assert_eq!(
        info.map(|i| i.message.trim().to_string()).as_deref(),
        Some("second")
    );
    std::fs::remove_dir_all(&root).ok();
}

fn change(path: &str, staged: Option<GitStatus>, unstaged: Option<GitStatus>) -> ChangeFileStatus {
    ChangeFileStatus {
        path: path.to_string(),
        old_path: None,
        staged_status: staged,
        unstaged_status: unstaged,
        status_label: String::new(),
    }
}

fn group(changes: &[ChangeFileStatus], area: Area) -> Vec<&str> {
    changes
        .iter()
        .filter(|c| in_area(c, area))
        .map(|c| c.path.as_str())
        .collect()
}

/// 四分组口径:未跟踪不落进「未暂存」,而**同一个文件可以同时进 staged 与
/// unstaged**(部分暂存);冲突文件只进冲突组。
#[test]
fn 分组口径含部分暂存与冲突() {
    let changes = [
        change("a.rs", Some(GitStatus::Added), None),
        // 部分暂存:两组都要出现
        change("b.rs", Some(GitStatus::Modified), Some(GitStatus::Modified)),
        change("c.rs", None, Some(GitStatus::Modified)),
        change("d.rs", None, Some(GitStatus::Untracked)),
        // git2 里冲突文件两侧都是 conflicted
        change(
            "e.rs",
            Some(GitStatus::Conflicted),
            Some(GitStatus::Conflicted),
        ),
    ];
    assert_eq!(group(&changes, Area::Conflicts), vec!["e.rs"]);
    assert_eq!(group(&changes, Area::Staged), vec!["a.rs", "b.rs"]);
    assert_eq!(group(&changes, Area::Unstaged), vec!["b.rs", "c.rs"]);
    assert_eq!(group(&changes, Area::Untracked), vec!["d.rs"]);
}

/// 「全部暂存并提交」要先确认的只有部分暂存的文件:只在一组里的、未跟踪的、
/// 冲突中的都不算(冲突时提交按钮本来就被拦下)。
#[test]
fn 全部暂存并提交只为部分暂存的文件确认() {
    let changes = [
        change("a.rs", Some(GitStatus::Added), None),
        change("b.rs", Some(GitStatus::Modified), Some(GitStatus::Modified)),
        change("c.rs", None, Some(GitStatus::Modified)),
        change("d.rs", None, Some(GitStatus::Untracked)),
        change("e.rs", Some(GitStatus::Added), Some(GitStatus::Deleted)),
        change(
            "f.rs",
            Some(GitStatus::Conflicted),
            Some(GitStatus::Conflicted),
        ),
    ];
    assert_eq!(partially_staged_paths(&changes), vec!["b.rs", "e.rs"]);
    assert!(partially_staged_paths(&changes[..1]).is_empty());
    // 确认框正文里真带着文件数
    let text = mt_i18n::t_in(mt_i18n::Locale::Zh, "gitCommit", "stageAllPartialConfirm");
    assert!(text.contains("{count}"), "{text}");
}

/// 列表高度按眼下的数据算(E2E r4 A12:外部改动多出「未暂存」组的那一帧,按上一帧量的
/// 高度分区,「未跟踪」组被挤出可见区)。分组标题 28 + 上边距 4、行 28、内边距 12;
/// 折起来的分组只算标题;树形视图把目录行也算上;没有变更时说不准。
#[test]
fn 列表高度按数据算() {
    let none_dirs = HashSet::new();
    let none_groups = HashSet::new();
    let height = |changes: &[ChangeFileStatus], groups: &HashSet<Area>, tree: bool| {
        ListModel::build(changes, groups, &none_dirs, tree).height
    };
    let untracked = vec![change("src/new.ts", None, Some(GitStatus::Untracked))];
    assert_eq!(
        height(&untracked, &none_groups, false),
        Some(12.0 + 32.0 + 28.0)
    );
    // 多出一组未暂存:两组标题 + 两行
    let mut both = untracked.clone();
    both.insert(0, change("src/api.ts", None, Some(GitStatus::Modified)));
    assert_eq!(
        height(&both, &none_groups, false),
        Some(12.0 + 2.0 * 32.0 + 2.0 * 28.0)
    );
    // 折起来的分组只算标题
    let folded: HashSet<Area> = [Area::Untracked].into_iter().collect();
    assert_eq!(
        height(&both, &folded, false),
        Some(12.0 + 2.0 * 32.0 + 28.0)
    );
    // 树形:每组多一行目录 src/
    assert_eq!(
        height(&both, &none_groups, true),
        Some(12.0 + 2.0 * 32.0 + 4.0 * 28.0)
    );
    assert_eq!(height(&[], &none_groups, false), None);
}

/// 取哪一个 status 由所在区决定 —— 部分暂存的文件在两组里显示的字母不同。
#[test]
fn 状态按区取值() {
    let file = change("b.rs", Some(GitStatus::Added), Some(GitStatus::Deleted));
    assert_eq!(status_label_for(status_in_area(&file, Area::Staged)), "A");
    assert_eq!(status_label_for(status_in_area(&file, Area::Unstaged)), "D");
    assert_eq!(
        status_label_for(status_in_area(&file, Area::Untracked)),
        "D",
        "untracked 区同样取 unstagedStatus"
    );
}

/// 提交消息的轻提示:标题按字符数(中文不按字节),开头空行不算标题,
/// 正文前缺空行才提示。
#[test]
fn 提交消息轻提示() {
    assert_eq!(message_hints(""), MessageHints::default());
    let ok = message_hints("feat: 标题\n\n正文");
    assert!(!ok.title_too_long && !ok.missing_blank_line);
    assert_eq!(ok.title_chars, 8);

    let crowded = message_hints("标题\n紧跟正文");
    assert!(crowded.missing_blank_line);

    let long = "字".repeat(73);
    let hints = message_hints(&long);
    assert_eq!(hints.title_chars, 73);
    assert!(hints.title_too_long);
    assert!(
        !message_hints(&"字".repeat(72)).title_too_long,
        "72 正好不算超"
    );

    let leading = message_hints("\n\n标题\n");
    assert_eq!(leading.title_chars, 2);
    assert!(!leading.missing_blank_line, "只有标题没有正文");
    assert!(
        !message_hints("标题\n   \n").missing_blank_line,
        "空白行算空行"
    );
}

#[test]
fn 确认框文件预览最多五个() {
    let paths: Vec<String> = (1..=7).map(|i| format!("f{i}")).collect();
    let lines = preview_paths(&paths);
    assert_eq!(lines.len(), 6);
    assert_eq!(lines[0], "• f1");
    assert!(lines[5].contains('2'), "{lines:?}");
    assert_eq!(preview_paths(&paths[..2]).len(), 2);
}

/// 折叠 key 带区名:同一路径在各个区各自独立折叠。
#[test]
fn 折叠_key_按区隔离() {
    let paths = [(0, "src/a.rs"), (1, "src/b.rs")];
    let mut collapsed = HashSet::new();
    collapsed.insert("staged:src".to_string());

    let rows = area_tree_rows(&paths, Area::Staged, &collapsed);
    assert_eq!(rows.len(), 1, "staged 区的 src 折起来了,只剩目录行");

    let rows = area_tree_rows(&paths, Area::Unstaged, &collapsed);
    assert_eq!(rows.len(), 3, "unstaged 区不受影响");
    assert!(matches!(rows[1], ChangeTreeRow::File { depth: 1, .. }));
}

/// 树形视图的行序(渲染、键盘、Shift 连选、diff 页签前后换文件都按它):目录在前、
/// 单链目录压缩成一行;键盘走的文件顺序与画出来的行序一致。
#[test]
fn 树形行序与键盘顺序一致() {
    let paths = [
        (0, "z.md"),
        (1, "src/ui/panel/a.rs"),
        (2, "src/ui/panel/b.rs"),
        (3, "docs/x.md"),
    ];
    let rows = area_tree_rows(&paths, Area::Unstaged, &HashSet::new());
    let dirs: Vec<&str> = rows
        .iter()
        .filter_map(|r| match r {
            ChangeTreeRow::Dir { name, .. } => Some(name.as_str()),
            ChangeTreeRow::File { .. } => None,
        })
        .collect();
    assert_eq!(dirs, ["docs", "src/ui/panel"], "src/ui/panel 压缩成一行");
    assert_eq!(git_ui::row_file_order(&rows), [3, 1, 2, 0]);
}

// ── 派生模型与换掉的逐帧现算逐行对账 ──
//
// 下面几个 `old_*` 是换成派生模型之前 render / `list_height` / `area_order` /
// `nav_rows` 的原样实现,留在测试里当对照:模型的行序、高度、显示顺序、键盘顺序
// 必须与它们逐项一致(界面零变化的判据)。

fn old_tree_rows(
    paths: &[(usize, &str)],
    area: Area,
    collapsed: &HashSet<String>,
) -> Vec<ChangeTreeRow> {
    git_ui::flatten_change_tree(&git_ui::build_change_tree(paths), &|full_path| {
        collapsed.contains(&dir_key(area, full_path))
    })
}

fn old_group_indices(changes: &[ChangeFileStatus], area: Area) -> Vec<usize> {
    changes
        .iter()
        .enumerate()
        .filter(|(_, c)| in_area(c, area))
        .map(|(i, _)| i)
        .collect()
}

fn old_group_paths(changes: &[ChangeFileStatus], area: Area) -> Vec<(usize, &str)> {
    old_group_indices(changes, area)
        .into_iter()
        .map(|i| (i, changes[i].path.as_str()))
        .collect()
}

fn old_list_height(
    changes: &[ChangeFileStatus],
    collapsed_groups: &HashSet<Area>,
    collapsed_dirs: &HashSet<String>,
    tree_mode: bool,
) -> Option<f32> {
    if changes.is_empty() {
        return None;
    }
    let mut height = LIST_PAD_Y;
    for area in Area::ORDER {
        let paths = old_group_paths(changes, area);
        if paths.is_empty() {
            continue;
        }
        height += GROUP_HEADER_GAP + GROUP_HEADER_H;
        if collapsed_groups.contains(&area) {
            continue;
        }
        let rows = if tree_mode {
            old_tree_rows(&paths, area, collapsed_dirs).len()
        } else {
            paths.len()
        };
        height += rows as f32 * ROW_HEIGHT;
    }
    Some(height)
}

fn old_area_order(
    changes: &[ChangeFileStatus],
    area: Area,
    collapsed_dirs: &HashSet<String>,
    tree_mode: bool,
) -> Vec<usize> {
    if !tree_mode {
        return old_group_indices(changes, area);
    }
    let paths = old_group_paths(changes, area);
    git_ui::row_file_order(&old_tree_rows(&paths, area, collapsed_dirs))
}

fn old_nav_rows(
    changes: &[ChangeFileStatus],
    collapsed_groups: &HashSet<Area>,
    collapsed_dirs: &HashSet<String>,
    tree_mode: bool,
) -> Vec<(Area, usize)> {
    Area::ORDER
        .into_iter()
        .filter(|area| !collapsed_groups.contains(area))
        .flat_map(|area| {
            old_area_order(changes, area, collapsed_dirs, tree_mode)
                .into_iter()
                .map(move |i| (area, i))
        })
        .collect()
}

/// 旧 `render_list` 往滚动容器里 push 的子元素序列(换成本模块的行类型)。
fn old_render_rows(
    changes: &[ChangeFileStatus],
    collapsed_groups: &HashSet<Area>,
    collapsed_dirs: &HashSet<String>,
    tree_mode: bool,
) -> Vec<ListRow> {
    let mut rows = Vec::new();
    for area in Area::ORDER {
        let indices = old_group_indices(changes, area);
        if indices.is_empty() {
            continue;
        }
        let collapsed = collapsed_groups.contains(&area);
        rows.push(ListRow::Header {
            area,
            count: indices.len(),
            collapsed,
        });
        if collapsed {
            continue;
        }
        if tree_mode {
            let paths = old_group_paths(changes, area);
            for row in old_tree_rows(&paths, area, collapsed_dirs) {
                rows.push(match row {
                    ChangeTreeRow::Dir {
                        name,
                        full_path,
                        depth,
                        collapsed,
                        file_count,
                    } => ListRow::Dir {
                        area,
                        name,
                        full_path,
                        depth,
                        collapsed,
                        file_count,
                    },
                    ChangeTreeRow::File { index, depth } => ListRow::File { area, index, depth },
                });
            }
        } else {
            rows.extend(indices.into_iter().map(|index| ListRow::File {
                area,
                index,
                depth: 0,
            }));
        }
    }
    rows
}

/// 测试用的确定性伪随机数(不引依赖)。
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn pick(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// 一份随机的变更列表:路径不重复(git 给的就不重复),目录名混大小写、文件与目录
/// 同名,状态覆盖部分暂存 / 冲突 / 未跟踪 / 重命名。
fn random_changes(rng: &mut Lcg, count: usize) -> Vec<ChangeFileStatus> {
    const DIRS: [&str; 7] = ["src", "Src", "ui", "UI", "a", "docs", "x"];
    const FILES: [&str; 7] = ["a.rs", "A.rs", "b.md", "x", "ui", "src", "Z.txt"];
    const STATUS: [(Option<GitStatus>, Option<GitStatus>); 8] = [
        (Some(GitStatus::Added), None),
        (Some(GitStatus::Modified), Some(GitStatus::Modified)),
        (None, Some(GitStatus::Modified)),
        (None, Some(GitStatus::Untracked)),
        (Some(GitStatus::Conflicted), Some(GitStatus::Conflicted)),
        (Some(GitStatus::Deleted), None),
        (Some(GitStatus::Renamed), Some(GitStatus::Deleted)),
        (None, Some(GitStatus::Deleted)),
    ];
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    while out.len() < count {
        let depth = rng.pick(4);
        let mut path: Vec<&str> = (0..depth).map(|_| DIRS[rng.pick(DIRS.len())]).collect();
        path.push(FILES[rng.pick(FILES.len())]);
        let path = path.join("/");
        if !seen.insert(path.clone()) {
            // 组合空间不大,撞了就换一个(数量给得小,不会死循环)
            continue;
        }
        let (staged, unstaged) = STATUS[rng.pick(STATUS.len())].clone();
        out.push(change(&path, staged, unstaged));
    }
    out
}

/// 某份变更列表里出现过的目录(全部前缀),随机挑一部分按区名拼成折叠 key。
fn random_collapsed_dirs(rng: &mut Lcg, changes: &[ChangeFileStatus]) -> HashSet<String> {
    let mut dirs: Vec<String> = Vec::new();
    for file in changes {
        let parts: Vec<&str> = file.path.split('/').collect();
        for end in 1..parts.len() {
            dirs.push(parts[..end].join("/"));
        }
    }
    let mut out = HashSet::new();
    if dirs.is_empty() {
        return out;
    }
    for _ in 0..rng.pick(4) {
        let area = Area::ORDER[rng.pick(4)];
        out.insert(dir_key(area, &dirs[rng.pick(dirs.len())]));
    }
    out
}

/// 派生模型与旧的逐帧现算逐项一致:行序(连标题 / 目录行的每个字段)、摊开总高、
/// 四组成员与显示顺序、键盘顺序;尺寸表与行一一对应。随机数据覆盖两种视图、分组
/// 折叠、目录折叠(压缩节点折叠 key 是最深那一级的完整路径)。
#[test]
fn 派生模型与逐帧现算逐行一致() {
    let mut rng = Lcg(0x5eed);
    for round in 0..300 {
        let count = rng.pick(40);
        let changes = random_changes(&mut rng, count);
        let groups: HashSet<Area> = Area::ORDER
            .into_iter()
            .filter(|_| rng.pick(4) == 0)
            .collect();
        let dirs = random_collapsed_dirs(&mut rng, &changes);
        for tree in [false, true] {
            let model = ListModel::build(&changes, &groups, &dirs, tree);
            let ctx = format!("round {round} tree {tree} groups {groups:?} dirs {dirs:?}");
            assert_eq!(
                model.rows,
                old_render_rows(&changes, &groups, &dirs, tree),
                "{ctx}"
            );
            assert_eq!(
                model.height,
                old_list_height(&changes, &groups, &dirs, tree),
                "{ctx}"
            );
            for area in Area::ORDER {
                assert_eq!(
                    model.members(area),
                    old_group_indices(&changes, area),
                    "{ctx}"
                );
                assert_eq!(
                    model.order(area),
                    old_area_order(&changes, area, &dirs, tree),
                    "{ctx} {area:?}"
                );
            }
            assert_eq!(
                model.nav().collect::<Vec<_>>(),
                old_nav_rows(&changes, &groups, &dirs, tree),
                "{ctx}"
            );
            assert_eq!(model.sizes.len(), model.rows.len());
            for (size, row) in model.sizes.iter().zip(&model.rows) {
                assert_eq!(f32::from(size.height), row.height());
            }
        }
    }
}

/// 键盘导航的下标映射:文件行 → 行号,行号 → 上下沿(从列表顶边量,含上内边距);
/// 折起来的分组里的文件没有行。
#[test]
fn 文件行的行号与上下沿() {
    let changes = vec![
        change("src/a.rs", Some(GitStatus::Added), None),
        change("src/b.rs", None, Some(GitStatus::Modified)),
        change("new.ts", None, Some(GitStatus::Untracked)),
    ];
    let (no_groups, no_dirs) = (HashSet::new(), HashSet::new());
    let model = ListModel::build(&changes, &no_groups, &no_dirs, false);
    // 已暂存标题 / a / 未暂存标题 / b / 未跟踪标题 / new
    assert_eq!(model.file_row(Area::Staged, 0), Some(1));
    assert_eq!(model.file_row(Area::Unstaged, 1), Some(3));
    assert_eq!(model.file_row(Area::Untracked, 2), Some(5));
    assert_eq!(model.file_row(Area::Staged, 1), None, "b 不在已暂存组");
    assert_eq!(model.row_span(0), Some((4.0, 36.0)), "标题连上外边距 32");
    assert_eq!(model.row_span(1), Some((36.0, 64.0)));
    let last_top = 4.0 + 3.0 * 32.0 + 2.0 * 28.0;
    assert_eq!(model.row_span(5), Some((last_top, 184.0)));
    assert_eq!(model.row_span(6), None);
    // 摊开总高 = 上内边距 + 全部行 + 下内边距,最后一行的下沿再加 8
    assert_eq!(model.height, Some(184.0 + 8.0));

    let folded: HashSet<Area> = [Area::Unstaged].into_iter().collect();
    let model = ListModel::build(&changes, &folded, &no_dirs, false);
    assert_eq!(
        model.file_row(Area::Unstaged, 1),
        None,
        "折起来的分组没有文件行"
    );
    assert_eq!(model.file_row(Area::Untracked, 2), Some(4));
    assert_eq!(model.order(Area::Unstaged), [1], "显示顺序不看分组折叠");
    assert_eq!(
        model.nav().collect::<Vec<_>>(),
        [(Area::Staged, 0), (Area::Untracked, 2)],
        "键盘跳过折起来的分组"
    );
}

/// 滚进视野的偏移与 gpui `scroll_to_item` 的默认策略一致:整行可见不动、上面露不全
/// 顶对齐、下面露不全底对齐、比视口高的行顶对齐;上下沿按整个列表框比。
#[test]
fn 滚进视野只滚最少的量() {
    // 视口 100,行在内容里 [40, 68)
    let reveal = |offset: f32, top: f32, viewport: f32| {
        reveal_offset(offset, top, top + ROW_HEIGHT, viewport)
    };
    assert_eq!(reveal(0.0, 40.0, 100.0), 0.0, "整行可见");
    assert_eq!(reveal(-50.0, 40.0, 100.0), -40.0, "在上面:顶对齐");
    assert_eq!(reveal(0.0, 120.0, 100.0), -48.0, "在下面:底对齐");
    assert_eq!(reveal(-10.0, 4.0, 100.0), -4.0, "露了半截也算");
    assert_eq!(reveal(-30.0, 40.0, 20.0), -40.0, "比视口高:顶对齐");
}

fn snapshot(changes: Vec<ChangeFileStatus>) -> Snapshot {
    Snapshot {
        changes,
        conflicts: Vec::new(),
        segments: Vec::new(),
        op_kind: None,
        target: Some(CommitTarget::Branch("main".into())),
        has_head: true,
        metadata_warning: None,
    }
}

fn landed_from(snap: &Snapshot) -> Landed<'_> {
    Landed {
        changes: &snap.changes,
        conflicts: &snap.conflicts,
        segments: &snap.segments,
        op_kind: snap.op_kind,
        target: snap.target.as_ref(),
        has_head: snap.has_head,
        load_error: None,
        metadata_warning: snap.metadata_warning.as_deref(),
    }
}

/// 落地去重:一模一样的回包不算变;任何一个会进界面(或菜单)的字段变了都算。
#[test]
fn 取数落地没变就不重画() {
    let files = vec![
        change("a.rs", Some(GitStatus::Modified), None),
        change("b.rs", None, Some(GitStatus::Untracked)),
    ];
    let base = snapshot(files.clone());
    let landed = landed_from(&base);
    assert!(!landed.differs(&snapshot(files.clone())), "同一份数据");

    let mut moved = files.clone();
    moved[0].unstaged_status = Some(GitStatus::Modified);
    assert!(landed.differs(&snapshot(moved)), "状态变了");
    let mut renamed = files.clone();
    renamed[1].old_path = Some("old.rs".into());
    assert!(landed.differs(&snapshot(renamed)), "旧路径变了");
    assert!(landed.differs(&snapshot(files[..1].to_vec())), "少了一个");

    let mut next = snapshot(files.clone());
    next.conflicts = vec![ConflictInfo {
        path: "a.rs".into(),
        ours: true,
        theirs: false,
    }];
    assert!(landed.differs(&next), "冲突细节变了(菜单文案跟着它)");
    let mut next = snapshot(files.clone());
    next.segments = vec![SegmentStatus::Ready { blocks: 2 }];
    assert!(
        landed.differs(&next),
        "能不能逐段解决变了(菜单项与行内按钮跟着它)"
    );
    let mut next = snapshot(files.clone());
    next.op_kind = Some(RepoOpKind::Rebase);
    assert!(landed.differs(&next), "进行中的操作变了");
    let mut next = snapshot(files.clone());
    next.target = Some(CommitTarget::Branch("dev".into()));
    assert!(landed.differs(&next), "提交目标(占位符)变了");
    let mut next = snapshot(files.clone());
    next.has_head = false;
    assert!(landed.differs(&next), "HEAD 没了");

    let errored = Landed {
        load_error: Some("boom"),
        ..landed_from(&base)
    };
    assert!(errored.differs(&snapshot(files)), "上次失败、这次成功");
}

/// A missing HEAD changes metadata, never silently clears amend intent.
#[test]
fn 取数落地的_amend_口径() {
    let base = snapshot(Vec::new());
    let mut next = snapshot(Vec::new());
    assert!(!landed_from(&base).differs(&next), "数据没变");
    next.has_head = false;
    assert!(landed_from(&base).differs(&next), "HEAD changed");
    assert!(
        !landed_from(&base).differs(&snapshot(Vec::new())),
        "没勾 amend"
    );
}

/// 取数失败:同一条错误反复落地不算变;换了原因、或此前还有数据都算。
#[test]
fn 取数失败的落地去重() {
    let empty = snapshot(Vec::new());
    let failing = Landed {
        load_error: Some("boom"),
        target: None,
        ..landed_from(&empty)
    };
    assert!(!failing.differs_on_error("boom"));
    assert!(failing.differs_on_error("bang"));
    let full = snapshot(vec![change("a.rs", None, Some(GitStatus::Modified))]);
    assert!(landed_from(&full).differs_on_error("boom"), "列表要清空");
}

/// 行 tooltip 现拼的文字与此前每帧预拼的一致:重命名带旧路径,部分暂存另注一句。
#[test]
fn 文件行提示文字() {
    let text = |status: Option<&GitStatus>| status_text(status).to_string();
    assert_eq!(
        file_row_tip("src/a.rs", None, Some(&GitStatus::Modified), false),
        format!("src/a.rs\n{}", text(Some(&GitStatus::Modified)))
    );
    assert_eq!(
        file_row_tip("b.rs", Some("a.rs"), Some(&GitStatus::Renamed), false),
        format!("a.rs → b.rs\n{}", text(Some(&GitStatus::Renamed)))
    );
    assert_eq!(
        file_row_tip("c.rs", None, Some(&GitStatus::Modified), true),
        format!(
            "c.rs\n{} · {}",
            text(Some(&GitStatus::Modified)),
            t("gitCommit", "partiallyStaged")
        )
    );
}
