//! daemon_spawn —— CLI 冷启动时 detached 拉起 `mt-ssh-cli daemon`。
//!
//! 拉起的 daemon 是常驻进程,必须与调用方彻底脱钩,共三件事:
//!
//! 1. **不继承调用方的标准句柄**(Windows)。std 的 `Command::spawn` 在 Windows 上以
//!    `bInheritHandles = TRUE` 调 `CreateProcessW`,且不带
//!    `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` —— 子进程继承本进程**所有**可继承句柄。调用方
//!    交给 CLI 的 stdout/stderr 管道写端恰好是可继承的(父进程正是靠继承把它们传给 CLI),
//!    于是被常驻 daemon 一并拿走:CLI 早已退出,`$(cli …)` / `cli … | tail` 的读端却要等
//!    daemon 空闲自退(10 分钟)才见 EOF,WSL interop 下调用本身都不返回。std 里能精确
//!    控制这件事的 `CommandExt::inherit_handles(false)` / `raw_attribute` 仍是 unstable,
//!    所以拉起前直接把本进程的三个标准句柄设成不可继承([`disable_std_handle_inheritance`])。
//!    改动只作用于本进程自己的句柄表,不波及父进程;CLI 除 daemon 外不再拉起任何子进程,
//!    即便将来要拉,`Stdio::inherit()` 也会另行复制出可继承句柄,不受影响。
//!    残留边界:调用方额外塞进来的**非标准**可继承句柄(例如 bash 的 `3>file`)仍会被
//!    daemon 继承 —— 根治只能自己调 `CreateProcessW` 配句柄白名单,目前不值得。
//! 2. **工作目录 = exe 所在目录**(取不到用系统临时目录)。不设的话 daemon 继承调用方的
//!    cwd —— 通常就是项目目录 —— Windows 上整段生命周期锁着它,改名/删除报 EBUSY。
//!    daemon 不依赖 cwd:upload/download 的本地路径在 CLI 端已绝对化,service 层
//!    `resolve_lenient` 只对相对路径才拼 cwd(收到的都是绝对路径),config.json / 审计
//!    日志 / known_hosts 都经环境变量与 home 定位。
//! 3. **stdout/stderr 接数据目录下的日志文件**([`crate::daemon_log`]),定位或打开失败
//!    退回 NUL。日志句柄经 `Stdio::from(File)` 只交给 daemon 这一个子进程。
//!
//! Unix 侧评估(结论:标准流不泄漏,不需要同类处理):std 在子进程里把 `Stdio::null()` /
//! 日志文件 dup2 到 0/1/2,调用方的标准流被覆盖、带不进 daemon;Rust 自己打开的 fd
//! (文件、socket、pipe)一律带 `O_CLOEXEC`,exec 时自动关闭。残留边界与 Windows 相同:
//! 调用方没设 CLOEXEC 的额外 fd(3 及以上)会跟进 daemon。第 2 条工作目录对 Unix 同样生效。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// detached 拉起 `<当前 exe> daemon`。
///
/// 成功只代表进程起来了;端点是否可连由调用方带退避重连确认。端点覆盖
/// (`MINITERM_SSH_CLI_ENDPOINT`)、`APPDATA` 等环境变量随环境继承透传,两端天然一致。
pub fn spawn_daemon_detached() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate own binary: {e}"))?;
    let (stdout, stderr) = daemon_output();
    let mut cmd = Command::new(&exe);
    cmd.arg("daemon")
        .current_dir(daemon_working_dir(&exe))
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        // DETACHED_PROCESS:不继承控制台(agent 的 Bash 结束不连坐 daemon);
        // CREATE_NEW_PROCESS_GROUP:Ctrl+C 信号不波及。
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        disable_std_handle_inheritance();
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 新进程组:调用方所在会话/作业结束不连坐 daemon(setsid 的轻量替代;
        // 标准流已改接 NUL / 日志文件,不依赖控制终端)。
        cmd.process_group(0);
    }
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to spawn daemon: {e}"))
}

/// daemon 的工作目录:exe 所在目录;取不到(或已不是目录)用系统临时目录。
pub fn daemon_working_dir(exe: &Path) -> PathBuf {
    exe.parent()
        .filter(|dir| dir.is_dir())
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir)
}

