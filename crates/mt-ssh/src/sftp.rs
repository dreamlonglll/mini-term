//! SFTP 只读原语(task 07-05-ssh-remote-projects PR2)。
//!
//! 在池里一条已认证的 [`CachedSession`] 上开一个 SFTP channel,提供主程序
//! 「远程项目」需要的只读操作:readdir / stat / canonicalize / 分块读文件。
//! 与 `pool.rs` 的 upload/download 不同,这里把 [`SftpHandle`] 作为**可复用句柄**
//! 返回给调用方 —— 一次远程会话扫描要做几十次 readdir/read,逐操作开 channel
//! 的往返开销不可接受。
//!
//! 锁语义:只在 `channel_open_session` 期间短暂持有 session 锁,channel 建成后
//! (`channel.into_stream()` 拿到独立流)立刻释放 —— russh 的 `Handle` 支持并发
//! channel,SFTP 长扫描不应阻塞同一连接上的其它操作(对齐
//! spec/backend/wsl-unc-session-scanning.md「缓存锁不得跨慢 IO」的精神)。
//!
//! 超时:构造时必须把协议层每请求超时(`SftpSession::set_timeout`,默认仅 10s)
//! 同步到调用方给的窗口,见 spec/backend/russh-sftp-file-transfer.md 坑 1。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::pool::{CachedSession, SftpTransferError};

/// 只读路径的分块缓冲。比 upload/download 的 8KB 大:russh-sftp 的 `File`
/// 会按服务器通告的 max read 长度(OpenSSH 通常 64KB)切请求,大缓冲能减少
/// 「读一个文件头」场景的网络往返数;内存占用仍是常数。
const SFTP_READ_CHUNK_BYTES: usize = 32 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 一条 readdir 结果。只保留远程文件树 / 会话扫描需要的最小字段。
#[derive(Debug, Clone)]
pub struct SftpDirEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    /// 修改时间(UNIX 秒)。SFTP v3 属性可缺省。
    pub mtime_secs: Option<u64>,
}

/// `lstat` 等价的条目类型。实现刻意通过父目录 `readdir` 获取类型，避免
/// `metadata/stat` 跟随叶子 symlink 后让删除/递归越过调用方的项目边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SftpNodeKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// A bounded full-file read. [`TooLarge`](Self::TooLarge) means the reader
/// observed at least `max_bytes + 1` bytes without retaining the rest of the
/// remote file in memory.
#[derive(Clone, PartialEq, Eq)]
pub enum SftpBoundedFileRead {
    Complete(Vec<u8>),
    TooLarge,
}

impl std::fmt::Debug for SftpBoundedFileRead {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Complete(bytes) => formatter
                .debug_struct("Complete")
                .field("byte_len", &bytes.len())
                .finish(),
            Self::TooLarge => formatter.write_str("TooLarge"),
        }
    }
}

impl SftpBoundedFileRead {
    /// Whether this complete bounded read is byte-for-byte equal to a saved
    /// baseline. Oversized files can never match an editable baseline.
    pub fn matches_bytes(&self, expected: &[u8]) -> bool {
        matches!(self, Self::Complete(bytes) if bytes.as_slice() == expected)
    }
}

/// Outcome of a staged in-memory file replacement. A normal optimistic save
/// can discover a late external change after staging but before promotion.
#[derive(Clone, PartialEq, Eq)]
pub enum SftpFileReplaceResult {
    Replaced { cleanup_warning: Option<String> },
    ExternalChange(SftpBoundedFileRead),
}

impl std::fmt::Debug for SftpFileReplaceResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Replaced { cleanup_warning } => formatter
                .debug_struct("Replaced")
                .field("has_cleanup_warning", &cleanup_warning.is_some())
                .finish(),
            Self::ExternalChange(current) => formatter
                .debug_tuple("ExternalChange")
                .field(current)
                .finish(),
        }
    }
}

/// A failed promotion probe permits rollback only when both follow-up lstats
/// produced an unambiguous state. Transport errors are not evidence that the
/// target is absent: promotion may already have succeeded on the server.
fn can_restore_verified_backup(
    target_state: &Result<Option<SftpNodeKind>, SftpTransferError>,
    backup_state: &Result<Option<SftpNodeKind>, SftpTransferError>,
) -> bool {
    matches!(target_state, Ok(None)) && matches!(backup_state, Ok(Some(SftpNodeKind::File)))
}

/// A failed promotion reply is accepted only when the staging name disappeared
/// and the verified regular-file backup still exists. Matching target bytes
/// alone are insufficient because another writer can create identical content.
fn can_accept_verified_promotion(
    staging_state: &Result<Option<SftpNodeKind>, SftpTransferError>,
    backup_state: &Result<Option<SftpNodeKind>, SftpTransferError>,
) -> bool {
    matches!(staging_state, Ok(None)) && matches!(backup_state, Ok(Some(SftpNodeKind::File)))
}

fn stale_editor_backup_kind(
    target_kind: Option<SftpNodeKind>,
    backup_kind: Option<SftpNodeKind>,
) -> Option<SftpNodeKind> {
    match (target_kind, backup_kind) {
        (Some(SftpNodeKind::File), Some(kind)) => Some(kind),
        _ => None,
    }
}

fn verify_stale_editor_backup_cleanup(
    backup: &str,
    remove_error: Option<SftpTransferError>,
    backup_state: Result<Option<SftpNodeKind>, SftpTransferError>,
) -> Result<(), SftpTransferError> {
    match backup_state {
        Ok(None) => Ok(()),
        Ok(Some(kind)) => {
            let remove_detail = remove_error
                .as_ref()
                .map(|error| format!("; remove failed: {}", error.message()))
                .unwrap_or_default();
            Err(SftpTransferError::Sftp(format!(
                "stale remote editor backup cleanup did not remove '{backup}' (remaining type {kind:?}){remove_detail}"
            )))
        }
        Err(probe_error) => {
            let remove_detail = remove_error
                .as_ref()
                .map(|error| format!("remove failed: {}; ", error.message()))
                .unwrap_or_default();
            Err(SftpTransferError::Sftp(format!(
                "stale remote editor backup cleanup state is uncertain at '{backup}': {remove_detail}verification failed: {}",
                probe_error.message()
            )))
        }
    }
}

/// 打开在某条 session 上的 SFTP 会话句柄。可跨多次操作复用;用完调 [`Self::close`]
/// (或直接 drop,底层 channel 随之关闭,close 只是显式礼貌收尾)。
pub struct SftpHandle {
    sftp: SftpSession,
    _lease: SftpLease,
}

struct SftpLease(Arc<CachedSession>);

impl Drop for SftpLease {
    fn drop(&mut self) {
        self.0.release_sftp_lease();
    }
}

