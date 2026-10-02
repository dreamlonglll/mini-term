//! 「关联 SSH」弹窗(对照 `src/components/SshAssocModal.tsx` 363 行)。
//!
//! 按项目设定 agent 可访问的 SSH 连接范围:勾选 ≥1 个 = 为该项目启用 SSH 工具
//! (CLI + Skill)并限定范围,全部取消 = 停用。
//!
//! # 逻辑都不在这儿
//!
//! 初始勾选([`crate::ssh_conn::initial_checked`])、范围等价([`same_scope`])、
//! 保存计划([`plan_assoc_save`])三条纯函数在 [`crate::ssh_conn`];
//! 「算计划 → 后台跑注册器 → 回主线程落配置」整条在
//! [`AppStore::apply_ssh_assoc`]。本模块只做四件事:画勾选列表、收集勾选、
//! `await` 那个 `Task`、按四档结果决定关窗/提示。
//!
//! ```text
//! Ok(None)                  → 直接关窗(从未启用、这次也没勾,没有生成物要 reconcile)
//! Ok(Some(o)) 且 o.silent   → 静默关窗(幂等 reconcile / 旧配置迁移,有效范围没变)
//! Ok(Some(o))               → 关窗 + 按 enabled/was_enabled 三档提示
//! Err(e)                    → 不关窗,弹「关联 SSH 失败」
//! ```
//!
//! 左栏分组 + 右栏桶与 [`crate::ssh_panel`] **同构且共用同一份视图件** ——
//! 原版靠 `import { GroupSidebarRow } from './SshModal'`,这里照办。
//!
//! # 整组关联(GPUI 版新增,原版没有)
//!
//! 左栏每个具名分组前有一个勾选框:勾上 = **整组关联**,组内现有的、以及之后才
//! 加进这个组的连接都对该项目可见(存成组名,写 sidecar 投影时才展开,见
//! [`mt_config::ProjectConfig::ssh_connection_groups`])。勾选框的点击只管关联,
//! 点行的其余位置仍是「切到这个分组看」—— 两件事分开,不会点一下就改了权限。
//!
//! 被整组罩住的连接在右栏画成「已勾 + 变淡 + 后缀说明」且点不动:它们的去留跟着
//! 组走,单条再勾 / 取消没有意义。保存时单勾列表里剔掉这些 id,免得连接挪出组后
//! 还被一条看不见的单勾留在范围里。
//!
//! [`same_scope`]: crate::ssh_conn::same_scope
//! [`plan_assoc_save`]: crate::ssh_conn::plan_assoc_save
//! [`AppStore::apply_ssh_assoc`]: crate::store::AppStore::apply_ssh_assoc

use std::collections::HashSet;

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Task, Window, div,
    prelude::FluentBuilder as _, px,
};

use crate::i18n::{t, tr};
use crate::prompt::{close_guarded, kind, open_guarded, show_alert};
use crate::ssh_conn::{
    SshGroupBucket, build_group_buckets, effective_scope, initial_checked, initial_checked_groups,
};
use crate::ssh_panel::{
    BucketCollapse, GroupKey, PANEL_W, conn_card, conn_text, panel_footer, panel_header,
    panel_total_h, render_conn_buckets, resolve_active, sidebar_row_with, visible_buckets,
};
use crate::store::{AppStore, SshAssocOutcome};
use crate::ui;

/// 侧栏宽度(与「SSH 连接」面板同,原版 `w-44`)。
const SIDEBAR_W: f32 = 176.0;
/// 左栏勾选框那一列的宽度(= `ui::checkbox` 的边长)。没有勾选框的行(全部 / 未分组)
/// 垫同宽占位,名字才对得齐。
const GROUP_CHECK_W: f32 = 14.0;

pub struct SshAssocPanel {
    store: Entity<AppStore>,
    project_id: String,
    project_name: String,
    /// 单勾的连接 id。被整组罩住的连接在不在这里都无所谓(保存时剔掉)——
    /// 留着是为了取消整组之后,用户原先单勾过的那几条还是勾着的。
    checked: HashSet<String>,
    /// 整组关联的分组名(已 trim)。
    checked_groups: HashSet<String>,
    /// 保存中:两颗按钮置灰、Esc 与遮罩都不给关(正在写配置,半途退出会让
    /// store 与磁盘不一致)。
    busy: bool,
    selected: GroupKey,
    collapsed: HashSet<String>,
    _task: Option<Task<()>>,
}

