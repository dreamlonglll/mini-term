use gpui::{AppContext as _, Context, IntoElement, Render, TestAppContext, VisualTestContext};
use gpui_component::text::TextView;

use super::*;

/// 窗口里只有一个挂了复制按钮的 TextView(与文件预览同一条接法)。
struct Host {
    feedback: Entity<CodeCopyFeedback>,
    markdown: &'static str,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(
            TextView::markdown("code-copy-test", self.markdown)
                .selectable(true)
                .code_block_actions(code_copy_actions(&self.feedback, 0)),
        )
    }
}

fn open<'a>(
    cx: &'a mut TestAppContext,
    markdown: &'static str,
) -> (Entity<CodeCopyFeedback>, &'a mut VisualTestContext) {
    cx.update(gpui_component::init);
    let feedback = cx.new(|_| CodeCopyFeedback::default());
    let for_host = feedback.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let host = cx.new(|_| Host {
            feedback: for_host,
            markdown,
        });
        gpui_component::Root::new(host, window, cx)
    });
    cx.run_until_parked();
    (feedback, cx)
}

#[gpui::test]
fn 点复制写入代码原文_短暂换成已复制再换回来(cx: &mut TestAppContext) {
    let (feedback, cx) = open(cx, "前文\n\n```rust\nfn main() {}\n```\n\n后文");
    let button = cx
        .debug_bounds("md-code-copy")
        .expect("代码块右上角应有复制按钮");
    assert!(cx.debug_bounds("md-code-copied").is_none());

    cx.simulate_click(button.center(), Default::default());
    cx.run_until_parked();
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("fn main() {}".to_string()),
        "剪贴板里是代码块原文,不带围栏与语言名"
    );
    assert!(
        cx.debug_bounds("md-code-copied").is_some(),
        "点完原地换成「已复制」(反馈实体一 notify,那一块就重画)"
    );
    assert!(cx.debug_bounds("md-code-copy").is_none());

    cx.executor().advance_clock(COPIED_FOR);
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("md-code-copy").is_some(),
        "到点换回复制按钮"
    );
    assert!(cx.debug_bounds("md-code-copied").is_none());
    cx.update(|_, cx| assert!(feedback.read(cx).copied.is_none()));
}

#[gpui::test]
fn 没有代码块就没有按钮(cx: &mut TestAppContext) {
    let (_, cx) = open(cx, "# 标题\n\n段落里的 `行内代码` 不算代码块");
    assert!(cx.debug_bounds("md-code-copy").is_none());
}

#[gpui::test]
fn 连点两块时旧的计时不会把新的提前撤掉(cx: &mut TestAppContext) {
    let feedback = cx.new(|_| CodeCopyFeedback::default());
    let first = CodeKey {
        scope: 0,
        start: Some(0),
    };
    let second = CodeKey {
        scope: 1,
        start: Some(0),
    };
    let half = COPIED_FOR / 2;

    feedback.update(cx, |feedback, cx| feedback.mark(first, cx));
    cx.executor().advance_clock(half);
    feedback.update(cx, |feedback, cx| feedback.mark(second, cx));
    assert!(feedback.read_with(cx, |feedback, _| feedback.is_copied(second)));
    assert!(!feedback.read_with(cx, |feedback, _| feedback.is_copied(first)));

    // 第一块的计时本该在这里到点;它已随 `_reset` 被换掉,第二块不受影响
    cx.executor().advance_clock(half);
    cx.run_until_parked();
    assert!(feedback.read_with(cx, |feedback, _| feedback.is_copied(second)));

    cx.executor().advance_clock(half);
    cx.run_until_parked();
    assert!(!feedback.read_with(cx, |feedback, _| feedback.is_copied(second)));
}
