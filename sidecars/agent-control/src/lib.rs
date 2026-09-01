//! 编排控制面的**线上形状 + 响应解析器**。
//!
//! 这一份被两边用：
//!
//! - `mt-agent-cli`（同工作区的 sidecar 二进制）：发请求、解析响应、出 JSON；
//! - 主仓 `mt-ai` 的对账测试（跨工作区 path 依赖，dev-dependency）：拿桌面侧
//!   **真 handler 的产出**喂进这里的解析器，证明两边对得上。
//!
//! 与 `mt_core::config_reader::ConfigSshView` 是同一种对账关系：桌面侧
//! （`mt_ai::control`）与 sidecar 侧隔着 crate 边界、各有一份类型，靠字段名对齐，
//! 由一条测试兜住。区别只在这一份是**给测试直接调**的，不必等真跑起来才发现漂移。
//!
//! 依赖表只有 serde 两件套 —— 主仓把它引进测试图，不该顺带背一棵大依赖树。

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 连接超时。控制端点是本机进程，慢到这个份上一定是出事了。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// 读响应的超时。
///
/// ⚠️ 必须**大于**桌面侧等主线程的那个时限（`mt_ai::control::ACTION_TIMEOUT`
/// 3 秒），否则起会话稍慢一点就变成 CLI 先断线 —— 编排者拿到的会是「够不着」，
/// 而不是桌面端给的那个明确答复。
///
/// 住在这个 crate 而不是 `mt-agent-cli` 的 bin 里，是因为那条不等式跨着**工作区
/// 边界**：主仓 `mt-ai` 的对账测试（`tests/orchestrator_wire.rs`）要拿两侧的真
/// 常量比一次，而它够不到 bin 里的私有常量。放在这里，那条断言才不是假保险。
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// 编排令牌的环境变量名（与 `mt_ai::control::TOKEN_ENV` 必须一致）。
pub const TOKEN_ENV: &str = "MINITERM_ORCHESTRATOR_TOKEN";
/// 编排者自身 pane 身份的环境变量名（与 `mt_ai::control::PANE_ENV` 必须一致）。
pub const PANE_ENV: &str = "MINITERM_ORCHESTRATOR_PANE";
/// 控制端点的路由前缀（与 `mt_ai::control::CONTROL_PREFIX` 必须一致）。
pub const CONTROL_PREFIX: &str = "/control/";

// ─── 自身身份（fail-closed 的第一道闸）─────────────────────────

/// 编排者的身份：一枚令牌 + 自己是哪个 pane。两者都由主程序在 spawn 时注入。
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub token: String,
    pub pane_id: u32,
}

/// 拿不到身份的三种情形。**都不是「猜一个默认值继续」的理由** —— 这个二进制
/// 在没有编排能力的 pane 里被跑到是常态（用户手敲、乐手 pane 里的 agent 乱试），
/// 明确拒绝就是正确行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityError {
    /// 没有令牌 = 这个 pane 没被授予编排能力。
    NotAnOrchestrator,
    /// 有令牌却没有 pane 身份 / 身份不是数字：注入链路坏了，不许放行。
    BrokenIdentity,
}

impl IdentityError {
    pub fn message(self) -> &'static str {
        match self {
            Self::NotAnOrchestrator => {
                "this pane has no orchestrator capability (no MINITERM_ORCHESTRATOR_TOKEN); \
                 enable 「允许编排」 on the AI launcher and start the session from it"
            }
            Self::BrokenIdentity => {
                "orchestrator token present but MINITERM_ORCHESTRATOR_PANE is missing or not a number"
            }
        }
    }
}

/// 从环境里取身份。取值函数注入，便于测试（`identity_from_env` 是它的实参版）。
pub fn identity_from(get: impl Fn(&str) -> Option<String>) -> Result<Identity, IdentityError> {
    let token = get(TOKEN_ENV)
        .filter(|t| !t.trim().is_empty())
        .ok_or(IdentityError::NotAnOrchestrator)?;
    let pane_id = get(PANE_ENV)
        .and_then(|v| v.trim().parse::<u32>().ok())
        .ok_or(IdentityError::BrokenIdentity)?;
    Ok(Identity { token, pane_id })
}

