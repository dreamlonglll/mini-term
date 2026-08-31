//! 终端与分屏相关的 `AppStore` 方法:新建/分屏/关闭/激活/焦点、pane 拖拽移动、
//! 双击最大化,以及 PTY 的起停与回收。
//!
//! 从 `store.rs` 原样搬来的几段(`// === 终端 ===` / `// === pane 拖拽移动 ===` /
//! `// === 双击最大化 ===`),段注释随代码走,逻辑一行未改。

use gpui::{AppContext, Context, Window};
use mt_config::{AiLauncher, ProjectConfig, ShellConfig};
use mt_pty::PtySpawn;
use mt_ui::{DwellConfig, TerminalStyle};

use crate::pane::{PaneEvent, TerminalPane};
use crate::tree::{
    AiSessionRef, DropZone, PaneState, PaneStatus, ProjectPanel, SplitDirection, SplitNode,
};

use super::pure::{
    next_maximized, resolve_auto_resume_command, resolve_resume_cwd, resolve_scrollback,
    terminal_style_from,
};
use super::AppStore;

impl AppStore {
    // === 终端 ===

    /// 新建一个终端 tab。
    ///
    /// - 项目还没有布局:建根叶子;
    /// - 已有布局:加进锚点 pane 所在叶子的 tab 栏并激活(锚点缺省 = 当前焦点 pane)。
    pub fn new_terminal(
        &mut self,
        project_id: &str,
        shell: Option<ShellConfig>,
        anchor_pane_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        self.new_terminal_with_cwd(project_id, shell, anchor_pane_id, None, window, cx)
    }

    /// 新建一个终端 tab 并按 AI 启动器把 agent 拉起来。
    ///
    /// 启动器是 `{名称, shell(可选), 命令}` 的具名条目(见 [`mt_config::AiLauncher`]),
    /// 桌面端与移动端**共用同一份配置**;这里是桌面端那条触发路径。
    ///
    /// 与移动端发起会话(`mobile_relay::MobileRelayBridge::try_start_session`)的区别
    /// 只在落点与善后:那边刻意用 `append_pane_background` 不抢焦点、要回执、要弹
    /// 审计 toast;桌面端是人自己点的,照常走 [`new_terminal`] 把焦点带过去,
    /// 不回执也不弹 toast。**改这里时记得看一眼那边**,两条路径共用启动器语义。
    ///
    /// [`new_terminal`]: Self::new_terminal
    pub fn new_terminal_from_launcher(
        &mut self,
        project_id: &str,
        launcher: &AiLauncher,
        anchor_pane_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        // 绑定的 shell 被删掉时退回默认 —— 总比不开好,用户在桌面看得到实情
        // (判据与移动端那条同一个 `resolve_shell`)
        let shell = self.resolve_shell(launcher.shell.as_deref())?;
        let pane_id = self.new_terminal(project_id, Some(shell), anchor_pane_id, window, cx)?;
        self.rename_pane(project_id, &pane_id, &launcher.name, cx);
        self.write_launcher_command(project_id, &pane_id, &launcher.command, cx);
        Some(pane_id)
    }

    /// 新建一个终端**面板**并按 AI 启动器把 agent 拉起来。
    /// 与 [`new_terminal_from_launcher`] 同,只是落点是新面板而非当前面板的 tab 栏。
    ///
    /// [`new_terminal_from_launcher`]: Self::new_terminal_from_launcher
    pub fn new_panel_from_launcher(
        &mut self,
        project_id: &str,
        launcher: &AiLauncher,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let shell = self.resolve_shell(launcher.shell.as_deref())?;
        let pane_id = self.new_panel(project_id, Some(shell), window, cx)?;
        self.rename_pane(project_id, &pane_id, &launcher.name, cx);
        self.write_launcher_command(project_id, &pane_id, &launcher.command, cx);
        Some(pane_id)
    }

    /// 把启动器命令连同回车写进 pane。
    ///
    /// ⚠️ 必须走 [`write_to_pane`] 而不是裸 PTY 写:AI 会话身份靠**输入检测**建立,
    /// 只有「往 shell 里敲进启动命令并回车」这条路能让 pane 进入 AI 会话状态。
    /// 把 AI CLI 当成 PTY 根程序 spawn(`shell -c "claude"`)会绕开检测,拿不到
    /// 状态徽章与对话镜像 —— 这是 ADR 0002 定下的纪律,别改。
    ///
    /// PTY 内核缓冲 stdin,shell 就绪前写入不丢(与移动端发起会话同一时序)。
    /// 写不进去时**保留 pane**:用户回头能看到它卡在哪。
    ///
    /// [`write_to_pane`]: Self::write_to_pane
    fn write_launcher_command(
        &mut self,
        project_id: &str,
        pane_id: &str,
        command: &str,
        cx: &mut Context<Self>,
    ) {
        self.write_to_pane(project_id, pane_id, &format!("{command}\r"), cx);
    }

