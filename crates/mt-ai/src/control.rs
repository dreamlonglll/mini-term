//! 编排控制面：本地 HTTP 上给「编排者」用的一组控制端点。
//!
//! 骨架与鉴权设计见 `docs/adr/0003-ai-session-orchestration.md`。
//!
//! # 它长在哪
//!
//! 与 hook 上报**共用同一个本地 HTTP 服务**（[`crate::hook_server`] 那个 tiny_http
//! 监听），只是换一组路由前缀 `/control/`。共用的理由有两条：端口发现那套
//! （`hook-server.json` + `MINITERM_HOOK_PORT`）sidecar 侧现成，不必再造一份；
//! 主程序也不必为编排再开一个监听端口。
//!
//! **`/hook` 那条路由一个字都不能动**（三家 CLI 已注册在用户机器上的 hook 命令
//! 按当前形态 POST 过来），控制端点只是在它旁边加分支。
//!
//! # 鉴权：fail-closed
//!
//! `/hook` 无鉴权（同机任意进程都能 POST），控制端点不行 —— 它能列出用户的项目、
//! 后续（工单 03）还能起进程。于是：
//!
//! - 令牌**随机生成、每 pane 一枚**，由主程序在 spawn 勾了「允许编排」的启动器
//!   那一刻登记进 [`ControlPlane`]，经 `MINITERM_ORCHESTRATOR_TOKEN` 注入子进程；
//! - 请求必须同时带令牌与**自身 pane 身份**（`MINITERM_ORCHESTRATOR_PANE`），
//!   两者对不上即拒 —— 身份随环境钉死，工单 03 的自指禁令不必靠猜；
//! - 无令牌 / 认不出的令牌 / 身份对不上 → 401，不做任何降级放行。
//!
//! # 桌面能力经注入 trait 提供
//!
//! 本 crate 不依赖 `mt-config` / `gpui`，项目表与启动器名单由宿主注入
//! （[`OrchestratorHost`]，与 `mt_relay::host::RelayHost` 同一个模式，
//! [`NoopOrchestratorHost`] 是给测试与「尚未接线」用的空实现）。
//!
//! **每次请求现查**：分组关系改了要即时生效，所以 handler 每次都问一遍宿主，
//! 不在授予令牌那一刻把可达项目算死。

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 控制端点的路由前缀。
pub const CONTROL_PREFIX: &str = "/control/";

/// 编排令牌的环境变量名（应用内部协议，`MINITERM_` 保留前缀保证用户/项目级
/// 环境变量覆盖不掉）。
pub const TOKEN_ENV: &str = "MINITERM_ORCHESTRATOR_TOKEN";
/// 编排者自身 pane 身份的环境变量名。
pub const PANE_ENV: &str = "MINITERM_ORCHESTRATOR_PANE";

/// 单个控制请求 body 的字节上限（现有命令的 body 只有一百来字节）。
const MAX_CONTROL_BODY_BYTES: usize = 64 * 1024;

// ─── 注入接口 ─────────────────────────────────────────────────

/// 启动器在控制面里的切面。
///
/// **只有 `id` + `name`**：ADR 0002 的边界在编排这条链路上照旧 —— 编排者只能按
/// id 引用具名启动器，命令文本不给它看，更不给它自拟。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlLauncher {
    pub id: String,
    pub name: String,
}

/// 项目在控制面里的切面。
///
/// `group_id` 是**已经解析好的所属分组 id**（宿主负责走项目树算出来，
/// 未分组为 `None`）。放在宿主那侧是因为分组树的形状属于配置层，
/// 而这里只需要一个可比较的归属标签。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlProject {
    pub id: String,
    pub name: String,
    pub path: String,
    pub group_id: Option<String>,
}

/// 控制面向桌面端要东西的入口（`RelayHost` 的同款注入 trait）。
///
/// **两个方法都在 HTTP 线程上被调用**，实现方自己负责跨线程取值
/// （`mt-app` 那边是主线程刷新、HTTP 线程只读的一份镜像）。
pub trait OrchestratorHost: Send + Sync + 'static {
    /// 当前配置里的 AI 启动器名单（全量：任何启动器都能当乐手，见 ADR 0003）。
    fn launchers(&self) -> Vec<ControlLauncher>;

    /// 当前项目表（含分组归属）。**每次请求现查** —— 改分组要即时生效。
    fn projects(&self) -> Vec<ControlProject>;
}