/// 进程环境版。
pub fn identity_from_env() -> Result<Identity, IdentityError> {
    identity_from(|k| std::env::var(k).ok())
}

/// 控制请求的 body（与桌面侧 `ControlRequest` 对齐）。
///
/// 命令各取所需，用不上的字段**整个不出线**（`skip_serializing_if`）——
/// `list-*` 那两条命令的请求体因此与工单 02 时一字不差。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlRequest {
    pub token: String,
    pub pane_id: u32,
    /// `start-session`：用哪个具名启动器（**只有 id** —— 命令文本从不经过这里）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launcher_id: Option<String>,
    /// `start-session`：落在哪个项目；不给就是编排者自己那个。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// 以某个乐手为目标的命令（`send`，工单 06~07 的 wait / read 同款）用它。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_pane_id: Option<u32>,
    /// `send`：要写穿进去的正文（编排者写的 prompt）。
    ///
    /// **只在这条命令的请求体里出现一次，别的地方一个字都不留** —— 它是用户
    /// 项目里的内容，与启动器的命令文本同一档待遇：不进日志、不进错误消息、
    /// 不进回执（回执只回 pane 编号与「有没有当成一块粘贴」）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl From<&Identity> for ControlRequest {
    fn from(id: &Identity) -> Self {
        Self {
            token: id.token.clone(),
            pane_id: id.pane_id,
            launcher_id: None,
            project_id: None,
            target_pane_id: None,
            text: None,
        }
    }
}

impl ControlRequest {
    /// `start-session` 的请求体。
    pub fn start_session(id: &Identity, launcher_id: &str, project_id: Option<&str>) -> Self {
        Self {
            launcher_id: Some(launcher_id.to_string()),
            project_id: project_id.map(str::to_string),
            ..Self::from(id)
        }
    }

    /// `send` 的请求体：往哪个乐手写、写什么。
    pub fn send(id: &Identity, target_pane_id: u32, text: &str) -> Self {
        Self {
            target_pane_id: Some(target_pane_id),
            text: Some(text.to_string()),
            ..Self::from(id)
        }
    }
}

// ─── 响应 ─────────────────────────────────────────────────────

/// 一条启动器。**只有 id 与展示名** —— 命令文本从不下发（ADR 0002/0003）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Launcher {
    pub id: String,
    pub name: String,
}

/// 一条可达项目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    /// 编排者自己所在的那条。
    #[serde(default)]
    pub current: bool,
    /// 能不能在这里起乐手（SSH 远程项目起不了）。
    ///
    /// **缺省 `false`**：认不出这个字段的旧桌面端在场时，宁可让编排者以为
    /// 都起不了、去试一次吃个明确错误，也别反过来（fail-closed 的取向）。
    #[serde(default)]
    pub can_start_sessions: bool,
}

/// 一个受编排会话（乐手）在编排者眼里的样子。
///
/// `start-session` 的回执与 `list-panes` 的每一条都是它 —— 两条命令共用一个类型，
/// 编排者只需要认识一种 pane 视图。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratedPane {
    /// 乐手的 pane 身份（= PTY 编号）。后续命令按它点名。
    pub pane_id: u32,
    pub project_id: String,
    pub project_name: String,
    pub launcher_id: String,
    /// 启动器展示名。**不是命令文本** —— 那东西一个字都不出桌面端。
    pub launcher_name: String,
    /// AI 状态：`idle` / `ai-idle` / `ai-working`。
    pub status: String,
    /// pane 还在桌面上吗。编排者退场不杀乐手；反过来乐手被用户关掉时这里是
    /// `false`，记账仍在（好让编排者看得见「我起的那个已经没了」）。
    #[serde(default)]
    pub alive: bool,
}

/// `send` 的回执。
///
/// **不带任何正文回显**，也不带状态列：写穿之后那一瞬的状态一定还是写之前的
/// 样子（agent 还没来得及反应），摆出来只会诱导编排者把它读成「干完了」。
/// 要看状态走 `wait`（工单 06）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendReceipt {
    /// 写进了哪个受编排会话。
    pub pane_id: u32,
    /// 是不是当成一整块粘贴送进去的（bracketed paste）。
    ///
    /// **缺省 `false`**：认不出这个字段的旧桌面端在场时，宁可让编排者以为
    /// 多行没走成整块、去核对一眼，也别反过来（与 `Project::can_start_sessions`
    /// 同一个 fail-closed 取向）。
    #[serde(default)]
    pub bracketed_paste: bool,
}

