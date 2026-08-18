//! 本 crate 自用的小工具。
//!
//! [`is_wsl_unc_path`] 是 `mt_core::parse_wsl_unc` 的判定半边(中转只需要知道
//! "是不是 WSL UNC 路径",不需要拆出的 distro / unix_path)。迁移期本 crate
//! **不引用 `src-tauri/`**:新工作区一旦反向依赖旧目录树,`cargo build -p mt-relay`
//! 就会把整套 Tauri 依赖拖进来,并存的前提也就没了。mt-ai / mt-config 也各留了
//! 一份同源复刻,去重放到收尾阶段(mt-core 物理移入 `crates/` 时)统一做。

/// 是否为 WSL UNC 路径。
///
/// 逐字对应 `mt_core::wsl_path::parse_unc` 的匹配部分:支持
/// `\\wsl$\<distro>\<rest>` / `\\wsl.localhost\...` / `\\?\UNC\...` 三种形式,
/// host 名大小写不敏感。纯字符串匹配,不做磁盘访问。
pub fn is_wsl_unc_path(path: &str) -> bool {
    // 先尝试剥 `\\?\UNC\` verbatim 前缀,剥不掉再尝试普通 `\\`。
    // 注意 strip_prefix("\\\\?\\UNC\\") 必须在 strip_prefix("\\\\") 之前,
    // 否则前者会被后者吞掉前两个反斜杠后落到非匹配分支。
    let Some(after_prefix) = path
        .strip_prefix(r"\\?\UNC\")
        .or_else(|| path.strip_prefix(r"\\"))
    else {
        return false;
    };

    // 分成 host \ distro \ rest 三段;distro 为空不算(如 `\\wsl$`)。
    let mut parts = after_prefix.splitn(3, '\\');
    let Some(host) = parts.next() else {
        return false;
    };
    let distro = parts.next().unwrap_or("");

    let host_lower = host.to_ascii_lowercase();
    (host_lower == "wsl$" || host_lower == "wsl.localhost") && !distro.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_all_wsl_unc_forms() {
        assert!(is_wsl_unc_path(r"\\wsl$\Ubuntu\home\u\proj"));
        assert!(is_wsl_unc_path(r"\\wsl.localhost\Debian\srv"));
        assert!(is_wsl_unc_path(r"\\?\UNC\wsl$\Ubuntu\home\u"));
        // host 名大小写不敏感
        assert!(is_wsl_unc_path(r"\\WSL.LocalHost\Ubuntu\home\u"));
        // distro 缺失不算
        assert!(!is_wsl_unc_path(r"\\wsl$"));
        // 普通本地 / 网络路径
        assert!(!is_wsl_unc_path(r"D:\Git\mini-term"));
        assert!(!is_wsl_unc_path("/home/u/proj"));
        assert!(!is_wsl_unc_path(r"\\server\share\dir"));
    }
}
