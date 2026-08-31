use std::sync::Arc;
use std::sync::atomic::Ordering;

use mt_config::SshConnection;
use mt_ssh::{CachedSession, SftpHandle, SftpNodeKind, run_bounded_exec_on_session};

use super::{
    LOCAL_TRANSFER_SEQUENCE, REMOTE_DELETE_EXEC_TIMEOUT, REMOTE_DELETE_OUTPUT_CAP,
    REMOTE_DELETE_PROBE_TIMEOUT, REMOTE_DELETE_SERVER_TIMEOUT_SECS, RemoteSshState,
    canonical_project_root, join_posix, normalize_absolute_posix, open_sftp,
    open_sftp_with_session, posix_relative, state, validate_remote_leaf_against_root,
};

pub(super) fn valid_sftp_child_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

fn split_sftp_leaf(path: &str) -> Result<(&str, &str), String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return Err("远程根目录不能作为文件条目操作".into());
    }
    let index = trimmed
        .rfind('/')
        .ok_or_else(|| format!("远程路径必须是绝对路径: {path}"))?;
    let parent = if index == 0 { "/" } else { &trimmed[..index] };
    let name = &trimmed[index + 1..];
    if !valid_sftp_child_name(name) {
        return Err(format!("服务器返回了无效目录项名: {name:?}"));
    }
    Ok((parent, name))
}

async fn remote_kind_if_present(
    sftp: &SftpHandle,
    path: &str,
) -> Result<Option<SftpNodeKind>, String> {
    split_sftp_leaf(path)?;
    sftp.try_node_kind(path)
        .await
        .map_err(|e| format!("读取远程条目类型失败: {}", e.message()))
}

async fn validate_remote_delete_leaf_against_root(
    sftp: &SftpHandle,
    canonical_root: &str,
    path: &str,
) -> Result<String, String> {
    let normalized = normalize_absolute_posix(path)?;
    if normalized == canonical_root {
        return Err("不能操作远程项目根目录".into());
    }
    let (parent, name) = split_sftp_leaf(&normalized)?;
    let canonical_parent = sftp
        .canonicalize(parent)
        .await
        .map_err(|e| format!("远程父目录不可访问: {}", e.message()))?;
    if canonical_parent != parent {
        return Err(format!(
            "远程父目录在删除期间被符号链接替换或重定向: {parent}"
        ));
    }
    if posix_relative(canonical_root, &canonical_parent).is_none() {
        return Err(format!("远程路径超出项目范围: {normalized}"));
    }
    Ok(join_posix(&canonical_parent, name))
}

async fn validate_remote_delete_directory_identity(
    sftp: &SftpHandle,
    canonical_root: &str,
    path: &str,
) -> Result<String, String> {
    let validated = validate_remote_delete_leaf_against_root(sftp, canonical_root, path).await?;
    let canonical = sftp
        .canonicalize(&validated)
        .await
        .map_err(|e| format!("远程目录不可访问: {}", e.message()))?;
    if canonical != validated || posix_relative(canonical_root, &canonical).is_none() {
        return Err(format!(
            "远程目录在删除期间被替换或移出项目范围: {validated}"
        ));
    }
    if remote_kind_if_present(sftp, &validated).await? != Some(SftpNodeKind::Directory) {
        return Err(format!("远程目录在删除期间发生变化: {validated}"));
    }
    Ok(validated)
}

