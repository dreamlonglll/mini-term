//! 面板 / 布局 / 持久化相关的 `AppStore` 方法:项目级终端面板、右侧抽屉、
//! 文件树展开状态、三栏尺寸、终端列表竖条,以及布局库(`layout.db`)与
//! 配置(`config.db`)的落盘。
//!
//! 原 `store.rs` 里那个独立的 `impl AppStore` 块整块搬来,段注释随代码走,
//! 逻辑一行未改。

use std::collections::HashSet;
use std::time::Duration;

use gpui::{Context, Window};
use mt_config::ShellConfig;

use crate::persist;
use crate::tree::{ProjectPanel, SplitNode};

use super::events::StoreChanged;
use super::pure::collect_node_ids;
use super::{AppStore, ConfigSection, StoreEvent};

/// 提交详情栏的缺省宽度:比抽屉缺省的 340 宽一截,正文与带行数的文件行都摆得开。
pub const GIT_DETAIL_DEFAULT_WIDTH: f64 = 480.0;
/// 提交详情栏拖到最窄 / 最宽(拖拽与读库都钳在这里)。
pub const GIT_DETAIL_MIN_WIDTH: f64 = 320.0;
pub const GIT_DETAIL_MAX_WIDTH: f64 = 960.0;
/// 右侧抽屉拖到最窄(`RightDrawer.tsx:8`;拖拽与读库都钳在这里)。
///
/// **没有固定上限**(issue #107 用户要求;原版 `RightDrawer.tsx:9` 钳在 720):最宽只受
/// 窗口限制,由抽屉层按当时的视口再钳(`workspace::drawer_max_width`)。落盘值不封顶 ——
/// 窗口临时变窄时只在显示上收进窗口,窗口放回来宽度跟着回来。
pub const RIGHT_DRAWER_MIN_WIDTH: f64 = 240.0;

impl AppStore {
    /// 移动端发起会话时挂 pane:追加到布局树**最左侧叶子**的 tab 栏末尾,
    /// **不激活、不抢焦点、不切项目**(远程操作不抢桌面现场,
    /// `src/utils/mobileStartSession.ts:100-110`)。
    ///
    /// 与 [`Self::new_terminal`] 的差别只有这一条 —— 那个走「锚点叶子 + 激活 +
    /// 抢焦点」。**别把两者合并**:手机上点一下就把桌面正在看的终端顶掉,
    /// 是原版专门避开的行为。
    ///
    /// 原版步 6「先建终端实例再写命令」在这里**自动满足**:`spawn_pane` 建 PTY 的
    /// 同时就把 `TerminalPane` 插进 `self.terminals`,不存在旧版那个
    /// 「pty-output 到了但实例还没建、AI 起来那一整段输出丢在地上」的窗口期。
    pub fn append_pane_background(
        &mut self,
        project_id: &str,
        shell: ShellConfig,
        custom_title: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        self.append_pane_background_with_cwd(project_id, shell, custom_title, None, window, cx)
    }

    /// 移动端发起/恢复会话时挂后台 pane,支持显式指定启动目录(Claude 子目录会话
    /// `--resume` 需要落在原启动目录)。其余语义与 [`Self::append_pane_background`] 一致:
    /// 不激活、不抢焦点、不切项目。
    pub fn append_pane_background_with_cwd(
        &mut self,
        project_id: &str,
        shell: ShellConfig,
        custom_title: Option<String>,
        cwd: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let project = self.project(project_id)?.clone();
        let mut pane = self.spawn_pane(&project, &shell, cwd, window, cx)?;
        pane.custom_title = custom_title;
        let pane_id = pane.id.clone();
        let pty_id = pane.pty_id;

        let Some(state) = self.project_states.get_mut(project_id) else {
            // 项目在起 PTY 期间被移除了 —— 新 PTY 无处安放,显式回收
            if let Some(pty_id) = pty_id {
                self.dispose_terminal(pty_id, cx);
            }
            return None;
        };
        if state.panels.is_empty() {
            // 项目还一个终端都没有:新建面板(含根叶子),否则终端区仍是空白
            let panel = ProjectPanel::new(SplitNode::leaf(pane));
            state.active_panel_id = Some(panel.id.clone());
            state.panels.push(panel);
        } else {
            // 挂进**活动面板**的最左侧叶子 —— 「不抢桌面现场」的语义下也不该
            // 去动用户看不见的后台面板
            let Some(layout) = state.active_layout_mut() else {
                if let Some(pty_id) = pty_id {
                    self.dispose_terminal(pty_id, cx);
                }
                return None;
            };
            // `append_pane(None, ..)` 的落点正是 `first_leaf_id()` = 最左侧叶子,
            // 但它顺手把 `active_pane_id` 指到了新 pane 上,而原版
            // `appendPaneToFirstLeaf` 明确**不动 activePaneId** —— 记下原值再还原。
            let leaf_id = layout.first_leaf_id();
            let prev_active = leaf_id
                .as_deref()
                .and_then(|id| layout.node(id))
                .and_then(|node| match node {
                    SplitNode::Leaf { active_pane_id, .. } => Some(active_pane_id.clone()),
                    SplitNode::Split { .. } => None,
                });
            if !layout.append_pane(None, pane) {
                if let Some(pty_id) = pty_id {
                    self.dispose_terminal(pty_id, cx);
                }
                return None;
            }
            if let (Some(leaf_id), Some(prev)) = (leaf_id, prev_active)
                && let Some(SplitNode::Leaf { active_pane_id, .. }) = layout.node_mut(&leaf_id)
            {
                *active_pane_id = prev;
            }
        }
        self.after_layout_change(project_id, cx);
        Some(pane_id)
    }

