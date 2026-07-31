//! Hook HTTP 服务器模块
//!
//! 在后台线程监听 `127.0.0.1` 的 HTTP 请求，接收 Claude Code / Codex 的
//! hook 事件上报，并通过 Tauri event 通知前端。

use crate::process_monitor::StatusEmitter;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::{AppHandle, Manager};

/// 默认监听端口
const DEFAULT_PORT: u16 = 23456;
/// 端口冲突时最多尝试的端口数
const MAX_PORT_ATTEMPTS: u16 = 5;
/// 每个 PTY 保留的已结束会话墓碑数量上限
const ENDED_SESSIONS_CAP: usize = 8;
/// 每个 PTY 跟踪的活跃会话数量上限（正常只有 1 个；嵌套非交互实例/事件乱序
/// 时短暂多个，上限只是防御事件丢失导致的累积）
const ACTIVE_SESSIONS_CAP: usize = 8;
/// Hook 事件的 JSON payload
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // 保留完整字段供未来 UI 细化使用
pub struct HookPayload {
    /// PTY ID（由 MINITERM_PTY_ID 环境变量传递）
    pub pty_id: Option<u32>,
    /// 事件名（如 UserPromptSubmit, PreToolUse 等）
    pub event: Option<String>,
    /// 来源 agent（claude-code / codex）
    pub agent: Option<String>,
    /// 会话 ID
    pub session_id: Option<String>,
    /// 工作目录
    pub cwd: Option<String>,
    /// 工具名称（PreToolUse/PostToolUse 时有值）
    pub tool_name: Option<String>,
    /// SessionEnd 的结束原因（clear / logout / prompt_input_exit / other），
    /// Claude Code 写在 stdin payload 里，sidecar 原样转发
    pub reason: Option<String>,
}

/// Hook 状态信息，供前端查询
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookStatusInfo {
    pub port: u16,
    pub running: bool,
}

/// pane 内 AI 会话的精确身份（hook 上报）。对话镜像用它把 pane 绑到
/// 确切的会话记录文件，避免同项目多 pane 串台。
#[derive(Debug, Clone)]
pub struct HookSessionId {
    /// 来源 agent（claude-code / codex），缺省按 Claude 处理
    pub agent: Option<String>,
    pub session_id: String,
}

/// Hook 状态管理器，记录每个 PTY 的最后 hook 事件时间和状态
#[derive(Clone)]
pub struct HookState {
    last_hook_time: Arc<Mutex<HashMap<u32, Instant>>>,
    last_hook_status: Arc<Mutex<HashMap<u32, String>>>,
    /// pty → 当前会话身份；/clear 等换会话时随下一个 hook 事件自动刷新
    last_session: Arc<Mutex<HashMap<u32, HookSessionId>>>,
    /// 记录哪些 PTY 曾经收到过 hook 事件（一旦标记，永不降级回轮询）
    hook_enabled: Arc<Mutex<std::collections::HashSet<u32>>>,
    /// pty → 已结束会话 id 的环形墓碑。hook 脚本是独立进程，POST 到达
    /// 顺序无保证：SessionEnd 之后仍可能收到旧会话迟到的 Stop/Notification，
    /// 若放行会把已退出的 pane 重新推回 ai-idle。`remove()` 不清墓碑
    /// （SessionEnd 自身要先打墓碑再 remove），PTY 关闭时由 `purge()` 清理。
    ended_sessions: Arc<Mutex<HashMap<u32, VecDeque<String>>>>,
    /// pty → 当前活跃会话 id 集合（有序去重）。SessionEnd 只有在移除该会话后
    /// 集合为空时才执行销毁动作：嵌套非交互实例（Bash 工具里跑 `claude -p` /
    /// `codex exec`，继承 MINITERM_PTY_ID）与"退出后立刻重开"的乱序场景下，
    /// pane 上还有别的活跃会话，误销毁会把正在工作的外层会话打回 idle。
    active_sessions: Arc<Mutex<HashMap<u32, VecDeque<String>>>>,
    port: Arc<Mutex<u16>>,
    /// 保存 server 实例，供运行时停止（Arc 共享给监听线程）
    server: Arc<Mutex<Option<Arc<tiny_http::Server>>>>,
}

