//! 中转协议 v2:JSON over WebSocket 的消息类型定义。
//!
//! 命名纪律(见 CONTEXT.md):移动端 / 中转服务器 / 配对 / 对话镜像 / 移动端指令 /
//! AI 启动器。所有消息经 `#[serde(tag = "type", rename_all_fields = "camelCase")]`
//! 序列化,与前端 TypeScript 手写镜像类型对齐;字段增删必须保持向后兼容或提升版本号。

use serde::{Deserialize, Serialize};

/// 协议版本。两端握手时严格相等校验,不匹配即拒绝(不静默错乱)。
/// v2:桌面端握手携带共享密钥 + 移动端可发起 AI 会话(docs/adr/0002)。
pub const PROTOCOL_VERSION: u32 = 2;

/// 移动端可见的单个 pane(仅处于 AI 会话中的 pane 才会出现,裸 shell 不进快照)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MobilePane {
    pub pane_id: String,
    /// 展示名(自定义标题或 shell 名)
    pub title: String,
    /// 与桌面端 PaneStatus 字符串一致:"ai-working" | "ai-idle" | "error"
    pub status: String,
}

/// 移动端可见的项目条目。
///
/// v2 起**全部**项目进快照(没有活跃 AI 会话的项目 `panes` 为空数组),
/// 供发起会话弹层选目标;"仅 AI 会话 pane 可见"的规则只作用于 `panes`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MobileProject {
    pub project_id: String,
    pub name: String,
    pub panes: Vec<MobilePane>,
    /// 能否在该项目发起新 AI 会话(桌面端判定:SSH 远程项目与 WSL 根项目为 false,
    /// 因为它们的对话镜像目前一定是空的)。移动端据此置灰,不自行推断。
    #[serde(default)]
    pub can_start_session: bool,
    /// 该项目在桌面端项目树里的祖先分组名链(根→父),顶层项目为空。
    ///
    /// 只下发组**名**:移动端据此还原桌面端的分组层级,分组 id 与桌面端折叠态都不下发
    /// (手机屏小,跟着桌面折叠反而找不到项目,折叠状态由手机侧自己管)。
    /// 加字段是向后兼容的:旧桌面端不发 → 移动端平铺;旧中转会把它丢掉,同样退化为平铺。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_path: Vec<String>,
}

/// 移动端可见的 AI 启动器条目:**只有** id 与展示名。
/// 命令与 shell 归桌面端配置所有,绝不下发(见 ADR 0002 的边界)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MobileLauncher {
    pub id: String,
    pub name: String,
}

/// 移动端指令发送失败的原因。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandFailReason {
    /// 桌面端离线:中转在路由层直接拒绝(不做存储转发)
    DesktopOffline,
    /// 目标 pane 已关闭/AI 会话已结束
    PaneNotFound,
    /// PTY 写入失败
    WriteFailed,
}

/// 移动端发起新 AI 会话失败的原因。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StartSessionFailReason {
    /// 桌面端离线:中转在路由层直接拒绝(与移动端指令的离线即拒一致)
    DesktopOffline,
    /// 目标项目已不存在
    ProjectNotFound,
    /// 启动器已被删除
    LauncherNotFound,
    /// 目标项目不支持远程发起(SSH 远程项目 / WSL 根项目:对话镜像不可用)
    NotSupported,
    /// 终端创建失败
    SpawnFailed,
}

/// 桌面端握手被拒绝的原因(中转 → 桌面端)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DesktopRejectReason {
    /// 协议版本不匹配:两端必须同版本升级
    VersionMismatch,
    /// 桌面端密钥缺失或与中转配置的不一致
    InvalidKey,
    /// 中转未配置 `MT_RELAY_DESKTOP_KEY`:fail-closed,拒绝一切桌面连接
    KeyNotConfigured,
}

/// 对话镜像中的一条消息。seq 在一次镜像绑定内从 0 连续递增,分页取数以此为锚。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorMessage {
    pub seq: u64,
    /// 来源:"desktop"(桌面输入)| "assistant"(AI 回复)| "mobile"(移动端指令)
    pub source: String,
    pub content: String,
    /// 会话记录中的 ISO 8601 时间戳,缺失时为空串
    pub timestamp: String,
}