impl Render for SshAssocPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl BucketCollapse for SshAssocPanel {
    fn collapsed_set(&mut self) -> &mut HashSet<String> {
        &mut self.collapsed
    }
}

/// 打开「关联 SSH」。`project_id` 不存在时什么都不做。
pub fn open(store: Entity<AppStore>, project_id: &str, window: &mut Window, cx: &mut App) {
    if crate::overlay::contains(crate::overlay::key(kind::SSH_ASSOC)) {
        return;
    }
    let Some(project) = store.read(cx).project(project_id).cloned() else {
        return;
    };
    let (all_ids, group_names) = {
        let store = store.read(cx);
        let all_ids: Vec<String> = store
            .ssh_connections()
            .iter()
            .map(|c| c.id.clone())
            .collect();
        let group_names =
            build_group_buckets(store.ssh_connections(), store.ssh_groups()).group_names();
        (all_ids, group_names)
    };
    let checked: HashSet<String> = initial_checked(&project, &all_ids).into_iter().collect();
    let checked_groups: HashSet<String> = initial_checked_groups(&project, &group_names)
        .into_iter()
        .collect();

    let state = cx.new(|_cx| SshAssocPanel {
        store,
        project_id: project.id.clone(),
        project_name: project.name.clone(),
        checked,
        checked_groups,
        busy: false,
        selected: GroupKey::All,
        collapsed: HashSet::new(),
        _task: None,
    });

    open_guarded(kind::SSH_ASSOC, window, cx, move |dialog, window, cx| {
        let busy = state.read(cx).busy;
        let total = panel_total_h(window.viewport_size());
        let body = render_body(&state, total, cx);
        dialog
            .p_0()
            .close_button(false)
            .w(px(PANEL_W))
            // 保存中不给关:正在写配置,半途退出会让 store 与磁盘不一致
            // (原版 `closeOnOverlay={!busy}` / `closeOnEscape={!busy}`)
            .overlay_closable(!busy)
            .keyboard(!busy)
            .child(body)
    });
}

// ─── 保存 ─────────────────────────────────────────────────────

/// 范围文案:全选 → 「全部 N 个连接」,否则 →「N 个连接」;有整组关联时再补一句
/// 「含 M 个整组关联的分组,之后加入的也自动可用」—— 只报此刻的台数,用户会以为
/// 组里以后新加的不算。
fn scope_desc(scope_len: usize, total_len: usize, group_count: usize) -> String {
    let scope = if scope_len == total_len {
        tr!("sshAssoc", "scopeAll", count = total_len)
    } else {
        tr!("sshAssoc", "scopeSubset", count = scope_len)
    };
    if group_count == 0 {
        scope
    } else {
        tr!(
            "sshAssoc",
            "scopeWithGroups",
            scope = scope,
            count = group_count
        )
    }
}

/// 三档提示的标题 / 正文 key。**动态 key 必须写成 match**(不能拼字符串):
/// `t()` 的 debug_assert 与 `i18n.rs` 的 `USED_KEYS` 表都要求 key 是字面量。
fn outcome_alert(outcome: &SshAssocOutcome, project_name: &str) -> (SharedString, SharedString) {
    let scope = scope_desc(outcome.scope_len, outcome.total_len, outcome.group_count);
    match (outcome.enabled, outcome.was_enabled) {
        (true, false) => (
            t("sshAssoc", "enabledTitle").into(),
            tr!(
                "sshAssoc",
                "enabledMessage",
                name = project_name.to_string(),
                scope = scope
            )
            .into(),
        ),
        (true, true) => (
            t("sshAssoc", "updatedTitle").into(),
            tr!(
                "sshAssoc",
                "updatedMessage",
                name = project_name.to_string(),
                scope = scope
            )
            .into(),
        ),
        _ => (
            t("sshAssoc", "disabledTitle").into(),
            tr!(
                "sshAssoc",
                "disabledMessage",
                name = project_name.to_string(),
                scope = scope
            )
            .into(),
        ),
    }
}