impl SftpHandle {
    /// 在已认证 session 上开 SFTP channel 并握手。
    ///
    /// 错误分类与 upload/download 一致:开 channel / subsystem / 握手失败都是
    /// `Transport`(caller 可 evict + 重连重试一次);后续各操作的失败是 `Sftp`
    /// 业务错(不 evict)。
    pub async fn open_on_session(
        session: Arc<CachedSession>,
        request_timeout: Duration,
    ) -> Result<Self, SftpTransferError> {
        session.acquire_sftp_lease();
        let lease = SftpLease(session.clone());
        // 只在开 channel 期间持锁;拿到独立 stream 后立刻释放。
        let channel = {
            let handle_guard = session.lock().await;
            let channel = handle_guard.channel_open_session().await.map_err(|e| {
                SftpTransferError::Transport(format!("channel_open_session failed: {e}"))
            })?;
            channel.request_subsystem(true, "sftp").await.map_err(|e| {
                SftpTransferError::Transport(format!("request_subsystem(sftp) failed: {e}"))
            })?;
            channel
        };
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| SftpTransferError::Transport(format!("sftp handshake failed: {e}")))?;
        // 协议层每请求超时默认 10s,必须同步到调用方窗口(下限 1s)。
        sftp.set_timeout(request_timeout.as_secs().max(1));
        Ok(Self {
            sftp,
            _lease: lease,
        })
    }

    /// 列目录。过滤 `.` / `..`;symlink 不解引用(`is_dir` 只反映条目自身类型)。
    pub async fn read_dir(&self, path: &str) -> Result<Vec<SftpDirEntry>, SftpTransferError> {
        let rd =
            self.sftp.read_dir(path).await.map_err(|e| {
                SftpTransferError::Sftp(format!("sftp readdir '{path}' failed: {e}"))
            })?;
        Ok(rd
            .filter(|entry| {
                let n = entry.file_name();
                n != "." && n != ".."
            })
            .map(|entry| {
                let file_type = entry.file_type();
                let meta = entry.metadata();
                SftpDirEntry {
                    name: entry.file_name(),
                    is_dir: file_type.is_dir(),
                    is_file: file_type.is_file(),
                    is_symlink: file_type.is_symlink(),
                    mtime_secs: meta.mtime.map(u64::from),
                }
            })
            .collect())
    }

    /// 规范化远程路径(SSH_FXP_REALPATH)。相对路径按 SFTP server 的初始 cwd
    /// (OpenSSH 为登录用户 home)解析 —— `canonicalize(".")` 即远程 `$HOME`。
    pub async fn canonicalize(&self, path: &str) -> Result<String, SftpTransferError> {
        self.sftp
            .canonicalize(path)
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp realpath '{path}' failed: {e}")))
    }

    /// stat 远程路径是否是目录(follow symlink)。路径不存在返回 `Err(Sftp)`。
    pub async fn is_dir(&self, path: &str) -> Result<bool, SftpTransferError> {
        let meta = self
            .sftp
            .metadata(path)
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp stat '{path}' failed: {e}")))?;
        Ok(meta.file_type().is_dir())
    }

    /// 远程路径是否存在(follow symlink)。IO 错误一律视为「不存在」交由上层降级。
    pub async fn exists(&self, path: &str) -> bool {
        self.sftp.try_exists(path).await.unwrap_or(false)
    }

    /// 不跟随叶子符号链接地查询条目类型。使用 `LSTAT` 而不是读取整个父目录，
    /// 避免对同一目录中的大量条目逐项检查时产生平方级响应数据。
    pub async fn try_node_kind(
        &self,
        path: &str,
    ) -> Result<Option<SftpNodeKind>, SftpTransferError> {
        use russh_sftp::client::error::Error as SftpClientError;
        use russh_sftp::protocol::StatusCode;

        let metadata = match self.sftp.symlink_metadata(path).await {
            Ok(metadata) => metadata,
            Err(SftpClientError::Status(status))
                if status.status_code == StatusCode::NoSuchFile =>
            {
                return Ok(None);
            }
            Err(error) => {
                return Err(SftpTransferError::Sftp(format!(
                    "sftp lstat '{path}' failed: {error}"
                )));
            }
        };
        let file_type = metadata.file_type();
        Ok(Some(if file_type.is_symlink() {
            SftpNodeKind::Symlink
        } else if file_type.is_dir() {
            SftpNodeKind::Directory
        } else if file_type.is_file() {
            SftpNodeKind::File
        } else {
            SftpNodeKind::Other
        }))
    }

    /// 与 [`Self::try_node_kind`] 相同，但不存在时返回明确错误。
    pub async fn node_kind(&self, path: &str) -> Result<SftpNodeKind, SftpTransferError> {
        self.try_node_kind(path)
            .await?
            .ok_or_else(|| SftpTransferError::Sftp(format!("远程路径不存在: '{path}'")))
    }

    /// 读文件头部:从 0 偏移最多读 `max_bytes`。用于 `.gitignore` / 会话文件
    /// 标题提取这类「只需要前若干 KB」的场景,绝不整文件进内存。
    pub async fn read_head(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, SftpTransferError> {
        self.read_from_offset(path, 0, max_bytes).await
    }

    /// 从字节偏移 `offset` 起最多读 `max_bytes`(增量读取会话正文用)。
    /// 返回读到的字节;不足 `max_bytes` 说明已到 EOF。
    pub async fn read_from_offset(
        &self,
        path: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<Vec<u8>, SftpTransferError> {
        let mut file = self
            .sftp
            .open(path)
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp open '{path}' failed: {e}")))?;
        if offset > 0 {
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| {
                    SftpTransferError::Sftp(format!("sftp seek '{path}'@{offset} failed: {e}"))
                })?;
        }
        let mut out: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; SFTP_READ_CHUNK_BYTES];
        while out.len() < max_bytes {
            let want = (max_bytes - out.len()).min(SFTP_READ_CHUNK_BYTES);
            let n = file
                .read(&mut buf[..want])
                .await
                .map_err(|e| SftpTransferError::Sftp(format!("sftp read '{path}' failed: {e}")))?;
            if n == 0 {
                break; // EOF
            }
            out.extend_from_slice(&buf[..n]);
        }
        Ok(out)
    }

    /// Read a complete regular file up to `max_bytes`, using one additional
    /// byte to distinguish an exact-limit file from an oversized one.
    ///
    /// The leaf is checked with `lstat` before and after the read so a symlink
    /// or special entry is never accepted as an editor document. Project-root
    /// containment remains the caller's responsibility because this crate
    /// intentionally has no project model.
    pub async fn read_file_bounded(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<SftpBoundedFileRead, SftpTransferError> {
        ensure_regular_file(self.node_kind(path).await?, path)?;
        let probe_bytes = max_bytes.checked_add(1).ok_or_else(|| {
            SftpTransferError::Sftp("bounded SFTP read limit is too large".into())
        })?;
        let bytes = self.read_from_offset(path, 0, probe_bytes).await?;
        ensure_regular_file(self.node_kind(path).await?, path)?;
        Ok(classify_bounded_file_bytes(bytes, max_bytes))
    }

    /// Refuse an ambiguous deterministic editor backup instead of restoring it
    /// automatically. Read-side validation uses this non-mutating guard;
    /// replacement performs stale-backup cleanup separately before staging.
    ///
    /// The caller must already have canonicalized and root-checked the parent.
    /// Recovery data is deliberately kept in place for explicit/manual action.
    pub async fn guard_file_replacement_state(
        &self,
        target: &str,
    ) -> Result<(), SftpTransferError> {
        let backup = editor_backup_path(target)?;
        let target_kind = self.try_node_kind(target).await?;
        let backup_kind = self.try_node_kind(&backup).await?;
        if let Some(kind) = ambiguous_editor_recovery_kind(target_kind, backup_kind) {
            Err(SftpTransferError::Sftp(format!(
                "remote editor recovery state is ambiguous: target '{target}' is missing and recovery data ({kind:?}) remains at '{backup}'; automatic restore was refused"
            )))
        } else {
            Ok(())
        }
    }

    /// Prepare the deterministic backup name for a new replacement. A regular
    /// target proves an existing regular backup is stale cleanup residue from a
    /// committed save; remove it and verify absence before creating staging.
    /// Missing/uncertain targets and unexpected backup types remain fail-closed.
    async fn prepare_file_replacement_state(&self, target: &str) -> Result<(), SftpTransferError> {
        let backup = editor_backup_path(target)?;
        let target_kind = self.try_node_kind(target).await?;
        let backup_kind = self.try_node_kind(&backup).await?;
        if let Some(kind) = ambiguous_editor_recovery_kind(target_kind, backup_kind) {
            return Err(SftpTransferError::Sftp(format!(
                "remote editor recovery state is ambiguous: target '{target}' is missing and recovery data ({kind:?}) remains at '{backup}'; automatic restore was refused"
            )));
        }
        let Some(stale_kind) = stale_editor_backup_kind(target_kind, backup_kind) else {
            return Ok(());
        };
        if stale_kind != SftpNodeKind::File {
            return Err(SftpTransferError::Sftp(format!(
                "remote editor backup has unexpected type {stale_kind:?} at '{backup}'; refusing to remove recovery data"
            )));
        }

        let remove_error = self.remove_file(&backup).await.err();
        let backup_state = self.try_node_kind(&backup).await;
        verify_stale_editor_backup_cleanup(&backup, remove_error, backup_state)
    }

    async fn regular_file_permissions(&self, path: &str) -> Result<u32, SftpTransferError> {
        let metadata = self.sftp.symlink_metadata(path).await.map_err(|error| {
            SftpTransferError::Sftp(format!("sftp lstat '{path}' failed: {error}"))
        })?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            SftpNodeKind::Symlink
        } else if file_type.is_dir() {
            SftpNodeKind::Directory
        } else if file_type.is_file() {
            SftpNodeKind::File
        } else {
            SftpNodeKind::Other
        };
        ensure_regular_file(kind, path)?;
        metadata.permissions.ok_or_else(|| {
            SftpTransferError::Sftp(format!(
                "remote server omitted permissions for editor target '{path}'"
            ))
        })
    }

    async fn set_file_permissions(
        &self,
        path: &str,
        permissions: u32,
    ) -> Result<(), SftpTransferError> {
        let mut attributes = russh_sftp::protocol::FileAttributes::empty();
        attributes.permissions = Some(permissions);
        self.sftp
            .set_metadata(path, attributes)
            .await
            .map_err(|error| {
                SftpTransferError::Sftp(format!(
                    "sftp preserve permissions on '{path}' failed: {error}"
                ))
            })
    }

    /// 逐级创建远程目录(`mkdir -p` 语义)。`path` 必须是 POSIX 绝对路径。
    ///
    /// SFTP 协议没有递归 mkdir,只能自顶向下逐级 `create_dir`。中间层已存在时
    /// server 回 FAILURE —— 这里一律忽略逐级错误,**成功与否只由最后的 stat 判定**
    /// (存在且是目录 = 成功),否则「目录已存在」会被误报成失败。
    ///
    /// 快路径:先 stat 整条路径,已是目录直接返回(重复粘贴只花 1 次往返)。
    pub async fn create_dir_all(&self, path: &str) -> Result<(), SftpTransferError> {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            return Ok(()); // 根目录必然存在
        }
        if !trimmed.starts_with('/') {
            return Err(SftpTransferError::Sftp(format!(
                "create_dir_all 需要绝对路径,收到 '{path}'"
            )));
        }
        // 快路径:已存在且是目录就不用逐级建。
        if let Ok(meta) = self.sftp.metadata(trimmed).await {
            return if meta.file_type().is_dir() {
                Ok(())
            } else {
                Err(SftpTransferError::Sftp(format!(
                    "远程路径 '{trimmed}' 已存在且不是目录"
                )))
            };
        }
        let mut prefix = String::new();
        for seg in trimmed.split('/').filter(|s| !s.is_empty()) {
            prefix.push('/');
            prefix.push_str(seg);
            // 已存在 / 无权限的层级都在这里失败,交给下方 stat 定论。
            let _ = self.sftp.create_dir(prefix.clone()).await;
        }
        match self.sftp.metadata(trimmed).await {
            Ok(meta) if meta.file_type().is_dir() => Ok(()),
            Ok(_) => Err(SftpTransferError::Sftp(format!(
                "远程路径 '{trimmed}' 已存在且不是目录"
            ))),
            Err(e) => Err(SftpTransferError::Sftp(format!(
                "创建远程目录 '{trimmed}' 失败: {e}"
            ))),
        }
    }

    /// 写一个**仅当不存在时才创建**的小文件(CREATE|EXCLUDE 语义)。
    ///
    /// 已存在时 server 回 FAILURE,调用方按「无需重写」处理即可 —— 这正是
    /// 幂等写标记文件(如自忽略的 `.gitignore`)想要的语义:一次往返,不用先 stat。
    ///
    /// 只用于小内容:全量 `write_all`,不分块。
    pub async fn write_new_file(
        &self,
        path: &str,
        contents: &[u8],
    ) -> Result<(), SftpTransferError> {
        use russh_sftp::protocol::OpenFlags;
        use tokio::io::AsyncWriteExt;

        let mut file = self
            .sftp
            .open_with_flags(
                path,
                OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::EXCLUDE,
            )
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp create '{path}' failed: {e}")))?;
        file.write_all(contents)
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp write '{path}' failed: {e}")))?;
        file.flush()
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp flush '{path}' failed: {e}")))?;
        file.shutdown()
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp close '{path}' failed: {e}")))?;
        Ok(())
    }

    /// 创建一个空文件，已存在时失败。
    pub async fn create_file(&self, path: &str) -> Result<(), SftpTransferError> {
        self.write_new_file(path, &[]).await
    }

    /// 创建单层目录，父目录必须已经存在。
    pub async fn create_dir(&self, path: &str) -> Result<(), SftpTransferError> {
        self.sftp
            .create_dir(path.to_string())
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp mkdir '{path}' failed: {e}")))
    }

    /// 删除文件或符号链接本身。
    pub async fn remove_file(&self, path: &str) -> Result<(), SftpTransferError> {
        self.sftp
            .remove_file(path)
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp remove file '{path}' failed: {e}")))
    }

    /// 删除空目录。
    pub async fn remove_dir(&self, path: &str) -> Result<(), SftpTransferError> {
        self.sftp
            .remove_dir(path)
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp remove dir '{path}' failed: {e}")))
    }

    /// 删除文件、符号链接、特殊条目或目录树。目录使用同一个 SFTP 会话做后序遍历，
    /// 不为每个子项重复握手；符号链接只删除链接本身。
    pub async fn remove_tree(
        &self,
        target: &str,
        target_kind: SftpNodeKind,
    ) -> Result<usize, SftpTransferError> {
        if target_kind != SftpNodeKind::Directory {
            self.remove_file(target).await?;
            return Ok(1);
        }

        // 先完整扫描/校验，再开始删除。这样服务器若返回异常目录项名，不会在发现
        // 错误前先删掉半棵树；请求量仍是每目录一次 readdir + 每条目一次删除。
        let mut scan = vec![target.to_string()];
        let mut directories = Vec::new();
        let mut leaves = Vec::new();
        while let Some(path) = scan.pop() {
            directories.push(path.clone());
            for entry in self.read_dir(&path).await? {
                if entry.name.is_empty()
                    || entry.name == "."
                    || entry.name == ".."
                    || entry.name.contains('/')
                    || entry.name.contains('\0')
                {
                    return Err(SftpTransferError::Sftp(format!(
                        "服务器返回了无效目录项名: {:?}",
                        entry.name
                    )));
                }
                let child = if path == "/" {
                    format!("/{}", entry.name)
                } else {
                    format!("{}/{}", path.trim_end_matches('/'), entry.name)
                };
                if entry.is_dir && !entry.is_symlink {
                    scan.push(child);
                } else {
                    leaves.push(child);
                }
            }
        }
        let mut removed = 0usize;
        for path in leaves {
            self.remove_file(&path).await?;
            removed += 1;
        }
        for path in directories.into_iter().rev() {
            self.remove_dir(&path).await?;
            removed += 1;
        }
        Ok(removed)
    }

    /// 同一远程文件系统内改名。目标已存在时由服务器返回冲突错误。
    pub async fn rename(&self, from: &str, to: &str) -> Result<(), SftpTransferError> {
        self.sftp.rename(from, to).await.map_err(|e| {
            SftpTransferError::Sftp(format!("sftp rename '{from}' -> '{to}' failed: {e}"))
        })
    }

    /// 生成同级、进程内唯一的隐藏暂存路径。调用方必须用排他创建裁决极小概率碰撞。
    pub fn temporary_sibling_path(&self, target: &str, role: &str) -> String {
        unique_sibling_path(target, role)
    }

    /// 用已经完整构建好的同级 staging 替换现有目标。目标先改名到隐藏 backup，
    /// promotion 失败会尽力恢复；backup 可以是非空目录。
    pub async fn replace_staged_entry(
        &self,
        staging: &str,
        target: &str,
    ) -> Result<(), SftpTransferError> {
        let backup = unique_sibling_path(target, "backup");
        if let Err(err) = self.rename(target, &backup).await {
            if let Ok(kind) = self.node_kind(staging).await {
                let _ = self.remove_tree(staging, kind).await;
            }
            return Err(err);
        }
        if let Err(promote_error) = self.rename(staging, target).await {
            let rollback = self.rename(&backup, target).await;
            if let Ok(kind) = self.node_kind(staging).await {
                let _ = self.remove_tree(staging, kind).await;
            }
            return match rollback {
                Ok(()) => Err(promote_error),
                Err(rollback_error) => Err(SftpTransferError::Sftp(format!(
                    "promotion failed: {}; rollback failed: {}; backup remains at '{}'",
                    promote_error.message(),
                    rollback_error.message(),
                    backup
                ))),
            };
        }
        let kind = self.node_kind(&backup).await.map_err(|error| {
            SftpTransferError::Sftp(format!(
                "replacement succeeded but backup could not be inspected at '{}': {}",
                backup,
                error.message()
            ))
        })?;
        self.remove_tree(&backup, kind).await.map_err(|error| {
            SftpTransferError::Sftp(format!(
                "replacement succeeded but backup cleanup failed at '{}': {}",
                backup,
                error.message()
            ))
        })?;
        Ok(())
    }

    async fn replace_staged_regular_file(
        &self,
        staging: &str,
        target: &str,
        contents: &[u8],
        max_bytes: usize,
        expected_current: Option<&[u8]>,
    ) -> Result<SftpFileReplaceResult, SftpTransferError> {
        let backup = match editor_backup_path(target) {
            Ok(backup) => backup,
            Err(error) => return Err(self.with_staging_cleanup_error(error, staging).await),
        };
        let backup_kind = match self.try_node_kind(&backup).await {
            Ok(kind) => kind,
            Err(error) => return Err(self.with_staging_cleanup_error(error, staging).await),
        };
        if let Some(kind) = backup_kind {
            let error = SftpTransferError::Sftp(format!(
                "remote editor backup already exists with type {kind:?}: '{backup}'; refusing to overwrite recovery data"
            ));
            return Err(self.with_staging_cleanup_error(error, staging).await);
        }

        let mut state_warning = None;
        if let Some(error) = self.rename(target, &backup).await.err() {
            let target_kind = self.try_node_kind(target).await;
            let backup_kind = self.try_node_kind(&backup).await;
            match (target_kind, backup_kind) {
                (Ok(None), Ok(Some(SftpNodeKind::File))) => {
                    // The request timed out or lost its reply after the server
                    // completed the rename. Continue from the observed state.
                    state_warning = Some(format!(
                        "isolation reply was lost but the backup state was verified: {}",
                        error.message()
                    ));
                }
                (Ok(Some(SftpNodeKind::File)), Ok(None)) => {
                    return Err(self.with_staging_cleanup_error(error, staging).await);
                }
                (target_state, backup_state) => {
                    return Err(SftpTransferError::Sftp(format!(
                        "remote editor isolation state is uncertain after '{}': target={target_state:?}, backup={backup_state:?}; recovery data kept at '{backup}', staging kept at '{staging}'",
                        error.message()
                    )));
                }
            }
        }

        let isolated = match self.read_file_bounded(&backup, max_bytes).await {
            Ok(current) => current,
            Err(error) => {
                return Err(self
                    .rollback_staged_regular_file(&backup, target, staging, error)
                    .await);
            }
        };
        let changed = matches!(&isolated, SftpBoundedFileRead::TooLarge)
            || expected_current.is_some_and(|expected| !isolated.matches_bytes(expected));
        if changed {
            if let Err(rollback_error) = self.rename(&backup, target).await {
                return Err(SftpTransferError::Sftp(format!(
                    "remote file changed after isolation; rollback request failed: {}; target/backup/staging final state is uncertain; recovery paths are backup='{}', staging='{}'",
                    rollback_error.message(),
                    backup,
                    staging
                )));
            }
            self.discard_file_staging(staging).await.map_err(|cleanup_error| {
                SftpTransferError::Sftp(format!(
                    "remote file changed after isolation; staging cleanup failed at '{staging}': {}",
                    cleanup_error.message()
                ))
            })?;
            return Ok(SftpFileReplaceResult::ExternalChange(isolated));
        }

        let permissions = match self.regular_file_permissions(&backup).await {
            Ok(permissions) => permissions,
            Err(error) => {
                return Err(self
                    .rollback_staged_regular_file(&backup, target, staging, error)
                    .await);
            }
        };
        if let Err(error) = self.set_file_permissions(staging, permissions).await {
            return Err(self
                .rollback_staged_regular_file(&backup, target, staging, error)
                .await);
        }
        if let Err(error) = self
            .node_kind(staging)
            .await
            .and_then(|kind| ensure_regular_file(kind, staging))
        {
            return Err(self
                .rollback_staged_regular_file(&backup, target, staging, error)
                .await);
        }

        let promote_error = self.rename(staging, target).await.err();
        let promote_error_message = promote_error
            .as_ref()
            .map(|error| error.message().to_string());
        match self.read_file_bounded(target, max_bytes).await {
            Ok(current) if current.matches_bytes(contents) => {
                let actual_permissions = self
                    .regular_file_permissions(target)
                    .await
                    .map_err(|error| {
                        SftpTransferError::Sftp(format!(
                            "remote editor promotion contents were verified but permissions could not be checked: {}; backup remains at '{backup}', staging path='{staging}'",
                            error.message()
                        ))
                    })?;
                if actual_permissions != permissions {
                    return Err(SftpTransferError::Sftp(format!(
                        "remote editor promotion installed unexpected permissions: expected {permissions:o}, found {actual_permissions:o}; backup remains at '{backup}', staging path='{staging}'"
                    )));
                }
                if let Some(error) = promote_error.as_ref() {
                    let staging_state = self.try_node_kind(staging).await;
                    let backup_state = self.try_node_kind(&backup).await;
                    if !can_accept_verified_promotion(&staging_state, &backup_state) {
                        return Err(SftpTransferError::Sftp(format!(
                            "remote editor promotion reply failed and completion could not be verified after '{}': staging={staging_state:?}, backup={backup_state:?}; target contents were matched but recovery data was preserved at backup='{backup}', staging='{staging}'",
                            error.message()
                        )));
                    }
                }
            }
            Ok(current) => {
                let observed = match current {
                    SftpBoundedFileRead::Complete(bytes) => {
                        format!("{} bytes", bytes.len())
                    }
                    SftpBoundedFileRead::TooLarge => "more than the editor limit".into(),
                };
                return Err(SftpTransferError::Sftp(format!(
                    "remote editor promotion did not install the staged contents; target now has {observed}; backup remains at '{backup}', staging state is unchanged or unknown"
                )));
            }
            Err(error) => {
                let target_state = self.try_node_kind(target).await;
                let backup_state = self.try_node_kind(&backup).await;
                if can_restore_verified_backup(&target_state, &backup_state) {
                    let rollback_result = self.rename(&backup, target).await;
                    let original = promote_error.unwrap_or(error);
                    return match rollback_result {
                        Ok(()) => Err(self.with_staging_cleanup_error(original, staging).await),
                        Err(rollback_error) => Err(SftpTransferError::Sftp(format!(
                            "{}; promotion state could not be read and rollback request failed: {}; target/backup/staging final state is uncertain; recovery paths are backup='{}', staging='{}'",
                            original.message(),
                            rollback_error.message(),
                            backup,
                            staging
                        ))),
                    };
                }
                return Err(SftpTransferError::Sftp(format!(
                    "remote editor promotion state is uncertain after '{}': target={target_state:?}, backup={backup_state:?}; recovery backup path='{}', staging path='{}' (final presence unknown)",
                    error.message(),
                    backup,
                    staging
                )));
            }
        }

        let mut warning = state_warning;
        if let Some(error) = promote_error_message.as_deref() {
            push_warning(
                &mut warning,
                format!(
                    "promotion reply was lost but the target contents were verified: {}",
                    error
                ),
            );
        }
        if let Err(error) = self.remove_file(&backup).await {
            push_warning(
                &mut warning,
                format!(
                    "replacement succeeded but recovery backup cleanup failed at '{backup}': {}",
                    error.message()
                ),
            );
        }
        Ok(SftpFileReplaceResult::Replaced {
            cleanup_warning: warning,
        })
    }

    async fn rollback_staged_regular_file(
        &self,
        backup: &str,
        target: &str,
        staging: &str,
        reason: SftpTransferError,
    ) -> SftpTransferError {
        match self.rename(backup, target).await {
            Ok(()) => self.with_staging_cleanup_error(reason, staging).await,
            Err(rollback_error) => SftpTransferError::Sftp(format!(
                "{}; rollback request failed: {}; target/backup/staging final state is uncertain; recovery paths are backup='{}', staging='{}'",
                reason.message(),
                rollback_error.message(),
                backup,
                staging
            )),
        }
    }

    /// Replace an existing regular remote file with bounded in-memory data.
    ///
    /// Contents are written to an exclusive, same-directory staging file and
    /// closed before the editor-specific backup-swap/rollback sequence. The target type is
    /// checked both before staging and immediately before promotion. A failed staging write keeps
    /// the original error and appends any cleanup failure instead of silently
    /// discarding it. The target is read again after staging so the size/type
    /// limit still applies to force-save; when `expected_current` is present,
    /// byte changes are also returned without promoting the staged content.
    pub async fn replace_file_contents(
        &self,
        target: &str,
        contents: &[u8],
        max_bytes: usize,
        expected_current: Option<&[u8]>,
    ) -> Result<SftpFileReplaceResult, SftpTransferError> {
        use russh_sftp::protocol::{FileAttributes, OpenFlags};

        if contents.len() > max_bytes {
            return Err(SftpTransferError::Sftp(format!(
                "remote file contents exceed the {max_bytes}-byte limit"
            )));
        }
        self.prepare_file_replacement_state(target).await?;
        ensure_regular_file(self.node_kind(target).await?, target)?;
        let permissions = self.regular_file_permissions(target).await?;
        let mut attributes = FileAttributes::empty();
        attributes.permissions = Some(permissions);

        let staging = unique_sibling_path(target, "partial");
        let mut remote = match self
            .sftp
            .open_with_flags_and_attributes(
                &staging,
                OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::EXCLUDE,
                attributes,
            )
            .await
        {
            Ok(remote) => remote,
            Err(error) => {
                let original =
                    SftpTransferError::Sftp(format!("sftp create '{staging}' failed: {error}"));
                let staging_state = self.try_node_kind(&staging).await;
                return Err(staging_create_error(original, &staging, staging_state));
            }
        };

        let write_result: Result<(), SftpTransferError> = async {
            for chunk in contents.chunks(SFTP_READ_CHUNK_BYTES) {
                remote.write_all(chunk).await.map_err(|error| {
                    SftpTransferError::Sftp(format!("sftp write '{staging}' failed: {error}"))
                })?;
            }
            remote.flush().await.map_err(|error| {
                SftpTransferError::Sftp(format!("sftp flush '{staging}' failed: {error}"))
            })?;
            remote.shutdown().await.map_err(|error| {
                SftpTransferError::Sftp(format!("sftp close '{staging}' failed: {error}"))
            })?;
            Ok(())
        }
        .await;
        drop(remote);

        if let Err(error) = write_result {
            return Err(self.with_staging_cleanup_error(error, &staging).await);
        }
        // Always re-read after staging, including force-save. Force may skip the
        // byte-equality comparison, but it must not skip the maximum target size
        // check or the regular-file checks performed by read_file_bounded.
        let current = match self.read_file_bounded(target, max_bytes).await {
            Ok(current) => current,
            Err(error) => {
                return Err(self.with_staging_cleanup_error(error, &staging).await);
            }
        };
        let changed = matches!(&current, SftpBoundedFileRead::TooLarge)
            || expected_current.is_some_and(|expected| !current.matches_bytes(expected));
        if changed {
            self.discard_file_staging(&staging)
                .await
                .map_err(|cleanup_error| {
                    SftpTransferError::Sftp(format!(
                        "remote file changed before promotion; staging cleanup failed at \
                     '{staging}': {}",
                        cleanup_error.message()
                    ))
                })?;
            return Ok(SftpFileReplaceResult::ExternalChange(current));
        }
        if let Err(error) = self
            .node_kind(target)
            .await
            .and_then(|kind| ensure_regular_file(kind, target))
        {
            return Err(self.with_staging_cleanup_error(error, &staging).await);
        }

        self.replace_staged_regular_file(&staging, target, contents, max_bytes, expected_current)
            .await
    }

    async fn with_staging_cleanup_error(
        &self,
        original: SftpTransferError,
        staging: &str,
    ) -> SftpTransferError {
        let cleanup = self.discard_file_staging(staging).await;
        append_cleanup_error(original, cleanup, staging)
    }

    async fn discard_file_staging(&self, staging: &str) -> Result<(), SftpTransferError> {
        match self.try_node_kind(staging).await {
            Ok(None) => Ok(()),
            Ok(Some(SftpNodeKind::Directory)) => Err(SftpTransferError::Sftp(format!(
                "refusing to recursively remove unexpected staging directory '{staging}'"
            ))),
            Ok(Some(_)) => self.remove_file(staging).await,
            Err(error) => Err(error),
        }
    }

    /// 把本地文件流式写到远端。新目标直接用 EXCLUDE 排他创建，避免提交阶段
    /// 覆盖竞态；覆盖目标使用同级临时文件 + backup-swap。
    pub async fn upload_file(
        &self,
        local_path: &Path,
        remote_path: &str,
        overwrite: bool,
    ) -> Result<u64, SftpTransferError> {
        use russh_sftp::protocol::OpenFlags;

        let expected_local = tokio::fs::symlink_metadata(local_path).await.map_err(|e| {
            SftpTransferError::Sftp(format!(
                "cannot inspect local file '{}': {e}",
                local_path.display()
            ))
        })?;
        if expected_local.file_type().is_symlink() || !expected_local.is_file() {
            return Err(SftpTransferError::Sftp(format!(
                "local upload source is not a regular file: '{}'",
                local_path.display()
            )));
        }
        let mut local = tokio::fs::File::open(local_path).await.map_err(|e| {
            SftpTransferError::Sftp(format!(
                "cannot open local file '{}': {e}",
                local_path.display()
            ))
        })?;
        let opened_local = local.metadata().await.map_err(|e| {
            SftpTransferError::Sftp(format!(
                "cannot inspect opened local file '{}': {e}",
                local_path.display()
            ))
        })?;
        ensure_same_local_file(local_path, &expected_local, &opened_local)?;
        let staging = if overwrite {
            unique_sibling_path(remote_path, "partial")
        } else {
            remote_path.to_string()
        };
        let mut remote = self
            .sftp
            .open_with_flags(
                &staging,
                OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::EXCLUDE,
            )
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp create '{staging}' failed: {e}")))?;

        let mut total = 0u64;
        let mut buf = vec![0u8; SFTP_READ_CHUNK_BYTES];
        let write_result: Result<(), SftpTransferError> = async {
            loop {
                let n = local.read(&mut buf).await.map_err(|e| {
                    SftpTransferError::Sftp(format!(
                        "read local file '{}' failed: {e}",
                        local_path.display()
                    ))
                })?;
                if n == 0 {
                    break;
                }
                remote.write_all(&buf[..n]).await.map_err(|e| {
                    SftpTransferError::Sftp(format!("sftp write '{staging}' failed: {e}"))
                })?;
                total += n as u64;
            }
            remote.flush().await.map_err(|e| {
                SftpTransferError::Sftp(format!("sftp flush '{staging}' failed: {e}"))
            })?;
            remote.shutdown().await.map_err(|e| {
                SftpTransferError::Sftp(format!("sftp close '{staging}' failed: {e}"))
            })?;
            Ok(())
        }
        .await;
        if let Err(err) = write_result {
            let _ = self.sftp.remove_file(&staging).await;
            return Err(err);
        }

        if overwrite {
            self.replace_staged_entry(&staging, remote_path).await?;
        }
        Ok(total)
    }

    /// 同一 SFTP 会话内复制远端文件，避免经本机临时文件中转。新目标使用
    /// EXCLUDE 排他创建；覆盖目标使用临时文件 + backup-swap。
    pub async fn copy_file(
        &self,
        source_path: &str,
        target_path: &str,
        overwrite: bool,
    ) -> Result<u64, SftpTransferError> {
        use russh_sftp::protocol::OpenFlags;

        let mut source = self.sftp.open(source_path).await.map_err(|e| {
            SftpTransferError::Sftp(format!("sftp open '{source_path}' failed: {e}"))
        })?;
        let staging = if overwrite {
            unique_sibling_path(target_path, "partial")
        } else {
            target_path.to_string()
        };
        let mut target = self
            .sftp
            .open_with_flags(
                &staging,
                OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::EXCLUDE,
            )
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp create '{staging}' failed: {e}")))?;

        let mut total = 0u64;
        let mut buf = vec![0u8; SFTP_READ_CHUNK_BYTES];
        let copy_result: Result<(), SftpTransferError> = async {
            loop {
                let n = source.read(&mut buf).await.map_err(|e| {
                    SftpTransferError::Sftp(format!("sftp read '{source_path}' failed: {e}"))
                })?;
                if n == 0 {
                    break;
                }
                target.write_all(&buf[..n]).await.map_err(|e| {
                    SftpTransferError::Sftp(format!("sftp write '{staging}' failed: {e}"))
                })?;
                total += n as u64;
            }
            target.flush().await.map_err(|e| {
                SftpTransferError::Sftp(format!("sftp flush '{staging}' failed: {e}"))
            })?;
            target.shutdown().await.map_err(|e| {
                SftpTransferError::Sftp(format!("sftp close '{staging}' failed: {e}"))
            })?;
            Ok(())
        }
        .await;
        if let Err(err) = copy_result {
            let _ = self.sftp.remove_file(&staging).await;
            return Err(err);
        }

        if overwrite {
            self.replace_staged_entry(&staging, target_path).await?;
        }
        Ok(total)
    }

    /// 把远端文件流式下载到本地。新目标用 `create_new` 排他创建；覆盖目标先写
    /// 唯一同级临时文件，完成后再通过 backup-swap 替换。
    pub async fn download_file(
        &self,
        remote_path: &str,
        local_path: &Path,
        overwrite: bool,
    ) -> Result<u64, SftpTransferError> {
        let mut remote = self.sftp.open(remote_path).await.map_err(|e| {
            SftpTransferError::Sftp(format!("sftp open '{remote_path}' failed: {e}"))
        })?;
        let staging = if overwrite {
            unique_local_sibling(local_path, "partial")
        } else {
            local_path.to_path_buf()
        };
        let mut local = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .await
            .map_err(|e| {
                SftpTransferError::Sftp(format!(
                    "cannot create local file '{}': {e}",
                    staging.display()
                ))
            })?;

        let mut total = 0u64;
        let mut buf = vec![0u8; SFTP_READ_CHUNK_BYTES];
        let copy_result: Result<(), SftpTransferError> = async {
            loop {
                let n = remote.read(&mut buf).await.map_err(|e| {
                    SftpTransferError::Sftp(format!("sftp read '{remote_path}' failed: {e}"))
                })?;
                if n == 0 {
                    break;
                }
                local.write_all(&buf[..n]).await.map_err(|e| {
                    SftpTransferError::Sftp(format!(
                        "write local file '{}' failed: {e}",
                        staging.display()
                    ))
                })?;
                total += n as u64;
            }
            local.flush().await.map_err(|e| {
                SftpTransferError::Sftp(format!(
                    "flush local file '{}' failed: {e}",
                    staging.display()
                ))
            })?;
            Ok(())
        }
        .await;
        drop(local);
        if let Err(err) = copy_result {
            let _ = tokio::fs::remove_file(&staging).await;
            return Err(err);
        }

        if overwrite {
            let backup = unique_local_sibling(local_path, "backup");
            if let Err(e) = tokio::fs::rename(local_path, &backup).await {
                let _ = tokio::fs::remove_file(&staging).await;
                return Err(SftpTransferError::Sftp(format!(
                    "cannot back up local target '{}': {e}",
                    local_path.display()
                )));
            }
            if let Err(err) = tokio::fs::rename(&staging, local_path).await {
                let rollback = tokio::fs::rename(&backup, local_path).await;
                let _ = tokio::fs::remove_file(&staging).await;
                return match rollback {
                    Ok(()) => Err(SftpTransferError::Sftp(format!(
                        "cannot promote local download '{}': {err}",
                        local_path.display()
                    ))),
                    Err(rollback_error) => Err(SftpTransferError::Sftp(format!(
                        "cannot promote local download '{}': {err}; rollback failed: \
                         {rollback_error}; backup remains at '{}'",
                        local_path.display(),
                        backup.display()
                    ))),
                };
            }
            remove_local_tree(&backup).await.map_err(|error| {
                SftpTransferError::Sftp(format!(
                    "download succeeded but backup cleanup failed at '{}': {error}",
                    backup.display()
                ))
            })?;
        }
        Ok(total)
    }

    /// 显式关闭 SFTP 会话(best-effort;drop 也会关底层 channel)。
    pub async fn close(self) {
        let _ = self.sftp.close().await;
    }
}

