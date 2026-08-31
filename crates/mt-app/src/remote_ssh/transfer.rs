use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use mt_config::SshConnection;
use mt_ssh::{SftpHandle, SftpNodeKind};

use super::{
    FileConflictStrategy, LOCAL_TRANSFER_SEQUENCE, PASTE_UPLOAD_REQUEST_TIMEOUT,
    PASTE_UPLOAD_TOTAL_TIMEOUT, join_posix, keep_both_name, keep_both_remote_path, open_sftp,
    open_sftp_with_session, paste_file_name, posix_relative, remote_home, resolve_paste_dir,
    split_posix_leaf, state, valid_remote_name, validate_remote_dir_under_root,
    validate_remote_leaf_under_root,
};

#[derive(Debug, Clone, Default)]
pub struct FileOperationSummary {
    pub completed: usize,
    pub skipped: usize,
    pub failed: usize,
    pub bytes: u64,
    pub warnings: Vec<String>,
}

async fn remove_remote_tree(
    sftp: &SftpHandle,
    target: String,
    target_kind: SftpNodeKind,
) -> Result<usize, String> {
    sftp.remove_tree(&target, target_kind)
        .await
        .map_err(|e| format!("删除远程条目失败: {}", e.message()))
}

async fn discard_remote_staged_entry(sftp: &SftpHandle, staging: &str) -> Result<(), String> {
    let kind = sftp
        .node_kind(staging)
        .await
        .map_err(|e| format!("远程暂存条目不可访问: {}", e.message()))?;
    remove_remote_tree(sftp, staging.to_string(), kind)
        .await
        .map(|_| ())
}

async fn commit_new_remote_staged_directory(
    sftp: &SftpHandle,
    staging: &str,
    target: &str,
) -> Result<(), String> {
    if let Err(error) = sftp.rename(staging, target).await {
        let cleanup = discard_remote_staged_entry(sftp, staging).await;
        return match cleanup {
            Ok(()) => Err(format!("提交远程目录失败: {}", error.message())),
            Err(cleanup_error) => Err(format!(
                "提交远程目录失败: {}; 清理暂存目录也失败: {cleanup_error}",
                error.message()
            )),
        };
    }
    Ok(())
}

/// 在同一远程项目中复制文件或目录；同名时自动生成副本名。
pub fn copy_entry_keep_both(
    conn: &SshConnection,
    project_root: &str,
    source_path: &str,
    target_dir: &str,
) -> Result<(String, FileOperationSummary), String> {
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let source = validate_remote_leaf_under_root(&sftp, project_root, source_path).await?;
            let target_dir =
                validate_remote_dir_under_root(&sftp, project_root, target_dir).await?;
            let (_, source_name) = split_posix_leaf(&source)?;
            let desired = join_posix(&target_dir, source_name);
            let target = keep_both_remote_path(&sftp, &desired).await?;
            let source_kind = sftp
                .node_kind(&source)
                .await
                .map_err(|e| format!("远程源条目不可访问: {}", e.message()))?;
            if source_kind == SftpNodeKind::Directory && posix_relative(&source, &target).is_some()
            {
                return Err("不能把远程目录复制到自身或其子目录".into());
            }
            let mut summary = FileOperationSummary::default();
            match source_kind {
                SftpNodeKind::Symlink | SftpNodeKind::Other => {
                    return Err("暂不复制远程符号链接或特殊文件".into());
                }
                SftpNodeKind::File => {
                    summary.bytes = sftp
                        .copy_file(&source, &target, false)
                        .await
                        .map_err(|e| format!("复制远程文件失败: {}", e.message()))?;
                    summary.completed = 1;
                }
                SftpNodeKind::Directory => {
                    let staging = sftp.temporary_sibling_path(&target, "copy-directory");
                    sftp.create_dir(&staging)
                        .await
                        .map_err(|e| format!("创建远程副本目录失败: {}", e.message()))?;
                    let copy_result: Result<(), String> = async {
                        let mut stack = vec![(source, staging.clone())];
                        while let Some((source_dir, target_dir)) = stack.pop() {
                            let entries = sftp
                                .read_dir(&source_dir)
                                .await
                                .map_err(|e| format!("读取远程源目录失败: {}", e.message()))?;
                            for entry in entries {
                                if !valid_remote_name(&entry.name) {
                                    return Err(format!(
                                        "服务器返回了无效条目名: {:?}",
                                        entry.name
                                    ));
                                }
                                let source_child = join_posix(&source_dir, &entry.name);
                                let target_child = join_posix(&target_dir, &entry.name);
                                if entry.is_symlink {
                                    summary.skipped += 1;
                                    summary
                                        .warnings
                                        .push(format!("已跳过符号链接: {source_child}"));
                                } else if entry.is_dir {
                                    sftp.create_dir(&target_child).await.map_err(|e| {
                                        format!("创建远程副本目录失败: {}", e.message())
                                    })?;
                                    summary.completed += 1;
                                    stack.push((source_child, target_child));
                                } else if entry.is_file {
                                    summary.bytes += sftp
                                        .copy_file(&source_child, &target_child, false)
                                        .await
                                        .map_err(|e| {
                                            format!("复制远程文件失败: {}", e.message())
                                        })?;
                                    summary.completed += 1;
                                } else {
                                    summary.skipped += 1;
                                    summary
                                        .warnings
                                        .push(format!("已跳过特殊文件: {source_child}"));
                                }
                            }
                        }
                        Ok(())
                    }
                    .await;
                    if let Err(error) = copy_result {
                        let cleanup = discard_remote_staged_entry(&sftp, &staging).await;
                        return match cleanup {
                            Ok(()) => Err(error),
                            Err(cleanup_error) => {
                                Err(format!("{error}; 清理远程暂存目录失败: {cleanup_error}"))
                            }
                        };
                    }
                    commit_new_remote_staged_directory(&sftp, &staging, &target).await?;
                    summary.completed += 1;
                }
            }
            Ok((target, summary))
        }
        .await;
        sftp.close().await;
        result
    })
}

