//! SFTP 传输端到端测试:进程内 russh 服务器 + 内存文件系统的 SFTP 子系统,对本 crate 的
//! `run_sftp_download_on_session` / `run_sftp_upload_on_session`。
//!
//! 只连 127.0.0.1 上本测试自己起的服务器;known_hosts 在临时目录里,不读 ssh config、
//! 不连 ssh-agent([`SshClientEnv::isolated`]);密码用明文(`mt_secret::reveal_global`
//! 对非信封值原样放行,不碰凭据密钥文件)。本地文件一律落在临时目录。
//!
//! 内存文件系统按 Linux 的口径模拟:属主 / 属组 / 权限位、非 root 不能改属主、只能把
//! 属组改成自己所在的组、写入清掉 setuid / setgid、软链逐段解析、标准 rename 不覆盖
//! 已存在的目标而 `posix-rename@openssh.com` 原子覆盖。另可注入:改名失败、读上限
//! (短读)、`limits@openssh.com`、应答延迟(高 RTT)、卡住读写的闸门。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use mt_core::SshConnection;
use mt_ssh::russh;
use mt_ssh::{CachedSession, PoolConfig, SftpTransferError, SshClientEnv, SshPool};
use russh::keys::PrivateKey;
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use russh_sftp::protocol::{
    Attrs, Data, ExtendedReply, File, FileAttributes, Handle, Name, OpenFlags, Packet, Status,
    StatusCode, Version,
};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Semaphore};

static UNIQUE: AtomicUsize = AtomicUsize::new(0);

const HOME: &str = "/home/u";
const UID: u32 = 1000;
const GID: u32 = 1000;
/// 另一个用户 / 组(如 www-data)。
const WWW: u32 = 33;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

// ============================================================================
// 夹具
// ============================================================================

fn ed25519(seed: u8) -> PrivateKey {
    PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(
        &[seed; 32],
    ))
}

/// 确定的伪随机内容(xorshift),不引入 rand。
fn pattern(len: usize, seed: u32) -> Vec<u8> {
    let mut state = seed.max(1);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 7) as u8
        })
        .collect()
}

/// 本地临时目录(要上传 / 下载的文件与 known_hosts),drop 时删除。
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "mt-ssh-sftp-e2e-{}-{}-{name}",
            std::process::id(),
            UNIQUE.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ============================================================================
// 内存文件系统
// ============================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    File(Vec<u8>),
    Dir,
    Symlink(String),
}

#[derive(Clone, Debug)]
struct Inode {
    kind: Kind,
    /// 权限位(含 setuid / setgid / sticky),不含类型位。
    mode: u32,
    uid: u32,
    gid: u32,
}

impl Inode {
    fn attrs(&self) -> FileAttributes {
        let (type_bits, size) = match &self.kind {
            Kind::File(data) => (0o100000, data.len() as u64),
            Kind::Dir => (0o040000, 0),
            Kind::Symlink(target) => (0o120000, target.len() as u64),
        };
        let mut attrs = FileAttributes::empty();
        attrs.size = Some(size);
        attrs.uid = Some(self.uid);
        attrs.gid = Some(self.gid);
        attrs.permissions = Some(type_bits | self.mode);
        attrs
    }
}

/// 登录用户。
#[derive(Clone)]
struct User {
    uid: u32,
    /// 第一个是主组(新建文件的属组)。
    gids: Vec<u32>,
}

impl User {
    fn normal(extra_groups: &[u32]) -> Self {
        let mut gids = vec![GID];
        gids.extend_from_slice(extra_groups);
        Self { uid: UID, gids }
    }

    fn root() -> Self {
        Self {
            uid: 0,
            gids: vec![0],
        }
    }

    /// `bit`:4 = 读,2 = 写。root 不受限。
    fn may(&self, inode: &Inode, bit: u32) -> bool {
        if self.uid == 0 {
            return true;
        }
        let shift = if inode.uid == self.uid {
            6
        } else if self.gids.contains(&inode.gid) {
            3
        } else {
            0
        };
        (inode.mode >> shift) & bit != 0
    }

    /// Linux 的 chown / chmod 规则(本测试用得到的部分)。
    fn apply(&self, inode: &mut Inode, attrs: &FileAttributes) -> Result<(), StatusCode> {
        if attrs.uid.is_some() || attrs.gid.is_some() {
            // 协议里两者共用一个标志位:只给一个时另一个按 0 发出。
            let uid = attrs.uid.unwrap_or(0);
            let gid = attrs.gid.unwrap_or(0);
            if self.uid != 0
                && (uid != inode.uid
                    || inode.uid != self.uid
                    || (gid != inode.gid && !self.gids.contains(&gid)))
            {
                return Err(StatusCode::PermissionDenied);
            }
            inode.uid = uid;
            inode.gid = gid;
        }
        if let Some(mode) = attrs.permissions {
            if self.uid != 0 && inode.uid != self.uid {
                return Err(StatusCode::PermissionDenied);
            }
            let mut mode = mode & 0o7777;
            // 不在文件的属组里时,内核悄悄丢掉 setgid。
            if self.uid != 0 && !self.gids.contains(&inode.gid) {
                mode &= !0o2000;
            }
            inode.mode = mode;
        }
        Ok(())
    }
}

fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(index) => path[..index].to_string(),
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

struct Fs {
    /// 规范化的绝对路径 → inode 号(改名只动这张表,inode 与属性随之搬走)。
    entries: BTreeMap<String, u64>,
    inodes: HashMap<u64, Inode>,
    next_ino: u64,
}

impl Fs {
    fn new() -> Self {
        let mut fs = Self {
            entries: BTreeMap::new(),
            inodes: HashMap::new(),
            next_ino: 1,
        };
        for (path, uid) in [("/", 0), ("/home", 0), (HOME, UID)] {
            fs.insert(
                path,
                Inode {
                    kind: Kind::Dir,
                    mode: 0o755,
                    uid,
                    gid: uid,
                },
            );
        }
        fs
    }

