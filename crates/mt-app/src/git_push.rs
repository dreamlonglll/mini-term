//! 「推送」对话框:仓库栏推送按钮 / 分支菜单「推送」(当前分支)打开。对标 IDEA 的
//! Push 对话框。
//!
//! ```text
//! ┌ 推送提交                                  ✕ ┐
//! │ [main] → [origin/main]   (将设置上游)       │   两枚胶囊 + 箭头
//! │ ┌──────────────────────────────────────┐ │
//! │ │ 1a2b3c4  修复登录页      张三 · 3 分钟前 │ │   点击看该提交的改动
//! │ └──────────────────────────────────────┘ │
//! │ ☐ 强制推送(--force-with-lease)             │   勾上:选项区转红 + 警告,确认键换危险色
//! │ (失败原文,可选中复制 / 被拒时提示先拉取)     │
//! │                         [取消]  [推送]     │   共用页脚(`git_ui::dialog_footer`)
//! └─────────────────────────────────────────┘
//! ```
//!
//! 待推送 = 分支有、**推送目标**的远程分支没有的提交(目标可以不是上游:从 origin/main
//! 拉出来的 feature 推到 origin/feature,这时目标行下面多一句说明上游不动);目标在本地
//! 还没有远程跟踪引用时取「不在任何远程分支上」的(最多 100 条)。
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
use mt_ui::tooltip::TooltipExt as _;

use crate::git_panel::GitPanel;
use crate::git_ui::{self, Tone};
use crate::i18n::{t, tr};
use crate::prompt::{self, Confirm, close_guarded, kind, open_guarded};
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
    let remote = target.label();
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
    if target.sets_upstream() {
        message.push_str(t("gitBranch", "upstreamSetSuffix"));
    }
    message
}

/// 目标与上游不同时目标行下面那句说明:「将推送到 origin/feature;当前上游是
/// origin/main,不会修改」。其余情况(没上游 / 推的就是上游)为 `None`。
pub(crate) fn upstream_note(target: &PushTarget) -> Option<String> {
    let upstream = target
        .upstream
        .as_deref()
        .filter(|_| target.upstream_differs)?;
    Some(tr!(
        "gitBranch",
        "pushOtherThanUpstream",
        target = target.label(),
        upstream = upstream
    ))
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
        let s = state.read(cx);
        let pushing = s.pushing;
        let force = s.force;
        let can_push = !pushing && matches!(s.plan, Some(PushPlan::Ready(_)));
        let push_label = if pushing {
            t("gitBranch", "pushing")
        } else if force {
            t("gitBranch", "forcePushOk")
        } else {
            t("gitBranch", "push")
        };
        let state_for_ok = state.clone();
        dialog
            .title(t("gitBranch", "pushTitle"))
            .w(px(git_ui::DIALOG_W_LG))
            // 推送进行中别让一下误点遮罩把框收了(结果回来要在框里显示)
            .overlay_closable(!pushing)
            // 勾上强推:确认键整颗换危险色(正文里那块选项区同时转红并给警告)
            .footer(git_ui::dialog_footer(
                push_label,
                Some(t("gitBranch", "cancel").into()),
                force,
                !can_push,
            ))
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
            target = target.label()
        ),
    )
    .ok_text(t("gitBranch", "forcePushOk"))
    .open(move |window, cx| do_push(&state, window, cx), window, cx);
}

