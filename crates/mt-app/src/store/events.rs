//! `AppStore` 的类型化变更事件。
//!
//! # 为什么要有它
//!
//! store 的每一处变化都以 `cx.notify()` 收尾,而 `cx.observe(&store)` 分不清是哪一种
//! 变化。AI 在跑时 OSC 标题每个 pane 约 4Hz 改一次
//! ([`AppStore::set_pane_osc_title_by_pty`]),每一次都把托盘同步、中转去抖、
//! 文档页签校验、Git / 会话面板比对……全叫醒一遍 —— 它们没有一个读 OSC 标题。
//! 事件让这些「要做事」的观察者只在自己读的数据变了时才动。
//!
//! # 两条通道并存(渐进式)
//!
//! - `cx.notify()` **除静默事件外一处没删**:render 里直接读 store 的视图照旧靠它重画;
//! - [`StoreEvent`] 给**非渲染**的观察者:同步托盘、推中转、跑校验、重拉面板数据。
//!
//! store 内部改完数据一律走 [`StoreChanged::changed`](先 emit 再 notify)。单测
//! `store里没有裸notify` 钉死「每一次 notify 都带着事件」—— 新写的 setter 忘了发
//! 事件会在测试里红,而不是等到真机上「该刷的没刷」。
//!
//! 反过来不是每个事件都带 notify:[`StoreEvent::is_silent`] 白名单里的热路径事件
//! **只 emit 不 notify**。gpui-fast 下 render 读过 store 的视图,store 每 notify 一次就
//! 失效重建一次,订阅里按 `touches_*` 过滤挡不住(见 `workspace/mod.rs::cached_panel`
//! 注释第 1 条);静默事件只叫醒在订阅里自己 notify 的那几个读者。
//!
//! # 数据 → 事件(订阅之前先查这张表)
//!
//! | 数据 | 会改动它的事件 |
//! |------|----------------|
//! | `active_project_id` | [`ActiveProjectChanged`] |
//! | `config.projects` 的增删与字段(名字/路径/描述/类型/环境变量/关联 SSH/父项目) | [`ProjectsChanged`] |
//! | `config.project_tree`(分组的建删改名折叠、项目挪位) | [`ProjectTreeChanged`] |
//! | 面板与分屏树、pane 增删、PTY 绑定与回收、活动面板 / tab、最大化、面板名 | [`LayoutChanged`] |
//! | **全部项目的 pane 集合** | [`LayoutChanged`] + [`ProjectsChanged`](删项目连 pane 一起没) |
//! | `focused_pane_id` | [`FocusedPaneChanged`] |
//! | pane 的 `status` / `attention` / 检测到的 agent | [`PaneStatusChanged`](补 PTY 时把起不来的 pane 标 error、重连回落 idle 这两处与 [`LayoutChanged`] 同发;PTY 退出 / 起不来落 error 也是它;AI 事件什么都没改时不发,见 `apply_ai_event` 的「同值不发」) |
//! | 项目行完成提示点(`needs_attention`) | [`PaneStatusChanged`](置位)、[`ActiveProjectChanged`](切过去即清) |
//! | PTY 退出登记(「已断开」遮罩) | [`PaneStatusChanged`](登记)、[`LayoutChanged`] / [`ProjectsChanged`](随 pane 回收) |
//! | pane 的 AI 会话身份 | [`PaneSessionChanged`] |
//! | pane 的续接标记(`resume_pending`) | [`PaneSessionChanged`](身份写入时清)、[`LayoutChanged`](补 PTY 写续接命令时清) |
//! | pane 的 OSC 标题(页签副段) | [`PaneTitleChanged`](只在**看得见的副段**变了才发;AI 在场时 spinner 写进标题这类看不见的变化只存不发) |
//! | pane 的自定义名 | [`PaneRenamed`] |
//! | AI 任务标记(⚑) | [`MarkersChanged`];随 pane 回收一并清掉时是 [`LayoutChanged`] / [`ProjectsChanged`] |
//! | 完成账本(未读 / 未看 / 完成序) | [`PaneStatusChanged`]、[`LayoutChanged`] / [`ProjectsChanged`](剔除已关 pane)、[`WindowFocusChanged`](聚焦即已读,顺带焦点 pane 算看过)、[`FocusedPaneChanged`](焦点给到谁谁就算看过,页签绿点熄灭;只有渲染读它)、[`DoneChanged`](手动清) |
//! | 主窗口聚焦 | [`WindowFocusChanged`] |
//! | 目录技术栈缓存 | [`DirKindsChanged`] |
//! | 文件树展开态 | [`ExpandedDirsChanged`] |
//! | 中转连接状态 | [`MobileRelayStatusChanged`] |
//! | 配置的其余各段 | [`Config`]`(`[`ConfigSection`]`)` |
//!
//! 一次操作可能连发几条(新建终端 = [`LayoutChanged`] + [`FocusedPaneChanged`]),
//! 观察者按「我读的数据」过滤即可,不必关心一次操作发了几条。
//!
//! # 不发事件的写入
//!
//! 与改造前「不 notify」的那批一一对应,没有任何观察者在它们变化时要做事:
//! 分隔条比例(`set_split_sizes`,拖动期间每帧一次)、三栏 / 中栏比例、抽屉宽度、
//! 窗口几何、用量面板偏好、续接反查写回的会话 cwd(`apply_resume_cwd`)、
//! fork 自记账的分支边(`consume_pending_fork`)、布局落盘缓存。
//!
//! # 时序(GPUI 口径)
//!
//! `cx.emit` 与 `cx.notify` 一样是**延后派发**的:订阅者在这次 `update` 结束、
//! effect 冲刷时才收到,读到的是这一轮改完之后的 store。订阅回调里读 store 安全
//! (它此刻不在租约里),但不要回头读 / 改**订阅者自己**(那才是重入)。
//! 另外 gpui-pre 0.3.5 的 `App::notify` 对正被窗口追踪的实体改走
//! `WindowInvalidator::invalidate_view`,在 draw 阶段里调用时**不派发观察者**;
//! emit 没有这条例外。
//!
//! [`AppStore::set_pane_osc_title_by_pty`]: super::AppStore::set_pane_osc_title_by_pty
//! [`ActiveProjectChanged`]: StoreEvent::ActiveProjectChanged
//! [`ProjectsChanged`]: StoreEvent::ProjectsChanged
//! [`ProjectTreeChanged`]: StoreEvent::ProjectTreeChanged
//! [`LayoutChanged`]: StoreEvent::LayoutChanged
//! [`FocusedPaneChanged`]: StoreEvent::FocusedPaneChanged
//! [`PaneStatusChanged`]: StoreEvent::PaneStatusChanged
//! [`PaneSessionChanged`]: StoreEvent::PaneSessionChanged
//! [`PaneTitleChanged`]: StoreEvent::PaneTitleChanged
//! [`PaneRenamed`]: StoreEvent::PaneRenamed
//! [`MarkersChanged`]: StoreEvent::MarkersChanged
//! [`DoneChanged`]: StoreEvent::DoneChanged
//! [`WindowFocusChanged`]: StoreEvent::WindowFocusChanged
//! [`DirKindsChanged`]: StoreEvent::DirKindsChanged
//! [`ExpandedDirsChanged`]: StoreEvent::ExpandedDirsChanged
//! [`MobileRelayStatusChanged`]: StoreEvent::MobileRelayStatusChanged
//! [`Config`]: StoreEvent::Config

