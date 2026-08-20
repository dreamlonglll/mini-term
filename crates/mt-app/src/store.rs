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
use mt_config::{
    AiLauncher, AppConfig, ConfigStore, MobileRelayConfig, ProjectConfig, SaveError,
    ShellConfig, SshConnection,
};
use mt_relay::MobileRelayStatusPayload;
use mt_pty::PtySpawn;
use mt_ui::icons::ProjectKind;
use mt_ui::theme_bridge::BackgroundArt;
use mt_ui::{DwellConfig, TerminalStyle, TerminalTheme};

use crate::ai::{AiBridge, AiEvent};
use crate::markers::{self, AiMarker, MarkerBatch};
use crate::notify::{AlertPlan, DoneTracker, NotifyPrefs, PaneRef, StatusTransition};
use crate::pane::{PaneEvent, TerminalPane};
use crate::persist;
use crate::project_tree;
use crate::session_panel::build_resume_command;
use crate::shell_ops::ShellList;
use crate::tree::{
    AiSessionRef, DropZone, PaneState, PaneStatus, SplitDirection, SplitNode, gen_id,
};

/// 单个项目的运行时状态(对应 `types.ts` 的 `ProjectState`)。
pub struct ProjectState {
    /// 终端布局树;`None` = 还没有终端(渲染空态)。
    pub layout: Option<SplitNode>,
    /// 由 layout 聚合出的项目级状态(error > ai-working > ai-idle > idle)。
    pub status: PaneStatus,
    /// 非激活项目里有 AI 任务完成 —— 项目行上的提示点。
    pub needs_attention: bool,
    /// 双击最大化的 pane:终端区只渲染它所在的那个叶子。
    ///
    /// **纯运行时,不落盘**(`types.ts::ProjectState.maximizedPaneId` 同样不进
    /// `savedLayout`,`persist.rs` 里一个字都不该出现它)。语义是「哪个 pane 被
    /// 铺满了」而不是「哪个叶子」—— 同组内切 tab 仍然保持最大化,与原版一致。
    pub maximized_pane_id: Option<String>,
}

impl ProjectState {
    fn new() -> Self {
        Self {
            layout: None,
            status: PaneStatus::Idle,
            needs_attention: false,
            maximized_pane_id: None,
        }
    }
}

struct GlobalStore(Entity<AppStore>);
impl Global for GlobalStore {}

/// 用量面板的六个偏好(对应旧版那六个 localStorage 键)。
///
/// 一把传是为了只触发一次 500ms 去抖写盘 —— 连点分段控件不该连写六次。
/// 取值合法性由面板侧的白名单/正则保证,store 只负责搬运。
pub struct UsagePrefs {
    pub scope: String,
    pub range: String,
    /// 项目**原始路径**;`None` = 整机。
    pub project: Option<String>,
    pub auto_refresh: u32,
    pub custom_from: String,
    pub custom_to: String,
}

/// 一次「关联 SSH」保存的结果(`SshAssocModal.tsx::handleSave` 收尾那一段
/// 要的全部素材)。由 [`AppStore::apply_ssh_assoc`] 返回。
pub struct SshAssocOutcome {
    /// 保存后该项目是否处于「已启用 SSH 工具」状态。
    pub enabled: bool,
    /// 保存**之前**是否已启用 —— 三条提示文案(启用/更新/停用)靠它分档。
    pub was_enabled: bool,
    /// 有效配置没变(幂等 reconcile / 存量迁移):落盘即可,**不弹提示**。
    pub silent: bool,
    /// 本次范围里的连接数与连接总数 —— 提示文案里的 `scopeAll` / `scopeSubset`。
    pub scope_len: usize,
    pub total_len: usize,
    /// 启用时的项目能力令牌(已由 [`AppStore::set_project_ssh_assoc`] 落盘,
    /// 这里带回只为调用方需要时展示/排查)。
    pub project_token: Option<String>,
    /// 注册器返回的中文提示(与装机版一字不差,不走 mt-i18n)。
    /// ⚠️ **当前没有读者**,与原版一致:`EnableSshToolsResult` 也带 `message`,
    /// 而 `SshAssocModal.tsx` 只取 `projectToken` —— 提示文案是弹窗自己按
    /// 启用/更新/停用三档拼的,注册器那句只进日志面。字段留着是为了不丢
    /// 服务层的返回信息(要排查「注册器到底做了什么」时它是唯一线索)。
    #[allow(dead_code)]
    pub message: String,
}

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
    /// 界面布局的落盘口(`layout.db`)。`None` = 库开不起来(盘满 / 权限),
    /// 此时布局**只在内存里活着**:界面照常用,退出即忘 —— 与配置加载失败时
    /// 「只读模式」同一条红线,绝不因为存不下就不让用。
    layout_store: Option<Arc<mt_layout::LayoutStore>>,
    /// 窗口几何(退出时的大小/位置/最大化态)。config 里没有对应字段 ——
    /// 这是 GPUI 版新补的能力,只住在 `layout.db` 与这里。
    window_geometry: Option<mt_layout::WindowGeometry>,
    /// 攒着待写的项目 id 与「全局项脏了」标记。防抖窗口内拖十次分隔条只落一次盘,
    /// 且不同项目的改动互不覆盖。
    layout_dirty_projects: HashSet<String>,
    layout_globals_dirty: bool,
    /// 布局防抖的代号,与 [`Self::save_generation`] 同一套路。
    /// **单独一份**:布局与配置现在写去两个地方,共用代号会让其中一路饿死。
    layout_save_generation: u64,
    _layout_save_task: Option<Task<()>>,

    pub active_project_id: Option<String>,
    project_states: HashMap<String, ProjectState>,
    /// ptyId → 终端视图。pane 只在树里存 id,视图挂这里(旧版 terminalCache)。
    terminals: HashMap<u32, Entity<TerminalPane>>,
    /// 每个 pane 的退出订阅,与 terminals 同生命周期。
    pane_subs: HashMap<u32, Subscription>,
    /// 当前拿着键盘焦点的 pane(旧版靠 DOM `activeElement` 推,这里显式维护)。
    pub focused_pane_id: Option<String>,

    /// 移动端中转的连接状态(`src/store.ts:702` 的 `mobileRelayStatus`)。
    /// **纯运行时,不落盘** —— 与 [`Self::focused_pane_id`] 同类。
    mobile_relay_status: Option<MobileRelayStatusPayload>,

    /// AI 任务标记(`src/store.ts:666-671` 的 `markersByPty`)。
    /// **纯运行时,不落盘**;pane 一没,这一份跟着没(见 [`Self::dispose_terminal`])。
    markers_by_pty: HashMap<u32, Vec<AiMarker>>,
    /// 「这个 pane 上次跳到哪条标记」的游标(`useMarkerHotkeys.ts:19` 的 `lastJumpRef`)。
    ///
    /// 原版那份是模块级 ref、**从不清理**(pane 关了条目还在,微量泄漏 +
    /// 「pty id 复用后游标是旧的」的边界)。这里与标记表同生共死,顺手修掉。
    marker_cursor: HashMap<u32, String>,

    /// 会话分支的**自记账登记**(`src/store.ts:173` 的 `pendingForks`)。
    /// mini-term 自己发起的 fork 在新 pane 的 PTY 上登记「等新会话身份」,
    /// hook 上报新 id 时落成 child→parent 边写进 `config.session_lineage`。
    /// **纯运行时,不落盘**;见 [`AppStore::register_pending_fork`]。
    pending_forks: HashMap<u32, PendingFork>,

    next_pty_id: u32,
    ai: AiBridge,

    /// 当前生效的终端配色(主题装配的产物,见 [`crate::theme`])。
    /// 新建终端拿它,已存在的终端由 [`AppStore::apply_theme_from_config`] 热更新。
    terminal_theme: TerminalTheme,
    /// 当前主题的背景图氛围层参数。**渲染归 mt-ui,这里只是数据落点**。
    background_art: Option<BackgroundArt>,

    /// 展开的目录(按项目)。运行时态,落盘走 `ProjectConfig::expanded_dirs`。
    expanded_dirs: HashMap<String, HashSet<String>>,

    /// 目录技术栈探测缓存(`src/store.ts:708` 的 `dirKinds`)。
    /// key = 目录路径**原样**;`None` = 已探测但识别不出(**不再重探**)。
    /// 项目根与文件树里的子工程目录共用这一份。
    dir_kinds: HashMap<String, Option<ProjectKind>>,
    /// 在途探测(`useProjectKinds.ts` 那个模块级 `pending`)。
    /// **不是可订阅状态**,只为去重 —— 变化不 notify。
    dir_kinds_pending: HashSet<String>,

    /// 已退出的 PTY(`src/store.ts:660` 的 `exitedPtyIds`,`pty-exit` 登记)。
    /// 悬停缩略图据此画「已断开」遮罩;远程 pane 的重连覆盖层随 #28。
    /// **纯运行时,不落盘**;pane 一没跟着没(见 [`Self::dispose_terminal`])。
    exited_ptys: HashSet<u32>,

    /// 完成队列(未读集合 + 完成序号),对应旧版的 unreadDonePaneIds / aiDoneOrder。
    done: DoneTracker,
    /// 主窗口是否聚焦。聚焦时完成的任务用户正看着,不计入「未读完成」。
    window_focused: bool,

    /// 防抖保存的代号:只有最后一次排上的任务才真写盘。
    save_generation: u64,
    _save_task: Option<Task<()>>,
}

/// 开布局库,顺带跑一次「从 config.json 迁入」。
///
/// 返回 `None` 的三种情形都按同一档降级处理:**布局本次不落盘**,界面照常用。
/// 其中迁移失败也返回 `None` 是刻意的 —— 让本次继续走内存里那份、下次启动重试,
/// 比拿一份半截数据把用户的布局盖掉强。
fn open_layout_store(config: &AppConfig, may_migrate: bool) -> Option<Arc<mt_layout::LayoutStore>> {
    let dir = match mt_config::active_data_dir() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("[layout] 定位数据目录失败({err:#}),本次布局不落盘");
            return None;
        }
    };
    let store = match mt_layout::LayoutStore::open_at(&dir) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("[layout] 布局库打不开({err:#}),本次布局不落盘");
            return None;
        }
    };
    if may_migrate && store.needs_config_migration() {
        let fallback = layout_migration_fallback(config, &dir);
        let source = fallback.as_ref().unwrap_or(config);
        match store.migrate_from_config(source) {
            Ok(n) => eprintln!(
                "[layout] 已迁入 {n} 个项目的布局 → {}",
                store.path().display()
            ),
            Err(err) => {
                eprintln!("[layout] 布局迁移失败({err:#}),本次布局不落盘");
                return None;
            }
        }
    }
    Some(Arc::new(store))
}

/// 布局迁移的**兜底数据源**:`{dir}/config.json.pre-sqlite`(配置搬进 config.db
/// 时留下的完整旧配置存档)。
///
/// 为什么需要它:配置迁移一完成,`config.json` 就被覆盖成只剩 SSH 的投影,而
/// `savedLayout` 此后只活在「内存里这一份 AppConfig」上。要是布局迁移偏偏在那一次
/// 失败(盘满 / 库被占用),下次启动的 config 是从 config.db 读的、`savedLayout`
/// 全是 `None` —— 重试也只会迁出一片空白,旧布局**永久丢失**。
///
/// 只在「传进来的 config 一个布局都没有」时才回退去读存档:正常首启走内存那份
/// (更新、且不必读盘);第二次以后 `needs_config_migration()` 已是 false,根本
/// 到不了这里。
fn layout_migration_fallback(config: &AppConfig, dir: &Path) -> Option<AppConfig> {
    if config.projects.iter().any(|p| p.saved_layout.is_some()) {
        return None;
    }
    let archive = dir.join("config.json.pre-sqlite");
    let archived = mt_config::read_config_from(&archive).ok().flatten()?;
    if !archived.projects.iter().any(|p| p.saved_layout.is_some()) {
        return None;
    }
    eprintln!(
        "[layout] 内存里的配置已无 savedLayout,改从存档迁移: {}",
        archive.display()
    );
    Some(archived)
}

/// 把库里的布局覆盖进 `config` 的对应字段(内存缓存),并清掉已删项目的残行。
///
/// 库里**没有**的全局项保持 config 里的值不动:`None` 的语义是「这个键没存过」,
/// 不是「用户把它设成了默认值」。项目级则相反 —— 逐个赋值(含赋 `None`),
/// 库才是唯一真相:用户把某项目的终端关光了,config.json 里的残留不该复活。
///
/// 返回窗口几何(config 里没有它的位置,由调用方单独接住)。
fn apply_layout_db(
    store: &mt_layout::LayoutStore,
    config: &mut AppConfig,
) -> Option<mt_layout::WindowGeometry> {
    let globals = store.load_globals();
    if globals.layout_sizes.is_some() {
        config.layout_sizes = globals.layout_sizes;
    }
    if globals.middle_column_sizes.is_some() {
        config.middle_column_sizes = globals.middle_column_sizes;
    }
    if let Some(visible) = globals.middle_column_visible {
        config.middle_column_visible = visible;
    }
    if globals.right_drawer_width.is_some() {
        config.right_drawer_width = globals.right_drawer_width;
    }

    let mut layouts = store.load_project_layouts();
    for project in config.projects.iter_mut() {
        project.saved_layout = layouts.remove(&project.id);
    }
    // 对一次账:删项目那条路径漏调也不会攒出无主行(项目 id 不复用)。
    let live: HashSet<String> = config.projects.iter().map(|p| p.id.clone()).collect();
    if let Err(err) = store.retain_projects(&live) {
        eprintln!("[layout] 清理无主项目行失败: {err:#}");
    }

    // 明显不可用的几何(尺寸为 0、NaN、小得放不下内容)当没存过 —— 让开窗
    // 那一步回落默认居中窗口,而不是开出一条缝。
    globals.window.filter(|geo| geo.is_sane())
}

