use serde::{Deserialize, Serialize};

/// 一条已保存的 SSH 连接。持久化在 `config.json` 的 `sshConnections` 数组里。
///
/// 该类型被 mini-term 主程序与 SSH MCP sidecar 共用,因此放在 `mt-core`。
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
    pub proxy_jump: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 是否允许终端里的 AI agent 通过 SSH MCP 调用此连接。默认 false。
    #[serde(default)]
    pub agent_accessible: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_connection_deserializes_without_agent_accessible() {
        // 旧 config.json 没有 agentAccessible 字段,须能反序列化并默认为 false
        let json = r#"{"id":"1","name":"prod","host":"10.0.0.5","port":22,"user":"root"}"#;
        let conn: SshConnection = serde_json::from_str(json).unwrap();
        assert_eq!(conn.id, "1");
        assert_eq!(conn.port, 22);
        assert!(!conn.agent_accessible);
        assert!(conn.password.is_none());
    }

    #[test]
    fn agent_accessible_round_trips() {
        let conn = SshConnection {
            id: "abc".into(),
            name: "jump-host".into(),
            host: "example.com".into(),
            port: 2222,
            user: "deploy".into(),
            password: Some("secret".into()),
            identity_file: None,
            proxy_jump: Some("user@bastion".into()),
            group: Some("内网".into()),
            agent_accessible: true,
        };
        let json = serde_json::to_string(&conn).unwrap();
        let parsed: SshConnection = serde_json::from_str(&json).unwrap();
        assert!(parsed.agent_accessible);
        assert_eq!(parsed.port, 2222);
        assert_eq!(parsed.proxy_jump.as_deref(), Some("user@bastion"));
    }

    #[test]
    fn agent_accessible_field_uses_camel_case() {
        let conn = SshConnection {
            id: "1".into(),
            name: "n".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            password: None,
            identity_file: None,
            proxy_jump: None,
            group: None,
            agent_accessible: true,
        };
        let json = serde_json::to_string(&conn).unwrap();
        assert!(json.contains("\"agentAccessible\":true"));
    }
}