type RemoteDirectoryCache = HashMap<String, HashMap<String, SftpNodeKind>>;

async fn remote_kind_cached(
    sftp: &SftpHandle,
    path: &str,
    cache: &mut RemoteDirectoryCache,
) -> Result<Option<SftpNodeKind>, String> {
    let (parent, name) = split_posix_leaf(path)?;
    if !cache.contains_key(parent) {
        let entries = sftp
            .read_dir(parent)
            .await
            .map_err(|e| format!("读取远程目录失败: {}", e.message()))?
            .into_iter()
            .map(|entry| {
                let kind = if entry.is_symlink {
                    SftpNodeKind::Symlink
                } else if entry.is_dir {
                    SftpNodeKind::Directory
                } else if entry.is_file {
                    SftpNodeKind::File
                } else {
                    SftpNodeKind::Other
                };
                (entry.name, kind)
            })
            .collect();
        cache.insert(parent.to_string(), entries);
    }
    Ok(cache
        .get(parent)
        .and_then(|entries| entries.get(name))
        .copied())
}

fn set_remote_kind_cached(
    cache: &mut RemoteDirectoryCache,
    path: &str,
    kind: SftpNodeKind,
) -> Result<(), String> {
    let (parent, name) = split_posix_leaf(path)?;
    if let Some(entries) = cache.get_mut(parent) {
        entries.insert(name.to_string(), kind);
    }
    Ok(())
}

fn remove_remote_kind_cached(cache: &mut RemoteDirectoryCache, path: &str) -> Result<(), String> {
    let (parent, name) = split_posix_leaf(path)?;
    if let Some(entries) = cache.get_mut(parent) {
        entries.remove(name);
    }
    Ok(())
}

fn invalidate_remote_cache_subtree(cache: &mut RemoteDirectoryCache, path: &str) {
    let prefix = format!("{}/", path.trim_end_matches('/'));
    cache.retain(|dir, _| dir != path && !dir.starts_with(&prefix));
}

fn invalidate_remote_parent_cache(cache: &mut RemoteDirectoryCache, path: &str) {
    if let Ok((parent, _)) = split_posix_leaf(path) {
        cache.remove(parent);
    }
}

async fn keep_both_remote_path_cached(
    sftp: &SftpHandle,
    desired: &str,
    cache: &mut RemoteDirectoryCache,
) -> Result<String, String> {
    let (parent, name) = split_posix_leaf(desired)?;
    let _ = remote_kind_cached(sftp, desired, cache).await?;
    let existing = cache
        .get(parent)
        .ok_or_else(|| format!("远程目录缓存缺失: {parent}"))?;
    for ordinal in 1..=10_000 {
        let candidate = keep_both_name(name, ordinal);
        if !existing.contains_key(&candidate) {
            return Ok(join_posix(parent, &candidate));
        }
    }
    Err(format!("无法为远程条目生成可用副本名: {desired}"))
}

fn local_kind(path: &Path) -> Result<SftpNodeKind, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| format!("无法读取本地条目 {}: {e}", path.display()))?;
    let ty = metadata.file_type();
    Ok(if ty.is_symlink() {
        SftpNodeKind::Symlink
    } else if ty.is_dir() {
        SftpNodeKind::Directory
    } else if ty.is_file() {
        SftpNodeKind::File
    } else {
        SftpNodeKind::Other
    })
}

fn remove_local_entry(path: &Path) -> Result<(), String> {
    match local_kind(path)? {
        SftpNodeKind::Directory => std::fs::remove_dir_all(path)
            .map_err(|e| format!("删除本地目录 {} 失败: {e}", path.display())),
        _ => std::fs::remove_file(path)
            .map_err(|e| format!("删除本地文件 {} 失败: {e}", path.display())),
    }
}

