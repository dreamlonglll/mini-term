//! 用外部编辑器 / 系统默认程序打开路径。
//!
//! # 与 Tauri 版的差别
//!
//! - 原实现签名是 `open_in_editor(app: AppHandle, path, editor_name)`,进函数后
//!   自己 `read_config(&app)` 去翻编辑器列表。这里把「选哪个编辑器」和「怎么启动」
//!   拆成 [`select_editor`] 与 [`open_in_editor`] 两步,前者是纯函数(可测),
//!   后者只认一个具体的 [`Editor`] —— 本 crate 因此不必依赖 `mt-config`。
//! - `open_path_with_default_app` 原本走 `tauri-plugin-opener`。没了 Tauri 就直接
//!   spawn 平台自带的打开器(Windows `explorer.exe` / macOS `open` / Linux `xdg-open`)。

use std::path::Path;
use std::process::Command;

use anyhow::{Result, anyhow, bail};

/// 一个外部编辑器条目。字段与 `AppConfig.editors` 对齐,由调用方从配置里映射过来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    pub name: String,
    pub command: String,
}

/// 挑出要用的编辑器:
/// - 指定了名字 → 按名字精确匹配(找不到就是 None,不做兜底);
/// - 没指定 → 用 `default_editor` 指向的那个,再退到列表第一个。
pub fn select_editor<'a>(
    editors: &'a [Editor],
    default_editor: Option<&str>,
    requested: Option<&str>,
) -> Option<&'a Editor> {
    match requested {
        Some(name) => editors.iter().find(|e| e.name == name),
        None => default_editor
            .and_then(|name| editors.iter().find(|e| e.name == name))
            .or_else(|| editors.first()),
    }
}

/// 用指定编辑器打开路径。编辑器为 `None`(没配过 / 名字没匹配上)时给出可操作的提示。
pub fn open_in_editor(editor: Option<&Editor>, path: &Path) -> Result<()> {
    let editor = editor.ok_or_else(|| {
        anyhow!("尚未配置外部编辑器,请在『设置 → 系统设置 → 外部编辑器』中添加。")
    })?;

    let exe = editor.command.trim();
    if exe.is_empty() {
        bail!("编辑器「{}」的可执行文件路径为空", editor.name);
    }

    let exe_path = Path::new(exe);
    if !exe_path.exists() {
        bail!("编辑器「{}」的路径不存在:{}", editor.name, exe);
    }

    let mut cmd = Command::new(exe_path);
    cmd.arg(path);
    hide_console_window(&mut cmd);

    cmd.spawn()
        .map(|_| ())
        .map_err(|e| anyhow!("启动编辑器「{}」失败:{}", editor.name, e))
}

/// 用系统默认应用打开文件/目录。
pub fn open_path_with_default_app(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("路径不存在:{}", path.display());
    }

    // 各平台自带的打开器。Windows 上 explorer 对文件走默认关联程序、对目录开资源管理器,
    // 且不像 `cmd /C start` 那样要担心 `&` 之类字符被 shell 二次解析。
    // (explorer 成功时也可能返回非 0 退出码,所以只 spawn 不等 status。)
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("explorer.exe");
        c.arg(path);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(path);
        c
    };

    hide_console_window(&mut cmd);
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| anyhow!("打开失败:{}", e))
}

/// Windows GUI 应用下 spawn 控制台子进程会闪黑框,统一抑制掉。
fn hide_console_window(_cmd: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        _cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editors() -> Vec<Editor> {
        vec![
            Editor {
                name: "VS Code".into(),
                command: "code.exe".into(),
            },
            Editor {
                name: "Zed".into(),
                command: "zed.exe".into(),
            },
        ]
    }

    #[test]
    fn select_by_explicit_name() {
        let list = editors();
        assert_eq!(
            select_editor(&list, Some("VS Code"), Some("Zed"))
                .unwrap()
                .name,
            "Zed"
        );
    }

    #[test]
    fn explicit_name_not_found_is_none() {
        let list = editors();
        assert!(select_editor(&list, Some("VS Code"), Some("Emacs")).is_none());
    }

    #[test]
    fn falls_back_to_default_then_first() {
        let list = editors();
        assert_eq!(select_editor(&list, Some("Zed"), None).unwrap().name, "Zed");
        // default 指向不存在的条目 → 退回列表第一个
        assert_eq!(
            select_editor(&list, Some("Emacs"), None).unwrap().name,
            "VS Code"
        );
        assert_eq!(select_editor(&list, None, None).unwrap().name, "VS Code");
        assert!(select_editor(&[], None, None).is_none());
    }

    #[test]
    fn open_without_editor_gives_actionable_hint() {
        let err = open_in_editor(None, Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("尚未配置外部编辑器"));
    }

    #[test]
    fn open_rejects_empty_or_missing_executable() {
        let empty = Editor {
            name: "空".into(),
            command: "   ".into(),
        };
        let err = open_in_editor(Some(&empty), Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("可执行文件路径为空"));

        let missing = Editor {
            name: "幽灵".into(),
            command: "definitely-not-here-xyz.exe".into(),
        };
        let err = open_in_editor(Some(&missing), Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("路径不存在"));
    }

    #[test]
    fn open_default_app_rejects_missing_path() {
        let err = open_path_with_default_app(Path::new("definitely-not-here-xyz"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("路径不存在"));
    }
}
