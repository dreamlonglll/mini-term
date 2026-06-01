use ignore::gitignore::Gitignore;
use notify::{Event as NotifyEvent, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};

/// 原子写文件:先写到同目录的临时文件,fsync 后再 rename 覆盖目标。
///
/// rename 在同一卷上是原子操作(Windows 下 Rust 的 std::fs::rename 走
/// MOVEFILE_REPLACE_EXISTING,可原子替换已存在文件),因此即便写入过程中崩溃/断电/
/// 磁盘满,目标文件要么是旧内容、要么是完整新内容,绝不会留下被截断的半截文件。
/// 用于所有「覆盖用户/全局既有配置」的写入(config.json、config.toml、.mcp.json、
/// settings.json、hooks.json 等),避免裸 fs::write 的 truncate-then-write 损坏用户配置。
pub fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "目标路径没有父目录")
    })?;
    // 临时文件必须与目标同目录,保证同卷,rename 才能原子
    let seq = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("tmp");
    let tmp = dir.join(format!(".{}.{}.{}.tmp", stem, std::process::id(), seq));

    let write_result = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.flush()?;
        let _ = f.sync_all(); // sync 失败不致命,尽力而为
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // 若目标已存在,把其权限位复制到临时文件,避免 rename 后权限退化为 umask 默认值
    // (Unix 下保护用户 chmod 600 的含 token 配置不被降级为 0644;Windows 上对应只读位)。
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }

    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn natural_cmp(a: &str, b: &str) -> Ordering {
    let a = a.to_lowercase();
    let b = b.to_lowercase();
    let mut ai = a.as_bytes().iter().peekable();
    let mut bi = b.as_bytes().iter().peekable();

    loop {
        match (ai.peek(), bi.peek()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(&&ac), Some(&&bc)) => {
                if ac.is_ascii_digit() && bc.is_ascii_digit() {
                    let mut an: u64 = 0;
                    while let Some(&&d) = ai.peek() {
                        if !d.is_ascii_digit() {
                            break;
                        }
                        an = an * 10 + (d - b'0') as u64;
                        ai.next();
                    }
                    let mut bn: u64 = 0;
                    while let Some(&&d) = bi.peek() {
                        if !d.is_ascii_digit() {
                            break;
                        }
                        bn = bn * 10 + (d - b'0') as u64;
                        bi.next();
                    }
                    match an.cmp(&bn) {
                        Ordering::Equal => continue,
                        ord => return ord,
                    }
                } else {
                    match ac.cmp(&bc) {
                        Ordering::Equal => {
                            ai.next();
                            bi.next();
                        }
                        ord => return ord,
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub ignored: bool,
}

/// 从 project_root 到 current 逐级收集 .gitignore，返回顺序为「根 → 当前」
///
/// 参考 git 的处理方式：每一层子目录都可以有自己的 .gitignore，
/// 子目录规则优先级高于父级（可通过 `!pattern` 取消父级的忽略）。
fn collect_gitignores(project_root: &Path, current: &Path) -> Vec<Gitignore> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut cur = current.to_path_buf();
    loop {
        dirs.push(cur.clone());
        if cur.as_path() == project_root {
            break;
        }
        match cur.parent() {
            Some(parent) if parent.starts_with(project_root) => {
                cur = parent.to_path_buf();
            }
            _ => break,
        }
    }
    dirs.reverse();

    dirs.iter()
        .filter_map(|dir| {
            let gi_path = dir.join(".gitignore");
            if !gi_path.exists() {
                return None;
            }
            let (gi, _err) = Gitignore::new(&gi_path);
            Some(gi)
        })
        .collect()
}

/// 按「根 → 当前」顺序合并 match 结果：后者覆盖前者，支持 `!pattern` 白名单
fn is_path_ignored(gitignores: &[Gitignore], full_path: &Path, is_dir: bool) -> bool {
    let mut ignored = false;
    for gi in gitignores {
        let m = gi.matched(full_path, is_dir);
        if m.is_whitelist() {
            ignored = false;
        } else if m.is_ignore() {
            ignored = true;
        }
    }
    ignored
}

pub const ALWAYS_IGNORE: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".next",
    "dist",
    "__pycache__",
    ".superpowers",
];

/// 纯字符串版剥 Windows verbatim 前缀,跨平台可测:
/// - `\\?\C:\foo` → `Some("C:\\foo")`
/// - `\\?\UNC\wsl$\Ubuntu\home` → `Some("\\\\wsl$\\Ubuntu\\home")`
/// - `\\?\UNC\wsl.localhost\Ubuntu\home` → `Some("\\\\wsl.localhost\\Ubuntu\\home")`
/// - Volume GUID `\\?\Volume{...}` 等其他 verbatim 形式 → `None` (保留原样)
/// - 非 verbatim 路径 → `None`
fn try_strip_windows_verbatim(s: &str) -> Option<String> {
    let rest = s.strip_prefix(r"\\?\")?;
    // UNC verbatim: `\\?\UNC\<host>\<rest>` → `\\<host>\<rest>`
    // canonicalize 在 WSL UNC 上会产出这种形式,前端不剥前缀的话路径无法直接粘进 shell。
    if let Some(unc_rest) = rest.strip_prefix(r"UNC\") {
        return Some(format!(r"\\{}", unc_rest));
    }
    // Drive verbatim: `\\?\<drive>:\...` → `<drive>:\...`
    let bytes = rest.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        return Some(rest.to_string());
    }
    None
}

/// Windows 上 `Path::canonicalize()` 会给路径加上 `\\?\` verbatim 前缀
/// (绕过 MAX_PATH 限制),这种形式回传前端后拖进 shell 不友好。
/// 同时剥掉盘符 `\\?\C:\...` 与 UNC `\\?\UNC\<host>\...` 两种形式;
/// Volume GUID 等其他特殊前缀保留不动。
#[cfg(windows)]
fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    match try_strip_windows_verbatim(&p.to_string_lossy()) {
        Some(stripped) => PathBuf::from(stripped),
        None => p,
    }
}

#[cfg(not(windows))]
fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    p
}

