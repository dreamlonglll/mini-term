//! 已存 SSH 密码的封存 / 解封在壳里的接线(`mt-secret`)。
//!
//! 磁盘与内存里 `SshConnection.password` 一律是信封串(`enc:v1:…`),只有三处
//! 需要明文:
//!
//! 1. 编辑表单回填(`ssh_panel::new_form`);
//! 2. 终端自动填充(`pane::connect_ssh` / `mt_remote::prepare_remote_launch`;
//!    后者在独立 crate 里,直连 `mt_secret::reveal_global`,与 [`reveal_password`] 同一把钥匙);
//! 3. `mt-ssh` 会话池认证 —— 那处在 mt-ssh 内部解,三个 sidecar 同一条路。
//!
//! 封存只发生在一处:[`crate::store::AppStore::upsert_ssh_connection`]。
//! 进程级凭据库由 `mt_config::ConfigStore::load` 登记(dev 实例的隔离目录也因此走对),
//! 这里只是取用。
//!
//! 移动端中转的桌面密钥(`MobileRelayConfig.desktop_key`)搭同一套:封存点是
//! [`crate::store::AppStore::set_mobile_relay_endpoint`]([`stored_relay_key`]),
//! 解封点两处 —— 启动建连(`mobile_relay::install`)与面板回填(`mobile_panel::open`),
//! 都走 [`reveal_relay_key`]。解开后的明文交给 mt-relay,mt-relay 本身不认识信封。
//!
//! 远程通知的三处凭据(Bark 设备 key / Telegram bot token / Webhook URL)同一口径:
//! 封存点 [`crate::store::AppStore::set_remote_notify_secret`]([`stored_secret`]),
//! 解封点两处 —— 发送那一刻(`remote_notify::build_request`)与设置页回填
//! (`settings::pages_remote_notify`),都走 [`reveal_secret`]。

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

/// 中转面板交来的桌面密钥明文 → 该存进配置的值。
///
/// 空串 = 未填,**原样存空**、不封存 —— 「没配」要是也变成一个信封,换机器后
/// 解不开时反倒要提示用户「重新填写」一个本来就没填过的东西。其余与
/// [`stored_password`] 同一口径(没改就沿用旧信封,不白白换 nonce 改写库)。
pub fn stored_relay_key(plain: &str, existing: &str) -> Result<String, String> {
    stored_secret(plain, existing)
}

/// 「空串 = 未填」类凭据的通用封存口径(中转桌面密钥、远程通知的三处凭据):
/// 空串原样存空,其余与 [`stored_password`] 相同(没改就沿用旧信封)。
pub fn stored_secret(plain: &str, existing: &str) -> Result<String, String> {
    if plain.is_empty() {
        return Ok(String::new());
    }
    stored_password(plain, Some(existing))
}

/// [`stored_secret`] 的反方向:遗留明文与空串原样放行,解不开给可展示的原因。
pub fn reveal_secret(stored: &str) -> Result<String, String> {
    reveal_password(stored)
}

/// 已存的中转桌面密钥 → 交给 mt-relay 的明文。遗留明文与空串原样放行。
///
/// 解不开(换了机器 / `credential.key` 丢了)返回 `Err(可展示的原因)`,调用方
/// **按未填写处理**:建连拿空串去握手,中转回「密钥不正确」后停在明确状态上、
/// 不重连不刷屏;面板把密钥框回填为空并就地标红,等用户重填、必要时重新配对。
/// 与 SSH 密码同一条红线:**绝不把信封当密钥送出去**。
pub fn reveal_relay_key(stored: &str) -> Result<String, String> {
    reveal_password(stored)
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

    #[test]
    fn 中转密钥封存往返且没改就沿用旧信封() {
        install_test_vault();
        let stored = stored_relay_key("relay-k3y", "").unwrap();
        assert!(mt_secret::is_sealed(&stored));
        assert!(!stored.contains("relay-k3y"));
        assert_eq!(reveal_relay_key(&stored).unwrap(), "relay-k3y");
        assert_eq!(
            stored_relay_key("relay-k3y", &stored).unwrap(),
            stored,
            "没改密钥不该换信封"
        );
        // 旧值是遗留明文(升级前落下的)→ 换成信封
        assert!(mt_secret::is_sealed(
            &stored_relay_key("relay-k3y", "relay-k3y").unwrap()
        ));
    }

    #[test]
    fn 中转密钥空串是未填不封存() {
        install_test_vault();
        assert_eq!(stored_relay_key("", "").unwrap(), "");
        // 清空已存密钥:存空串,不是「空明文的信封」
        let stored = stored_relay_key("relay-k3y", "").unwrap();
        assert_eq!(stored_relay_key("", &stored).unwrap(), "");
        assert_eq!(reveal_relay_key("").unwrap(), "");
    }

    /// 换机器 / 密钥文件丢了:别的钥匙封的信封解不开,给出可展示的原因,
    /// 而**不是**把信封原样当密钥交出去。
    #[test]
    fn 中转密钥解不开时报错而不是交出信封() {
        install_test_vault();
        let foreign = mt_secret::Vault::generate()
            .unwrap()
            .seal("relay-k3y")
            .unwrap();
        let err = reveal_relay_key(&foreign).expect_err("别的钥匙封的信封不该解得开");
        assert!(!err.is_empty());
        assert!(!err.contains("enc:"), "原因里不该带信封本身: {err}");
        // 遗留明文(升级窗口期)照常放行
        assert_eq!(reveal_relay_key("legacy-k3y").unwrap(), "legacy-k3y");
    }
}
