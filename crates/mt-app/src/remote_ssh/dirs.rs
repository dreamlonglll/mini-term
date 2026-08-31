use std::path::PathBuf;
use std::sync::Arc;

use mt_ai::sessions::normalize_unix_path;
use mt_config::SshConnection;
use mt_project::fs::{ALWAYS_IGNORE, FileEntry, TextGitignore, natural_cmp};

use super::{
    DEFAULT_REMOTE_PASTE_DIR, GITIGNORE_MAX_BYTES, expand_tilde, join_posix, lock, open_sftp,
    posix_relative, remote_home, split_posix_leaf, state, valid_remote_name, valid_sftp_child_name,
    validate_remote_dir_under_root, validate_remote_leaf_under_root,
};

#[derive(Debug, Clone)]
pub struct RemoteDirectoryEntry {
    pub name: String,
    pub path: String,
    pub is_symlink: bool,
}

#[derive(Debug, Clone)]
pub struct RemoteDirectoryListing {
    pub canonical_path: String,
    pub directories: Vec<RemoteDirectoryEntry>,
}

/// 把配置里的「远程粘贴落盘目录」解析成远端绝对路径。
///
/// 三种写法(对齐 `AppConfig::remote_paste_dir` 的文档):
/// - 相对路径 `.mini-term/pasted` → 相对**项目根**展开(默认形态,图片落项目内)
/// - `~` / `~/xxx` → 远程 home 展开
/// - 绝对路径 `/tmp/mini-term` → 原样
///
/// **保证返回的路径不含 `..` 段**。这条路径最终会拼进 SFTP **写**操作 ——
/// 逃出项目根 / home 的写入不是这个功能该有的能力,宁可报错。
/// 判定放在归一之后,`project_path`(调用方传入)带 `..` 的情形一并挡掉,
/// 而不只是校验用户填的 `dest_dir`。
pub(super) fn resolve_paste_dir(
    project_path: &str,
    home: &str,
    dest_dir: &str,
) -> Result<String, String> {
    // 用户可能顺手填了反斜杠,统一成 POSIX 分隔符再判定。
    let raw = dest_dir.trim().replace('\\', "/");
    let raw = if raw.trim().is_empty() {
        DEFAULT_REMOTE_PASTE_DIR.to_string()
    } else {
        raw
    };

    let abs = if raw.starts_with('/') {
        raw.clone()
    } else if raw == "~" || raw.starts_with("~/") {
        expand_tilde(&raw, home)
    } else {
        // 相对项目根。项目根必须是绝对路径(添加远程项目时已 canonicalize)。
        if !project_path.starts_with('/') {
            return Err(format!("远程项目路径不是绝对路径: {project_path}"));
        }
        join_posix(project_path, raw.trim_start_matches('/'))
    };

    // 归一:丢掉空段与 `.` 段。`./x` 和 `x` 必须解析成同一条路径,否则
    // `/proj/.` 这种写法会绕过下游「目录是否严格位于项目内」的判定。
    // 注意 `.` / `..` 都是**整段**比较,`.mini-term` 这类点开头的目录名不受影响。
    let normalized: Vec<&str> = abs
        .split('/')
        .filter(|seg| !seg.is_empty() && *seg != ".")
        .collect();
    if normalized.is_empty() {
        return Err("远程粘贴目录解析为空".into());
    }
    // 归一后再查 `..`:此时 dest_dir 与 project_path 两部分都已合入 abs,
    // 一处判定覆盖两个来源。
    if normalized.contains(&"..") {
        return Err("远程粘贴目录不能包含 `..`".into());
    }
    Ok(format!("/{}", normalized.join("/")))
}

/// 从本地临时文件路径提取文件名。两种分隔符都切 —— 传进来的是 Windows 路径,
/// 不能让 `\` 残留在远端路径里。
pub(super) fn paste_file_name(local_path: &str) -> Result<String, String> {
    let name = local_path.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("无法从本地路径提取文件名: {local_path}"));
    }
    Ok(name.to_string())
}

// ---------------------------------------------------------------------------
// 入口 1:远程文件树
// ---------------------------------------------------------------------------

