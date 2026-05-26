use std::path::Path;
use std::process::Command;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

/// 使用用户在设置中配置的外部编辑器打开指定路径。
///
/// 编辑器列表由后端直接从 config.json 读取（`AppConfig.editors`），
/// 前端仅传入要打开的目录/文件路径和可选的编辑器名称。
/// 若未指定名称，使用 `default_editor`；若无默认则取列表第一个。
#[tauri::command]
pub fn open_in_editor(
    app: AppHandle,
    path: String,
    editor_name: Option<String>,
) -> Result<(), String> {
    let cfg = crate::config::read_config(&app);

    let editor = if let Some(ref name) = editor_name {
        cfg.editors.iter().find(|e| e.name == *name)
    } else {
        cfg.default_editor
            .as_ref()
            .and_then(|name| cfg.editors.iter().find(|e| &e.name == name))
            .or_else(|| cfg.editors.first())
    };

    let editor = editor
        .ok_or("尚未配置外部编辑器,请在『设置 → 系统设置 → 外部编辑器』中添加。".to_string())?;

    let exe = editor.command.trim();
    if exe.is_empty() {
        return Err(format!("编辑器「{}」的可执行文件路径为空", editor.name));
    }

    let exe_path = Path::new(exe);
    if !exe_path.exists() {
        return Err(format!("编辑器「{}」的路径不存在:{}", editor.name, exe));
    }

    let mut cmd = Command::new(exe_path);
    cmd.arg(&path);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("启动编辑器「{}」失败:{}", editor.name, e))
}

/// 用系统默认应用打开文件/目录。
///
/// 把入口收拢到后端 command 后，前端 capability 可以移除 allow-open-path，缩小攻击面。
#[tauri::command]
pub fn open_path_with_default_app(app: AppHandle, path: String) -> Result<(), String> {
    let p = Path::new(&path);
    if !p.exists() {
        return Err(format!("路径不存在:{}", path));
    }
    app.opener()
        .open_path(&path, None::<&str>)
        .map_err(|e| format!("打开失败:{}", e))
}
