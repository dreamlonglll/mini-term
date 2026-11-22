//! Git 面板的非 GPUI 纯逻辑：目录/输出监听、代次防陈旧、diff 词级/折叠/选行与历史多选模型。

pub mod diff_fold;
pub mod diff_picks;
pub mod diff_word;
pub mod dir_watch;
pub mod flight;
pub mod history_selection;
pub mod watch;
