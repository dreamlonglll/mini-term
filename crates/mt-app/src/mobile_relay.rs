//! 移动端中转的接线层:把 [`mt_relay::MobileRelayManager`] 装进 GPUI 壳。
//!
//! ```text
//!  中转 ──WSS──► mt-relay 的 tokio 任务
//!                  │  RelayEvents(状态 / 配对码 / 改名 / 发起会话)
//!                  │  RelayHost::write_pty(移动端指令)
//!                  ▼
//!            futures mpsc channel        ← 唯一的跨线程口
//!                  ▼
//!        RelayBridge 的前台泵(主线程)──► AppStore / Window
//!
//!  AppStore ──StoreEvent──► 150ms 去抖 ──► 内容去重 ──► update_sessions
//!                                   └──► 镜像快照(启动器 / 项目 / 活 PTY)
//!                                        ▲
//!                            RelayHost 在 tokio 线程上只读它
//! ```
//!
//! # 为什么必须过 channel
//!
//! [`RelayHost`] / [`RelayEvents`] 的**十一个方法全部在 mt-relay 自持的 tokio 运行时
//! 上被同步调用**(`host.rs:14-15`),而 gpui 的 `Entity` 只能在主线程碰。分工:
//!
//! - 四个 events + `write_pty` → channel 回主线程([`RelaySignal`]);
//! - 三个 AI 查询(`hook_session` / `ai_session_agent` / `ai_session_started_at`)
//!   → **直接透传** [`AiBridge`](它内部全是 `Arc` + `Mutex`,本来就跨线程安全);
//! - `launchers` / `project` → 主线程刷新的[镜像快照](HostMirror);
//! - `bracketed_paste` → 镜像里存的终端模拟器 `Arc` 句柄,现读 VT 模式位。
//!
//! # tokio 运行时
//!
//! mt-app **没有**全局 tokio 运行时(gpui 自带 executor),所以这里用
//! [`MobileRelayManager::new`],让 mt-relay 自持它那两个工作线程(首次 `apply`
//! 时才惰性创建)。不为这一处给 mt-app 引 tokio —— 那会在进程里多出一个线程池,
//! 而中转是低频链路。宿主将来若真有了运行时,改用 `with_runtime` 注入即可。
//!
//! # ADR 0002 的边界
//!
//! 启动器的 `command` / `shell` **只在桌面端进程内流转**:发给移动端的
//! `MobileLauncher` 只有 `id` + `name`(mt-relay 的 `send_snapshot` 已经落死),
//! 上层要做的是**别自己另开一条把 command 送出去的路** —— 日志、错误消息、
//! 失败回执一律不带命令原文([`StartSessionFailReason`] 是闭集枚举,不带自由文本)。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedSender};
use gpui::{App, AppContext, Context, Entity, Global, Subscription, Task, WeakEntity, Window};
use mt_config::{AiLauncher, AppConfig, ProjectConfig};
use mt_relay::host::{HookSessionId, RelayEvents, RelayHost, RelayProject};
use mt_relay::{
    MobileRelayManager, MobileRelayStatusPayload, RenamePanePayload, StartSessionFailReason,
    StartSessionPayload, SyncPane, SyncProject,
};
use mt_terminal::TerminalEmulator;
use mt_terminal::alacritty_terminal::term::TermMode;
use parking_lot::Mutex;

use crate::ai::AiBridge;
use crate::i18n::tr;
use crate::store::{AppStore, StoreEvent};
use crate::tree::PaneStatus;

/// 结构同步的去抖(原版 `mobileSessionSync.ts:101` 的 150ms)。
///
/// **两道闸一个都不能省**(坑 9):store 的变化频率远高于 zustand 的 `subscribe`
/// (终端状态、焦点、布局全在同一个 entity 上)。去掉去抖或内容去重,WebSocket
/// 上就会出现每秒几十条 `SessionsDelta`。
///
/// 触发源已从 `cx.observe(&store)`(每次 notify 都醒)收窄为订阅 [`StoreEvent`]
/// 里快照真正读的那几类(OSC 标题、焦点、主题字号这些不再触发),但 AI 状态本身
/// 就可能每秒反复上报,两道闸照旧保留。
const SYNC_DEBOUNCE: Duration = Duration::from_millis(150);

// ─── 跨线程信号 ───────────────────────────────────────────────

/// tokio 线程送上来的东西。**唯一**的跨线程口。
pub enum RelaySignal {
    /// 连接状态变化(原 `mobile-relay-status`)。
    Status(MobileRelayStatusPayload),
    /// 中转签发的一次性配对码(原 `mobile-relay-pairing-code`)。
    PairingCode(String),
    /// 移动端改会话名(原 `mobile-rename-pane`)。标题已收敛过。
    RenamePane(RenamePanePayload),
    /// 移动端发起新 AI 会话(原 `mobile-start-session`),校验已通过。
    StartSession(StartSessionPayload),
    /// 移动端指令的写穿请求(`RelayHost::write_pty` 的主线程落点)。
    WritePty { pty_id: u32, data: String },
}

// ─── RelayHost 用的镜像快照 ───────────────────────────────────

/// 主线程按节奏刷新、tokio 线程只读的那一份桌面状态切面。
///
/// `launchers` / `project` 是低频数据,150ms 的陈旧度可以接受;
/// `live_ptys` 给 `write_pty` 做预检(见 [`HostImpl::write_pty`])。
#[derive(Default)]
struct HostMirror {
    launchers: Vec<mt_relay::AiLauncher>,
    projects: HashMap<String, RelayProject>,
    live_ptys: HashSet<u32>,
    /// 活 PTY 的终端模拟器句柄:`bracketed_paste` 拿它**现读** VT 模式位,
    /// 不吃快照的陈旧度(agent 启动那一刻才开 bracketed paste)
    emulators: HashMap<u32, Arc<TerminalEmulator>>,
}

/// 该 PTY 的终端此刻开没开 bracketed paste —— 与桌面端粘贴(`paste_to_bytes`)
/// 同一判据。镜像里还没有它(刚建的 pane 等下一轮刷新)按没开处理。
/// 句柄先取出再读模式位:不持着镜像锁去等终端锁(reader 线程推进 grid 时持它)。
fn bracketed_paste_of(mirror: &Mutex<HostMirror>, pty_id: u32) -> bool {
    let emulator = mirror.lock().emulators.get(&pty_id).cloned();
    emulator.is_some_and(|e| e.mode().contains(TermMode::BRACKETED_PASTE))
}

/// `mt_config::AiLauncher` → `mt_relay::AiLauncher`。
///
/// 两个**同形但不同 crate** 的类型(mt-relay 刻意不依赖 mt-config,`host.rs:29-32`),
/// 没有 `From` 可用,只能逐字段搬。
fn to_relay_launcher(l: &AiLauncher) -> mt_relay::AiLauncher {
    mt_relay::AiLauncher {
        id: l.id.clone(),
        name: l.name.clone(),
        shell: l.shell.clone(),
        command: l.command.clone(),
    }
}