    // === 项目级终端面板 ===

    /// 换活动面板。目标不存在 / 已是活动的都是 no-op。
    /// 切过去才起 PTY(与切项目同一懒创建时机);最大化态只对上一个面板有意义,
    /// 一并清掉;活动下标随布局落盘。
    pub fn set_active_panel(&mut self, project_id: &str, panel_id: &str, cx: &mut Context<Self>) {
        let Some(state) = self.project_states.get_mut(project_id) else {
            return;
        };
        if !state.panels.iter().any(|p| p.id == panel_id)
            || state.active_panel_id.as_deref() == Some(panel_id)
        {
            return;
        }
        state.active_panel_id = Some(panel_id.to_string());
        state.maximized_pane_id = None;
        self.hydrate_project(project_id, cx);
        self.save_project_layout_soon(project_id, cx);
        cx.changed(StoreEvent::LayoutChanged);
    }

    /// 换活动面板并把键盘焦点交给它当前激活的 pane(竖条点击的落点)。
    pub fn switch_panel(
        &mut self,
        project_id: &str,
        panel_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_panel(project_id, panel_id, cx);
        let target = self
            .project_states
            .get(project_id)
            .and_then(|s| s.active_layout())
            .and_then(|l| l.first_active_pane())
            .map(|p| p.id.clone());
        if let Some(pane_id) = target {
            self.focus_pane(project_id, &pane_id, window, cx);
        }
    }

    /// 新建一个项目级面板(单 pane 起步),设为活动并聚焦。
    pub fn new_panel(
        &mut self,
        project_id: &str,
        shell: Option<ShellConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let project = self.project(project_id)?.clone();
        let shell = shell.or_else(|| self.resolve_shell(None))?;
        let pane = self.spawn_pane(&project, &shell, None, window, cx)?;
        let pane_id = pane.id.clone();

        let state = self.project_states.get_mut(project_id)?;
        let panel = ProjectPanel::new(SplitNode::leaf(pane));
        state.active_panel_id = Some(panel.id.clone());
        state.panels.push(panel);
        state.maximized_pane_id = None;
        self.after_layout_change(project_id, cx);
        self.focus_pane(project_id, &pane_id, window, cx);
        Some(pane_id)
    }

    /// 关闭一整个面板(它的全部 pane)。复用 [`Self::close_pane`] 的回收链路,
    /// 最后一个 pane 关掉时面板自然消失、活动指针挪到邻位。
    pub fn close_panel(&mut self, project_id: &str, panel_id: &str, cx: &mut Context<Self>) {
        let pane_ids: Vec<String> = self
            .project_states
            .get(project_id)
            .and_then(|s| s.panels.iter().find(|p| p.id == panel_id))
            .map(|p| p.layout.panes().into_iter().map(|x| x.id.clone()).collect())
            .unwrap_or_default();
        for pane_id in pane_ids {
            self.close_pane(project_id, &pane_id, cx);
        }
    }

