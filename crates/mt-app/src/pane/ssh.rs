//! pane 的内置 SSH 胶水:链路(远程项目 pane 的基底 / 右键「SSH 连接」的接管层)的前台任务与
//! 回调、接管的开始与收场、过闸的输出回调。纯逻辑(路由表、连接中状态机、终端内行编辑、等首屏、
//! 输出闸)在 mt-app-core 的 `ssh_takeover` / `shell_link` / `line_input` / `output_gate`,
//! 这里只剩 GPUI 胶水。总体设计见父模块注释「后端」。

use mt_app_core::shell_link::{Feed, FeedAction, PromptEvent, SessionEnd, ShellLink, ShellPort};
use mt_app_core::ssh_takeover::{BaseExit, FirstScreen};
use mt_remote::{
    PromptReply, ShellEnd, ShellExit, ShellOpenError, ShellOpenErrorKind, ShellSession,
    TerminalPrompt,
};
use mt_terminal::TakeoverSnapshot;
use mt_terminal::alacritty_terminal::vte::ansi::Handler as _;

use super::*;

/// mt-ssh 的 `ShellSession` 在 mt-app-core 一侧的形状。两个方法都只是入队,不阻塞主线程。
pub(super) struct RemoteShell(pub(super) ShellSession);

impl ShellPort for RemoteShell {
    fn write(&self, bytes: &[u8]) -> Result<(), String> {
        self.0.write(bytes)
    }

    fn resize(&self, rows: u16, cols: u16) -> Result<bool, String> {
        // `ShellSession` 是 (cols, rows),PTY 一侧与 ShellPort 是 (rows, cols)
        self.0.resize(cols, rows)
    }
}

/// 一条内置 SSH 链路连同它的输出闸与前台任务(基底或接管层)。
pub(super) struct LinkSlot {
    /// 代数,见 [`TerminalPane::next_link_id`]。
    pub(super) id: u64,
    pub(super) link: ShellLink<RemoteShell, PromptReply>,
    /// 链路的输出闸:建连提示画完之前 `Hold`,就绪后 `Pass`,结束 / 关闭后 `Drop`。
    pub(super) gate: Arc<OutputGate>,
    /// 提示 → 建连结果 → 远端退出 的前台任务([`spawn_link`])。**drop = 中止建连**
    /// (`mt_remote::OpeningShell` 随之丢弃);任务自己回调里收场时先 `detach`,不在运行中的任务里
    /// 丢自己的句柄。接管等首屏期间还没起。
    pub(super) task: Option<Task<()>>,
    /// `user@host:port`(「Connecting to …」与取消那一行)。
    pub(super) target: String,
    /// `Connection to <host> closed.` 里的主机名。
    pub(super) host: String,
    /// 已经被 `shutdown` 收掉了。
    pub(super) closed: bool,
}

impl LinkSlot {
    pub(super) fn new(
        id: u64,
        conn: &SshConnection,
        rows: u16,
        cols: u16,
        gate: Arc<OutputGate>,
    ) -> Self {
        Self {
            id,
            link: ShellLink::new(rows, cols),
            gate,
            task: None,
            target: mt_remote::terminal_target(conn),
            host: mt_remote::terminal_host(conn),
            closed: false,
        }
    }

    /// 自己的前台任务正在回调里收场:句柄 detach(它马上自己跑完),不在运行中的任务里丢它。
    fn release_task(&mut self) {
        if let Some(task) = self.task.take() {
            task.detach();
        }
    }

    /// 收掉:中止建连 / 关会话(`ShellSession` 的 Drop = close)、闸落成丢弃。
    pub(super) fn close(&mut self) {
        self.task = None;
        drop(self.link.end());
        self.gate.set(GateState::Drop);
        self.closed = true;
    }
}

impl Drop for LinkSlot {
    fn drop(&mut self) {
        // 还在暂扣的输出线程不能一直等下去;会话随 `link` 一起丢弃 = close
        self.gate.set(GateState::Drop);
    }
}

/// 链路在 pane 里的角色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LinkRole {
    Base,
    Overlay,
}

/// 右键「SSH 连接」(内置客户端)的接管层,见模块注释「后端」。
pub(super) struct Takeover {
    /// 「SSH 已连接」灯上的名字(连接名,没起名就是主机)。
    pub(super) conn_name: String,
    pub(super) conn: SshConnection,
    pub(super) stage: TakeoverStage,
    /// 叠加层的链路。等首屏时就已经在了(预输入 / Ctrl+C 要有去处),建连任务接管开始时才起。
    pub(super) slot: LinkSlot,
    /// 接管前的标题(shell 报的 OSC 标题),结束时还原。
    pub(super) saved_title: Option<String>,
    /// 接管期间基底死了:接管结束、屏幕还原之后再报。
    pub(super) base_exit: Option<DeferredExit>,
}

