//! 装机版的诊断日志落盘:把没有控制台时的 stderr / stdout 接到
//! `{active_data_dir}/mini-term.log`。
//!
//! # 为什么需要这一层
//!
//! release 在 Windows 上走 GUI 子系统(见 `main.rs` 文件头的 `windows_subsystem`),
//! 进程没有控制台,`GetStdHandle(STD_ERROR_HANDLE)` 返回 NULL,std 的 stderr
//! 写入把这种情况当 EBADF **静默吞掉**(`library/std/src/sys/stdio/windows.rs`
//! 的 `is_ebadf`)。于是全仓一百多处 `eprintln!`、panic 钩子、启动埋点在装机版里
//! 一个字都留不下 —— 用户报「状态灯不亮 / 闪退」时交不出任何线索。
//!
//! # 做法:换句柄,不换调用点
//!
//! 不引日志框架、不改任何 `eprintln!`:以追加方式打开日志文件,`SetStdHandle`
//! 装到 STD_ERROR_HANDLE 与 STD_OUTPUT_HANDLE 上。std 的 stderr **每次写都重新
//! `GetStdHandle`**(同一文件的 `write()`),不缓存句柄,所以装上之后一切照旧走
//! `eprintln!` 就落文件;非控制台句柄走的是裸 `WriteFile`,UTF-8 字节原样进文件。
//! 第三方 crate 往 stderr 写的东西也一并收下。
//!
//! # 什么时候装
//!
//! 只在**本来就没有 stderr** 时装 —— 也就是装机版。`cargo run` 有控制台时不碰,
//! 日志照旧附着当前终端;E2E 驱动脚本抓 stderr 的那条路也不受影响。
//! `MT_LOG_FILE=1` 可强制落文件(在控制台里复现装机版这条路径时用)。
//!
//! macOS / Linux 暂不接:那两家由 Finder / 桌面启动时 fd 2 通常指向 /dev/null
//! 或系统日志,与「句柄为空」不是同一种缺席,判据另议;本模块在非 Windows 上是
//! 空操作,`MT_LOG_FILE` 也只在 Windows 生效。
//!
//! # 轮转
//!
//! 启动时若文件超过 [`MAX_LOG_BYTES`],改名成 `mini-term.log.1`(只留一代,旧的
//! 覆盖)再新开。**运行期不轮转**:`eprintln!` 是稀疏事件(启动埋点、失败分支、
//! panic),一次会话写不到这个量;真要在运行期切文件得换掉 std 句柄上的写入方,
//! 那就不再是「零改动接管」了。
//!
//! 每次启动先写一行会话头(版本 / pid / 时刻),多次运行的日志拼在同一个文件里
//! 也分得开。

use std::path::{Path, PathBuf};

/// 日志文件名。落在 `{active_data_dir}` 下,与 `config.db` / `layout.db` 同目录。
pub const LOG_FILE_NAME: &str = "mini-term.log";

/// 启动时超过这个体积就轮转。2 MB 够装几千次启动的埋点与偶发的错误行。
pub const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

/// 强制落文件的环境变量。取值 `1` 生效,其它一律按未设处理。
const FORCE_ENV: &str = "MT_LOG_FILE";

/// 在 `main` 的**第一行**调用。装不上(目录建不出来 / 文件开不了 / 平台不支持)
/// 就什么都不做 —— 日志是锦上添花,不能成为启动失败的理由。
pub fn install() {
    let force = std::env::var_os(FORCE_ENV).is_some_and(|v| v == "1");
    if !should_redirect(platform::stderr_present(), force) {
        return;
    }
    let Some(path) = log_path() else {
        return;
    };
    rotate_if_oversized(&path, MAX_LOG_BYTES);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    use std::io::Write as _;
    let _ = writeln!(
        file,
        "{}",
        session_header(
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            &chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
    );
    platform::redirect_std_handles(file);
}

/// 要不要接管:没有 stderr 才接,或者被环境变量强制。纯函数,便于单测。
fn should_redirect(stderr_present: bool, force: bool) -> bool {
    force || !stderr_present
}

/// `{active_data_dir}/mini-term.log`,目录不存在就建。
fn log_path() -> Option<PathBuf> {
    let dir = mt_config::active_data_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(LOG_FILE_NAME))
}

/// 超过 `limit` 字节就把 `path` 改名成 `path.1`(旧的一代覆盖掉)。
/// 改名失败(文件被占着之类)就原地继续追加,不阻断启动。
fn rotate_if_oversized(path: &Path, limit: u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= limit {
        return;
    }
    let rotated = rotated_path(path);
    let _ = std::fs::remove_file(&rotated);
    let _ = std::fs::rename(path, &rotated);
}