/// SFTP readdir 远程目录,返回与本地 `mt_project::fs::list_directory` 同构的
/// [`FileEntry`] 列表。
///
/// 忽略过滤 = 项目根 `.gitignore`(读一次、按 connId+projectRoot 缓存)
/// + [`ALWAYS_IGNORE`] 固定黑名单(目录直接隐藏)。
///
/// `refresh_ignore=true` 强制重读 .gitignore(树顶手动刷新按钮用)。
///
/// **阻塞**,丢 `background_executor`。
pub fn list_directory(
    conn: &SshConnection,
    path: &str,
    project_root: &str,
    refresh_ignore: bool,
) -> Result<Vec<FileEntry>, String> {
    let st = state();
    let ignore_key = format!("{}|{}", conn.id, normalize_unix_path(project_root));
    if refresh_ignore {
        lock(&st.gitignore_cache).remove(&ignore_key);
    }
    // 锁即取即放;miss 时在 SFTP 打开后无锁读取,再短暂加锁写回。
    let cached_ignore = lock(&st.gitignore_cache).get(&ignore_key).cloned();

    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let gitignore = match cached_ignore {
                Some(g) => g,
                None => {
                    let gi_path = join_posix(project_root, ".gitignore");
                    // .gitignore 不存在 / 读失败 → 空规则,静默降级。
                    let content = match sftp.read_head(&gi_path, GITIGNORE_MAX_BYTES).await {
                        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                        Err(_) => String::new(),
                    };
                    let g = Arc::new(TextGitignore::from_text(&content));
                    lock(&st.gitignore_cache).insert(ignore_key.clone(), g.clone());
                    g
                }
            };

            let entries = sftp
                .read_dir(path)
                .await
                .map_err(|e| format!("读取远程目录失败: {}", e.message()))?;

            let mut out: Vec<FileEntry> = entries
                .into_iter()
                .filter_map(|e| {
                    // FileTree 目前用宿主 `PathBuf` 承载远程路径；反斜杠在 Windows
                    // 会被解释成分隔符，因此无法无损、安全地操作这类远程名称。
                    if !valid_remote_name(&e.name) {
                        return None;
                    }
                    // ALWAYS_IGNORE 目录完全隐藏(与本地树一致)
                    if e.is_dir && ALWAYS_IGNORE.contains(&e.name.as_str()) {
                        return None;
                    }
                    let full = join_posix(path, &e.name);
                    let ignored = posix_relative(project_root, &full)
                        .map(|rel| gitignore.is_ignored(&rel, e.is_dir))
                        .unwrap_or(false);
                    Some(FileEntry {
                        name: e.name,
                        // 远程路径是 POSIX 字符串,`PathBuf` 在这里只是容器 ——
                        // 拼接一律走 `join_posix`,绝不用 `Path::join`(会插 `\`)。
                        path: PathBuf::from(full),
                        is_dir: e.is_dir,
                        ignored,
                    })
                })
                .collect();
            out.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.ignored.cmp(&b.ignored))
                    .then_with(|| natural_cmp(&a.name, &b.name))
            });
            Ok(out)
        }
        .await;
        sftp.close().await;
        result
    })
}

/// 列一个目录的**分流开关**:远程项目走上面的 SFTP 那条,本地项目走
/// [`mt_project::fs::list_directory`]。两条路返回同一个 [`FileEntry`]。
///
/// 文件树只需问一次「这个项目有没有远程连接」
/// ([`AppStore::remote_connection_of`](crate::store::AppStore::remote_connection_of),
/// 断链时是 `None`)就能共用同一段加载代码 —— 分流判据只有这一处,不会出现
/// 「树顶刷新走了本地、展开子目录走了远程」这类半截状态。
///
/// 断链项目由 FileTree 在进入此分流函数前拦住，绝不会把远程 POSIX 路径当成本机
/// 路径读取。
///
/// **阻塞**,丢 `background_executor`。
pub fn list_directory_for(
    remote: Option<&SshConnection>,
    project_root: &std::path::Path,
    dir: &std::path::Path,
    refresh_ignore: bool,
) -> Result<Vec<FileEntry>, String> {
    match remote {
        Some(conn) => list_directory(
            conn,
            &dir.to_string_lossy(),
            &project_root.to_string_lossy(),
            refresh_ignore,
        ),
        None => mt_project::fs::list_directory(project_root, dir).map_err(|e| format!("{e:#}")),
    }
}

// ---------------------------------------------------------------------------
// 入口 2:远程目录验证(「添加远程项目」保存前)
// ---------------------------------------------------------------------------

/// 验证远程路径是一个存在的目录,返回展开后的绝对路径。
/// `~` / `~/xxx` 用 SFTP canonicalize 展开;不存在或不是目录返回 Err。
///
/// 兼作**连接测试**:走完整的「取 session → 认证 → 开 SFTP → canonicalize」,
/// 连不上时的错误面与真实使用一致(原版没有独立的 test 命令,同一条路)。
///
/// **阻塞**,丢 `background_executor`。
pub fn validate_dir(conn: &SshConnection, path: &str) -> Result<String, String> {
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let trimmed = path.trim();
            let expanded = if trimmed.is_empty() || trimmed == "~" || trimmed.starts_with("~/") {
                let home = remote_home(st, &sftp, &conn.id).await?;
                expand_tilde(trimmed, &home)
            } else {
                trimmed.to_string()
            };
            let canonical = sftp
                .canonicalize(&expanded)
                .await
                .map_err(|e| format!("远程路径无效: {}", e.message()))?;
            let is_dir = sftp
                .is_dir(&canonical)
                .await
                .map_err(|e| format!("远程路径不可访问: {}", e.message()))?;
            if !is_dir {
                return Err(format!("远程路径不是目录: {canonical}"));
            }
            Ok(canonical)
        }
        .await;
        sftp.close().await;
        result
    })
}

