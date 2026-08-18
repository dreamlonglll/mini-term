//! Modal 批:终端配置 / 重命名 / 移除项目确认 / 添加项目。
//!
//! 全部走 [`gpui_component::dialog::Dialog`] + [`gpui_component::input`],
//! 因此窗口的根视图必须是 `gpui_component::Root`(见 `main.rs`)—— Input 内部会
//! `Root::update` 登记当前焦点输入框,不是 Root 会直接 panic。
//!
//! # 与旧版的对照
//!
//! | 旧版 | 这里 |
//! |---|---|
//! | `SettingsModal.tsx` 的 `TerminalSettings`(shell 列表 + 默认项) | [`open_terminal_settings`] |
//! | tab 右键菜单 → 重命名 | [`open_rename_pane`] |
//! | 移除项目的确认框(收编 `project_list` 的「点两次」临时方案) | [`open_confirm_remove_project`] |
//! | 添加项目(目录选择) | [`open_add_project`] |
//!
//! # 状态放在哪
//!
//! Dialog 的 builder 是 `Fn`,**每帧都会被重新调用**,不能把编辑中的表单状态藏在
//! 闭包捕获的普通变量里。表单状态一律放进 `Entity`:gpui 会把渲染期间读过的
//! entity 记进窗口的失效表,`cx.notify()` 即触发重画(见 `App::notify`)。

use gpui::{
    App, AppContext, ClickEvent, Entity, InteractiveElement, IntoElement, ParentElement,
    PathPromptOptions, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::WindowExt as _;
use gpui_component::dialog::DialogButtonProps;
use gpui_component::input::{Input, InputState};
use mt_config::ShellConfig;

use crate::shell_ops::{parse_args, valid_shell};
use crate::store::AppStore;
use crate::ui;

// ─── 终端配置 ─────────────────────────────────────────────────

/// 终端配置对话框里那份「正在编辑的行」。
struct ShellForm {
    /// `None` = 表单没打开;`Some(None)` = 新增;`Some(Some(i))` = 编辑第 i 行。
    editing: Option<Option<usize>>,
    name: Entity<InputState>,
    command: Entity<InputState>,
    args: Entity<InputState>,
    /// 名字/命令为空时的提示(旧版是按钮直接不响应,这里明说为什么)。
    error: Option<&'static str>,
}

impl ShellForm {
    fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            editing: None,
            name: cx.new(|cx| InputState::new(window, cx).placeholder("名称,如 PowerShell")),
            command: cx
                .new(|cx| InputState::new(window, cx).placeholder("可执行文件,如 pwsh.exe")),
            args: cx.new(|cx| InputState::new(window, cx).placeholder("参数(空格分隔,可留空)")),
            error: None,
        }
    }

    fn fill(&mut self, shell: Option<&ShellConfig>, window: &mut Window, cx: &mut App) {
        let (name, command, args) = match shell {
            Some(s) => (
                s.name.clone(),
                s.command.clone(),
                s.args.clone().unwrap_or_default().join(" "),
            ),
            None => (String::new(), String::new(), String::new()),
        };
        self.name
            .update(cx, |s, cx| s.set_value(name, window, cx));
        self.command
            .update(cx, |s, cx| s.set_value(command, window, cx));
        self.args
            .update(cx, |s, cx| s.set_value(args, window, cx));
        self.error = None;
    }

    fn to_shell(&self, cx: &App) -> Option<ShellConfig> {
        let name = self.name.read(cx).value().trim().to_string();
        let command = self.command.read(cx).value().trim().to_string();
        if !valid_shell(&name, &command) {
            return None;
        }
        Some(ShellConfig {
            name,
            command,
            args: parse_args(&self.args.read(cx).value()),
        })
    }
}

/// 打开「终端配置」。
pub fn open_terminal_settings(store: Entity<AppStore>, window: &mut Window, cx: &mut App) {
    let form = cx.new(|cx| ShellForm::new(window, cx));
    window.open_dialog(cx, move |dialog, window, cx| {
        let body = render_terminal_settings(&store, &form, window, cx);
        dialog
            .title("终端配置")
            .w(px(560.0))
            .child(div().px(px(20.0)).child(body))
    });
}