    fn insert(&mut self, path: &str, inode: Inode) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(ino, inode);
        self.entries.insert(path.to_string(), ino);
        ino
    }

    fn ino(&self, path: &str) -> Option<u64> {
        self.entries.get(path).copied()
    }

    fn node(&self, path: &str) -> Option<&Inode> {
        self.ino(path).and_then(|ino| self.inodes.get(&ino))
    }

    /// 在 `dir` 里建 / 删 / 改名条目要求它是目录且对用户可写。
    fn dir_writable(&self, user: &User, dir: &str) -> Result<(), StatusCode> {
        match self.node(dir) {
            Some(inode) if inode.kind == Kind::Dir => {
                if user.may(inode, 2) {
                    Ok(())
                } else {
                    Err(StatusCode::PermissionDenied)
                }
            }
            _ => Err(StatusCode::NoSuchFile),
        }
    }

    fn names(&self, dir: &str) -> Vec<String> {
        let prefix = if dir == "/" {
            "/".to_string()
        } else {
            format!("{dir}/")
        };
        self.entries
            .keys()
            .filter_map(|path| path.strip_prefix(&prefix))
            .filter(|rest| !rest.is_empty() && !rest.contains('/'))
            .map(str::to_string)
            .collect()
    }

    /// 逐段解析(中间的软链一律跟随,最后一段看 `follow_last`)。`allow_missing`:最后
    /// 一段不存在时返回它将被创建的路径。相对路径从 `base` 起。
    fn resolve(
        &self,
        base: &str,
        path: &str,
        follow_last: bool,
        allow_missing: bool,
        depth: usize,
    ) -> Result<String, StatusCode> {
        if depth > 40 {
            return Err(StatusCode::Failure);
        }
        let mut current = if path.starts_with('/') {
            "/".to_string()
        } else {
            base.to_string()
        };
        let parts: Vec<&str> = path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect();
        for (index, part) in parts.iter().enumerate() {
            let last = index + 1 == parts.len();
            if *part == ".." {
                current = parent_of(&current);
                continue;
            }
            let candidate = join(&current, part);
            let Some(inode) = self.node(&candidate) else {
                return if last && allow_missing {
                    Ok(candidate)
                } else {
                    Err(StatusCode::NoSuchFile)
                };
            };
            match &inode.kind {
                Kind::Symlink(target) if !last || follow_last => {
                    current =
                        self.resolve(&current, target, true, last && allow_missing, depth + 1)?;
                    if !last && !matches!(self.node(&current).map(|n| &n.kind), Some(Kind::Dir)) {
                        return Err(StatusCode::NoSuchFile);
                    }
                }
                Kind::Dir => current = candidate,
                _ if last => current = candidate,
                _ => return Err(StatusCode::NoSuchFile),
            }
        }
        Ok(current)
    }
}

// ============================================================================
// SFTP 子系统
// ============================================================================

#[derive(Clone)]
struct SftpConfig {
    user: User,
    posix_rename: bool,
    /// `limits@openssh.com`:(最大包长, 最大读, 最大写, 最大句柄数)。
    limits: Option<[u64; 4]>,
    /// 每次读最多给这么多字节(没通告上限、实际更小的服务器 → 短读)。
    max_read: Option<u32>,
    /// 服务器 → 客户端方向的应答延迟(模拟高 RTT,服务器自己不被拖住)。
    reply_delay: Option<Duration>,
    /// 读 / 写请求在这里排队,测试放行前一律卡住。
    gate: Option<Arc<Semaphore>>,
}

impl Default for SftpConfig {
    fn default() -> Self {
        Self {
            user: User::normal(&[]),
            posix_rename: true,
            limits: None,
            max_read: None,
            reply_delay: None,
            gate: None,
        }
    }
}

/// 改名失败注入:`from` / `to` 分别包含给定片段的改名一律失败(`posix` 区分两种改名)。
#[derive(Clone)]
struct RenameFault {
    posix: bool,
    from: String,
    to: String,
}

#[derive(Default)]
struct Stats {
    reads: AtomicUsize,
    writes: AtomicUsize,
    max_read_len: AtomicUsize,
    /// 应答进入延迟线的个数 / 真正写回客户端的个数 / 两者之差的最大值。
    queued: AtomicUsize,
    delivered: AtomicUsize,
    max_outstanding: AtomicUsize,
    renames: Mutex<Vec<String>>,
    /// 新建文件时的 (路径, 权限)。
    created: Mutex<Vec<(String, u32)>>,
}

struct Shared {
    fs: Mutex<Fs>,
    stats: Stats,
    faults: Mutex<Vec<RenameFault>>,
}

struct OpenFile {
    ino: u64,
    read: bool,
    write: bool,
}

struct SftpHandler {
    config: SftpConfig,
    shared: Arc<Shared>,
    handles: HashMap<String, OpenFile>,
    next_handle: u64,
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

/// `posix-rename@openssh.com` 的请求体:两个 SSH string。
fn parse_string_pair(data: &[u8]) -> Option<(String, String)> {
    let mut rest = data;
    let mut take = || -> Option<String> {
        let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let value = String::from_utf8(rest.get(4..4 + len)?.to_vec()).ok()?;
        rest = &rest[4 + len..];
        Some(value)
    };
    let first = take()?;
    let second = take()?;
    Some((first, second))
}

impl SftpHandler {
    fn fs(&self) -> MutexGuard<'_, Fs> {
        self.shared.fs.lock().unwrap()
    }

    async fn pass_gate(&self) -> Result<(), StatusCode> {
        if let Some(gate) = &self.config.gate {
            gate.acquire()
                .await
                .map_err(|_| StatusCode::Failure)?
                .forget();
        }
        Ok(())
    }

    fn open_file(&self, handle: &str) -> Result<&OpenFile, StatusCode> {
        self.handles.get(handle).ok_or(StatusCode::Failure)
    }

    fn rename_entry(&self, old: &str, new: &str, posix: bool) -> Result<(), StatusCode> {
        let label = if posix { "posix-rename" } else { "rename" };
        self.shared
            .stats
            .renames
            .lock()
            .unwrap()
            .push(format!("{label} {old} -> {new}"));
        let faults = self.shared.faults.lock().unwrap().clone();
        if faults.iter().any(|fault| {
            fault.posix == posix && old.contains(&fault.from) && new.contains(&fault.to)
        }) {
            return Err(StatusCode::Failure);
        }
        let mut fs = self.fs();
        let from = fs.resolve(HOME, old, false, false, 0)?;
        let to = fs.resolve(HOME, new, false, true, 0)?;
        let ino = fs.ino(&from).ok_or(StatusCode::NoSuchFile)?;
        if fs.inodes[&ino].kind == Kind::Dir {
            return Err(StatusCode::OpUnsupported);
        }
        match fs.node(&to).map(|node| node.kind == Kind::Dir) {
            Some(true) => return Err(StatusCode::Failure),
            // 标准 SFTP rename(OpenSSH 的实现)不覆盖已存在的目标。
            Some(false) if !posix => return Err(StatusCode::Failure),
            _ => {}
        }
        fs.dir_writable(&self.config.user, &parent_of(&from))?;
        fs.dir_writable(&self.config.user, &parent_of(&to))?;
        fs.entries.remove(&from);
        fs.entries.insert(to, ino);
        Ok(())
    }
}

