//! 一个项目都没有时的首屏(对照 `src/components/FirstRunGuide.tsx`)。
//!
//! # 它不是弹窗
//!
//! 原版没有「首启标记」这类持久化字段,也没有关闭按钮 —— 触发条件就一个:
//! `config.projects.length === 0`(`App.tsx:534`),渲染位置是终端那一栏的空白处。
//! 添完项目它自然消失,删光项目它自然回来。所以 GPUI 侧同样**不进
//! [`crate::overlay`] 覆盖物栈、不走 [`crate::prompt`]**,就是
//! [`TerminalArea`](crate::terminal_area::TerminalArea) 的一个早退分支。
//!
//! # 与原版的一处偏差(有理由,见报告)
//!
//! **「添加本地项目」走 [`crate::modal::open_add_project`]**(路径输入框 +
//! 「浏览…」)而不是像原版那样直接弹系统目录选择框。理由是 GPUI 侧「添加项目」
//! 只该有一条路:项目列表底部那颗按钮已经是这个弹窗,而它多出来的手输那一路
//! 是有意保留的(UNC / WSL 路径在目录选择框里常常点不到,见 `modal.rs`)。
//! 同一个动作在两处表现不同才是真的坏体验。
//!
//! 第二颗按钮「添加 SSH 远程项目」由 BB-b 补上,走
//! [`crate::remote_project::open`] —— 与项目列表底部那颗 `SSH` 钮同一种覆盖物,
//! 两处入口不会各开一个。

use gpui::{
    App, ClickEvent, Div, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::hotkeys;
use crate::i18n::t;
use crate::modal;
use crate::store::AppStore;
use crate::ui;

/// 键位提示那三条的 id(`FirstRunGuide.tsx:36-40` 逐条照抄,顺序也一样)。
///
/// 只存 id,显示串与描述都从 [`crate::hotkeys`] 那张唯一事实来源里取 ——
/// 原版这里是手写 `hotkeyLabel('newTerminal')` + `t('settings.shortcuts.newTerminal')`
/// 两处各写一遍,改键位时漏掉一处就漂移。
pub const HINT_IDS: [&str; 3] = ["newTerminal", "switchProject", "terminalSearch"];

/// 首屏本体。挂在终端栏(`bg-terminal` 底色),整块垂直居中。
///
/// 尺寸逐条照 TSX:`gap-6` = 24px、`px-8` = 32px、标题与副标题之间 `space-y-1.5`
/// = 6px、键位提示行之间同样 6px、提示块与上方按钮之间多 `pt-2` = 8px。
pub fn guide(store: Entity<AppStore>) -> Div {
    div()
        .size_full()
        .bg(ui::bg_terminal())
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(24.0))
        .px(px(32.0))
        .child(
            // 标题 + 副标题(`space-y-1.5`)
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        // `text-base font-medium`
                        .text_size(ui::font_px(14.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(ui::text_primary())
                        .child(t("app", "firstRun.title")),
                )
                .child(
                    div()
                        .text_size(ui::font_px(13.0))
                        .text_color(ui::text_muted())
                        .child(t("app", "firstRun.subtitle")),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .child(add_local_button(store.clone()))
                .child(add_remote_button(store)),
        )
        .child(hints())
}

/// 主按钮:accent 描边 + `--accent-subtle` 底,hover 转 `--accent-muted`
/// (`FirstRunGuide.tsx:29-31` 的 `primary`)。
fn add_local_button(store: Entity<AppStore>) -> impl IntoElement {
    div()
        .id("first-run-add-local")
        .px(px(16.0))
        .py(px(10.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(ui::accent())
        .bg(ui::accent_subtle())
        .text_size(ui::font_px(13.0))
        .text_color(ui::accent())
        .cursor_pointer()
        .hover(|el| el.bg(ui::accent_muted()))
        .child(t("app", "firstRun.addLocal"))
        .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            modal::open_add_project(store.clone(), window, cx);
        })
}

/// 次按钮:边框 + 淡字,hover 转 accent(`FirstRunGuide.tsx:32-34` 的 `secondary`)。
///
/// 点开的是与项目列表底部 `SSH` 钮**同一个**弹窗(同一种覆盖物,防叠开)。
fn add_remote_button(store: Entity<AppStore>) -> impl IntoElement {
    div()
        .id("first-run-add-remote")
        .px(px(16.0))
        .py(px(10.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(ui::border_default())
        .text_size(ui::font_px(13.0))
        .text_color(ui::text_secondary())
        .cursor_pointer()
        .hover(|el| el.border_color(ui::accent()).text_color(ui::accent()))
        .child(t("app", "firstRun.addRemote"))
        .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            crate::remote_project::open(store.clone(), None, window, cx);
        })
}

/// 「常用快捷键」提示块。
fn hints() -> Div {
    let mut block = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(6.0))
        // 原版 `pt-2`
        .pt(px(8.0))
        .text_size(ui::font_px(11.0))
        .text_color(ui::text_muted())
        .child(
            div()
                .opacity(0.7)
                .child(t("app", "firstRun.hintsTitle")),
        );
    for id in HINT_IDS {
        let Some(desc_key) = hotkeys::hotkey_desc_key(id) else {
            continue;
        };
        block = block.child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .child(ui::kbd(hotkeys::hotkey_label(id)))
                .child(div().child(t("settings", desc_key))),
        );
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三条提示的 id 必须都在键位表里 —— 不在的话那颗键帽会画成空白,
    /// 而这种「写着有快捷键、其实取不到」正是 `hotkeys.rs` 整张表要防的事。
    #[test]
    fn 三条键位提示都取得到() {
        for id in HINT_IDS {
            assert!(
                !hotkeys::hotkey_label(id).is_empty(),
                "{id} 在键位表里取不到显示串"
            );
            assert!(
                hotkeys::hotkey_desc_key(id).is_some(),
                "{id} 在键位表里取不到描述 key"
            );
        }
    }

    /// 顺序与原版 `FirstRunGuide.tsx:36-40` 一致:新建终端 / 切换项目 / 终端查找。
    #[test]
    fn 提示顺序照抄原版() {
        assert_eq!(HINT_IDS, ["newTerminal", "switchProject", "terminalSearch"]);
    }
}