fn create_local_operation_container(target: &Path, role: &str) -> Result<PathBuf, String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("无法获取本地目标父目录: {}", target.display()))?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("entry");
    for _ in 0..10_000 {
        let sequence = LOCAL_TRANSFER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{name}.mt-{role}-{}-{sequence}",
            std::process::id()
        ));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "创建本地操作目录 {} 失败: {error}",
                    candidate.display()
                ));
            }
        }
    }
    Err(format!("无法为本地目标分配暂存目录: {}", target.display()))
}

fn create_local_staging_directory(target: &Path) -> Result<(PathBuf, PathBuf), String> {
    let container = create_local_operation_container(target, "download")?;
    let staging = container.join("entry");
    if let Err(error) = std::fs::create_dir(&staging) {
        let _ = std::fs::remove_dir(&container);
        return Err(format!(
            "创建本地暂存目录 {} 失败: {error}",
            staging.display()
        ));
    }
    Ok((container, staging))
}

fn commit_new_local_staged_directory(
    staging: &Path,
    staging_container: &Path,
    target: &Path,
) -> Result<(), String> {
    match std::fs::symlink_metadata(target) {
        Ok(_) => {
            let _ = remove_local_entry(staging_container);
            return Err(format!(
                "提交本地下载目录时目标已存在: {}",
                target.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let _ = remove_local_entry(staging_container);
            return Err(format!(
                "提交前检查本地目标 {} 失败: {error}",
                target.display()
            ));
        }
    }
    if let Err(error) = std::fs::rename(staging, target) {
        let cleanup = remove_local_entry(staging_container);
        return match cleanup {
            Ok(()) => Err(format!(
                "提交本地下载目录 {} 失败: {error}",
                target.display()
            )),
            Err(cleanup_error) => Err(format!(
                "提交本地下载目录 {} 失败: {error}; 清理暂存目录也失败: {cleanup_error}",
                target.display()
            )),
        };
    }
    std::fs::remove_dir(staging_container).map_err(|error| {
        format!(
            "清理本地下载暂存目录 {} 失败: {error}",
            staging_container.display()
        )
    })
}

fn replace_local_staged_entry(
    staging: &Path,
    staging_container: &Path,
    target: &Path,
) -> Result<(), String> {
    let backup_container = create_local_operation_container(target, "backup")?;
    let backup = backup_container.join("entry");
    if let Err(error) = std::fs::rename(target, &backup) {
        let _ = remove_local_entry(staging_container);
        let _ = std::fs::remove_dir(&backup_container);
        return Err(format!("备份本地目标 {} 失败: {error}", target.display()));
    }
    if let Err(promote_error) = std::fs::rename(staging, target) {
        let rollback = std::fs::rename(&backup, target);
        let _ = remove_local_entry(staging_container);
        let _ = std::fs::remove_dir(&backup_container);
        return match rollback {
            Ok(()) => Err(format!(
                "提交本地下载 {} 失败: {promote_error}",
                target.display()
            )),
            Err(rollback_error) => Err(format!(
                "提交本地下载失败且恢复失败: {promote_error}; rollback: {rollback_error}; backup: {}",
                backup.display()
            )),
        };
    }
    std::fs::remove_dir(staging_container).map_err(|error| {
        format!(
            "清理本地下载暂存目录 {} 失败: {error}",
            staging_container.display()
        )
    })?;
    remove_local_entry(&backup)
        .map_err(|error| format!("下载完成但清理备份 {} 失败: {error}", backup.display()))?;
    std::fs::remove_dir(&backup_container).map_err(|error| {
        format!(
            "清理本地备份目录 {} 失败: {error}",
            backup_container.display()
        )
    })?;
    Ok(())
}

fn keep_both_local_path(desired: &Path) -> Result<PathBuf, String> {
    if std::fs::symlink_metadata(desired).is_err() {
        return Ok(desired.to_path_buf());
    }
    let name = desired
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("本地目标名称不是有效 UTF-8: {}", desired.display()))?;
    for ordinal in 1..=10_000 {
        let candidate = desired.with_file_name(keep_both_name(name, ordinal));
        if std::fs::symlink_metadata(&candidate).is_err() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "无法为本地条目生成可用副本名: {}",
        desired.display()
    ))
}

pub(super) fn collect_upload_conflicts(
    existing: &HashSet<String>,
    local_paths: &[PathBuf],
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut reported = HashSet::new();
    let mut conflicts = Vec::new();
    for path in local_paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let repeated_in_batch = !seen.insert(name.to_string());
        if (existing.contains(name) || repeated_in_batch) && reported.insert(name.to_string()) {
            conflicts.push(name.to_string());
        }
    }
    conflicts
}

/// 上传前扫描顶层冲突；返回发生冲突的本地条目名称。
pub fn upload_conflicts(
    conn: &SshConnection,
    project_root: &str,
    target_dir: &str,
    local_paths: &[PathBuf],
) -> Result<Vec<String>, String> {
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let target_dir =
                validate_remote_dir_under_root(&sftp, project_root, target_dir).await?;
            let existing: HashSet<String> = sftp
                .read_dir(&target_dir)
                .await
                .map_err(|e| format!("读取远程目录失败: {}", e.message()))?
                .into_iter()
                .map(|entry| entry.name)
                .collect();
            Ok(collect_upload_conflicts(&existing, local_paths))
        }
        .await;
        sftp.close().await;
        result
    })
}