pub(super) enum TakeoverStage {
    /// 等基底首屏画完,还没拍快照。计时器掉了就不会再看,必须存着。
    Waiting { timer: Option<Task<()>> },
    /// 已拍快照(收场时交还 `end_takeover`)。快照有一整屏的格子,装箱免得撑大整个枚举。
    Active {
        snapshot: Option<Box<TakeoverSnapshot>>,
    },
}

/// 挂起的基底退出。
pub(super) enum DeferredExit {
    /// 本地 PTY 的子进程退了(退出码)。
    Pty(Option<u32>),
    /// 内置 SSH 基底收场:要画进终端的那一行 + 退出码。
    Shell { notice: String, code: Option<u32> },
}

/// 接管怎么收场的。
enum TakeoverEnd {
    /// 远端退出 / 连接断开:收场那一行;断线时带原因(另给 toast)。
    Ended {
        notice: String,
        lost: Option<String>,
    },
    /// 建连失败(`ShellOpenError.message`)。
    Failed(String),
    /// 用户在终端里取消了建连。
    Cancelled(String),
    /// 右键「断开 SSH」:收场那一行(在跑时是 `Connection to <host> closed.`)。
    Disconnected(String),
}

impl TerminalPane {
    pub(super) fn write_overlay(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        let Some(takeover) = self.takeover.as_mut() else {
            return;
        };
        let waiting = matches!(takeover.stage, TakeoverStage::Waiting { .. });
        let feed = takeover.slot.link.feed(bytes);
        if waiting {
            // 还在等基底首屏:屏幕仍归基底,不回显;敲的字进预输入,Ctrl+C = 这次连接作罢
            if feed.action == FeedAction::Cancel {
                self.drop_takeover();
                self.toast_later(
                    ToastKind::Info,
                    t("terminal", "sshConnectCancelled").into(),
                    cx,
                );
            }
            return;
        }
        self.apply_feed(LinkRole::Overlay, bytes, feed, cx);
    }

    /// 把一次写入在链路上的结果落地:回显画进终端、读完的一行应答给 SSH 层、取消 / 转发。
    pub(super) fn apply_feed(
        &mut self,
        role: LinkRole,
        bytes: &[u8],
        feed: Feed<PromptReply>,
        cx: &mut Context<Self>,
    ) {
        if !feed.show.is_empty() {
            self.show(&feed.show);
        }
        if let Some(answer) = feed.answer {
            // 明文整块交给应答通道,这边不留副本(取走后 `Zeroizing` 里只剩空串)
            let line = answer.line.map(|mut line| std::mem::take(&mut *line));
            answer.reply.answer(line);
        }
        match feed.action {
            FeedAction::Consumed | FeedAction::Dropped => {}
            FeedAction::Forward => {
                self.observe_user_write(bytes, cx);
                self.write_link(role, bytes);
            }
            FeedAction::Cancel => self.cancel_link(role, cx),
        }
    }

    /// 写进一条在跑的链路(不过旁路;调用方已经过了)。
    fn write_link(&mut self, role: LinkRole, bytes: &[u8]) {
        let pty_id = self.pty_id;
        if let Some(slot) = self.slot_mut(role)
            && let Err(err) = slot.link.write(bytes)
        {
            tracing::warn!("[pane {pty_id}] 写 SSH 会话失败: {err}");
        }
    }

    fn slot_mut(&mut self, role: LinkRole) -> Option<&mut LinkSlot> {
        match role {
            LinkRole::Base => match &mut self.base {
                PaneBase::Shell(slot) => Some(slot),
                PaneBase::Pty(_) => None,
            },
            LinkRole::Overlay => self.takeover.as_mut().map(|takeover| &mut takeover.slot),
        }
    }

    /// 代数为 `id` 的链路是哪一条(接管层只在已拍快照之后才有前台任务)。旧任务回调 → `None`。
    fn role_of(&self, id: u64) -> Option<LinkRole> {
        if let PaneBase::Shell(slot) = &self.base
            && slot.id == id
            && !slot.closed
        {
            return Some(LinkRole::Base);
        }
        self.takeover
            .as_ref()
            .filter(|takeover| {
                takeover.slot.id == id && matches!(takeover.stage, TakeoverStage::Active { .. })
            })
            .map(|_| LinkRole::Overlay)
    }

    /// 主线程自己往终端里画字(建连提示、行编辑回显、收场那一行),再叫醒唤醒循环排重绘。
    ///
    /// 不会与输出线程交错:基底接管期间关着闸,链路建连期间暂扣着,收场前先关闸。
    fn show(&mut self, bytes: &[u8]) {
        self.emulator.advance(bytes);
        let _ = self.signal.unbounded_send(PaneSignal::Output);
    }

    /// 同 [`Self::show`],光标不在行首时先换行(收场那一行要单独成行)。
    fn show_line(&mut self, text: &str) {
        let mid_line = self
            .emulator
            .with_term(|term| term.grid().cursor.point.column.0 > 0);
        if mid_line {
            self.show(b"\r\n");
        }
        self.show(text.as_bytes());
    }
}