/// `mt_config::ProjectConfig` → `mt_relay::RelayProject`(镜像里的项目切面)。
///
/// 只搬两个字段,但它们**共同决定移动端能不能在该项目发起新会话**
/// (`mt_relay::can_start_session`:远程项目与 WSL 根项目一律置灰)。
/// `ssh_connection_id` **照实读**:U 批时全仓没有任何项目带它,于是远程项目
/// 被误判为可发起;BB-a 批把「添加远程项目」接上之后这条判据自动生效 ——
/// 单测钉的就是这条(`远程项目镜像照实带连接id且不可发起会话`)。
fn to_relay_project(p: &ProjectConfig) -> RelayProject {
    RelayProject {
        path: p.path.clone(),
        ssh_connection_id: p.ssh_connection_id.clone(),
    }
}

struct HostImpl {
    mirror: Arc<Mutex<HostMirror>>,
    ai: AiBridge,
    tx: UnboundedSender<RelaySignal>,
}

impl RelayHost for HostImpl {
    fn launchers(&self) -> Vec<mt_relay::AiLauncher> {
        self.mirror.lock().launchers.clone()
    }

    fn project(&self, project_id: &str) -> Option<RelayProject> {
        self.mirror.lock().projects.get(project_id).cloned()
    }

    /// **预检 + 乐观 Ok**(规格 §1.5.3 的路 A)。
    ///
    /// 签名是同步返回 `Result`,而真正的写穿必须回主线程走
    /// [`AppStore::write_to_pane`](与本人键入同一条链路,AI 输入观察不旁路)。
    /// 于是先查活 PTY 镜像挡掉「pane 已经没了」这一档,再把请求投进 channel 后
    /// 返回 `Ok` —— 回执语义从「已写入 PTY」弱化成「已排队写入」。
    ///
    /// 这个弱化是可接受的:mt-relay 自己的注释本来就写着「回执仅表示『已写入
    /// PTY』,AI 真正接收以镜像回流为准」,而 `handle_mobile_command` 在调这里
    /// **之前**已经用它自己的 `pane_ptys` 映射挡掉了 `PaneNotFound` 那一档。
    /// 只有真出现「回执说成功但命令没进去」的投诉时,才换成 oneshot + 超时阻塞。
    fn write_pty(&self, pty_id: u32, data: String) -> Result<(), String> {
        if !self.mirror.lock().live_ptys.contains(&pty_id) {
            return Err(format!("pty {pty_id} is not live"));
        }
        self.tx
            .unbounded_send(RelaySignal::WritePty { pty_id, data })
            .map_err(|_| "relay pump closed".to_string())
    }

    /// 移动端指令据此决定正文包不包 `ESC[200~ … ESC[201~`(mt-relay 那边先写正文、
    /// 隔一拍再单独写回车)。在 tokio 线程上调:只碰镜像里的 `Arc` 句柄,不碰实体。
    fn bracketed_paste(&self, pty_id: u32) -> bool {
        bracketed_paste_of(&self.mirror, pty_id)
    }

    fn hook_session(&self, pty_id: u32) -> Option<HookSessionId> {
        self.ai.perception().hooks().session_of(pty_id)
    }

    /// **如实**返回输入检测到的 agent 名(坑 3)。
    ///
    /// 镜像绑定据此判断「这个 agent 有没有会话记录」:opencode 这类只靠输入
    /// 检测识别的 agent 必须拿到空镜像 —— 退启发式会绑到同项目里别家的最新会话
    /// 文件,把别人的对话贴到这个 pane 上(比空镜像更糟)。判定已在
    /// `mt_relay::mirror::agent_has_session_log` 里,**这里要做的就是别绕开它**:
    /// 不为了「让镜像有东西看」返回 `None` 或伪造成 `"claude"`。
    fn ai_session_agent(&self, pty_id: u32) -> Option<String> {
        self.ai.perception().tracker().ai_session_agent(pty_id)
    }

    fn ai_session_started_at(&self, pty_id: u32) -> Option<SystemTime> {
        self.ai.perception().tracker().ai_session_started_at(pty_id)
    }
}

struct EventsImpl {
    tx: UnboundedSender<RelaySignal>,
}

impl RelayEvents for EventsImpl {
    fn status_changed(&self, status: MobileRelayStatusPayload) {
        let _ = self.tx.unbounded_send(RelaySignal::Status(status));
    }
    fn pairing_code(&self, code: String) {
        let _ = self.tx.unbounded_send(RelaySignal::PairingCode(code));
    }
    fn rename_pane(&self, payload: RenamePanePayload) {
        let _ = self.tx.unbounded_send(RelaySignal::RenamePane(payload));
    }
    fn start_session(&self, payload: StartSessionPayload) {
        let _ = self.tx.unbounded_send(RelaySignal::StartSession(payload));
    }
}

// ─── 快照组装(纯数据,可测) ─────────────────────────────────

/// 项目在快照里需要的最小切面。
///
/// 单独一个类型是为了让 [`build_snapshot`] 不依赖 `AppConfig` / `AppStore` ——
/// 顺序规则与可见性规则是这一批最容易写错的地方,必须能拿裸数据钉住。
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectFacet {
    pub id: String,
    pub name: String,
    pub path: String,
    pub ssh_connection_id: Option<String>,
    /// 祖先分组名链(根→父),顶层项目为空。
    pub group_path: Vec<String>,
}

/// pane 在快照里需要的最小切面。
#[derive(Clone, Debug, PartialEq)]
pub struct PaneFacet {
    pub id: String,
    /// `customTitle ?? shellName`(`mobileSessionSync.ts:65`)。
    pub title: String,
    pub status: PaneStatus,
    pub pty_id: Option<u32>,
    /// 桌面端黄灯(`PaneState.attention`:提问待答/等待授权),透传给移动端
    pub attention: bool,
}

/// 组装好的一条 pane。**自带 `PartialEq`**:内容去重直接比结构,
/// 不必像原版那样 `JSON.stringify`(`SyncPane` 只 derive `Deserialize`,
/// 序列化不了,而逐字段比较与 JSON 比较是同一口径)。
#[derive(Clone, Debug, PartialEq)]
pub struct SnapPane {
    pub pane_id: String,
    pub title: String,
    pub status: String,
    pub pty_id: Option<u32>,
    /// 进 `PartialEq` 才能让黄灯变化触发内容去重后的重发
    pub needs_attention: bool,
}

/// 组装好的一条项目。
#[derive(Clone, Debug, PartialEq)]
pub struct SnapProject {
    pub project_id: String,
    pub name: String,
    /// 项目根路径:后端镜像订阅据此定位会话记录文件,**不转发给移动端**。
    pub path: String,
    pub ssh_connection_id: Option<String>,
    pub group_path: Vec<String>,
    pub panes: Vec<SnapPane>,
}

impl From<SnapProject> for SyncProject {
    fn from(p: SnapProject) -> Self {
        SyncProject {
            project_id: p.project_id,
            name: p.name,
            path: p.path,
            ssh_connection_id: p.ssh_connection_id,
            group_path: p.group_path,
            panes: p
                .panes
                .into_iter()
                .map(|x| SyncPane {
                    pane_id: x.pane_id,
                    title: x.title,
                    status: x.status,
                    pty_id: x.pty_id,
                    needs_attention: x.needs_attention,
                })
                .collect(),
        }
    }
}

