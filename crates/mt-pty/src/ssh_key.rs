//! SSH 私钥临时副本与权限收紧。
//!
//! OpenSSH 客户端(`ssh -i <key>`)会拒绝权限过于开放的私钥文件
//! (`WARNING: UNPROTECTED PRIVATE KEY FILE!`)。连接时把用户私钥复制到一份
//! 仅当前用户可读写的临时副本,用 `ssh -i <临时副本>` 连接,不修改用户原始密钥文件。
//!
//! **仅供主程序通过 PTY 拉起系统 `ssh` 客户端的两条路径使用**:
//! - 终端右键「SSH 连接」(`mt-app::pane::connect_ssh`)
//! - SSH 远程项目启动器(`mt-remote::prepare_remote_launch`)
//!
//! `mt-ssh` / sidecar 走进程内 `russh` 直接读私钥字节,不经过本模块。
//!
//! # 为什么不用 `icacls /inheritance:r /grant:r <USERNAME>:F`(Windows)
//!
//! `icacls /inheritance:r` **只会删除带 `(I)` 标记的继承 ACE**,不会删除已有的显式 ACE;
//! `/grant:r <user>:F` 也只会替换该用户自己的显式 ACE。当 `%TEMP%` 存在第三方工具
//! (如 Codex Windows Sandbox 的 `CodexSandboxUsers`)写入的双重可继承 ACE 或显式 ACE 时,
//! NTFS 会在新建文件上留下不带 `(I)` 标记的显式 ACE,`icacls` 根本清不掉,导致 Windows
//! OpenSSH 直接拒绝加载临时副本。
//!
//! 因此 Windows 侧改用 Win32 API:
//! 1. 通过当前进程 token(`GetTokenInformation(TokenUser)`)取真实用户 SID,不依赖
//!    `%USERNAME%` 环境变量;
//! 2. 创建临时文件时直接传入 `SECURITY_ATTRIBUTES`(`O:<sid>D:P(A;;FA;;;<sid>)`),
//!    从文件诞生的第一刻就阻断继承、仅授权当前用户,且提权运行时 owner 也不会漂移到
//!    `Administrators`;
//! 3. `restrict_permissions` 调 `SetNamedSecurityInfoW` 带
//!    `DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION` **整张覆写** DACL,
//!    连根清除一切显式 / 继承 / 重复 ACE。
//!
//! # Unix 侧加固(macOS / Linux)
//!
//! - 临时目录按用户隔离:优先 `$XDG_RUNTIME_DIR/mini-term-ssh-keys`(校验真实目录、属主与
//!   权限),回退 `{temp}/mini-term-ssh-keys-{euid}`,避免 Linux 多用户共享 `/tmp` 冲突;
//! - 目录严格设为 `0700` 并拒绝 symlink / 非当前用户属主;
//! - 副本以 `O_CREAT | O_EXCL` + `0600` 建在同目录临时文件,写入并再 `chmod 0600`
//!   (把 Linux POSIX Default ACL 的 `ACL_MASK` 归零)后原子 `rename` 替换目标文件,
//!   彻底消除先 `copy` 再 `chmod` 的权限暴露窗口。

use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const TEMP_KEYS_DIR_NAME: &str = "mini-term-ssh-keys";

/// 进程内递增序号,用于给同目录临时文件生成唯一后缀,避免多线程并发准备同一把私钥时撞名。
static TEMP_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// 临时私钥目录:
/// - Windows: `{temp}/mini-term-ssh-keys`(Windows `%TEMP%` 本身已按用户隔离)
/// - Unix: 优先安全可用的 `$XDG_RUNTIME_DIR/mini-term-ssh-keys`,
///   否则 `{temp}/mini-term-ssh-keys-{euid}`
pub fn temp_keys_dir() -> PathBuf {
    platform::temp_keys_dir()
}

/// 清理临时私钥目录,启动时调用一次,清除上次遗留的副本。
/// 仿 `clipboard::cleanup_old_files()`:清理失败不 panic,且绝不跟随 symlink。
pub fn cleanup_ssh_temp_keys() {
    platform::cleanup_ssh_temp_keys();
}