async fn upload_path_tree(
    sftp: &SftpHandle,
    local_root: PathBuf,
    remote_root: String,
    strategy: FileConflictStrategy,
    summary: &mut FileOperationSummary,
    remote_cache: &mut RemoteDirectoryCache,
) -> Result<(), String> {
    enum UploadWork {
        Visit {
            local: PathBuf,
            desired_remote: String,
            inside_staging: bool,
            staging_replaces_existing: bool,
        },
        CommitDirectory {
            staging: String,
            target: String,
            replace_existing: bool,
            summary_before: FileOperationSummary,
        },
    }

    let mut stack = vec![UploadWork::Visit {
        local: local_root,
        desired_remote: remote_root,
        inside_staging: false,
        staging_replaces_existing: false,
    }];
    let mut staged_directories = HashSet::new();
    let mut result: Result<(), String> = async {
        while let Some(work) = stack.pop() {
            let (local, desired_remote, inside_staging, staging_replaces_existing) = match work {
                UploadWork::Visit {
                    local,
                    desired_remote,
                    inside_staging,
                    staging_replaces_existing,
                } => (
                    local,
                    desired_remote,
                    inside_staging,
                    staging_replaces_existing,
                ),
                UploadWork::CommitDirectory {
                    staging,
                    target,
                    replace_existing,
                    summary_before,
                } => {
                    let commit_result = if replace_existing {
                        sftp.replace_staged_entry(&staging, &target)
                            .await
                            .map_err(|e| format!("替换远程目录失败: {}", e.message()))
                    } else {
                        commit_new_remote_staged_directory(sftp, &staging, &target).await
                    };
                    if let Err(error) = commit_result {
                        let rollback_summary = stack
                            .iter()
                            .find_map(|work| match work {
                                UploadWork::CommitDirectory { summary_before, .. } => {
                                    Some(summary_before.clone())
                                }
                                UploadWork::Visit { .. } => None,
                            })
                            .unwrap_or(summary_before);
                        *summary = rollback_summary;
                        invalidate_remote_parent_cache(remote_cache, &target);
                        invalidate_remote_parent_cache(remote_cache, &staging);
                        invalidate_remote_cache_subtree(remote_cache, &target);
                        invalidate_remote_cache_subtree(remote_cache, &staging);
                        return Err(error);
                    }
                    staged_directories.remove(&staging);
                    invalidate_remote_cache_subtree(remote_cache, &target);
                    invalidate_remote_cache_subtree(remote_cache, &staging);
                    remove_remote_kind_cached(remote_cache, &staging)?;
                    set_remote_kind_cached(remote_cache, &target, SftpNodeKind::Directory)?;
                    summary.completed += 1;
                    continue;
                }
            };
            let kind = match local_kind(&local) {
                Ok(kind) => kind,
                Err(error) if inside_staging => {
                    return Err(format!("远程暂存目录未完整构建: {error}"));
                }
                Err(error) => {
                    summary.failed += 1;
                    summary.warnings.push(error);
                    continue;
                }
            };
            if matches!(kind, SftpNodeKind::Symlink | SftpNodeKind::Other) {
                let warning = format!("已跳过本地符号链接或特殊文件: {}", local.display());
                if staging_replaces_existing {
                    return Err(format!("远程暂存目录未完整构建: {warning}"));
                }
                summary.skipped += 1;
                summary.warnings.push(warning);
                continue;
            }

            let existing = remote_kind_cached(sftp, &desired_remote, remote_cache).await?;
            if inside_staging && existing.is_some() {
                return Err(format!(
                    "远程暂存目录被意外修改，拒绝提交: {desired_remote}"
                ));
            }
            let (remote, existing) = match (existing, strategy) {
                (Some(_), FileConflictStrategy::Skip) => {
                    summary.skipped += 1;
                    continue;
                }
                (Some(_), FileConflictStrategy::KeepBoth) => (
                    keep_both_remote_path_cached(sftp, &desired_remote, remote_cache).await?,
                    None,
                ),
                (existing, _) => (desired_remote, existing),
            };

            match kind {
                SftpNodeKind::Directory => {
                    let mut completes_immediately = true;
                    let (child_remote_base, child_inside_staging, child_staging_replaces_existing) =
                        match existing {
                            None if inside_staging => {
                                sftp.create_dir(&remote)
                                    .await
                                    .map_err(|e| format!("创建远程目录失败: {}", e.message()))?;
                                set_remote_kind_cached(
                                    remote_cache,
                                    &remote,
                                    SftpNodeKind::Directory,
                                )?;
                                remote_cache.insert(remote.clone(), HashMap::new());
                                (remote.clone(), true, staging_replaces_existing)
                            }
                            Some(SftpNodeKind::Directory)
                                if strategy == FileConflictStrategy::Overwrite =>
                            {
                                (remote.clone(), inside_staging, staging_replaces_existing)
                            }
                            existing => {
                                let replace_existing = existing.is_some();
                                let staging = sftp.temporary_sibling_path(&remote, "directory");
                                sftp.create_dir(&staging).await.map_err(|e| {
                                    format!("创建远程暂存目录失败: {}", e.message())
                                })?;
                                staged_directories.insert(staging.clone());
                                set_remote_kind_cached(
                                    remote_cache,
                                    &staging,
                                    SftpNodeKind::Directory,
                                )?;
                                remote_cache.insert(staging.clone(), HashMap::new());
                                stack.push(UploadWork::CommitDirectory {
                                    staging: staging.clone(),
                                    target: remote.clone(),
                                    replace_existing,
                                    summary_before: summary.clone(),
                                });
                                completes_immediately = false;
                                (staging, true, staging_replaces_existing || replace_existing)
                            }
                        };
                    if completes_immediately {
                        summary.completed += 1;
                    }
                    let entries = std::fs::read_dir(&local)
                        .map_err(|e| format!("读取本地目录 {} 失败: {e}", local.display()))?;
                    let mut children = Vec::new();
                    for entry in entries {
                        let entry = entry
                            .map_err(|e| format!("读取本地目录项 {} 失败: {e}", local.display()))?;
                        let name = entry.file_name();
                        let Some(name) = name.to_str() else {
                            let warning = format!(
                                "已跳过名称不是有效 UTF-8 的本地条目: {}",
                                entry.path().display()
                            );
                            if child_staging_replaces_existing {
                                return Err(format!("远程暂存目录未完整构建: {warning}"));
                            }
                            summary.skipped += 1;
                            summary.warnings.push(warning);
                            continue;
                        };
                        if !valid_remote_name(name) {
                            let warning = format!("已跳过远程不支持的名称: {name}");
                            if child_staging_replaces_existing {
                                return Err(format!("远程暂存目录未完整构建: {warning}"));
                            }
                            summary.skipped += 1;
                            summary.warnings.push(warning);
                            continue;
                        }
                        children.push(UploadWork::Visit {
                            local: entry.path(),
                            desired_remote: join_posix(&child_remote_base, name),
                            inside_staging: child_inside_staging,
                            staging_replaces_existing: child_staging_replaces_existing,
                        });
                    }
                    children.reverse();
                    stack.extend(children);
                }
                SftpNodeKind::File => {
                    let overwrite =
                        existing.is_some() && strategy == FileConflictStrategy::Overwrite;
                    summary.bytes += sftp
                        .upload_file(&local, &remote, overwrite)
                        .await
                        .map_err(|e| format!("上传文件失败: {}", e.message()))?;
                    invalidate_remote_cache_subtree(remote_cache, &remote);
                    set_remote_kind_cached(remote_cache, &remote, SftpNodeKind::File)?;
                    summary.completed += 1;
                }
                SftpNodeKind::Symlink | SftpNodeKind::Other => unreachable!(),
            }
        }
        Ok(())
    }
    .await;

    if result.is_err() {
        if let Some(summary_before) = stack.iter().find_map(|work| match work {
            UploadWork::CommitDirectory { summary_before, .. } => Some(summary_before.clone()),
            UploadWork::Visit { .. } => None,
        }) {
            *summary = summary_before;
        }
        let mut cleanup_errors = Vec::new();
        for staging in staged_directories {
            if let Ok(kind) = sftp.node_kind(&staging).await
                && let Err(error) = sftp.remove_tree(&staging, kind).await
            {
                cleanup_errors.push(format!("{staging}: {}", error.message()));
            }
        }
        if !cleanup_errors.is_empty()
            && let Err(original) = &result
        {
            let original = original.clone();
            result = Err(format!(
                "{original}; 清理远程暂存目录失败: {}",
                cleanup_errors.join("; ")
            ));
        }
    }
    result
}

