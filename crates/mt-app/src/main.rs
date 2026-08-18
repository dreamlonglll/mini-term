//! mini-term 的 GPUI 应用壳。
//!
//! 现阶段只做一件事:开一个窗口。它的作用是把 GPUI + wgpu 那棵依赖树在本机
//! 真正编译、链接、跑起来 —— 在往里填 26.8k 行前端逻辑之前,先证明地基能立住。
//!
//! 全局状态(对应现有 `src/store.ts`)、三栏布局、Tab 与 SplitNode 树都在这里,
//! 但要等 `mt-ui::TerminalElement` 可用之后再动工。

use gpui::{
    App, AppContext, Application, Bounds, Context, IntoElement, ParentElement, Render, Styled,
    Window, WindowBounds, WindowOptions, div, px, rgb, size,
};

/// 应用根视图。后续会长成「ProjectList | FileTree | TerminalArea」三栏。
struct Shell;

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .gap_2()
            .p_8()
            .bg(rgb(0x1a1a1a))
            .text_color(rgb(0xe6e6e6))
            .child("mini-term — GPUI shell")
            .child(
                div()
                    .text_color(rgb(0x8a8a8a))
                    .child("骨架阶段：窗口已起。下一步是 mt-ui::TerminalElement。"),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_cx| Shell),
        )
        .expect("打开窗口失败");
        cx.activate(true);
    });
}