// ─── 内置 SSH:链路回调与右键「SSH 连接」接管(见模块注释「后端」) ───────────────

impl TerminalPane {
    /// 接管阶段(路由表的输入),见 [`ssh_takeover::route`]。
    pub(super) fn takeover_state(&self) -> Option<TakeoverState> {
        self.takeover.as_ref().map(|takeover| match takeover.stage {
            TakeoverStage::Waiting { .. } => TakeoverState::Waiting,
            TakeoverStage::Active { .. } if takeover.slot.link.phase() == LinkPhase::Running => {
                TakeoverState::Running
            }
            TakeoverStage::Active { .. } => TakeoverState::Connecting,
        })
    }

    /// 在接管中(含还在等首屏)。右键「SSH 连接」据此提示「先断开」,右键菜单据此出「断开 SSH」。
    pub fn in_ssh_takeover(&self) -> bool {
        self.takeover.is_some()
    }

    /// 内置 SSH 基底还在建连(可能正等用户在终端里答密码)。这时敲进去的字归它的行编辑器,
    /// 回退那条路的 `ssh …\r` 不能往里写 —— 会被当成口令交出去。
    pub fn base_connecting(&self) -> bool {
        matches!(&self.base, PaneBase::Shell(slot) if slot.link.phase() == LinkPhase::Connecting)
    }

    fn take_link_id(&mut self) -> u64 {
        let id = self.next_link_id;
        self.next_link_id += 1;
        id
    }

    /// 输出回调的配料。基底带上 quick_ssh 旁路(回退的 `ssh …` 被敲进内置 SSH 的远程 pane 时
    /// 也认得出),接管层不带 —— 叠加会话的「已连接」按它自己的生命周期报。
    fn output_sink(&self, with_quick_ssh: bool) -> OutputSink {
        OutputSink {
            pty_id: self.pty_id,
            emulator: self.emulator.clone(),
            ai: self.ai.clone(),
            signal: self.signal.clone(),
            quick_ssh: with_quick_ssh
                .then(|| (self.quick_ssh.clone(), self.quick_ssh_watching.clone())),
        }
    }

    /// 基底退出了(本地 PTY 子进程退出 / 内置 SSH 基底收场),按接管阶段路由:没在接管就当场报,
    /// 等首屏时接管作罢再报,接管中挂到接管结束、屏幕还原之后。返回是否当场报了。
    pub(super) fn report_base_exit(&mut self, exit: DeferredExit, cx: &mut Context<Self>) -> bool {
        match ssh_takeover::route(self.takeover_state()).base_exit {
            BaseExit::Report => {}
            BaseExit::CancelTakeover => self.drop_takeover(),
            BaseExit::Defer => {
                if let Some(takeover) = self.takeover.as_mut() {
                    takeover.base_exit.get_or_insert(exit);
                }
                return false;
            }
        }
        self.apply_base_exit(exit, cx);
        true
    }

    fn apply_base_exit(&mut self, exit: DeferredExit, cx: &mut Context<Self>) {
        let code = match exit {
            DeferredExit::Pty(code) => code,
            DeferredExit::Shell { notice, code } => {
                self.show_line(&notice);
                code
            }
        };
        self.emit_exit(code, cx);
    }

    /// 丢掉接管层,不还原屏幕、不提示:还没拍快照(等首屏时作罢),或终端马上就没了。
    /// 等首屏的计时器、叠加层的建连任务与会话随之取消。
    pub(super) fn drop_takeover(&mut self) {
        if let Some(mut takeover) = self.takeover.take() {
            takeover.slot.close();
        }
    }

    /// 等首屏的计时器自己回调里要改阶段:先把它的句柄 detach(它马上就跑完),
    /// 不在运行中的任务里丢自己的句柄。
    fn release_waiting_timer(&mut self) {
        if let Some(Takeover {
            stage: TakeoverStage::Waiting { timer },
            ..
        }) = self.takeover.as_mut()
            && let Some(timer) = timer.take()
        {
            timer.detach();
        }
    }

    /// 没在读行时按了 Ctrl+C:中止建连(丢掉前台任务 = 丢掉 `OpeningShell`,连接随之断开),
    /// 呈现「已取消」。
    fn cancel_link(&mut self, role: LinkRole, cx: &mut Context<Self>) {
        let Some(slot) = self.slot_mut(role) else {
            return;
        };
        slot.task = None;
        drop(slot.link.end());
        slot.gate.set(GateState::Drop);
        let message = cancelled_message(&slot.target);
        match role {
            LinkRole::Base => self.fail_base_link(&message, cx),
            LinkRole::Overlay => self.finish_takeover(TakeoverEnd::Cancelled(message), cx),
        }
    }

