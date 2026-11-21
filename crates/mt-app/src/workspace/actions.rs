use super::*;

impl Workspace {
    pub(super) fn on_new_terminal(
        &mut self,
        _: &NewTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_terminal_page(window, cx) {
            return;
        }
        let Some(project_id) = self.store.read(cx).active_project_id.clone() else {
            return;
        };
        let anchor = self.target_pane(cx).map(|(_, pane)| pane);
        self.store.update(cx, |store, cx| {
            store.new_terminal(&project_id, None, anchor, window, cx);
        });
    }

    /// Ctrl+Shift+W = 关**整组**(原版 `closePane` 调的是 `closeLeaf`),
    /// 不是关当前这一个 tab —— 单个 tab 走 tab 上的 ×。
    ///
    /// 走 [`pane_actions::close_leaf_of_pane`] 而不是直接调 store:关闭前要盘点
    /// 组里活着的 AI 会话并确认(三条关闭路径共用同一个入口)。
    ///
    /// 停在页签上时关的是**当前页签**(与 × 同一条路,脏的照常先问)。那条路也刷新连发计时:
    /// 按住不放把页签关光、落回终端页之后,后续连发照样被 [`pane_actions::swallows_close_hotkey`]
    /// 吞掉,不会顺势把终端一组组关掉。
    pub(super) fn on_close_pane(
        &mut self,
        _: &ClosePane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = std::time::Instant::now();
        match self.hotkey_page(window, cx) {
            HotkeyPage::Yield => return,
            HotkeyPage::Terminal => {}
            HotkeyPage::Document => {
                self.last_close_hotkey = Some(now);
                self.workbench_area
                    .update(cx, |area, cx| area.close_active_tab(window, cx));
                return;
            }
        }
        let last = self.last_close_hotkey.replace(now);
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        // 目标组全部已退出时偏好开着也免确认,连发闸要跟着管(见 `swallows_close_hotkey`)
        let store = self.store.read(cx);
        let dead = pane_actions::leaf_of_pane_is_dead(store, &project_id, &pane_id);
        if pane_actions::swallows_close_hotkey(store.config(), dead, last, now) {
            return;
        }
        pane_actions::close_leaf_of_pane(self.store.clone(), project_id, pane_id, window, cx);
    }

    pub(super) fn on_next_pane(
        &mut self,
        _: &NextPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_pane(1, window, cx);
    }

    pub(super) fn on_prev_pane(
        &mut self,
        _: &PrevPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_pane(-1, window, cx);
    }

    /// 终端页:叶内换 tab;停在页签上:在「终端页 + 各页签」之间循环
    /// (`WorkbenchArea::cycle_page`)。
    pub(super) fn cycle_pane(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        match self.hotkey_page(window, cx) {
            HotkeyPage::Yield => return,
            HotkeyPage::Terminal => {}
            HotkeyPage::Document => {
                self.workbench_area
                    .update(cx, |area, cx| area.cycle_page(delta, window, cx));
                return;
            }
        }
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        self.store.update(cx, |store, cx| {
            store.cycle_pane(&project_id, &pane_id, delta, window, cx)
        });
    }

    pub(super) fn on_select_pane(
        &mut self,
        action: &SelectPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if yields_to_overlay(window, cx) || !self.terminal_page_active(cx) {
            return;
        }
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        let index = action.0;
        self.store.update(cx, |store, cx| {
            store.select_pane_by_index(&project_id, &pane_id, index, window, cx)
        });
    }

    pub(super) fn on_split_right(
        &mut self,
        _: &SplitRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_terminal_page(window, cx) {
            return;
        }
        self.split(SplitDirection::Horizontal, window, cx);
    }

    pub(super) fn on_split_down(
        &mut self,
        _: &SplitDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_terminal_page(window, cx) {
            return;
        }
        self.split(SplitDirection::Vertical, window, cx);
    }

    /// Ctrl+`:停在页签上 → 回终端页并把焦点还给终端;停在终端页 → 回最近停过的那个页签
    /// (没有页签就什么都不做)。
    pub(super) fn on_toggle_workbench_page(
        &mut self,
        _: &ToggleWorkbenchPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.hotkey_page(window, cx) {
            HotkeyPage::Yield => {}
            HotkeyPage::Terminal => {
                self.workbench_area
                    .update(cx, |area, cx| area.return_to_last_tab(window, cx));
            }
            HotkeyPage::Document => {
                self.workbench_area
                    .update(cx, |area, cx| area.activate_terminal(window, cx));
            }
        }
    }

