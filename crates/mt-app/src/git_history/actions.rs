//! 提交行的右键菜单(对标 IDEA Log 的右键)与背后的改写操作、对话框。
//!
//! # 菜单为什么异步弹
//!
//! 「已在当前分支历史上」「已推送」要走 git2 的图可达判定 —— 十万级提交的仓库里老提交
//! 要走一段,不能放在 UI 线程上。右键先在后台算 [`CommitRelation`],回来再弹;本地仓库
//! 通常是毫秒级,感觉不到延迟。算不出来(仓库坏了)也照样弹,改写类项置灰并写明原因。
//!
//! # 失败可见
//!
//! 菜单直接触发的操作失败弹可选中复制原文的详情框(与面板上的操作同一口径,见
//! `git_ui::show_git_error`;toast 5s 就收,装不下 hook 的多行输出);对话框里的操作
//! 失败写回对话框就地红字 —— 详情框与 toast 叠在对话框上都不顺手。等待期间对话框被
//! Esc 关掉的话,退到提示框。
//!
//! # 防重复
//!
//! 同一时刻只跑一个改写操作([`GitHistoryContent::busy`]):进行中菜单里的改写项置灰,
//! 过滤栏右侧显示「拣选中…」这类状态;对话框的确认键在等待期间换成「执行中…」并禁用。
//! 跨区的防重入走仓库的写操作闸(`git_ui::gate`):面板在拉取、更改区在提交时,菜单里的
//! 改写项同样置灰并写明原因,点进来也不执行。

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{
    App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, Focusable as _,
    InteractiveElement, IntoElement, ParentElement, Pixels, Point, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::dialog::{Confirm as DialogConfirm, DialogFooter};
use gpui_component::input::{Input, InputState, Textarea, TextareaState};
use mt_project::git::{self, CommitRelation, GitCommitInfo, PickOutcome, ResetMode};

use super::{GitHistoryContent, GitHistoryEvent};
use crate::git_ui::{self, FormOp, error_text};
use crate::i18n::{t, tr};
use crate::menu::{self, MenuEntry, MenuItem};
use crate::prompt::{Confirm, close_guarded, is_open, kind, open_guarded, show_alert};
use crate::ui;

gpui::actions!(
    mini_term,
    [
        /// 「修改提交消息」对话框里 Ctrl/Cmd+Enter 确认(多行框里回车是换行)
        GitLogRewordConfirm
    ]
);

/// 「修改提交消息」对话框正文的键位上下文(`hotkeys.rs` 按它绑 Ctrl/Cmd+Enter)。
pub(crate) const REWORD_CONTEXT: &str = "GitLogReword";

/// 对话框的表单状态。Dialog 的 builder 每帧重跑,可变状态得放实体里。
struct DialogForm {
    /// 执行中 / 上一次失败的原文(git 的 stderr,就地红字)。
    op: FormOp,
    /// 新建分支:创建后检出(默认勾)。
    checkout: bool,
    /// 重置:选中的模式(默认 mixed,与 `git reset` 缺省一致)。
    mode: ResetMode,
    /// 重置:会不会丢掉已推送的提交(后台判定,回来之前按不会)。
    drops_pushed: bool,
}

impl DialogForm {
    fn new() -> Self {
        Self {
            op: FormOp::default(),
            checkout: true,
            mode: ResetMode::Mixed,
            drops_pushed: false,
        }
    }
}

impl Render for DialogForm {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

// ─── 菜单 ─────────────────────────────────────────────────────

/// 右键提交行:后台算关系 → 弹菜单。
pub(super) fn open_commit_menu(
    this: &mut GitHistoryContent,
    index: usize,
    position: Point<Pixels>,
    window: &mut Window,
    cx: &mut Context<GitHistoryContent>,
) {
    let Some(commit) = this.commits.get(index).cloned() else {
        return;
    };
    let repo_key = this.repo_path.clone();
    let repo = PathBuf::from(&this.repo_path);
    let hash = commit.hash.clone();
    cx.spawn_in(window, async move |this, cx| {
        let (relation, remotes) = cx
            .background_executor()
            .spawn(async move {
                let relation = git::commit_relation(&repo, &hash).map_err(|e| error_text(&e));
                let remotes = git::list_remote_names(&repo).unwrap_or_default();
                (relation, remotes)
            })
            .await;
        let _ = this.update_in(cx, |this: &mut GitHistoryContent, window, cx| {
            // 算的这会儿换了仓库:菜单作废
            if this.repo_path != repo_key {
                return;
            }
            let entries = commit_menu(this, &commit, relation, remotes, cx);
            menu::show(position, entries, window, cx);
        });
    })
    .detach();
}

/// 可用就挂动作,不可用就置灰并把原因写在 detail 行。
fn action_item(
    label: impl Into<SharedString>,
    blocked: Option<String>,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> MenuItem {
    let item = MenuItem::new(label);
    match blocked {
        Some(reason) => item.disabled(true).detail(reason),
        None => item.on_click(on_click),
    }
}

fn reason(key: &'static str) -> Option<String> {
    Some(t("gitLog", key).to_string())
}

fn commit_menu(
    this: &GitHistoryContent,
    commit: &GitCommitInfo,
    relation: Result<CommitRelation, String>,
    remotes: Vec<String>,
    cx: &mut Context<GitHistoryContent>,
) -> Vec<MenuEntry> {
    let entity = cx.entity();
    // 自己的改写还在跑 → 另一区的写操作占着这个仓库的闸
    let busy = this.busy.and_then(|_| reason("reason.busy")).or_else(|| {
        git_ui::gate::is_busy(&this.repo_path).then(|| git_ui::gate_busy_text().to_string())
    });
    let (rel, rel_error) = match relation {
        Ok(rel) => (Some(rel), None),
        Err(err) => (
            None,
            Some(tr!("gitLog", "reason.relationFailed", detail = err)),
        ),
    };
    let repo_busy = rel
        .as_ref()
        .filter(|r| r.busy_state)
        .and_then(|_| reason("reason.repoBusy"));
    // 改写类项的公共前置:没在忙 → 关系算得出 → 仓库不在进行中状态
    let base = busy.clone().or(rel_error).or(repo_busy);
    let rel = rel.unwrap_or_default();
    let is_merge = rel.parent_count > 1;

    // 每个菜单项各要一份
    let with = |f: fn(
        &mut GitHistoryContent,
        &GitCommitInfo,
        &CommitRelation,
        &mut Window,
        &mut Context<GitHistoryContent>,
    )| {
        let entity = entity.clone();
        let commit = commit.clone();
        let rel = rel.clone();
        move |window: &mut Window, cx: &mut App| {
            entity.update(cx, |this, cx| f(this, &commit, &rel, window, cx));
        }
    };

    let hash = commit.hash.clone();
    let short = commit.short_hash.clone();
    let index = this.commits.iter().position(|c| c.hash == commit.hash);
    let mut entries: Vec<MenuEntry> = vec![
        menu::item(t("gitLog", "menu.copyFullHash"), move |_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(hash.clone()));
        }),
        menu::item(t("gitLog", "menu.copyShortHash"), move |_window, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(short.clone()));
        }),
        menu::separator(),
        {
            let entity = entity.clone();
            MenuItem::new(t("gitHistoryContent", "viewChanges"))
                .on_click(move |window, cx| {
                    if let Some(index) = index {
                        entity.update(cx, |this, cx| this.view_commit(index, window, cx));
                    }
                })
                .into()
        },
        menu::separator(),
    ];

    // ── 检出 / 建分支 / 建标签
    let already_here = rel.is_head && rel.head_branch.is_none();
    entries.push(
        action_item(
            t("gitLog", "menu.checkout"),
            base.clone().or_else(|| {
                already_here
                    .then(|| reason("reason.alreadyCheckedOut"))
                    .flatten()
            }),
            with(|this, commit, _rel, window, cx| this.confirm_checkout(commit, window, cx)),
        )
        .into(),
    );
    entries.push(
        action_item(
            t("gitLog", "menu.newBranch"),
            busy.clone(),
            with(|this, commit, _rel, window, cx| this.open_branch_dialog(commit, window, cx)),
        )
        .into(),
    );
    entries.push(
        action_item(
            t("gitLog", "menu.newTag"),
            busy.clone(),
            with(|this, commit, _rel, window, cx| this.open_tag_dialog(commit, window, cx)),
        )
        .into(),
    );
    entries.push(menu::separator());

    // ── 拣选 / 还原
    let mut pick = action_item(
        t("gitLog", "menu.cherryPick"),
        base.clone().or_else(|| {
            rel.in_head_history
                .then(|| reason("reason.inHistory"))
                .flatten()
        }),
        with(|this, commit, rel, window, cx| this.cherry_pick(commit, rel, window, cx)),
    );
    let mut revert = action_item(
        t("gitLog", "menu.revert"),
        base.clone().or_else(|| {
            (!rel.in_head_history)
                .then(|| reason("reason.notInHistory"))
                .flatten()
        }),
        with(|this, commit, rel, window, cx| this.revert(commit, rel, window, cx)),
    );
    if is_merge {
        // 合并提交:以第一个父为主线(-m 1),确认框里还会再说一遍
        if pick.is_actionable() {
            pick = pick.detail(t("gitLog", "reason.mergeMainline"));
        }
        if revert.is_actionable() {
            revert = revert.detail(t("gitLog", "reason.mergeMainline"));
        }
    }
    entries.push(pick.into());
    entries.push(revert.into());
    entries.push(menu::separator());

    // ── 重置 / 撤销 / 改消息
    entries.push(
        action_item(
            t("gitLog", "menu.reset"),
            base.clone(),
            with(|this, commit, rel, window, cx| this.open_reset_dialog(commit, rel, window, cx)),
        )
        .into(),
    );
    let head_only = || (!rel.is_head).then(|| reason("reason.notHead")).flatten();
    let mut undo = action_item(
        t("gitLog", "menu.undo"),
        base.clone()
            .or_else(head_only)
            .or_else(|| match rel.parent_count {
                0 => reason("reason.root"),
                1 => None,
                _ => reason("reason.merge"),
            }),
        with(|this, commit, rel, window, cx| this.undo_commit(commit, rel, window, cx)),
    );
    let mut reword = action_item(
        t("gitLog", "menu.reword"),
        base.clone().or_else(head_only),
        with(|this, commit, rel, window, cx| this.open_reword_dialog(commit, rel, window, cx)),
    );
    if rel.pushed {
        if undo.is_actionable() {
            undo = undo.detail(t("gitLog", "reason.pushed"));
        }
        if reword.is_actionable() {
            reword = reword.detail(t("gitLog", "reason.pushed"));
        }
    }
    entries.push(undo.into());
    entries.push(reword.into());

    // ── 指向它的标签:每个一组子菜单(删除 / 推送到各远程)
    let tags = this
        .tags_by_commit
        .get(&commit.hash)
        .cloned()
        .unwrap_or_default();
    if !tags.is_empty() {
        entries.push(menu::separator());
    }
    for tag in tags {
        let mut sub: Vec<MenuEntry> = Vec::new();
        {
            let entity = entity.clone();
            let tag = tag.clone();
            sub.push(
                action_item(
                    t("gitLog", "menu.deleteTag"),
                    busy.clone(),
                    move |window, cx| {
                        entity.update(cx, |this, cx| this.confirm_delete_tag(&tag, window, cx));
                    },
                )
                .danger()
                .into(),
            );
        }
        sub.push(menu::separator());
        if remotes.is_empty() {
            sub.push(
                MenuItem::new(t("gitLog", "reason.noRemote"))
                    .disabled(true)
                    .into(),
            );
        }
        for remote in &remotes {
            let entity = entity.clone();
            let (tag, remote_name) = (tag.clone(), remote.clone());
            sub.push(
                action_item(
                    tr!("gitLog", "menu.pushTag", remote = remote),
                    busy.clone(),
                    move |window, cx| {
                        let (tag, remote) = (tag.clone(), remote_name.clone());
                        entity.update(cx, |this, cx| this.push_tag(tag, remote, window, cx));
                    },
                )
                .into(),
            );
        }
        entries.push(
            MenuItem::new(tr!("gitLog", "menu.tag", name = tag))
                .submenu(sub)
                .into(),
        );
    }
    entries
}

