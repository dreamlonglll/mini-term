use std::collections::HashSet;
use std::sync::Arc;

use mt_config::SshConnection;
use mt_project::encoding::TextEncoding;
use mt_project::fs::FileContentResult;
use mt_ssh::SftpHandle;
use mt_ssh::sftp::{SftpBoundedFileRead, SftpFileReplaceResult};

use super::{
    REMOTE_DOCUMENT_MAX_BYTES, REMOTE_DOCUMENT_TOO_LARGE_SAVE_ERROR,
    canonical_remote_document_root, connection_fingerprint, join_posix, open_sftp,
    split_posix_leaf, state, validate_remote_document_file_against_root,
};

/// 上传/下载冲突的用户选择。一次批处理内对所有剩余冲突沿用同一策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileConflictStrategy {
    Skip,
    Overwrite,
    KeepBoth,
}

/// Opaque optimistic-concurrency token returned only for editable text files
/// (UTF-8, or a non-UTF-8 encoding that round-trips losslessly — see
/// `mt_project::encoding`). Callers retain it with the draft and must send it
/// back to [`save_file_content`]. Raw bytes stay private so UI code cannot
/// fabricate a baseline for a binary, lossy or oversized file; the encoding
/// rides along so a save writes the draft back in the file's own encoding.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteFileBaseline {
    pub(super) connection_id: String,
    pub(super) connection_fingerprint: u64,
    pub(super) canonical_root: String,
    pub(super) canonical_path: String,
    pub(super) bytes: Arc<[u8]>,
    pub(super) encoding: TextEncoding,
}

impl std::fmt::Debug for RemoteFileBaseline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteFileBaseline")
            .field("connection_id", &self.connection_id)
            .field("byte_len", &self.bytes.len())
            .field("encoding", &self.encoding)
            .finish()
    }
}

impl RemoteFileBaseline {
    /// Number of raw remote bytes represented by this baseline.
    #[cfg(test)]
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }
}

/// Result of a bounded remote document read. `baseline` is present only when
/// `content` is editable text (UTF-8 or a losslessly round-tripping legacy /
/// UTF-16 encoding).
#[derive(Clone)]
pub struct RemoteFileReadResult {
    pub content: FileContentResult,
    pub baseline: Option<RemoteFileBaseline>,
}

impl std::fmt::Debug for RemoteFileReadResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteFileReadResult")
            .field("content_len", &self.content.content.len())
            .field("is_binary", &self.content.is_binary)
            .field("too_large", &self.content.too_large)
            .field("has_baseline", &self.baseline.is_some())
            .finish()
    }
}

/// A normal save either commits and returns the next baseline or reports the
/// current remote value without modifying it. The caller may reload `current`
/// or explicitly retry [`save_file_content`] with `force = true`.
#[derive(Debug, Clone)]
pub enum RemoteFileSaveResult {
    Saved {
        baseline: RemoteFileBaseline,
        warning: Option<String>,
    },
    ExternalChange {
        current: RemoteFileReadResult,
    },
}

pub(super) fn build_remote_file_read_result(
    conn: &SshConnection,
    canonical_root: String,
    canonical_path: String,
    read: SftpBoundedFileRead,
) -> RemoteFileReadResult {
    let fingerprint = connection_fingerprint(conn);
    match read {
        SftpBoundedFileRead::TooLarge => RemoteFileReadResult {
            content: FileContentResult::too_large(),
            baseline: None,
        },
        SftpBoundedFileRead::Complete(bytes) => {
            // 与本地文档同一把尺子(`mt_project::encoding`):UTF-8 原样,GBK / UTF-16 这类
            // 能逐字节写回的照样可编辑,解码有损的只读(没有基线 = 保存一律拒绝)
            let baseline_bytes: Arc<[u8]> = Arc::from(bytes.as_slice());
            let content = FileContentResult::from_bytes(bytes);
            let editable = !content.is_binary && content.encoding.writable();
            let baseline = editable.then(|| RemoteFileBaseline {
                connection_id: conn.id.clone(),
                connection_fingerprint: fingerprint,
                canonical_root,
                canonical_path,
                bytes: baseline_bytes,
                encoding: content.encoding,
            });
            RemoteFileReadResult { content, baseline }
        }
    }
}

pub(super) fn validate_remote_file_baseline_connection(
    conn: &SshConnection,
    baseline: &RemoteFileBaseline,
) -> Result<(), String> {
    if conn.id != baseline.connection_id {
        return Err("远程文件所属 SSH 连接已变化，请重新打开文件".into());
    }
    if connection_fingerprint(conn) != baseline.connection_fingerprint {
        return Err("SSH 连接配置已变化，请重新打开远程文件后再保存".into());
    }
    Ok(())
}