pub(super) fn shell_quote_posix(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(super) fn remote_delete_command(
    target: &str,
    proof_path: &str,
    proof_nonce: &str,
) -> Result<String, String> {
    let (parent, name) = split_sftp_leaf(target)?;
    let (proof_parent, proof_name) = split_sftp_leaf(proof_path)?;
    if proof_parent != parent {
        return Err("远程删除验证标记必须与目标位于同一目录".into());
    }
    let relative = format!("./{name}");
    let proof_relative = format!("./{proof_name}");
    Ok(format!(
        "cd -P {} 2>/dev/null && [ \"$(pwd -P)\" = {} ] && \
         [ -d {} ] && [ ! -L {} ] && [ -f {} ] && [ ! -L {} ] && \
         [ \"$(cat -- {})\" = {} ] && rm -f -- {} && \
         exec timeout {} rm -rf -- {}",
        shell_quote_posix(parent),
        shell_quote_posix(parent),
        shell_quote_posix(&relative),
        shell_quote_posix(&relative),
        shell_quote_posix(&proof_relative),
        shell_quote_posix(&proof_relative),
        shell_quote_posix(&proof_relative),
        shell_quote_posix(proof_nonce),
        shell_quote_posix(&proof_relative),
        REMOTE_DELETE_SERVER_TIMEOUT_SECS,
        shell_quote_posix(&relative),
    ))
}

async fn create_remote_delete_proof(
    sftp: &SftpHandle,
    target: &str,
) -> Result<(String, String), String> {
    for _ in 0..16 {
        let proof_path = sftp.temporary_sibling_path(target, "delete-proof");
        let sequence = LOCAL_TRANSFER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let nonce = format!(
            "mt-delete-proof-{}-{timestamp}-{sequence}",
            std::process::id()
        );
        match sftp.write_new_file(&proof_path, nonce.as_bytes()).await {
            Ok(()) => return Ok((proof_path, nonce)),
            Err(error) => match sftp.try_node_kind(&proof_path).await {
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => {
                    return Err(format!("创建远程删除验证标记失败: {}", error.message()));
                }
            },
        }
    }
    Err("无法分配唯一的远程删除验证标记".into())
}

async fn cleanup_remote_delete_proof(sftp: &SftpHandle, proof_path: &str) -> Result<(), String> {
    match sftp
        .try_node_kind(proof_path)
        .await
        .map_err(|error| format!("检查远程删除验证标记失败: {}", error.message()))?
    {
        None => Ok(()),
        Some(SftpNodeKind::Directory) => Err(format!(
            "远程删除验证标记被替换为目录，已拒绝清理: {proof_path}"
        )),
        Some(_) => sftp
            .remove_file(proof_path)
            .await
            .map_err(|error| format!("清理远程删除验证标记失败: {}", error.message())),
    }
}

fn remote_exec_failure_detail(output: &mt_ssh::BoundedExecOutput) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let mut detail = if output.timed_out {
        "服务端删除命令超时，远端状态仍需确认".to_string()
    } else {
        match output.exit_code {
            Some(code) => format!("服务端删除命令退出码: {code}"),
            None => "服务端删除命令未返回退出码".to_string(),
        }
    };
    if !stderr.is_empty() {
        detail.push_str("; stderr: ");
        detail.push_str(stderr);
    }
    detail
}

async fn remove_remote_tree_safely(
    sftp: &SftpHandle,
    canonical_root: &str,
    target: &str,
) -> Result<usize, String> {
    enum RemoveWork {
        Visit(String),
        RemoveDirectory(String),
    }

    let mut stack = vec![RemoveWork::Visit(target.to_string())];
    let mut removed = 0usize;
    while let Some(work) = stack.pop() {
        match work {
            RemoveWork::Visit(path) => {
                let path =
                    validate_remote_delete_leaf_against_root(sftp, canonical_root, &path).await?;
                let Some(kind) = remote_kind_if_present(sftp, &path).await? else {
                    continue;
                };
                if kind == SftpNodeKind::Directory {
                    let path =
                        validate_remote_delete_directory_identity(sftp, canonical_root, &path)
                            .await?;
                    let entries = sftp
                        .read_dir(&path)
                        .await
                        .map_err(|e| format!("读取远程目录失败: {}", e.message()))?;
                    stack.push(RemoveWork::RemoveDirectory(path.clone()));
                    for entry in entries.into_iter().rev() {
                        if !valid_sftp_child_name(&entry.name) {
                            return Err(format!("服务器返回了无效目录项名: {:?}", entry.name));
                        }
                        stack.push(RemoveWork::Visit(join_posix(&path, &entry.name)));
                    }
                } else {
                    sftp.remove_file(&path)
                        .await
                        .map_err(|e| format!("删除远程条目失败: {}", e.message()))?;
                    removed += 1;
                }
            }
            RemoveWork::RemoveDirectory(path) => {
                let path =
                    validate_remote_delete_leaf_against_root(sftp, canonical_root, &path).await?;
                let Some(kind) = remote_kind_if_present(sftp, &path).await? else {
                    continue;
                };
                if kind == SftpNodeKind::Directory {
                    validate_remote_delete_directory_identity(sftp, canonical_root, &path).await?;
                    sftp.remove_dir(&path)
                        .await
                        .map_err(|e| format!("删除远程目录失败: {}", e.message()))?;
                } else {
                    sftp.remove_file(&path)
                        .await
                        .map_err(|e| format!("删除远程条目失败: {}", e.message()))?;
                }
                removed += 1;
            }
        }
    }
    Ok(removed)
}

