//! 跨工作区对账:**桌面侧真 handler 的产出**能被 sidecar CLI 自己的解析器读懂。
//!
//! 接法照抄 `mt-config` 里的「投影能被_sidecar_的解析器读懂」:那条直接调
//! `mt_core` 那份 sidecar 在用的解析器,这条直接调 `mt_agent_control`
//! (`sidecars/agent-control`,`mt-agent-cli` 在用的那一份)。
//!
//! 两侧隔着**工作区**边界、各有一份类型定义,没有共享类型,只靠字段名对齐 ——
//! 漂移一次就是编排者拿到空名单还以为「没配启动器」。这条测试就是那道护栏:
//! 请求方向(CLI 构造 → handler 解析)与响应方向(handler 产出 → CLI 解析)
//! 各走一遍真代码,中间不手写任何 JSON 字面量。

use std::sync::Arc;

use mt_ai::control::{
    AiSessionState, ControlLauncher, ControlPlane, ControlProject, Delivered, OrchestratorActions,
    OrchestratorHost, PaneInput, PaneLiveness, PaneSession, ScreenFailure, SendFailure,
    StartFailure, StartSessionSpec, StartedSession, TranscriptSource,
};
use mt_agent_control::{
    ControlRequest, Identity, parse_launchers, parse_panes, parse_projects, parse_screen,
    parse_send_receipt, parse_started_pane, parse_transcript,
};

/// 桌面能力的假宿主(与 mt-app 那份真实现同形)。
struct Host;

impl OrchestratorHost for Host {
    fn launchers(&self) -> Vec<ControlLauncher> {
        vec![
            ControlLauncher {
                id: "claude".into(),
                name: "Claude".into(),
            },
            ControlLauncher {
                id: "codex".into(),
                name: "Codex".into(),
            },
        ]
    }

    fn projects(&self) -> Vec<ControlProject> {
        vec![
            ControlProject {
                id: "p-self".into(),
                name: "前端".into(),
                path: "D:\\repos\\web".into(),
                group_id: Some("g1".into()),
                ssh_connection_id: None,
            },
            ControlProject {
                id: "p-sibling".into(),
                name: "后端".into(),
                path: "D:\\repos\\api".into(),
                group_id: Some("g1".into()),
                ssh_connection_id: None,
            },
            ControlProject {
                id: "p-remote".into(),
                name: "远程".into(),
                path: "/srv/api".into(),
                group_id: Some("g1".into()),
                ssh_connection_id: Some("conn-1".into()),
            },
            ControlProject {
                id: "p-outsider".into(),
                name: "无关项目".into(),
                path: "D:\\repos\\other".into(),
                group_id: None,
                ssh_connection_id: None,
            },
        ]
    }

    /// 乐手 101 是个 hook 上报过身份的 Claude 会话;102 是只有输入检测认得出的
    /// opencode(没有会话记录那一档)。与 mt-app 那份真实现同形:照实报,不加工。
    fn pane_session(&self, pane_id: u32) -> Option<PaneSession> {
        match pane_id {
            101 => Some(PaneSession {
                session_id: Some("s-1".into()),
                agent: Some("claude-code".into()),
            }),
            102 => Some(PaneSession {
                session_id: None,
                agent: Some("opencode".into()),
            }),
            _ => None,
        }
    }
}

/// 假会话记录(**不碰用户真实的 `~/.claude`**)。
struct Transcripts;

impl TranscriptSource for Transcripts {
    fn read(
        &self,
        agent: &str,
        session_id: &str,
        _project_path: &str,
    ) -> Option<Vec<mt_ai::AiSessionMessage>> {
        if (agent, session_id) != ("claude", "s-1") {
            return None;
        }
        Some(
            [("user", "跑一下测试"), ("assistant", "跑完了")]
                .iter()
                .map(|(role, content)| mt_ai::AiSessionMessage {
                    role: (*role).into(),
                    content: (*content).into(),
                    timestamp: "2026-09-01T00:00:00Z".into(),
                })
                .collect(),
        )
    }
}