pub(super) fn validate_remote_file_baseline_path(
    baseline: &RemoteFileBaseline,
    canonical_root: &str,
    canonical_path: &str,
) -> Result<(), String> {
    if canonical_root != baseline.canonical_root {
        return Err("远程项目根已变化，请重新打开文件".into());
    }
    if canonical_path != baseline.canonical_path {
        return Err("远程文件路径身份已变化，请重新打开文件".into());
    }
    Ok(())
}

pub(super) fn should_block_remote_save(
    current: &SftpBoundedFileRead,
    expected: &RemoteFileBaseline,
    force: bool,
) -> bool {
    match current {
        // “仍然覆盖”只跳过内容相等比较，不得跳过目标文件大小上限。
        SftpBoundedFileRead::TooLarge => true,
        SftpBoundedFileRead::Complete(_) if force => false,
        SftpBoundedFileRead::Complete(_) => !current.matches_bytes(expected.bytes.as_ref()),
    }
}

async fn read_remote_file_with_sftp(
    conn: &SshConnection,
    sftp: &SftpHandle,
    project_root: &str,
    path: &str,
) -> Result<RemoteFileReadResult, String> {
    let canonical_root = canonical_remote_document_root(sftp, project_root).await?;
    let canonical_path =
        validate_remote_document_file_against_root(sftp, &canonical_root, path).await?;
    let read = sftp
        .read_file_bounded(&canonical_path, REMOTE_DOCUMENT_MAX_BYTES)
        .await
        .map_err(|error| format!("读取远程文件失败: {}", error.message()))?;

    let root_after_read = canonical_remote_document_root(sftp, project_root).await?;
    let path_after_read =
        validate_remote_document_file_against_root(sftp, &root_after_read, path).await?;
    if root_after_read != canonical_root || path_after_read != canonical_path {
        return Err("远程文件路径在读取期间发生变化，请重试".into());
    }

    Ok(build_remote_file_read_result(
        conn,
        canonical_root,
        canonical_path,
        read,
    ))
}

/// Read one remote editor document with the same 1 MiB text/binary/oversize
/// contract as `mt_project::fs::read_file_content`.
///
/// This is a synchronous service boundary and must run on GPUI's background
/// executor. It opens/reuses the pooled SSH session, canonicalizes the project
/// root and parent, rejects symlink/special leaves, and reads at most 1 MiB + 1
/// byte.
pub fn read_file_content(
    conn: &SshConnection,
    project_root: &str,
    path: &str,
) -> Result<RemoteFileReadResult, String> {
    let st = state();
    st.block_on(async {
        let sftp = open_sftp(st, conn).await?;
        read_remote_file_with_sftp(conn, &sftp, project_root, path).await
    })
}