impl russh_sftp::server::Handler for SftpHandler {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        let mut version = Version::new();
        if self.config.posix_rename {
            version
                .extensions
                .insert("posix-rename@openssh.com".into(), "1".into());
        }
        if self.config.limits.is_some() {
            version
                .extensions
                .insert("limits@openssh.com".into(), "1".into());
        }
        Ok(version)
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let user = self.config.user.clone();
        let create = pflags.contains(OpenFlags::CREATE);
        let exclusive = create && pflags.contains(OpenFlags::EXCLUDE);
        let read = pflags.contains(OpenFlags::READ);
        let write = pflags.contains(OpenFlags::WRITE);
        let ino = {
            let shared = self.shared.clone();
            let mut fs = shared.fs.lock().unwrap();
            let path = fs.resolve(HOME, &filename, !exclusive, create, 0)?;
            let (ino, created) = match fs.ino(&path) {
                Some(_) if exclusive => return Err(StatusCode::Failure),
                Some(ino) => (ino, false),
                None if create => {
                    fs.dir_writable(&user, &parent_of(&path))?;
                    let mode = attrs.permissions.map_or(0o666, |mode| mode & 0o7777) & !0o022;
                    shared
                        .stats
                        .created
                        .lock()
                        .unwrap()
                        .push((path.clone(), mode));
                    let inode = Inode {
                        kind: Kind::File(Vec::new()),
                        mode,
                        uid: user.uid,
                        gid: user.gids[0],
                    };
                    (fs.insert(&path, inode), true)
                }
                None => return Err(StatusCode::NoSuchFile),
            };
            let inode = fs.inodes.get_mut(&ino).unwrap();
            if inode.kind == Kind::Dir && write {
                return Err(StatusCode::Failure);
            }
            // 新建的文件不查权限(open(O_CREAT) 返回的句柄照样可写)。
            if !created && ((read && !user.may(inode, 4)) || (write && !user.may(inode, 2))) {
                return Err(StatusCode::PermissionDenied);
            }
            if pflags.contains(OpenFlags::TRUNCATE) {
                if let Kind::File(data) = &mut inode.kind {
                    data.clear();
                }
            }
            ino
        };
        let handle = format!("h{}", self.next_handle);
        self.next_handle += 1;
        self.handles
            .insert(handle.clone(), OpenFile { ino, read, write });
        Ok(Handle { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.handles.remove(&handle).ok_or(StatusCode::Failure)?;
        Ok(ok(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        self.shared.stats.reads.fetch_add(1, Ordering::SeqCst);
        self.shared
            .stats
            .max_read_len
            .fetch_max(len as usize, Ordering::SeqCst);
        self.pass_gate().await?;
        let file = self.open_file(&handle)?;
        if !file.read {
            return Err(StatusCode::PermissionDenied);
        }
        let fs = self.fs();
        let Kind::File(data) = &fs.inodes[&file.ino].kind else {
            return Err(StatusCode::Failure);
        };
        let size = data.len() as u64;
        if offset >= size {
            return Err(StatusCode::Eof);
        }
        let len = self.config.max_read.map_or(len, |max| len.min(max));
        let end = (offset + u64::from(len)).min(size);
        Ok(Data {
            id,
            data: data[offset as usize..end as usize].to_vec(),
        })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        self.shared.stats.writes.fetch_add(1, Ordering::SeqCst);
        self.pass_gate().await?;
        let file = self.open_file(&handle)?;
        if !file.write {
            return Err(StatusCode::PermissionDenied);
        }
        let ino = file.ino;
        let user_is_root = self.config.user.uid == 0;
        let mut fs = self.fs();
        let inode = fs.inodes.get_mut(&ino).unwrap();
        let Kind::File(content) = &mut inode.kind else {
            return Err(StatusCode::Failure);
        };
        let end = offset as usize + data.len();
        if content.len() < end {
            content.resize(end, 0);
        }
        content[offset as usize..end].copy_from_slice(&data);
        // 与 Linux 一样:非 root 写入清掉 setuid / setgid。
        if !user_is_root {
            inode.mode &= !0o6000;
        }
        Ok(ok(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let fs = self.fs();
        let path = fs.resolve(HOME, &path, false, false, 0)?;
        let attrs = fs.node(&path).ok_or(StatusCode::NoSuchFile)?.attrs();
        Ok(Attrs { id, attrs })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let fs = self.fs();
        let path = fs.resolve(HOME, &path, true, false, 0)?;
        let attrs = fs.node(&path).ok_or(StatusCode::NoSuchFile)?.attrs();
        Ok(Attrs { id, attrs })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let ino = self.open_file(&handle)?.ino;
        let attrs = self.fs().inodes[&ino].attrs();
        Ok(Attrs { id, attrs })
    }

    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let user = self.config.user.clone();
        let mut fs = self.fs();
        let path = fs.resolve(HOME, &path, true, false, 0)?;
        let ino = fs.ino(&path).ok_or(StatusCode::NoSuchFile)?;
        user.apply(fs.inodes.get_mut(&ino).unwrap(), &attrs)?;
        Ok(ok(id))
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        handle: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let ino = self.open_file(&handle)?.ino;
        let user = self.config.user.clone();
        user.apply(self.fs().inodes.get_mut(&ino).unwrap(), &attrs)?;
        Ok(ok(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let mut fs = self.fs();
        let path = fs.resolve(HOME, &filename, false, false, 0)?;
        if fs.node(&path).map(|node| &node.kind) == Some(&Kind::Dir) {
            return Err(StatusCode::Failure);
        }
        fs.dir_writable(&self.config.user, &parent_of(&path))?;
        fs.entries.remove(&path);
        Ok(ok(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        self.rename_entry(&oldpath, &newpath, false)?;
        Ok(ok(id))
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let fs = self.fs();
        let path = fs.resolve(HOME, &path, false, false, 0)?;
        match &fs.node(&path).ok_or(StatusCode::NoSuchFile)?.kind {
            Kind::Symlink(target) => Ok(Name {
                id,
                files: vec![File::dummy(target.clone())],
            }),
            _ => Err(StatusCode::Failure),
        }
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let path = self.fs().resolve(HOME, &path, true, true, 0)?;
        Ok(Name {
            id,
            files: vec![File::dummy(path)],
        })
    }

    async fn extended(
        &mut self,
        id: u32,
        request: String,
        data: Vec<u8>,
    ) -> Result<Packet, Self::Error> {
        match request.as_str() {
            "limits@openssh.com" => {
                let limits = self.config.limits.ok_or(StatusCode::OpUnsupported)?;
                let data = limits
                    .iter()
                    .flat_map(|value| value.to_be_bytes())
                    .collect();
                Ok(Packet::ExtendedReply(ExtendedReply { id, data }))
            }
            "posix-rename@openssh.com" if self.config.posix_rename => {
                let (old, new) = parse_string_pair(&data).ok_or(StatusCode::BadMessage)?;
                self.rename_entry(&old, &new, true)?;
                Ok(Packet::Status(ok(id)))
            }
            _ => Err(StatusCode::OpUnsupported),
        }
    }
}

/// 服务器 → 客户端方向的延迟线:每个应答排队 `delay` 后才写回 channel,服务器照常处理
/// 下一个请求 —— 模拟高 RTT 链路。顺带统计「已应答、还没送达」的最大个数:它就是客户端
/// 同时在途的请求数(客户端不流水线时恒为 1)。
struct DelayLine {
    tx: mpsc::UnboundedSender<(tokio::time::Instant, Vec<u8>)>,
    delay: Duration,
    shared: Arc<Shared>,
}

impl AsyncWrite for DelayLine {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let stats = &self.shared.stats;
        let queued = stats.queued.fetch_add(1, Ordering::SeqCst) + 1;
        let outstanding = queued.saturating_sub(stats.delivered.load(Ordering::SeqCst));
        stats
            .max_outstanding
            .fetch_max(outstanding, Ordering::SeqCst);
        let _ = self
            .tx
            .send((tokio::time::Instant::now() + self.delay, buf.to_vec()));
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

async fn serve_sftp(
    stream: russh::ChannelStream<Msg>,
    handler: SftpHandler,
    delay: Option<Duration>,
    shared: Arc<Shared>,
) {
    let Some(delay) = delay else {
        russh_sftp::server::run(stream, handler).await;
        return;
    };
    let (reader, mut writer) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::unbounded_channel::<(tokio::time::Instant, Vec<u8>)>();
    let forward = shared.clone();
    tokio::spawn(async move {
        while let Some((due, bytes)) = rx.recv().await {
            tokio::time::sleep_until(due).await;
            if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
                break;
            }
            forward.stats.delivered.fetch_add(1, Ordering::SeqCst);
        }
        let _ = writer.shutdown().await;
    });
    let line = DelayLine { tx, delay, shared };
    russh_sftp::server::run(tokio::io::join(reader, line), handler).await;
}

// ============================================================================
// 进程内 SSH 服务器
// ============================================================================

struct SshHandler {
    config: SftpConfig,
    shared: Arc<Shared>,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl server::Handler for SshHandler {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::reject())
    }

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if password == "pw" {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.channels.insert(channel.id(), channel);
        Ok(true)
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.channels.remove(&channel_id) {
            Some(channel) if name == "sftp" => {
                session.channel_success(channel_id)?;
                let handler = SftpHandler {
                    config: self.config.clone(),
                    shared: self.shared.clone(),
                    handles: HashMap::new(),
                    next_handle: 0,
                };
                serve_sftp(
                    channel.into_stream(),
                    handler,
                    self.config.reply_delay,
                    self.shared.clone(),
                )
                .await;
            }
            _ => session.channel_failure(channel_id)?,
        }
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.remove(&channel);
        session.channel_success(channel)?;
        let reply = format!("exec:{}", String::from_utf8_lossy(data));
        session.data(channel, reply.into_bytes())?;
        session.exit_status_request(channel, 0)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

struct TestServer {
    port: u16,
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn start(config: SftpConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ssh_config = Arc::new(server::Config {
            keys: vec![ed25519(201)],
            auth_rejection_time: Duration::from_millis(5),
            auth_rejection_time_initial: Some(Duration::ZERO),
            methods: MethodSet::from(&[MethodKind::Password][..]),
            inactivity_timeout: None,
            ..Default::default()
        });
        let shared = Arc::new(Shared {
            fs: Mutex::new(Fs::new()),
            stats: Stats::default(),
            faults: Mutex::new(Vec::new()),
        });
        let task = {
            let shared = shared.clone();
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    let handler = SshHandler {
                        config: config.clone(),
                        shared: shared.clone(),
                        channels: HashMap::new(),
                    };
                    let ssh_config = ssh_config.clone();
                    tokio::spawn(async move {
                        if let Ok(session) = server::run_stream(ssh_config, socket, handler).await {
                            let _ = session.await;
                        }
                    });
                }
            })
        };
        Self { port, shared, task }
    }

    fn fs(&self) -> MutexGuard<'_, Fs> {
        self.shared.fs.lock().unwrap()
    }

    fn stats(&self) -> &Stats {
        &self.shared.stats
    }

    fn put(&self, path: &str, kind: Kind, mode: u32, uid: u32, gid: u32) {
        self.fs().insert(
            path,
            Inode {
                kind,
                mode,
                uid,
                gid,
            },
        );
    }

    fn put_file(&self, path: &str, data: &[u8], mode: u32, uid: u32, gid: u32) {
        self.put(path, Kind::File(data.to_vec()), mode, uid, gid);
    }

    fn put_symlink(&self, path: &str, target: &str) {
        self.put(path, Kind::Symlink(target.into()), 0o777, UID, GID);
    }

    fn put_dir(&self, path: &str) {
        self.put(path, Kind::Dir, 0o755, UID, GID);
    }

    fn node(&self, path: &str) -> Option<Inode> {
        self.fs().node(path).cloned()
    }

    fn ino(&self, path: &str) -> Option<u64> {
        self.fs().ino(path)
    }

    fn data(&self, path: &str) -> Option<Vec<u8>> {
        match self.node(path)?.kind {
            Kind::File(data) => Some(data),
            _ => None,
        }
    }

    fn names(&self, dir: &str) -> Vec<String> {
        self.fs().names(dir)
    }

    /// 目录里 mini-term 留下的临时 / 备份文件。
    fn leftovers(&self, dir: &str) -> Vec<String> {
        self.names(dir)
            .into_iter()
            .filter(|name| name.contains(".mt-"))
            .collect()
    }

    fn renames(&self) -> Vec<String> {
        self.stats().renames.lock().unwrap().clone()
    }

    fn fail_renames(&self, posix: bool, from: &str, to: &str) {
        self.shared.faults.lock().unwrap().push(RenameFault {
            posix,
            from: from.into(),
            to: to.into(),
        });
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ============================================================================
// 客户端
// ============================================================================

struct Client {
    pool: SshPool,
    conn: SshConnection,
    dir: TempDir,
}

impl Client {
    async fn connect(server: &TestServer, name: &str) -> (Self, Arc<CachedSession>) {
        let dir = TempDir::new(name);
        let pool = SshPool::with_env(
            PoolConfig {
                connect_timeout: Duration::from_secs(10),
                ..PoolConfig::default()
            },
            SshClientEnv::isolated(dir.path("known_hosts")),
        );
        let conn = SshConnection {
            id: name.into(),
            name: name.into(),
            host: "127.0.0.1".into(),
            port: server.port,
            user: "u".into(),
            password: Some("pw".into()),
            identity_file: None,
            group: None,
            extra: Default::default(),
        };
        let session = pool.try_acquire(&conn).await.expect("connects");
        (Self { pool, conn, dir }, session)
    }

    fn write_local(&self, name: &str, data: &[u8]) -> PathBuf {
        let path = self.dir.path(name);
        std::fs::write(&path, data).unwrap();
        path
    }

    /// 本地目录里没传完的下载残留。
    fn partials(&self) -> Vec<String> {
        self.dir
            .names()
            .into_iter()
            .filter(|name| name.contains("mt-sftp-partial"))
            .collect()
    }
}

async fn download(
    session: &CachedSession,
    remote: &str,
    local: &Path,
) -> Result<u64, SftpTransferError> {
    let local = local.to_str().unwrap();
    tokio::time::timeout(
        Duration::from_secs(60),
        mt_ssh::run_sftp_download_on_session(session, remote, local, TRANSFER_TIMEOUT),
    )
    .await
    .expect("download must not hang")
}

async fn upload(
    session: &CachedSession,
    local: &Path,
    remote: &str,
) -> Result<u64, SftpTransferError> {
    let local = local.to_str().unwrap();
    tokio::time::timeout(
        Duration::from_secs(60),
        mt_ssh::run_sftp_upload_on_session(session, local, remote, TRANSFER_TIMEOUT),
    )
    .await
    .expect("upload must not hang")
}

async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 从错误信息里取出 `'<前缀>…'` 引起来的路径。
fn quoted_path_after<'a>(message: &'a str, marker: &str) -> &'a str {
    let start = message
        .find(marker)
        .unwrap_or_else(|| panic!("{marker} in {message}"))
        + marker.len();
    let rest = &message[start..];
    &rest[..rest.find('\'').unwrap()]
}

// ============================================================================
// B2-2 下载流水线
// ============================================================================

/// 高 RTT 下多个读请求同时在途,结果按偏移顺序拼回原文件;已存在的本地文件被原子替换。
#[tokio::test]
async fn download_keeps_many_reads_in_flight_and_reassembles_them_in_order() {
    let delay = Duration::from_millis(50);
    let server = TestServer::start(SftpConfig {
        reply_delay: Some(delay),
        ..SftpConfig::default()
    })
    .await;
    let content = pattern(2 * 1024 * 1024 + 777, 7);
    server.put_file("/home/u/big.bin", &content, 0o644, UID, GID);
    let (client, session) = Client::connect(&server, "pipelined-download").await;
    let local = client.write_local("big.bin", b"stale local copy");

    let started = Instant::now();
    let copied = download(&session, "/home/u/big.bin", &local).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(copied, content.len() as u64);
    assert!(std::fs::read(&local).unwrap() == content, "content differs");
    let outstanding = server.stats().max_outstanding.load(Ordering::SeqCst);
    assert!(
        outstanding >= 8,
        "only {outstanding} requests were in flight"
    );
    // 一个请求一个往返要 65 × 50ms 以上;流水线只要几个往返。
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    assert!(client.partials().is_empty(), "{:?}", client.dir.names());
    client.pool.shutdown().await;
}

/// 服务器每次只给 10000 字节(没通告上限):每个短读都从实际位置补读;文件大小恰为块的
/// 整数倍、空文件都能正确收尾。
#[tokio::test]
async fn download_handles_short_reads_eof_and_exact_chunk_sizes() {
    let server = TestServer::start(SftpConfig {
        max_read: Some(10_000),
        ..SftpConfig::default()
    })
    .await;
    let cases = [
        ("odd.bin", pattern(100_003, 1)),
        ("exact.bin", pattern(3 * 32 * 1024, 2)),
        ("empty.bin", Vec::new()),
        ("larger.bin", pattern(1_500_001, 3)),
    ];
    for (name, content) in &cases {
        server.put_file(&format!("/home/u/{name}"), content, 0o644, UID, GID);
    }
    let (client, session) = Client::connect(&server, "short-reads").await;
    for (name, content) in &cases {
        let local = client.dir.path(name);
        let copied = download(&session, &format!("/home/u/{name}"), &local)
            .await
            .unwrap();
        assert_eq!(copied, content.len() as u64, "{name}");
        assert!(std::fs::read(&local).unwrap() == *content, "{name} differs");
    }
    assert!(client.partials().is_empty(), "{:?}", client.dir.names());
    client.pool.shutdown().await;
}

/// 通告了 `limits@openssh.com`:读请求不超过服务器给的读上限;没有短读时,大小恰为块的
/// 整数倍与空文件同样正确收尾。
#[tokio::test]
async fn download_respects_the_advertised_read_limit() {
    let server = TestServer::start(SftpConfig {
        limits: Some([64 * 1024, 16 * 1024, 16 * 1024, 64]),
        ..SftpConfig::default()
    })
    .await;
    let cases = [
        ("limited.bin", pattern(200_000, 4)),
        ("exact.bin", pattern(4 * 16 * 1024, 5)),
        ("empty.bin", Vec::new()),
    ];
    for (name, content) in &cases {
        server.put_file(&format!("/home/u/{name}"), content, 0o644, UID, GID);
    }
    let (client, session) = Client::connect(&server, "limits").await;
    for (name, content) in &cases {
        let local = client.dir.path(name);
        let copied = download(&session, &format!("/home/u/{name}"), &local)
            .await
            .unwrap();
        assert_eq!(copied, content.len() as u64, "{name}");
        assert!(std::fs::read(&local).unwrap() == *content, "{name} differs");
    }
    let largest = server.stats().max_read_len.load(Ordering::SeqCst);
    assert!(largest <= 16 * 1024, "read request of {largest} bytes");
    client.pool.shutdown().await;
}

/// 远端不存在 / 是目录:业务错,本地已有的文件原样,不留临时文件。
#[tokio::test]
async fn failed_downloads_leave_the_local_target_alone() {
    let server = TestServer::start(SftpConfig::default()).await;
    server.put_dir("/srv");
    let (client, session) = Client::connect(&server, "download-errors").await;
    let local = client.write_local("keep.txt", b"keep me");

    let error = download(&session, "/home/u/missing.txt", &local)
        .await
        .unwrap_err();
    assert!(!error.is_transport(), "{error}");
    assert!(
        error.message().contains("sftp open '/home/u/missing.txt'"),
        "{error}"
    );

    let error = download(&session, "/srv", &local).await.unwrap_err();
    assert!(!error.is_transport(), "{error}");
    assert!(error.message().contains("is a directory"), "{error}");

    assert_eq!(std::fs::read(&local).unwrap(), b"keep me");
    assert!(client.partials().is_empty(), "{:?}", client.dir.names());
    client.pool.shutdown().await;
}

// ============================================================================
// B2-3 上传:属性、软链、替换
// ============================================================================

/// 替换已存在的文件:posix-rename 原子覆盖(新 inode),权限与属主不变,setuid 写完补回;
/// 临时文件一出生就是目标的权限(0600 的文件新内容不会先以 0644 落地)。
#[tokio::test]
async fn upload_replaces_atomically_and_keeps_mode_and_owner() {
    let server = TestServer::start(SftpConfig::default()).await;
    server.put_file("/home/u/secret.env", b"old", 0o600, UID, GID);
    server.put_file("/home/u/tool", b"#!/bin/sh\n", 0o4755, UID, GID);
    let (client, session) = Client::connect(&server, "replace").await;
    let before = server.ino("/home/u/secret.env").unwrap();

    let content = pattern(300_000, 5);
    let local = client.write_local("secret.env", &content);
    assert_eq!(
        upload(&session, &local, "/home/u/secret.env")
            .await
            .unwrap(),
        content.len() as u64
    );
    let node = server.node("/home/u/secret.env").unwrap();
    assert!(server.data("/home/u/secret.env").unwrap() == content);
    assert_eq!((node.mode, node.uid, node.gid), (0o600, UID, GID));
    assert_ne!(
        server.ino("/home/u/secret.env").unwrap(),
        before,
        "replaced, not rewritten"
    );
    let created = server.stats().created.lock().unwrap().clone();
    assert!(
        created
            .iter()
            .any(|(path, mode)| path.contains("/home/u/.secret.env.mt-partial-") && *mode == 0o600),
        "{created:?}"
    );
    let renames = server.renames();
    assert_eq!(renames.len(), 1, "{renames:?}");
    assert!(renames[0].starts_with("posix-rename /home/u/.secret.env.mt-partial-"));

    let local = client.write_local("tool", b"#!/bin/sh\necho new\n");
    upload(&session, &local, "/home/u/tool").await.unwrap();
    assert_eq!(server.node("/home/u/tool").unwrap().mode, 0o4755);

    // 新路径(含相对路径,相对 SFTP 初始目录)照旧新建,权限按服务器默认。
    let local = client.write_local("new.txt", b"fresh");
    upload(&session, &local, "/home/u/new.txt").await.unwrap();
    upload(&session, &local, "relative.txt").await.unwrap();
    assert_eq!(server.data("/home/u/new.txt").unwrap(), b"fresh");
    assert_eq!(server.data("/home/u/relative.txt").unwrap(), b"fresh");
    assert_eq!(server.node("/home/u/new.txt").unwrap().mode, 0o644);
    assert!(
        server.leftovers(HOME).is_empty(),
        "{:?}",
        server.names(HOME)
    );
    client.pool.shutdown().await;
}

/// 属组能对上(chgrp 到自己所在的组)就照常替换;属主是别人、换不过来时改为原地覆盖写,
/// 文件的 inode、属主、属组、权限都不变。
#[tokio::test]
async fn upload_keeps_the_group_and_overwrites_foreign_files_in_place() {
    let server = TestServer::start(SftpConfig {
        user: User::normal(&[WWW]),
        ..SftpConfig::default()
    })
    .await;
    server.put_file("/home/u/shared.conf", b"old", 0o640, UID, WWW);
    server.put_file("/home/u/site.conf", b"old site", 0o664, WWW, WWW);
    let (client, session) = Client::connect(&server, "ownership").await;

    let before = server.ino("/home/u/shared.conf").unwrap();
    let local = client.write_local("shared.conf", b"new shared");
    upload(&session, &local, "/home/u/shared.conf")
        .await
        .unwrap();
    let node = server.node("/home/u/shared.conf").unwrap();
    assert_eq!(server.data("/home/u/shared.conf").unwrap(), b"new shared");
    assert_eq!((node.mode, node.uid, node.gid), (0o640, UID, WWW));
    assert_ne!(server.ino("/home/u/shared.conf").unwrap(), before);

    let before = server.ino("/home/u/site.conf").unwrap();
    let renames_before = server.renames().len();
    let content = pattern(500_000, 6);
    let local = client.write_local("site.conf", &content);
    upload(&session, &local, "/home/u/site.conf").await.unwrap();
    let node = server.node("/home/u/site.conf").unwrap();
    assert!(server.data("/home/u/site.conf").unwrap() == content);
    assert_eq!((node.mode, node.uid, node.gid), (0o664, WWW, WWW));
    assert_eq!(server.ino("/home/u/site.conf").unwrap(), before, "in place");
    assert_eq!(
        server.renames().len(),
        renames_before,
        "{:?}",
        server.renames()
    );
    assert!(
        server.leftovers(HOME).is_empty(),
        "{:?}",
        server.names(HOME)
    );
    client.pool.shutdown().await;
}

/// 别人的文件、自己既改不了属主也写不了它:报错,文件原样,不留临时文件。
#[tokio::test]
async fn upload_refuses_to_touch_a_foreign_file_it_cannot_write() {
    let server = TestServer::start(SftpConfig::default()).await;
    server.put_file("/home/u/readonly.conf", b"old", 0o644, WWW, WWW);
    let (client, session) = Client::connect(&server, "foreign").await;
    let local = client.write_local("readonly.conf", b"new");
    let error = upload(&session, &local, "/home/u/readonly.conf")
        .await
        .unwrap_err();
    assert!(!error.is_transport(), "{error}");
    assert!(error.message().contains("left untouched"), "{error}");
    assert!(error.message().contains("33:33"), "{error}");
    assert_eq!(server.data("/home/u/readonly.conf").unwrap(), b"old");
    assert!(
        server.leftovers(HOME).is_empty(),
        "{:?}",
        server.names(HOME)
    );
    client.pool.shutdown().await;
}

/// 目录不让建文件、文件本身组可写(如 /etc 下的配置):像 scp 一样原地写;文件也不可写
/// 时报错、不动它;新文件在这种目录里建不了,照实报错。
#[tokio::test]
async fn upload_writes_in_place_when_no_staging_copy_can_be_created() {
    let server = TestServer::start(SftpConfig {
        user: User::normal(&[WWW]),
        ..SftpConfig::default()
    })
    .await;
    server.put("/srv", Kind::Dir, 0o755, 0, 0);
    server.put("/srv/app.conf", Kind::File(b"old".to_vec()), 0o664, 0, WWW);
    server.put(
        "/srv/locked.conf",
        Kind::File(b"old".to_vec()),
        0o644,
        0,
        WWW,
    );
    let (client, session) = Client::connect(&server, "locked-dir").await;
    let local = client.write_local("app.conf", b"new");

    let before = server.ino("/srv/app.conf").unwrap();
    upload(&session, &local, "/srv/app.conf").await.unwrap();
    let node = server.node("/srv/app.conf").unwrap();
    assert_eq!(server.data("/srv/app.conf").unwrap(), b"new");
    assert_eq!((node.mode, node.uid, node.gid), (0o664, 0, WWW));
    assert_eq!(server.ino("/srv/app.conf").unwrap(), before, "in place");

    let error = upload(&session, &local, "/srv/locked.conf")
        .await
        .unwrap_err();
    assert!(
        error.message().contains("no staging copy can be created"),
        "{error}"
    );
    assert!(error.message().contains("left untouched"), "{error}");
    assert_eq!(server.data("/srv/locked.conf").unwrap(), b"old");

    let error = upload(&session, &local, "/srv/new.conf").await.unwrap_err();
    assert!(error.message().contains("sftp create"), "{error}");
    assert!(server.node("/srv/new.conf").is_none());
    assert!(
        server.leftovers("/srv").is_empty(),
        "{:?}",
        server.names("/srv")
    );
    client.pool.shutdown().await;
}

/// root 登录:属主属组整个对齐后照常替换。
#[tokio::test]
async fn upload_as_root_restores_the_original_owner() {
    let server = TestServer::start(SftpConfig {
        user: User::root(),
        ..SftpConfig::default()
    })
    .await;
    server.put_file("/home/u/app.conf", b"old", 0o640, WWW, WWW);
    let (client, session) = Client::connect(&server, "root").await;
    let before = server.ino("/home/u/app.conf").unwrap();
    let local = client.write_local("app.conf", b"new");
    upload(&session, &local, "/home/u/app.conf").await.unwrap();
    let node = server.node("/home/u/app.conf").unwrap();
    assert_eq!(server.data("/home/u/app.conf").unwrap(), b"new");
    assert_eq!((node.mode, node.uid, node.gid), (0o640, WWW, WWW));
    assert_ne!(server.ino("/home/u/app.conf").unwrap(), before);
    client.pool.shutdown().await;
}

/// 软链:写到最终文件(临时文件建在那个文件的目录里),软链保持软链;链式软链同理;
/// 悬空软链在它指向的位置新建文件;成环与指向目录的软链报错。
#[tokio::test]
async fn upload_through_symlinks_writes_the_final_file_and_keeps_the_links() {
    let server = TestServer::start(SftpConfig::default()).await;
    server.put_dir("/srv");
    server.put_dir("/srv/data");
    server.put_file("/srv/data/real.txt", b"old", 0o640, UID, GID);
    server.put_symlink("/home/u/link.txt", "../../srv/data/real.txt");
    server.put_symlink("/home/u/a", "b");
    server.put_symlink("/home/u/b", "c.txt");
    server.put_file("/home/u/c.txt", b"old c", 0o600, UID, GID);
    server.put_symlink("/home/u/dangling", "created-by-upload.txt");
    server.put_symlink("/home/u/loop1", "loop2");
    server.put_symlink("/home/u/loop2", "loop1");
    server.put_symlink("/home/u/dirlink", "/srv/data");
    let (client, session) = Client::connect(&server, "symlinks").await;
    let local = client.write_local("payload", b"through the link");

    upload(&session, &local, "/home/u/link.txt").await.unwrap();
    assert_eq!(
        server.data("/srv/data/real.txt").unwrap(),
        b"through the link"
    );
    assert_eq!(server.node("/srv/data/real.txt").unwrap().mode, 0o640);
    assert_eq!(
        server.node("/home/u/link.txt").unwrap().kind,
        Kind::Symlink("../../srv/data/real.txt".into())
    );
    assert_eq!(server.names("/srv/data"), ["real.txt"]);

    upload(&session, &local, "/home/u/a").await.unwrap();
    assert_eq!(server.data("/home/u/c.txt").unwrap(), b"through the link");
    assert_eq!(
        server.node("/home/u/a").unwrap().kind,
        Kind::Symlink("b".into())
    );
    assert_eq!(
        server.node("/home/u/b").unwrap().kind,
        Kind::Symlink("c.txt".into())
    );

    upload(&session, &local, "/home/u/dangling").await.unwrap();
    assert_eq!(
        server.data("/home/u/created-by-upload.txt").unwrap(),
        b"through the link"
    );
    assert_eq!(
        server.node("/home/u/dangling").unwrap().kind,
        Kind::Symlink("created-by-upload.txt".into())
    );

    let error = upload(&session, &local, "/home/u/loop1").await.unwrap_err();
    assert!(
        error
            .message()
            .contains("too many levels of symbolic links"),
        "{error}"
    );
    let error = upload(&session, &local, "/home/u/dirlink")
        .await
        .unwrap_err();
    assert!(error.message().contains("is a directory"), "{error}");
    assert!(error.message().contains("/srv/data"), "{error}");

    assert!(
        server.leftovers(HOME).is_empty(),
        "{:?}",
        server.names(HOME)
    );
    assert!(server.leftovers("/srv/data").is_empty());
    client.pool.shutdown().await;
}

/// 没有 posix-rename 的服务器:「目标 → 备份、临时 → 目标、删备份」;就位失败回滚,
/// 回滚也失败时原文件留在备份名、上传的数据留在临时名,两个路径都写进错误。
#[tokio::test]
async fn backup_swap_never_loses_the_original() {
    let server = TestServer::start(SftpConfig {
        posix_rename: false,
        ..SftpConfig::default()
    })
    .await;
    for name in ["plain.txt", "a.txt", "b.txt", "c.txt"] {
        server.put_file(&format!("/home/u/{name}"), b"old", 0o600, UID, GID);
    }
    let (client, session) = Client::connect(&server, "backup-swap").await;
    let local = client.write_local("payload", b"uploaded");

    // 正常路径。
    upload(&session, &local, "/home/u/plain.txt").await.unwrap();
    assert_eq!(server.data("/home/u/plain.txt").unwrap(), b"uploaded");
    assert_eq!(server.node("/home/u/plain.txt").unwrap().mode, 0o600);
    let renames = server.renames();
    assert_eq!(renames.len(), 2, "{renames:?}");
    assert!(renames[0].starts_with("rename /home/u/plain.txt -> /home/u/.plain.txt.mt-backup-"));
    assert!(renames[1].starts_with("rename /home/u/.plain.txt.mt-partial-"));
    assert!(renames[1].ends_with(" -> /home/u/plain.txt"));

    // 就位失败:原文件挪回去。
    server.fail_renames(false, ".mt-partial-", "/home/u/a.txt");
    let error = upload(&session, &local, "/home/u/a.txt").await.unwrap_err();
    assert!(
        error.message().contains("the original was restored"),
        "{error}"
    );
    assert_eq!(server.data("/home/u/a.txt").unwrap(), b"old");

    // 先挪原文件就失败:什么都没动。
    server.fail_renames(false, "/home/u/c.txt", ".mt-backup-");
    let error = upload(&session, &local, "/home/u/c.txt").await.unwrap_err();
    assert!(
        error.message().contains("cannot move the original"),
        "{error}"
    );
    assert_eq!(server.data("/home/u/c.txt").unwrap(), b"old");
    assert!(
        server.leftovers(HOME).is_empty(),
        "{:?}",
        server.names(HOME)
    );

    // 就位与回滚都失败:两份数据都还在,路径写进错误。
    server.fail_renames(false, ".mt-partial-", "/home/u/b.txt");
    server.fail_renames(false, ".mt-backup-", "/home/u/b.txt");
    let error = upload(&session, &local, "/home/u/b.txt").await.unwrap_err();
    let message = error.message();
    let backup = quoted_path_after(message, "the original is at '");
    let staging = quoted_path_after(message, "the uploaded data at '");
    assert!(backup.contains("/home/u/.b.txt.mt-backup-"), "{message}");
    assert!(staging.contains("/home/u/.b.txt.mt-partial-"), "{message}");
    assert_eq!(server.data(backup).unwrap(), b"old");
    assert_eq!(server.data(staging).unwrap(), b"uploaded");
    assert!(server.node("/home/u/b.txt").is_none());
    client.pool.shutdown().await;
}

/// posix-rename 失败:原子操作没发生,原文件原样,临时文件删掉。
#[tokio::test]
async fn failed_posix_rename_keeps_the_original_and_drops_the_staging_file() {
    let server = TestServer::start(SftpConfig::default()).await;
    server.put_file("/home/u/d.txt", b"old", 0o600, UID, GID);
    server.fail_renames(true, ".mt-partial-", "/home/u/d.txt");
    let (client, session) = Client::connect(&server, "posix-fail").await;
    let local = client.write_local("payload", b"uploaded");
    let error = upload(&session, &local, "/home/u/d.txt").await.unwrap_err();
    assert!(!error.is_transport(), "{error}");
    assert!(
        error
            .message()
            .contains("cannot move the upload into place"),
        "{error}"
    );
    assert_eq!(server.data("/home/u/d.txt").unwrap(), b"old");
    assert!(
        server.leftovers(HOME).is_empty(),
        "{:?}",
        server.names(HOME)
    );
    client.pool.shutdown().await;
}

/// 同一路径的两次并发上传各写各的临时文件:结果是其中一份完整内容,不会拼成一半一半。
#[tokio::test]
async fn concurrent_uploads_to_the_same_path_do_not_mix() {
    let server = TestServer::start(SftpConfig {
        reply_delay: Some(Duration::from_millis(20)),
        ..SftpConfig::default()
    })
    .await;
    let (client, session) = Client::connect(&server, "race").await;
    let first = pattern(400_000, 8);
    let second = pattern(400_000, 9);
    let first_path = client.write_local("first", &first);
    let second_path = client.write_local("second", &second);
    let (a, b) = tokio::join!(
        upload(&session, &first_path, "/home/u/race.bin"),
        upload(&session, &second_path, "/home/u/race.bin"),
    );
    a.unwrap();
    b.unwrap();
    let result = server.data("/home/u/race.bin").unwrap();
    assert!(result == first || result == second, "contents were mixed");
    assert!(
        server.leftovers(HOME).is_empty(),
        "{:?}",
        server.names(HOME)
    );
    client.pool.shutdown().await;
}

// ============================================================================
// B2-1 传输不再占着会话锁
// ============================================================================

/// 传输卡在服务器上(读 / 写都被闸门挡住)时,同一连接上的 acquire 与 exec 照常完成。
async fn exec_completes_while_a_transfer_is_stuck(upload_direction: bool) {
    let gate = Arc::new(Semaphore::new(0));
    let server = TestServer::start(SftpConfig {
        gate: Some(gate.clone()),
        ..SftpConfig::default()
    })
    .await;
    let content = pattern(1024 * 1024, 10);
    server.put_file("/home/u/blocked.bin", &content, 0o644, UID, GID);
    let (client, session) = Client::connect(&server, "not-blocked").await;
    let local = if upload_direction {
        client.write_local("upload.bin", &content)
    } else {
        client.dir.path("download.bin")
    };

    let transfer = {
        let session = session.clone();
        let local = local.clone();
        tokio::spawn(async move {
            if upload_direction {
                upload(&session, &local, "/home/u/uploaded.bin").await
            } else {
                download(&session, "/home/u/blocked.bin", &local).await
            }
        })
    };
    let stats = &server.shared.stats;
    wait_until("the transfer to reach the server", || {
        stats.reads.load(Ordering::SeqCst) + stats.writes.load(Ordering::SeqCst) > 0
    })
    .await;

    let output = tokio::time::timeout(Duration::from_secs(5), async {
        let again = client.pool.try_acquire(&client.conn).await.unwrap();
        assert!(
            Arc::ptr_eq(&again, &session),
            "the cached session is reused"
        );
        mt_ssh::run_bounded_exec_on_session(&again, "ping", Duration::from_secs(5), 1024)
            .await
            .unwrap()
    })
    .await
    .expect("acquire + exec must not wait for the transfer");
    assert_eq!(output.stdout, b"exec:ping");
    assert!(
        !transfer.is_finished(),
        "the transfer is still held at the gate"
    );

    gate.add_permits(1_000_000);
    let copied = transfer.await.unwrap().unwrap();
    assert_eq!(copied, content.len() as u64);
    if upload_direction {
        assert!(server.data("/home/u/uploaded.bin").unwrap() == content);
    } else {
        assert!(std::fs::read(&local).unwrap() == content);
    }
    client.pool.shutdown().await;
}

#[tokio::test]
async fn download_in_progress_does_not_block_the_connection() {
    exec_completes_while_a_transfer_is_stuck(false).await;
}

#[tokio::test]
async fn upload_in_progress_does_not_block_the_connection() {
    exec_completes_while_a_transfer_is_stuck(true).await;
}

/// 外层超时丢弃传输:本地临时文件当场删掉、已有的本地文件不动;远端临时文件在后台删掉。
#[tokio::test]
async fn cancelled_transfers_clean_up_their_temporary_files() {
    let gate = Arc::new(Semaphore::new(0));
    let server = TestServer::start(SftpConfig {
        gate: Some(gate.clone()),
        ..SftpConfig::default()
    })
    .await;
    server.put_file("/home/u/slow.bin", &pattern(100_000, 11), 0o644, UID, GID);
    let (client, session) = Client::connect(&server, "cancel").await;

    let local = client.write_local("slow.bin", b"keep me");
    let target = local.to_str().unwrap().to_string();
    let cancelled = tokio::time::timeout(
        Duration::from_millis(300),
        mt_ssh::run_sftp_download_on_session(
            &session,
            "/home/u/slow.bin",
            &target,
            TRANSFER_TIMEOUT,
        ),
    )
    .await;
    assert!(cancelled.is_err(), "the download should still be stuck");
    assert!(client.partials().is_empty(), "{:?}", client.dir.names());
    assert_eq!(std::fs::read(&local).unwrap(), b"keep me");

    let source = client.write_local("upload.bin", &pattern(100_000, 12));
    let source = source.to_str().unwrap().to_string();
    let cancelled = tokio::time::timeout(
        Duration::from_millis(300),
        mt_ssh::run_sftp_upload_on_session(
            &session,
            &source,
            "/home/u/cancelled.bin",
            TRANSFER_TIMEOUT,
        ),
    )
    .await;
    assert!(cancelled.is_err(), "the upload should still be stuck");
    gate.add_permits(1_000_000);
    wait_until("the staging file to be removed", || {
        server.leftovers(HOME).is_empty()
    })
    .await;
    assert!(server.node("/home/u/cancelled.bin").is_none());
    client.pool.shutdown().await;
}
