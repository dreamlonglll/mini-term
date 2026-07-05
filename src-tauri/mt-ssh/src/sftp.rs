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

use std::time::Duration;

use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::pool::{CachedSession, SftpTransferError};

/// 只读路径的分块缓冲。比 upload/download 的 8KB 大:russh-sftp 的 `File`
/// 会按服务器通告的 max read 长度(OpenSSH 通常 64KB)切请求,大缓冲能减少
/// 「读一个文件头」场景的网络往返数;内存占用仍是常数。
const SFTP_READ_CHUNK_BYTES: usize = 32 * 1024;

/// 一条 readdir 结果。只保留远程文件树 / 会话扫描需要的最小字段。
#[derive(Debug, Clone)]
pub struct SftpDirEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    /// 修改时间(UNIX 秒)。SFTP v3 属性可缺省。
    pub mtime_secs: Option<u64>,
}

/// 打开在某条 session 上的 SFTP 会话句柄。可跨多次操作复用;用完调 [`Self::close`]
/// (或直接 drop,底层 channel 随之关闭,close 只是显式礼貌收尾)。
pub struct SftpHandle {
    sftp: SftpSession,
}

impl SftpHandle {
    /// 在已认证 session 上开 SFTP channel 并握手。
    ///
    /// 错误分类与 upload/download 一致:开 channel / subsystem / 握手失败都是
    /// `Transport`(caller 可 evict + 重连重试一次);后续各操作的失败是 `Sftp`
    /// 业务错(不 evict)。
    pub async fn open_on_session(
        session: &CachedSession,
        request_timeout: Duration,
    ) -> Result<Self, SftpTransferError> {
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
        Ok(Self { sftp })
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

    /// 显式关闭 SFTP 会话(best-effort;drop 也会关底层 channel)。
    pub async fn close(self) {
        let _ = self.sftp.close().await;
    }
}
