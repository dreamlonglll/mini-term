//! 文件树的列举与增删改,以及所有「覆盖既有配置」都要用的原子写。
//!
//! 从 `src-tauri/src/fs.rs` 移入。去掉了 `#[tauri::command]`,路径参数由
//! `String` 换成 `&Path`,错误从 `Result<T, String>` 换成 `anyhow::Result<T>` ——
//! 面向用户的错误文案一字未改(前端曾直接把它们弹出来,GPUI 侧同样要能直接显示)。
//!
//! 目录监听(`fs-change`)不在本模块,见 [`crate::watch`]。

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

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

/// 由**一段文本**构建的 `.gitignore` 匹配器(远程文件树用)。
///
/// 本地树走 [`collect_gitignores`],逐级 `Gitignore::new(path)` 读真实文件;
/// SSH 远程项目的 `.gitignore` 是经 SFTP 读来的字节,**不落本地盘**,只能逐行
/// `add_line` 喂进 builder —— 于是需要这一层。
///
/// 落在 mt-project 而不是调用方(`mt_app::remote_ssh`):`ignore` crate 是本
/// crate 的既有依赖,`Gitignore` 也是本模块的内部类型,包一层就不用把它连同
/// 依赖一起暴露给壳层。**匹配一律用相对项目根的 POSIX 路径** —— Windows 的
/// `Path` 语义对 POSIX 绝对路径有歧义(`/a/b` 在 Windows 上不是绝对路径),
/// 相对路径两平台行为一致。
pub struct TextGitignore(Gitignore);

impl TextGitignore {
    /// 逐行喂 `add_line` 构建。单行非法(坏 glob)忽略该行、不影响其余行,
    /// 与 git 自身的行为一致;整体构建失败退化为空规则。
    pub fn from_text(content: &str) -> Self {
        let mut builder = ignore::gitignore::GitignoreBuilder::new("");
        for line in content.lines() {
            let _ = builder.add_line(None, line);
        }
        Self(builder.build().unwrap_or_else(|_| Gitignore::empty()))
    }