/// 收紧临时私钥副本权限,仅当前用户可读写。
///
/// - Windows:用当前进程 `TokenUser` SID 构造受保护 DACL(`D:P(A;;FA;;;<sid>)`),
///   调 `SetNamedSecurityInfoW` **全量覆写**整个 DACL,清除一切继承与显式 ACE。
/// - Unix:设为 `0o600`。
pub fn restrict_permissions(path: &Path) -> Result<(), String> {
    platform::restrict_permissions(path)
}

/// 把私钥复制到权限收紧的临时副本,返回临时副本路径。
///
/// 临时文件名按源路径稳定哈希派生:同一把 key 重连复用/原子替换同一文件,
/// 不无限累积。源文件不存在或不是普通文件直接返回 `Err`。
pub fn prepare_ssh_key(identity_file: &str) -> Result<String, String> {
    let dir = temp_keys_dir();
    prepare_ssh_key_in(identity_file, &dir)
}

/// [`prepare_ssh_key`] 的可注入目录版本(便于测试隔离,不干扰运行中的实例目录)。
fn prepare_ssh_key_in(identity_file: &str, dir: &Path) -> Result<String, String> {
    let src = PathBuf::from(identity_file);
    if !src.is_file() {
        return Err(format!("私钥文件不存在: {identity_file}"));
    }

    platform::ensure_temp_keys_dir(dir)?;

    // 源路径稳定哈希作为文件名,重连时复用同一临时文件
    let mut hasher = DefaultHasher::new();
    identity_file.hash(&mut hasher);
    let key_hash = hasher.finish();
    let dest = dir.join(format!("{key_hash:016x}.key"));

    let mut src_file = File::open(&src).map_err(|e| format!("读取私钥失败: {e}"))?;

    // 在同目录以严格权限新建唯一临时文件 → 流式写入 → 再次收紧权限 → 原子替换目标。
    // 这样既无宽权限暴露窗口、不会把源文件的只读属性或旧目标的脏 DACL 带进来,
    // 也不会在并发重连同一把 key 时把正在读取的目标文件截断写花。
    let seq = TEMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{key_hash:016x}.{}.{seq}.tmp", std::process::id()));

    let write_res = (|| -> Result<(), String> {
        let mut tmp_file = platform::create_restricted_temp_file(&tmp)?;
        std::io::copy(&mut src_file, &mut tmp_file).map_err(|e| format!("复制私钥失败: {e}"))?;
        tmp_file
            .flush()
            .map_err(|e| format!("刷新私钥副本失败: {e}"))?;
        drop(tmp_file);

        // 防御性收口:Windows 抵御极少数文件过滤驱动在 CreateFileW 后改写 DACL;
        // Linux 抵御目录 POSIX Default ACL,确保 ACL_MASK 归零。
        restrict_permissions(&tmp)?;
        replace_file(&tmp, &dest)
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }

    Ok(dest.to_string_lossy().into_owned())
}

/// 把同目录临时文件 `tmp` 原子替换到 `dest`。
fn replace_file(tmp: &Path, dest: &Path) -> Result<(), String> {
    match fs::rename(tmp, dest) {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(first_err) => {
            // 老版本 `std::fs::copy` 会把源文件的只读属性带到旧副本上,导致覆盖 rename
            // 报 AccessDenied。仅在旧 `dest` 是普通文件(非 symlink)时清掉只读位重试一次。
            if let Ok(meta) = fs::symlink_metadata(dest)
                && !meta.file_type().is_symlink()
                && meta.is_file()
                && meta.permissions().readonly()
            {
                let mut perms = meta.permissions();
                // 仅在 cfg(windows) 分支执行:Windows 上只清除 FILE_ATTRIBUTE_READONLY,
                // 不涉及 Unix 的 world-writable 问题。
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                if fs::set_permissions(dest, perms).is_ok() && fs::rename(tmp, dest).is_ok() {
                    return Ok(());
                }
            }
            Err(format!("写入私钥副本失败: {first_err}"))
        }
        #[cfg(not(windows))]
        Err(err) => Err(format!("写入私钥副本失败: {err}")),
    }
}

/// Unix 临时目录选址纯函数(供单测在全平台覆盖)。
#[cfg(any(unix, test))]
fn choose_unix_temp_keys_dir(
    xdg_runtime_dir: Option<&Path>,
    temp_dir: &Path,
    euid: u32,
) -> PathBuf {
    if let Some(runtime_dir) = xdg_runtime_dir {
        return runtime_dir.join(TEMP_KEYS_DIR_NAME);
    }
    temp_dir.join(format!("{TEMP_KEYS_DIR_NAME}-{euid}"))
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::fs::{self, File};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation, IsValidSid,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
        TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::TEMP_KEYS_DIR_NAME;

    pub(super) fn temp_keys_dir() -> PathBuf {
        std::env::temp_dir().join(TEMP_KEYS_DIR_NAME)
    }

    pub(super) fn cleanup_ssh_temp_keys() {
        let dir = temp_keys_dir();
        // 绝不跟随 symlink / junction 删除别处目录
        if let Ok(meta) = fs::symlink_metadata(&dir)
            && !meta.file_type().is_symlink()
            && meta.is_dir()
        {
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// RAII 封装由 `ConvertStringSecurityDescriptorToSecurityDescriptorW` 分配的安全描述符。
    struct OwnedSecurityDescriptor {
        raw: PSECURITY_DESCRIPTOR,
    }

    impl OwnedSecurityDescriptor {
        fn from_sddl(sddl: &str) -> Result<Self, String> {
            let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
            let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: wide 以 NUL 结尾;成功时 psd 由系统 LocalAlloc,在 Drop 中 LocalFree。
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut psd,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || psd.is_null() {
                return Err(format!(
                    "构造安全描述符失败({sddl}): {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(Self { raw: psd })
        }

        /// 取该安全描述符内部的 DACL 指针(在 `self` 存活期间有效)。
        fn dacl(&self) -> Result<*mut ACL, String> {
            let mut present: i32 = 0;
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut defaulted: i32 = 0;
            // SAFETY: self.raw 是有效的自相关安全描述符,返回的 dacl 指向其内部内存。
            let ok = unsafe {
                GetSecurityDescriptorDacl(self.raw, &mut present, &mut dacl, &mut defaulted)
            };
            if ok == 0 || present == 0 || dacl.is_null() {
                return Err(format!(
                    "提取安全描述符 DACL 失败: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(dacl)
        }
    }

    impl Drop for OwnedSecurityDescriptor {
        fn drop(&mut self) {
            if !self.raw.is_null() {
                // SAFETY: raw 来自 ConvertStringSecurityDescriptorToSecurityDescriptorW。
                unsafe {
                    LocalFree(self.raw);
                }
            }
        }
    }

    /// 取当前进程 token 的 `TokenUser` SID 字符串(如 `S-1-5-21-...`)。
    pub(super) fn current_user_sid_string() -> Result<String, String> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: GetCurrentProcess 返回无需关闭的伪句柄。
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(format!(
                "OpenProcessToken 失败: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: token 是刚打开的独占有效句柄,交给 OwnedHandle 负责 CloseHandle。
        let token = unsafe { OwnedHandle::from_raw_handle(token) };

        let mut needed: u32 = 0;
        // SAFETY: 探长度调用,缓冲为 NULL、长度为 0。
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        if needed == 0 {
            return Err(format!(
                "GetTokenInformation(TokenUser) 探测长度失败: {}",
                std::io::Error::last_os_error()
            ));
        }

        // TOKEN_USER 含指针字段,按 u64 缓冲保证对齐。
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        // SAFETY: buf 至少 needed 字节且 8 字节对齐。
        let ok = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buf.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(format!(
                "GetTokenInformation(TokenUser) 失败: {}",
                std::io::Error::last_os_error()
            ));
        }

        // SAFETY: 成功后 buf 开头为 TOKEN_USER,User.Sid 指向 buf 内部。
        let sid: PSID = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
        if unsafe { IsValidSid(sid) } == 0 {
            return Err("当前进程 TokenUser SID 无效".to_string());
        }

        let mut pstr: *mut u16 = std::ptr::null_mut();
        // SAFETY: sid 有效;成功时 pstr 由系统 LocalAlloc、以 NUL 结尾,读完即 LocalFree。
        unsafe {
            if ConvertSidToStringSidW(sid, &mut pstr) == 0 || pstr.is_null() {
                return Err(format!(
                    "ConvertSidToStringSidW 失败: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut len = 0usize;
            while *pstr.add(len) != 0 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(pstr, len));
            LocalFree(pstr as *mut c_void);
            Ok(text)
        }
    }

    fn to_wide_null(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// 用 SDDL 描述的 DACL 全量覆写 `path` 的 DACL 并阻断继承。
    fn set_protected_dacl(path: &Path, sddl: &str) -> Result<(), String> {
        let sd = OwnedSecurityDescriptor::from_sddl(sddl)?;
        let dacl = sd.dacl()?;
        let wide_path = to_wide_null(path);

        // SAFETY: wide_path 以 NUL 结尾;dacl 指向仍存活的 sd 内部。
        // DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION 会整张替换
        // 目标对象的 DACL 并设置 SE_DACL_PROTECTED,清除所有既有显式与继承 ACE。
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide_path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(format!(
                "SetNamedSecurityInfoW 收紧权限失败({}): {}",
                path.display(),
                std::io::Error::from_raw_os_error(status as i32)
            ));
        }
        Ok(())
    }

    pub(super) fn restrict_permissions(path: &Path) -> Result<(), String> {
        let sid = current_user_sid_string()?;
        set_protected_dacl(path, &format!("D:P(A;;FA;;;{sid})"))
    }

    pub(super) fn ensure_temp_keys_dir(dir: &Path) -> Result<(), String> {
        if let Err(err) = fs::create_dir_all(dir) {
            return Err(format!("创建临时密钥目录失败: {err}"));
        }
        // 无论目录是刚建还是已存在,都校验它不是 symlink / junction 且确实是目录
        let meta = fs::symlink_metadata(dir).map_err(|e| format!("检查临时密钥目录失败: {e}"))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(format!(
                "临时密钥目录路径不是普通目录(拒绝写入): {}",
                dir.display()
            ));
        }
        // 收紧目录本身 DACL(阻断 %TEMP% 的脏继承,只给当前用户可继承的完全控制)
        let sid = current_user_sid_string()?;
        set_protected_dacl(dir, &format!("D:P(A;OICI;FA;;;{sid})"))
    }

    pub(super) fn create_restricted_temp_file(path: &Path) -> Result<File, String> {
        let sid = current_user_sid_string()?;
        // O:<sid> 明确把 owner 钉在当前用户(提权令牌下不漂移到 Administrators);
        // D:P(A;;FA;;;<sid>) 从创建瞬间即阻断继承且仅允许当前用户完全控制。
        let sd = OwnedSecurityDescriptor::from_sddl(&format!("O:{sid}D:P(A;;FA;;;{sid})"))?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.raw,
            bInheritHandle: 0,
        };
        let wide_path = to_wide_null(path);

        // SAFETY: wide_path 以 NUL 结尾;sa.lpSecurityDescriptor 在本调用期间有效。
        // CREATE_NEW 保证不覆盖已有文件或跟随既有目标;dwShareMode = 0 写完前独占。
        let handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                FILE_GENERIC_WRITE,
                0,
                &sa,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(format!(
                "创建受保护临时私钥文件失败({}): {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }

        // SAFETY: handle 是刚成功创建的独占文件句柄,转交给 OwnedHandle / File 管理。
        let owned = unsafe { OwnedHandle::from_raw_handle(handle) };
        Ok(File::from(owned))
    }
}

#[cfg(unix)]
mod platform {
    use std::fs::{self, DirBuilder, File, OpenOptions};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use super::{TEMP_KEYS_DIR_NAME, choose_unix_temp_keys_dir};

    /// 当前进程有效用户 ID(极简 FFI,避免只为一个调用引入 libc 依赖)。
    fn effective_uid() -> u32 {
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        // SAFETY: geteuid 无参数、无副作用、永不失败。
        unsafe { geteuid() }
    }

    /// 校验 `$XDG_RUNTIME_DIR` 是否指向当前用户独占的安全目录(非 symlink、属主为 euid、
    /// group/other 无权限)。不满足时不采用,回退到带 euid 后缀的 temp_dir。
    fn verified_xdg_runtime_dir(euid: u32) -> Option<PathBuf> {
        let raw = std::env::var_os("XDG_RUNTIME_DIR")?;
        let path = PathBuf::from(raw);
        if !path.is_absolute() {
            return None;
        }
        let meta = fs::symlink_metadata(&path).ok()?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return None;
        }
        if meta.uid() != euid || (meta.mode() & 0o077) != 0 {
            return None;
        }
        Some(path)
    }

    pub(super) fn temp_keys_dir() -> PathBuf {
        let euid = effective_uid();
        let xdg = verified_xdg_runtime_dir(euid);
        choose_unix_temp_keys_dir(xdg.as_deref(), &std::env::temp_dir(), euid)
    }

    /// 只删除「非 symlink + 是目录 + 属主为当前 euid」的目录。
    fn remove_owned_dir(dir: &Path, euid: u32) {
        if let Ok(meta) = fs::symlink_metadata(dir)
            && !meta.file_type().is_symlink()
            && meta.is_dir()
            && meta.uid() == euid
        {
            let _ = fs::remove_dir_all(dir);
        }
    }

    pub(super) fn cleanup_ssh_temp_keys() {
        let euid = effective_uid();
        let current = temp_keys_dir();
        remove_owned_dir(&current, euid);

        // 顺手清理旧版遗留的无 uid 后缀目录(仅当属主确实是当前用户且非 symlink)
        let legacy = std::env::temp_dir().join(TEMP_KEYS_DIR_NAME);
        if legacy != current {
            remove_owned_dir(&legacy, euid);
        }
    }

    pub(super) fn restrict_permissions(path: &Path) -> Result<(), String> {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置文件权限失败: {e}"))
    }

    pub(super) fn ensure_temp_keys_dir(dir: &Path) -> Result<(), String> {
        let euid = effective_uid();
        match fs::symlink_metadata(dir) {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)
                    .map_err(|e| format!("创建临时密钥目录失败: {e}"))?;
            }
            Err(err) => {
                return Err(format!("检查临时密钥目录失败: {err}"));
            }
        }
        // 统一在 create 之后再核验一次(消除 NotFound → create 窗口期被预占 symlink / 异属主目录的竞态)
        let meta = fs::symlink_metadata(dir).map_err(|e| format!("检查临时密钥目录失败: {e}"))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(format!(
                "临时密钥目录路径不是普通目录(拒绝写入): {}",
                dir.display()
            ));
        }
        if meta.uid() != euid {
            return Err(format!(
                "临时密钥目录属主与当前用户不一致(拒绝写入): {}",
                dir.display()
            ));
        }
        // 显式收紧到 0700(补齐旧目录或异常 umask 的情况)
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("收紧临时密钥目录权限失败: {e}"))
    }

    pub(super) fn create_restricted_temp_file(path: &Path) -> Result<File, String> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("创建受保护临时私钥文件失败: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_dir(tag: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TEMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "mt-pty-ssh-key-test-{tag}-{}-{ts}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn choose_unix_temp_keys_dir_prefers_xdg_runtime_and_isolates_fallback_by_uid() {
        let temp = Path::new("/tmp");
        assert_eq!(
            choose_unix_temp_keys_dir(Some(Path::new("/run/user/1000")), temp, 1000),
            PathBuf::from("/run/user/1000/mini-term-ssh-keys")
        );
        assert_eq!(
            choose_unix_temp_keys_dir(None, temp, 1000),
            PathBuf::from("/tmp/mini-term-ssh-keys-1000")
        );
        assert_ne!(
            choose_unix_temp_keys_dir(None, temp, 1000),
            choose_unix_temp_keys_dir(None, temp, 1001),
            "共享 /tmp 下不同用户必须落到不同目录"
        );
    }

    #[test]
    fn temp_keys_dir_has_expected_name() {
        let dir = temp_keys_dir();
        let name = dir.file_name().unwrap().to_string_lossy();
        #[cfg(windows)]
        {
            assert!(dir.starts_with(std::env::temp_dir()));
            assert_eq!(name, "mini-term-ssh-keys");
        }
        #[cfg(unix)]
        {
            assert!(
                name == "mini-term-ssh-keys" || name.starts_with("mini-term-ssh-keys-"),
                "unexpected temp_keys_dir name: {name}"
            );
        }
    }

    #[test]
    fn prepare_ssh_key_errors_on_missing_file() {
        let err = prepare_ssh_key("/definitely/not/a/real/key/file").unwrap_err();
        assert!(err.contains("私钥文件不存在"));
    }

    #[test]
    fn prepare_ssh_key_rejects_directory_as_source() {
        let root = unique_test_dir("dir-src");
        let err = prepare_ssh_key_in(&root.to_string_lossy(), &root.join("keys")).unwrap_err();
        assert!(err.contains("私钥文件不存在"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prepare_ssh_key_copies_is_stable_and_updates_without_leftover_tmp() {
        let root = unique_test_dir("stable");
        let keys_dir = root.join("keys");
        let src = root.join("id_ed25519");
        fs::write(&src, "PRIVATE KEY V1").unwrap();
        let src_str = src.to_string_lossy().into_owned();

        let dest1 = prepare_ssh_key_in(&src_str, &keys_dir).unwrap();
        assert_eq!(fs::read_to_string(&dest1).unwrap(), "PRIVATE KEY V1");

        // 修改源文件内容后再次准备:复用同一目标文件名并原子更新内容
        fs::write(&src, "PRIVATE KEY V2").unwrap();
        let dest2 = prepare_ssh_key_in(&src_str, &keys_dir).unwrap();
        assert_eq!(dest1, dest2, "同一源路径应派生同一临时副本路径");
        assert_eq!(fs::read_to_string(&dest2).unwrap(), "PRIVATE KEY V2");

        // 目录里不应遗留 .tmp 临时文件
        let entries: Vec<String> = fs::read_dir(&keys_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries.len(), 1, "目录里只应有唯一目标副本: {entries:?}");
        assert!(entries[0].ends_with(".key"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prepare_ssh_key_handles_readonly_source_and_existing_readonly_dest() {
        let root = unique_test_dir("readonly");
        let keys_dir = root.join("keys");
        let src = root.join("readonly_key");
        fs::write(&src, "RO KEY V1").unwrap();
        let mut perms = fs::metadata(&src).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&src, perms).unwrap();

        let src_str = src.to_string_lossy().into_owned();
        let dest = prepare_ssh_key_in(&src_str, &keys_dir).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "RO KEY V1");

        // 模拟老版本留下的只读副本,再次准备仍应能成功覆写
        let mut dest_perms = fs::metadata(&dest).unwrap().permissions();
        dest_perms.set_readonly(true);
        fs::set_permissions(&dest, dest_perms).unwrap();

        let dest2 = prepare_ssh_key_in(&src_str, &keys_dir).unwrap();
        assert_eq!(dest, dest2);
        assert_eq!(fs::read_to_string(&dest2).unwrap(), "RO KEY V1");

        // 清理前恢复源文件可写(Windows 上只读文件 remove_dir_all 会失败)
        #[cfg(windows)]
        {
            let mut src_perms = fs::metadata(&src).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            src_perms.set_readonly(false);
            let _ = fs::set_permissions(&src, src_perms);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&src, fs::Permissions::from_mode(0o600));
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn unix_prepare_ssh_key_enforces_0700_dir_and_0600_file_from_wide_source() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_test_dir("unix-perms");
        let keys_dir = root.join("keys");
        let src = root.join("wide_key");
        fs::write(&src, "SECRET KEY").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o644)).unwrap();

        let dest = prepare_ssh_key_in(&src.to_string_lossy(), &keys_dir).unwrap();
        let dir_mode = fs::metadata(&keys_dir).unwrap().permissions().mode() & 0o777;
        let file_mode = fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "临时私钥目录必须为 0700");
        assert_eq!(file_mode, 0o600, "临时私钥副本必须为 0600");

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn unix_prepare_ssh_key_rejects_symlink_dir() {
        use std::os::unix::fs::symlink;

        let root = unique_test_dir("unix-symlink");
        let real_dir = root.join("real");
        fs::create_dir_all(&real_dir).unwrap();
        let link_dir = root.join("link");
        symlink(&real_dir, &link_dir).unwrap();

        let src = root.join("id_ed25519");
        fs::write(&src, "SECRET KEY").unwrap();

        let err = prepare_ssh_key_in(&src.to_string_lossy(), &link_dir).unwrap_err();
        assert!(err.contains("不是普通目录"), "应拒绝 symlink 目录: {err}");

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    mod windows_acl_tests {
        use super::*;
        use std::ffi::c_void;
        use std::os::windows::ffi::OsStrExt;

        use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
        };
        use windows_sys::Win32::Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetSecurityDescriptorControl,
            GetSecurityDescriptorDacl, INHERITED_ACE, OWNER_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
        };

        /// `winnt.h`: `ACCESS_ALLOWED_ACE_TYPE`
        const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

        fn sid_to_string(sid: PSID) -> String {
            let mut pstr: *mut u16 = std::ptr::null_mut();
            assert_ne!(unsafe { ConvertSidToStringSidW(sid, &mut pstr) }, 0);
            let mut len = 0usize;
            unsafe {
                while *pstr.add(len) != 0 {
                    len += 1;
                }
                let text = String::from_utf16_lossy(std::slice::from_raw_parts(pstr, len));
                LocalFree(pstr as *mut c_void);
                text
            }
        }

        /// 读出路径的 owner SID、DACL 保护位与每条 ACE 的 `(ace_type, is_inherited, sid_string)`。
        fn inspect_security(path: &Path) -> (String, bool, Vec<(u8, bool, String)>) {
            let wide: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let mut owner: PSID = std::ptr::null_mut();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let status = unsafe {
                GetNamedSecurityInfoW(
                    wide.as_ptr(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    &mut owner,
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut psd,
                )
            };
            assert_eq!(
                status, ERROR_SUCCESS,
                "GetNamedSecurityInfoW 失败: {status}"
            );
            let owner_sid = sid_to_string(owner);

            let mut control: u16 = 0;
            let mut revision: u32 = 0;
            assert_ne!(
                unsafe { GetSecurityDescriptorControl(psd, &mut control, &mut revision) },
                0
            );
            let protected = (control & SE_DACL_PROTECTED) != 0;

            let mut size_info = ACL_SIZE_INFORMATION {
                AceCount: 0,
                AclBytesInUse: 0,
                AclBytesFree: 0,
            };
            assert_ne!(
                unsafe {
                    GetAclInformation(
                        dacl,
                        (&mut size_info as *mut ACL_SIZE_INFORMATION).cast(),
                        std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                        AclSizeInformation,
                    )
                },
                0
            );

            let mut aces = Vec::new();
            for i in 0..size_info.AceCount {
                let mut ace_ptr: *mut c_void = std::ptr::null_mut();
                assert_ne!(unsafe { GetAce(dacl, i, &mut ace_ptr) }, 0);
                let header = unsafe { &*(ace_ptr as *const ACE_HEADER) };
                // ACCESS_ALLOWED_ACE / ACCESS_DENIED_ACE 的 SidStart 都在同一偏移处
                let allowed = unsafe { &*(ace_ptr as *const ACCESS_ALLOWED_ACE) };
                let sid_ptr = std::ptr::addr_of!(allowed.SidStart) as PSID;
                let inherited = (u32::from(header.AceFlags) & INHERITED_ACE) != 0;
                aces.push((header.AceType, inherited, sid_to_string(sid_ptr)));
            }

            unsafe { LocalFree(psd) };
            (owner_sid, protected, aces)
        }

        /// 给指定文件写一条包含「当前用户 + Everyone 显式只读」的脏 DACL,
        /// 复现「父目录或第三方沙箱遗留显式 ACE 导致旧 `icacls /inheritance:r` 清不掉」的场景。
        fn stamp_polluted_explicit_dacl(path: &Path, user_sid: &str) {
            let sddl = format!("D:P(A;;FA;;;{user_sid})(A;;0x1200a9;;;WD)");
            let wide_sddl: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
            let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            assert_ne!(
                unsafe {
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        wide_sddl.as_ptr(),
                        SDDL_REVISION_1,
                        &mut psd,
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            let mut present: i32 = 0;
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut defaulted: i32 = 0;
            assert_ne!(
                unsafe { GetSecurityDescriptorDacl(psd, &mut present, &mut dacl, &mut defaulted) },
                0
            );
            let wide_path: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let status = unsafe {
                SetNamedSecurityInfoW(
                    wide_path.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    dacl,
                    std::ptr::null_mut(),
                )
            };
            unsafe { LocalFree(psd) };
            assert_eq!(status, ERROR_SUCCESS, "植入脏 DACL 失败: {status}");
        }

        #[test]
        fn windows_restrict_permissions_wipes_explicit_foreign_aces() {
            let root = unique_test_dir("win-wipe-explicit");
            let key = root.join("polluted.key");
            fs::write(&key, "PRIVATE KEY").unwrap();

            let me = platform::current_user_sid_string().unwrap();
            stamp_polluted_explicit_dacl(&key, &me);

            let (_, _, before) = inspect_security(&key);
            assert!(
                before.iter().any(|(_, _, sid)| sid == "S-1-1-0"),
                "前置条件:文件上必须先存在显式 Everyone ACE,实际: {before:?}"
            );

            restrict_permissions(&key).unwrap();

            let (owner, protected, after) = inspect_security(&key);
            assert_eq!(owner, me);
            assert!(protected, "收紧后必须设置 SE_DACL_PROTECTED 阻断继承");
            assert_eq!(
                after,
                vec![(ACCESS_ALLOWED_ACE_TYPE, false, me)],
                "收紧后 DACL 必须严格只剩当前用户 SID 一条允许项,不得残留任何显式/继承 ACE"
            );

            let _ = fs::remove_dir_all(&root);
        }

        #[test]
        fn windows_prepare_ssh_key_leaves_only_current_user_sid_in_dacl() {
            let root = unique_test_dir("win-prepare-dacl");
            let keys_dir = root.join("keys");
            let src = root.join("id_ed25519");
            fs::write(&src, "PRIVATE KEY CONTENT").unwrap();

            let me = platform::current_user_sid_string().unwrap();
            let dest = prepare_ssh_key_in(&src.to_string_lossy(), &keys_dir).unwrap();

            let (_, dir_protected, dir_aces) = inspect_security(&keys_dir);
            assert!(dir_protected, "临时私钥目录必须阻断继承");
            assert_eq!(dir_aces, vec![(ACCESS_ALLOWED_ACE_TYPE, false, me.clone())]);

            let (file_owner, file_protected, file_aces) = inspect_security(Path::new(&dest));
            assert_eq!(file_owner, me, "临时私钥文件的 owner 必须是当前用户");
            assert!(file_protected, "临时私钥文件必须阻断继承");
            assert_eq!(file_aces, vec![(ACCESS_ALLOWED_ACE_TYPE, false, me)]);

            let _ = fs::remove_dir_all(&root);
        }
    }
}