fn ensure_regular_file(kind: SftpNodeKind, path: &str) -> Result<(), SftpTransferError> {
    match kind {
        SftpNodeKind::File => Ok(()),
        SftpNodeKind::Directory => Err(SftpTransferError::Sftp(format!(
            "remote editor target is a directory: '{path}'"
        ))),
        SftpNodeKind::Symlink => Err(SftpTransferError::Sftp(format!(
            "remote editor target is a symbolic link: '{path}'"
        ))),
        SftpNodeKind::Other => Err(SftpTransferError::Sftp(format!(
            "remote editor target is not a regular file: '{path}'"
        ))),
    }
}

fn classify_bounded_file_bytes(bytes: Vec<u8>, max_bytes: usize) -> SftpBoundedFileRead {
    if bytes.len() > max_bytes {
        SftpBoundedFileRead::TooLarge
    } else {
        SftpBoundedFileRead::Complete(bytes)
    }
}

fn append_cleanup_error(
    original: SftpTransferError,
    cleanup: Result<(), SftpTransferError>,
    staging: &str,
) -> SftpTransferError {
    match cleanup {
        Ok(()) => original,
        Err(cleanup_error) => SftpTransferError::Sftp(format!(
            "{}; staging cleanup failed at '{}': {}",
            original.message(),
            staging,
            cleanup_error.message()
        )),
    }
}

