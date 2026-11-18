//! 本地删除进系统回收站(Windows 回收站 / macOS 废纸篓 / freedesktop 回收站,走 `trash` crate)。
//!
//! # 安全口径与永久删除同一把尺子
//!
//! 下手前先过 [`mt_project::fs::resolve_deletable_entry`] —— 永久删除(`delete_entry`)用的
//! 也是它:条目必须在项目根内(父目录跟随链接后仍在根内),**叶子本身不跟随**,不能是项目根。
//! 交给 `trash` 的是校验后的那条路径;`trash` 自己也只规范化父目录、保留叶子名
//! (`trash::canonicalize_paths`),所以进回收站的是链接 / junction 本身,不是它指向的内容。
//!
//! # 进不了回收站的位置不静默硬删
//!
//! - WSL(`\\wsl$` / `\\wsl.localhost`)与其它 UNC 路径、Windows 上不是本机固定盘的位置
//!   (网络映射盘、U 盘这类没有回收站的盘):不先试回收站,直接判成 [`RecycleSupport::Unsupported`],
//!   由调用方问一句「无法移到回收站,是否永久删除?」;
//! - 试了但失败(权限、路径太深、条目超出回收站容量……):`trash` 带 `FOF_NO_UI` 时拒收的条目
//!   是**跳过并报错**而不是永久删掉,同样回到那一问。
//!
//! # 为什么在一条新线程上调
//!
//! Windows 上 `trash` 走 `IFileOperation`,先在**当前线程** `CoInitializeEx` 成单线程套间,
//! 初始化失败直接 panic(`trash` 的 `CoInitializer::new`)。gpui 的后台执行器是系统线程池,
//! 池里的线程可能已被别的代码按多线程套间初始化过 COM —— 混进去就是 `RPC_E_CHANGED_MODE`
//! → panic。每次删除起一条只活这一下的线程:COM 状态干净,线程退出时 `trash` 的线程局部
//! 析构顺带 `CoUninitialize`,万一 panic 也只折成这一次删除的错误。

use std::path::{Path, PathBuf};

/// 这个位置能不能进回收站(删除确认框据此选文案)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RecycleSupport {
    /// 本机固定盘上的路径(macOS / Linux 一律先试,试不了再说)。
    Supported,
    /// WSL / UNC / 网络盘这类没有回收站的位置:不试,直接问要不要永久删除。
    Unsupported,
}

/// 一次「移到回收站」失败的原因。两种要分开处理:校验没过(路径不在项目根内……)照常报错;
/// 回收站拒收才问要不要改成永久删除。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TrashFailure {
    /// 删除前的安全校验没过 —— 永久删除也会被同一道校验拦下,不提供「改为永久删除」。
    Invalid(String),
    /// 回收站没收(位置不支持 / 权限 / 太大 / 太深……)。
    Refused(String),
}

/// 判这个位置有没有回收站。纯字符串判得出的先判(WSL、UNC),Windows 上再问一次盘的类型。
pub(super) fn recycle_support(path: &Path) -> RecycleSupport {
    if is_unc_like(&path.to_string_lossy()) {
        return RecycleSupport::Unsupported;
    }
    if drive_has_recycle_bin(path) {
        RecycleSupport::Supported
    } else {
        RecycleSupport::Unsupported
    }
}

/// `\\server\share`、`//server/share`、WSL 的 `\\wsl$\…` / `\\wsl.localhost\…`,以及剥不掉的
/// `\\?\UNC\…` 形态:都是网络 / 虚拟文件系统上的路径,没有回收站。
/// 本机盘符的 verbatim 形态(`\\?\C:\…`)不算。
fn is_unc_like(path: &str) -> bool {
    if mt_core::parse_wsl_unc(path).is_some() {
        return true;
    }
    let stripped = mt_core::path_key::strip_verbatim_prefix(path);
    let mut chars = stripped.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some('\\' | '/'), Some('\\' | '/'))
    )
}

/// Windows:只有本机固定盘(`DRIVE_FIXED`)才有回收站 —— 网络映射盘、U 盘、光驱删了就是没了,
/// 交给 `IFileOperation` 去试,轻则报错、重则按系统设置直接永久删。判不出来(没有盘符)按没有算。
#[cfg(windows)]
fn drive_has_recycle_bin(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::{Component, Prefix};

    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    let letter = match prefix.kind() {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,
        _ => return false,
    };
    let root: Vec<u16> = std::ffi::OsStr::new(&format!("{}:\\", letter as char))
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `root` 是以 0 结尾的 UTF-16 串,调用期间一直活着
    let kind = unsafe {
        windows::Win32::Storage::FileSystem::GetDriveTypeW(windows::core::PCWSTR(root.as_ptr()))
    };
    // DRIVE_FIXED = 3(windows crate 里这个常量挂在另一个 feature 下,数值是 Win32 定死的)
    kind == 3
}

