//! SSH 交互浮层:把 AI 经 mini-term-ssh skill(`mt-ssh-cli`)发给服务器的命令与服务器的
//! 原样输出,显示在悬浮框里。规格:`docs/specs/ssh-activity-overlay-v1.md`。
//!
//! 入口按钮在终端组标题栏右侧的控件簇里(「⚑N → SSH → 查找 → …」,对应该组 active pane,
//! 见 `terminal_area::render_leaf_controls`);悬浮框在 pane 内容区右上角(与 Ctrl+F 查找条
//! 同一锚点,查找条开着时落在它下方)。用户真机看过后定的位置,与规格初稿不同。
//!
//! ```text
//! AI 的 Bash 工具 → mt-ssh-cli(继承 pane 环境:MINITERM_SSH_FEED=127.0.0.1:<port>/<nonce>)
//!    └─ 后台线程把 start / out / end 写成 NDJSON → 127.0.0.1:<port>(一次调用一条 TCP 连接)
//! wire::FeedServer 监听线程 → 按 nonce 认 pane → mpsc → [`start`] 起的 GPUI 任务
//!    → AppStore::terminal(pty_id) → TerminalPane 的 [`SshActivityView`]
//!    ├─ 悬浮框(pane 内容区右上角)
//!    └─ 标题栏按钮读 [`SshActivityView::button_state`](TerminalArea 画)
//! ```
//!
//! - **监听**:启动时在 `127.0.0.1:0` 绑随机端口,不依赖 hook 开关;绑不上只记日志、不注入变量,
//!   功能整体关闭(见 [`start`])。
//! - **nonce**:每个本地 PTY 一个 128 bit 随机数,起 PTY 时登记([`feed_env`],
//!   `store::panes::start_pty_with`)、PTY 退出 / 关闭 / 重启换 PTY 时注销([`forget_pty`])。
//!   远程 SSH 项目的 pane 不注入(AI 跑在远端);WSL pane 照常注入给 `wsl.exe`,但不进
//!   `WSLENV`、到不了发行版内 —— 即不支持,符合规格口径。
//! - **记录**只在内存里(每个 pane 的 [`SshActivityView`] 持有),随 pane 实体一起没;
//!   关 pane / 重启换 PTY 时由 `store::ai::dispose_terminal` 显式清一次。
//!
//! 日志纪律:本模块的 `eprintln!` 只写连接数 / 错误类别,**绝不写命令原文或输出**。

mod view;

use mt_app_core::ssh_activity::{model, wire};

use std::sync::OnceLock;

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::App;

pub use view::{ButtonState, SshActivityView};
use wire::{FEED_ENV, FeedEvent, FeedServer};

use crate::store::AppStore;

/// 进程级唯一的监听。没起来(绑定失败)时保持空,[`feed_env`] 一律返回 `None`。
static FEED: OnceLock<FeedServer> = OnceLock::new();

/// 起监听 + 起 UI 线程上的事件泵。`main` 里在 `AppStore` 装好之后、任何 PTY 起来之前调一次。
pub fn start(cx: &mut App) {
    if FEED.get().is_some() {
        return;
    }
    let (tx, mut rx) = mpsc::unbounded::<FeedEvent>();
    let server = match FeedServer::bind(move |event| {
        let _ = tx.unbounded_send(event);
    }) {
        Ok(server) => server,
        Err(err) => {
            tracing::warn!(
                "[ssh-activity] 监听 127.0.0.1 失败,SSH 交互浮层不可用: {:?}",
                err.kind()
            );
            return;
        }
    };
    if FEED.set(server).is_err() {
        return;
    }
    // 后台线程 → UI 线程:与 `main.rs` 里 AI 状态泵同一套路。每次醒来把排队的事件一次抽干,
    // 按 pane 分组交给各自的视图(同一 pane 的一批事件只 update 一次)。
    cx.spawn(async move |cx| {
        while let Some(first) = rx.next().await {
            let mut batch = vec![first];
            while let Ok(more) = rx.try_recv() {
                batch.push(more);
            }
            cx.update(|cx| dispatch(batch, cx));
        }
    })
    .detach();
}

/// 给一个即将起来的本地 PTY 登记 nonce,返回要注入的 `(变量名, 值)`。监听没起来时 `None`。
pub fn feed_env(pty_id: u32) -> Option<(String, String)> {
    let value = FEED.get()?.register(pty_id)?;
    Some((FEED_ENV.to_string(), value))
}

/// 注销一个 PTY 的 nonce(退出 / 关闭 / 重启换 PTY)。之后用它连上来的一律断开。
pub fn forget_pty(pty_id: u32) {
    if let Some(feed) = FEED.get() {
        feed.forget(pty_id);
    }
}

fn dispatch(batch: Vec<FeedEvent>, cx: &mut App) {
    let mut groups: Vec<(u32, Vec<FeedEvent>)> = Vec::new();
    for event in batch {
        let pty_id = event.pty_id();
        match groups.iter_mut().find(|(id, _)| *id == pty_id) {
            Some((_, events)) => events.push(event),
            None => groups.push((pty_id, vec![event])),
        }
    }
    let store = AppStore::global(cx);
    for (pty_id, events) in groups {
        let view = store
            .read(cx)
            .terminal(pty_id)
            .map(|pane| pane.read(cx).ssh_activity().clone());
        // pane 已经关了:事件丢弃(nonce 注销前已在路上的那几帧)
        if let Some(view) = view {
            view.update(cx, |view, cx| view.apply(events, cx));
        }
    }
}