fn save(state: &Entity<SshAssocPanel>, window: &mut Window, cx: &mut App) {
    if state.read(cx).busy {
        return;
    }
    // 始终存显式 id 列表且顺序取 `allIds` —— 与原版 `allIds.filter(...)` 同。
    // 被整组罩住的 id 不进单勾列表(见模块注释);组名按左栏展示序存。
    let (project_id, project_name, scope, groups) = {
        let panel = state.read(cx);
        let store = panel.store.read(cx);
        let connections = store.ssh_connections();
        let groups: Vec<String> = build_group_buckets(connections, store.ssh_groups())
            .group_names()
            .into_iter()
            .filter(|g| panel.checked_groups.contains(g))
            .collect();
        let covered: HashSet<String> = effective_scope(&[], &groups, connections)
            .into_iter()
            .collect();
        let scope: Vec<String> = connections
            .iter()
            .map(|c| c.id.clone())
            .filter(|id| panel.checked.contains(id) && !covered.contains(id))
            .collect();
        (
            panel.project_id.clone(),
            panel.project_name.clone(),
            scope,
            groups,
        )
    };

    let task = state.read(cx).store.clone();
    let apply = state.update(cx, |panel, cx| {
        panel.busy = true;
        cx.notify();
        task.update(cx, |store, cx| {
            store.apply_ssh_assoc(&project_id, scope, groups, cx)
        })
    });

    let state_for_task = state.clone();
    let handle = window.spawn(cx, async move |cx| {
        let result = apply.await;
        let _ = cx.update(|window, cx| match result {
            // 从未启用、这次也没勾:没有生成物要 reconcile,直接关窗
            Ok(None) => {
                close(&state_for_task, window, cx);
            }
            Ok(Some(outcome)) => {
                close(&state_for_task, window, cx);
                // 幂等 reconcile / 存量迁移:落盘即可,不弹提示
                if outcome.silent {
                    return;
                }
                let (title, message) = outcome_alert(&outcome, &project_name);
                show_alert(title, message, window, cx);
            }
            Err(err) => {
                // 失败**不关窗**(原版 `setBusy(false)` 后弹提示,弹窗留着让用户重试)
                state_for_task.update(cx, |panel, cx| {
                    panel.busy = false;
                    cx.notify();
                });
                show_alert(t("sshAssoc", "saveFailedTitle"), err, window, cx);
            }
        });
    });
    state.update(cx, |panel, _cx| panel._task = Some(handle));
}

/// 关窗。`window.close_dialog` 不触发 `Dialog::on_close`,必须走
/// [`close_guarded`] 自己摘覆盖物栈(见 `prompt.rs` 的第六条路)。
fn close(state: &Entity<SshAssocPanel>, window: &mut Window, cx: &mut App) {
    state.update(cx, |panel, cx| {
        panel.busy = false;
        cx.notify();
    });
    close_guarded(kind::SSH_ASSOC, window, cx);
}

// ─── 渲染 ─────────────────────────────────────────────────────

struct Frame {
    total: usize,
    named: Vec<(String, Vec<mt_config::SshConnection>)>,
    ungrouped: Vec<mt_config::SshConnection>,
    order: Vec<SshGroupBucket>,
    active: GroupKey,
    collapsed: HashSet<String>,
    checked: HashSet<String>,
    checked_groups: HashSet<String>,
    /// 被整组关联罩住的连接 id —— 这些行画成已勾、点不动。
    covered: HashSet<String>,
    /// 此刻实际可见的连接数(单勾 ∪ 组内成员),「已选 N / M」的 N。
    effective_count: usize,
    busy: bool,
    project_name: String,
}