fn staging_create_error(
    original: SftpTransferError,
    staging: &str,
    staging_state: Result<Option<SftpNodeKind>, SftpTransferError>,
) -> SftpTransferError {
    match staging_state {
        Ok(None) => original,
        Ok(Some(kind)) => SftpTransferError::Sftp(format!(
            "{}; the create reply failed but staging now exists as {kind:?} at '{staging}'; ownership is uncertain, so the path was preserved for manual recovery",
            original.message()
        )),
        Err(probe_error) => SftpTransferError::Sftp(format!(
            "{}; staging final state is uncertain at '{}': {}",
            original.message(),
            staging,
            probe_error.message()
        )),
    }
}

fn split_parent_name(path: &str) -> Result<(&str, &str), SftpTransferError> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return Err(SftpTransferError::Sftp(
            "远程根目录不能作为叶子条目操作".into(),
        ));
    }
    let Some(index) = trimmed.rfind('/') else {
        return Err(SftpTransferError::Sftp(format!(
            "远程路径必须是绝对路径: '{path}'"
        )));
    };
    let parent = if index == 0 { "/" } else { &trimmed[..index] };
    let name = &trimmed[index + 1..];
    if name.is_empty() || name == "." || name == ".." || name.contains('\0') {
        return Err(SftpTransferError::Sftp(format!(
            "远程路径条目名无效: '{path}'"
        )));
    }
    Ok((parent, name))
}

