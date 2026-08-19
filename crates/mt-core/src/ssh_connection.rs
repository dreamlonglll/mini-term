use serde::{Deserialize, Serialize};

/// 一条已保存的 SSH 连接。持久化在 `config.json` 的 `sshConnections` 数组里。
///
/// 该类型被 mini-term 主程序与 SSH MCP sidecar 共用,因此放在 `mt-core`。
///
/// 所有连接默认都能被 SSH MCP 工具访问;具体哪个项目的 agent 能看到哪些连接,
/// 由 `config.json` 里项目的 `sshConnectionIds` 决定(见 `config_reader`),
/// 连接本身不再带可见性开关。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshConnection {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_connection_deserializes() {
        let json = r#"{"id":"1","name":"prod","host":"10.0.0.5","port":22,"user":"root"}"#;
        let conn: SshConnection = serde_json::from_str(json).unwrap();
        assert_eq!(conn.id, "1");
        assert_eq!(conn.port, 22);
        assert!(conn.password.is_none());
    }

    #[test]
    fn connection_round_trips() {
        let conn = SshConnection {
            id: "abc".into(),
            name: "prod".into(),
            host: "example.com".into(),
            port: 2222,
            user: "deploy".into(),
            password: Some("secret".into()),
            identity_file: None,
            group: Some("内网".into()),
        };
        let json = serde_json::to_string(&conn).unwrap();
        let parsed: SshConnection = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.port, 2222);
        assert_eq!(parsed.group.as_deref(), Some("内网"));
    }

    #[test]
    fn fields_use_camel_case() {
        let conn = SshConnection {
            id: "1".into(),
            name: "n".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            password: None,
            identity_file: Some("/k".into()),
            group: None,
        };
        let json = serde_json::to_string(&conn).unwrap();
        assert!(json.contains("\"identityFile\":\"/k\""));
    }

    #[test]
    fn legacy_proxy_jump_field_is_ignored() {
        // 老配置文件里残留的 proxyJump 字段应被 serde 静默忽略,
        // 不破坏整体反序列化。
        let json = r#"{"id":"1","name":"n","host":"h","port":22,"user":"u","proxyJump":"user@bastion"}"#;
        let conn: SshConnection = serde_json::from_str(json).unwrap();
        assert_eq!(conn.id, "1");
    }
}
