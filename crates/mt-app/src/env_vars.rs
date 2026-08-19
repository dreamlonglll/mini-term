//! 项目环境变量弹窗(对照 `src/components/ProjectEnvVarsModal.tsx` 285 行)。
//!
//! 一张键值对表:勾选框(启用/禁用)+ 变量名 + 变量值 + 删行,底下一颗「+ 新增
//! 一行」。保存时丢掉「key 空白」的占位行,落进 `ProjectConfig::env_vars`,
//! 由 [`AppStore::start_pty`] 在**新建终端**时注入(已有终端不受影响 —— 底栏
//! 那句脚注说的就是这件事)。
//!
//! # 校验(纯逻辑,单测钉死)
//!
//! [`compute_errors`] 逐行判,优先级照抄原版:
//! **空 key > `MINITERM_` 前缀 > 保留的 `WSLENV` > 非法字符 > 与别行重复 > value 非法**。
//! 有任一行报错就禁用「保存」。
//!
//! - `MINITERM_` 是内部协议前缀(`MINITERM_PTY_ID` 等 hook 定位键),放开会让
//!   用户改掉 AI 状态上报的定位;
//! - `WSLENV` 由 Rust 端在 WSL 分支拼装注入(`K1/u:K2/u:` + 宿主既有),
//!   允许覆盖会破坏拼接结果。大小写敏感比较 —— 与 Microsoft 官方一致。
//!
//! # 与原版的两处差异
//!
//! 1. **`key` 的合法字符判定手写而不是正则**(`^[A-Za-z_][A-Za-z0-9_]*$`)——
//!    为一条固定规则拖一个 `regex` 进依赖树不值当,行为逐字等价且有单测。
//! 2. **保存不做乐观更新+回滚**:原版先 `setConfig` 再 `await saveConfigToDisk`,
//!    失败回滚;GPUI 侧 [`AppStore::set_project_env_vars`] 的落盘是同步的
//!    (`save_config_now`),写失败只会在 stderr 留痕 —— 与本壳其它每一处配置
//!    写入同一条路,不为这一个弹窗另造回滚机制。

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Input, InputState};
use mt_config::ProjectEnvVar;

use crate::i18n::{t, tr};
use crate::prompt::{close_guarded, kind, open_guarded};
use crate::ssh_panel::panel_header;
use crate::store::AppStore;
use crate::ui;

/// 弹窗宽度(原版 `w-[640px]`)。
const PANEL_W: f32 = 640.0;
/// 表格区高度(原版 `max-h-[80vh]`,gpui 没有视口单位)。
const BODY_H: f32 = 380.0;

// ─── 校验(纯逻辑) ───────────────────────────────────────────

/// 一行的错误档。文案 key 见 [`RowError::key`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowError {
    EmptyKey,
    ProtectedPrefix,
    ReservedWslenv,
    InvalidKey,
    DuplicateKey,
    InvalidValue,
}

impl RowError {
    /// `envVars.error.*` 里对应那条。**返回字面量**(不是拼出来的串)——
    /// `t()` 的 debug_assert 与 `i18n.rs` 的 `USED_KEYS` 表都要求 key 是字面量。
    pub fn key(self) -> &'static str {
        match self {
            Self::EmptyKey => "error.emptyKey",
            Self::ProtectedPrefix => "error.protectedPrefix",
            Self::ReservedWslenv => "error.reservedWslenv",
            Self::InvalidKey => "error.invalidKey",
            Self::DuplicateKey => "error.duplicateKey",
            Self::InvalidValue => "error.invalidValue",
        }
    }
}