/// Safely save one previously loaded remote text document, written back in
/// the encoding recorded in `expected` (UTF-8 unchanged; a draft the encoding
/// cannot represent is refused before touching the remote file).
///
/// A normal save (`force = false`) re-reads the bounded remote contents and
/// returns [`RemoteFileSaveResult::ExternalChange`] instead of writing when the
/// byte baseline changed. `force = true` skips only that byte comparison; it
/// still repeats connection, canonical-root, canonical-leaf, regular-file, and
/// size validation before a same-directory staged backup-swap.
pub fn save_file_content(
    conn: &SshConnection,
    project_root: &str,
    path: &str,
    content: &str,
    expected: &RemoteFileBaseline,
    force: bool,
) -> Result<RemoteFileSaveResult, String> {
    if content.len() > REMOTE_DOCUMENT_MAX_BYTES {
        return Err("内容过大(>1MB)，拒绝写入远程文件".into());
    }
    if expected.bytes.len() > REMOTE_DOCUMENT_MAX_BYTES {
        return Err("远程文件基线无效，请重新打开文件".into());
    }
    // 按打开时的编码写回(UTF-8 原样)。编码表示不了的字符在连远端之前就拒绝,
    // 远程文件一字不动
    let encoded = expected
        .encoding
        .encode(content)
        .map_err(|error| error.to_string())?;
    if encoded.len() > REMOTE_DOCUMENT_MAX_BYTES {
        return Err("内容过大(>1MB)，拒绝写入远程文件".into());
    }
    validate_remote_file_baseline_connection(conn, expected)?;

    let st = state();
    st.block_on(async {
        let sftp = open_sftp(st, conn).await?;
        let canonical_root = canonical_remote_document_root(&sftp, project_root).await?;
        let canonical_path =
            validate_remote_document_file_against_root(&sftp, &canonical_root, path).await?;
        validate_remote_file_baseline_path(expected, &canonical_root, &canonical_path)?;

        let current = sftp
            .read_file_bounded(&canonical_path, REMOTE_DOCUMENT_MAX_BYTES)
            .await
            .map_err(|error| format!("保存前读取远程文件失败: {}", error.message()))?;

        let root_after_read = canonical_remote_document_root(&sftp, project_root).await?;
        let path_after_read =
            validate_remote_document_file_against_root(&sftp, &root_after_read, path).await?;
        validate_remote_file_baseline_path(expected, &root_after_read, &path_after_read)?;

        if force && matches!(&current, SftpBoundedFileRead::TooLarge) {
            return Err(REMOTE_DOCUMENT_TOO_LARGE_SAVE_ERROR.into());
        }
        if should_block_remote_save(&current, expected, force) {
            return Ok(RemoteFileSaveResult::ExternalChange {
                current: build_remote_file_read_result(
                    conn,
                    root_after_read,
                    path_after_read,
                    current,
                ),
            });
        }

        // The optimistic content comparison above is not a transaction. Repeat
        // identity/type validation immediately before constructing and
        // promoting the staging file so a changed parent or leaf never inherits
        // the earlier check.
        let commit_root = canonical_remote_document_root(&sftp, project_root).await?;
        let commit_path =
            validate_remote_document_file_against_root(&sftp, &commit_root, path).await?;
        validate_remote_file_baseline_path(expected, &commit_root, &commit_path)?;
        let expected_at_commit = (!force).then_some(expected.bytes.as_ref());
        let replace_result = sftp
            .replace_file_contents(
                &commit_path,
                &encoded,
                REMOTE_DOCUMENT_MAX_BYTES,
                expected_at_commit,
            )
            .await
            .map_err(|error| format!("保存远程文件失败: {}", error.message()))?;
        let cleanup_warning = match replace_result {
            SftpFileReplaceResult::ExternalChange(current) => {
                if force && matches!(&current, SftpBoundedFileRead::TooLarge) {
                    return Err(REMOTE_DOCUMENT_TOO_LARGE_SAVE_ERROR.into());
                }
                let root_after_staging =
                    canonical_remote_document_root(&sftp, project_root).await?;
                let path_after_staging =
                    validate_remote_document_file_against_root(&sftp, &root_after_staging, path)
                        .await?;
                validate_remote_file_baseline_path(
                    expected,
                    &root_after_staging,
                    &path_after_staging,
                )?;
                return Ok(RemoteFileSaveResult::ExternalChange {
                    current: build_remote_file_read_result(
                        conn,
                        root_after_staging,
                        path_after_staging,
                        current,
                    ),
                });
            }
            SftpFileReplaceResult::Replaced { cleanup_warning } => cleanup_warning,
        };

        Ok(RemoteFileSaveResult::Saved {
            baseline: RemoteFileBaseline {
                connection_id: conn.id.clone(),
                connection_fingerprint: connection_fingerprint(conn),
                canonical_root: commit_root,
                canonical_path: commit_path,
                bytes: Arc::from(encoded.as_slice()),
                encoding: expected.encoding,
            },
            warning: cleanup_warning,
        })
    })
}

/// VS Code 风格的同名副本名。目录与文件共用，文件保留最后一个扩展名。
pub fn keep_both_name(name: &str, ordinal: usize) -> String {
    let suffix = if ordinal <= 1 {
        " copy".to_string()
    } else {
        format!(" copy {ordinal}")
    };
    if name.starts_with('.') && !name[1..].contains('.') {
        return format!("{name}{suffix}");
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => {
            format!("{stem}{suffix}.{ext}")
        }
        _ => format!("{name}{suffix}"),
    }
}

pub(super) async fn keep_both_remote_path(
    sftp: &SftpHandle,
    desired: &str,
) -> Result<String, String> {
    let (parent, name) = split_posix_leaf(desired)?;
    let existing: HashSet<String> = sftp
        .read_dir(parent)
        .await
        .map_err(|e| format!("读取远程目录失败: {}", e.message()))?
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    if !existing.contains(name) {
        return Ok(desired.to_string());
    }
    for ordinal in 1..=10_000 {
        let candidate = keep_both_name(name, ordinal);
        if !existing.contains(&candidate) {
            return Ok(join_posix(parent, &candidate));
        }
    }
    Err(format!("无法为远程条目生成可用副本名: {desired}"))
}