/// 桌面端 → 中转
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum DesktopToRelay {
    /// 握手:桌面端连上 WebSocket 后必须发送的第一条消息。
    /// `desktop_key` 是部署方配置在中转上的共享密钥(v2 起必填,不匹配即拒)。
    Hello {
        protocol_version: u32,
        desktop_key: String,
    },
    /// 请求签发一次性配对码(用于桌面端展示二维码)。旧配对码立即作废。
    RequestPairingCode,
    /// 重置配对:吊销移动端长期凭证与未用配对码,踢掉在线移动端。
    ResetPairing,
    /// 结构全量快照(连上中转后/收到快照请求时/启动器配置变化时发送)。
    /// 含全部项目(无活跃 pane 的 `panes` 为空)与可用 AI 启动器名单。
    SessionsSnapshot {
        projects: Vec<MobileProject>,
        #[serde(default)]
        launchers: Vec<MobileLauncher>,
    },
    /// 活跃 AI 会话结构增量:整项目 upsert + 项目移除。
    SessionsDelta {
        upserts: Vec<MobileProject>,
        removed_project_ids: Vec<String>,
    },
    /// 对话镜像初始快照(订阅成功/镜像绑定切换时发送,最近若干条)。
    MirrorSnapshot {
        pane_id: String,
        messages: Vec<MirrorMessage>,
        /// 是否还有更早的历史可分页加载
        has_more: bool,
    },
    /// 对话镜像增量:新出现的消息(桌面输入/AI 回复实时回流)。
    MirrorAppend {
        pane_id: String,
        messages: Vec<MirrorMessage>,
    },
    /// 分页历史响应:seq 早于请求锚点的一段消息。
    MirrorHistory {
        pane_id: String,
        messages: Vec<MirrorMessage>,
        has_more: bool,
    },
    /// 被订阅的 pane 已关闭/AI 会话已结束:移动端应提示并返回列表。
    PaneClosed { pane_id: String },
    /// 移动端指令回执:ok = 已写入 PTY(不承诺 AI 已接收,以镜像回流为准)。
    CommandReceipt {
        pane_id: String,
        command_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<CommandFailReason>,
    },
    /// 发起会话回执:ok = pane 已创建且启动命令已写入 PTY,**不**承诺 AI 已起来。
    /// 真正的成功信号是该 pane 出现在活跃会话快照里。
    StartSessionReceipt {
        request_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<StartSessionFailReason>,
    },
}