/// 校验 target 必须在 project_root 内,防止前端构造 `../../etc/passwd` 之类的
/// 路径逃逸出项目根目录。
///
/// 用 `canonicalize` 同时解析符号链接和 `..`,要求 project_root 必须存在。
/// `must_exist=true` 时 target 也必须存在(用于 list/read/rename 旧路径);
/// `must_exist=false` 时仅 canonicalize 父目录后拼上 file_name,允许 target
/// 本身不存在(用于 create_file/create_directory 这类创建场景)。
///
/// 返回校验后的绝对路径(Windows 上已剥 `\\?\` 前缀),后续 IO 直接用它,
/// 避免重复访问磁盘。
fn verify_under_project_root(
    project_root: &str,
    target: &str,
    must_exist: bool,
) -> Result<PathBuf, String> {
    let root = Path::new(project_root)
        .canonicalize()
        .map(strip_verbatim_prefix)
        .map_err(|e| format!("项目根目录无效: {}: {}", project_root, e))?;

    let target_path = Path::new(target);
    let canon = if must_exist {
        target_path
            .canonicalize()
            .map(strip_verbatim_prefix)
            .map_err(|e| format!("路径不可访问: {}: {}", target, e))?
    } else {
        let parent = target_path
            .parent()
            .ok_or_else(|| format!("无法获取父目录: {}", target))?;
        let parent_canon = parent
            .canonicalize()
            .map(strip_verbatim_prefix)
            .map_err(|e| format!("父目录不可访问: {}: {}", parent.display(), e))?;
        let name = target_path
            .file_name()
            .ok_or_else(|| format!("缺少文件名: {}", target))?;
        parent_canon.join(name)
    };

    if !canon.starts_with(&root) {
        return Err(format!(
            "路径不在项目根目录内: {} (root={})",
            canon.display(),
            root.display()
        ));
    }
    Ok(canon)
}

/// 过滤出有效的目录路径（用于拖拽添加项目时验证）
#[tauri::command]
pub fn filter_directories(paths: Vec<String>) -> Vec<String> {
    paths
        .into_iter()
        .filter(|p| Path::new(p).is_dir())
        .collect()
}

