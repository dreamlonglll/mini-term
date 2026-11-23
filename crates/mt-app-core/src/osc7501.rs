//! OSC 7501（Program Status Protocol）的系统 ConPTY 环境变量兜底。
//!
//! # 为什么多数场景不需要环境变量
//!
//! `mt-terminal` 会主动应答 `ESC ] 7501 ; ? (BEL|ST)` 查询。实测便携 ConPTY 1.24 与
//! 非 Windows PTY 的输入输出双向都原样透传 OSC 7501，Antigravity CLI（`agy` 1.5.14）
//! 启动时自动探测即可启用，无需注入任何环境变量（WSL 与内置 SSH 同理自动探测）。
//!
//! # 为什么 Windows 系统 ConPTY 需要兜底
//!
//! 实测 Windows 系统 `conhost.exe` 会原样透传输出方向的 OSC 7501，但会静默吞掉宿主
//! 写回输入方向的 OSC 应答 —— `agy` 的 `ESC ] 7501 ; ? BEL` 探测拿不到回应，会自动关掉
//! 状态上报。因此在 Windows 回落系统 ConPTY 的本机 pane 上，只要用户未显式设置
//! `AGY_CLI_OSC7501` 且未通过 `MT_DISABLE_OSC7501=1` 禁用，就注入 `AGY_CLI_OSC7501=on`
//! 绕过探测握手（用户自己在设置或环境里设为 `off` 仍优先）。

use std::ffi::OsStr;

const DISABLE_ENV: &str = "MT_DISABLE_OSC7501";
const AGY_OSC7501_ENV: &str = "AGY_CLI_OSC7501";

/// 新建终端时为系统 ConPTY 兜底注入的 `AGY_CLI_OSC7501=on`。`None` = 不注入。
pub fn agy_env_fallback(is_remote: bool, is_wsl: bool) -> Option<(String, String)> {
    decide_agy_env_fallback(
        std::env::var_os(DISABLE_ENV).as_deref(),
        cfg!(windows),
        mt_pty::conpty::portable_active(),
        is_remote,
        is_wsl,
        std::env::var_os(AGY_OSC7501_ENV).as_deref(),
    )
}

fn decide_agy_env_fallback(
    disable_flag: Option<&OsStr>,
    windows: bool,
    portable: bool,
    is_remote: bool,
    is_wsl: bool,
    inherited: Option<&OsStr>,
) -> Option<(String, String)> {
    let disabled = disable_flag.is_some_and(|v| v == "1");
    (!disabled && windows && !portable && !is_remote && !is_wsl && inherited.is_none())
        .then(|| (AGY_OSC7501_ENV.to_string(), "on".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 仅在_windows_系统_conpty_的本机_pane_注入兜底() {
        let on = Some(("AGY_CLI_OSC7501".to_string(), "on".to_string()));
        assert_eq!(
            decide_agy_env_fallback(None, true, false, false, false, None),
            on
        );
        // 便携 ConPTY 双向透传，靠标准查询握手自动协商，不注变量
        assert_eq!(
            decide_agy_env_fallback(None, true, true, false, false, None),
            None
        );
        // 非 Windows 不注变量
        assert_eq!(
            decide_agy_env_fallback(None, false, false, false, false, None),
            None
        );
        // 远程 / WSL 不注变量
        assert_eq!(
            decide_agy_env_fallback(None, true, false, true, false, None),
            None
        );
        assert_eq!(
            decide_agy_env_fallback(None, true, false, false, true, None),
            None
        );
        // 禁用开关生效时不注变量
        assert_eq!(
            decide_agy_env_fallback(Some(OsStr::new("1")), true, false, false, false, None),
            None
        );
        // 环境已预设时不覆盖用户选择
        assert_eq!(
            decide_agy_env_fallback(None, true, false, false, false, Some(OsStr::new("off"))),
            None
        );
    }
}