/// 被拒 / 出错的响应。`code` 是闭集（桌面侧 `ControlError::code`），
/// CLI 按它决定退出码，`message` 只给人看。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlFailure {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl ControlFailure {
    /// 「这个 pane 没有编排能力」这一类（鉴权失败），CLI 据此给专门的退出码。
    pub fn is_denied(&self) -> bool {
        matches!(self.code.as_str(), "missingToken" | "invalidToken")
    }

    /// 桌面端在，但没答上来（主线程忙死 / 动作泵没接线）。
    ///
    /// 与「连都连不上」归进同一档退出码：两种处境下编排者都**改自己的请求也没用**。
    ///
    /// ⚠️ 但 `desktopBusy` **不等于「没起成」**：桌面端一旦把乐手起出来就先落记账、
    /// 再谈回执（`mt_ai::control` 的记账契约），所以没答上来的只是这一趟往返。
    /// 正确的下一步是先 `list-panes` 看一眼那个会话在不在，**别无脑重试** ——
    /// 重试很可能是在起第二个。
    pub fn is_desktop_unavailable(&self) -> bool {
        self.code == "desktopBusy"
    }

    fn malformed(status: u16, why: &str) -> Self {
        Self {
            status,
            code: "malformedResponse".to_string(),
            message: format!("cannot parse control response: {why}"),
        }
    }
}

/// 拆信封：`{"ok":true,"data":{…}}` → data；`{"ok":false,"error":{…}}` → 失败。
///
/// 状态码只作为兜底信号：**以 body 为准**，body 认不出来才拿状态码报错 ——
/// 中间挡了个代理之类的意外情形下，错误至少还能被读懂。
fn envelope(status: u16, body: &str) -> Result<serde_json::Value, ControlFailure> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Err(ControlFailure::malformed(status, "not JSON"));
    };
    if value.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        return value
            .get("data")
            .cloned()
            .ok_or_else(|| ControlFailure::malformed(status, "ok response without data"));
    }
    let error = value.get("error");
    let code = error
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("unknownError");
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("control request failed");
    Err(ControlFailure {
        status,
        code: code.to_string(),
        message: message.to_string(),
    })
}

/// 解析 `list-launchers` 的响应。
pub fn parse_launchers(status: u16, body: &str) -> Result<Vec<Launcher>, ControlFailure> {
    let data = envelope(status, body)?;
    let list = data
        .get("launchers")
        .cloned()
        .ok_or_else(|| ControlFailure::malformed(status, "missing `launchers`"))?;
    serde_json::from_value(list)
        .map_err(|e| ControlFailure::malformed(status, &format!("bad `launchers`: {e}")))
}

/// 解析 `list-projects` 的响应。
pub fn parse_projects(status: u16, body: &str) -> Result<Vec<Project>, ControlFailure> {
    let data = envelope(status, body)?;
    let list = data
        .get("projects")
        .cloned()
        .ok_or_else(|| ControlFailure::malformed(status, "missing `projects`"))?;
    serde_json::from_value(list)
        .map_err(|e| ControlFailure::malformed(status, &format!("bad `projects`: {e}")))
}

/// 解析 `start-session` 的回执（单个乐手）。
pub fn parse_started_pane(status: u16, body: &str) -> Result<OrchestratedPane, ControlFailure> {
    let data = envelope(status, body)?;
    let pane = data
        .get("pane")
        .cloned()
        .ok_or_else(|| ControlFailure::malformed(status, "missing `pane`"))?;
    serde_json::from_value(pane)
        .map_err(|e| ControlFailure::malformed(status, &format!("bad `pane`: {e}")))
}

/// 解析 `list-panes` 的响应（自己名下的全部乐手）。
pub fn parse_panes(status: u16, body: &str) -> Result<Vec<OrchestratedPane>, ControlFailure> {
    let data = envelope(status, body)?;
    let list = data
        .get("panes")
        .cloned()
        .ok_or_else(|| ControlFailure::malformed(status, "missing `panes`"))?;
    serde_json::from_value(list)
        .map_err(|e| ControlFailure::malformed(status, &format!("bad `panes`: {e}")))
}