use gpui::{Context, EventEmitter};

use super::AppStore;

/// store 的一次变化「属于哪一类」。见模块注释那张「数据 → 事件」表。
///
/// **不带载荷**(不带项目 / pane id)是盘点之后的结论:现有的非渲染观察者没有
/// 一个按项目或按 pane 取舍 —— 托盘、中转、文档校验要的都是「这一类数据变没变」。
/// 带上 id 只会让 OSC 标题那条热路径每次多克隆两个 `String`,且无人读取。
/// 出现按项目过滤的观察者时再给对应变体加字段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreEvent {
    /// 活动项目换了(切项目 / 删掉了活动项目后回落)。
    ActiveProjectChanged,
    /// `config.projects` 增删或字段变了。删项目时它的 pane 与完成记录一并消失。
    ProjectsChanged,
    /// 项目树(分组 / 次序 / 折叠)变了。
    ProjectTreeChanged,
    /// 某项目的终端面板 / 分屏结构变了:pane 增删、PTY 起停与回收(连同它的标记与
    /// 退出登记)、活动面板或 tab、最大化、面板名、补 PTY 时清续接标记。pane 集合
    /// 变化时完成账本随之剔除已关的 pane。
    LayoutChanged,
    /// 键盘焦点 pane 换了。
    FocusedPaneChanged,
    /// 某 pane 的 AI 状态 / attention 黄灯 / 检测到的 agent 变了(含 PTY 退出落 error)。
    /// 完成账本与项目行的完成提示点随之更新。
    PaneStatusChanged,
    /// 某 pane 通过右键「SSH 连接」的已连接状态变了(与 AI 状态完全正交)。
    /// 只唤醒展示它的页签栏(`TerminalArea`)、项目列表(`ProjectList`)与顶栏胶囊
    /// (`TitleBar`),不触碰托盘、中转或全局 AI 徽标。
    PaneSshStatusChanged,
    /// 某 pane 的 AI 会话身份写入了(hook 上报 / 续接 / 跳转;手动写入时连带清续接标记)。
    PaneSessionChanged,
    /// 某 pane 的 OSC 窗口标题变了 —— **热路径**(cmd 每条命令都改一次标题,AI 工作时约
    /// 4Hz/pane)。**静默事件**:只 emit 不 notify,见 [`Self::is_silent`]。
    PaneTitleChanged,
    /// 某 pane 的自定义名变了(F2 / 右键 / 移动端改名)。
    PaneRenamed,
    /// 某 pane 的 AI 任务标记(⚑)变了。
    MarkersChanged,
    /// 完成账本被手动清空(`clear_unread_done`)。其余改账本的路径见模块注释。
    DoneChanged,
    /// 主窗口聚焦状态变了。聚焦时连带把「未读完成」清空。
    WindowFocusChanged,
    /// 目录技术栈探测缓存写入 / 失效。
    DirKindsChanged,
    /// 文件树的目录展开态变了。
    ExpandedDirsChanged,
    /// 中转连接状态变了。
    MobileRelayStatusChanged,
    /// 配置的某一段变了。
    Config(ConfigSection),
}