async fn restore_isolated_remote_entry(
    sftp: &SftpHandle,
    isolation: &str,
    target: &str,
) -> Result<(), String> {
    if remote_kind_if_present(sftp, isolation).await?.is_none() {
        return Ok(());
    }
    if remote_kind_if_present(sftp, target).await?.is_some() {
        return Err(format!(
            "原路径已被重新创建，未覆盖；剩余条目保留在: {isolation}"
        ));
    }
    sftp.rename(isolation, target).await.map_err(|error| {
        format!(
            "恢复远程条目失败: {}; 剩余条目保留在: {isolation}",
            error.message()
        )
    })
}

async fn remove_remote_leaf_via_isolation(
    sftp: &SftpHandle,
    canonical_root: &str,
    target: &str,
) -> Result<usize, String> {
    let target = validate_remote_delete_leaf_against_root(sftp, canonical_root, target).await?;
    let isolation = loop {
        let candidate = sftp.temporary_sibling_path(&target, "delete-isolation");
        if remote_kind_if_present(sftp, &candidate).await?.is_none() {
            break candidate;
        }
    };
    sftp.rename(&target, &isolation)
        .await
        .map_err(|error| format!("隔离远程待删除条目失败: {}", error.message()))?;

    let isolated =
        validate_remote_delete_leaf_against_root(sftp, canonical_root, &isolation).await?;
    match remote_kind_if_present(sftp, &isolated).await? {
        Some(SftpNodeKind::Directory) => {
            let restore = restore_isolated_remote_entry(sftp, &isolated, &target).await;
            match restore {
                Ok(()) => Err("远程条目在删除期间变成了目录，已恢复原路径".into()),
                Err(restore_error) => Err(format!("远程条目在删除期间变成了目录；{restore_error}")),
            }
        }
        Some(_) => {
            if let Err(error) = sftp.remove_file(&isolated).await {
                let restore = restore_isolated_remote_entry(sftp, &isolated, &target).await;
                return match restore {
                    Ok(()) => Err(format!("删除远程条目失败: {}", error.message())),
                    Err(restore_error) => Err(format!(
                        "删除远程条目失败: {}; {restore_error}",
                        error.message()
                    )),
                };
            }
            Ok(1)
        }
        None => Ok(1),
    }
}

async fn remove_remote_directory_via_isolation(
    sftp: &SftpHandle,
    canonical_root: &str,
    target: &str,
) -> Result<usize, String> {
    let target = validate_remote_delete_directory_identity(sftp, canonical_root, target).await?;
    let isolation = loop {
        let candidate = sftp.temporary_sibling_path(&target, "delete-isolation");
        if remote_kind_if_present(sftp, &candidate).await?.is_none() {
            break candidate;
        }
    };
    sftp.rename(&target, &isolation)
        .await
        .map_err(|error| format!("隔离远程待删除目录失败: {}", error.message()))?;

    if let Err(error) =
        validate_remote_delete_directory_identity(sftp, canonical_root, &isolation).await
    {
        let restore = restore_isolated_remote_entry(sftp, &isolation, &target).await;
        return match restore {
            Ok(()) => Err(format!("隔离后的远程目录校验失败: {error}")),
            Err(restore_error) => Err(format!(
                "隔离后的远程目录校验失败: {error}; {restore_error}"
            )),
        };
    }

    match remove_remote_tree_safely(sftp, canonical_root, &isolation).await {
        Ok(removed) => Ok(removed),
        Err(error) => {
            let restore = restore_isolated_remote_entry(sftp, &isolation, &target).await;
            match restore {
                Ok(()) => Err(error),
                Err(restore_error) => Err(format!("{error}; {restore_error}")),
            }
        }
    }
}

async fn remove_remote_directory_via_fresh_session(
    st: &RemoteSshState,
    conn: &SshConnection,
    project_root: &str,
    target: &str,
) -> Result<usize, String> {
    let fresh_sftp = open_sftp(st, conn).await?;
    let result = async {
        let canonical_root = canonical_project_root(&fresh_sftp, project_root).await?;
        remove_remote_directory_via_isolation(&fresh_sftp, &canonical_root, target).await
    }
    .await;
    fresh_sftp.close().await;
    result
}