    /// 改面板名。空字符串 = 恢复默认(按序号显示)。
    /// 与 pane 改名不同,这个**落盘**(磁盘格式的 `SavedTab.customTitle` 本来就在)。
    pub fn rename_panel(
        &mut self,
        project_id: &str,
        panel_id: &str,
        title: &str,
        cx: &mut Context<Self>,
    ) {
        let title = title.trim();
        if let Some(state) = self.project_states.get_mut(project_id)
            && let Some(panel) = state.panel_mut(panel_id)
        {
            panel.custom_title = if title.is_empty() {
                None
            } else {
                Some(title.to_string())
            };
            self.save_project_layout_soon(project_id, cx);
            cx.changed(StoreEvent::LayoutChanged);
        }
    }

    // === 右侧抽屉宽度 ===

    /// 抽屉宽度。缺省 **340**(`App.tsx:541` 的 `?? 340`),不窄于
    /// [`RIGHT_DRAWER_MIN_WIDTH`];上限不在这里钳,见该常量注释。
    pub fn right_drawer_width(&self) -> f64 {
        self.config
            .right_drawer_width
            .unwrap_or(340.0)
            .max(RIGHT_DRAWER_MIN_WIDTH)
    }

    // === Git 「更改」区的视图模式 ===

    /// `config.gitChangesViewMode`。**是 String 不是枚举**(磁盘格式与装机版共用),
    /// 手改成坏值不能拖垮整份 config —— 认不出一律回落 `"list"`(照 `locale` 的做法)。
    pub fn git_changes_view_mode(&self) -> &str {
        match self.config.git_changes_view_mode.as_str() {
            "tree" => "tree",
            _ => "list",
        }
    }

    pub fn set_git_changes_view_mode(&mut self, mode: &str, cx: &mut Context<Self>) {
        let mode = if mode == "tree" { "tree" } else { "list" };
        if self.config.git_changes_view_mode == mode {
            return;
        }
        self.config.git_changes_view_mode = mode.to_string();
        self.save_config_soon(cx);
        cx.changed(StoreEvent::Config(ConfigSection::View));
    }

    // === 提交详情 / 提交 diff 左栏的改动文件视图 ===

    /// `config.gitCommitFilesViewMode`:是不是树形(缺省是)。与 [`Self::git_changes_view_mode`]
    /// 同样存成 String,认不出的值回落缺省的树形。
    pub fn git_commit_files_tree(&self) -> bool {
        self.config.git_commit_files_view_mode != "list"
    }

    pub fn set_git_commit_files_tree(&mut self, tree: bool, cx: &mut Context<Self>) {
        if self.git_commit_files_tree() == tree {
            return;
        }
        self.config.git_commit_files_view_mode = if tree { "tree" } else { "list" }.to_string();
        self.save_config_soon(cx);
        cx.changed(StoreEvent::Config(ConfigSection::View));
    }

    pub fn set_right_drawer_width(&mut self, width: f64, cx: &mut Context<Self>) {
        let width = width.max(RIGHT_DRAWER_MIN_WIDTH);
        if self.config.right_drawer_width == Some(width) {
            return;
        }
        self.config.right_drawer_width = Some(width);
        self.save_layout_soon(cx);
    }

    // === Git 抽屉左侧的「提交详情」栏 ===

    /// 提交详情栏的宽度,钳在 [`GIT_DETAIL_MIN_WIDTH`]~[`GIT_DETAIL_MAX_WIDTH`]。窗口窄时
    /// 实际画多宽由抽屉层再按剩余空间让(见 `main.rs` 的 `render_commit_detail_layer`)。
    pub fn git_detail_width(&self) -> f64 {
        self.git_detail_width
            .unwrap_or(GIT_DETAIL_DEFAULT_WIDTH)
            .clamp(GIT_DETAIL_MIN_WIDTH, GIT_DETAIL_MAX_WIDTH)
    }

    pub fn set_git_detail_width(&mut self, width: f64, cx: &mut Context<Self>) {
        let width = width.clamp(GIT_DETAIL_MIN_WIDTH, GIT_DETAIL_MAX_WIDTH);
        if self.git_detail_width == Some(width) {
            return;
        }
        self.git_detail_width = Some(width);
        self.save_layout_soon(cx);
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
        cx.changed(StoreEvent::ExpandedDirsChanged);
    }

