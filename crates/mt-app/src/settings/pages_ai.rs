//! 设置面板的 ai-notification(完成提醒)与 ai-hook(hook 注册)两页。
//!
//! hook 页的三件事都要写用户主目录 / 起端口,一律丢后台再回主线程改状态:
//! 注册现状扫描(`refresh_hook_state`)、注册/卸载(`run_hook_action`)、
//! 开关 hook server(`toggle_hook_server`)。默认勾选的判据是纯函数
//! [`super::default_selected_agents`]。

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, PathPromptOptions,
    SharedString, StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};
use mt_ai::hook_registry::{self, HookAgent};

use crate::i18n::{t, tr};
use crate::ui;

use super::{SettingsView, default_selected_agents};
use super::widgets::{banner, page_root, section, snippet_file_name, snippet_lines, toggle_row};

impl SettingsView {
    // ── ai-notification 页 ──

    pub(super) fn render_notification_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let config = self.store.read(cx).config();
        let popup = config.ai_completion_popup;
        let flash = config.ai_completion_taskbar_flash;
        let sound = config.ai_completion_sound;
        let sound_path = config.ai_completion_sound_path.clone();
        let attention = config.ai_attention_notify;

        let path_label = sound_path
            .clone()
            .unwrap_or_else(|| t("settings", "aiNotification.defaultSound").to_string());