/// 按**项目树的深度优先序**列出全部项目并带上祖先分组名链。
///
/// 逐条照抄 `src/utils/projectTree.ts:296-321` 的 `getProjectsWithGroupPath`:
///
/// - 递归 `config.project_tree`,遇分组就把组名压进 `group_path` 继续下钻;
/// - 已见过的项目 id 跳过(去重);
/// - **不在树里的项目**(异常配置兜底)按 `config.projects` 顺序追加到顶层,
///   `group_path` 为空;
/// - **折叠态不下发** —— 折叠是桌面侧栏的视图状态,移动端要的是完整清单。
///
/// 注意顺序**不是** `config.projects` 的存储序:移动端顺序渲染就能还原桌面端
/// 侧栏的排列,分组层级靠每项自带的 `group_path` 还原。
pub fn ordered_projects(config: &AppConfig) -> Vec<ProjectFacet> {
    fn walk(
        items: &[mt_config::ProjectTreeItem],
        group_path: &[String],
        config: &AppConfig,
        seen: &mut HashSet<String>,
        out: &mut Vec<ProjectFacet>,
    ) {
        for item in items {
            match item {
                mt_config::ProjectTreeItem::Group(group) => {
                    let mut next = group_path.to_vec();
                    next.push(group.name.clone());
                    walk(&group.children, &next, config, seen, out);
                }
                mt_config::ProjectTreeItem::ProjectId(id) => {
                    let Some(project) = config.projects.iter().find(|p| &p.id == id) else {
                        continue;
                    };
                    if !seen.insert(project.id.clone()) {
                        continue;
                    }
                    out.push(ProjectFacet {
                        id: project.id.clone(),
                        name: project.name.clone(),
                        path: project.path.clone(),
                        ssh_connection_id: project.ssh_connection_id.clone(),
                        group_path: group_path.to_vec(),
                    });
                }
            }
        }
    }

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    if let Some(tree) = config.project_tree.as_ref() {
        walk(tree, &[], config, &mut seen, &mut out);
    }
    for project in &config.projects {
        if seen.contains(&project.id) {
            continue;
        }
        out.push(ProjectFacet {
            id: project.id.clone(),
            name: project.name.clone(),
            path: project.path.clone(),
            ssh_connection_id: project.ssh_connection_id.clone(),
            group_path: Vec::new(),
        });
    }
    out
}

/// 组装一次结构快照。逐条照抄 `mobileSessionSync.ts:47-82` 的可见性规则:
///
/// - **项目上报全集**(不是「只有活跃会话的项目」)—— 手机的发起弹层要能选到
///   还没有会话的项目;裁剪只作用于 `panes`;
/// - **pane 只有 AI 会话中的进快照**:`ai-working` / `ai-idle`,外加
///   「**曾是 AI 会话且现处 error 态**」的 pane。裸 shell 一律不出现。
///
/// `ai_pane_ids` 是**跨调用状态**(坑 10),每轮重算后整体替换:做成局部变量的话,
/// AI 会话崩溃后 pane 会立刻从手机列表里消失 —— 用户看到的是「会话凭空没了」
/// 而不是「会话出错了」。
pub fn build_snapshot(
    projects: &[ProjectFacet],
    panes: &HashMap<String, Vec<PaneFacet>>,
    ai_pane_ids: &mut HashSet<String>,
) -> Vec<SnapProject> {
    let mut next_ai: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(projects.len());
    for project in projects {
        let mut snap_panes = Vec::new();
        for pane in panes.get(&project.id).map(Vec::as_slice).unwrap_or(&[]) {
            let is_ai = matches!(pane.status, PaneStatus::AiWorking | PaneStatus::AiIdle);
            let is_ai_error = pane.status == PaneStatus::Error && ai_pane_ids.contains(&pane.id);
            if !is_ai && !is_ai_error {
                continue;
            }
            next_ai.insert(pane.id.clone());
            snap_panes.push(SnapPane {
                pane_id: pane.id.clone(),
                title: pane.title.clone(),
                status: pane.status.as_str().to_string(),
                pty_id: pane.pty_id,
                needs_attention: pane.attention,
            });
        }
        out.push(SnapProject {
            project_id: project.id.clone(),
            name: project.name.clone(),
            path: project.path.clone(),
            ssh_connection_id: project.ssh_connection_id.clone(),
            group_path: project.group_path.clone(),
            panes: snap_panes,
        });
    }
    *ai_pane_ids = next_ai;
    out
}

// ─── 配对链接与二维码几何(纯函数,可测) ────────────────────

/// 中转地址(ws/wss)→ 移动端网页地址(http/https)。
///
/// 逐字照搬 `MobileRelayModal.tsx:19-26`:去尾部斜杠 → `wss://` 换 `https://`、
/// `ws://` 换 `http://`、已经是 `http(s)://` 原样、其余一律补 `https://`。
pub fn relay_http_base(relay_url: &str) -> String {
    let trimmed = relay_url.trim().trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("wss://") {
        return format!("https://{rest}");
    }
    if let Some(rest) = trimmed.strip_prefix("ws://") {
        return format!("http://{rest}");
    }
    if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        return trimmed.to_string();
    }
    format!("https://{trimmed}")
}

/// 配对链接(`MobileRelayModal.tsx:89`)。
pub fn pair_url(relay_url: &str, code: &str) -> String {
    format!("{}/#pair={code}", relay_http_base(relay_url))
}

/// 二维码画布边长(原版 `QRCode.toDataURL(.., { width: 260 })`)。
pub const QR_CANVAS_PX: f32 = 260.0;
/// 静区宽度,单位是**模块**不是像素。
///
/// 原版写的是 `margin: 1`,而 `qrcode` 这个 npm 包的默认值是 4 —— 照抄 1,
/// 不要「顺手改回标准值」。
pub const QR_QUIET_MODULES: usize = 1;

/// 一张画好排版的二维码。
#[derive(Clone)]
pub struct QrMatrix {
    /// 模块数(不含静区),即 `QrCode::width()`。
    pub width: usize,
    /// `width * width` 的位矩阵,行优先;`true` = 深色模块。
    pub dark: Vec<bool>,
    /// 单个模块画多少像素(整数,免得逐模块累计出半像素缝)。
    pub module_px: f32,
    /// 实际绘制的边长 = `module_px * (width + 2 * 静区)`,≤ [`QR_CANVAS_PX`]。
    pub draw_px: f32,
}

/// 模块像素与实际绘制边长。
///
/// **向下取整**再乘回去:相机识别靠的是模块边界干净,浮点模块宽会让相邻模块
/// 之间出现半像素的灰缝(抗锯齿),扫码成功率明显下降。取整之后画面比画布小
/// 一点,居中显示即可。至少 1px —— 画布再小也不能画出零宽模块。
pub fn qr_module_px(width: usize, canvas_px: f32, quiet: usize) -> (f32, f32) {
    let total = width + quiet * 2;
    if total == 0 {
        return (0.0, 0.0);
    }
    let module = (canvas_px / total as f32).floor().max(1.0);
    (module, module * total as f32)
}