/// 什么都不做的宿主实现。
///
/// 只给测试和「尚未接线」的占位场景用：名单恒空，编排者会看到「项目不可用」。
/// 生产路径必须注入真正的实现。
pub struct NoopOrchestratorHost;

impl OrchestratorHost for NoopOrchestratorHost {
    fn launchers(&self) -> Vec<ControlLauncher> {
        Vec::new()
    }
    fn projects(&self) -> Vec<ControlProject> {
        Vec::new()
    }
}

// ─── 令牌与授予 ───────────────────────────────────────────────

/// 一枚已登记的编排能力。
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    /// 编排者自己的 pane（= `MINITERM_PTY_ID`）。
    pub pane_id: u32,
    /// 编排者所在项目 —— 可达范围的原点。
    pub project_id: String,
}

/// 控制面本体。内部全是 `Arc`，`Clone` 即同一份。
#[derive(Clone, Default)]
pub struct ControlPlane {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    host: Mutex<Option<Arc<dyn OrchestratorHost>>>,
    /// 令牌登记。`grants` 与 `tokens` 是同一份事实的两个索引,必须在**同一把锁**
    /// 下变更 —— 拆成两把锁时 `grant`(先 grants 后 tokens)与 `revoke_pane`
    /// (先 tokens 后 grants)的加锁顺序相反,是典型的 AB-BA 死锁雷。
    registry: Mutex<TokenRegistry>,
}

#[derive(Default)]
struct TokenRegistry {
    /// token → 授予。
    grants: HashMap<String, Grant>,
    /// pane → token（pane 关闭时按 pane 撤销，重复授予时顶掉旧的）。
    tokens: HashMap<u32, String>,
}

impl ControlPlane {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入桌面能力。未注入时等价于 [`NoopOrchestratorHost`]。
    pub fn set_host(&self, host: Arc<dyn OrchestratorHost>) {
        *self.inner.host.lock() = Some(host);
    }

    /// 授予一枚编排令牌：随机生成、每 pane 一枚，同一 pane 重复授予顶掉旧的。
    pub fn grant(&self, pane_id: u32, project_id: &str) -> String {
        let token = new_token();
        let mut registry = self.inner.registry.lock();
        if let Some(old) = registry.tokens.insert(pane_id, token.clone()) {
            registry.grants.remove(&old);
        }
        registry.grants.insert(
            token.clone(),
            Grant {
                pane_id,
                project_id: project_id.to_string(),
            },
        );
        token
    }

    /// pane 关闭 / 重开 PTY：撤销它手上的令牌。
    pub fn revoke_pane(&self, pane_id: u32) {
        let mut registry = self.inner.registry.lock();
        if let Some(token) = registry.tokens.remove(&pane_id) {
            registry.grants.remove(&token);
        }
    }

    /// 该 pane 当前是否持有编排能力（UI 标识与工单 03 的裁决用）。
    pub fn is_orchestrator(&self, pane_id: u32) -> bool {
        self.inner.registry.lock().tokens.contains_key(&pane_id)
    }

    /// 校验令牌 + 自称身份。任何一环对不上都是 401，不降级。
    fn authorize(&self, token: &str, pane_id: u32) -> Result<Grant, ControlError> {
        if token.is_empty() {
            return Err(ControlError::MissingToken);
        }
        let registry = self.inner.registry.lock();
        let Some(grant) = registry.grants.get(token) else {
            return Err(ControlError::InvalidToken);
        };
        if grant.pane_id != pane_id {
            // 令牌与自称的 pane 对不上：要么被抄去了别处，要么调用方在撒谎。
            return Err(ControlError::InvalidToken);
        }
        Ok(grant.clone())
    }

    fn host_launchers(&self) -> Vec<ControlLauncher> {
        match self.inner.host.lock().as_ref() {
            Some(h) => h.launchers(),
            None => Vec::new(),
        }
    }

    fn host_projects(&self) -> Vec<ControlProject> {
        match self.inner.host.lock().as_ref() {
            Some(h) => h.projects(),
            None => Vec::new(),
        }
    }

