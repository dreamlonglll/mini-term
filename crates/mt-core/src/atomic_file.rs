//! 原子写文件 —— 工作区唯一一份实现。
//!
//! 收尾-1 批之前这个函数在四处各有一份逐字副本(`src-tauri/src/fs.rs`、
//! `mt-project::fs`、`mt-config::config`、`mt-ai::util`),原因都是同一个:
//! 当时没有一个「谁都能依赖、又不会把依赖方向弄反」的叶子 crate。
//! `mt-core` 移入 `crates/` 后它就是那个叶子(依赖表只有 serde/serde_json/dirs,
//! 且被三个 sidecar 小二进制直接链接),故实现收归此处,原三处改为 `pub use`。
//!
//! 实现一字未改,`static COUNTER` 由「每个 crate 一个」并成「全进程一个」——
//! 计数器只用于拼临时文件名的唯一性,合并只会让并发写更不容易撞名。
//!
//! 之后唯一的行为变化:Windows 上最后那一步 rename 被别的进程短暂占用挡住时退避重试
//! 约 1 秒([`rename_replacing`]),其它平台与其它错误照旧立即返回。

use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

/// 原子写文件:先写到同目录的临时文件,fsync 后再 rename 覆盖目标。
///
/// rename 在同一卷上是原子操作(Windows 下 Rust 的 `std::fs::rename` 走
/// `MOVEFILE_REPLACE_EXISTING`,可原子替换已存在文件),因此即便写入过程中崩溃/断电/
/// 磁盘满,目标文件要么是旧内容、要么是完整新内容,绝不会留下被截断的半截文件。
/// 用于所有「覆盖用户/全局既有配置」的写入(config.json、config.toml、.mcp.json、
/// settings.json、hooks.json 等),避免裸 `fs::write` 的 truncate-then-write 损坏用户配置。
///
/// Windows 上目标或临时文件正被别的进程短暂开着(杀软扫描、索引服务、同时在读配置的
/// sidecar……)时 rename 会报拒绝访问,这一步退避重试约 1 秒;仍失败就删掉临时文件、
/// 返回最后一次的错误,目标保持旧内容。
pub fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "目标路径没有父目录")
    })?;
    // 临时文件必须与目标同目录,保证同卷,rename 才能原子
    let seq = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("tmp");
    let tmp = dir.join(format!(".{}.{}.{}.tmp", stem, std::process::id(), seq));

    let write_result = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.flush()?;
        let _ = f.sync_all(); // sync 失败不致命,尽力而为
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // 若目标已存在,把其权限位复制到临时文件,避免 rename 后权限退化为 umask 默认值
    // (Unix 下保护用户 chmod 600 的含 token 配置不被降级为 0644;Windows 上对应只读位)。
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }

    match rename_replacing(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Windows 上 rename 被短暂占用挡住时最多重试几次。第 n 次重试前等 n × 5 毫秒,
/// 20 次累计 1.05 秒。
const RENAME_RETRIES: u32 = 20;
const RENAME_BACKOFF_STEP_MS: u64 = 5;

/// 把写好的临时文件 rename 成目标,替换已有文件。
///
/// 只在 Windows 上对「被占用」类错误([`is_transiently_busy`])退避重试:那边被打开着
/// (没给 `FILE_SHARE_DELETE`)的文件不能被替换,而杀软 / 索引服务开一下就放;POSIX
/// 的 rename 不受别人开着文件影响,其它平台不重试。
///
/// 没有照搬 Winc 在重试用完后「先把目标挪开再换上新文件」的兜底:挪开与换上之间目标
/// 路径上没有文件,这时崩溃或有别的进程来读(sidecar 每次请求都重读 config.json,读不到
/// 就 fail-closed 拒绝)都会出错,违背本函数「要么旧、要么新」的承诺;而它能救的只有
/// 「对方开文件时给了 `FILE_SHARE_DELETE`、文件系统却不支持 POSIX 式替换(FAT / exFAT)」
/// 这一种情形,数据目录所在的 NTFS 用不上。
///
/// 移植自 Winc(Apache-2.0)`crates/winc-config/src/files.rs` 的 `replace_file`(重试部分)。
fn rename_replacing(from: &Path, to: &Path) -> io::Result<()> {
    rename_with_retry(|| fs::rename(from, to), cfg!(windows), std::thread::sleep)
}

/// [`rename_replacing`] 的可测核心:`retry_busy` 为真且错误属于「被占用」时退避重试,
/// 重试用完仍失败返回最后一次的错误;其余情形第一次的结果原样返回。
fn rename_with_retry(
    mut rename: impl FnMut() -> io::Result<()>,
    retry_busy: bool,
    mut sleep: impl FnMut(Duration),
) -> io::Result<()> {
    let mut tries = 0;
    loop {
        match rename() {
            Err(e) if retry_busy && tries < RENAME_RETRIES && is_transiently_busy(&e) => {
                tries += 1;
                sleep(Duration::from_millis(
                    RENAME_BACKOFF_STEP_MS * u64::from(tries),
                ));
            }
            result => return result,
        }
    }
}

/// 「别的进程正开着这个文件」那一类错误:拒绝访问(目标被不带 `FILE_SHARE_DELETE`
/// 打开着)与共享冲突(临时文件正被扫描)。后者 std 没有归进 `PermissionDenied`
/// (落在未分类里),按 Windows 原始错误码认。
fn is_transiently_busy(e: &io::Error) -> bool {
    /// `ERROR_SHARING_VIOLATION`(winerror.h)。只在 Windows 上是这个含义。
    const ERROR_SHARING_VIOLATION: i32 = 32;
    e.kind() == io::ErrorKind::PermissionDenied
        || (cfg!(windows) && e.raw_os_error() == Some(ERROR_SHARING_VIOLATION))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_overwrites() {
        let dir = std::env::temp_dir().join(format!("mt-core-atomic-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("f.txt");

        atomic_write(&target, b"first").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "first");

        atomic_write(&target, b"second-longer-content").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "second-longer-content");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("mt-core-atomic-tmp-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("g.txt");

        atomic_write(&target, b"hello").unwrap();
        atomic_write(&target, b"world").unwrap();

        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件: {leftovers:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    fn busy() -> io::Error {
        io::Error::from(io::ErrorKind::PermissionDenied)
    }

    /// 非 Windows 的口径(`retry_busy = false`):被拒绝也只试一次、不等待。
    #[test]
    fn 不重试时失败立即返回() {
        let mut calls = 0;
        let mut slept = Vec::new();
        let result = rename_with_retry(
            || {
                calls += 1;
                Err(busy())
            },
            false,
            |d| slept.push(d),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(calls, 1);
        assert!(slept.is_empty());
    }

    #[test]
    fn 被占用时退避重试直到成功() {
        let mut calls = 0;
        let mut slept = Vec::new();
        let result = rename_with_retry(
            || {
                calls += 1;
                if calls <= 3 {
                    Err(busy())
                } else {
                    Ok(())
                }
            },
            true,
            |d| slept.push(d),
        );
        assert!(result.is_ok());
        assert_eq!(calls, 4);
        let ms: Vec<u128> = slept.iter().map(Duration::as_millis).collect();
        assert_eq!(ms, [5, 10, 15]);
    }

    /// 一直被占用:约 1 秒后放弃,错误原样交回(不吞掉)。
    #[test]
    fn 一直被占用约一秒后返回原错误() {
        let mut calls = 0;
        let mut slept = Duration::ZERO;
        let result = rename_with_retry(
            || {
                calls += 1;
                Err(busy())
            },
            true,
            |d| slept += d,
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(calls, RENAME_RETRIES + 1);
        assert_eq!(slept, Duration::from_millis(1050));
    }

    #[test]
    fn 其它错误不重试() {
        let mut calls = 0;
        let result = rename_with_retry(
            || {
                calls += 1;
                Err(io::Error::from(io::ErrorKind::NotFound))
            },
            true,
            |_| panic!("不该等待"),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(calls, 1);
    }

    /// 真落盘的失败路径:目标是个非空目录,rename 必然失败。错误照样交回、临时文件
    /// 删干净;非 Windows 不重试、立即返回(Windows 上这是拒绝访问,先重试约 1 秒)。
    #[test]
    fn rename_失败时报错且不留临时文件() {
        let dir = std::env::temp_dir().join(format!("mt-core-atomic-fail-{}", std::process::id()));
        let target = dir.join("occupied");
        fs::create_dir_all(target.join("child")).unwrap();

        let started = std::time::Instant::now();
        assert!(atomic_write(&target, b"x").is_err());
        if !cfg!(windows) {
            assert!(
                started.elapsed() < Duration::from_millis(500),
                "非 Windows 不该重试: {:?}",
                started.elapsed()
            );
        }
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件: {leftovers:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Windows:目标被别的句柄独占(不共享删除)着、稍后释放 —— 重试期间等到释放就写成。
    #[cfg(windows)]
    #[test]
    fn 目标被短暂独占时重试后写成() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = std::env::temp_dir().join(format!("mt-core-atomic-busy-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("busy.txt");
        fs::write(&target, "old").unwrap();

        // share_mode(0):不与任何人共享,替换它的 rename 会被拒绝
        let holder = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&target)
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(holder);
        });

        atomic_write(&target, b"new").unwrap();
        release.join().unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");

        let _ = fs::remove_dir_all(&dir);
    }
}