    /// 内置 SSH 基底没连上(失败 / 取消):醒目的一行画进终端,落成「退出」——
    /// 远程 pane 的重连遮罩照常出现(与 ssh.exe 报错退出 255 同一个样子)。
    fn fail_base_link(&mut self, message: &str, cx: &mut Context<Self>) {
        self.show_line(&shell_link::error_notice(message));
        self.settle_spawn_waiters(false);
        self.emit_exit(None, cx);
    }

    /// 链路的前台任务送来一条建连提示(旧链路的提示随之丢弃:读行的应答句柄 drop = 取消)。
    fn on_link_prompt(&mut self, id: u64, prompt: TerminalPrompt) {
        let Some(role) = self.role_of(id) else {
            return;
        };
        let event = match prompt {
            TerminalPrompt::Notice(text) => PromptEvent::Notice(text),
            TerminalPrompt::ReadLine {
                prompt,
                echo,
                reply,
            } => PromptEvent::ReadLine {
                prompt,
                echo,
                reply,
            },
        };
        let Some(slot) = self.slot_mut(role) else {
            return;
        };
        let show = slot.link.prompt(event);
        if !show.is_empty() {
            self.show(&show);
        }
    }

    /// 建连有了结果(这时提示都已画完)。返回 `true` = 会话在跑,前台任务接着等它退出。
    fn on_link_opened(
        &mut self,
        id: u64,
        result: Result<ShellSession, ShellOpenError>,
        cx: &mut Context<Self>,
    ) -> bool {
        // 旧链路:会话随 `result` 丢弃 = 关闭
        let Some(role) = self.role_of(id) else {
            return false;
        };
        let error = match result {
            Ok(session) => return self.link_opened(role, session, cx),
            Err(error) => error,
        };
        tracing::warn!(
            "[pane {}] SSH 建连没成({:?}): {}",
            self.pty_id,
            error.kind,
            error.message
        );
        if let Some(slot) = self.slot_mut(role) {
            slot.release_task();
            drop(slot.link.end());
            slot.gate.set(GateState::Drop);
        }
        match role {
            LinkRole::Base => self.fail_base_link(&error.message, cx),
            LinkRole::Overlay => {
                let end = if user_cancelled(error.kind) {
                    TakeoverEnd::Cancelled(error.message)
                } else {
                    TakeoverEnd::Failed(error.message)
                };
                self.finish_takeover(end, cx);
            }
        }
        false
    }

    /// 建连成功:落成在跑 → 补 resize → 放行远端输出 → 冲刷预输入(这时才过旁路)。
    fn link_opened(
        &mut self,
        role: LinkRole,
        session: ShellSession,
        cx: &mut Context<Self>,
    ) -> bool {
        let pty_id = self.pty_id;
        let Some(slot) = self.slot_mut(role) else {
            return false;
        };
        let opened = match slot.link.opened(RemoteShell(session)) {
            Ok(opened) => opened,
            // 已经取消 / 关闭了(不该发生):会话随之丢弃 = 关闭
            Err(session) => {
                drop(session);
                return false;
            }
        };
        // 提示都画完了,远端输出可以进了
        slot.gate.set(GateState::Pass);
        match opened.resized {
            Some(Ok(true)) => self.ai.perception().note_resize(pty_id),
            Some(Err(err)) => tracing::warn!("[pane {pty_id}] SSH resize 失败: {err}"),
            Some(Ok(false)) | None => {}
        }
        if !opened.typeahead.is_empty() {
            self.observe_user_write(&opened.typeahead, cx);
            self.write_link(role, &opened.typeahead);
        }
        match role {
            LinkRole::Base => {
                self.base_ready_at = Some(Instant::now());
                self.settle_spawn_waiters(true);
            }
            LinkRole::Overlay => {
                if let Some(takeover) = &self.takeover {
                    cx.emit(PaneEvent::SshConnected(Some(takeover.conn_name.clone())));
                }
            }
        }
        cx.notify();
        true
    }

    /// 远端退出 / 连接断开(mt-ssh 保证之前的输出都已交付)。
    fn on_link_exit(&mut self, id: u64, exit: ShellExit, cx: &mut Context<Self>) {
        let Some(role) = self.role_of(id) else {
            return;
        };
        let Some(slot) = self.slot_mut(role) else {
            return;
        };
        slot.release_task();
        drop(slot.link.end());
        slot.gate.set(GateState::Drop);
        let end = session_end(&exit);
        let notice = shell_link::end_notice(&slot.host, &end);
        let code = exit_code(&exit);
        match role {
            LinkRole::Base => {
                self.report_base_exit(DeferredExit::Shell { notice, code }, cx);
            }
            LinkRole::Overlay => {
                let lost = match end {
                    SessionEnd::Lost(reason) => Some(reason),
                    SessionEnd::Closed => None,
                };
                self.finish_takeover(TakeoverEnd::Ended { notice, lost }, cx);
            }
        }
    }