impl HookState {
    pub fn new() -> Self {
        Self {
            last_hook_time: Arc::new(Mutex::new(HashMap::new())),
            last_hook_status: Arc::new(Mutex::new(HashMap::new())),
            last_session: Arc::new(Mutex::new(HashMap::new())),
            hook_enabled: Arc::new(Mutex::new(std::collections::HashSet::new())),
            ended_sessions: Arc::new(Mutex::new(HashMap::new())),
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
            port: Arc::new(Mutex::new(0)),
            server: Arc::new(Mutex::new(None)),
        }
    }

    /// 检查指定 PTY 是否已启用 hook（曾经收到过 hook 事件）
    ///
    /// 一旦启用，完全信任 hook 状态，不再降级回进程轮询。
    pub fn is_hook_enabled(&self, pty_id: u32) -> bool {
        self.hook_enabled.lock().unwrap().contains(&pty_id)
    }

    /// 获取指定 PTY 的 hook 状态
    pub fn get_status(&self, pty_id: u32) -> Option<String> {
        self.last_hook_status.lock().unwrap().get(&pty_id).cloned()
    }

    /// 当前会话身份;从未收到带 session_id 的事件时返回 None
    pub fn session_of(&self, pty_id: u32) -> Option<HookSessionId> {
        self.last_session.lock().unwrap().get(&pty_id).cloned()
    }

    /// 记录 hook 上报的会话身份(每个事件都带,直接覆盖即可)。
    /// 返回身份是否发生变化(新 pane 或换会话),变化时调用方通知前端。
    fn record_session(&self, pty_id: u32, agent: Option<String>, session_id: String) -> bool {
        let mut map = self.last_session.lock().unwrap();
        let changed = map
            .get(&pty_id)
            .map_or(true, |prev| prev.session_id != session_id);
        map.insert(pty_id, HookSessionId { agent, session_id });
        changed
    }

    /// 更新指定 PTY 的 hook 状态
    pub(crate) fn update(&self, pty_id: u32, status: String) {
        self.hook_enabled.lock().unwrap().insert(pty_id);
        self.last_hook_time
            .lock()
            .unwrap()
            .insert(pty_id, Instant::now());
        self.last_hook_status.lock().unwrap().insert(pty_id, status);
    }

    /// 移除指定 PTY 的 hook 状态。不清墓碑：SessionEnd 打完墓碑后调用
    /// 本方法，墓碑要继续挡住旧会话的迟到事件。
    pub fn remove(&self, pty_id: u32) {
        self.hook_enabled.lock().unwrap().remove(&pty_id);
        self.last_hook_time.lock().unwrap().remove(&pty_id);
        self.last_hook_status.lock().unwrap().remove(&pty_id);
        self.last_session.lock().unwrap().remove(&pty_id);
    }

    /// PTY 关闭时的彻底清理：hook 状态 + 墓碑 + 活跃会话集
    pub fn purge(&self, pty_id: u32) {
        self.remove(pty_id);
        self.ended_sessions.lock().unwrap().remove(&pty_id);
        self.active_sessions.lock().unwrap().remove(&pty_id);
    }

    /// 记录会话为活跃。任意非 SessionEnd 事件都调（不只 SessionStart：
    /// hook server 中途启用时首个事件可能是 Stop/PreToolUse）。
    /// 有序去重；超容量挤掉最老的——正常情况集合里只有 1 个。
    fn note_session_active(&self, pty_id: u32, session_id: &str) {
        let mut map = self.active_sessions.lock().unwrap();
        let queue = map.entry(pty_id).or_default();
        if queue.iter().any(|s| s == session_id) {
            return;
        }
        if queue.len() >= ACTIVE_SESSIONS_CAP {
            queue.pop_front();
        }
        queue.push_back(session_id.to_string());
    }