fn read_frame(state: &Entity<SshAssocPanel>, cx: &App) -> Frame {
    let panel = state.read(cx);
    let store = panel.store.read(cx);
    let connections = store.ssh_connections().to_vec();
    let buckets = build_group_buckets(&connections, store.ssh_groups());
    let group_names = buckets.group_names();
    let active = resolve_active(&panel.selected, &group_names, !buckets.ungrouped.is_empty());
    let linked: Vec<String> = group_names
        .iter()
        .filter(|g| panel.checked_groups.contains(*g))
        .cloned()
        .collect();
    let covered: HashSet<String> = effective_scope(&[], &linked, &connections)
        .into_iter()
        .collect();
    let effective_count = connections
        .iter()
        .filter(|c| covered.contains(&c.id) || panel.checked.contains(&c.id))
        .count();
    Frame {
        total: connections.len(),
        named: buckets.named.clone(),
        ungrouped: buckets.ungrouped.clone(),
        order: buckets.display_order(),
        active,
        collapsed: panel.collapsed.clone(),
        checked: panel.checked.clone(),
        checked_groups: panel.checked_groups.clone(),
        covered,
        effective_count,
        busy: panel.busy,
        project_name: panel.project_name.clone(),
    }
}

fn render_body(state: &Entity<SshAssocPanel>, total: gpui::Pixels, cx: &mut App) -> AnyElement {
    let frame = read_frame(state, cx);
    div()
        .h(total)
        .flex()
        .flex_col()
        .child(panel_header(
            kind::SSH_ASSOC,
            t("sshAssoc", "title"),
            Some(tr!(
                "sshAssoc",
                "subtitle",
                name = frame.project_name.clone()
            )),
            !frame.busy,
        ))
        .child(
            div()
                .flex_1()
                .flex()
                .min_h(px(0.0))
                .child(render_sidebar(state, &frame))
                .child(render_list(state, &frame)),
        )
        .child(render_footer(state, &frame))
        .into_any_element()
}

/// 左栏那一列勾选框的占位(全部 / 未分组两行没有整组关联)。
fn group_check_spacer() -> AnyElement {
    div().w(px(GROUP_CHECK_W)).flex_none().into_any_element()
}

/// 左栏具名分组前的整组关联勾选框。自己吃掉点击 —— 不然会连带触发整行的
/// 「切到这个分组看」,勾一下关联就顺手换了右栏视图。
fn group_checkbox(state: &Entity<SshAssocPanel>, name: &str, on: bool) -> AnyElement {
    ui::checkbox(SharedString::from(format!("ssh-assoc-group-cb-{name}")), on)
        .on_click({
            let state = state.clone();
            let name = name.to_string();
            move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                cx.stop_propagation();
                let name = name.clone();
                state.update(cx, |panel, cx| {
                    if !panel.checked_groups.remove(&name) {
                        panel.checked_groups.insert(name);
                    }
                    cx.notify();
                });
            }
        })
        .into_any_element()
}

fn render_sidebar(state: &Entity<SshAssocPanel>, frame: &Frame) -> AnyElement {
    let pick = |state: &Entity<SshAssocPanel>, key: GroupKey| {
        let state = state.clone();
        move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
            let key = key.clone();
            state.update(cx, |panel, cx| {
                panel.selected = key;
                cx.notify();
            });
        }
    };
    let mut bar = div()
        .id("ssh-assoc-sidebar")
        .w(px(SIDEBAR_W))
        .flex_none()
        .h_full()
        .overflow_y_scroll()
        .py(px(8.0))
        .flex()
        .flex_col()
        .gap(px(2.0))
        .border_r_1()
        .border_color(ui::border_subtle())
        .child(
            sidebar_row_with(
                "ssh-assoc-all",
                Some(group_check_spacer()),
                t("sshAssoc", "allConnections"),
                frame.total,
                frame.active == GroupKey::All,
                false,
            )
            .on_click(pick(state, GroupKey::All)),
        );
    for (name, items) in &frame.named {
        let key = GroupKey::Named(name.clone());
        bar = bar.child(
            sidebar_row_with(
                SharedString::from(format!("ssh-assoc-group-{name}")),
                Some(group_checkbox(
                    state,
                    name,
                    frame.checked_groups.contains(name),
                )),
                name.clone(),
                items.len(),
                frame.active == key,
                false,
            )
            .on_click(pick(state, key)),
        );
    }
    if !frame.ungrouped.is_empty() {
        bar = bar.child(
            sidebar_row_with(
                "ssh-assoc-ungrouped",
                Some(group_check_spacer()),
                t("sshAssoc", "ungrouped"),
                frame.ungrouped.len(),
                frame.active == GroupKey::Ungrouped,
                false,
            )
            .on_click(pick(state, GroupKey::Ungrouped)),
        );
    }
    bar.into_any_element()
}