/// 上传一批本地文件/文件夹到远程目录。目录 Overwrite 为递归合并并保留目标独有项。
pub fn upload_paths(
    conn: &SshConnection,
    project_root: &str,
    target_dir: &str,
    local_paths: &[PathBuf],
    strategy: FileConflictStrategy,
) -> Result<FileOperationSummary, String> {
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let target_dir =
                validate_remote_dir_under_root(&sftp, project_root, target_dir).await?;
            let mut summary = FileOperationSummary::default();
            let mut remote_cache = RemoteDirectoryCache::new();
            for local in local_paths {
                let Some(name) = local.file_name().and_then(|name| name.to_str()) else {
                    summary.skipped += 1;
                    summary.warnings.push(format!(
                        "已跳过名称不是有效 UTF-8 的本地条目: {}",
                        local.display()
                    ));
                    continue;
                };
                if !valid_remote_name(name) {
                    summary.skipped += 1;
                    summary
                        .warnings
                        .push(format!("已跳过远程不支持的名称: {name}"));
                    continue;
                }
                if let Err(error) = upload_path_tree(
                    &sftp,
                    local.clone(),
                    join_posix(&target_dir, name),
                    strategy,
                    &mut summary,
                    &mut remote_cache,
                )
                .await
                {
                    remote_cache.clear();
                    summary.failed += 1;
                    summary.warnings.push(error);
                }
            }
            Ok(summary)
        }
        .await;
        sftp.close().await;
        result
    })
}