/// 桌面动作的假实现(与 mt-app 那份真实现同形:起会话回主线程、死活现查)。
struct Actions;

impl OrchestratorActions for Actions {
    fn start_session(&self, spec: StartSessionSpec) -> Result<StartedSession, StartFailure> {
        // pane 编号在真桌面上是 PTY 编号;这里给个稳定值好断言
        assert_eq!(spec.orchestrator_pane_id(), 7);
        // 记账先落地、再谈回执 —— `landed` 是造出 `StartedSession` 的唯一路径
        Ok(spec.landed(101, "大脑"))
    }

    /// 与 mt-app 那份真实现同形:按目标终端的真实模式挑一份,如实回报挑了哪份。
    /// 这里的乐手是个正常的 AI TUI(开着 bracketed paste)。
    fn send_input(&self, _pane_id: u32, input: PaneInput) -> Result<Delivered, SendFailure> {
        // 桌面侧唯一要做的判断就是这一次挑选
        assert!(input.bytes(true).starts_with("\u{1b}[200~"));
        Ok(Delivered {
            bracketed_paste: true,
        })
    }

    /// 与 mt-app 那份真实现同形:钳位在控制面做完了,这里照办即可。
    fn read_screen(&self, _pane_id: u32, lines: usize) -> Result<Vec<String>, ScreenFailure> {
        let rows = vec![
            "$ claude".to_string(),
            "> 跑一下测试".to_string(),
            "Allow Bash(cargo test)? (y/n)".to_string(),
        ];
        let skip = rows.len().saturating_sub(lines);
        Ok(rows.into_iter().skip(skip).collect())
    }

    fn pane_liveness(&self, _pane_id: u32) -> PaneLiveness {
        PaneLiveness {
            alive: true,
            status: "ai-idle".into(),
            ai_session: AiSessionState::Active,
        }
    }
}

/// 一套「编排者 pane」现场:授予令牌 + CLI 侧按同一身份构造请求体。
fn granted() -> (ControlPlane, Identity, String) {
    let plane = ControlPlane::new();
    plane.set_host(Arc::new(Host));
    plane.set_actions(Arc::new(Actions));
    // 会话记录顶成假的:默认实现会去翻用户真实的 `~/.claude` / `~/.codex`
    plane.set_transcripts(Arc::new(Transcripts));
    let token = plane.grant(7, "p-self");
    // 请求体由 **CLI 那一侧**构造(它是 sidecar 里的同一段代码),
    // 桌面 handler 必须原样认得 —— 请求方向的对账。
    let identity = Identity {
        token,
        pane_id: 7,
    };
    let body = serde_json::to_string(&ControlRequest::from(&identity)).unwrap();
    (plane, identity, body)
}

#[test]
fn 启动器名单能被_sidecar_解析器读懂() {
    let (plane, _id, body) = granted();
    let outcome = plane.handle("list-launchers", &body);
    assert_eq!(outcome.status, 200, "{}", outcome.body);

    let launchers = parse_launchers(outcome.status, &outcome.body)
        .expect("sidecar 的解析器必须读得懂桌面 handler 的产出");
    let names: Vec<&str> = launchers.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, vec!["Claude", "Codex"]);
    let ids: Vec<&str> = launchers.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(ids, vec!["claude", "codex"]);
}

#[test]
fn 可达项目能被_sidecar_解析器读懂() {
    let (plane, _id, body) = granted();
    let outcome = plane.handle("list-projects", &body);
    assert_eq!(outcome.status, 200, "{}", outcome.body);

    let projects = parse_projects(outcome.status, &outcome.body)
        .expect("sidecar 的解析器必须读得懂桌面 handler 的产出");
    let ids: Vec<&str> = projects.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["p-self", "p-sibling", "p-remote"],
        "范围裁决也一并对账"
    );
    // `current` 这个布尔真的传到了 CLI 侧(编排者靠它知道自己在哪)
    assert!(projects[0].current);
    assert!(!projects[1].current);
    assert_eq!(projects[0].path, "D:\\repos\\web");
    // 「这个项目能不能起乐手」也要透到 CLI 侧,省得编排者白试一次
    assert!(projects[0].can_start_sessions);
    assert!(!projects[2].can_start_sessions, "远程项目当不了乐手宿主");
}