/// 为“新建远程项目”提供的轻量目录浏览；不应用项目 `.gitignore` 或固定隐藏目录。
/// **阻塞**,调用方必须放到 background executor。
pub fn browse_directory(
    conn: &SshConnection,
    requested_path: &str,
) -> Result<RemoteDirectoryListing, String> {
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let trimmed = requested_path.trim();
            let expanded = if trimmed.is_empty() || trimmed == "~" || trimmed.starts_with("~/") {
                let home = remote_home(st, &sftp, &conn.id).await?;
                expand_tilde(trimmed, &home)
            } else {
                trimmed.to_string()
            };
            let canonical = sftp
                .canonicalize(&expanded)
                .await
                .map_err(|e| format!("远程路径无效: {}", e.message()))?;
            if !sftp
                .is_dir(&canonical)
                .await
                .map_err(|e| format!("远程路径不可访问: {}", e.message()))?
            {
                return Err(format!("远程路径不是目录: {canonical}"));
            }
            let entries = sftp
                .read_dir(&canonical)
                .await
                .map_err(|e| format!("读取远程目录失败: {}", e.message()))?;
            let mut directories = Vec::new();
            for entry in entries {
                if !valid_sftp_child_name(&entry.name) {
                    continue;
                }
                let path = join_posix(&canonical, &entry.name);
                let browsable =
                    entry.is_dir || (entry.is_symlink && sftp.is_dir(&path).await.unwrap_or(false));
                if !browsable {
                    continue;
                }
                directories.push(RemoteDirectoryEntry {
                    path,
                    name: entry.name,
                    is_symlink: entry.is_symlink,
                });
            }
            directories.sort_by(|a, b| natural_cmp(&a.name, &b.name));
            Ok(RemoteDirectoryListing {
                canonical_path: canonical,
                directories,
            })
        }
        .await;
        sftp.close().await;
        result
    })
}

/// 在远程项目目录中新建文件或文件夹。
pub fn create_entry(
    conn: &SshConnection,
    project_root: &str,
    parent_dir: &str,
    name: &str,
    is_dir: bool,
) -> Result<String, String> {
    if !valid_remote_name(name) {
        return Err(format!("文件名无效: {name}"));
    }
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let parent = validate_remote_dir_under_root(&sftp, project_root, parent_dir).await?;
            let target = join_posix(&parent, name);
            if is_dir {
                sftp.create_dir(&target)
                    .await
                    .map_err(|e| format!("创建远程文件夹失败: {}", e.message()))?;
            } else {
                sftp.create_file(&target)
                    .await
                    .map_err(|e| format!("创建远程文件失败: {}", e.message()))?;
            }
            Ok(target)
        }
        .await;
        sftp.close().await;
        result
    })
}

/// 重命名远程条目；新名称只允许单个 POSIX basename。
pub fn rename_entry(
    conn: &SshConnection,
    project_root: &str,
    path: &str,
    new_name: &str,
) -> Result<String, String> {
    if !valid_remote_name(new_name) {
        return Err(format!("文件名无效: {new_name}"));
    }
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let source = validate_remote_leaf_under_root(&sftp, project_root, path).await?;
            let (parent, _) = split_posix_leaf(&source)?;
            let target = join_posix(parent, new_name);
            sftp.rename(&source, &target)
                .await
                .map_err(|e| format!("重命名远程条目失败: {}", e.message()))?;
            Ok(target)
        }
        .await;
        sftp.close().await;
        result
    })
}

/// 连接自检:只探到远程 `$HOME` 为止,返回它。
///
/// 原版没有独立的「测试连接」command(SshModal 只做 CRUD),这里把
/// [`validate_dir`] 的前半段单独暴露,BB-b 若要做「测试」按钮可以直接用 ——
/// 不新增任何行为,失败文案与真实使用同源。
/// ⚠️ **有意保留的无调用点代码**(窄作用域 `allow`,不是整模块的那种):
/// BB-b 逐字复刻 `SshModal.tsx` 时确认原版**没有**「测试连接」按钮,所以没有
/// 入口接它。删掉的话哪天要加这颗按钮又得把同一段重写一遍,而它与
/// [`validate_dir`] 共用同一条失败面 —— 留着零成本。
#[allow(dead_code)]
pub fn probe_connection(conn: &SshConnection) -> Result<String, String> {
    validate_dir(conn, "~")
}
