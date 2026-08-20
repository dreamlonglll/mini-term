//! 本 crate 自用的小工具。
//!
//! [`is_wsl_unc_path`] 是 `mt_core::parse_wsl_unc` 的判定半边(中转只需要知道
//! "是不是 WSL UNC 路径",不需要拆出的 distro / unix_path)。
//!
//! 收尾-1 批之前这里是独立复刻的第三份匹配逻辑(mt-ai / mt-pty 走 mt-core /
//! mt-relay),理由是迁移期本 crate 不能引用 `src-tauri/`。mt-core 物理移入
//! `crates/` 后复刻已删:判定改为直接问 `mt_core::parse_wsl_unc` 有没有解出结果,
//! 函数签名、行为与下面的回归测试都一字未改。

/// 是否为 WSL UNC 路径。
///
/// 支持 `\\wsl$\<distro>\<rest>` / `\\wsl.localhost\...` / `\\?\UNC\...` 三种形式,
/// host 名大小写不敏感,distro 缺失不算。纯字符串匹配,不做磁盘访问。
///
/// 与 `mt_core::parse_wsl_unc` 是同一份判定:解得出 `WslPath` 即为 WSL UNC 路径
/// (mt-core 侧「host 命中 + distro 非空」两个条件与本函数原复刻逐条对应)。
pub fn is_wsl_unc_path(path: &str) -> bool {
    mt_core::parse_wsl_unc(path).is_some()
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
