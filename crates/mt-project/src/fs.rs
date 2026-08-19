//! 文件树的列举与增删改,以及所有「覆盖既有配置」都要用的原子写。
//!
//! 从 `src-tauri/src/fs.rs` 移入。去掉了 `#[tauri::command]`,路径参数由
//! `String` 换成 `&Path`,错误从 `Result<T, String>` 换成 `anyhow::Result<T>` ——
//! 面向用户的错误文案一字未改(前端曾直接把它们弹出来,GPUI 侧同样要能直接显示)。
//!
//! 目录监听(`fs-change`)不在本模块,见 [`crate::watch`]。

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use ignore::gitignore::Gitignore;
use serde::Serialize;

/// 原子写文件:先写到同目录的临时文件,fsync 后再 rename 覆盖目标。
///
/// 收尾-1 批把工作区里的三份逐字副本(本模块、`mt_config::config`、`mt_ai::util`)
/// 合并进叶子 crate `mt-core`,这里改为再导出 —— 公开路径
/// `mt_project::fs::atomic_write` 与函数签名一字未改,本模块内的调用点
/// (`write_file_content`)和下面的回归测试也照旧。
/// 实现与「为什么必须原子写」的完整说明见 `mt_core::atomic_write`。
pub use mt_core::atomic_write;

/// 自然排序比较(数字段按数值比)。公开出去:远程文件树(尚未移植的
/// `remote_ssh.rs`)将复用同一排序规则,保证本地/远程树观感一致。
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
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

/// 文件树的一行。`path` 由 `String` 改为 `PathBuf` —— 原来是为了序列化给
/// 前端才拍平成字符串,现在消费方就在同一进程里。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
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

/// 永远不进文件树、也永远不进搜索的目录名。
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
    // canonicalize 在 WSL UNC 上会产出这种形式,不剥前缀的话路径无法直接粘进 shell。
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
/// (绕过 MAX_PATH 限制),这种形式拖进 shell 不友好。
/// 同时剥掉盘符 `\\?\C:\...` 与 UNC `\\?\UNC\<host>\...` 两种形式;
/// Volume GUID 等其他特殊前缀保留不动。
#[cfg(windows)]
pub fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    match try_strip_windows_verbatim(&p.to_string_lossy()) {
        Some(stripped) => PathBuf::from(stripped),
        None => p,
    }
}

#[cfg(not(windows))]
pub fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    p
}

/// 校验 target 必须在 project_root 内,防止调用方(UI 里的重命名输入框、
/// 拖放来的路径等)构造 `../../etc/passwd` 之类的路径逃逸出项目根目录。
///
/// 用 `canonicalize` 同时解析符号链接和 `..`,要求 project_root 必须存在。
/// `must_exist=true` 时 target 也必须存在(用于 list/read/rename 旧路径);
/// `must_exist=false` 时仅 canonicalize 父目录后拼上 file_name,允许 target
/// 本身不存在(用于 create_file/create_directory 这类创建场景)。
///
/// 返回校验后的绝对路径(Windows 上已剥 `\\?\` 前缀),后续 IO 直接用它,
/// 避免重复访问磁盘。
fn verify_under_project_root(
    project_root: &Path,
    target: &Path,
    must_exist: bool,
) -> Result<PathBuf> {
    let root = project_root
        .canonicalize()
        .map(strip_verbatim_prefix)
        .map_err(|e| anyhow!("项目根目录无效: {}: {}", project_root.display(), e))?;

    let canon = if must_exist {
        target
            .canonicalize()
            .map(strip_verbatim_prefix)
            .map_err(|e| anyhow!("路径不可访问: {}: {}", target.display(), e))?
    } else {
        let parent = target
            .parent()
            .ok_or_else(|| anyhow!("无法获取父目录: {}", target.display()))?;
        let parent_canon = parent
            .canonicalize()
            .map(strip_verbatim_prefix)
            .map_err(|e| anyhow!("父目录不可访问: {}: {}", parent.display(), e))?;
        let name = target
            .file_name()
            .ok_or_else(|| anyhow!("缺少文件名: {}", target.display()))?;
        parent_canon.join(name)
    };

    if !canon.starts_with(&root) {
        bail!(
            "路径不在项目根目录内: {} (root={})",
            canon.display(),
            root.display()
        );
    }
    Ok(canon)
}

/// 过滤出有效的目录路径（用于拖拽添加项目时验证）
pub fn filter_directories(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.into_iter().filter(|p| p.is_dir()).collect()
}

