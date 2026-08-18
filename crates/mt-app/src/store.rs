//! 全局状态。对应 `src/store.ts` 的那一份 zustand store。
//!
//! # 形状
//!
//! ```text
//! AppStore
//!  ├─ config: AppConfig            ← mt-config 加载/保存(带写盘令牌)
//!  ├─ active_project_id
//!  ├─ project_states: {projectId → ProjectState{ layout: Option<SplitNode>, status }}
//!  ├─ terminals:      {ptyId → Entity<TerminalPane>}   ← 旧版的 terminalCache
//!  ├─ focused_pane_id                                   ← 旧版靠 DOM 焦点推,这里显式记
//!  └─ ai: AiBridge                                      ← hook / monitor / 输入输出旁路
//! ```
//!
//! store 本身是一个 gpui `Entity`,放在 `Global` 里给所有视图取用;视图通过
//! `cx.observe(&store)` 订阅变化 —— 等价于 zustand 的 `useAppStore(selector)`,
//! 只是粒度粗一档(整棵重画,终端内容不受影响:那一层在 `TerminalPane` 自己的
//! entity 上,不随 store 的 notify 重跑)。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AppContext, Context, Entity, Global, Subscription, Task, Window};
use mt_config::{AppConfig, ConfigStore, ProjectConfig, SaveError, ShellConfig};
use mt_pty::PtySpawn;
use mt_ui::theme_bridge::BackgroundArt;
use mt_ui::{TerminalStyle, TerminalTheme};

use crate::ai::{AiBridge, AiEvent};
use crate::notify::{AlertPlan, DoneTracker, NotifyPrefs, PaneRef, StatusTransition};
use crate::pane::{PaneEvent, TerminalPane};
use crate::persist;
use crate::session_panel::build_resume_command;
use crate::shell_ops::ShellList;
use crate::tree::{
    AiSessionRef, PaneState, PaneStatus, SplitDirection, SplitNode, gen_id,
};

/// 单个项目的运行时状态(对应 `types.ts` 的 `ProjectState`)。
pub struct ProjectState {
    /// 终端布局树;`None` = 还没有终端(渲染空态)。
    pub layout: Option<SplitNode>,
    /// 由 layout 聚合出的项目级状态(error > ai-working > ai-idle > idle)。
    pub status: PaneStatus,
    /// 非激活项目里有 AI 任务完成 —— 项目行上的提示点。
    pub needs_attention: bool,
}

impl ProjectState {
    fn new() -> Self {
        Self {
            layout: None,
            status: PaneStatus::Idle,
            needs_attention: false,
        }
    }
}

struct GlobalStore(Entity<AppStore>);
impl Global for GlobalStore {}

/// 一次 AI 事件算出来的提醒动作 + 播报所需的上下文。
///
/// 提示音与任务栏闪烁要碰 `Window`(拿 HWND),而 AI 事件是从后台 channel 泵进来的
/// —— store 只算「该做什么」,真正执行留给持有 window 的 [`crate::Workspace`]。
pub struct PendingAlert {
    pub plan: AlertPlan,
    pub project_id: String,
    pub project_name: String,
    /// 自定义提示音路径(`config.aiCompletionSoundPath`)。
    pub sound_path: Option<String>,
}

pub struct AppStore {
    config: AppConfig,
    /// 写盘令牌(乐观并发);0 = 还没成功 load 过,此时一律不写盘。
    token: u64,
    config_store: Arc<ConfigStore>,

    pub active_project_id: Option<String>,
    project_states: HashMap<String, ProjectState>,
    /// ptyId → 终端视图。pane 只在树里存 id,视图挂这里(旧版 terminalCache)。
    terminals: HashMap<u32, Entity<TerminalPane>>,
    /// 每个 pane 的退出订阅,与 terminals 同生命周期。
    pane_subs: HashMap<u32, Subscription>,
    /// 当前拿着键盘焦点的 pane(旧版靠 DOM `activeElement` 推,这里显式维护)。
    pub focused_pane_id: Option<String>,

    next_pty_id: u32,
    ai: AiBridge,

    /// 当前生效的终端配色(主题装配的产物,见 [`crate::theme`])。
    /// 新建终端拿它,已存在的终端由 [`AppStore::apply_theme_from_config`] 热更新。
    terminal_theme: TerminalTheme,
    /// 当前主题的背景图氛围层参数。**渲染归 mt-ui,这里只是数据落点**。
    background_art: Option<BackgroundArt>,

    /// 展开的目录(按项目)。运行时态,落盘走 `ProjectConfig::expanded_dirs`。
    expanded_dirs: HashMap<String, HashSet<String>>,

    /// 完成队列(未读集合 + 完成序号),对应旧版的 unreadDonePaneIds / aiDoneOrder。
    done: DoneTracker,
    /// 主窗口是否聚焦。聚焦时完成的任务用户正看着,不计入「未读完成」。
    window_focused: bool,

    /// 防抖保存的代号:只有最后一次排上的任务才真写盘。
    save_generation: u64,
    _save_task: Option<Task<()>>,
}

