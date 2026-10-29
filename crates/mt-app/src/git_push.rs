//! 「推送」对话框:仓库栏推送按钮 / 分支菜单「推送」(当前分支)打开。对标 IDEA 的
//! Push 对话框。
//!
//! ```text
//! ┌ 推送提交                                  ✕ ┐
//! │ [main] → [origin/main]   (将设置上游)       │   两枚胶囊 + 箭头
//! │ ┌──────────────────────────────────────┐ │
//! │ │ 1a2b3c4  修复登录页      张三 · 3 分钟前 │ │   点击看该提交的改动
//! │ └──────────────────────────────────────┘ │
//! │ ☐ 强制推送(--force-with-lease)             │   勾上:底部按钮区整体换危险色 + 警告
//! │ (失败原文,可选中复制 / 被拒时提示先拉取)     │
//! │                         [取消]  [推送]     │
//! └─────────────────────────────────────────┘
//! ```
//!
//! 待推送 = 分支有、上游没有的提交;没上游时取「不在任何远程分支上」的(最多 100 条)。
//! 没有待推送提交也能打开(没上游时推送仍有意义:在远程建分支并设上游)。
//! detached HEAD / 没有远程时推送按钮禁用并说明原因。强推要再确认一次。

use std::path::Path;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::input::TextareaState;
use mt_project::git::{OutgoingCommits, PushOptions, PushOutcome, PushPlan, PushTarget};

use crate::git_panel::GitPanel;
use crate::git_ui::{self, Tone};
use crate::i18n::{t, tr};
use crate::prompt::{self, Confirm, close_guarded, dialog_title, kind, open_guarded};
use crate::ui;

/// 最多列出多少条待推送提交。
const OUTGOING_LIMIT: usize = 100;

struct PushDialog {
    panel: Entity<GitPanel>,
    repo: String,
    /// `None` = 还在加载。
    plan: Option<PushPlan>,
    outgoing: Option<OutgoingCommits>,
    load_error: Option<String>,
    force: bool,
    pushing: bool,
    /// 上一次推送失败的原文(可选中复制)+ 是不是「被拒、应先拉取」那种。
    error: Option<(Entity<TextareaState>, bool)>,
}

impl Render for PushDialog {
    /// 状态盒子。真正的画面由 Dialog 的 builder 每帧重建。
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 推送成功的摘要:「已推送 3 个提交到 origin/main」,没上游时补「并设为上游」。
pub(crate) fn pushed_summary(
    target: &PushTarget,
    count: usize,
    truncated: bool,
    forced: bool,
) -> String {
    let remote = format!("{}/{}", target.remote, target.remote_branch);
    let mut message = if forced {
        tr!("gitBranch", "forcePushed", target = remote.as_str())
    } else if count == 0 {
        tr!("gitBranch", "pushedNothing", target = remote.as_str())
    } else {
        let count = if truncated {
            format!("{count}+")
        } else {
            count.to_string()
        };
        tr!(
            "gitBranch",
            "pushed",
            count = count,
            target = remote.as_str()
        )
    };
    if !target.has_upstream {
        message.push_str(t("gitBranch", "upstreamSetSuffix"));
    }
    message
}

/// 打开推送对话框(当前分支)。已经开着时是空操作。
///
/// `repo` 由调用方给:调用点在 Git 面板自己的 update 里,这时再 `panel.read` 会撞上
/// 「实体正被更新」的借用。
pub(crate) fn open(panel: Entity<GitPanel>, repo: String, window: &mut Window, cx: &mut App) {
    if prompt::is_open(kind::GIT_PUSH) || repo.is_empty() {
        return;
    }
    let state = cx.new(|_| PushDialog {
        panel,
        repo,
        plan: None,
        outgoing: None,
        load_error: None,
        force: false,
        pushing: false,
        error: None,
    });
    load(&state, cx);
    open_guarded(kind::GIT_PUSH, window, cx, move |dialog, _window, cx| {
        let body = render_body(&state, cx);
        let pushing = state.read(cx).pushing;
        let state_for_ok = state.clone();
        dialog
            .title(dialog_title(kind::GIT_PUSH, t("gitBranch", "pushTitle")))
            .w(px(560.0))
            // 推送进行中别让一下误点遮罩把框收了(结果回来要在框里显示)
            .overlay_closable(!pushing)
            .child(body)
            // Enter = 推送(条件不满足时 `start_push` 自己什么都不做)
            .on_ok(move |_: &ClickEvent, window, cx| {
                start_push(&state_for_ok, window, cx);
                false
            })
    });
}

/// 后台读推送计划与待推送提交。
fn load(state: &Entity<PushDialog>, cx: &mut App) {
    let repo = state.read(cx).repo.clone();
    let state = state.clone();
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                let path = Path::new(&repo);
                let plan = mt_project::git::push_plan(path, None)?;
                let outgoing = mt_project::git::outgoing_commits(path, None, OUTGOING_LIMIT)?;
                anyhow::Ok((plan, outgoing))
            })
            .await;
        state.update(cx, |s: &mut PushDialog, cx| {
            match result {
                Ok((plan, outgoing)) => {
                    s.plan = Some(plan);
                    s.outgoing = Some(outgoing);
                }
                Err(err) => s.load_error = Some(format!("{err:#}")),
            }
            cx.notify();
        });
    })
    .detach();
}

