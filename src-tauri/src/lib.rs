mod ai_sessions;
mod clipboard;
mod config;
mod conpty_bootstrap;
mod editor;
mod fs;
mod git;
mod hook_registry;
mod hook_server;
mod mobile_mirror;
mod mobile_relay;
mod process_monitor;
mod pty;
mod remote_ssh;
mod search;
mod ssh;
mod ssh_mcp_registry;
mod window_input_recovery;
mod window_theme;
mod wsl_distros;

use tauri::Manager;

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
        .manage(mobile_relay::MobileRelayManager::new())
        .manage(remote_ssh::RemoteSshState::new())
        .setup(|app| {
            // portable-pty 0.8.1 会在第一次 openpty 时进程级缓存 ConPTY 函数表；
            // 因此便携 DLL 的资源校验和绝对路径预载必须是 setup 的第一项，早于
            // 任何可能创建 PTY 的初始化；预载引用保留到进程退出且不修改 PATH。
            #[cfg(windows)]
            conpty_bootstrap::initialize(app.handle());

            // identifier 从 com.tauri-app.tauri-app 切换为 com.mini-term.app 后,
            // 第一次启动时把旧 app_data_dir 下的 config.json 拷到新目录,
            // 必须发生在任何 read_config 之前。
            config::migrate_legacy_app_data(app.handle());
            clipboard::cleanup_old_clipboard_images();
            ssh::cleanup_ssh_temp_keys();

            // 初始化 hook 状态并注册为 Tauri managed state
            let hook_state = hook_server::HookState::new();
            app.manage(hook_state.clone());

            // 状态发射器:monitor 轮询与 hook server 直推共用同一份去重表,
            // 避免迟到 hook 事件推错状态后 monitor 的纠正被去重吞掉
            let status_emitter = process_monitor::StatusEmitter::new();
            app.manage(status_emitter.clone());

            // 读取配置，仅当 hookEnabled == true 时才启动 hook server
            let app_config = config::read_config(app.handle());
            if app_config.hook_enabled {
                if let Err(e) = hook_server::start_hook_server(
                    app.handle().clone(),
                    hook_state.clone(),
                    status_emitter.clone(),
                ) {
                    eprintln!("[setup] hook server 启动失败: {}", e);
                }
            }

            // 启动进程监控（传入 hook_state 实现 hook 优先 + 轮询降级）
            let pty_manager = app.state::<crate::pty::PtyManager>();
            let pty_clone = pty_manager.inner().clone();
            process_monitor::start_monitor(
                app.handle().clone(),
                pty_clone,
                hook_state,
                status_emitter,
            );

            // 已配置中转地址时,启动对中转服务器的出站长连(断线自动指数退避重连)
            if let Some(relay) = app_config.mobile_relay.as_ref() {
                if !relay.relay_url.trim().is_empty() {
                    app.state::<mobile_relay::MobileRelayManager>().apply(
                        app.handle(),
                        &relay.relay_url,
                        &relay.desktop_key,
                    );
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // 窗口失焦时释放鼠标捕获，防止外部工具（截图等）与 WebView2
            // 事件处理冲突导致输入锁定。
            // 左键按下时不能立即取消正常的拖动/缩放；window_input_recovery
            // 会等待松开后投递 WM_CANCELMODE，补上此前缺失的延迟清理。
            if let tauri::WindowEvent::Focused(false) = event {
                window_input_recovery::recover_after_focus_loss(window);
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
            remote_ssh::ssh_remote_list_directory,
            remote_ssh::ssh_remote_validate_dir,
            remote_ssh::ssh_remote_ai_sessions,
            remote_ssh::ssh_remote_ai_session_content,
            remote_ssh::ssh_remote_upload_paste,
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
            git::list_worktrees,
            git::add_worktree,
            git::remove_worktree,
            git::prune_worktrees,
            git::get_worktree_branches,
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
            mobile_relay::mobile_relay_apply,
            mobile_relay::mobile_relay_status,
            mobile_relay::mobile_relay_request_pairing_code,
            mobile_relay::mobile_relay_reset_pairing,
            mobile_relay::mobile_relay_update_sessions,
            mobile_relay::mobile_relay_launchers_changed,
            mobile_relay::mobile_relay_start_session_result,
            mobile_relay::mobile_relay_check_launcher_command,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // app 退出时优雅关掉远程 SSH 会话池(abort reaper + 并发 disconnect,
            // 单 session 2s 超时),避免远端留 dangling session 只能等 TCP 超时回收。
            // 对齐 mt-ssh-mcp sidecar 退出前 pool.shutdown() 的钩子。
            if let tauri::RunEvent::Exit = event {
                app_handle
                    .state::<remote_ssh::RemoteSshState>()
                    .shutdown_pool_blocking();
            }
        });
}
