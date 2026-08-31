use mt_ssh::{SftpHandle, SftpNodeKind};

// ---------------------------------------------------------------------------
// POSIX 路径纯函数(单测覆盖)
// ---------------------------------------------------------------------------

/// POSIX 路径拼接。`dir` 为绝对路径;根目录 `/` 不产生双斜杠。
pub fn join_posix(dir: &str, name: &str) -> String {
    let d = dir.trim_end_matches('/');
    if d.is_empty() {
        format!("/{name}")
    } else {
        format!("{d}/{name}")
    }
}

/// POSIX 路径父目录；不使用宿主平台 `Path`，因此远端文件名里的反斜杠不会在
/// Windows 客户端上被误当成分隔符。
pub fn parent_posix(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return None;
    }
    let index = trimmed.rfind('/')?;
    Some(if index == 0 {
        "/".into()
    } else {
        trimmed[..index].to_string()
    })
}

/// 计算 `full` 相对 `root` 的 POSIX 相对路径。不在 root 下返回 None。
/// **匹配 gitignore 必须用相对路径**:Windows 的 `Path` 语义对 POSIX 绝对路径
/// 有歧义(`/a/b` 在 Windows 上不是绝对路径),相对路径两平台行为一致。
pub fn posix_relative(root: &str, full: &str) -> Option<String> {
    let root_t = root.trim_end_matches('/');
    let full_t = full.trim_end_matches('/');
    if root_t.is_empty() {
        // root 是 `/`
        return Some(full_t.trim_start_matches('/').to_string());
    }
    if full_t == root_t {
        return Some(String::new());
    }
    full_t
        .strip_prefix(root_t)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(str::to_string)
}

pub(super) fn valid_remote_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains(':')
        && !name.contains('\0')
}

pub(super) fn split_posix_leaf(path: &str) -> Result<(&str, &str), String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return Err("远程根目录不能作为文件条目操作".into());
    }
    let index = trimmed
        .rfind('/')
        .ok_or_else(|| format!("远程路径必须是绝对路径: {path}"))?;
    let parent = if index == 0 { "/" } else { &trimmed[..index] };
    let name = &trimmed[index + 1..];
    if !valid_remote_name(name) {
        return Err(format!("远程文件名无效: {name}"));
    }
    Ok((parent, name))
}

pub(super) fn normalize_absolute_posix(path: &str) -> Result<String, String> {
    if !path.starts_with('/') {
        return Err(format!("远程路径必须是绝对路径: {path}"));
    }
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => return Err(format!("远程路径不能包含 `..`: {path}")),
            value if value.contains('\0') => return Err("远程路径不能包含 NUL".into()),
            value => segments.push(value),
        }
    }
    if segments.is_empty() {
        Ok("/".into())
    } else {
        Ok(format!("/{}", segments.join("/")))
    }
}

pub(super) async fn canonical_project_root(
    sftp: &SftpHandle,
    project_root: &str,
) -> Result<String, String> {
    let normalized = normalize_absolute_posix(project_root)?;
    sftp.canonicalize(&normalized)
        .await
        .map_err(|e| format!("远程项目根不可访问: {}", e.message()))
}

pub(super) async fn validate_remote_dir_under_root(
    sftp: &SftpHandle,
    project_root: &str,
    dir: &str,
) -> Result<String, String> {
    let root = canonical_project_root(sftp, project_root).await?;
    let normalized = normalize_absolute_posix(dir)?;
    let canonical = sftp
        .canonicalize(&normalized)
        .await
        .map_err(|e| format!("远程目录不可访问: {}", e.message()))?;
    if posix_relative(&root, &canonical).is_none() {
        return Err(format!("远程目录超出项目范围: {canonical}"));
    }
    let is_dir = sftp
        .is_dir(&canonical)
        .await
        .map_err(|e| format!("远程目录不可访问: {}", e.message()))?;
    if !is_dir {
        return Err(format!("远程路径不是目录: {canonical}"));
    }
    Ok(canonical)
}

pub(super) async fn validate_remote_leaf_under_root(
    sftp: &SftpHandle,
    project_root: &str,
    path: &str,
) -> Result<String, String> {
    let root = canonical_project_root(sftp, project_root).await?;
    validate_remote_leaf_against_root(sftp, &root, path).await
}

pub(super) async fn validate_remote_leaf_against_root(
    sftp: &SftpHandle,
    canonical_root: &str,
    path: &str,
) -> Result<String, String> {
    let normalized = normalize_absolute_posix(path)?;
    if normalized == canonical_root {
        return Err("不能操作远程项目根目录".into());
    }
    let (parent, name) = split_posix_leaf(&normalized)?;
    let canonical_parent = sftp
        .canonicalize(parent)
        .await
        .map_err(|e| format!("远程父目录不可访问: {}", e.message()))?;
    if posix_relative(canonical_root, &canonical_parent).is_none() {
        return Err(format!("远程路径超出项目范围: {normalized}"));
    }
    Ok(join_posix(&canonical_parent, name))
}

pub(super) async fn canonical_remote_document_root(
    sftp: &SftpHandle,
    project_root: &str,
) -> Result<String, String> {
    let canonical_root = canonical_project_root(sftp, project_root).await?;
    match sftp
        .node_kind(&canonical_root)
        .await
        .map_err(|error| format!("远程项目根不可访问: {}", error.message()))?
    {
        SftpNodeKind::Directory => Ok(canonical_root),
        _ => Err(format!("远程项目根不是目录: {canonical_root}")),
    }
}

pub(super) async fn validate_remote_document_file_against_root(
    sftp: &SftpHandle,
    canonical_root: &str,
    path: &str,
) -> Result<String, String> {
    let target = validate_remote_leaf_against_root(sftp, canonical_root, path).await?;
    sftp.guard_file_replacement_state(&target)
        .await
        .map_err(|error| format!("远程文件存在未决的保存恢复状态: {}", error.message()))?;
    match sftp
        .node_kind(&target)
        .await
        .map_err(|error| format!("远程文件不可访问: {}", error.message()))?
    {
        SftpNodeKind::File => Ok(target),
        SftpNodeKind::Directory => Err(format!("远程路径不是文件: {target}")),
        SftpNodeKind::Symlink => Err(format!("远程文件不能是符号链接: {target}")),
        SftpNodeKind::Other => Err(format!("远程路径不是普通文件: {target}")),
    }
}

/// 把 `~` / `~/xxx` 展开为远程绝对路径(home 来自 SFTP canonicalize(".")).
/// 空输入视同 `~`;非 `~` 前缀原样返回(交给 SFTP canonicalize 处理相对路径)。
pub(super) fn expand_tilde(path: &str, home: &str) -> String {
    let home_t = home.trim_end_matches('/');
    let home_norm = if home_t.is_empty() { "/" } else { home_t };
    let p = path.trim();
    if p.is_empty() || p == "~" {
        return home_norm.to_string();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        let rest = rest.trim_start_matches('/');
        if rest.is_empty() {
            return home_norm.to_string();
        }
        return join_posix(home_norm, rest);
    }
    p.to_string()
}