/// `start-session` 的回执:请求方向(CLI 构造带 launcherId 的体)与响应方向
/// (handler 的 pane 视图)各走一遍真代码。
#[test]
fn 起会话回执能被_sidecar_解析器读懂() {
    let (plane, identity, _body) = granted();
    let body =
        serde_json::to_string(&ControlRequest::start_session(&identity, "codex", Some("p-sibling")))
            .unwrap();
    let outcome = plane.handle("start-session", &body);
    assert_eq!(outcome.status, 200, "{}", outcome.body);

    let pane = parse_started_pane(outcome.status, &outcome.body)
        .expect("sidecar 的解析器必须读得懂桌面 handler 的产出");
    assert_eq!(pane.pane_id, 101);
    assert_eq!(pane.project_id, "p-sibling");
    assert_eq!(pane.project_name, "后端");
    assert_eq!(pane.launcher_id, "codex");
    assert_eq!(pane.launcher_name, "Codex");
    assert_eq!(pane.status, "ai-idle");
    assert!(pane.alive);
    // ADR 0002 的边界:命令文本一个字都不出桌面端
    assert!(
        !outcome.body.contains("command"),
        "回执不许带命令: {}",
        outcome.body
    );
}

/// `list-panes`:起过之后列得出来,且形状与回执同款。
#[test]
fn 乐手名单能被_sidecar_解析器读懂() {
    let (plane, identity, body) = granted();

    // 还没起过:空名单是合法的成功响应,不是解析失败
    let outcome = plane.handle("list-panes", &body);
    let panes = parse_panes(outcome.status, &outcome.body).expect("空名单也要解析得动");
    assert!(panes.is_empty());

    let start =
        serde_json::to_string(&ControlRequest::start_session(&identity, "claude", None)).unwrap();
    assert_eq!(plane.handle("start-session", &start).status, 200);

    let outcome = plane.handle("list-panes", &body);
    let panes = parse_panes(outcome.status, &outcome.body)
        .expect("sidecar 的解析器必须读得懂桌面 handler 的产出");
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].pane_id, 101);
    assert_eq!(panes[0].project_id, "p-self", "不给 projectId 就落在本项目");
    assert_eq!(panes[0].launcher_name, "Claude");
}

/// `send` 的回执:请求方向(CLI 构造带 targetPaneId + text 的体)与响应方向
/// (handler 的写穿回执)各走一遍真代码。
#[test]
fn 写穿回执能被_sidecar_解析器读懂() {
    let (plane, identity, _body) = granted();
    // 先起一个乐手 —— 可见范围铁律要求目标必须是自己起的
    let start =
        serde_json::to_string(&ControlRequest::start_session(&identity, "claude", None)).unwrap();
    assert_eq!(plane.handle("start-session", &start).status, 200);

    // 多行正文(含代码块)走一遍真的线上形状:CLI 侧的请求体构造 → 桌面 handler
    let prompt = "修一下这个:\n```rust\nfn main() {}\n```";
    let body = serde_json::to_string(&ControlRequest::send(&identity, 101, prompt)).unwrap();
    let outcome = plane.handle("send", &body);
    assert_eq!(outcome.status, 200, "{}", outcome.body);

    let sent = parse_send_receipt(outcome.status, &outcome.body)
        .expect("sidecar 的解析器必须读得懂桌面 handler 的产出");
    assert_eq!(sent.pane_id, 101);
    assert!(sent.bracketed_paste, "整块粘贴这一位要透到 CLI 侧");
    // 正文一个字都不许回显(ADR 0002 的防线延伸到编排者写的 prompt)
    assert!(
        !outcome.body.contains("fn main"),
        "回执回显了正文: {}",
        outcome.body
    );
}

