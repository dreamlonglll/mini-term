//! WSL UNC 路径解析。
//!
//! 用户在 Windows 上把 `\\wsl$\Ubuntu\home\user\proj` 加为项目根时,
//! 调用方需要识别该路径并提取 distro 名与 Linux 形式的子路径,
//! 然后用 `wsl.exe -d <distro> --cd <unix-path>` 启动终端
//! (CreateProcess 接收 UNC cwd 时 cmd.exe 会静默退回 C:\Windows,
//! 业界一致做法见 Windows Terminal `MangleStartingDirectoryForWSL` /
//! VS Code `getWslProfiles` / wezterm `WslDomain`)。
//!
//! 支持的输入形式:
//! - `\\wsl$\<distro>\<rest>` (Win10 18342+ 旧形式,仍兼容)
//! - `\\wsl.localhost\<distro>\<rest>` (Win10 build 21354+ 推荐形式)
//! - `\\?\UNC\wsl$\<distro>\<rest>` (Rust `canonicalize` 在 UNC 上的输出)
//! - `\\?\UNC\wsl.localhost\<distro>\<rest>` (同上)
//!
//! host 名按大小写不敏感匹配 (`WSL$` / `Wsl.Localhost` 也能识别),
//! distro 名保留原大小写 (`Ubuntu-22.04` 不会被改成 `ubuntu-22.04`)。

/// 解析结果。
///
/// `unix_path` 始终以 `/` 起头,空 path (如 `\\wsl$\Ubuntu`) 归一为 `/`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WslPath {
    pub distro: String,
    pub unix_path: String,
}

/// 解析任意路径字符串,匹配 WSL UNC 形式时返回 `Some(WslPath)`,否则 `None`。
///
/// 函数是纯字符串匹配,不做磁盘访问,跨平台行为一致 (Linux/macOS 上传入
/// `/home/u/proj` 这种普通路径自然不匹配 `\\` 前缀,返回 None)。
pub fn parse_unc(path: &str) -> Option<WslPath> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_wsl_dollar_form() {
        let p = parse_unc(r"\\wsl$\Ubuntu\home\user\proj").unwrap();
        assert_eq!(p.distro, "Ubuntu");
        assert_eq!(p.unix_path, "/home/user/proj");
    }

    #[test]
    fn parse_wsl_localhost_form() {
        let p = parse_unc(r"\\wsl.localhost\Ubuntu\home\user\proj").unwrap();
        assert_eq!(p.distro, "Ubuntu");
        assert_eq!(p.unix_path, "/home/user/proj");
    }

    #[test]
    fn parse_verbatim_unc_wsl_dollar() {
        let p = parse_unc(r"\\?\UNC\wsl$\Ubuntu\home\user").unwrap();
        assert_eq!(p.distro, "Ubuntu");
        assert_eq!(p.unix_path, "/home/user");
    }

    #[test]
    fn parse_verbatim_unc_wsl_localhost() {
        let p = parse_unc(r"\\?\UNC\wsl.localhost\Ubuntu\home\user").unwrap();
        assert_eq!(p.distro, "Ubuntu");
        assert_eq!(p.unix_path, "/home/user");
    }

    #[test]
    fn host_match_is_case_insensitive() {
        let p = parse_unc(r"\\WSL$\Ubuntu\home").unwrap();
        assert_eq!(p.distro, "Ubuntu");
        assert_eq!(p.unix_path, "/home");

        let p2 = parse_unc(r"\\Wsl.LocalHost\Ubuntu\home").unwrap();
        assert_eq!(p2.distro, "Ubuntu");
    }

    #[test]
    fn distro_name_preserves_case_and_special_chars() {
        let p = parse_unc(r"\\wsl$\Ubuntu-22.04\home").unwrap();
        assert_eq!(p.distro, "Ubuntu-22.04");
        assert_eq!(p.unix_path, "/home");
    }

    #[test]
    fn distro_root_returns_unix_root() {
        let p = parse_unc(r"\\wsl$\Ubuntu").unwrap();
        assert_eq!(p.distro, "Ubuntu");
        assert_eq!(p.unix_path, "/");
    }

    #[test]
    fn empty_distro_returns_none() {
        assert!(parse_unc(r"\\wsl$").is_none());
        assert!(parse_unc(r"\\wsl$\").is_none());
        assert!(parse_unc(r"\\?\UNC\wsl$\").is_none());
    }

    #[test]
    fn non_wsl_unc_returns_none() {
        assert!(parse_unc(r"\\server\share\folder").is_none());
        assert!(parse_unc(r"\\?\UNC\server\share\folder").is_none());
    }

    #[test]
    fn windows_drive_path_returns_none() {
        assert!(parse_unc(r"C:\proj").is_none());
        assert!(parse_unc(r"\\?\C:\proj").is_none());
    }

    #[test]
    fn unix_style_path_returns_none() {
        assert!(parse_unc("/home/user/proj").is_none());
        assert!(parse_unc("").is_none());
    }

    #[test]
    fn trailing_separator_is_preserved_in_unix_path() {
        let p = parse_unc(r"\\wsl$\Ubuntu\home\").unwrap();
        assert_eq!(p.distro, "Ubuntu");
        assert_eq!(p.unix_path, "/home/");
    }
}