    /// 处理一条控制请求。`command` 是 `/control/` 之后那一段。
    pub fn handle(&self, command: &str, body: &str) -> ControlOutcome {
        let request: ControlRequest = match serde_json::from_str(body) {
            Ok(r) => r,
            Err(_) => return ControlError::BadRequest.into_outcome(),
        };
        let grant = match self.authorize(&request.token, request.pane_id) {
            Ok(g) => g,
            Err(e) => return e.into_outcome(),
        };
        match command {
            "list-launchers" => {
                let launchers = self
                    .host_launchers()
                    .into_iter()
                    .map(|l| LauncherView {
                        id: l.id,
                        name: l.name,
                    })
                    .collect();
                ok_outcome(&ControlData::Launchers { launchers })
            }
            "list-projects" => {
                let all = self.host_projects();
                let reachable = reachable_projects(&all, &grant.project_id);
                if reachable.is_empty() {
                    // 编排者所在的项目已经不在项目表里（被删了 / 配置没接线）。
                    // 与「分组里只有自己」区分得开：后者至少有自己那一条。
                    return ControlError::ProjectUnavailable.into_outcome();
                }
                let projects = reachable
                    .into_iter()
                    .map(|p| ProjectView {
                        current: p.id == grant.project_id,
                        id: p.id,
                        name: p.name,
                        path: p.path,
                    })
                    .collect();
                ok_outcome(&ControlData::Projects { projects })
            }
            _ => ControlError::UnknownCommand.into_outcome(),
        }
    }
}