    /// 新建终端并指定启动目录。
    ///
    /// 单独一个入口是因为 `claude --resume` 只认「启动目录」对应的会话桶 ——
    /// 子目录里起的会话在项目根恢复会报 `No conversation found`
    /// (对应 `src/utils/sessionJump.ts:90-99`)。除此之外与 [`new_terminal`] 同。
    ///
    /// [`new_terminal`]: Self::new_terminal
    pub fn new_terminal_with_cwd(
        &mut self,
        project_id: &str,
        shell: Option<ShellConfig>,
        anchor_pane_id: Option<String>,
        cwd: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let project = self.project(project_id)?.clone();
        let shell = shell.or_else(|| self.resolve_shell(None))?;
        let pane = self.spawn_pane(&project, &shell, cwd, window, cx)?;
        let pane_id = pane.id.clone();

        let anchor = anchor_pane_id.or_else(|| self.focused_pane_id.clone());
        let state = self.project_states.get_mut(project_id)?;
        if state.panels.is_empty() {
            let panel = ProjectPanel::new(SplitNode::leaf(pane));
            state.active_panel_id = Some(panel.id.clone());
            state.panels.push(panel);
        } else {
            // 锚点在哪个面板就落哪个面板(缺省的焦点 pane 就在活动面板上),
            // 锚点失效则回落活动面板
            let anchor = anchor.filter(|id| state.pane(id).is_some());
            let target = anchor
                .as_deref()
                .and_then(|id| state.panel_id_of_pane(id))
                .map(str::to_string)
                .or_else(|| state.active_panel().map(|p| p.id.clone()))?;
            let layout = &mut state.panel_mut(&target)?.layout;
            layout.append_pane(anchor.as_deref(), pane);
        }
        self.after_layout_change(project_id, cx);
        self.focus_pane(project_id, &pane_id, window, cx);
        Some(pane_id)
    }

    /// 在指定 pane 处分屏。分屏继承源 pane 的 cwd 覆盖。
    pub fn split_pane(
        &mut self,
        project_id: &str,
        pane_id: &str,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        self.split_pane_with_cwd(project_id, pane_id, direction, None, window, cx)
    }

    /// 分屏并**显式指定**新 PTY 的启动目录。
    ///
    /// 单独一个入口是给「分支会话到新分屏」用的:fork 出的会话必须落在源会话
    /// 记录的目录(`splitPane(…, { cwd })` 的等价物),见 [`resolve_fork_cwd`]。
    /// `cwd = None` 时与 [`split_pane`] 完全相同 —— 继承源 pane 的 cwd 覆盖
    /// (worktree 终端分出来的屏理应还在 worktree 里)。
    ///
    /// [`split_pane`]: Self::split_pane
    pub fn split_pane_with_cwd(
        &mut self,
        project_id: &str,
        pane_id: &str,
        direction: SplitDirection,
        cwd: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let project = self.project(project_id)?.clone();
        let source_cwd = cwd.or_else(|| {
            self.project_states
                .get(project_id)
                .and_then(|s| s.pane(pane_id))
                .and_then(|p| p.cwd.clone())
        });
        let shell_name = self
            .project_states
            .get(project_id)
            .and_then(|s| s.pane(pane_id))
            .map(|p| p.shell_name.clone());
        let shell = self.resolve_shell(shell_name.as_deref())?;

        let pane = self.spawn_pane(&project, &shell, source_cwd, window, cx)?;
        let new_pane_id = pane.id.clone();
        let new_leaf = SplitNode::leaf(pane);

        let state = self.project_states.get_mut(project_id)?;
        // 在目标 pane 所在的面板里分屏;pane 没了(含整个面板没了)与树变换
        // 未命中同一档处置 —— 回收无处安放的新 PTY
        let inserted = state
            .layout_of_pane_mut(pane_id)
            .map(|layout| layout.insert_split(pane_id, direction, new_leaf))
            .unwrap_or(false);
        if !inserted {
            // 目标 pane 在起 PTY 期间被关掉了 —— 新 PTY 无处安放,显式回收,
            // 否则后端留一个谁也看不见、谁也杀不掉的孤儿子进程。
            let orphan: Vec<u32> = self
                .terminals
                .keys()
                .copied()
                .filter(|id| !self.pty_in_any_layout(*id))
                .collect();
            for id in orphan {
                self.dispose_terminal(id, cx);
            }
            return None;
        }
        // 最大化状态下分出来的新格落在**被隐藏的整树**里,看不见会让人以为分屏坏了
        // —— 先自动还原(原版 `paneActions.ts::splitPane` 尾部同一句)
        self.clear_maximized(project_id);
        self.after_layout_change(project_id, cx);
        self.focus_pane(project_id, &new_pane_id, window, cx);
        Some(new_pane_id)
    }

