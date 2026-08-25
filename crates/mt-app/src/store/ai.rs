//! AI 感知相关的 `AppStore` 方法:AI 任务标记(⚑)、AI 事件落地、通知 / 待办、
//! 会话分支自记账。
//!
//! 从 `store.rs` 原样搬来的几段(`// === AI 任务标记 ===` / `// === AI 事件 ===` /
//! `// === 通知 / 待办 ===` / `// === 会话分支自记账 ===`),段注释随代码走,
//! 逻辑一行未改。终端回收(`dispose_terminal` 一族)跟着标记段一起来 ——
//! 它做的正是「清 AI 感知痕迹」,原文件里也紧挨着标记段。

use gpui::Context;

use crate::ai::AiEvent;
use crate::markers::{self, AiMarker, MarkerBatch};
use crate::notify::{NotifyPrefs, PaneRef, StatusTransition};
use crate::tree::{AiSessionRef, PaneStatus};

use super::pure::{
    collect_ai_projects, compute_title_bar_light, push_lineage_edge, resolve_fork_edge, AiProjects,
    DoneScope, PendingFork, TitleBarLight,
};
use super::{AppStore, PendingAlert};

impl AppStore {
    // === AI 任务标记(⚑)===

    /// 某个 pane 的标记列表(没有就是空)。对应 `store.ts:1225` 的 `getMarkersForPty`。
    ///
    /// ⚠️ 这是**内部全量**,含正文还没验明正身的候选条目。给用户看的一律走
    /// [`Self::visible_markers_for_pty`] —— 见 [`crate::markers::AiMarker::confirmed`]。
    pub fn markers_for_pty(&self, pty_id: u32) -> &[AiMarker] {
        self.markers_by_pty
            .get(&pty_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// 能给用户看的那些(`⚑ N` 的计数与下拉列表**共用这一个口**)。
    pub fn visible_markers_for_pty(&self, pty_id: u32) -> Vec<AiMarker> {
        markers::visible(self.markers_for_pty(pty_id))
            .cloned()
            .collect()
    }

    /// 落一批标记(pane 在 [`crate::pane::TerminalPane::write`] 里当场取好锚点后发来)。
    ///
    /// 节奏照抄 `useAiSubmitMarker.ts:20-23`:**追加之后立刻剪一遍枝**,
    /// 不在渲染路径上剪(见 [`crate::markers`] 的模块注释)。
    // 拆分前是私有方法;调用点在 `store::panes` 的 PTY 事件订阅里,升到 `pub(super)`。
    pub(super) fn add_markers(&mut self, pty_id: u32, batch: MarkerBatch, cx: &mut Context<Self>) {
        if batch.submits.is_empty() {
            return;
        }
        // 先把旧条目收拾一遍(锚点行已经不是原来那行的降级或删、挂着的补锚),再追加
        // 新的 —— 「⚑ N」不能一直挂着已经跳不对的条目。
        // 新条目要在这之后 push:它的指纹刚取,自己校验自己没有意义。
        self.refresh_markers(pty_id, cx);
        let list = self.markers_by_pty.entry(pty_id).or_default();
        for submit in batch.submits {
            markers::push_marker(list, pty_id, submit, batch.anchor);
        }
        markers::prune(list, batch.history, batch.max_scrollback);
        // 过滤后为空则连键一起删(`store.ts:1219` 的同一处置)
        let empty = list.is_empty();
        if empty {
            self.markers_by_pty.remove(&pty_id);
            self.marker_cursor.remove(&pty_id);
        }
        cx.notify();
    }

    /// 收拾一遍某个 pane 的标记:**失效的处置 + 挂着的补锚**,返回「列表变过没有」。
    ///
    /// 三件事按这个顺序,少一步或者换个顺序都不对:
    ///
    /// 1. [`markers::prune`] —— scrollback 装满,整份作废(算术锚点从此不可信);
    /// 2. [`markers::prune_stale`] —— 校验已定锚的那些:锚点行已经不是原来那行的,
    ///    键入的降级回挂起、猜来的删(分流理由见 [`crate::markers`] 模块注释);
    /// 3. [`markers::relocate_pending`] —— 给挂着的(含上一步刚降级的)补锚。
    ///    **必须排在校验之后**:刚补上的指纹是从同一份 grid 读的,当轮自校必过、
    ///    白跑;而这个顺序让降级的条目当轮就能回扫找回,不用灰一拍等下一次。
    ///
    /// 跑的时机:新增标记时、跳转前、下拉打开时 —— **一律不在渲染路径上**
    /// (见 [`crate::markers`] 模块注释)。
    fn refresh_markers(&mut self, pty_id: u32, cx: &mut Context<Self>) -> bool {
        let Some(entity) = self.terminals.get(&pty_id).cloned() else {
            return false;
        };
        let pane = entity.read(cx);
        let (history, max) = pane.scrollback_state();
        // alt screen 期间读的是备用 grid,校验会把整份标记误杀、回扫也扫不到主屏
        let probe_ok = pane.can_probe_lines();
        let (bottom, viewport) = pane.scan_bounds();
        let Some(list) = self.markers_by_pty.get_mut(&pty_id) else {
            return false;
        };
        let mut changed = markers::prune(list, history, max);
        if probe_ok {
            changed |= markers::prune_stale(list, |anchor| pane.line_fingerprint(anchor));
            changed |= markers::relocate_pending(list, bottom, viewport, |row| pane.line_text(row));
        }
        let empty = list.is_empty();
        if empty {
            self.markers_by_pty.remove(&pty_id);
            self.marker_cursor.remove(&pty_id);
        }
        changed
    }

    /// 打开「⚑」下拉之前收拾一遍 —— 用户要看的这一眼必须是最新的:AI 刚把排队的
    /// 那条处理掉的话,这次补锚就能让它从「灰的、点不动」变回可跳。
    pub fn refresh_markers_for_pty(&mut self, pty_id: u32, cx: &mut Context<Self>) {
        if self.refresh_markers(pty_id, cx) {
            cx.notify();
        }
    }

    /// 整份丢掉(`store.ts:1205-1211` 的 `clearMarkersForPty`)。游标一并清 ——
    /// 原版那份游标从不清理,这里顺手修掉。
    fn clear_markers_for_pty(&mut self, pty_id: u32) {
        self.markers_by_pty.remove(&pty_id);
        self.marker_cursor.remove(&pty_id);
    }

    /// 跳到某一条标记:滚到视口顶部 + 闪 300ms,并把游标推到它身上。
    ///
    /// 浮层点击与 Ctrl+Shift+↑/↓ **走的是同一条路**(原版 `useMarkerHotkeys.ts:56`
    /// 与 `MarkerList.tsx:36-39` 调的都是 `scrollToMarker`),**不关任何东西**。
    ///
    /// 返回「这一下真的跳了没有」:跳不动的三种情形(pane 没了 / 标记还挂着没定位 /
    /// pane 正在 alt screen 里)都是 `false`,调用方据此**不推游标、不关浮层**。
    pub fn jump_to_marker(&mut self, pty_id: u32, marker_id: &str, cx: &mut Context<Self>) -> bool {
        let Some(entity) = self.terminals.get(&pty_id).cloned() else {
            return false;
        };
        // 跳之前先收拾一遍:挂着的趁机补锚(点的可能正是刚被 AI 处理掉的那条),
        // 锚点已经不可信的宁可什么都不做,也不能跳到错的行上 —— 见
        // [`Self::refresh_markers`] 与 [`crate::markers`] 模块注释
        if self.refresh_markers(pty_id, cx) {
            cx.notify();
        }
        let Some(anchor) = self
            .markers_for_pty(pty_id)
            .iter()
            .find(|m| m.id == marker_id)
            // 还挂着的跳不了:那条消息还没上屏,没有目标行可跳。**静默不动**,
            // 与「列表空 / 到头」同一个处置(`useMarkerHotkeys.ts:39`、`:50`)
            .and_then(|m| m.anchor.settled())
        else {
            return false;
        };
        // 跳不动(pane 正在 alt screen 里)就不推游标 —— 连按方向键不该空走格子
        if entity.update(cx, |pane, cx| pane.scroll_to_marker(anchor, cx)) {
            self.marker_cursor.insert(pty_id, marker_id.to_string());
            return true;
        }
        false
    }

    /// Ctrl+Shift+↑ / ↓。`dir = -1` 上一条、`+1` 下一条,**非环形**。
    ///
    /// 目标 pane 的解析与其它全局动作同口径:焦点 pane → 布局里第一个激活 pane
    /// ([`Self::active_pane_id`],原版是 `focusedPtyIdFromDom()` → `resolveActivePane`)。
    /// 列表空 / 到头都是静默不动,不弹任何提示(`useMarkerHotkeys.ts:39`、`:50`)。
    pub fn step_marker(&mut self, dir: i32, cx: &mut Context<Self>) {
        let Some(project_id) = self.active_project_id.clone() else {
            return;
        };
        let Some(pty_id) = self
            .active_pane_id(&project_id)
            .and_then(|pane_id| {
                self.project_states
                    .get(&project_id)
                    .and_then(|s| s.pane(&pane_id))
                    .and_then(|p| p.pty_id)
            })
        else {
            return;
        };
        // 先收拾一遍再挑目标:否则刚被 AI 处理掉的那条还挂着「跳不了」的旧状态,
        // 这一下会白白跳过它
        self.refresh_markers_for_pty(pty_id, cx);
        let mut cursor = self
            .marker_cursor
            .get(&pty_id)
            .and_then(|id| self.markers_for_pty(pty_id).iter().position(|m| &m.id == id));
        let len = self.markers_for_pty(pty_id).len();
        // 还挂着的条目跳不动,连按时要**跨过去**继续找下一条 —— 停在它身上的话
        // 游标不会推进,再按一次还是它,方向键就卡死了
        let target = loop {
            let Some(next) = markers::next_index(cursor, len, dir) else {
                return;
            };
            match self.markers_for_pty(pty_id).get(next) {
                Some(marker) if marker.anchor.settled().is_some() => break marker.id.clone(),
                Some(_) => cursor = Some(next),
                None => return,
            }
        };
        self.jump_to_marker(pty_id, &target, cx);
    }

    /// 回收一个终端:kill 子进程 + 清 AI 感知痕迹 + 摘掉视图与订阅。
    // 拆分前是私有方法;调用点散在 `projects` / `panes` / `ssh` / `layout`,升到 `pub(super)`。
    pub(super) fn dispose_terminal(&mut self, pty_id: u32, cx: &mut Context<Self>) {
        // 对应 `terminalCache.ts:546` 的 `aiPtyIds.delete(ptyId)` ——
        // 不摘的话新 PTY 复用同一个编号时会被误当成 AI pane(嗅探静默失效)
        crate::git_watch::forget_pane(pty_id);
        // 关 pane / 关整组 / 项目移除三条路的唯一汇合点,标记与游标在这里一并回收
        // (原版分散在 `setProjectLayout` 的 ptyId 集合比对、`disposePane`、
        // `removeProject` 三处,漏一处就是「pty id 复用后接手了上一任的标记」)
        self.clear_markers_for_pty(pty_id);
        // 分支登记同理:留着会让复用同一编号的新 PTY 认领上一任的 fork 登记
        self.clear_pending_fork(pty_id);
        // 退出登记同理:留着会让复用同一编号的新 PTY 一开就顶着「已断开」遮罩
        self.exited_ptys.remove(&pty_id);
        if let Some(entity) = self.terminals.remove(&pty_id) {
            // 组合中关 pane:先把预编辑收掉,免得 IME 还挂在一个即将消失的
            // 输入宿主上(marked range 不收回,下一次按键会被 IME 永久劫持)
            entity.update(cx, |pane, cx| {
                pane.clear_preedit(cx);
                pane.shutdown();
            });
        }
        self.pane_subs.remove(&pty_id);
    }

    // 拆分前是私有方法;调用点在 `store::panes::split_pane_with_cwd`,升到 `pub(super)`。
    pub(super) fn pty_in_any_layout(&self, pty_id: u32) -> bool {
        self.project_states
            .values()
            .flat_map(|s| s.layouts())
            .any(|l| l.pane_by_pty(pty_id).is_some())
    }

    /// 子进程退出:pane 落 `error`。
    ///
    /// 旧版就是这个语义(`pty-exit` → `updatePaneStatusByPty('error')`):pane 不
    /// 自动关闭,用户主动 `exit` 与异常断开不做区分,画面留在原地可回看。
    // 拆分前是私有方法;调用点在 `store::panes` 的 PTY 事件订阅里,升到 `pub(super)`。
    pub(super) fn on_pty_exit(&mut self, pty_id: u32, code: Option<u32>, cx: &mut Context<Self>) {
        if let Some(code) = code
            && code != 0
        {
            eprintln!("[store] pane {pty_id} 子进程退出,退出码 {code}");
        }
        // fork 命令没能起起会话就退了 —— 这条登记不该等到下一个进程头上
        // (原版把 `clearPendingFork` 挂在 `pty-exit` 监听里,同一时机)
        self.clear_pending_fork(pty_id);
        // 原版 `App.tsx:359` 的 `markPtyExited`:与状态落 error 同一时机
        self.exited_ptys.insert(pty_id);
        let mut touched: Option<String> = None;
        for (pid, state) in self.project_states.iter_mut() {
            let hit = state
                .layouts_mut()
                .any(|layout| layout.update_status_by_pty(pty_id, PaneStatus::Error, false, None));
            if hit {
                state.status = state.highest_status();
                touched = Some(pid.clone());
                break;
            }
        }
        if touched.is_some() {
            cx.notify();
        }
    }

    // === AI 事件 ===

    /// 后台线程送上来的 AI 事件(见 `ai.rs` 的接线图)。
    ///
    /// 返回值是要执行的提醒动作(提示音 / 任务栏闪烁 / toast),由调用方在持有
    /// `Window` 的地方兑现 —— 见 [`PendingAlert`]。
    pub fn apply_ai_event(
        &mut self,
        event: AiEvent,
        cx: &mut Context<Self>,
    ) -> Option<PendingAlert> {
        match event {
            AiEvent::Status(change) => {
                let status = PaneStatus::from_str(&change.status)?;
                // Git 面板的 pty-output 嗅探要跳过 AI pane 的输出。判据与
                // `App.tsx:284` 的 `markAiPty(ptyId, status === 'ai-working' ||
                // status === 'ai-idle')` 一字不差(见 `git_watch` 模块注释)。
                crate::git_watch::set_ai_pane(
                    change.pty_id,
                    matches!(status, PaneStatus::AiWorking | PaneStatus::AiIdle),
                );
                // attention 与状态解耦:codex 的 PermissionRequest 状态是 ai-working
                // 但同样要点黄灯。判定按事件名,与旧版 isAttentionCause 同一张表。
                let attention = change
                    .cause
                    .as_deref()
                    .map(mt_ai::is_attention_cause)
                    .unwrap_or(false);

                let mut owner: Option<String> = None;
                let mut pane_id = String::new();
                let mut old_status = PaneStatus::Idle;
                let mut old_attention = false;
                'projects: for (pid, state) in self.project_states.iter_mut() {
                    let mut hit = false;
                    // 跨全部面板找:后台面板里的 AI 状态一样要亮灯
                    for layout in state.layouts_mut() {
                        let Some(pane) = layout.pane_by_pty(change.pty_id) else {
                            continue;
                        };
                        old_status = pane.status;
                        old_attention = pane.attention;
                        pane_id = pane.id.clone();
                        layout.update_status_by_pty(
                            change.pty_id,
                            status,
                            attention,
                            change.agent.as_deref(),
                        );
                        hit = true;
                        break;
                    }
                    if hit {
                        state.status = state.highest_status();
                        owner = Some(pid.clone());
                        break 'projects;
                    }
                }
                let owner = owner?;
                let project_active = self.active_project_id.as_deref() == Some(owner.as_str());

                let plan = self.done.apply(
                    &StatusTransition {
                        pane_id: &pane_id,
                        old_status,
                        new_status: status,
                        old_attention,
                        cause: change.cause.as_deref(),
                        window_focused: self.window_focused,
                        project_active,
                    },
                    &self.notify_prefs(),
                );
                if plan.mark_needs_attention
                    && let Some(state) = self.project_states.get_mut(&owner)
                {
                    state.needs_attention = true;
                }
                cx.notify();

                if plan.is_empty() {
                    return None;
                }
                Some(PendingAlert {
                    plan,
                    project_name: self
                        .project(&owner)
                        .map(|p| p.name.clone())
                        .unwrap_or_else(|| owner.clone()),
                    project_id: owner,
                    sound_path: self.config.ai_completion_sound_path.clone(),
                })
            }
            AiEvent::Session(identity) => {
                let mut owner: Option<String> = None;
                let session = AiSessionRef {
                    agent: identity.agent.clone(),
                    session_id: identity.session_id.clone(),
                    cwd: identity.cwd.clone(),
                };
                for (pid, state) in self.project_states.iter_mut() {
                    if let Some(pane) = state.pane_by_pty_mut(identity.pty_id) {
                        pane.ai_session = Some(session.clone());
                        owner = Some(pid.clone());
                        break;
                    }
                }
                // 会话身份随布局落盘 —— 重启后据此续接
                if let Some(owner) = owner {
                    self.save_project_layout_soon(&owner, cx);
                    cx.notify();
                }
                // 分支自记账:这个 pane 是 fork 出来的话,新身份到手即落边。
                // **必须在这里**而不是等 pane 变 ai-working —— 身份只上报一次,
                // 错过就再没有第二次机会把 child→parent 记下来。
                self.consume_pending_fork(identity.pty_id, &session, cx);
                None
            }
        }
    }

    fn notify_prefs(&self) -> NotifyPrefs {
        NotifyPrefs {
            sound: self.config.ai_completion_sound,
            flash: self.config.ai_completion_taskbar_flash,
            popup: self.config.ai_completion_popup,
            attention_notify: self.config.ai_attention_notify,
        }
    }

    // === 通知 / 待办 ===

    /// 主窗口聚焦状态(旧版 `setWindowFocused`)。聚焦时完成的任务不计未读。
    ///
    /// **聚焦即已读**:旧版 `App.tsx` 的 `onFocusChanged` 里 `focused` 一到就
    /// `clearUnreadDone()` —— 人已经回到窗口前了,绿灯必须熄,否则它会一直亮到
    /// 下次手动点掉为止。少了这一句「未读完成」就成了只增不减的计数。
    pub fn set_window_focused(&mut self, focused: bool, cx: &mut Context<Self>) {
        if self.window_focused == focused {
            return;
        }
        self.window_focused = focused;
        if focused {
            self.done.clear_unread();
        }
        cx.notify();
    }

    /// 主窗口是否聚焦。托盘的闪烁策略要看它(聚焦不闪),而托盘的推送发生在
    /// store 观察者里、手上没有 `Window`,只能从这里读。
    pub fn window_focused(&self) -> bool {
        self.window_focused
    }

    /// 未读完成数(旧版托盘绿灯的计数,这里给壳内徽章用)。
    pub fn unread_done_count(&self) -> usize {
        self.done.unread_count()
    }

    /// 全局 AI 状态(边条上那颗徽标点)。逐条对照 `ActivityBar.tsx` 的 `globalStatus`:
    /// 取所有项目里优先级最高的一档,**`error` 先压成 `idle`** —— 某个 shell
    /// `exit 1` 不该让整条边栏亮红点,那会盖住真正在跑的 AI。
    pub fn global_ai_status(&self) -> PaneStatus {
        let mut highest = PaneStatus::Idle;
        for state in self.project_states.values() {
            let status = match state.status {
                PaneStatus::Error => PaneStatus::Idle,
                other => other,
            };
            if status.priority() > highest.priority() {
                highest = status;
            }
        }
        highest
    }

    /// 全部(或某个项目的)pane 的一份只读快照。
    ///
    /// 三处聚合(挑待办 / 按项目聚合 / 标题栏状态灯)都从这一份出发,免得各写
    /// 一遍「跳过还没有 layout 的项目」这类边角。
    ///
    /// ⚠️ **顺序不确定**:`project_states` 是 `HashMap`,遍历顺序每次都可能不同。
    /// 消费方要么与顺序无关(取最高档),要么自己排序(见 [`collect_ai_projects`])。
    fn pane_refs(&self, only_project: Option<&str>) -> Vec<PaneRef<'_>> {
        self.project_states
            .iter()
            .filter(|(pid, _)| only_project.is_none_or(|only| only == pid.as_str()))
            .flat_map(|(pid, state)| {
                state.all_panes().into_iter().map(move |p| PaneRef {
                    project_id: pid.as_str(),
                    pane_id: p.id.as_str(),
                    status: p.status,
                    attention: p.attention,
                })
            })
            .collect()
    }

