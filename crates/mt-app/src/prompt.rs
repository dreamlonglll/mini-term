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
//! 因为 Esc 只关栈顶)。这里按 `kind` 记一张「开着的种类」表:同种类第二次直接
//! 忽略,**不同**种类照样能叠(设置框里再弹确认框是合法的,原版 `prompt.ts` 也
//! 专门为此写了栈顶判定)。
//!
//! 摘表放在 `Dialog::on_close` 里 —— 它在确定 / 取消 / Esc / 遮罩 / 关闭按钮
//! **五条路**上都会被调到(见 dialog.rs 的 `render`),不会漏掉某一条把种类
//! 永久钉在表里。

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{
    App, AppContext, ClickEvent, Global, IntoElement, ParentElement, SharedString, Styled, Window,
    div, px,
};
use gpui_component::WindowExt as _;
use gpui_component::dialog::{Dialog, DialogButtonProps};
use gpui_component::input::{Input, InputState};

use crate::i18n::t;
use crate::ui;

// ─── 防叠开 ───────────────────────────────────────────────────

/// 当前开着的弹窗种类。
#[derive(Default)]
struct OpenDialogs(HashSet<&'static str>);
impl Global for OpenDialogs {}

/// 同种类只允许开一个的 `open_dialog`。`kind` 是种类标识,取值见
/// [`kind`] 模块里的常量 —— 写字面量容易打错,而打错的后果是守卫静默失效。
pub fn open_guarded<F>(kind: &'static str, window: &mut Window, cx: &mut App, build: F)
where
    F: Fn(Dialog, &mut Window, &mut App) -> Dialog + 'static,
{
    if !cx.default_global::<OpenDialogs>().0.insert(kind) {
        return;
    }
    window.open_dialog(cx, move |dialog, window, cx| {
        // on_close 放在最后 —— 它会覆盖 build 里设过的同名回调,而这一条
        // (摘掉种类标记)漏了就再也开不出同种类的弹窗了
        build(dialog, window, cx).on_close(move |_: &ClickEvent, _window, cx| {
            cx.default_global::<OpenDialogs>().0.remove(kind);
        })
    });
}

/// 这种弹窗现在开着吗。给「开之前要先做点别的」的调用方提前判一次用
/// (见 [`show_prompt`]:它得先建输入框实体并抢焦点,被守卫拦下就白抢了)。
fn is_open(kind: &'static str, cx: &mut App) -> bool {
    cx.default_global::<OpenDialogs>().0.contains(kind)
}

/// 弹窗种类标识。一个常量 = 一种「同时只能开一个」的弹窗。
pub mod kind {
    pub const SETTINGS: &str = "settings";
    pub const ADD_PROJECT: &str = "add-project";
    pub const RENAME_PANE: &str = "rename-pane";
    pub const REMOVE_PROJECT: &str = "remove-project";
    /// 通用输入框(重命名 / 新建文件 / 编辑描述…)。
    pub const PROMPT: &str = "prompt";
    /// 通用确认框(关终端 / 删文件…)。
    pub const CONFIRM: &str = "confirm";
    /// 通用提示框(操作失败)。
    pub const ALERT: &str = "alert";
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
    // 守卫要在**建输入框之前**判:open_guarded 里那道判定拦下来的时候,
    // 焦点已经被下面这个新输入框抢走了(而它永远不会被画出来)
    if is_open(kind::PROMPT, cx) {
        return;
    }
    let title = title.into();
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder.into())
            .default_value(default_value.into())
    });
    // 打开即可直接打字,不必先点一下输入框。
    //
    // ⚠️ 原版还会在有默认值时 `input.select()` 全选(重命名多半是整个换掉),
    // 这里做不到:`InputState::select_all` 是 `pub(super)`,组件库没给公开入口 ——
    // 只能靠用户自己 Ctrl+A。缺口已记入报告,与 `modal::open_rename_pane` 同一条。
    input.update(cx, |state, cx| state.focus(window, cx));
    let on_ok = Rc::new(on_ok);

    open_guarded(kind::PROMPT, window, cx, move |dialog, _window, _cx| {
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
                .w(px(380.0))
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
            .w(px(380.0))
            .alert()
            .button_props(DialogButtonProps::default().ok_text(t("prompt", "ok")))
            .child(body(&message, &[]))
    });
}

/// 正文 + 补充行。文案里的 `\n` 要真换行(确认框普遍用它排版),
/// 而 gpui 的文本不认转义符,得自己拆成多个 child。
fn body(message: &str, detail: &[String]) -> gpui::AnyElement {
    let mut el = div().px(px(20.0)).flex().flex_col().gap(px(4.0));
    for line in message.split('\n') {
        el = el.child(
            div()
                .text_size(px(13.0))
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
                    .text_size(px(11.0))
                    .text_color(ui::text_muted())
                    .child(line.clone()),
            );
        }
        el = el.child(list);
    }
    el.into_any_element()
}