/// 编码一段文本成二维码。纠错等级 **M**(`qrcode` npm 包不指定时的默认值)。
///
/// 失败(文本太长,超出 40 版最大容量)返回 `None` —— 配对链接只有几十字节,
/// 正常路径上到不了这里。
pub fn encode_qr(text: &str) -> Option<QrMatrix> {
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M)
        .ok()?;
    let width = code.width();
    let dark = code
        .to_colors()
        .into_iter()
        .map(|c| c == qrcode::Color::Dark)
        .collect();
    let (module_px, draw_px) = qr_module_px(width, QR_CANVAS_PX, QR_QUIET_MODULES);
    Some(QrMatrix {
        width,
        dark,
        module_px,
        draw_px,
    })
}

// ─── 启动器编辑的纯逻辑(可测) ───────────────────────────────

/// 草稿能不能保存(原版 `disabled={!name.trim() || !command.trim()}`)。
pub fn launcher_draft_valid(name: &str, command: &str) -> bool {
    !name.trim().is_empty() && !command.trim().is_empty()
}

/// 要不要显示命令识别警告(`AiLauncherSection.tsx:42-61`)。
///
/// **空命令不提示**(别拿假警告吓人);否则问一次
/// [`mt_relay::check_launcher_command`] —— 它是同步纯函数(内部就是
/// `mt_ai::is_interactive_ai_command`),原版那套 `cancelled` 防竞态整个不需要。
///
/// 这条警告**不阻塞保存**:它只是把失败从「手机上等 15 秒超时」前移到配置时,
/// 不是安全防线(防线是「命令只能来自桌面端配置」)。
pub fn command_warning(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() {
        return false;
    }
    !mt_relay::check_launcher_command(command)
}

/// 列表行的副行文案:`"{shell} › {command}"`(U+203A)或裸命令。
pub fn launcher_subtitle(shell: Option<&str>, command: &str) -> String {
    match shell.filter(|s| !s.is_empty()) {
        Some(shell) => format!("{shell} \u{203a} {command}"),
        None => command.to_string(),
    }
}

/// 把一条草稿并进名单:`id` 非空 = 替换同 id 那条,否则追加到末尾。
///
/// `shell` 为空串时字段**整个不写**(`None`),不是写 `""` —— 与原版
/// `...(draft.shell ? { shell } : {})` 一致,也与 `AiLauncher` 的
/// `skip_serializing_if = "Option::is_none"` 对齐(磁盘格式一字不改)。
pub fn upsert_launcher(
    list: &[AiLauncher],
    id: &str,
    name: &str,
    shell: Option<&str>,
    command: &str,
) -> Vec<AiLauncher> {
    let entry = AiLauncher {
        id: if id.is_empty() {
            // 对名单去重:移动端按 id 引用启动器,撞上会指到另一条
            crate::tree::gen_unique_id("launcher", |candidate| {
                list.iter().any(|l| l.id == candidate)
            })
        } else {
            id.to_string()
        },
        name: name.trim().to_string(),
        shell: shell
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        command: command.trim().to_string(),
    };
    if id.is_empty() {
        let mut next = list.to_vec();
        next.push(entry);
        return next;
    }
    list.iter()
        .map(|l| if l.id == id { entry.clone() } else { l.clone() })
        .collect()
}

// ─── 桥本体 ───────────────────────────────────────────────────

struct GlobalRelay(Entity<RelayBridge>);
impl Global for GlobalRelay {}

/// 取全局的中转桥。[`install`] 之前(或纯单测里)返回 `None`。
pub fn bridge(cx: &App) -> Option<Entity<RelayBridge>> {
    cx.try_global::<GlobalRelay>().map(|g| g.0.clone())
}

pub struct RelayBridge {
    manager: Arc<MobileRelayManager>,
    store: Entity<AppStore>,
    mirror: Arc<Mutex<HostMirror>>,
    /// 开着的「移动端」面板。配对码到达时用它;面板关着直接丢弃 ——
    /// 留着的旧码可能已被后续操作作废(原版关闭面板会 `setQrDataUrl(null)`)。
    panel: Option<WeakEntity<crate::mobile_panel::MobilePanel>>,
    /// 上一次发出去的快照(内容去重的第二道闸)。
    last_sent: Option<Vec<SnapProject>>,
    /// 「曾是 AI 会话」的 pane 集合,跨调用保留(坑 10)。
    ai_pane_ids: HashSet<String>,
    sync_generation: u64,
    _sync_task: Option<Task<()>>,
    _pump: Task<()>,
    /// store 事件订阅(结构同步的触发源)。
    _store_events: Subscription,
}

impl RelayBridge {
    pub fn manager(&self) -> Arc<MobileRelayManager> {
        self.manager.clone()
    }

    /// 面板打开 / 关闭时登记自己(配对码的去处)。
    pub fn set_panel(&mut self, panel: Option<WeakEntity<crate::mobile_panel::MobilePanel>>) {
        self.panel = panel;
    }

    /// 保存中转地址与密钥并重建连接。
    ///
    /// 顺序与原版 `applyRelaySettings` 一致:**先落盘再 apply**
    /// (`set_mobile_relay_endpoint` 内部走 `save_config_now`)。
    ///
    /// `key` 是面板里的明文:落盘前在 store 那边封存,建连用的仍是这份明文。
    /// 封存失败时密钥没存下来,但**这一次照样建连**(用户刚填的值就在手上),
    /// 原因经 `Err` 交回面板就地提示。
    pub fn apply_settings(&self, url: &str, key: &str, cx: &mut App) -> Result<(), String> {
        let (url, key) = (url.trim().to_string(), key.trim().to_string());
        let saved = self.store.update(cx, |store, cx| {
            store.set_mobile_relay_endpoint(&url, &key, cx)
        });
        self.manager.apply(&url, &key);
        saved
    }

