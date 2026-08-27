//! 设置面板的 system(托盘 / 启动)、editor(外部编辑器)、shortcuts(快捷键)
//! 与 about(版本)四页。
//!
//! about 页的版本比较与 GitHub 查询在 [`crate::update_check`],这里只有那颗
//! 「检查更新」按钮的后台任务与结果渲染。

use std::path::PathBuf;

use gpui::{
    AnyElement, App, Context, InteractiveElement, IntoElement, ParentElement, PathPromptOptions,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::input::Input;
use mt_config::{AppConfig, EditorConfig};

use crate::hotkeys;
use crate::i18n::{t, tr};
use crate::prompt::show_alert;
use crate::ui;
use crate::update_check::{compare_versions, fetch_latest_release, format_published_at};

use super::{SettingsView, tray_children_visible};
use super::widgets::{
    banner, dashed_button, form_card, number_row, page_root, radio_dot, section, shortcut_row,
    toggle_row,
};

#[derive(Debug, PartialEq, Eq)]
enum DownloadDirUpdate {
    Keep,
    Set(String),
}

/// 目录选择的纯状态归约：取消和无效结果都不得覆盖上一个有效配置。
fn reduce_download_dir_selection(
    selection: Option<Result<String, String>>,
) -> (DownloadDirUpdate, Option<String>) {
    match selection {
        None => (DownloadDirUpdate::Keep, None),
        Some(Ok(path)) => (DownloadDirUpdate::Set(path), None),
        Some(Err(error)) => (DownloadDirUpdate::Keep, Some(error)),
    }
}

impl SettingsView {
    // ── system 页 ──

    pub(super) fn render_system_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let (
            tray,
            click_focus,
            auto_resume,
            download_dir_custom,
            download_dir_path,
            download_dir_validation_path,
            download_dir_resolve_error,
        ) = {
            let config = self.store.read(cx).config();
            let (path, validation_path, error) = match config.resolved_download_dir() {
                Ok(path) => (
                    path.to_string_lossy().into_owned(),
                    Some(path),
                    None,
                ),
                Err(err) => (
                    "—".into(),
                    None,
                    Some(format!(
                        "{}: {err:#}",
                        t("settings", "system.downloadDirectoryInvalid")
                    )),
                ),
            };
            (
                config.tray_status_enabled.unwrap_or(true),
                config.tray_click_focus.unwrap_or(true),
                config.ai_auto_resume.unwrap_or(true),
                config.download_dir.is_some(),
                path,
                validation_path,
                error,
            )
        };
        if let Some(path) = download_dir_validation_path {
            let key = path.to_string_lossy().into_owned();
            if !self.download_dir_busy
                && self.download_dir_validation_key.as_deref() != Some(key.as_str())
            {
                self.start_download_dir_validation(path, key, cx);
            }
        }
        let download_dir_error = self
            .download_dir_error
            .clone()
            .or(download_dir_resolve_error);
        let download_dir_busy = self.download_dir_busy;

        page_root()
            .child(
                section("system.trayGroup")
                    .child(toggle_row(
                        "tray-enabled",
                        "system.trayStatusTitle",
                        "system.trayStatusDesc",
                        tray,
                        false,
                        |this, next, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.patch_config(|c| c.tray_status_enabled = Some(next), cx)
                            });
                        },
                        cx,
                    ))
                    // ⚠️ 总开关关掉时这两行**整个不渲染**(不是置灰)——
                    // 与 clipboard 页的处理不一样,原版就是这么写的
                    .when(tray_children_visible(tray), |el| {
                        el.child(toggle_row(
                            "tray-click-focus",
                            "system.trayClickFocusTitle",
                            "system.trayClickFocusDesc",
                            click_focus,
                            false,
                            |this, next, _window, cx| {
                                this.store.update(cx, |store, cx| {
                                    store.patch_config(|c| c.tray_click_focus = Some(next), cx)
                                });
                            },
                            cx,
                        ))
                        .child(number_row(
                            "system.trayMaxTitle",
                            "system.trayMaxDesc",
                            &self.num_tray_max,
                            false,
                        ))
                    }),
            )
            .child(section("system.startupGroup").child(toggle_row(
                "ai-auto-resume",
                "system.aiAutoResumeTitle",
                "system.aiAutoResumeDesc",
                auto_resume,
                false,
                |this, next, _window, cx| {
                    this.store.update(cx, |store, cx| {
                        store.patch_config(|c| c.ai_auto_resume = Some(next), cx)
                    });
                },
                cx,
            )))
            .child(
                section("system.downloadDirectoryGroup")
                    .child(
                        ui::setting_row(
                            t("settings", "system.downloadDirectoryTitle"),
                            Some(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.0))
                                    .child(ui::desc_text(t(
                                        "settings",
                                        "system.downloadDirectoryDesc",
                                    )))
                                    .child(
                                        div()
                                            .pt(px(4.0))
                                            .text_size(ui::font_px(11.0))
                                            .text_color(ui::text_secondary())
                                            .child(format!(
                                                "{}: {download_dir_path}",
                                                t(
                                                    "settings",
                                                    "system.downloadDirectoryCurrent"
                                                )
                                            )),
                                    )
                                    .into_any_element(),
                            ),
                            false,
                            div()
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    ui::primary_button(
                                        "download-dir-choose",
                                        if download_dir_busy {
                                            t("settings", "system.downloadDirectoryChoosing")
                                        } else {
                                            t("settings", "system.downloadDirectoryChoose")
                                        },
                                    )
                                    .when(download_dir_busy, |el| el.opacity(0.5))
                                    .when(!download_dir_busy, |el| {
                                        el.on_click(cx.listener(|this, _, _window, cx| {
                                            this.browse_download_dir(cx)
                                        }))
                                    }),
                                )
                                .when(download_dir_custom, |el| {
                                    el.child(
                                        ui::ghost_button(
                                            "download-dir-reset",
                                            t("settings", "system.downloadDirectoryReset"),
                                        )
                                        .when(download_dir_busy, |el| el.opacity(0.5))
                                        .when(!download_dir_busy, |el| {
                                            el.on_click(cx.listener(|this, _, _window, cx| {
                                                this.restore_default_download_dir(cx)
                                            }))
                                        }),
                                    )
                                }),
                        ),
                    )
                    .when_some(download_dir_error, |el, err| {
                        el.child(banner(err, ui::color_error()))
                    }),
            )
            .into_any_element()
    }

    fn start_download_dir_validation(
        &mut self,
        path: PathBuf,
        key: String,
        cx: &mut Context<Self>,
    ) {
        self.download_dir_busy = true;
        self.download_dir_error = None;
        self.download_dir_validation_key = Some(key.clone());
        self._download_dir_job = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    AppConfig::validate_download_dir(&path).map_err(|error| format!("{error:#}"))
                })
                .await;
            let _ = this.update(cx, |this: &mut SettingsView, cx| {
                let current_key = this
                    .store
                    .read(cx)
                    .config()
                    .resolved_download_dir()
                    .ok()
                    .map(|path| path.to_string_lossy().into_owned());
                if current_key.as_deref() != Some(key.as_str()) {
                    this.download_dir_busy = false;
                    this.download_dir_validation_key = None;
                    cx.notify();
                    return;
                }
                this.download_dir_busy = false;
                this.download_dir_error = result.err().map(|detail| {
                    format!(
                        "{}: {detail}",
                        t("settings", "system.downloadDirectoryInvalid")
                    )
                });
                cx.notify();
            });
        }));
    }

    fn browse_download_dir(&mut self, cx: &mut Context<Self>) {
        if self.download_dir_busy {
            return;
        }
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t("settings", "system.downloadDirectoryDialogTitle").into()),
        });
        self.download_dir_busy = true;
        self.download_dir_error = None;
        cx.notify();

        self._download_dir_job = Some(cx.spawn(async move |this, cx| {
            let paths = match paths.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) => {
                    let _ = this.update(cx, |this: &mut SettingsView, cx| {
                        this.finish_download_dir_selection(None, cx);
                    });
                    return;
                }
                Ok(Err(err)) => {
                    let detail = err.to_string();
                    let _ = this.update(cx, |this: &mut SettingsView, cx| {
                        this.finish_download_dir_selection(Some(Err(detail)), cx);
                    });
                    return;
                }
                Err(err) => {
                    let detail = err.to_string();
                    let _ = this.update(cx, |this: &mut SettingsView, cx| {
                        this.finish_download_dir_selection(Some(Err(detail)), cx);
                    });
                    return;
                }
            };
            let Some(path) = paths.into_iter().next() else {
                let _ = this.update(cx, |this: &mut SettingsView, cx| {
                    this.finish_download_dir_selection(None, cx);
                });
                return;
            };

            let result = cx
                .background_executor()
                .spawn(async move {
                    let Some(text) = path.to_str().map(str::to_owned) else {
                        return Err("selected path is not valid UTF-8".to_string());
                    };
                    AppConfig::validate_download_dir(&path)
                        .map_err(|err| format!("{err:#}"))?;
                    Ok(text)
                })
                .await;
            let _ = this.update(cx, |this: &mut SettingsView, cx| {
                this.finish_download_dir_selection(Some(result), cx);
            });
        }));
    }

    fn finish_download_dir_selection(
        &mut self,
        selection: Option<Result<String, String>>,
        cx: &mut Context<Self>,
    ) {
        let (update, error) = reduce_download_dir_selection(selection);
        self.download_dir_busy = false;
        self.download_dir_error = error.map(|detail| {
            format!(
                "{}: {detail}",
                t("settings", "system.downloadDirectoryInvalid")
            )
        });
        if let DownloadDirUpdate::Set(path) = update {
            self.download_dir_validation_key = Some(path.clone());
            self.store.update(cx, |store, cx| {
                store.patch_config(|config| config.download_dir = Some(path), cx)
            });
        }
        cx.notify();
    }

    fn restore_default_download_dir(&mut self, cx: &mut Context<Self>) {
        if self.download_dir_busy {
            return;
        }
        self.download_dir_busy = true;
        self.download_dir_error = None;
        cx.notify();
        self._download_dir_job = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let path = AppConfig::system_download_dir().map_err(|error| format!("{error:#}"))?;
                    std::fs::create_dir_all(&path).map_err(|error| {
                        format!("无法创建下载目录 {}: {error}", path.display())
                    })?;
                    AppConfig::validate_download_dir(&path)
                        .map_err(|error| format!("{error:#}"))?;
                    Ok::<String, String>(path.to_string_lossy().into_owned())
                })
                .await;
            let _ = this.update(cx, |this: &mut SettingsView, cx| {
                this.download_dir_busy = false;
                match result {
                    Ok(path) => {
                        this.download_dir_error = None;
                        this.download_dir_validation_key = Some(path);
                        this.store.update(cx, |store, cx| {
                            store.patch_config(|config| config.download_dir = None, cx)
                        });
                    }
                    Err(detail) => {
                        this.download_dir_error = Some(format!(
                            "{}: {detail}",
                            t("settings", "system.downloadDirectoryInvalid")
                        ));
                    }
                }
                cx.notify();
            });
        }));
    }

    // ── editor 页 ──

    pub(super) fn render_editor_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let config = self.store.read(cx).config();
        let editors = config.editors.clone();
        let default_editor = config.default_editor.clone().unwrap_or_default();
        let editing = self.editor_editing;

        let mut rows = div().flex().flex_col().gap(px(8.0));
        for (idx, editor) in editors.iter().enumerate() {
            if editing == Some(Some(idx)) {
                rows = rows.child(self.render_editor_form(cx));
                continue;
            }
            let is_default = editor.name == default_editor;
            rows = rows.child(
                ui::settings_card()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        radio_dot(format!("editor-default-{idx}"), is_default).on_click(
                            cx.listener(move |this, _, _window, cx| {
                                let name = this.store.read(cx).config().editors[idx].name.clone();
                                this.store.update(cx, |store, cx| {
                                    store.patch_config(|c| c.default_editor = Some(name), cx)
                                });
                            }),
                        ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(ui::text_primary())
                                    .child(editor.name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(ui::font_px(11.0))
                                    .text_color(ui::text_muted())
                                    .child(editor.command.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(4.0))
                            .child(
                                ui::ghost_button(
                                    SharedString::from(format!("editor-edit-{idx}")),
                                    t("settings", "common.edit"),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let editor =
                                        this.store.read(cx).config().editors.get(idx).cloned();
                                    this.editor_editing = Some(Some(idx));
                                    this.fill_editor_form(editor.as_ref(), window, cx);
                                })),
                            )
                            .child(
                                ui::danger_button(
                                    SharedString::from(format!("editor-del-{idx}")),
                                    t("settings", "common.delete"),
                                )
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.delete_editor(idx, cx);
                                })),
                            ),
                    ),
            );
        }
        if editing == Some(None) {
            rows = rows.child(self.render_editor_form(cx));
        }

        page_root()
            .child(
                section("editor.externalEditor")
                    .child(rows)
                    .child(
                        dashed_button("editor-add", t("settings", "editor.addEditor")).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.editor_editing = Some(None);
                                this.fill_editor_form(None, window, cx);
                            }),
                        ),
                    )
                    .child(ui::hint(t("settings", "editor.editorDefaultHint"))),
            )
            .into_any_element()
    }

    fn fill_editor_form(
        &mut self,
        editor: Option<&EditorConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (name, command) = match editor {
            Some(e) => (e.name.clone(), e.command.clone()),
            None => (String::new(), String::new()),
        };
        self.editor_name.update(cx, |s, cx| {
            s.set_placeholder(t("settings", "editor.newEditorNamePlaceholder"), window, cx);
            s.set_value(name, window, cx);
        });
        self.editor_command.update(cx, |s, cx| {
            s.set_placeholder(
                t("settings", "editor.newEditorCommandPlaceholder"),
                window,
                cx,
            );
            s.set_value(command, window, cx);
        });
        cx.notify();
    }

    fn render_editor_form(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let adding = self.editor_editing == Some(None);
        form_card(adding)
            .child(Input::new(&self.editor_name))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().flex_1().child(Input::new(&self.editor_command)))
                    // 「...」浏览按钮(shell 列表没有这一颗)
                    .child(
                        ui::ghost_button("editor-browse", "...").on_click(cx.listener(
                            |this, _, window, cx| this.browse_editor_path(window, cx),
                        )),
                    )
                    .child(
                        ui::primary_button(
                            "editor-save",
                            if adding {
                                t("settings", "common.add")
                            } else {
                                t("settings", "common.save")
                            },
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.save_editor(window, cx))),
                    )
                    .child(
                        ui::ghost_button("editor-cancel", t("settings", "common.cancel")).on_click(
                            cx.listener(|this, _, _window, cx| {
                                this.editor_editing = None;
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn browse_editor_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Windows 上原版带 `.exe` 过滤;gpui 的选择框没有过滤能力(见 §7 坑 2)
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t("settings", "editor.browseDialogTitle").into()),
        });
        // `spawn_in` 而不是 `spawn`:回填输入框的 `set_value` 要 `&mut Window`,
        // 只有 `AsyncWindowContext` 给得出来
        self._job = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let text = path.to_string_lossy().to_string();
            let _ = this.update_in(cx, |this: &mut SettingsView, window, cx| {
                this.editor_command
                    .update(cx, |state, cx| state.set_value(text, window, cx));
            });
        }));
    }

    fn save_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.editor_name.read(cx).value().trim().to_string();
        let command = self.editor_command.read(cx).value().trim().to_string();
        if name.is_empty() || command.is_empty() {
            return;
        }
        let editing = self.editor_editing;
        let editors = self.store.read(cx).config().editors.clone();
        // **重名校验**:shell 列表没有这一条,编辑器列表有(原版 :1432/:1462)
        let clash = editors.iter().enumerate().any(|(i, e)| {
            e.name == name && editing.flatten() != Some(i)
        });
        if clash {
            show_alert(
                t("settings", "menu.editor"),
                tr!("settings", "editor.editorExistsAlert", name = name),
                window,
                cx,
            );
            return;
        }

        let mut editors = editors;
        let mut default_editor = self.store.read(cx).config().default_editor.clone();
        match editing {
            Some(Some(idx)) if idx < editors.len() => {
                let was_default = default_editor.as_deref() == Some(editors[idx].name.as_str());
                editors[idx] = EditorConfig {
                    name: name.clone(),
                    command,
                };
                if was_default {
                    default_editor = Some(name);
                }
            }
            _ => {
                editors.push(EditorConfig {
                    name: name.clone(),
                    command,
                });
                if default_editor.is_none() {
                    default_editor = Some(name);
                }
            }
        }
        self.store.update(cx, |store, cx| {
            store.patch_config(
                move |c| {
                    c.editors = editors;
                    c.default_editor = default_editor;
                },
                cx,
            )
        });
        self.editor_editing = None;
        cx.notify();
    }

    fn delete_editor(&mut self, idx: usize, cx: &mut Context<Self>) {
        let mut editors = self.store.read(cx).config().editors.clone();
        if idx >= editors.len() {
            return;
        }
        editors.remove(idx);
        let current = self.store.read(cx).config().default_editor.clone();
        // 删掉的正是默认项时落到剩下的第一个;**空列表写 `None` 而不是空串**
        let default_editor = match current {
            Some(name) if editors.iter().any(|e| e.name == name) => Some(name),
            _ => editors.first().map(|e| e.name.clone()),
        };
        self.store.update(cx, |store, cx| {
            store.patch_config(
                move |c| {
                    c.editors = editors;
                    c.default_editor = default_editor;
                },
                cx,
            )
        });
        self.editor_editing = None;
        cx.notify();
    }

    // ── shortcuts 页 ──

    pub(super) fn render_shortcuts_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let smart = self.store.read(cx).config().smart_copy_paste;
        let mut root = page_root();

        for (group_key, items) in hotkeys::groups() {
            let mut rows = div().flex().flex_col().gap(px(4.0));
            for def in items {
                rows = rows.child(shortcut_row(
                    t("settings", def.desc_key),
                    hotkeys::combo_label(&def.combo),
                ));
            }
            // 智能 Ctrl+C/V 开启时才存在,附在「复制粘贴」组末尾
            if smart && group_key == "shortcuts.clipboard" {
                let modifier = hotkeys::combo_label(&hotkeys::Combo {
                    modifier: true,
                    shift: false,
                    alt: false,
                    key: "C",
                });
                let paste = hotkeys::combo_label(&hotkeys::Combo {
                    modifier: true,
                    shift: false,
                    alt: false,
                    key: "V",
                });
                rows = rows
                    .child(shortcut_row(t("settings", "shortcuts.copyDesc"), modifier))
                    .child(shortcut_row(
                        t("settings", "shortcuts.pasteToTerminal"),
                        paste,
                    ));
            }
            root = root.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(ui::settings_section_title(t("settings", group_key)))
                    .child(rows),
            );
        }

        root.child(ui::hint(t("settings", "shortcuts.footer")))
            .into_any_element()
    }

    // ── about 页 ──

    fn check_update(&mut self, cx: &mut Context<Self>) {
        if self.checking {
            return;
        }
        self.checking = true;
        self.update_error = None;
        self.latest = None;
        cx.notify();
        self._job = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { fetch_latest_release() })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.checking = false;
                match result {
                    Ok(release) => this.latest = Some(release),
                    Err(err) => this.update_error = Some(err),
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn render_about_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let current = env!("CARGO_PKG_VERSION");
        let latest = self.latest.clone();
        let has_update = latest
            .as_ref()
            .is_some_and(|r| compare_versions(&r.version, current).is_gt());

        page_root()
            .child(ui::settings_section_title(t("settings", "about.versionInfo")))
            .child(
                ui::settings_card()
                    .px(px(16.0))
                    .py(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::text_secondary())
                            .child(t("settings", "about.currentVersion")),
                    )
                    .child(
                        div()
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::accent())
                            .child(format!("v{current}")),
                    ),
            )
            .child(
                div()
                    .id("about-check")
                    .w_full()
                    .flex()
                    .justify_center()
                    .py(px(10.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(ui::border_default())
                    .text_size(ui::font_px(13.0))
                    .text_color(ui::text_secondary())
                    .when(self.checking, |el| el.opacity(0.5))
                    .when(!self.checking, |el| {
                        el.cursor_pointer()
                            .hover(|el| el.border_color(ui::accent()).text_color(ui::accent()))
                            .on_click(cx.listener(|this, _, _window, cx| this.check_update(cx)))
                    })
                    .child(if self.checking {
                        t("settings", "about.checking")
                    } else {
                        t("settings", "about.checkUpdate")
                    }),
            )
            .when_some(self.update_error.clone(), |el, err| {
                el.child(banner(err, ui::color_error()))
            })
            .when_some(latest, |el, release| {
                el.child(
                    ui::settings_card()
                        .px(px(16.0))
                        .py(px(12.0))
                        .when(has_update, |el| el.border_color(ui::accent()))
                        .child(if has_update {
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(12.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .child(
                                            div()
                                                .text_size(ui::font_px(13.0))
                                                .text_color(ui::text_primary())
                                                .child(t("settings", "about.newVersionFound")),
                                        )
                                        .child(
                                            div()
                                                .text_size(ui::font_px(13.0))
                                                .text_color(ui::accent())
                                                .child(release.version.clone()),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_size(ui::font_px(11.0))
                                        .text_color(ui::text_muted())
                                        .child(tr!(
                                            "settings",
                                            "about.publishedAt",
                                            date = format_published_at(&release.published_at)
                                        )),
                                )
                                .child(
                                    ui::primary_button(
                                        "about-download",
                                        t("settings", "about.downloadFromGitHub"),
                                    )
                                    .w_full()
                                    .py(px(8.0))
                                    .on_click(move |_, _window, cx: &mut App| {
                                        cx.open_url(&release.url);
                                    }),
                                )
                        } else {
                            div()
                                .text_size(ui::font_px(13.0))
                                .text_color(ui::text_secondary())
                                .child(t("settings", "about.upToDate"))
                        }),
                )
            })
            .child(ui::hint(t("settings", "about.footer")))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_dir_selection_only_updates_for_valid_choice() {
        assert_eq!(
            reduce_download_dir_selection(None),
            (DownloadDirUpdate::Keep, None)
        );
        assert_eq!(
            reduce_download_dir_selection(Some(Err("not writable".into()))),
            (
                DownloadDirUpdate::Keep,
                Some("not writable".to_string())
            )
        );
        assert_eq!(
            reduce_download_dir_selection(Some(Ok("/downloads".into()))),
            (DownloadDirUpdate::Set("/downloads".into()), None)
        );
    }
}
