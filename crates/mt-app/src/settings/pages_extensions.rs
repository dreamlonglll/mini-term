//! 设置面板的 extensions(按需扩展)页:远程 Git helper(阶段 A),阶段 B / C 的 SSH 工具与
//! 语法高亮扩展包进了清单就自动多出一行。
//!
//! 状态表、下载与 toast 在 [`crate::extensions`];本页只画行、发起动作。阻塞的动作(删除 /
//! 导入 / 打开目录)一律丢后台,结果就地写进页内的提示条 —— 设置是弹窗,toast 在遮罩下看不见。

use gpui::{
    AnyElement, Context, IntoElement, ParentElement, PathPromptOptions, SharedString,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};
use gpui_component::input::Input;
use mt_extensions::{ComponentId, ComponentStatus, ManifestState};

use crate::extensions::{self, ComponentRow, JobState, Origin};
use crate::i18n::{t, tr};
use crate::ui;

use super::SettingsView;
use super::widgets::{banner, page_root, section};

/// 页内提示条:导入结果 / 删除失败 / 打开目录失败。
#[derive(Clone, Debug)]
pub(super) struct ExtensionsNotice {
    pub text: String,
    pub error: bool,
}

impl SettingsView {
    pub(super) fn render_extensions_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // 每次打开设置后第一次进这一页:后台重 stat 一遍(用户可能在外面动过扩展目录)
        if !self.ext_refreshed {
            self.ext_refreshed = true;
            cx.background_executor()
                .spawn(async { extensions::refresh_statuses() })
                .detach();
        }

