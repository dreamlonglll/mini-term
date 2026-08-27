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
        let rd = self
            .sftp
            .read_dir(path)
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp readdir '{path}' failed: {e}")))?;
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
            let n = file.read(&mut buf[..want]).await.map_err(|e| {
                SftpTransferError::Sftp(format!("sftp read '{path}' failed: {e}"))
            })?;
            if n == 0 {
                break; // EOF
            }
            out.extend_from_slice(&buf[..n]);
        }
        Ok(out)
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
        self.sftp
            .rename(from, to)
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp rename '{from}' -> '{to}' failed: {e}")))
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
            .map_err(|e| {
                SftpTransferError::Sftp(format!("sftp create '{staging}' failed: {e}"))
            })?;

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

        let mut source = self
            .sftp
            .open(source_path)
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp open '{source_path}' failed: {e}")))?;
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
            .map_err(|e| {
                SftpTransferError::Sftp(format!("sftp create '{staging}' failed: {e}"))
            })?;

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
        let mut remote = self
            .sftp
            .open(remote_path)
            .await
            .map_err(|e| SftpTransferError::Sftp(format!("sftp open '{remote_path}' failed: {e}")))?;
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

fn unique_local_sibling(target: &Path, role: &str) -> std::path::PathBuf {
    let seq = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let name = target.file_name().and_then(|name| name.to_str()).unwrap_or("item");
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