/// 「推送」:强推先再确认一次,其余直接推。
fn start_push(state: &Entity<PushDialog>, window: &mut Window, cx: &mut App) {
    let s = state.read(cx);
    if s.pushing {
        return;
    }
    let Some(PushPlan::Ready(target)) = s.plan.clone() else {
        return;
    };
    if !s.force {
        do_push(state, window, cx);
        return;
    }
    let state = state.clone();
    Confirm::new(
        t("gitBranch", "forceConfirmTitle"),
        tr!(
            "gitBranch",
            "forceConfirmMessage",
            branch = target.branch.as_str(),
            target = format!("{}/{}", target.remote, target.remote_branch)
        ),
    )
    .ok_text(t("gitBranch", "forcePushOk"))
    .open(move |window, cx| do_push(&state, window, cx), window, cx);
}

fn do_push(state: &Entity<PushDialog>, window: &mut Window, cx: &mut App) {
    let (panel, repo, force, count, truncated) = {
        let s = state.read(cx);
        let (count, truncated) = s
            .outgoing
            .as_ref()
            .map_or((0, false), |o| (o.commits.len(), o.truncated));
        (s.panel.clone(), s.repo.clone(), s.force, count, truncated)
    };
    state.update(cx, |s, cx| {
        s.pushing = true;
        s.error = None;
        cx.notify();
    });
    panel.update(cx, |p, cx| p.set_push_busy(true, cx));
    let state = state.clone();
    window
        .spawn(cx, async move |cx| {
            let options = PushOptions {
                branch: None,
                force_with_lease: force,
            };
            let result = cx
                .background_executor()
                // ⚠️ 网络操作,最多阻塞 120s
                .spawn(async move { mt_project::git::git_push_with(Path::new(&repo), &options) })
                .await;
            let _ = cx.update(|window, cx| {
                panel.update(cx, |p, cx| p.set_push_busy(false, cx));
                let failure = match result {
                    Ok(PushOutcome::Pushed(target)) => {
                        close_guarded(kind::GIT_PUSH, window, cx);
                        let message = pushed_summary(&target, count, truncated, force);
                        panel.update(cx, |p, cx| {
                            p.notify_result(message, Tone::Info, cx);
                            p.refresh_all(cx);
                        });
                        None
                    }
                    Ok(PushOutcome::Rejected { detail }) => Some((detail, true)),
                    Err(err) => Some((format!("{err:#}"), false)),
                };
                let Some((detail, rejected)) = failure else {
                    return;
                };
                eprintln!("[git] 推送失败: {detail}");
                // 等结果期间对话框被 Esc 关了:原因不能丢,退到详情框
                if !prompt::is_open(kind::GIT_PUSH) {
                    let text = if rejected {
                        format!("{}\n\n{detail}", t("gitBranch", "rejectedHint"))
                    } else {
                        detail
                    };
                    git_ui::show_git_error(
                        tr!("gitBranch", "opFailed", op = t("gitBranch", "opPush")),
                        text,
                        window,
                        cx,
                    );
                    return;
                }
                let text_state = git_ui::error_text_state(&detail, 8, window, cx);
                state.update(cx, |s, cx| {
                    s.pushing = false;
                    s.error = Some((text_state, rejected));
                    cx.notify();
                });
                panel.update(cx, |p, cx| p.refresh_all(cx));
            });
        })
        .detach();
}