/// daemon 的 stdout / stderr:同一个日志文件的两个句柄(追加写)。数据目录定位不到
/// 或文件打不开就退回 NUL —— 日志只是排障辅助,绝不阻塞拉起。
fn daemon_output() -> (Stdio, Stdio) {
    let Some(path) = crate::daemon_log::log_path() else {
        return (Stdio::null(), Stdio::null());
    };
    match crate::daemon_log::open_for_daemon(&path) {
        Ok(file) => match file.try_clone() {
            Ok(dup) => (Stdio::from(dup), Stdio::from(file)),
            Err(_) => (Stdio::null(), Stdio::from(file)),
        },
        Err(_) => (Stdio::null(), Stdio::null()),
    }
}

/// 把本进程的 stdin / stdout / stderr 设为不可继承(见模块注释第 1 条)。
#[cfg(windows)]
pub fn disable_std_handle_inheritance() {
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: GetStdHandle 只读本进程参数块里的句柄值。
        let handle = unsafe { GetStdHandle(which) };
        if is_usable_std_handle(handle) {
            // 失败(例如古早 Windows 的控制台伪句柄)无从补救,也不影响 CLI 本身,忽略。
            let _ = clear_inherit_flag(handle);
        }
    }
}

/// `GetStdHandle` 的返回值能不能拿去改属性。两种「没有句柄」都得跳过:
/// - NULL:本进程根本没拿到这个标准句柄(DETACHED 拉起、父进程没给);
/// - `INVALID_HANDLE_VALUE`:调用失败。它数值上恰好等于「当前进程」伪句柄,
///   绝不能拿去 `SetHandleInformation`。
#[cfg(windows)]
fn is_usable_std_handle(handle: windows_sys::Win32::Foundation::HANDLE) -> bool {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    handle != 0 && handle != INVALID_HANDLE_VALUE
}

/// 清掉句柄的可继承位,其余属性不动;返回是否成功。
#[cfg(windows)]
fn clear_inherit_flag(handle: windows_sys::Win32::Foundation::HANDLE) -> bool {
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
    // SAFETY: handle 是本进程句柄表里的有效句柄;只改 INHERIT 位,不关闭它。
    unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_dir_is_the_exe_directory() {
        let exe = std::env::current_exe().unwrap();
        assert_eq!(daemon_working_dir(&exe), exe.parent().unwrap());
    }

    #[test]
    fn working_dir_falls_back_to_temp() {
        // 裸文件名:parent 是空路径,不是目录。
        assert_eq!(
            daemon_working_dir(Path::new("mt-ssh-cli.exe")),
            std::env::temp_dir()
        );
        // 所在目录已不存在(exe 被挪走等极端情况)。
        let gone = std::env::temp_dir()
            .join(format!("mt-ssh-cli-gone-{}", std::process::id()))
            .join("mt-ssh-cli.exe");
        assert_eq!(daemon_working_dir(&gone), std::env::temp_dir());
    }

    #[cfg(windows)]
    #[test]
    fn std_handle_sentinels_are_skipped() {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        assert!(!is_usable_std_handle(0));
        assert!(!is_usable_std_handle(INVALID_HANDLE_VALUE));
        assert!(is_usable_std_handle(0x44));
    }

    #[cfg(windows)]
    fn inherit_flag(handle: windows_sys::Win32::Foundation::HANDLE) -> bool {
        use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};
        let mut flags = 0u32;
        // SAFETY: handle 有效,flags 是本地可写变量。
        assert_ne!(unsafe { GetHandleInformation(handle, &mut flags) }, 0);
        flags & HANDLE_FLAG_INHERIT != 0
    }

    #[cfg(windows)]
    #[test]
    fn clear_inherit_flag_makes_an_inheritable_handle_private() {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT};

        let path = std::env::temp_dir().join(format!(
            "mt-ssh-cli-inherit-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = std::fs::File::create(&path).unwrap();
        let handle = file.as_raw_handle() as HANDLE;
        // 先人为设成可继承 —— 模拟父进程为传给 CLI 而建的管道写端。
        // SAFETY: handle 由 file 持有,在本测试内有效。
        assert_ne!(
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) },
            0
        );
        assert!(inherit_flag(handle));

        assert!(clear_inherit_flag(handle));
        assert!(!inherit_flag(handle));
        drop(file);
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(windows)]
    #[test]
    fn std_handles_end_up_non_inheritable() {
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };

        // 只改本测试进程自己的句柄表;拉子进程的 `Stdio::inherit()` 会另行复制可继承句柄,
        // 不影响其它用例。
        disable_std_handle_inheritance();
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: 同 disable_std_handle_inheritance。
            let handle = unsafe { GetStdHandle(which) };
            if is_usable_std_handle(handle) {
                assert!(
                    !inherit_flag(handle),
                    "std handle {which} still inheritable"
                );
            }
        }
    }
}