/// 工单 05 新增的两个错误码两侧一致。
#[test]
fn 写穿的错误码两侧一致() {
    let (plane, identity, _body) = granted();
    let start =
        serde_json::to_string(&ControlRequest::start_session(&identity, "claude", None)).unwrap();
    assert_eq!(plane.handle("start-session", &start).status, 200);

    // 空正文:裸回车就是替用户按确认(ADR 0003 的「不代答」)
    let body = serde_json::to_string(&ControlRequest::send(&identity, 101, "  \n ")).unwrap();
    let err = parse_send_receipt(200, &plane.handle("send", &body).body).unwrap_err();
    assert_eq!(err.code, "emptyInput");
    assert!(!err.is_denied(), "不是鉴权问题");
    assert!(!err.is_desktop_unavailable(), "也不是够不着");
    assert!(
        !err.message.contains("musician"),
        "用户可见文案一律用 orchestrated session(术语表)"
    );

    // 不是自己起的乐手 —— 统一的「不存在」语义
    let body = serde_json::to_string(&ControlRequest::send(&identity, 4242, "干活")).unwrap();
    let outcome = plane.handle("send", &body);
    let err = parse_send_receipt(outcome.status, &outcome.body).unwrap_err();
    assert_eq!(err.code, "paneNotFound");
    assert_eq!(err.status, 404);

    // 自指禁令
    let body = serde_json::to_string(&ControlRequest::send(&identity, 7, "干活")).unwrap();
    let outcome = plane.handle("send", &body);
    let err = parse_send_receipt(outcome.status, &outcome.body).unwrap_err();
    assert_eq!(err.code, "selfTarget");
}

/// 工单 03 新增的错误码两侧一致 —— CLI 按 code 分档退出码,漂移了就只剩
/// 「反正失败了」。
#[test]
fn 新增错误码两侧一致() {
    let (plane, identity, _body) = granted();
    let start = |launcher: &str, project: Option<&str>| {
        let body =
            serde_json::to_string(&ControlRequest::start_session(&identity, launcher, project))
                .unwrap();
        plane.handle("start-session", &body)
    };

    // 启动器不存在
    let outcome = start("grok", None);
    let err = parse_started_pane(outcome.status, &outcome.body).unwrap_err();
    assert_eq!(err.code, "launcherNotFound");
    assert!(!err.is_denied() && !err.is_desktop_unavailable());

    // 组外项目(与「不存在的项目」同码)
    let outcome = start("claude", Some("p-outsider"));
    let err = parse_started_pane(outcome.status, &outcome.body).unwrap_err();
    assert_eq!(err.code, "projectUnreachable");
    assert_eq!(err.status, 403);

    // SSH 远程项目
    let outcome = start("claude", Some("p-remote"));
    let err = parse_started_pane(outcome.status, &outcome.body).unwrap_err();
    assert_eq!(err.code, "remoteProjectUnsupported");

    // 名额满了(把上限拧到 0 就是「一个都不许起」)
    plane.set_session_cap(0);
    let outcome = start("claude", None);
    let err = parse_started_pane(outcome.status, &outcome.body).unwrap_err();
    assert_eq!(err.code, "sessionLimitReached");
    assert_eq!(err.status, 429);
    assert!(!err.is_denied(), "名额满了不是鉴权问题");
}

/// `read-transcript` 的增量回执两侧对得上:请求方向(CLI 构造带 `cursor` 的体)
/// 与响应方向(handler 的 transcript)各走一遍真代码。
#[test]
fn 会话记录增量能被_sidecar_解析器读懂() {
    let (plane, identity, _body) = granted();
    // 先起一个乐手 —— 可见范围铁律要求目标必须是自己起的(落在 101)
    let start =
        serde_json::to_string(&ControlRequest::start_session(&identity, "claude", None)).unwrap();
    assert_eq!(plane.handle("start-session", &start).status, 200);

    // 从头读
    let body = serde_json::to_string(&ControlRequest::read_transcript(&identity, 101, None)).unwrap();
    let outcome = plane.handle("read-transcript", &body);
    assert_eq!(outcome.status, 200, "{}", outcome.body);
    let t = parse_transcript(outcome.status, &outcome.body)
        .expect("sidecar 的解析器必须读得懂桌面 handler 的产出");
    assert_eq!(t.pane_id, 101);
    assert_eq!(t.agent, "claude", "hook 上报的 claude-code 要收敛成家族名");
    assert_eq!(t.session_id, "s-1");
    assert_eq!((t.cursor, t.next_cursor, t.total), (0, 2, 2));
    assert!(!t.has_more);
    assert_eq!(t.messages.len(), 2);
    assert_eq!(t.messages[0].seq, 0);
    assert_eq!(t.messages[0].role, "user");
    assert_eq!(t.messages[1].content, "跑完了");
    assert!(!t.messages[1].truncated);

    // 带游标读:**只给之后的**(这里是「之后什么都没有」)
    let body =
        serde_json::to_string(&ControlRequest::read_transcript(&identity, 101, Some(2))).unwrap();
    let outcome = plane.handle("read-transcript", &body);
    let t = parse_transcript(outcome.status, &outcome.body).unwrap();
    assert!(t.messages.is_empty());
    assert_eq!(t.cursor, t.next_cursor, "没有新内容时游标原地不动");
}

