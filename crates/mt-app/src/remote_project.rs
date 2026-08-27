//! 「添加远程项目」弹窗(对照 `src/components/AddRemoteProjectModal.tsx` 297 行)。
//!
//! 选一条已存的 SSH 连接 + 手输远程路径(默认 `~`),保存前先
//! [`crate::remote_ssh::validate_dir`] 验证目录存在(`~` 由 SFTP canonicalize
//! 展开),拿返回的**展开绝对路径**落 config;项目名默认取路径末段,可编辑。
//!
//! ```text
//! 「添加」→ [后台] validate_dir(conn, path)
//!            ├─ Err(msg) → 弹窗留着,红字显示 msg(busy 复位)
//!            └─ Ok(canonical) → add_remote_project + 展开目标分组 + 切过去 + 关窗
//! ```
//!
//! ⚠️ `validate_dir` 是**阻塞**函数(TCP + KEX + SFTP 往返),雷打不动丢
//! `background_executor` —— 主线程直调就是整个窗口卡住,见 `remote_ssh` 的线程口径。
//!
//! 连接选择区(左栏分组 + 右栏单选列表)与 [`crate::ssh_panel`] 同构且共用
//! 同一份视图件。
//!
//! # 三个入口
//!
//! 项目列表底部的 `SSH` 钮(根层)、分组右键「添加远程项目」(落进该组)、
//! 首启引导第二颗按钮。三者共用 [`crate::overlay::kind::ADD_REMOTE_PROJECT`]
//! 一种覆盖物 —— 防的正是「两处入口各开一个」。

use std::collections::HashSet;

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Task,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use mt_config::SshConnection;

use crate::i18n::t;
use crate::prompt::{autofocus, close_guarded, kind, open_guarded};
use crate::ssh_conn::{SshGroupBucket, build_group_buckets};
use crate::ssh_panel::{
    BucketCollapse, GroupKey, PANEL_W, conn_card, conn_text, panel_footer, panel_header,
    panel_total_h, render_conn_buckets, resolve_active, sidebar_row, visible_buckets,
};
use crate::store::AppStore;
use crate::ui;

const SIDEBAR_W: f32 = 176.0;

pub struct AddRemotePanel {
    store: Entity<AppStore>,
    /// 选中的连接 id(原版是 `<input type=radio>` 的受控值)。
    connection_id: String,
    path: Entity<InputState>,
    name: Entity<InputState>,
    busy: bool,
    error: String,
    /// 从分组右键进来时的目标分组;`None` = 加到根层。
    target_group: Option<String>,
    selected: GroupKey,
    collapsed: HashSet<String>,
    /// 输入框里按过回车、还没被消费。**订阅拿不到 `&mut Window`**(提交要起
    /// 后台任务并可能关窗),所以订阅只置这个标志,由下一帧的 Dialog builder
    /// 消费 —— builder 手里两样都有。
    enter_pressed: bool,
    _subs: Vec<Subscription>,
    _task: Option<Task<()>>,
}

impl Render for AddRemotePanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl BucketCollapse for AddRemotePanel {
    fn collapsed_set(&mut self) -> &mut HashSet<String> {
        &mut self.collapsed
    }
}

/// 打开「添加远程项目」。`target_group` 非空 = 新项目直接落进该分组。
pub fn open(
    store: Entity<AppStore>,
    target_group: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    // 守卫要在**建输入框之前**判(与 `prompt::show_prompt` 同一条)
    if crate::overlay::contains(crate::overlay::key(kind::ADD_REMOTE_PROJECT)) {
        return;
    }
    // 默认选第一条连接(原版 `connections[0]?.id ?? ''`)
    let first = store
        .read(cx)
        .ssh_connections()
        .first()
        .map(|c| c.id.clone())
        .unwrap_or_default();

    let path = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(t("remoteProject", "pathPlaceholder"))
            .default_value("~")
    });
    let name =
        cx.new(|cx| InputState::new(window, cx).placeholder(t("remoteProject", "namePlaceholder")));
    // 打开即可直接接着 `~` 往下敲路径。聚焦排在 `open_guarded` 之后,
    // 判据见 `prompt::autofocus`
    let path_for_focus = path.clone();

    let state = cx.new(|cx| {
        // 两个输入框里按回车 = 点「添加」(原版那两处 `onKeyDown`)。
        // `InputState` 单行模式下无条件 emit `PressEnter`,订阅它比抢键位直白。
        let on_enter = |this: &mut AddRemotePanel,
                        _e,
                        event: &InputEvent,
                        cx: &mut Context<AddRemotePanel>| {
            if !this.busy && matches!(event, InputEvent::PressEnter { .. }) {
                this.enter_pressed = true;
                cx.notify();
            }
        };
        let subs = vec![cx.subscribe(&path, on_enter), cx.subscribe(&name, on_enter)];
        AddRemotePanel {
            store,
            connection_id: first,
            path,
            name,
            busy: false,
            error: String::new(),
            target_group,
            selected: GroupKey::All,
            collapsed: HashSet::new(),
            enter_pressed: false,
            _subs: subs,
            _task: None,
        }
    });

    open_guarded(
        kind::ADD_REMOTE_PROJECT,
        window,
        cx,
        move |dialog, window, cx| {
            // 回车提交要 `&mut Window`(要起后台任务并可能关窗),订阅里拿不到,
            // 于是订阅只置一个标志,下一帧的 builder 里消费掉。
            // **必须 `window.defer`**:builder 跑在 `Root::render` 里,当场
            // `state.update` + 起任务是「渲染期间改状态」,与 Z 批那条
            // 「toast 点击必须 defer」同一个坑。
            if state.read(cx).enter_pressed {
                state.update(cx, |panel, _cx| panel.enter_pressed = false);
                let state = state.clone();
                window.defer(cx, move |window, cx| save(&state, window, cx));
            }
            let busy = state.read(cx).busy;
            let total = panel_total_h(window.viewport_size());
            let body = render_body(&state, total, cx);
            dialog
                .p_0()
                .close_button(false)
                .w(px(PANEL_W))
                // 保存中不给关:正在做远程校验,中途退出会留下半截状态
                .overlay_closable(!busy)
                .keyboard(!busy)
                .child(body)
        },
    );

    autofocus(&path_for_focus, window, cx);
}