/// macOS / Linux:回收站能不能收由 `trash` 自己判(跨设备、没有回收站目录的挂载点会报错,
/// 不会悄悄永久删除),这里一律先试。
#[cfg(not(windows))]
fn drive_has_recycle_bin(_path: &Path) -> bool {
    true
}

/// 把项目里的一个条目移到回收站。阻塞(回收大目录、网络盘都可能慢),调用方丢后台。
pub(super) fn move_to_trash(project_root: &Path, path: &Path) -> Result<(), TrashFailure> {
    let target = mt_project::fs::resolve_deletable_entry(project_root, path)
        .map_err(|err| TrashFailure::Invalid(format!("{err:#}")))?;
    trash_on_fresh_thread(target).map_err(TrashFailure::Refused)
}

/// 在一条只活这一下的线程上调 `trash::delete`(理由见模块注释「为什么在一条新线程上调」)。
fn trash_on_fresh_thread(target: PathBuf) -> Result<(), String> {
    let handle = std::thread::Builder::new()
        .name("mt-recycle".into())
        .spawn(move || trash::delete(&target).map_err(|err| err.to_string()))
        .map_err(|err| format!("{err}"))?;
    match handle.join() {
        Ok(result) => result,
        Err(_) => Err("回收站操作意外中断".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WSL 与其它 UNC 路径不试回收站;本机盘符(含 verbatim 形态)不在此列。
    #[test]
    fn wsl_与_unc_路径判成没有回收站() {
        for path in [
            r"\\wsl$\Ubuntu\home\u\proj\a.rs",
            r"\\wsl.localhost\Ubuntu\home\u\proj",
            r"\\WSL.LOCALHOST\Ubuntu-22.04\x",
            r"\\?\UNC\wsl$\Ubuntu\home",
            r"\\server\share\dir\a.txt",
            "//server/share/a.txt",
        ] {
            assert!(is_unc_like(path), "{path}");
            assert_eq!(
                recycle_support(Path::new(path)),
                RecycleSupport::Unsupported
            );
        }
        for path in [
            r"C:\Users\u\proj\a.rs",
            r"\\?\D:\proj\a.rs",
            "/home/u/proj/a.rs",
        ] {
            assert!(!is_unc_like(path), "{path}");
        }
    }

    /// 校验没过(根外 / 项目根本身)报 `Invalid`,不碰回收站,也就不会提供「改为永久删除」。
    #[test]
    fn 校验不过的条目不进回收站() {
        let root = std::env::temp_dir().join(format!(
            "mini-term-recycle-invalid-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        assert!(matches!(
            move_to_trash(&root, &root),
            Err(TrashFailure::Invalid(_))
        ));
        let outside = root
            .parent()
            .unwrap()
            .join("mini-term-recycle-not-here.txt");
        assert!(matches!(
            move_to_trash(&root, &outside),
            Err(TrashFailure::Invalid(_))
        ));
        std::fs::remove_dir_all(&root).ok();
    }

    /// 真进一次回收站:临时目录里建一个文件、回收、原路径不在了。会往本机回收站里放一个
    /// 临时文件(不读也不清回收站),所以平时不跑:`cargo test -p mt-app -- --ignored 回收站`。
    #[test]
    #[ignore = "会往本机回收站里放一个临时文件,手动跑"]
    fn 回收站真收下临时文件() {
        let root = std::env::temp_dir().join(format!("mini-term-recycle-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("recycle-me.txt");
        std::fs::write(&file, "x").unwrap();
        move_to_trash(&root, &file).unwrap();
        assert!(!file.exists());
        std::fs::remove_dir_all(&root).ok();
    }

    /// junction 进回收站的是链接本身:指向的目录与里面的文件原样留着。同上,手动跑。
    #[cfg(windows)]
    #[test]
    #[ignore = "会往本机回收站里放一个临时 junction,手动跑"]
    fn 回收站收的是_junction_本身() {
        let root =
            std::env::temp_dir().join(format!("mini-term-recycle-link-{}", std::process::id()));
        let target = root.join("real");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.txt"), "keep").unwrap();
        let link = root.join("link");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success());
        move_to_trash(&root, &link).unwrap();
        assert!(
            std::fs::symlink_metadata(&link).is_err(),
            "链接本身进了回收站"
        );
        assert!(target.join("keep.txt").exists(), "指向的内容原样留着");
        std::fs::remove_dir_all(&root).ok();
    }
}
