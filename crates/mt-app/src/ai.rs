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
        let bridge = Self {
            perception,
            live_panes,
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

    /// 登记一个活着的 pane(新建 PTY 之后立刻调)。
    pub fn add_pane(&self, pane_id: u32) {
        let mut panes = self.live_panes.lock();
        if !panes.contains(&pane_id) {
            panes.push(pane_id);
        }
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