#[tauri::command]
pub fn list_directory(project_root: String, path: String) -> Result<Vec<FileEntry>, String> {
    let dir = verify_under_project_root(&project_root, &path, true)?;
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", path));
    }
    let gitignores = collect_gitignores(Path::new(&project_root), &dir);
    let mut entries: Vec<FileEntry> = fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().ok()?.is_dir();
            let full_path = entry.path();
            // ALWAYS_IGNORE 目录仍然完全隐藏
            if is_dir && ALWAYS_IGNORE.contains(&name.as_str()) {
                return None;
            }
            let ignored = is_path_ignored(&gitignores, &full_path, is_dir);
            Some(FileEntry {
                name,
                path: full_path.to_string_lossy().to_string(),
                is_dir,
                ignored,
            })
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.ignored.cmp(&b.ignored))
            .then_with(|| natural_cmp(&a.name, &b.name))
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
        Self {
            watchers: Arc::new(Mutex::new(HashMap::new())),
        }
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
                let _ = app_clone.emit(
                    "fs-change",
                    FsChangePayload {
                        project_path: project_path_clone.clone(),
                        path: p.to_string_lossy().to_string(),
                        kind: format!("{:?}", event.kind),
                    },
                );
            }
        }
    })
    .map_err(|e| e.to_string())?;

    watcher
        .watch(&watch_path, RecursiveMode::NonRecursive)
        .map_err(|e| e.to_string())?;

    let mut watchers = state.watchers.lock().unwrap();
    watchers.insert(path, watcher);
    Ok(())
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileContentResult {
    pub content: String,
    pub is_binary: bool,
    pub too_large: bool,
}

const MAX_FILE_VIEW_SIZE: u64 = 1_048_576; // 1MB

#[tauri::command]
pub fn read_file_content(project_root: String, path: String) -> Result<FileContentResult, String> {
    let p = verify_under_project_root(&project_root, &path, true)?;
    if !p.is_file() {
        return Err(format!("不是文件: {}", path));
    }
    let metadata = fs::metadata(&p).map_err(|e| e.to_string())?;
    if metadata.len() > MAX_FILE_VIEW_SIZE {
        return Ok(FileContentResult {
            content: String::new(),
            is_binary: false,
            too_large: true,
        });
    }
    let bytes = fs::read(&p).map_err(|e| e.to_string())?;
    match String::from_utf8(bytes) {
        Ok(s) => Ok(FileContentResult {
            content: s,
            is_binary: false,
            too_large: false,
        }),
        Err(_) => Ok(FileContentResult {
            content: String::new(),
            is_binary: true,
            too_large: false,
        }),
    }
}

#[tauri::command]
pub fn create_file(project_root: String, path: String) -> Result<(), String> {
    let p = verify_under_project_root(&project_root, &path, false)?;
    if p.exists() {
        return Err(format!("已存在: {}", path));
    }
    fs::write(&p, "").map_err(|e| e.to_string())
}