// ─── 执行器 ───────────────────────────────────────────────────

impl GitHistoryContent {
    /// 跑一个由菜单直接触发的改写操作:防重复 → 占闸、后台执行([`git_ui::spawn_gated`])
    /// → 成功回调(toast / 选中)或失败详情框 → 重载历史并通知容器。
    fn run_op<T: Send + 'static>(
        &mut self,
        action: &'static str,
        op: impl FnOnce(&Path) -> anyhow::Result<T> + Send + 'static,
        on_ok: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.repo_path.is_empty() {
            return;
        }
        if let Some(running) = self.busy {
            self.toast_info(
                tr!("gitLog", "toast.busy", action = t("gitLog", running)),
                cx,
            );
            return;
        }
        let this = cx.entity().downgrade();
        let started = git_ui::spawn_gated(
            &self.repo_path,
            op,
            move |result, window, cx| {
                let _ = this.update(cx, |this: &mut Self, cx| {
                    this.busy = None;
                    match result {
                        Ok(value) => on_ok(this, value, window, cx),
                        Err(err) => this.show_failure(action, &err, window, cx),
                    }
                    // 失败也可能动过仓库(拣选停在半路之类):一律重载
                    this.finish_mutation(cx);
                });
            },
            window,
            cx,
        );
        if !started {
            git_ui::toast(
                &self.store,
                git_ui::gate_busy_text().to_string(),
                git_ui::Tone::Attention,
                cx,
            );
            return;
        }
        self.busy = Some(action);
        cx.notify();
    }

    /// 同 [`Self::run_op`],但由对话框触发:成功关框,失败把原因写回表单、框留着。
    /// 调用方已把表单置成执行中([`FormOp::begin`]);没能开始(上一个还在跑、闸被占)
    /// 时同样把原因写回表单。
    #[allow(clippy::too_many_arguments)]
    fn run_dialog_op<T: Send + 'static>(
        &mut self,
        dialog: &'static str,
        form: Entity<DialogForm>,
        action: &'static str,
        op: impl FnOnce(&Path) -> anyhow::Result<T> + Send + 'static,
        on_ok: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let refuse = |message: String, cx: &mut Context<Self>| {
            form.update(cx, |f, cx| {
                f.op.fail(message);
                cx.notify();
            });
        };
        if let Some(running) = self.busy {
            refuse(
                tr!("gitLog", "toast.busy", action = t("gitLog", running)),
                cx,
            );
            return;
        }
        let this = cx.entity().downgrade();
        let form_done = form.clone();
        let started = git_ui::spawn_gated(
            &self.repo_path,
            op,
            move |result, window, cx| {
                let _ = this.update(cx, |this: &mut Self, cx| {
                    this.busy = None;
                    match result {
                        Ok(value) => {
                            form_done.update(cx, |f, _| f.op.finish());
                            close_guarded(dialog, window, cx);
                            on_ok(this, value, window, cx);
                        }
                        Err(err) => {
                            eprintln!("[git] {action} 失败: {err:#}");
                            let message = error_text(&err);
                            form_done.update(cx, |f, cx| {
                                f.op.fail(message.clone());
                                cx.notify();
                            });
                            // 等待期间框被 Esc 关了:原因不能丢,退到提示框
                            if !is_open(dialog) {
                                show_alert(t("gitLog", action), message, window, cx);
                            }
                        }
                    }
                    this.finish_mutation(cx);
                });
            },
            window,
            cx,
        );
        if !started {
            refuse(git_ui::gate_busy_text().to_string(), cx);
            return;
        }
        self.busy = Some(action);
        cx.notify();
    }

    /// 改写之后:历史重载(保留旧列表不闪),容器重载分支 / 更改。
    fn finish_mutation(&mut self, cx: &mut Context<Self>) {
        self.reload(cx);
        cx.emit(GitHistoryEvent::RepoChanged);
    }

    /// 改写后新的 HEAD 选中并拉详情(列表重载完会按 hash 认出来)。
    fn select_after(&mut self, hash: String, window: &mut Window, cx: &mut Context<Self>) {
        if !hash.is_empty() {
            self.select_hash(hash, window, cx);
        }
    }

    // ── 检出 ──────────────────────────────────────────────

    fn confirm_checkout(
        &mut self,
        commit: &GitCommitInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entity = cx.entity();
        let (hash, short) = (commit.hash.clone(), commit.short_hash.clone());
        Confirm::new(
            t("gitLog", "confirm.checkoutTitle"),
            tr!(
                "gitLog",
                "confirm.checkoutMessage",
                hash = short,
                title = commit.message
            ),
        )
        .ok_text(t("gitLog", "confirm.checkoutOk"))
        .cancel_text(t("gitLog", "confirm.cancel"))
        .open(
            move |window, cx| {
                let (hash, short) = (hash.clone(), short.clone());
                entity.update(cx, |this, cx| {
                    let op_hash = hash.clone();
                    this.run_op(
                        "action.checkout",
                        move |repo| git::checkout_commit(repo, &op_hash),
                        move |this, (), _window, cx| {
                            this.toast_info(tr!("gitLog", "toast.checkedOut", hash = short), cx);
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

    // ── 拣选 / 还原 ───────────────────────────────────────

    fn cherry_pick(
        &mut self,
        commit: &GitCommitInfo,
        rel: &CommitRelation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pick_like(commit, rel.parent_count > 1, false, window, cx);
    }

    fn revert(
        &mut self,
        commit: &GitCommitInfo,
        rel: &CommitRelation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pick_like(commit, rel.parent_count > 1, true, window, cx);
    }

    /// 拣选 / 还原:合并提交先弹确认(说明 `-m 1`),普通提交直接做(IDEA 同款)。
    fn pick_like(
        &mut self,
        commit: &GitCommitInfo,
        merge: bool,
        revert: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (hash, short) = (commit.hash.clone(), commit.short_hash.clone());
        let run = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            let (action, op_hash) = (
                if revert {
                    "action.revert"
                } else {
                    "action.cherryPick"
                },
                hash.clone(),
            );
            let short = short.clone();
            this.run_op(
                action,
                move |repo| {
                    if revert {
                        git::revert_commit(repo, &op_hash)
                    } else {
                        git::cherry_pick(repo, &op_hash)
                    }
                },
                move |this, outcome, window, cx| match outcome {
                    PickOutcome::Done { new_head } => {
                        let key = if revert {
                            "toast.reverted"
                        } else {
                            "toast.picked"
                        };
                        this.toast_info(tr!("gitLog", key, hash = short), cx);
                        this.select_after(new_head, window, cx);
                    }
                    PickOutcome::Conflicts => {
                        git_ui::toast(
                            &this.store,
                            tr!(
                                "gitLog",
                                "toast.conflicts",
                                action = t("gitLog", action),
                                hash = short
                            ),
                            git_ui::Tone::Attention,
                            cx,
                        );
                    }
                },
                window,
                cx,
            );
        };
        if !merge {
            run(self, window, cx);
            return;
        }
        let entity = cx.entity();
        let (title, message) = if revert {
            (
                t("gitLog", "confirm.revertMergeTitle"),
                tr!(
                    "gitLog",
                    "confirm.revertMergeMessage",
                    hash = commit.short_hash
                ),
            )
        } else {
            (
                t("gitLog", "confirm.pickMergeTitle"),
                tr!(
                    "gitLog",
                    "confirm.pickMergeMessage",
                    hash = commit.short_hash
                ),
            )
        };
        let run = Rc::new(run);
        Confirm::new(title, message)
            .ok_text(t("gitLog", "confirm.okContinue"))
            .cancel_text(t("gitLog", "confirm.cancel"))
            .open(
                move |window, cx| {
                    let run = run.clone();
                    entity.update(cx, |this, cx| run(this, window, cx));
                },
                window,
                cx,
            );
    }

    // ── 撤销提交 ──────────────────────────────────────────

    fn undo_commit(
        &mut self,
        commit: &GitCommitInfo,
        rel: &CommitRelation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (hash, short) = (commit.hash.clone(), commit.short_hash.clone());
        let message = match commit.body.as_deref().map(str::trim_end) {
            Some(body) if !body.trim().is_empty() => format!("{}\n\n{body}", commit.message),
            _ => commit.message.clone(),
        };
        let run = move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
            let (op_hash, short, message) = (hash.clone(), short.clone(), message.clone());
            this.run_op(
                "action.undo",
                move |repo| git::undo_last_commit(repo, &op_hash),
                move |this, parent, window, cx| {
                    this.toast_info(tr!("gitLog", "toast.undone", hash = short), cx);
                    this.select_after(parent, window, cx);
                    cx.emit(GitHistoryEvent::CommitUndone { message });
                },
                window,
                cx,
            );
        };
        if !rel.pushed {
            // 改动回到暂存区,随时能再提交 —— 不打断
            run(self, window, cx);
            return;
        }
        let entity = cx.entity();
        let run = Rc::new(run);
        danger_confirm(
            t("gitLog", "confirm.undoTitle").into(),
            tr!(
                "gitLog",
                "confirm.undoPushedMessage",
                hash = commit.short_hash,
                title = commit.message
            ),
            t("gitLog", "confirm.undoOk").into(),
            move |window, cx| {
                let run = run.clone();
                entity.update(cx, |this, cx| run(this, window, cx));
            },
            window,
            cx,
        );
    }

    // ── 标签:删除 / 推送 ─────────────────────────────────

    fn confirm_delete_tag(&mut self, tag: &str, window: &mut Window, cx: &mut Context<Self>) {
        let entity = cx.entity();
        let name = tag.to_string();
        danger_confirm(
            t("gitLog", "confirm.deleteTagTitle").into(),
            tr!("gitLog", "confirm.deleteTagMessage", name = tag),
            t("gitLog", "confirm.deleteTagOk").into(),
            move |window, cx| {
                let name = name.clone();
                entity.update(cx, |this, cx| {
                    let op_name = name.clone();
                    this.run_op(
                        "action.deleteTag",
                        move |repo| git::delete_tag(repo, &op_name),
                        move |this, (), _window, cx| {
                            this.toast_info(tr!("gitLog", "toast.tagDeleted", name = name), cx);
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

    fn push_tag(
        &mut self,
        tag: String,
        remote: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (op_tag, op_remote) = (tag.clone(), remote.clone());
        self.run_op(
            "action.pushTag",
            move |repo| git::push_tag(repo, &op_remote, &op_tag),
            move |this, (), _window, cx| {
                this.toast_info(
                    tr!("gitLog", "toast.tagPushed", name = tag, remote = remote),
                    cx,
                );
            },
            window,
            cx,
        );
    }

    // ── 新建分支 ──────────────────────────────────────────

    fn open_branch_dialog(
        &mut self,
        commit: &GitCommitInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if is_open(kind::GIT_LOG_NEW_BRANCH) {
            return;
        }
        let entity = cx.entity();
        let form = cx.new(|_| DialogForm::new());
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t("gitLog", "branch.placeholder")));
        let existing: Vec<String> = self
            .branches
            .iter()
            .filter(|b| !b.is_remote)
            .map(|b| b.name.clone())
            .collect();
        let base = tr!(
            "gitLog",
            "branch.base",
            hash = commit.short_hash,
            title = commit.message
        );
        let hash = commit.hash.clone();
        let focus_input = input.clone();

        open_guarded(
            kind::GIT_LOG_NEW_BRANCH,
            window,
            cx,
            move |dialog, _window, cx| {
                let name = input.read(cx).value().trim().to_string();
                let invalid = validate_branch(&name, &existing);
                let f = form.read(cx);
                let (busy, error, checkout) = (f.op.busy, f.op.error.clone(), f.checkout);
                let form_toggle = form.clone();
                // 名字一边打一边校验(空名不报,等点「创建」再说)
                let name_field = field_group(t("gitLog", "branch.nameLabel"))
                    .child(Input::new(&input))
                    .when_some(invalid.filter(|_| !name.is_empty()), |el, msg| {
                        el.child(git_ui::notice_line(msg, ui::color_error()))
                    });
                let checkout_row = git_ui::check_row(
                    "git-log-branch-checkout",
                    checkout,
                    t("gitLog", "branch.checkout"),
                    false,
                )
                .on_click(move |_: &ClickEvent, _window, cx| {
                    form_toggle.update(cx, |f, cx| {
                        f.checkout = !f.checkout;
                        cx.notify();
                    });
                });
                let (entity, form_ok, input_ok, hash, existing) = (
                    entity.clone(),
                    form.clone(),
                    input.clone(),
                    hash.clone(),
                    existing.clone(),
                );
                dialog
                    .title(t("gitLog", "branch.title"))
                    .w(px(git_ui::DIALOG_W_MD))
                    .overlay_closable(true)
                    .footer(form_footer(t("gitLog", "branch.ok"), false, busy))
                    .child(
                        git_ui::form_body()
                            .child(git_ui::hint_line(base.clone()))
                            .child(name_field)
                            .child(checkout_row)
                            .when_some(error, |el, msg| el.child(git_ui::error_block(msg))),
                    )
                    .on_ok(move |_: &ClickEvent, window, cx| {
                        if form_ok.read(cx).op.busy {
                            return false;
                        }
                        let name = input_ok.read(cx).value().trim().to_string();
                        if let Some(msg) = validate_branch(&name, &existing) {
                            form_ok.update(cx, |f, cx| {
                                f.op.fail(msg);
                                cx.notify();
                            });
                            return false;
                        }
                        let checkout = form_ok.read(cx).checkout;
                        if !begin(&form_ok, cx) {
                            return false;
                        }
                        let (op_name, op_hash) = (name.clone(), hash.clone());
                        let form = form_ok.clone();
                        entity.update(cx, |this, cx| {
                            this.run_dialog_op(
                                kind::GIT_LOG_NEW_BRANCH,
                                form,
                                "action.createBranch",
                                move |repo| {
                                    git::create_branch_at(repo, &op_name, &op_hash, checkout)
                                },
                                move |this, (), _window, cx| {
                                    let key = if checkout {
                                        "toast.branchCreatedCheckedOut"
                                    } else {
                                        "toast.branchCreated"
                                    };
                                    this.toast_info(tr!("gitLog", key, name = name), cx);
                                },
                                window,
                                cx,
                            );
                        });
                        false
                    })
            },
        );
        focus_later(focus_input.read(cx).focus_handle(cx), window, cx);
    }

    // ── 新建标签 ──────────────────────────────────────────

    fn open_tag_dialog(
        &mut self,
        commit: &GitCommitInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if is_open(kind::GIT_LOG_NEW_TAG) {
            return;
        }
        let entity = cx.entity();
        let form = cx.new(|_| DialogForm::new());
        let name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t("gitLog", "tag.placeholder")));
        let message_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(3)
                .placeholder(t("gitLog", "tag.messagePlaceholder"))
        });
        let existing: Vec<String> = self.tags.iter().map(|t| t.name.clone()).collect();
        let base = tr!(
            "gitLog",
            "branch.base",
            hash = commit.short_hash,
            title = commit.message
        );
        let hash = commit.hash.clone();
        let focus_input = name_input.clone();

        open_guarded(
            kind::GIT_LOG_NEW_TAG,
            window,
            cx,
            move |dialog, _window, cx| {
                let name = name_input.read(cx).value().trim().to_string();
                let invalid = validate_tag(&name, &existing);
                let f = form.read(cx);
                let (busy, error) = (f.op.busy, f.op.error.clone());
                let (entity, form_ok, name_ok, message_ok, hash, existing) = (
                    entity.clone(),
                    form.clone(),
                    name_input.clone(),
                    message_input.clone(),
                    hash.clone(),
                    existing.clone(),
                );
                dialog
                    .title(t("gitLog", "tag.title"))
                    .w(px(git_ui::DIALOG_W_MD))
                    .overlay_closable(true)
                    .footer(form_footer(t("gitLog", "branch.ok"), false, busy))
                    .child(
                        git_ui::form_body()
                            .child(git_ui::hint_line(base.clone()))
                            .child(
                                field_group(t("gitLog", "tag.nameLabel"))
                                    .child(Input::new(&name_input))
                                    .when_some(invalid.filter(|_| !name.is_empty()), |el, msg| {
                                        el.child(git_ui::notice_line(msg, ui::color_error()))
                                    }),
                            )
                            .child(
                                field_group(t("gitLog", "tag.messageLabel"))
                                    .child(Textarea::new(&message_input)),
                            )
                            .when_some(error, |el, msg| el.child(git_ui::error_block(msg))),
                    )
                    .on_ok(move |_: &ClickEvent, window, cx| {
                        if form_ok.read(cx).op.busy {
                            return false;
                        }
                        let name = name_ok.read(cx).value().trim().to_string();
                        if let Some(msg) = validate_tag(&name, &existing) {
                            form_ok.update(cx, |f, cx| {
                                f.op.fail(msg);
                                cx.notify();
                            });
                            return false;
                        }
                        let message = message_ok.read(cx).value().trim().to_string();
                        if !begin(&form_ok, cx) {
                            return false;
                        }
                        let (op_name, op_hash) = (name.clone(), hash.clone());
                        let form = form_ok.clone();
                        entity.update(cx, |this, cx| {
                            this.run_dialog_op(
                                kind::GIT_LOG_NEW_TAG,
                                form,
                                "action.createTag",
                                move |repo| {
                                    let message = Some(message.as_str()).filter(|m| !m.is_empty());
                                    git::create_tag(repo, &op_name, &op_hash, message)
                                },
                                move |this, (), _window, cx| {
                                    this.toast_info(
                                        tr!("gitLog", "toast.tagCreated", name = name),
                                        cx,
                                    );
                                },
                                window,
                                cx,
                            );
                        });
                        false
                    })
            },
        );
        focus_later(focus_input.read(cx).focus_handle(cx), window, cx);
    }

    // ── 修改提交消息 ──────────────────────────────────────

    fn open_reword_dialog(
        &mut self,
        commit: &GitCommitInfo,
        rel: &CommitRelation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if is_open(kind::GIT_LOG_REWORD) {
            return;
        }
        let entity = cx.entity();
        let form = cx.new(|_| DialogForm::new());
        // 预填完整消息(标题 + 正文)
        let full = match commit.body.as_deref().filter(|b| !b.trim().is_empty()) {
            Some(body) => format!("{}\n\n{}", commit.message, body.trim_end()),
            None => commit.message.clone(),
        };
        let input = cx.new(|cx| TextareaState::new(window, cx).rows(6).default_value(full));
        let pushed = rel.pushed;
        let hint = tr!("gitLog", "reword.hint", hash = commit.short_hash);
        let hash = commit.hash.clone();
        let focus_input = input.clone();

        open_guarded(
            kind::GIT_LOG_REWORD,
            window,
            cx,
            move |dialog, _window, cx| {
                let f = form.read(cx);
                let (busy, error) = (f.op.busy, f.op.error.clone());
                let empty = input.read(cx).value().trim().is_empty();
                let (entity, form_ok, input_ok, hash) =
                    (entity.clone(), form.clone(), input.clone(), hash.clone());
                dialog
                    .title(t("gitLog", "reword.title"))
                    .w(px(git_ui::DIALOG_W_LG))
                    .overlay_closable(true)
                    .footer(form_footer(t("gitLog", "reword.ok"), false, busy))
                    .child(
                        git_ui::form_body()
                            .key_context(REWORD_CONTEXT)
                            .on_action(|_: &GitLogRewordConfirm, window, cx| {
                                window.dispatch_action(
                                    Box::new(DialogConfirm { secondary: false }),
                                    cx,
                                );
                            })
                            .child(git_ui::hint_line(hint.clone()))
                            .when(pushed, |el| {
                                el.child(git_ui::notice_line(
                                    t("gitLog", "reword.pushedWarn"),
                                    ui::color_warning(),
                                ))
                            })
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(6.0))
                                    .child(Textarea::new(&input))
                                    .when(empty, |el| {
                                        el.child(git_ui::notice_line(
                                            t("gitLog", "reword.errEmpty"),
                                            ui::color_error(),
                                        ))
                                    }),
                            )
                            .when_some(error, |el, msg| el.child(git_ui::error_block(msg))),
                    )
                    .on_ok(move |_: &ClickEvent, window, cx| {
                        if form_ok.read(cx).op.busy {
                            return false;
                        }
                        let message = input_ok.read(cx).value().trim().to_string();
                        if message.is_empty() {
                            return false;
                        }
                        if !begin(&form_ok, cx) {
                            return false;
                        }
                        let op_hash = hash.clone();
                        let form = form_ok.clone();
                        entity.update(cx, |this, cx| {
                            this.run_dialog_op(
                                kind::GIT_LOG_REWORD,
                                form,
                                "action.reword",
                                move |repo| git::reword_head(repo, &op_hash, &message),
                                move |this, new_head: String, window, cx| {
                                    let short: String = new_head.chars().take(7).collect();
                                    this.toast_info(
                                        tr!("gitLog", "toast.reworded", hash = short),
                                        cx,
                                    );
                                    this.select_after(new_head, window, cx);
                                },
                                window,
                                cx,
                            );
                        });
                        false
                    })
            },
        );
        focus_later(focus_input.read(cx).focus_handle(cx), window, cx);
    }

    // ── 重置 ──────────────────────────────────────────────

    fn open_reset_dialog(
        &mut self,
        commit: &GitCommitInfo,
        rel: &CommitRelation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if is_open(kind::GIT_LOG_RESET) {
            return;
        }
        let entity = cx.entity();
        let form = cx.new(|_| DialogForm::new());
        let branch = rel
            .head_branch
            .clone()
            .unwrap_or_else(|| t("gitLog", "ref.head").to_string());
        let target = tr!(
            "gitLog",
            "reset.target",
            branch = branch,
            hash = commit.short_hash,
            title = commit.message
        );
        let off_history = !rel.in_head_history;
        let (hash, short) = (commit.hash.clone(), commit.short_hash.clone());
        // 会不会丢掉已推送的提交:要走图可达判定,放后台;回来之前不显示警告
        {
            let (repo, target, form) = (PathBuf::from(&self.repo_path), hash.clone(), form.clone());
            cx.spawn(async move |_, cx| {
                let drops = cx
                    .background_executor()
                    .spawn(async move { git::reset_drops_pushed(&repo, &target) })
                    .await;
                let drops = drops.unwrap_or_else(|err| {
                    eprintln!("[git] 重置前的推送判定失败(不显示警告): {err:#}");
                    false
                });
                form.update(cx, |f, cx| {
                    f.drops_pushed = drops;
                    cx.notify();
                });
            })
            .detach();
        }

        open_guarded(
            kind::GIT_LOG_RESET,
            window,
            cx,
            move |dialog, _window, cx| {
                let f = form.read(cx);
                let (busy, error, mode) = (f.op.busy, f.op.error.clone(), f.mode);
                let drops_pushed = f.drops_pushed;
                let mut modes = div().flex().flex_col().gap(px(4.0));
                for (m, key) in [
                    (ResetMode::Soft, "reset.soft"),
                    (ResetMode::Mixed, "reset.mixed"),
                    (ResetMode::Keep, "reset.keep"),
                    (ResetMode::Hard, "reset.hard"),
                ] {
                    let form = form.clone();
                    modes = modes.child(mode_row(m, key, m == mode, move |_window, cx| {
                        form.update(cx, |f, cx| {
                            f.mode = m;
                            f.op.error = None;
                            cx.notify();
                        });
                    }));
                }
                let (entity, form_ok, hash, short, branch) = (
                    entity.clone(),
                    form.clone(),
                    hash.clone(),
                    short.clone(),
                    branch.clone(),
                );
                dialog
                    .title(t("gitLog", "reset.title"))
                    .w(px(git_ui::DIALOG_W_LG))
                    .overlay_closable(true)
                    .footer(form_footer(
                        t("gitLog", "reset.ok"),
                        mode == ResetMode::Hard,
                        busy,
                    ))
                    .child(
                        git_ui::form_body()
                            .child(
                                div()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(ui::text_primary())
                                    .child(target.clone()),
                            )
                            // 会改写已推送的历史:四种模式都要看见(Hard 另有二次确认)
                            .when(drops_pushed, |el| {
                                el.child(pushed_warning(t("gitLog", "reset.pushedWarn")))
                            })
                            .when(off_history, |el| {
                                el.child(git_ui::notice_line(
                                    t("gitLog", "reset.notInHistory"),
                                    ui::color_warning(),
                                ))
                            })
                            .child(modes)
                            .when_some(error, |el, msg| el.child(git_ui::error_block(msg))),
                    )
                    .on_ok(move |_: &ClickEvent, window, cx| {
                        if form_ok.read(cx).op.busy {
                            return false;
                        }
                        let mode = form_ok.read(cx).mode;
                        let hard_message = tr!("gitLog", "reset.hardMessage", hash = short);
                        let (entity, form, hash, short, branch) = (
                            entity.clone(),
                            form_ok.clone(),
                            hash.clone(),
                            short.clone(),
                            branch.clone(),
                        );
                        let start = move |window: &mut Window, cx: &mut App| {
                            if !begin(&form, cx) {
                                return;
                            }
                            let (op_hash, hash, short, branch) =
                                (hash.clone(), hash.clone(), short.clone(), branch.clone());
                            let form = form.clone();
                            entity.update(cx, |this, cx| {
                                this.run_dialog_op(
                                    kind::GIT_LOG_RESET,
                                    form,
                                    "action.reset",
                                    move |repo| git::reset_to(repo, &op_hash, mode),
                                    move |this, (), window, cx| {
                                        this.toast_info(
                                            tr!(
                                                "gitLog",
                                                "toast.reset",
                                                branch = branch,
                                                hash = short,
                                                mode = mode.name()
                                            ),
                                            cx,
                                        );
                                        this.select_after(hash, window, cx);
                                    },
                                    window,
                                    cx,
                                );
                            });
                        };
                        if mode == ResetMode::Hard {
                            // 危险:再确认一次;重置框留着,确认后才真正执行
                            danger_confirm(
                                t("gitLog", "reset.hardTitle").into(),
                                hard_message,
                                t("gitLog", "reset.ok").into(),
                                start,
                                window,
                                cx,
                            );
                        } else {
                            start(window, cx);
                        }
                        false
                    })
            },
        );
    }
}

// ─── 校验 ─────────────────────────────────────────────────────

/// 分支名的即时校验。`None` = 可以提交。
fn validate_branch(name: &str, existing: &[String]) -> Option<String> {
    if name.is_empty() {
        return Some(t("gitLog", "branch.errEmpty").to_string());
    }
    if !git::is_valid_branch_name(name) {
        return Some(t("gitLog", "branch.errInvalid").to_string());
    }
    if existing.iter().any(|b| b == name) {
        return Some(t("gitLog", "branch.errExists").to_string());
    }
    None
}

/// 标签名的即时校验。`None` = 可以提交。
fn validate_tag(name: &str, existing: &[String]) -> Option<String> {
    if name.is_empty() {
        return Some(t("gitLog", "tag.errEmpty").to_string());
    }
    if !git::is_valid_tag_name(name) {
        return Some(t("gitLog", "tag.errInvalid").to_string());
    }
    if existing.iter().any(|b| b == name) {
        return Some(t("gitLog", "tag.errExists").to_string());
    }
    None
}

// ─── 对话框小件 ───────────────────────────────────────────────

// 正文骨架、说明行、警告 / 错误行、复选框、页脚都走 `git_ui` 的共用件(与分支弹窗、
// 推送对话框同一套);这里只剩本模块特有的几件。

/// 确认之后、交给执行器之前把表单置成执行中([`FormOp::begin`])。已在执行中返回 `false`。
fn begin(form: &Entity<DialogForm>, cx: &mut App) -> bool {
    form.update(cx, |f, cx| {
        let started = f.op.begin();
        cx.notify();
        started
    })
}

/// 「将改写已推送的历史」:警告色淡底的醒目块(比普通说明行更扎眼,四种模式都显示)。
fn pushed_warning(text: &'static str) -> gpui::Div {
    let color = ui::color_warning();
    div()
        .px(px(10.0))
        .py(px(8.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(ui::with_alpha(color, 0.45))
        .bg(ui::with_alpha(color, 0.10))
        .child(git_ui::notice_line(text, color).font_weight(gpui::FontWeight::SEMIBOLD))
}

/// 「取消 / 确认」页脚(共用的 [`git_ui::dialog_footer`]):确认键在等待期间换成
/// 「执行中…」并禁用(防重复提交);`danger` 时确认键用危险色(Hard 重置)。
fn form_footer(ok: &'static str, danger: bool, busy: bool) -> DialogFooter {
    let label = if busy { t("gitLog", "working") } else { ok };
    git_ui::dialog_footer(
        label,
        Some(t("gitLog", "confirm.cancel").into()),
        danger,
        busy,
    )
}

/// 一个表单字段:标签 + 控件 + 就地校验,三者靠得近(6px),字段之间才是正文的 12px。
fn field_group(label: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(git_ui::field_label(label))
}

/// 重置模式的一行:单选圈 + 说明。Hard 的说明用危险色。
fn mode_row(
    mode: ResetMode,
    key: &'static str,
    selected: bool,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let danger = mode == ResetMode::Hard;
    let ring = if selected {
        ui::accent()
    } else {
        ui::border_strong()
    };
    div()
        .id(SharedString::from(format!("git-log-reset-{}", mode.name())))
        .flex()
        .items_center()
        .gap(px(10.0))
        .px(px(10.0))
        .py(px(8.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .border_1()
        .border_color(if selected {
            ui::with_alpha(ui::accent(), 0.5)
        } else {
            ui::border_default()
        })
        .when(selected, |el| el.bg(ui::accent_subtle()))
        .when(!selected, |el| el.hover(|el| el.bg(git_ui::hover_bg())))
        .child(
            div()
                .flex_none()
                .w(px(14.0))
                .h(px(14.0))
                .rounded_full()
                .border_1()
                .border_color(ring)
                .flex()
                .items_center()
                .justify_center()
                .when(selected, |el| {
                    el.child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(ui::accent()))
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(ui::font_px(12.0))
                .text_color(if danger {
                    ui::color_error()
                } else {
                    ui::text_primary()
                })
                .child(t("gitLog", key)),
        )
        .on_click(move |_: &ClickEvent, window, cx| on_click(window, cx))
}

/// 危险操作的二次确认(删标签 / 撤销已推送的提交 / Hard 重置):确认键是危险色。
fn danger_confirm(
    title: SharedString,
    message: String,
    ok: SharedString,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let on_ok = Rc::new(on_ok);
    open_guarded(
        kind::GIT_LOG_DANGER,
        window,
        cx,
        move |dialog, _window, _cx| {
            let on_ok = on_ok.clone();
            // 一行一个块(原文里的空行保留成空行),行距紧凑,别吃正文的 12px 间距
            let mut lines = div().flex().flex_col().gap(px(2.0));
            for line in message.split('\n') {
                lines = lines.child(
                    div()
                        .text_size(ui::font_px(13.0))
                        .text_color(ui::text_primary())
                        .child(if line.is_empty() {
                            SharedString::from(" ")
                        } else {
                            SharedString::from(line.to_string())
                        }),
                );
            }
            dialog
                .title(title.clone())
                .w(px(git_ui::DIALOG_W_SM))
                .overlay_closable(true)
                .footer(git_ui::dialog_footer(
                    ok.clone(),
                    Some(t("gitLog", "confirm.cancel").into()),
                    true,
                    false,
                ))
                .child(git_ui::form_body().child(lines))
                .on_ok(move |_: &ClickEvent, window, cx| {
                    on_ok(window, cx);
                    true
                })
        },
    );
}

/// 弹窗一开就把焦点交给首个输入框。**必须排在 `open_guarded` 之后**:弹窗打开会把
/// 焦点抢到自己的面板上,`defer` 到本轮 effect 之后再聚焦(同 `prompt::autofocus`)。
fn focus_later(handle: gpui::FocusHandle, window: &mut Window, cx: &mut App) {
    window.defer(cx, move |window, cx| window.focus(&handle, cx));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 分支名与标签名即时校验() {
        let existing = vec!["main".to_string(), "v1".to_string()];
        assert_eq!(validate_branch("", &existing), reason("branch.errEmpty"));
        assert_eq!(
            validate_branch("a b", &existing),
            reason("branch.errInvalid")
        );
        assert_eq!(
            validate_branch("-x", &existing),
            reason("branch.errInvalid")
        );
        assert_eq!(
            validate_branch("main", &existing),
            reason("branch.errExists")
        );
        assert_eq!(validate_branch("feature/x", &existing), None);

        assert_eq!(validate_tag("", &existing), reason("tag.errEmpty"));
        assert_eq!(validate_tag("v1..2", &existing), reason("tag.errInvalid"));
        assert_eq!(validate_tag("v1", &existing), reason("tag.errExists"));
        assert_eq!(validate_tag("v2.0.0", &existing), None);
    }
}
