//! SSH 输出扫描:从 PTY 输出里识别密码提示 / 认证失败,用于自动填充密码。
//!
//! 这些都是纯函数,被 mini-term 主程序的 PTY 自动填充与 SSH MCP sidecar 共用。

/// 去除 ANSI 转义序列，返回纯文本
pub fn strip_ansi_codes(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some(&'[') => {
                    chars.next(); // consume '['
                                  // CSI sequence: skip until final byte (0x40–0x7E)
                    for c2 in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c2) {
                            break;
                        }
                    }
                }
                Some(&'O') => {
                    chars.next();
                    chars.next();
                } // SS3: ESC O <final>
                _ => {
                    chars.next();
                } // other two-char escape
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// SSH 输出扫描结果
pub enum SshPromptScan {
    /// 尚未命中
    None,
    /// 命中密码提示,应回写密码
    Password,
    /// 命中 "Permission denied, please try again.",密码错误,应停止自动填充
    AuthFailed,
}

/// 扫描(已 strip ANSI、仅保留尾部的)SSH 输出文本,判定是否到达密码提示。
/// 先判 AuthFailed:错误密码重试场景下残留 buffer 可能同时含旧的失败提示与新的
/// 密码提示,此时必须停止而非继续灌密码。
pub fn scan_ssh_prompt(text: &str) -> SshPromptScan {
    let lower = text.to_lowercase();
    if lower.contains("permission denied, please try again") {
        return SshPromptScan::AuthFailed;
    }
    // 命中 "<user>@<host>'s password:" 与键盘交互的 "Password:";
    // host-key 确认提示以 "?" 结尾、passphrase 提示以 "':" 结尾,均不会误命中。
    if lower.trim_end().ends_with("password:") {
        return SshPromptScan::Password;
    }
    SshPromptScan::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_ssh_prompt_detects_password() {
        assert!(matches!(
            scan_ssh_prompt("root@10.0.0.5's password: "),
            SshPromptScan::Password
        ));
        assert!(matches!(
            scan_ssh_prompt("Password:"),
            SshPromptScan::Password
        ));
    }

    #[test]
    fn scan_ssh_prompt_detects_auth_failure() {
        assert!(matches!(
            scan_ssh_prompt("Permission denied, please try again."),
            SshPromptScan::AuthFailed
        ));
    }

    #[test]
    fn scan_ssh_prompt_auth_failure_takes_priority() {
        // 错误密码后:残留 buffer 同时含失败提示与新密码提示 → 必须停止
        let buf = "Permission denied, please try again.\r\nroot@host's password: ";
        assert!(matches!(scan_ssh_prompt(buf), SshPromptScan::AuthFailed));
    }

    #[test]
    fn scan_ssh_prompt_ignores_hostkey_and_passphrase() {
        assert!(matches!(
            scan_ssh_prompt("Are you sure you want to continue connecting (yes/no/[fingerprint])? "),
            SshPromptScan::None
        ));
        assert!(matches!(
            scan_ssh_prompt("Enter passphrase for key '/home/u/.ssh/id_rsa': "),
            SshPromptScan::None
        ));
    }

    #[test]
    fn scan_ssh_prompt_ignores_plain_output() {
        assert!(matches!(
            scan_ssh_prompt("Last login: Mon May 18\r\n$ "),
            SshPromptScan::None
        ));
    }

    #[test]
    fn strip_ansi_codes_removes_csi_sequences() {
        assert_eq!(strip_ansi_codes("\x1b[31mred\x1b[0m"), "red");
    }

    #[test]
    fn strip_ansi_codes_keeps_plain_text() {
        assert_eq!(strip_ansi_codes("hello world"), "hello world");
    }
}
