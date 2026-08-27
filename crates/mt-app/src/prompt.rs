//! 命令式弹窗:prompt / confirm / alert。对应 `src/utils/prompt.ts`。
//!
//! # 与 `modal.rs` 的分工
//!
//! [`crate::modal`] 里那几个是**有自己表单的**弹窗(终端配置、添加项目);这里是
//! 三个**通用**弹窗 —— 调用点只给标题与文案,不关心怎么画。原版同样是这么分的
//! (`components/*Modal.tsx` vs `utils/prompt.ts`)。
//!
//! 三个都走 [`gpui_component::dialog::Dialog`],因此窗口根视图必须是
//! `gpui_component::Root`(见 `main.rs`)。
//!
//! # 防叠开([`open_guarded`])
//!
//! 审计记的「同一 modal 可叠开(缺 isOpen 守卫)」就修在这儿:`window.open_dialog`
//! 是**栈**,连按两次 Ctrl+, 会摞出两个一模一样的设置框(下面那个永远关不掉,
//! 因为 Esc 只关栈顶)。守卫走 [`crate::overlay`] 那个统一的覆盖物栈:同种类第二次
//! 直接忽略,**不同**种类照样能叠(设置框里再弹确认框是合法的,原版 `prompt.ts`
//! 也专门为此写了栈顶判定)。P 批把右键菜单与三件新浮层一并并进那个栈,
//! 全局快捷键的让路判据从此只有它一处。
//!
//! 摘表放在 `Dialog::on_close` 里 —— 它在确定 / 取消 / Esc / 遮罩 / 关闭按钮
//! **五条路**上都会被调到(见 dialog.rs 的 `render`),不会漏掉某一条把种类
//! 永久钉在表里。**第六条路是程序化关闭**([`close_guarded`]):
//! `window.close_dialog` 只弹 Root 的栈、**不会**触发 `on_close`,所以那条路要
//! 自己摘表,否则该种类再也开不出来。

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    App, AppContext, ClickEvent, Entity, Focusable as _, InteractiveElement, IntoElement,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_component::WindowExt as _;
use gpui_component::dialog::{Dialog, DialogButtonProps};
use gpui_component::input::{Input, InputState, SelectAll};

use crate::i18n::{t, tr};
use crate::overlay;
use crate::ui;

// ─── 防叠开 ───────────────────────────────────────────────────

/// 弹窗种类标识。真身在 [`crate::overlay::kind`](crate::overlay::kind) ——
/// 弹窗只是覆盖物的一种,常量表和右键菜单/浮层共用一份。
pub use crate::overlay::kind;