fn render_list(state: &Entity<SshAssocPanel>, frame: &Frame) -> AnyElement {
    let mut list = div()
        .id("ssh-assoc-list")
        .flex_1()
        .min_w(px(0.0))
        .h_full()
        .overflow_y_scroll()
        .px(px(20.0))
        .py(px(16.0))
        .flex()
        .flex_col()
        .gap(px(12.0));

    if frame.total == 0 {
        return list
            .child(
                div()
                    .py(px(40.0))
                    .flex()
                    .justify_center()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .child(t("sshAssoc", "empty")),
            )
            .into_any_element();
    }

    let buckets = visible_buckets(&frame.order, &frame.active);
    // 全选 / 全不选作用于「当前看得见的连接」:在某个分组里点全选,不该顺手
    // 把别的组也勾上(原版 `visibleIds`)
    let visible_ids: Vec<String> = buckets
        .iter()
        .flat_map(|b| b.items.iter().map(|c| c.id.clone()))
        .collect();
    // 「全不选」连看得见的整组关联一起摘:不摘的话组内那几行仍是勾着的,
    // 点了全不选却还有勾,像是按钮坏了。「全选」不替用户勾组 —— 组是另一层权限
    let visible_groups: Vec<String> = buckets.iter().filter_map(|b| b.group.clone()).collect();

    list = list.child(
        div()
            .flex()
            .items_center()
            .justify_between()
            .text_size(ui::font_px(11.0))
            .text_color(ui::text_muted())
            .child(tr!(
                "sshAssoc",
                "selectedCount",
                checked = frame.effective_count,
                total = frame.total
            ))
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        div()
                            .id("ssh-assoc-select-all")
                            .cursor_pointer()
                            .hover(|el| el.text_color(ui::accent()))
                            .child(t("sshAssoc", "selectAll"))
                            .on_click(set_many(state, visible_ids.clone(), Vec::new(), true)),
                    )
                    .child(div().opacity(0.4).child("|"))
                    .child(
                        div()
                            .id("ssh-assoc-select-none")
                            .cursor_pointer()
                            .hover(|el| el.text_color(ui::accent()))
                            .child(t("sshAssoc", "selectNone"))
                            .on_click(set_many(state, visible_ids, visible_groups, false)),
                    ),
            ),
    );

    // 正看着一个整组关联的分组:顶上说一句「之后加进来的也算」—— 右栏只列得出
    // 此刻的成员,这半句语义只能写出来
    if let GroupKey::Named(name) = &frame.active
        && frame.checked_groups.contains(name)
    {
        list = list.child(
            div()
                .px(px(10.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(ui::accent())
                .text_size(ui::font_px(11.0))
                .text_color(ui::accent())
                .child(tr!("sshAssoc", "groupLinkedHint", group = name.clone())),
        );
    }

    // 骨架与另两个弹窗共用(见 [`crate::ssh_panel::render_conn_buckets`]);
    // 本弹窗的行内容是勾选框 + 整行可点
    list = list.children(render_conn_buckets(
        state,
        buckets,
        &frame.active,
        &frame.collapsed,
        !frame.named.is_empty(),
        "ssh-assoc-bucket-",
        t("sshAssoc", "ungrouped"),
        |conn| {
            let id = conn.id.clone();
            let covered = frame.covered.contains(&id);
            let on = covered || frame.checked.contains(&id);
            let suffix = if covered {
                t("sshAssoc", "coveredByGroup")
            } else {
                ""
            };
            conn_card(SharedString::from(format!("ssh-assoc-row-{id}")), false)
                .when(!covered, |el| el.cursor_pointer())
                // 被整组罩住:勾是组给的,单条点不动 —— 变淡告诉用户「这条不归你单独管」
                .when(covered, |el| el.opacity(0.6))
                .child(
                    ui::checkbox(SharedString::from(format!("ssh-assoc-cb-{id}")), on)
                        .when(covered, |el| el.cursor_default()),
                )
                .child(conn_text(conn, suffix))
                // 整行可点(原版是 `<label>` 包着 checkbox)
                .when(!covered, |el| {
                    el.on_click({
                        let state = state.clone();
                        let id = id.clone();
                        move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                            let id = id.clone();
                            state.update(cx, |panel, cx| {
                                if !panel.checked.remove(&id) {
                                    panel.checked.insert(id);
                                }
                                cx.notify();
                            });
                        }
                    })
                })
                .into_any_element()
        },
    ));
    list.into_any_element()
}