    /// 右键「SSH 连接」(连接没勾系统 OpenSSH):在这个终端的基底上叠一条内置 SSH 会话,见模块
    /// 注释「后端」。基底首屏还没画完就先等(见 [`ssh_takeover::first_screen_step`]),这期间
    /// 敲的字进预输入、Ctrl+C 作罢。
    ///
    /// 接不了时原样交回准入结论(已经在接管中 / 基底在建连或已经死了),由调用方提示。
    pub fn request_takeover(
        &mut self,
        conn: SshConnection,
        cx: &mut Context<Self>,
    ) -> Result<(), Admission> {
        match ssh_takeover::admit(self.base_state(), self.takeover.is_some()) {
            Admission::Wait => {}
            refused => return Err(refused),
        }
        let size = self.emulator.term_size();
        let id = self.take_link_id();
        let gate = Arc::new(OutputGate::new(GateState::Hold));
        let slot = LinkSlot::new(
            id,
            &conn,
            size.screen_lines as u16,
            size.columns as u16,
            gate,
        );
        let conn_name = if conn.name.trim().is_empty() {
            conn.host.clone()
        } else {
            conn.name.clone()
        };
        self.takeover = Some(Takeover {
            conn_name,
            conn,
            stage: TakeoverStage::Waiting { timer: None },
            slot,
            saved_title: None,
            base_exit: None,
        });
        self.poll_first_screen(cx);
        Ok(())
    }

    /// 等首屏:能接就接,不能就过一会儿再看(计时器回调也走这里)。
    fn poll_first_screen(&mut self, cx: &mut Context<Self>) {
        if !matches!(
            self.takeover,
            Some(Takeover {
                stage: TakeoverStage::Waiting { .. },
                ..
            })
        ) {
            return;
        }
        let prompt_drawn = self
            .emulator
            .with_term(|term| term.grid().cursor.point.column.0 > 0);
        let step = ssh_takeover::first_screen_step(
            Instant::now(),
            self.base_state(),
            self.base_ready_at,
            self.last_output_at,
            prompt_drawn,
        );
        match step {
            FirstScreen::Ready => {
                self.release_waiting_timer();
                self.begin_takeover(cx);
            }
            FirstScreen::Abandon => {
                self.release_waiting_timer();
                self.drop_takeover();
            }
            FirstScreen::Wait(delay) => {
                let timer = cx.spawn(async move |pane, cx| {
                    cx.background_executor().timer(delay).await;
                    let _ =
                        pane.update(cx, |pane: &mut TerminalPane, cx| pane.poll_first_screen(cx));
                });
                if let Some(Takeover {
                    stage: TakeoverStage::Waiting { timer: slot },
                    ..
                }) = self.takeover.as_mut()
                    && let Some(old) = slot.replace(timer)
                {
                    // 旧的多半就是正在回调的这一个:detach,不在运行中的任务里丢它
                    old.detach();
                }
            }
        }
    }

    /// 接管开始:基底关闸 → 收掉基底留下的应答 / 标题 → 拍快照 → 换行 + 「正在连接」→ 起建连。
    fn begin_takeover(&mut self, cx: &mut Context<Self>) {
        if self.takeover.is_none() {
            return;
        }
        // 返回时没有在途的基底输出(它在闸锁里推进)。C 批的约定:先停转发,再 begin_takeover
        self.base_gate.set(GateState::Drop);
        // 此刻路由仍是「等首屏」:基底留下的应答照给基底,标题记下来
        self.drain_term_events(cx);
        let saved_title = self.osc_title.clone();
        let snapshot = self.emulator.begin_takeover();
        // 本地 shell 那半行输入不该跟远端敲的拼在一起认 AI 命令
        self.ai.perception().pane_closed(self.pty_id);
        let sink = self.output_sink(false);
        let Some(takeover) = self.takeover.as_mut() else {
            return;
        };
        takeover.saved_title = saved_title;
        takeover.stage = TakeoverStage::Active {
            snapshot: Some(Box::new(snapshot)),
        };
        let size = self.emulator.term_size();
        let (rows, cols) = takeover
            .slot
            .link
            .request_size()
            .unwrap_or((size.screen_lines as u16, size.columns as u16));
        let request = mt_remote::login_shell_request(rows, cols);
        let notice = shell_link::connecting_notice(&takeover.slot.target);
        let (id, conn, gate) = (
            takeover.slot.id,
            takeover.conn.clone(),
            takeover.slot.gate.clone(),
        );
        // 光标还停在本地提示符后面:先换行(C 批的约定)
        self.show(b"\r\n");
        self.show(notice.as_bytes());
        let task = spawn_link(id, conn, request, sink.callback(gate), cx);
        if let Some(takeover) = self.takeover.as_mut() {
            takeover.slot.task = Some(task);
        }
        self.notify_view(cx);
        cx.notify();
    }