/// [`StoreEvent::Config`] 的分段。按「谁会因此要做事」切,而不是按字段表逐个列。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigSection {
    /// 主题(亮暗 / 皮肤 / 终端跟随主题)与界面字号、字族。
    Appearance,
    /// 终端渲染参数:字号 / 字族 / 连体字 / 回滚行数 / 拖选停留时长。
    Terminal,
    /// 界面语言。
    Locale,
    /// shell 列表与默认 shell。
    Shells,
    /// SSH 连接表与分组。
    SshConnections,
    /// 中转地址 / 桌面密钥 / AI 启动器。
    MobileRelay,
    /// 命令库。
    CommandLibrary,
    /// 界面视图偏好:会话列表视图、Git 更改视图、中栏显隐、终端列表竖条显隐。
    View,
    /// 通用配置补丁(`patch_config`,设置页那一大片开关;托盘开关与上限也在这里)。
    Settings,
}

impl EventEmitter<StoreEvent> for AppStore {}

/// store 内部「改完了」的唯一收口:先发类型化事件,再照旧 notify(静默事件不 notify,
/// 见 [`StoreEvent::is_silent`])。
///
/// 做成 `Context` 上的方法而不是 `AppStore` 的方法:调用点常常还拿着
/// `self.project_states` 的可变借用,只借 `cx` 就不会和它打架。
pub(super) trait StoreChanged {
    fn changed(&mut self, event: StoreEvent);
}

impl StoreChanged for Context<'_, AppStore> {
    fn changed(&mut self, event: StoreEvent) {
        self.emit(event);
        if !event.is_silent() {
            self.notify();
        }
    }
}

// ─── 各观察者的过滤判定 ─────────────────────────────────────────
//
// 判定集中在这里而不是散在各观察者里:「观察者 → 它读的数据 → 它订阅的事件」
// 这张对照表只有一份,改数据来源时一眼能看到谁要跟着改(单测逐条钉死)。
// 每条的依赖清单对应着那个观察者真正读 store 的代码,漏一条就是「该刷的没刷」。

impl StoreEvent {
    /// **静默事件**:[`StoreChanged::changed`] 对它只 emit、不 notify。
    ///
    /// notify 的代价在 gpui-fast 下是「render 读过 store 的视图全部重建」—— 项目列表、
    /// 终端列表竖条、标题栏、工作区外壳、文件树……一次 cmd 改标题就是六到九个视图。
    /// 热路径上只有少数视图真读的数据改成只 emit:真读它的视图在订阅里按 `touches_*`
    /// 自己 notify,其余视图不再被叫醒。
    ///
    /// 进白名单的三条前提(单测 `静默事件的渲染读者都自己notify` 钉死前两条):
    /// 1. render 里读这份数据的视图全都订阅了 store 事件、在对应 `touches_*` 放行时对
    ///    自己 `cx.notify()` —— 漏一个就是那块界面不刷新;
    /// 2. `touches_workspace` 不含它。根视图一旦被 notify,其下读过 store 的非 cached 视图
    ///    会因 store 在这一帧被 update 过照样重建(gpui-fast 的 `inside_notified`,见
    ///    `fast/dependencies.rs` 的 `dependencies_changed`),静默就白做了;
    /// 3. 没有观察者靠 notify 取它(`cx.observe(&store)`):眼下全仓没有 observe store 的,
    ///    非渲染观察者一律走事件。
    ///
    /// [`Self::PaneTitleChanged`] 的渲染读者:终端区的页签栏与折叠标题条、tab 悬停卡
    /// (`TerminalArea`,`touches_terminal_area`)、快速切换弹窗(`ProjectSwitcher`,
    /// `touches_project_switcher`)。项目行悬停卡上各格的标签也带 OSC 副段,但它不靠
    /// 事件:卡开着时项目列表按 500ms 节拍自己重画(`ProjectList::keep_preview_alive`,
    /// 缩略图本来就是这个新鲜度)。其余取标题的地方(worktree 占用清单、远程通知正文)
    /// 是当场取值,不在渲染路径上。
    pub fn is_silent(self) -> bool {
        matches!(self, Self::PaneTitleChanged)
    }

    /// 活动项目(id,或它的路径 / 远程与否)可能变了。
    ///
    /// Git 面板与会话面板据此比对项目路径、决定重拉;全局搜索据此作废旧结果。
    /// 三者读的都是 `active_project()` = `active_project_id` 在 `config.projects`
    /// 里查到的那一条,所以是这两类事件。
    pub fn touches_active_project(self) -> bool {
        matches!(self, Self::ActiveProjectChanged | Self::ProjectsChanged)
    }

    /// 托盘快照(`Workspace::sync_tray` → `tray::build_snapshot`)读的数据可能变了。
    ///
    /// 依赖:`trayStatusEnabled` / `trayMaxProjects`(设置页走 `patch_config`)、窗口
    /// 聚焦、全部 pane 的状态与黄灯、pane 集合、项目名与次序、未读完成账本、菜单
    /// 文案的语言。**不含** OSC 标题 / 自定义名 / 焦点 pane / 主题字号。
    pub fn touches_tray(self) -> bool {
        matches!(
            self,
            Self::PaneStatusChanged
                | Self::LayoutChanged
                | Self::ProjectsChanged
                | Self::DoneChanged
                | Self::WindowFocusChanged
                | Self::Config(ConfigSection::Settings | ConfigSection::Locale)
        )
    }