/// 中转 → 桌面端
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum RelayToDesktop {
    /// 握手成功。
    HelloAck { protocol_version: u32 },
    /// 握手拒绝(版本不匹配 / 密钥不对 / 中转未配置密钥);发送后中转立即关闭连接。
    /// 桌面端据 reason 分别给出"升级"与"配置密钥"的提示,两种都不再自动重连。
    /// 版本字段仅 `VersionMismatch` 时携带。
    HelloReject {
        reason: DesktopRejectReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_version: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actual_version: Option<u32>,
    },
    /// 响应 RequestPairingCode:新签发的一次性配对码。
    PairingCode { code: String },
    /// 配对状态变化:移动端兑换凭证成功(true)/凭证被吊销或重置(false)。
    /// 桌面端握手成功后也会立即收到一次当前状态。
    PairingUpdate { paired: bool },
    /// 移动端上线,请桌面端回发一份最新的 SessionsSnapshot(中转不缓存结构数据)。
    SessionsSnapshotRequest,
    /// 移动端订阅某 pane 的对话镜像(转发自移动端)。
    SubscribePane { pane_id: String },
    /// 移动端退订(显式退出镜像页/移动端断线时由中转代发)。
    UnsubscribePane { pane_id: String },
    /// 移动端请求更早的镜像历史(转发自移动端)。
    RequestMirrorHistory { pane_id: String, before_seq: u64 },
    /// 移动端指令(转发自移动端):到达即写入目标 pane 的 PTY,不排队。
    MobileCommand {
        pane_id: String,
        command_id: String,
        text: String,
    },
    /// 移动端重命名会话(转发自移动端):改的是目标 pane 的自定义标题。
    RenamePane { pane_id: String, title: String },
    /// 移动端发起新 AI 会话(原样转发自移动端):按 `launcher_id` 引用桌面端配置的
    /// 具名启动器,命令文本从不经过移动端或中转。
    StartAiSession {
        request_id: String,
        project_id: String,
        launcher_id: String,
    },
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
    /// 订阅某 pane 的对话镜像(进入镜像页)。
    SubscribePane { pane_id: String },
    /// 退订(返回列表)。
    UnsubscribePane { pane_id: String },
    /// 上拉加载更早的镜像历史。
    RequestMirrorHistory { pane_id: String, before_seq: u64 },
    /// 移动端指令:写穿目标 pane 的 PTY(等价桌面敲入同样内容并回车)。
    /// command_id 由移动端生成,用于回执关联。
    MobileCommand {
        pane_id: String,
        command_id: String,
        text: String,
    },
    /// 重命名会话:改目标 pane 的自定义标题(桌面端 tab 栏同步显示)。
    ///
    /// 无回执:改名成功与否由结构增量把新 title 推回来体现——那既是反馈也是真相,
    /// 再加一条回执只是把同一件事说两遍。空 title = 清除自定义名(回落 shell 名),
    /// 与桌面端右键重命名留空同义。
    RenamePane { pane_id: String, title: String },
    /// 发起新 AI 会话:在 `project_id` 项目里按 `launcher_id` 启动器新开一个 tab。
    /// request_id 由移动端生成,用于回执关联。
    StartAiSession {
        request_id: String,
        project_id: String,
        launcher_id: String,
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
    /// 桌面端在线状态(握手成功后立即推一次,此后变化时推送)。
    Presence { desktop_online: bool },
    /// 结构全量快照(转发自桌面端):全部项目 + 可用 AI 启动器名单。
    SessionsSnapshot {
        projects: Vec<MobileProject>,
        #[serde(default)]
        launchers: Vec<MobileLauncher>,
    },
    /// 活跃 AI 会话结构增量(转发自桌面端)。
    SessionsDelta {
        upserts: Vec<MobileProject>,
        removed_project_ids: Vec<String>,
    },
    /// 对话镜像初始快照(转发自桌面端;仅已订阅 pane)。
    MirrorSnapshot {
        pane_id: String,
        messages: Vec<MirrorMessage>,
        has_more: bool,
    },
    /// 对话镜像增量(转发自桌面端;仅已订阅 pane)。
    MirrorAppend {
        pane_id: String,
        messages: Vec<MirrorMessage>,
    },
    /// 分页历史响应(转发自桌面端)。
    MirrorHistory {
        pane_id: String,
        messages: Vec<MirrorMessage>,
        has_more: bool,
    },
    /// 被订阅的 pane 已关闭/AI 会话结束(转发自桌面端)。
    PaneClosed { pane_id: String },
    /// 移动端指令回执:桌面端写入结果,或中转在桌面离线时的路由层拒绝。
    CommandReceipt {
        pane_id: String,
        command_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<CommandFailReason>,
    },
    /// 发起会话回执:桌面端的创建结果,或中转在桌面离线时的路由层拒绝。
    StartSessionReceipt {
        request_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<StartSessionFailReason>,
    },
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
    fn protocol_version_is_v2() {
        // 两端严格相等校验:版本号是唯一的兼容性闸门,改动必须是有意的
        assert_eq!(PROTOCOL_VERSION, 2);
    }

    #[test]
    fn desktop_hello_camel_case_round_trip() {
        let msg = DesktopToRelay::Hello {
            protocol_version: PROTOCOL_VERSION,
            desktop_key: "s3cret".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            json.contains(r#""type":"hello""#)
                && json.contains(r#""protocolVersion":2"#)
                && json.contains(r#""desktopKey":"s3cret""#),
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
            reason: DesktopRejectReason::VersionMismatch,
            expected_version: Some(2),
            actual_version: Some(99),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            json.contains(r#""reason":"versionMismatch""#)
                && json.contains(r#""expectedVersion":2"#)
                && json.contains(r#""actualVersion":99"#),
            "{json}"
        );
        let parsed: RelayToDesktop = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn hello_reject_key_reasons_omit_versions() {
        for reason in [
            DesktopRejectReason::InvalidKey,
            DesktopRejectReason::KeyNotConfigured,
        ] {
            let msg = RelayToDesktop::HelloReject {
                reason,
                expected_version: None,
                actual_version: None,
            };
            let json = serde_json::to_string(&msg).unwrap();
            assert!(
                !json.contains("Version"),
                "密钥类拒绝不应携带版本字段: {json}"
            );
            assert_eq!(serde_json::from_str::<RelayToDesktop>(&json).unwrap(), msg);
        }
        // camelCase 枚举值口径(前端手写镜像按此匹配)
        let json = serde_json::to_string(&DesktopRejectReason::KeyNotConfigured).unwrap();
        assert_eq!(json, r#""keyNotConfigured""#);
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

    fn sample_project() -> MobileProject {
        MobileProject {
            project_id: "p1".into(),
            name: "demo".into(),
            panes: vec![MobilePane {
                pane_id: "pane-1".into(),
                title: "claude".into(),
                status: "ai-working".into(),
            }],
            can_start_session: true,
            group_path: vec!["工作".into(), "后端".into()],
        }
    }

    #[test]
    fn sessions_snapshot_camel_case_round_trip() {
        let msg = DesktopToRelay::SessionsSnapshot {
            projects: vec![sample_project()],
            launchers: vec![MobileLauncher {
                id: "l1".into(),
                name: "Claude".into(),
            }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            json.contains(r#""projectId":"p1""#)
                && json.contains(r#""paneId":"pane-1""#)
                && json.contains(r#""status":"ai-working""#)
                && json.contains(r#""canStartSession":true"#)
                && json.contains(r#""groupPath":["工作","后端"]"#)
                && json.contains(r#""launchers":[{"id":"l1","name":"Claude"}]"#),
            "serde camelCase 对齐被破坏: {json}"
        );
        // 启动器只下发 id 与展示名:命令/shell 绝不出现在 wire 上
        assert!(!json.contains("command") && !json.contains("shell"), "{json}");
        assert_eq!(serde_json::from_str::<DesktopToRelay>(&json).unwrap(), msg);
    }

    #[test]
    fn snapshot_carries_project_without_panes() {
        // v2:没有活跃 AI 会话的项目也进快照(panes 空数组),供发起弹层选目标
        let empty = MobileProject {
            project_id: "p2".into(),
            name: "idle-proj".into(),
            panes: vec![],
            can_start_session: false,
            group_path: vec![],
        };
        let msg = RelayToMobile::SessionsSnapshot {
            projects: vec![empty.clone()],
            launchers: vec![],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""panes":[]"#) && json.contains(r#""canStartSession":false"#), "{json}");
        // 顶层项目不占 wire:空 groupPath 整个字段省略
        assert!(!json.contains("groupPath"), "{json}");
        assert_eq!(serde_json::from_str::<RelayToMobile>(&json).unwrap(), msg);
    }

    #[test]
    fn project_without_group_path_field_parses_as_flat() {
        // 旧桌面端 / 旧中转发来的载荷不含 groupPath:必须解析成空链(平铺),不是报错
        let json = r#"{"projectId":"p3","name":"legacy","panes":[],"canStartSession":true}"#;
        let parsed: MobileProject = serde_json::from_str(json).unwrap();
        assert!(parsed.group_path.is_empty());
    }

    #[test]
    fn start_ai_session_and_receipt_round_trip() {
        let req = MobileToRelay::StartAiSession {
            request_id: "req-1".into(),
            project_id: "p1".into(),
            launcher_id: "l1".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            json.contains(r#""type":"startAiSession""#)
                && json.contains(r#""requestId":"req-1""#)
                && json.contains(r#""launcherId":"l1""#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<MobileToRelay>(&json).unwrap(), req);

        let fwd = RelayToDesktop::StartAiSession {
            request_id: "req-1".into(),
            project_id: "p1".into(),
            launcher_id: "l1".into(),
        };
        let json = serde_json::to_string(&fwd).unwrap();
        assert_eq!(serde_json::from_str::<RelayToDesktop>(&json).unwrap(), fwd);

        // 成功回执携带 paneId、不携带 reason
        let ok = DesktopToRelay::StartSessionReceipt {
            request_id: "req-1".into(),
            ok: true,
            pane_id: Some("pane-9".into()),
            reason: None,
        };
        let json = serde_json::to_string(&ok).unwrap();
        assert!(json.contains(r#""paneId":"pane-9""#) && !json.contains("reason"), "{json}");
        assert_eq!(serde_json::from_str::<DesktopToRelay>(&json).unwrap(), ok);

        // 失败回执携带 reason、不携带 paneId
        let fail = RelayToMobile::StartSessionReceipt {
            request_id: "req-2".into(),
            ok: false,
            pane_id: None,
            reason: Some(StartSessionFailReason::LauncherNotFound),
        };
        let json = serde_json::to_string(&fail).unwrap();
        assert!(
            json.contains(r#""reason":"launcherNotFound""#) && !json.contains("paneId"),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<RelayToMobile>(&json).unwrap(), fail);
    }

    #[test]
    fn start_session_fail_reasons_serialize_camel_case() {
        let cases = [
            (StartSessionFailReason::DesktopOffline, r#""desktopOffline""#),
            (StartSessionFailReason::ProjectNotFound, r#""projectNotFound""#),
            (StartSessionFailReason::LauncherNotFound, r#""launcherNotFound""#),
            (StartSessionFailReason::NotSupported, r#""notSupported""#),
            (StartSessionFailReason::SpawnFailed, r#""spawnFailed""#),
        ];
        for (reason, expected) in cases {
            assert_eq!(serde_json::to_string(&reason).unwrap(), expected);
        }
    }

    #[test]
    fn sessions_delta_and_presence_round_trip() {
        // 增量不带 launchers:启动器变化走重发全量快照(不为它单开增量消息)
        let delta = RelayToMobile::SessionsDelta {
            upserts: vec![sample_project()],
            removed_project_ids: vec!["p9".into()],
        };
        let json = serde_json::to_string(&delta).unwrap();
        assert!(json.contains(r#""removedProjectIds":["p9"]"#), "{json}");
        assert_eq!(serde_json::from_str::<RelayToMobile>(&json).unwrap(), delta);

        let presence = RelayToMobile::Presence {
            desktop_online: true,
        };
        let json = serde_json::to_string(&presence).unwrap();
        assert!(json.contains(r#""desktopOnline":true"#), "{json}");
        assert_eq!(
            serde_json::from_str::<RelayToMobile>(&json).unwrap(),
            presence
        );

        let req = RelayToDesktop::SessionsSnapshotRequest;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"type":"sessionsSnapshotRequest"}"#);
    }

    #[test]
    fn mirror_messages_camel_case_round_trip() {
        let snapshot = DesktopToRelay::MirrorSnapshot {
            pane_id: "pane-1".into(),
            messages: vec![MirrorMessage {
                seq: 42,
                source: "desktop".into(),
                content: "hello".into(),
                timestamp: "2026-07-24T12:00:00Z".into(),
            }],
            has_more: true,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(
            json.contains(r#""paneId":"pane-1""#)
                && json.contains(r#""hasMore":true"#)
                && json.contains(r#""seq":42"#),
            "serde camelCase 对齐被破坏: {json}"
        );
        assert_eq!(
            serde_json::from_str::<DesktopToRelay>(&json).unwrap(),
            snapshot
        );

        let sub = MobileToRelay::SubscribePane {
            pane_id: "pane-1".into(),
        };
        let json = serde_json::to_string(&sub).unwrap();
        assert_eq!(json, r#"{"type":"subscribePane","paneId":"pane-1"}"#);

        let req = MobileToRelay::RequestMirrorHistory {
            pane_id: "pane-1".into(),
            before_seq: 70,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""beforeSeq":70"#), "{json}");
        assert_eq!(serde_json::from_str::<MobileToRelay>(&json).unwrap(), req);

        let closed = RelayToMobile::PaneClosed {
            pane_id: "pane-1".into(),
        };
        let json = serde_json::to_string(&closed).unwrap();
        assert_eq!(serde_json::from_str::<RelayToMobile>(&json).unwrap(), closed);
    }

    #[test]
    fn rename_pane_round_trip() {
        let req = MobileToRelay::RenamePane {
            pane_id: "pane-1".into(),
            title: "重构登录".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            json.contains(r#""type":"renamePane""#) && json.contains(r#""paneId":"pane-1""#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<MobileToRelay>(&json).unwrap(), req);

        // 转发到桌面端的那一跳字段同名;空 title 合法(= 清除自定义名)
        let fwd = RelayToDesktop::RenamePane {
            pane_id: "pane-1".into(),
            title: String::new(),
        };
        let json = serde_json::to_string(&fwd).unwrap();
        assert!(json.contains(r#""type":"renamePane""#), "{json}");
        assert_eq!(serde_json::from_str::<RelayToDesktop>(&json).unwrap(), fwd);
    }

    #[test]
    fn mobile_command_and_receipt_round_trip() {
        let cmd = MobileToRelay::MobileCommand {
            pane_id: "pane-1".into(),
            command_id: "cmd-1".into(),
            text: "继续".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(
            json.contains(r#""type":"mobileCommand""#) && json.contains(r#""commandId":"cmd-1""#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<MobileToRelay>(&json).unwrap(), cmd);

        let ok = RelayToMobile::CommandReceipt {
            pane_id: "pane-1".into(),
            command_id: "cmd-1".into(),
            ok: true,
            reason: None,
        };
        let json = serde_json::to_string(&ok).unwrap();
        assert!(!json.contains("reason"), "成功回执不应携带 reason: {json}");
        assert_eq!(serde_json::from_str::<RelayToMobile>(&json).unwrap(), ok);

        let fail = RelayToMobile::CommandReceipt {
            pane_id: "pane-1".into(),
            command_id: "cmd-2".into(),
            ok: false,
            reason: Some(CommandFailReason::DesktopOffline),
        };
        let json = serde_json::to_string(&fail).unwrap();
        assert!(json.contains(r#""reason":"desktopOffline""#), "{json}");
        assert_eq!(serde_json::from_str::<RelayToMobile>(&json).unwrap(), fail);
    }
}