/// `^[A-Za-z_][A-Za-z0-9_]*$`(原版 `KEY_PATTERN`)。
fn key_pattern_ok(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 逐行算错误。返回与入参等长的表(`None` = 该行没问题)。
///
/// **key 与 value 都空白的行整行跳过**——那是「+ 新增一行」留下的占位行,
/// 用户还没填就报错等于一开弹窗满屏红字。
pub fn compute_errors(rows: &[(String, String)]) -> Vec<Option<RowError>> {
    // 重复判定按**原文**计数(不 trim),与原版 `keyCount` 同口径
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (key, _) in rows {
        if !key.is_empty() {
            *counts.entry(key.as_str()).or_insert(0) += 1;
        }
    }
    rows.iter()
        .map(|(key, value)| {
            if key.trim().is_empty() && value.trim().is_empty() {
                return None;
            }
            if key.trim().is_empty() {
                return Some(RowError::EmptyKey);
            }
            if key.starts_with("MINITERM_") {
                return Some(RowError::ProtectedPrefix);
            }
            if key == "WSLENV" {
                return Some(RowError::ReservedWslenv);
            }
            if !key_pattern_ok(key) {
                return Some(RowError::InvalidKey);
            }
            if counts.get(key.as_str()).copied().unwrap_or(0) > 1 {
                return Some(RowError::DuplicateKey);
            }
            if value.contains(['\n', '\r', '\0']) {
                return Some(RowError::InvalidValue);
            }
            None
        })
        .collect()
}

/// 保存时的清洗:丢掉「key 空白」的占位行,**保留 `enabled=false` 的行**
/// (取消勾选时 value 保留但不注入,是配置字段本身的语义)。
pub fn clean_rows(rows: &[(String, String, bool)]) -> Vec<ProjectEnvVar> {
    rows.iter()
        .filter(|(key, _, _)| !key.trim().is_empty())
        .map(|(key, value, enabled)| ProjectEnvVar {
            key: key.clone(),
            value: value.clone(),
            enabled: *enabled,
        })
        .collect()
}

/// 路径是不是 WSL UNC 形态(顶部那条绿色提示的显隐条件,`isWslPath`)。
fn is_wsl_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase().replace('/', "\\");
    lower.starts_with("\\\\wsl$\\") || lower.starts_with("\\\\wsl.localhost\\")
}

// ─── 视图 ─────────────────────────────────────────────────────

/// 编辑中的一行。`rid` 只用来做元素 id 与「刚新增的那一行」定位,不落盘。
struct Row {
    rid: u64,
    key: Entity<InputState>,
    value: Entity<InputState>,
    enabled: bool,
}

pub struct EnvVarsPanel {
    store: Entity<AppStore>,
    project_id: String,
    project_name: String,
    project_path: String,
    rows: Vec<Row>,
    next_rid: u64,
}

impl Render for EnvVarsPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl EnvVarsPanel {
    fn snapshot(&self, cx: &App) -> Vec<(String, String, bool)> {
        self.rows
            .iter()
            .map(|r| {
                (
                    r.key.read(cx).value().to_string(),
                    r.value.read(cx).value().to_string(),
                    r.enabled,
                )
            })
            .collect()
    }
}

/// 打开项目环境变量弹窗。
pub fn open(store: Entity<AppStore>, project_id: &str, window: &mut Window, cx: &mut App) {
    if crate::overlay::contains(crate::overlay::key(kind::PROJECT_ENV_VARS)) {
        return;
    }
    let Some(project) = store.read(cx).project(project_id).cloned() else {
        return;
    };

    let mut rows = Vec::new();
    let mut next_rid = 0u64;
    for var in &project.env_vars {
        next_rid += 1;
        rows.push(new_row(next_rid, &var.key, &var.value, var.enabled, window, cx));
    }
    // 一条都没有时留一行空白占位(原版同款),否则弹窗开出来是空的
    if rows.is_empty() {
        next_rid += 1;
        rows.push(new_row(next_rid, "", "", true, window, cx));
    }

    let state = cx.new(|_cx| EnvVarsPanel {
        store,
        project_id: project.id.clone(),
        project_name: project.name.clone(),
        project_path: project.path.clone(),
        rows,
        next_rid,
    });

    open_guarded(
        kind::PROJECT_ENV_VARS,
        window,
        cx,
        move |dialog, _window, cx| {
            let body = render_body(&state, cx);
            dialog
                .p_0()
                .close_button(false)
                .w(px(PANEL_W))
                // 一整屏手填的键值对,点遮罩关掉代价太大;Esc 仍是逃生口
                .overlay_closable(false)
                .child(body)
        },
    );
}