    /// 移动端结构同步(`RelayBridge::sync_now` 与 `refresh_mirror`)读的数据可能变了。
    ///
    /// 依赖:项目的名字 / 路径 / 远程连接、项目树次序与分组名链、pane 集合与各自的
    /// PTY 编号、pane 状态与黄灯、pane 会话身份(历史列表「运行中」与 resume 精确镜像)、
    /// pane 标题(`customTitle ?? shellName`,**不是** OSC 标题)、AI 启动器。
    pub fn touches_relay_sync(self) -> bool {
        matches!(
            self,
            Self::ProjectsChanged
                | Self::ProjectTreeChanged
                | Self::LayoutChanged
                | Self::PaneStatusChanged
                | Self::PaneSessionChanged
                | Self::PaneRenamed
                | Self::Config(ConfigSection::MobileRelay)
        )
    }

    /// 文档页签的来源身份可能失效了(`WorkbenchArea` 的 retain + `validate_remote_source`)。
    ///
    /// 依赖:项目还在不在、项目根路径、项目引用的 SSH 连接及其指纹。
    pub fn touches_document_sources(self) -> bool {
        matches!(
            self,
            Self::ProjectsChanged | Self::Config(ConfigSection::SshConnections)
        )
    }

    /// 项目列表的后台探测(worktree 徽章 / 技术栈补探 / 回到窗口时的失效清理)
    /// 要不要重新过一遍闸。
    ///
    /// 依赖:本地项目的路径集合、技术栈缓存里「还没探过」的那些、窗口聚焦的上升沿。
    pub fn touches_project_probes(self) -> bool {
        matches!(
            self,
            Self::ProjectsChanged | Self::DirKindsChanged | Self::WindowFocusChanged
        )
    }

    /// 「添加远程项目」弹窗选中的连接要不要重定(`ssh_conn::reselect_connection`)。
    ///
    /// 依赖:连接表本身 —— 用户从弹窗里跳去「SSH 连接」面板新建一条,回来要选中它。
    pub fn touches_ssh_connections(self) -> bool {
        matches!(self, Self::Config(ConfigSection::SshConnections))
    }

    // ─── 核心渲染视图的细粒度过滤判定 ────────────────────────────

    /// 主窗口根视图(`Workspace`)渲染依赖的状态是否可能变了。
    ///
    /// 依赖:三栏与抽屉几何、中栏与终端列表竖条显隐、未读完成计数、全局 AI 状态灯、
    /// 活动项目(提交详情栏归属)、背景图/主题/语言/更新提示与动画开关。
    /// **不含** OSC 标题 / 焦点 pane / 改名 / 会话绑定 / 标记 / 技术栈 / 展开目录 / 分组树。
    pub fn touches_workspace(self) -> bool {
        matches!(
            self,
            Self::ActiveProjectChanged
                | Self::ProjectsChanged
                | Self::LayoutChanged
                | Self::PaneStatusChanged
                | Self::DoneChanged
                | Self::WindowFocusChanged
                | Self::Config(
                    ConfigSection::Appearance
                        | ConfigSection::Locale
                        | ConfigSection::View
                        | ConfigSection::Settings
                )
        )
    }

    /// 右栏工作区外壳(`WorkbenchArea`)渲染依赖的状态是否可能变了。
    ///
    /// 依赖:活动项目 id、项目列表与 SSH 连接来源、外观/语言与设置。
    /// **不含** 终端/AI 全部高频事件与文件树/项目树状态。
    pub fn touches_workbench_area(self) -> bool {
        matches!(
            self,
            Self::ActiveProjectChanged
                | Self::ProjectsChanged
                | Self::Config(
                    ConfigSection::Appearance
                        | ConfigSection::Locale
                        | ConfigSection::SshConnections
                        | ConfigSection::Settings
                )
        )
    }

    /// 终端分屏区(`TerminalArea`)渲染依赖的状态是否可能变了。
    ///
    /// 依赖:活动项目与分屏树、最大化、焦点 pane、状态灯/退出/未看完成、会话徽章、
    /// tab 标题(OSC + 改名)、AI 标记数、终端/外观/语言/shell/SSH/命令库/设置。
    /// **不含** 项目分组树、技术栈缓存、文件树展开态、中转状态、视图偏好。
    pub fn touches_terminal_area(self) -> bool {
        matches!(
            self,
            Self::ActiveProjectChanged
                | Self::ProjectsChanged
                | Self::LayoutChanged
                | Self::FocusedPaneChanged
                | Self::PaneStatusChanged
                | Self::PaneSshStatusChanged
                | Self::PaneSessionChanged
                | Self::PaneTitleChanged
                | Self::PaneRenamed
                | Self::MarkersChanged
                | Self::DoneChanged
                | Self::WindowFocusChanged
                | Self::Config(
                    ConfigSection::Appearance
                        | ConfigSection::Terminal
                        | ConfigSection::Locale
                        | ConfigSection::Shells
                        | ConfigSection::SshConnections
                        | ConfigSection::CommandLibrary
                        | ConfigSection::Settings
                )
        )
    }

    /// 终端面板列表竖条(`TerminalsPanel`)渲染依赖的状态是否可能变了。
    ///
    /// 依赖:活动项目、面板列表与面板名(`LayoutChanged`)、聚合状态灯、AI 会话指示、
    /// 外观/语言与自动续接设置。**不含** OSC 标题、pane 改名、焦点 pane、标记与文件树。
    pub fn touches_terminals_panel(self) -> bool {
        matches!(
            self,
            Self::ActiveProjectChanged
                | Self::ProjectsChanged
                | Self::LayoutChanged
                | Self::PaneStatusChanged
                | Self::PaneSessionChanged
                | Self::Config(
                    ConfigSection::Appearance | ConfigSection::Locale | ConfigSection::Settings
                )
        )
    }

