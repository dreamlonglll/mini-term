//! AI 感知的接线层:把 [`mt_ai::AiPerception`] 装进 GPUI 壳。
//!
//! ```text
//! 用户键入 ─┬─→ perception.observe_input(pane_id, bytes)   ← 必须在写 PTY 之前
//!           └─→ pty.write(bytes)
//! 子进程输出 ┬─→ emulator.advance(bytes)
//!            └─→ perception.observe_output(pane_id, bytes)
//! hook / monitor ─→ StatusSink ─→ mpsc channel ─→ 主线程任务 ─→ AppStore ─→ cx.notify()
//! ```
//!
//! **为什么状态要过一道 channel**:hook server 与 500ms 轮询都在后台线程上,
//! 而 gpui 的 `Entity` 只能在主线程碰。与终端重绘唤醒同一套路数(见 `pane.rs`),
//! 后台线程只管往 channel 里丢,主线程上的前台任务醒来后再改 store。

use std::path::PathBuf;
use std::sync::Arc;

use futures::channel::mpsc::{self, UnboundedSender};
use mt_ai::{AiPerception, SessionIdentity, StatusChange, StatusSink};
use parking_lot::Mutex;

/// 后台线程送上来的 AI 事件。
pub enum AiEvent {
    /// 状态变化(原 `pty-status-change`)。
    Status(StatusChange),
    /// hook 上报的会话身份(原 `pty-ai-session`)。
    Session(SessionIdentity),
}

struct ChannelSink {
    tx: UnboundedSender<AiEvent>,
}

impl StatusSink for ChannelSink {
    fn status_changed(&self, change: StatusChange) {
        let _ = self.tx.unbounded_send(AiEvent::Status(change));
    }

    fn session_identified(&self, identity: SessionIdentity) {
        let _ = self.tx.unbounded_send(AiEvent::Session(identity));
    }
}

/// AI 感知 + 它需要的两样上层信息:活 pane 列表(给 500ms 轮询)与 hook 端口
/// (注入新 PTY 的环境变量,省得 hook 二进制每次去读端口文件)。
#[derive(Clone)]
pub struct AiBridge {
    perception: AiPerception,
    /// monitor 线程每 500ms 读一次。`mt-ai` 不认识 PTY,列表只能由这里提供。
    live_panes: Arc<Mutex<Vec<u32>>>,
    /// 编排控制面的桌面状态镜像(主线程刷、控制 HTTP 线程读)。
    /// 住在这里是因为控制面本身长在 `AiPerception` 上(与 hook 共用监听)。
    orchestrator_mirror: crate::orchestrator::SharedMirror,
    data_dir: PathBuf,
}

impl AiBridge {
    /// 建桥并把接收端交出去。`hook_enabled` 为真时顺带起 hook server。
    pub fn new(hook_enabled: bool) -> (Self, mpsc::UnboundedReceiver<AiEvent>) {
        let (tx, rx) = mpsc::unbounded();
        let perception = AiPerception::new(Arc::new(ChannelSink { tx }));
        // 数据目录统一走 mt_config —— hook-server.json 与 usage.db 必须落在
        // 与装机版同一个目录下(见迁移文档的技术债清单)。
        let data_dir = crate::app_data_dir();

        let live_panes: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        // 编排控制面的宿主一开机就注入:令牌只在用户勾了「允许编排」的启动器起
        // pane 时才发,注不注入宿主与授不授权无关 —— 而没注入宿主的控制面会把
        // 一切都答成空,排查起来像「配置没生效」。
        let orchestrator_mirror: crate::orchestrator::SharedMirror = Arc::new(Mutex::new(
            crate::orchestrator::OrchestratorMirror::default(),
        ));
        perception
            .control()
            .set_host(Arc::new(crate::orchestrator::HostImpl::new(
                orchestrator_mirror.clone(),
            )));

        let bridge = Self {
            perception,
            live_panes,
            orchestrator_mirror,
            data_dir,
        };

        if hook_enabled
            && let Err(err) = bridge.perception.start_hook_server(bridge.data_dir.clone())
        {
            eprintln!("[ai] hook server 起不来: {err}");
        }

        // 输入检测那一路(无 hook 时的降级判定)不依赖 hook server,轮询恒开。
        let panes = bridge.live_panes.clone();
        bridge
            .perception
            .start_monitor(Box::new(move || panes.lock().clone()));

        (bridge, rx)
    }

    pub fn perception(&self) -> &AiPerception {
        &self.perception
    }

    /// hook server 端口;0 = 没起来。注入给子进程的 `MINITERM_HOOK_PORT`。
    pub fn hook_port(&self) -> u16 {
        self.perception.hooks().get_port()
    }

    /// 授予某个 pane 编排能力,返回要注入进它 PTY 的令牌。
    ///
    /// **唯一调用点**是「按勾了『允许编排』的启动器起会话」那条路
    /// (`store::panes`),别在别处发令牌 —— 授予入口只有一个,是 ADR 0003 的
    /// 信任根。
    pub fn grant_orchestration(&self, pane_id: u32, project_id: &str) -> String {
        self.perception.control().grant(pane_id, project_id)
    }