impl AppStore {
    /// 装配 store:加载配置 → 恢复各项目布局(不起 PTY,PTY 在首次显示时懒起)。
    pub fn new(config_store: Arc<ConfigStore>, ai: AiBridge, cx: &mut Context<Self>) -> Self {
        let _ = cx;
        let (config, token) = match config_store.load() {
            Ok(loaded) => (loaded.config, loaded.token),
            Err(err) => {
                // 加载失败**绝不**伪装成空配置:令牌留 0,后续所有保存都会被自己挡下,
                // 免得一次读盘故障把用户的项目列表清空(旧版同一条红线)。
                eprintln!("[store] 配置加载失败({err:#}),本次以只读模式运行");
                (AppConfig::default(), 0)
            }
        };

        let mut project_states = HashMap::new();
        let mut expanded_dirs = HashMap::new();
        for project in &config.projects {
            let mut state = ProjectState::new();
            if let Some(saved) = &project.saved_layout {
                state.layout = persist::restore_layout(saved, &config);
                if let Some(layout) = &state.layout {
                    state.status = layout.highest_status();
                }
            }
            project_states.insert(project.id.clone(), state);
            expanded_dirs.insert(
                project.id.clone(),
                project.expanded_dirs.iter().cloned().collect(),
            );
        }

        let active_project_id = config
            .last_active_project_id
            .clone()
            .filter(|id| project_states.contains_key(id))
            .or_else(|| config.projects.first().map(|p| p.id.clone()));

        Self {
            config,
            token,
            config_store,
            active_project_id,
            project_states,
            terminals: HashMap::new(),
            pane_subs: HashMap::new(),
            focused_pane_id: None,
            next_pty_id: 1,
            ai,
            // 真正的配色在 `apply_theme_from_config` 里装配(要 `&mut App` 取系统
            // 外观 / 装 gpui-component 主题层),这里先给个能跑的初值
            terminal_theme: TerminalTheme::default(),
            background_art: None,
            expanded_dirs,
            done: DoneTracker::default(),
            window_focused: true,
            save_generation: 0,
            _save_task: None,
        }
    }

    // === 全局取用 ===

    pub fn set_global(store: Entity<AppStore>, cx: &mut App) {
        cx.set_global(GlobalStore(store));
    }

    pub fn global(cx: &App) -> Entity<AppStore> {
        cx.global::<GlobalStore>().0.clone()
    }

    // === 只读访问 ===

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn projects(&self) -> &[ProjectConfig] {
        &self.config.projects
    }

    pub fn project(&self, id: &str) -> Option<&ProjectConfig> {
        self.config.projects.iter().find(|p| p.id == id)
    }

    pub fn project_state(&self, id: &str) -> Option<&ProjectState> {
        self.project_states.get(id)
    }

    pub fn active_project(&self) -> Option<&ProjectConfig> {
        self.active_project_id.as_deref().and_then(|id| self.project(id))
    }

    pub fn active_layout(&self) -> Option<&SplitNode> {
        self.active_project_id
            .as_deref()
            .and_then(|id| self.project_states.get(id))
            .and_then(|s| s.layout.as_ref())
    }

    pub fn terminal(&self, pty_id: u32) -> Option<&Entity<TerminalPane>> {
        self.terminals.get(&pty_id)
    }

    // === 项目 ===