/// 同种类只允许开一个的 `open_dialog`。`kind` 是种类标识,取值见
/// [`kind`] 模块里的常量 —— 写字面量容易打错,而打错的后果是守卫静默失效。
pub fn open_guarded<F>(kind: &'static str, window: &mut Window, cx: &mut App, build: F)
where
    F: Fn(Dialog, &mut Window, &mut App) -> Dialog + 'static,
{
    open_guarded_with_close(kind, window, cx, build, |_window, _cx| {});
}

fn open_guarded_with_close<F, C>(
    kind: &'static str,
    window: &mut Window,
    cx: &mut App,
    build: F,
    on_close: C,
) -> bool
where
    F: Fn(Dialog, &mut Window, &mut App) -> Dialog + 'static,
    C: Fn(&mut Window, &mut App) + 'static,
{
    if !overlay::push(overlay::key(kind)) {
        on_close(window, cx);
        return false;
    }
    let on_close = Rc::new(on_close);
    window.open_dialog(cx, move |dialog, window, cx| {
        let on_close = on_close.clone();
        // on_close 与 close_button 都放在最后 —— 它们会覆盖 build 里设过的同名
        // 设置:on_close 漏了(摘不掉种类标记)就再也开不出同种类的弹窗;
        // close_button 见 [`dialog_title`] 的注释,画出来是**空白但仍可点**的一块。
        build(dialog, window, cx)
            .close_button(false)
            .on_close(move |_: &ClickEvent, window, cx| {
                overlay::pop(overlay::key(kind));
                on_close(window, cx);
            })
    });
    true
}

/// 主动关掉某种弹窗(Ctrl+Shift+F 第二次按下要能把搜索框关回去)。
///
/// 只在它**正在栈顶**时才动手:上面还压着别人(比如搜索框里又弹了确认框)的话,
/// `window.close_dialog` 关掉的会是那个别人。返回值 = 这次有没有真关。
pub fn close_guarded(kind: &'static str, window: &mut Window, cx: &mut App) -> bool {
    if !overlay::is_top(overlay::key(kind)) {
        return false;
    }
    overlay::pop(overlay::key(kind));
    window.close_dialog(cx);
    true
}

// ─── 标题行 ───────────────────────────────────────────────────

/// 弹窗标题行:标题 + 右上角**自绘**的 ✕。
///
/// **为什么不用 `Dialog::close_button`**:它画的是 `IconName::Close` → `svg()`
/// → `AssetSource`,而本仓没注册任何 asset source(判据见
/// `mt_ui::icons::vector` 模块注释),渲染出来是**空白但仍可点**的一块 ——
/// 用户点得到、看不见。[`open_guarded`] 因此统一把它关掉,需要 ✕ 的弹窗
/// 改用本函数。
///
/// 给**没有底部按钮**的弹窗用(移动端中转、worktree 这类):它们唯一的出口就是
/// 右上角(Esc 也行,但那是看不见的知识)。带「取消」的确认框不必用 —— 底部
/// 那颗就是出口。
///
/// 关窗走 [`close_guarded`](自己摘覆盖物栈的登记),与三个 SSH 弹窗的
/// `panel_header` 同一条路。
pub fn dialog_title(kind: &'static str, title: impl Into<SharedString>) -> impl IntoElement {
    let title = title.into();
    div()
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .child(div().flex_1().truncate().child(title))
        .child(
            div()
                .id(SharedString::from(format!("{kind}-close")))
                .flex_none()
                .w(px(20.0))
                .h(px(20.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .cursor_pointer()
                .hover(|el| el.text_color(ui::text_primary()).bg(ui::bg_overlay()))
                .child("✕")
                .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    close_guarded(kind, window, cx);
                }),
        )
}

/// 这种弹窗现在开着吗。给「开之前/开之后要先做点别的」的调用方提前判一次用:
///
/// - 开之前(见 [`show_prompt`]):建输入框实体这类活儿被守卫拦下就白干了;
/// - 开之后(见 [`autofocus`]):[`open_guarded`] 拦下时**弹窗根本没开**,这时
///   再聚焦等于把焦点送给一个永远不会被画出来的输入框 —— 键盘从此落进虚空,
///   终端一个字也收不到。
///
/// 两头的活儿都靠调用点开头这一句提前 `return` 挡掉,所以它必须在**第一行**。
pub fn is_open(kind: &'static str) -> bool {
    overlay::contains(overlay::key(kind))
}

// ─── 自动聚焦 ─────────────────────────────────────────────────

/// 把焦点交给弹窗里的输入框。**必须排在 [`open_guarded`] 之后**调用。
///
/// # 为什么不能在开弹窗之前聚焦
///
/// `window.open_dialog` 内部有一句 `focus_handle.focus(window)`(gpui-component
/// `root.rs`)——**弹窗一开就把焦点抢到自己的面板上**。而 `Window::focus` 只是
/// 把 `window.focus` 改写成新的 id、后来者无条件覆盖前者,所以「先聚焦输入框、
/// 再开弹窗」等于白聚焦:落地时焦点在 Dialog 面板上。
///
/// 这正是「弹出来整段是选中的、敲字却毫无反应,鼠标点一下才能打」的成因 ——
/// 全选走的是 `focus.dispatch_action(&SelectAll, ..)`,它沿 dispatch tree 找
/// handler、**不要求该节点持有焦点**,所以选区照样画得出来;键盘输入要的却是
/// 真焦点,那时还挂在 Dialog 面板上,键落进的是弹窗而不是输入框。
///
/// # 为什么还要再 defer 一层
///
/// `open_dialog` 的抢焦点发生在 `Root::update` 里,与调用点同处一轮 effect;
/// 排在它后面直接 focus 虽然通常也能赢,但 `window.defer` 让这一手稳稳落在
/// **本轮 effect 全部跑完之后**,不必去推敲弹窗内部还会不会再动焦点。
/// 输入框元素这时尚未画出并不要紧:`window.focus` 只记 id,下一帧
/// `track_focus` 自会接上(`project_switcher` / `search_modal` 一直这么用)。
///
/// # 与 Dialog 键位的关系
///
/// 焦点落到输入框后,Esc / 回车照旧管用:单行 `InputState` 的 `escape` /
/// `enter` 处理器都以 `cx.propagate()` 收尾(注释原话 "e.g.: In a dialog to
/// confirm"),动作继续沿 dispatch tree 冒到外层 Dialog 的 `Cancel` / `Confirm`。
/// 唯一会吞掉 Esc 的是 `clean_on_escape`,本仓一处没用。
pub fn autofocus(input: &Entity<InputState>, window: &mut Window, cx: &mut App) {
    let input = input.clone();
    window.defer(cx, move |window, cx| {
        input.update(cx, |state, cx| state.focus(window, cx));
    });
}

// ─── prompt ───────────────────────────────────────────────────

/// 输入框弹窗,替代 `window.prompt`。
///
/// `on_ok` 只在点「确定」/ 回车时调用,拿到的是**原样**的输入串:
/// 空串是有意义的输入(「清掉描述」),要不要 `trim` / 拒空由调用方决定 ——
/// 原版把这条写进了注释,因为曾经把空串和「取消」一起压成 null,导致
/// 重命名过的终端再也改不回默认名。
pub fn show_prompt(
    title: impl Into<SharedString>,
    placeholder: impl Into<SharedString>,
    default_value: impl Into<SharedString>,
    on_ok: impl Fn(String, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    // 守卫要在**建输入框之前**判:`open_guarded` 里那道判定拦下来的时候弹窗
    // 压根没开,底下那句 `autofocus` 会把焦点送给一个永远不会被画出来的输入框
    if is_open(kind::PROMPT) {
        return;
    }
    let title = title.into();
    let default_value = default_value.into();
    // 原版:`if (defaultValue) input.select()` —— 空默认值不全选(没东西可选)
    let select_all = Cell::new(!default_value.is_empty());
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder.into())
            .default_value(default_value)
    });
    let on_ok = Rc::new(on_ok);
    // 打开即可直接打字,不必先点一下输入框。真正的聚焦排在 `open_guarded`
    // **之后**(见 [`autofocus`]),这里只是先留一份引用
    let input_for_focus = input.clone();

    open_guarded(kind::PROMPT, window, cx, move |dialog, window, cx| {
        // 「有默认值就全选」(重命名多半是整个换掉)。`InputState::select_all`
        // 是 `pub(super)`,但它是 `input::SelectAll` 这个**公开 action** 的
        // handler —— 把动作派发到输入框的焦点节点就等价于用户按了 Ctrl+A。
        // 时机是唯一的坑:`dispatch_action` 查的是 `rendered_frame` 的 dispatch
        // tree,而输入框这一刻(builder 正在组装本帧的元素)还没画出来,所以挂
        // `on_next_frame` —— 它在下一帧开画前跑,那时 `rendered_frame` 正是含
        // 输入框的这一帧。手法与 `project_list::start_rename` 同一条。
        //
        // builder 每帧都会被 Root 调一遍,`Cell` 保证只派发**第一帧**那一次。
        if select_all.take() {
            let focus = input.read(cx).focus_handle(cx);
            window.on_next_frame(move |window, cx| {
                focus.dispatch_action(&SelectAll, window, cx);
            });
        }
        let input_for_ok = input.clone();
        let on_ok = on_ok.clone();
        dialog
            .title(title.clone())
            .w(px(360.0))
            .confirm()
            // 遮罩点击 = 取消(原版 prompt-overlay 的点击行为)
            .overlay_closable(true)
            .button_props(
                DialogButtonProps::default()
                    .ok_text(t("prompt", "confirm"))
                    .cancel_text(t("prompt", "cancel")),
            )
            .child(div().px(px(20.0)).child(Input::new(&input)))
            .on_ok(move |_: &ClickEvent, window, cx| {
                let value = input_for_ok.read(cx).value().to_string();
                on_ok(value, window, cx);
                true
            })
    });

    autofocus(&input_for_focus, window, cx);
}