// ─── 保存 ─────────────────────────────────────────────────────

fn save(state: &Entity<AddRemotePanel>, window: &mut Window, cx: &mut App) {
    if state.read(cx).busy {
        return;
    }
    let (conn, path, name, target_group) = {
        let panel = state.read(cx);
        let conn = panel
            .store
            .read(cx)
            .ssh_connections()
            .iter()
            .find(|c| c.id == panel.connection_id)
            .cloned();
        (
            conn,
            panel.path.read(cx).value().to_string(),
            panel.name.read(cx).value().to_string(),
            panel.target_group.clone(),
        )
    };
    let Some(conn) = conn else {
        state.update(cx, |panel, cx| {
            panel.error = t("remoteProject", "errorNoConnection").to_string();
            cx.notify();
        });
        return;
    };
    state.update(cx, |panel, cx| {
        panel.busy = true;
        panel.error.clear();
        cx.notify();
    });

    let conn_id = conn.id.clone();
    let submitted_path = path.clone();
    let submitted_name = name.clone();
    let submitted_group = target_group.clone();
    let state_for_task = state.clone();
    let handle = window.spawn(cx, async move |cx| {
        // [后台] `~` 展开 + canonicalize + stat 目录;不存在 / 非目录 / 连不上 → Err
        let result = cx
            .background_executor()
            .spawn(async move { crate::remote_ssh::validate_dir(&conn, &path) })
            .await;
        let _ = cx.update(|window, cx| {
            let (busy, current_conn, current_path, current_name, current_group) = {
                let panel = state_for_task.read(cx);
                let path = panel.path.clone();
                let name = panel.name.clone();
                (
                    panel.busy,
                    panel.connection_id.clone(),
                    path,
                    name,
                    panel.target_group.clone(),
                )
            };
            let snapshot_matches = busy
                && current_conn == conn_id
                && current_path.read(cx).value() == submitted_path
                && current_name.read(cx).value() == submitted_name
                && current_group == submitted_group;
            if !snapshot_matches {
                state_for_task.update(cx, |panel, cx| {
                    panel.busy = false;
                    cx.notify();
                });
                return;
            }

            match result {
                Ok(canonical) => {
                    // 校验期间允许其它种类的覆盖物叠到上面。只有当前添加项目弹窗
                    // 已回到栈顶时才真正写配置，否则 `close_guarded` 无法关闭它，
                    // 用户稍后再次点击会重复创建同一项目。
                    if !crate::overlay::is_top(crate::overlay::key(kind::ADD_REMOTE_PROJECT)) {
                        state_for_task.update(cx, |panel, cx| {
                            panel.busy = false;
                            panel.error =
                                t("remoteProject", "errorCloseOverlayBeforeSave").to_string();
                            cx.notify();
                        });
                        return;
                    }
                    state_for_task.update(cx, |panel, cx| {
                        let id = panel.store.update(cx, |store, cx| {
                            let id = store.add_remote_project(
                                &name,
                                &conn_id,
                                &canonical,
                                target_group.as_deref(),
                                cx,
                            );
                            // 目标分组若折叠则展开,确保新项目可见(与本地「添加项目」一致)
                            if let Some(group_id) = target_group.as_deref()
                                && store
                                    .config()
                                    .project_tree
                                    .as_ref()
                                    .and_then(|tree| {
                                        crate::project_tree::find_group_in_tree(tree, group_id)
                                    })
                                    .is_some_and(|g| g.collapsed)
                            {
                                store.toggle_group_collapse(group_id, cx);
                            }
                            id
                        });
                        panel
                            .store
                            .update(cx, |store, cx| store.set_active_project(&id, cx));
                        panel.busy = false;
                        cx.notify();
                    });
                    let closed = close_guarded(kind::ADD_REMOTE_PROJECT, window, cx);
                    debug_assert!(closed, "添加远程项目写入前已确认弹窗位于栈顶");
                }
                Err(err) => {
                    // 校验失败**不关窗**:用户刚打的路径还在框里,改一改就能再试
                    state_for_task.update(cx, |panel, cx| {
                        panel.busy = false;
                        panel.error = err;
                        cx.notify();
                    });
                }
            }
        });
    });
    state.update(cx, |panel, _cx| panel._task = Some(handle));
}

