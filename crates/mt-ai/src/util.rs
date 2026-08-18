//! 本 crate 自用的小工具。
//!
//! 这里两个函数都是**逐字复制**自迁移期仍留在 `src-tauri/` 下的模块
//! (`fs::atomic_write` → 将来的 `mt-project`;`mt_core::parse_wsl_unc` → 将来
//! 随 `remote_ssh.rs` 一起挪进 `crates/`,见 docs/gpui-migration.md 第 7 节)。
//! 在那两处落位之前,本 crate **不引用 `src-tauri/`**:新工作区一旦反向依赖旧
//! 目录树,`cargo build -p mt-ai` 就会把整套 Tauri 依赖拖进来,迁移期并存的
//! 前提也就没了。两份实现的去重放到收尾阶段做。

use std::fs;
use std::path::Path;

/// 原子写文件:同目录临时文件 + rename。
///
/// 逐字复制自 `src-tauri/src/fs.rs::atomic_write`。hook 注册要改用户的
/// `settings.json` / `hooks.json` / `config.toml`,写一半崩掉会毁掉用户配置。
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

/// WSL UNC 路径的解析结果。
///
/// `unix_path` 始终以 `/` 起头,空 path (如 `\\wsl$\Ubuntu`) 归一为 `/`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WslPath {
    pub distro: String,
    pub unix_path: String,
}

/// 解析任意路径字符串,匹配 WSL UNC 形式时返回 `Some(WslPath)`,否则 `None`。
///
/// 逐字复制自 `mt_core::wsl_path::parse_unc`(原 `mt_core::parse_wsl_unc`)。
/// 支持 `\\wsl$\<distro>\<rest>` / `\\wsl.localhost\...` / `\\?\UNC\...` 三种形式,
/// host 名大小写不敏感,distro 名保留原大小写。纯字符串匹配,不做磁盘访问。
pub fn parse_wsl_unc(path: &str) -> Option<WslPath> {
    // 先尝试剥 `\\?\UNC\` verbatim 前缀,剥不掉再尝试普通 `\\`。
    // 注意 strip_prefix("\\\\?\\UNC\\") 必须在 strip_prefix("\\\\") 之前,
    // 否则前者会被后者吞掉前两个反斜杠后落到非匹配分支。
    let after_prefix = path
        .strip_prefix(r"\\?\UNC\")
        .or_else(|| path.strip_prefix(r"\\"))?;

    // 分成 host \ distro \ rest 三段。splitn(3) 保证 rest 里可继续含反斜杠。
    let mut parts = after_prefix.splitn(3, '\\');
    let host = parts.next()?;
    let distro = parts.next()?;
    let rest = parts.next().unwrap_or("");

    let host_lower = host.to_ascii_lowercase();
    if host_lower != "wsl$" && host_lower != "wsl.localhost" {
        return None;
    }

    if distro.is_empty() {
        return None;
    }

    // Linux 路径用 `/`。空 rest 表示 distro 根目录,归一为 `/`。
    let unix_path = if rest.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", rest.replace('\\', "/"))
    };

    Some(WslPath {
        distro: distro.to_string(),
        unix_path,
    })
}
