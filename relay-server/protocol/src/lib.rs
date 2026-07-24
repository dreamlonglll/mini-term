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
    /// 请求签发一次性配对码(用于桌面端展示二维码)。旧配对码立即作废。
    RequestPairingCode,
    /// 重置配对:吊销移动端长期凭证与未用配对码,踢掉在线移动端。
    ResetPairing,
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
    /// 响应 RequestPairingCode:新签发的一次性配对码。
    PairingCode { code: String },
    /// 配对状态变化:移动端兑换凭证成功(true)/凭证被吊销或重置(false)。
    /// 桌面端握手成功后也会立即收到一次当前状态。
    PairingUpdate { paired: bool },
}

/// 移动端 → 中转
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum MobileToRelay {
    /// 握手:二选一携带一次性配对码(扫码首连)或长期凭证(重连)。
    Hello {
        protocol_version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pairing_code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<String>,
    },
}

/// 中转 → 移动端
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RelayToMobile {
    /// 握手成功。configured pairing 时携带新签发的长期凭证,重连时为 None。
    HelloAck {
        protocol_version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<String>,
    },
    /// 握手拒绝;发送后中转立即关闭连接。
    HelloReject { reason: MobileRejectReason },
    /// 已建立的连接被吊销(新设备配对顶替 / 桌面端重置配对),随后关闭连接。
    /// 移动端应清除本地凭证并提示重新扫码。
    Revoked,
}

/// 移动端握手被拒绝的原因。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MobileRejectReason {
    /// 协议版本不匹配
    VersionMismatch,
    /// 配对码无效/已用/已过期
    InvalidPairingCode,
    /// 凭证无效或已被吊销
    InvalidCredential,
    /// 既无配对码也无凭证
    MissingAuth,
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

    #[test]
    fn mobile_hello_with_pairing_code_round_trip() {
        let msg = MobileToRelay::Hello {
            protocol_version: PROTOCOL_VERSION,
            pairing_code: Some("abc123".into()),
            credential: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            json.contains(r#""pairingCode":"abc123""#) && !json.contains("credential"),
            "{json}"
        );
        let parsed: MobileToRelay = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn mobile_hello_with_credential_round_trip() {
        let msg = MobileToRelay::Hello {
            protocol_version: PROTOCOL_VERSION,
            pairing_code: None,
            credential: Some("tok".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: MobileToRelay = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn mobile_reject_reason_serializes_camel_case() {
        let msg = RelayToMobile::HelloReject {
            reason: MobileRejectReason::InvalidPairingCode,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""reason":"invalidPairingCode""#), "{json}");
        let parsed: RelayToMobile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn pairing_messages_round_trip() {
        let code = RelayToDesktop::PairingCode {
            code: "deadbeef".into(),
        };
        let json = serde_json::to_string(&code).unwrap();
        assert!(json.contains(r#""type":"pairingCode""#), "{json}");
        assert_eq!(serde_json::from_str::<RelayToDesktop>(&json).unwrap(), code);

        let update = RelayToDesktop::PairingUpdate { paired: true };
        let json = serde_json::to_string(&update).unwrap();
        assert!(json.contains(r#""type":"pairingUpdate""#), "{json}");
        assert_eq!(serde_json::from_str::<RelayToDesktop>(&json).unwrap(), update);

        let req = DesktopToRelay::RequestPairingCode;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"type":"requestPairingCode"}"#);

        let ack = RelayToMobile::HelloAck {
            protocol_version: PROTOCOL_VERSION,
            credential: Some("secret".into()),
        };
        let json = serde_json::to_string(&ack).unwrap();
        assert_eq!(serde_json::from_str::<RelayToMobile>(&json).unwrap(), ack);
    }
}