impl AppStore {
    /// 装配 store:加载配置 → 恢复各项目布局(不起 PTY,PTY 在首次显示时懒起)。
    pub fn new(config_store: Arc<ConfigStore>, ai: AiBridge, cx: &mut Context<Self>) -> Self {
        let _ = cx;
        let (mut config, token) = match config_store.load() {
            Ok(loaded) => (loaded.config, loaded.token),
            Err(err) => {
                // 加载失败**绝不**伪装成空配置:令牌留 0,后续所有保存都会被自己挡下,
                // 免得一次读盘故障把用户的项目列表清空(旧版同一条红线)。
                eprintln!("[store] 配置加载失败({err:#}),本次以只读模式运行");
                (AppConfig::default(), 0)
            }
        };

        // 布局库:开库 →(首次)从 config.json 灌一次 → 把库里的值覆盖回
        // `config` 的对应字段。**覆盖这一步是整个改造的支点** —— 各处 getter
        // 照旧读 `self.config.*`(它现在是内存缓存),只有落盘那一步改了道。
        // 配置加载失败(token=0)时不迁移:那份 config 是空默认值,灌进去等于
        // 拿一份伪造的空布局把用户真实的布局盖掉。
        let layout_store = open_layout_store(&config, token != 0);
        let window_geometry = layout_store
            .as_ref()
            .map(|store| apply_layout_db(store, &mut config))
            .unwrap_or_default();

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
            layout_store,
            window_geometry,
            layout_dirty_projects: HashSet::new(),
            layout_globals_dirty: false,
            layout_save_generation: 0,
            _layout_save_task: None,
            active_project_id,
            project_states,
            terminals: HashMap::new(),
            pane_subs: HashMap::new(),
            focused_pane_id: None,
            mobile_relay_status: None,
            markers_by_pty: HashMap::new(),
            marker_cursor: HashMap::new(),
            pending_forks: HashMap::new(),
            next_pty_id: 1,
            ai,
            // 真正的配色在 `apply_theme_from_config` 里装配(要 `&mut App` 取系统
            // 外观 / 装 gpui-component 主题层),这里先给个能跑的初值
            terminal_theme: TerminalTheme::default(),
            background_art: None,
            expanded_dirs,
            dir_kinds: HashMap::new(),
            dir_kinds_pending: HashSet::new(),
            exited_ptys: HashSet::new(),
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

    /// 这个 PTY 已经退出了吗(`exitedPtyIds.has`)。
    pub fn is_pty_exited(&self, pty_id: u32) -> bool {
        self.exited_ptys.contains(&pty_id)
    }

    // === 目录技术栈探测(`useProjectKinds.ts`) ===

    /// 读缓存。`None` = 还没探过;`Some(None)` = 探过但识别不出。
    pub fn dir_kind(&self, path: &str) -> Option<Option<ProjectKind>> {
        self.dir_kinds.get(path).copied()
    }

    /// 批量探测(去重 + 带缓存)。**只接本地路径**,远程由调用方跳过。
    ///
    /// 每条路径一个后台任务:`detect_local` 要读目录、可能还读 `package.json`,
    /// 在主线程上跑会把网络盘/WSL 上的一次悬停做成秒级卡顿。
    pub fn ensure_dir_kinds(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        for path in paths {
            if self.dir_kinds.contains_key(&path) || !self.dir_kinds_pending.insert(path.clone()) {
                continue;
            }
            cx.spawn(async move |this, cx| {
                let probe = path.clone();
                let kind = cx
                    .background_executor()
                    .spawn(async move {
                        crate::project_kind::detect_local(std::path::Path::new(&probe))
                    })
                    .await;
                let _ = this.update(cx, |store: &mut AppStore, cx| {
                    store.dir_kinds_pending.remove(&path);
                    store.set_dir_kind(path, kind, cx);
                });
            })
            .detach();
        }
    }

    /// 写缓存并通知(`setDirKind`)。识别不出也要写 —— 否则每帧重探。
    pub fn set_dir_kind(&mut self, path: String, kind: Option<ProjectKind>, cx: &mut Context<Self>) {
        self.dir_kinds.insert(path, kind);
        cx.notify();
    }

    /// 失效(`removeDirKind`):项目根的标记文件变动时调。下一轮 `ensure` 会重探。
    pub fn remove_dir_kind(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.dir_kinds.remove(path).is_some() {
            cx.notify();
        }
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

    /// 按路径找项目(`store.ts::findProjectByPath`)。
    ///
    /// 比对走 [`normalize_path`](crate::git_worktree::normalize_path)(分隔符统一 +
    /// 去尾斜杠 + 转小写),与 worktree「是否已是项目」的判据同一份。
    /// SSH 远程项目排除在外 —— worktree 的路径是本机路径。
    pub fn find_project_by_path(&self, path: &str) -> Option<&ProjectConfig> {
        let target = crate::git_worktree::normalize_path(path);
        self.config.projects.iter().find(|p| {
            p.ssh_connection_id.is_none() && crate::git_worktree::normalize_path(&p.path) == target
        })
    }

    /// 添加项目并**返回它的 id**;`parent` 非空时挂成子项目。
    ///
    /// 对应 `store.ts:777-799` 的 `addProject(project, parentProjectId)`:
    /// - 父项目必须真实存在,否则回落为普通顶层项目(防止产生渲染不出来的孤儿);
    /// - **子项目不进 `projectTree`**(移动出去时才转成普通树节点);
    /// - 路径已经是项目 → 返回既有 id,不重复添加(`GitWorktreeModal.tsx:341-351`)。
    ///
    /// 与 [`add_project`](Self::add_project) 的差别只有「带父项目 + 返回 id +
    /// 不自动切过去」三条 —— worktree「设为项目」要自己决定切不切。
    pub fn add_project_at(
        &mut self,
        path: &Path,
        parent: Option<&str>,
        cx: &mut Context<Self>,
    ) -> String {
        let path_str = path.to_string_lossy().to_string();
        if let Some(existing) = self.find_project_by_path(&path_str).map(|p| p.id.clone()) {
            return existing;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path_str.clone());
        let id = gen_id("proj");
        let parent_ok = parent.filter(|pid| self.config.projects.iter().any(|p| p.id == *pid));

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
            parent_project_id: parent_ok.map(str::to_string),
            kind_override: None,
        });
        if parent_ok.is_none() {
            let tree = self.config.project_tree.get_or_insert_with(Vec::new);
            tree.push(mt_config::ProjectTreeItem::ProjectId(id.clone()));
        }
        self.project_states.insert(id.clone(), ProjectState::new());
        self.expanded_dirs.insert(id.clone(), HashSet::new());
        self.save_config_soon(cx);
        cx.notify();
        id
    }