/// 点一条待推送提交:取文件列表,开提交 diff 弹窗(叠在推送对话框之上)。
fn view_commit(
    state: &Entity<PushDialog>,
    hash: String,
    message: String,
    window: &mut Window,
    cx: &mut App,
) {
    let (repo, store) = {
        let s = state.read(cx);
        (s.repo.clone(), s.panel.read(cx).store().clone())
    };
    let (repo_for_task, hash_for_task) = (repo.clone(), hash.clone());
    window
        .spawn(cx, async move |cx| {
            let files = cx
                .background_executor()
                .spawn(async move {
                    mt_project::git::get_commit_files(Path::new(&repo_for_task), &hash_for_task)
                })
                .await;
            let _ = cx.update(|window, cx| match files {
                Ok(files) => {
                    crate::git_diff::open_commit_diff(store, repo, hash, message, files, window, cx)
                }
                Err(err) => git_ui::show_git_error(
                    t("gitBranch", "viewCommitFailed"),
                    format!("{err:#}"),
                    window,
                    cx,
                ),
            });
        })
        .detach();
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn hint_line(text: impl Into<SharedString>, color: gpui::Hsla) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(ui::font_px(12.0))
        .text_color(color)
        .child(git_ui::icon(git_ui::ICON_ALERT, 12.0, color))
        .child(text.into())
        .into_any_element()
}

