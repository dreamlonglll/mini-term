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

use serde::{Deserialize, Serialize};

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
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlRequest {
    pub token: String,
    pub pane_id: u32,
}

impl From<&Identity> for ControlRequest {
    fn from(id: &Identity) -> Self {
        Self {
            token: id.token.clone(),
            pane_id: id.pane_id,
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
}