        let mut buttons = div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .child(
                ui::ghost_button("sound-preview", t("settings", "aiNotification.preview"))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        let path = this.store.read(cx).config().ai_completion_sound_path.clone();
                        crate::notify::play_sound(path.as_deref());
                    })),
            )
            .child(
                ui::ghost_button("sound-choose", t("settings", "aiNotification.chooseFile"))
                    .on_click(cx.listener(|this, _, _window, cx| this.choose_sound_file(cx))),
            );
        // 「清除」仅当已有自定义路径时才渲染(原版 :1319)
        if sound_path.is_some() {
            buttons = buttons.child(
                ui::danger_button("sound-clear", t("settings", "aiNotification.clear")).on_click(
                    cx.listener(|this, _, _window, cx| {
                        this.sound_warning = false;
                        this.store.update(cx, |store, cx| {
                            store.patch_config(|c| c.ai_completion_sound_path = None, cx)
                        });
                    }),
                ),
            );
        }

        page_root()
            .child(
                section("aiNotification.method")
                    .child(toggle_row(
                        "notify-popup",
                        "aiNotification.popup",
                        "aiNotification.popupDesc",
                        popup,
                        false,
                        |this, next, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.patch_config(|c| c.ai_completion_popup = next, cx)
                            });
                        },
                        cx,
                    ))
                    .child(toggle_row(
                        "notify-flash",
                        "aiNotification.taskbarFlash",
                        "aiNotification.taskbarFlashDesc",
                        flash,
                        false,
                        |this, next, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.patch_config(|c| c.ai_completion_taskbar_flash = next, cx)
                            });
                        },
                        cx,
                    ))
                    .child(toggle_row(
                        "notify-sound",
                        "aiNotification.sound",
                        "aiNotification.soundDesc",
                        sound,
                        false,
                        |this, next, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.patch_config(|c| c.ai_completion_sound = next, cx)
                            });
                        },
                        cx,
                    ))
                    // 提示音总开关关掉时整行置灰
                    .child(ui::setting_row(
                        t("settings", "aiNotification.customSound"),
                        Some(ui::desc_text(path_label).truncate().into_any_element()),
                        !sound,
                        buttons,
                    ))
                    // GPUI 侧的提示音只认 .wav(`notify.rs:234-267`),其余静默回落
                    // 系统提示音 —— 选到别的格式时把这条说出来
                    .when(self.sound_warning, |el| {
                        el.child(banner(
                            t("settings", "aiNotification.wavOnly").to_string(),
                            ui::color_warning(),
                        ))
                    })
                    .child(ui::hint(t("settings", "aiNotification.footer"))),
            )
            .child(
                section("aiNotification.trigger")
                    .child(toggle_row(
                        "notify-attention",
                        "aiNotification.attention",
                        "aiNotification.attentionDesc",
                        attention,
                        false,
                        |this, next, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.patch_config(|c| c.ai_attention_notify = next, cx)
                            });
                        },
                        cx,
                    ))
                    .child(ui::hint(t("settings", "aiNotification.attentionFooter"))),
            )
            .into_any_element()
    }

    fn choose_sound_file(&mut self, cx: &mut Context<Self>) {
        // gpui 的选择框没有扩展名过滤(`PathPromptOptions` 只有四个字段),
        // 原版那 6 种格式的 filter 做不到 —— 选完自己校验
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t("settings", "aiNotification.soundDialogTitle").into()),
        });
        self._job = Some(cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let is_wav = path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("wav"))
                .unwrap_or(false);
            let text = path.to_string_lossy().to_string();
            let _ = this.update(cx, |this: &mut SettingsView, cx| {
                this.sound_warning = !is_wav;
                this.store.update(cx, |store, cx| {
                    store.patch_config(|c| c.ai_completion_sound_path = Some(text), cx)
                });
                cx.notify();
            });
        }));
    }

    // ── ai-hook 页 ──

    pub(super) fn refresh_hook_state(&mut self, cx: &mut Context<Self>) {
        let status = self.store.read(cx).ai().hook_status();
        self.hook_running = status.running;
        self.hook_port = status.port;
        // 注册现状要读三家的配置文件 —— 丢后台,回主线程再改状态
        self._job = Some(cx.spawn(async move |this, cx| {
            let list = cx
                .background_executor()
                .spawn(async { hook_registry::get_ai_hook_registrations() })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.selected_agents.is_none() {
                    this.selected_agents = Some(default_selected_agents(&list));
                }
                this.registrations = list;
                cx.notify();
            });
        }));
    }

    /// 当前勾选的注入目标。
    fn agents(&self) -> Vec<String> {
        self.selected_agents.clone().unwrap_or_default()
    }

    fn run_hook_action(&mut self, register: bool, cx: &mut Context<Self>) {
        let agents: Vec<HookAgent> = self
            .agents()
            .iter()
            .filter_map(|a| match a.as_str() {
                "claude" => Some(HookAgent::Claude),
                "codex" => Some(HookAgent::Codex),
                "grok" => Some(HookAgent::Grok),
                "omp" => Some(HookAgent::Omp),
                _ => None,
            })
            .collect();
        // 空选择由按钮 disabled 挡住;真走到这里也不能放行 ——
        // 后端对空列表会回落成「三家全上」(hook_registry::resolve_targets)
        if agents.is_empty() {
            return;
        }
        self.hook_busy = true;
        self.hook_result.clear();
        cx.notify();
        // 注册要往用户主目录写配置文件(还会复制 hook 二进制),必须丢后台
        self._job = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if register {
                        hook_registry::register_ai_hooks(Some(agents))
                    } else {
                        hook_registry::unregister_ai_hooks(Some(agents))
                    }
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.hook_busy = false;
                this.hook_result = match result {
                    Ok(msg) => msg,
                    Err(err) => err,
                };
                // 跑完刷一次现状(徽章要变)
                this.refresh_hook_state(cx);
                cx.notify();
            });
        }));
    }

    fn toggle_hook_server(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.hook_busy {
            return;
        }
        self.hook_busy = true;
        cx.notify();
        let ai = self.store.read(cx).ai();
        let store = self.store.clone();
        self._job = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ai.set_hook_enabled(enabled) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.hook_busy = false;
                match result {
                    // **成功了才写配置**(原版 handleToggleHook 的同一顺序):
                    // 端口被占时配置不该记成「已开」
                    Ok(()) => store.update(cx, |store, cx| {
                        store.patch_config(|c| c.hook_enabled = enabled, cx)
                    }),
                    Err(err) => this.hook_result = err,
                }
                this.refresh_hook_state(cx);
                cx.notify();
            });
        }));
    }

    fn toggle_snippet(&mut self, cx: &mut Context<Self>) {
        if self.show_snippet {
            self.show_snippet = false;
            cx.notify();
            return;
        }
        self._job = Some(cx.spawn(async move |this, cx| {
            let data = cx
                .background_executor()
                .spawn(async { hook_registry::get_hook_config_snippet() })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.snippet = data.ok();
                this.show_snippet = true;
                cx.notify();
            });
        }));
    }

    pub(super) fn render_hook_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let enabled = self.store.read(cx).config().hook_enabled;
        let agents = self.agents();
        let busy = self.hook_busy;

        // 注入目标列表
        let mut targets = ui::settings_card().p_0().flex().flex_col().child(
            div()
                .px(px(12.0))
                .pt(px(10.0))
                .pb(px(4.0))
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(t("settings", "aiHook.targetsLabel")),
        );
        for reg in &self.registrations {
            let checked = agents.contains(&reg.agent);
            let (badge, color) = if reg.registered == 0 {
                (
                    t("settings", "aiHook.stateAbsent").to_string(),
                    ui::text_muted(),
                )
            } else if reg.registered < reg.total {
                (
                    tr!(
                        "settings",
                        "aiHook.stateStale",
                        n = reg.registered,
                        total = reg.total
                    ),
                    ui::color_warning(),
                )
            } else {
                (
                    tr!("settings", "aiHook.stateReady", n = reg.total),
                    ui::color_success(),
                )
            };
            let agent = reg.agent.clone();
            targets = targets.child(
                div()
                    .id(SharedString::from(format!("hook-target-{}", reg.agent)))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .cursor_pointer()
                    .hover(|el| el.bg(ui::border_subtle()))
                    .child(ui::checkbox(
                        SharedString::from(format!("hook-check-{}", reg.agent)),
                        checked,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(ui::text_primary())
                                    .child(reg.label.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(ui::font_px(11.0))
                                    .text_color(ui::text_muted())
                                    .child(reg.file.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(ui::font_px(11.0))
                            .text_color(color)
                            .child(badge),
                    )
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        let mut list = this.agents();
                        match list.iter().position(|a| *a == agent) {
                            Some(idx) => {
                                list.remove(idx);
                            }
                            None => list.push(agent.clone()),
                        }
                        this.selected_agents = Some(list);
                        cx.notify();
                    })),
            );
        }

        // 开关关闭时整块置灰(错误条不受影响,见下)
        let body = div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .when(!enabled, |el| el.opacity(0.5))
            .child(
                ui::settings_card()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .mb(px(4.0))
                            .child(
                                div()
                                    .w(px(8.0))
                                    .h(px(8.0))
                                    .flex_none()
                                    .rounded_full()
                                    .bg(if self.hook_running {
                                        ui::color_success()
                                    } else {
                                        ui::border_strong()
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(ui::text_primary())
                                    .child(format!(
                                        "{} {}",
                                        t("settings", "aiHook.serverLabel"),
                                        if self.hook_running {
                                            tr!("settings", "aiHook.serverRunning", port = self.hook_port)
                                        } else {
                                            t("settings", "aiHook.serverStopped").to_string()
                                        }
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .child(t("settings", "aiHook.serverDesc")),
                    ),
            )
            .child(targets)
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        div().flex_1().child(
                            ui::primary_button(
                                "hook-register",
                                if busy {
                                    t("settings", "aiHook.registering")
                                } else {
                                    t("settings", "aiHook.register")
                                },
                            )
                            .py(px(8.0))
                            .when(busy || agents.is_empty(), |el| el.opacity(0.5))
                            .when(!busy && !agents.is_empty(), |el| {
                                el.on_click(cx.listener(|this, _, _window, cx| {
                                    this.run_hook_action(true, cx)
                                }))
                            }),
                        ),
                    )
                    .child(
                        div().flex_1().child(
                            ui::ghost_button(
                                "hook-unregister",
                                if busy {
                                    t("settings", "aiHook.unregistering")
                                } else {
                                    t("settings", "aiHook.unregister")
                                },
                            )
                            .py(px(8.0))
                            .when(busy || agents.is_empty(), |el| el.opacity(0.5))
                            .when(!busy && !agents.is_empty(), |el| {
                                el.on_click(cx.listener(|this, _, _window, cx| {
                                    this.run_hook_action(false, cx)
                                }))
                            }),
                        ),
                    ),
            )
            .when(agents.is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .justify_center()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(t("settings", "aiHook.noTargetSelected")),
                )
            })
            .child(
                div()
                    .id("hook-snippet-toggle")
                    .w_full()
                    .flex()
                    .justify_center()
                    .py(px(8.0))
                    .cursor_pointer()
                    .text_size(ui::font_px(13.0))
                    .text_color(ui::text_muted())
                    .hover(|el| el.text_color(ui::accent()))
                    .child(if self.show_snippet {
                        t("settings", "aiHook.collapseSnippet")
                    } else {
                        t("settings", "aiHook.showSnippet")
                    })
                    .on_click(cx.listener(|this, _, _window, cx| this.toggle_snippet(cx))),
            )
            .when(self.show_snippet, |el| {
                el.children(self.render_snippet(cx))
            })
            .child(ui::hint(t("settings", "aiHook.footer")));

        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(ui::settings_section_title(t("settings", "aiHook.title")))
            .child(toggle_row(
                "hook-enable",
                "aiHook.enableHook",
                "aiHook.enableHookDesc",
                enabled,
                false,
                |this, next, _window, cx| this.toggle_hook_server(next, cx),
                cx,
            ))
            // 结果/错误条**始终可见**,不受下面那块的置灰影响
            .when(!self.hook_result.is_empty(), |el| {
                el.child(
                    ui::settings_card().child(
                        div()
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_secondary())
                            .children(
                                self.hook_result
                                    .split('\n')
                                    .map(|line| div().child(line.to_string()))
                                    .collect::<Vec<_>>(),
                            ),
                    ),
                )
            })
            .child(body)
            .into_any_element()
    }

    /// 配置片段面板(四个 tab,标签是字面量不走 i18n)。
    fn render_snippet(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let data = self.snippet.as_ref()?;
        let mut tabs = div()
            .flex()
            .border_b_1()
            .border_color(ui::border_subtle());
        for (key, label) in [
            ("claude", "Claude Code"),
            ("codex", "Codex"),
            ("grok", "Grok"),
            ("omp", "oh-my-pi"),
        ] {
            let active = self.snippet_tab == key;
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("snippet-tab-{key}")))
                    .flex_1()
                    .flex()
                    .justify_center()
                    .py(px(6.0))
                    .cursor_pointer()
                    .text_size(ui::font_px(11.0))
                    .when(active, |el| {
                        el.text_color(ui::accent()).border_b_2().border_color(ui::accent())
                    })
                    .when(!active, |el| el.text_color(ui::text_muted()))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.snippet_tab = key;
                        cx.notify();
                    })),
            );
        }

        let mut content = div()
            .id("snippet-body")
            .px(px(12.0))
            .py(px(8.0))
            .max_h(px(256.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .text_size(ui::font_px(10.0))
            .text_color(ui::text_muted());
        let section_of = |value: &serde_json::Value, name: &str| value.get(name).cloned();
        if self.snippet_tab == "claude" {
            if let Some(claude) = section_of(data, "claude") {
                content = content
                    .child(snippet_file_name(
                        claude.get("file").and_then(|v| v.as_str()).unwrap_or(""),
                        None,
                    ))
                    .children(snippet_lines(
                        claude.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                    ));
            }
        } else if let Some(files) = section_of(data, self.snippet_tab)
            .and_then(|v| v.get("files").cloned())
            .and_then(|v| v.as_array().cloned())
        {
            for (i, file) in files.iter().enumerate() {
                content = content
                    .child(
                        snippet_file_name(
                            file.get("file").and_then(|v| v.as_str()).unwrap_or(""),
                            file.get("note").and_then(|v| v.as_str()),
                        )
                        .when(i > 0, |el| {
                            el.mt(px(12.0))
                                .pt(px(12.0))
                                .border_t_1()
                                .border_color(ui::border_subtle())
                        }),
                    )
                    .children(snippet_lines(
                        file.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                    ));
            }
        }

        Some(
            div()
                .rounded(px(4.0))
                .border_1()
                .border_color(ui::border_default())
                .bg(ui::bg_base())
                .overflow_hidden()
                .child(tabs)
                .child(content)
                .into_any_element(),
        )
    }
}