/// `read-screen` 的回执两侧对得上,且**行数参数真的透到桌面侧**。
#[test]
fn 终端画面能被_sidecar_解析器读懂() {
    let (plane, identity, _body) = granted();
    let start =
        serde_json::to_string(&ControlRequest::start_session(&identity, "claude", None)).unwrap();
    assert_eq!(plane.handle("start-session", &start).status, 200);

    let body = serde_json::to_string(&ControlRequest::read_screen(&identity, 101, None)).unwrap();
    let outcome = plane.handle("read-screen", &body);
    assert_eq!(outcome.status, 200, "{}", outcome.body);
    let s = parse_screen(outcome.status, &outcome.body)
        .expect("sidecar 的解析器必须读得懂桌面 handler 的产出");
    assert_eq!(s.pane_id, 101);
    assert_eq!(
        s.lines,
        vec!["$ claude", "> 跑一下测试", "Allow Bash(cargo test)? (y/n)"]
    );
    assert!(!s.truncated);

    // 指定行数:尾部那一行(审批提示原文 —— read-screen 的头号用途)
    let body =
        serde_json::to_string(&ControlRequest::read_screen(&identity, 101, Some(1))).unwrap();
    let outcome = plane.handle("read-screen", &body);
    let s = parse_screen(outcome.status, &outcome.body).unwrap();
    assert_eq!(s.lines, vec!["Allow Bash(cargo test)? (y/n)"]);
}

/// 工单 07 新增的两个错误码两侧一致,且**能力分层的下一步写在消息里**。
#[test]
fn 读命令的错误码两侧一致() {
    let (plane, identity, _body) = granted();
    let start =
        serde_json::to_string(&ControlRequest::start_session(&identity, "claude", None)).unwrap();
    assert_eq!(plane.handle("start-session", &start).status, 200);

    let read = |target: u32| {
        let body =
            serde_json::to_string(&ControlRequest::read_transcript(&identity, target, None))
                .unwrap();
        plane.handle("read-transcript", &body)
    };

    // 不是自己起的乐手 —— 统一的「不存在」语义(102 是 Host 认得的另一个 pane,
    // 但它不在这个编排者的记账里)
    let outcome = read(4242);
    let err = parse_transcript(outcome.status, &outcome.body).unwrap_err();
    assert_eq!(err.code, "paneNotFound");
    assert_eq!(err.status, 404);
    let outcome = read(102);
    let err = parse_transcript(outcome.status, &outcome.body).unwrap_err();
    assert_eq!(
        err.code, "paneNotFound",
        "别人的 pane 必须与「不存在」不可区分"
    );

    // 自指禁令
    let outcome = read(7);
    let err = parse_transcript(outcome.status, &outcome.body).unwrap_err();
    assert_eq!(err.code, "selfTarget");

    // 两个新码都落在「改你的请求」那一档
    for code in ["transcriptUnsupported", "sessionUnidentified"] {
        let f = mt_agent_control::ControlFailure {
            status: 409,
            code: code.into(),
            message: String::new(),
        };
        assert!(!f.is_denied(), "{code} 不是鉴权失败");
        assert!(!f.is_desktop_unavailable(), "{code} 不是够不着");
    }
}

