//! 用量统计:会话轮次解析、SQLite ledger、聚合与计价。
//!
//! 与 Tauri 几乎无耦合(只有两个 `#[tauri::command]` 入口),是整个后端里
//! **迁移成本最低**的一块,可以先搬它来验证工作区结构是否好用。
//!
//! # 待移入
//!
//! | 来源 | 行数 |
//! |---|---|
//! | `src-tauri/src/usage_stats/mod.rs` | 253 |
//! | `src-tauri/src/usage_stats/turns.rs` | 1180 |
//! | `src-tauri/src/usage_stats/ledger.rs` | 1018 |
//! | `src-tauri/src/aggregate.rs` | 743 |
//! | `src-tauri/src/pricing.rs` | 319 |
//!
//! # 注意
//!
//! - 分桶按记录自身时刻求当地偏移(DST 地区历史记录不错日),`chrono-tz` 带
//!   IANA 数据,这条不要在迁移中简化成固定偏移。
//! - ledger 的并发测试历史上是 flaky(见 `project_pr43_review` 的记录),
//!   搬过来跑红时先确认是不是同一个已知抖动,别当成迁移引入的回归。