    /// `rel_path` 为相对项目根的 POSIX 路径(空串 = 项目根本身,永不忽略)。
    /// 白名单(`!pattern`)由 `Gitignore` 内部按 gitignore 语义处理(后规则覆盖前规则)。
    pub fn is_ignored(&self, rel_path: &str, is_dir: bool) -> bool {
        if rel_path.is_empty() {
            return false;
        }
        self.0.matched(rel_path, is_dir).is_ignore()
    }
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
#[cfg(any(windows, test))]
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
/// project_root 与 target 的父目录会 `canonicalize`,从而解析父级符号链接和
/// `..`;target 叶子本身**不 canonicalize**,避免删除/重命名时把符号链接替换成
/// 它指向的真实路径。`must_exist=true` 用 `symlink_metadata` 检查叶子存在,所以
/// 指向不存在目标的断链也属于一个可操作的既有条目。
///
/// 返回校验后的绝对路径(Windows 上已剥 `\\?\` 前缀),后续 IO 直接用它,
/// 避免重复访问磁盘。
///
/// `pub(crate)`:目录监听([`crate::watch::FsWatcher::watch`])要用同一把尺子
/// 校验待监听目录,不再单独造一份口径不同的实现。
pub(crate) fn verify_under_project_root(
    project_root: &Path,
    target: &Path,
    must_exist: bool,
) -> Result<PathBuf> {
    let root = project_root
        .canonicalize()
        .map(strip_verbatim_prefix)
        .map_err(|e| anyhow!("项目根目录无效: {}: {}", project_root.display(), e))?;

    // project_root 自己可能是 symlink。调用方把根本身(包括 `root/.`)作为
    // target 时必须返回 canonical root;普通叶子只 canonicalize 父目录。
    // 判断 root alias 时先看 lstat:项目内“指回根”的叶子 symlink
    // 仍应保留为链接。
    let target_is_root = target == project_root
        || fs::symlink_metadata(target)
            .ok()
            .filter(|meta| !meta.file_type().is_symlink())
            .and_then(|_| target.canonicalize().ok())
            .map(strip_verbatim_prefix)
            .is_some_and(|candidate| candidate == root);
    let canon = if target_is_root {
        root.clone()
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
    if must_exist {
        fs::symlink_metadata(&canon)
            .map_err(|e| anyhow!("路径不可访问: {}: {}", target.display(), e))?;
    }
    Ok(canon)
}

/// 内容读取/目录遍历需要跟随叶子 symlink 时使用:先以“不跟随叶子”的
/// 口径确认链接条目本身在项目内,再 canonicalize 目标并二次确认最终路径
/// 仍在项目根内。
fn verify_followed_under_project_root(project_root: &Path, target: &Path) -> Result<PathBuf> {
    let leaf = verify_under_project_root(project_root, target, true)?;
    let root = project_root
        .canonicalize()
        .map(strip_verbatim_prefix)
        .map_err(|e| anyhow!("项目根目录无效: {}: {}", project_root.display(), e))?;
    let followed = leaf
        .canonicalize()
        .map(strip_verbatim_prefix)
        .map_err(|e| anyhow!("路径不可访问: {}: {}", target.display(), e))?;
    if !followed.starts_with(&root) {
        bail!(
            "路径不在项目根目录内: {} (root={})",
            followed.display(),
            root.display()
        );
    }
    Ok(followed)
}

/// 过滤出有效的目录路径（用于拖拽添加项目时验证）
pub fn filter_directories(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.into_iter().filter(|p| p.is_dir()).collect()
}

/// 列举目录:隐藏 [`ALWAYS_IGNORE`],其余按 .gitignore 打 `ignored` 标记(不隐藏),
/// 排序为「目录优先 → 未忽略优先 → 名称自然序」。
pub fn list_directory(project_root: &Path, path: &Path) -> Result<Vec<FileEntry>> {
    let dir = verify_followed_under_project_root(project_root, path)?;
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
    let p = verify_followed_under_project_root(project_root, path)?;
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
    let p = verify_followed_under_project_root(project_root, path)?;
    if !p.is_file() {
        bail!("不是文件: {}", path.display());
    }
    atomic_write(&p, content.as_bytes())?;
    Ok(())
}

pub fn create_file(project_root: &Path, path: &Path) -> Result<()> {
    let p = verify_under_project_root(project_root, path, false)?;
    if path_entry_exists(&p)? {
        bail!("已存在: {}", path.display());
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&p)?;
    Ok(())
}

pub fn create_directory(project_root: &Path, path: &Path) -> Result<()> {
    let p = verify_under_project_root(project_root, path, false)?;
    if path_entry_exists(&p)? {
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
    if path_entry_exists(&new_canon)? {
        bail!("目标已存在: {}", new_canon.display());
    }
    fs::rename(&old_canon, &new_canon)?;
    Ok(new_canon)
}

/// 移动(换父目录、保留原名),返回新的绝对路径。
///
/// 文件树的拖拽移动与「移动到」菜单共用这一条。与 [`rename_entry`] 同一套
/// 校验:源与目标目录都必须在项目根内;另外多三道闸 ——
/// 不能移项目根、目录不能移进自身或其子孙(`fs::rename` 在这种情形下要么报
/// 错要么把整棵树绕成环,不同平台行为不一)、目标已存在时**不覆盖**(与
/// 重命名同一口径,用户改名再移)。
///
/// 源已经在目标目录里(`source.parent == target_dir`)按错误返回:UI 层应当在
/// 那之前就把这种落点判成无效,这里兜底。
pub fn move_entry(project_root: &Path, source: &Path, target_dir: &Path) -> Result<PathBuf> {
    let root = project_root
        .canonicalize()
        .map(strip_verbatim_prefix)
        .map_err(|e| anyhow!("项目根目录无效: {}: {}", project_root.display(), e))?;
    let source_canon = verify_under_project_root(project_root, source, true)?;
    if source_canon == root {
        bail!("不能移动项目根目录");
    }
    let target_dir_canon = verify_under_project_root(project_root, target_dir, true)?;
    if !target_dir_canon.is_dir() {
        bail!("目标不是目录: {}", target_dir.display());
    }
    let name = source_canon
        .file_name()
        .ok_or_else(|| anyhow!("缺少文件名: {}", source.display()))?;
    let destination = target_dir_canon.join(name);
    if destination == source_canon {
        bail!("已在该目录中: {}", source.display());
    }
    let source_is_dir = fs::symlink_metadata(&source_canon)
        .with_context(|| format!("读取源条目失败: {}", source_canon.display()))?
        .is_dir();
    if source_is_dir && target_dir_canon.starts_with(&source_canon) {
        bail!(
            "不能把目录移动到自身或其子目录: {} → {}",
            source.display(),
            target_dir.display()
        );
    }
    if path_entry_exists(&destination)? {
        bail!("目标已存在: {}", destination.display());
    }
    fs::rename(&source_canon, &destination)
        .with_context(|| format!("移动失败: {} → {}", source.display(), destination.display()))?;
    Ok(destination)
}

/// 本地复制遇到同名目标时的落盘策略。`Skip` 由批处理层在调用本函数前
/// 处理;这一层只负责两种真正会写盘的行为。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CopyConflictPolicy {
    KeepBoth,
    Overwrite,
}

/// 为文件或目录生成第一个尚未占用的 VS Code 风格副本路径。
///
/// 文件只把后缀插入最后一个扩展名前:`a.tar.gz` → `a.tar copy.gz`;
/// 目录名不解析扩展:`folder.v1` → `folder.v1 copy`。存在性检查使用
/// `symlink_metadata`,所以断链同样占用名称,不会被新文件意外跟随覆盖。
pub fn keep_both_path(path: &Path, is_dir: bool) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("无法获取父目录: {}", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("缺少文件名: {}", path.display()))?;

    for copy_number in 1u64.. {
        let candidate = parent.join(keep_both_name(name, is_dir, copy_number));
        if !path_entry_exists(&candidate)? {
            return Ok(candidate);
        }
    }
    unreachable!("u64 副本序号耗尽")
}

/// 在同一个本地项目内递归复制一个文件或目录,返回实际目标路径。
///
/// - `KeepBoth`:同名时生成 ` copy` / ` copy 2` 路径;
/// - `Overwrite`:文件使用 temp + backup-swap,目录与目录递归合并且保留
///   目标独有项;
/// - 叶子或树内 symlink、socket、FIFO、device 一律报 unsupported,绝不跟随。
///
/// 目录合并按条目提交,不是整棵树事务:中途 IO 失败时,此前成功的条目会
/// 保留,调用方应把错误作为“部分完成”展示,不能宣称整批回滚。
pub fn copy_entry(
    project_root: &Path,
    source: &Path,
    destination: &Path,
    policy: CopyConflictPolicy,
) -> Result<PathBuf> {
    let root = project_root
        .canonicalize()
        .map(strip_verbatim_prefix)
        .map_err(|e| anyhow!("项目根目录无效: {}: {}", project_root.display(), e))?;
    let source = verify_under_project_root(project_root, source, true)?;
    let source_meta = fs::symlink_metadata(&source)
        .with_context(|| format!("读取源条目失败: {}", source.display()))?;
    if source == root {
        bail!("不能复制项目根目录");
    }
    if source_meta.file_type().is_symlink() {
        bail!("不支持复制符号链接: {}", source.display());
    }
    if !source_meta.is_dir() && !source_meta.is_file() {
        bail!("不支持复制特殊文件: {}", source.display());
    }

    let requested_target = verify_under_project_root(project_root, destination, false)?;
    let requested_exists = path_entry_exists(&requested_target)?;
    ensure_copyable_tree(&source, &source_meta)?;

    if policy == CopyConflictPolicy::KeepBoth {
        let mut target = if requested_exists {
            keep_both_path(&requested_target, source_meta.is_dir())?
        } else {
            requested_target.clone()
        };
        loop {
            validate_copy_target(&root, &source, source_meta.is_dir(), &target)?;
            match copy_entry_to_new(&source, &source_meta, &target) {
                Ok(()) => return Ok(target),
                Err(e) if error_is_already_exists(&e) => {
                    // 列目录/选名之后的竞态由排他创建裁决;被抢占就重新生成
                    // 下一后缀,不能把并发者的条目静默覆盖掉。
                    target = keep_both_path(&requested_target, source_meta.is_dir())?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    validate_copy_target(&root, &source, source_meta.is_dir(), &requested_target)?;
    if path_entry_exists(&requested_target)? {
        overwrite_entry(&source, &source_meta, &requested_target)?;
    } else {
        match copy_entry_to_new(&source, &source_meta, &requested_target) {
            Ok(()) => {}
            Err(e) if error_is_already_exists(&e) => {
                // 预检后出现的新冲突仍遵循 Overwrite,而不是随机报
                // AlreadyExists。
                overwrite_entry(&source, &source_meta, &requested_target)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(requested_target)
}

fn validate_copy_target(
    root: &Path,
    source: &Path,
    source_is_dir: bool,
    target: &Path,
) -> Result<()> {
    if target == root {
        bail!("不能覆盖项目根目录");
    }
    if source == target {
        bail!("源路径与目标路径相同: {}", source.display());
    }
    if source_is_dir && (target.starts_with(source) || source.starts_with(target)) {
        bail!(
            "源目录与目标目录不能互相包含: {} → {}",
            source.display(),
            target.display()
        );
    }
    Ok(())
}

fn error_is_already_exists(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_error| io_error.kind() == std::io::ErrorKind::AlreadyExists)
    })
}

fn keep_both_name(name: &OsStr, is_dir: bool, copy_number: u64) -> OsString {
    let suffix = if copy_number == 1 {
        " copy".to_string()
    } else {
        format!(" copy {copy_number}")
    };

    if is_dir {
        let mut result = OsString::from(name);
        result.push(suffix);
        return result;
    }

    let name_path = Path::new(name);
    let stem = name_path.file_stem().unwrap_or(name);
    let mut result = OsString::from(stem);
    result.push(suffix);
    if let Some(extension) = name_path.extension() {
        result.push(".");
        result.push(extension);
    }
    result
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("检查路径失败: {}", path.display())),
    }
}

fn ensure_copyable_tree(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    let current = fs::symlink_metadata(path)
        .with_context(|| format!("读取源条目失败: {}", path.display()))?;
    ensure_entry_unchanged(path, metadata, &current)?;
    let file_type = current.file_type();
    if file_type.is_symlink() {
        bail!("不支持复制符号链接: {}", path.display());
    }
    if file_type.is_file() {
        return Ok(());
    }
    if !file_type.is_dir() {
        bail!("不支持复制特殊文件: {}", path.display());
    }

    let entries =
        fs::read_dir(path).with_context(|| format!("读取目录失败: {}", path.display()))?;
    let after_open = fs::symlink_metadata(path)
        .with_context(|| format!("重新读取源目录失败: {}", path.display()))?;
    ensure_entry_unchanged(path, &current, &after_open)?;
    for entry in entries {
        let entry = entry.with_context(|| format!("读取目录项失败: {}", path.display()))?;
        let current_parent = fs::symlink_metadata(path)
            .with_context(|| format!("重新读取源目录失败: {}", path.display()))?;
        ensure_entry_unchanged(path, &after_open, &current_parent)?;
        let child = entry.path();
        let child_meta = fs::symlink_metadata(&child)
            .with_context(|| format!("读取源条目失败: {}", child.display()))?;
        ensure_copyable_tree(&child, &child_meta)?;
    }
    Ok(())
}

fn copy_entry_to_new(source: &Path, source_meta: &fs::Metadata, target: &Path) -> Result<()> {
    if source_meta.file_type().is_symlink() {
        bail!("不支持复制符号链接: {}", source.display());
    }
    if !source_meta.is_dir() && !source_meta.is_file() {
        bail!("不支持复制特殊文件: {}", source.display());
    }
    // 新建/KeepBoth 必须以排他创建为最终仲裁。标准库没有跨平台的
    // rename-no-replace;先检查再 rename 在 Unix 会覆盖竞态创建的目标。
    // 因此直接以 create_new/create_dir 创建最终路径,失败时由下层清理半成品。
    if source_meta.is_dir() {
        copy_directory_to_new(source, source_meta, target)
    } else {
        copy_regular_file_to_new(source, source_meta, target)
    }
}

fn copy_regular_file_to_new(
    source: &Path,
    source_meta: &fs::Metadata,
    target: &Path,
) -> Result<()> {
    if !source_meta.is_file() || source_meta.file_type().is_symlink() {
        bail!("源条目不是普通文件: {}", source.display());
    }
    let before_open = fs::symlink_metadata(source)
        .with_context(|| format!("重新读取源文件失败: {}", source.display()))?;
    ensure_entry_unchanged(source, source_meta, &before_open)?;
    let mut input =
        fs::File::open(source).with_context(|| format!("打开源文件失败: {}", source.display()))?;
    let opened_meta = input
        .metadata()
        .with_context(|| format!("读取已打开源文件失败: {}", source.display()))?;
    ensure_entry_unchanged(source, &before_open, &opened_meta)?;
    let after_open = fs::symlink_metadata(source)
        .with_context(|| format!("重新读取源文件失败: {}", source.display()))?;
    ensure_entry_unchanged(source, &opened_meta, &after_open)?;
    // `create_new` 成功后才进入清理分支;若并发者抢先占用 target,绝不能把
    // 对方创建的文件当成自己的半成品删掉。
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .with_context(|| format!("创建目标文件失败: {}", target.display()))?;
    let result = (|| -> Result<()> {
        std::io::copy(&mut input, &mut output)
            .with_context(|| format!("复制文件失败: {}", source.display()))?;
        output
            .flush()
            .with_context(|| format!("刷新目标文件失败: {}", target.display()))?;
        let _ = output.sync_all();
        fs::set_permissions(target, source_meta.permissions())
            .with_context(|| format!("复制文件权限失败: {}", target.display()))?;
        Ok(())
    })();
    drop(output);
    if result.is_err() {
        let _ = fs::remove_file(target);
    }
    result
}

fn copy_directory_to_new(source: &Path, source_meta: &fs::Metadata, target: &Path) -> Result<()> {
    if !source_meta.is_dir() || source_meta.file_type().is_symlink() {
        bail!("源条目不是普通目录: {}", source.display());
    }
    fs::create_dir(target).with_context(|| format!("创建目标目录失败: {}", target.display()))?;
    let result = (|| -> Result<()> {
        let before_open = fs::symlink_metadata(source)
            .with_context(|| format!("重新读取源目录失败: {}", source.display()))?;
        ensure_entry_unchanged(source, source_meta, &before_open)?;
        let entries =
            fs::read_dir(source).with_context(|| format!("读取目录失败: {}", source.display()))?;
        let after_open = fs::symlink_metadata(source)
            .with_context(|| format!("重新读取源目录失败: {}", source.display()))?;
        ensure_entry_unchanged(source, &before_open, &after_open)?;
        for entry in entries {
            let entry = entry.with_context(|| format!("读取目录项失败: {}", source.display()))?;
            let current_parent = fs::symlink_metadata(source)
                .with_context(|| format!("重新读取源目录失败: {}", source.display()))?;
            ensure_entry_unchanged(source, &after_open, &current_parent)?;
            let child_source = entry.path();
            let child_target = target.join(entry.file_name());
            let child_meta = fs::symlink_metadata(&child_source)
                .with_context(|| format!("读取源条目失败: {}", child_source.display()))?;
            if child_meta.is_dir() {
                copy_directory_to_new(&child_source, &child_meta, &child_target)?;
            } else if child_meta.is_file() {
                copy_regular_file_to_new(&child_source, &child_meta, &child_target)?;
            } else if child_meta.file_type().is_symlink() {
                bail!("不支持复制符号链接: {}", child_source.display());
            } else {
                bail!("不支持复制特殊文件: {}", child_source.display());
            }
        }
        fs::set_permissions(target, source_meta.permissions())
            .with_context(|| format!("复制目录权限失败: {}", target.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(target);
    }
    result
}

/// 防止预检与实际打开之间的普通路径被替换成 symlink/另一条目。Unix 额外
/// 比较 `(dev, ino)`;其他平台至少比较不跟随链接得到的条目类型。它不能
/// 替代 OS 级 openat/no-follow capability,但能关闭普通并发替换窗口并保证
/// 检测到的 symlink 永远不会进入复制读取。
fn ensure_entry_unchanged(
    path: &Path,
    expected: &fs::Metadata,
    actual: &fs::Metadata,
) -> Result<()> {
    let expected_type = expected.file_type();
    let actual_type = actual.file_type();
    if expected_type.is_symlink()
        || actual_type.is_symlink()
        || expected_type.is_dir() != actual_type.is_dir()
        || expected_type.is_file() != actual_type.is_file()
    {
        bail!("文件操作期间条目发生变化或变为符号链接: {}", path.display());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
            bail!("文件操作期间条目被替换: {}", path.display());
        }
    }
    Ok(())
}

fn overwrite_entry(source: &Path, source_meta: &fs::Metadata, target: &Path) -> Result<()> {
    let target_meta = fs::symlink_metadata(target)
        .with_context(|| format!("读取目标条目失败: {}", target.display()))?;
    if source_meta.is_dir() && target_meta.is_dir() && !target_meta.file_type().is_symlink() {
        return merge_directories(source, source_meta, target, &target_meta);
    }

    let (staging_container, staging) = stage_entry(source, source_meta, target)?;
    replace_with_backup(&staging, &staging_container, target)
}

fn stage_entry(
    source: &Path,
    source_meta: &fs::Metadata,
    target: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let container = create_unique_operation_dir(target, "copy")?;
    let staging = container.join("entry");
    let result = if source_meta.is_dir() {
        copy_directory_to_new(source, source_meta, &staging)
    } else {
        copy_regular_file_to_new(source, source_meta, &staging)
    };
    if let Err(e) = result {
        let _ = remove_path_no_follow(&container);
        return Err(e);
    }
    Ok((container, staging))
}

fn merge_directories(
    source: &Path,
    expected_source: &fs::Metadata,
    target: &Path,
    expected_target: &fs::Metadata,
) -> Result<()> {
    let source_before = fs::symlink_metadata(source)
        .with_context(|| format!("重新读取源目录失败: {}", source.display()))?;
    ensure_entry_unchanged(source, expected_source, &source_before)?;
    let target_before = fs::symlink_metadata(target)
        .with_context(|| format!("重新读取目标目录失败: {}", target.display()))?;
    ensure_entry_unchanged(target, expected_target, &target_before)?;
    let entries =
        fs::read_dir(source).with_context(|| format!("读取目录失败: {}", source.display()))?;
    let source_after = fs::symlink_metadata(source)
        .with_context(|| format!("重新读取源目录失败: {}", source.display()))?;
    ensure_entry_unchanged(source, &source_before, &source_after)?;

    for entry in entries {
        let entry = entry.with_context(|| format!("读取目录项失败: {}", source.display()))?;
        let current_source = fs::symlink_metadata(source)
            .with_context(|| format!("重新读取源目录失败: {}", source.display()))?;
        ensure_entry_unchanged(source, &source_after, &current_source)?;
        let current_target = fs::symlink_metadata(target)
            .with_context(|| format!("重新读取目标目录失败: {}", target.display()))?;
        ensure_entry_unchanged(target, &target_before, &current_target)?;
        let child_source = entry.path();
        let child_target = target.join(entry.file_name());
        let source_meta = fs::symlink_metadata(&child_source)
            .with_context(|| format!("读取源条目失败: {}", child_source.display()))?;

        if !path_entry_exists(&child_target)? {
            copy_entry_to_new(&child_source, &source_meta, &child_target)?;
            continue;
        }

        let target_meta = fs::symlink_metadata(&child_target)
            .with_context(|| format!("读取目标条目失败: {}", child_target.display()))?;
        if source_meta.is_dir() && target_meta.is_dir() && !target_meta.file_type().is_symlink() {
            merge_directories(&child_source, &source_meta, &child_target, &target_meta)?;
        } else {
            overwrite_entry(&child_source, &source_meta, &child_target)?;
        }
    }
    Ok(())
}

fn unique_operation_path(target: &Path, kind: &str) -> Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("无法获取父目录: {}", target.display()))?;
    let name = target
        .file_name()
        .ok_or_else(|| anyhow!("缺少文件名: {}", target.display()))?;
    loop {
        let seq = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let mut candidate_name = OsString::from(".");
        candidate_name.push(name);
        candidate_name.push(format!(".mt-{kind}-{}-{seq}", std::process::id()));
        let candidate = parent.join(candidate_name);
        if !path_entry_exists(&candidate)? {
            return Ok(candidate);
        }
    }
}

fn replace_with_backup(staging: &Path, staging_container: &Path, target: &Path) -> Result<()> {
    let backup_container = match create_unique_operation_dir(target, "backup") {
        Ok(path) => path,
        Err(e) => {
            let _ = remove_path_no_follow(staging_container);
            return Err(e);
        }
    };
    // container 由 create_dir 排他创建,其内部名称只归本次操作所有,避免
    // `rename(target, backup)` 覆盖并发者抢占的同级 backup 路径。
    let backup = backup_container.join("entry");
    if let Err(e) = fs::rename(target, &backup) {
        let _ = remove_path_no_follow(staging_container);
        let _ = fs::remove_dir(&backup_container);
        return Err(e).with_context(|| format!("备份既有目标失败: {}", target.display()));
    }

    if let Err(promote_error) = fs::rename(staging, target) {
        let rollback_result = fs::rename(&backup, target);
        let _ = remove_path_no_follow(staging_container);
        if let Err(rollback_error) = rollback_result {
            bail!(
                "提交覆盖结果失败且恢复备份失败: {}: {}; rollback: {}; backup: {}",
                target.display(),
                promote_error,
                rollback_error,
                backup.display()
            );
        }
        let _ = fs::remove_dir(&backup_container);
        return Err(promote_error)
            .with_context(|| format!("提交覆盖结果失败: {}", target.display()));
    }

    let staging_cleanup = fs::remove_dir(staging_container).err();
    remove_path_no_follow(&backup)
        .with_context(|| format!("覆盖成功但清理备份失败: {}", backup.display()))?;
    fs::remove_dir(&backup_container)
        .with_context(|| format!("清理备份目录失败: {}", backup_container.display()))?;
    if let Some(e) = staging_cleanup {
        return Err(e)
            .with_context(|| format!("清理暂存目录失败: {}", staging_container.display()));
    }
    Ok(())
}

fn create_unique_operation_dir(target: &Path, kind: &str) -> Result<PathBuf> {
    loop {
        let candidate = unique_operation_path(target, kind)?;
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("创建操作目录失败: {}", candidate.display()));
            }
        }
    }
}