fn unique_sibling_path(target: &str, role: &str) -> String {
    let seq = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let (parent, name) = split_parent_name(target).unwrap_or(("/", "item"));
    let prefix = if parent == "/" { "/" } else { "" };
    if parent == "/" {
        format!(
            "{prefix}.{name}.mt-{role}-{}-{timestamp}-{seq}",
            std::process::id()
        )
    } else {
        format!(
            "{parent}/.{name}.mt-{role}-{}-{timestamp}-{seq}",
            std::process::id()
        )
    }
}

fn editor_backup_path(target: &str) -> Result<String, SftpTransferError> {
    let (parent, name) = split_parent_name(target)?;
    if parent == "/" {
        Ok(format!("/.{name}.mt-editor-backup"))
    } else {
        Ok(format!("{parent}/.{name}.mt-editor-backup"))
    }
}

fn ambiguous_editor_recovery_kind(
    target_kind: Option<SftpNodeKind>,
    backup_kind: Option<SftpNodeKind>,
) -> Option<SftpNodeKind> {
    if target_kind.is_none() {
        backup_kind
    } else {
        None
    }
}

fn push_warning(warning: &mut Option<String>, message: String) {
    *warning = Some(match warning.take() {
        Some(existing) => format!("{existing}; {message}"),
        None => message,
    });
}