fn render_terminal_settings(
    store: &Entity<AppStore>,
    form: &Entity<ShellForm>,
    _window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    let list = store.read(cx).shell_list();
    let font_size = store.read(cx).config().terminal_font_size;
    let editing = form.read(cx).editing;

    let mut rows = div().flex().flex_col().gap(px(4.0));
    for (idx, shell) in list.shells.iter().enumerate() {
        // 这一行正在编辑 → 让位给表单,不重复画
        if editing == Some(Some(idx)) {
            rows = rows.child(render_shell_form(store, form, cx));
            continue;
        }
        let is_default = shell.name == list.default_shell;
        let detail = match &shell.args {
            Some(args) if !args.is_empty() => format!("{} {}", shell.command, args.join(" ")),
            _ => shell.command.clone(),
        };

        let store_default = store.clone();
        let name_for_default = shell.name.clone();
        let form_edit = form.clone();
        let store_edit = store.clone();
        let store_delete = store.clone();
        let form_delete = form.clone();

        rows = rows.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(10.0))
                .py(px(6.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(ui::border_subtle())
                .bg(ui::bg_base())
                // 默认项单选点
                .child(
                    div()
                        .id(SharedString::from(format!("shell-default-{idx}")))
                        .w(px(12.0))
                        .h(px(12.0))
                        .flex_none()
                        .rounded_full()
                        .border_2()
                        .border_color(if is_default {
                            ui::accent()
                        } else {
                            ui::border_default()
                        })
                        .when(is_default, |el| el.bg(ui::accent()))
                        .cursor_pointer()
                        .on_click(move |_, _window, cx| {
                            let name = name_for_default.clone();
                            store_default.update(cx, |store, cx| {
                                let mut list = store.shell_list();
                                list.set_default(&name);
                                store.apply_shell_list(list, cx);
                            });
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(13.0))
                                .text_color(ui::text_primary())
                                .child(shell.name.clone()),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(11.0))
                                .text_color(ui::text_muted())
                                .child(detail),
                        ),
                )
                .child(
                    ui::ghost_button(SharedString::from(format!("shell-edit-{idx}")), "编辑")
                        .on_click(move |_, window, cx| {
                            let shell = store_edit
                                .read(cx)
                                .config()
                                .available_shells
                                .get(idx)
                                .cloned();
                            form_edit.update(cx, |form, cx| {
                                form.editing = Some(Some(idx));
                                form.fill(shell.as_ref(), window, cx);
                                cx.notify();
                            });
                        }),
                )
                .child(
                    ui::danger_button(SharedString::from(format!("shell-del-{idx}")), "删除")
                        .on_click(move |_, _window, cx| {
                            store_delete.update(cx, |store, cx| {
                                let mut list = store.shell_list();
                                list.remove(idx);
                                store.apply_shell_list(list, cx);
                            });
                            // 编辑中的行号会被这次删除搞错位,一并收掉表单
                            form_delete.update(cx, |form, cx| {
                                form.editing = None;
                                cx.notify();
                            });
                        }),
                ),
        );
    }

    if editing == Some(None) {
        rows = rows.child(render_shell_form(store, form, cx));
    }

    let form_add = form.clone();
    let store_font_dec = store.clone();
    let store_font_inc = store.clone();

    div()
        .flex()
        .flex_col()
        .gap(px(14.0))
        .child(
            div()
                .child(ui::section_title("可用终端"))
                .child(rows)
                .child(
                    div().mt(px(8.0)).child(
                        ui::ghost_button("shell-add", "+ 添加终端").on_click(
                            move |_, window, cx| {
                                form_add.update(cx, |form, cx| {
                                    form.editing = Some(None);
                                    form.fill(None, window, cx);
                                    cx.notify();
                                });
                            },
                        ),
                    ),
                ),
        )
        .child(
            div()
                .child(ui::section_title("字号"))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .child(ui::ghost_button("font-dec", "−").on_click(move |_, _window, cx| {
                            store_font_dec.update(cx, |store, cx| {
                                let now = store.config().terminal_font_size;
                                store.set_terminal_font_size(now - 1.0, cx);
                            });
                        }))
                        .child(
                            div()
                                .w(px(44.0))
                                .text_size(px(13.0))
                                .text_color(ui::text_primary())
                                .child(format!("{font_size:.0} px")),
                        )
                        .child(ui::ghost_button("font-inc", "+").on_click(move |_, _window, cx| {
                            store_font_inc.update(cx, |store, cx| {
                                let now = store.config().terminal_font_size;
                                store.set_terminal_font_size(now + 1.0, cx);
                            });
                        }))
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(ui::text_muted())
                                .child("改动作用于新建的终端"),
                        ),
                ),
        )
        .into_any_element()
}

