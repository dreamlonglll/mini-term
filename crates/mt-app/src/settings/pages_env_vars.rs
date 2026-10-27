//! 设置面板的 env-vars 页:全局终端环境变量(`AppConfig::terminal_env_vars`)。
//!
//! 每个**本地**终端新建时都注入;项目级环境变量(项目右键 → 环境变量)同名优先,
//! 合并在 `store::merge_terminal_env`。表格本体(行状态 / 表头 / 一行 / 校验)与
//! 项目环境变量弹窗共用,在 [`crate::env_vars`];这里只管设置页这一侧的口径:
//!
//! - **没有保存按钮**(设置页通例):输入框失焦 / 回车、勾选、删行即提交;
//! - **表里有一行报错就整表不提交**:只落盘没报错的行的话,改名途中撞了别行的名
//!   (两行都报重复)会把那两行一起从配置里抹掉。已保存的仍是上一份合法的表,
//!   页脚另起一行红字说明;
//! - 关掉设置面板时兜底再提交一次(见 `SettingsView::new` 的 `on_release`)——
//!   直接点 ✕ 关面板不保证输入框先失焦。

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Subscription, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::input::{InputEvent, InputState};
use mt_config::ProjectEnvVar;

use crate::env_vars::{
    EnvRow, clean_rows, new_row, snapshot, snapshot_errors, table_header, table_row,
};
use crate::i18n::t;
use crate::prompt::autofocus;
use crate::ui;

use super::SettingsView;
use super::widgets::{dashed_button, page_root, section};

impl SettingsView {
    // ── 行状态 ──

    /// 按已保存的值建表(面板打开时一次)。
    pub(super) fn init_env_rows(
        &mut self,
        vars: &[ProjectEnvVar],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for var in vars {
            self.push_env_row(&var.key, &var.value, var.enabled, window, cx);
        }
    }

    /// 追加一行并挂上两个输入框的订阅,返回新行。
    fn push_env_row(
        &mut self,
        key: &str,
        value: &str,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> EnvRow {
        self.env_next_rid += 1;
        let row = new_row(self.env_next_rid, key, value, enabled, window, cx);
        let subs = [
            Self::subscribe_env_input(&row.key, window, cx),
            Self::subscribe_env_input(&row.value, window, cx),
        ];
        self.env_subs.insert(row.rid, subs);
        self.env_rows.push(row.clone());
        row
    }

    fn subscribe_env_input(
        input: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            input,
            window,
            |this: &mut Self, _input, event: &InputEvent, _window, cx| match event {
                // 边打字边重算校验(红框 / 红字),落盘等失焦或回车
                InputEvent::Change => cx.notify(),
                InputEvent::Blur | InputEvent::PressEnter { .. } => this.commit_env_vars(cx),
                _ => {}
            },
        )
    }

    fn toggle_env_row(&mut self, rid: u64, cx: &mut Context<Self>) {
        if let Some(row) = self.env_rows.iter_mut().find(|r| r.rid == rid) {
            row.enabled = !row.enabled;
        }
        self.commit_env_vars(cx);
        cx.notify();
    }

    fn remove_env_row(&mut self, rid: u64, cx: &mut Context<Self>) {
        self.env_rows.retain(|r| r.rid != rid);
        self.env_subs.remove(&rid);
        self.commit_env_vars(cx);
        cx.notify();
    }

    // ── 提交 ──

    /// 表里此刻的内容该不该落盘:整表无错、且清洗后与已保存的不同才给值。
    fn pending_env_vars(&self, cx: &App) -> Option<Vec<ProjectEnvVar>> {
        let snapshot = snapshot(&self.env_rows, cx);
        if snapshot_errors(&snapshot).iter().any(Option::is_some) {
            return None;
        }
        let vars = clean_rows(&snapshot);
        (self.store.read(cx).config().terminal_env_vars != vars).then_some(vars)
    }

    /// 收 `&mut App` 而不是 `Context<Self>`:关面板时的兜底提交在 `on_release` 里,
    /// 那里只有 `App`。
    pub(super) fn commit_env_vars(&mut self, cx: &mut App) {
        if let Some(vars) = self.pending_env_vars(cx) {
            self.store
                .update(cx, |store, cx| store.set_terminal_env_vars(vars, cx));
        }
    }

    // ── env-vars 页 ──

    pub(super) fn render_env_vars_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.env_rows.clone();
        let errors = snapshot_errors(&snapshot(&rows, cx));
        let has_errors = errors.iter().any(Option::is_some);

        let mut body = section("envVars.section");
        // 一行都没有时连表头一起不画,只剩下面那颗「+ 添加」
        if !rows.is_empty() {
            let mut list = div().flex().flex_col().gap(px(6.0));
            for (idx, row) in rows.iter().enumerate() {
                let rid = row.rid;
                list = list.child(table_row(
                    row,
                    errors.get(idx).copied().flatten(),
                    cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.toggle_env_row(rid, cx)
                    }),
                    cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.remove_env_row(rid, cx)
                    }),
                ));
            }
            body = body.child(ui::settings_card().child(table_header()).child(list));
        }

        page_root()
            .child(
                body.child(
                    dashed_button("env-add", t("settings", "envVars.addVar")).on_click(
                        cx.listener(|this, _, window, cx| {
                            let row = this.push_env_row("", "", true, window, cx);
                            // 点了「添加」就该能直接打字,不必再点一下新行
                            autofocus(&row.key, window, cx);
                            cx.notify();
                        }),
                    ),
                )
                .when(has_errors, |el| {
                    el.child(
                        ui::hint(t("settings", "envVars.hasErrors")).text_color(ui::color_error()),
                    )
                })
                .child(ui::hint(t("settings", "envVars.hint"))),
            )
            .into_any_element()
    }
}