    pub fn set_active_project(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.active_project_id.as_deref() == Some(id) {
            return;
        }
        self.active_project_id = Some(id.to_string());
        if let Some(state) = self.project_states.get_mut(id) {
            state.needs_attention = false;
        }
        self.config.last_active_project_id = Some(id.to_string());
        // 切过去才起 PTY:恢复出来的布局在这一刻补齐(旧版的懒创建时机)
        self.hydrate_project(id, cx);
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 添加项目(目录路径)。名字取目录名。
    pub fn add_project(&mut self, path: &Path, cx: &mut Context<Self>) {
        let path_str = path.to_string_lossy().to_string();
        if let Some(existing) = self
            .config
            .projects
            .iter()
            .find(|p| p.path == path_str)
            .map(|p| p.id.clone())
        {
            self.set_active_project(&existing, cx);
            return;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path_str.clone());
        let id = gen_id("proj");

        self.config.projects.push(ProjectConfig {
            id: id.clone(),
            name,
            path: path_str,
            description: None,
            saved_layout: None,
            expanded_dirs: Vec::new(),
            ssh_mcp_enabled: false,
            ssh_cli_token: None,
            ssh_connection_ids: None,
            env_vars: Vec::new(),
            wsl_sessions_distro: None,
            ssh_connection_id: None,
            parent_project_id: None,
            kind_override: None,
        });
        // projectTree 是「分组 + 排序」那一层;这里只保证新项目出现在树里,
        // 分组编辑是后续批次的事。
        let tree = self.config.project_tree.get_or_insert_with(Vec::new);
        tree.push(mt_config::ProjectTreeItem::ProjectId(id.clone()));

        self.project_states.insert(id.clone(), ProjectState::new());
        self.expanded_dirs.insert(id.clone(), HashSet::new());
        self.active_project_id = Some(id.clone());
        self.config.last_active_project_id = Some(id);
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 移除项目:先回收它所有 pane 的 PTY,再从配置里摘掉。
    pub fn remove_project(&mut self, id: &str, cx: &mut Context<Self>) {
        let pty_ids: Vec<u32> = self
            .project_states
            .get(id)
            .and_then(|s| s.layout.as_ref())
            .map(|l| l.pty_ids())
            .unwrap_or_default();
        for pty_id in pty_ids {
            self.dispose_terminal(pty_id, cx);
        }

        self.project_states.remove(id);
        self.expanded_dirs.remove(id);
        self.done.retain_panes(&self.live_pane_ids());
        self.config.projects.retain(|p| p.id != id);
        if let Some(tree) = self.config.project_tree.as_mut() {
            remove_from_tree(tree, id);
        }
        if self.active_project_id.as_deref() == Some(id) {
            self.active_project_id = self.config.projects.first().map(|p| p.id.clone());
            self.config.last_active_project_id = self.active_project_id.clone();
        }
        self.save_config_soon(cx);
        cx.notify();
    }

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
        let project = self.project(project_id)?.clone();
        let shell = shell.or_else(|| self.resolve_shell(None))?;
        let pane = self.spawn_pane(&project, &shell, None, window, cx)?;
        let pane_id = pane.id.clone();

        let anchor = anchor_pane_id.or_else(|| self.focused_pane_id.clone());
        let state = self.project_states.get_mut(project_id)?;
        match state.layout.as_mut() {
            None => state.layout = Some(SplitNode::leaf(pane)),
            Some(layout) => {
                let anchor = anchor.filter(|id| layout.pane(id).is_some());
                layout.append_pane(anchor.as_deref(), pane);
            }
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
        let project = self.project(project_id)?.clone();
        let source_cwd = self
            .project_states
            .get(project_id)
            .and_then(|s| s.layout.as_ref())
            .and_then(|l| l.pane(pane_id))
            .and_then(|p| p.cwd.clone());
        let shell_name = self
            .project_states
            .get(project_id)
            .and_then(|s| s.layout.as_ref())
            .and_then(|l| l.pane(pane_id))
            .map(|p| p.shell_name.clone());
        let shell = self.resolve_shell(shell_name.as_deref())?;

        let pane = self.spawn_pane(&project, &shell, source_cwd, window, cx)?;
        let new_pane_id = pane.id.clone();
        let new_leaf = SplitNode::leaf(pane);

        let state = self.project_states.get_mut(project_id)?;
        let Some(layout) = state.layout.as_mut() else {
            return None;
        };
        if !layout.insert_split(pane_id, direction, new_leaf) {
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
        self.after_layout_change(project_id, cx);
        self.focus_pane(project_id, &new_pane_id, window, cx);
        Some(new_pane_id)
    }

    /// 关闭一个 pane:回收 PTY,再把它从树里摘掉(树空了 = 项目回到空态)。
    pub fn close_pane(&mut self, project_id: &str, pane_id: &str, cx: &mut Context<Self>) {
        let pty_id = self
            .project_states
            .get(project_id)
            .and_then(|s| s.layout.as_ref())
            .and_then(|l| l.pane(pane_id))
            .and_then(|p| p.pty_id);
        if let Some(pty_id) = pty_id {
            self.dispose_terminal(pty_id, cx);
        }
        let Some(state) = self.project_states.get_mut(project_id) else {
            return;
        };
        if let Some(layout) = state.layout.take() {
            state.layout = layout.remove_pane(pane_id);
        }
        if self.focused_pane_id.as_deref() == Some(pane_id) {
            self.focused_pane_id = self
                .project_states
                .get(project_id)
                .and_then(|s| s.layout.as_ref())
                .and_then(|l| l.first_active_pane())
                .map(|p| p.id.clone());
        }
        self.after_layout_change(project_id, cx);
    }

    /// 关掉某个 pane **所在的整组**(Ctrl+Shift+W 的落点)。
    pub fn close_leaf_of_pane(&mut self, project_id: &str, pane_id: &str, cx: &mut Context<Self>) {
        let leaf_id = self
            .project_states
            .get(project_id)
            .and_then(|s| s.layout.as_ref())
            .and_then(|l| l.leaf_of_pane(pane_id))
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
            .and_then(|s| s.layout.as_ref())
            .map(|l| match l.node(leaf_id) {
                Some(SplitNode::Leaf { panes, .. }) => {
                    panes.iter().map(|p| p.id.clone()).collect()
                }
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
        if let Some(state) = self.project_states.get_mut(project_id)
            && let Some(layout) = state.layout.as_mut()
        {
            layout.activate_pane(pane_id);
        }
        self.focus_pane(project_id, pane_id, window, cx);
        self.save_layout_soon(project_id, cx);
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
            .and_then(|s| s.layout.as_ref())
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
            .and_then(|s| s.layout.as_ref())
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
            .filter_map(|s| s.layout.as_ref())
            .find_map(|l| l.pane(&pane_id).and_then(|p| p.pty_id));
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
            .and_then(|s| s.layout.as_ref())
            .and_then(|l| l.pane(pane_id))
            .and_then(|p| p.pty_id);
        if let Some(entity) = pty_id.and_then(|id| self.terminals.get(&id)) {
            entity.update(cx, |pane, _| pane.focus(window));
        }
        cx.notify();
    }

    /// 当前项目里该操作哪个 pane:焦点 pane → 布局里第一个激活 pane
    /// (旧版 `resolveActivePane`,它以 DOM 焦点为准)。
    pub fn active_pane_id(&self, project_id: &str) -> Option<String> {
        let layout = self.project_states.get(project_id)?.layout.as_ref()?;
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
            .and_then(|s| s.layout.as_ref())
            .and_then(|l| l.pane(pane_id))
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
            .and_then(|s| s.layout.as_mut())
            .and_then(|l| l.node_mut(node_id))
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
            self.save_layout_soon(project_id, cx);
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
        let pending: Vec<Pending> = self
            .project_states
            .get(project_id)
            .and_then(|s| s.layout.as_ref())
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
                    && let Some(layout) = state.layout.as_mut()
                    && let Some(pane) = layout.pane_mut(&item.pane_id)
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
                && let Some(layout) = state.layout.as_mut()
                && let Some(pane) = layout.pane_mut(&item.pane_id)
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
                && let Some(layout) = state.layout.as_mut()
                && let Some(pane) = layout.pane_mut(&item.pane_id)
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
                self.save_layout_soon(project_id, cx);
            }
        }
        cx.notify();
    }

    /// 起 PTY 并拼出 `PaneState`。
    fn spawn_pane(
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
    fn start_pty(
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

        let spec = PtySpawn {
            program: shell.command.clone(),
            args: shell.args.clone().unwrap_or_default(),
            cwd: Some(cwd.clone()),
            env,
            rows: mt_pty::INITIAL_PTY_ROWS,
            cols: mt_pty::INITIAL_PTY_COLS,
        };
        // 项目级环境变量走 user_env —— 它会被 `MINITERM_` 前缀过滤挡一道,
        // 用户手改 config.json 也覆盖不掉内部协议变量。
        let user_env: Vec<(String, String)> = project
            .env_vars
            .iter()
            .filter(|v| v.enabled)
            .map(|v| (v.key.clone(), v.value.clone()))
            .collect();

        let style = self.terminal_style();
        let theme = self.terminal_theme.clone();
        let ai = self.ai.clone();
        let entity = cx.new(|cx| {
            TerminalPane::new(pty_id, spec, user_env, style, theme, ai, cx)
        });

        // 子进程退出 → pane 状态 error(与旧版 pty-exit 同语义);
        // 用户键入 → 清 attention 黄灯(与旧版 clearPaneAttentionByPty 同语义)
        let sub = cx.subscribe(&entity, move |store, _entity, event: &PaneEvent, cx| {
            match event {
                PaneEvent::Exited(code) => store.on_pty_exit(pty_id, *code, cx),
                PaneEvent::UserInput => store.clear_pane_attention_by_pty(pty_id, cx),
            }
        });
        self.pane_subs.insert(pty_id, sub);
        self.terminals.insert(pty_id, entity);
        pty_id
    }

    fn terminal_style(&self) -> TerminalStyle {
        let mut style = TerminalStyle::default();
        style.font_size = gpui::px(self.config.terminal_font_size as f32);
        if let Some(family) = &self.config.terminal_font_family
            && !family.trim().is_empty()
        {
            style.font_family = family.clone().into();
        }
        style
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

    /// 回收一个终端:kill 子进程 + 清 AI 感知痕迹 + 摘掉视图与订阅。
    fn dispose_terminal(&mut self, pty_id: u32, cx: &mut Context<Self>) {
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

    fn pty_in_any_layout(&self, pty_id: u32) -> bool {
        self.project_states
            .values()
            .filter_map(|s| s.layout.as_ref())
            .any(|l| l.pane_by_pty(pty_id).is_some())
    }

    /// 子进程退出:pane 落 `error`。
    ///
    /// 旧版就是这个语义(`pty-exit` → `updatePaneStatusByPty('error')`):pane 不
    /// 自动关闭,用户主动 `exit` 与异常断开不做区分,画面留在原地可回看。
    fn on_pty_exit(&mut self, pty_id: u32, code: Option<u32>, cx: &mut Context<Self>) {
        if let Some(code) = code
            && code != 0
        {
            eprintln!("[store] pane {pty_id} 子进程退出,退出码 {code}");
        }
        let mut touched: Option<String> = None;
        for (pid, state) in self.project_states.iter_mut() {
            if let Some(layout) = state.layout.as_mut()
                && layout.update_status_by_pty(pty_id, PaneStatus::Error, false, None)
            {
                state.status = layout.highest_status();
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
                for (pid, state) in self.project_states.iter_mut() {
                    let Some(layout) = state.layout.as_mut() else {
                        continue;
                    };
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
                    state.status = layout.highest_status();
                    owner = Some(pid.clone());
                    break;
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
                for (pid, state) in self.project_states.iter_mut() {
                    if let Some(layout) = state.layout.as_mut()
                        && let Some(pane) = layout.pane_by_pty_mut(identity.pty_id)
                    {
                        pane.ai_session = Some(AiSessionRef {
                            agent: identity.agent.clone(),
                            session_id: identity.session_id.clone(),
                            cwd: identity.cwd.clone(),
                        });
                        owner = Some(pid.clone());
                        break;
                    }
                }
                // 会话身份随布局落盘 —— 重启后据此续接
                if let Some(owner) = owner {
                    self.save_layout_soon(&owner, cx);
                    cx.notify();
                }
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

    /// 未读完成数(旧版托盘绿灯的计数,这里给壳内徽章用)。
    pub fn unread_done_count(&self) -> usize {
        self.done.unread_count()
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
        let refs: Vec<PaneRef<'_>> = self
            .project_states
            .iter()
            .filter(|(pid, _)| only_project.is_none_or(|only| only == pid.as_str()))
            .filter_map(|(pid, state)| state.layout.as_ref().map(|l| (pid, l)))
            .flat_map(|(pid, layout)| {
                layout.panes().into_iter().map(move |p| PaneRef {
                    project_id: pid.as_str(),
                    pane_id: p.id.as_str(),
                    status: p.status,
                    attention: p.attention,
                })
            })
            .collect();
        crate::notify::pick_attention_target(refs, self.done.order())
    }

    /// 用户对 pane 键入 = 已在处理待确认事项,清掉 attention 黄灯
    /// (旧版 `clearPaneAttentionByPty`)。
    ///
    /// codex 批准后直到 PostToolUse 没有任何 hook 事件,不清会误挂整个执行期。
    pub fn clear_pane_attention_by_pty(&mut self, pty_id: u32, cx: &mut Context<Self>) {
        let mut changed = false;
        for state in self.project_states.values_mut() {
            let Some(layout) = state.layout.as_mut() else {
                continue;
            };
            if let Some(pane) = layout.pane_by_pty_mut(pty_id)
                && pane.attention
            {
                pane.attention = false;
                changed = true;
                break;
            }
        }
        if changed {
            cx.notify();
        }
    }

    // === 主题 ===

    /// 按当前配置装配主题:gpui-component 主题层 + 壳配色 + 终端配色。
    ///
    /// **已存在的终端也热更新** —— 对应旧版
    /// `terminalCache.ts::updateAllTerminalThemes`,不然换主题只有新开的终端跟着变。
    ///
    /// 启动、切亮暗、切皮肤、开关 `terminalFollowTheme` 全走这一条路。
    pub fn apply_theme_from_config(
        &mut self,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let applied = crate::theme::apply(&self.config, window, cx);
        if let Some(failed) = &applied.failed_pack {
            // 只清内存不落盘:主题目录可能只是这次读不到(盘没挂载、文件正被替换),
            // 落盘会把用户的选择永久抹掉,下次启动就找不回来了(旧版同一红线)。
            self.config.custom_theme_id = None;
            eprintln!("[store] 主题包 {failed} 本次不可用,已回落内置外观(配置未改盘)");
        }
        crate::ui::set_palette(applied.palette);
        self.background_art = applied.background;
        self.terminal_theme = applied.terminal.clone();

        let entities: Vec<Entity<TerminalPane>> = self.terminals.values().cloned().collect();
        for entity in entities {
            let theme = applied.terminal.clone();
            entity.update(cx, |pane, cx| pane.set_theme(theme, cx));
        }
        cx.notify();
    }

    /// 当前主题的背景图参数(渲染归 mt-ui,这里只是取数口)。
    #[allow(dead_code)] // 消费方是 mt-ui 的背景图渲染,尚未落地
    pub fn background_art(&self) -> Option<&BackgroundArt> {
        self.background_art.as_ref()
    }

    /// 切内置亮/暗/跟随系统(`light` / `dark` / `auto`)。
    ///
    /// **切亮暗 = 退出外置皮肤** —— 皮肤的明暗由作者在 `theme.json` 里定死,
    /// 留着它这一步就没有效果(旧版 themePackManager 的同一条约定)。
    #[allow(dead_code)] // 设置面板「外观」页的落点(下一批)
    pub fn set_theme_mode(&mut self, mode: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.config.theme = mode.to_string();
        self.config.custom_theme_id = None;
        self.apply_theme_from_config(Some(window), cx);
        self.save_config_soon(cx);
    }

    /// 切外置主题包;`None` = 退出皮肤回内置外观。
    ///
    /// 装不上返回 `false` 且**不落盘**:内存里已经回落内置,配置里那条
    /// `customThemeId` 不该被这次失败改掉。
    #[allow(dead_code)] // 设置面板「外观」页的落点(下一批)
    pub fn set_theme_pack(
        &mut self,
        theme_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.config.custom_theme_id = theme_id.clone();
        self.apply_theme_from_config(Some(window), cx);
        if self.config.custom_theme_id != theme_id {
            return false;
        }
        self.save_config_soon(cx);
        true
    }

    /// 终端配色跟不跟随主题。关掉 = 终端固定内置暗色(旧版同一行为)。
    #[allow(dead_code)] // 设置面板「外观」页的落点(下一批)
    pub fn set_terminal_follow_theme(
        &mut self,
        follow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.config.terminal_follow_theme == follow {
            return;
        }
        self.config.terminal_follow_theme = follow;
        self.apply_theme_from_config(Some(window), cx);
        self.save_config_soon(cx);
    }

    // === 终端配置(shell 列表)===

    pub fn shell_list(&self) -> ShellList {
        ShellList {
            shells: self.config.available_shells.clone(),
            default_shell: self.config.default_shell.clone(),
        }
    }

    pub fn apply_shell_list(&mut self, list: ShellList, cx: &mut Context<Self>) {
        self.config.available_shells = list.shells;
        self.config.default_shell = list.default_shell;
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 终端字号。改完立刻作用于**新建**的终端;已开的终端沿用创建时的样式
    /// (旧版靠 xterm 的 options 热改,自研渲染器的样式热更新留给渲染批)。
    pub fn set_terminal_font_size(&mut self, size: f64, cx: &mut Context<Self>) {
        let size = size.clamp(8.0, 32.0);
        if (self.config.terminal_font_size - size).abs() < f64::EPSILON {
            return;
        }
        self.config.terminal_font_size = size;
        self.save_config_soon(cx);
        cx.notify();
    }

    // === 界面语言 ===

    /// 当前界面语言。取自配置,认不出(或没设过)时回落到**进程内实际生效**的那个
    /// —— 也就是启动时 `i18n::install` 按系统语言探测出来的结果,这样语言切换
    /// 段控件的高亮与眼前看到的文案始终一致。
    pub fn locale(&self) -> mt_i18n::Locale {
        self.config
            .locale
            .as_deref()
            .and_then(mt_i18n::Locale::from_code)
            .unwrap_or_else(mt_i18n::locale)
    }

    /// 切界面语言。对应 TS 侧 `useI18nStore.setLang`,只是落点从 localStorage
    /// 换成了 `config.locale`(GPUI 没有 localStorage,配置文件是唯一的持久层)。
    ///
    /// **一定要落盘**:探测出来的语言不写、用户选的语言必写 —— 否则下次启动又被
    /// 系统语言盖回去,选择等于没生效。
    pub fn set_locale(&mut self, locale: mt_i18n::Locale, cx: &mut Context<Self>) {
        let code = locale.code().to_string();
        if self.config.locale.as_deref() == Some(code.as_str()) && mt_i18n::locale() == locale {
            return;
        }
        self.config.locale = Some(code);
        // 进程内切换 + 全窗口重绘(观察者顺带把 gpui-component 的 rust-i18n 也改了)
        crate::i18n::switch(locale, cx);
        self.save_config_soon(cx);
        cx.notify();
    }

    // === pane 重命名 ===

    /// 改 tab 标题。空字符串 = 恢复默认(shell 名)。
    ///
    /// **不落盘** —— `SavedPane` 里没有这个字段,装机版同样只在运行时保留
    /// (`serializeSplitNode` 只写 shellName/cwd/aiSession)。磁盘格式一字不改。
    pub fn rename_pane(
        &mut self,
        project_id: &str,
        pane_id: &str,
        title: &str,
        cx: &mut Context<Self>,
    ) {
        let title = title.trim();
        if let Some(state) = self.project_states.get_mut(project_id)
            && let Some(layout) = state.layout.as_mut()
            && let Some(pane) = layout.pane_mut(pane_id)
        {
            pane.custom_title = if title.is_empty() {
                None
            } else {
                Some(title.to_string())
            };
            cx.notify();
        }
    }

    // === 右侧抽屉宽度 ===

    pub fn right_drawer_width(&self) -> f64 {
        self.config.right_drawer_width.unwrap_or(320.0).clamp(240.0, 720.0)
    }

    pub fn set_right_drawer_width(&mut self, width: f64, cx: &mut Context<Self>) {
        let width = width.clamp(240.0, 720.0);
        if self.config.right_drawer_width == Some(width) {
            return;
        }
        self.config.right_drawer_width = Some(width);
        self.save_config_soon(cx);
    }

    // === 文件树展开状态 ===

    pub fn is_dir_expanded(&self, project_id: &str, path: &str) -> bool {
        self.expanded_dirs
            .get(project_id)
            .map(|set| set.contains(path))
            .unwrap_or(false)
    }

    pub fn set_dir_expanded(
        &mut self,
        project_id: &str,
        path: &str,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        let set = self.expanded_dirs.entry(project_id.to_string()).or_default();
        if expanded {
            set.insert(path.to_string());
        } else {
            set.remove(path);
        }
        let dirs: Vec<String> = set.iter().cloned().collect();
        if let Some(project) = self
            .config
            .projects
            .iter_mut()
            .find(|p| p.id == project_id)
        {
            project.expanded_dirs = dirs;
        }
        self.save_config_soon(cx);
        cx.notify();
    }

    // === 三栏尺寸 ===

    pub fn set_layout_sizes(&mut self, sizes: Vec<f64>, cx: &mut Context<Self>) {
        if self.config.layout_sizes.as_ref() == Some(&sizes) {
            return;
        }
        self.config.layout_sizes = Some(sizes);
        self.save_config_soon(cx);
    }

    pub fn set_middle_column_sizes(&mut self, sizes: Vec<f64>, cx: &mut Context<Self>) {
        if self.config.middle_column_sizes.as_ref() == Some(&sizes) {
            return;
        }
        self.config.middle_column_sizes = Some(sizes);
        self.save_config_soon(cx);
    }

    pub fn toggle_middle_column(&mut self, cx: &mut Context<Self>) {
        self.config.middle_column_visible = !self.config.middle_column_visible;
        self.save_config_soon(cx);
        cx.notify();
    }

    // === 持久化 ===

    fn after_layout_change(&mut self, project_id: &str, cx: &mut Context<Self>) {
        if let Some(state) = self.project_states.get_mut(project_id) {
            state.status = state
                .layout
                .as_ref()
                .map(|l| l.highest_status())
                .unwrap_or(PaneStatus::Idle);
        }
        // 关掉的 pane 一并撤出完成队列:否则未读计数会往一个已经不存在的 pane
        // 上跳,两张表也会随开关终端无界增长(旧版 setProjectLayout 的同一段)。
        self.done.retain_panes(&self.live_pane_ids());
        self.save_layout_soon(project_id, cx);
        cx.notify();
    }

    /// 全部项目里活着的 pane id。
    fn live_pane_ids(&self) -> HashSet<String> {
        self.project_states
            .values()
            .filter_map(|s| s.layout.as_ref())
            .flat_map(|l| l.panes().into_iter().map(|p| p.id.clone()))
            .collect()
    }

    /// 全部项目里活着的 split/leaf 节点 id —— 供 `TerminalArea` 回收分隔条状态。
    pub fn live_node_ids(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        for state in self.project_states.values() {
            if let Some(layout) = state.layout.as_ref() {
                collect_node_ids(layout, &mut out);
            }
        }
        out
    }

    fn save_layout_soon(&mut self, project_id: &str, cx: &mut Context<Self>) {
        let saved = self
            .project_states
            .get(project_id)
            .map(|s| persist::serialize_layout(s.layout.as_ref()));
        if let Some(saved) = saved
            && let Some(project) = self
                .config
                .projects
                .iter_mut()
                .find(|p| p.id == project_id)
        {
            project.saved_layout = Some(saved);
        }
        self.save_config_soon(cx);
    }

    /// 防抖写盘(500ms,与旧版 `saveLayoutToConfig` 同节奏)。
    pub fn save_config_soon(&mut self, cx: &mut Context<Self>) {
        self.save_generation += 1;
        let generation = self.save_generation;
        self._save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let _ = this.update(cx, |store, _cx| {
                if store.save_generation == generation {
                    store.save_config_now();
                }
            });
        }));
    }

    /// 立即写盘(退出前 / 项目切换)。
    ///
    /// 令牌语义与装机版一致:令牌过期说明别处写过配置,必须先重读拿到新令牌。
    /// 单进程壳里「别处」只可能是本进程的另一次 load,手上这份就是最新的,
    /// 于是重读一次令牌后原样重写。
    pub fn save_config_now(&mut self) {
        if self.token == 0 {
            return; // 配置没加载成功过,不许写盘覆盖磁盘
        }
        match self.config_store.save(self.token, &self.config) {
            Ok(()) => {}
            Err(SaveError::StaleToken { .. }) => match self.config_store.load() {
                Ok(loaded) => {
                    self.token = loaded.token;
                    if let Err(err) = self.config_store.save(self.token, &self.config) {
                        eprintln!("[store] 配置重试保存失败: {err}");
                    }
                }
                Err(err) => eprintln!("[store] 令牌过期后重读配置失败: {err:#}"),
            },
            Err(err) => eprintln!("[store] 配置保存失败: {err}"),
        }
    }
}

/// 启动恢复某个 pane 时该不该自动续接、续接命令是什么
/// (逐条对照 `src/utils/aiResume.ts::resolveAutoResumeCommand`)。
///
/// 汇总全部否决条件,返回 `None` = 不写命令:
/// - `enabled`:系统设置里的「启动自动续接 AI 会话」开关(`config.aiAutoResume`,
///   缺省开启)。关掉只影响写不写命令,`ai_session` 身份照旧随布局持久化;
/// - `resume_pending`:布局恢复置位、写一次即清,防重复写;
/// - `remote`:远程 pane 的 PTY 是 ssh 启动器,启动初期可能停在口令交互上,
///   预写的命令会被当口令消费;
/// - id 非法:见 [`build_resume_command`] 的白名单。
///
/// **`enabled == false` 时调用方不该清 `resume_pending`** —— 标记的语义是
/// 「这个 pane 还没续过」,不是「这次启动没续」;清了开关中途打开也续不上。
pub fn resolve_auto_resume_command(
    enabled: bool,
    resume_pending: bool,
    session: Option<&AiSessionRef>,
    remote: bool,
) -> Option<String> {
    if !enabled || !resume_pending || remote {
        return None;
    }
    let session = session?;
    build_resume_command(session.agent.as_deref().unwrap_or(""), &session.session_id)
}

/// 续接时 PTY 该以哪个目录启动(`PaneGroup.tsx` 的 `resolveResumeCwd`)。
///
/// 会话记录里带 cwd 就用它;存量记录没有就按 id 反查 jsonl —— `claude --resume`
/// 只认「启动目录」对应的会话桶,起于子目录的会话在项目根恢复会报
/// `No conversation found`。**codex 的会话不按目录分桶,不反查**。
///
/// 目录不在盘上(worktree 移除、项目搬家)一律当查不到:那本是「续接得更准」的
/// 优化,不该把 pane 拖成起不来 —— 退回 pane 自己的 cwd / 项目根,
/// 大不了 resume 找不到会话桶。
fn resolve_resume_cwd(session: &AiSessionRef) -> Option<String> {
    if let Some(cwd) = session.cwd.as_deref() {
        return Path::new(cwd).is_dir().then(|| cwd.to_string());
    }
    if session.agent.as_deref() == Some("codex") {
        return None;
    }
    mt_ai::sessions::lookup_ai_session_cwd(session.session_id.clone())
}

fn collect_node_ids(node: &SplitNode, out: &mut HashSet<String>) {
    out.insert(node.id().to_string());
    if let SplitNode::Split { children, .. } = node {
        for c in children {
            collect_node_ids(c, out);
        }
    }
}

/// 从 projectTree 里摘掉一个项目(递归进分组)。
fn remove_from_tree(tree: &mut Vec<mt_config::ProjectTreeItem>, project_id: &str) {
    tree.retain_mut(|item| match item {
        mt_config::ProjectTreeItem::ProjectId(id) => id != project_id,
        mt_config::ProjectTreeItem::Group(group) => {
            remove_from_tree(&mut group.children, project_id);
            true
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(agent: Option<&str>, id: &str) -> AiSessionRef {
        AiSessionRef {
            agent: agent.map(str::to_string),
            session_id: id.to_string(),
            cwd: None,
        }
    }

    /// 命令按 agent 分派;未知 / 缺省 agent 兜底 claude(与旧版一致)。
    #[test]
    fn 自动续接命令按_agent_分派() {
        let s = session(Some("codex"), "rollout_9");
        assert_eq!(
            resolve_auto_resume_command(true, true, Some(&s), false).as_deref(),
            Some("codex resume rollout_9")
        );
        let s = session(Some("grok"), "0199-x");
        assert_eq!(
            resolve_auto_resume_command(true, true, Some(&s), false).as_deref(),
            Some("grok --resume 0199-x")
        );
        let s = session(None, "abc-123");
        assert_eq!(
            resolve_auto_resume_command(true, true, Some(&s), false).as_deref(),
            Some("claude --resume abc-123")
        );
    }

    /// 四条否决条件逐条生效。
    #[test]
    fn 自动续接的四条否决() {
        let s = session(Some("claude"), "abc-123");
        // 开关关掉
        assert!(resolve_auto_resume_command(false, true, Some(&s), false).is_none());
        // 标记已清(写过一次了)
        assert!(resolve_auto_resume_command(true, false, Some(&s), false).is_none());
        // 远程 pane
        assert!(resolve_auto_resume_command(true, true, Some(&s), true).is_none());
        // 没有会话身份
        assert!(resolve_auto_resume_command(true, true, None, false).is_none());
    }

    /// id 白名单:这条命令是要原样写进 PTY 的,shell 元字符一律拦下。
    #[test]
    fn 自动续接的_id_白名单() {
        for bad in ["a b", "a;rm -rf /", "a|b", "a\nb", "a$(x)", "a`x`", "a'b", ""] {
            let s = session(Some("claude"), bad);
            assert!(
                resolve_auto_resume_command(true, true, Some(&s), false).is_none(),
                "应拒绝: {bad:?}"
            );
        }
    }

    /// 会话 cwd:目录不在盘上一律当查不到,不能把 pane 拖成起不来。
    #[test]
    fn 会话目录不存在时不作数() {
        let mut s = session(Some("claude"), "abc-123");
        s.cwd = Some("D:/definitely-not-here/xyz".into());
        assert_eq!(resolve_resume_cwd(&s), None);

        let tmp = std::env::temp_dir();
        s.cwd = Some(tmp.to_string_lossy().to_string());
        assert_eq!(resolve_resume_cwd(&s), Some(tmp.to_string_lossy().to_string()));
    }

    /// codex 会话不按目录分桶 —— 没有 cwd 就是没有,不去反查。
    #[test]
    fn codex_会话不反查目录() {
        let s = session(Some("codex"), "rollout_9");
        assert_eq!(resolve_resume_cwd(&s), None);
    }
}
