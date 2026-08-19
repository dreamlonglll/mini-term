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

use std::fs;
use std::path::Path;

/// 原子写文件:先写到同目录的临时文件,fsync 后再 rename 覆盖目标。
///
/// rename 在同一卷上是原子操作(Windows 下 Rust 的 `std::fs::rename` 走
/// `MOVEFILE_REPLACE_EXISTING`,可原子替换已存在文件),因此即便写入过程中崩溃/断电/
/// 磁盘满,目标文件要么是旧内容、要么是完整新内容,绝不会留下被截断的半截文件。
/// 用于所有「覆盖用户/全局既有配置」的写入(config.json、config.toml、.mcp.json、
/// settings.json、hooks.json 等),避免裸 `fs::write` 的 truncate-then-write 损坏用户配置。
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

    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
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
}
