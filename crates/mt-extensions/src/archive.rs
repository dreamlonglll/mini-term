//! zip 的读取与逐文件校验。
//!
//! 口径与主题包导入同一套,只是更严:扩展 zip 里**只许**有清单列出的那几个文件 ——
//! 条目数对不上、多一个、少一个、重名、目录条目、符号链接、非常规文件、加密条目、
//! 声明大小与清单不符、解出来超长或哈希不符,一律拒。条目名必须与清单文件名**逐字
//! 相等**,清单文件名又是校验过的单段名(见 [`crate::is_safe_file_name`]),所以越界
//! 路径(`../x`、`/abs`、`a/b`、`C:x`)不可能落到暂存目录之外。

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use sha2::{Digest, Sha256};
use zip::ZipArchive;
use zip::read::ZipFile;

use crate::error::ExtensionError;
use crate::manifest::Component;

/// Unix mode 的文件类型位。
const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;

/// 把 `component` 的 zip 逐条解进 `staging`(调用方建好的空目录)并逐文件校验。
/// 失败时 `staging` 里可能留有半成品,由调用方整个删掉。
pub(crate) fn extract_component(
    file: File,
    component: &Component,
    staging: &Path,
) -> Result<(), ExtensionError> {
    let asset = component.asset.as_str();
    let mut archive = ZipArchive::new(file)
        .map_err(|e| ExtensionError::integrity(format!("{asset}: not a valid zip: {e}")))?;
    if archive.len() != component.files.len() {
        return Err(ExtensionError::integrity(format!(
            "{asset}: expected {} entries, found {}",
            component.files.len(),
            archive.len()
        )));
    }
    let mut seen: HashSet<String> = HashSet::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| ExtensionError::integrity(format!("{asset}: entry #{index}: {e}")))?;
        let name = entry
            .name()
            .map_err(|e| ExtensionError::integrity(format!("{asset}: entry #{index} name: {e}")))?
            .into_owned();
        require_regular(&entry, asset, &name)?;
        let Some(spec) = component.file(&name) else {
            return Err(ExtensionError::integrity(format!(
                "{asset}: unexpected entry {name:?}"
            )));
        };
        if !seen.insert(spec.name.clone()) {
            return Err(ExtensionError::integrity(format!(
                "{asset}: duplicate entry {name:?}"
            )));
        }
        if entry.size() != spec.size {
            return Err(ExtensionError::integrity(format!(
                "{asset}: entry {name:?} declares {} bytes, manifest says {}",
                entry.size(),
                spec.size
            )));
        }
        let target = staging.join(&spec.name);
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| ExtensionError::io(format!("creating {}", target.display()), e))?;
        let (written, digest) = copy_hashed(&mut entry, &mut out, spec.size)
            .map_err(|e| prefix(e, &format!("{asset}: entry {name:?}")))?;
        if written != spec.size {
            return Err(ExtensionError::integrity(format!(
                "{asset}: entry {name:?} unpacked to {written} bytes, manifest says {}",
                spec.size
            )));
        }
        if digest != spec.sha256 {
            return Err(ExtensionError::integrity(format!(
                "{asset}: entry {name:?} sha256 mismatch"
            )));
        }
        out.sync_all()
            .map_err(|e| ExtensionError::io(format!("flushing {}", target.display()), e))?;
        drop(out);
        set_mode(&target, spec.executable)?;
    }
    if seen.len() != component.files.len() {
        return Err(ExtensionError::integrity(format!(
            "{asset}: missing entries"
        )));
    }
    Ok(())
}

/// 只收常规文件条目:目录、符号链接、带非常规文件类型位的条目一律拒。
pub(crate) fn require_regular<R: Read + ?Sized>(
    entry: &ZipFile<'_, R>,
    asset: &str,
    name: &str,
) -> Result<(), ExtensionError> {
    if entry.is_dir() || entry.is_symlink() {
        return Err(ExtensionError::integrity(format!(
            "{asset}: entry {name:?} is not a regular file"
        )));
    }
    if let Some(mode) = entry.unix_mode() {
        let kind = mode & S_IFMT;
        if kind != 0 && kind != S_IFREG {
            return Err(ExtensionError::integrity(format!(
                "{asset}: entry {name:?} has non-regular file mode {mode:o}"
            )));
        }
    }
    Ok(())
}

/// 边读边写边算 sha256,读到超过 `limit` 立刻报错(不信 zip 里声明的大小)。
pub(crate) fn copy_hashed(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    limit: u64,
) -> Result<(u64, String), ExtensionError> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| ExtensionError::integrity(format!("read failed: {e}")))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > limit {
            return Err(ExtensionError::integrity(format!(
                "exceeds the expected {limit} bytes"
            )));
        }
        hasher.update(&buffer[..read]);
        writer
            .write_all(&buffer[..read])
            .map_err(|e| ExtensionError::Io(format!("write failed: {e}")))?;
    }
    Ok((total, hex(&hasher.finalize())))
}

/// 读完整个 reader 算 sha256;超过 `limit` 报错。
pub(crate) fn sha256_reader(reader: &mut dyn Read, limit: u64) -> Result<String, ExtensionError> {
    let (_, digest) = copy_hashed(reader, &mut std::io::sink(), limit)?;
    Ok(digest)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    out
}

fn prefix(error: ExtensionError, context: &str) -> ExtensionError {
    match error {
        ExtensionError::Integrity(detail) => {
            ExtensionError::Integrity(format!("{context}: {detail}"))
        }
        ExtensionError::Io(detail) => ExtensionError::Io(format!("{context}: {detail}")),
        other => other,
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, executable: bool) -> Result<(), ExtensionError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = if executable { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| ExtensionError::io(format!("chmod {}", path.display()), e))
}

#[cfg(not(unix))]
fn set_mode(path: &Path, _executable: bool) -> Result<(), ExtensionError> {
    // Windows 没有可执行位;只确认文件确实落盘了
    fs::symlink_metadata(path)
        .map(|_| ())
        .map_err(|e| ExtensionError::io(format!("stat {}", path.display()), e))
}