/// 解析 `send` 的回执。
pub fn parse_send_receipt(status: u16, body: &str) -> Result<SendReceipt, ControlFailure> {
    let data = envelope(status, body)?;
    let sent = data
        .get("sent")
        .cloned()
        .ok_or_else(|| ControlFailure::malformed(status, "missing `sent`"))?;
    serde_json::from_value(sent)
        .map_err(|e| ControlFailure::malformed(status, &format!("bad `sent`: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── 身份 ─────────────────────────────────────────────────

    #[test]
    fn 令牌与身份都在时解析出编排者身份() {
        let id = identity_from(|k| match k {
            TOKEN_ENV => Some("tok".into()),
            PANE_ENV => Some("7".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(id.token, "tok");
        assert_eq!(id.pane_id, 7);
    }

    /// 普通 pane 里跑（没有注入）→ 明确拒绝，不去猜端口也不发请求。
    #[test]
    fn 没有令牌就是普通_pane() {
        assert_eq!(
            identity_from(|_| None).unwrap_err(),
            IdentityError::NotAnOrchestrator
        );
        // 空串与空白同样不算
        assert_eq!(
            identity_from(|k| (k == TOKEN_ENV).then(|| "   ".to_string())).unwrap_err(),
            IdentityError::NotAnOrchestrator
        );
    }

    /// 有令牌没身份 / 身份不是数字:注入链路坏了,一样拒绝(不许默认成 0)。
    #[test]
    fn 身份缺失或非法一律拒绝() {
        for pane in [None, Some("".to_string()), Some("abc".to_string()), Some("-1".to_string())] {
            let err = identity_from(|k| match k {
                TOKEN_ENV => Some("tok".into()),
                PANE_ENV => pane.clone(),
                _ => None,
            })
            .unwrap_err();
            assert_eq!(err, IdentityError::BrokenIdentity, "pane={pane:?}");
        }
    }

    #[test]
    fn 请求体按_camel_case_出线() {
        let id = Identity {
            token: "t".into(),
            pane_id: 3,
        };
        let json = serde_json::to_string(&ControlRequest::from(&id)).unwrap();
        assert_eq!(json, r#"{"token":"t","paneId":3}"#);
    }

    // ─── 响应 ─────────────────────────────────────────────────

    #[test]
    fn 解析启动器名单() {
        let body = r#"{"ok":true,"data":{"launchers":[{"id":"claude","name":"Claude"}]}}"#;
        let list = parse_launchers(200, body).unwrap();
        assert_eq!(
            list,
            vec![Launcher {
                id: "claude".into(),
                name: "Claude".into()
            }]
        );
    }

    #[test]
    fn 解析可达项目() {
        let body = r#"{"ok":true,"data":{"projects":[
            {"id":"p1","name":"前端","path":"D:\\a","current":true},
            {"id":"p2","name":"后端","path":"D:\\b","current":false}]}}"#;
        let list = parse_projects(200, body).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].current);
        assert_eq!(list[1].path, "D:\\b");
    }

    /// 被拒时错误码要原样透出来 —— CLI 靠它区分「不是编排者」与「请求本身不对」。
    #[test]
    fn 被拒响应解析成闭集错误码() {
        let body = r#"{"ok":false,"error":{"code":"invalidToken","message":"nope"}}"#;
        let err = parse_launchers(401, body).unwrap_err();
        assert_eq!(err.code, "invalidToken");
        assert_eq!(err.status, 401);
        assert!(err.is_denied());

        let body = r#"{"ok":false,"error":{"code":"projectUnavailable","message":"gone"}}"#;
        let err = parse_projects(409, body).unwrap_err();
        assert_eq!(err.code, "projectUnavailable");
        assert!(!err.is_denied(), "项目没了不是鉴权失败");
    }

    /// 认不出的响应不许当成空名单成功返回。
    #[test]
    fn 认不出的响应是失败而不是空名单() {
        for (status, body) in [
            (200, "not json"),
            (200, r#"{"ok":true}"#),
            (200, r#"{"ok":true,"data":{}}"#),
            (500, "<html>proxy error</html>"),
        ] {
            let err = parse_launchers(status, body).unwrap_err();
            assert_eq!(err.code, "malformedResponse", "body={body}");
        }
    }

    /// 服务端加了字段（工单 03 会往里塞东西）不许让解析器崩：未知字段忽略。
    #[test]
    fn 未知字段向前兼容() {
        let body = r#"{"ok":true,"data":{"projects":[
            {"id":"p1","name":"n","path":"p","current":true,"futureField":42}],"extra":1}}"#;
        assert_eq!(parse_projects(200, body).unwrap().len(), 1);
    }

    // ─── 工单 03 的两条命令 ───────────────────────────────────

    /// `start-session` 的请求体：**只带 id**，没给的字段整个不出线。
    #[test]
    fn 起会话请求体只带_id() {
        let id = Identity {
            token: "t".into(),
            pane_id: 3,
        };
        let json = serde_json::to_string(&ControlRequest::start_session(&id, "codex", None)).unwrap();
        assert_eq!(json, r#"{"token":"t","paneId":3,"launcherId":"codex"}"#);

        let json =
            serde_json::to_string(&ControlRequest::start_session(&id, "codex", Some("p-api")))
                .unwrap();
        assert_eq!(
            json,
            r#"{"token":"t","paneId":3,"launcherId":"codex","projectId":"p-api"}"#
        );
        // 命令文本没有出线的通道 —— 类型上就没有那个字段
        assert!(!json.contains("command"));
    }

    /// `list-*` 的请求体不受工单 03 的字段扩展影响（工单 02 那版一字不差）。
    #[test]
    fn 列表命令的请求体未被新字段污染() {
        let id = Identity {
            token: "t".into(),
            pane_id: 3,
        };
        let json = serde_json::to_string(&ControlRequest::from(&id)).unwrap();
        assert_eq!(json, r#"{"token":"t","paneId":3}"#);
    }

    #[test]
    fn 解析起会话回执() {
        let body = r#"{"ok":true,"data":{"pane":{"paneId":101,"projectId":"p1",
            "projectName":"前端","launcherId":"codex","launcherName":"Codex",
            "status":"ai-idle","alive":true}}}"#;
        let pane = parse_started_pane(200, body).unwrap();
        assert_eq!(pane.pane_id, 101);
        assert_eq!(pane.launcher_name, "Codex");
        assert_eq!(pane.status, "ai-idle");
        assert!(pane.alive);
    }

    #[test]
    fn 解析乐手名单() {
        let body = r#"{"ok":true,"data":{"panes":[
            {"paneId":101,"projectId":"p1","projectName":"前端","launcherId":"codex",
             "launcherName":"Codex","status":"ai-working","alive":true},
            {"paneId":102,"projectId":"p1","projectName":"前端","launcherId":"claude",
             "launcherName":"Claude","status":"idle","alive":false}]}}"#;
        let panes = parse_panes(200, body).unwrap();
        assert_eq!(panes.len(), 2);
        assert!(panes[0].alive);
        assert!(!panes[1].alive, "关掉的乐手照列，只是 alive 为假");
    }

    /// 空名单是**合法**的成功响应（还没起过乐手），不许被当成解析失败。
    #[test]
    fn 空乐手名单是成功() {
        let body = r#"{"ok":true,"data":{"panes":[]}}"#;
        assert!(parse_panes(200, body).unwrap().is_empty());
    }

    /// 认不出的响应照旧算失败，两条新解析器一视同仁。
    #[test]
    fn 新命令的坏响应也算失败() {
        for (status, body) in [(200, "not json"), (200, r#"{"ok":true,"data":{}}"#)] {
            assert_eq!(
                parse_panes(status, body).unwrap_err().code,
                "malformedResponse"
            );
            assert_eq!(
                parse_started_pane(status, body).unwrap_err().code,
                "malformedResponse"
            );
        }
    }

    // ─── 工单 05：send ────────────────────────────────────────

    /// `send` 的请求体：只带目标编号与正文，别人的字段一个不出线。
    #[test]
    fn 写穿请求体只带目标与正文() {
        let id = Identity {
            token: "t".into(),
            pane_id: 3,
        };
        let json = serde_json::to_string(&ControlRequest::send(&id, 101, "干活")).unwrap();
        assert_eq!(
            json,
            r#"{"token":"t","paneId":3,"targetPaneId":101,"text":"干活"}"#
        );
        // 启动器那两个字段不该跟着出线
        assert!(!json.contains("launcherId"));
        assert!(!json.contains("projectId"));
    }

    /// 多行正文经 JSON 转义出线 —— 换行归一是**桌面侧**的事，
    /// CLI 这一侧原样送过去，不许在这儿先改一遍（改两遍就是两种口径）。
    #[test]
    fn 多行正文原样出线由桌面侧归一() {
        let id = Identity {
            token: "t".into(),
            pane_id: 3,
        };
        let req = ControlRequest::send(&id, 101, "第一行\n第二行");
        assert_eq!(req.text.as_deref(), Some("第一行\n第二行"));
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""text":"第一行\n第二行""#), "{json}");
    }

    #[test]
    fn 解析写穿回执() {
        let body = r#"{"ok":true,"data":{"sent":{"paneId":101,"bracketedPaste":true}}}"#;
        let sent = parse_send_receipt(200, body).unwrap();
        assert_eq!(sent.pane_id, 101);
        assert!(sent.bracketed_paste);
    }

    /// 旧桌面端不认得 `bracketedPaste` 时缺省 `false`：宁可让编排者以为多行
    /// 没走成整块、去核对一眼，也别反过来（fail-closed 取向）。
    #[test]
    fn 回执缺字段时保守当成没整块粘贴() {
        let body = r#"{"ok":true,"data":{"sent":{"paneId":101}}}"#;
        assert!(!parse_send_receipt(200, body).unwrap().bracketed_paste);
    }

    /// 认不出的响应照旧算失败，不许退化成「反正发出去了」。
    #[test]
    fn 写穿的坏响应也算失败() {
        for (status, body) in [(200, "not json"), (200, r#"{"ok":true,"data":{}}"#)] {
            assert_eq!(
                parse_send_receipt(status, body).unwrap_err().code,
                "malformedResponse",
                "body={body}"
            );
        }
    }

    /// 工单 05 的两个错误码落在「改你的请求」那一档（既不是没能力，也不是够不着）。
    #[test]
    fn 写穿错误码的分档() {
        let f = |code: &str| ControlFailure {
            status: 400,
            code: code.into(),
            message: String::new(),
        };
        for code in ["emptyInput", "sendFailed"] {
            assert!(!f(code).is_denied(), "{code} 不是鉴权失败");
            assert!(!f(code).is_desktop_unavailable(), "{code} 不是够不着");
        }
    }

    /// 工单 03 新增的错误码要落在正确的退出码档位上。
    #[test]
    fn 新错误码的分档() {
        let f = |code: &str| ControlFailure {
            status: 400,
            code: code.into(),
            message: String::new(),
        };
        // 桌面端没答上来 = 「够不着」那一档（过会儿再试，别改请求）
        assert!(f("desktopBusy").is_desktop_unavailable());
        assert!(!f("desktopBusy").is_denied());
        // 其余都是「请求被拒」：编排者该改自己的请求或等名额
        for code in [
            "launcherNotFound",
            "projectUnreachable",
            "remoteProjectUnsupported",
            "sessionLimitReached",
            "startFailed",
            "selfTarget",
            "paneNotFound",
            "paneGone",
        ] {
            assert!(!f(code).is_denied(), "{code} 不是鉴权失败");
            assert!(!f(code).is_desktop_unavailable(), "{code} 不是够不着");
        }
    }

    /// 远程项目的 `canStartSessions` 要透到 CLI 侧（编排者据此不去白试一次）。
    #[test]
    fn 可达项目带上能否起会话() {
        let body = r#"{"ok":true,"data":{"projects":[
            {"id":"p1","name":"本地","path":"D:\\a","current":true,"canStartSessions":true},
            {"id":"p2","name":"远程","path":"/srv/api","current":false,"canStartSessions":false}]}}"#;
        let list = parse_projects(200, body).unwrap();
        assert!(list[0].can_start_sessions);
        assert!(!list[1].can_start_sessions, "远程项目起不了乐手");
    }
}