    /// 只回收某个项目的终端,**不删项目**(`projectActions.ts:25-32` 的
    /// `disposeProjectTerminals`)。
    ///
    /// worktree 删除必须先走这一步:Windows 上 shell 占着目录会让
    /// `git worktree remove` 直接失败。
    pub fn dispose_project_terminals(&mut self, project_id: &str, cx: &mut Context<Self>) {
        let pty_ids: Vec<u32> = self
            .project_states
            .get(project_id)
            .and_then(|s| s.layout.as_ref())
            .map(|l| l.pty_ids())
            .unwrap_or_default();
        for pty_id in pty_ids {
            self.dispose_terminal(pty_id, cx);
        }
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

    /// 改项目显示名(`store.ts::renameProject`)。空名不接受 —— 列表上会变成
    /// 一行只有路径的空条目,而原版的内联重命名框同样在空串时直接放弃。
    pub fn rename_project(&mut self, id: &str, name: &str, cx: &mut Context<Self>) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let Some(project) = self.config.projects.iter_mut().find(|p| p.id == id) else {
            return;
        };
        if project.name == name {
            return;
        }
        project.name = name.to_string();
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 设置项目需求描述;空串 = 清除(`store.ts::setProjectDescription` 的
    /// `description || undefined` 同语义 —— 存空串会让 `skip_serializing_if`
    /// 失效,配置文件里留一堆 `"description": ""`)。
    pub fn set_project_description(&mut self, id: &str, description: &str, cx: &mut Context<Self>) {
        let next = match description.trim() {
            "" => None,
            text => Some(text.to_string()),
        };
        let Some(project) = self.config.projects.iter_mut().find(|p| p.id == id) else {
            return;
        };
        if project.description == next {
            return;
        }
        project.description = next;
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 项目级环境变量(`ProjectEnvVarsModal` 的落盘那一半)。
    ///
    /// **立即落盘**而不是 500ms 防抖:整屏手填的键值对不该在防抖窗口里被一次
    /// 崩溃吃掉(与 SSH 连接同一条理由)。入参已由弹窗清洗过 —— 这里不做校验,
    /// 校验的唯一实现在 `env_vars::compute_errors`(单测钉死)。
    ///
    /// 生效面:只影响**之后新建**的终端(`start_pty` 里读 `env_vars`),
    /// 已有终端不受影响 —— 弹窗底栏那句脚注说的就是这件事。
    pub fn set_project_env_vars(
        &mut self,
        project_id: &str,
        vars: Vec<mt_config::ProjectEnvVar>,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.config.projects.iter_mut().find(|p| p.id == project_id) else {
            return;
        };
        project.env_vars = vars;
        self.save_config_now();
        cx.notify();
    }

    /// 项目类型徽标覆盖:`None` = 自动探测,`Some("none")` = 不显示,
    /// 其余是技术栈 key(直接喂 `TechIcon`)。对应 `ProjectList.tsx` 的
    /// `setProjectKindOverride`(它是「改 config + 立刻落盘」两步)。
    pub fn set_project_kind_override(
        &mut self,
        id: &str,
        kind: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let next = kind.map(|k| k.to_string());
        let Some(project) = self.config.projects.iter_mut().find(|p| p.id == id) else {
            return;
        };
        if project.kind_override == next {
            return;
        }
        project.kind_override = next;
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
        // 它的 toast 一并撤掉(`store.ts:859`)—— 留着的话点下去会跳向一个
        // 已经不存在的项目
        crate::toast::remove_project(id, cx);
        self.config.projects.retain(|p| p.id != id);
        if let Some(tree) = self.config.project_tree.as_mut() {
            remove_from_tree(tree, id);
        }
        if self.active_project_id.as_deref() == Some(id) {
            self.active_project_id = self.config.projects.first().map(|p| p.id.clone());
            self.config.last_active_project_id = self.active_project_id.clone();
        }
        // 它在布局库里的那一行一并删掉。`flush_layout_now` 查不到项目时按删行
        // 处理,所以这里只要把 id 标脏即可(项目 id 不复用,不怕标错)。
        self.layout_dirty_projects.insert(id.to_string());
        self.schedule_layout_flush(cx);
        self.save_config_soon(cx);
        cx.notify();
    }

    // === 项目分组(`store.ts:1266-1313` 的五个 action) ===

    /// `ensureTree`(`store.ts:611-617`)的 Rust 版:第一次碰分组时把
    /// `projectTree` 补齐,免得后面的树操作全落进 `None` 里静默失效。
    ///
    /// **旧格式迁移不在这里**:`projectGroups`/`projectOrdering` → `projectTree`
    /// 已经由 `mt_config::migrate_config` 在读盘时做过一遍(config.rs:646-676),
    /// 这里只补「压根没有过分组」的那一档。
    ///
    /// ⚠️ 与 TS 的一处有意偏差:铺初值时**跳过 worktree 子项目**。那边是
    /// `projects.map(p => p.id)` 一个不落,但「子项目不进 projectTree」是两侧
    /// 共同的不变量(见 [`Self::add_project_at`]),把它们塞进去会让
    /// `get_ordered_tree` 同时按树序和父项目序各排一次。
    fn ensure_tree(&mut self) {
        if self
            .config
            .project_tree
            .as_ref()
            .is_some_and(|tree| !tree.is_empty())
        {
            return;
        }
        let ids: Vec<mt_config::ProjectTreeItem> = self
            .config
            .projects
            .iter()
            .filter(|p| p.parent_project_id.is_none())
            .map(|p| mt_config::ProjectTreeItem::ProjectId(p.id.clone()))
            .collect();
        self.config.project_tree = Some(ids);
    }

    /// 新建分组。`parent_group_id` 为 `None` = 建在顶层,**一律追加到末尾**。
    ///
    /// 父组找不到时 `insert_into_tree` 返回 false,原版就此**静默丢弃**
    /// (`store.ts:1266-1273` 不看返回值)—— 这里照抄:能走到这一步说明右键菜单
    /// 拿的是刚渲染过的组 id,丢弃比"悄悄建到顶层"更容易暴露真正的 bug。
    pub fn create_group(
        &mut self,
        name: &str,
        parent_group_id: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        self.ensure_tree();
        let group = mt_config::ProjectGroup {
            id: gen_id("group"),
            name: name.to_string(),
            collapsed: false,
            children: Vec::new(),
        };
        let tree = self.config.project_tree.get_or_insert_with(Vec::new);
        project_tree::insert_into_tree(
            tree,
            parent_group_id,
            mt_config::ProjectTreeItem::Group(group),
            None,
        );
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 删分组。**组员(含子组)原位晋升到父级,一个都不删** —— 与原版
    /// `removeGroupAndPromoteChildren` 同语义,所以确认框那句"会移到上一级"是真的。
    pub fn remove_group(&mut self, group_id: &str, cx: &mut Context<Self>) {
        let Some(tree) = self.config.project_tree.as_mut() else {
            return;
        };
        if !project_tree::remove_group_and_promote_children(tree, group_id) {
            return;
        }
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 改分组名。空名不接受(调用方那边也 `trim` 过一道,两处都拦)。
    pub fn rename_group(&mut self, group_id: &str, name: &str, cx: &mut Context<Self>) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let Some(tree) = self.config.project_tree.as_mut() else {
            return;
        };
        let Some(group) = project_tree::find_group_in_tree_mut(tree, group_id) else {
            return;
        };
        if group.name == name {
            return;
        }
        group.name = name.to_string();
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 折叠 / 展开。**只影响侧栏渲染**:移动端快照那条路
    /// (`mobile_relay::ordered_projects`)刻意不跳过折叠组,折不折叠由手机自己决定。
    pub fn toggle_group_collapse(&mut self, group_id: &str, cx: &mut Context<Self>) {
        let Some(tree) = self.config.project_tree.as_mut() else {
            return;
        };
        let Some(group) = project_tree::find_group_in_tree_mut(tree, group_id) else {
            return;
        };
        group.collapsed = !group.collapsed;
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 把节点(项目或分组)移到 `target_group_id` 里的 `index` 位置。
    /// `target_group_id = None` = 根层;`index = None` = 追加到末尾。
    ///
    /// 两条边界逐条对照 `store.ts:1296-1313`:
    /// - **树里找不到**:那多半是 worktree 子项目(它按设计就不在树里,位置由
    ///   父项目派生)。此时「移动 = 脱离父项目」:清掉 `parentProjectId`,再把裸
    ///   id 当普通树节点插进去。既不是子项目又不在树里 → 什么都不做。
    /// - **目标组找不到**:原版此时节点已经被摘下来却插不回去(`insertIntoTree`
    ///   返回 false 就丢了)。这里退回根层末尾 —— 分组被并发删掉是唯一能触发的
    ///   路径,丢掉一整个子树换不来任何好处。
    ///
    /// 返回值 = 这次有没有真的动过树。
    pub fn move_item(
        &mut self,
        item_id: &str,
        target_group_id: Option<&str>,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.ensure_tree();
        let removed = self
            .config
            .project_tree
            .as_mut()
            .and_then(|tree| project_tree::remove_from_tree(tree, item_id));

        let removed = match removed {
            Some(item) => item,
            None => {
                let Some(child) = self
                    .config
                    .projects
                    .iter_mut()
                    .find(|p| p.id == item_id && p.parent_project_id.is_some())
                else {
                    return false;
                };
                child.parent_project_id = None;
                mt_config::ProjectTreeItem::ProjectId(item_id.to_string())
            }
        };

        let tree = self.config.project_tree.get_or_insert_with(Vec::new);
        if !project_tree::insert_into_tree(tree, target_group_id, removed, index) {
            // 上面那段注释的第二条:目标组没了,退回根层末尾而不是把子树扔掉
            let fallback = mt_config::ProjectTreeItem::ProjectId(item_id.to_string());
            project_tree::insert_into_tree(tree, None, fallback, None);
        }
        self.save_config_soon(cx);
        cx.notify();
        true
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
        self.new_terminal_with_cwd(project_id, shell, anchor_pane_id, None, window, cx)
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
                .and_then(|s| s.layout.as_ref())
                .and_then(|l| l.pane(pane_id))
                .and_then(|p| p.cwd.clone())
        });
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
        let Some(next) = self
            .project_states
            .get(project_id)
            .and_then(|s| s.layout.as_ref())
            .and_then(|l| l.move_pane_in_layout(pane_id, target_pane_id, zone))
        else {
            return;
        };
        if let Some(state) = self.project_states.get_mut(project_id) {
            state.layout = Some(next);
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
            .and_then(|s| s.layout.as_ref())
            .and_then(|l| l.move_pane_to_tab_index(pane_id, anchor_pane_id, index))
        else {
            return;
        };
        if let Some(state) = self.project_states.get_mut(project_id) {
            state.layout = Some(next);
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
        let layout = state.layout.as_ref()?;
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
        let Some(layout) = state.layout.as_ref() else {
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
                self.save_project_layout_soon(project_id, cx);
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

        // SSH 远程分支:直接 spawn `ssh` 作 PTY 子进程(不经本地 shell,对齐 WSL
        // 启动器重写模式)。本地 cwd 用兜底目录 —— 远程目录由 ssh 的远端命令
        // `cd '<path>' && exec $SHELL -l` 进入,项目的 `path` 是远程 POSIX 路径,
        // 传给 portable-pty 只会让 ConPTY 静默退回 `$USERPROFILE`。
        //
        // AI 状态感知在这条路上走 PTY 输入/输出扫描的降级路径(输入检测作用于
        // 数据流,对远程天然可用);hook 精确状态不可用,PRD 已接受。
        //
        // 项目级环境变量对远程 pane **不注入**(装机版同款:那些变量属于本地
        // 机器,注给本地 ssh 客户端毫无意义)。
        let remote = project.ssh_connection_id.as_deref().map(|conn_id| {
            crate::remote_ssh::find_connection(&self.config.ssh_connections, conn_id)
                .and_then(|conn| crate::remote_ssh::prepare_remote_launch(&conn, &project.path))
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
    fn selection_dwell(&self) -> DwellConfig {
        DwellConfig::from_secs(self.config.selection_auto_copy_secs.unwrap_or(1.0) as f32)
    }

    fn terminal_style(&self) -> TerminalStyle {
        terminal_style_from(
            self.config.terminal_font_size,
            self.config.terminal_font_family.as_deref(),
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

    // === AI 任务标记(⚑)===

    /// 某个 pane 的标记列表(没有就是空)。对应 `store.ts:1225` 的 `getMarkersForPty`。
    pub fn markers_for_pty(&self, pty_id: u32) -> &[AiMarker] {
        self.markers_by_pty
            .get(&pty_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// 落一批标记(pane 在 [`crate::pane::TerminalPane::write`] 里当场取好锚点后发来)。
    ///
    /// 节奏照抄 `useAiSubmitMarker.ts:20-23`:**追加之后立刻剪一遍枝**,
    /// 不在渲染路径上剪(见 [`crate::markers`] 的模块注释)。
    fn add_markers(&mut self, pty_id: u32, batch: MarkerBatch, cx: &mut Context<Self>) {
        if batch.submits.is_empty() {
            return;
        }
        let list = self.markers_by_pty.entry(pty_id).or_default();
        for (line, ts) in batch.submits {
            markers::push_marker(list, pty_id, line, ts, batch.anchor);
        }
        markers::prune(list, batch.history, batch.max_scrollback);
        // 过滤后为空则连键一起删(`store.ts:1219` 的同一处置)
        if list.is_empty() {
            self.markers_by_pty.remove(&pty_id);
            self.marker_cursor.remove(&pty_id);
        }
        cx.notify();
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
    pub fn jump_to_marker(&mut self, pty_id: u32, marker_id: &str, cx: &mut Context<Self>) {
        let Some(entity) = self.terminals.get(&pty_id).cloned() else {
            return;
        };
        // 跳之前先剪一遍:锚点已经不可信的话宁可什么都不做,也不能跳到错的行上
        let (history, max) = entity.read(cx).scrollback_state();
        if let Some(list) = self.markers_by_pty.get_mut(&pty_id)
            && markers::prune(list, history, max)
        {
            self.markers_by_pty.remove(&pty_id);
            self.marker_cursor.remove(&pty_id);
            cx.notify();
        }
        let Some(anchor) = self
            .markers_for_pty(pty_id)
            .iter()
            .find(|m| m.id == marker_id)
            .map(|m| m.anchor)
        else {
            return;
        };
        // 跳不动(pane 正在 alt screen 里)就不推游标 —— 连按方向键不该空走格子
        if entity.update(cx, |pane, cx| pane.scroll_to_marker(anchor, cx)) {
            self.marker_cursor.insert(pty_id, marker_id.to_string());
        }
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
                    .and_then(|s| s.layout.as_ref())
                    .and_then(|l| l.pane(&pane_id))
                    .and_then(|p| p.pty_id)
            })
        else {
            return;
        };
        let cursor = self
            .marker_cursor
            .get(&pty_id)
            .and_then(|id| self.markers_for_pty(pty_id).iter().position(|m| &m.id == id));
        let len = self.markers_for_pty(pty_id).len();
        let Some(next) = markers::next_index(cursor, len, dir) else {
            return;
        };
        let Some(target) = self.markers_for_pty(pty_id).get(next).map(|m| m.id.clone()) else {
            return;
        };
        self.jump_to_marker(pty_id, &target, cx);
    }

    /// 回收一个终端:kill 子进程 + 清 AI 感知痕迹 + 摘掉视图与订阅。
    fn dispose_terminal(&mut self, pty_id: u32, cx: &mut Context<Self>) {
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
        // fork 命令没能起起会话就退了 —— 这条登记不该等到下一个进程头上
        // (原版把 `clearPendingFork` 挂在 `pty-exit` 监听里,同一时机)
        self.clear_pending_fork(pty_id);
        // 原版 `App.tsx:359` 的 `markPtyExited`:与状态落 error 同一时机
        self.exited_ptys.insert(pty_id);
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
                let session = AiSessionRef {
                    agent: identity.agent.clone(),
                    session_id: identity.session_id.clone(),
                    cwd: identity.cwd.clone(),
                };
                for (pid, state) in self.project_states.iter_mut() {
                    if let Some(layout) = state.layout.as_mut()
                        && let Some(pane) = layout.pane_by_pty_mut(identity.pty_id)
                    {
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
            .filter_map(|(pid, state)| state.layout.as_ref().map(|l| (pid, l)))
            .flat_map(|(pid, layout)| {
                layout.panes().into_iter().map(move |p| PaneRef {
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
        let panes = self.pane_refs(None);
        match scope {
            DoneScope::All => {
                let order = self.done.order();
                collect_ai_projects(panes, self.config.projects.as_slice(), |id| {
                    order.contains_key(id)
                })
            }
            DoneScope::Unread => collect_ai_projects(panes, self.config.projects.as_slice(), |id| {
                self.done.is_unread(id)
            }),
        }
    }

    /// 标题栏那颗全局状态灯(`TitleBar.tsx::computeLight`)。
    ///
    /// ⚠️ 与边条徽标的 [`AppStore::global_ai_status`] **口径不同**:边条把 `error`
    /// 压成 `idle`(一个 `exit 1` 的 shell 不该盖住真在跑的 AI),标题栏灯反过来
    /// 把 `error` 列为最高一档,另外还多一个 `done` 档。两处不可互相复用。
    pub fn title_bar_light(&self) -> TitleBarLight {
        let order = self.done.order();
        compute_title_bar_light(self.pane_refs(None), |id| order.contains_key(id))
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
            let Some(layout) = state.layout.as_ref() else {
                continue;
            };
            for pane in layout.panes() {
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
            && let Some(layout) = state.layout.as_mut()
            && let Some(pane) = layout.pane_mut(pane_id)
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

    // === AI 历史面板视图偏好 ===

    /// 会话列表视图(`"flat"` | `"tree"`)。认不出/没设过 = 平铺
    /// (原版 `SessionList.tsx:242` 的 `?? 'flat'`)。
    pub fn session_list_view(&self) -> &str {
        match self.config.session_list_view.as_deref() {
            Some("tree") => "tree",
            _ => "flat",
        }
    }

    pub fn set_session_list_view(&mut self, view: &str, cx: &mut Context<Self>) {
        if self.session_list_view() == view {
            return;
        }
        self.config.session_list_view = Some(view.to_string());
        self.save_config_soon(cx);
        cx.notify();
    }

    // === 用量面板偏好 ===

    /// 六个偏好**一把写** —— 六个 setter 各自触发一次 500ms 去抖没有意义。
    pub fn set_usage_prefs(&mut self, prefs: UsagePrefs, cx: &mut Context<Self>) {
        self.config.usage_scope = Some(prefs.scope);
        self.config.usage_range = Some(prefs.range);
        self.config.usage_project = prefs.project;
        self.config.usage_auto_refresh = Some(prefs.auto_refresh);
        self.config.usage_custom_from = Some(prefs.custom_from);
        self.config.usage_custom_to = Some(prefs.custom_to);
        self.save_config_soon(cx);
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

    // === 通用配置补丁 ===

    /// 写一份配置补丁并落盘(对应原版 `SettingsModal.tsx:59-70` 的 `useConfigPatch`)。
    ///
    /// 设置页上百个开关全走这一条:改字段 → 500ms 防抖落盘 → `cx.notify()`。
    /// 需要**额外副作用**的那几项(主题 / 字号 / 字族 / 回滚行数 / 停留时长)
    /// 各有自己的 setter,不要拿这个入口去改它们 —— 热更新会漏。
    pub fn patch_config(&mut self, edit: impl FnOnce(&mut AppConfig), cx: &mut Context<Self>) {
        edit(&mut self.config);
        self.save_config_soon(cx);
        cx.notify();
    }

    // === 终端渲染参数(四项热更新)===

    /// 终端字号。**热更新全部已开终端** —— 原版由 `TerminalInstance` 订阅 config
    /// 改 `term.options.fontSize`,这里走 `TerminalView::set_style`(cell 尺寸随之
    /// 变化,下一帧连带 resize grid 与 PTY)。
    pub fn set_terminal_font_size(&mut self, size: f64, cx: &mut Context<Self>) {
        let size = size.clamp(8.0, 32.0);
        if (self.config.terminal_font_size - size).abs() < f64::EPSILON {
            return;
        }
        self.config.terminal_font_size = size;
        self.apply_terminal_style(cx);
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 终端字族。空串 = 回落默认(写 `None`,不落空串)。
    ///
    /// 用户自选字体也会**自动补 CJK 回退**,与原版 `resolveTerminalFontFamily`
    /// (terminalCache.ts:53-58)同语义 —— 见 [`terminal_style_from`]。
    pub fn set_terminal_font_family(&mut self, family: Option<String>, cx: &mut Context<Self>) {
        let next = family
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty());
        if self.config.terminal_font_family == next {
            return;
        }
        self.config.terminal_font_family = next;
        self.apply_terminal_style(cx);
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 回滚行数。**热更新全部已开终端**:调小时 alacritty 的 `update_history`
    /// 当场裁掉多余历史并释放内存(原版 `updateAllTerminalScrollback` 同效果)。
    pub fn set_terminal_scrollback(&mut self, lines: u32, cx: &mut Context<Self>) {
        let lines = resolve_scrollback(lines as f64);
        if self.config.terminal_scrollback == lines {
            return;
        }
        self.config.terminal_scrollback = lines;
        let entities: Vec<Entity<TerminalPane>> = self.terminals.values().cloned().collect();
        for entity in entities {
            entity.update(cx, |pane, _| pane.set_scrollback(lines as usize));
        }
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 拖选停留自动复制时长。`0` = 关掉停留语义(退回「松手即复制」)。
    ///
    /// 存量终端要连带下发 —— 不然改了只对新开的终端生效。
    pub fn set_selection_auto_copy_secs(&mut self, secs: f64, cx: &mut Context<Self>) {
        if self.config.selection_auto_copy_secs == Some(secs) {
            return;
        }
        self.config.selection_auto_copy_secs = Some(secs);
        let dwell = self.selection_dwell();
        let entities: Vec<Entity<TerminalPane>> = self.terminals.values().cloned().collect();
        for entity in entities {
            entity.update(cx, |pane, cx| pane.set_selection_dwell(dwell, cx));
        }
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 把当前的终端字号/字族下发给**全部**已开终端。
    fn apply_terminal_style(&mut self, cx: &mut Context<Self>) {
        let style = self.terminal_style();
        let entities: Vec<Entity<TerminalPane>> = self.terminals.values().cloned().collect();
        for entity in entities {
            let style = style.clone();
            entity.update(cx, |pane, cx| pane.set_style(style, cx));
        }
    }

    // === 界面字号 / 字族 ===

    /// 把 `uiFontSize` / `uiFontFamily` 装进 [`crate::ui`] 的快照。
    ///
    /// **启动时也要调**(在建任何视图之前),否则首帧按默认 13px 画出来再被刷一遍。
    /// 与 `apply_theme_from_config` 同形:改一次快照,下一帧所有视图跟着变。
    pub fn apply_ui_font(&self) {
        crate::ui::set_ui_font(
            self.config.ui_font_size,
            self.config.ui_font_family.as_deref(),
        );
    }

    /// 界面字号(滑块 10..20)。**即时全局**,等价于原版改 `html` 的 `font-size`。
    pub fn set_ui_font_size(&mut self, size: f64, cx: &mut Context<Self>) {
        if (self.config.ui_font_size - size).abs() < f64::EPSILON {
            return;
        }
        self.config.ui_font_size = size;
        self.apply_ui_font();
        // 字号散在几十个 `render` 里,没有哪个 Entity 能代表「全部文字」——
        // 与切语言同一处理:让所有窗口重画(设置页一辈子也拖不了几次滑块)
        cx.refresh_windows();
        self.save_config_soon(cx);
        cx.notify();
    }

    /// 界面字族。空串 = 回落平台默认(写 `None`,不落空串)。
    pub fn set_ui_font_family(&mut self, family: Option<String>, cx: &mut Context<Self>) {
        let next = family
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty());
        if self.config.ui_font_family == next {
            return;
        }
        self.config.ui_font_family = next;
        self.apply_ui_font();
        cx.refresh_windows();
        self.save_config_soon(cx);
        cx.notify();
    }

    // === AI 感知(hook 页要用)===

    /// AI 桥的一份克隆(hook 服务器开关 / 状态查询)。
    pub fn ai(&self) -> AiBridge {
        self.ai.clone()
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

    /// 移动端改会话名:按 `pane_id` **全局**定位 —— 移动端只认得 pane,
    /// 不知道它挂在哪个项目下(`src/store.ts:1163-1180`)。
    ///
    /// 空串 = 清除自定义名、回落 shell 名。**不落盘**:`SavedPane` 里没有
    /// `customTitle`,AI 会话本来也活不过重启。
    ///
    /// 与 [`Self::rename_pane`] 并存:那条是 F2 / 右键改名(知道项目、要 trim),
    /// 这条是移动端来的 —— 标题**已经在 mt-relay 里收敛过**
    /// (trim + 去控制字符 + 64 字符限长,`relay.rs:709-716`),
    /// 这里不再叠加任何收敛,否则两处限长会打架。
    pub fn rename_pane_by_id(&mut self, pane_id: &str, title: &str, cx: &mut Context<Self>) {
        if rename_pane_in_states(&mut self.project_states, pane_id, title) {
            cx.notify();
        }
    }

    // === 移动端中转 ===

    /// `pty_id` → `(project_id, pane_id)` 反查。
    ///
    /// 移动端指令只带 PTY 编号,而 [`Self::write_to_pane`] 要「项目 + pane」。
    pub fn pane_of_pty(&self, pty_id: u32) -> Option<(String, String)> {
        find_pane_of_pty(&self.project_states, pty_id)
    }

    /// 这个 pane 的 PTY 起来了吗。
    ///
    /// `spawn_pane` 就算 PTY 起不来也照样返回 `PaneState`(视图里画一行红字),
    /// 而 [`Self::write_to_pane`] 在没有 PTY 时是静默丢弃的 —— 移动端发起会话的
    /// 回执要靠这一条把「终端根本没起来」与「命令已写入」分开。
    pub fn pane_pty_alive(&self, pty_id: u32, cx: &App) -> bool {
        self.terminals
            .get(&pty_id)
            .is_some_and(|entity| entity.read(cx).spawn_error().is_none())
    }

    /// 中转连接状态(`RelayEvents::status_changed` 的落点)。
    pub fn mobile_relay_status(&self) -> Option<&MobileRelayStatusPayload> {
        self.mobile_relay_status.as_ref()
    }

    pub fn set_mobile_relay_status(
        &mut self,
        status: MobileRelayStatusPayload,
        cx: &mut Context<Self>,
    ) {
        if self.mobile_relay_status.as_ref() == Some(&status) {
            return;
        }
        self.mobile_relay_status = Some(status);
        cx.notify();
    }

    /// 移动端中转配置的**读**口径:整块缺失时回落 `Default`(含预置两条启动器),
    /// 与 `mt_config` 的迁移(`config.rs:666`)同口径。
    pub fn mobile_relay(&self) -> MobileRelayConfig {
        self.config.mobile_relay.clone().unwrap_or_default()
    }

    /// 移动端中转配置的**改**口径,对应原版 `withMobileRelayDefaults` 的
    /// `{ relayUrl:'', desktopKey:'', launchers:[], ...current, ...patch }`:
    /// 整块缺失时 `launchers` 取**空列表而不是预置两条** ——
    /// 凭空补预置会跟后端「用户删光是有意结果」的迁移规则打架
    /// (`src/utils/mobileRelayConfig.ts:8-10`)。
    ///
    /// 与 [`Self::mobile_relay`] 的差别只在「整块缺失」这一种情况下可见,
    /// 而 `load()` 的迁移保证了正常路径上这一块必然在场。
    fn mobile_relay_for_patch(&self) -> MobileRelayConfig {
        self.config
            .mobile_relay
            .clone()
            .unwrap_or_else(|| MobileRelayConfig {
                relay_url: String::new(),
                desktop_key: String::new(),
                launchers: Vec::new(),
            })
    }

    /// 写中转地址 + 桌面端密钥,**其余字段(启动器)一个不动**。
    ///
    /// 立即落盘而不是 500ms 防抖(坑 8):原版是 `await saveConfigToDisk` 之后
    /// 才 `apply`,用户点完「保存并连接」立刻关掉应用,地址不该丢。
    pub fn set_mobile_relay_endpoint(&mut self, url: &str, key: &str, cx: &mut Context<Self>) {
        let mut relay = self.mobile_relay_for_patch();
        relay.relay_url = url.to_string();
        relay.desktop_key = key.to_string();
        self.config.mobile_relay = Some(relay);
        self.save_config_now();
        cx.notify();
    }

    /// 写启动器名单,**地址与密钥一个不动**。同样立即落盘。
    pub fn set_launchers(&mut self, launchers: Vec<AiLauncher>, cx: &mut Context<Self>) {
        let mut relay = self.mobile_relay_for_patch();
        relay.launchers = launchers;
        self.config.mobile_relay = Some(relay);
        self.save_config_now();
        cx.notify();
    }
}

// ===========================================================================
// SSH(audit #28,BB-a 批)
// ===========================================================================
//
// 三块:① 连接表 / 分组 CRUD(`SshModal.tsx` 的 `persist` 那一串);
// ② 远程项目(`AddRemoteProjectModal.tsx` + `remoteProject.ts`);
// ③ 「关联 SSH」的启用/停用(`SshAssocModal.tsx::handleSave`);
// 外加断线重连(`PaneGroup.tsx::handleReconnect` + `resetPaneForReconnect`)。
//
// BB-b 已把消费方全部接上(三个弹窗 + 远程项目 UI + 断线遮罩 + 文件树/会话
// 面板的远程分流),BB-a 留的 `allow(dead_code)` 随之删除 —— 从此这里多一个
// 没人调的函数就会在 `cargo check` 里红。
impl AppStore {
    /// 已保存的 SSH 连接(`config.sshConnections`)。
    pub fn ssh_connections(&self) -> &[SshConnection] {
        &self.config.ssh_connections
    }

    /// 显式创建的 SSH 分组名(允许空组;连接的 `group` 字段仍是归属单一来源)。
    pub fn ssh_groups(&self) -> &[String] {
        &self.config.ssh_groups
    }

    /// 新增或更新一条连接(按 id 判定,`SshModal.tsx::handleSave`)。
    ///
    /// **立即落盘**而不是 500ms 防抖:原版这条路是 `await saveConfigToDisk`,
    /// 密码/私钥路径这类东西不该在防抖窗口里被一次崩溃吃掉。
    pub fn upsert_ssh_connection(&mut self, conn: SshConnection, cx: &mut Context<Self>) {
        match self
            .config
            .ssh_connections
            .iter_mut()
            .find(|c| c.id == conn.id)
        {
            Some(slot) => *slot = conn,
            None => self.config.ssh_connections.push(conn),
        }
        self.save_config_now();
        cx.notify();
    }

    /// 删除一条连接(二次确认由调用方做 —— 原版同款)。
    ///
    /// **不级联清理**引用它的项目 / `sshConnectionIds`:原版就是这个语义,
    /// 远程项目因此进入「断链」错误态(仍可见、可删),关联范围静默收窄。
    pub fn remove_ssh_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        let before = self.config.ssh_connections.len();
        self.config.ssh_connections.retain(|c| c.id != id);
        if self.config.ssh_connections.len() == before {
            return;
        }
        self.save_config_now();
        cx.notify();
    }

    /// 新建一个空分组(重名则只切选中态,由调用方处理)。返回是否真的新建了。
    pub fn create_ssh_group(&mut self, name: &str, cx: &mut Context<Self>) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let exists = self
            .config
            .ssh_groups
            .iter()
            .any(|n| n.trim() == name)
            || self
                .config
                .ssh_connections
                .iter()
                .any(|c| c.group.as_deref().map(str::trim) == Some(name));
        if exists {
            return false;
        }
        self.config.ssh_groups.push(name.to_string());
        self.save_config_now();
        cx.notify();
        true
    }

    /// 分组改名:连接归属改名 + `sshGroups` 同步替换。
    /// 重命名为已有组名时**自然合并、去重**(原版 `renameGroup` 的注释原话)。
    pub fn rename_ssh_group(&mut self, old_name: &str, new_name: &str, cx: &mut Context<Self>) {
        let next = new_name.trim();
        if next.is_empty() || next == old_name {
            return;
        }
        self.config.ssh_groups =
            crate::ssh_conn::merge_ssh_groups_on_rename(&self.config.ssh_groups, old_name, next);
        for c in &mut self.config.ssh_connections {
            if c.group.as_deref().map(str::trim).filter(|g| !g.is_empty()) == Some(old_name) {
                c.group = Some(next.to_string());
            }
        }
        self.save_config_now();
        cx.notify();
    }

    /// 解散分组:组里的连接回落「未分组」,组名从 `sshGroups` 移除(连接不删)。
    pub fn dissolve_ssh_group(&mut self, name: &str, cx: &mut Context<Self>) {
        self.config.ssh_groups.retain(|n| n.trim() != name);
        for c in &mut self.config.ssh_connections {
            if c.group.as_deref().map(str::trim).filter(|g| !g.is_empty()) == Some(name) {
                c.group = None;
            }
        }
        self.save_config_now();
        cx.notify();
    }

    /// 把一条连接挪进某个分组(`group = None` = 挪到未分组)。
    pub fn move_ssh_connection_to_group(
        &mut self,
        conn_id: &str,
        group: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let target = group.map(str::trim).filter(|g| !g.is_empty());
        let Some(conn) = self
            .config
            .ssh_connections
            .iter_mut()
            .find(|c| c.id == conn_id)
        else {
            return;
        };
        let current = conn.group.as_deref().map(str::trim).filter(|g| !g.is_empty());
        if current == target {
            return;
        }
        conn.group = target.map(str::to_string);
        self.save_config_now();
        cx.notify();
    }

    // --- 远程项目 ---

    /// 这个项目是 SSH 远程项目吗(`remoteProject.ts::isRemoteProject`)。
    pub fn is_remote_project(&self, project_id: &str) -> bool {
        self.project(project_id)
            .is_some_and(crate::ssh_conn::is_remote_project)
    }

    /// 远程项目引用的连接;**断链**(连接被删)时 `None`。
    ///
    /// 返回克隆而不是引用:调用方多半要把它丢进 `background_executor`
    /// (`remote_ssh` 的入口全是阻塞函数,见那个模块的线程口径)。
    pub fn remote_connection_of(&self, project_id: &str) -> Option<SshConnection> {
        let project = self.project(project_id)?;
        crate::ssh_conn::remote_connection(project, &self.config.ssh_connections).cloned()
    }

    /// pane 显示名的统一口径:自定义名 > 远程连接名 > shell 名
    /// (`remoteProject.ts::paneDisplayLabel`)。tab 栏与项目预览浮层共用,
    /// 防两处口径漂移。
    pub fn pane_display_label(&self, project_id: &str, pane: &PaneState) -> String {
        if let Some(title) = pane.custom_title.as_deref().filter(|t| !t.is_empty()) {
            return title.to_string();
        }
        if let Some(project) = self.project(project_id)
            && crate::ssh_conn::is_remote_project(project)
        {
            return crate::ssh_conn::remote_pane_label(project, &self.config.ssh_connections);
        }
        pane.shell_name.clone()
    }

    /// 添加一个 SSH 远程项目并返回它的 id(`AddRemoteProjectModal.tsx::handleSave`
    /// 的落盘那一半 —— 远程路径的 `~` 展开与目录校验由调用方先跑
    /// [`crate::remote_ssh::validate_dir`],这里只接**已 canonicalize 的绝对路径**)。
    ///
    /// - `name` 为空时取路径末段(再取不到就用整条路径),与原版一字不差;
    /// - 远程项目**不参与** [`Self::find_project_by_path`] 的去重(那条判据显式
    ///   排除了 `ssh_connection_id.is_some()` 的项目):两台机器上的
    ///   `/home/u/proj` 是两个项目;
    /// - `target_group` 非空时落进该分组(分组折叠由调用方展开)。
    pub fn add_remote_project(
        &mut self,
        name: &str,
        connection_id: &str,
        remote_path: &str,
        target_group: Option<&str>,
        cx: &mut Context<Self>,
    ) -> String {
        let final_name = crate::ssh_conn::remote_project_name(name, remote_path);
        let id = gen_id("proj");
        self.config.projects.push(ProjectConfig {
            id: id.clone(),
            name: final_name,
            path: remote_path.to_string(),
            description: None,
            saved_layout: None,
            expanded_dirs: Vec::new(),
            ssh_mcp_enabled: false,
            ssh_cli_token: None,
            ssh_connection_ids: None,
            env_vars: Vec::new(),
            wsl_sessions_distro: None,
            ssh_connection_id: Some(connection_id.to_string()),
            parent_project_id: None,
            kind_override: None,
        });
        let tree = self.config.project_tree.get_or_insert_with(Vec::new);
        tree.push(mt_config::ProjectTreeItem::ProjectId(id.clone()));
        self.project_states.insert(id.clone(), ProjectState::new());
        self.expanded_dirs.insert(id.clone(), HashSet::new());
        if let Some(group_id) = target_group {
            self.move_item(&id, Some(group_id), None, cx);
        }
        self.save_config_now();
        cx.notify();
        id
    }

    // --- 「关联 SSH」(SSH 工具 = CLI + Skill)---

    /// 把「关联 SSH」的结果写回项目配置(`SshAssocModal.tsx` 落盘那一段)。
    ///
    /// 范围**始终存显式 id 列表**,不用 `None` 表示「全选」—— 见
    /// [`crate::ssh_conn::plan_assoc_save`] 里那条 v0.6.3 承诺。
    pub fn set_project_ssh_assoc(
        &mut self,
        project_id: &str,
        enabled: bool,
        project_token: Option<String>,
        scope: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.config.projects.iter_mut().find(|p| p.id == project_id) else {
            return;
        };
        project.ssh_mcp_enabled = enabled;
        project.ssh_cli_token = if enabled { project_token } else { None };
        project.ssh_connection_ids = if enabled { Some(scope) } else { None };
        self.save_config_now();
        cx.notify();
    }

    /// 「关联 SSH」保存的**完整**动作:算计划 → 后台跑注册器 → 回主线程落配置。
    ///
    /// 返回 `Task`,BB-b 的弹窗 `await` 它拿结果:`Ok(None)` = 什么都没做
    /// (从未启用且这次也没勾),`Ok(Some(outcome))` = 已落盘,按
    /// [`SshAssocOutcome::silent`] 决定弹不弹提示。
    ///
    /// **注册器是阻塞文件 IO**(还要写 home 下的 Codex / Claude 配置),
    /// 全程在 `background_executor` 上,主线程只负责最后那一次 `set_...`。
    pub fn apply_ssh_assoc(
        &mut self,
        project_id: &str,
        checked: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<SshAssocOutcome>, String>> {
        let Some(project) = self.project(project_id).cloned() else {
            return Task::ready(Ok(None));
        };
        let all_ids: Vec<String> = self
            .config
            .ssh_connections
            .iter()
            .map(|c| c.id.clone())
            .collect();
        let plan = crate::ssh_conn::plan_assoc_save(&project, &checked, &all_ids);
        let project_id = project_id.to_string();
        let project_dir = project.path.clone();
        let existing_token = project.ssh_cli_token.clone();

        cx.spawn(async move |this, cx| {
            let outcome = match plan {
                crate::ssh_conn::AssocPlan::NoOp => return Ok(None),
                crate::ssh_conn::AssocPlan::Enable {
                    silent,
                    was_enabled,
                } => {
                    let dir = project_dir.clone();
                    let token = existing_token.clone();
                    let res = cx
                        .background_executor()
                        .spawn(async move {
                            crate::ssh_registry::enable(&dir, token.as_deref())
                        })
                        .await?;
                    SshAssocOutcome {
                        enabled: true,
                        was_enabled,
                        silent,
                        scope_len: checked.len(),
                        total_len: all_ids.len(),
                        project_token: Some(res.project_token),
                        message: res.message,
                    }
                }
                crate::ssh_conn::AssocPlan::Disable => {
                    let dir = project_dir.clone();
                    let message = cx
                        .background_executor()
                        .spawn(async move { crate::ssh_registry::disable(&dir) })
                        .await?;
                    SshAssocOutcome {
                        enabled: false,
                        was_enabled: true,
                        silent: false,
                        scope_len: 0,
                        total_len: all_ids.len(),
                        project_token: None,
                        message,
                    }
                }
            };
            let scope = if outcome.enabled { checked } else { Vec::new() };
            let token = outcome.project_token.clone();
            let enabled = outcome.enabled;
            this.update(cx, |store: &mut AppStore, cx| {
                store.set_project_ssh_assoc(&project_id, enabled, token, scope, cx);
            })
            .map_err(|e| e.to_string())?;
            Ok(Some(outcome))
        })
    }

    // =======================================================================
    // 断线重连(exitedPtyIds 体系的写侧)
    // =======================================================================

    // 原版 `store.ts::clearPtyExited` 在这里**刻意没有对应物**:
    // GPUI 侧唯一的调用时机是重连,而 `reset_pane_for_reconnect` 走的
    // `dispose_terminal` 已经把退出登记连同标记/游标一起摘了(见那边的注释)。
    // 再留一个公开的单点摘除函数只会多一条会漂移的路。

    /// 远程 pane 重连:回收旧 PTY(连同标记/退出登记),就地起一条新的。
    ///
    /// 对应 `PaneGroup.tsx::handleReconnect` + `store.ts::resetPaneForReconnect`
    /// 那一对。原版是「置 `ptyId=undefined` + `status=idle`,让懒创建 effect
    /// 重新 `create_pty`」两步;GPUI 侧 PTY 是即时创建的,于是并成一步 ——
    /// **可观察行为完全一致**(旧终端连同回滚缓冲一并销毁,新会话从空屏开始)。
    ///
    /// 选清屏而非保留历史,理由照抄原版:新 PTY 的输出从头开始,旧 buffer 的
    /// 光标/滚动状态与新会话无法衔接,保留反而会出现「半屏旧内容 + 新登录横幅」
    /// 的错位;且 dispose 一并回收标记,链路与关 tab 完全一致,无新状态机。
    ///
    /// 返回新 PTY 编号;项目/pane 不在了返回 `None`。
    /// **本地 pane 同样适用** —— 原版覆盖层只画在远程 pane 上,但动作本身与
    /// 「远程」无关,判定留给调用方(BB-b 的覆盖层)。
    pub fn reset_pane_for_reconnect(
        &mut self,
        project_id: &str,
        pane_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<u32> {
        let project = self.project(project_id)?.clone();
        let old_pty = self
            .project_states
            .get(project_id)
            .and_then(|s| s.layout.as_ref())
            .and_then(|l| l.pane(pane_id))
            .and_then(|p| p.pty_id);
        // dispose 里已经做了:kill 子进程 + 清标记与游标 + 摘退出登记
        // (`clearMarkersForPty` / `clearPtyExited` 在原版是分开两调,这里同源)
        if let Some(old) = old_pty {
            self.dispose_terminal(old, cx);
        }

        let (shell_name, cwd) = {
            let pane = self
                .project_states
                .get(project_id)
                .and_then(|s| s.layout.as_ref())
                .and_then(|l| l.pane(pane_id))?;
            (pane.shell_name.clone(), pane.cwd.clone())
        };
        let shell = self.resolve_shell(Some(&shell_name))?;
        let new_pty = self.start_pty(&project, &shell, cwd.as_deref(), cx);

        let state = self.project_states.get_mut(project_id)?;
        let layout = state.layout.as_mut()?;
        let pane = layout.pane_mut(pane_id)?;
        pane.pty_id = Some(new_pty);
        pane.status = PaneStatus::Idle;
        state.status = layout.highest_status();
        self.after_layout_change(project_id, cx);
        Some(new_pty)
    }
}

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
        let project = self.project(project_id)?.clone();
        let mut pane = self.spawn_pane(&project, &shell, None, window, cx)?;
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
        match state.layout.as_mut() {
            // 项目还一个终端都没有:新建根叶子,否则终端区仍是空白
            None => state.layout = Some(SplitNode::leaf(pane)),
            Some(layout) => {
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
        }
        self.after_layout_change(project_id, cx);
        Some(pane_id)
    }

    // === 右侧抽屉宽度 ===

    /// 抽屉宽度。缺省 **340**(`App.tsx:541` 的 `?? 340`),钳在 240~720
    /// (`RightDrawer.tsx:8-9`)。
    pub fn right_drawer_width(&self) -> f64 {
        self.config.right_drawer_width.unwrap_or(340.0).clamp(240.0, 720.0)
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
        cx.notify();
    }

    pub fn set_right_drawer_width(&mut self, width: f64, cx: &mut Context<Self>) {
        let width = width.clamp(240.0, 720.0);
        if self.config.right_drawer_width == Some(width) {
            return;
        }
        self.config.right_drawer_width = Some(width);
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
        cx.notify();
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
            // 被最大化的那个 pane 关掉了 → 自动回落显示整树。原版是在渲染处
            // 「按 id 查不到叶子就退回整树」,这里顺手把陈旧 id 也清掉:留着它
            // 只会让 `maximized_pane_id()` 每帧多查一次,且没有任何复活路径
            // (pane id 是进程内单调递增的,不会被重新分配)。
            if let Some(id) = state.maximized_pane_id.clone()
                && state.layout.as_ref().and_then(|l| l.pane(&id)).is_none()
            {
                state.maximized_pane_id = None;
            }
        }
        // 关掉的 pane 一并撤出完成队列:否则未读计数会往一个已经不存在的 pane
        // 上跳,两张表也会随开关终端无界增长(旧版 setProjectLayout 的同一段)。
        self.done.retain_panes(&self.live_pane_ids());
        self.save_project_layout_soon(project_id, cx);
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

    // ─── 布局落盘(layout.db)────────────────────────────────────────────
    //
    // 与配置分家的理由见 `mt-layout` 的模块注释:布局是交互频次的数据,不该
    // 每改一次就把整份 config.json 连同 .bak 重写一遍。这里只保留防抖 ——
    // 一次 upsert 便宜,但拖分隔条期间每帧一次仍是浪费。

    /// 把某个项目当前的树序列化进内存缓存,并排上落盘。
    fn save_project_layout_soon(&mut self, project_id: &str, cx: &mut Context<Self>) {
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
    fn schedule_layout_flush(&mut self, cx: &mut Context<Self>) {
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

    /// 立即把攒下的布局写进 `layout.db`(退出前 / 防抖到点)。
    ///
    /// 库开不起来时是 no-op —— 脏标记照样清掉,免得每次退出都重试一遍必然失败的
    /// 写入、把日志刷满。
    pub fn flush_layout_now(&mut self) {
        let dirty_projects = std::mem::take(&mut self.layout_dirty_projects);
        let globals_dirty = std::mem::take(&mut self.layout_globals_dirty);
        let Some(store) = self.layout_store.clone() else {
            return;
        };

        if globals_dirty {
            let globals = mt_layout::GlobalLayout {
                layout_sizes: self.config.layout_sizes.clone(),
                middle_column_sizes: self.config.middle_column_sizes.clone(),
                middle_column_visible: Some(self.config.middle_column_visible),
                right_drawer_width: self.config.right_drawer_width,
                window: self.window_geometry,
            };
            if let Err(err) = store.save_globals(&globals) {
                eprintln!("[layout] 全局布局写盘失败: {err:#}");
            }
        }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        for project_id in dirty_projects {
            // 项目在防抖窗口里被删掉了 → 删行(它的树已经不在 config 里了)
            let Some(project) = self.config.projects.iter().find(|p| p.id == project_id) else {
                if let Err(err) = store.delete_project_layout(&project_id) {
                    eprintln!("[layout] 删除项目 {project_id} 的布局失败: {err:#}");
                }
                continue;
            };
            let result = match project.saved_layout.as_ref() {
                Some(layout) => store.save_project_layout(&project_id, layout, now_ms),
                None => store.delete_project_layout(&project_id),
            };
            if let Err(err) = result {
                eprintln!("[layout] 项目 {project_id} 的布局写盘失败: {err:#}");
            }
        }
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

    /// 立即写盘(退出前 / 项目切换)。
    ///
    /// 令牌语义与装机版一致:令牌过期说明别处写过配置,必须先重读拿到新令牌。
    /// 单进程壳里「别处」只可能是本进程的另一次 load,手上这份就是最新的,
    /// 于是重读一次令牌后原样重写。
    ///
    /// 顺手把布局也刷下去:两条落盘路径分家后,退出钩子只调这一个入口 ——
    /// 让它把两边都收干净,比要求每个调用点记得调两次可靠。
    pub fn save_config_now(&mut self) {
        self.flush_layout_now();
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

// ─── AI 项目聚合 / 标题栏状态灯的纯函数(可测) ────────────────

/// [`AppStore::ai_projects`] 的 done 判据取哪一套。
///
/// 原版 `collectAiProjects` 把这件事做成了参数(`donePaneIds`),两个调用点各传
/// 各的集合;这里把选择权收成一个枚举,判据本身仍住在 `DoneTracker` 里。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DoneScope {
    /// 全部完成记录(旧版 `aiDoneOrder`)。**不看窗口焦点** —— 标题栏胶囊与
    /// 全局状态灯用这一套(`TitleBar.tsx:118` 原注释)。
    All,
    /// 未读完成(旧版 `unreadDonePaneIds`,聚焦即清)。托盘用这一套 ——
    /// 绿灯的语义是「有你还没看过的回答」,窗口一聚焦就该灭。
    Unread,
}

/// 一个项目在托盘菜单 / 标题栏胶囊里的档位。
///
/// **声明顺序即排序**(`AI_PROJECT_KIND_ORDER`:attention 0 > working 1 >
/// done 2 > idle 3),`derive(Ord)` 直接给出同一个次序。
/// ⚠️ 与「点击跳转」的优先级有意不同(那条是 待确认 > 最先完成 > 处理中,
/// 见 [`crate::notify::pick_attention_target`])。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum AiProjectKind {
    Attention,
    Working,
    Done,
    Idle,
}

impl AiProjectKind {
    /// 与 TS 侧 `AiProjectEntry['kind']` 一字不差的字符串口径。
    ///
    /// **仍然只有单测在用**(所以 `allow(dead_code)` 还留着)。此前这里预告
    /// 「托盘菜单的标签会用到它」—— 实际没有:TS 侧是拿 kind 字符串去拼
    /// `app.trayStatus.${kind}` 这个 key,而 Rust 的 `t()` 只吃 `&'static str`,
    /// 拼不出来,于是那条路走的是 [`Self::tray_status_key`](见下),emoji 那半
    /// 走 [`crate::tray::kind_emoji`] 的 match。留着它是为了钉住四个档位的对外
    /// 字符串口径与 TS 一致。
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attention => "attention",
            Self::Working => "working",
            Self::Done => "done",
            Self::Idle => "idle",
        }
    }

    /// 下拉行右侧那句状态文案的 key(`app.trayStatus.{kind}`,与托盘菜单共用)。
    pub fn tray_status_key(self) -> &'static str {
        match self {
            Self::Attention => "trayStatus.attention",
            Self::Working => "trayStatus.working",
            Self::Done => "trayStatus.done",
            Self::Idle => "trayStatus.idle",
        }
    }
}

/// 进入 AI agent 的一个项目(对应 TS 的 `AiProjectEntry`)。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AiProjectEntry {
    pub id: String,
    pub name: String,
    pub kind: AiProjectKind,
}

/// [`collect_ai_projects`] 的产物:三个 **pane 级**计数 + 按项目聚合的明细。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct AiProjects {
    pub attention: usize,
    pub working: usize,
    pub done: usize,
    pub entries: Vec<AiProjectEntry>,
}

/// 按项目聚合出「进入 AI agent 的项目」。逐条照抄 `store.ts:273-315`:
///
/// - **入选**:项目下任一 pane 处于 attention / working / ai-idle / done 四态之一
///   (`ai-idle` 只是「agent 在场」,照样入列,但**不点灯**);
/// - **档位**:项目内取最高一档 attention > working > done > idle;
/// - **pane 级计数**:`status == error || attention` 记 attention,`ai-working` 记
///   working,`is_done(pane) && status != ai-working` 记 done ——
///   注意 done 与前三条**不是** if/else 链,一个 pane 可以同时进 attention 与 done
///   的计数(原版就是两段独立判断);
/// - **名字**:配置里查不到就退回项目 id(原版 `?? pid`)。
///
/// # 与原版唯一的偏差:同档内的先后
///
/// TS 侧 `entries.sort()` 是**稳定**排序,同档内保留 `projectStates` 的插入序;
/// Rust 侧的来源是 `HashMap`,遍历序每次都可能不同。这里改用**配置里的项目次序**
/// 当同档内的第二关键字 —— 既确定,又与项目列表上下顺序一致。
pub fn collect_ai_projects<'a>(
    panes: impl IntoIterator<Item = PaneRef<'a>>,
    projects: &[ProjectConfig],
    is_done: impl Fn(&str) -> bool,
) -> AiProjects {
    // 项目 id → (最高档的四个标志位, 配置里的次序)
    let mut acc: HashMap<&'a str, [bool; 4]> = HashMap::new();
    let mut out = AiProjects::default();

    for pane in panes {
        let slot = acc.entry(pane.project_id).or_insert([false; 4]);
        if pane.status == PaneStatus::Error || pane.attention {
            out.attention += 1;
            slot[0] = true;
        } else if pane.status == PaneStatus::AiWorking {
            out.working += 1;
            slot[1] = true;
        } else if pane.status == PaneStatus::AiIdle {
            slot[3] = true;
        }
        // 只数仍存在的 pane(关掉即失效);又开始工作的不再算「已完成」
        if is_done(pane.pane_id) && pane.status != PaneStatus::AiWorking {
            out.done += 1;
            slot[2] = true;
        }
    }

    let rank = |id: &str| {
        projects
            .iter()
            .position(|p| p.id == id)
            .unwrap_or(usize::MAX)
    };
    let mut entries: Vec<(usize, AiProjectEntry)> = acc
        .into_iter()
        .filter_map(|(id, [attention, working, done, idle])| {
            if !(attention || working || done || idle) {
                return None;
            }
            let kind = if attention {
                AiProjectKind::Attention
            } else if working {
                AiProjectKind::Working
            } else if done {
                AiProjectKind::Done
            } else {
                AiProjectKind::Idle
            };
            let name = projects
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| id.to_string());
            Some((
                rank(id),
                AiProjectEntry {
                    id: id.to_string(),
                    name,
                    kind,
                },
            ))
        })
        .collect();
    entries.sort_by(|a, b| a.1.kind.cmp(&b.1.kind).then(a.0.cmp(&b.0)));
    out.entries = entries.into_iter().map(|(_, e)| e).collect();
    out
}

/// 标题栏那颗全局状态灯的五档(`TitleBar.tsx:57` 的 `LightKind`)。
///
/// **声明顺序即优先级**(idle 最低、error 最高),`derive(Ord)` 直接可比 ——
/// 原版那张 `LIGHT_ORDER` 表不必再抄一遍。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Debug)]
pub enum TitleBarLight {
    #[default]
    Idle,
    Done,
    Working,
    Attention,
    Error,
}

impl TitleBarLight {
    /// tooltip / aria-label 的 key(`app.titleBar.status.{light}`)。
    pub fn i18n_key(self) -> &'static str {
        match self {
            Self::Error => "titleBar.status.error",
            Self::Attention => "titleBar.status.attention",
            Self::Working => "titleBar.status.working",
            Self::Done => "titleBar.status.done",
            Self::Idle => "titleBar.status.idle",
        }
    }
}

/// 遍历所有项目所有 pane,取最紧急的一档(`TitleBar.tsx::computeLight`)。
///
/// 判据是 **if/else 链,先中先算**:`error` → `attention` → `ai-working` →
/// 「完成过」。一个 pane 只贡献一档。
pub fn compute_title_bar_light<'a>(
    panes: impl IntoIterator<Item = PaneRef<'a>>,
    is_done: impl Fn(&str) -> bool,
) -> TitleBarLight {
    let mut light = TitleBarLight::Idle;
    for pane in panes {
        let bump = if pane.status == PaneStatus::Error {
            TitleBarLight::Error
        } else if pane.attention {
            TitleBarLight::Attention
        } else if pane.status == PaneStatus::AiWorking {
            TitleBarLight::Working
        } else if is_done(pane.pane_id) {
            TitleBarLight::Done
        } else {
            continue;
        };
        light = light.max(bump);
    }
    light
}

// ─── 移动端中转的纯逻辑(可测) ───────────────────────────────
//
// 两条都拆成自由函数,是因为它们的语义(全局定位、空串清名、命中即收工)
// 比调用点更值得钉住,而 `AppStore` 的方法要 `Context<Self>` —— 单测里没有。

/// 在**全部项目**的布局里按 `pane_id` 定位并改自定义名。返回「有没有真改动」。
///
/// - 空标题 = 清除自定义名(回落 shell 名);
/// - `pane_id` 全局唯一,命中即收工,不再看其它项目;
/// - 一个都没命中:什么都不改。
/// 最大化开关的三态口径,抽成纯函数好单测(`store.ts:938` 那一行的等价物):
/// 传 `Some(id)` 且当前不是它 → 换成它;传 `None`、或传的正是当前值 → 还原。
///
/// 「传的正是当前值 → 还原」就是双击/点按钮的 toggle 语义:同一个 pane 再来一次
/// 就是收回去。
fn next_maximized(current: Option<&str>, requested: Option<&str>) -> Option<String> {
    match requested {
        Some(id) if current != Some(id) => Some(id.to_string()),
        _ => None,
    }
}

fn rename_pane_in_states(
    states: &mut HashMap<String, ProjectState>,
    pane_id: &str,
    title: &str,
) -> bool {
    let next = if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    };
    for state in states.values_mut() {
        let Some(layout) = state.layout.as_mut() else {
            continue;
        };
        let Some(pane) = layout.pane_mut(pane_id) else {
            continue;
        };
        if pane.custom_title == next {
            return false;
        }
        pane.custom_title = next;
        return true;
    }
    false
}

/// `pty_id` → `(project_id, pane_id)`。
fn find_pane_of_pty(
    states: &HashMap<String, ProjectState>,
    pty_id: u32,
) -> Option<(String, String)> {
    states.iter().find_map(|(project_id, state)| {
        state
            .layout
            .as_ref()
            .and_then(|layout| layout.pane_by_pty(pty_id))
            .map(|pane| (project_id.clone(), pane.id.clone()))
    })
}

// ─── 终端渲染参数的纯函数(可测) ──────────────────────────────

/// 回滚行数上限(`src/utils/terminalScrollback.ts::MAX_SCROLLBACK`)。
pub const MAX_SCROLLBACK: u32 = 200_000;
/// 回滚行数缺省值(同上的 `DEFAULT_SCROLLBACK`;`config.rs` 的 serde 默认同值)。
pub const DEFAULT_SCROLLBACK: u32 = 10_000;

/// 回滚行数的钳制,逐条对照 `terminalScrollback.ts::resolveScrollback`:
/// **非数字 / NaN / 负数 → 回落 10000**;否则 `min(round(v), 200000)`。
///
/// 入参取 `f64` 是为了把「用户在输入框里打了什么」这一路也覆盖进来 ——
/// 配置字段虽是 `u32`,设置页拿到的是一串文本。
pub fn resolve_scrollback(raw: f64) -> u32 {
    if !raw.is_finite() || raw < 0.0 {
        return DEFAULT_SCROLLBACK;
    }
    (raw.round() as u64).min(MAX_SCROLLBACK as u64) as u32
}

/// CSS 通用族名。gpui 的字体解析不认它们,留在回退串里等于占一个查不到的位置。
const GENERIC_FAMILIES: [&str; 5] = ["monospace", "sans-serif", "serif", "system-ui", "ui-monospace"];

/// CJK 回退串(`terminalCache.ts:48` 的 `CJK_FALLBACK_FONTS`)。
/// 原版把它接在**用户自选字体**后面,这里同样。
const CJK_FALLBACK_FONTS: [&str; 3] = ["Microsoft YaHei", "PingFang SC", "Noto Sans CJK SC"];
/// emoji 回退。`TerminalStyle::default()` 里本来就有,自定义字族时别弄丢。
const EMOJI_FALLBACK: &str = "Segoe UI Emoji";

/// `config.terminalFontSize` + `terminalFontFamily` → [`TerminalStyle`]。
///
/// 字族那一串是 CSS `font-family` 语法(原版直接喂 xterm),而
/// [`TerminalStyle`] 是「主字体 + 回退列表」两段式:取首项当主字体,其余进回退,
/// 再自动补 CJK 与 emoji —— 与原版 `resolveTerminalFontFamily` 同语义
/// (它是往用户串后面拼 `CJK_FALLBACK_FONTS`)。
///
/// 字族为空 / 只写了通用族名时整段回落 [`TerminalStyle::default`]。
pub fn terminal_style_from(size: f64, family: Option<&str>) -> TerminalStyle {
    let mut style = TerminalStyle {
        font_size: gpui::px(size as f32),
        ..TerminalStyle::default()
    };
    let Some(list) = family.map(str::trim).filter(|s| !s.is_empty()) else {
        return style;
    };
    let mut families = crate::ui::font_family_list(list);
    families.retain(|f| !GENERIC_FAMILIES.contains(&f.to_ascii_lowercase().as_str()));
    if families.is_empty() {
        return style;
    }
    style.font_family = families.remove(0).into();
    for extra in CJK_FALLBACK_FONTS.iter().chain([&EMOJI_FALLBACK]) {
        if !families.iter().any(|f| f == extra) {
            families.push((*extra).to_string());
        }
    }
    style.font_fallbacks = families.into_iter().map(Into::into).collect();
    style
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

/// fork 出的新 PTY 该以哪个目录启动。
///
/// 取值链与续接**完全同一条**([`resolve_resume_cwd`]):hook 上报的 `session.cwd`
/// (带 `is_dir` 预检)→(claude 系)`lookup_ai_session_cwd` 反查 → `None` 回落
/// 源 pane 目录。`claude --resume … --fork-session` 与 `--resume` 一样只认
/// 「启动目录」对应的会话桶,起于子目录的会话在别处 fork 会报
/// `No conversation found`;codex 不按目录分桶,继承源 pane 目录即可
/// (还避开它的 `resume_cwd` 选目录提问)。
///
/// **同步磁盘遍历**,调用方必须丢后台(见 [`fork_pane_session`])。
pub fn resolve_fork_cwd(session: &AiSessionRef) -> Option<String> {
    resolve_resume_cwd(session)
}

/// 一条待落账的 fork 登记(`src/store.ts:173` 的 `pendingForks` 值)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFork {
    /// 归一化(小写)的 agent 标识。
    pub agent: String,
    /// 被 fork 的那个会话 id。
    pub parent_session_id: String,
}

/// 一次 pending 登记遇上新会话身份时该不该落边(纯逻辑,`consumePendingFork` 的判据)。
///
/// 三条否决(逐条照抄原版):
/// 1. **agent 不符** —— fork 失败后用户在同一个 pane 里起了别家,登记只作废不记边;
/// 2. **id 为空** —— 身份还没成形;
/// 3. **id 等于父** —— claude 的 `--resume` 幂等上报同一个 id(没真分出去)。
///
/// 归一化口径与 [`crate::session_branch::branch_caps_for_agent`] 同:两边都先小写。
/// 同 agent 的**全新**会话被误记仍有残余风险 —— 磁盘边合并时优先、且该 pane
/// 首次身份即消费,窗口压到最小(原版同一条注释)。
pub fn resolve_fork_edge(
    pending: &PendingFork,
    session: &AiSessionRef,
) -> Option<mt_config::SavedLineageEdge> {
    let agent = session
        .agent
        .as_deref()
        .unwrap_or("claude")
        .to_ascii_lowercase();
    if agent != pending.agent {
        return None;
    }
    if session.session_id.is_empty() || session.session_id == pending.parent_session_id {
        return None;
    }
    Some(mt_config::SavedLineageEdge {
        agent,
        session_id: session.session_id.clone(),
        parent_session_id: pending.parent_session_id.clone(),
        // 分叉点 uuid 只有 Claude 的磁盘指针有这个精度;自记账拿不到
        fork_point_uuid: None,
    })
}

/// 把一条边并进自记账表;child 已有边就**不覆盖**,返回是否真写了。
///
/// 「先记为准」:同一个 child 不可能有两个父,后来的那条只可能是误记
/// (磁盘合并层还会再压一层,见 `session_branch::merge_lineage_edges`)。
pub fn push_lineage_edge(
    existing: &mut Vec<mt_config::SavedLineageEdge>,
    edge: mt_config::SavedLineageEdge,
) -> bool {
    if existing.iter().any(|e| e.session_id == edge.session_id) {
        return false;
    }
    existing.push(edge);
    true
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

    fn project(id: &str, name: &str) -> ProjectConfig {
        ProjectConfig {
            id: id.to_string(),
            name: name.to_string(),
            path: format!("/tmp/{id}"),
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
        }
    }

    fn pane<'a>(project_id: &'a str, pane_id: &'a str, status: PaneStatus, attention: bool) -> PaneRef<'a> {
        PaneRef {
            project_id,
            pane_id,
            status,
            attention,
        }
    }

    fn kinds(projects: &AiProjects) -> Vec<(&str, &'static str)> {
        projects
            .entries
            .iter()
            .map(|e| (e.id.as_str(), e.kind.as_str()))
            .collect()
    }

    /// 入选口径:任一 pane 有 AI 会话(含 ai-idle)即入列;纯 shell 的项目不入列。
    #[test]
    fn ai项目入选只看有没有_ai会话() {
        let projects = [project("p1", "一"), project("p2", "二"), project("p3", "三")];
        let panes = vec![
            // p1:只有裸 shell —— 不入列
            pane("p1", "a", PaneStatus::Idle, false),
            // p2:agent 在场但空闲 —— 入列,档位 idle
            pane("p2", "b", PaneStatus::AiIdle, false),
            // p3:正在跑
            pane("p3", "c", PaneStatus::AiWorking, false),
        ];
        let out = collect_ai_projects(panes, &projects, |_| false);
        assert_eq!(kinds(&out), vec![("p3", "working"), ("p2", "idle")]);
        assert_eq!((out.attention, out.working, out.done), (0, 1, 0));
    }

    /// 项目内取最高一档:attention > working > done > idle。
    #[test]
    fn ai项目档位取项目内最高一档() {
        let projects = [project("p1", "一")];
        // 同一个项目里三个 pane,最高的是 attention
        let panes = vec![
            pane("p1", "a", PaneStatus::AiIdle, false),
            pane("p1", "b", PaneStatus::AiWorking, false),
            pane("p1", "c", PaneStatus::AiIdle, true),
        ];
        let out = collect_ai_projects(panes, &projects, |_| false);
        assert_eq!(kinds(&out), vec![("p1", "attention")]);
        // pane 级计数与项目档位是两回事:working 那个照样计数
        assert_eq!((out.attention, out.working, out.done), (1, 1, 0));
    }

    /// `error` 与 `attention` 同归 attention 一档(原版 `status==='error' || pane.attention`)。
    #[test]
    fn 异常_pane_算待确认() {
        let projects = [project("p1", "一")];
        let out = collect_ai_projects(
            vec![pane("p1", "a", PaneStatus::Error, false)],
            &projects,
            |_| false,
        );
        assert_eq!(kinds(&out), vec![("p1", "attention")]);
        assert_eq!(out.attention, 1);
    }

    /// done 判据:在集合里且**不在跑**才算;又开始工作的不再算「已完成」。
    #[test]
    fn 已完成判据排除又开始跑的() {
        let projects = [project("p1", "一"), project("p2", "二")];
        let panes = vec![
            pane("p1", "a", PaneStatus::AiIdle, false),
            pane("p2", "b", PaneStatus::AiWorking, false),
        ];
        // 两个 pane 都在 done 集合里,但 b 正在跑 —— 只有 a 算完成
        let out = collect_ai_projects(panes, &projects, |_| true);
        assert_eq!(out.done, 1);
        assert_eq!(kinds(&out), vec![("p2", "working"), ("p1", "done")]);
    }

    /// 排序:attention > working > done > idle;同档内按**配置里的项目次序**。
    #[test]
    fn ai项目排序按档位再按配置次序() {
        let projects = [
            project("p1", "一"),
            project("p2", "二"),
            project("p3", "三"),
            project("p4", "四"),
            project("p5", "五"),
        ];
        let panes = vec![
            pane("p5", "e", PaneStatus::AiIdle, false),
            pane("p4", "d", PaneStatus::AiIdle, false),
            pane("p3", "c", PaneStatus::AiWorking, false),
            pane("p2", "b", PaneStatus::AiWorking, false),
            pane("p1", "a", PaneStatus::AiIdle, true),
        ];
        let out = collect_ai_projects(panes, &projects, |_| false);
        assert_eq!(
            kinds(&out),
            vec![
                ("p1", "attention"),
                ("p2", "working"),
                ("p3", "working"),
                ("p4", "idle"),
                ("p5", "idle"),
            ]
        );
    }

    /// 名字取配置;配置里查不到就退回项目 id(原版 `?? pid`)。
    #[test]
    fn ai项目名缺配置时退回_id() {
        let projects = [project("p1", "正经名字")];
        let panes = vec![
            pane("p1", "a", PaneStatus::AiIdle, false),
            pane("ghost", "b", PaneStatus::AiIdle, false),
        ];
        let out = collect_ai_projects(panes, &projects, |_| false);
        let names: Vec<&str> = out.entries.iter().map(|e| e.name.as_str()).collect();
        // 查不到的排在最后(rank = usize::MAX)
        assert_eq!(names, vec!["正经名字", "ghost"]);
    }

    /// **不裁剪、不限条数**(与托盘的 `trayMaxProjects` 不同 —— 那道闸在调用方)。
    #[test]
    fn ai项目列表不做截断() {
        let projects: Vec<ProjectConfig> = (0..30)
            .map(|i| project(&format!("p{i}"), &format!("项目{i}")))
            .collect();
        let ids: Vec<String> = (0..30).map(|i| format!("p{i}")).collect();
        let panes: Vec<PaneRef<'_>> = ids
            .iter()
            .map(|id| pane(id.as_str(), id.as_str(), PaneStatus::AiIdle, false))
            .collect();
        let out = collect_ai_projects(panes, &projects, |_| false);
        assert_eq!(out.entries.len(), 30);
    }

    /// 空输入 = 空结果(下拉里那条「暂无进入 AI 会话的项目」的判据)。
    #[test]
    fn 没有_ai_会话时列表为空() {
        let out = collect_ai_projects(Vec::new(), &[], |_| false);
        assert_eq!(out, AiProjects::default());
    }

    /// 状态灯五档的优先级:error 最高,idle 兜底(**与边条口径相反**)。
    #[test]
    fn 状态灯取最紧急一档() {
        let done = |id: &str| id == "d";
        // 空 = idle
        assert_eq!(compute_title_bar_light(Vec::new(), done), TitleBarLight::Idle);
        // 完成
        assert_eq!(
            compute_title_bar_light(vec![pane("p", "d", PaneStatus::Idle, false)], done),
            TitleBarLight::Done
        );
        // 处理中压过完成
        assert_eq!(
            compute_title_bar_light(
                vec![
                    pane("p", "d", PaneStatus::Idle, false),
                    pane("p", "w", PaneStatus::AiWorking, false),
                ],
                done
            ),
            TitleBarLight::Working
        );
        // 待确认压过处理中
        assert_eq!(
            compute_title_bar_light(
                vec![
                    pane("p", "w", PaneStatus::AiWorking, false),
                    pane("p", "a", PaneStatus::AiIdle, true),
                ],
                done
            ),
            TitleBarLight::Attention
        );
        // error 压过一切 —— 标题栏灯**保留** error,不像边条那样压成 idle
        assert_eq!(
            compute_title_bar_light(
                vec![
                    pane("p", "a", PaneStatus::AiIdle, true),
                    pane("p", "e", PaneStatus::Error, false),
                ],
                done
            ),
            TitleBarLight::Error
        );
    }

    /// 判据是 if/else 链,一个 pane 只贡献一档:`error` 的 pane 即便也在 done
    /// 集合里,也只按 error 算(不会因为「完成过」被降档)。
    #[test]
    fn 状态灯一个_pane_只贡献一档() {
        // attention 的 pane 同时在 done 集合里 —— 取 attention 不取 done
        assert_eq!(
            compute_title_bar_light(vec![pane("p", "x", PaneStatus::AiIdle, true)], |_| true),
            TitleBarLight::Attention
        );
        // 正在跑的 pane 同时在 done 集合里 —— 取 working
        assert_eq!(
            compute_title_bar_light(vec![pane("p", "x", PaneStatus::AiWorking, false)], |_| true),
            TitleBarLight::Working
        );
    }

    /// 五档各自的 tooltip key 都指向 `app.titleBar.status.*`(拼错就是空 tooltip)。
    #[test]
    fn 状态灯文案_key_齐全() {
        for (light, key) in [
            (TitleBarLight::Error, "titleBar.status.error"),
            (TitleBarLight::Attention, "titleBar.status.attention"),
            (TitleBarLight::Working, "titleBar.status.working"),
            (TitleBarLight::Done, "titleBar.status.done"),
            (TitleBarLight::Idle, "titleBar.status.idle"),
        ] {
            assert_eq!(light.i18n_key(), key);
            for locale in mt_i18n::Locale::ALL {
                assert!(
                    mt_i18n::lookup(locale, "app", key).is_some(),
                    "字典缺条目 app.{key}({locale})"
                );
            }
        }
        for kind in [
            AiProjectKind::Attention,
            AiProjectKind::Working,
            AiProjectKind::Done,
            AiProjectKind::Idle,
        ] {
            for locale in mt_i18n::Locale::ALL {
                assert!(
                    mt_i18n::lookup(locale, "app", kind.tray_status_key()).is_some(),
                    "字典缺条目 app.{}({locale})",
                    kind.tray_status_key()
                );
            }
        }
    }

    fn session(agent: Option<&str>, id: &str) -> AiSessionRef {
        AiSessionRef {
            agent: agent.map(str::to_string),
            session_id: id.to_string(),
            cwd: None,
        }
    }

    // ─── 移动端改会话名 / pty 反查 ───────────────────────────

    /// 两个项目各一棵布局,pane id 与 pty id 都在其中。
    fn two_projects() -> (HashMap<String, ProjectState>, String, String) {
        let mut a = PaneState::new("pwsh");
        a.pty_id = Some(1);
        let mut b = PaneState::new("bash");
        b.pty_id = Some(2);
        let (a_id, b_id) = (a.id.clone(), b.id.clone());

        let mut states = HashMap::new();
        let mut sa = ProjectState::new();
        sa.layout = Some(SplitNode::leaf(a));
        states.insert("p-a".to_string(), sa);
        let mut sb = ProjectState::new();
        sb.layout = Some(SplitNode::leaf(b));
        states.insert("p-b".to_string(), sb);
        // 布局还没建出来的项目也要能安全跳过
        states.insert("p-empty".to_string(), ProjectState::new());
        (states, a_id, b_id)
    }

    fn title_of(states: &HashMap<String, ProjectState>, pane_id: &str) -> Option<String> {
        states
            .values()
            .filter_map(|s| s.layout.as_ref())
            .find_map(|l| l.pane(pane_id))
            .and_then(|p| p.custom_title.clone())
    }

    /// 移动端只认得 pane —— 改名必须跨项目找,而且找的是**第二个**项目里那个
    /// 也要能命中(HashMap 的遍历顺序不定,这条同时钉住「不依赖顺序」)。
    #[test]
    fn 改会话名按_pane_id_跨项目定位() {
        let (mut states, _a_id, b_id) = two_projects();
        assert!(rename_pane_in_states(&mut states, &b_id, "手机改的名"));
        assert_eq!(title_of(&states, &b_id).as_deref(), Some("手机改的名"));
    }

    /// 空串 = 清掉自定义名、回落 shell 名(不是存一个空标题)。
    #[test]
    fn 改会话名传空串等于清除自定义名() {
        let (mut states, a_id, _) = two_projects();
        assert!(rename_pane_in_states(&mut states, &a_id, "X"));
        assert!(rename_pane_in_states(&mut states, &a_id, ""));
        assert_eq!(title_of(&states, &a_id), None);
        // 已经是默认名了,再清一次不算改动(省掉一次无谓的重绘)
        assert!(!rename_pane_in_states(&mut states, &a_id, ""));
    }

    /// 一个都没命中:什么都不改,也不报错(pane 可能刚被关掉)。
    #[test]
    fn 改会话名未命中时什么都不改() {
        let (mut states, a_id, b_id) = two_projects();
        assert!(!rename_pane_in_states(&mut states, "pane-不存在", "X"));
        assert_eq!(title_of(&states, &a_id), None);
        assert_eq!(title_of(&states, &b_id), None);
    }

    /// 同名再改一次不算改动 —— 结构同步的内容去重靠它少发一轮。
    #[test]
    fn 改会话名同名时不算改动() {
        let (mut states, a_id, _) = two_projects();
        assert!(rename_pane_in_states(&mut states, &a_id, "同一个名"));
        assert!(!rename_pane_in_states(&mut states, &a_id, "同一个名"));
    }

    #[test]
    fn pty_反查得到项目与_pane() {
        let (states, a_id, b_id) = two_projects();
        assert_eq!(
            find_pane_of_pty(&states, 1),
            Some(("p-a".to_string(), a_id))
        );
        assert_eq!(
            find_pane_of_pty(&states, 2),
            Some(("p-b".to_string(), b_id))
        );
        assert_eq!(find_pane_of_pty(&states, 99), None);
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

    /// 回滚行数的四条钳制分支(`resolveScrollback` 逐条对照)。
    #[test]
    fn 回滚行数钳制的四个分支() {
        // 0 是合法值(等于不留历史),**不能**被当成「没设」回落默认
        assert_eq!(resolve_scrollback(0.0), 0);
        assert_eq!(resolve_scrollback(-1.0), DEFAULT_SCROLLBACK);
        assert_eq!(resolve_scrollback(999_999.0), MAX_SCROLLBACK);
        assert_eq!(resolve_scrollback(f64::NAN), DEFAULT_SCROLLBACK);
        assert_eq!(resolve_scrollback(f64::INFINITY), DEFAULT_SCROLLBACK);
        // 小数四舍五入
        assert_eq!(resolve_scrollback(1234.6), 1235);
        assert_eq!(resolve_scrollback(MAX_SCROLLBACK as f64), MAX_SCROLLBACK);
    }

    /// 终端字族:首项当主字体,其余进回退,并**自动补 CJK 与 emoji**。
    #[test]
    fn 终端字族自动补_cjk_回退() {
        let style = terminal_style_from(
            15.0,
            Some("'JetBrainsMono Nerd Font', 'Cascadia Code', monospace"),
        );
        assert_eq!(style.font_size, gpui::px(15.0));
        assert_eq!(style.font_family.as_ref(), "JetBrainsMono Nerd Font");
        let fallbacks: Vec<String> = style
            .font_fallbacks
            .iter()
            .map(|f| f.to_string())
            .collect();
        assert_eq!(fallbacks[0], "Cascadia Code");
        // 通用族名 `monospace` 被丢掉(gpui 认不出来)
        assert!(!fallbacks.iter().any(|f| f == "monospace"));
        for cjk in CJK_FALLBACK_FONTS {
            assert!(fallbacks.iter().any(|f| f == cjk), "缺 CJK 回退 {cjk}");
        }
        assert!(fallbacks.iter().any(|f| f == EMOJI_FALLBACK));
    }

    /// 字族为空 / 只有通用族名时整段回落默认样式(只改字号)。
    #[test]
    fn 终端字族为空时回落默认() {
        let default = TerminalStyle::default();
        for family in [None, Some(""), Some("   "), Some("monospace, serif")] {
            let style = terminal_style_from(14.0, family);
            assert_eq!(style.font_family, default.font_family, "{family:?}");
            assert_eq!(style.font_fallbacks, default.font_fallbacks, "{family:?}");
        }
    }

    /// 重复声明 CJK 字体时不该在回退串里出现两次。
    #[test]
    fn 终端字族回退不重复() {
        let style = terminal_style_from(14.0, Some("Consolas, 'Microsoft YaHei'"));
        let yahei = style
            .font_fallbacks
            .iter()
            .filter(|f| f.as_ref() == "Microsoft YaHei")
            .count();
        assert_eq!(yahei, 1);
    }

    // ---- 会话分支自记账 ----

    fn pending(agent: &str, parent: &str) -> PendingFork {
        PendingFork {
            agent: agent.to_string(),
            parent_session_id: parent.to_string(),
        }
    }

    fn identity(agent: Option<&str>, id: &str) -> AiSessionRef {
        AiSessionRef {
            agent: agent.map(str::to_string),
            session_id: id.to_string(),
            cwd: None,
        }
    }

    /// 正常流转:登记 claude 的 fork,新身份到手 → 落一条 child→parent 边。
    #[test]
    fn fork_登记遇上新身份落边() {
        let edge = resolve_fork_edge(&pending("claude", "parent-1"), &identity(Some("claude"), "child-1"))
            .expect("该落边");
        assert_eq!(edge.agent, "claude");
        assert_eq!(edge.session_id, "child-1");
        assert_eq!(edge.parent_session_id, "parent-1");
        assert_eq!(edge.fork_point_uuid, None, "自记账拿不到分叉点 uuid");

        // hook 上报 `claude-code`,登记时已归一化成小写;两边都归一化后才比得上
        assert!(
            resolve_fork_edge(&pending("claude-code", "p"), &identity(Some("Claude-Code"), "c"))
                .is_some(),
            "大小写不该拦下自己人"
        );
        // agent 缺省按 claude
        assert!(resolve_fork_edge(&pending("claude", "p"), &identity(None, "c")).is_some());
    }

    /// 三条否决:agent 不符 / 身份为空 / 新 id 等于父。
    #[test]
    fn fork_登记的三条否决() {
        // fork 失败后用户在同一个 pane 里起了别家 —— 只作废不记边
        assert!(
            resolve_fork_edge(&pending("claude", "p"), &identity(Some("codex"), "c")).is_none(),
            "agent 不符"
        );
        assert!(
            resolve_fork_edge(&pending("claude", "p"), &identity(Some("claude"), "")).is_none(),
            "身份还没成形"
        );
        // claude 的 --resume 幂等上报同一个 id:没真分出去,不该记一条自环
        assert!(
            resolve_fork_edge(&pending("claude", "same"), &identity(Some("claude"), "same"))
                .is_none(),
            "自指边"
        );
    }

    /// 「先记为准」:同一个 child 已有边就不覆盖(同一个孩子不可能有两个父)。
    #[test]
    fn 自记账表按_child_去重() {
        let mut table = Vec::new();
        let edge = |child: &str, parent: &str| mt_config::SavedLineageEdge {
            agent: "claude".into(),
            session_id: child.into(),
            parent_session_id: parent.into(),
            fork_point_uuid: None,
        };
        assert!(push_lineage_edge(&mut table, edge("c", "p1")));
        assert!(!push_lineage_edge(&mut table, edge("c", "p2")), "不覆盖");
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].parent_session_id, "p1", "先记的那条留下");
        // 别的 child 照常进表
        assert!(push_lineage_edge(&mut table, edge("c2", "p2")));
        assert_eq!(table.len(), 2);
    }

    /// **落盘格式与 Tauri 版一字不差**(`src-tauri/src/config.rs::SavedLineageEdge`
    /// 与 `src/types.ts::LineageEdge` 同构):camelCase 键、`forkPointUuid` 为空时
    /// **整个键省略**。两版共用同一个 `config.json`,多一个 `"forkPointUuid":null`
    /// 就是脏文件;少一个 `parentSessionId` 就是整条边读不回来。
    #[test]
    fn 自记账边磁盘格式与_tauri_版互读() {
        let edge = mt_config::SavedLineageEdge {
            agent: "claude".into(),
            session_id: "child-1".into(),
            parent_session_id: "parent-1".into(),
            fork_point_uuid: None,
        };
        assert_eq!(
            serde_json::to_string(&edge).unwrap(),
            r#"{"agent":"claude","sessionId":"child-1","parentSessionId":"parent-1"}"#,
            "自记账写出去的形状 = TS 侧 consumePendingFork 写的那三个键"
        );

        // 带分叉点 uuid 的形态(磁盘扫描补出来的边回写时可能带)
        let with_uuid = mt_config::SavedLineageEdge {
            fork_point_uuid: Some("m1".into()),
            ..edge
        };
        assert_eq!(
            serde_json::to_string(&with_uuid).unwrap(),
            r#"{"agent":"claude","sessionId":"child-1","parentSessionId":"parent-1","forkPointUuid":"m1"}"#
        );

        // 反向:Tauri 版写的两种形状都读得回来
        let parsed: mt_config::SavedLineageEdge = serde_json::from_str(
            r#"{"agent":"codex","sessionId":"c","parentSessionId":"p"}"#,
        )
        .unwrap();
        assert_eq!(parsed.agent, "codex");
        assert_eq!(parsed.session_id, "c");
        assert_eq!(parsed.parent_session_id, "p");
        assert_eq!(parsed.fork_point_uuid, None, "缺字段按 None,不许炸");
    }

    /// 自记账边喂给 mt-ai 的转换是逐字段直传(`session_panel` / `branch_family`
    /// 两处各写一遍,漂了就会出现「传过去的父 id 是空的」)。
    #[test]
    fn 自记账边转成_mt_ai_形态逐字段直传() {
        let saved = mt_config::SavedLineageEdge {
            agent: "claude".into(),
            session_id: "c".into(),
            parent_session_id: "p".into(),
            fork_point_uuid: Some("m1".into()),
        };
        let bookkept = mt_ai::sessions::BookkeptLineageEdge {
            agent: saved.agent.clone(),
            session_id: saved.session_id.clone(),
            parent_session_id: saved.parent_session_id.clone(),
            fork_point_uuid: saved.fork_point_uuid.clone(),
        };
        assert_eq!(bookkept.agent, "claude");
        assert_eq!(bookkept.session_id, "c");
        assert_eq!(bookkept.parent_session_id, "p");
        assert_eq!(bookkept.fork_point_uuid.as_deref(), Some("m1"));
    }

    /// 最大化开关的三态:换人 / 同一个再来一次收回 / 显式传 None 收回。
    #[test]
    fn 最大化开关三态() {
        assert_eq!(next_maximized(None, Some("p1")).as_deref(), Some("p1"));
        assert_eq!(next_maximized(Some("p1"), Some("p1")), None, "再点一次收回");
        assert_eq!(
            next_maximized(Some("p1"), Some("p2")).as_deref(),
            Some("p2"),
            "换一个组直接换过去,不需要先还原"
        );
        assert_eq!(next_maximized(Some("p1"), None), None, "显式还原");
        assert_eq!(next_maximized(None, None), None);
    }
}