fn do_push(state: &Entity<PushDialog>, window: &mut Window, cx: &mut App) {
    let (panel, repo, force, count, truncated, expected_oid) = {
        let s = state.read(cx);
        let (count, truncated) = s
            .outgoing
            .as_ref()
            .map_or((0, false), |o| (o.commits.len(), o.truncated));
        // 强推的 lease 期望值取**预览这一刻**的远程跟踪值:对话框开着期间点过 Fetch 的话,
        // 别人刚推上去、本地还没整合的提交不能被当成「见过的」覆盖掉
        let expected_oid = match &s.plan {
            Some(PushPlan::Ready(target)) => target.tracking_oid.clone(),
            _ => None,
        };
        (
            s.panel.clone(),
            s.repo.clone(),
            s.force,
            count,
            truncated,
            expected_oid,
        )
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
                expected_oid,
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

fn render_body(state: &Entity<PushDialog>, cx: &mut App) -> AnyElement {
    let s = state.read(cx);
    let force = s.force;
    let pushing = s.pushing;
    let mut root = git_ui::form_body();

    if let Some(err) = &s.load_error {
        return root
            .child(git_ui::notice_line(err.clone(), ui::color_error()))
            .into_any_element();
    }
    let (Some(plan), Some(outgoing)) = (s.plan.clone(), s.outgoing.clone()) else {
        return root
            .child(git_ui::loading_state(t("gitBranch", "loading")))
            .into_any_element();
    };

    // ── 目标行 ──
    let target = match &plan {
        PushPlan::Ready(target) => Some(target.clone()),
        _ => None,
    };
    root = root.child(match &plan {
        PushPlan::Ready(target) => {
            let remote = target.label();
            let row = div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .min_w_0()
                .child(git_ui::icon(
                    git_ui::ICON_BRANCH,
                    git_ui::ICON_SIZE,
                    ui::text_muted(),
                ))
                .child(
                    git_ui::ref_pill(
                        "git-push-branch",
                        target.branch.clone(),
                        git_ui::RefKind::Head,
                    )
                    .tip(target.branch.clone()),
                )
                .child(git_ui::icon(
                    git_ui::ICON_ARROW_RIGHT,
                    git_ui::ICON_SIZE,
                    ui::text_muted(),
                ))
                .child(
                    git_ui::ref_pill("git-push-remote", remote.clone(), git_ui::RefKind::Remote)
                        .tip(remote),
                )
                .when(target.sets_upstream(), |el| {
                    el.child(
                        div()
                            .flex_none()
                            .text_size(ui::font_px(12.0))
                            .text_color(ui::text_muted())
                            .child(t("gitBranch", "willSetUpstream")),
                    )
                });
            // 目标不是上游(从 origin/main 拉出来的 feature 推到 origin/feature):说清楚
            // 推到哪、上游保持原样 —— 不然用户会以为在往 main 上推
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .min_w_0()
                .child(row)
                .when_some(upstream_note(target), |el, note| {
                    el.child(git_ui::hint_line(note))
                })
                .into_any_element()
        }
        PushPlan::Detached => {
            git_ui::notice_line(t("gitBranch", "pushDetached"), ui::color_warning())
                .into_any_element()
        }
        PushPlan::NoRemote { .. } => {
            git_ui::notice_line(t("gitBranch", "pushNoRemote"), ui::color_warning())
                .into_any_element()
        }
    });

    // ── 待推送提交 ──
    let now = now_secs();
    let mut list = div()
        .id("git-push-commits")
        .max_h(px(300.0))
        .overflow_y_scroll()
        .p(px(4.0));
    if outgoing.commits.is_empty() {
        list = list.child(git_ui::empty_state(
            git_ui::ICON_CLEAN,
            if target.as_ref().is_some_and(PushTarget::sets_upstream) {
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
            git_ui::list_row(
                SharedString::from(format!("git-push-commit-{index}")),
                false,
            )
            .child(
                git_ui::mono(div())
                    .flex_none()
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
                    .max_w(px(180.0))
                    .truncate()
                    .text_size(ui::font_px(12.0))
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
            .when_some(count_line, |el, line| el.child(git_ui::hint_line(line)))
            .child(
                div()
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(ui::border_subtle())
                    .bg(ui::bg_surface())
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
                    el.child(git_ui::notice_line(
                        t("gitBranch", "rejectedHint"),
                        ui::color_warning(),
                    ))
                })
                .child(git_ui::error_text_box(text)),
        );
    }

    // ── 强推选项。勾上时整块换危险色并给警告,页脚确认键同时转危险色 ──
    let danger = ui::color_error();
    let state_for_force = state.clone();
    let force_row = git_ui::check_row("git-push-force", force, t("gitBranch", "forcePush"), true)
        .on_click(move |_: &ClickEvent, _window, cx| {
            state_for_force.update(cx, |s, cx| {
                if !s.pushing {
                    s.force = !s.force;
                    cx.notify();
                }
            });
        });
    let warning = target.as_ref().filter(|_| force).map(|target| {
        git_ui::notice_line(
            tr!("gitBranch", "forceWarning", target = target.label()),
            danger,
        )
    });

    root.child(
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .px(px(12.0))
            .py(px(8.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(if force {
                ui::with_alpha(danger, 0.45)
            } else {
                ui::border_subtle()
            })
            .when(force, |el| el.bg(ui::with_alpha(danger, 0.08)))
            .child(force_row)
            .children(warning),
    )
    .when(pushing, |el| {
        el.child(git_ui::busy_line(t("gitBranch", "pushing")))
    })
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
            upstream: has_upstream.then(|| "origin/main".to_string()),
            upstream_differs: false,
            tracking_oid: None,
        }
    }

    /// 只有「配了上游、但这次推的不是它」才出那行说明,且写明目标与上游。
    #[test]
    fn 目标与上游不同时才有说明() {
        assert_eq!(upstream_note(&target(true)), None);
        assert_eq!(upstream_note(&target(false)), None);
        let feature = PushTarget {
            branch: "feature".into(),
            remote_branch: "feature".into(),
            upstream_differs: true,
            ..target(true)
        };
        let note = upstream_note(&feature).expect("应有说明");
        assert!(
            note.contains("origin/feature") && note.contains("origin/main"),
            "{note}"
        );
        // 已有上游:推完不补「并设为上游」
        let summary = pushed_summary(&feature, 1, false, false);
        assert!(
            !summary.ends_with(t("gitBranch", "upstreamSetSuffix")),
            "{summary}"
        );
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