    /// 启动器名单变化后:落盘 + 让中转重发一次全量快照
    /// (手机侧的发起弹层立即看到新名单)。
    pub fn save_launchers(&mut self, launchers: Vec<AiLauncher>, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, cx| store.set_launchers(launchers, cx));
        // 镜像要立刻跟上 —— 手机可能下一秒就按 id 发起会话,等 150ms 去抖那一轮
        // 刷新会让刚加的启动器短暂「不存在」
        self.refresh_mirror(cx);
        self.manager.launchers_changed();
    }

    fn deliver_pairing_code(&mut self, code: String, cx: &mut Context<Self>) {
        // 面板关着时直接丢弃:泵是全局的(原版靠 hook 只在 modal mount 时注册),
        // 而旧码可能已被后续操作作废
        if !crate::overlay::contains(crate::overlay::key(crate::overlay::kind::MOBILE_RELAY)) {
            return;
        }
        let Some(panel) = self.panel.as_ref().and_then(|p| p.upgrade()) else {
            return;
        };
        // 用**配置里已保存**的地址拼配对链接,不是输入框里的草稿值 ——
        // 草稿还没 apply 的话中转根本不在那个地址上
        let relay_url = self.store.read(cx).mobile_relay().relay_url;
        let url = pair_url(&relay_url, &code);
        panel.update(cx, |panel, cx| panel.set_pairing_code(code, url, cx));
    }

    /// 移动端发起会话:**外层统一回执**,结构上杜绝漏回执(坑 5)。
    ///
    /// 成功那一支的回执要等 PTY 定局:PTY 在后台起(见 `pane` 模块注释),
    /// 建完 pane、写完命令那一刻还不知道终端到底起没起来。等待口
    /// ([`TerminalPane::spawn_settled`](crate::pane::TerminalPane::spawn_settled))
    /// 保证必有答复 —— 定局即答,pane 先没了就是 `Canceled`,都按各自结论回执一次。
    /// 回执走 `manager` 的克隆,不依赖桥实体还在。
    fn start_session(
        &mut self,
        payload: StartSessionPayload,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request_id = payload.request_id.clone();
        match self.try_start_session(&payload, window, cx) {
            Ok((pane_id, spawned)) => {
                let manager = self.manager.clone();
                let project_id = payload.project_id.clone();
                let launcher_name = payload.launcher_name.clone();
                cx.spawn(async move |_, cx| {
                    if !spawned.await.unwrap_or(false) {
                        manager.start_session_result(
                            request_id,
                            false,
                            None,
                            Some(StartSessionFailReason::SpawnFailed),
                        );
                        return;
                    }
                    // 桌面端 toast。凭证被盗时这是唯一的审计迹象,所以即便不切过去
                    // 也要弹。走自建 toast 层的 `mobile-session` 档:info 图标 + 点击切
                    // 项目(原版 `mobileStartSession.ts:122-127` 就是这一档)。**不去重**
                    // —— 连开两个会话该看到两条,原版这条也是裸 `pushNotification`。
                    // 项目名由标题行展示,正文只补启动器名。
                    cx.update(|cx| {
                        let project_name = AppStore::global(cx)
                            .read(cx)
                            .project(&project_id)
                            .map(|p| p.name.clone())
                            .unwrap_or_default();
                        crate::toast::push_message(
                            crate::notify::ToastKind::MobileSession,
                            project_id,
                            project_name,
                            tr!("app", "mobileStartSession", launcher = launcher_name),
                            cx,
                        );
                    });
                    manager.start_session_result(request_id, true, Some(pane_id), None);
                })
                .detach();
            }
            Err(reason) => self
                .manager
                .start_session_result(request_id, false, None, Some(reason)),
        }
    }

    /// 逐步照抄 `src/utils/mobileStartSession.ts:54-130`。
    ///
    /// `Result` 的每一条 `?` 早退都由调用方兜住回执 —— 这正是把它拆成内层函数
    /// 的原因(原版 5 处失败分支 + 1 处成功**全都**手动调了 `reportResult`,
    /// GPUI 侧用 `?` 极容易漏)。
    ///
    /// `Ok` 带回「PTY 起没起来」的等待口,成功回执与审计 toast 都要等它定局
    /// (见 [`Self::start_session`])。
    fn try_start_session(
        &mut self,
        payload: &StartSessionPayload,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(String, futures::channel::oneshot::Receiver<bool>), StartSessionFailReason> {
        // 1. 项目还在吗。mt-relay 侧已经校验过一遍,但从校验到执行之间用户
        //    可能刚好把项目移除了,所以 ProjectNotFound 这一档必须保留
        let shell = {
            let store = self.store.read(cx);
            if store.project(&payload.project_id).is_none() {
                return Err(StartSessionFailReason::ProjectNotFound);
            }
            // 2. shell:启动器绑定的 → default_shell → 列表首项。
            //    绑定的 shell 被删掉时退回默认 —— 总比不开好,用户在桌面能看到实情。
            // 3. 一个 shell 都没配:开不出终端,也没有能写进布局的 shellName
            store
                .resolve_shell(payload.shell_name.as_deref())
                .ok_or(StartSessionFailReason::SpawnFailed)?
        };

        // 4~7. 建 PTY + 建 pane + 挂进最左侧叶子末尾(不激活、不抢焦点、不切项目)。
        //      customTitle 用启动器名:回到电脑前一眼看出这个标签是什么。
        let pane_id = self
            .store
            .update(cx, |store, cx| {
                store.append_pane_background(
                    &payload.project_id,
                    shell,
                    Some(payload.launcher_name.clone()),
                    window,
                    cx,
                )
            })
            .ok_or(StartSessionFailReason::SpawnFailed)?;

        // 8. 写启动命令 + 回车。AI 会话身份靠输入检测建立,只有「往 shell 里敲进
        //    启动命令并回车」这条路能让 pane 进入 AI 会话状态。
        //    写不进去时**保留 pane**:用户回桌面能看到它卡在哪。PTY 还在后台起时
        //    命令进 pane 的写入队列,回填时按序冲刷。
        //
        //    ⚠️ `TerminalPane::write` 在起失败后是**静默丢弃**的(返回值只说明
        //    「找到了那个终端实体」),所以还要单独问一句 PTY 起来了没 ——
        //    否则 shell 路径失效时手机会拿到成功回执然后干等 15s 超时。PTY 在后台起,
        //    这一问的答案要等定局,交给调用方 await。
        let data = format!("{}\r", payload.command);
        let (written, spawned) = self.store.update(cx, |store, cx| {
            let written = store.write_to_pane(&payload.project_id, &pane_id, &data, cx);
            let pty_id = store
                .project_state(&payload.project_id)
                .and_then(|s| s.pane(&pane_id))
                .and_then(|p| p.pty_id);
            (
                written,
                pty_id.map(|pty_id| store.pane_spawn_settled(pty_id, cx)),
            )
        });
        let Some(spawned) = spawned.filter(|_| written) else {
            return Err(StartSessionFailReason::SpawnFailed);
        };
        // 9. 审计 toast 与成功回执在调用方等到 PTY 起来之后再发
        Ok((pane_id, spawned))
    }

    fn apply_signal(&mut self, signal: RelaySignal, window: &mut Window, cx: &mut Context<Self>) {
        match signal {
            RelaySignal::Status(status) => {
                self.store
                    .update(cx, |store, cx| store.set_mobile_relay_status(status, cx));
            }
            RelaySignal::PairingCode(code) => self.deliver_pairing_code(code, cx),
            RelaySignal::RenamePane(payload) => {
                // **不回执** —— 改完的新名字会随结构增量推回手机,那既是反馈也是真相
                self.store.update(cx, |store, cx| {
                    store.rename_pane_by_id(&payload.pane_id, &payload.title, cx)
                });
            }
            RelaySignal::StartSession(payload) => self.start_session(payload, window, cx),
            RelaySignal::WritePty { pty_id, data } => {
                let Some((project_id, pane_id)) = self.store.read(cx).pane_of_pty(pty_id) else {
                    // 预检到落地之间 pane 被关掉了。mt-relay 已经回过「已排队」的
                    // 成功回执,这里没有能改口的通道 —— 手机侧靠镜像回流看不到
                    // 响应,与真实终端上「命令发出去但进程刚好退了」同形。
                    return;
                };
                self.store.update(cx, |store, cx| {
                    store.write_to_pane(&project_id, &pane_id, &data, cx);
                });
            }
        }
    }

    /// 刷新 [`RelayHost`] 用的镜像快照(主线程写,tokio 线程读)。
    fn refresh_mirror(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let launchers = store
            .mobile_relay()
            .launchers
            .iter()
            .map(to_relay_launcher)
            .collect();
        let mut projects = HashMap::new();
        let mut live_ptys = HashSet::new();
        for project in store.projects() {
            projects.insert(project.id.clone(), to_relay_project(project));
            if let Some(state) = store.project_state(&project.id) {
                live_ptys.extend(state.pty_ids());
            }
        }
        let emulators = live_ptys
            .iter()
            .filter_map(|&pty_id| Some((pty_id, store.terminal(pty_id)?.read(cx).emulator())))
            .collect();
        *self.mirror.lock() = HostMirror {
            launchers,
            projects,
            live_ptys,
            emulators,
        };
    }

    /// 组装 + 内容去重 + 喂给 mt-relay。
    fn sync_now(&mut self, cx: &mut Context<Self>) {
        self.refresh_mirror(cx);

        let (ordered, panes) = {
            let store = self.store.read(cx);
            let ordered = ordered_projects(store.config());
            let mut panes: HashMap<String, Vec<PaneFacet>> = HashMap::new();
            for project in &ordered {
                // 跨全部面板平铺:移动端不感知面板层,后台面板的终端一样要能看
                let facets = store
                    .project_state(&project.id)
                    .map(|state| {
                        state
                            .all_panes()
                            .into_iter()
                            .map(|pane| PaneFacet {
                                id: pane.id.clone(),
                                title: pane.label().to_string(),
                                status: pane.status,
                                pty_id: pane.pty_id,
                                attention: pane.attention,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                panes.insert(project.id.clone(), facets);
            }
            (ordered, panes)
        };

        let snapshot = build_snapshot(&ordered, &panes, &mut self.ai_pane_ids);
        if self.last_sent.as_ref() == Some(&snapshot) {
            return;
        }
        self.last_sent = Some(snapshot.clone());
        self.manager
            .update_sessions(snapshot.into_iter().map(SyncProject::from).collect());
    }

    /// 去抖排期(照 `AppStore::save_config_soon`,用代号防旧任务晚到)。
    fn schedule_sync(&mut self, cx: &mut Context<Self>) {
        self.sync_generation += 1;
        let generation = self.sync_generation;
        self._sync_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SYNC_DEBOUNCE).await;
            let _ = this.update(cx, |this: &mut RelayBridge, cx| {
                if this.sync_generation == generation {
                    this.sync_now(cx);
                }
            });
        }));
    }
}