    /// 「进入 AI agent 的项目」按项目聚合(`store.ts::collectAiProjects` 等价物)。
    ///
    /// 标题栏的项目切换胶囊与托盘菜单(T 批)共用这一份,唯一的差别是 done 判据
    /// 从哪来 —— 见 [`DoneScope`]。
    pub fn ai_projects(&self, scope: DoneScope) -> AiProjects {
        self.ai_projects_of(&self.pane_refs(None), scope)
    }

    /// [`Self::ai_projects`] 的「pane 快照已经在手上」版本。
    ///
    /// 拆出来只为一件事:标题栏那一帧要把**同一份**快照喂给两个聚合器
    /// (见 [`Self::title_bar_snapshot`]),不该为此扫两遍全部 pane。
    fn ai_projects_of(&self, panes: &[PaneRef<'_>], scope: DoneScope) -> AiProjects {
        let projects = self.config.projects.as_slice();
        match scope {
            DoneScope::All => {
                let order = self.done.order();
                collect_ai_projects(panes.iter().copied(), projects, |id| order.contains_key(id))
            }
            DoneScope::Unread => {
                collect_ai_projects(panes.iter().copied(), projects, |id| self.done.is_unread(id))
            }
        }
    }

    /// 标题栏一帧要的两件事:那颗全局状态灯(`TitleBar.tsx::computeLight`)+
    /// 项目切换胶囊的下拉列表。
    ///
    /// ⚠️ 状态灯与边条徽标的 [`AppStore::global_ai_status`] **口径不同**:边条把
    /// `error` 压成 `idle`(一个 `exit 1` 的 shell 不该盖住真在跑的 AI),标题栏灯
    /// 反过来把 `error` 列为最高一档,另外还多一个 `done` 档。两处不可互相复用。
    ///
    /// # 为什么合成一个方法
    ///
    /// 拆成两个 getter 就要各扫一遍 `pane_refs(None)`(全项目 flat_map + collect
    /// 一个 Vec),而标题栏**每帧都要**:它挂了 `window_control_area`,套不了
    /// view 级缓存(理由见 `main.rs::cached_panel` 与标题栏挂载点的注释),
    /// 所以那两遍是真的每帧各来一次。
    ///
    /// 两条结果的 done 判据都取 [`DoneScope::All`](`aiDoneOrder`,不看窗口焦点),
    /// 与标题栏那两处消费点原本的口径逐字一致 —— 托盘用的是
    /// [`DoneScope::Unread`],**不能**并进来。
    ///
    /// # 为什么不做脏标记缓存
    ///
    /// 评估后判为不划算:两条结果的输入横跨 `project_states`(30 处 `&mut`
    /// 触点)、`config.projects`(22 处)与 `done` 账本(11 处),没有任何一个
    /// 收口函数覆盖得住全部失效点。漏一处的后果是**状态灯从此不更新**,
    /// 比多扫一遍 pane 严重得多。合成一次遍历省下的,正好是能确定省下的那一半。
    pub fn title_bar_snapshot(&self) -> (TitleBarLight, AiProjects) {
        let panes = self.pane_refs(None);
        let order = self.done.order();
        let light = compute_title_bar_light(panes.iter().copied(), |id| order.contains_key(id));
        (light, self.ai_projects_of(&panes, DoneScope::All))
    }

    pub fn is_pane_unread_done(&self, pane_id: &str) -> bool {
        self.done.is_unread(pane_id)
    }

    pub fn clear_unread_done(&mut self, cx: &mut Context<Self>) {
        self.done.clear_unread();
        cx.notify();
    }

    /// 「下一件该我做的事」在哪个 pane。`only_project` 限定项目内挑。
    pub fn next_attention_target(&self, only_project: Option<&str>) -> Option<(String, String)> {
        crate::notify::pick_attention_target(self.pane_refs(only_project), self.done.order())
    }

    /// 按 `session_id` 跨**全部项目**找「在跑」的 pane。对应
    /// `src/utils/sessionJump.ts::findLiveSessionPane`。
    ///
    /// 三个条件缺一不可:① 会话身份匹配;② PTY 活着;③ 状态在
    /// `{AiWorking, AiIdle}` 里。第三条不能省 —— `ai_session` 在 AI 退出后为
    /// **续接语义刻意保留**(status 落回 idle),只看身份会把「claude 已退出的
    /// shell」当成在跑,点过去对着一个死会话。
    ///
    /// # `exitedPtyIds` 的等价物
    ///
    /// 原版第二条查的是 `!exitedPtyIds.has(pane.ptyId)`,而 mt-app 没有这张表
    /// (审计第 73 行记着这条缺失)。PTY 退出时 store 会把 pane 打成
    /// [`PaneStatus::Error`],而 `Error` 本就不在第三条的白名单里 —— 两条合起来
    /// **实际等价**,不必为此新增一份状态。
    pub fn find_live_session_pane(&self, session_id: &str) -> Option<(String, String, PaneStatus)> {
        for (project_id, state) in self.project_states.iter() {
            for pane in state.all_panes() {
                let matches = pane
                    .ai_session
                    .as_ref()
                    .is_some_and(|s| s.session_id == session_id);
                if matches
                    && pane.pty_id.is_some()
                    && matches!(pane.status, PaneStatus::AiWorking | PaneStatus::AiIdle)
                {
                    return Some((project_id.clone(), pane.id.clone(), pane.status));
                }
            }
        }
        None
    }

    /// 把恢复出来的会话身份**当场**写回 pane(对应 `setPaneAiSessionByPty`)。
    ///
    /// 不能干等 hook:codex resume 不会重新上报 SessionStart,新 pane 会永远
    /// 拿不到身份,右键的分支入口随之消失(claude 会上报同 id 幂等覆盖)。
    /// 身份随布局持久化,重启自动续接顺带受益。
    pub fn set_pane_ai_session(
        &mut self,
        project_id: &str,
        pane_id: &str,
        session: AiSessionRef,
        cx: &mut Context<Self>,
    ) {
        let mut pty_id = None;
        if let Some(state) = self.project_states.get_mut(project_id)
            && let Some(pane) = state.pane_mut(pane_id)
        {
            pane.ai_session = Some(session.clone());
            // 身份是自己写进去的,不是「待续接」——别让下次启动再敲一遍命令
            pane.resume_pending = false;
            pty_id = pane.pty_id;
            self.save_project_layout_soon(project_id, cx);
            cx.notify();
        }
        // 与 hook 上报那条路同一个消费点(原版两条都走 `setPaneAiSessionByPty`)。
        // 走到这里的多半是 resume/跳转,没有登记 → 空操作。
        if let Some(pty_id) = pty_id {
            self.consume_pending_fork(pty_id, &session, cx);
        }
    }

    // === 会话分支自记账 ===
    //
    // 设计: `docs/plans/2026-08-14-session-branch-tree-design.md`。
    // mini-term 自己发起的 fork 在新 pane 的 PTY 上登记「等新会话身份」,hook 上报
    // 新 id 时落成 child→parent 边写进 `config.session_lineage`。磁盘扫描
    // (`scan_session_lineage`)是权威且合并时优先,这里只兜两件事:文件尚未落盘的
    // 窗口期,以及 **Claude 的 CLI fork 压根不写磁盘指针**(`forkedFrom` 只有
    // `/branch` 路径写)——那种边只存在于自记账。

    /// 登记一次 fork:`pty_id` 上跑起来的下一个会话身份是 `parent_session_id` 的孩子。
    pub fn register_pending_fork(&mut self, pty_id: u32, agent: &str, parent_session_id: &str) {
        self.pending_forks.insert(
            pty_id,
            PendingFork {
                agent: agent.to_ascii_lowercase(),
                parent_session_id: parent_session_id.to_string(),
            },
        );
    }

    /// 丢掉一个 PTY 的登记(子进程退出 / 终端回收)。
    ///
    /// 不清的话:fork 命令没起成会话,这条登记会一直挂着,等 pty id 被复用之后
    /// 认领**下一个进程**的会话身份,凭空造出一条假分支边(原版 `clearPendingFork`
    /// 挂在 `pty-exit` 上是同一条理由)。
    pub fn clear_pending_fork(&mut self, pty_id: u32) {
        self.pending_forks.remove(&pty_id);
    }

    /// 消费**一次性**的 fork 登记。判据是纯函数 [`resolve_fork_edge`];
    /// 无论落不落边,登记都当场作废(agent 不符 = fork 失败后起了别家)。
    fn consume_pending_fork(
        &mut self,
        pty_id: u32,
        session: &AiSessionRef,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.pending_forks.remove(&pty_id) else {
            return;
        };
        let Some(edge) = resolve_fork_edge(&pending, session) else {
            return;
        };
        if push_lineage_edge(&mut self.config.session_lineage, edge) {
            self.save_config_soon(cx);
        }
    }
}
