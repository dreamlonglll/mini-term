//! 终端渲染:grid → GPUI 元素。
//!
//! 两层用法,按需求挑:
//!
//! - [`TerminalView`]:**默认选这个**。gpui `Entity`,自带焦点 / 键盘 / IME /
//!   剪贴板,宿主只要给一个 emulator 和一条「字节往哪写」的回调。
//! - [`TerminalElement`]:裸元素。要自己接焦点、键盘与 `EntityInputHandler`
//!   才能用,IME 尤其麻烦 —— 除非有特殊布局需求,不要直接用它。

pub mod colors;
pub mod damage;
mod element;
pub mod ime;
mod input;
pub mod mouse;
pub mod scrollbar;
pub mod search;
pub mod search_bar;
pub mod selection_dwell;
mod theme;
mod view;

pub use damage::{CellSignature, DamageStats, FrameKey, RowCache, row_signature};
pub use element::{
    FlashLine, FrameGeometry, InstallInputHandler, OnGridResize, OnInput, PreeditText,
    PreparedFrame, TerminalElement,
};
pub use ime::{ImeState, Preedit, commit_to_bytes};
pub use input::{is_text_input_key, keystroke_to_bytes, paste_to_bytes};
pub use scrollbar::{ScrollbarHit, ScrollbarLayout, ScrollbarStyle};
pub use search::{
    HighlightKind, HighlightSpan, SearchDirection, SearchHighlights, SearchLimits, SearchMatch,
    SearchOptions, TerminalSearch, advance_index, build_pattern, escape_literal, index_at_or_after,
    is_word_char, whole_word_ok,
};
pub use search_bar::{
    OnSearchClose, SearchBarEvent, SearchBarLabels, TerminalSearchBar, counter_text,
};
pub use selection_dwell::{CopiedTip, DwellConfig, DwellTracker, OnSelectionCopied, ReleaseAction};
pub use mouse::{
    GridPos, MouseAction, MouseBtn, MouseMods, WheelDir, mouse_report_bytes,
    mouse_reporting_active, prefers_local_handling,
};
pub use theme::{SearchColors, TerminalStyle, TerminalTheme, rgb8};
pub use view::{OnPaste, PasteAction, SmartCopyPaste, TerminalView};

/// OSC 调色板查询的应答色。见 [`colors::color_request_rgb`]。
pub use colors::color_request_rgb;