/// `mini-term.log` → `mini-term.log.1`。
fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

/// 会话头。`eprintln!` 的内容没有时间戳,这一行是同文件里多次运行之间唯一的分界。
fn session_header(version: &str, pid: u32, started_at: &str) -> String {
    format!("==== mini-term v{version} pid={pid} started {started_at} ====")
}

#[cfg(windows)]
mod platform {
    use std::fs::File;
    use std::os::windows::io::IntoRawHandle as _;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };

    /// 进程现在有没有可用的 stderr。GUI 子系统且无人继承句柄时是 NULL;
    /// `windows` crate 把 INVALID_HANDLE_VALUE 映成 `Err`,NULL 有没有被一并
    /// 归入 `is_invalid` 各版本不一,所以两种都当「没有」处理。
    pub(super) fn stderr_present() -> bool {
        // SAFETY: 只查询进程的标准句柄表,不改任何状态。
        match unsafe { GetStdHandle(STD_ERROR_HANDLE) } {
            Ok(handle) => !handle.0.is_null(),
            Err(_) => false,
        }
    }

    /// 把 stderr / stdout 都指到这个文件。句柄**故意泄漏**:它就是进程余生的
    /// stderr,关掉等于把日志再次扔进黑洞。
    pub(super) fn redirect_std_handles(file: File) {
        let handle = HANDLE(file.into_raw_handle());
        // SAFETY: 句柄由刚打开的 File 交出所有权,之后不再经 Rust 侧关闭;
        // 调用发生在 `main` 第一行,尚无其它线程读写标准句柄。
        unsafe {
            let _ = SetStdHandle(STD_ERROR_HANDLE, handle);
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, handle);
        }
    }
}

#[cfg(not(windows))]
mod platform {
    /// 非 Windows 一律按「有 stderr」处理 —— 见模块注释,那两家的缺席形态不同。
    pub(super) fn stderr_present() -> bool {
        true
    }

    /// 非 Windows 不接管;文件在这里被丢弃,只留下那一行会话头。
    pub(super) fn redirect_std_handles(_file: std::fs::File) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 没有_stderr_才接管_强制开关例外() {
        assert!(should_redirect(false, false), "装机版:没有 stderr,接管");
        assert!(!should_redirect(true, false), "cargo run:有控制台,不碰");
        assert!(should_redirect(true, true), "MT_LOG_FILE=1 强制落文件");
        assert!(should_redirect(false, true));
    }

    #[test]
    fn 会话头带版本_pid_与时刻() {
        let line = session_header("1.2.6", 4242, "2026-09-05T10:00:00+08:00");
        assert_eq!(
            line,
            "==== mini-term v1.2.6 pid=4242 started 2026-09-05T10:00:00+08:00 ===="
        );
    }

    #[test]
    fn 轮转文件名只是在原名后追加_1() {
        let p = rotated_path(Path::new(r"D:\data\mini-term.log"));
        assert_eq!(p.file_name().unwrap(), "mini-term.log.1");
        assert_eq!(p.parent(), Path::new(r"D:\data\mini-term.log").parent());
    }

    #[test]
    fn 超限才轮转_且只留一代() {
        let dir = std::env::temp_dir().join(format!("mt-logfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(LOG_FILE_NAME);
        let old = rotated_path(&path);

        // 不存在:不动
        rotate_if_oversized(&path, 10);
        assert!(!path.exists() && !old.exists());

        // 没超限:不动
        std::fs::write(&path, b"short").unwrap();
        rotate_if_oversized(&path, 10);
        assert_eq!(std::fs::read(&path).unwrap(), b"short");
        assert!(!old.exists());

        // 超限:挪成 .1,原位腾空
        std::fs::write(&path, vec![b'x'; 11]).unwrap();
        rotate_if_oversized(&path, 10);
        assert!(!path.exists(), "原文件应已改名");
        assert_eq!(std::fs::read(&old).unwrap().len(), 11);

        // 再超限一次:上一代被覆盖,不会长出 .2
        std::fs::write(&path, vec![b'y'; 12]).unwrap();
        rotate_if_oversized(&path, 10);
        assert_eq!(std::fs::read(&old).unwrap().len(), 12, "旧的一代被新的覆盖");
        assert!(!dir.join("mini-term.log.2").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