/// **能力分层走到线上**:无会话记录的 agent 起的乐手,`read-transcript` 明确报错
/// (且消息里写着改用 `read-screen`),`read-screen` 照常可用。
///
/// 这里另起一个 plane,让 `Host::pane_session` 认得的那个 opencode pane(102)
/// 真的成为这个编排者名下的乐手。
#[test]
fn 无记录_agent_的能力分层走到线上() {
    /// 起出来的乐手就是 102 —— `Host::pane_session` 把它报成 opencode。
    struct OpencodeActions;

    impl OrchestratorActions for OpencodeActions {
        fn start_session(&self, spec: StartSessionSpec) -> Result<StartedSession, StartFailure> {
            Ok(spec.landed(102, "大脑"))
        }
        fn send_input(&self, _pane_id: u32, _input: PaneInput) -> Result<Delivered, SendFailure> {
            Ok(Delivered {
                bracketed_paste: false,
            })
        }
        fn read_screen(&self, _pane_id: u32, _lines: usize) -> Result<Vec<String>, ScreenFailure> {
            Ok(vec!["opencode > 在跑".to_string()])
        }
        fn pane_liveness(&self, _pane_id: u32) -> PaneLiveness {
            PaneLiveness {
                alive: true,
                status: "ai-idle".into(),
                ai_session: AiSessionState::Active,
            }
        }
    }

    let plane = ControlPlane::new();
    plane.set_host(Arc::new(Host));
    plane.set_actions(Arc::new(OpencodeActions));
    plane.set_transcripts(Arc::new(Transcripts));
    let identity = Identity {
        token: plane.grant(7, "p-self"),
        pane_id: 7,
    };
    let start =
        serde_json::to_string(&ControlRequest::start_session(&identity, "claude", None)).unwrap();
    assert_eq!(plane.handle("start-session", &start).status, 200);

    // transcript：明确报错，并告诉编排者改用哪条命令
    let body = serde_json::to_string(&ControlRequest::read_transcript(&identity, 102, None)).unwrap();
    let outcome = plane.handle("read-transcript", &body);
    let err = parse_transcript(outcome.status, &outcome.body).unwrap_err();
    assert_eq!(err.code, "transcriptUnsupported");
    assert_eq!(err.status, 409);
    assert!(
        err.message.contains("read-screen"),
        "得写清下一步: {}",
        err.message
    );
    assert!(
        !err.message.contains("musician"),
        "用户可见文案一律 orchestrated session(术语表)"
    );

    // read-screen：对它照常可用 —— 那就是它的兜底
    let body = serde_json::to_string(&ControlRequest::read_screen(&identity, 102, None)).unwrap();
    let outcome = plane.handle("read-screen", &body);
    assert_eq!(outcome.status, 200, "{}", outcome.body);
    let s = parse_screen(outcome.status, &outcome.body).unwrap();
    assert_eq!(s.lines, vec!["opencode > 在跑"]);
}

/// 被拒响应也要对得上:CLI 靠 `code` 分档退出码,漂移了就只剩「反正失败了」。
#[test]
fn 被拒响应的错误码两侧一致() {
    let plane = ControlPlane::new();
    plane.set_host(Arc::new(Host));

    // 无令牌(普通 pane 里跑 CLI)
    let anonymous = serde_json::to_string(&ControlRequest::from(&Identity {
        token: String::new(),
        pane_id: 7,
    }))
    .unwrap();
    let outcome = plane.handle("list-launchers", &anonymous);
    let err = parse_launchers(outcome.status, &outcome.body).unwrap_err();
    assert_eq!(err.code, "missingToken");
    assert!(err.is_denied(), "CLI 据此给「你不是编排者」那档退出码");

    // 伪造令牌
    let forged = serde_json::to_string(&ControlRequest::from(&Identity {
        token: "deadbeef".into(),
        pane_id: 7,
    }))
    .unwrap();
    let outcome = plane.handle("list-projects", &forged);
    let err = parse_projects(outcome.status, &outcome.body).unwrap_err();
    assert_eq!(err.code, "invalidToken");
    assert!(err.is_denied());

    // 未知命令(CLI 比桌面端新时的样子:CLI 发了一条这边还不认识的命令)。
    // ⚠️ 这里的命令名必须是**永远不会被实现**的那种:原来写的是 `send`,
    // 工单 05 把它做出来之后这条测试就成了「send 居然是未知命令」。
    let (plane, _id, body) = granted();
    let outcome = plane.handle("no-such-command", &body);
    let err = parse_launchers(outcome.status, &outcome.body).unwrap_err();
    assert_eq!(err.code, "unknownCommand");
    assert!(!err.is_denied(), "命令不认识不是鉴权问题");
}