    /// 文件树头部「全部折叠」:清空这个项目的展开态并落盘,返回清掉之前展开着的那些
    /// (文件树据此摘掉它们的目录监听,与逐个折叠同一口径)。
    ///
    /// 展开态是落盘的(`ProjectConfig::expanded_dirs`),早就看不见的深层目录也一直留在
    /// 表里 —— 这是唯一能把它们一次清干净的入口。本来就是空的不写盘、不发事件。
    pub fn collapse_all_dirs(&mut self, project_id: &str, cx: &mut Context<Self>) -> Vec<String> {
        let collapsed: Vec<String> = self
            .expanded_dirs
            .get_mut(project_id)
            .map(|set| set.drain().collect())
            .unwrap_or_default();
        let persisted = self
            .config
            .projects
            .iter_mut()
            .find(|p| p.id == project_id)
            .map(|project| std::mem::take(&mut project.expanded_dirs))
            .unwrap_or_default();
        if collapsed.is_empty() && persisted.is_empty() {
            return collapsed;
        }
        self.save_config_soon(cx);
        cx.changed(StoreEvent::ExpandedDirsChanged);
        collapsed
    }

    // === 三栏尺寸 ===

    pub fn set_layout_sizes(&mut self, sizes: Vec<f64>, cx: &mut Context<Self>) {
        if self.config.layout_sizes.as_ref() == Some(&sizes) {
            return;
        }
        self.config.layout_sizes = Some(sizes);
        self.save_layout_soon(cx);
    }

    pub fn set_middle_column_sizes(&mut self, sizes: Vec<f64>, cx: &mut Context<Self>) {
        if self.config.middle_column_sizes.as_ref() == Some(&sizes) {
            return;
        }
        self.config.middle_column_sizes = Some(sizes);
        self.save_layout_soon(cx);
    }

    pub fn toggle_middle_column(&mut self, cx: &mut Context<Self>) {
        self.config.middle_column_visible = !self.config.middle_column_visible;
        self.save_layout_soon(cx);
        cx.changed(StoreEvent::Config(ConfigSection::View));
    }

    // === 终端列表竖条 ===

    /// 终端区右缘的「终端列表」竖条是否展开。
    pub fn terminals_panel_visible(&self) -> bool {
        self.terminals_panel_visible
    }

    pub fn toggle_terminals_panel(&mut self, cx: &mut Context<Self>) {
        self.terminals_panel_visible = !self.terminals_panel_visible;
        self.save_layout_soon(cx);
        cx.changed(StoreEvent::Config(ConfigSection::View));
    }

    // === 持久化 ===

    // 拆分前是私有方法;调用点在 `store::panes` 与 `store::ssh`,升到 `pub(super)`。
    pub(super) fn after_layout_change(&mut self, project_id: &str, cx: &mut Context<Self>) {
        if let Some(state) = self.project_states.get_mut(project_id) {
            // 被最大化的那个 pane 关掉了(或随面板切换离开了活动面板)→ 自动回落
            // 显示整树。原版是在渲染处「按 id 查不到叶子就退回整树」,这里顺手把
            // 陈旧 id 也清掉:留着它只会让 `maximized_pane_id()` 每帧多查一次,
            // 且没有任何复活路径(pane id 是进程内单调递增的,不会被重新分配)。
            if let Some(id) = state.maximized_pane_id.clone()
                && state.active_layout().and_then(|l| l.pane(&id)).is_none()
            {
                state.maximized_pane_id = None;
            }
        }
        // 关掉的 pane 一并撤出完成队列:否则未读计数会往一个已经不存在的 pane
        // 上跳,两张表也会随开关终端无界增长(旧版 setProjectLayout 的同一段)。
        self.done.retain_panes(&self.live_pane_ids());
        self.save_project_layout_soon(project_id, cx);
        cx.changed(StoreEvent::LayoutChanged);
    }

    /// 全部项目里活着的 pane id。
    // 拆分前是私有方法;调用点在 `store::projects::remove_project`,升到 `pub(super)`。
    pub(super) fn live_pane_ids(&self) -> HashSet<String> {
        self.project_states
            .values()
            .flat_map(|s| s.all_panes().into_iter().map(|p| p.id.clone()))
            .collect()
    }

    /// 全部项目里活着的 split/leaf 节点 id —— 供 `TerminalArea` 回收分隔条状态。
    pub fn live_node_ids(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        for state in self.project_states.values() {
            for layout in state.layouts() {
                collect_node_ids(layout, &mut out);
            }
        }
        out
    }