// ─── 渲染 ─────────────────────────────────────────────────────

struct Frame {
    total: usize,
    named: Vec<(String, Vec<SshConnection>)>,
    ungrouped: Vec<SshConnection>,
    order: Vec<SshGroupBucket>,
    active: GroupKey,
    collapsed: HashSet<String>,
    connection_id: String,
    busy: bool,
    error: String,
}

fn read_frame(state: &Entity<AddRemotePanel>, cx: &App) -> Frame {
    let panel = state.read(cx);
    let store = panel.store.read(cx);
    let connections = store.ssh_connections().to_vec();
    let buckets = build_group_buckets(&connections, store.ssh_groups());
    let group_names = buckets.group_names();
    let active = resolve_active(&panel.selected, &group_names, !buckets.ungrouped.is_empty());
    Frame {
        total: connections.len(),
        named: buckets.named.clone(),
        ungrouped: buckets.ungrouped.clone(),
        order: buckets.display_order(),
        active,
        collapsed: panel.collapsed.clone(),
        connection_id: panel.connection_id.clone(),
        busy: panel.busy,
        error: panel.error.clone(),
    }
}

fn render_body(state: &Entity<AddRemotePanel>, total: gpui::Pixels, cx: &mut App) -> AnyElement {
    let frame = read_frame(state, cx);
    let mut root = div().h(total).flex().flex_col().child(panel_header(
        kind::ADD_REMOTE_PROJECT,
        t("remoteProject", "title"),
        Some(t("remoteProject", "subtitle").to_string()),
        !frame.busy,
    ));

    if frame.total == 0 {
        // 一条连接都没有:整个选择区与表单都不画,只给一句引导
        root = root.child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px(px(32.0))
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(t("remoteProject", "noConnections")),
        );
    } else {
        root = root
            .child(
                div()
                    .flex_1()
                    .flex()
                    .min_h(px(0.0))
                    .child(render_sidebar(state, &frame))
                    .child(render_list(state, &frame)),
            )
            .child(render_form(state, &frame, cx));
    }

    root.child(render_footer(state, &frame)).into_any_element()
}

