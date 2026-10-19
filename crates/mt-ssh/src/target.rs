//! 连接配置 → 建连目标:主机名、端口、登录名、私钥与 known_hosts 名。
//!
//! 只看连接配置本身。`~/.ssh` 里本 crate 只碰 `known_hosts`(见 `known_hosts.rs`):
//! 不读 `~/.ssh/config`(别名、`HostName`、跳板、`IdentityFile` 一律不认),也不试默认私钥
//! `~/.ssh/id_*`。连接里填的 host 就是真正去连的主机名,私钥只用连接里配的那一把。
//! (曾按终端里系统 ssh 的口径读过 `~/.ssh/config` 与默认私钥,随后撤回。)

use std::path::{Path, PathBuf};

use mt_core::SshConnection;

/// 连接里配的私钥。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdentitySpec {
    /// 用户写的原样(报错里展示)。
    pub(crate) display: String,
    /// 展开 `~` 后的路径;展开失败时是失败原因(留到认证阶段写进报错)。
    pub(crate) path: Result<PathBuf, String>,
}

/// 一条连接解析后的建连目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedHost {
    pub(crate) host_name: String,
    pub(crate) port: u16,
    pub(crate) user: String,
    /// 连接里配的私钥;没配时为 `None`(公钥只剩 ssh-agent 里的 key)。
    pub(crate) identity: Option<IdentitySpec>,
    /// known_hosts 查找 / 写入名:主机名转小写,非 22 端口按 `[host]:port` 带上
    /// (OpenSSH 写入口径;匹配本身不区分大小写)。
    pub(crate) known_hosts_name: String,
}

impl ResolvedHost {
    /// `host_name:port`,报错与阶段描述用。
    pub(crate) fn endpoint(&self) -> String {
        format!("{}:{}", self.host_name, self.port)
    }
}

/// 解析一条连接的建连目标(对齐终端 `ssh [-p port] [-i key] user@host` 的效果):
///
/// - 主机名:连接的 host 原样(去掉首尾空白);
/// - 端口:0 视作 22;
/// - 用户:连接的用户为空时用本机用户名(OpenSSH 同样如此),两边都没有时报错;
/// - 私钥:连接配了才有,`~` 展开到 home;展开失败不在这里报错,留给认证阶段。
pub(crate) fn resolve_target(
    conn: &SshConnection,
    home: Option<&Path>,
    local_user: Option<&str>,
) -> Result<ResolvedHost, String> {
    let host_name = conn.host.trim();
    if host_name.is_empty() {
        return Err("ssh connection has an empty host".into());
    }
    let port = if conn.port == 0 { 22 } else { conn.port };
    let user = if !conn.user.trim().is_empty() {
        conn.user.clone()
    } else if let Some(user) = local_user {
        user.to_string()
    } else {
        return Err(format!(
            "no user name for {host_name}: the connection has no user and the local user name \
             is unknown"
        ));
    };
    let identity = conn
        .identity_file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|raw| IdentitySpec {
            display: raw.to_string(),
            path: expand_tilde(raw, home),
        });
    Ok(ResolvedHost {
        known_hosts_name: crate::known_hosts::host_pattern(&host_name.to_ascii_lowercase(), port),
        host_name: host_name.to_string(),
        port,
        user,
        identity,
    })
}

/// 把 `~` / `~/…` / `~\…` 展开到 home。`~user` 形式不支持(报错,不瞎猜别人的 home)。
fn expand_tilde(path: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    let rest = if path == "~" {
        Some("")
    } else {
        path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"))
    };
    match rest {
        Some(rest) => match home {
            Some(home) if rest.is_empty() => Ok(home.to_path_buf()),
            Some(home) => Ok(home.join(rest)),
            None => Err(format!(
                "cannot expand '~' in '{path}': home directory unknown"
            )),
        },
        None if path.starts_with('~') => Err(format!(
            "cannot expand '{path}': only '~' / '~/...' (the current user's home) is supported"
        )),
        None => Ok(PathBuf::from(path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(host: &str, port: u16, user: &str) -> SshConnection {
        SshConnection {
            id: "c".into(),
            name: "c".into(),
            host: host.into(),
            port,
            user: user.into(),
            password: None,
            identity_file: None,
            group: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn expand_tilde_variants() {
        let home = Path::new("/home/me");
        assert_eq!(expand_tilde("~", Some(home)).unwrap(), home);
        assert_eq!(
            expand_tilde("~/.ssh/id", Some(home)).unwrap(),
            home.join(".ssh/id")
        );
        assert_eq!(
            expand_tilde(r"~\.ssh\id", Some(home)).unwrap(),
            home.join(r".ssh\id")
        );
        assert_eq!(
            expand_tilde("/abs/key", Some(home)).unwrap(),
            PathBuf::from("/abs/key")
        );
        assert!(expand_tilde("~/.ssh/id", None).is_err());
        assert!(expand_tilde("~other/.ssh/id", Some(home)).is_err());
    }

    #[test]
    fn resolve_target_uses_the_connection_as_is() {
        let home = Path::new("/home/me");
        let target = resolve_target(
            &connection(" Web.Example.com ", 0, "deploy"),
            Some(home),
            None,
        )
        .unwrap();
        assert_eq!(target.host_name, "Web.Example.com");
        assert_eq!(target.port, 22);
        assert_eq!(target.user, "deploy");
        assert_eq!(target.identity, None);
        assert_eq!(target.known_hosts_name, "web.example.com");
        assert_eq!(target.endpoint(), "Web.Example.com:22");

        let mut conn = connection("h.example.com", 2222, "");
        conn.identity_file = Some("~/.ssh/work".into());
        let target = resolve_target(&conn, Some(home), Some("me")).unwrap();
        assert_eq!(target.user, "me", "空用户回落到本机用户名");
        assert_eq!(target.known_hosts_name, "[h.example.com]:2222");
        let identity = target.identity.unwrap();
        assert_eq!(identity.display, "~/.ssh/work");
        assert_eq!(identity.path.unwrap(), home.join(".ssh/work"));
    }

    #[test]
    fn resolve_target_reports_what_it_cannot_resolve() {
        assert!(resolve_target(&connection("  ", 22, "u"), None, None)
            .unwrap_err()
            .contains("empty host"));
        let err = resolve_target(&connection("h.example.com", 22, ""), None, None).unwrap_err();
        assert!(err.starts_with("no user name"), "{err}");

        // home 未知:`~` 展开失败留到认证阶段作为该私钥的失败原因,目标本身照常。
        let mut conn = connection("h.example.com", 22, "u");
        conn.identity_file = Some("~/.ssh/key".into());
        let target = resolve_target(&conn, None, None).unwrap();
        assert!(target.identity.unwrap().path.is_err());
    }
}