    // === pane 拖拽移动 / 合并 / 重排(v0.14.0)===

    /// 拖拽移动 pane:`Center` 并入目标组的 tab 栏,四边在目标组对应方向分屏。
    /// 对应 `paneActions.ts::movePane`。
    ///
    /// 树变换是纯函数([`SplitNode::move_pane_in_layout`]),返回 `None` = 拖回
    /// 原位,这里直接不写 —— 不写就不落盘、不 notify,一次无效拖拽零副作用。
    pub fn move_pane(
        &mut self,
        project_id: &str,
        pane_id: &str,
        target_pane_id: &str,
        zone: DropZone,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 拖拽只发生在看得见的那棵树上 —— 源与目标都在活动面板里
        let Some(next) = self
            .project_states
            .get(project_id)
            .and_then(|s| s.active_layout())
            .and_then(|l| l.move_pane_in_layout(pane_id, target_pane_id, zone))
        else {
            return;
        };
        if let Some(state) = self.project_states.get_mut(project_id)
            && let Some(layout) = state.active_layout_mut()
        {
            *layout = next;
        }
        // 与 split_pane 同一处置:最大化状态下四边分屏会落进隐藏的整树,先还原。
        // `move_pane_to_tab` **不需要** —— 最大化时 tab 栏只能同组重排,结果就在眼前。
        self.clear_maximized(project_id);
        self.after_layout_change(project_id, cx);
        self.focus_pane(project_id, pane_id, window, cx);
    }

    /// 拖到 tab 栏的精确落位:同组前后换位,跨组按插入位并入并激活。
    /// 对应 `paneActions.ts::movePaneToTab`。
    pub fn move_pane_to_tab(
        &mut self,
        project_id: &str,
        pane_id: &str,
        anchor_pane_id: &str,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(next) = self
            .project_states
            .get(project_id)
            .and_then(|s| s.active_layout())
            .and_then(|l| l.move_pane_to_tab_index(pane_id, anchor_pane_id, index))
        else {
            return;
        };
        if let Some(state) = self.project_states.get_mut(project_id)
            && let Some(layout) = state.active_layout_mut()
        {
            *layout = next;
        }
        self.after_layout_change(project_id, cx);
        self.focus_pane(project_id, pane_id, window, cx);
    }

    // === 双击最大化(v0.14.0,纯运行时状态)===

    /// 当前被最大化的 pane。**只在布局真的分了屏时才作数** —— 单格布局下
    /// 「最大化」没有意义,原版 `TerminalArea.tsx` 也是拿 `layout.type === 'split'`
    /// 与门之后才去找那个叶子的。
    pub fn maximized_pane_id(&self, project_id: &str) -> Option<&str> {
        let state = self.project_states.get(project_id)?;
        let layout = state.active_layout()?;
        if !matches!(layout, SplitNode::Split { .. }) {
            return None;
        }
        state.maximized_pane_id.as_deref()
    }

    /// 双击 tab 栏空白处 / 点最大化钮的落点,对应 `PaneGroup.tsx::toggleMaximize`:
    /// **本组**已经是最大化的那一组就还原,否则把本组铺满(仅当真的分了屏)。
    ///
    /// 判据落在**叶子**上而不是 pane 上 —— 最大化之后在组内切了 tab,
    /// `maximized_pane_id` 还指着切换前那个 pane,但用户看到的仍是这一组铺满,
    /// 这时再双击一次理应还原(拿 pane id 直接比会变成「换成另一个 pane」)。
    pub fn toggle_maximized_leaf(
        &mut self,
        project_id: &str,
        anchor_pane_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.project_states.get(project_id) else {
            return;
        };
        let Some(layout) = state.active_layout() else {
            return;
        };
        let anchor_leaf = layout.leaf_of_pane(anchor_pane_id).map(|l| l.id().to_string());
        let current_leaf = state
            .maximized_pane_id
            .as_deref()
            .and_then(|id| layout.leaf_of_pane(id))
            .map(|l| l.id().to_string());
        let is_split = matches!(layout, SplitNode::Split { .. });

        if anchor_leaf.is_some() && anchor_leaf == current_leaf {
            self.set_maximized(project_id, None, cx);
        } else if is_split {
            self.set_maximized(project_id, Some(anchor_pane_id), cx);
        }
    }