    /// 接管收场:关叠加层的闸 → 收掉远端留下的应答 / 标题 → 收场那一行进远端屏 → 还原屏幕与标题
    /// → 基底开闸 → 「SSH 已连接」归还 → 补报挂起的基底退出 → 提示。
    fn finish_takeover(&mut self, end: TakeoverEnd, cx: &mut Context<Self>) {
        let Some(takeover) = self.takeover.as_mut() else {
            return;
        };
        takeover.slot.gate.set(GateState::Drop);
        // 叠加层此时已结束,路由把应答丢掉 —— 绝不能落到基底上
        self.drain_term_events(cx);
        let Some(mut takeover) = self.takeover.take() else {
            return;
        };
        let snapshot = match &mut takeover.stage {
            TakeoverStage::Active { snapshot } => snapshot.take(),
            TakeoverStage::Waiting { .. } => None,
        };
        let (line, toast) = match end {
            TakeoverEnd::Ended { notice, lost } => (
                notice,
                lost.map(|reason| {
                    let message = tr!("terminal", "sshConnectionLost", detail = reason);
                    (ToastKind::Error, message)
                }),
            ),
            TakeoverEnd::Failed(message) => {
                let toast = tr!("terminal", "sshConnectFailed", detail = message.as_str());
                (
                    shell_link::error_notice(&message),
                    Some((ToastKind::Error, toast)),
                )
            }
            TakeoverEnd::Cancelled(message) => (
                shell_link::error_notice(&message),
                Some((
                    ToastKind::Info,
                    t("terminal", "sshConnectCancelled").to_string(),
                )),
            ),
            TakeoverEnd::Disconnected(notice) => (notice, None),
        };
        if let Some(snapshot) = snapshot {
            // 收场那一行留在远端内容末尾,随它一起进回看
            self.show_line(&line);
            self.emulator.end_takeover(*snapshot);
            // 标题还原成接管前的:经 Term 设,之后 `set_options` 重发的也是它
            let saved = takeover.saved_title.take();
            self.emulator.with_term_mut(|term| term.set_title(saved));
            self.drain_term_events(cx);
            self.base_gate.set(GateState::Pass);
            // 远端会话里认出来的 AI 状态随会话一起作废,monitor 下一轮落回本地 shell 的样子
            self.ai.perception().pane_closed(self.pty_id);
        }
        // 「SSH 已连接」那颗灯归还回退那套(多半没连 = 清掉)
        let quick = self.quick_ssh.lock().connected_name().map(str::to_string);
        cx.emit(PaneEvent::SshConnected(quick));
        let base_exit = takeover.base_exit.take();
        // 叠加会话(还开着的话)随之关闭
        drop(takeover);
        if let Some(exit) = base_exit {
            self.apply_base_exit(exit, cx);
        }
        if let Some((kind, message)) = toast {
            self.toast_later(kind, message, cx);
        }
        self.notify_view(cx);
        cx.notify();
    }

    /// 右键「断开 SSH」:还在等首屏就作罢;在建连就中止;在跑就关会话
    /// (`Connection to <host> closed.`,与 OpenSSH `~.` 同一句)。
    pub fn disconnect_takeover(&mut self, cx: &mut Context<Self>) {
        let Some(takeover) = self.takeover.as_mut() else {
            return;
        };
        if matches!(takeover.stage, TakeoverStage::Waiting { .. }) {
            self.drop_takeover();
            return;
        }
        let notice = if takeover.slot.link.phase() == LinkPhase::Running {
            shell_link::end_notice(&takeover.slot.host, &SessionEnd::Closed)
        } else {
            shell_link::error_notice(&cancelled_message(&takeover.slot.target))
        };
        // 先关会话(ShellSession 的 Drop = close:之后不交付输出、不回调 on_exit),再中止前台任务
        takeover.slot.gate.set(GateState::Drop);
        drop(takeover.slot.link.end());
        takeover.slot.task = None;
        self.finish_takeover(TakeoverEnd::Disconnected(notice), cx);
    }

    /// 应用退出:关掉内置 SSH 会话(基底与接管层),不画、不报。关闭序列入队即返回,
    /// 进程随后退出时来不及发完的由系统关 socket 兜底 —— 不留连接。
    pub fn close_ssh_sessions(&mut self) {
        if let Some(mut takeover) = self.takeover.take() {
            takeover.slot.close();
        }
        if let PaneBase::Shell(slot) = &mut self.base {
            slot.close();
        }
    }

    /// 推一条 toast —— 挪到本轮 effect 之后:这条路可能跑在 `store.update` 里
    /// (`write_to_pane` → `write` → Ctrl+C 取消),那时去读 store 会撞上同一实体的租借。
    fn toast_later(&self, kind: ToastKind, message: String, cx: &mut Context<Self>) {
        let pty_id = self.pty_id;
        cx.defer(move |cx| toast_for_pty(pty_id, kind, message, cx));
    }
}