fn unique_local_sibling(target: &Path, role: &str) -> std::path::PathBuf {
    let seq = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("item");
    target.with_file_name(format!(
        ".{name}.mt-{role}-{}-{timestamp}-{seq}",
        std::process::id(),
    ))
}

async fn remove_local_tree(path: &Path) -> std::io::Result<()> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        tokio::fs::remove_dir_all(path).await
    } else {
        tokio::fs::remove_file(path).await
    }
}

fn ensure_same_local_file(
    path: &Path,
    expected: &std::fs::Metadata,
    opened: &std::fs::Metadata,
) -> Result<(), SftpTransferError> {
    if expected.file_type().is_symlink() || !expected.is_file() || !opened.is_file() {
        return Err(SftpTransferError::Sftp(format!(
            "local upload source changed type while opening: '{}'",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if expected.dev() != opened.dev() || expected.ino() != opened.ino() {
            return Err(SftpTransferError::Sftp(format!(
                "local upload source was replaced while opening: '{}'",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_file_kind_guard_rejects_non_regular_entries() {
        assert!(ensure_regular_file(SftpNodeKind::File, "/p/a.txt").is_ok());
        for kind in [
            SftpNodeKind::Directory,
            SftpNodeKind::Symlink,
            SftpNodeKind::Other,
        ] {
            let error = ensure_regular_file(kind, "/p/a.txt").unwrap_err();
            assert!(error.message().contains("/p/a.txt"));
        }
    }

    #[test]
    fn bounded_file_read_classifies_the_exact_limit_and_one_extra_byte() {
        let exact = classify_bounded_file_bytes(vec![0; 4], 4);
        assert_eq!(exact, SftpBoundedFileRead::Complete(vec![0; 4]));
        assert!(exact.matches_bytes(&[0; 4]));
        assert_eq!(
            classify_bounded_file_bytes(vec![0; 5], 4),
            SftpBoundedFileRead::TooLarge
        );
        assert!(!SftpBoundedFileRead::TooLarge.matches_bytes(&[0; 4]));
    }

    #[test]
    fn remote_editor_debug_output_does_not_expose_file_bytes_or_cleanup_errors() {
        let read = SftpBoundedFileRead::Complete(b"remote-secret".to_vec());
        let read_debug = format!("{read:?}");
        assert!(read_debug.contains("byte_len"));
        assert!(!read_debug.contains("remote-secret"));

        let replaced = SftpFileReplaceResult::Replaced {
            cleanup_warning: Some("/private/path: permission denied".into()),
        };
        let replace_debug = format!("{replaced:?}");
        assert!(replace_debug.contains("has_cleanup_warning"));
        assert!(!replace_debug.contains("/private/path"));
    }

    #[test]
    fn automatic_editor_recovery_refuses_only_missing_targets_with_backup_data() {
        assert_eq!(
            ambiguous_editor_recovery_kind(None, Some(SftpNodeKind::File)),
            Some(SftpNodeKind::File)
        );
        assert_eq!(
            ambiguous_editor_recovery_kind(None, Some(SftpNodeKind::Other)),
            Some(SftpNodeKind::Other)
        );
        assert_eq!(
            ambiguous_editor_recovery_kind(Some(SftpNodeKind::File), Some(SftpNodeKind::File)),
            None
        );
        assert_eq!(ambiguous_editor_recovery_kind(None, None), None);
    }

    #[test]
    fn committed_target_classifies_regular_backup_as_stale_cleanup_residue() {
        assert_eq!(
            stale_editor_backup_kind(Some(SftpNodeKind::File), Some(SftpNodeKind::File)),
            Some(SftpNodeKind::File)
        );
        assert_eq!(
            stale_editor_backup_kind(None, Some(SftpNodeKind::File)),
            None,
            "missing targets must preserve ambiguous recovery data"
        );
        assert_eq!(
            stale_editor_backup_kind(Some(SftpNodeKind::Directory), Some(SftpNodeKind::File)),
            None,
            "invalid targets must not authorize backup deletion"
        );
        assert_eq!(
            stale_editor_backup_kind(Some(SftpNodeKind::File), None),
            None
        );
        assert_eq!(
            stale_editor_backup_kind(Some(SftpNodeKind::File), Some(SftpNodeKind::Directory)),
            Some(SftpNodeKind::Directory),
            "the caller must see and refuse an unexpected backup type"
        );
    }

    #[test]
    fn stale_backup_cleanup_requires_verified_absence() {
        let path = "/srv/project/.notes.md.mt-editor-backup";
        assert!(
            verify_stale_editor_backup_cleanup(path, None, Ok(None)).is_ok(),
            "verified absence permits the next save"
        );
        assert!(
            verify_stale_editor_backup_cleanup(
                path,
                Some(SftpTransferError::Transport("lost reply".into())),
                Ok(None),
            )
            .is_ok(),
            "a lost remove reply is harmless once absence is verified"
        );

        let remains = verify_stale_editor_backup_cleanup(
            path,
            Some(SftpTransferError::Sftp("permission denied".into())),
            Ok(Some(SftpNodeKind::File)),
        )
        .unwrap_err();
        assert!(remains.message().contains("permission denied"));
        assert!(remains.message().contains("remaining type File"));

        let uncertain = verify_stale_editor_backup_cleanup(
            path,
            None,
            Err(SftpTransferError::Transport("lstat timeout".into())),
        )
        .unwrap_err();
        assert!(uncertain.message().contains("state is uncertain"));
        assert!(uncertain.message().contains("lstat timeout"));
    }

    #[test]
    fn editor_staging_paths_are_hidden_unique_siblings() {
        let first = unique_sibling_path("/srv/project/src/main.rs", "partial");
        let second = unique_sibling_path("/srv/project/src/main.rs", "partial");
        assert!(first.starts_with("/srv/project/src/.main.rs.mt-partial-"));
        assert!(second.starts_with("/srv/project/src/.main.rs.mt-partial-"));
        assert_ne!(first, second);

        let root_child = unique_sibling_path("/notes.md", "backup");
        assert!(root_child.starts_with("/.notes.md.mt-backup-"));

        assert_eq!(
            editor_backup_path("/srv/project/src/main.rs").unwrap(),
            "/srv/project/src/.main.rs.mt-editor-backup"
        );
        assert_eq!(
            editor_backup_path("/notes.md").unwrap(),
            "/.notes.md.mt-editor-backup"
        );
    }

    #[test]
    fn staging_cleanup_errors_preserve_the_original_failure() {
        let original = SftpTransferError::Sftp("write failed".into());
        let cleanup = SftpTransferError::Sftp("permission denied".into());
        let combined = append_cleanup_error(original, Err(cleanup), "/p/.a.partial");
        assert!(combined.message().contains("write failed"));
        assert!(combined.message().contains("permission denied"));
        assert!(combined.message().contains("/p/.a.partial"));
    }

    #[test]
    fn promotion_probe_errors_never_authorize_rollback() {
        let missing = Ok(None);
        let backup = Ok(Some(SftpNodeKind::File));
        assert!(can_restore_verified_backup(&missing, &backup));

        let target_error = Err(SftpTransferError::Transport("timeout".into()));
        assert!(!can_restore_verified_backup(&target_error, &backup));
        let backup_error = Err(SftpTransferError::Sftp("lstat failed".into()));
        assert!(!can_restore_verified_backup(&missing, &backup_error));
        let replaced = Ok(Some(SftpNodeKind::File));
        assert!(!can_restore_verified_backup(&replaced, &backup));
    }

    #[test]
    fn lost_promotion_requires_missing_staging_and_regular_backup() {
        let missing = Ok(None);
        let backup = Ok(Some(SftpNodeKind::File));
        assert!(can_accept_verified_promotion(&missing, &backup));

        let staging_remains = Ok(Some(SftpNodeKind::File));
        assert!(!can_accept_verified_promotion(&staging_remains, &backup));
        let no_backup = Ok(None);
        assert!(!can_accept_verified_promotion(&missing, &no_backup));
        let staging_probe_failed = Err(SftpTransferError::Transport("timeout".into()));
        assert!(!can_accept_verified_promotion(
            &staging_probe_failed,
            &backup
        ));
    }

    #[test]
    fn staging_create_failure_reports_observed_or_uncertain_state() {
        let path = "/srv/project/.notes.md.mt-partial";
        let existing = staging_create_error(
            SftpTransferError::Sftp("create timed out".into()),
            path,
            Ok(Some(SftpNodeKind::File)),
        );
        assert!(existing.message().contains("create timed out"));
        assert!(existing.message().contains("ownership is uncertain"));
        assert!(existing.message().contains(path));

        let uncertain = staging_create_error(
            SftpTransferError::Sftp("create timed out".into()),
            path,
            Err(SftpTransferError::Transport("lstat timed out".into())),
        );
        assert!(uncertain.message().contains("create timed out"));
        assert!(uncertain.message().contains("lstat timed out"));
        assert!(uncertain.message().contains(path));
    }
}
