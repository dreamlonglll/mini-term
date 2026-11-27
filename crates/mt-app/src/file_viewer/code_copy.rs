//! Markdown 代码块右上角的「复制」按钮。文件预览与会话面板的正文都走
//! gpui-component 的 `TextView`,这里给它们共用一份
//! [`TextView::code_block_actions`](gpui_component::text::TextView::code_block_actions)
//! 回调。
//!
//! # 形态跟着上游走
//!
//! 上游把回调返回的元素**常驻**在每个围栏代码块的右上角(gpui-base
//! `text/node.rs` 的 `CodeBlock::render`:`absolute().top_2().right_2()`,外面垫一层
//! 代码块底色的圆角底),没有「悬停代码块才出现」的钩子 —— 代码块那层 div 是组件
//! 自己建的,挂不上 `group`。所以按钮常驻,用弱化的配色压住存在感。
//!
//! 上游一旦拿到回调就不再画它自己右上角的语言名(同一处的 `lang_label`),这里把
//! 语言名放回按钮左边,观感与接入前一致。
//!
//! # 反馈
//!
//! 点了把代码块原文([`CodeBlock::code`])写进剪贴板,按钮原地换成「✓ 已复制」停
//! [`COPIED_FOR`],与命令库、SSH 交互浮层的复制反馈同一口径(都是 1.2s、成功色)。
//! 「刚复制的是哪一块」记在宿主持有的 [`CodeCopyFeedback`] 实体上:回调本身每帧都被
//! 重建、要 `Send + Sync`,存不了状态。回调渲染时读了这个实体,gpui-fast 的视图保留
//! 把它记成该 TextView 的依赖,实体一 notify 那一块就会重画,不必宿主再转发。

use std::time::Duration;

use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, Entity, InteractiveElement as _,
    IntoElement as _, MouseButton, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _, Task, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::text::CodeBlock;
use mt_ui::icons::VectorIcon;
use mt_ui::tooltip::TooltipExt as _;

use crate::git::ui::{ICON_CHECK, ICON_COPY};
use crate::i18n::t;
use crate::ui;

/// 「已复制」停留多久。命令库(`command_library::popover` 的 `COPIED_MS`)与
/// SSH 交互浮层(`ssh_activity::view` 的 `COPIED_FOR`)都是这一档。
const COPIED_FOR: Duration = Duration::from_millis(1200);

/// 代码块正文的上内边距。上游在右上角画语言名时把它从 14px 撑到 28px
/// (`CodeBlock::render` 的 `pt(...)`),给角上那一行让位;挂了回调就不撑了,
/// 而按钮行(8px 顶距 + 20px 高)比语言名还高,首行右端会被它盖住 —— 由宿主的
/// `TextViewStyle::code_block` 补回来,见 [`reserve_code_actions_space`]。
const CODE_TOP_PADDING: f32 = 28.0;

/// 按钮行的高度(复制钮与「已复制」两态同高,换态时角上那一块不跳)。
const ACTIONS_ROW_H: f32 = 20.0;

/// 一个代码块在宿主里的身份:宿主给的分区号(文件预览是块序号、会话面板是消息
/// 序号)加代码块在那一段 markdown 里的起始字节。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CodeKey {
    scope: usize,
    start: Option<usize>,
}

/// 刚复制过的那一块(见模块注释「反馈」)。宿主 `cx.new(|_| CodeCopyFeedback::default())`
/// 建一份、交给 [`code_copy_actions`]。
#[derive(Default)]
pub(crate) struct CodeCopyFeedback {
    copied: Option<CodeKey>,
    /// 到点把「已复制」撤掉;再复制一次就换掉它(旧的那一拍不会把新的撤早)
    _reset: Option<Task<()>>,
}

impl CodeCopyFeedback {
    fn is_copied(&self, key: CodeKey) -> bool {
        self.copied == Some(key)
    }