    /// SessionEnd：把该会话移出活跃集，返回移除后活跃集是否已空。
    /// 为空 → 这是 pane 上最后一个会话，调用方执行销毁动作；
    /// 非空 → pane 上还有别的活跃会话（嵌套 `claude -p` / 退出后立刻重开的
    /// 乱序），只打墓碑不销毁。payload 无 session_id 时不移除，仅报告空否。
    fn end_session(&self, pty_id: u32, session_id: Option<&str>) -> bool {
        let mut map = self.active_sessions.lock().unwrap();
        let Some(queue) = map.get_mut(&pty_id) else {
            return true;
        };
        if let Some(sid) = session_id {
            queue.retain(|s| s != sid);
        }
        let empty = queue.is_empty();
        if empty {
            map.remove(&pty_id);
        }
        empty
    }

    /// 给已结束的会话 id 打墓碑
    pub fn mark_session_ended(&self, pty_id: u32, session_id: String) {
        let mut map = self.ended_sessions.lock().unwrap();
        let queue = map.entry(pty_id).or_default();
        if queue.iter().any(|s| s == &session_id) {
            return;
        }
        if queue.len() >= ENDED_SESSIONS_CAP {
            queue.pop_front();
        }
        queue.push_back(session_id);
    }

    /// 该会话是否已被打墓碑（已结束）
    pub fn is_session_ended(&self, pty_id: u32, session_id: &str) -> bool {
        self.ended_sessions
            .lock()
            .unwrap()
            .get(&pty_id)
            .map_or(false, |q| q.iter().any(|s| s == session_id))
    }

    /// 获取当前服务器端口
    pub fn get_port(&self) -> u16 {
        *self.port.lock().unwrap()
    }

    /// 设置服务器端口
    fn set_port(&self, port: u16) {
        *self.port.lock().unwrap() = port;
    }

    /// 保存 server 实例
    fn set_server(&self, server: Option<Arc<tiny_http::Server>>) {
        *self.server.lock().unwrap() = server;
    }

    /// 检查 server 是否正在运行
    pub fn is_server_running(&self) -> bool {
        self.server.lock().unwrap().is_some()
    }
}

/// 将 hook 事件名映射为 PTY 状态
///
/// - ai-working: 表示 AI 正在处理（思考/工具调用/子代理/压缩）
/// - ai-idle: 表示 AI 等待用户输入（停止/权限请求/通知等）
/// - SessionEnd 单独处理（清除 hook 状态），不在此映射
fn map_event_to_status(event: &str, agent: Option<&str>) -> Option<&'static str> {
    // Codex 的 PermissionRequest 在审批 UI 弹出前触发，批准后直接执行工具，
    // 直到 PostToolUse 之前不再有任何 hook 事件。若映射为 ai-idle，批准后
    // 整个命令执行期间状态都会卡在 ai-idle，且审批弹出时误报"任务完成"，
    // 因此对 Codex 保持 ai-working（仍处于任务中）。
    if event == "PermissionRequest" && agent == Some("codex") {
        return Some("ai-working");
    }
    match event {
        // ai-working 状态：AI 正在积极工作
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "SubagentStart" | "SubagentStop"
        | "PreCompact" | "PostCompact" => Some("ai-working"),
        // ai-idle 状态：AI 等待用户输入
        "SessionStart" | "Stop" | "PermissionRequest" | "Notification" | "Elicitation" => {
            Some("ai-idle")
        }
        _ => None,
    }
}