pub(super) fn ensure_local_download_target(
    download_root: &Path,
    target: &Path,
) -> Result<(), String> {
    if !download_root.is_absolute() {
        return Err(format!(
            "下载根目录必须是绝对路径: {}",
            download_root.display()
        ));
    }
    if !target.starts_with(download_root) {
        return Err(format!(
            "本地下载目标逃出下载目录: {} (root: {})",
            target.display(),
            download_root.display()
        ));
    }
    Ok(())
}

pub(super) fn checked_local_download_child(
    download_root: &Path,
    parent: &Path,
    name: &str,
) -> Result<PathBuf, String> {
    if !valid_remote_name(name) {
        return Err(format!("远程文件名不能安全落到本机: {name:?}"));
    }
    ensure_local_download_target(download_root, parent)?;
    let target = parent.join(name);
    ensure_local_download_target(download_root, &target)?;
    Ok(target)
}

/// 下载前检查顶层目标是否已存在。
pub fn download_conflicts(
    download_dir: &Path,
    remote_paths: &[PathBuf],
) -> Result<Vec<String>, String> {
    let mut conflicts = Vec::new();
    for path in remote_paths {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("远程下载目标名称无效: {}", path.display()))?;
        let target = checked_local_download_child(download_dir, download_dir, name)?;
        if std::fs::symlink_metadata(target).is_ok() {
            conflicts.push(name.to_string());
        }
    }
    Ok(conflicts)
}

