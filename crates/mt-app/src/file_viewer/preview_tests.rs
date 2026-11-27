use super::*;

#[test]
fn 远程图片必须先获批准_本地图片保持自动加载() {
    assert!(!markdown_image_can_load(true, false));
    assert!(markdown_image_can_load(true, true));
    assert!(markdown_image_can_load(false, false));
}

#[test]
fn svg_判定_不被查询串骗到() {
    // 徽章 URL 常带 `?style=`,扩展名只看路径那一截
    assert!(is_svg_target(
        "https://img.shields.io/badge/a-b.svg?style=flat"
    ));
    assert!(is_svg_target("D:\\icons\\a.SVG"));
    assert!(!is_svg_target("https://x.dev/a.png"));
    assert!(!is_svg_target("a/b.svg.png"), "只看最后一段扩展名");
}

/// frontmatter 那一块真在 TextView 里画一遍:开了扩展才是代码块(右上角有复制
/// 按钮),复制出来的是 YAML 原文;同一段原文不开扩展就只是分隔线 + 标题。
mod frontmatter_view {
    use gpui::{
        AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
        Styled as _, TestAppContext, VisualTestContext, Window, div,
    };
    use gpui_component::text::TextView;

    use super::super::super::code_copy::{CodeCopyFeedback, code_copy_actions};
    use super::super::super::markdown::{MdSegment, split_md_document};
    use super::super::frontmatter_text_view;

    type BuildView = Box<dyn Fn(&Entity<CodeCopyFeedback>) -> TextView>;

    struct Host {
        feedback: Entity<CodeCopyFeedback>,
        build: BuildView,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child((self.build)(&self.feedback))
        }
    }

    fn open(cx: &mut TestAppContext, build: BuildView) -> &mut VisualTestContext {
        cx.update(gpui_component::init);
        let feedback = cx.new(|_| CodeCopyFeedback::default());
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let host = cx.new(|_| Host { feedback, build });
            gpui_component::Root::new(host, window, cx)
        });
        cx.run_until_parked();
        cx
    }

    fn frontmatter_source() -> SharedString {
        let doc = split_md_document("---\ntitle: 示例\nlayout: post\n---\n\n# 正文\n");
        let Some(MdSegment::Frontmatter(raw)) = doc.segs.first() else {
            panic!("应切出 frontmatter 块:{:?}", doc.segs);
        };
        raw.clone().into()
    }

    #[gpui::test]
    fn 开了扩展画成代码块_复制得到_yaml_原文(cx: &mut TestAppContext) {
        let raw = frontmatter_source();
        let cx = open(
            cx,
            Box::new(move |feedback| {
                frontmatter_text_view("fm".into(), raw.clone())
                    .code_block_actions(code_copy_actions(feedback, 0))
            }),
        );
        let button = cx
            .debug_bounds("md-code-copy")
            .expect("frontmatter 应画成带复制按钮的代码块");
        cx.simulate_click(button.center(), Default::default());
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("title: 示例\nlayout: post".to_string())
        );
    }

    #[gpui::test]
    fn 不开扩展时同一段原文不是代码块(cx: &mut TestAppContext) {
        let raw = frontmatter_source();
        let cx = open(
            cx,
            Box::new(move |feedback| {
                TextView::markdown("fm", raw.clone())
                    .code_block_actions(code_copy_actions(feedback, 0))
            }),
        );
        assert!(
            cx.debug_bounds("md-code-copy").is_none(),
            "不开 frontmatter 时它是分隔线 + setext 标题,没有代码块"
        );
    }
}