/// 启动 hook HTTP 服务器
///
/// 在后台线程监听，接收 hook 事件后通过 Tauri event 通知前端。
/// 端口从 DEFAULT_PORT 开始尝试，冲突时自动递增。
/// 返回 `Err` 表示无法绑定端口，调用方应将错误提示给用户。
pub fn start_hook_server(
    app: AppHandle,
    hook_state: HookState,
    emitter: StatusEmitter,
) -> Result<(), String> {
    // 如果已经在运行，不重复启动
    if hook_state.is_server_running() {
        eprintln!("[hook-server] 服务器已在运行，跳过启动");
        return Ok(());
    }

    // 在当前线程绑定端口，以便同步获取 server 实例
    let bound = {
        let mut result = None;
        for offset in 0..MAX_PORT_ATTEMPTS {
            let port = DEFAULT_PORT + offset;
            let addr = format!("127.0.0.1:{}", port);
            match tiny_http::Server::http(&addr) {
                Ok(s) => {
                    eprintln!("[hook-server] 监听 {}", addr);
                    hook_state.set_port(port);
                    result = Some((s, port));
                    break;
                }
                Err(e) => {
                    eprintln!("[hook-server] 端口 {} 被占用: {}", port, e);
                }
            }
        }
        result
    };

    let (server, port) = match bound {
        Some(s) => s,
        None => {
            eprintln!("[hook-server] 无法绑定任何端口，hook 服务器未启动");
            return Err("无法绑定端口 (23456-23460)，hook 服务器启动失败".to_string());
        }
    };

    // 用 Arc 包装 server，共享给 HookState 和监听线程
    let server = Arc::new(server);
    hook_state.set_server(Some(server.clone()));

    // 写入端口文件
    write_port_file(&app, port);

    std::thread::spawn(move || {
        // 处理请求
        for mut request in server.incoming_requests() {
            if request.method() != &tiny_http::Method::Post {
                let response =
                    tiny_http::Response::from_string("Method Not Allowed").with_status_code(405);
                let _ = request.respond(response);
                continue;
            }

            let url = request.url().to_string();
            if url != "/hook" {
                let response = tiny_http::Response::from_string("Not Found").with_status_code(404);
                let _ = request.respond(response);
                continue;
            }

            // 读取 body
            let mut body = String::new();
            if request.as_reader().read_to_string(&mut body).is_err() {
                let response =
                    tiny_http::Response::from_string("Bad Request").with_status_code(400);
                let _ = request.respond(response);
                continue;
            }

            // 解析 JSON payload
            let payload: HookPayload = match serde_json::from_str(&body) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("[hook-server] JSON 解析失败: {}", e);
                    let response =
                        tiny_http::Response::from_string("Bad Request").with_status_code(400);
                    let _ = request.respond(response);
                    continue;
                }
            };

            // 立即响应 200，不阻塞 hook 脚本
            let response = tiny_http::Response::from_string("OK").with_status_code(200);
            let _ = request.respond(response);

            // 处理事件
            if let (Some(pty_id), Some(ref event)) = (payload.pty_id, &payload.event) {
                if event == "SessionEnd" {
                    // 只用 payload 自带的 session_id 打墓碑。不要退回 session_of:
                    // 新会话的 SessionStart 若先到,session_of 已是新会话,兜底会
                    // 把新会话误打进墓碑,冻结其全部后续事件。
                    if let Some(sid) = payload.session_id.clone() {
                        hook_state.mark_session_ended(pty_id, sid);
                    }
                    let was_last =
                        hook_state.end_session(pty_id, payload.session_id.as_deref());
                    if payload.reason.as_deref() == Some("clear") {
                        // /clear 换会话不是退出：紧随其后的 SessionStart 会带新
                        // session id 刷新状态，这里只靠墓碑挡住旧会话的迟到事件
                        eprintln!(
                            "[hook-server] pty_id={} event=SessionEnd(clear) -> 仅记录墓碑",
                            pty_id
                        );
                    } else if !was_last {
                        // pane 上还有别的活跃会话:嵌套非交互实例(Bash 工具里跑
                        // `claude -p` / `codex exec`,继承 MINITERM_PTY_ID)结束,
                        // 或退出后立刻重开、新 SessionStart 先到的乱序。此时清
                        // hook 状态 / AI 会话标记会误杀仍在跑的会话,只留墓碑。
                        eprintln!(
                            "[hook-server] pty_id={} event=SessionEnd 非最后活跃会话,仅记录墓碑",
                            pty_id
                        );
                    } else {
                        // 最后一个活跃会话结束 → 权威退出信号：清 hook 状态回退
                        // 到轮询，同时清输入检测的 AI 会话标记——双击 Ctrl+C
                        // 间隔超窗漏检时靠这里自愈
                        hook_state.remove(pty_id);
                        app.state::<crate::pty::PtyManager>().clear_ai_session(pty_id);
                        emitter.emit_if_changed(&app, pty_id, "idle");
                        eprintln!(
                            "[hook-server] pty_id={} event=SessionEnd(reason={:?}) -> hook 已清除，回退到 idle",
                            pty_id, payload.reason
                        );
                    }
                } else {
                    // 已结束会话的迟到事件直接丢弃：hook 脚本是独立进程，
                    // POST 到达顺序无保证，放行会把退出后的 pane 推回 ai-idle
                    if let Some(sid) = payload.session_id.as_deref() {
                        if hook_state.is_session_ended(pty_id, sid) {
                            eprintln!(
                                "[hook-server] pty_id={} event={} 来自已结束会话 {}，忽略",
                                pty_id, event, sid
                            );
                            continue;
                        }
                    }
                    // 会话身份先于状态映射记录:即使事件不映射状态(如未知事件),
                    // session_id 也是有效信息;/clear 换会话时靠这里自动刷新
                    if let Some(sid) = payload.session_id.clone() {
                        hook_state.note_session_active(pty_id, &sid);
                        if hook_state.record_session(pty_id, payload.agent.clone(), sid.clone()) {
                            // 会话身份变化(新会话/换会话)时通知前端,供布局持久化
                            // 记录「退出时该 pane 正跑着哪个 AI 会话」以便重启续接
                            let _ = tauri::Emitter::emit(
                                &app,
                                "pty-ai-session",
                                serde_json::json!({
                                    "ptyId": pty_id,
                                    "agent": payload.agent.clone(),
                                    "sessionId": sid,
                                }),
                            );
                        }
                    }
                    if let Some(status) = map_event_to_status(event, payload.agent.as_deref()) {
                        // hook 事件是 AI 进程存活的直接证据:输入检测漏判启动
                        // (别名/包装脚本)或误判退出(任务中双击 Ctrl+C)时,
                        // 靠这里把 AI 会话标记扶正,保住后续 marker/移动端语义
                        app.state::<crate::pty::PtyManager>().mark_ai_session(pty_id);
                        hook_state.update(pty_id, status.to_string());

                        // 通知前端（与 process_monitor 共享同一份去重表）
                        emitter.emit_if_changed(&app, pty_id, status);

                        eprintln!(
                            "[hook-server] pty_id={} event={} -> status={}",
                            pty_id, event, status
                        );
                    }
                }
            }
        }
    });

    Ok(())
}