fn render_shell_form(
    store: &Entity<AppStore>,
    form: &Entity<ShellForm>,
    cx: &mut App,
) -> gpui::AnyElement {
    let f = form.read(cx);
    let (name, command, args) = (f.name.clone(), f.command.clone(), f.args.clone());
    let error = f.error;

    let store_save = store.clone();
    let form_save = form.clone();
    let form_cancel = form.clone();

    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .p(px(10.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(ui::accent())
        .bg(ui::bg_base())
        .child(Input::new(&name))
        .child(Input::new(&command))
        .child(Input::new(&args))
        .when_some(error, |el, msg| {
            el.child(
                div()
                    .text_size(px(11.0))
                    .text_color(ui::color_error())
                    .child(msg),
            )
        })
        .child(
            div()
                .flex()
                .gap(px(6.0))
                .child(ui::primary_button("shell-save", "保存").on_click(
                    move |_, _window, cx| {
                        let Some(shell) = form_save.read(cx).to_shell(cx) else {
                            form_save.update(cx, |form, cx| {
                                form.error = Some("名称与可执行文件都不能为空");
                                cx.notify();
                            });
                            return;
                        };
                        let editing = form_save.read(cx).editing;
                        store_save.update(cx, |store, cx| {
                            let mut list = store.shell_list();
                            match editing {
                                Some(Some(idx)) => list.update(idx, shell),
                                _ => list.add(shell),
                            }
                            store.apply_shell_list(list, cx);
                        });
                        form_save.update(cx, |form, cx| {
                            form.editing = None;
                            cx.notify();
                        });
                    },
                ))
                .child(ui::ghost_button("shell-cancel", "取消").on_click(
                    move |_, _window, cx| {
                        form_cancel.update(cx, |form, cx| {
                            form.editing = None;
                            cx.notify();
                        });
                    },
                )),
        )
        .into_any_element()
}

// ─── 重命名 ───────────────────────────────────────────────────

/// 重命名一个终端 tab。留空 = 恢复默认(shell 名)。
///
/// **不落盘**:`SavedPane` 里没有 customTitle 字段,装机版同样只在运行时保留 ——
/// 磁盘格式一字不改是这次迁移的红线。
pub fn open_rename_pane(
    store: Entity<AppStore>,
    project_id: String,
    pane_id: String,
    current: String,
    window: &mut Window,
    cx: &mut App,
) {
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder("留空恢复默认名称")
            .default_value(current)
    });
    // 打开即可直接改名,不必先点一下输入框
    input.update(cx, |state, cx| state.focus(window, cx));

    window.open_dialog(cx, move |dialog, _window, _cx| {
        let store = store.clone();
        let project_id = project_id.clone();
        let pane_id = pane_id.clone();
        let input_for_ok = input.clone();
        dialog
            .title("重命名标签")
            .w(px(380.0))
            .confirm()
            .button_props(DialogButtonProps::default().ok_text("确定").cancel_text("取消"))
            .child(div().px(px(20.0)).child(Input::new(&input)))
            .on_ok(move |_: &ClickEvent, _window, cx| {
                let title = input_for_ok.read(cx).value().to_string();
                store.update(cx, |store, cx| {
                    store.rename_pane(&project_id, &pane_id, &title, cx)
                });
                true
            })
    });
}