async fn download_remote_tree(
    sftp: &SftpHandle,
    remote_root: String,
    download_root: &Path,
    local_root: PathBuf,
    strategy: FileConflictStrategy,
    summary: &mut FileOperationSummary,
) -> Result<(), String> {
    ensure_local_download_target(download_root, &local_root)?;
    enum DownloadWork {
        Visit {
            remote: String,
            desired_local: PathBuf,
            known_kind: Option<SftpNodeKind>,
            inside_staging: bool,
            staging_replaces_existing: bool,
        },
        CommitDirectory {
            staging: PathBuf,
            staging_container: PathBuf,
            target: PathBuf,
            replace_existing: bool,
            summary_before: FileOperationSummary,
        },
    }

    let mut stack = vec![DownloadWork::Visit {
        remote: remote_root,
        desired_local: local_root,
        known_kind: None,
        inside_staging: false,
        staging_replaces_existing: false,
    }];
    let mut staging_containers = HashSet::new();
    let mut result: Result<(), String> = async {
        while let Some(work) = stack.pop() {
            let (remote, desired_local, known_kind, inside_staging, staging_replaces_existing) =
                match work {
                    DownloadWork::Visit {
                        remote,
                        desired_local,
                        known_kind,
                        inside_staging,
                        staging_replaces_existing,
                    } => (
                        remote,
                        desired_local,
                        known_kind,
                        inside_staging,
                        staging_replaces_existing,
                    ),
                    DownloadWork::CommitDirectory {
                        staging,
                        staging_container,
                        target,
                        replace_existing,
                        summary_before,
                    } => {
                        ensure_local_download_target(download_root, &staging)?;
                        ensure_local_download_target(download_root, &staging_container)?;
                        ensure_local_download_target(download_root, &target)?;
                        let commit_result = if replace_existing {
                            replace_local_staged_entry(&staging, &staging_container, &target)
                        } else {
                            commit_new_local_staged_directory(&staging, &staging_container, &target)
                        };
                        if let Err(error) = commit_result {
                            let rollback_summary = stack
                                .iter()
                                .find_map(|work| match work {
                                    DownloadWork::CommitDirectory { summary_before, .. } => {
                                        Some(summary_before.clone())
                                    }
                                    DownloadWork::Visit { .. } => None,
                                })
                                .unwrap_or(summary_before);
                            *summary = rollback_summary;
                            return Err(error);
                        }
                        staging_containers.remove(&staging_container);
                        summary.completed += 1;
                        continue;
                    }
                };
            ensure_local_download_target(download_root, &desired_local)?;
            let kind = match known_kind {
                Some(kind) => kind,
                None => sftp
                    .node_kind(&remote)
                    .await
                    .map_err(|e| format!("远程条目不可访问: {}", e.message()))?,
            };
            if matches!(kind, SftpNodeKind::Symlink | SftpNodeKind::Other) {
                if staging_replaces_existing {
                    return Err(format!(
                        "本地下载暂存目录未完整构建: 远程条目不可传输: {remote}"
                    ));
                }
                summary.skipped += 1;
                summary
                    .warnings
                    .push(format!("已跳过远程符号链接或特殊文件: {remote}"));
                continue;
            }

            let existing = std::fs::symlink_metadata(&desired_local)
                .ok()
                .map(|metadata| {
                    let ty = metadata.file_type();
                    if ty.is_symlink() {
                        SftpNodeKind::Symlink
                    } else if ty.is_dir() {
                        SftpNodeKind::Directory
                    } else if ty.is_file() {
                        SftpNodeKind::File
                    } else {
                        SftpNodeKind::Other
                    }
                });
            if inside_staging && existing.is_some() {
                return Err(format!(
                    "本地下载暂存目录被意外修改，拒绝提交: {}",
                    desired_local.display()
                ));
            }
            let (local, existing) = match (existing, strategy) {
                (Some(_), FileConflictStrategy::Skip) => {
                    summary.skipped += 1;
                    continue;
                }
                (Some(_), FileConflictStrategy::KeepBoth) => {
                    (keep_both_local_path(&desired_local)?, None)
                }
                (existing, _) => (desired_local, existing),
            };
            ensure_local_download_target(download_root, &local)?;

            match kind {
                SftpNodeKind::Directory => {
                    let mut completes_immediately = true;
                    let (child_local_base, child_inside_staging, child_staging_replaces_existing) =
                        match existing {
                            None if inside_staging => {
                                std::fs::create_dir(&local).map_err(|e| {
                                    format!("创建本地下载目录 {} 失败: {e}", local.display())
                                })?;
                                (local.clone(), true, staging_replaces_existing)
                            }
                            Some(SftpNodeKind::Directory)
                                if strategy == FileConflictStrategy::Overwrite =>
                            {
                                (local.clone(), inside_staging, staging_replaces_existing)
                            }
                            existing => {
                                let replace_existing = existing.is_some();
                                let (staging_container, staging) =
                                    create_local_staging_directory(&local)?;
                                ensure_local_download_target(download_root, &staging_container)?;
                                ensure_local_download_target(download_root, &staging)?;
                                staging_containers.insert(staging_container.clone());
                                stack.push(DownloadWork::CommitDirectory {
                                    staging: staging.clone(),
                                    staging_container,
                                    target: local.clone(),
                                    replace_existing,
                                    summary_before: summary.clone(),
                                });
                                completes_immediately = false;
                                (staging, true, staging_replaces_existing || replace_existing)
                            }
                        };
                    if completes_immediately {
                        summary.completed += 1;
                    }
                    let entries = sftp
                        .read_dir(&remote)
                        .await
                        .map_err(|e| format!("读取远程目录失败: {}", e.message()))?;
                    for entry in entries.into_iter().rev() {
                        if !valid_remote_name(&entry.name) {
                            if child_staging_replaces_existing {
                                return Err(format!(
                                    "本地下载暂存目录未完整构建: 服务器返回了无效条目名: {:?}",
                                    entry.name
                                ));
                            }
                            summary.skipped += 1;
                            summary
                                .warnings
                                .push(format!("服务器返回了无效条目名: {:?}", entry.name));
                            continue;
                        }
                        let kind = if entry.is_symlink {
                            SftpNodeKind::Symlink
                        } else if entry.is_dir {
                            SftpNodeKind::Directory
                        } else if entry.is_file {
                            SftpNodeKind::File
                        } else {
                            SftpNodeKind::Other
                        };
                        stack.push(DownloadWork::Visit {
                            remote: join_posix(&remote, &entry.name),
                            desired_local: checked_local_download_child(
                                download_root,
                                &child_local_base,
                                &entry.name,
                            )?,
                            known_kind: Some(kind),
                            inside_staging: child_inside_staging,
                            staging_replaces_existing: child_staging_replaces_existing,
                        });
                    }
                }
                SftpNodeKind::File => {
                    let overwrite =
                        existing.is_some() && strategy == FileConflictStrategy::Overwrite;
                    summary.bytes += sftp
                        .download_file(&remote, &local, overwrite)
                        .await
                        .map_err(|e| format!("下载远程文件失败: {}", e.message()))?;
                    summary.completed += 1;
                }
                SftpNodeKind::Symlink | SftpNodeKind::Other => unreachable!(),
            }
        }
        Ok(())
    }
    .await;

    if result.is_err() {
        if let Some(summary_before) = stack.iter().find_map(|work| match work {
            DownloadWork::CommitDirectory { summary_before, .. } => Some(summary_before.clone()),
            DownloadWork::Visit { .. } => None,
        }) {
            *summary = summary_before;
        }
        let mut cleanup_errors = Vec::new();
        for staging_container in staging_containers {
            if std::fs::symlink_metadata(&staging_container).is_ok()
                && let Err(error) = remove_local_entry(&staging_container)
            {
                cleanup_errors.push(error);
            }
        }
        if !cleanup_errors.is_empty()
            && let Err(original) = &result
        {
            let original = original.clone();
            result = Err(format!(
                "{original}; 清理本地下载暂存目录失败: {}",
                cleanup_errors.join("; ")
            ));
        }
    }
    result
}

