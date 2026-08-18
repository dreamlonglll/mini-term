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
use mt_ui::{TerminalStyle, TerminalTheme};

use crate::ai::{AiBridge, AiEvent};
use crate::pane::{PaneEvent, TerminalPane};
use crate::persist;
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

    /// 展开的目录(按项目)。运行时态,落盘走 `ProjectConfig::expanded_dirs`。
    expanded_dirs: HashMap<String, HashSet<String>>,

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
            expanded_dirs,
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
        if let Some(state) = self.project_states.get_mut(project_id)
            && let Some(layout) = state.layout.as_mut()
        {
            layout.activate_pane(pane_id);
        }
        self.focus_pane(project_id, pane_id, window, cx);
        self.save_layout_soon(project_id, cx);
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
    pub fn hydrate_project(&mut self, project_id: &str, cx: &mut Context<Self>) {
        let Some(project) = self.project(project_id).cloned() else {
            return;
        };
        let pending: Vec<(String, String, Option<String>)> = self
            .project_states
            .get(project_id)
            .and_then(|s| s.layout.as_ref())
            .map(|l| {
                l.panes()
                    .into_iter()
                    .filter(|p| p.pty_id.is_none())
                    .map(|p| (p.id.clone(), p.shell_name.clone(), p.cwd.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if pending.is_empty() {
            return;
        }
        for (pane_id, shell_name, cwd) in pending {
            let Some(shell) = self.resolve_shell(Some(&shell_name)) else {
                continue;
            };
            let pty_id = self.start_pty(&project, &shell, cwd.as_deref(), cx);
            if let Some(state) = self.project_states.get_mut(project_id)
                && let Some(layout) = state.layout.as_mut()
                && let Some(pane) = layout.pane_mut(&pane_id)
            {
                pane.pty_id = Some(pty_id);
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
        let theme = TerminalTheme::default();
        let ai = self.ai.clone();
        let entity = cx.new(|cx| {
            TerminalPane::new(pty_id, spec, user_env, style, theme, ai, cx)
        });

        // 子进程退出 → pane 状态 error(与旧版 pty-exit 同语义)
        let sub = cx.subscribe(&entity, move |store, _entity, event: &PaneEvent, cx| {
            let PaneEvent::Exited(code) = event;
            store.on_pty_exit(pty_id, *code, cx);
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
            entity.update(cx, |pane, _| pane.shutdown());
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
    pub fn apply_ai_event(&mut self, event: AiEvent, cx: &mut Context<Self>) {
        match event {
            AiEvent::Status(change) => {
                let Some(status) = PaneStatus::from_str(&change.status) else {
                    return;
                };
                // attention 与状态解耦:codex 的 PermissionRequest 状态是 ai-working
                // 但同样要点黄灯。判定按事件名,与旧版 isAttentionCause 同一张表。
                let attention = change
                    .cause
                    .as_deref()
                    .map(mt_ai::is_attention_cause)
                    .unwrap_or(false);

                let mut owner: Option<String> = None;
                let mut old_status = PaneStatus::Idle;
                for (pid, state) in self.project_states.iter_mut() {
                    let Some(layout) = state.layout.as_mut() else {
                        continue;
                    };
                    let Some(pane) = layout.pane_by_pty(change.pty_id) else {
                        continue;
                    };
                    old_status = pane.status;
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
                let Some(owner) = owner else { return };

                // 完成判定:ai-working → ai-idle 的下降沿,且成因不是「等你确认」。
                // 无成因 = 无 hook 的降级路径,下降沿是唯一完成信号,放行。
                let is_completion = old_status == PaneStatus::AiWorking
                    && status == PaneStatus::AiIdle
                    && !attention;
                if is_completion
                    && self.active_project_id.as_deref() != Some(owner.as_str())
                    && let Some(state) = self.project_states.get_mut(&owner)
                {
                    state.needs_attention = true;
                }
                cx.notify();
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
            }
        }
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
        self.save_layout_soon(project_id, cx);
        cx.notify();
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