// ─── 移除项目确认 ─────────────────────────────────────────────

/// 移除项目前的确认。
///
/// 收编 `project_list` 里那个「点两次才真删」的临时方案:移除是不可逆的
/// (配置里的布局、展开目录一起没),必须让用户看清楚删的是哪一个。
pub fn open_confirm_remove_project(
    store: Entity<AppStore>,
    project_id: String,
    project_name: String,
    project_path: String,
    window: &mut Window,
    cx: &mut App,
) {
    window.open_dialog(cx, move |dialog, _window, _cx| {
        let store = store.clone();
        let project_id = project_id.clone();
        dialog
            .title("移除项目")
            .w(px(420.0))
            .confirm()
            .button_props(
                DialogButtonProps::default()
                    .ok_text("移除")
                    .cancel_text("取消"),
            )
            .child(
                div()
                    .px(px(20.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(ui::text_primary())
                            .child(format!("从列表中移除「{project_name}」?")),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(ui::text_muted())
                            .child(project_path.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(ui::text_secondary())
                            .child("该项目的终端会被关闭,保存的分屏布局与展开目录一并清除。磁盘上的文件不受影响。"),
                    ),
            )
            .on_ok(move |_: &ClickEvent, _window, cx| {
                store.update(cx, |store, cx| store.remove_project(&project_id, cx));
                true
            })
    });
}

// ─── 添加项目 ─────────────────────────────────────────────────

/// 添加项目:路径输入 + 「浏览…」调平台目录选择框。
///
/// gpui 直接给了 `prompt_for_paths`,不必自己造;手输那一路留着,是因为 UNC /
/// WSL 路径在目录选择框里常常点不到。
pub fn open_add_project(store: Entity<AppStore>, window: &mut Window, cx: &mut App) {
    let input = cx.new(|cx| {
        InputState::new(window, cx).placeholder("项目目录,如 D:\\Git\\mini-term")
    });
    input.update(cx, |state, cx| state.focus(window, cx));

    window.open_dialog(cx, move |dialog, _window, _cx| {
        let store = store.clone();
        let input_for_ok = input.clone();
        let input_for_browse = input.clone();
        dialog
            .title("添加项目")
            .w(px(460.0))
            .confirm()
            .button_props(
                DialogButtonProps::default()
                    .ok_text("添加")
                    .cancel_text("取消"),
            )
            .child(
                div()
                    .px(px(20.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.0))
                            .child(div().flex_1().child(Input::new(&input)))
                            .child(ui::ghost_button("browse-dir", "浏览…").on_click(
                                move |_, window, cx| {
                                    let paths = cx.prompt_for_paths(PathPromptOptions {
                                        files: false,
                                        directories: true,
                                        multiple: false,
                                        prompt: Some("选择项目目录".into()),
                                    });
                                    let input = input_for_browse.clone();
                                    window
                                        .spawn(cx, async move |cx| {
                                            let Ok(Ok(Some(paths))) = paths.await else {
                                                return;
                                            };
                                            let Some(path) = paths.into_iter().next() else {
                                                return;
                                            };
                                            let text = path.to_string_lossy().to_string();
                                            let _ = cx.update(|window, cx| {
                                                input.update(cx, |state, cx| {
                                                    state.set_value(text, window, cx)
                                                });
                                            });
                                        })
                                        .detach();
                                },
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(ui::text_muted())
                            .child("目录不存在时不会添加。"),
                    ),
            )
            .on_ok(move |_: &ClickEvent, _window, cx| {
                let raw = input_for_ok.read(cx).value().trim().to_string();
                let path = std::path::PathBuf::from(&raw);
                // 目录不存在就把对话框留着 —— 关掉的话用户刚打的路径就没了
                if raw.is_empty() || !path.is_dir() {
                    return false;
                }
                store.update(cx, |store, cx| store.add_project(&path, cx));
                true
            })
    });
}