    // ─── 布局落盘(layout.db)────────────────────────────────────────────
    //
    // 与配置分家的理由见 `mt-layout` 的模块注释:布局是交互频次的数据,不该
    // 每改一次就把整份 config.json 连同 .bak 重写一遍。这里只保留防抖 ——
    // 一次 upsert 便宜,但拖分隔条期间每帧一次仍是浪费。

    /// 把某个项目当前的树序列化进内存缓存,并排上落盘。
    // 拆分前是私有方法;调用点在 `store::panes` 与 `store::ai`,升到 `pub(super)`。
    pub(super) fn save_project_layout_soon(&mut self, project_id: &str, cx: &mut Context<Self>) {
        let saved = self
            .project_states
            .get(project_id)
            .map(|s| persist::serialize_layout(&s.panels, s.active_panel_index()));
        if let Some(saved) = saved
            && let Some(project) = self
                .config
                .projects
                .iter_mut()
                .find(|p| p.id == project_id)
        {
            project.saved_layout = Some(saved);
        }
        self.layout_dirty_projects.insert(project_id.to_string());
        self.schedule_layout_flush(cx);
    }

    /// 全局布局项(三栏比例 / 中栏比例 / 中栏显隐 / 抽屉宽度 / 窗口几何)脏了。
    fn save_layout_soon(&mut self, cx: &mut Context<Self>) {
        self.layout_globals_dirty = true;
        self.schedule_layout_flush(cx);
    }

    /// 防抖 300ms。比配置那条(500ms)短:单行 upsert 的代价远低于整份
    /// config.json 重写,没必要为攒批多等。
    // 拆分前是私有方法;调用点在 `store::projects::remove_project`,升到 `pub(super)`。
    pub(super) fn schedule_layout_flush(&mut self, cx: &mut Context<Self>) {
        self.layout_save_generation += 1;
        let generation = self.layout_save_generation;
        self._layout_save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            let _ = this.update(cx, |store, _cx| {
                if store.layout_save_generation == generation {
                    store.flush_layout_now();
                }
            });
        }));
    }

    /// 立即把攒下的布局脏快照交给后台写线程写进 `layout.db`(退出前 / 防抖到点)。
    ///
    /// 主线程只采快照并入队,不阻塞在 SQLite 事务上;进程退出前由 `App::on_app_quit`
    /// 同步 `drain` 保证最后一次布局必落盘。库开不起来时是 no-op —— 脏标记照样清掉,
    /// 免得每次退出都重试一遍必然失败的写入、把日志刷满。
    pub fn flush_layout_now(&mut self) {
        let dirty_projects = std::mem::take(&mut self.layout_dirty_projects);
        let globals_dirty = std::mem::take(&mut self.layout_globals_dirty);
        let Some(writer) = self.layout_writer.as_ref() else {
            return;
        };

        let globals = globals_dirty.then(|| mt_layout::GlobalLayout {
            layout_sizes: self.config.layout_sizes.clone(),
            middle_column_sizes: self.config.middle_column_sizes.clone(),
            middle_column_visible: Some(self.config.middle_column_visible),
            right_drawer_width: self.config.right_drawer_width,
            terminals_panel_visible: Some(self.terminals_panel_visible),
            git_detail_width: self.git_detail_width,
            window: self.window_geometry,
        });

        let mut projects = Vec::with_capacity(dirty_projects.len());
        if !dirty_projects.is_empty() {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            for project_id in dirty_projects {
                // 项目在防抖窗口里被删掉了 → 删行(它的树已经不在 config 里了)
                let op = match self
                    .config
                    .projects
                    .iter()
                    .find(|p| p.id == project_id)
                    .and_then(|p| p.saved_layout.as_ref())
                {
                    Some(layout) => super::layout_writer::ProjectLayoutOp::Save {
                        layout: layout.clone(),
                        now_ms,
                    },
                    None => super::layout_writer::ProjectLayoutOp::Delete,
                };
                projects.push((project_id, op));
            }
        }

        writer.enqueue(super::layout_writer::LayoutBatch { globals, projects });
    }

    /// 窗口几何(退出时的样子)。`None` = 没存过 / 存的值不可用,由开窗那一步
    /// 回落默认居中窗口。
    pub fn window_geometry(&self) -> Option<mt_layout::WindowGeometry> {
        self.window_geometry
    }

    /// 窗口被拖动 / 缩放 / 最大化后记一笔。值没变就不排落盘 ——
    /// gpui 的 bounds 观察者在拖动期间是每帧回调的。
    pub fn set_window_geometry(
        &mut self,
        geometry: mt_layout::WindowGeometry,
        cx: &mut Context<Self>,
    ) {
        if !geometry.is_sane() || self.window_geometry == Some(geometry) {
            return;
        }
        self.window_geometry = Some(geometry);
        self.save_layout_soon(cx);
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

    /// 立即把配置**排上落盘**(退出前 / 项目切换 / 那十来个不肯等防抖的写入点)。
    ///
    /// # 「立即」现在的含义:入队即快照,不是同步写完
    ///
    /// 方法名与全部调用点保持原样,但这一调不再在 UI 线程上碰磁盘:它把
    /// `self.config` 克隆一份交给单写者后台线程(见
    /// [`crate::store::config_writer`]),自己毫秒级返回。
    ///
    /// **语义没有退回防抖**:调用方之所以选这条而不是 500ms 的
    /// [`Self::save_config_soon`],要的是「这一刻的内容已经定死、崩溃也不会丢」
    /// (SSH 密码、私钥路径、项目环境变量)。克隆发生在返回之前,写线程一直阻塞
    /// 在条件变量上、入队即被唤醒 —— 风险窗口是一次线程唤醒加一次事务,不是
    /// 半秒钟的防抖窗。丢失窗口内多次调用会被折叠成最新一份(全量快照,
    /// 合法性论证见那个模块的注释)。
    ///
    /// 令牌语义与装机版一致:令牌过期说明别处写过配置,必须先重读拿到新令牌。
    /// 单进程壳里「别处」只可能是本进程的另一次 load,手上这份就是最新的,
    /// 于是重读一次令牌后原样重写。**这一步刻意留在主线程** —— 它要回写
    /// `self.token`,而且实际走不到(load 只发生在启动)。
    ///
    /// 顺手把布局也刷下去:两条落盘路径分家后,退出钩子只调这一个入口 ——
    /// 让它把两边都收干净,比要求每个调用点记得调两次可靠。布局同样走后台单写者
    /// [`crate::store::layout_writer::LayoutWriter`] 入队,退出时由 `App::on_app_quit`
    /// 统一排干。
    pub fn save_config_now(&mut self) {
        self.flush_layout_now();
        if self.token == 0 {
            return; // 配置没加载成功过,不许写盘覆盖磁盘
        }
        if self.token != self.config_store.current_token() {
            match self.config_store.load() {
                Ok(loaded) => self.token = loaded.token,
                Err(err) => {
                    tracing::warn!("[store] 令牌过期后重读配置失败: {err:#}");
                    return;
                }
            }
        }
        self.config_writer
            .enqueue(&self.config_store, self.token, self.config.clone());
    }
}

