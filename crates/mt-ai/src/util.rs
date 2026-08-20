//! 本 crate 自用的小工具 —— 收尾-1 批起全部转为 `mt-core` 的再导出。
//!
//! 迁移期这里放过 `atomic_write` 与 `parse_wsl_unc` 的逐字副本,理由是当时
//! `mt-core` 还在 `src-tauri/` 下:新工作区一旦反向依赖旧目录树,
//! `cargo build -p mt-ai` 就会把整套 Tauri 依赖拖进来。
//! `mt-core` 物理移入 `crates/` 后这个理由消失,两份副本已删除,
//! 本模块只保留同名的再导出,**调用点一行未改**(`crate::util::atomic_write`
//! / `crate::util::parse_wsl_unc` 路径依旧成立)。

/// 原子写文件:同目录临时文件 + rename。
///
/// hook 注册要改用户的 `settings.json` / `hooks.json` / `config.toml`,
/// 写一半崩掉会毁掉用户配置。实现见 `mt_core::atomic_write`。
pub use mt_core::atomic_write;

/// WSL UNC 路径解析(`\\wsl$\<distro>\<rest>` / `\\wsl.localhost\...` /
/// `\\?\UNC\...` 三种形式,host 名大小写不敏感)。实现见 `mt_core::parse_wsl_unc`。
///
/// 返回类型 `mt_core::WslPath`(字段 `distro` / `unix_path` 与原副本同名同义)
/// 不在这里再导出:本模块是 crate 私有的,唯一调用点 `sessions.rs` 只取字段、
/// 不写类型名,多导一个名字只会换来 `unused_imports` 警告。需要写出类型时
/// 直接用 `mt_core::WslPath`。
pub use mt_core::parse_wsl_unc;
