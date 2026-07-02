mod ai_sessions;
mod cc_connect;
mod clipboard;
mod config;
mod editor;
mod fs;
mod git;
mod hook_registry;
mod hook_server;
mod process_monitor;
mod pty;
mod search;
mod ssh;
mod ssh_mcp_registry;
mod window_theme;
mod wsl_distros;

use tauri::Manager;

#[cfg(windows)]
extern "system" {
    fn ReleaseCapture() -> i32;
    fn GetAsyncKeyState(v_key: i32) -> i16;
}

#[cfg(windows)]
const VK_LBUTTON: i32 = 0x01;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .manage(pty::PtyManager::new())
        .manage(fs::FsWatcherManager::new())
        .manage(search::SearchManager::new())
        .manage(cc_connect::CcConnectManager::new())
        .setup(|app| {
            // identifier 从 com.tauri-app.tauri-app 切换为 com.mini-term.app 后,
            // 第一次启动时把旧 app_data_dir 下的 config.json 拷到新目录,
            // 必须发生在任何 read_config 之前。
            config::migrate_legacy_app_data(app.handle());
            clipboard::cleanup_old_clipboard_images();
            ssh::cleanup_ssh_temp_keys();

            // 初始化 hook 状态并注册为 Tauri managed state
            let hook_state = hook_server::HookState::new();
            app.manage(hook_state.clone());

            // 读取配置，仅当 hookEnabled == true 时才启动 hook server
            let app_config = config::read_config(app.handle());
            if app_config.hook_enabled {
                if let Err(e) =
                    hook_server::start_hook_server(app.handle().clone(), hook_state.clone())
                {
                    eprintln!("[setup] hook server 启动失败: {}", e);
                }
            }

            // 启动进程监控（传入 hook_state 实现 hook 优先 + 轮询降级）
            let pty_manager = app.state::<crate::pty::PtyManager>();
            let pty_clone = pty_manager.inner().clone();
            process_monitor::start_monitor(app.handle().clone(), pty_clone, hook_state);
            Ok(())
        })
        .on_window_event(|_window, event| {
            // 窗口失焦时释放鼠标捕获，防止外部工具（截图等）与 WebView2
            // 事件处理冲突导致输入锁定。
            // 但若用户正按住左键发起 modal move/size loop（拖拽标题栏 /
            // 窗口边缘 resize），WebView2 子窗口会失焦触发该事件，此时
            // ReleaseCapture 会取消系统的鼠标捕获并立即终止 modal loop，
            // 表现为拖拽和 resize "光标变化但不生效"。
            // 因此左键按下时跳过释放，留给系统自然处理；松开时再释放。
            if let tauri::WindowEvent::Focused(false) = event {
                #[cfg(windows)]
                unsafe {
                    if (GetAsyncKeyState(VK_LBUTTON) as u16 & 0x8000) == 0 {
                        ReleaseCapture();
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            config::load_config,
            config::save_config,
            pty::create_pty,
            pty::write_pty,
            pty::resize_pty,
            pty::kill_pty,
            pty::arm_ssh_autofill,
            ssh::prepare_ssh_key,
            fs::list_directory,
            fs::watch_directory,
            fs::unwatch_directory,
            fs::create_file,
            fs::create_directory,
            fs::read_file_content,
            fs::rename_entry,
            fs::delete_entry,
            fs::filter_directories,
            ai_sessions::get_ai_sessions,
            ai_sessions::get_wsl_ai_sessions,
            ai_sessions::get_ai_session_content,
            wsl_distros::list_wsl_distros,
            git::get_git_status,
            git::get_git_diff,
            git::discover_git_repos,
            git::get_git_log,
            git::get_repo_branches,
            git::get_commit_files,
            git::get_commit_file_diff,
            git::git_pull,
            git::git_push,
            git::get_changes_status,
            git::git_stage,
            git::git_unstage,
            git::git_stage_all,
            git::git_unstage_all,
            git::git_commit,
            git::git_discard_file,
            editor::open_in_editor,
            editor::open_path_with_default_app,
            clipboard::read_clipboard_image,
            clipboard::save_clipboard_text,
            search::start_search,
            search::cancel_search,
            hook_registry::register_ai_hooks,
            hook_registry::unregister_ai_hooks,
            hook_registry::get_hook_config_snippet,
            hook_registry::get_hook_status,
            hook_server::toggle_hook_server,
            ssh_mcp_registry::enable_ssh_mcp,
            ssh_mcp_registry::disable_ssh_mcp,
            window_theme::set_window_dark_mode,
            cc_connect::cc_connect_probe,
            cc_connect::cc_connect_read_token,
            cc_connect::cc_connect_config_path,
            cc_connect::cc_connect_start,
            cc_connect::cc_connect_stop,
            cc_connect::cc_connect_restart,
            cc_connect::cc_connect_list_projects,
            cc_connect::cc_connect_import_project,
            cc_connect::cc_connect_import_projects,
            cc_connect::cc_connect_unlink_project,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
