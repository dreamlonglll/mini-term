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

use crate::i18n::{Locale, t};
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
            name: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t("settings", "terminal.newNamePlaceholder"))
            }),
            command: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(t("settings", "terminal.newCommandPlaceholder"))
            }),
            args: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t("settings", "terminal.newArgsPlaceholder"))
            }),
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
        // 占位串在 `new` 里只取过一次;这里重设一遍,免得对话框开着的时候切了
        // 语言(语言段控件就在同一个对话框里),下次点「添加终端」还是旧语言。
        self.name.update(cx, |s, cx| {
            s.set_placeholder(t("settings", "terminal.newNamePlaceholder"), window, cx);
            s.set_value(name, window, cx);
        });
        self.command.update(cx, |s, cx| {
            s.set_placeholder(t("settings", "terminal.newCommandPlaceholder"), window, cx);
            s.set_value(command, window, cx);
        });
        self.args.update(cx, |s, cx| {
            s.set_placeholder(t("settings", "terminal.newArgsPlaceholder"), window, cx);
            s.set_value(args, window, cx);
        });
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
            .title(t("settings", "title"))
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
                    ui::ghost_button(
                        SharedString::from(format!("shell-edit-{idx}")),
                        t("settings", "common.edit"),
                    )
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
                    ui::danger_button(
                        SharedString::from(format!("shell-del-{idx}")),
                        t("settings", "common.delete"),
                    )
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
        .child(render_language_section(store, cx))
        .child(
            div()
                .child(ui::section_title(t("settings", "terminal.availableTerminals")))
                .child(rows)
                .child(
                    div().mt(px(8.0)).child(
                        ui::ghost_button("shell-add", t("settings", "terminal.addTerminal"))
                            .on_click(move |_, window, cx| {
                                form_add.update(cx, |form, cx| {
                                    form.editing = Some(None);
                                    form.fill(None, window, cx);
                                    cx.notify();
                                });
                            }),
                    ),
                ),
        )
        .child(
            div()
                .child(ui::section_title(t("settings", "font.terminalFontSize")))
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
                                // 原版终端字号是热更新的,没有这句提示;
                                // `settings.terminal.fontSizeNewOnly` 是 M 批补的条目。
                                .child(t("settings", "terminal.fontSizeNewOnly")),
                        ),
                ),
        )
        .into_any_element()
}

/// 语言切换段控件。逐条对照 `src/components/LanguageToggle.tsx`:
/// 两个选项、各写各自的母语名(中文 / English —— endonym 不翻译)、
/// 选中项 accent 底色白字,未选中透明底淡字。位置也照搬 ——
/// 原版挂在设置面板「主题与语言」页的第一节(`SettingsModal.tsx` 的
/// `<Section title={t('settings.appearance.language')}>`),GPUI 的设置对话框
/// 目前只有这一页,于是放在最上面。
fn render_language_section(store: &Entity<AppStore>, cx: &mut App) -> gpui::AnyElement {
    let current = store.read(cx).locale();

    let mut seg = div()
        .flex()
        .rounded(px(4.0))
        .overflow_hidden()
        .border_1()
        .border_color(ui::border_default());
    for option in Locale::ALL {
        let active = option == current;
        let store = store.clone();
        seg = seg.child(
            div()
                .id(SharedString::from(format!("lang-{}", option.code())))
                .px(px(12.0))
                .py(px(3.0))
                .text_size(px(12.0))
                .cursor_pointer()
                .when(active, |el| {
                    el.bg(ui::accent()).text_color(ui::bg_base())
                })
                .when(!active, |el| {
                    el.text_color(ui::text_muted())
                        .hover(|el| el.text_color(ui::text_primary()))
                })
                .on_click(move |_, _window, cx| {
                    store.update(cx, |store, cx| store.set_locale(option, cx));
                })
                // 永远显示母语名,不随当前语言变
                .child(option.native_name()),
        );
    }

    div()
        .child(ui::section_title(t("settings", "appearance.language")))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(ui::text_secondary())
                        .child(t("settings", "appearance.languageLabel")),
                )
                .child(seg),
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
                .child(ui::primary_button("shell-save", t("settings", "common.save")).on_click(
                    move |_, _window, cx| {
                        let Some(shell) = form_save.read(cx).to_shell(cx) else {
                            form_save.update(cx, |form, cx| {
                                // 原版是「名字/命令为空时保存按钮直接不响应」,没有这句
                                // 提示文案 —— 借用 envVars 里语义最近的那条通用校验串。
                                form.error = Some(t("envVars", "hasErrors"));
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
                .child(ui::ghost_button("shell-cancel", t("settings", "common.cancel")).on_click(
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
            .placeholder(t("fileTree", "prompt.renameMessage"))
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
            .title(t("paneGroup", "renameTerminal"))
            .w(px(380.0))
            .confirm()
            .button_props(
                DialogButtonProps::default()
                    .ok_text(t("prompt", "confirm"))
                    .cancel_text(t("prompt", "cancel")),
            )
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
            .title(t("projectList", "removeConfirm.title"))
            .w(px(420.0))
            .confirm()
            .button_props(
                DialogButtonProps::default()
                    .ok_text(t("projectList", "removeConfirm.confirm"))
                    .cancel_text(t("projectList", "removeConfirm.cancel")),
            )
            .child(
                div()
                    .px(px(20.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    // 正文与原版一样是「前缀 + 项目名 + 后缀」三段拼(后缀那半句
                    // 已经把"只从列表移除、不删文件"说清楚了,不必另起一行)
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(ui::text_primary())
                            .child(format!(
                                "{}{}{}",
                                t("projectList", "removeConfirm.messagePrefix"),
                                project_name,
                                t("projectList", "removeConfirm.messageSuffix"),
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(ui::text_muted())
                            .child(project_path.clone()),
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
    // 原版加项目走的是系统目录选择框,没有手输框;这条占位串与下面的路径提示
    // 是 GPUI 侧独有的,`projectList.{pathPlaceholder,pathHint}` 由 M 批补进 TS 源头。
    let input =
        cx.new(|cx| InputState::new(window, cx).placeholder(t("projectList", "pathPlaceholder")));
    input.update(cx, |state, cx| state.focus(window, cx));

    window.open_dialog(cx, move |dialog, _window, _cx| {
        let store = store.clone();
        let input_for_ok = input.clone();
        let input_for_browse = input.clone();
        dialog
            .title(t("projectList", "menu.addProject"))
            .w(px(460.0))
            .confirm()
            .button_props(
                DialogButtonProps::default()
                    .ok_text(t("settings", "common.add"))
                    .cancel_text(t("settings", "common.cancel")),
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
                            .child(ui::ghost_button("browse-dir", t("worktree", "browse")).on_click(
                                move |_, window, cx| {
                                    let paths = cx.prompt_for_paths(PathPromptOptions {
                                        files: false,
                                        directories: true,
                                        multiple: false,
                                        // 系统目录选择框的标题。原版用的是 Tauri
                                        // 的默认标题,这条 key 是 M 批新补的
                                        prompt: Some(
                                            t("projectList", "chooseDirDialogTitle").into(),
                                        ),
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
                            // 见上面手输框的说明,原版没有这条提示。
                            .child(t("projectList", "pathHint")),
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