/// 建连没成是不是用户自己取消的。呈现口径:取消 → 「已取消」(接管时 toast 是信息级);其余 ——
/// 连接配置本身有问题(`Config`:主机为空、没有用户名……)、主机密钥被拒、认证失败、网络、超时、
/// 开 channel 被拒 —— 一律按失败:终端里一行醒目的 `message`,远程 pane 落成退出(重连遮罩照常,
/// 改好配置再点重连),接管还原屏幕后补一条错误 toast。写成穷举:mt-ssh 再加分类时这里要跟着定。
fn user_cancelled(kind: ShellOpenErrorKind) -> bool {
    match kind {
        ShellOpenErrorKind::Cancelled => true,
        ShellOpenErrorKind::Config
        | ShellOpenErrorKind::HostKey
        | ShellOpenErrorKind::Auth
        | ShellOpenErrorKind::Network
        | ShellOpenErrorKind::Timeout
        | ShellOpenErrorKind::Channel => false,
    }
}

/// 取消建连那一行的原话(与 mt-ssh 在读行里被取消时报的同一句)。
fn cancelled_message(target: &str) -> String {
    format!("ssh connection to {target} was cancelled")
}

/// mt-ssh 的收场 → 画进终端的那一行用哪种说法。
fn session_end(exit: &ShellExit) -> SessionEnd {
    match &exit.reason {
        ShellEnd::Exited | ShellEnd::Closed => SessionEnd::Closed,
        ShellEnd::ConnectionLost(reason) => SessionEnd::Lost(reason.clone()),
    }
}

/// 远程 pane 退出码:远端程序报了退出码才有(0 = 正常收工,灯按灰画);断线 / 只关了 channel
/// 都按「取不到」算 —— 与 ssh.exe 断线退出 255 一样亮红灯。
fn exit_code(exit: &ShellExit) -> Option<u32> {
    match exit.reason {
        ShellEnd::Exited => exit.code,
        ShellEnd::Closed | ShellEnd::ConnectionLost(_) => None,
    }
}

/// 给 pane 所在项目推一条 toast(项目取不到就挂空项目)。
pub(super) fn toast_for_pty(pty_id: u32, kind: ToastKind, message: String, cx: &mut App) {
    let (project_id, project_name) = {
        let store = AppStore::global(cx).read(cx);
        let project = store
            .pane_of_pty(pty_id)
            .and_then(|(project_id, _)| store.project(&project_id));
        (
            project.map(|p| p.id.clone()).unwrap_or_default(),
            project.map(|p| p.name.clone()).unwrap_or_default(),
        )
    };
    toast::push_message(kind, project_id, project_name, message, cx);
}

/// 输出回调的配料(本地 PTY 的 reader 线程 / 内置 SSH 的输出线程上跑)。
#[derive(Clone)]
pub(super) struct OutputSink {
    pub(super) pty_id: u32,
    pub(super) emulator: Arc<TerminalEmulator>,
    pub(super) ai: AiBridge,
    pub(super) signal: mpsc::UnboundedSender<PaneSignal>,
    /// 右键「SSH 连接」回退那套的已连接状态机旁路(接管层不带)。
    pub(super) quick_ssh: Option<(Arc<Mutex<QuickSshTracker>>, Arc<AtomicBool>)>,
}

impl OutputSink {
    /// 过闸的输出回调:放行时整段旁路在闸锁里跑(见 [`mt_app_core::output_gate`]),
    /// 关闸时一个字节都不进 —— 不进 emulator、不进 AI 感知、不进 git 观察。
    pub(super) fn callback(&self, gate: Arc<OutputGate>) -> impl FnMut(&[u8]) + Send + 'static {
        let sink = self.clone();
        move |bytes: &[u8]| {
            gate.run(|| sink.deliver(bytes));
        }
    }

    fn deliver(&self, bytes: &[u8]) {
        let pty_id = self.pty_id;
        // 直接推进状态机,没有 IPC、没有批缓冲、没有序列化。
        self.emulator.advance(bytes);
        // OSC 7501 程序状态上报（如 agy 的权限确认 / 提问等待黄灯）：无新事件时单次原子读即返回空。
        for status in self.emulator.drain_program_statuses() {
            self.ai
                .perception()
                .observe_osc_status(pty_id, map_osc_status(status));
        }
        // AI 感知的输出旁路(命令 echo 回扫 + 输出活跃度)
        self.ai.perception().observe_output(pty_id, bytes);
        // Git 面板的输出旁路(外部跑了 git 命令 → 刷新变更与仓库元信息)。
        // **这条线程上不跑任何模式匹配**:总闸关着时只有一次原子读,
        // 开着时也只是把尾部字节塞进有界环形缓冲,5 条口径在主线程节拍上跑。
        // 详见 `git_watch` 模块注释(后续 Y 批的 git 着色与本条共用)。
        crate::git::watch::observe_output(pty_id, bytes);
        // 右键「SSH 连接」的已连接状态机旁路:普通终端只有一次原子读(零开销),
        // 已进入原生 Connected 态后 `on_output` 同样直接短路。
        if let Some((quick_ssh, watching)) = &self.quick_ssh
            && watching.load(Ordering::Relaxed)
        {
            let outcome = quick_ssh.lock().on_output(bytes, Instant::now());
            if let QuickSshOutcome::Changed(next) = outcome {
                if next.is_none() {
                    watching.store(false, Ordering::Relaxed);
                }
                let _ = self.signal.unbounded_send(PaneSignal::SshConnected(next));
            }
        }
        let _ = self.signal.unbounded_send(PaneSignal::Output);
    }
}