// ─── confirm ──────────────────────────────────────────────────

/// 确认框。参数够多,走 builder 而不是一长串位置参数。
pub struct Confirm {
    title: SharedString,
    message: SharedString,
    /// 正文下面的补充行(灰字,一行一条)。原版没有这一段;
    /// 「关整组」要在这里列出正在跑 AI 的终端名。
    detail: Vec<String>,
    ok_text: SharedString,
    cancel_text: SharedString,
}

impl Confirm {
    pub fn new(title: impl Into<SharedString>, message: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            detail: Vec::new(),
            ok_text: t("prompt", "confirm").into(),
            cancel_text: t("prompt", "cancel").into(),
        }
    }

    pub fn detail(mut self, lines: Vec<String>) -> Self {
        self.detail = lines;
        self
    }

    pub fn ok_text(mut self, text: impl Into<SharedString>) -> Self {
        self.ok_text = text.into();
        self
    }

    pub fn cancel_text(mut self, text: impl Into<SharedString>) -> Self {
        self.cancel_text = text.into();
        self
    }

    /// 弹出来。`on_ok` 只在点「确定」时调用。
    pub fn open(
        self,
        on_ok: impl Fn(&mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) {
        let on_ok = Rc::new(on_ok);
        open_guarded(kind::CONFIRM, window, cx, move |dialog, _window, _cx| {
            let on_ok = on_ok.clone();
            dialog
                .title(self.title.clone())
                // 原版 `.prompt-dialog` 三件套(prompt/confirm/alert)统一 360px
                .w(px(360.0))
                .confirm()
                .overlay_closable(true)
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(self.ok_text.clone())
                        .cancel_text(self.cancel_text.clone()),
                )
                .child(body(&self.message, &self.detail))
                .on_ok(move |_: &ClickEvent, window, cx| {
                    on_ok(window, cx);
                    true
                })
        });
    }
}

