//! 设置面板的 terminal(Shell 列表 + 行为)与 clipboard(复制粘贴)两页。
//!
//! 两页共用「即时生效 + 即时落盘」的口径,数字行仍是草稿态 —— 归一与提交在
//! [`super`] 的 `commit_number`,这里只画。

use gpui::{
    AnyElement, Context, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::input::Input;
use mt_config::ShellConfig;

use crate::i18n::t;
use crate::shell_ops::{parse_args, valid_shell};
use crate::ui;

use super::SettingsView;
use super::widgets::{
    dashed_button, form_card, number_row, page_root, radio_dot, section, toggle_row,
};

impl SettingsView {
    // ── terminal 页 ──

    pub(super) fn render_terminal_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let list = self.store.read(cx).shell_list();
        let editing = self.shell_editing;
        let animations = self
            .store
            .read(cx)
            .config()
            .terminal_animations
            .unwrap_or(true);

        let mut rows = div().flex().flex_col().gap(px(8.0));
        for (idx, shell) in list.shells.iter().enumerate() {
            if editing == Some(Some(idx)) {
                rows = rows.child(self.render_shell_form(cx));
                continue;
            }
            let is_default = shell.name == list.default_shell;
            let detail = match &shell.args {
                Some(args) if !args.is_empty() => format!("{} {}", shell.command, args.join(" ")),
                _ => shell.command.clone(),
            };
            rows = rows.child(
                ui::settings_card()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        radio_dot(format!("shell-default-{idx}"), is_default).on_click(cx.listener(
                            move |this, _, _window, cx| {
                                let name = this.store.read(cx).config().available_shells[idx]
                                    .name
                                    .clone();
                                this.store.update(cx, |store, cx| {
                                    let mut list = store.shell_list();
                                    list.set_default(&name);
                                    store.apply_shell_list(list, cx);
                                });
                            },
                        )),
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
                                    .child(shell.name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(ui::font_px(11.0))
                                    .text_color(ui::text_muted())
                                    .child(detail),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(4.0))
                            .child(
                                ui::ghost_button(
                                    SharedString::from(format!("shell-edit-{idx}")),
                                    t("settings", "common.edit"),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let shell = this
                                        .store
                                        .read(cx)
                                        .config()
                                        .available_shells
                                        .get(idx)
                                        .cloned();
                                    this.shell_editing = Some(Some(idx));
                                    this.fill_shell_form(shell.as_ref(), window, cx);
                                })),
                            )
                            .child(
                                ui::danger_button(
                                    SharedString::from(format!("shell-del-{idx}")),
                                    t("settings", "common.delete"),
                                )
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.store.update(cx, |store, cx| {
                                        let mut list = store.shell_list();
                                        list.remove(idx);
                                        store.apply_shell_list(list, cx);
                                    });
                                    // 编辑中的行号会被这次删除搞错位,一并收掉表单
                                    this.shell_editing = None;
                                    cx.notify();
                                })),
                            ),
                    ),
            );
        }
        if editing == Some(None) {
            rows = rows.child(self.render_shell_form(cx));
        }

        page_root()
            .child(
                section("terminal.availableTerminals")
                    .child(rows)
                    .child(
                        dashed_button("shell-add", t("settings", "terminal.addTerminal")).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.shell_editing = Some(None);
                                this.fill_shell_form(None, window, cx);
                            }),
                        ),
                    )
                    .child(ui::hint(t("settings", "terminal.defaultHint"))),
            )
            .child(
                section("terminal.behavior")
                    .child(number_row(
                        "terminal.scrollback",
                        "terminal.scrollbackDesc",
                        &self.num_scrollback,
                        false,
                    ))
                    // 终端区换场动画总开关。⚠️ 只管终端区那批(切 tab/面板/
                    // 最大化/拆分),抽屉进出场等浮层动画不归它 ——
                    // 那批是布局反馈不是装饰
                    .child(toggle_row(
                        "terminal-animations",
                        "terminal.animationsTitle",
                        "terminal.animationsDesc",
                        animations,
                        false,
                        |this, next, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.patch_config(|c| c.terminal_animations = Some(next), cx)
                            });
                        },
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn fill_shell_form(
        &mut self,
        shell: Option<&ShellConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (name, command, args) = match shell {
            Some(s) => (
                s.name.clone(),
                s.command.clone(),
                s.args.clone().unwrap_or_default().join(" "),
            ),
            None => (String::new(), String::new(), String::new()),
        };
        // 占位串在建输入框时只取过一次;这里重设一遍,免得面板开着的时候切了语言
        // (语言段控件就在同一个面板里),下次点「添加终端」还是旧语言。
        self.shell_name.update(cx, |s, cx| {
            s.set_placeholder(t("settings", "terminal.newNamePlaceholder"), window, cx);
            s.set_value(name, window, cx);
        });
        self.shell_command.update(cx, |s, cx| {
            s.set_placeholder(t("settings", "terminal.newCommandPlaceholder"), window, cx);
            s.set_value(command, window, cx);
        });
        self.shell_args.update(cx, |s, cx| {
            s.set_placeholder(t("settings", "terminal.newArgsPlaceholder"), window, cx);
            s.set_value(args, window, cx);
        });
        self.shell_error = None;
        cx.notify();
    }

    fn render_shell_form(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let adding = self.shell_editing == Some(None);
        form_card(adding)
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    // 原版是 `flex-1` : `flex-[2]`;gpui 没有任意 grow 系数,
                    // 名称列给固定宽、命令列吃掉剩余 —— 同样的 1:2 观感
                    .child(div().w(px(150.0)).flex_none().child(Input::new(&self.shell_name)))
                    .child(div().flex_1().child(Input::new(&self.shell_command))),
            )
            .child(Input::new(&self.shell_args))
            .when_some(self.shell_error, |el, msg| {
                el.child(
                    div()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::color_error())
                        .child(msg),
                )
            })
            .child(
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(
                        ui::primary_button(
                            "shell-save",
                            if adding {
                                t("settings", "common.add")
                            } else {
                                t("settings", "common.save")
                            },
                        )
                        .on_click(cx.listener(|this, _, _window, cx| this.save_shell(cx))),
                    )
                    .child(
                        ui::ghost_button("shell-cancel", t("settings", "common.cancel")).on_click(
                            cx.listener(|this, _, _window, cx| {
                                this.shell_editing = None;
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn save_shell(&mut self, cx: &mut Context<Self>) {
        let name = self.shell_name.read(cx).value().trim().to_string();
        let command = self.shell_command.read(cx).value().trim().to_string();
        if !valid_shell(&name, &command) {
            // 原版是「名字/命令为空时保存按钮直接不响应」,没有这句提示文案 ——
            // 借用 envVars 里语义最近的那条通用校验串。
            self.shell_error = Some(t("envVars", "hasErrors"));
            cx.notify();
            return;
        }
        let shell = ShellConfig {
            name,
            command,
            args: parse_args(&self.shell_args.read(cx).value()),
        };
        let editing = self.shell_editing;
        self.store.update(cx, |store, cx| {
            let mut list = store.shell_list();
            match editing {
                Some(Some(idx)) => list.update(idx, shell),
                _ => list.add(shell),
            }
            store.apply_shell_list(list, cx);
        });
        self.shell_editing = None;
        cx.notify();
    }

    // ── clipboard 页 ──

    pub(super) fn render_clipboard_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let config = self.store.read(cx).config();
        let smart = config.smart_copy_paste;
        let long_paste = config.long_paste_to_file;

        page_root()
            .child(
                section("clipboard.copyPaste")
                    .child(toggle_row(
                        "clip-smart",
                        "clipboard.smartCopyPasteTitle",
                        "clipboard.smartCopyPasteDesc",
                        smart,
                        false,
                        |this, next, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.patch_config(|c| c.smart_copy_paste = next, cx)
                            });
                        },
                        cx,
                    ))
                    .child(number_row(
                        "clipboard.autoCopyDwellTitle",
                        "clipboard.autoCopyDwellDesc",
                        &self.num_dwell,
                        false,
                    )),
            )
            .child(
                section("clipboard.longPaste")
                    .child(toggle_row(
                        "clip-long-paste",
                        "clipboard.longPasteTitle",
                        "clipboard.longPasteDesc",
                        long_paste,
                        false,
                        |this, next, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.patch_config(|c| c.long_paste_to_file = next, cx)
                            });
                        },
                        cx,
                    ))
                    // 总开关关掉时下面两行**置灰**(与 system 页的托盘子项不同,
                    // 那边是整个不渲染)
                    .child(number_row(
                        "clipboard.lineThreshold",
                        "clipboard.lineThresholdDesc",
                        &self.num_line_threshold,
                        !long_paste,
                    ))
                    .child(number_row(
                        "clipboard.charThreshold",
                        "clipboard.charThresholdDesc",
                        &self.num_char_threshold,
                        !long_paste,
                    ))
                    .child(ui::hint(t("settings", "clipboard.longPasteFooter"))),
            )
            .child(
                section("clipboard.remotePaste")
                    // 这一段不是 SettingRow,是一张标题 + 说明 + 整宽输入框的卡片
                    .child(
                        ui::settings_card()
                            .child(
                                div()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(ui::text_primary())
                                    .child(t("settings", "clipboard.remotePasteDir")),
                            )
                            .child(
                                div()
                                    .mb(px(8.0))
                                    .text_size(ui::font_px(11.0))
                                    .text_color(ui::text_muted())
                                    .child(t("settings", "clipboard.remotePasteDirDesc")),
                            )
                            .child(Input::new(&self.txt_remote_paste_dir)),
                    )
                    .child(ui::hint(t("settings", "clipboard.remotePasteFooter"))),
            )
            .into_any_element()
    }
}
