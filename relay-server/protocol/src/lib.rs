//! 中转协议 v1:JSON over WebSocket 的消息类型定义。
//!
//! 命名纪律(见 CONTEXT.md):移动端 / 中转服务器 / 配对 / 对话镜像 / 移动端指令。
//! 所有消息经 `#[serde(tag = "type", rename_all_fields = "camelCase")]` 序列化,
//! 与前端 TypeScript 手写镜像类型对齐;字段增删必须保持向后兼容或提升版本号。

use serde::{Deserialize, Serialize};

/// 协议版本。两端握手时校验,不匹配即拒绝(不静默错乱)。
pub const PROTOCOL_VERSION: u32 = 1;

/// 桌面端 → 中转
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum DesktopToRelay {
    /// 握手:桌面端连上 WebSocket 后必须发送的第一条消息。
    Hello { protocol_version: u32 },
}

/// 中转 → 桌面端
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RelayToDesktop {
    /// 握手成功。
    HelloAck { protocol_version: u32 },
    /// 版本不匹配等握手拒绝;发送后中转立即关闭连接。
    /// 桌面端据 expected/actual 给出明确升级提示,不再自动重连。
    HelloReject {
        expected_version: u32,
        actual_version: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_hello_camel_case_round_trip() {
        let msg = DesktopToRelay::Hello {
            protocol_version: PROTOCOL_VERSION,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            json.contains(r#""type":"hello""#) && json.contains(r#""protocolVersion":1"#),
            "serde camelCase 对齐被破坏: {json}"
        );
        let parsed: DesktopToRelay = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn hello_ack_round_trip() {
        let msg = RelayToDesktop::HelloAck {
            protocol_version: PROTOCOL_VERSION,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"helloAck""#), "{json}");
        let parsed: RelayToDesktop = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn hello_reject_round_trip() {
        let msg = RelayToDesktop::HelloReject {
            expected_version: 1,
            actual_version: 99,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            json.contains(r#""expectedVersion":1"#) && json.contains(r#""actualVersion":99"#),
            "{json}"
        );
        let parsed: RelayToDesktop = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn unknown_message_type_is_error_not_panic() {
        let err = serde_json::from_str::<DesktopToRelay>(r#"{"type":"noSuchMessage"}"#);
        assert!(err.is_err());
    }
}
