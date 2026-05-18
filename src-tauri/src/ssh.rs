/// SSH 私钥权限自动处理。
/// Windows OpenSSH 会因私钥文件 ACL 权限过于开放而拒绝使用
/// (`WARNING: UNPROTECTED PRIVATE KEY FILE!`)。连接时把私钥复制到一份
/// 仅当前用户可读写的临时副本,用 `ssh -i <临时副本>` 连接,绕过该检查,
/// 不修改用户的原始密钥文件。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

/// 临时私钥目录:`{temp}/mini-term-ssh-keys`。
fn temp_keys_dir() -> PathBuf {
    std::env::temp_dir().join("mini-term-ssh-keys")
}

/// 清理临时私钥目录,启动时调用一次,清除上次遗留的副本。
/// 仿 `clipboard::cleanup_old_clipboard_images()`:清理失败不 panic。
pub fn cleanup_ssh_temp_keys() {
    let _ = std::fs::remove_dir_all(temp_keys_dir());
}

/// 收紧临时私钥副本权限,仅当前用户可读写。
#[cfg(windows)]
fn restrict_permissions(path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let username = std::env::var("USERNAME")
        .map_err(|_| "无法获取当前用户名(USERNAME 环境变量)".to_string())?;

    // /inheritance:r 移除继承的 ACE; /grant:r 仅授予当前用户完全控制
    let status = std::process::Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{}:F", username))
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("执行 icacls 失败: {e}"))?;

    if !status.success() {
        return Err(format!("icacls 收紧权限失败,退出码: {:?}", status.code()));
    }
    Ok(())
}

/// 收紧临时私钥副本权限,仅当前用户可读写。
#[cfg(not(windows))]
fn restrict_permissions(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("设置文件权限失败: {e}"))
}

/// 把私钥复制到权限收紧的临时副本,返回临时副本路径。
///
/// 临时文件名按源路径稳定哈希派生:同一把 key 重连复用/覆盖同一文件,
/// 不无限累积。源文件不存在直接返回 `Err`。
#[tauri::command]
pub fn prepare_ssh_key(identity_file: String) -> Result<String, String> {
    let src = PathBuf::from(&identity_file);
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
