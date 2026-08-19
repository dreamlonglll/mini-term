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

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// 当前 app identifier,决定 app data 子目录名(与 `tauri.conf.json` 一致)。
pub const APP_IDENTIFIER: &str = "com.mini-term.app";

/// 历史 identifier。0.2.20 及之前版本使用模板默认值,从 0.2.21 开始切换到
/// `com.mini-term.app`。首次启动时一次性把旧目录下的 config.json 拷到新目录,
/// 旧文件保留不删,作为回退兜底。
pub const LEGACY_IDENTIFIER: &str = "com.tauri-app.tauri-app";

/// **开发隔离逃生门**:设了它,用户数据目录整个换到这个路径。
///
/// 装机版正在跑的时候直接 `cargo run` 会与它共用同一个目录 —— 配置被两边轮流
/// 改写、皮肤目录也是同一份。设了这个环境变量就整个隔离出去,与 Tauri 那边
/// 靠 `--config` 覆盖 identifier 是同一招。`mt-app::app_data_dir` 与
/// [`active_data_dir`] 是同一口径的两个入口(前者是后者的不返错版本)。
pub const DATA_DIR_ENV: &str = "MT_APP_DATA_DIR";

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

/// `MT_APP_DATA_DIR` 的取值 → 覆盖目录。空串按「没设」处理
/// (`MT_APP_DATA_DIR=` 这种写法在 shell 里等同于取消设置,不该把数据目录
/// 指到当前工作目录上)。纯函数,便于单测 —— 直接 `set_var` 会污染同进程里
/// 并行跑的其它测试。
fn data_dir_override(raw: Option<&OsStr>) -> Option<PathBuf> {
    raw.filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// **生效中**的用户数据目录:`MT_APP_DATA_DIR` 优先,否则 [`app_data_dir`]。
///
/// config.json 与 themes/ 都按这条口径落盘 —— dev 实例开着隔离目录时,
/// 皮肤列表理应看见隔离目录里的包(此前 [`themes_dir`] 钉死在装机版目录上,
/// mt-app 只能自己拼路径绕开)。
///
/// ⚠️ **hook 端口文件刻意不走这条**:`hook-server.json` 由 sidecar 侧按装机版
/// 路径去找(`mt-core::config_reader`),跟着换会让 dev 实例收不到任何 hook 事件。
/// 那一路仍旧调 [`app_data_dir`]。
pub fn active_data_dir() -> Result<PathBuf> {
    match data_dir_override(std::env::var_os(DATA_DIR_ENV).as_deref()) {
        Some(dir) => Ok(dir),
        None => app_data_dir(),
    }
}

/// `{active_data_dir}/themes`。
pub fn themes_dir() -> Result<PathBuf> {
    Ok(active_data_dir()?.join("themes"))
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

    /// `MT_APP_DATA_DIR` 的解析口径:非空即覆盖,空串按「没设」。
    ///
    /// 直接 `set_var` 会污染同进程里并行跑的其它测试(且 2024 edition 起是
    /// `unsafe`),所以钉的是纯函数那一层。
    #[test]
    fn 数据目录覆盖只认非空取值() {
        assert_eq!(
            data_dir_override(Some(OsStr::new(r"D:\dev\mini-term-gpui-dev"))),
            Some(PathBuf::from(r"D:\dev\mini-term-gpui-dev"))
        );
        assert_eq!(data_dir_override(Some(OsStr::new(""))), None, "空串=没设");
        assert_eq!(data_dir_override(None), None);
    }

    /// 皮肤目录挂在**生效中**的数据目录下 —— dev 实例设了 `MT_APP_DATA_DIR`
    /// 就该看见隔离目录里的皮肤包(此前钉死在装机版目录上,
    /// mt-app 只好自己拼路径绕开 `ThemePacks::open()`)。
    #[test]
    fn 皮肤目录跟随生效数据目录() {
        assert_eq!(themes_dir().unwrap(), active_data_dir().unwrap().join("themes"));
        // 没设环境变量时两条口径重合(CI 与开发机的常态)
        if std::env::var_os(DATA_DIR_ENV).is_none() {
            assert_eq!(active_data_dir().unwrap(), app_data_dir().unwrap());
        }
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