    /// 受编排会话的出身快照(pty 编号 → 谁起的 / 那个人还在不在)。
    ///
    /// tab 上那枚「受编排」标识读它。**别每帧调** —— 记账在控制面那把锁后面,
    /// 而那把锁 hook / 控制 HTTP 线程也在用;渲染侧按
    /// [`Self::session_origins_version`] 缓存,号没变就不必取。
    pub fn session_origins(&self) -> std::collections::HashMap<u32, mt_ai::SessionOrigin> {
        self.perception.control().origins()
    }

    /// 出身记账的版本号。**先读它、再取快照**(顺序理由见 `mt_ai` 那侧的注释)。
    pub fn session_origins_version(&self) -> u64 {
        self.perception.control().origins_version()
    }

    /// 把当前配置刷进编排控制面:镜像 + 并发上限。
    ///
    /// 控制面在 hook 那条 HTTP 线程上跑,碰不得 gpui 实体,只能读这份镜像;
    /// 而「改分组即时生效」要求它别陈旧 —— 调用点在配置落盘那一处
    /// (`store::layout::save_config_now`)与启动接线处,配置一变就跟。
    ///
    /// 上限也在这里推一次:它是**配置的一部分**,启动时得从盘上那份接上,
    /// 配置若哪天由别的路径整体换掉(重读 / 迁移)也得跟着走。改设置项那一下
    /// 另走 [`Self::set_orchestrator_session_cap`] —— 那条不等 500ms 防抖。
    pub fn refresh_orchestrator_mirror(&self, config: &mt_config::AppConfig) {
        self.orchestrator_mirror.lock().replace(config);
        self.set_orchestrator_session_cap(crate::orchestrator::resolve_session_cap(
            config.orchestrator_session_cap,
        ));
    }

    /// 把受编排会话并发上限推给控制面(工单 08 的设置项落点)。
    ///
    /// 控制面里那是个原子量,写完立刻对**后续**的 `start-session` 裁决生效,
    /// 已存活的乐手一个不动(裁决只在起会话那一行读它)。
    pub fn set_orchestrator_session_cap(&self, cap: usize) {
        self.perception.control().set_session_cap(cap);
    }

    /// 运行时开关 hook server(设置页「Hook 事件」的落点,原 `toggle_hook_server`)。
    ///
    /// **起服务器要绑端口 + 写 `hook-server.json`**,调用方一律丢
    /// `cx.background_executor()`;成功了才写配置(原版 `handleToggleHook` 的同一
    /// 顺序 —— 端口被占时配置不该记成「已开」)。
    pub fn set_hook_enabled(&self, enabled: bool) -> Result<(), String> {
        self.perception
            .set_hook_server_enabled(&self.data_dir, enabled)
    }

    /// hook server 当前状态(原 `get_hook_status`)。纯内存读,不碰盘。
    pub fn hook_status(&self) -> mt_ai::HookStatusInfo {
        mt_ai::hook_server::hook_status(self.perception.hooks())
    }

    /// 登记一个活着的 pane(新建 PTY 之后立刻调)。
    pub fn add_pane(&self, pane_id: u32) {
        let mut panes = self.live_panes.lock();
        if !panes.contains(&pane_id) {
            panes.push(pane_id);
        }
    }

    /// 这个 pane 还活着吗。
    ///
    /// 读的就是 500ms 轮询那份活 pane 名册([`Self::add_pane`] 登记、
    /// [`Self::remove_pane`] 注销),所以**后台线程随手可问** —— 编排控制面
    /// 判「乐手还占不占名额」要在 HTTP 线程上一次问好几个 pane,跳主线程太贵。
    pub fn is_pane_live(&self, pane_id: u32) -> bool {
        self.live_panes.lock().contains(&pane_id)
    }

    /// 注销 pane:轮询列表与 `mt-ai` 内部的旁路状态一起清干净。
    pub fn remove_pane(&self, pane_id: u32) {
        self.live_panes.lock().retain(|id| *id != pane_id);
        self.perception.pane_closed(pane_id);
    }

    /// 退出时收摊:停 hook server 并删掉端口文件。
    ///
    /// 不做这一步的话,`hook-server.json` 会留着一个已经死掉的端口 —— 下一次
    /// 起的 AI 会话若没继承 `MINITERM_HOOK_PORT`,就会照着这个文件往空气里汇报。
    /// (装机版与 GPUI 壳同时在跑时两者会互抢这个文件,dev 期已知,见交付说明。)
    pub fn shutdown(&self) {
        // 没起过就别动那个文件 —— 它可能是另一个壳(装机版)的
        if self.hook_port() > 0 {
            self.perception.stop_hook_server(&self.data_dir);
        }
    }
}
