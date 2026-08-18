//! app data 目录定位与历史 identifier 迁移。
//!
//! # 为什么可以直接用 `dirs` 拼
//!
//! 原实现走 `tauri::AppHandle::path().app_data_dir()`。Tauri v2 桌面端的这个方法
//! 本身就是 `dirs::data_dir()?.join(identifier)`(tauri-2.11.1
//! `src/path/desktop.rs:247`),而 tauri 依赖的正是 `dirs = "6"` —— 与本工作区
//! `[workspace.dependencies]` 里锁的同一大版本。因此下面拼出来的路径与旧版
//! **落在同一个磁盘位置**,升级上来的用户不会看见配置"凭空消失":
//!
//! | 平台 | 实际路径 |
//! |---|---|
//! | Windows | `%APPDATA%\com.mini-term.app`(即 `FOLDERID_RoamingAppData`,**不是** Local) |
//! | macOS | `~/Library/Application Support/com.mini-term.app` |
//! | Linux | `$XDG_DATA_HOME/com.mini-term.app`,回落 `~/.local/share/com.mini-term.app` |
//!
//! 本机实测佐证:`C:\Users\<user>\AppData\Roaming\com.mini-term.app\` 下确有
//! `config.json` / `config.json.bak` / `themes/` / `usage.db` / `hook-server.json`,
//! 而 `AppData\Local\com.mini-term.app\` 只有 WebView2 自己的 `EBWebView`
//! (那是 `app_local_data_dir`,与本模块无关)。sidecar 侧的
//! `mt-core::config_reader::config_json_path` 也是按 `%APPDATA%` 分支自己拼的同一条路径。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// 当前 app identifier,决定 app data 子目录名(与 `tauri.conf.json` 一致)。
pub const APP_IDENTIFIER: &str = "com.mini-term.app";

/// 历史 identifier。0.2.20 及之前版本使用模板默认值,从 0.2.21 开始切换到
/// `com.mini-term.app`。首次启动时一次性把旧目录下的 config.json 拷到新目录,
/// 旧文件保留不删,作为回退兜底。
pub const LEGACY_IDENTIFIER: &str = "com.tauri-app.tauri-app";

/// 用户数据根目录(identifier 的上一级)。
pub fn data_root() -> Result<PathBuf> {
    dirs::data_dir().context("无法定位用户数据目录")
}

/// `{data_root}/com.mini-term.app`。**不保证目录存在**,只做路径拼接。
pub fn app_data_dir() -> Result<PathBuf> {
    Ok(data_root()?.join(APP_IDENTIFIER))
}

/// 同 [`app_data_dir`],外加 `create_dir_all`(失败忽略,与旧 `config_path` 同语义:
/// 真写不进去时由后续的读写报错,而不是在取路径这一步就炸)。
pub fn ensure_app_data_dir() -> Result<PathBuf> {
    let dir = app_data_dir()?;
    fs::create_dir_all(&dir).ok();
    Ok(dir)
}

/// `{app_data_dir}/config.json`。
pub fn config_path() -> Result<PathBuf> {
    Ok(ensure_app_data_dir()?.join("config.json"))
}

/// `{app_data_dir}/themes`。
pub fn themes_dir() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("themes"))
}

/// 在应用启动早期调用,保证所有配置读取之前完成 identifier 迁移。
pub fn migrate_legacy_app_data() {
    if let Ok(new_dir) = app_data_dir() {
        migrate_app_data_at(&new_dir);
    }
}

/// 纯函数版本,接收新 app_data_dir 路径。便于单元测试。
///
/// 行为:
/// 1. 新目录已有 `config.json` → 直接返回(已迁移过 / 全新用户首次保存生成)
/// 2. 新目录无 `config.json`,但老 identifier 目录有 → 拷过来
/// 3. 老目录也没有 → 返回(全新安装)
///
/// 老 config.json 不删除,作为回退兜底。create_dir_all / copy 失败仅打印日志,
/// 不 panic —— 后续读取会在缺文件时退化为 default。
pub(crate) fn migrate_app_data_at(new_dir: &Path) {
    let new_config = new_dir.join("config.json");
    if new_config.exists() {
        return;
    }
    let Some(base_dir) = new_dir.parent() else {
        return;
    };
    let old_config = base_dir.join(LEGACY_IDENTIFIER).join("config.json");
    if !old_config.exists() {
        return;
    }
    if let Err(e) = fs::create_dir_all(new_dir) {
        eprintln!("[migrate] 创建新数据目录失败 {}: {e}", new_dir.display());
        return;
    }
    match fs::copy(&old_config, &new_config) {
        Ok(_) => {
            eprintln!(
                "[migrate] 已将旧 config.json 迁移至新目录: {}",
                new_config.display()
            );
        }
        Err(e) => {
            eprintln!("[migrate] 拷贝旧 config.json 失败: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_root(label: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mini-term-test-{label}-{ts}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn migrate_copies_legacy_config_when_new_dir_empty() {
        let root = unique_test_root("migrate-copy");
        let new_dir = root.join(APP_IDENTIFIER);
        let old_dir = root.join(LEGACY_IDENTIFIER);
        fs::create_dir_all(&old_dir).unwrap();
        let payload = r#"{"projects":[],"defaultShell":"cmd","availableShells":[]}"#;
        fs::write(old_dir.join("config.json"), payload).unwrap();

        migrate_app_data_at(&new_dir);

        let migrated = fs::read_to_string(new_dir.join("config.json")).unwrap();
        assert_eq!(migrated, payload);
        // 旧文件保留作为兜底
        assert!(old_dir.join("config.json").exists());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn migrate_skips_when_new_config_already_exists() {
        let root = unique_test_root("migrate-skip-exists");
        let new_dir = root.join(APP_IDENTIFIER);
        fs::create_dir_all(&new_dir).unwrap();
        fs::write(new_dir.join("config.json"), "current").unwrap();

        let old_dir = root.join(LEGACY_IDENTIFIER);
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join("config.json"), "legacy").unwrap();

        migrate_app_data_at(&new_dir);

        let after = fs::read_to_string(new_dir.join("config.json")).unwrap();
        assert_eq!(after, "current");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn migrate_noop_when_legacy_missing() {
        let root = unique_test_root("migrate-noop");
        let new_dir = root.join(APP_IDENTIFIER);

        migrate_app_data_at(&new_dir);

        // 没有任何东西被创建
        assert!(!new_dir.join("config.json").exists());

        fs::remove_dir_all(&root).ok();
    }

    /// 去 Tauri 化后路径必须与 `AppHandle::path().app_data_dir()` 逐段一致:
    /// 目录名恒为 identifier,父目录恒为 `dirs::data_dir()`(Windows 上是 Roaming)。
    #[test]
    fn app_data_dir_mirrors_tauri_layout() {
        let dir = app_data_dir().unwrap();
        assert_eq!(dir.file_name().unwrap(), APP_IDENTIFIER);
        assert_eq!(dir.parent().unwrap(), dirs::data_dir().unwrap());
        assert_eq!(config_path().unwrap(), dir.join("config.json"));
        assert_eq!(themes_dir().unwrap(), dir.join("themes"));
        #[cfg(target_os = "windows")]
        {
            // Roaming,不是 Local —— 存量用户的 config.json 就在这
            let roaming = std::env::var("APPDATA").unwrap();
            assert_eq!(dir.parent().unwrap(), Path::new(&roaming));
        }
    }
}