/// 随机令牌：两个 v4 UUID 拼成 256 bit 的十六进制串。
///
/// 用 `uuid` 而不是自己搓熵：它的 v4 走 `getrandom`（操作系统 CSPRNG），
/// 而本 crate 原本没有随机数依赖，`uuid` 已经在工作区里。
fn new_token() -> String {
    format!(
        "{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

/// 可达项目 = 本项目 + 同分组项目；未分组只有本项目（ADR 0003）。
///
/// 找不到本项目时返回空 —— 调用方据此报「项目不可用」。顺序照抄宿主给的顺序
/// （桌面侧栏序），本项目**不**特意提到最前面。
pub fn reachable_projects(all: &[ControlProject], own_project_id: &str) -> Vec<ControlProject> {
    let Some(own) = all.iter().find(|p| p.id == own_project_id) else {
        return Vec::new();
    };
    match own.group_id.as_deref() {
        None => vec![own.clone()],
        Some(group) => all
            .iter()
            .filter(|p| p.group_id.as_deref() == Some(group))
            .cloned()
            .collect(),
    }
}

// ─── 线上形状 ─────────────────────────────────────────────────

/// 控制请求的 body。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ControlRequest {
    #[serde(default)]
    token: String,
    /// 调用方自称的 pane 身份（来自 `MINITERM_ORCHESTRATOR_PANE`）。
    pane_id: u32,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ControlData {
    Launchers { launchers: Vec<LauncherView> },
    Projects { projects: Vec<ProjectView> },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LauncherView {
    id: String,
    name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectView {
    id: String,
    name: String,
    path: String,
    /// 编排者自己所在的那条。
    current: bool,
}

/// 一条控制请求的结论：HTTP 状态码 + JSON body。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlOutcome {
    pub status: u16,
    pub body: String,
}

/// 错误是**闭集**：CLI 按 code 分支，文案只给人看。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlError {
    MissingToken,
    InvalidToken,
    BadRequest,
    UnknownCommand,
    ProjectUnavailable,
    PayloadTooLarge,
}

impl ControlError {
    fn code(self) -> &'static str {
        match self {
            Self::MissingToken => "missingToken",
            Self::InvalidToken => "invalidToken",
            Self::BadRequest => "badRequest",
            Self::UnknownCommand => "unknownCommand",
            Self::ProjectUnavailable => "projectUnavailable",
            Self::PayloadTooLarge => "payloadTooLarge",
        }
    }

    fn status(self) -> u16 {
        match self {
            Self::MissingToken | Self::InvalidToken => 401,
            Self::BadRequest => 400,
            Self::UnknownCommand => 404,
            Self::ProjectUnavailable => 409,
            Self::PayloadTooLarge => 413,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::MissingToken => "no orchestrator token in this pane",
            Self::InvalidToken => "orchestrator token rejected",
            Self::BadRequest => "malformed control request",
            Self::UnknownCommand => "unknown control command",
            Self::ProjectUnavailable => "orchestrator project is no longer available",
            Self::PayloadTooLarge => "control request body too large",
        }
    }

    fn into_outcome(self) -> ControlOutcome {
        ControlOutcome {
            status: self.status(),
            body: format!(
                r#"{{"ok":false,"error":{{"code":"{}","message":"{}"}}}}"#,
                self.code(),
                self.message()
            ),
        }
    }
}

fn ok_outcome(data: &ControlData) -> ControlOutcome {
    // 手工拼壳:`{"ok":true,"data":<data>}`。data 侧走 serde,壳只有两个字面量键。
    let payload = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
    ControlOutcome {
        status: 200,
        body: format!(r#"{{"ok":true,"data":{payload}}}"#),
    }
}

// ─── HTTP 落点 ────────────────────────────────────────────────

/// 控制路由：这是一条控制请求就地处理完（返回 `None`），否则把请求**原样交还**
/// 给调用方去走 hook 那条路（返回 `Some`）。
///
/// 交还而不是借用，是因为 `tiny_http::Request::respond` 吃 `self`。
///
/// 与 `/hook` 的另一处差别：那条为了不阻塞 hook 脚本先回 200 再处理，这条**必须**
/// 先处理再回响应 —— 调用方等的就是数据。
pub(crate) fn try_handle_control(
    mut request: tiny_http::Request,
    plane: &ControlPlane,
) -> Option<tiny_http::Request> {
    let url = request.url().to_string();
    let Some(command) = url.strip_prefix(CONTROL_PREFIX) else {
        return Some(request);
    };
    if request.method() != &tiny_http::Method::Post {
        respond(request, ControlError::BadRequest.into_outcome());
        return None;
    }
    // 与 hook 端点同款两道闸：先看声明的长度，再用 take() 兜住谎报/分块传输。
    if request
        .body_length()
        .is_some_and(|n| n > MAX_CONTROL_BODY_BYTES)
    {
        respond(request, ControlError::PayloadTooLarge.into_outcome());
        return None;
    }
    let mut body = String::new();
    let read = {
        use std::io::Read;
        request
            .as_reader()
            .take(MAX_CONTROL_BODY_BYTES as u64 + 1)
            .read_to_string(&mut body)
    };
    if read.is_err() {
        respond(request, ControlError::BadRequest.into_outcome());
        return None;
    }
    if body.len() > MAX_CONTROL_BODY_BYTES {
        respond(request, ControlError::PayloadTooLarge.into_outcome());
        return None;
    }
    // 命令名里带查询串 / 多层路径的一律落进「未知命令」，别去猜
    let outcome = plane.handle(command, &body);
    respond(request, outcome);
    None
}

fn respond(request: tiny_http::Request, outcome: ControlOutcome) {
    let response = tiny_http::Response::from_string(outcome.body)
        .with_status_code(outcome.status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header"),
        );
    let _ = request.respond(response);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpStream;
    use std::time::Duration;

    // ─── 假宿主 ───────────────────────────────────────────────

    /// 注入式假宿主。项目表放在 `Mutex` 里是为了**中途改分组**：
    /// 「改分组即时生效」这条只有在同一个 plane 上前后各请求一次才证得出来。
    #[derive(Default)]
    struct FakeHost {
        launchers: Mutex<Vec<ControlLauncher>>,
        projects: Mutex<Vec<ControlProject>>,
    }

    impl OrchestratorHost for Arc<FakeHost> {
        fn launchers(&self) -> Vec<ControlLauncher> {
            self.launchers.lock().clone()
        }
        fn projects(&self) -> Vec<ControlProject> {
            self.projects.lock().clone()
        }
    }

    fn launcher(id: &str, name: &str) -> ControlLauncher {
        ControlLauncher {
            id: id.into(),
            name: name.into(),
        }
    }

    fn project(id: &str, group: Option<&str>) -> ControlProject {
        ControlProject {
            id: id.into(),
            name: format!("项目{id}"),
            path: format!("D:\\repos\\{id}"),
            group_id: group.map(str::to_string),
        }
    }

    // ─── HTTP 级脚手架 ────────────────────────────────────────

    /// 起一个**真的** tiny_http 服务，路由分发与生产路径是同一段代码
    /// （[`try_handle_control`]）。端口取 0 让内核挑，绝不去碰 23456 那几个
    /// —— 用户机器上很可能正跑着装机版。
    fn serve(plane: ControlPlane) -> u16 {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                if let Some(request) = try_handle_control(request, &plane) {
                    // 非控制路由：模拟 hook 循环的「其余一律 404」
                    let _ = request.respond(
                        tiny_http::Response::from_string("Not Found").with_status_code(404),
                    );
                }
            }
        });
        port
    }

    /// 裸 HTTP POST，返回 (状态码, body)。
    fn post(port: u16, path: &str, body: &str) -> (u16, String) {
        request_raw(port, "POST", path, Some(body))
    }

    fn request_raw(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
        let addr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let payload = body.unwrap_or("");
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        stream.write_all(req.as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let status: u16 = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap();
        (status, body.to_string())
    }

    fn json(body: &str) -> serde_json::Value {
        serde_json::from_str(body).expect("响应必须是 JSON")
    }

    fn error_code(body: &str) -> String {
        json(body)["error"]["code"].as_str().unwrap().to_string()
    }

    /// 一套「已授予编排能力的编排者 pane」的现场。
    fn granted() -> (ControlPlane, Arc<FakeHost>, u16, String) {
        let host = Arc::new(FakeHost::default());
        *host.launchers.lock() = vec![launcher("claude", "Claude"), launcher("codex", "Codex")];
        *host.projects.lock() = vec![project("p-self", None)];
        let plane = ControlPlane::new();
        plane.set_host(Arc::new(host.clone()));
        let token = plane.grant(7, "p-self");
        let port = serve(plane.clone());
        (plane, host, port, token)
    }

    // ─── 鉴权 fail-closed ─────────────────────────────────────

    /// 普通 pane 里跑 CLI：没有令牌 → 明确被拒（演示口径的另一半）。
    #[test]
    fn 无令牌一律被拒() {
        let (_plane, _host, port, _token) = granted();
        for cmd in ["list-launchers", "list-projects"] {
            let (status, body) = post(port, &format!("/control/{cmd}"), r#"{"paneId":7}"#);
            assert_eq!(status, 401, "{cmd}");
            assert_eq!(error_code(&body), "missingToken", "{cmd}");
            assert_eq!(json(&body)["ok"], false);
        }
    }

    /// 伪造 / 猜的令牌一律被拒，且**不泄露**任何数据。
    #[test]
    fn 坏令牌与伪造令牌一律被拒() {
        let (_plane, _host, port, token) = granted();
        let forged = format!("{}0", &token[..token.len() - 1]); // 改最后一位
        for bad in ["", "not-a-token", forged.as_str()] {
            let payload = format!(r#"{{"token":"{bad}","paneId":7}}"#);
            let (status, body) = post(port, "/control/list-launchers", &payload);
            assert_eq!(status, 401, "token={bad}");
            assert!(!body.contains("Claude"), "被拒的请求不许带出数据: {body}");
        }
    }

    /// 令牌被抄到别的 pane 去用：自称身份与令牌登记的 pane 对不上 → 拒。
    #[test]
    fn 身份与令牌对不上被拒() {
        let (_plane, _host, port, token) = granted();
        let payload = format!(r#"{{"token":"{token}","paneId":8}}"#);
        let (status, body) = post(port, "/control/list-launchers", &payload);
        assert_eq!(status, 401);
        assert_eq!(error_code(&body), "invalidToken");
    }

    /// pane 关掉之后令牌立刻作废（重开的 pane 是新身份，够不到前世的能力）。
    #[test]
    fn 撤销后令牌立即失效() {
        let (plane, _host, port, token) = granted();
        let payload = format!(r#"{{"token":"{token}","paneId":7}}"#);
        assert_eq!(post(port, "/control/list-launchers", &payload).0, 200);

        plane.revoke_pane(7);
        let (status, body) = post(port, "/control/list-launchers", &payload);
        assert_eq!(status, 401);
        assert_eq!(error_code(&body), "invalidToken");
    }

    /// 同一 pane 再次授予会顶掉旧令牌（PTY 重开、SSH 重连）。
    #[test]
    fn 重复授予顶掉旧令牌() {
        let (plane, _host, port, old) = granted();
        let new = plane.grant(7, "p-self");
        assert_ne!(old, new);

        let old_payload = format!(r#"{{"token":"{old}","paneId":7}}"#);
        assert_eq!(post(port, "/control/list-launchers", &old_payload).0, 401);
        let new_payload = format!(r#"{{"token":"{new}","paneId":7}}"#);
        assert_eq!(post(port, "/control/list-launchers", &new_payload).0, 200);
    }

    /// 令牌不可预测：每次授予都是新的一枚，且长度够。
    #[test]
    fn 令牌每次都不同() {
        let plane = ControlPlane::new();
        let a = plane.grant(1, "p");
        let b = plane.grant(2, "p");
        assert_ne!(a, b);
        assert_eq!(a.len(), 64, "两个 v4 UUID 的十六进制 = 64 字符");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ─── list-launchers ───────────────────────────────────────

    /// 勾了「允许编排」的启动器起的 pane：令牌可用，拿得到启动器名单。
    #[test]
    fn 编排者_pane_能列出启动器() {
        let (_plane, _host, port, token) = granted();
        let payload = format!(r#"{{"token":"{token}","paneId":7}}"#);
        let (status, body) = post(port, "/control/list-launchers", &payload);
        assert_eq!(status, 200);
        let v = json(&body);
        assert_eq!(v["ok"], true);
        let list = v["data"]["launchers"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["id"], "claude");
        assert_eq!(list[0]["name"], "Claude");
        // ADR 0002 的边界：命令文本一个字都不给编排者看
        assert!(!body.contains("command"), "启动器命令不得出现在响应里: {body}");
        assert!(!body.contains("shell"), "启动器 shell 不得出现在响应里: {body}");
    }

    // ─── list-projects 的可达范围 ─────────────────────────────

    /// 未分组项目：只有本项目。
    #[test]
    fn 未分组项目只能看到自己() {
        let (_plane, host, port, token) = granted();
        *host.projects.lock() = vec![
            project("p-self", None),
            project("p-other", None),
            project("p-grouped", Some("g1")),
        ];
        let payload = format!(r#"{{"token":"{token}","paneId":7}}"#);
        let (status, body) = post(port, "/control/list-projects", &payload);
        assert_eq!(status, 200);
        let list = json(&body)["data"]["projects"].clone();
        let list = list.as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["id"], "p-self");
        assert_eq!(list[0]["current"], true);
    }

    /// 同分组项目可达，组外项目一概不可见。
    #[test]
    fn 同分组项目可达而组外不可见() {
        let (_plane, host, port, token) = granted();
        *host.projects.lock() = vec![
            project("p-self", Some("g1")),
            project("p-sibling", Some("g1")),
            project("p-outsider", Some("g2")),
            project("p-loose", None),
        ];
        let payload = format!(r#"{{"token":"{token}","paneId":7}}"#);
        let (_status, body) = post(port, "/control/list-projects", &payload);
        let ids: Vec<String> = json(&body)["data"]["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["p-self", "p-sibling"]);
        assert!(!body.contains("p-outsider"), "组外项目泄露: {body}");
        assert!(!body.contains("p-loose"), "未分组项目泄露: {body}");
    }

    /// **改分组即时生效**：同一个 plane、同一枚令牌，前后两次请求结论不同 ——
    /// 可达范围是每次请求现查的，不是授予那一刻算死的。
    #[test]
    fn 改分组即时生效() {
        let (_plane, host, port, token) = granted();
        *host.projects.lock() = vec![project("p-self", None), project("p-friend", Some("g1"))];
        let payload = format!(r#"{{"token":"{token}","paneId":7}}"#);

        let (_s, before) = post(port, "/control/list-projects", &payload);
        assert_eq!(
            json(&before)["data"]["projects"].as_array().unwrap().len(),
            1
        );

        // 用户把两个项目拖进同一分组
        *host.projects.lock() = vec![
            project("p-self", Some("g1")),
            project("p-friend", Some("g1")),
        ];
        let (_s, after) = post(port, "/control/list-projects", &payload);
        let ids: Vec<String> = json(&after)["data"]["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["p-self", "p-friend"], "改分组必须即时生效");
    }

    /// 编排者所在的项目被删掉：给明确错误，而不是一个空列表。
    #[test]
    fn 项目没了给明确错误() {
        let (_plane, host, port, token) = granted();
        *host.projects.lock() = vec![project("p-other", None)];
        let payload = format!(r#"{{"token":"{token}","paneId":7}}"#);
        let (status, body) = post(port, "/control/list-projects", &payload);
        assert_eq!(status, 409);
        assert_eq!(error_code(&body), "projectUnavailable");
    }

    /// 宿主没接线（Noop）时不许把「反正没配置」当成放行的理由。
    #[test]
    fn 未接线宿主不放行也不崩() {
        let plane = ControlPlane::new();
        let token = plane.grant(1, "p-self");
        let port = serve(plane.clone());
        let payload = format!(r#"{{"token":"{token}","paneId":1}}"#);

        let (status, body) = post(port, "/control/list-launchers", &payload);
        assert_eq!(status, 200);
        assert!(
            json(&body)["data"]["launchers"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let (status, _body) = post(port, "/control/list-projects", &payload);
        assert_eq!(status, 409, "项目表空 = 本项目不可达");
    }

    // ─── 协议边界 ─────────────────────────────────────────────

    #[test]
    fn 未知命令与坏_json_有各自的语义() {
        let (_plane, _host, port, token) = granted();
        let payload = format!(r#"{{"token":"{token}","paneId":7}}"#);

        let (status, body) = post(port, "/control/list-everything", &payload);
        assert_eq!(status, 404);
        assert_eq!(error_code(&body), "unknownCommand");

        let (status, body) = post(port, "/control/list-launchers", "not json");
        assert_eq!(status, 400);
        assert_eq!(error_code(&body), "badRequest");

        // 鉴权在命令分发之前：未知命令也不该成为免鉴权的口子
        let (status, body) = post(port, "/control/list-everything", r#"{"paneId":7}"#);
        assert_eq!(status, 401);
        assert_eq!(error_code(&body), "missingToken");
    }

    #[test]
    fn 非_post_与超大_body_被拒() {
        let (_plane, _host, port, token) = granted();
        let (status, _body) = request_raw(port, "GET", "/control/list-launchers", None);
        assert_eq!(status, 400);

        let huge = format!(
            r#"{{"token":"{token}","paneId":7,"pad":"{}"}}"#,
            "x".repeat(MAX_CONTROL_BODY_BYTES)
        );
        let (status, body) = post(port, "/control/list-launchers", &huge);
        assert_eq!(status, 413);
        assert_eq!(error_code(&body), "payloadTooLarge");
    }

    /// 控制路由不许吃掉 hook 那条路（`/hook` 一个字都不能动）。
    #[test]
    fn 非控制路由原样交还() {
        let (_plane, _host, port, _token) = granted();
        let (status, _body) = post(port, "/hook", "{}");
        assert_eq!(status, 404, "本测试服务只接控制路由,交还的请求应走到 404");
    }

    // ─── 纯裁决 ───────────────────────────────────────────────

    #[test]
    fn 可达范围的纯函数口径() {
        let all = vec![
            project("a", Some("g1")),
            project("b", Some("g1")),
            project("c", None),
        ];
        assert_eq!(
            reachable_projects(&all, "a")
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(
            reachable_projects(&all, "c")
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            vec!["c"]
        );
        assert!(reachable_projects(&all, "missing").is_empty());
    }

    #[test]
    fn 是不是编排者可查() {
        let plane = ControlPlane::new();
        assert!(!plane.is_orchestrator(3));
        plane.grant(3, "p");
        assert!(plane.is_orchestrator(3));
        plane.revoke_pane(3);
        assert!(!plane.is_orchestrator(3));
    }
}
