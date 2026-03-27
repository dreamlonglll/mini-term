use ignore::gitignore::Gitignore;
use notify::{RecommendedWatcher, RecursiveMode, Watcher, Event as NotifyEvent};
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

fn build_gitignore(project_root: &Path) -> Option<Gitignore> {
    let gitignore_path = project_root.join(".gitignore");
    if !gitignore_path.exists() {
        return None;
    }
    let (gi, _err) = Gitignore::new(&gitignore_path);
    Some(gi)
}

const ALWAYS_IGNORE: &[&str] = &[".git", "node_modules", "target", ".next", "dist", "__pycache__", ".superpowers"];

fn should_ignore(name: &str, full_path: &Path, is_dir: bool, gitignore: &Option<Gitignore>) -> bool {
    if is_dir && ALWAYS_IGNORE.contains(&name) {
        return true;
    }
    if let Some(gi) = gitignore {
        return gi.matched(full_path, is_dir).is_ignore();
    }
    false
}

#[tauri::command]
pub fn list_directory(project_root: String, path: String) -> Result<Vec<FileEntry>, String> {
    let dir = Path::new(&path);
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", path));
    }
    let gitignore = build_gitignore(Path::new(&project_root));
    let mut entries: Vec<FileEntry> = fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().ok()?.is_dir();
            let full_path = entry.path();
            if should_ignore(&name, &full_path, is_dir, &gitignore) {
                return None;
            }
            Some(FileEntry {
                name,
                path: full_path.to_string_lossy().to_string(),
                is_dir,
            })
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FsChangePayload {
    project_path: String,
    path: String,
    kind: String,
}

pub struct FsWatcherManager {
    watchers: Arc<Mutex<HashMap<String, RecommendedWatcher>>>,
}

impl FsWatcherManager {
    pub fn new() -> Self {
        Self { watchers: Arc::new(Mutex::new(HashMap::new())) }
    }
}

#[tauri::command]
pub fn watch_directory(
    app: AppHandle,
    state: tauri::State<'_, FsWatcherManager>,
    path: String,
    project_path: String,
) -> Result<(), String> {
    let watch_path = PathBuf::from(&path);
    let project_path_clone = project_path.clone();
    let app_clone = app.clone();

    let mut watcher = notify::recommended_watcher(move |res: Result<NotifyEvent, _>| {
        if let Ok(event) = res {
            for p in &event.paths {
                let _ = app_clone.emit("fs-change", FsChangePayload {
                    project_path: project_path_clone.clone(),
                    path: p.to_string_lossy().to_string(),
                    kind: format!("{:?}", event.kind),
                });
            }
        }
    }).map_err(|e| e.to_string())?;

    watcher.watch(&watch_path, RecursiveMode::NonRecursive).map_err(|e| e.to_string())?;

    let mut watchers = state.watchers.lock().unwrap();
    watchers.insert(path, watcher);
    Ok(())
}

#[tauri::command]
pub fn create_file(path: String) -> Result<(), String> {
    let p = Path::new(&path);
    if p.exists() {
        return Err(format!("已存在: {}", path));
    }
    fs::write(p, "").map_err(|e| e.to_string())
}

#[tauri::command]
pub fn create_directory(path: String) -> Result<(), String> {
    let p = Path::new(&path);
    if p.exists() {
        return Err(format!("已存在: {}", path));
    }
    fs::create_dir(p).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn unwatch_directory(state: tauri::State<'_, FsWatcherManager>, path: String) -> Result<(), String> {
    let mut watchers = state.watchers.lock().unwrap();
    watchers.remove(&path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_ignore_node_modules() {
        let path = Path::new("node_modules");
        assert!(should_ignore("node_modules", path, true, &None));
        let git_path = Path::new(".git");
        assert!(should_ignore(".git", git_path, true, &None));
    }

    #[test]
    fn should_not_ignore_src() {
        let path = Path::new("src");
        assert!(!should_ignore("src", path, true, &None));
    }
}