    pub(super) fn split(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        self.store.update(cx, |store, cx| {
            store.split_pane(&project_id, &pane_id, direction, window, cx);
        });
    }

    pub(super) fn on_toggle_middle(
        &mut self,
        _: &ToggleMiddleColumn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if yields_to_overlay(window, cx) {
            return;
        }
        self.store
            .update(cx, |store, cx| store.toggle_middle_column(cx));
    }

    /// F2。**这是全仓唯一一条 F2 绑定的唯一处理器** —— 项目列表与文件树的行级
    /// 按键(Enter/Space、Delete、F2)只有 F2 与全局键位表撞车,所以它不在行上
    /// 另绑一条 action,而是从这里按「有没有列表行拿着焦点」分流。
    ///
    /// 两处各绑一条 F2 的话,gpui 会按 dispatch 深度选行上那条(比 workspace 深),
    /// 于是终端里按 F2 与列表里按 F2 变成两套语义、且谁赢取决于焦点在哪 ——
    /// 正是 Y 批记档要求避免的「同源判定」问题。
    pub(super) fn on_rename_pane(
        &mut self,
        _: &RenamePane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if yields_to_overlay(window, cx) {
            return;
        }
        // 项目列表的行拿着焦点 → 改那一行的名字(项目行 / 分组行都算)
        if self
            .project_list
            .update(cx, |list, cx| list.rename_focused_row(window, cx))
        {
            return;
        }
        // 文件树的行拿着焦点 → 改那个文件 / 目录的名字(与它右键「重命名」同一条路)
        if self
            .file_tree
            .update(cx, |tree, cx| tree.rename_focused_row(window, cx))
        {
            return;
        }
        if !self.terminal_page_active(cx) {
            return;
        }
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        let current = self
            .store
            .read(cx)
            .active_layout()
            .and_then(|l| l.pane(&pane_id))
            .map(|p| p.label().to_string())
            .unwrap_or_default();
        modal::open_rename_pane(self.store.clone(), project_id, pane_id, current, window, cx);
    }

    pub(super) fn on_open_settings(
        &mut self,
        _: &OpenTerminalSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 白名单动作:覆盖物压着照样开(设置面板本身就是弹窗),但**焦点在输入框
        // 里时仍然让路** —— 原版 isTypingTarget 那道闸排在白名单之前
        if yields_to_typing(window, cx) {
            return;
        }
        settings::open_settings(self.store.clone(), None, window, cx);
    }

    pub(super) fn on_toggle_sessions(
        &mut self,
        _: &ToggleSessions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if yields_to_overlay(window, cx) {
            return;
        }
        self.toggle_drawer(DrawerPanel::Sessions, cx);
    }

    /// 边条两颗按钮用的开关:相同则收起,否则换过去
    /// (`store.ts:686` 的 `toggleRightDrawer`)。
    pub(super) fn toggle_drawer(&mut self, panel: DrawerPanel, cx: &mut Context<Self>) {
        let next = if self.right_drawer == Some(panel) {
            None
        } else {
            Some(panel)
        };
        self.set_drawer(next, cx);
    }

    /// 抽屉内 segmented 切换用的:直接换过去,**不做「再点一次关闭」**
    /// (`store.ts:687` 原注释)。
    pub(super) fn open_drawer(&mut self, panel: DrawerPanel, cx: &mut Context<Self>) {
        self.set_drawer(Some(panel), cx);
    }

