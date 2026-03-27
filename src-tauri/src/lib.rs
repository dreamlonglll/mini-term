mod ai_sessions;
mod config;
mod fs;
mod process_monitor;
mod pty;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(pty::PtyManager::new())
        .manage(fs::FsWatcherManager::new())
        .setup(|app| {
            let pty_manager = app.state::<crate::pty::PtyManager>();
            let pty_clone = pty_manager.inner().clone();
            process_monitor::start_monitor(app.handle().clone(), pty_clone);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            config::load_config,
            config::save_config,
            pty::create_pty,
            pty::write_pty,
            pty::resize_pty,
            pty::kill_pty,
            fs::list_directory,
            fs::watch_directory,
            fs::unwatch_directory,
            fs::create_file,
            fs::create_directory,
            ai_sessions::get_ai_sessions,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
