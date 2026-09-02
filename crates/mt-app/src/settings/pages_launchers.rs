//! 设置面板 terminal 页里的「AI 启动器」一节。
//!
//! 启动器的增删改原本住在「移动端」面板(它同时是手机能起哪些 agent 的名单),
//! 桌面端用户在「新建终端」菜单里认识它之后却要到移动端面板里去改 —— 入口与
//! 用途不在一处。2026-09-02 迁到这里,与 shell 列表并排:一条启动器 = 一个
//! shell + 一条命令,本来就是「可用终端」的延伸。**数据没动**:仍是
//! `config.mobileRelay.launchers`,落盘走 `RelayBridge::save_launchers`
//! (顺带刷镜像、让中转重发快照,手机侧立刻看到新名单);中转桥没装时退到
//! `AppStore::set_launchers` 只落盘。
//!
//! 表单形态照 shell 列表([`super::Editing`]:编辑中的行让位给表单,新增表单
//! 挂在列表末尾),与移动端面板时代「编辑中的行仍显示、表单在下方」不同 ——
//! 这里按设置面板的既有口径统一。纯逻辑(草稿校验 / 命令警告 / 副行文案 /
//! 并入名单)仍在 `mobile_relay`,那边的测试原封不动。

use gpui::{
    AnyElement, ClickEvent, Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::input::Input;
use mt_config::AiLauncher;
use mt_ui::icons::{AiVendor, BrandIcon};

use crate::i18n::t;
use crate::menu;
use crate::mobile_relay::{command_warning, launcher_draft_valid, launcher_subtitle, upsert_launcher};
use crate::prompt::autofocus;
use crate::ui;

use super::SettingsView;
use super::widgets::{dashed_button, form_card, section};

impl SettingsView {
    /// 「AI 启动器」一节:标题 + 说明 +(空名单警告)+ 列表 + 「+ 添加启动器」。
    pub(super) fn render_launchers_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let launchers = self.store.read(cx).mobile_relay().launchers;
        let editing = self.launcher_editing;

        let mut rows = div().flex().flex_col().gap(px(8.0));
        for (idx, launcher) in launchers.iter().enumerate() {
            if editing == Some(Some(idx)) {
                rows = rows.child(self.render_launcher_form(cx));
                continue;
            }
            rows = rows.child(self.render_launcher_row(idx, launcher, cx));
        }
        if editing == Some(None) {
            rows = rows.child(self.render_launcher_form(cx));
        }

        section("launchers.title")
            .child(ui::hint(t("settings", "launchers.intro")))
            // 空名单**红字**:「新建终端」菜单少一段,手机也发不起会话
            .when(launchers.is_empty() && editing.is_none(), |el| {
                el.child(
                    div()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::color_error())
                        .child(t("settings", "launchers.empty")),
                )
            })
            .child(rows)
            .child(
                dashed_button("launcher-add", t("settings", "launchers.add")).on_click(cx.listener(
                    |this, _, window, cx| {
                        this.launcher_editing = Some(None);
                        this.fill_launcher_form(None, window, cx);
                    },
                )),
            )
            .into_any_element()
    }

    /// 一条启动器:品牌图标 + 名称(+ 编排徽标)+ 副行 + 编辑 / 删除。
    fn render_launcher_row(
        &self,
        idx: usize,
        launcher: &AiLauncher,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let subtitle = launcher_subtitle(launcher.shell.as_deref(), &launcher.command);
        // 从命令文本推断品牌(识别不出回退 Bot)
        let vendor = AiVendor::infer(None, Some(&launcher.command));

        ui::settings_card()
            .flex()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .flex_none()
                    .text_color(ui::text_secondary())
                    .child(BrandIcon::new(vendor).size(px(16.0))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(ui::text_primary())
                                    .child(SharedString::from(launcher.name.clone())),
                            )
                            // 授了编排能力的条目在列表里一眼看得出来 —— 这是个
                            // 权限位,不该只在编辑表单里才可见
                            .when(launcher.orchestration, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .px(px(6.0))
                                        .rounded(px(3.0))
                                        .bg(ui::accent_muted())
                                        .text_size(ui::font_px(10.0))
                                        .text_color(ui::accent())
                                        .child(t("settings", "launchers.orchestrationBadge")),
                                )
                            }),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .child(SharedString::from(subtitle)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(px(4.0))
                    .child(
                        ui::ghost_button(
                            SharedString::from(format!("launcher-edit-{idx}")),
                            t("settings", "common.edit"),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let launcher = this
                                .store
                                .read(cx)
                                .mobile_relay()
                                .launchers
                                .get(idx)
                                .cloned();
                            this.launcher_editing = Some(Some(idx));
                            this.fill_launcher_form(launcher.as_ref(), window, cx);
                        })),
                    )
                    .child(
                        ui::danger_button(
                            SharedString::from(format!("launcher-del-{idx}")),
                            t("settings", "common.delete"),
                        )
                        // **无二次确认**(移动端面板时代就是直接删)
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            let mut list = this.store.read(cx).mobile_relay().launchers;
                            if idx < list.len() {
                                list.remove(idx);
                            }
                            // 编辑中的行号会被这次删除搞错位,一并收掉表单
                            this.launcher_editing = None;
                            this.persist_launchers(list, cx);
                            cx.notify();
                        })),
                    ),
            )
            .into_any_element()
    }

    fn fill_launcher_form(
        &mut self,
        launcher: Option<&AiLauncher>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (name, command, shell, orchestration) = match launcher {
            Some(l) => (
                l.name.clone(),
                l.command.clone(),
                l.shell.clone(),
                l.orchestration,
            ),
            // 新启动器默认**不**带编排能力(fail-closed,ADR 0003)
            None => (String::new(), String::new(), None, false),
        };
        // 占位串重设一遍:面板开着的时候可能切了语言(与 shell 表单同一条)
        self.launcher_name.update(cx, |s, cx| {
            s.set_placeholder(t("settings", "launchers.namePlaceholder"), window, cx);
            s.set_value(name, window, cx);
        });
        self.launcher_command.update(cx, |s, cx| {
            s.set_placeholder(t("settings", "launchers.commandPlaceholder"), window, cx);
            s.set_value(command, window, cx);
        });
        self.launcher_shell = shell;
        self.launcher_orchestration = orchestration;
        // 点了「添加」/「编辑」就该能直接敲名字,不必再点一下表单
        autofocus(&self.launcher_name, window, cx);
        cx.notify();
    }

    /// 草稿表单:名称 + 命令 / shell 选择 / 命令警告 / 「允许编排」/ 保存 / 取消。
    fn render_launcher_form(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let adding = self.launcher_editing == Some(None);
        let shell_label: SharedString = self
            .launcher_shell
            .clone()
            .map(SharedString::from)
            .unwrap_or_else(|| t("settings", "launchers.defaultShell").into());
        let name_value = self.launcher_name.read(cx).value().to_string();
        let command_value = self.launcher_command.read(cx).value().to_string();
        // 同步纯函数,不阻塞保存(它只是把失败从「手机上等 15 秒超时」前移到配置时)
        let warn = command_warning(&command_value);
        let can_save = launcher_draft_valid(&name_value, &command_value);
        let orchestration = self.launcher_orchestration;

        form_card(adding)
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    // 与 shell 表单同一比例:名称列固定宽、命令列吃掉剩余
                    .child(
                        div()
                            .w(px(150.0))
                            .flex_none()
                            .child(Input::new(&self.launcher_name)),
                    )
                    .child(div().flex_1().child(Input::new(&self.launcher_command))),
            )
            // shell 选择:自建菜单的 picker 按钮(第一项「使用默认 shell」)
            .child(
                div()
                    .id("launcher-shell-picker")
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(12.0))
                    .py(px(6.0))
                    .rounded(px(4.0))
                    .bg(ui::bg_surface())
                    .border_1()
                    .border_color(ui::border_default())
                    .cursor_pointer()
                    .hover(|el| el.border_color(ui::accent()))
                    .text_size(ui::font_px(13.0))
                    .text_color(ui::text_primary())
                    .child(shell_label)
                    .child(div().text_color(ui::text_muted()).child("▾"))
                    .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                        let view = cx.entity();
                        let current = this.launcher_shell.clone();
                        let shells: Vec<String> = this
                            .store
                            .read(cx)
                            .config()
                            .available_shells
                            .iter()
                            .map(|s| s.name.clone())
                            .collect();
                        let mark = |on: bool| if on { "✓ " } else { "   " };
                        let mut entries = vec![menu::item(
                            format!(
                                "{}{}",
                                mark(current.is_none()),
                                t("settings", "launchers.defaultShell")
                            ),
                            {
                                let view = view.clone();
                                move |_window, cx| {
                                    view.update(cx, |this, cx| {
                                        this.launcher_shell = None;
                                        cx.notify();
                                    })
                                }
                            },
                        )];
                        for name in shells {
                            let selected = current.as_deref() == Some(name.as_str());
                            entries.push(menu::item(format!("{}{name}", mark(selected)), {
                                let view = view.clone();
                                move |_window, cx| {
                                    let name = name.clone();
                                    view.update(cx, |this, cx| {
                                        this.launcher_shell = Some(name);
                                        cx.notify();
                                    })
                                }
                            }));
                        }
                        menu::show(event.position(), entries, window, cx);
                    })),
            )
            // **黄字不是红字** —— 它不阻塞保存
            .when(warn, |el| {
                el.child(
                    div()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::color_ai_working())
                        .child(t("settings", "launchers.commandWarning")),
                )
            })
            // 「允许编排」开关(ADR 0003 的信任根:编排能力**只能**从这里授予)。
            // 它是**授权**不是提示,所以带一行说明。
            .child(
                div()
                    .id("launcher-orchestration")
                    .flex()
                    .items_start()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .child(
                        div()
                            .mt(px(2.0))
                            .child(ui::checkbox("launcher-orchestration-box", orchestration)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(ui::text_primary())
                                    .child(t("settings", "launchers.orchestration")),
                            )
                            .child(
                                div()
                                    .text_size(ui::font_px(11.0))
                                    .text_color(ui::text_muted())
                                    .child(t("settings", "launchers.orchestrationHint")),
                            ),
                    )
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.launcher_orchestration = !this.launcher_orchestration;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(
                        ui::primary_button(
                            "launcher-save",
                            if adding {
                                t("settings", "common.add")
                            } else {
                                t("settings", "common.save")
                            },
                        )
                        // 名称 / 命令为空时按钮变淡且不响应(与移动端面板时代一致)
                        .opacity(if can_save { 1.0 } else { 0.4 })
                        .on_click(cx.listener(|this, _, _window, cx| this.save_launcher(cx))),
                    )
                    .child(
                        ui::ghost_button("launcher-cancel", t("settings", "common.cancel")).on_click(
                            cx.listener(|this, _, _window, cx| {
                                this.launcher_editing = None;
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn save_launcher(&mut self, cx: &mut Context<Self>) {
        let name = self.launcher_name.read(cx).value().to_string();
        let command = self.launcher_command.read(cx).value().to_string();
        if !launcher_draft_valid(&name, &command) {
            return;
        }
        let list = self.store.read(cx).mobile_relay().launchers;
        // 编辑态按行号取回原 id(替换同 id 那条);新增传空串让 upsert 生成
        let id = match self.launcher_editing {
            Some(Some(idx)) => list.get(idx).map(|l| l.id.clone()).unwrap_or_default(),
            _ => String::new(),
        };
        let next = upsert_launcher(
            &list,
            &id,
            &name,
            self.launcher_shell.as_deref(),
            &command,
            self.launcher_orchestration,
        );
        self.launcher_editing = None;
        self.persist_launchers(next, cx);
        cx.notify();
    }

    /// 落盘启动器名单。有中转桥就走它:除了落盘还刷镜像 + 让中转重发全量快照
    /// (手机可能下一秒就按 id 发起会话);桥没装时只落盘。
    fn persist_launchers(&self, launchers: Vec<AiLauncher>, cx: &mut Context<Self>) {
        match crate::mobile_relay::bridge(cx) {
            Some(bridge) => bridge.update(cx, |bridge, cx| bridge.save_launchers(launchers, cx)),
            None => self
                .store
                .update(cx, |store, cx| store.set_launchers(launchers, cx)),
        }
    }
}