    /// 顶栏(`TitleBar`)渲染依赖的状态是否可能变了。
    ///
    /// 依赖:`title_bar_snapshot()`(全项目 pane 状态/黄灯/完成序与项目名)、活动项目名、
    /// 窗口聚焦(清未读)、外观与语言。**不含** OSC 标题、焦点 pane、改名、会话绑定与文件树。
    pub fn touches_title_bar(self) -> bool {
        matches!(
            self,
            Self::ActiveProjectChanged
                | Self::ProjectsChanged
                | Self::LayoutChanged
                | Self::PaneStatusChanged
                | Self::PaneSshStatusChanged
                | Self::DoneChanged
                | Self::WindowFocusChanged
                | Self::Config(ConfigSection::Appearance | ConfigSection::Locale)
        )
    }

    /// 中栏上段项目列表(`ProjectList`)渲染与 `done_tags_stale` 跟踪依赖的状态是否可能变了。
    ///
    /// 依赖:活动项目、项目表与分组树、布局/状态/会话/pane 改名(悬停预览标签)、技术栈徽标、
    /// 中栏显隐(`View`)、远程连接徽章、预览终端样式、外观/语言/自动续接设置。
    /// **不含** OSC 标题(4Hz 热路径)、焦点 pane、AI 标记、文件树展开态、中转状态。
    pub fn touches_project_list(self) -> bool {
        matches!(
            self,
            Self::ActiveProjectChanged
                | Self::ProjectsChanged
                | Self::ProjectTreeChanged
                | Self::LayoutChanged
                | Self::PaneStatusChanged
                | Self::PaneSshStatusChanged
                | Self::PaneSessionChanged
                | Self::PaneRenamed
                | Self::DirKindsChanged
                | Self::Config(
                    ConfigSection::Appearance
                        | ConfigSection::Terminal
                        | ConfigSection::Locale
                        | ConfigSection::SshConnections
                        | ConfigSection::View
                        | ConfigSection::Settings
                )
        )
    }

    /// 中栏下段文件树(`FileTree`)渲染与 `sync_project` 依赖的状态是否可能变了。
    ///
    /// 与 `file_tree::store_render_signature` 五项指纹逐一对齐:活动项目与后端连接、
    /// 外部编辑器配置与远程 Git 授权(`Settings`)、展开目录(`ExpandedDirsChanged`)、
    /// 目录技术栈(`DirKindsChanged`)、主题/UI 字体/语言。
    /// **不含** 终端/AI 全部高频事件与项目分组树。
    pub fn touches_file_tree(self) -> bool {
        matches!(
            self,
            Self::ActiveProjectChanged
                | Self::ProjectsChanged
                | Self::DirKindsChanged
                | Self::ExpandedDirsChanged
                | Self::Config(
                    ConfigSection::Appearance
                        | ConfigSection::Locale
                        | ConfigSection::SshConnections
                        | ConfigSection::Settings
                )
        )
    }

    /// 右侧会话抽屉(`SessionPanel`)渲染依赖的状态是否可能变了。
    ///
    /// 依赖:活动项目、项目名、远程连接名、树模式在跑会话实时状态(`find_live_session_pane`)、
    /// 视图切换(`View`)与外观/语言。**不含** OSC 标题、改名、焦点、标记与文件树。
    pub fn touches_session_panel(self) -> bool {
        matches!(
            self,
            Self::ActiveProjectChanged
                | Self::ProjectsChanged
                | Self::LayoutChanged
                | Self::PaneStatusChanged
                | Self::PaneSessionChanged
                | Self::Config(
                    ConfigSection::Appearance
                        | ConfigSection::Locale
                        | ConfigSection::SshConnections
                        | ConfigSection::View
                )
        )
    }

    /// 右侧 Git 抽屉(`GitPanel`)渲染依赖的状态是否可能变了。
    ///
    /// 依赖:活动项目、远程连接与远程 Git 授权(`Settings`)、更改区列表/树视图(`View`)、
    /// 外观与语言。**不含** 终端/AI 全部高频事件、项目树与文件树展开态。
    pub fn touches_git_panel(self) -> bool {
        matches!(
            self,
            Self::ActiveProjectChanged
                | Self::ProjectsChanged
                | Self::Config(
                    ConfigSection::Appearance
                        | ConfigSection::Locale
                        | ConfigSection::SshConnections
                        | ConfigSection::View
                        | ConfigSection::Settings
                )
        )
    }