async fn delete_remote_directory(
    st: &RemoteSshState,
    conn: &SshConnection,
    project_root: &str,
    session: &Arc<CachedSession>,
    sftp: &SftpHandle,
    canonical_root: &str,
    target: &str,
) -> Result<usize, String> {
    // 只绑定并验证删除根目录。服务端 `rm` 自己完成递归；若先用 SFTP 扫描整棵树，
    // 大目录仍会因网络传输和目录往返退化为线性预处理，抵消快速路径的意义。
    let target = validate_remote_delete_directory_identity(sftp, canonical_root, target).await?;
    let capability = run_bounded_exec_on_session(
        session,
        "command -v timeout >/dev/null 2>&1 && command -v rm >/dev/null 2>&1 && \
         command -v cat >/dev/null 2>&1",
        REMOTE_DELETE_PROBE_TIMEOUT,
        1024,
    )
    .await;
    match capability {
        Ok(output)
            if !output.requires_session_retirement()
                && !output.timed_out
                && output.exit_code == Some(0) => {}
        Ok(output) if output.requires_session_retirement() => {
            st.pool().evict_if_same(&conn.id, session).await;
            return remove_remote_directory_via_fresh_session(st, conn, project_root, &target)
                .await;
        }
        Ok(_) => {
            return remove_remote_directory_via_isolation(sftp, canonical_root, &target).await;
        }
        Err(_) => {
            st.pool().evict_if_same(&conn.id, session).await;
            return remove_remote_directory_via_fresh_session(st, conn, project_root, &target)
                .await;
        }
    }

    let (proof_path, proof_nonce) = match create_remote_delete_proof(sftp, &target).await {
        Ok(proof) => proof,
        Err(_) => {
            return remove_remote_directory_via_isolation(sftp, canonical_root, &target).await;
        }
    };
    let command = remote_delete_command(&target, &proof_path, &proof_nonce)?;
    let execution = run_bounded_exec_on_session(
        session,
        &command,
        REMOTE_DELETE_EXEC_TIMEOUT,
        REMOTE_DELETE_OUTPUT_CAP,
    )
    .await;
    match &execution {
        Ok(output) if output.requires_session_retirement() => {
            st.pool().evict_if_same(&conn.id, session).await;
        }
        Err(_) => {
            st.pool().evict_if_same(&conn.id, session).await;
        }
        _ => {}
    }
    let proof_cleanup = cleanup_remote_delete_proof(sftp, &proof_path).await;
    let post_target =
        validate_remote_delete_leaf_against_root(sftp, canonical_root, &target).await?;
    if remote_kind_if_present(sftp, &post_target).await?.is_none() {
        if let Err(cleanup_error) = proof_cleanup {
            return Err(format!("远程目录已删除，但{cleanup_error}"));
        }
        // 调用方只关心成功与否；快速路径不为统计条目重新扫描整棵树。
        return Ok(1);
    }

    match execution {
        Ok(output) if output.safe_to_fallback() => {
            proof_cleanup?;
            remove_remote_directory_via_isolation(sftp, canonical_root, &target).await
        }
        Ok(output) if output.requires_session_retirement() && !output.state.may_have_started() => {
            proof_cleanup?;
            remove_remote_directory_via_fresh_session(st, conn, project_root, &target).await
        }
        Ok(output) => {
            let cleanup = proof_cleanup
                .err()
                .map(|error| format!("；{error}"))
                .unwrap_or_default();
            Err(format!(
                "{}；为避免与仍可能运行的服务端删除并发，未启动 SFTP 回退{cleanup}",
                remote_exec_failure_detail(&output)
            ))
        }
        Err(error) => {
            proof_cleanup?;
            remove_remote_directory_via_fresh_session(st, conn, project_root, &target)
                .await
                .map_err(|fallback_error| {
                    format!("服务端删除通道失败: {error}; SFTP 回退也失败: {fallback_error}")
                })
        }
    }
}

/// 删除远程文件、符号链接或目录。普通目录先校验删除根并用 SFTP nonce 证明 shell
/// 与 SFTP 看见同一父目录，再优先使用带 `timeout` 的服务端 `rm`；能力不可用时先
/// 原子改名到随机隔离路径，再用一个复用 SFTP handle 后序删除。叶子 symlink 只删除
/// 链接自身，路径式 fallback 的每一步仍会重新校验 canonical parent。
pub fn delete_entry(conn: &SshConnection, project_root: &str, path: &str) -> Result<usize, String> {
    let st = state();
    st.block_on(async move {
        let (session, sftp) = open_sftp_with_session(st, conn).await?;
        let result = async {
            let canonical_root = canonical_project_root(&sftp, project_root).await?;
            let target = validate_remote_leaf_against_root(&sftp, &canonical_root, path).await?;
            let kind = remote_kind_if_present(&sftp, &target)
                .await?
                .ok_or_else(|| format!("远程条目不存在: {target}"))?;
            if kind == SftpNodeKind::Directory {
                delete_remote_directory(
                    st,
                    conn,
                    project_root,
                    &session,
                    &sftp,
                    &canonical_root,
                    &target,
                )
                .await
            } else {
                remove_remote_leaf_via_isolation(&sftp, &canonical_root, &target).await
            }
        }
        .await;
        sftp.close().await;
        result
    })
}