    /// 切换最大化的底层写入。逐字照抄 `store.ts::toggleMaximizedPane` 的三态口径
    /// ([`next_maximized`])。
    ///
    /// ⚠️ 原版在这里还挂了一段 `suppress-pane-enter`(最大化/还原会让 React 重挂
    /// `PaneGroup`,整树的淡入动画会重播成满屏闪动)。GPUI 侧**结构性不需要**:
    /// 进场动画的进度表按 `项目\u{1}叶子` 索引且不按帧回收,同一个叶子换个容器
    /// 渲染时拿到的还是那条早就跑完的进度(见 `terminal_area::wrap_pane_enter`)。
    fn set_maximized(&mut self, project_id: &str, pane_id: Option<&str>, cx: &mut Context<Self>) {
        let Some(state) = self.project_states.get_mut(project_id) else {
            return;
        };
        let next = next_maximized(state.maximized_pane_id.as_deref(), pane_id);
        if state.maximized_pane_id == next {
            return;
        }
        state.maximized_pane_id = next;
        cx.notify();
    }

    /// 无条件还原(分屏 / 拖拽移动落地前调),不 notify —— 调用方随后都会走
    /// `after_layout_change`,那里统一 notify。
    fn clear_maximized(&mut self, project_id: &str) {
        if let Some(state) = self.project_states.get_mut(project_id) {
            state.maximized_pane_id = None;
        }
    }

    /// 关闭一个 pane:回收 PTY,再把它从所在面板的树里摘掉
    /// (面板随最后一个 pane 一起消失;面板全没了 = 项目回到空态)。
    pub fn close_pane(&mut self, project_id: &str, pane_id: &str, cx: &mut Context<Self>) {
        let pty_id = self
            .project_states
            .get(project_id)
            .and_then(|s| s.pane(pane_id))
            .and_then(|p| p.pty_id);
        if let Some(pty_id) = pty_id {
            self.dispose_terminal(pty_id, cx);
        }
        let Some(state) = self.project_states.get_mut(project_id) else {
            return;
        };
        state.remove_pane(pane_id);
        if self.focused_pane_id.as_deref() == Some(pane_id) {
            self.focused_pane_id = self
                .project_states
                .get(project_id)
                .and_then(|s| s.active_layout())
                .and_then(|l| l.first_active_pane())
                .map(|p| p.id.clone());
        }
        // 关掉的可能是活动面板的最后一个 pane → 活动指针挪到了邻位面板,
        // 而那个面板可能是恢复出来、从没显示过的(pane 还没有 PTY)—— 补起来
        self.hydrate_project(project_id, cx);
        self.after_layout_change(project_id, cx);
    }

    /// 关掉某个 pane **所在的整组**(Ctrl+Shift+W 的落点)。
    pub fn close_leaf_of_pane(&mut self, project_id: &str, pane_id: &str, cx: &mut Context<Self>) {
        let leaf_id = self
            .project_states
            .get(project_id)
            .and_then(|s| s.leaf_of_pane(pane_id))
            .map(|node| node.id().to_string());
        if let Some(leaf_id) = leaf_id {
            self.close_leaf(project_id, &leaf_id, cx);
        }
    }

    /// 关闭一整个叶子(它的全部 tab)。
    pub fn close_leaf(&mut self, project_id: &str, leaf_id: &str, cx: &mut Context<Self>) {
        let pane_ids: Vec<String> = self
            .project_states
            .get(project_id)
            .and_then(|s| s.node(leaf_id))
            .map(|node| match node {
                SplitNode::Leaf { panes, .. } => panes.iter().map(|p| p.id.clone()).collect(),
                _ => Vec::new(),
            })
            .unwrap_or_default();
        for pane_id in pane_ids {
            self.close_pane(project_id, &pane_id, cx);
        }
    }

    /// 激活叶子里的某个 tab 并把焦点交给它。
    pub fn activate_pane(
        &mut self,
        project_id: &str,
        pane_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 切走之前把上一个 pane 的 IME 预编辑串收掉,否则组合中失焦会在画面上
        // 留一串下划线残影(而且那次组合的候选框还挂在旧位置)。
        self.clear_preedit_of_focused(cx);
        // 目标 pane 可能在别的面板上(跳待办/会话跳转/未读完成都按 pane 定位)——
        // 先把那个面板切成活动的,否则「跳过去了但画面没变」
        let owner_panel = self
            .project_states
            .get(project_id)
            .and_then(|s| s.panel_id_of_pane(pane_id))
            .map(str::to_string);
        if let Some(panel_id) = owner_panel {
            self.set_active_panel(project_id, &panel_id, cx);
        }
        if let Some(state) = self.project_states.get_mut(project_id)
            && let Some(layout) = state.layout_of_pane_mut(pane_id)
        {
            layout.activate_pane(pane_id);
        }
        self.focus_pane(project_id, pane_id, window, cx);
        self.save_project_layout_soon(project_id, cx);
    }