/// 建桥、登记全局、按配置建连一次。
///
/// 返回的实体要被宿主持有(泵与观察者靠它的生命周期保活),
/// 与 `Workspace::_ai_pump` 同一种分工。
pub fn install(store: Entity<AppStore>, window: &mut Window, cx: &mut App) -> Entity<RelayBridge> {
    let (tx, mut rx) = mpsc::unbounded::<RelaySignal>();
    let mirror = Arc::new(Mutex::new(HostMirror::default()));
    let ai = store.read(cx).ai();

    let host: Arc<dyn RelayHost> = Arc::new(HostImpl {
        mirror: mirror.clone(),
        ai,
        tx: tx.clone(),
    });
    let events: Arc<dyn RelayEvents> = Arc::new(EventsImpl { tx });
    let manager = Arc::new(MobileRelayManager::new(host, events));

    let entity = cx.new(|cx: &mut Context<RelayBridge>| {
        // 只订阅快照真正读的那几类变化(见 `StoreEvent::touches_relay_sync`)。
        // 此前挂在 notify 上,多个 pane 的 OSC 标题(约 4Hz/pane)会把 150ms 去抖
        // 一直往后推 —— 四个 pane 交错改标题时间隔常在 100ms 以内,AI 状态变化
        // 反而迟迟推不出去。
        let store_events = cx.subscribe(
            &store,
            |this: &mut RelayBridge, _, event: &StoreEvent, cx| {
                if event.touches_relay_sync() {
                    this.schedule_sync(cx);
                }
            },
        );
        // 泵要 `spawn_in`:发起会话得建 pane(要 `&mut Window`)、弹 toast
        // (`window.push_notification`)也得有窗口
        let pump = cx.spawn_in(window, async move |this, cx| {
            while let Some(signal) = rx.next().await {
                if this
                    .update_in(cx, |this: &mut RelayBridge, window, cx| {
                        this.apply_signal(signal, window, cx)
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        RelayBridge {
            manager: manager.clone(),
            store: store.clone(),
            mirror,
            panel: None,
            last_sent: None,
            ai_pane_ids: HashSet::new(),
            sync_generation: 0,
            _sync_task: None,
            _pump: pump,
            _store_events: store_events,
        }
    });
    cx.set_global(GlobalRelay(entity.clone()));

    // 先把镜像与快照喂满,再建连 —— 反过来的话握手成功那一瞬间发出去的全量
    // 快照是空的(`launchers()` 也会返回空,手机看到「没有可用启动器」)
    entity.update(cx, |this, cx| this.sync_now(cx));
    let relay = store.read(cx).mobile_relay();
    if !relay.relay_url.trim().is_empty() {
        // 库里是信封,建连前在这里解开。解不开(换机器 / 密钥文件丢了)就按
        // 「未填密钥」建连:中转回「密钥不正确」后连接循环停在那个状态上,
        // 不重试不刷屏;面板打开时再就地提示重填(`mobile_panel::open`)。
        let key = crate::secrets::reveal_relay_key(&relay.desktop_key).unwrap_or_else(|err| {
            eprintln!("[mobile-relay] 已存桌面密钥无法解密,按未填写处理: {err}");
            String::new()
        });
        manager.apply(&relay.relay_url, &key);
    }
    entity
}

#[cfg(test)]
mod tests {
    use super::*;
    use mt_config::{ProjectGroup, ProjectTreeItem};

    fn project(id: &str, name: &str) -> ProjectConfig {
        ProjectConfig::new(id, name, format!("D:/{name}"))
    }

    fn pane(id: &str, status: PaneStatus, pty: Option<u32>) -> PaneFacet {
        PaneFacet {
            id: id.to_string(),
            title: format!("t-{id}"),
            status,
            pty_id: pty,
            attention: false,
        }
    }

    // ── RelayHost 镜像里的项目切面(U 批遗留的 can_start_session 误判)──

    /// U 批记档:「`can_start_session` 对 SSH 远程项目误判 true」的根因是
    /// **当时全仓没有任何项目带 `ssh_connection_id`**(没有「添加远程项目」入口),
    /// 而不是镜像造假。BB-a 把入口接上之后这条判据自动生效 —— 这里钉死。
    #[test]
    fn 远程项目镜像照实带连接id且不可发起会话() {
        let mut remote = project("p1", "远程");
        remote.path = "/home/u/proj".into();
        remote.ssh_connection_id = Some("conn-1".into());
        let facet = to_relay_project(&remote);
        assert_eq!(facet.ssh_connection_id.as_deref(), Some("conn-1"));
        assert!(
            !mt_relay::can_start_session(&facet.path, facet.ssh_connection_id.as_deref()),
            "远程项目的镜像一定是空的,移动端不该能在上面发起会话"
        );

        // 本地项目照旧可发起(同一条映射,不许把 None 造成 Some)
        let local = project("p2", "本地");
        let facet2 = to_relay_project(&local);
        assert!(facet2.ssh_connection_id.is_none());
        assert!(mt_relay::can_start_session(
            &facet2.path,
            facet2.ssh_connection_id.as_deref()
        ));
    }

    /// 移动端指令包不包 bracketed paste,看终端**此刻**的 VT 模式位(agent 启动后才
    /// 开,退出时关);镜像里还没有的 PTY 按没开处理。
    #[test]
    fn 移动端指令的粘贴包裹跟随终端模式位() {
        let emulator = Arc::new(TerminalEmulator::new(mt_terminal::TermSize::new(80, 24)));
        let mirror = Mutex::new(HostMirror::default());
        mirror.lock().emulators.insert(7, emulator.clone());
        assert!(!bracketed_paste_of(&mirror, 7));
        emulator.advance(b"\x1b[?2004h");
        assert!(bracketed_paste_of(&mirror, 7));
        emulator.advance(b"\x1b[?2004l");
        assert!(!bracketed_paste_of(&mirror, 7));
        assert!(!bracketed_paste_of(&mirror, 8));
    }

    // ── relayHttpBase 四种前缀 ──────────────────────────────

    #[test]
    fn 中转地址转网页地址的四种前缀() {
        assert_eq!(relay_http_base("wss://r.example.com"), "https://r.example.com");
        assert_eq!(relay_http_base("ws://r.example.com"), "http://r.example.com");
        assert_eq!(relay_http_base("https://r.example.com"), "https://r.example.com");
        assert_eq!(relay_http_base("http://r.example.com"), "http://r.example.com");
        // 没有前缀 → 补 https
        assert_eq!(relay_http_base("r.example.com"), "https://r.example.com");
    }

    #[test]
    fn 中转地址去掉尾部斜杠且忽略首尾空白() {
        assert_eq!(relay_http_base("  wss://r.example.com///  "), "https://r.example.com");
        assert_eq!(pair_url("wss://r.example.com/", "AB12"), "https://r.example.com/#pair=AB12");
    }

    // ── 二维码几何 ─────────────────────────────────────────

    #[test]
    fn 二维码模块像素向下取整且不超画布() {
        // 21 模块(版本 1)+ 2 静区 = 23;260/23 = 11.30 → 11
        let (module, draw) = qr_module_px(21, 260.0, 1);
        assert_eq!(module, 11.0);
        assert_eq!(draw, 11.0 * 23.0);
        assert!(draw <= 260.0);
        // 静区照抄原版的 1 模块,不是 qrcode 包默认的 4
        assert_eq!(QR_QUIET_MODULES, 1);
    }

    #[test]
    fn 二维码模块至少一像素() {
        // 画布远小于模块数时也不能画出零宽模块(否则整张图是空白)
        let (module, draw) = qr_module_px(177, 20.0, 1);
        assert_eq!(module, 1.0);
        assert_eq!(draw, 179.0);
    }

    #[test]
    fn 配对链接能编成二维码且位矩阵尺寸自洽() {
        let qr = encode_qr(&pair_url("wss://relay.example.com", "ABCD1234")).expect("能编码");
        assert_eq!(qr.dark.len(), qr.width * qr.width);
        // 定位图案:左上角第一个模块必然是深色
        assert!(qr.dark[0]);
        assert!(qr.module_px >= 1.0);
        assert!(qr.draw_px <= QR_CANVAS_PX);
    }

    // ── 启动器校验与命令识别 ───────────────────────────────

    #[test]
    fn 启动器草稿名称与命令都不能是空白() {
        assert!(launcher_draft_valid("Claude", "claude"));
        assert!(!launcher_draft_valid("  ", "claude"));
        assert!(!launcher_draft_valid("Claude", "   "));
        assert!(!launcher_draft_valid("", ""));
    }

    /// 口径必须与 PTY 输入检测同源(两处漂移就会出现「面板说没问题、
    /// 手机上却永远等不到 AI 会话」)。这里钉住 mt-relay pin 测试的同一组样本。
    #[test]
    fn 命令识别警告的口径() {
        for ok in [
            "claude",
            "codex",
            "grok",
            "omp",
            "claude --dangerously-skip-permissions",
            "grok --resume",
            "omp --continue",
        ] {
            assert!(!command_warning(ok), "{ok} 应该被识别");
        }
        for bad in ["npm test", "claude -p 'hi'", "codex --version"] {
            assert!(command_warning(bad), "{bad} 应该出警告");
        }
        // 空命令不提示 —— 刚点开表单时不该先甩一条红字
        assert!(!command_warning(""));
        assert!(!command_warning("   "));
    }

    #[test]
    fn 启动器副行有_shell_时才带前缀() {
        assert_eq!(launcher_subtitle(Some("pwsh"), "claude"), "pwsh \u{203a} claude");
        assert_eq!(launcher_subtitle(None, "claude"), "claude");
        // 空串等同于没绑定
        assert_eq!(launcher_subtitle(Some(""), "claude"), "claude");
    }

    #[test]
    fn 保存草稿时空_shell_不写字段() {
        let next = upsert_launcher(&[], "", "  Claude  ", Some("   "), " claude ");
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].name, "Claude");
        assert_eq!(next[0].command, "claude");
        assert!(next[0].shell.is_none(), "空 shell 不该写成空串");
        assert!(!next[0].id.is_empty());
    }

    #[test]
    fn 编辑替换同_id_那条而新增追加末尾() {
        let list = vec![
            AiLauncher {
                id: "claude".into(),
                name: "Claude".into(),
                shell: None,
                command: "claude".into(),
            },
            AiLauncher {
                id: "codex".into(),
                name: "Codex".into(),
                shell: None,
                command: "codex".into(),
            },
        ];
        let edited = upsert_launcher(&list, "codex", "Codex 新", Some("pwsh"), "codex resume");
        assert_eq!(edited.len(), 2);
        assert_eq!(edited[0].id, "claude");
        assert_eq!(edited[1].id, "codex");
        assert_eq!(edited[1].name, "Codex 新");
        assert_eq!(edited[1].shell.as_deref(), Some("pwsh"));

        let added = upsert_launcher(&list, "", "Grok", None, "grok");
        assert_eq!(added.len(), 3);
        assert_eq!(added[2].name, "Grok");
        // 预置条目的 id 是 "claude" / "codex",新 id 不能与之撞车
        assert!(added[2].id != "claude" && added[2].id != "codex");
    }

    // ── 快照顺序与可见性 ───────────────────────────────────

    #[test]
    fn 项目按树的深度优先序展开并带分组名链() {
        let mut config = AppConfig::default();
        config.projects = vec![project("a", "A"), project("b", "B"), project("c", "C")];
        config.project_tree = Some(vec![
            ProjectTreeItem::ProjectId("c".into()),
            ProjectTreeItem::Group(ProjectGroup {
                id: "g1".into(),
                name: "工作".into(),
                // 折叠态**不下发**:折叠是桌面侧栏的视图状态
                collapsed: true,
                children: vec![
                    ProjectTreeItem::ProjectId("a".into()),
                    ProjectTreeItem::Group(ProjectGroup {
                        id: "g2".into(),
                        name: "子组".into(),
                        collapsed: false,
                        children: vec![ProjectTreeItem::ProjectId("b".into())],
                    }),
                ],
            }),
        ]);

        let ordered = ordered_projects(&config);
        let ids: Vec<&str> = ordered.iter().map(|p| p.id.as_str()).collect();
        // 存储序是 a,b,c —— 树序是 c,a,b
        assert_eq!(ids, ["c", "a", "b"]);
        assert!(ordered[0].group_path.is_empty());
        assert_eq!(ordered[1].group_path, ["工作"]);
        assert_eq!(ordered[2].group_path, ["工作", "子组"]);
    }

    #[test]
    fn 不在树里的项目追加到顶层且不重复() {
        let mut config = AppConfig::default();
        config.projects = vec![project("a", "A"), project("b", "B")];
        config.project_tree = Some(vec![
            ProjectTreeItem::ProjectId("b".into()),
            // 重复出现的 id 只算第一次
            ProjectTreeItem::ProjectId("b".into()),
            // 树里有、projects 里没有的 id 直接跳过
            ProjectTreeItem::ProjectId("ghost".into()),
        ]);
        let ordered = ordered_projects(&config);
        let ids: Vec<&str> = ordered.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["b", "a"]);
        assert!(ordered[1].group_path.is_empty());
    }

    #[test]
    fn 没有项目树时退回存储序() {
        let mut config = AppConfig::default();
        config.projects = vec![project("a", "A"), project("b", "B")];
        config.project_tree = None;
        let ids: Vec<String> = ordered_projects(&config).into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn 快照报项目全集但只报_ai_会话的_pane() {
        let projects = vec![
            ProjectFacet {
                id: "a".into(),
                name: "A".into(),
                path: "D:/A".into(),
                ssh_connection_id: None,
                group_path: vec!["组".into()],
            },
            // 一个 pane 都没有的项目**照样上报** —— 手机的发起弹层要能选到它
            ProjectFacet {
                id: "empty".into(),
                name: "Empty".into(),
                path: "D:/E".into(),
                ssh_connection_id: None,
                group_path: vec![],
            },
        ];
        let mut panes = HashMap::new();
        panes.insert(
            "a".to_string(),
            vec![
                pane("p1", PaneStatus::AiWorking, Some(7)),
                pane("p2", PaneStatus::AiIdle, None),
                // 裸 shell 一律不出现
                pane("p3", PaneStatus::Idle, Some(8)),
                // 从没进过 AI 会话的 error pane 也不出现
                pane("p4", PaneStatus::Error, Some(9)),
            ],
        );

        let mut ai_pane_ids = HashSet::new();
        let snapshot = build_snapshot(&projects, &panes, &mut ai_pane_ids);
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[1].panes.len(), 0);
        let ids: Vec<&str> = snapshot[0].panes.iter().map(|p| p.pane_id.as_str()).collect();
        assert_eq!(ids, ["p1", "p2"]);
        assert_eq!(snapshot[0].panes[0].status, "ai-working");
        assert_eq!(snapshot[0].panes[0].pty_id, Some(7));
        assert_eq!(snapshot[0].panes[1].pty_id, None);
        assert_eq!(snapshot[0].group_path, ["组"]);
        assert_eq!(ai_pane_ids, HashSet::from(["p1".to_string(), "p2".to_string()]));
    }

    /// 黄灯(attention)必须进快照并参与 `PartialEq`:不透传手机看不到
    /// 「等你处理」;不参与相等比较则黄灯变化会被 `last_sent` 内容去重吞掉。
    #[test]
    fn attention_进快照且变化触发不相等() {
        let projects = vec![ProjectFacet {
            id: "a".into(),
            name: "A".into(),
            path: "D:/A".into(),
            ssh_connection_id: None,
            group_path: vec![],
        }];
        let calm = pane("p1", PaneStatus::AiWorking, Some(1));
        let hot = PaneFacet {
            attention: true,
            ..calm.clone()
        };

        let mut ids = HashSet::new();
        let mut panes = HashMap::new();
        panes.insert("a".to_string(), vec![calm]);
        let before = build_snapshot(&projects, &panes, &mut ids);
        assert!(!before[0].panes[0].needs_attention);

        panes.insert("a".to_string(), vec![hot]);
        let after = build_snapshot(&projects, &panes, &mut ids);
        assert!(after[0].panes[0].needs_attention);
        assert_ne!(before, after, "黄灯变化必须打破内容去重");
    }

    /// 坑 10:`ai_pane_ids` 是跨调用状态。AI 会话崩成 error 之后 pane 必须**还在**
    /// 手机列表里 —— 做成局部变量的话用户看到的是「会话凭空没了」。
    #[test]
    fn 曾是_ai_会话的_pane_转_error_后仍在快照里() {
        let projects = vec![ProjectFacet {
            id: "a".into(),
            name: "A".into(),
            path: "D:/A".into(),
            ssh_connection_id: None,
            group_path: vec![],
        }];
        let mut ai_pane_ids = HashSet::new();

        let mut panes = HashMap::new();
        panes.insert("a".to_string(), vec![pane("p1", PaneStatus::AiWorking, Some(1))]);
        let first = build_snapshot(&projects, &panes, &mut ai_pane_ids);
        assert_eq!(first[0].panes.len(), 1);

        // 崩了
        panes.insert("a".to_string(), vec![pane("p1", PaneStatus::Error, Some(1))]);
        let second = build_snapshot(&projects, &panes, &mut ai_pane_ids);
        assert_eq!(second[0].panes.len(), 1);
        assert_eq!(second[0].panes[0].status, "error");

        // pane 被关掉(不在列表里了)→ 集合随之收缩,不会无界增长
        panes.insert("a".to_string(), Vec::new());
        let third = build_snapshot(&projects, &panes, &mut ai_pane_ids);
        assert!(third[0].panes.is_empty());
        assert!(ai_pane_ids.is_empty());
    }

    /// 内容去重的判据:同样的输入必须组出**相等**的快照,否则 150ms 去抖之后
    /// 每一轮都会发一条 SessionsDelta。
    #[test]
    fn 相同输入组出相等快照() {
        let projects = vec![ProjectFacet {
            id: "a".into(),
            name: "A".into(),
            path: "D:/A".into(),
            ssh_connection_id: None,
            group_path: vec![],
        }];
        let mut panes = HashMap::new();
        panes.insert("a".to_string(), vec![pane("p1", PaneStatus::AiIdle, Some(1))]);

        let mut ids_a = HashSet::new();
        let mut ids_b = HashSet::new();
        assert_eq!(
            build_snapshot(&projects, &panes, &mut ids_a),
            build_snapshot(&projects, &panes, &mut ids_b)
        );
        // 状态一变就不再相等
        panes.insert("a".to_string(), vec![pane("p1", PaneStatus::AiWorking, Some(1))]);
        assert_ne!(
            build_snapshot(&projects, &panes, &mut ids_a),
            {
                let mut ids = HashSet::new();
                let mut old = HashMap::new();
                old.insert("a".to_string(), vec![pane("p1", PaneStatus::AiIdle, Some(1))]);
                build_snapshot(&projects, &old, &mut ids)
            }
        );
    }
}