fn remove_path_no_follow(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e).with_context(|| format!("读取待删除条目失败: {}", path.display()));
        }
    };

    if metadata.file_type().is_symlink() {
        // Unix 的目录 symlink 用 remove_file;Windows 目录 symlink 通常要求
        // remove_dir。先走不跟随的 remove_file,失败后仅对 symlink 回退 remove_dir。
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(file_error) => fs::remove_dir(path).map_err(|dir_error| {
                anyhow!(
                    "删除符号链接失败: {}: {}; remove_dir: {}",
                    path.display(),
                    file_error,
                    dir_error
                )
            }),
        };
    }
    if metadata.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn delete_entry(project_root: &Path, path: &Path) -> Result<()> {
    let target = verify_under_project_root(project_root, path, true)?;
    // 多一道保险:绝不允许删除项目根目录本身
    // 必须同样剥掉 `\\?\`,否则与 verify_under_project_root 返回的
    // target 形式不一致
    let root = project_root.canonicalize().map(strip_verbatim_prefix)?;
    if target == root {
        bail!("不能删除项目根目录");
    }
    remove_path_no_follow(&target)
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

    // --- TextGitignore(远程文件树的根 .gitignore 匹配)---
    // 三条断言自 `src-tauri/src/remote_ssh.rs` 的同名测试原样搬来:
    // 那边的 `build_remote_gitignore` + `is_remote_entry_ignored` 就是这一层。

    #[test]
    fn text_gitignore_matches_relative_paths() {
        let gi = TextGitignore::from_text("node_modules/\n*.log\nbuild/\n");
        assert!(gi.is_ignored("node_modules", true));
        assert!(gi.is_ignored("app.log", false));
        assert!(gi.is_ignored("src/deep/trace.log", false));
        assert!(gi.is_ignored("src/build", true));
        assert!(!gi.is_ignored("src/main.rs", false));
        // 目录规则(尾 `/`)不忽略同名文件
        assert!(!gi.is_ignored("build", false));
    }

    #[test]
    fn text_gitignore_supports_whitelist_override() {
        let gi = TextGitignore::from_text("*.log\n!keep.log\n");
        assert!(gi.is_ignored("a.log", false));
        assert!(!gi.is_ignored("keep.log", false));
    }

    #[test]
    fn text_gitignore_empty_and_invalid_lines_are_safe() {
        let gi = TextGitignore::from_text("");
        assert!(!gi.is_ignored("anything", false));
        // 空相对路径(项目根本身)永不忽略
        let gi2 = TextGitignore::from_text("*\n");
        assert!(!gi2.is_ignored("", true));
        // 坏 glob 只废掉那一行,其余规则照常生效
        let gi3 = TextGitignore::from_text("[unclosed\n*.tmp\n");
        assert!(gi3.is_ignored("a.tmp", false));
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
    fn move_entry_file_into_subdirectory() {
        let (root, file) = make_test_project();
        let target = root.join("sub");
        fs::create_dir(&target).unwrap();
        let moved = move_entry(&root, &file, &target).expect("move 失败");
        assert_eq!(
            moved,
            strip_verbatim_prefix(target.canonicalize().unwrap()).join("inside.txt")
        );
        assert!(moved.exists());
        assert!(!file.exists(), "源文件应被移走");
        assert_eq!(fs::read(&moved).unwrap(), b"hi");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn move_entry_directory_keeps_children() {
        let (root, _) = make_test_project();
        let dir = root.join("pkg");
        fs::create_dir_all(dir.join("deep")).unwrap();
        fs::write(dir.join("deep").join("a.txt"), "a").unwrap();
        let target = root.join("lib");
        fs::create_dir(&target).unwrap();
        let moved = move_entry(&root, &dir, &target).expect("move 失败");
        assert!(moved.join("deep").join("a.txt").exists());
        assert!(!dir.exists());
        fs::remove_dir_all(&root).ok();
    }

    /// 目录不能移进自身或其子孙 —— `fs::rename` 在这种情形下各平台行为不一,
    /// 必须在调用前拒绝。
    #[test]
    fn move_entry_rejects_moving_directory_into_itself() {
        let (root, _) = make_test_project();
        let dir = root.join("pkg");
        fs::create_dir_all(dir.join("deep")).unwrap();
        assert!(move_entry(&root, &dir, &dir).is_err(), "移进自身");
        assert!(
            move_entry(&root, &dir, &dir.join("deep")).is_err(),
            "移进子孙"
        );
        assert!(dir.join("deep").exists(), "拒绝后原树一字未动");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn move_entry_rejects_same_parent_existing_target_and_root() {
        let (root, file) = make_test_project();
        // 已在目标目录里
        assert!(move_entry(&root, &file, &root).is_err());
        // 目标已存在同名条目:不覆盖
        let target = root.join("sub");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("inside.txt"), "taken").unwrap();
        assert!(move_entry(&root, &file, &target).is_err());
        assert_eq!(fs::read(target.join("inside.txt")).unwrap(), b"taken");
        assert!(file.exists());
        // 项目根本身不能动
        assert!(move_entry(&root, &root, &target).is_err());
        // 目标不是目录
        assert!(move_entry(&root, &file, &target.join("inside.txt")).is_err());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn move_entry_rejects_target_outside_project() {
        let (root, file) = make_test_project();
        let outside = std::env::temp_dir();
        assert!(
            move_entry(&root, &file, &outside).is_err(),
            "目标在项目根之外"
        );
        assert!(file.exists());
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

    /// 大目录删除仍委托给平台递归原语；FileTree 会在提交后台任务前解除 watcher。
    /// 这里用 5,000 个文件守住递归完整性，实际 UI 非阻塞由调用链结构保证。
    #[test]
    fn delete_entry_handles_directory_with_many_files() {
        let (root, _) = make_test_project();
        let large = root.join("large");
        for directory_index in 0..50 {
            let directory = large.join(format!("d{directory_index}"));
            fs::create_dir_all(&directory).unwrap();
            for file_index in 0..100 {
                fs::write(directory.join(format!("f{file_index}.txt")), []).unwrap();
            }
        }

        delete_entry(&root, &large).unwrap();

        assert!(fs::symlink_metadata(&large).is_err());
        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn delete_entry_removes_leaf_symlinks_without_touching_targets() {
        use std::os::unix::fs::symlink;

        let (root, inside_file) = make_test_project();
        let inside_dir = root.join("inside-dir");
        fs::create_dir(&inside_dir).unwrap();
        fs::write(inside_dir.join("keep.txt"), "inside").unwrap();

        let outside = std::env::temp_dir().join(format!(
            "mini-term-fs-symlink-targets-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(outside.join("dir")).unwrap();
        let outside_file = outside.join("file.txt");
        fs::write(&outside_file, "outside").unwrap();
        fs::write(outside.join("dir").join("keep.txt"), "outside-dir").unwrap();

        let links = [
            (root.join("inside-file-link"), inside_file.clone()),
            (root.join("inside-dir-link"), inside_dir.clone()),
            (root.join("outside-file-link"), outside_file.clone()),
            (root.join("outside-dir-link"), outside.join("dir")),
        ];
        for (link, target) in &links {
            symlink(target, link).unwrap();
            let verified = verify_under_project_root(&root, link, true).unwrap();
            assert_eq!(
                verified.as_path(),
                link.as_path(),
                "校验不得把叶子链接替换成目标"
            );
            delete_entry(&root, link).unwrap();
            assert!(
                fs::symlink_metadata(link).is_err(),
                "链接自身应被删除: {}",
                link.display()
            );
            assert!(target.exists(), "链接目标必须保留: {}", target.display());
        }
        assert_eq!(fs::read_to_string(&inside_file).unwrap(), "hi");
        assert_eq!(
            fs::read_to_string(inside_dir.join("keep.txt")).unwrap(),
            "inside"
        );
        assert_eq!(fs::read_to_string(&outside_file).unwrap(), "outside");
        assert_eq!(
            fs::read_to_string(outside.join("dir").join("keep.txt")).unwrap(),
            "outside-dir"
        );

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&outside).ok();
    }

    #[cfg(unix)]
    #[test]
    fn copy_entry_rejects_leaf_and_nested_symlinks_without_following() {
        use std::os::unix::fs::symlink;

        let (root, inside_file) = make_test_project();
        let leaf_link = root.join("leaf-link");
        symlink(&inside_file, &leaf_link).unwrap();
        let leaf_target = root.join("leaf-copy");
        let err = copy_entry(
            &root,
            &leaf_link,
            &leaf_target,
            CopyConflictPolicy::KeepBoth,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("符号链接"));
        assert!(!leaf_target.exists());
        assert_eq!(fs::read_to_string(&inside_file).unwrap(), "hi");

        let source_dir = root.join("source-with-link");
        fs::create_dir(&source_dir).unwrap();
        fs::write(source_dir.join("regular.txt"), "regular").unwrap();
        symlink(&inside_file, source_dir.join("nested-link")).unwrap();
        let directory_target = root.join("directory-copy");
        let err = copy_entry(
            &root,
            &source_dir,
            &directory_target,
            CopyConflictPolicy::KeepBoth,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("符号链接"));
        assert!(
            fs::symlink_metadata(&directory_target).is_err(),
            "预检失败时不应创建部分目标"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn content_read_rejects_leaf_symlink_that_escapes_project() {
        use std::os::unix::fs::symlink;

        let (root, _) = make_test_project();
        let outside = std::env::temp_dir().join(format!(
            "mini-term-fs-read-link-target-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&outside, "secret").unwrap();
        let link = root.join("outside-link");
        symlink(&outside, &link).unwrap();

        let err = read_file_content(&root, &link).unwrap_err().to_string();
        assert!(err.contains("不在项目根目录内"));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "secret");

        fs::remove_dir_all(&root).ok();
        fs::remove_file(&outside).ok();
    }

    #[cfg(unix)]
    #[test]
    fn broken_symlink_counts_as_existing_and_can_be_deleted() {
        use std::os::unix::fs::symlink;

        let (root, _) = make_test_project();
        let missing_target = root.join("missing-target");
        let link = root.join("broken-link");
        symlink(&missing_target, &link).unwrap();

        let err = create_file(&root, &link).unwrap_err().to_string();
        assert!(err.contains("已存在"));
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!missing_target.exists());

        delete_entry(&root, &link).unwrap();
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(!missing_target.exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn keep_both_path_uses_copy_suffix_and_last_extension() {
        let (root, _) = make_test_project();
        let report = root.join("report.txt");
        fs::write(&report, "one").unwrap();
        fs::write(root.join("report copy.txt"), "two").unwrap();
        assert_eq!(
            keep_both_path(&report, false).unwrap(),
            root.join("report copy 2.txt")
        );

        let archive = root.join("archive.tar.gz");
        fs::write(&archive, "archive").unwrap();
        assert_eq!(
            keep_both_path(&archive, false).unwrap(),
            root.join("archive.tar copy.gz")
        );

        let dot_file = root.join(".env");
        fs::write(&dot_file, "env").unwrap();
        assert_eq!(
            keep_both_path(&dot_file, false).unwrap(),
            root.join(".env copy")
        );

        let dotted_dir = root.join("folder.v1");
        fs::create_dir(&dotted_dir).unwrap();
        assert_eq!(
            keep_both_path(&dotted_dir, true).unwrap(),
            root.join("folder.v1 copy")
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn copy_entry_keep_both_recursively_copies_directory() {
        let (root, _) = make_test_project();
        let source = root.join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("top.txt"), "top").unwrap();
        fs::write(source.join("nested").join("deep.txt"), "deep").unwrap();

        let actual = copy_entry(&root, &source, &source, CopyConflictPolicy::KeepBoth).unwrap();
        assert_eq!(actual, root.join("source copy"));
        assert_eq!(fs::read_to_string(actual.join("top.txt")).unwrap(), "top");
        assert_eq!(
            fs::read_to_string(actual.join("nested").join("deep.txt")).unwrap(),
            "deep"
        );
        assert!(operation_artifacts(&root).is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn copy_entry_overwrites_file_via_backup_swap() {
        let (root, _) = make_test_project();
        let source = root.join("source.txt");
        let target = root.join("target.txt");
        fs::write(&source, "new-content").unwrap();
        fs::write(&target, "old-content").unwrap();

        let actual = copy_entry(&root, &source, &target, CopyConflictPolicy::Overwrite).unwrap();
        assert_eq!(actual, target);
        assert_eq!(fs::read_to_string(&source).unwrap(), "new-content");
        assert_eq!(fs::read_to_string(&target).unwrap(), "new-content");
        assert!(operation_artifacts(&root).is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn backup_swap_restores_original_when_promote_fails() {
        let (root, _) = make_test_project();
        let target = root.join("target.txt");
        let staging_container = create_unique_operation_dir(&target, "copy").unwrap();
        let missing_staging = staging_container.join("missing-entry");
        fs::write(&target, "original").unwrap();

        let err = replace_with_backup(&missing_staging, &staging_container, &target)
            .unwrap_err()
            .to_string();
        assert!(err.contains("提交覆盖结果失败"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "original");
        assert!(operation_artifacts(&root).is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn copy_entry_overwrite_merges_directories_and_preserves_target_only_items() {
        let (root, _) = make_test_project();
        let source = root.join("source");
        let target = root.join("target");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::create_dir_all(target.join("nested")).unwrap();
        fs::write(source.join("common.txt"), "source-common").unwrap();
        fs::write(source.join("source-only.txt"), "source-only").unwrap();
        fs::write(source.join("nested").join("common.txt"), "source-nested").unwrap();
        fs::write(target.join("common.txt"), "target-common").unwrap();
        fs::write(target.join("target-only.txt"), "target-only").unwrap();
        fs::write(target.join("nested").join("common.txt"), "target-nested").unwrap();
        fs::write(target.join("nested").join("target-only.txt"), "keep").unwrap();

        copy_entry(&root, &source, &target, CopyConflictPolicy::Overwrite).unwrap();
        assert_eq!(
            fs::read_to_string(target.join("common.txt")).unwrap(),
            "source-common"
        );
        assert_eq!(
            fs::read_to_string(target.join("source-only.txt")).unwrap(),
            "source-only"
        );
        assert_eq!(
            fs::read_to_string(target.join("target-only.txt")).unwrap(),
            "target-only"
        );
        assert_eq!(
            fs::read_to_string(target.join("nested").join("common.txt")).unwrap(),
            "source-nested"
        );
        assert_eq!(
            fs::read_to_string(target.join("nested").join("target-only.txt")).unwrap(),
            "keep"
        );
        assert!(operation_artifacts(&root).is_empty());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn copy_entry_overwrite_replaces_conflicting_entry_type() {
        let (root, _) = make_test_project();
        let source = root.join("source.txt");
        let target = root.join("target");
        fs::write(&source, "replacement").unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(target.join("old.txt"), "old").unwrap();

        copy_entry(&root, &source, &target, CopyConflictPolicy::Overwrite).unwrap();
        assert!(target.is_file());
        assert_eq!(fs::read_to_string(&target).unwrap(), "replacement");
        assert!(operation_artifacts(&root).is_empty());
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
    fn delete_entry_rejects_project_root_dot_alias() {
        let (root, _) = make_test_project();
        let err = delete_entry(&root, &root.join("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("不能删除项目根目录"));
        assert!(root.exists());
        fs::remove_dir_all(&root).ok();
    }

    fn operation_artifacts(dir: &Path) -> Vec<PathBuf> {
        fn collect(dir: &Path, result: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).unwrap().filter_map(|entry| entry.ok()) {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.contains(".mt-copy-") || name.contains(".mt-backup-") {
                    result.push(path);
                    continue;
                }
                if fs::symlink_metadata(&path)
                    .ok()
                    .is_some_and(|metadata| metadata.is_dir())
                {
                    collect(&path, result);
                }
            }
        }

        let mut result = Vec::new();
        collect(dir, &mut result);
        result
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