    /// 换抽屉。可见性要透给两个面板 —— 收着的时候会话面板不该去扫会话
    /// (WSL 那一路会冷启动整台 VM),Git 面板不该去 `discover_git_repos`(扫盘)。
    pub(super) fn set_drawer(&mut self, next: Option<DrawerPanel>, cx: &mut Context<Self>) {
        if self.right_drawer == next {
            return;
        }
        let now = std::time::Instant::now();
        let prev = self.right_drawer;
        self.right_drawer = next;
        // 换面板才播 push / 滑块;从收着打开直接落位(开抽屉本身有整层滑入)
        match (prev, next) {
            (Some(old), Some(new)) => {
                self.drawer_last_panel = Some(new);
                self.drawer_swap
                    .observe(&new, |_, _| drawer_panel_order(old, new));
                self.drawer_tab.retarget_at(drawer_tab_left(new), now);
            }
            (None, Some(new)) => {
                self.drawer_last_panel = Some(new);
                self.drawer_presence.show(now);
                self.drawer_swap.jump(&new);
                self.drawer_tab = mt_ui::motion::ValueTween::settled(
                    drawer_tab_left(new),
                    mt_ui::motion::TAB_INDICATOR,
                );
                // 重新打开 Git 抽屉时若本来就选着一条提交:随整层抽屉一起滑入即可,
                // 详情栏一步到位,避免双重滑入叠加。
                if new == DrawerPanel::Git && self.git_panel.read(cx).detail_pane(cx).is_some() {
                    self.detail_presence.retarget(1.0, now, false);
                } else {
                    self.detail_presence = overlay_motion::Presence::hidden(
                        mt_ui::motion::OVERLAY_IN,
                        mt_ui::motion::OVERLAY_OUT,
                    );
                    self.detail_last = None;
                }
            }
            (Some(old), None) => {
                self.drawer_last_panel = Some(old);
                self.drawer_presence.hide(now);
                self.drawer_drag = None;
                self.detail_drag = None;
                // 整层抽屉收起时把详情栏当前开合进度定格为刚体,随抽屉一并滑出视口右缘;
                // 关动画模式立即卸载,零残留。
                if !self.drawer_presence.mounted_at(now) {
                    self.drawer_last_panel = None;
                    self.detail_last = None;
                    self.detail_presence = overlay_motion::Presence::hidden(
                        mt_ui::motion::OVERLAY_IN,
                        mt_ui::motion::OVERLAY_OUT,
                    );
                } else if self.detail_last.is_some() {
                    let frozen = self.detail_presence.value_at(now);
                    self.detail_presence.retarget(frozen, now, false);
                }
            }
            (None, None) => {}
        }
        self.session_panel.update(cx, |panel, cx| {
            panel.set_visible(next == Some(DrawerPanel::Sessions), cx)
        });
        self.git_panel.update(cx, |panel, cx| {
            panel.set_visible(next == Some(DrawerPanel::Git), cx)
        });
        cx.notify();
    }

    pub(super) fn on_toggle_usage(
        &mut self,
        _: &ToggleUsage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if yields_to_overlay(window, cx) {
            return;
        }
        self.toggle_usage(window, cx);
    }