fn render_body(state: &Entity<PushDialog>, cx: &mut App) -> AnyElement {
    let s = state.read(cx);
    let mono = git_ui::mono_font(s.panel.read(cx).store(), cx);
    let force = s.force;
    let pushing = s.pushing;
    let mut root = div()
        .px(px(16.0))
        .pb(px(16.0))
        .flex()
        .flex_col()
        .gap(px(12.0));

    if let Some(err) = &s.load_error {
        return root
            .child(hint_line(err.clone(), ui::color_error()))
            .into_any_element();
    }
    let (Some(plan), Some(outgoing)) = (s.plan.clone(), s.outgoing.clone()) else {
        return root
            .child(
                div()
                    .py(px(24.0))
                    .flex()
                    .justify_center()
                    .child(git_ui::busy_line(t("gitBranch", "loading"))),
            )
            .into_any_element();
    };

    // ── 目标行 ──
    let target = match &plan {
        PushPlan::Ready(target) => Some(target.clone()),
        _ => None,
    };
    root = root.child(match &plan {
        PushPlan::Ready(target) => div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(git_ui::icon(
                git_ui::ICON_BRANCH,
                git_ui::ICON_SIZE,
                ui::text_muted(),
            ))
            .child(git_ui::pill(target.branch.clone(), ui::accent()))
            .child(git_ui::icon(
                git_ui::ICON_ARROW_RIGHT,
                git_ui::ICON_SIZE,
                ui::text_muted(),
            ))
            .child(git_ui::pill(
                format!("{}/{}", target.remote, target.remote_branch),
                ui::color_info(),
            ))
            .when(!target.has_upstream, |el| {
                el.child(
                    div()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_muted())
                        .child(t("gitBranch", "willSetUpstream")),
                )
            })
            .into_any_element(),
        PushPlan::Detached => hint_line(t("gitBranch", "pushDetached"), ui::color_warning()),
        PushPlan::NoRemote { .. } => hint_line(t("gitBranch", "pushNoRemote"), ui::color_warning()),
    });

    // ── 待推送提交 ──
    let now = now_secs();
    let mut list = div()
        .id("git-push-commits")
        .max_h(px(300.0))
        .overflow_y_scroll()
        .py(px(4.0));
    if outgoing.commits.is_empty() {
        list = list.child(git_ui::empty_state(
            git_ui::ICON_CHECK,
            if target.as_ref().is_some_and(|t| !t.has_upstream) {
                t("gitBranch", "noOutgoingNewUpstream")
            } else {
                t("gitBranch", "noOutgoing")
            },
            None,
        ));
    }
    for (index, commit) in outgoing.commits.iter().enumerate() {
        let (state_for_click, hash, message) =
            (state.clone(), commit.hash.clone(), commit.message.clone());
        list = list.child(
            div()
                .id(SharedString::from(format!("git-push-commit-{index}")))
                .flex()
                .items_center()
                .gap(px(10.0))
                .h(px(28.0))
                .mx(px(4.0))
                .px(px(8.0))
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|el| el.bg(ui::bg_elevated()))
                .child(
                    div()
                        .flex_none()
                        .font_family(mono.clone())
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_muted())
                        .child(commit.short_hash.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(ui::font_px(13.0))
                        .text_color(ui::text_primary())
                        .child(commit.message.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(format!(
                            "{} · {}",
                            commit.author,
                            crate::git_history::format_relative_time(commit.timestamp, now)
                        )),
                )
                .on_click(move |_: &ClickEvent, window, cx| {
                    view_commit(&state_for_click, hash.clone(), message.clone(), window, cx);
                }),
        );
    }
    let count_line = if outgoing.commits.is_empty() {
        None
    } else if outgoing.truncated {
        Some(tr!(
            "gitBranch",
            "outgoingTruncated",
            count = outgoing.commits.len()
        ))
    } else {
        Some(tr!(
            "gitBranch",
            "outgoingCount",
            count = outgoing.commits.len()
        ))
    };
    root = root.child(
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .when_some(count_line, |el, line| {
                el.child(
                    div()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_muted())
                        .child(line),
                )
            })
            .child(
                div()
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(ui::border_subtle())
                    .bg(ui::bg_base())
                    .child(list),
            ),
    );

    // ── 失败原文 ──
    if let Some((text, rejected)) = &s.error {
        root = root.child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .when(*rejected, |el| {
                    el.child(hint_line(
                        t("gitBranch", "rejectedHint"),
                        ui::color_warning(),
                    ))
                })
                .child(git_ui::error_text_box(text)),
        );
    }

    // ── 底部:强推选项 + 按钮。勾上强推时整块换危险色并给警告 ──
    let ready = target.is_some();
    let can_push = ready && !pushing;
    let danger = ui::color_error();
    let state_for_force = state.clone();
    let state_for_push = state.clone();
    let force_row = div()
        .id("git-push-force")
        .flex()
        .items_center()
        .gap(px(8.0))
        .cursor_pointer()
        .text_size(ui::font_px(12.0))
        .text_color(if force { danger } else { ui::text_secondary() })
        .child(git_ui::check_box(force, danger))
        .child(t("gitBranch", "forcePush"))
        .on_click(move |_: &ClickEvent, _window, cx| {
            state_for_force.update(cx, |s, cx| {
                if !s.pushing {
                    s.force = !s.force;
                    cx.notify();
                }
            });
        });
    let push_label = if pushing {
        t("gitBranch", "pushing")
    } else if force {
        t("gitBranch", "forcePushOk")
    } else {
        t("gitBranch", "push")
    };
    let push_button = ui::primary_button("git-push-ok", push_label)
        .when(force, |el| el.bg(danger))
        .when(!can_push, |el| el.opacity(0.5).cursor_default())
        .when(can_push, |el| {
            el.on_click(move |_: &ClickEvent, window, cx| start_push(&state_for_push, window, cx))
        });
    let cancel_button = ui::ghost_button("git-push-cancel", t("gitBranch", "cancel")).on_click(
        |_: &ClickEvent, window, cx| {
            close_guarded(kind::GIT_PUSH, window, cx);
        },
    );
    let warning = target.as_ref().filter(|_| force).map(|target| {
        hint_line(
            tr!(
                "gitBranch",
                "forceWarning",
                target = format!("{}/{}", target.remote, target.remote_branch)
            ),
            danger,
        )
    });

    root.child(
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .px(px(12.0))
            .py(px(10.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(if force {
                ui::with_alpha(danger, 0.45)
            } else {
                ui::border_subtle()
            })
            .when(force, |el| el.bg(ui::with_alpha(danger, 0.08)))
            .child(force_row)
            .children(warning)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .when(pushing, |el| {
                        el.child(
                            div()
                                .mr_auto()
                                .child(git_ui::busy_line(t("gitBranch", "pushing"))),
                        )
                    })
                    .child(cancel_button)
                    .child(push_button),
            ),
    )
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(has_upstream: bool) -> PushTarget {
        PushTarget {
            branch: "main".into(),
            remote: "origin".into(),
            remote_branch: "main".into(),
            has_upstream,
        }
    }

    /// 推送摘要:带提交数与目标;截断时写 `100+`;没上游时补「设为上游」;强推单独一句。
    #[test]
    fn 推送摘要() {
        let msg = pushed_summary(&target(true), 3, false, false);
        assert!(msg.contains('3') && msg.contains("origin/main"), "{msg}");
        let msg = pushed_summary(&target(true), 100, true, false);
        assert!(msg.contains("100+"), "{msg}");
        let plain = pushed_summary(&target(true), 0, false, false);
        let with_upstream = pushed_summary(&target(false), 0, false, false);
        assert!(with_upstream.starts_with(&plain), "{with_upstream}");
        assert!(with_upstream.len() > plain.len());
        let forced = pushed_summary(&target(true), 2, false, true);
        assert_ne!(forced, pushed_summary(&target(true), 2, false, false));
    }
}
