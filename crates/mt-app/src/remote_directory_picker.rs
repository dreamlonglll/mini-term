//! Lightweight remote directory browser used by the add-remote-project form.

use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, StatefulInteractiveElement, Styled, Task, Window, div,
    prelude::FluentBuilder as _, px,
};
use mt_config::SshConnection;

use crate::i18n::t;
use crate::prompt::{close_guarded, dialog_title, kind, open_guarded};
use crate::remote_ssh::{RemoteDirectoryEntry, RemoteDirectoryListing};
use crate::ui;

type SelectCallback = Rc<dyn Fn(String, &mut Window, &mut App)>;

struct PickerState {
    connection: SshConnection,
    current_path: String,
    requested_path: String,
    directories: Vec<RemoteDirectoryEntry>,
    has_valid_current: bool,
    loading: bool,
    error: Option<String>,
    request_id: u64,
    on_select: SelectCallback,
    _task: Option<Task<()>>,
}

impl PickerState {
    fn load(&mut self, requested: String, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.request_id = self.request_id.wrapping_add(1);
        let request_id = self.request_id;
        let connection_id = self.connection.id.clone();
        let connection = self.connection.clone();
        self.loading = true;
        self.error = None;
        self.requested_path = requested.clone();
        cx.notify();

        self._task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::remote_ssh::browse_directory(&connection, &requested) })
                .await;
            let _ = this.update(cx, |state, cx| {
                if state.request_id != request_id || state.connection.id != connection_id {
                    return;
                }
                state.loading = false;
                match result {
                    Ok(RemoteDirectoryListing {
                        canonical_path,
                        directories,
                    }) => {
                        state.current_path = canonical_path;
                        state.directories = directories;
                        state.has_valid_current = true;
                        state.error = None;
                    }
                    Err(error) => state.error = Some(error),
                }
                cx.notify();
            });
        }));
    }
}

pub fn open(
    connection: SshConnection,
    initial_path: String,
    on_select: impl Fn(String, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    if crate::overlay::contains(crate::overlay::key(kind::REMOTE_DIRECTORY_PICKER)) {
        return;
    }
    let requested = if initial_path.trim().is_empty() {
        "~".to_string()
    } else {
        initial_path
    };
    let initial_path = requested.clone();
    let state = cx.new(move |_| PickerState {
        connection,
        current_path: initial_path.clone(),
        requested_path: initial_path,
        directories: Vec::new(),
        has_valid_current: false,
        loading: false,
        error: None,
        request_id: 0,
        on_select: Rc::new(on_select),
        _task: None,
    });

    let dialog_state = state.clone();
    open_guarded(
        kind::REMOTE_DIRECTORY_PICKER,
        window,
        cx,
        move |dialog, window, cx| {
            dialog
                .title(dialog_title(
                    kind::REMOTE_DIRECTORY_PICKER,
                    t("remoteProject", "picker.title"),
                ))
                .w(ui::clamp_dialog_width(
                    px(560.0),
                    window.viewport_size(),
                ))
                .overlay_closable(false)
                .child(render_body(&dialog_state, cx))
        },
    );
    state.update(cx, |state, cx| state.load(requested, cx));
}

fn render_body(state: &Entity<PickerState>, cx: &mut App) -> AnyElement {
    let snapshot = {
        let state = state.read(cx);
        (
            state.current_path.clone(),
            state.requested_path.clone(),
            state.directories.clone(),
            state.has_valid_current,
            state.loading,
            state.error.clone(),
        )
    };
    let (current, requested, directories, has_valid_current, loading, error) = snapshot;

    let nav_button = |id: &'static str, label: &'static str, target: String| {
        let state = state.clone();
        ui::ghost_button(id, t("remoteProject", label))
            .opacity(if loading { 0.4 } else { 1.0 })
            .on_click(move |_: &ClickEvent, _window, cx| {
                if state.read(cx).loading {
                    return;
                }
                let target = target.clone();
                state.update(cx, |state, cx| state.load(target, cx));
            })
    };
    let up = crate::remote_ssh::parent_posix(&current).unwrap_or_else(|| "/".into());
    let mut list = div()
        .id("remote-directory-picker-list")
        .h(px(300.0))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .border_1()
        .border_color(ui::border_subtle())
        .rounded(px(4.0));
    if loading {
        list = list.child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(ui::text_muted())
                .child(t("remoteProject", "picker.loading")),
        );
    } else if directories.is_empty() {
        list = list.child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(ui::text_muted())
                .child(t("remoteProject", "picker.empty")),
        );
    } else {
        for entry in directories {
            let state = state.clone();
            let path = entry.path.clone();
            list = list.child(
                div()
                    .id(gpui::SharedString::from(format!(
                        "remote-picker-{}",
                        entry.path
                    )))
                    .px(px(10.0))
                    .py(px(7.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .hover(|el| el.bg(ui::bg_overlay()))
                    .on_click(move |_: &ClickEvent, _window, cx| {
                        if state.read(cx).loading {
                            return;
                        }
                        let path = path.clone();
                        state.update(cx, |state, cx| state.load(path, cx));
                    })
                    .child(if entry.is_symlink { "↪" } else { "▸" })
                    .child(entry.name),
            );
        }
    }

    div()
        .px(px(18.0))
        .pb(px(16.0))
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(nav_button("remote-picker-home", "picker.home", "~".into()))
                .child(nav_button("remote-picker-root", "picker.root", "/".into()))
                .child(nav_button("remote-picker-up", "picker.up", up)),
        )
        .child(
            div()
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(4.0))
                .bg(ui::bg_surface())
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_secondary())
                .child(current.clone()),
        )
        .child(list)
        .when_some(error, |el, error| {
            let state = state.clone();
            let retry_path = requested.clone();
            el.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::color_error())
                    .child(error)
                    .child(
                        ui::ghost_button(
                            "remote-picker-retry",
                            t("remoteProject", "picker.retry"),
                        )
                        .on_click(move |_: &ClickEvent, _window, cx| {
                            if state.read(cx).loading {
                                return;
                            }
                            let path = retry_path.clone();
                            state.update(cx, |state, cx| state.load(path, cx));
                        }),
                    ),
            )
        })
        .child(
            div()
                .flex()
                .items_center()
                .justify_end()
                .gap(px(8.0))
                .child(
                    ui::ghost_button(
                        "remote-picker-cancel",
                        t("remoteProject", "cancel"),
                    )
                    .on_click(|_: &ClickEvent, window, cx| {
                        close_guarded(kind::REMOTE_DIRECTORY_PICKER, window, cx);
                    }),
                )
                .child(
                    ui::primary_button(
                        "remote-picker-choose",
                        t("remoteProject", "picker.chooseCurrent"),
                    )
                    .when(loading || !has_valid_current, |el| el.opacity(0.4))
                    .on_click({
                        let state = state.clone();
                        move |_: &ClickEvent, window, cx| {
                            let (loading, has_valid_current, choose_path, on_select) = {
                                let state = state.read(cx);
                                (
                                    state.loading,
                                    state.has_valid_current,
                                    state.current_path.clone(),
                                    state.on_select.clone(),
                                )
                            };
                            if loading || !has_valid_current {
                                return;
                            }
                            if !close_guarded(kind::REMOTE_DIRECTORY_PICKER, window, cx) {
                                return;
                            }
                            on_select(choose_path, window, cx);
                        }
                    }),
                ),
        )
        .into_any_element()
}