    fn mark(&mut self, key: CodeKey, cx: &mut Context<Self>) {
        self.copied = Some(key);
        self._reset = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            let _ = this.update(cx, |this: &mut CodeCopyFeedback, cx| {
                if this.copied == Some(key) {
                    this.copied = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }
}

/// 给宿主的 `TextViewStyle::code_block` 补上正文上内边距(理由见 [`CODE_TOP_PADDING`])。
/// 挂了 [`code_copy_actions`] 的 TextView 都要配它。
pub(crate) fn reserve_code_actions_space(
    code_block: gpui::StyleRefinement,
) -> gpui::StyleRefinement {
    code_block.pt(px(CODE_TOP_PADDING))
}

/// `TextView::code_block_actions` 的回调。`scope` 在同一个 `feedback` 底下要能把
/// 各个 TextView 分开(见 [`CodeKey`])。
///
/// 只捕获 `WeakEntity`:回调存进 TextView 的状态里,不该反过来把反馈实体续命。
pub(crate) fn code_copy_actions(
    feedback: &Entity<CodeCopyFeedback>,
    scope: usize,
) -> impl Fn(&CodeBlock, &mut Window, &mut App) -> AnyElement + Send + Sync + 'static {
    let feedback = feedback.downgrade();
    move |block, _window, cx| {
        let key = CodeKey {
            scope,
            start: block.span.map(|span| span.start),
        };
        let copied = feedback
            .upgrade()
            .is_some_and(|feedback| feedback.read(cx).is_copied(key));
        render_actions(block, key, copied, feedback.clone(), cx)
    }
}

fn render_actions(
    block: &CodeBlock,
    key: CodeKey,
    copied: bool,
    feedback: WeakEntity<CodeCopyFeedback>,
    cx: &App,
) -> AnyElement {
    // 角上这一行在代码块里面,不改字族就继承等宽字;上游的语言名用的是界面字族
    let family = ui::ui_font_family().or_else(|| {
        cx.try_global::<gpui_base::Theme>()
            .map(|theme| theme.tokens.typography.sans.clone())
    });
    let row = div()
        .h(px(ACTIONS_ROW_H))
        .flex()
        .items_center()
        .gap(px(4.0))
        .when_some(family, |el, family| el.font_family(family))
        .text_size(ui::font_px(11.0))
        .line_height(px(16.0));
    if copied {
        return row
            .debug_selector(|| "md-code-copied".into())
            .px(px(4.0))
            .text_color(ui::color_success())
            .child(VectorIcon::new(ICON_CHECK, px(12.0)).ink(ui::color_success()))
            .child(t("fileViewer", "codeCopied"))
            .into_any_element();
    }
    let code = block.code();
    row.when_some(block.lang().filter(|lang| !lang.is_empty()), |el, lang| {
        el.child(div().pl(px(4.0)).text_color(ui::text_muted()).child(lang))
    })
    .child(
        // id 不必带块号:上游在每个代码块外面套了一层按块编号的 id
        // (`block_element_id("codeblock", ..)`),里面的 id 各自成域
        div()
            .id("md-code-copy")
            .debug_selector(|| "md-code-copy".into())
            .size(px(ACTIONS_ROW_H))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.0))
            .cursor_pointer()
            .hover(|el| el.bg(ui::border_subtle()))
            .tip(t("fileViewer", "copyCode"))
            // 按下不起窗口级拖选(代码块所在的 TextView 可选中),口径同
            // `title_bar::drag_area`;不停止冒泡,外层拿焦点照旧
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                gpui_component::GlobalState::suppress_text_selection(cx)
            })
            .on_click(move |_: &ClickEvent, _window, cx| {
                cx.stop_propagation();
                cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()));
                if let Some(feedback) = feedback.upgrade() {
                    feedback.update(cx, |feedback, cx| feedback.mark(key, cx));
                }
            })
            .child(VectorIcon::new(ICON_COPY, px(13.0)).ink(ui::text_secondary())),
    )
    .into_any_element()
}

#[cfg(test)]
#[path = "code_copy_tests.rs"]
mod tests;