/// 桌面侧等主线程的时限必须**短于** CLI 那侧的读超时,否则起会话稍慢一点就变成
/// CLI 先断线 —— 编排者拿到的会是「够不着」,而不是桌面端给的那个明确答复。
///
/// 这条不等式跨着工作区边界,两侧各有一份常量,**只有在这里能拿真值比一次**。
/// (它此前长在 `mt-app` 里,拿的是一个字面量 `Duration::from_secs(5)`——
/// 改了 CLI 那侧的真常量它不会红,是假保险。)
#[test]
fn 桌面动作超时短于_cli_读超时() {
    assert!(
        mt_ai::control::ACTION_TIMEOUT < mt_agent_control::READ_TIMEOUT,
        "留给 HTTP 往返的富余没了: 桌面 {:?} vs CLI {:?}",
        mt_ai::control::ACTION_TIMEOUT,
        mt_agent_control::READ_TIMEOUT
    );
    assert!(
        mt_ai::control::ACTION_TIMEOUT >= std::time::Duration::from_secs(1),
        "太短会把「主线程正忙」误判成「卡死」"
    );
}

/// `desktopBusy` 的**新语义**:它不是「没起成」,而是「没答上来,那个会话可能
/// 已经起来了」。CLI 侧据此把它归进「改请求也没用」那一档退出码,而消息本身
/// 要指向 `list-panes` —— 无脑重试正是这条错误码最容易诱发的错误动作。
#[test]
fn desktop_busy_的语义两侧一致() {
    let plane = ControlPlane::new();
    // 不注入动作实现 = 泵没接线,起会话恒答 DesktopBusy(fail-closed)
    plane.set_host(Arc::new(Host));
    let token = plane.grant(7, "p-self");
    let identity = Identity { token, pane_id: 7 };
    let body =
        serde_json::to_string(&ControlRequest::start_session(&identity, "claude", None)).unwrap();

    let outcome = plane.handle("start-session", &body);
    let err = parse_started_pane(outcome.status, &outcome.body).unwrap_err();
    assert_eq!(err.code, "desktopBusy");
    assert_eq!(err.status, 503);
    assert!(err.is_desktop_unavailable(), "CLI 归进「够不着」那一档退出码");
    assert!(
        err.message.contains("list-panes"),
        "得告诉编排者先查一眼再决定,而不是重试: {}",
        err.message
    );
    assert!(
        !err.message.contains("musician"),
        "用户可见文案一律用 orchestrated session(术语表)"
    );
}

/// 环境变量名两侧必须一字不差 —— 主程序按这个名字注入,CLI 按这个名字读。
#[test]
fn 环境变量名与路由前缀两侧一致() {
    assert_eq!(mt_ai::control::TOKEN_ENV, mt_agent_control::TOKEN_ENV);
    assert_eq!(mt_ai::control::PANE_ENV, mt_agent_control::PANE_ENV);
    assert_eq!(
        mt_ai::control::CONTROL_PREFIX,
        mt_agent_control::CONTROL_PREFIX
    );
    // 保留前缀:用户/项目级环境变量覆盖不掉内部协议变量,靠的就是这个前缀
    assert!(mt_ai::control::TOKEN_ENV.starts_with("MINITERM_"));
    assert!(mt_ai::control::PANE_ENV.starts_with("MINITERM_"));
}