        let mut components = section("extensions.componentsGroup")
            .child(ui::desc_text(t("settings", "extensions.intro")));
        let mut loaded = false;
        match extensions::manifest_state() {
            ManifestState::Missing => {
                components = components.child(banner(
                    t("settings", "extensions.noManifest").to_string(),
                    ui::color_warning(),
                ));
            }
            ManifestState::TooNew { schema } => {
                components = components.child(banner(
                    tr!("settings", "extensions.manifestTooNew", schema = schema),
                    ui::color_error(),
                ));
            }
            ManifestState::Invalid(detail) => {
                components = components.child(banner(
                    tr!("settings", "extensions.manifestInvalid", detail = detail),
                    ui::color_error(),
                ));
            }
            ManifestState::Loaded(manifest) => {
                loaded = true;
                let rows = extensions::rows();
                let downloadable: Vec<ComponentId> = rows
                    .iter()
                    .filter(|row| row.status == ComponentStatus::Missing && row.job.is_none())
                    .map(|row| row.component.id)
                    .collect();
                for row in &rows {
                    components = components.child(self.render_extension_row(row, cx));
                }
                let busy = self.ext_busy;
                components = components.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(8.0))
                        .child(
                            ui::primary_button(
                                "ext-download-all",
                                t("settings", "extensions.downloadAll"),
                            )
                            .when(downloadable.is_empty(), |el| el.opacity(0.5))
                            .when(!downloadable.is_empty(), |el| {
                                el.on_click(cx.listener(move |this, _, _window, cx| {
                                    this.ext_notice = None;
                                    for id in &downloadable {
                                        drop(extensions::ensure(*id, Origin::User));
                                    }
                                    cx.notify();
                                }))
                            }),
                        )
                        .child(
                            ui::ghost_button(
                                "ext-import",
                                t("settings", "extensions.importOffline"),
                            )
                            .when(busy, |el| el.opacity(0.5))
                            .when(!busy, |el| {
                                el.on_click(
                                    cx.listener(|this, _, _window, cx| this.import_extensions(cx)),
                                )
                            }),
                        )
                        .child(
                            ui::ghost_button("ext-open-dir", t("settings", "extensions.openDir"))
                                .on_click(
                                    cx.listener(|this, _, _window, cx| {
                                        this.open_extensions_dir(cx)
                                    }),
                                ),
                        ),
                );
                if let Some(bundle) = &manifest.offline_bundle {
                    components = components.child(ui::hint(tr!(
                        "settings",
                        "extensions.offlineBundleHint",
                        name = bundle
                    )));
                }
            }
        }
        if let Some(notice) = self.ext_notice.clone() {
            components = components.child(banner(
                notice.text,
                if notice.error {
                    ui::color_error()
                } else {
                    ui::color_success()
                },
            ));
        }

        page_root()
            .child(components)
            .when(loaded, |el| el.child(self.render_extensions_source(cx)))
            .into_any_element()
    }

    fn render_extension_row(&self, row: &ComponentRow, cx: &mut Context<Self>) -> AnyElement {
        let id = row.component.id;
        let (status_text, status_color) = match (&row.job, &row.failure, &row.status) {
            (Some(JobState::Verifying), _, _) => (
                t("settings", "extensions.statusVerifying").to_string(),
                ui::color_info(),
            ),
            (Some(job), _, _) => (
                tr!(
                    "settings",
                    "extensions.statusDownloading",
                    percent = job.percent()
                ),
                ui::color_info(),
            ),
            (None, _, ComponentStatus::Installed(_)) => (
                t("settings", "extensions.statusInstalled").to_string(),
                ui::color_success(),
            ),
            (None, _, ComponentStatus::Bundled) => (
                t("settings", "extensions.statusBundled").to_string(),
                ui::color_success(),
            ),
            (None, Some(error), ComponentStatus::Missing) => (
                tr!(
                    "settings",
                    "extensions.statusFailed",
                    reason = extensions::describe(error)
                ),
                ui::color_error(),
            ),
            (None, None, ComponentStatus::Missing) => (
                t("settings", "extensions.statusMissing").to_string(),
                ui::text_muted(),
            ),
        };
        let running = row.job.is_some();
        let installed = matches!(row.status, ComponentStatus::Installed(_));
        let missing = row.status == ComponentStatus::Missing;
        let busy = self.ext_busy;
        let download_label = if row.failure.is_some() {
            t("settings", "extensions.retry")
        } else {
            t("settings", "extensions.download")
        };

        ui::settings_card()
            .flex()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::text_primary())
                            .child(extensions::component_name(id)),
                    )
                    .child(ui::desc_text(extensions::component_description(id)))
                    .child(ui::desc_text(tr!(
                        "settings",
                        "extensions.size",
                        download = extensions::format_size(row.component.size),
                        installed = extensions::format_size(row.component.unpacked_size())
                    )))
                    .child(
                        div()
                            .pt(px(2.0))
                            .text_size(ui::font_px(11.0))
                            .text_color(status_color)
                            .children(
                                status_text
                                    .split('\n')
                                    .map(|line| div().child(line.to_string()))
                                    .collect::<Vec<_>>(),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .gap(px(6.0))
                    .when(missing && !running, |el| {
                        el.child(
                            ui::primary_button(
                                SharedString::from(format!("ext-download-{id}")),
                                download_label,
                            )
                            .on_click(cx.listener(
                                move |this, _, _window, cx| {
                                    this.ext_notice = None;
                                    drop(extensions::ensure(id, Origin::User));
                                    cx.notify();
                                },
                            )),
                        )
                    })
                    .when(installed && !running, |el| {
                        el.child(
                            ui::danger_button(
                                SharedString::from(format!("ext-delete-{id}")),
                                t("settings", "extensions.delete"),
                            )
                            .when(busy, |el| el.opacity(0.5))
                            .when(!busy, |el| {
                                el.on_click(cx.listener(move |this, _, _window, cx| {
                                    this.delete_extension(id, cx)
                                }))
                            }),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_extensions_source(&self, cx: &mut Context<Self>) -> AnyElement {
        let draft = self.txt_extensions_url.read(cx).value().to_string();
        let invalid = !draft.trim().is_empty() && !extensions::is_valid_template(&draft);
        let custom = self
            .store
            .read(cx)
            .config()
            .extensions_download_url
            .is_some();
        section("extensions.sourceGroup")
            .child(
                ui::settings_card()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::text_primary())
                            .child(t("settings", "extensions.sourceTitle")),
                    )
                    .child(ui::desc_text(t("settings", "extensions.sourceDesc")))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Input::new(&self.txt_extensions_url)),
                            )
                            .when(custom || !draft.trim().is_empty(), |el| {
                                el.child(
                                    ui::ghost_button(
                                        "ext-source-reset",
                                        t("settings", "extensions.sourceReset"),
                                    )
                                    .flex_none()
                                    .on_click(cx.listener(
                                        |this, _, window, cx| {
                                            this.txt_extensions_url.update(cx, |state, cx| {
                                                state.set_value("", window, cx)
                                            });
                                            this.store.update(cx, |store, cx| {
                                                store.set_extensions_download_url(None, cx)
                                            });
                                            cx.notify();
                                        },
                                    )),
                                )
                            }),
                    ),
            )
            .when(invalid, |el| {
                el.child(banner(
                    t("settings", "extensions.sourceInvalid").to_string(),
                    ui::color_error(),
                ))
            })
            .into_any_element()
    }

    fn delete_extension(&mut self, id: ComponentId, cx: &mut Context<Self>) {
        if self.ext_busy {
            return;
        }
        self.ext_busy = true;
        self.ext_notice = None;
        cx.notify();
        self._ext_job = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { extensions::remove(id) })
                .await;
            let _ = this.update(cx, |this: &mut SettingsView, cx| {
                this.ext_busy = false;
                if let Err(error) = result {
                    this.ext_notice = Some(ExtensionsNotice {
                        text: format!(
                            "{}: {}",
                            extensions::component_name(id),
                            extensions::describe(&error)
                        ),
                        error: true,
                    });
                }
                cx.notify();
            });
        }));
    }

    fn import_extensions(&mut self, cx: &mut Context<Self>) {
        // gpui 的选择框没有扩展名过滤,选错文件由导入自己报错
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t("settings", "extensions.importDialogTitle").into()),
        });
        self._ext_job = Some(cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this: &mut SettingsView, cx| {
                this.ext_busy = true;
                this.ext_notice = None;
                cx.notify();
            });
            let result = cx
                .background_executor()
                .spawn(async move { extensions::import_offline(&path) })
                .await;
            let _ = this.update(cx, |this: &mut SettingsView, cx| {
                this.ext_busy = false;
                this.ext_notice = Some(import_notice(result));
                cx.notify();
            });
        }));
    }

    fn open_extensions_dir(&mut self, cx: &mut Context<Self>) {
        let Some(root) = extensions::extensions()
            .root()
            .map(|root| root.to_path_buf())
        else {
            self.ext_notice = Some(ExtensionsNotice {
                text: t("settings", "extensions.errNoRoot").to_string(),
                error: true,
            });
            cx.notify();
            return;
        };
        // 建目录 + 起文件管理器都是阻塞 IO,丢后台;失败写进页内提示条
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    std::fs::create_dir_all(&root)?;
                    crate::fs_ops::reveal_in_file_manager(&root)
                })
                .await;
            if let Err(error) = result {
                tracing::warn!("[settings] 打开扩展目录失败: {error}");
                let _ = this.update(cx, |this: &mut SettingsView, cx| {
                    this.ext_notice = Some(ExtensionsNotice {
                        text: format!("{}: {error}", t("settings", "extensions.errIo")),
                        error: true,
                    });
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

/// 导入结果 → 页内提示条(纯函数,可测)。
fn import_notice(
    result: Result<mt_extensions::ImportReport, mt_extensions::ExtensionError>,
) -> ExtensionsNotice {
    let report = match result {
        Ok(report) => report,
        Err(error) => {
            return ExtensionsNotice {
                text: extensions::describe_extension_error(&error),
                error: true,
            };
        }
    };
    let mut lines = Vec::new();
    if report.installed.is_empty() && report.failed.is_empty() {
        lines.push(t("settings", "extensions.importNothing").to_string());
    }
    if !report.installed.is_empty() {
        lines.push(tr!(
            "settings",
            "extensions.importDone",
            count = report.installed.len()
        ));
    }
    if report.skipped > 0 {
        lines.push(tr!(
            "settings",
            "extensions.importSkipped",
            count = report.skipped
        ));
    }
    for (id, error) in &report.failed {
        lines.push(tr!(
            "settings",
            "extensions.importFailedItem",
            name = extensions::component_name(*id),
            reason = extensions::describe_extension_error(error)
        ));
    }
    ExtensionsNotice {
        text: lines.join("\n"),
        error: !report.failed.is_empty() || report.installed.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mt_extensions::{ExtensionError, ImportReport};

    #[test]
    fn 导入结果的提示条() {
        let ok = import_notice(Ok(ImportReport {
            installed: vec![ComponentId::GitHelperLinuxX86_64],
            skipped: 2,
            failed: vec![],
        }));
        assert!(!ok.error);
        assert_eq!(ok.text.lines().count(), 2);

        let nothing = import_notice(Ok(ImportReport::default()));
        assert!(nothing.error, "一个都没装上要标红");
        assert_eq!(nothing.text.lines().count(), 1);

        let partial = import_notice(Ok(ImportReport {
            installed: vec![ComponentId::GitHelperLinuxX86_64],
            skipped: 0,
            failed: vec![(
                ComponentId::GitHelperLinuxAarch64,
                ExtensionError::Integrity("sha256 mismatch".into()),
            )],
        }));
        assert!(partial.error);
        assert!(partial.text.contains("sha256 mismatch"));

        let failed = import_notice(Err(ExtensionError::NoManifest));
        assert!(failed.error);
    }
}
