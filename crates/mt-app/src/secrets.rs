//! 已存 SSH 密码的封存 / 解封在壳里的接线(`mt-secret`)。
//!
//! 磁盘与内存里 `SshConnection.password` 一律是信封串(`enc:v1:…`),只有三处
//! 需要明文:
//!
//! 1. 编辑表单回填(`ssh_panel::new_form`);
//! 2. 终端自动填充(`pane::connect_ssh` / `remote_ssh::prepare_remote_launch`);
//! 3. `mt-ssh` 会话池认证 —— 那处在 mt-ssh 内部解,三个 sidecar 同一条路。
//!
//! 封存只发生在一处:[`crate::store::AppStore::upsert_ssh_connection`]。
//! 进程级凭据库由 `mt_config::ConfigStore::load` 登记(dev 实例的隔离目录也因此走对),
//! 这里只是取用。

use gpui::App;

use crate::i18n::t;
use crate::notify::ToastKind;

/// 表单交来的明文 → 该存进配置的值。
///
/// 明文与旧信封解出来一致就**沿用旧信封**:信封每次封存 nonce 都不同,不沿用的话
/// [`crate::ssh_conn::ssh_session_identity_changed`] 会把「没改密码」误判成身份
/// 变了,白白作废池里的 session(sidecar 的池按同一字段比对,同理)。
pub fn stored_password(plain: &str, existing: Option<&str>) -> Result<String, String> {
    if let Some(existing) = existing.filter(|e| mt_secret::is_sealed(e))
        && mt_secret::reveal_global(existing).ok().as_deref() == Some(plain)
    {
        return Ok(existing.to_string());
    }
    mt_secret::seal_global(plain).map_err(|e| e.to_string())
}

/// 已存值 → 明文。遗留明文原样放行;解不开给可直接展示的中文
/// (`mt_secret::SecretError` 的 `Display`)。
pub fn reveal_password(stored: &str) -> Result<String, String> {
    mt_secret::reveal_global(stored).map_err(|e| e.to_string())
}

/// 解封失败的 toast。没有项目上下文,用合成的「SSH」项目名
/// (与 `toast::push_wsl_override` 同一种做法)。
///
/// ⚠️ 只给**不在弹窗里**的路径用(终端右键「SSH 连接」那条):toast 层画在弹窗遮罩
/// 之下,SSH 面板开着时推的 toast 根本看不见(真机验过),面板内的两处错误改成
/// 就地提示(`ssh_panel::ConnForm::password_unreadable` / `SshPanel::notice`)。
pub fn toast_password_error(message: String, cx: &mut App) {
    crate::toast::push_message(
        ToastKind::PasteError,
        "ssh-credential".into(),
        t("sshModal", "title").to_string(),
        message,
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试进程里登记一把随机钥匙(先到先得;真机数据目录那把不参与)。
    fn install_test_vault() {
        mt_secret::install(mt_secret::Vault::generate().unwrap());
    }

    #[test]
    fn 明文进信封出且能解回() {
        install_test_vault();
        let stored = stored_password("hunter2", None).unwrap();
        assert!(mt_secret::is_sealed(&stored));
        assert!(!stored.contains("hunter2"));
        assert_eq!(reveal_password(&stored).unwrap(), "hunter2");
    }

    #[test]
    fn 密码没改就沿用旧信封() {
        install_test_vault();
        let first = stored_password("hunter2", None).unwrap();
        let again = stored_password("hunter2", Some(&first)).unwrap();
        assert_eq!(again, first, "没改密码不该换信封,否则会误判身份变了");
        let changed = stored_password("hunter3", Some(&first)).unwrap();
        assert_ne!(changed, first);
        assert_eq!(reveal_password(&changed).unwrap(), "hunter3");
    }

    #[test]
    fn 旧值是遗留明文时不会被当成信封沿用() {
        install_test_vault();
        let stored = stored_password("hunter2", Some("hunter2")).unwrap();
        assert!(mt_secret::is_sealed(&stored), "遗留明文必须被换成信封");
    }

    #[test]
    fn 遗留明文原样解出() {
        assert_eq!(reveal_password("legacy-plain").unwrap(), "legacy-plain");
    }
}