// ─── alert ────────────────────────────────────────────────────

/// 只有一个「知道了」的提示框,替代 `window.alert`(原版失败提示走的
/// Tauri `message()`,这里统一收到自己的弹窗里)。
pub fn show_alert(
    title: impl Into<SharedString>,
    message: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) {
    let title = title.into();
    let message = message.into();
    open_guarded(kind::ALERT, window, cx, move |dialog, _window, _cx| {
        dialog
            .title(title.clone())
            // 原版 `.prompt-dialog` 三件套(prompt/confirm/alert)统一 360px
            .w(px(360.0))
            .alert()
            .button_props(DialogButtonProps::default().ok_text(t("prompt", "ok")))
            .child(body(&message, &[]))
    });
}

const FILE_CONFLICT_PREVIEW_LIMIT: usize = 5;

fn file_conflict_preview(conflicts: &[String]) -> (Vec<String>, usize) {
    let preview = conflicts
        .iter()
        .take(FILE_CONFLICT_PREVIEW_LIMIT)
        .cloned()
        .collect::<Vec<_>>();
    let remaining = conflicts.len().saturating_sub(preview.len());
    (preview, remaining)
}

/// 上传/下载遇到同名目标时的三选一弹窗。点击遮罩或 Esc 等同取消。
pub fn show_file_conflict_choice(
    conflicts: Vec<String>,
    on_choice: impl Fn(crate::remote_ssh::FileConflictStrategy, &mut Window, &mut App) + 'static,
    on_cancel: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let (preview, remaining) = file_conflict_preview(&conflicts);
    let mut details = preview
        .into_iter()
        .map(|name| format!("• {name}"))
        .collect::<Vec<_>>();
    if remaining > 0 {
        details.push(tr!("fileTree", "conflict.remaining", count = remaining));
    }
    let message = t("fileTree", "conflict.message");
    let on_choice = Rc::new(on_choice);
    open_guarded_with_close(
        kind::FILE_CONFLICT,
        window,
        cx,
        move |dialog, _window, _cx| {
            let button = |id: &'static str,
                          label: SharedString,
                          strategy: crate::remote_ssh::FileConflictStrategy,
                          primary: bool| {
                let on_choice = on_choice.clone();
                let el = if primary {
                    ui::primary_button(id, label)
                } else {
                    ui::ghost_button(id, label)
                };
                el.on_click(move |_: &ClickEvent, window, cx| {
                    close_guarded(kind::FILE_CONFLICT, window, cx);
                    on_choice(strategy, window, cx);
                })
            };
            dialog
                .title(t("fileTree", "conflict.title"))
                .w(px(420.0))
                .overlay_closable(true)
                .child(
                    div()
                        .pb(px(16.0))
                        .flex()
                        .flex_col()
                        .gap(px(14.0))
                        .child(body(message, &details))
                        .child(
                            div()
                                .px(px(20.0))
                                .flex()
                                .items_center()
                                .justify_end()
                                .gap(px(8.0))
                                .child(button(
                                    "file-conflict-skip",
                                    t("fileTree", "conflict.skip").into(),
                                    crate::remote_ssh::FileConflictStrategy::Skip,
                                    false,
                                ))
                                .child(button(
                                    "file-conflict-keep-both",
                                    t("fileTree", "conflict.keepBoth").into(),
                                    crate::remote_ssh::FileConflictStrategy::KeepBoth,
                                    false,
                                ))
                                .child(button(
                                    "file-conflict-overwrite",
                                    t("fileTree", "conflict.overwrite").into(),
                                    crate::remote_ssh::FileConflictStrategy::Overwrite,
                                    true,
                                )),
                        ),
                )
        },
        on_cancel,
    );
}