    /// 快速项目/终端切换弹窗(`ProjectSwitcher`)渲染依赖的状态是否可能变了。
    ///
    /// 依赖:项目与分组树、活动项目、面板与 pane 布局/焦点/状态/会话/标题/改名、
    /// 外观/语言/远程连接名/自动续接设置。
    pub fn touches_project_switcher(self) -> bool {
        matches!(
            self,
            Self::ActiveProjectChanged
                | Self::ProjectsChanged
                | Self::ProjectTreeChanged
                | Self::LayoutChanged
                | Self::FocusedPaneChanged
                | Self::PaneStatusChanged
                | Self::PaneSessionChanged
                | Self::PaneTitleChanged
                | Self::PaneRenamed
                | Self::Config(
                    ConfigSection::Appearance
                        | ConfigSection::Locale
                        | ConfigSection::SshConnections
                        | ConfigSection::Settings
                )
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个变体一个样本。**穷举 match 不带通配**:加了新变体这里编不过,
    /// 逼着把它在下面几条判定里过一遍。
    fn all_events() -> Vec<StoreEvent> {
        let sections = [
            ConfigSection::Appearance,
            ConfigSection::Terminal,
            ConfigSection::Locale,
            ConfigSection::Shells,
            ConfigSection::SshConnections,
            ConfigSection::MobileRelay,
            ConfigSection::CommandLibrary,
            ConfigSection::View,
            ConfigSection::Settings,
        ];
        for s in sections {
            match s {
                ConfigSection::Appearance
                | ConfigSection::Terminal
                | ConfigSection::Locale
                | ConfigSection::Shells
                | ConfigSection::SshConnections
                | ConfigSection::MobileRelay
                | ConfigSection::CommandLibrary
                | ConfigSection::View
                | ConfigSection::Settings => {}
            }
        }
        let mut out = vec![
            StoreEvent::ActiveProjectChanged,
            StoreEvent::ProjectsChanged,
            StoreEvent::ProjectTreeChanged,
            StoreEvent::LayoutChanged,
            StoreEvent::FocusedPaneChanged,
            StoreEvent::PaneStatusChanged,
            StoreEvent::PaneSshStatusChanged,
            StoreEvent::PaneSessionChanged,
            StoreEvent::PaneTitleChanged,
            StoreEvent::PaneRenamed,
            StoreEvent::MarkersChanged,
            StoreEvent::DoneChanged,
            StoreEvent::WindowFocusChanged,
            StoreEvent::DirKindsChanged,
            StoreEvent::ExpandedDirsChanged,
            StoreEvent::MobileRelayStatusChanged,
        ];
        for e in &out {
            match e {
                StoreEvent::ActiveProjectChanged
                | StoreEvent::ProjectsChanged
                | StoreEvent::ProjectTreeChanged
                | StoreEvent::LayoutChanged
                | StoreEvent::FocusedPaneChanged
                | StoreEvent::PaneStatusChanged
                | StoreEvent::PaneSshStatusChanged
                | StoreEvent::PaneSessionChanged
                | StoreEvent::PaneTitleChanged
                | StoreEvent::PaneRenamed
                | StoreEvent::MarkersChanged
                | StoreEvent::DoneChanged
                | StoreEvent::WindowFocusChanged
                | StoreEvent::DirKindsChanged
                | StoreEvent::ExpandedDirsChanged
                | StoreEvent::MobileRelayStatusChanged
                | StoreEvent::Config(_) => {}
            }
        }
        out.extend(sections.map(StoreEvent::Config));
        out
    }

    /// 某条判定放行的全部事件(按 `all_events` 的次序)。
    fn accepted(pred: impl Fn(StoreEvent) -> bool) -> Vec<StoreEvent> {
        all_events().into_iter().filter(|e| pred(*e)).collect()
    }

    /// 改造的出发点:AI 工作时的 OSC 标题热路径**一个非渲染观察者都不叫醒**。
    /// 焦点 / 标记 / 主题字号同理 —— 它们只有 render 在读。
    #[test]
    fn 标题等纯渲染数据不唤醒任何非渲染观察者() {
        let render_only = [
            StoreEvent::PaneSshStatusChanged,
            StoreEvent::PaneTitleChanged,
            StoreEvent::FocusedPaneChanged,
            StoreEvent::MarkersChanged,
            StoreEvent::ExpandedDirsChanged,
            StoreEvent::MobileRelayStatusChanged,
            StoreEvent::Config(ConfigSection::Appearance),
            StoreEvent::Config(ConfigSection::Terminal),
            StoreEvent::Config(ConfigSection::Shells),
            StoreEvent::Config(ConfigSection::CommandLibrary),
            StoreEvent::Config(ConfigSection::View),
        ];
        for e in render_only {
            assert!(!e.touches_active_project(), "{e:?}");
            assert!(!e.touches_tray(), "{e:?}");
            assert!(!e.touches_relay_sync(), "{e:?}");
            assert!(!e.touches_document_sources(), "{e:?}");
            assert!(!e.touches_project_probes(), "{e:?}");
            assert!(!e.touches_ssh_connections(), "{e:?}");
        }
    }

    #[test]
    fn 托盘只认状态结构账本聚焦与托盘开关语言() {
        assert_eq!(
            accepted(StoreEvent::touches_tray),
            vec![
                StoreEvent::ProjectsChanged,
                StoreEvent::LayoutChanged,
                StoreEvent::PaneStatusChanged,
                StoreEvent::DoneChanged,
                StoreEvent::WindowFocusChanged,
                StoreEvent::Config(ConfigSection::Locale),
                StoreEvent::Config(ConfigSection::Settings),
            ]
        );
    }

    /// 中转推的 pane 标题是 `customTitle ?? shellName`:改名要推,OSC 标题不推。
    /// 项目树次序决定手机列表的顺序,分组折叠虽不下发也会走一遍(内容去重挡住)。
    #[test]
    fn 中转认改名不认osc标题() {
        assert_eq!(
            accepted(StoreEvent::touches_relay_sync),
            vec![
                StoreEvent::ProjectsChanged,
                StoreEvent::ProjectTreeChanged,
                StoreEvent::LayoutChanged,
                StoreEvent::PaneStatusChanged,
                StoreEvent::PaneSessionChanged,
                StoreEvent::PaneRenamed,
                StoreEvent::Config(ConfigSection::MobileRelay),
            ]
        );
        assert!(!StoreEvent::PaneTitleChanged.touches_relay_sync());
    }

    /// 文档页签校验:删项目 / 改 SSH 连接会让远程文档的来源失效;
    /// 纯 AI 状态与布局变化不该让每个页签都跑一遍校验。
    #[test]
    fn 文档校验只认项目与ssh连接() {
        assert_eq!(
            accepted(StoreEvent::touches_document_sources),
            vec![
                StoreEvent::ProjectsChanged,
                StoreEvent::Config(ConfigSection::SshConnections),
            ]
        );
    }

    #[test]
    fn 活动项目比对只认切项目与项目表变化() {
        assert_eq!(
            accepted(StoreEvent::touches_active_project),
            vec![
                StoreEvent::ActiveProjectChanged,
                StoreEvent::ProjectsChanged
            ]
        );
        // 分组折叠 / 挪位不改任何项目的路径
        assert!(!StoreEvent::ProjectTreeChanged.touches_active_project());
    }

    #[test]
    fn 项目探测认项目表技术栈缓存与窗口聚焦() {
        assert_eq!(
            accepted(StoreEvent::touches_project_probes),
            vec![
                StoreEvent::ProjectsChanged,
                StoreEvent::WindowFocusChanged,
                StoreEvent::DirKindsChanged,
            ]
        );
    }

    /// 添加远程项目重定选中只看连接表:AI 状态、布局这些热路径不该让它每次都比一遍。
    #[test]
    fn 添加远程项目只认ssh连接表() {
        assert_eq!(
            accepted(StoreEvent::touches_ssh_connections),
            vec![StoreEvent::Config(ConfigSection::SshConnections)]
        );
    }

    /// 高频 OSC 标题变化只应唤醒真正展示标题的终端区与切换弹窗,不惊动侧栏与抽屉。
    #[test]
    fn osc标题变化不唤醒无关核心视图() {
        let title = StoreEvent::PaneTitleChanged;
        assert!(title.touches_terminal_area());
        assert!(title.touches_project_switcher());
        assert!(!title.touches_workspace());
        assert!(!title.touches_workbench_area());
        assert!(!title.touches_terminals_panel());
        assert!(!title.touches_title_bar());
        assert!(!title.touches_project_list());
        assert!(!title.touches_file_tree());
        assert!(!title.touches_session_panel());
        assert!(!title.touches_git_panel());
    }

    /// 文件树展开态变化只应唤醒文件树本身。
    #[test]
    fn 展开目录变化只唤醒文件树() {
        let expanded = StoreEvent::ExpandedDirsChanged;
        assert!(expanded.touches_file_tree());
        assert!(!expanded.touches_workspace());
        assert!(!expanded.touches_workbench_area());
        assert!(!expanded.touches_terminal_area());
        assert!(!expanded.touches_terminals_panel());
        assert!(!expanded.touches_title_bar());
        assert!(!expanded.touches_project_list());
        assert!(!expanded.touches_session_panel());
        assert!(!expanded.touches_git_panel());
        assert!(!expanded.touches_project_switcher());
    }

    /// 逐项钉死其余核心视图的精确事件集合,防止高频热路径回流。
    #[test]
    fn 核心视图谓词过滤高频无关事件() {
        // 文件树与 Git 面板免疫全部终端/AI 高频状态
        for e in [
            StoreEvent::LayoutChanged,
            StoreEvent::FocusedPaneChanged,
            StoreEvent::PaneStatusChanged,
            StoreEvent::PaneSshStatusChanged,
            StoreEvent::PaneSessionChanged,
            StoreEvent::PaneTitleChanged,
            StoreEvent::PaneRenamed,
            StoreEvent::MarkersChanged,
            StoreEvent::DoneChanged,
            StoreEvent::WindowFocusChanged,
            StoreEvent::ProjectTreeChanged,
            StoreEvent::MobileRelayStatusChanged,
        ] {
            assert!(!e.touches_file_tree(), "FileTree 不应响应 {e:?}");
            assert!(!e.touches_git_panel(), "GitPanel 不应响应 {e:?}");
            assert!(!e.touches_workbench_area(), "WorkbenchArea 不应响应 {e:?}");
        }

        // TerminalsPanel / TitleBar / ProjectList / SessionPanel 不响应焦点与标记与中转
        for e in [
            StoreEvent::FocusedPaneChanged,
            StoreEvent::MarkersChanged,
            StoreEvent::MobileRelayStatusChanged,
        ] {
            assert!(!e.touches_terminals_panel(), "{e:?}");
            assert!(!e.touches_title_bar(), "{e:?}");
            assert!(!e.touches_project_list(), "{e:?}");
            assert!(!e.touches_session_panel(), "{e:?}");
            assert!(!e.touches_workspace(), "{e:?}");
        }

        // 必达护栏:关键视觉数据绝不漏订阅
        assert!(StoreEvent::PaneSshStatusChanged.touches_terminal_area());
        assert!(StoreEvent::PaneSshStatusChanged.touches_project_list());
        assert!(StoreEvent::PaneSshStatusChanged.touches_title_bar());
        assert!(!StoreEvent::PaneSshStatusChanged.touches_workspace());
        assert!(!StoreEvent::PaneSshStatusChanged.touches_terminals_panel());
        assert!(!StoreEvent::PaneSshStatusChanged.touches_project_switcher());
        assert!(!StoreEvent::PaneSshStatusChanged.touches_session_panel());
        assert!(!StoreEvent::PaneSshStatusChanged.touches_file_tree());
        assert!(!StoreEvent::PaneSshStatusChanged.touches_git_panel());
        assert!(!StoreEvent::PaneSshStatusChanged.touches_workbench_area());
        assert!(StoreEvent::Config(ConfigSection::View).touches_workspace());
        assert!(StoreEvent::Config(ConfigSection::View).touches_project_list());
        assert!(StoreEvent::Config(ConfigSection::View).touches_session_panel());
        assert!(StoreEvent::Config(ConfigSection::View).touches_git_panel());
        assert!(StoreEvent::DirKindsChanged.touches_project_list());
        assert!(StoreEvent::DirKindsChanged.touches_file_tree());
        assert!(StoreEvent::PaneStatusChanged.touches_session_panel());
        assert!(StoreEvent::PaneSessionChanged.touches_session_panel());
        assert!(StoreEvent::MarkersChanged.touches_terminal_area());
        assert!(StoreEvent::FocusedPaneChanged.touches_terminal_area());
        assert!(StoreEvent::DoneChanged.touches_title_bar());
        assert!(StoreEvent::DoneChanged.touches_workspace());
    }

    /// 静默白名单逐条钉死(前提见 [`StoreEvent::is_silent`]):加进来的事件,它的渲染读者
    /// 必须都在 `touches_*` 里自己 notify,且根视图不被它叫醒。加新的静默事件时在这里
    /// 补上它的读者清单 —— 漏一个读者,那块界面就停在旧值上。
    #[test]
    fn 静默事件的渲染读者都自己notify() {
        assert_eq!(
            accepted(StoreEvent::is_silent),
            vec![StoreEvent::PaneTitleChanged]
        );
        for e in accepted(StoreEvent::is_silent) {
            assert!(!e.touches_workspace(), "{e:?} 会叫醒根视图,静默白做");
        }
        // PaneTitleChanged 的读者:终端区(页签栏 / 折叠标题条 / tab 悬停卡)与快速切换弹窗
        let title = StoreEvent::PaneTitleChanged;
        assert!(title.touches_terminal_area());
        assert!(title.touches_project_switcher());
    }

    /// 收口 [`StoreChanged::changed`]:静默事件照样 emit、不 notify;其余事件 emit + notify。
    #[gpui::test]
    fn changed对静默事件只发不通知(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use std::cell::Cell;
        use std::rc::Rc;

        let store = cx.new(|cx| AppStore::git_test_store(mt_config::AppConfig::default(), cx));
        let notified = Rc::new(Cell::new(0usize));
        let emitted = Rc::new(Cell::new(0usize));
        let _subs = cx.update(|cx| {
            let (n, e) = (notified.clone(), emitted.clone());
            (
                cx.observe(&store, move |_, _| n.set(n.get() + 1)),
                cx.subscribe(&store, move |_, _: &StoreEvent, _| e.set(e.get() + 1)),
            )
        });
        for event in [StoreEvent::PaneTitleChanged, StoreEvent::PaneRenamed] {
            notified.set(0);
            emitted.set(0);
            cx.update(|cx| store.update(cx, |_, cx| cx.changed(event)));
            cx.run_until_parked();
            assert_eq!(emitted.get(), 1, "{event:?} 要 emit");
            assert_eq!(
                notified.get(),
                usize::from(!event.is_silent()),
                "{event:?} 的 notify 次数"
            );
        }
    }

    /// 「每一次 notify 都带着事件」的机械护栏:store 目录下除本文件外,
    /// 非注释行里不许出现裸 `cx.notify()` —— 一律走 [`StoreChanged::changed`]。
    ///
    /// 漏发事件的后果是订阅者该刷新的不刷新(比多刷一次严重得多),而这类遗漏
    /// 在代码评审里极难看出来;这里让它在 `cargo test` 里直接红。
    ///
    /// 只管这一个方向。反方向「每个事件都带 notify」**不成立**:静默事件只 emit
    /// (见 [`StoreEvent::is_silent`]),它的渲染读者靠订阅自己 notify —— 由
    /// `静默事件的渲染读者都自己notify` 与 `changed对静默事件只发不通知` 两条钉着。
    ///
    /// 运行时扫整个目录而不是手列文件:store 新增文件自动纳入,不会漏在护栏外。
    #[test]
    fn store里没有裸notify() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/store");
        let mut sources: Vec<(String, String)> = std::fs::read_dir(&dir)
            .expect("读 store 目录")
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
            .filter(|path| path.file_name().is_some_and(|name| name != "events.rs"))
            .map(|path| {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                let text = std::fs::read_to_string(&path).expect("读 store 源码");
                (name, text)
            })
            .collect();
        sources.sort();
        assert!(
            sources.len() >= 10,
            "store 目录下的源码没扫全:只找到 {} 个",
            sources.len()
        );
        for (name, text) in sources {
            for (no, line) in text.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                assert!(
                    !code.contains("cx.notify()"),
                    "{name}:{} 有裸 notify,改用 `cx.changed(StoreEvent::…)`:{line}",
                    no + 1
                );
            }
        }
    }
}
