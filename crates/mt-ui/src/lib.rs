//! GPUI 渲染层:终端 element、复用组件、主题桥。不含业务逻辑。
//!
//! # 本 crate 要产出的东西
//!
//! ## 1. `TerminalElement` —— 整个改造的核心
//!
//! 把 [`mt_terminal::TerminalEmulator`] 的 grid 画成 GPUI 元素。这是自研部分
//! 里工作量与风险都最大的一块,参考量级见 `docs/gpui-migration.md`。
//! 必须自己实现的:字形绘制与列宽、光标、选择高亮、滚动回看、IME 预编辑浮层。
//!
//! 背景图透出的关键:**背景色等于默认背景的 cell 不发 quad**。
//!
//! ## 2. 布局复用件(尽量用 gpui-component,别自己造)
//!
//! | mini-term 现状 | GPUI 侧对应 |
//! |---|---|
//! | Allotment 三栏主布局 | `gpui_component::resizable` |
//! | 递归 SplitNode 树(分屏) | 同上,嵌套使用;树结构本身是业务,留在 `mt-app` |
//! | FileTree | `gpui_component::tree` |
//! | Tab 栏 | `gpui_component::tab` |
//! | 各种 Modal | `gpui_component::dialog` |
//! | 自研 zustand i18n | `gpui-component` 依赖 `rust-i18n`,可复用;字典从 `src/locales/*.ts` 转 |
//!
//! ## 3. 主题桥
//!
//! `gpui_component::theme` 已有 JSON schema + 运行时注册表,mini-term 的主题包
//! 里「配色」那一半映射过去,「背景图 / 字体 / 终端配色」留在 `mt-config`。
//!
//! # 进度
//!
//! - ✅ `TerminalElement`(见 [`terminal`] 模块):逐 cell 绘制、ANSI/256/truecolor、
//!   bold/italic/underline/inverse、块状光标、滚轮回看、鼠标选择 + 剪贴板。
//! - ⬜ 布局复用件、主题桥:未动工。
//! - ⬜ IME:结构上已留出挂载点([`terminal::InstallInputHandler`]),尚未接。

pub mod terminal;

pub use terminal::{
    InstallInputHandler, OnGridResize, OnInput, TerminalElement, TerminalStyle, TerminalTheme,
    keystroke_to_bytes, paste_to_bytes, rgb8,
};