/// 下载一个或多个远程条目到本地目录。
pub fn download_entries(
    conn: &SshConnection,
    project_root: &str,
    remote_paths: &[PathBuf],
    download_dir: &Path,
    strategy: FileConflictStrategy,
) -> Result<FileOperationSummary, String> {
    if !download_dir.is_absolute() {
        return Err(format!(
            "下载目录必须是绝对路径: {}",
            download_dir.display()
        ));
    }
    std::fs::create_dir_all(download_dir)
        .map_err(|e| format!("无法创建下载目录 {}: {e}", download_dir.display()))?;
    mt_config::AppConfig::validate_download_dir(download_dir)
        .map_err(|e| format!("下载目录不可用: {e:#}"))?;
    let download_root = std::fs::canonicalize(download_dir)
        .map_err(|e| format!("无法解析下载目录 {}: {e}", download_dir.display()))?;
    let st = state();
    st.block_on(async move {
        let sftp = open_sftp(st, conn).await?;
        let result = async {
            let mut summary = FileOperationSummary::default();
            for remote_path in remote_paths {
                let remote = validate_remote_leaf_under_root(
                    &sftp,
                    project_root,
                    &remote_path.to_string_lossy(),
                )
                .await?;
                let (_, name) = split_posix_leaf(&remote)?;
                let local_root =
                    checked_local_download_child(&download_root, &download_root, name)?;
                if let Err(error) = download_remote_tree(
                    &sftp,
                    remote,
                    &download_root,
                    local_root,
                    strategy,
                    &mut summary,
                )
                .await
                {
                    summary.failed += 1;
                    summary.warnings.push(error);
                }
            }
            Ok(summary)
        }
        .await;
        sftp.close().await;
        result
    })
}

// ---------------------------------------------------------------------------
// 入口 3:粘贴内容上传(issue #36)
// ---------------------------------------------------------------------------

/// 把本地临时文件(剪贴板图片 / 长文本转存)上传到远程项目,返回**远端绝对路径**。
///
/// 背景:远程项目的 pane 跑的是本地 `ssh` 客户端,粘贴走本地链路只会得到一个
/// Windows 路径 —— 远端 agent 读不到。这里另开一条 SFTP(池里同一条 session)
/// 把文件送过去,调用方再把返回的远端路径粘进终端。
///
/// 目标目录由 `dest_dir` 决定(见 [`resolve_paste_dir`]),不存在则逐级创建。
/// 同名覆盖:文件名由调用方生成(`paste-<ms>.txt`),带毫秒时间戳,实际不会撞。
///
/// **阻塞**,丢 `background_executor`。
pub fn upload_paste(
    conn: &SshConnection,
    project_path: &str,
    local_path: &str,
    dest_dir: &str,
) -> Result<String, String> {
    let st = state();
    let file_name = paste_file_name(local_path)?;
    st.block_on(async move {
        let (session, sftp) = open_sftp_with_session(st, conn).await?;

        // 整段(建目录 + 上传)套一层墙钟上限:见 PASTE_UPLOAD_TOTAL_TIMEOUT。
        let result = tokio::time::timeout(PASTE_UPLOAD_TOTAL_TIMEOUT, async {
            let home = remote_home(st, &sftp, &conn.id).await?;
            let dir = resolve_paste_dir(project_path, &home, dest_dir)?;
            sftp.create_dir_all(&dir)
                .await
                .map_err(|e| format!("创建远程粘贴目录失败: {}", e.message()))?;

            // 目录**严格位于**项目内(默认形态)时放一个自忽略的 .gitignore ——
            // 否则每次粘图都会把用户仓库的 `git status` 弄脏。
            // CREATE|EXCLUDE 语义天然幂等,已存在就失败,失败也无所谓:这只是体面,
            // 绝不能拖累粘贴本身。
            //
            // 空相对路径(dir 就是项目根)必须排除 —— 那会在仓库根写下一个内容为
            // `*` 的 .gitignore,把用户整个仓库忽略掉。
            if posix_relative(project_path, &dir).is_some_and(|rel| !rel.is_empty()) {
                let _ = sftp
                    .write_new_file(&join_posix(&dir, ".gitignore"), b"*\n")
                    .await;
            }

            let remote_path = join_posix(&dir, &file_name);
            mt_ssh::run_sftp_upload_on_session(
                &session,
                local_path,
                &remote_path,
                PASTE_UPLOAD_REQUEST_TIMEOUT,
            )
            .await
            .map_err(|e| format!("上传到远程失败: {}", e.message()))?;
            session.touch();
            Ok(remote_path)
        })
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "上传到远程超时({}s)",
                PASTE_UPLOAD_TOTAL_TIMEOUT.as_secs()
            ))
        });

        sftp.close().await;
        result
    })
}
