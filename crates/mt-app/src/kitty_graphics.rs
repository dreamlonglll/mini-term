//! Kitty 图形协议的总开关与环境变量注入。协议本身在 `mt_terminal::kitty`。
//!
//! # 默认关闭
//!
//! 只有 `MT_ENABLE_KITTY_GRAPHICS=1` 才开,不进设置页。关着时预解析器不介入,字节流与
//! 现在一样,也不注入 `PI_IMAGE_PROTOCOL`。
//!
//! # 为什么还要看 ConPTY 后端
//!
//! 实测(真实 pi-tui → ConPTY → 宿主,SHA-256 校验):便携 ConPTY 1.24 把 APC 字节完好
//! 透传,系统 conhost 把图形序列整段丢弃。回落系统 ConPTY 时开了也画不出来,还会让
//! pi 以为能发图、发完屏幕上什么都没有,所以按没开算。非 Windows 没有 ConPTY,可以开。
//!
//! # 注入
//!
//! `PI_IMAGE_PROTOCOL=kitty` 直接盖过 pi 的探测(不用伪装 `TERM_PROGRAM`)。只给本机
//! shell:SSH 的 ssh 客户端不转发环境变量,WSL 的 `wsl.exe` 不经 `WSLENV` 也传不进
//! distro;这两条路上图形序列能不能到达也没验过。用户优先:放进 `PtySpawn::env`,而
//! `mt_pty` 的启动计划把用户的环境变量排在它后面;启动 mini-term 时环境里已经有这个
//! 变量也不覆盖。

use std::ffi::OsStr;

const ENABLE_ENV: &str = "MT_ENABLE_KITTY_GRAPHICS";
const PROTOCOL_ENV: &str = "PI_IMAGE_PROTOCOL";

/// 这次运行要不要开 Kitty 图形。**便携 ConPTY 预载之后才有意义**:`main()` 一开始就做了,
/// 建 pane 都在那之后。
pub fn active() -> bool {
    decide(
        std::env::var_os(ENABLE_ENV).as_deref(),
        cfg!(windows),
        mt_pty::conpty::portable_active(),
    )
}

/// 开关只认 `1`,别的写法一律当没设(与 `MT_LOG_FILE` / `MT_DISABLE_PORTABLE_CONPTY` 同一口径)。
fn decide(flag: Option<&OsStr>, windows: bool, portable: bool) -> bool {
    flag.is_some_and(|value| value == "1") && (!windows || portable)
}

/// 新建终端时注入的环境变量。`None` = 这个 pane 不注入。
pub fn protocol_env(
    active: bool,
    is_remote: bool,
    is_wsl: bool,
    inherited: Option<&OsStr>,
) -> Option<(String, String)> {
    (active && !is_remote && !is_wsl && inherited.is_none())
        .then(|| (PROTOCOL_ENV.to_string(), "kitty".to_string()))
}

/// 这个 pane 进的是 WSL 吗:配的 shell 就是 `wsl`,或者 cwd 是 WSL UNC(`mt_pty` 会改用
/// `wsl.exe` 启动)。
pub fn is_wsl_launch(cwd: &str, shell_command: &str) -> bool {
    let program = shell_command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(shell_command);
    let stem = program.rsplit_once('.').map_or(program, |(stem, _)| stem);
    stem.eq_ignore_ascii_case("wsl") || mt_core::parse_wsl_unc(cwd).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 开关只认_1() {
        let one = Some(OsStr::new("1"));
        assert!(decide(one, true, true));
        for value in ["0", "", "true", " 1"] {
            assert!(!decide(Some(OsStr::new(value)), true, true), "{value:?}");
        }
        assert!(!decide(None, true, true));
    }

    /// Windows 上只有便携后端才开;别的平台没有这一说。
    #[test]
    fn 系统_conhost_上不开() {
        let one = Some(OsStr::new("1"));
        assert!(!decide(one, true, false));
        assert!(decide(one, false, false));
    }

    #[test]
    fn 注入只给开着的本机_pane() {
        let inject = Some(("PI_IMAGE_PROTOCOL".to_string(), "kitty".to_string()));
        assert_eq!(protocol_env(true, false, false, None), inject);
        assert_eq!(protocol_env(false, false, false, None), None, "没开");
        assert_eq!(protocol_env(true, true, false, None), None, "SSH");
        assert_eq!(protocol_env(true, false, true, None), None, "WSL");
        assert_eq!(
            protocol_env(true, false, false, Some(OsStr::new("none"))),
            None,
            "环境里已经有的不覆盖"
        );
    }

    #[test]
    fn 认得出_wsl() {
        for command in ["wsl", "wsl.exe", "WSL.EXE", r"C:\Windows\System32\wsl.exe"] {
            assert!(is_wsl_launch(r"C:\work", command), "{command}");
        }
        for command in [
            "pwsh.exe",
            "cmd",
            "/bin/bash",
            "nu.exe",
            r"D:\wsl-tools\pwsh.exe",
        ] {
            assert!(!is_wsl_launch(r"C:\work", command), "{command}");
        }
        assert!(
            is_wsl_launch(r"\\wsl$\Ubuntu\home\u", "pwsh.exe"),
            "cwd 是 WSL UNC"
        );
    }
}