    /// 开合用量面板。可见性要透给面板 —— 它常驻不销毁,自动刷新定时器只能靠
    /// 这个开关闸住(不然关掉之后还在每 5s 扫会话文件)。
    pub(super) fn toggle_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.usage_open = !self.usage_open;
        if self.usage_open && self.usage_panel.is_none() {
            let store = self.store.clone();
            let dir = app_data_dir();
            self.usage_panel = Some(cx.new(|cx| UsagePanel::new(store, dir, window, cx)));
        }
        let open = self.usage_open;
        if let Some(panel) = self.usage_panel.as_ref() {
            panel.update(cx, |panel, cx| panel.set_visible(open, cx));
        }
        cx.notify();
    }

    /// 跳到「下一件该我做的事」(旧版点标题栏状态灯的动作)。
    ///
    /// 落点算法与托盘左键**共用** [`Self::focus_attention_target`](原版也是同一个
    /// `focusAttentionTarget`);这里多一句清未读 —— 点状态灯就是「我看过了」。
    pub(super) fn on_jump_attention(
        &mut self,
        _: &JumpAttention,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if yields_to_overlay(window, cx) {
            return;
        }
        if self.focus_attention_target(None, window, cx) {
            self.store
                .update(cx, |store, cx| store.clear_unread_done(cx));
        }
    }

    pub(super) fn on_focus_left(
        &mut self,
        _: &FocusLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_adjacent(Direction::Left, window, cx);
    }
    pub(super) fn on_focus_right(
        &mut self,
        _: &FocusRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_adjacent(Direction::Right, window, cx);
    }
    pub(super) fn on_focus_up(&mut self, _: &FocusUp, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_adjacent(Direction::Up, window, cx);
    }
    pub(super) fn on_focus_down(
        &mut self,
        _: &FocusDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_adjacent(Direction::Down, window, cx);
    }

    pub(super) fn focus_adjacent(
        &mut self,
        dir: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 这四条只从快捷键进来,守卫放这一处就够(Alt+方向键在文本输入框里
        // 还是「按词移动」,让路给弹窗是必须的)。
        //
        // **没挪成就放行**(`propagate`):让路、不在终端页、那个方向没有相邻分屏,按键都
        // 原样落回焦点所在处 —— fish / zsh 与不少 TUI 拿 Alt+方向键按词移动,单屏时吞掉
        // 它等于把这组键整个废掉。gpui 先派发 action,处理器 propagate 之后才跑 key 监听
        // (`Window::dispatch_key_event`:binding 循环里 `propagate_event` 仍为真就落到
        // `finish_dispatch_key_event`),终端照常把它编码进 PTY。
        let moved = !yields_to_overlay(window, cx)
            && self.terminal_page_active(cx)
            && self
                .terminal_area
                .update(cx, |area, cx| area.focus_adjacent(dir, window, cx));
        if !moved {
            cx.propagate();
        }
    }

    /// Ctrl+F:文件页交给当前文档；终端页在**当前焦点 pane** 上开查找条。
    ///
    /// 与原版有一处差:原版在「当前 pane 还没有 ptyId」时**不拦**这次按键
    /// (让 Ctrl+F 原样落进终端,发 `\x06`),而 gpui 的 action 一旦绑上就必然吞掉
    /// 按键、没有「退回按键」的通路。取舍是:PTY 还没起来的空 pane 上按 Ctrl+F
    /// 什么也不发生 —— 那个 pane 本来也没有终端能收这个字节。
    pub(super) fn on_terminal_search(
        &mut self,
        _: &TerminalSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.terminal_page_active(cx) {
            // 文档编辑器本身也是 `Input`,不能套用终端页的 typing guard,否则编辑器
            // 获得焦点后 Ctrl+F 会被提前吞掉；真正的弹窗仍然必须优先处理快捷键。
            if !overlay::allows(overlay::Yield::ToOverlay) {
                return;
            }
            self.workbench_area
                .update(cx, |area, cx| area.search_active_document(window, cx));
            return;
        }
        if yields_to_overlay(window, cx) {
            return;
        }
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        let pane = {
            let store = self.store.read(cx);
            store
                .project_state(&project_id)
                .and_then(|state| state.pane(&pane_id))
                .and_then(|pane| pane.pty_id)
                .and_then(|pty_id| store.terminal(pty_id).cloned())
        };
        let Some(pane) = pane else { return };
        pane.update(cx, |pane, cx| pane.open_search(window, cx));
    }

    /// Ctrl+Shift+K:开合命令库浮层(issue #81)。落在**当前焦点 pane** 上,
    /// 与控制条那颗钮同一个入口(`TerminalArea::toggle_command_popover`)。
    ///
    /// toggle 的「关」这一半要排在 `yields_to_overlay` **之前**:浮层开着时它自己
    /// 就在覆盖物栈里,先判让路的话第二次按永远关不掉(全局搜索那条踩过同一个坑)。
    pub(super) fn on_toggle_command_library(
        &mut self,
        _: &ToggleCommandLibrary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if overlay::is_top(overlay::key(overlay::kind::COMMAND_LIBRARY)) {
            self.terminal_area.update(cx, |area, cx| {
                area.close_command_popover(window, cx);
            });
            return;
        }
        if yields_to_overlay(window, cx) || !self.terminal_page_active(cx) {
            return;
        }
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        self.terminal_area.update(cx, |area, cx| {
            area.toggle_command_popover(&project_id, &pane_id, window, cx);
        });
    }

    /// Ctrl+Shift+F:开合全局搜索。
    ///
    /// **两道闸都不加**,与原版有一处有意的偏差:原版把 globalSearch 放进白名单
    /// 时写着「它是 toggle,弹窗开着时按第二次才能关掉」,可搜索框一打开焦点就在
    /// 它自己的输入框里,`isTypingTarget` 那道闸先一步把这条挡掉了 —— 注释里的
    /// toggle 实际做不到。这里让它真的 toggle(按注释的意图,不是按它的 bug)。
    pub(super) fn on_global_search(
        &mut self,
        _: &GlobalSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        search_modal::toggle(self.store.clone(), window, cx);
    }

    /// Ctrl+Shift+O:开合快速打开文件。
    ///
    /// 两道闸只加覆盖物那一道:
    ///
    /// - **不看**「焦点在输入框里」([`yields_to_typing`]):文件页的代码编辑器就是输入框,
    ///   看着代码想跳到另一个文件正是最常用的场合,让路就等于这个键在文件页里废了;
    ///   Ctrl+Shift+O 在输入框里也没有别的含义;
    /// - **看**覆盖物栈:设置之类的弹窗压着时不在它上面再叠一层(从那里打开的文件落在弹窗
    ///   底下看不见)。
    ///
    /// toggle 的「关」排在让路之前:面板开着时它自己就在覆盖物栈里(同命令库那条)。
    pub(super) fn on_quick_open(
        &mut self,
        _: &QuickOpenFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if prompt::close_guarded(overlay::kind::QUICK_OPEN, window, cx) {
            return;
        }
        if !overlay::allows(overlay::Yield::ToOverlay) {
            return;
        }
        quick_open::open(self.store.clone(), window, cx);
    }

    /// Ctrl+Shift+↑:跳到上一个 AI 任务标记。首次按跳**最新一条**,
    /// 之后每按一次往上一格,到顶停住(非环形,见 [`markers::next_index`])。
    ///
    /// ⚠️ **加了 `yields_to_overlay`,与原版有意不同**:原版这两条不走
    /// `useGlobalHotkeys`,自己挂 capture 阶段的 window 监听
    /// (`useMarkerHotkeys.ts:59`),因此绕过了「焦点在输入框里」与「弹窗压着」
    /// 两道闸。方向键在输入框里有明确语义,在设置对话框里按 Ctrl+Shift+↑ 去跳终端
    /// 是意外行为 —— 这里让它与其余全局动作同口径。
    pub(super) fn on_marker_prev(
        &mut self,
        _: &MarkerPrev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if yields_to_overlay(window, cx) || !self.terminal_page_active(cx) {
            return;
        }
        self.store.update(cx, |store, cx| store.step_marker(-1, cx));
    }

    /// Ctrl+Shift+↓:跳到下一个 AI 任务标记。首次按跳**最早一条**。
    /// 让路口径见 [`Self::on_marker_prev`]。
    pub(super) fn on_marker_next(
        &mut self,
        _: &MarkerNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if yields_to_overlay(window, cx) || !self.terminal_page_active(cx) {
            return;
        }
        self.store.update(cx, |store, cx| store.step_marker(1, cx));
    }

    // ─── 页签与分屏的键盘路径 ─────────────────────────────────────
    //
    // 让路口径与同类动作一致:弹窗 / 输入框让路(`yields_to_overlay`),只在终端页生效。
    // 调比例那四条与 Alt+方向键一样,没动成就 `propagate` 把按键还给终端。

    /// Ctrl+Shift+X:关当前**单个**页签,与点页签上的 × 同一个入口
    /// ([`pane_actions::close_pane`]:AI 感知确认、已退出免确认)。按住不放的连发闸与
    /// Ctrl+Shift+W 共用一份时刻:两个关闭键交替连按也只算一次。
    pub(super) fn on_close_terminal_tab(
        &mut self,
        _: &CloseTerminalTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = std::time::Instant::now();
        // 停在文件 / diff 页签上:关的是当前页签,与 Ctrl+Shift+W 在那一页的分流同一口径
        // (那一页没有「整组」可言,两个键落到同一个动作上)
        match self.hotkey_page(window, cx) {
            HotkeyPage::Yield => return,
            HotkeyPage::Terminal => {}
            HotkeyPage::Document => {
                self.last_close_hotkey = Some(now);
                self.workbench_area
                    .update(cx, |area, cx| area.close_active_tab(window, cx));
                return;
            }
        }
        let last = self.last_close_hotkey.replace(now);
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        let store = self.store.read(cx);
        let dead = pane_actions::pane_is_dead(store, &project_id, &pane_id);
        if pane_actions::swallows_close_hotkey(store.config(), dead, last, now) {
            return;
        }
        pane_actions::close_pane(self.store.clone(), project_id, pane_id, window, cx);
    }

    /// Ctrl+Shift+M:最大化 / 还原当前分组。与页签栏的按钮、双击页签栏空白处、页签右键
    /// 同一个动作(单格布局下是空操作,与按钮不画同一判据)。
    pub(super) fn on_toggle_maximize_pane(
        &mut self,
        _: &ToggleMaximizePane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if yields_to_overlay(window, cx) || !self.terminal_page_active(cx) {
            return;
        }
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        self.store.update(cx, |store, cx| {
            store.toggle_maximized_leaf(&project_id, &pane_id, cx)
        });
    }

    pub(super) fn on_move_tab_left(
        &mut self,
        _: &MoveTabLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_tab(-1, window, cx);
    }
    pub(super) fn on_move_tab_right(
        &mut self,
        _: &MoveTabRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_tab(1, window, cx);
    }

    /// Ctrl+Shift+PageUp / PageDown:当前页签在所属分组内挪一格,到头不循环。
    pub(super) fn move_tab(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        if yields_to_overlay(window, cx) || !self.terminal_page_active(cx) {
            return;
        }
        let Some((project_id, pane_id)) = self.target_pane(cx) else {
            return;
        };
        self.store.update(cx, |store, cx| {
            store.move_pane_within_leaf(&project_id, &pane_id, delta, cx)
        });
    }

    pub(super) fn on_resize_split_left(
        &mut self,
        _: &ResizeSplitLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.resize_split(Direction::Left, window, cx);
    }
    pub(super) fn on_resize_split_right(
        &mut self,
        _: &ResizeSplitRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.resize_split(Direction::Right, window, cx);
    }
    pub(super) fn on_resize_split_up(
        &mut self,
        _: &ResizeSplitUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.resize_split(Direction::Up, window, cx);
    }
    pub(super) fn on_resize_split_down(
        &mut self,
        _: &ResizeSplitDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.resize_split(Direction::Down, window, cx);
    }

    /// Alt+Shift+方向键:把当前 pane 那一侧最近的分隔线朝箭头方向推一步
    /// ([`TerminalArea::resize_split`])。**没动成就放行**(让路、不在终端页、那个方向
    /// 没有分隔线、正最大化着),理由与 [`Self::focus_adjacent`] 相同:单屏时这组键归终端。
    pub(super) fn resize_split(
        &mut self,
        dir: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let handled = !yields_to_overlay(window, cx)
            && self.terminal_page_active(cx)
            && self
                .terminal_area
                .update(cx, |area, cx| area.resize_split(dir, window, cx));
        if !handled {
            cx.propagate();
        }
    }

    /// Ctrl+Shift+Z:重新打开当前项目最近关掉的终端(`store::reopen`)。按**活动项目**取
    /// 而不是焦点 pane:关光了终端、停在空态页时正是最需要它的时候。栈空时什么也不做。
    pub(super) fn on_reopen_closed_terminal(
        &mut self,
        _: &ReopenClosedTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 停在文件 / diff 页签上:重开的是最近关掉的**页签**(两处「重新打开已关闭的…」
        // 共用一个键,落在眼前那一页自己的已关闭栈上)
        match self.hotkey_page(window, cx) {
            HotkeyPage::Yield => return,
            HotkeyPage::Terminal => {}
            HotkeyPage::Document => {
                self.workbench_area.update(cx, |area, cx| {
                    area.reopen_closed_tab_of_active_project(window, cx)
                });
                return;
            }
        }
        let Some(project_id) = self.store.read(cx).active_project_id.clone() else {
            return;
        };
        self.store.update(cx, |store, cx| {
            store.reopen_closed_pane(&project_id, window, cx);
        });
    }
    /// Ctrl+Shift+P:项目快速切换器。
    pub(super) fn on_switch_project(
        &mut self,
        _: &SwitchProject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if yields_to_overlay(window, cx) {
            return;
        }
        project_switcher::open(self.store.clone(), window, cx);
    }
}
