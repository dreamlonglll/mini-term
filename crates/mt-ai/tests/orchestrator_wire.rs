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
    ControlLauncher, ControlPlane, ControlProject, OrchestratorActions, OrchestratorHost,
    PaneLiveness, StartFailure, StartSessionSpec, StartedSession,
};
use mt_agent_control::{
    ControlRequest, Identity, parse_launchers, parse_panes, parse_projects, parse_started_pane,
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
        assert_eq!(spec.orchestrator_pane_id, 7);
        Ok(StartedSession { pane_id: 101 })
    }

    fn pane_liveness(&self, _pane_id: u32) -> PaneLiveness {
        PaneLiveness {
            alive: true,
            status: "ai-idle".into(),
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

    // 未知命令(CLI 比桌面端新时的样子:CLI 发了一条这边还不认识的命令)
    let (plane, _id, body) = granted();
    let outcome = plane.handle("send", &body);
    let err = parse_launchers(outcome.status, &outcome.body).unwrap_err();
    assert_eq!(err.code, "unknownCommand");
    assert!(!err.is_denied(), "命令不认识不是鉴权问题");
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
