//! 空壳 stub：经根 `Cargo.toml` 的 `[patch.crates-io]` 替换 `gpui-kit 0.10` 家族中
//! 残留的非可选 `gpui-pre` 依赖，避免拉入并编译第二套 GPUI 引擎。
//! 任何漏开 `gpui-fast` feature 的依赖都会在此立刻暴露编译错误。
