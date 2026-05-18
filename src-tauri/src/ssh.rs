//! SSH 私钥权限自动处理的 Tauri 命令薄包装。
//!
//! 实际逻辑在 `mt-core`(tauri-free,可被 SSH MCP sidecar 共用);本文件只把
//! 它包装成 `#[tauri::command]` 并重导出启动清理函数给 `lib.rs`。

/// 清理临时私钥目录,启动时调用一次,清除上次遗留的副本。
/// 重导出 `mt-core` 的实现,保持 `lib.rs` 中 `ssh::cleanup_ssh_temp_keys()` 调用不变。
pub use mt_core::cleanup_ssh_temp_keys;

/// 把私钥复制到权限收紧的临时副本,返回临时副本路径。
///
/// 临时文件名按源路径稳定哈希派生:同一把 key 重连复用/覆盖同一文件,
/// 不无限累积。源文件不存在直接返回 `Err`。
#[tauri::command]
pub fn prepare_ssh_key(identity_file: String) -> Result<String, String> {
    mt_core::prepare_ssh_key(&identity_file)
}