/// 停止 hook HTTP 服务器
///
/// 取出保存的 server 实例，调用 `unblock()` 中断阻塞循环，
/// 清理端口文件并重置端口。
pub fn stop_hook_server(hook_state: &HookState, app: &AppHandle) {
    let server = hook_state.server.lock().unwrap().take();
    if let Some(s) = server {
        s.unblock();
        eprintln!("[hook-server] 服务器已停止");
    }
    hook_state.set_port(0);
    // 清理端口文件
    delete_port_file(app);
}

/// 运行时切换 hook server 开关
#[tauri::command]
pub fn toggle_hook_server(
    app: AppHandle,
    hook_state: tauri::State<'_, HookState>,
    emitter: tauri::State<'_, StatusEmitter>,
    enabled: bool,
) -> Result<(), String> {
    if enabled {
        if !hook_state.is_server_running() {
            start_hook_server(app, hook_state.inner().clone(), emitter.inner().clone())?;
        }
    } else if hook_state.is_server_running() {
        stop_hook_server(hook_state.inner(), &app);
    }
    Ok(())
}

/// 将端口信息写入 app_data_dir/hook-server.json
fn write_port_file(app: &AppHandle, port: u16) {
    if let Ok(dir) = app.path().app_data_dir() {
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("hook-server.json");
        let content = format!("{{\"port\":{}}}", port);
        if let Err(e) = crate::fs::atomic_write(&path, content.as_bytes()) {
            eprintln!("[hook-server] 写入端口文件失败 {}: {}", path.display(), e);
        } else {
            eprintln!("[hook-server] 端口文件已写入 {}", path.display());
        }
    }
}