    /// 叶内环形切 tab(Ctrl+Tab / Ctrl+Shift+Tab)。只有一个 tab 时什么也不做。
    pub fn cycle_pane(
        &mut self,
        project_id: &str,
        from_pane_id: &str,
        delta: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self
            .project_states
            .get(project_id)
            .and_then(|s| s.layout_of_pane(from_pane_id))
            .and_then(|l| l.cycle_target(from_pane_id, delta));
        if let Some(target) = target {
            self.activate_pane(project_id, &target, window, cx);
        }
    }

    /// 选中叶内第 `index` 个 tab(Ctrl+1..9,**1-based**)。越界不动。
    pub fn select_pane_by_index(
        &mut self,
        project_id: &str,
        from_pane_id: &str,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self
            .project_states
            .get(project_id)
            .and_then(|s| s.layout_of_pane(from_pane_id))
            .and_then(|l| l.pane_at_index(from_pane_id, index));
        if let Some(target) = target {
            self.activate_pane(project_id, &target, window, cx);
        }
    }

    /// 把当前焦点 pane 的 IME 预编辑串收掉(切 tab / 关 pane 之前)。
    fn clear_preedit_of_focused(&mut self, cx: &mut Context<Self>) {
        let Some(pane_id) = self.focused_pane_id.clone() else {
            return;
        };
        let pty_id = self
            .project_states
            .values()
            .find_map(|s| s.pane(&pane_id).and_then(|p| p.pty_id));
        if let Some(entity) = pty_id.and_then(|id| self.terminals.get(&id)).cloned() {
            entity.update(cx, |pane, cx| pane.clear_preedit(cx));
        }
    }

    /// 把键盘焦点交给某个 pane。
    pub fn focus_pane(
        &mut self,
        project_id: &str,
        pane_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focused_pane_id = Some(pane_id.to_string());
        let pty_id = self
            .project_states
            .get(project_id)
            .and_then(|s| s.pane(pane_id))
            .and_then(|p| p.pty_id);
        if let Some(entity) = pty_id.and_then(|id| self.terminals.get(&id)) {
            entity.update(cx, |pane, _| pane.focus(window));
        }
        cx.notify();
    }

    /// 当前项目里该操作哪个 pane:焦点 pane → 布局里第一个激活 pane
    /// (旧版 `resolveActivePane`,它以 DOM 焦点为准)。
    pub fn active_pane_id(&self, project_id: &str) -> Option<String> {
        let layout = self.project_states.get(project_id)?.active_layout()?;
        self.focused_pane_id
            .clone()
            .filter(|id| layout.pane(id).is_some())
            .or_else(|| layout.first_active_pane().map(|p| p.id.clone()))
    }