#[cfg(test)]
mod tests {
    use gpui::AppContext as _;
    use mt_config::{AppConfig, ProjectConfig};

    use super::AppStore;

    #[gpui::test]
    fn 全部折叠清空本项目展开态并交回清掉的目录(cx: &mut gpui::TestAppContext) {
        let mut project = ProjectConfig::new("p", "P", "/repo");
        project.expanded_dirs = vec!["/repo/src".into(), "/repo/src/deep".into()];
        let config = AppConfig {
            projects: vec![project, ProjectConfig::new("q", "Q", "/other")],
            ..AppConfig::default()
        };
        let store = cx.new(|cx| AppStore::git_test_store(config, cx));
        cx.update(|cx| {
            store.update(cx, |store, cx| {
                store.set_dir_expanded("q", "/other/a", true, cx);
                let mut collapsed = store.collapse_all_dirs("p", cx);
                collapsed.sort();
                assert_eq!(collapsed, ["/repo/src", "/repo/src/deep"]);
                assert!(!store.is_dir_expanded("p", "/repo/src"));
                assert!(!store.is_dir_expanded("p", "/repo/src/deep"));
                // 落盘那一份也清了,不然重启后又全展开回来
                assert!(store.project("p").unwrap().expanded_dirs.is_empty());
                // 别的项目不受影响
                assert!(store.is_dir_expanded("q", "/other/a"));
                // 已经全折着时再按一次是空操作
                assert!(store.collapse_all_dirs("p", cx).is_empty());
            })
        });
    }
}