/// 列举目录:隐藏 [`ALWAYS_IGNORE`],其余按 .gitignore 打 `ignored` 标记(不隐藏),
/// 排序为「目录优先 → 未忽略优先 → 名称自然序」。
pub fn list_directory(project_root: &Path, path: &Path) -> Result<Vec<FileEntry>> {
    let dir = verify_under_project_root(project_root, path, true)?;
    if !dir.is_dir() {
        bail!("Not a directory: {}", path.display());
    }
    let gitignores = collect_gitignores(project_root, &dir);
    let mut entries: Vec<FileEntry> = fs::read_dir(&dir)
        .with_context(|| format!("读取目录失败: {}", dir.display()))?
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
                path: full_path,
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

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileContentResult {
    pub content: String,
    pub is_binary: bool,
    pub too_large: bool,
}

/// 内置查看器/编辑器能打开的最大文件尺寸,读写两侧共用。
pub const MAX_FILE_VIEW_SIZE: u64 = 1_048_576; // 1MB

pub fn read_file_content(project_root: &Path, path: &Path) -> Result<FileContentResult> {
    let p = verify_under_project_root(project_root, path, true)?;
    if !p.is_file() {
        bail!("不是文件: {}", path.display());
    }
    let metadata = fs::metadata(&p)?;
    if metadata.len() > MAX_FILE_VIEW_SIZE {
        return Ok(FileContentResult {
            content: String::new(),
            is_binary: false,
            too_large: true,
        });
    }
    let bytes = fs::read(&p)?;
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

pub fn write_file_content(project_root: &Path, path: &Path, content: &str) -> Result<()> {
    // 与读侧同一上限:编辑器根本打不开 >1MB 的文件,超限内容只可能来自
    // 绕过编辑器直接调本函数的路径,这一层不依赖调用方的约束
    if content.len() as u64 > MAX_FILE_VIEW_SIZE {
        bail!("内容过大(>1MB),拒绝写入");
    }
    let p = verify_under_project_root(project_root, path, true)?;
    if !p.is_file() {
        bail!("不是文件: {}", path.display());
    }
    atomic_write(&p, content.as_bytes())?;
    Ok(())
}

pub fn create_file(project_root: &Path, path: &Path) -> Result<()> {
    let p = verify_under_project_root(project_root, path, false)?;
    if p.exists() {
        bail!("已存在: {}", path.display());
    }
    fs::write(&p, "")?;
    Ok(())
}

pub fn create_directory(project_root: &Path, path: &Path) -> Result<()> {
    let p = verify_under_project_root(project_root, path, false)?;
    if p.exists() {
        bail!("已存在: {}", path.display());
    }
    fs::create_dir(&p)?;
    Ok(())
}

/// 重命名(同目录内改名),返回新的绝对路径。
pub fn rename_entry(project_root: &Path, old_path: &Path, new_name: &str) -> Result<PathBuf> {
    let old_canon = verify_under_project_root(project_root, old_path, true)?;
    let parent = old_canon
        .parent()
        .ok_or_else(|| anyhow!("无法获取父目录"))?;
    let new_path = parent.join(new_name);
    // new_name 可能含 `../` 等,必须再校验一遍新路径仍在 project_root 内
    let new_canon = verify_under_project_root(project_root, &new_path, false)?;
    if new_canon.exists() {
        bail!("目标已存在: {}", new_canon.display());
    }
    fs::rename(&old_canon, &new_canon)?;
    Ok(new_canon)
}

pub fn delete_entry(project_root: &Path, path: &Path) -> Result<()> {
    let target = verify_under_project_root(project_root, path, true)?;
    // 多一道保险:绝不允许删除项目根目录本身
    // 必须同样剥掉 `\\?\`,否则与 verify_under_project_root 返回的 target 形式不一致
    let root = project_root.canonicalize().map(strip_verbatim_prefix)?;
    if target == root {
        bail!("不能删除项目根目录");
    }
    if target.is_dir() {
        fs::remove_dir_all(&target)?;
    } else {
        fs::remove_file(&target)?;
    }
    Ok(())
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
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "second-longer-content"
        );

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
        let canon = verify_under_project_root(&root, &file, true).unwrap();
        assert!(canon.starts_with(strip_verbatim_prefix(root.canonicalize().unwrap())));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn verify_rejects_dotdot_escape() {
        let (root, _) = make_test_project();
        // 构造一个理论上指向 root 之外的相对路径(../something)
        let escape = root.join("..").join("definitely-not-here.txt");
        let err = verify_under_project_root(&root, &escape, false)
            .unwrap_err()
            .to_string();
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

        let err = verify_under_project_root(&root, &other_file, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("不在项目根目录内"));

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&other).ok();
    }

    #[test]
    fn write_file_content_writes_inside_project() {
        let (root, file) = make_test_project();
        write_file_content(&root, &file, "新内容\r\n第二行").unwrap();
        // CRLF 原样落盘:行尾保真由编辑器负责,这一层不做任何归一
        assert_eq!(fs::read_to_string(&file).unwrap(), "新内容\r\n第二行");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn write_file_content_rejects_escape() {
        let (root, _) = make_test_project();
        let escape = root.join("..").join("evil-write.txt");
        let err = write_file_content(&root, &escape, "x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("不在项目根目录内") || err.contains("不可访问"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn write_file_content_rejects_directory() {
        let (root, _) = make_test_project();
        // 目标是目录时应报语义明确的错误,而不是走到 rename 覆盖目录
        let err = write_file_content(&root, &root, "x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("不是文件"));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn write_file_content_rejects_oversize() {
        let (root, file) = make_test_project();
        let before = fs::read(&file).unwrap();
        let err = write_file_content(&root, &file, &"a".repeat((MAX_FILE_VIEW_SIZE + 1) as usize))
            .unwrap_err()
            .to_string();
        assert!(err.contains("过大"));
        // 拒绝发生在写入之前,原文件必须一字未动
        assert_eq!(fs::read(&file).unwrap(), before);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rename_entry_inside_project_succeeds() {
        let (root, old_file) = make_test_project();
        let result = rename_entry(&root, &old_file, "renamed.txt");
        assert!(result.is_ok(), "rename 失败: {:?}", result);
        let new_path = root.join("renamed.txt");
        assert!(new_path.exists(), "新文件应存在: {}", new_path.display());
        assert!(!old_file.exists(), "旧文件应被移除");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rename_entry_dotdot_in_new_name_rejected() {
        let (root, old_file) = make_test_project();
        let result = rename_entry(&root, &old_file, "../escape.txt");
        assert!(result.is_err(), "应拒绝 ../ 逃逸");
        // 旧文件应未被改动
        assert!(old_file.exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn delete_entry_file_inside_project_succeeds() {
        let (root, file) = make_test_project();
        let result = delete_entry(&root, &file);
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
        let result = delete_entry(&root, &sub);
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

        let err = delete_entry(&root, &other_file).unwrap_err().to_string();
        assert!(err.contains("不在项目根目录内"));
        assert!(other_file.exists(), "项目外的文件不应被删除");

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&other).ok();
    }

    #[test]
    fn delete_entry_rejects_project_root_itself() {
        let (root, _) = make_test_project();
        let err = delete_entry(&root, &root).unwrap_err().to_string();
        assert!(err.contains("不能删除项目根目录"));
        assert!(root.exists(), "项目根目录不应被删除");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn verify_create_file_in_project() {
        let (root, _) = make_test_project();
        let new_file = root.join("brand-new.txt");
        let canon = verify_under_project_root(&root, &new_file, false).unwrap();
        assert!(canon.starts_with(strip_verbatim_prefix(root.canonicalize().unwrap())));
        assert!(!canon.exists()); // 文件还没创建
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_directory_hides_always_ignore_and_sorts() {
        let (root, _) = make_test_project();
        fs::create_dir(root.join("node_modules")).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("a2.txt"), "x").unwrap();
        fs::write(root.join("a10.txt"), "x").unwrap();

        let entries = list_directory(&root, &root).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        // 目录优先;a2 在 a10 之前(自然序);node_modules 完全不出现
        assert_eq!(names, vec!["src", "a2.txt", "a10.txt", "inside.txt"]);
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
        assert!(
            try_strip_windows_verbatim(r"\\?\Volume{12345678-1234-1234-1234-123456789012}\foo")
                .is_none()
        );
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
    /// 否则拿到的路径拖进 shell 不友好。
    #[cfg(windows)]
    #[test]
    fn verify_strips_verbatim_prefix_in_result() {
        let (root, file) = make_test_project();
        let canon = verify_under_project_root(&root, &file, true).unwrap();
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
        let plain = verify_under_project_root(&root, &file, true).unwrap();
        // 用 canonicalize 拿到的 verbatim 形式作为输入,verify 后应该剥成同样结果
        let verbatim_root = root.canonicalize().unwrap();
        let verbatim_file = file.canonicalize().unwrap();
        let from_verbatim =
            verify_under_project_root(&verbatim_root, &verbatim_file, true).unwrap();
        assert_eq!(plain, from_verbatim);
        fs::remove_dir_all(&root).ok();
    }
}