/// 删除端口文件 app_data_dir/hook-server.json
fn delete_port_file(app: &AppHandle) {
    if let Ok(dir) = app.path().app_data_dir() {
        let path = dir.join("hook-server.json");
        if path.exists() {
            if let Err(e) = std::fs::remove_file(&path) {
                eprintln!("[hook-server] 删除端口文件失败 {}: {}", path.display(), e);
            } else {
                eprintln!("[hook-server] 端口文件已删除 {}", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_state_records_and_clears_session_identity() {
        let state = HookState::new();
        assert!(state.session_of(1).is_none());

        state.record_session(1, Some("claude-code".into()), "sid-a".into());
        let s = state.session_of(1).unwrap();
        assert_eq!(s.session_id, "sid-a");
        assert_eq!(s.agent.as_deref(), Some("claude-code"));

        // /clear 换会话:同 pty 覆盖为新 id
        state.record_session(1, Some("claude-code".into()), "sid-b".into());
        assert_eq!(state.session_of(1).unwrap().session_id, "sid-b");

        // SessionEnd / PTY 关闭走 remove:会话身份一并清除
        state.remove(1);
        assert!(state.session_of(1).is_none());
    }

    #[test]
    fn end_session_last_active_triggers_teardown() {
        let state = HookState::new();
        // 从未见过任何会话:保守按"最后一个"处理(执行销毁,对齐旧行为)
        assert!(state.end_session(1, Some("sid-a")));

        // 正常生命周期:唯一活跃会话结束 -> 销毁
        state.note_session_active(1, "sid-a");
        assert!(state.end_session(1, Some("sid-a")));
    }

    #[test]
    fn nested_session_end_keeps_outer_alive() {
        let state = HookState::new();
        // 外层交互会话 A 活跃中,嵌套非交互实例 B(claude -p)启动又结束
        state.note_session_active(1, "sid-outer");
        state.note_session_active(1, "sid-nested");
        assert!(!state.end_session(1, Some("sid-nested"))); // 不销毁:A 还在
        assert!(state.end_session(1, Some("sid-outer"))); // A 退出才销毁
    }

    #[test]
    fn exit_restart_race_skips_teardown() {
        let state = HookState::new();
        // 退出后立刻重开:新会话 B 的 SessionStart 先到,旧会话 A 的 SessionEnd 迟到
        state.note_session_active(1, "sid-a");
        state.note_session_active(1, "sid-b");
        assert!(!state.end_session(1, Some("sid-a"))); // B 活跃,不销毁
    }

    #[test]
    fn end_session_unknown_sid_respects_remaining_active() {
        let state = HookState::new();
        state.note_session_active(1, "sid-a");
        // 未知会话结束(其 Start 早于 hook server 启用):A 仍活跃,不销毁
        assert!(!state.end_session(1, Some("sid-x")));
        // payload 无 session_id:按剩余活跃集判断
        assert!(!state.end_session(1, None));
        assert!(state.end_session(1, Some("sid-a")));
    }

    #[test]
    fn note_session_active_dedup_and_cap() {
        let state = HookState::new();
        // 重复 note 去重,不占额外容量
        state.note_session_active(1, "sid-0");
        state.note_session_active(1, "sid-0");
        // 再 note sid-1..sid-CAP,溢出一格 → 最老的 sid-0 被挤出
        for i in 1..ACTIVE_SESSIONS_CAP + 1 {
            state.note_session_active(1, &format!("sid-{}", i));
        }
        // 结束 sid-1..sid-(CAP-1):每次集合都还非空
        for i in 1..ACTIVE_SESSIONS_CAP {
            assert!(!state.end_session(1, Some(&format!("sid-{}", i))));
        }
        // 结束最后一个成员即空——证明 sid-0 确实已被挤出(否则此处非空)
        assert!(state.end_session(1, Some(&format!("sid-{}", ACTIVE_SESSIONS_CAP))));
    }

    #[test]
    fn purge_clears_active_sessions() {
        let state = HookState::new();
        state.note_session_active(1, "sid-a");
        state.purge(1);
        // purge 后无残留:未知 sid 结束按空集处理
        assert!(state.end_session(1, Some("sid-b")));
    }

    #[test]
    fn tombstone_blocks_ended_session() {
        let state = HookState::new();
        assert!(!state.is_session_ended(1, "sid-a"));
        state.mark_session_ended(1, "sid-a".into());
        assert!(state.is_session_ended(1, "sid-a"));
        // 其他会话 / 其他 pty 不受影响
        assert!(!state.is_session_ended(1, "sid-b"));
        assert!(!state.is_session_ended(2, "sid-a"));
    }

    #[test]
    fn tombstone_survives_remove_cleared_by_purge() {
        let state = HookState::new();
        state.update(1, "ai-idle".into());
        state.mark_session_ended(1, "sid-a".into());

        // SessionEnd 路径:先打墓碑再 remove,墓碑必须存活
        state.remove(1);
        assert!(!state.is_hook_enabled(1));
        assert!(state.is_session_ended(1, "sid-a"));

        // PTY 关闭走 purge:墓碑一并清理
        state.purge(1);
        assert!(!state.is_session_ended(1, "sid-a"));
    }

    #[test]
    fn tombstone_capped_and_deduped() {
        let state = HookState::new();
        // 重复打墓碑不占额外容量
        state.mark_session_ended(1, "sid-0".into());
        state.mark_session_ended(1, "sid-0".into());
        for i in 1..ENDED_SESSIONS_CAP + 2 {
            state.mark_session_ended(1, format!("sid-{}", i));
        }
        // 超容量后最老的被挤出,最新的保留
        assert!(!state.is_session_ended(1, "sid-0"));
        assert!(state.is_session_ended(1, &format!("sid-{}", ENDED_SESSIONS_CAP + 1)));
    }

    #[test]
    fn codex_permission_request_maps_to_ai_working() {
        assert_eq!(
            map_event_to_status("PermissionRequest", Some("codex")),
            Some("ai-working")
        );
    }

    #[test]
    fn claude_permission_request_keeps_ai_idle() {
        assert_eq!(
            map_event_to_status("PermissionRequest", Some("claude-code")),
            Some("ai-idle")
        );
        // agent 字段缺失时保持原有行为
        assert_eq!(
            map_event_to_status("PermissionRequest", None),
            Some("ai-idle")
        );
    }

    #[test]
    fn other_events_unaffected_by_agent() {
        assert_eq!(
            map_event_to_status("Stop", Some("codex")),
            Some("ai-idle")
        );
        assert_eq!(
            map_event_to_status("PreToolUse", Some("codex")),
            Some("ai-working")
        );
        assert_eq!(map_event_to_status("Unknown", Some("codex")), None);
    }
}