fn map_osc_status(status: mt_terminal::ProgramStatus) -> mt_ai::OscStatus {
    let state = match status.state {
        mt_terminal::ProgramState::Idle => mt_ai::OscProgramState::Idle,
        mt_terminal::ProgramState::Working => mt_ai::OscProgramState::Working,
        mt_terminal::ProgramState::Blocked => mt_ai::OscProgramState::Blocked,
        mt_terminal::ProgramState::Done => mt_ai::OscProgramState::Done,
        mt_terminal::ProgramState::Error => mt_ai::OscProgramState::Error,
        mt_terminal::ProgramState::Clear => mt_ai::OscProgramState::Clear,
    };
    let kind = status.kind.map(|k| match k {
        mt_terminal::BlockedKind::Permission => mt_ai::OscBlockedKind::Permission,
        mt_terminal::BlockedKind::Question => mt_ai::OscBlockedKind::Question,
        mt_terminal::BlockedKind::Auth => mt_ai::OscBlockedKind::Auth,
        mt_terminal::BlockedKind::Other => mt_ai::OscBlockedKind::Other,
    });
    mt_ai::OscStatus {
        state,
        kind,
        app: status.app,
        id: status.id,
        msg: status.msg,
    }
}

/// 起一条内置 SSH 链路的前台任务。建连当场在 mt-remote 的共享运行时上开始,这边**严格按序**
/// 回主线程:
///
/// 1. 收提示画进终端,直到提示流结束(`open_shell` 返回时 mt-ssh 摘掉发送端 = 提示都送完了);
/// 2. 建连结果([`TerminalPane::on_link_opened`],成功则放行远端输出 —— 所以提示一定先于远端
///    输出上屏);
/// 3. 远端退出([`TerminalPane::on_link_exit`];宿主自己关的不回调,退出通道随之断开,任务收摊)。
///
/// 任务被 drop(Ctrl+C / 断开 / 关 pane)= `OpeningShell` 随之丢弃 = 中止建连。
pub(super) fn spawn_link<F>(
    id: u64,
    conn: SshConnection,
    request: mt_remote::ShellRequest,
    on_output: F,
    cx: &mut Context<TerminalPane>,
) -> Task<()>
where
    F: FnMut(&[u8]) + Send + 'static,
{
    let (exit_tx, exit_rx) = oneshot::channel::<ShellExit>();
    let (mut prompts, opening) =
        mt_remote::open_terminal_shell(conn, request, on_output, move |exit| {
            let _ = exit_tx.send(exit);
        });
    cx.spawn(async move |pane, cx| {
        while let Some(prompt) = prompts.recv().await {
            let shown = pane.update(cx, |pane: &mut TerminalPane, _cx| {
                pane.on_link_prompt(id, prompt)
            });
            if shown.is_err() {
                return;
            }
        }
        let result = opening.await;
        let running = pane
            .update(cx, |pane: &mut TerminalPane, cx| {
                pane.on_link_opened(id, result, cx)
            })
            .unwrap_or(false);
        if !running {
            return;
        }
        let Ok(exit) = exit_rx.await else {
            return;
        };
        let _ = pane.update(cx, |pane: &mut TerminalPane, cx| {
            pane.on_link_exit(id, exit, cx)
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内置 SSH 收场:远端报了退出码才有码(0 = 正常收工),只关 channel / 断线都按取不到算;
    /// 画进终端的那一行断线时用原因。
    #[test]
    fn 内置_ssh_收场的退出码与说法() {
        let exit = |code, reason| ShellExit {
            code,
            signal: None,
            reason,
        };
        assert_eq!(exit_code(&exit(Some(0), ShellEnd::Exited)), Some(0));
        assert_eq!(exit_code(&exit(Some(3), ShellEnd::Exited)), Some(3));
        assert_eq!(exit_code(&exit(None, ShellEnd::Closed)), None);
        assert_eq!(
            exit_code(&exit(Some(0), ShellEnd::ConnectionLost("x".into()))),
            None
        );
        assert_eq!(
            session_end(&exit(Some(0), ShellEnd::Exited)),
            SessionEnd::Closed
        );
        assert_eq!(
            session_end(&exit(None, ShellEnd::Closed)),
            SessionEnd::Closed
        );
        assert_eq!(
            session_end(&exit(None, ShellEnd::ConnectionLost("Timeout".into()))),
            SessionEnd::Lost("Timeout".into())
        );
        assert_eq!(
            cancelled_message("u@h:22"),
            "ssh connection to u@h:22 was cancelled"
        );
    }
}