fn render_sidebar(state: &Entity<AddRemotePanel>, frame: &Frame) -> AnyElement {
    let pick = |state: &Entity<AddRemotePanel>, key: GroupKey| {
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
        .id("remote-sidebar")
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
            sidebar_row(
                "remote-all",
                t("remoteProject", "allConnections"),
                frame.total,
                frame.active == GroupKey::All,
                false,
            )
            .on_click(pick(state, GroupKey::All)),
        );
    for (name, items) in &frame.named {
        let key = GroupKey::Named(name.clone());
        bar = bar.child(
            sidebar_row(
                SharedString::from(format!("remote-group-{name}")),
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
            sidebar_row(
                "remote-ungrouped",
                t("remoteProject", "ungrouped"),
                frame.ungrouped.len(),
                frame.active == GroupKey::Ungrouped,
                false,
            )
            .on_click(pick(state, GroupKey::Ungrouped)),
        );
    }
    bar.into_any_element()
}

fn render_list(state: &Entity<AddRemotePanel>, frame: &Frame) -> AnyElement {
    let has_named = !frame.named.is_empty();
    let mut list = div()
        .id("remote-conn-list")
        .flex_1()
        .min_w(px(0.0))
        .h_full()
        .overflow_y_scroll()
        .px(px(20.0))
        .py(px(16.0))
        .flex()
        .flex_col()
        .gap(px(12.0));

    // 骨架与另两个弹窗共用(见 [`crate::ssh_panel::render_conn_buckets`]);
    // 本弹窗的行内容是单选圆点
    list = list.children(render_conn_buckets(
        state,
        visible_buckets(&frame.order, &frame.active),
        &frame.active,
        &frame.collapsed,
        has_named,
        "remote-bucket-",
        t("remoteProject", "ungrouped"),
        |conn| {
            let id = conn.id.clone();
            let selected = frame.connection_id == id;
            conn_card(SharedString::from(format!("remote-row-{id}")), selected)
                .cursor_pointer()
                // 单选钮:实心圆点表示选中(与 `ui::checkbox` 同尺寸)
                .child(
                    div()
                        .flex_none()
                        .w(px(14.0))
                        .h(px(14.0))
                        .rounded_full()
                        .border_1()
                        .border_color(if selected {
                            ui::accent()
                        } else {
                            ui::border_strong()
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(selected, |el| {
                            el.child(div().w(px(7.0)).h(px(7.0)).rounded_full().bg(ui::accent()))
                        }),
                )
                .child(conn_text(conn, ""))
                .on_click({
                    let state = state.clone();
                    let id = id.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        if state.read(cx).busy {
                            return;
                        }
                        let id = id.clone();
                        let changed = state.read(cx).connection_id != id;
                        state.update(cx, |panel, cx| {
                            panel.connection_id = id;
                            panel.error.clear();
                            cx.notify();
                        });
                        if changed {
                            let path = state.read(cx).path.clone();
                            path.update(cx, |input, cx| input.set_value("~", window, cx));
                        }
                    }
                })
                .into_any_element()
        },
    ));
    list.into_any_element()
}

/// 路径 / 项目名(固定在连接区下方,不随列表滚动)。
fn render_form(state: &Entity<AddRemotePanel>, frame: &Frame, cx: &App) -> AnyElement {
    let (path, name) = {
        let panel = state.read(cx);
        (panel.path.clone(), panel.name.clone())
    };
    let row = |label: &'static str, control: gpui::AnyElement| {
        div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .w(px(160.0))
                    .flex_none()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .child(label),
            )
            .child(div().flex_1().min_w(px(0.0)).child(control))
    };
    div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .px(px(20.0))
        .py(px(16.0))
        .border_t_1()
        .border_color(ui::border_subtle())
        .child(row(
            t("remoteProject", "pathLabel"),
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(Input::new(&path).disabled(frame.busy)),
                )
                .child(
                    ui::ghost_button("remote-project-browse", t("remoteProject", "browse"))
                        .opacity(if frame.busy { 0.4 } else { 1.0 })
                        .on_click({
                            let state = state.clone();
                            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                let (busy, connection, initial, input) = {
                                    let panel = state.read(cx);
                                    let connection = panel
                                        .store
                                        .read(cx)
                                        .ssh_connections()
                                        .iter()
                                        .find(|connection| connection.id == panel.connection_id)
                                        .cloned();
                                    (
                                        panel.busy,
                                        connection,
                                        panel.path.read(cx).value().to_string(),
                                        panel.path.clone(),
                                    )
                                };
                                if busy {
                                    return;
                                }
                                let Some(connection) = connection else {
                                    state.update(cx, |panel, cx| {
                                        panel.error =
                                            t("remoteProject", "errorNoConnection").to_string();
                                        cx.notify();
                                    });
                                    return;
                                };
                                let panel_state = state.clone();
                                crate::remote_directory_picker::open(
                                    connection,
                                    initial,
                                    move |selected, window, cx| {
                                        input.update(cx, |input, cx| {
                                            input.set_value(selected, window, cx)
                                        });
                                        panel_state.update(cx, |panel, cx| {
                                            panel.error.clear();
                                            cx.notify();
                                        });
                                    },
                                    window,
                                    cx,
                                );
                            }
                        }),
                )
                .into_any_element(),
        ))
        .child(row(
            t("remoteProject", "nameLabel"),
            Input::new(&name).disabled(frame.busy).into_any_element(),
        ))
        .when(!frame.error.is_empty(), |el| {
            el.child(
                div()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::color_error())
                    .child(frame.error.clone()),
            )
        })
        .into_any_element()
}

fn render_footer(state: &Entity<AddRemotePanel>, frame: &Frame) -> AnyElement {
    let busy = frame.busy;
    let disabled = busy || frame.total == 0;
    panel_footer(t("remoteProject", "footerHint"))
        .child(
            ui::ghost_button("remote-cancel", t("remoteProject", "cancel"))
                .opacity(if busy { 0.4 } else { 1.0 })
                .on_click({
                    let state = state.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        if state.read(cx).busy {
                            return;
                        }
                        close_guarded(kind::ADD_REMOTE_PROJECT, window, cx);
                    }
                }),
        )
        .child(
            ui::primary_button(
                "remote-save",
                if busy {
                    t("remoteProject", "validating")
                } else {
                    t("remoteProject", "save")
                },
            )
            .opacity(if disabled { 0.4 } else { 1.0 })
            .on_click({
                let state = state.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    if disabled {
                        return;
                    }
                    save(&state, window, cx);
                }
            }),
        )
        .into_any_element()
}