/// 「全选 / 全不选」:批量改看得见的单勾;`groups` 非空时连这些组的整组关联一起
/// 改(只有「全不选」会传)。
fn set_many(
    state: &Entity<SshAssocPanel>,
    ids: Vec<String>,
    groups: Vec<String>,
    on: bool,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let state = state.clone();
    move |_event, _window, cx| {
        let (ids, groups) = (ids.clone(), groups.clone());
        state.update(cx, |panel, cx| {
            for id in ids {
                if on {
                    panel.checked.insert(id);
                } else {
                    panel.checked.remove(&id);
                }
            }
            for group in groups {
                if on {
                    panel.checked_groups.insert(group);
                } else {
                    panel.checked_groups.remove(&group);
                }
            }
            cx.notify();
        });
    }
}

fn render_footer(state: &Entity<SshAssocPanel>, frame: &Frame) -> AnyElement {
    let busy = frame.busy;
    panel_footer(
        if frame.checked.is_empty() && frame.checked_groups.is_empty() {
            t("sshAssoc", "footerHintEmpty")
        } else {
            t("sshAssoc", "footerHintSelected")
        },
    )
    .child(
        ui::ghost_button("ssh-assoc-cancel", t("sshAssoc", "cancel"))
            .opacity(if busy { 0.4 } else { 1.0 })
            .on_click({
                let state = state.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    if state.read(cx).busy {
                        return;
                    }
                    close(&state, window, cx);
                }
            }),
    )
    .child(
        ui::primary_button(
            "ssh-assoc-save",
            if busy {
                t("sshAssoc", "saving")
            } else {
                t("sshAssoc", "save")
            },
        )
        .opacity(if busy { 0.4 } else { 1.0 })
        .on_click({
            let state = state.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| save(&state, window, cx)
        }),
    )
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(enabled: bool, was_enabled: bool, scope: usize, total: usize) -> SshAssocOutcome {
        SshAssocOutcome {
            enabled,
            was_enabled,
            silent: false,
            scope_len: scope,
            total_len: total,
            group_count: 0,
            project_token: None,
            message: String::new(),
        }
    }

    /// 范围文案:全选走 `scopeAll`(带总数),子集走 `scopeSubset`。
    #[test]
    fn 范围文案按是否全选分档() {
        let all = scope_desc(3, 3, 0);
        let subset = scope_desc(1, 3, 0);
        assert_ne!(all, subset);
        assert!(all.contains('3'));
        assert!(subset.contains('1'));
        // 一个都没勾(停用档)也是子集文案
        assert!(scope_desc(0, 3, 0).contains('0'));
    }

    /// 有整组关联时文案带上组数,且包着原来那段台数 —— 两样都得让用户看到。
    #[test]
    fn 整组关联的范围文案带组数() {
        let plain = scope_desc(2, 5, 0);
        let grouped = scope_desc(2, 5, 7);
        assert_ne!(plain, grouped);
        assert!(grouped.contains(&plain), "台数那段要原样保留: {grouped}");
        assert!(grouped.contains('7'));
    }

    /// 三档提示各走各的标题:首次启用 / 更新范围 / 停用,两两不同。
    #[test]
    fn 三档提示标题互不相同() {
        let first = outcome_alert(&outcome(true, false, 2, 3), "P").0;
        let updated = outcome_alert(&outcome(true, true, 2, 3), "P").0;
        let disabled = outcome_alert(&outcome(false, true, 0, 3), "P").0;
        assert_ne!(first, updated);
        assert_ne!(updated, disabled);
        assert_ne!(first, disabled);
    }

    /// 正文里必须带项目名与范围 —— 少了范围用户就不知道这次到底放开了几台。
    #[test]
    fn 提示正文带项目名与范围() {
        let (_, message) = outcome_alert(&outcome(true, false, 2, 3), "我的项目");
        assert!(message.contains("我的项目"));
        assert!(message.contains('2'));
    }
}
