//! 终端渲染:grid → GPUI 元素。

pub mod colors;
mod element;
mod input;
mod theme;

pub use element::{InstallInputHandler, OnGridResize, OnInput, PreparedFrame, TerminalElement};
pub use input::{keystroke_to_bytes, paste_to_bytes};
pub use theme::{TerminalStyle, TerminalTheme, rgb8};