fn new_row(
    rid: u64,
    key: &str,
    value: &str,
    enabled: bool,
    window: &mut Window,
    cx: &mut App,
) -> Row {
    Row {
        rid,
        key: cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t("envVars", "keyPlaceholder"))
                .default_value(key.to_string())
        }),
        value: cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t("envVars", "valuePlaceholder"))
                .default_value(value.to_string())
        }),
        enabled,
    }
}

fn save(state: &Entity<EnvVarsPanel>, window: &mut Window, cx: &mut App) {
    let snapshot = state.read(cx).snapshot(cx);
    let pairs: Vec<(String, String)> = snapshot
        .iter()
        .map(|(k, v, _)| (k.clone(), v.clone()))
        .collect();
    if compute_errors(&pairs).iter().any(Option::is_some) {
        return;
    }
    let vars = clean_rows(&snapshot);
    state.update(cx, |panel, cx| {
        let project_id = panel.project_id.clone();
        panel
            .store
            .update(cx, |store, cx| {
                store.set_project_env_vars(&project_id, vars, cx)
            });
    });
    close_guarded(kind::PROJECT_ENV_VARS, window, cx);
}

fn render_body(state: &Entity<EnvVarsPanel>, cx: &mut App) -> AnyElement {
    let (project_name, project_path, rows, snapshot) = {
        let panel = state.read(cx);
        let rows: Vec<(u64, Entity<InputState>, Entity<InputState>, bool)> = panel
            .rows
            .iter()
            .map(|r| (r.rid, r.key.clone(), r.value.clone(), r.enabled))
            .collect();
        (
            panel.project_name.clone(),
            panel.project_path.clone(),
            rows,
            panel.snapshot(cx),
        )
    };
    let pairs: Vec<(String, String)> = snapshot
        .iter()
        .map(|(k, v, _)| (k.clone(), v.clone()))
        .collect();
    let errors = compute_errors(&pairs);
    let has_errors = errors.iter().any(Option::is_some);

    let mut table = div()
        .id("env-vars-body")
        .h(px(BODY_H))
        .overflow_y_scroll()
        .px(px(20.0))
        .py(px(16.0))
        .flex()
        .flex_col()
        // 表头
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .mb(px(8.0))
                .text_size(ui::font_px(10.0))
                .text_color(ui::text_muted())
                .child(div().w(px(16.0)).flex_none().child(t("envVars", "colEnabled")))
                .child(div().flex_1().min_w(px(0.0)).child(t("envVars", "keyHeader")))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(t("envVars", "valueHeader")),
                )
                .child(div().w(px(24.0)).flex_none()),
        );

    let mut list = div().flex().flex_col().gap(px(6.0));
    for (idx, (rid, key_input, value_input, enabled)) in rows.into_iter().enumerate() {
        let err = errors.get(idx).copied().flatten();
        list = list.child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            ui::checkbox(SharedString::from(format!("env-on-{rid}")), enabled)
                                .tooltip(move |window, cx| {
                                    gpui_component::tooltip::Tooltip::new(if enabled {
                                        t("envVars", "rowEnabled")
                                    } else {
                                        t("envVars", "rowDisabled")
                                    })
                                    .build(window, cx)
                                })
                                .on_click({
                                    let state = state.clone();
                                    move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                                        state.update(cx, |panel, cx| {
                                            if let Some(row) =
                                                panel.rows.iter_mut().find(|r| r.rid == rid)
                                            {
                                                row.enabled = !row.enabled;
                                            }
                                            cx.notify();
                                        });
                                    }
                                }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                // 报错行描红边:gpui 的 `Input` 自带边框,这里在外层
                                // 套一圈同色边(1px)最不破版
                                .when(err.is_some(), |el| {
                                    el.rounded(px(4.0))
                                        .border_1()
                                        .border_color(ui::color_error())
                                })
                                .child(Input::new(&key_input)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .when(err.is_some(), |el| {
                                    el.rounded(px(4.0))
                                        .border_1()
                                        .border_color(ui::color_error())
                                })
                                .child(Input::new(&value_input)),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("env-del-{rid}")))
                                .w(px(24.0))
                                .h(px(24.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(4.0))
                                .cursor_pointer()
                                .text_size(ui::font_px(11.0))
                                .text_color(ui::text_muted())
                                .hover(|el| el.text_color(ui::color_error()))
                                .tooltip(|window, cx| {
                                    gpui_component::tooltip::Tooltip::new(t("envVars", "removeRow"))
                                        .build(window, cx)
                                })
                                .child("✕")
                                .on_click({
                                    let state = state.clone();
                                    move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                                        state.update(cx, |panel, cx| {
                                            panel.rows.retain(|r| r.rid != rid);
                                            cx.notify();
                                        });
                                    }
                                }),
                        ),
                )
                .when_some(err, |el, err| {
                    el.child(
                        div()
                            .ml(px(24.0))
                            .text_size(ui::font_px(10.0))
                            .text_color(ui::color_error())
                            .child(t("envVars", err.key())),
                    )
                }),
        );
    }

    table = table.child(list).child(
        div().mt(px(12.0)).child(
            div()
                .id("env-add-row")
                .cursor_pointer()
                .text_size(ui::font_px(11.0))
                .text_color(ui::accent())
                .child(t("envVars", "addRow"))
                .on_click({
                    let state = state.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let rid = state.read(cx).next_rid + 1;
                        let row = new_row(rid, "", "", true, window, cx);
                        state.update(cx, |panel, cx| {
                            panel.next_rid = rid;
                            panel.rows.push(row);
                            cx.notify();
                        });
                    }
                }),
        ),
    );

    let mut root = div()
        .flex()
        .flex_col()
        .child(panel_header(
            kind::PROJECT_ENV_VARS,
            t("envVars", "title"),
            Some(tr!("envVars", "subtitle", name = project_name)),
            true,
        ));

    if is_wsl_path(&project_path) {
        // WSL 项目:WSLENV 透传说明(原版是一整段带 `<code>` 的绿色提示条,
        // gpui 无行内富文本,拼成一句纯文本)
        root = root.child(
            div()
                .mx(px(20.0))
                .mt(px(12.0))
                .px(px(12.0))
                .py(px(8.0))
                .rounded(px(4.0))
                .bg(ui::with_alpha(ui::color_success(), 0.1))
                .border_1()
                .border_color(ui::with_alpha(ui::color_success(), 0.3))
                .text_size(ui::font_px(11.0))
                .text_color(ui::color_success())
                .child(format!(
                    "✓ {}/u{}/home/u/...{}~/.bashrc{}export{}",
                    t("envVars", "wsl.part1"),
                    t("envVars", "wsl.part2"),
                    t("envVars", "wsl.part3"),
                    t("envVars", "wsl.part4"),
                    t("envVars", "wsl.part5"),
                )),
        );
    }

    root.child(table)
        .child(render_footer(state, has_errors))
        .into_any_element()
}

