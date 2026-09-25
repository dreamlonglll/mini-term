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
//! - `cx.notify()` **一处没删**:render 里直接读 store 的视图照旧靠它重画;
//! - [`StoreEvent`] 给**非渲染**的观察者:同步托盘、推中转、跑校验、重拉面板数据。
//!
//! store 内部改完数据一律走 [`StoreChanged::changed`](先 emit 再 notify)。单测
//! `store里没有裸notify` 钉死「每一次 notify 都带着事件」—— 新写的 setter 忘了发
//! 事件会在测试里红,而不是等到真机上「该刷的没刷」。
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
//! | pane 的 `status` / `attention` / 检测到的 agent | [`PaneStatusChanged`](补 PTY 时把起不来的 pane 标 error、重连回落 idle 这两处与 [`LayoutChanged`] 同发;PTY 退出 / 起不来落 error 也是它) |
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
    /// 某 pane 的 AI 会话身份写入了(hook 上报 / 续接 / 跳转;手动写入时连带清续接标记)。
    PaneSessionChanged,
    /// 某 pane 的 OSC 窗口标题变了 —— **热路径**(AI 工作时约 4Hz/pane)。
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

/// store 内部「改完了」的唯一收口:先发类型化事件,再照旧 notify。
///
/// 做成 `Context` 上的方法而不是 `AppStore` 的方法:调用点常常还拿着
/// `self.project_states` 的可变借用,只借 `cx` 就不会和它打架。
pub(super) trait StoreChanged {
    fn changed(&mut self, event: StoreEvent);
}

impl StoreChanged for Context<'_, AppStore> {
    fn changed(&mut self, event: StoreEvent) {
        self.emit(event);
        self.notify();
    }
}

// ─── 各观察者的过滤判定 ─────────────────────────────────────────
//
// 判定集中在这里而不是散在各观察者里:「观察者 → 它读的数据 → 它订阅的事件」
// 这张对照表只有一份,改数据来源时一眼能看到谁要跟着改(单测逐条钉死)。
// 每条的依赖清单对应着那个观察者真正读 store 的代码,漏一条就是「该刷的没刷」。

impl StoreEvent {
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
    /// PTY 编号、pane 状态与黄灯、pane 标题(`customTitle ?? shellName`,**不是**
    /// OSC 标题)、AI 启动器。
    pub fn touches_relay_sync(self) -> bool {
        matches!(
            self,
            Self::ProjectsChanged
                | Self::ProjectTreeChanged
                | Self::LayoutChanged
                | Self::PaneStatusChanged
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
    /// 焦点 / 标记 / 会话身份 / 主题字号同理 —— 它们只有 render 在读。
    #[test]
    fn 标题等纯渲染数据不唤醒任何非渲染观察者() {
        let render_only = [
            StoreEvent::PaneTitleChanged,
            StoreEvent::FocusedPaneChanged,
            StoreEvent::MarkersChanged,
            StoreEvent::PaneSessionChanged,
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

    /// 「每一次 notify 都带着事件」的机械护栏:store 目录下除本文件外,
    /// 非注释行里不许出现裸 `cx.notify()` —— 一律走 [`StoreChanged::changed`]。
    ///
    /// 漏发事件的后果是订阅者该刷新的不刷新(比多刷一次严重得多),而这类遗漏
    /// 在代码评审里极难看出来;这里让它在 `cargo test` 里直接红。
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