    /// 把一段文本当作用户键入写进某个 pane。
    ///
    /// 走 `TerminalPane::write` 而不是裸 PTY 写,是为了保住 AI 输入检测那一路 ——
    /// 与用户自己敲这条命令完全同一条链路,pane 因此能正常进入 AI 会话状态
    /// (旧版 `writePtyInput` 的同一条红线)。
    pub fn write_to_pane(
        &mut self,
        project_id: &str,
        pane_id: &str,
        text: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pty_id) = self
            .project_states
            .get(project_id)
            .and_then(|s| s.pane(pane_id))
            .and_then(|p| p.pty_id)
        else {
            return false;
        };
        let Some(entity) = self.terminals.get(&pty_id).cloned() else {
            return false;
        };
        let bytes = text.as_bytes().to_vec();
        entity.update(cx, |pane, cx| pane.write(&bytes, cx));
        true
    }

    /// 分屏分隔条拖动后的比例回写。
    pub fn set_split_sizes(
        &mut self,
        project_id: &str,
        node_id: &str,
        sizes: Vec<f64>,
        cx: &mut Context<Self>,
    ) {
        let changed = self
            .project_states
            .get_mut(project_id)
            .and_then(|s| s.node_mut(node_id))
            .map(|node| match node {
                SplitNode::Split {
                    sizes: current,
                    children,
                    ..
                } => {
                    if sizes.len() != children.len() || *current == sizes {
                        false
                    } else {
                        *current = sizes;
                        true
                    }
                }
                SplitNode::Leaf { .. } => false,
            })
            .unwrap_or(false);
        if changed {
            self.save_project_layout_soon(project_id, cx);
        }
    }

    /// 恢复出来的布局里,pane 还没有 PTY(重启后 PTY 当然不在了)。
    /// 项目第一次被显示时把它们补起来 —— 与旧版「PaneGroup 懒创建」同一时机。
    ///
    /// # AI 自动续接
    ///
    /// 逐条搬运 `src/components/PaneGroup.tsx` 的那个 effect 与
    /// `src/utils/aiResume.ts`:
    ///
    /// 1. **起 PTY 的目录**用会话记录的 cwd —— `claude --resume` 只认「启动目录」
    ///    对应的会话桶,起于子目录的会话在项目根恢复会报 `No conversation found`;
    ///    但 **pane 自己的 cwd 优先**(那是用户显式给这个 pane 定的目录,worktree
    ///    终端靠它),会话 cwd 只在 pane 没指定时兜底;
    /// 2. 存量记录没有 cwd 时向 `mt_ai` 反查 jsonl,查到随身份写回并持久化,
    ///    下次重启免查;codex 会话不按目录分桶,不反查;
    /// 3. 写完 resume **只清 `resume_pending`、保留 `ai_session`** ——
    ///    codex resume 不会重新上报 SessionStart,身份清了第二次重启就断代;
    /// 4. 否决条件全在 [`resolve_auto_resume_command`]。
    pub fn hydrate_project(&mut self, project_id: &str, cx: &mut Context<Self>) {
        let Some(project) = self.project(project_id).cloned() else {
            return;
        };
        // SSH 远程项目的 PTY 是 ssh 启动器,启动初期可能停在口令交互上,预写的
        // 命令会被当口令消费;远端会话身份也不来自本机 hook(mt-ssh 尚未进
        // crates/,这里只把这条守卫先立住)。
        let remote = project.ssh_connection_id.is_some();
        // 缺省开启(`config.aiAutoResume`)
        let auto_resume = self.config.ai_auto_resume.unwrap_or(true);

        struct Pending {
            pane_id: String,
            shell_name: String,
            cwd: Option<String>,
            ai_session: Option<AiSessionRef>,
            resume_pending: bool,
        }
        // 只补**活动面板**:后台面板与后台项目同一档懒创建时机,
        // 切过去(`set_active_panel`)才起 PTY
        let pending: Vec<Pending> = self
            .project_states
            .get(project_id)
            .and_then(|s| s.active_layout())
            .map(|l| {
                l.panes()
                    .into_iter()
                    // status == error 的 pane 不重开(旧版 effect 的同一条守卫):
                    // 它上次就是起不来 / 已退出,自动重来只会刷屏
                    .filter(|p| p.pty_id.is_none() && p.status != PaneStatus::Error)
                    .map(|p| Pending {
                        pane_id: p.id.clone(),
                        shell_name: p.shell_name.clone(),
                        cwd: p.cwd.clone(),
                        ai_session: p.ai_session.clone(),
                        resume_pending: p.resume_pending,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if pending.is_empty() {
            return;
        }

        for item in pending {
            let Some(shell) = self.resolve_shell(Some(&item.shell_name)) else {
                // 一个 shell 都没有 —— 旧版把 pane 标成 error 而不是静默跳过
                if let Some(state) = self.project_states.get_mut(project_id)
                    && let Some(pane) = state.pane_mut(&item.pane_id)
                {
                    pane.status = PaneStatus::Error;
                }
                continue;
            };

            // 这一轮要不要续接(开关 + 标记 + 远程),决定了会不会去查会话 cwd
            let session = (auto_resume && item.resume_pending && !remote)
                .then(|| item.ai_session.clone())
                .flatten();
            let resume_cwd = session.as_ref().and_then(resolve_resume_cwd);
            // pane 自己的 cwd 优先,会话 cwd 兜底
            let start_cwd = item.cwd.clone().or_else(|| resume_cwd.clone());

            let pty_id = self.start_pty(&project, &shell, start_cwd.as_deref(), cx);
            if let Some(state) = self.project_states.get_mut(project_id)
                && let Some(pane) = state.pane_mut(&item.pane_id)
            {
                pane.pty_id = Some(pty_id);
            }

            let Some(command) = resolve_auto_resume_command(
                auto_resume,
                item.resume_pending,
                item.ai_session.as_ref(),
                remote,
            ) else {
                continue;
            };

            // 先清标记再写命令(顺序同旧版):标记的语义是「这个 pane 还没续过」
            let mut session_patch: Option<AiSessionRef> = None;
            if let Some(state) = self.project_states.get_mut(project_id)
                && let Some(pane) = state.pane_mut(&item.pane_id)
            {
                pane.resume_pending = false;
                // 反查所得的启动目录随身份写回,下次重启直达不再查
                if let Some(cwd) = resume_cwd.as_ref()
                    && let Some(sess) = pane.ai_session.as_mut()
                    && sess.cwd.as_deref() != Some(cwd.as_str())
                {
                    sess.cwd = Some(cwd.clone());
                    session_patch = Some(sess.clone());
                }
            }
            // PTY 内核缓冲 stdin,shell 就绪前写入不丢(与移动端发起会话同一时序)。
            // 走 `write_to_pane` 而不是裸 PTY 写:AI 输入检测那一路要看得见这条命令,
            // pane 才会正常进入 AI 会话状态。
            self.write_to_pane(project_id, &item.pane_id, &format!("{command}\r"), cx);
            if session_patch.is_some() {
                self.save_project_layout_soon(project_id, cx);
            }
        }
        cx.notify();
    }

    /// 起 PTY 并拼出 `PaneState`。
    // 拆分前是私有方法;调用点在 `store::layout`(挂后台 pane / 新建面板),升到 `pub(super)`。
    pub(super) fn spawn_pane(
        &mut self,
        project: &ProjectConfig,
        shell: &ShellConfig,
        cwd_override: Option<String>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PaneState> {
        let pty_id = self.start_pty(project, shell, cwd_override.as_deref(), cx);
        let mut pane = PaneState::new(shell.name.clone());
        pane.pty_id = Some(pty_id);
        pane.cwd = cwd_override;
        Some(pane)
    }

    /// 真正起一个 PTY + 终端视图,返回 pane 编号。
    ///
    /// PTY 起不到(shell 路径没了 / 目录不存在)时不 panic 也不静默:视图里显示
    /// 错误文本,pane 照样存在,用户看得见是哪个 tab 出的问题。
    // 拆分前是私有方法;调用点在 `store::ssh::reset_pane_for_reconnect`,升到 `pub(super)`。
    pub(super) fn start_pty(
        &mut self,
        project: &ProjectConfig,
        shell: &ShellConfig,
        cwd_override: Option<&str>,
        cx: &mut Context<Self>,
    ) -> u32 {
        let pty_id = self.next_pty_id;
        self.next_pty_id += 1;

        let cwd = cwd_override
            .map(str::to_string)
            .unwrap_or_else(|| project.path.clone());
        let mut env = vec![
            // hook 子进程靠它关联回具体 pane(与装机版同一个变量名,不能改)
            ("MINITERM_PTY_ID".to_string(), pty_id.to_string()),
        ];
        let hook_port = self.ai.hook_port();
        if hook_port > 0 {
            env.push(("MINITERM_HOOK_PORT".to_string(), hook_port.to_string()));
        }

        // SSH 远程分支:直接 spawn `ssh` 作 PTY 子进程(不经本地 shell,对齐 WSL
        // 启动器重写模式)。本地 cwd 用兜底目录 —— 远程目录由 ssh 的远端命令
        // `cd '<path>' 2>/dev/null; exec $SHELL -l` 进入,项目的 `path` 是远程 POSIX 路径,
        // 传给 portable-pty 只会让 ConPTY 静默退回 `$USERPROFILE`。
        //
        // AI 状态感知在这条路上走 PTY 输入/输出扫描的降级路径(输入检测作用于
        // 数据流,对远程天然可用);hook 精确状态不可用,PRD 已接受。
        //
        // 项目级环境变量对远程 pane **不注入**(装机版同款:那些变量属于本地
        // 机器,注给本地 ssh 客户端毫无意义)。
        let remote = project.ssh_connection_id.as_deref().map(|conn_id| {
            crate::remote_ssh::find_connection(&self.config.ssh_connections, conn_id)
                .and_then(|conn| crate::remote_ssh::prepare_remote_launch(&conn, &cwd))
        });
        let (spec, extras) = match remote {
            None => (
                PtySpawn {
                    program: shell.command.clone(),
                    args: shell.args.clone().unwrap_or_default(),
                    cwd: Some(cwd.clone()),
                    env,
                    rows: mt_pty::INITIAL_PTY_ROWS,
                    cols: mt_pty::INITIAL_PTY_COLS,
                },
                crate::pane::RemoteLaunchExtras::default(),
            ),
            Some(Ok(launch)) => (
                PtySpawn {
                    program: launch.program,
                    args: launch.args,
                    cwd: Some(mt_pty::fallback_local_cwd()),
                    env,
                    rows: mt_pty::INITIAL_PTY_ROWS,
                    cols: mt_pty::INITIAL_PTY_COLS,
                },
                crate::pane::RemoteLaunchExtras {
                    ssh_password: launch.password,
                    preflight_error: None,
                },
            ),
            Some(Err(err)) => (
                // 预检失败:不 spawn,pane 里直接显示这条错误(见 RemoteLaunchExtras)。
                // spec 的内容此时不会被用到,给一份无害的占位。
                PtySpawn {
                    program: shell.command.clone(),
                    args: Vec::new(),
                    cwd: None,
                    env,
                    rows: mt_pty::INITIAL_PTY_ROWS,
                    cols: mt_pty::INITIAL_PTY_COLS,
                },
                crate::pane::RemoteLaunchExtras {
                    ssh_password: None,
                    preflight_error: Some(err),
                },
            ),
        };
        let is_remote = project.ssh_connection_id.is_some();
        // 项目级环境变量走 user_env —— 它会被 `MINITERM_` 前缀过滤挡一道,
        // 用户手改配置(现在是 config.db)也覆盖不掉内部协议变量。
        // 远程 pane 不注入(见上方分支注释)。
        let user_env: Vec<(String, String)> = if is_remote {
            Vec::new()
        } else {
            project
                .env_vars
                .iter()
                .filter(|v| v.enabled)
                .map(|v| (v.key.clone(), v.value.clone()))
                .collect()
        };

        let style = self.terminal_style();
        let theme = self.terminal_theme.clone();
        let dwell = self.selection_dwell();
        // 回滚行数在**建终端时**就要喂进 alacritty 的 `term::Config` ——
        // 它决定 grid 的历史容量,晚一步只能靠 `set_options` 补(见 `apply_scrollback`)
        let scrollback = resolve_scrollback(self.config.terminal_scrollback as f64) as usize;
        let ai = self.ai.clone();
        let entity = cx.new(|cx| {
            TerminalPane::new(
                pty_id, spec, user_env, style, theme, dwell, scrollback, ai, extras, cx,
            )
        });

        // 子进程退出 → pane 状态 error(与旧版 pty-exit 同语义);
        // 用户键入 → 清 attention 黄灯(与旧版 clearPaneAttentionByPty 同语义)
        let sub = cx.subscribe(&entity, move |store, _entity, event: &PaneEvent, cx| {
            match event {
                PaneEvent::Exited(code) => store.on_pty_exit(pty_id, *code, cx),
                PaneEvent::UserInput => store.clear_pane_attention_by_pty(pty_id, cx),
                // AI 任务标记。**必须走事件而不是在 write 里直接 update store** ——
                // `write_to_pane` 是在 `store.update` 里调 `pane.write` 的,那里再去
                // `AppStore::global(cx).update` 就是同一实体的嵌套 update(gpui 直接 panic)。
                // `cx.emit` 是延后派发的,天然绕开。
                PaneEvent::AiMarks(batch) => store.add_markers(pty_id, batch.clone(), cx),
            }
        });
        self.pane_subs.insert(pty_id, sub);
        self.terminals.insert(pty_id, entity);
        pty_id
    }

    /// 拖选停留自动复制的参数(`config.selectionAutoCopySecs`)。
    ///
    /// **缺省 1 秒**,与前端 `config.selectionAutoCopySecs ?? 1` 一字不差;填 0
    /// 就是关掉停留语义(退回「松手即复制」)。设置页改了这一项之后走
    /// [`Self::apply_selection_dwell`] 给存量终端下发 —— 与 `apply_theme` 同形。
    // 拆分前是私有方法;调用点在 `store::prefs::set_selection_auto_copy_secs`,升到 `pub(super)`。
    pub(super) fn selection_dwell(&self) -> DwellConfig {
        DwellConfig::from_secs(self.config.selection_auto_copy_secs.unwrap_or(1.0) as f32)
    }

    // 拆分前是私有方法;调用点在 `store::prefs::apply_terminal_style`,升到 `pub(super)`。
    pub(super) fn terminal_style(&self) -> TerminalStyle {
        terminal_style_from(
            self.config.terminal_font_size,
            self.config.terminal_font_family.as_deref(),
            self.config.terminal_ligatures,
        )
    }

    /// 解析要用的 shell:指定名 → `defaultShell` → 列表首项。
    pub fn resolve_shell(&self, preferred: Option<&str>) -> Option<ShellConfig> {
        let shells = &self.config.available_shells;
        preferred
            .and_then(|name| shells.iter().find(|s| s.name == name))
            .or_else(|| shells.iter().find(|s| s.name == self.config.default_shell))
            .or_else(|| shells.first())
            .cloned()
    }
}
