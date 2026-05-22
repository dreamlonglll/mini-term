//! SSH 私钥权限自动处理(tauri-free 核心逻辑)。
//!
//! Windows OpenSSH 会因私钥文件 ACL 权限过于开放而拒绝使用
//! (`WARNING: UNPROTECTED PRIVATE KEY FILE!`)。连接时把私钥复制到一份
//! 仅当前用户可读写的临时副本,用 `ssh -i <临时副本>` 连接,绕过该检查,
//! 不修改用户的原始密钥文件。
//!
//! **仅供 mini-term 主程序内置终端的 SSH 启动路径使用**:主程序通过 PTY
//! 拉起 `ssh` 客户端进程,需要这一层权限收紧的临时副本(详见
//! `src-tauri/src/ssh.rs` 与 `TerminalInstance.tsx`)。
//!
//! `mt-ssh-mcp` sidecar 自 v0.4.10 起已迁移到 `russh` 进程内会话池
//! (`src-tauri/mt-sidecars/src/pool.rs`),库直接读密钥 bytes 进内存,不再
//! 经过 `ssh` 客户端的文件权限校验 —— sidecar 路径**不再调用本模块**,
//! 但函数仍保留在 `mt-core`,因为主程序 PTY 路径仍依赖它们。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// 临时私钥目录:`{temp}/mini-term-ssh-keys`。
pub fn temp_keys_dir() -> PathBuf {
    std::env::temp_dir().join("mini-term-ssh-keys")
}

/// 清理临时私钥目录,启动时调用一次,清除上次遗留的副本。
/// 仿 `clipboard::cleanup_old_clipboard_images()`:清理失败不 panic。
pub fn cleanup_ssh_temp_keys() {
    let _ = std::fs::remove_dir_all(temp_keys_dir());
}

/// 收紧临时私钥副本权限,仅当前用户可读写。
#[cfg(windows)]
pub fn restrict_permissions(path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let username = std::env::var("USERNAME")
        .map_err(|_| "无法获取当前用户名(USERNAME 环境变量)".to_string())?;

    // /inheritance:r 移除继承的 ACE; /grant:r 仅授予当前用户完全控制
    // icacls 默认会往 stdout 打印 "processed file ..." —— SSH MCP sidecar 的
    // stdout 是 MCP 协议专用通道,必须丢弃 icacls 的 stdout/stderr。
    let status = std::process::Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{}:F", username))
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("执行 icacls 失败: {e}"))?;

    if !status.success() {
        return Err(format!("icacls 收紧权限失败,退出码: {:?}", status.code()));
    }
    Ok(())
}

/// 收紧临时私钥副本权限,仅当前用户可读写。
#[cfg(not(windows))]
pub fn restrict_permissions(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("设置文件权限失败: {e}"))
}

/// 把私钥复制到权限收紧的临时副本,返回临时副本路径。
///
/// 临时文件名按源路径稳定哈希派生:同一把 key 重连复用/覆盖同一文件,
/// 不无限累积。源文件不存在直接返回 `Err`。
pub fn prepare_ssh_key(identity_file: &str) -> Result<String, String> {
    let src = PathBuf::from(identity_file);
    if !src.is_file() {
        return Err(format!("私钥文件不存在: {identity_file}"));
    }

    let dir = temp_keys_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时密钥目录失败: {e}"))?;

    // 源路径稳定哈希作为文件名,重连时复用同一临时文件
    let mut hasher = DefaultHasher::new();
    identity_file.hash(&mut hasher);
    let dest = dir.join(format!("{:016x}.key", hasher.finish()));

    std::fs::copy(&src, &dest).map_err(|e| format!("复制私钥失败: {e}"))?;
    restrict_permissions(&dest)?;

    Ok(dest.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_keys_dir_is_under_system_temp() {
        let dir = temp_keys_dir();
        assert!(dir.starts_with(std::env::temp_dir()));
        assert!(dir.ends_with("mini-term-ssh-keys"));
    }

    #[test]
    fn prepare_ssh_key_errors_on_missing_file() {
        let err = prepare_ssh_key("/definitely/not/a/real/key/file").unwrap_err();
        assert!(err.contains("私钥文件不存在"));
    }

    #[test]
    fn prepare_ssh_key_copies_and_is_stable() {
        // 在临时目录造一把假私钥,验证复制成功且同源路径派生同一目标文件名
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let src = std::env::temp_dir().join(format!("mt-core-fake-key-{ts}"));
        std::fs::write(&src, "PRIVATE KEY CONTENT").unwrap();
        let src_str = src.to_string_lossy().into_owned();

        let dest1 = prepare_ssh_key(&src_str).unwrap();
        let dest2 = prepare_ssh_key(&src_str).unwrap();
        assert_eq!(dest1, dest2, "同一源路径应派生同一临时副本路径");
        assert_eq!(
            std::fs::read_to_string(&dest1).unwrap(),
            "PRIVATE KEY CONTENT"
        );

        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dest1);
    }
}