/// 正文 + 补充行。文案里的 `\n` 要真换行(确认框普遍用它排版),
/// 而 gpui 的文本不认转义符,得自己拆成多个 child。
fn body(message: &str, detail: &[String]) -> gpui::AnyElement {
    let mut el = div().px(px(20.0)).flex().flex_col().gap(px(4.0));
    for line in message.split('\n') {
        el = el.child(
            div()
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_primary())
                // 空行也要占一行高,不然 `\n\n` 排版会塌掉
                .child(if line.is_empty() {
                    SharedString::from(" ")
                } else {
                    SharedString::from(line.to_string())
                }),
        );
    }
    if !detail.is_empty() {
        let mut list = div().mt(px(4.0)).flex().flex_col().gap(px(2.0));
        for line in detail {
            list = list.child(
                div()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .child(line.clone()),
            );
        }
        el = el.child(list);
    }
    el.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_conflict_preview_lists_at_most_five_names() {
        let conflicts = (1..=7)
            .map(|index| format!("file-{index}.txt"))
            .collect::<Vec<_>>();
        let (preview, remaining) = file_conflict_preview(&conflicts);

        assert_eq!(preview.as_slice(), &conflicts[..5]);
        assert_eq!(remaining, 2);
    }

    #[test]
    fn file_conflict_preview_keeps_short_lists_complete() {
        let conflicts = vec!["a.txt".to_string(), "b.txt".to_string()];
        let (preview, remaining) = file_conflict_preview(&conflicts);

        assert_eq!(preview, conflicts);
        assert_eq!(remaining, 0);
    }
}