#[tauri::command]
pub fn create_directory(project_root: String, path: String) -> Result<(), String> {
    let p = verify_under_project_root(&project_root, &path, false)?;
    if p.exists() {
        return Err(format!("已存在: {}", path));
    }
    fs::create_dir(&p).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn unwatch_directory(
    state: tauri::State<'_, FsWatcherManager>,
    path: String,
) -> Result<(), String> {
    let mut watchers = state.watchers.lock().unwrap();
    watchers.remove(&path);
    Ok(())
}

#[tauri::command]
pub fn rename_entry(
    project_root: String,
    old_path: String,
    new_name: String,
) -> Result<String, String> {
    let old_canon = verify_under_project_root(&project_root, &old_path, true)?;
    let parent = old_canon
        .parent()
        .ok_or_else(|| "无法获取父目录".to_string())?;
    let new_path = parent.join(&new_name);
    // new_name 可能含 `../` 等,必须再校验一遍新路径仍在 project_root 内
    let new_canon =
        verify_under_project_root(&project_root, new_path.to_string_lossy().as_ref(), false)?;
    if new_canon.exists() {
        return Err(format!("目标已存在: {}", new_canon.display()));
    }
    fs::rename(&old_canon, &new_canon).map_err(|e| e.to_string())?;
    Ok(new_canon.to_string_lossy().to_string())
}

#[tauri::command]
pub fn delete_entry(project_root: String, path: String) -> Result<(), String> {
    let target = verify_under_project_root(&project_root, &path, true)?;
    // 多一道保险:绝不允许删除项目根目录本身
    // 必须同样剥掉 `\\?\`,否则与 verify_under_project_root 返回的 target 形式不一致
    let root = Path::new(&project_root)
        .canonicalize()
        .map(strip_verbatim_prefix)
        .map_err(|e| e.to_string())?;
    if target == root {
        return Err("不能删除项目根目录".to_string());
    }
    if target.is_dir() {
        fs::remove_dir_all(&target).map_err(|e| e.to_string())
    } else {
        fs::remove_file(&target).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_ignore_contains_common_build_dirs() {
        assert!(ALWAYS_IGNORE.contains(&".git"));
        assert!(ALWAYS_IGNORE.contains(&"node_modules"));
        assert!(ALWAYS_IGNORE.contains(&"target"));
    }

    #[test]
    fn is_path_ignored_empty_returns_false() {
        assert!(!is_path_ignored(&[], Path::new("/any/path"), false));
    }

    #[test]
    fn atomic_write_creates_and_overwrites() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mini-term-atomic-{ts}"));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("conf.json");

        // 目标不存在 → 创建
        atomic_write(&target, b"first").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "first");

        // 目标已存在 → 原子覆盖(Windows 下也应成功,验证 rename 替换语义)
        atomic_write(&target, b"second-longer-content").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "second-longer-content");

        // 不应残留任何 .tmp 临时文件
        let leftover: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "残留临时文件: {:?}", leftover);

        let _ = fs::remove_dir_all(&dir);
    }

    fn make_test_project() -> (PathBuf, PathBuf) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mini-term-fs-test-{ts}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let inner_file = root.join("inside.txt");
        fs::write(&inner_file, "hi").unwrap();
        (root, inner_file)
    }

    #[test]
    fn verify_accepts_path_inside_project() {
        let (root, file) = make_test_project();
        let canon = verify_under_project_root(
            root.to_string_lossy().as_ref(),
            file.to_string_lossy().as_ref(),
            true,
        )
        .unwrap();
        assert!(canon.starts_with(strip_verbatim_prefix(root.canonicalize().unwrap())));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn verify_rejects_dotdot_escape() {
        let (root, _) = make_test_project();
        // 构造一个理论上指向 root 之外的相对路径(../something)
        let escape = root.join("..").join("definitely-not-here.txt");
        let err = verify_under_project_root(
            root.to_string_lossy().as_ref(),
            escape.to_string_lossy().as_ref(),
            false,
        )
        .unwrap_err();
        assert!(err.contains("不在项目根目录内") || err.contains("不可访问"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn verify_rejects_unrelated_absolute_path() {
        let (root, _) = make_test_project();
        // 创建另一个完全独立的目录,模拟"读项目外的文件"
        let other = std::env::temp_dir().join(format!(
            "mini-term-fs-other-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&other).unwrap();
        let other_file = other.join("evil.txt");
        fs::write(&other_file, "x").unwrap();

        let err = verify_under_project_root(
            root.to_string_lossy().as_ref(),
            other_file.to_string_lossy().as_ref(),
            true,
        )
        .unwrap_err();
        assert!(err.contains("不在项目根目录内"));

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&other).ok();
    }

    #[test]
    fn rename_entry_inside_project_succeeds() {
        let (root, old_file) = make_test_project();
        let result = rename_entry(
            root.to_string_lossy().to_string(),
            old_file.to_string_lossy().to_string(),
            "renamed.txt".to_string(),
        );
        assert!(result.is_ok(), "rename 失败: {:?}", result);
        let new_path = root.join("renamed.txt");
        assert!(new_path.exists(), "新文件应存在: {}", new_path.display());
        assert!(!old_file.exists(), "旧文件应被移除");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rename_entry_dotdot_in_new_name_rejected() {
        let (root, old_file) = make_test_project();
        let result = rename_entry(
            root.to_string_lossy().to_string(),
            old_file.to_string_lossy().to_string(),
            "../escape.txt".to_string(),
        );
        assert!(result.is_err(), "应拒绝 ../ 逃逸");
        // 旧文件应未被改动
        assert!(old_file.exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn delete_entry_file_inside_project_succeeds() {
        let (root, file) = make_test_project();
        let result = delete_entry(
            root.to_string_lossy().to_string(),
            file.to_string_lossy().to_string(),
        );
        assert!(result.is_ok(), "delete 失败: {:?}", result);
        assert!(!file.exists(), "文件应被删除");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn delete_entry_directory_recursively() {
        let (root, _) = make_test_project();
        let sub = root.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("nested.txt"), "x").unwrap();
        let result = delete_entry(
            root.to_string_lossy().to_string(),
            sub.to_string_lossy().to_string(),
        );
        assert!(result.is_ok(), "目录删除失败: {:?}", result);
        assert!(!sub.exists(), "子目录应被递归删除");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn delete_entry_rejects_path_outside_project() {
        let (root, _) = make_test_project();
        let other = std::env::temp_dir().join(format!(
            "mini-term-fs-other-del-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&other).unwrap();
        let other_file = other.join("evil.txt");
        fs::write(&other_file, "x").unwrap();

        let err = delete_entry(
            root.to_string_lossy().to_string(),
            other_file.to_string_lossy().to_string(),
        )
        .unwrap_err();
        assert!(err.contains("不在项目根目录内"));
        assert!(other_file.exists(), "项目外的文件不应被删除");

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&other).ok();
    }

    #[test]
    fn delete_entry_rejects_project_root_itself() {
        let (root, _) = make_test_project();
        let err = delete_entry(
            root.to_string_lossy().to_string(),
            root.to_string_lossy().to_string(),
        )
        .unwrap_err();
        assert!(err.contains("不能删除项目根目录"));
        assert!(root.exists(), "项目根目录不应被删除");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn verify_create_file_in_project() {
        let (root, _) = make_test_project();
        let new_file = root.join("brand-new.txt");
        let canon = verify_under_project_root(
            root.to_string_lossy().as_ref(),
            new_file.to_string_lossy().as_ref(),
            false,
        )
        .unwrap();
        assert!(canon.starts_with(strip_verbatim_prefix(root.canonicalize().unwrap())));
        assert!(!canon.exists()); // 文件还没创建
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn try_strip_drive_verbatim() {
        assert_eq!(
            try_strip_windows_verbatim(r"\\?\C:\foo\bar"),
            Some(r"C:\foo\bar".to_string())
        );
        assert_eq!(
            try_strip_windows_verbatim(r"\\?\D:\"),
            Some(r"D:\".to_string())
        );
    }

    #[test]
    fn try_strip_unc_verbatim_wsl_dollar() {
        assert_eq!(
            try_strip_windows_verbatim(r"\\?\UNC\wsl$\Ubuntu\home\user"),
            Some(r"\\wsl$\Ubuntu\home\user".to_string())
        );
    }

    #[test]
    fn try_strip_unc_verbatim_wsl_localhost() {
        assert_eq!(
            try_strip_windows_verbatim(r"\\?\UNC\wsl.localhost\Ubuntu\home\user"),
            Some(r"\\wsl.localhost\Ubuntu\home\user".to_string())
        );
    }

    #[test]
    fn try_strip_unc_verbatim_generic_server() {
        // 非 WSL 的 UNC 也应剥前缀(canonicalize 对任何 UNC 都会加前缀)
        assert_eq!(
            try_strip_windows_verbatim(r"\\?\UNC\server\share\folder"),
            Some(r"\\server\share\folder".to_string())
        );
    }

    #[test]
    fn try_strip_volume_guid_returns_none() {
        // Volume GUID 形式不剥(保留原行为,这种路径通常用户也不会拿到)
        assert!(try_strip_windows_verbatim(
            r"\\?\Volume{12345678-1234-1234-1234-123456789012}\foo"
        )
        .is_none());
    }

    #[test]
    fn try_strip_non_verbatim_returns_none() {
        assert!(try_strip_windows_verbatim(r"C:\foo").is_none());
        assert!(try_strip_windows_verbatim(r"\\wsl$\Ubuntu\home").is_none());
        assert!(try_strip_windows_verbatim("/home/user").is_none());
        assert!(try_strip_windows_verbatim("").is_none());
    }

    /// host 名大小写不该被 try_strip 改写(strip 是纯字符串提取,
    /// 不归一化大小写;归一化由 wsl_path::parse_unc 负责)
    #[test]
    fn try_strip_preserves_host_case() {
        assert_eq!(
            try_strip_windows_verbatim(r"\\?\UNC\WSL$\Ubuntu\home"),
            Some(r"\\WSL$\Ubuntu\home".to_string())
        );
        assert_eq!(
            try_strip_windows_verbatim(r"\\?\UNC\Wsl.LocalHost\Ubuntu\home"),
            Some(r"\\Wsl.LocalHost\Ubuntu\home".to_string())
        );
    }

    /// `\\?\UNC\` 后只跟一个 host 而无 share/rest 也应剥成 `\\<host>`
    #[test]
    fn try_strip_unc_host_only() {
        assert_eq!(
            try_strip_windows_verbatim(r"\\?\UNC\wsl$"),
            Some(r"\\wsl$".to_string())
        );
    }

    // ─── PathBuf 包装版(cfg(windows))与 verify_under_project_root 集成 ───

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_prefix_pathbuf_strips_drive_form() {
        let stripped = strip_verbatim_prefix(PathBuf::from(r"\\?\C:\Users\u\proj"));
        assert_eq!(stripped, PathBuf::from(r"C:\Users\u\proj"));
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_prefix_pathbuf_strips_unc_form() {
        let stripped = strip_verbatim_prefix(PathBuf::from(r"\\?\UNC\wsl$\Ubuntu\home\user\proj"));
        assert_eq!(stripped, PathBuf::from(r"\\wsl$\Ubuntu\home\user\proj"));
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_prefix_pathbuf_is_noop_on_volume_guid() {
        // Volume GUID 形式保留原样(verbatim 但不在我们处理的两类前缀里)
        let original = PathBuf::from(r"\\?\Volume{12345678-1234-1234-1234-123456789012}\foo");
        let stripped = strip_verbatim_prefix(original.clone());
        assert_eq!(stripped, original);
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_prefix_pathbuf_is_noop_on_already_clean_path() {
        let original = PathBuf::from(r"C:\Users\u\proj");
        let stripped = strip_verbatim_prefix(original.clone());
        assert_eq!(stripped, original);
    }

    /// 在 Windows 上 `canonicalize` 临时目录会得到 `\\?\C:\...` 形式;
    /// 经过 verify_under_project_root 之后,返回值必须已剥掉 verbatim 前缀,
    /// 否则前端拿到的路径拖进 shell 不友好。
    #[cfg(windows)]
    #[test]
    fn verify_strips_verbatim_prefix_in_result() {
        let (root, file) = make_test_project();
        let canon = verify_under_project_root(
            root.to_string_lossy().as_ref(),
            file.to_string_lossy().as_ref(),
            true,
        )
        .unwrap();
        let s = canon.to_string_lossy();
        assert!(
            !s.starts_with(r"\\?\"),
            "verify 返回的路径不应包含 \\?\\ verbatim 前缀: {s}"
        );
        fs::remove_dir_all(&root).ok();
    }

    /// canonicalize 直接传 verbatim 路径仍能 work,verify 返回的剥前缀路径
    /// 与原路径(剥前缀后)应等价 —— 验证 root 与 target 都剥前缀后
    /// starts_with 比较的对称性。
    #[cfg(windows)]
    #[test]
    fn verify_equivalence_between_verbatim_and_plain_input() {
        let (root, file) = make_test_project();
        let plain = verify_under_project_root(
            root.to_string_lossy().as_ref(),
            file.to_string_lossy().as_ref(),
            true,
        )
        .unwrap();
        // 用 canonicalize 拿到的 verbatim 形式作为输入,verify 后应该剥成同样结果
        let verbatim_root = root.canonicalize().unwrap();
        let verbatim_file = file.canonicalize().unwrap();
        let from_verbatim = verify_under_project_root(
            verbatim_root.to_string_lossy().as_ref(),
            verbatim_file.to_string_lossy().as_ref(),
            true,
        )
        .unwrap();
        assert_eq!(plain, from_verbatim);
        fs::remove_dir_all(&root).ok();
    }
}