fn render_footer(state: &Entity<EnvVarsPanel>, has_errors: bool) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(12.0))
        .px(px(20.0))
        .py(px(10.0))
        .border_t_1()
        .border_color(ui::border_subtle())
        .child(
            div()
                .flex_1()
                .text_size(ui::font_px(10.0))
                .text_color(ui::text_muted())
                .child(if has_errors {
                    t("envVars", "hasErrors")
                } else {
                    t("envVars", "footnote")
                }),
        )
        .child(
            ui::ghost_button("env-cancel", t("envVars", "cancel")).on_click(
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    close_guarded(kind::PROJECT_ENV_VARS, window, cx);
                },
            ),
        )
        .child(
            ui::primary_button("env-save", t("envVars", "save"))
                .opacity(if has_errors { 0.4 } else { 1.0 })
                .on_click({
                    let state = state.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        save(&state, window, cx);
                    }
                }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// key 与 value 都空白的占位行不报错(刚点「新增一行」的那一刻)。
    #[test]
    fn 空白占位行不报错() {
        assert_eq!(compute_errors(&rows(&[("", ""), ("  ", "  ")])), vec![None, None]);
    }

    /// 只填了 value 的行报「key 不能为空」。
    #[test]
    fn 只填值报空key() {
        assert_eq!(
            compute_errors(&rows(&[("", "x")])),
            vec![Some(RowError::EmptyKey)]
        );
    }

    /// 优先级:受保护前缀压过「非法字符」与「重复」。
    #[test]
    fn 受保护前缀优先于其它错() {
        // `MINITERM_A-B` 同时违反前缀与字符集,报前缀
        assert_eq!(
            compute_errors(&rows(&[("MINITERM_A-B", "1")])),
            vec![Some(RowError::ProtectedPrefix)]
        );
        // 两行同名且都带前缀 → 两行都报前缀(不是重复)
        assert_eq!(
            compute_errors(&rows(&[("MINITERM_X", "1"), ("MINITERM_X", "2")])),
            vec![
                Some(RowError::ProtectedPrefix),
                Some(RowError::ProtectedPrefix)
            ]
        );
    }

    /// `WSLENV` 大小写敏感:只有全大写那个被拦(与 Microsoft 官方一致)。
    #[test]
    fn wslenv_大小写敏感() {
        assert_eq!(
            compute_errors(&rows(&[("WSLENV", "a")])),
            vec![Some(RowError::ReservedWslenv)]
        );
        assert_eq!(compute_errors(&rows(&[("wslenv", "a")])), vec![None]);
    }

    /// key 字符集:首字符不能是数字,只认 `a-zA-Z0-9_`。
    #[test]
    fn key字符集() {
        assert!(key_pattern_ok("_A1"));
        assert!(key_pattern_ok("PATH"));
        assert!(!key_pattern_ok("1A"));
        assert!(!key_pattern_ok("A-B"));
        assert!(!key_pattern_ok("A B"));
        assert!(!key_pattern_ok("变量"));
        assert!(!key_pattern_ok(""));
        assert_eq!(
            compute_errors(&rows(&[("1A", "x")])),
            vec![Some(RowError::InvalidKey)]
        );
    }

    /// 重复 key:两行都标红(原版同款),按原文比对。
    #[test]
    fn 重复key两行都报() {
        assert_eq!(
            compute_errors(&rows(&[("A", "1"), ("A", "2"), ("B", "3")])),
            vec![
                Some(RowError::DuplicateKey),
                Some(RowError::DuplicateKey),
                None
            ]
        );
    }

    /// value 里的换行 / NUL 一律拦下 —— 它们会把 `key=value` 的注入格式撑破。
    #[test]
    fn value非法字符() {
        for bad in ["a\nb", "a\rb", "a\0b"] {
            assert_eq!(
                compute_errors(&rows(&[("K", bad)])),
                vec![Some(RowError::InvalidValue)],
                "应拒绝 {bad:?}"
            );
        }
        assert_eq!(compute_errors(&rows(&[("K", "a b\tc")])), vec![None]);
    }

    /// 清洗:丢掉 key 空白的占位行,**保留取消勾选的行**(value 留着不注入)。
    #[test]
    fn 清洗保留禁用行只丢空key() {
        let out = clean_rows(&[
            ("A".into(), "1".into(), true),
            ("".into(), "orphan".into(), true),
            ("  ".into(), "".into(), true),
            ("B".into(), "2".into(), false),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].key, "A");
        assert!(out[0].enabled);
        assert_eq!(out[1].key, "B");
        assert!(!out[1].enabled, "取消勾选的行必须留着");
    }

    /// 每个错误档都有对应的字典 key(错档新增时这条会红)。
    #[test]
    fn 每个错误档都有文案key() {
        for err in [
            RowError::EmptyKey,
            RowError::ProtectedPrefix,
            RowError::ReservedWslenv,
            RowError::InvalidKey,
            RowError::DuplicateKey,
            RowError::InvalidValue,
        ] {
            assert!(err.key().starts_with("error."));
        }
    }

    #[test]
    fn wsl路径判定() {
        assert!(is_wsl_path("\\\\wsl$\\Ubuntu\\home\\u"));
        assert!(is_wsl_path("//wsl.localhost/Debian/srv"));
        assert!(!is_wsl_path("D:\\Git\\x"));
    }
}
