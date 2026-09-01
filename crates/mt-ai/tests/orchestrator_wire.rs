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
    OrchestratorHost, PaneInput, PaneLiveness, SendFailure, StartFailure, StartSessionSpec,
    StartedSession,
};
use mt_agent_control::{
    ControlRequest, Identity, parse_launchers, parse_panes, parse_projects, parse_send_receipt,
    parse_started_pane,
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
