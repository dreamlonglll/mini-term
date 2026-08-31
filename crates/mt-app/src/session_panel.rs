//! AI 历史面板(右侧抽屉)。对应 `src/components/SessionList.tsx`。
//!
//! ```text
//! 项目切换 ─→ refresh(force=false)
//!              ├─ background: mt_ai::sessions::get_ai_sessions(宿主,秒出)
//!              ├─ background: mt_ai::sessions::scan_session_lineage(分支边,树视图用)
//!              └─ background: mt_ai::sessions::get_wsl_ai_sessions(9P + 可能的 VM 冷启动)
//!                     ↓ 各自回主线程 setState,按时间戳降序混排
//! 右键一行 ─→ 查看 / 在当前终端恢复 / 新标签恢复 / 复制命令
//! 树模式点一行 ─→ 已在跑就跳过去,没在跑就新终端 resume
//! ```
//!
//! **三个慢函数必须丢后台**(看板技术债清单明示):`get_ai_session_content`、
//! `get_wsl_ai_sessions` 与 `scan_session_lineage` 原本靠 Tauri 命令层挪出主线程;
//! 现在是普通同步函数,WSL 冷启动秒级,落在 GPUI 主线程上就是整个窗口卡住。
//!
//! **惰性加载那道闸不许绕过**:`visible` / `stale` 双标记(收起时项目切换不去扫)
//! 是 GPUI 侧补的防线 —— 旧版收起时组件根本没挂载。分支边扫描同样挂在这道闸后面。
//!
//! # 来源三选一(唯一分流开关)
//!
//! [`crate::ssh_conn::session_source`] 是**唯一**判据(BB-b 接上):
//!
//! ```text
//! Remote(conn)  → 只走 remote_ssh::ai_sessions([后台]);宿主 / lineage / WSL
//!                 三路一条都不发 —— 本地 `get_ai_sessions` 对远程 POSIX 路径
//!                 会去本机 `~/.claude/projects` 找同名编码目录,命中的是
//!                 **另一台机器**上同路径的会话
//! BrokenRemote  → 空表 + 断链提示。**绝不退回 Local**(同上,会把本机会话
//!                 贴到远程项目上)
//! Local         → 宿主 + lineage + 可选 WSL,三路并发,照旧
//! ```
//!
//! # 与旧版的偏差
//!
//! - 会话正文查看是**面板内预览**而不是独立弹窗(形态不同,动作齐全);
//! - **断链时多一句提示**(原版是静默空列表:它的 `ssh_remote_ai_sessions`
//!   在后端把断链吞成空表,前端看不出区别)。空列表配「一条会话都没有」会让人
//!   以为远端真没会话,而实情是连接被删了 —— 记为改良。
//!
//! # 与 [`crate::branch_family`] 的分工
//!
//! pane 右键「查看会话分支」的悬停家族面板是**另一个组件**:它挂在菜单的自绘
//! 子菜单挂载点上、取数口是 pane 自己的 `ai_session`、只画那一支家族。两边共用
//! 的是纯逻辑([`crate::session_branch`])与节点点击行为([`jump_to_session`])
//! —— 点同一种节点必须有同一种行为。

use gpui::{
    AnyElement, App, Context, Entity, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Task,
    Window, div, prelude::FluentBuilder, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::text::{TextView, TextViewStyle};
use mt_ui::tooltip::Tooltip;
use mt_ai::sessions::{AiSession, AiSessionMessage, LineageEdge};
use mt_ui::icons::{AiVendor, BrandIcon, StatusDot, StatusKind};

use crate::i18n::{t, tr};
use crate::menu;
use crate::notify::ToastKind;
use crate::session_branch::{build_session_tree, flatten_session_tree, merge_lineage_edges};
use crate::store::AppStore;
use crate::toast;
use crate::tree::{AiSessionRef, PaneStatus};
use crate::ui;

/// 一页多少条(与旧版 `PAGE_SIZE` 同值)。
const PAGE_SIZE: usize = 20;

/// 会话正文一屏渲染多少条消息。
///
/// **不是观感取舍,是硬约束**:每条消息的正文是一个 [`TextView`],首帧那次
/// markdown 解析(含代码块高亮)是**同步**跑在主线程上的(组件只把后续更新
/// 丢后台),且每个 TextView 常驻一前一后两个 task。一个长会话上千条消息全铺
/// 出去就是开面板即卡窗口 —— 所以按页给,底下留「加载更多」。
const PREVIEW_PAGE_SIZE: usize = 40;

/// 该会话对应的 resume 命令;id 形态异常返回 `None`。
///
/// sessionId 会被原样拼进写进 PTY 的命令行,必须过白名单:字母数字与 `-_`
/// (Claude UUID、Codex rollout id 与 Grok UUIDv7 的实际形态)。两个来源
/// ——持久化布局与会话记录文件内容——都不是可信输入,空格/引号/管道/换行
/// 等一切 shell 元字符在此拦截(逐条对照 `src/utils/aiResume.ts`)。
pub fn build_resume_command(agent: &str, session_id: &str) -> Option<String> {
    if session_id.is_empty() || session_id.len() > 128 {
        return None;
    }
    if !session_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(match agent {
        "codex" => format!("codex resume {session_id}"),
        "grok" => format!("grok --resume {session_id}"),
        _ => format!("claude --resume {session_id}"),
    })
}

/// 点一个会话节点该发生什么。对应 `src/utils/sessionJump.ts::jumpToSession`。
///
/// ```text
/// live 存在   → 切项目 + 激活那个 pane
/// live 不存在 →
///   ├─ 会话来自 WSL / 远程 → 提示「无法在本机恢复」,不做任何事
///   ├─ agent 无 resume 能力 → 静默不做
///   └─ 否则 → 新开终端 + 写 resume 命令 + 回写会话身份
/// ```
///
/// **自由函数**而不是 [`SessionPanel`] 的方法:分支树视图(本面板)与
/// pane 右键的悬停家族面板([`crate::branch_family`])是两个组件、点同一种节点,
/// 行为必须逐字相同。返回的 `Task` 由调用方持有 —— 丢掉就等于取消。
pub(crate) fn jump_to_session(
    store: &Entity<AppStore>,
    session: AiSession,
    window: &mut Window,
    cx: &mut App,
) -> Task<()> {
    if let Some((project_id, pane_id, _)) = store.read(cx).find_live_session_pane(&session.id) {
        store.update(cx, |store, cx| {
            store.set_active_project(&project_id, cx);
            store.activate_pane(&project_id, &pane_id, window, cx);
        });
        crate::workbench_area::activate_terminal_page(window, cx);
        return Task::ready(());
    }

    // WSL / SSH 远程来源的会话记录在别的机器/发行版里,本地 resume 恢复不了。
    // 走 toast(原版 `pushNotification`,kind `wsl-info`)—— Q 批写这段时
    // mt-app 还没有 toast 体系,退而用通用提示框;Z 批的 `toast.rs` 到位后回换,
    // 与原版同形(不打断、右下角自动消失)。
    if session.wsl_distro.is_some() || session.ssh_connection_id.is_some() {
        let (project_id, project_name) = store
            .read(cx)
            .active_project()
            .map(|p| (p.id.clone(), p.name.clone()))
            .unwrap_or_default();
        toast::push_message(
            ToastKind::WslInfo,
            project_id,
            project_name,
            t("sessionList", "branchTree.remoteResumeUnsupported").to_string(),
            cx,
        );
        return Task::ready(());
    }

    let Some(command) = build_resume_command(&session.session_type, &session.id) else {
        // opencode / pi 之类没有 resume 能力的 agent:静默不做
        return Task::ready(());
    };
    let Some(project_id) = store.read(cx).active_project_id.clone() else {
        return Task::ready(());
    };
    let anchor = store.read(cx).active_pane_id(&project_id);
    // `claude --resume` 只认「启动目录」对应的会话桶:子目录里起的会话在项目根
    // 恢复会报 `No conversation found`,先反查记录的 cwd。codex 不按目录分桶;
    // grok 虽按 cwd 分桶,但列表只捞「解码目录名全等于项目根」的会话。
    // 反查是**同步磁盘遍历**,跳转路径上照样丢后台。
    let needs_cwd = session.session_type == "claude";
    let session_id = session.id.clone();
    let store = store.clone();
    window.spawn(cx, async move |cx| {
        let cwd = if needs_cwd {
            cx.background_executor()
                .spawn(async move { mt_ai::sessions::lookup_ai_session_cwd(session_id) })
                .await
        } else {
            None
        };
        let _ = cx.update(|window, cx| {
            store.update(cx, |store, cx| {
                // 用**返回的那个 pane**,不能事后再取活动 pane:焦点还没落下去
                let Some(pane_id) =
                    store.new_terminal_with_cwd(&project_id, None, anchor, cwd.clone(), window, cx)
                else {
                    return;
                };
                store.write_to_pane(&project_id, &pane_id, &format!("{command}\r"), cx);
                // 恢复出的会话身份**当场**写回 pane,不等 hook ——
                // codex resume 不会重新上报 SessionStart
                store.set_pane_ai_session(
                    &project_id,
                    &pane_id,
                    AiSessionRef {
                        agent: Some(session.session_type.clone()),
                        session_id: session.id.clone(),
                        cwd,
                    },
                    cx,
                );
                store.focus_pane(&project_id, &pane_id, window, cx);
            });
            crate::workbench_area::activate_terminal_page(window, cx);
        });
    })
}

/// 项目是否有 WSL 会话来源:UNC 形态的 WSL 根项目,或显式配置了发行版。
///
/// (`mt_ai` 的 `parse_wsl_unc` 目前是 crate 私有,这里按前缀判一道;
/// 见交付说明的「接线需求」。)
fn has_wsl_source(path: &str, distro: Option<&str>) -> bool {
    if distro.is_some_and(|d| !d.is_empty()) {
        return true;
    }
    let lower = path.to_ascii_lowercase().replace('/', "\\");
    lower.starts_with("\\\\wsl$\\") || lower.starts_with("\\\\wsl.localhost\\")
}

/// `PaneStatus` → `StatusKind`(mt-ui 不能反向依赖 mt-app,在这里转一次)。
fn status_kind(status: PaneStatus) -> StatusKind {
    match status {
        PaneStatus::Idle => StatusKind::Idle,
        PaneStatus::AiIdle => StatusKind::AiIdle,
        PaneStatus::AiWorking => StatusKind::AiWorking,
        PaneStatus::Error => StatusKind::Error,
    }
}

/// ISO 8601 → 「刚刚 / n 分钟前 / n 小时前 / n 天前 / 月-日」。
fn format_time(iso: &str) -> String {
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(iso) else {
        return String::new();
    };
    let now = chrono::Local::now();
    let minutes = (now.timestamp() - ts.timestamp()) / 60;
    if minutes < 1 {
        return t("sessionList", "time.justNow").to_string();
    }
    if minutes < 60 {
        return tr!("sessionList", "time.minutesAgo", n = minutes);
    }
    let hours = minutes / 60;
    if hours < 24 {
        return tr!("sessionList", "time.hoursAgo", n = hours);
    }
    let days = hours / 24;
    if days < 7 {
        return tr!("sessionList", "time.daysAgo", n = days);
    }
    let local = ts.with_timezone(&chrono::Local);
    use chrono::Datelike;
    if local.year() == now.year() {
        tr!(
            "sessionList",
            "time.monthDay",
            m = local.month(),
            d = local.day()
        )
    } else {
        format!("{}/{}/{}", local.year(), local.month(), local.day())
    }
}

/// 消息头那一行的时间(ISO 8601 → 本地时钟 `月-日 时:分:秒`)。
///
/// 会话列表那边给的是「几分钟前」(找会话用),正文里要的是**绝对时刻** ——
/// 看一段对话是什么时候发生的、两条之间隔了多久,相对时间答不了。带秒是因为
/// 相邻消息常落在同一分钟内。
///
/// 时间戳解析不出来(远古记录 / 字段缺失)返回 `None`,那条就不显示时间 ——
/// 宁可少一行灰字,也不显示 `1970-01-01`。
fn format_message_time(iso: &str) -> Option<String> {
    let ts = chrono::DateTime::parse_from_rfc3339(iso.trim()).ok()?;
    Some(
        ts.with_timezone(&chrono::Local)
            .format("%m-%d %H:%M:%S")
            .to_string(),
    )
}

/// 会话正文的富文本排版。取自 [`crate::file_viewer`] 的 markdown 预览那份,
/// 按抽屉里的 12px 正文重新定基准(那边是 14px 的文档视图)。
fn preview_text_style(cx: &mut App) -> TextViewStyle {
    let mut code_block = gpui::StyleRefinement::default();
    {
        let text = code_block.text.get_or_insert_default();
        text.font_size = Some(ui::font_px(11.0).into());
        text.line_height = Some(gpui::relative(1.5).into());
    }
    TextViewStyle {
        highlight_theme: cx.theme().highlight_theme.clone(),
        is_dark: cx.theme().mode.is_dark(),
        heading_base_font_size: ui::font_px(12.0),
        // 抽屉窄,段距按文档视图的 1rem 给会把一条消息撑得很散
        paragraph_gap: gpui::rems(0.5),
        code_block,
        ..Default::default()
    }
    .heading_font_size(|level, base| match level {
        1 => base * 1.4,
        2 => base * 1.2,
        3 => base * 1.1,
        _ => base,
    })
}

/// 会话正文预览的一次加载。
struct Preview {
    /// 会话 id。只用于给每条消息的 [`TextView`] 拼稳定且**跨会话不撞**的
    /// element id —— 光按序号编的话,换一个会话看会命中上一个会话同序号那条
    /// 的缓存状态,首帧显示的是别人的正文。
    session_id: String,
    title: String,
    loading: bool,
    error: Option<String>,
    messages: Vec<AiSessionMessage>,
    /// 与 `messages` 同下标的安全渲染副本；复制动作仍使用原始正文。
    rendered_messages: Vec<String>,
    /// 已铺出去的条数(见 [`PREVIEW_PAGE_SIZE`])。
    shown: usize,
    /// 可复制的 resume 命令(拼不出来则为 None)。
    command: Option<String>,
}

impl Preview {
    /// 「复制全文」的文本:整份对话按 `角色 · 时间` + 正文平铺。
    ///
    /// 逐条选中复制是 [`TextView`] 的能力,但它的选区**只在单个 TextView 内**
    /// (选区坐标是相对自己 bounds 算的),跨消息拖不出来 —— 整份对话只能由
    /// 这里拼。
    fn all_text(&self) -> String {
        self.messages
            .iter()
            .map(|m| {
                let role = if m.role == "user" { "User" } else { "Assistant" };
                match format_message_time(&m.timestamp) {
                    Some(time) => format!("{role} · {time}\n{}", m.content),
                    None => format!("{role}\n{}", m.content),
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// 平铺 / 分支树两种视图。取值与 `AppConfig::session_list_view` 的字面量同。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewMode {
    Flat,
    Tree,
}

impl ViewMode {
    fn key(self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Tree => "tree",
        }
    }

    fn toggled(self) -> Self {
        match self {
            Self::Flat => Self::Tree,
            Self::Tree => Self::Flat,
        }
    }
}

pub struct SessionPanel {
    store: Entity<AppStore>,
    /// 上次拉取用的项目路径 —— 项目切换时据此重拉。
    project_path: Option<String>,
    host: Vec<AiSession>,
    wsl: Vec<AiSession>,
    /// 磁盘扫描出来的分支边。树视图的数据面,平铺不消费。
    lineage: Vec<LineageEdge>,
    /// 自记账边(`AppConfig::session_lineage`)。本面板**只读**;
    /// 写入端是 pane 右键的「分支会话到新分屏」
    /// (`pane_actions::fork_pane_session` → `AppStore::consume_pending_fork`)。
    bookkept: Vec<LineageEdge>,
    loading: bool,
    wsl_loading: bool,
    display_count: usize,
    /// 平铺 / 树。**存本地态而不是每次 render 去读 config** ——
    /// 面板已经 `cx.observe(&store)`,读 config 会让每次 store 变化都重算。
    view: ViewMode,
    /// 请求序号:项目切换后旧请求(尤其是慢的 WSL)返回时不得覆盖新项目的列表。
    request_id: u64,
    /// 抽屉是否展开。旧版 `SessionList` 挂在 `RightDrawer` 里,收起时压根不挂载,
    /// 自然也不会去扫会话 —— 这里是常驻实体,只能自己记一份可见性。
    visible: bool,
    /// 关着的时候项目切过 → 打开时补拉一次。
    stale: bool,
    /// 当前项目是 SSH 远程项目(转圈提示要区分 WSL 与远程两种来源)。
    remote: bool,
    /// 断链的远程项目:列表恒空,头部给一句断链提示。
    remote_broken: bool,
    preview: Option<Preview>,
    _tasks: Vec<Task<()>>,
}

impl SessionPanel {
    pub fn new(store: Entity<AppStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |this: &mut Self, _, cx| {
            // 项目切了才重拉;别的 store 变化(状态灯之类)只重画
            let path = this.store.read(cx).active_project().map(|p| p.path.clone());
            if path != this.project_path {
                if this.visible {
                    this.refresh(false, cx);
                } else {
                    // 收着的时候不去扫:WSL 那一路要冷启动整台 VM,不该由「切了个
                    // 项目」触发(旧版收起时组件根本没挂载)
                    this.stale = true;
                }
            }
            cx.notify();
        })
        .detach();
        let view = match store.read(cx).session_list_view() {
            "tree" => ViewMode::Tree,
            _ => ViewMode::Flat,
        };
        Self {
            store,
            project_path: None,
            host: Vec::new(),
            wsl: Vec::new(),
            lineage: Vec::new(),
            bookkept: Vec::new(),
            loading: false,
            wsl_loading: false,
            display_count: PAGE_SIZE,
            view,
            request_id: 0,
            visible: false,
            stale: true,
            remote: false,
            remote_broken: false,
            preview: None,
            _tasks: Vec::new(),
        }
    }

    fn toggle_view(&mut self, cx: &mut Context<Self>) {
        self.view = self.view.toggled();
        let key = self.view.key();
        self.store
            .update(cx, |store, cx| store.set_session_list_view(key, cx));
        cx.notify();
    }

    /// 抽屉开合。第一次展开(或关着的时候项目切过)在这里补拉。
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible && self.stale {
            self.refresh(false, cx);
        }
    }

    /// 重拉两个来源 + 分支边。`force` 绕过 `mt_ai` 的会话缓存(手动刷新用)。
    ///
    /// **只在 `visible` 为真时被调用**(见 `new` 里的 observe 与 [`set_visible`])——
    /// 新增的分支边扫描同样挂在这道闸后面,不许绕过去直接拉。
    ///
    /// [`set_visible`]: Self::set_visible
    pub fn refresh(&mut self, force: bool, cx: &mut Context<Self>) {
        let (project, source) = {
            let store = self.store.read(cx);
            let project = store
                .active_project()
                .map(|p| (p.path.clone(), p.wsl_sessions_distro.clone()));
            // **唯一的来源分流开关**(见模块注释):三条并发请求与远程那一条
            // 不会同时发出
            let source = store.active_project().map(|p| {
                crate::ssh_conn::session_source(p, store.ssh_connections())
            });
            (project, source)
        };
        // 自记账边:mini-term 自己发起的 fork 当场记下的 child→parent。
        // **必须传** —— Claude 的 CLI fork 不写磁盘指针,这些边的「分叉后第一问」
        // 标题只能由 mt-ai 拿父子文件比对补出。两个 crate 各持一份同构结构,
        // 转换是上层的活(`sessions.rs:1480-1484` 的设计)。
        let saved = self.store.read(cx).config().session_lineage.clone();
        let bookkept: Vec<mt_ai::sessions::BookkeptLineageEdge> = saved
            .iter()
            .map(|e| mt_ai::sessions::BookkeptLineageEdge {
                agent: e.agent.clone(),
                session_id: e.session_id.clone(),
                parent_session_id: e.parent_session_id.clone(),
                fork_point_uuid: e.fork_point_uuid.clone(),
            })
            .collect();
        // 同一批边留一份 `LineageEdge` 形态,给「扫描失败」时的兜底合并用
        self.bookkept = saved
            .iter()
            .map(|e| LineageEdge {
                agent: e.agent.clone(),
                session_id: e.session_id.clone(),
                parent_session_id: e.parent_session_id.clone(),
                fork_point_uuid: e.fork_point_uuid.clone(),
                branch_title: None,
            })
            .collect();
        self.request_id += 1;
        let req = self.request_id;
        self.stale = false;
        self.host.clear();
        self.wsl.clear();
        self.lineage.clear();
        self.display_count = PAGE_SIZE;
        self.preview = None;
        self._tasks.clear();

        let Some((path, distro)) = project else {
            self.project_path = None;
            self.loading = false;
            self.wsl_loading = false;
            self.remote = false;
            self.remote_broken = false;
            cx.notify();
            return;
        };
        self.project_path = Some(path.clone());
        self.loading = true;

        // ── SSH 远程项目:**只**取远程来源 ────────────────────────
        match source {
            Some(crate::ssh_conn::SessionSource::BrokenRemote) => {
                // 连接被删:什么都取不到。**绝不退回本地扫描** —— 那会把本机
                // 同路径的会话贴到这个远程项目上(见 `ssh_conn::SessionSource`)
                self.remote = true;
                self.remote_broken = true;
                self.loading = false;
                self.wsl_loading = false;
                cx.notify();
                return;
            }
            Some(crate::ssh_conn::SessionSource::Remote(conn)) => {
                self.remote = true;
                self.remote_broken = false;
                self.wsl_loading = false;
                let remote_path = path.clone();
                self._tasks.push(cx.spawn(async move |this, cx| {
                    // [后台] SFTP 往返,秒级;`ai_sessions` 永不返 Err
                    // (失败静默降级为空表,与原版同)
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            crate::remote_ssh::ai_sessions(&conn, &remote_path, force)
                        })
                        .await;
                    let _ = this.update(cx, |this: &mut Self, cx| {
                        if this.request_id != req {
                            return;
                        }
                        this.host = result.unwrap_or_default();
                        this.loading = false;
                        cx.notify();
                    });
                }));
                cx.notify();
                return;
            }
            _ => {
                self.remote = false;
                self.remote_broken = false;
            }
        }

        // 宿主来源:秒出,先显示
        let host_path = path.clone();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { mt_ai::sessions::get_ai_sessions(host_path) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.request_id != req {
                    return;
                }
                this.host = result.unwrap_or_default();
                this.loading = false;
                cx.notify();
            });
        }));

        // 分支边:与会话列表**并行**拉取(只读文件头,同量级),同一个请求序号守卫;
        // 失败按无分支处理,不影响会话列表
        let lineage_path = path.clone();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    mt_ai::sessions::scan_session_lineage(lineage_path, Some(bookkept))
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.request_id != req {
                    return;
                }
                // 扫描永远返回 Vec(内部逐文件容错),失败 = 空表 = 按无分支处理
                this.lineage = result;
                cx.notify();
            });
        }));

        // WSL 来源:并行请求,到达后合并(不阻塞宿主显示)
        if has_wsl_source(&path, distro.as_deref()) {
            self.wsl_loading = true;
            self._tasks.push(cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        mt_ai::sessions::get_wsl_ai_sessions(path, distro, Some(force))
                    })
                    .await;
                let _ = this.update(cx, |this: &mut Self, cx| {
                    if this.request_id != req {
                        return;
                    }
                    this.wsl = result.unwrap_or_default();
                    this.wsl_loading = false;
                    cx.notify();
                });
            }));
        } else {
            self.wsl_loading = false;
        }
        cx.notify();
    }

    /// 两个来源按时间戳降序混排(与后端排序口径一致:ISO 8601 字符串比较)。
    fn merged(&self) -> Vec<&AiSession> {
        let mut all: Vec<&AiSession> = self.host.iter().chain(self.wsl.iter()).collect();
        if !self.wsl.is_empty() {
            all.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        }
        all
    }

    /// 渲染用的行:`(会话, 连线前缀, 显示标题)`。
    ///
    /// **两模式共用同一套行渲染** —— 平铺模式 `prefix` 为空串、标题就是会话标题,
    /// 树只是列表长出了结构。
    ///
    /// ⚠️ 树模式的分页 `take(display_count)` 在**拍平之后**做 —— 反过来会把父
    /// 截掉、子全变成孤儿根。
    fn rows(&self) -> Vec<(AiSession, String, String)> {
        let all = self.merged();
        if self.view == ViewMode::Flat {
            return all
                .into_iter()
                .take(self.display_count)
                .map(|s| (s.clone(), String::new(), s.title.clone()))
                .collect();
        }
        // 磁盘边 + 自记账边(**磁盘优先**)。自记账那份在扫描时已被 mt-ai 补过
        // 标题并入返回值,这里再合一次是为了兜住「扫描整个失败但自记账还在」的窗口
        // (原版 `SessionList.tsx:260` 同样合两次)。
        let edges = merge_lineage_edges(self.lineage.clone(), self.bookkept.clone());
        let ids: Vec<String> = all.iter().map(|s| s.id.clone()).collect();
        let timestamps: Vec<String> = all.iter().map(|s| s.timestamp.clone()).collect();
        let roots = build_session_tree(&ids, &timestamps, &edges);
        flatten_session_tree(&roots)
            .into_iter()
            .take(self.display_count)
            .filter_map(|row| {
                let session = all.get(row.index)?;
                // fork 是**整份复制**,标题字段连同首条消息一起继承自根会话,
                // 分支之间全同名 —— 真正区分一条分支的是它岔开后干了什么
                let title = row
                    .edge
                    .and_then(|i| edges.get(i))
                    .and_then(|e| e.branch_title.clone())
                    .unwrap_or_else(|| session.title.clone());
                Some(((*session).clone(), row.prefix, title))
            })
            .collect()
    }

    /// 在当前活动 pane 里恢复会话。没有终端时退化成「开一个新的再恢复」。
    fn resume(&mut self, command: String, new_tab: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.store.read(cx).active_project_id.clone() else {
            return;
        };
        let existing = self.store.read(cx).active_pane_id(&project_id);
        self.store.update(cx, |store, cx| {
            let target = if new_tab || existing.is_none() {
                // 不能事后再 resolveActivePane:新终端的焦点还没落下去,
                // 拿到的会是用户原本待着的那个 —— 命令就敲进别人的会话了
                store.new_terminal(&project_id, None, existing.clone(), window, cx)
            } else {
                existing.clone()
            };
            let Some(pane_id) = target else { return };
            store.write_to_pane(&project_id, &pane_id, &format!("{command}\r"), cx);
            store.focus_pane(&project_id, &pane_id, window, cx);
        });
        crate::workbench_area::activate_terminal_page(window, cx);
    }

    /// 会话行的右键菜单(`SessionList.tsx:378-401`,顺序照抄):
    ///
    /// ```text
    /// 查看                        ← 恒在
    /// ──────────                  ← 仅当 canResumeHere
    /// 在当前终端恢复
    /// 在新终端标签恢复
    /// ──────────                  ← 仅当 cmd 拼得出
    /// 复制恢复命令
    /// ```
    ///
    /// `canResumeHere = cmd.is_some() && wsl_distro.is_none() && ssh_connection_id.is_none()`
    /// —— 会话来自别处时把命令敲进本机终端跑不通,只留「查看 / 复制命令」。
    fn row_menu(&self, session: &AiSession, cx: &mut Context<Self>) -> Vec<menu::MenuEntry> {
        let entity = cx.entity();
        let command = build_resume_command(&session.session_type, &session.id);
        let can_resume_here = command.is_some()
            && session.wsl_distro.is_none()
            && session.ssh_connection_id.is_none();

        let mut entries = vec![menu::item(t("sessionList", "view"), {
            let entity = entity.clone();
            let session = session.clone();
            move |_window, cx: &mut App| {
                let session = session.clone();
                entity.update(cx, |this, cx| this.open_preview(&session, cx));
            }
        })];
        if can_resume_here && let Some(cmd) = command.clone() {
            entries.push(menu::separator());
            for (label, new_tab) in [
                (t("sessionList", "resumeHere"), false),
                (t("sessionList", "resumeInNewTab"), true),
            ] {
                let entity = entity.clone();
                let cmd = cmd.clone();
                entries.push(menu::item(label, move |window, cx: &mut App| {
                    let cmd = cmd.clone();
                    entity.update(cx, |this, cx| this.resume(cmd, new_tab, window, cx));
                }));
            }
        }
        if let Some(cmd) = command {
            entries.push(menu::separator());
            entries.push(menu::item(
                t("sessionList", "copyResumeCommand"),
                move |_window, cx: &mut App| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(cmd.clone()));
                },
            ));
        }
        entries
    }

    fn open_preview(&mut self, session: &AiSession, cx: &mut Context<Self>) {
        let Some(project_path) = self.project_path.clone() else {
            return;
        };
        self.preview = Some(Preview {
            session_id: session.id.clone(),
            title: session.title.clone(),
            loading: true,
            error: None,
            messages: Vec::new(),
            rendered_messages: Vec::new(),
            shown: PREVIEW_PAGE_SIZE,
            command: build_resume_command(&session.session_type, &session.id),
        });
        let session_type = session.session_type.clone();
        let session_id = session.id.clone();
        let distro = session.wsl_distro.clone();
        // 远程会话的正文在**另一台机器**上,只能走 SFTP 读(`ai_session_content`);
        // 连接从主线程取好再传进后台(`remote_ssh` 的线程口径)
        let remote = {
            let store = self.store.read(cx);
            store
                .active_project_id
                .as_deref()
                .and_then(|id| store.remote_connection_of(id))
        };
        self._tasks.push(cx.spawn(async move |this, cx| {
            // 正文可能几 MB + WSL 9P / SFTP 往返,雷打不动丢后台
            let result = cx
                .background_executor()
                .spawn(async move {
                    let result = match remote {
                        // 循环续读到文件末尾:单次 SFTP 读封顶 8 MB,只读一段的话
                        // 大会话后半截会被静默丢掉(前进保证与总量护栏在 all 里)
                        Some(conn) => crate::remote_ssh::ai_session_content_all(
                            &conn,
                            &session_type,
                            &session_id,
                            &project_path,
                        ),
                        None => mt_ai::sessions::get_ai_session_content(
                            session_type,
                            session_id,
                            project_path,
                            distro,
                        ),
                    };
                    result.map(|messages| {
                        let rendered_messages = messages
                            .iter()
                            .map(|message| {
                                crate::file_viewer::sanitize_session_markdown(&message.content)
                            })
                            .collect::<Vec<_>>();
                        (messages, rendered_messages)
                    })
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                let Some(preview) = this.preview.as_mut() else {
                    return;
                };
                preview.loading = false;
                match result {
                    Ok((messages, rendered_messages)) => {
                        preview.messages = messages;
                        preview.rendered_messages = rendered_messages;
                    }
                    Err(err) => preview.error = Some(err),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 会话正文预览。
    ///
    /// **正文走 [`TextView`] 而不是静态文本**:GPUI 的 `div().child(文本)` 画出来
    /// 的字一个都选不中,而 gpui-component 里唯一带选区的文本渲染器就是它
    /// (`selectable(true)` + `Ctrl/Cmd+C`,与文件查看器的 markdown 预览同一个)。
    /// 顺带把会话正文按 markdown 排出来 —— 三家 agent 的回复本来就是 markdown。
    ///
    /// 选区**跨不了消息**(组件的选区坐标相对单个 TextView 自己的 bounds),
    /// 所以每条消息头上留一颗「复制」、顶上留一颗「复制全文」把整条/整份兜住。
    fn render_preview(
        &mut self,
        preview_title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(preview) = self.preview.as_ref() else {
            return div().into_any_element();
        };
        let loading = preview.loading;
        let error = preview.error.clone();
        let command = preview.command.clone();
        let session_key = preview.session_id.clone();
        let total = preview.messages.len();
        let shown = preview.shown.min(total);
        let has_messages = total > 0;
        let text_style = preview_text_style(cx);

        let mut body = div()
            .id("session-preview-body")
            .flex_1()
            .overflow_y_scroll()
            .px(px(10.0))
            .flex()
            .flex_col()
            .gap(px(8.0));

        if loading {
            body = body.child(
                div()
                    .py(px(12.0))
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(t("sessionViewer", "loading")),
            );
        }
        if let Some(err) = error {
            body = body.child(
                div()
                    .py(px(12.0))
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::color_error())
                    .child(err),
            );
        }
        if !loading && !has_messages {
            body = body.child(
                div()
                    .py(px(12.0))
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(t("sessionViewer", "emptyContent")),
            );
        }

        // 逐条引用着画,不整页 clone:终端一跑起来整窗每帧重绘(GPUI 的
        // notify 是整窗口口径),一页几十条正文每帧复制一遍就是白烧内存带宽
        for (ix, msg) in preview.messages.iter().take(shown).enumerate() {
            let is_user = msg.role == "user";
            let time = format_message_time(&msg.timestamp);
            let content = msg.content.clone();
            let rendered_content = preview.rendered_messages[ix].clone();
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .p(px(8.0))
                    .rounded(px(4.0))
                    .bg(if is_user {
                        ui::bg_overlay()
                    } else {
                        ui::bg_base()
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .text_size(ui::font_px(10.0))
                            .text_color(ui::text_muted())
                            // 旧版 `SessionViewerModal.tsx` 这两个角色名就是硬编码的
                            // 英文字面量(不进字典),照抄
                            .child(if is_user { "User" } else { "Assistant" })
                            .child(div().flex_1().when_some(time, |el, time| el.child(time)))
                            .child(
                                div()
                                    .id(SharedString::from(format!("session-msg-copy-{ix}")))
                                    .cursor_pointer()
                                    .hover(|el| el.text_color(ui::accent()))
                                    .child(t("sessionViewer", "copyMessage"))
                                    .on_click(move |_, _window, cx| {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            content.clone(),
                                        ));
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .text_size(ui::font_px(12.0))
                            .text_color(ui::text_secondary())
                            .child(
                                TextView::markdown(
                                    // id 带会话 id:换会话看时不复用上一份的解析缓存
                                    SharedString::from(format!("session-msg-{session_key}-{ix}")),
                                    rendered_content,
                                    window,
                                    cx,
                                )
                                .style(text_style.clone())
                                .selectable(true),
                            ),
                    ),
            );
        }

        if shown < total {
            let remaining = total - shown;
            body = body.child(
                div()
                    .id("session-preview-more")
                    .py(px(8.0))
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .cursor_pointer()
                    .hover(|el| el.text_color(ui::accent()))
                    .child(tr!("sessionList", "loadMore", n = remaining))
                    .on_click(cx.listener(|this: &mut Self, _, _window, cx| {
                        if let Some(preview) = this.preview.as_mut() {
                            preview.shown += PREVIEW_PAGE_SIZE;
                        }
                        cx.notify();
                    })),
            );
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(10.0))
                    .py(px(6.0))
                    .border_b_1()
                    .border_color(ui::border_subtle())
                    .child(
                        ui::ghost_button(
                            "session-preview-back",
                            format!("‹ {}", t("fileViewer", "back")),
                        )
                        .on_click(cx.listener(|this: &mut Self, _, _window, cx| {
                            this.preview = None;
                            cx.notify();
                        })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_size(ui::font_px(12.0))
                            .text_color(ui::text_primary())
                            .child(preview_title),
                    )
                    .when(has_messages, |el| {
                        el.child(
                            ui::ghost_button("session-copy-all", t("sessionViewer", "copyAll"))
                                .tooltip(move |window, cx| {
                                    Tooltip::new(tr!("sessionViewer", "messageCount", count = total))
                                        .build(window, cx)
                                })
                                .on_click(cx.listener(|this: &mut Self, _, _window, cx| {
                                    let Some(preview) = this.preview.as_ref() else {
                                        return;
                                    };
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        preview.all_text(),
                                    ));
                                })),
                        )
                    })
                    .when_some(command, |el, command| {
                        el.child(
                            ui::ghost_button(
                                "session-copy-cmd",
                                t("sessionList", "copyResumeCommand"),
                            )
                            .on_click(move |_, _window, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                    command.clone(),
                                ));
                            }),
                        )
                    }),
            )
            .child(body)
            .into_any_element()
    }
}

impl Render for SessionPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let container = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(ui::bg_surface())
            .border_l_1()
            .border_color(ui::border_default());

        if let Some(title) = self.preview.as_ref().map(|p| p.title.clone()) {
            let body = self.render_preview(title, window, cx);
            return container.child(body);
        }

        let rows = self.rows();
        let sessions: Vec<AiSession> = rows.iter().map(|(s, _, _)| s.clone()).collect();
        let total = self.host.len() + self.wsl.len();
        let has_more = self.display_count < total;
        let loading = self.loading;
        let wsl_loading = self.wsl_loading;
        let has_project = self.project_path.is_some();
        let tree = self.view == ViewMode::Tree;
        let remote = self.remote;
        let remote_broken = self.remote_broken;
        // 行尾的远程来源标识:连接名(连接被删时回退 'SSH')。
        // 逐行现查连接表,规模是连接条数
        let remote_name_of = |session: &AiSession| -> Option<String> {
            let id = session.ssh_connection_id.as_deref()?;
            Some(
                self.store
                    .read(cx)
                    .ssh_connections()
                    .iter()
                    .find(|c| c.id == id)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "SSH".to_string()),
            )
        };
        // 树模式的在跑徽章要对 pane 状态保持反应性 —— 面板已经 observe 了 store,
        // 这里逐行现查即可(跨项目遍历,规模是 pane 数)
        let live_of: Vec<Option<(String, PaneStatus)>> = if tree {
            sessions
                .iter()
                .map(|s| {
                    self.store
                        .read(cx)
                        .find_live_session_pane(&s.id)
                        .map(|(project_id, _, status)| (project_id, status))
                })
                .collect()
        } else {
            vec![None; sessions.len()]
        };
        let project_name_of = |project_id: &str| -> String {
            self.store
                .read(cx)
                .projects()
                .iter()
                .find(|p| p.id == project_id)
                .map(|p| p.name.clone())
                .unwrap_or_default()
        };

        let mut list = div()
            .id("session-list")
            .flex_1()
            .overflow_y_scroll()
            .px(px(6.0))
            .flex()
            .flex_col();

        if remote_broken {
            // 断链:列表恒空 + 一句明确提示(原版是静默空表,见模块注释)
            list = list.child(
                div()
                    .py(px(12.0))
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::color_error())
                    .child(t("projectList", "remoteBrokenTitle")),
            );
        } else if loading && sessions.is_empty() {
            list = list.child(
                div()
                    .py(px(12.0))
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(t("sessionList", "loading")),
            );
        } else if sessions.is_empty() {
            list = list.child(
                div()
                    .py(px(12.0))
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(if has_project {
                        t("sessionList", "empty")
                    } else {
                        t("sessionList", "selectProject")
                    }),
            );
        }

        for (i, (session, prefix, display_title)) in rows.into_iter().enumerate() {
            let key = format!(
                "{}-{}-{}",
                session.session_type,
                session
                    .wsl_distro
                    .as_deref()
                    .or(session.ssh_connection_id.as_deref())
                    .unwrap_or("host"),
                session.id
            );
            let remote_badge = remote_name_of(&session);
            let time = format_time(&session.timestamp);
            // 行图标**两套口径**(`SessionList.tsx:339-342`):
            // 树模式按最新模型推厂商(claude CLI 挂 GLM / DeepSeek 中转是常见用法,
            // CLI ≠ 模型厂商);平铺模式沿用 CLI 图标。
            let vendor = if tree {
                AiVendor::for_session(&session.session_type, session.model.as_deref())
            } else {
                // 平铺的缺省是 claude(原版 `TYPE_VENDOR[...] ?? 'claude'`)
                AiVendor::from_session_type(&session.session_type)
                    .or(Some(AiVendor::Claude))
            };
            let wsl_badge = session.wsl_distro.clone();
            let live = live_of.get(i).cloned().flatten();
            let live_project = live
                .as_ref()
                .map(|(project_id, _)| project_name_of(project_id));
            // tooltip:树模式区分 live / 非 live,平铺恒是会话标题
            let tip: SharedString = if tree {
                match &live_project {
                    Some(name) => {
                        tr!("sessionList", "branchTree.runningIn", project = name.clone()).into()
                    }
                    None => format!(
                        "{display_title}\n{}",
                        t("sessionList", "branchTree.clickToResume")
                    )
                    .into(),
                }
            } else {
                session.title.clone().into()
            };
            let session_for_menu = session.clone();
            let session_for_click = session.clone();

            list = list.child(
                div()
                    .id(SharedString::from(format!("session-row-{key}")))
                    .flex()
                    .items_start()
                    .gap(px(8.0))
                    .px(px(10.0))
                    .py(px(6.0))
                    .rounded(px(4.0))
                    .text_size(ui::font_px(12.0))
                    .hover(|el| el.bg(ui::border_subtle()))
                    // 树模式整行可点(跳转 / 恢复),平铺不可点
                    .when(tree, |el| el.cursor_pointer())
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .when(tree, |el| {
                        el.on_click(cx.listener(move |this: &mut Self, _, window, cx| {
                            let task =
                                jump_to_session(&this.store, session_for_click.clone(), window, cx);
                            this._tasks.push(task);
                        }))
                    })
                    // 四项右键菜单(N 批的 `menu.rs` 已就位,行内按钮随之收回)
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this: &mut Self, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            let entries = this.row_menu(&session_for_menu, cx);
                            menu::show(event.position, entries, window, cx);
                        }),
                    )
                    // 树连线前缀:**等宽字体 + 不换行 + 不截断**,`│├└` 才对得齐
                    .when(!prefix.is_empty(), |el| {
                        el.child(
                            div()
                                .flex_none()
                                .mt(px(1.0))
                                .font_family("monospace")
                                .whitespace_nowrap()
                                .text_color(ui::text_muted())
                                .child(prefix.clone()),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .w(px(16.0))
                            .h(px(16.0))
                            .mt(px(1.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                BrandIcon::new(vendor)
                                    .size(px(14.0))
                                    // VectorIcon 自己画,不继承 text_color
                                    .color(ui::text_secondary()),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    // 在跑状态点
                                    .when_some(live.as_ref(), |el, (_, status)| {
                                        el.child(
                                            StatusDot::new(status_kind(*status))
                                                .size(px(11.0))
                                                .color(ui::status_color(*status))
                                                .contrast(ui::bg_surface()),
                                        )
                                    })
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.0))
                                            .truncate()
                                            .text_color(ui::text_secondary())
                                            .child(display_title.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    // 时间/徽章行不覆盖字号 —— 原版
                                    // (`SessionList.tsx:423`)与行同为 `text-xs`,
                                    // 靠 `--text-muted` 拉开层次
                                    .mt(px(2.0))
                                    .text_color(ui::text_muted())
                                    .child(match (wsl_badge, remote_badge) {
                                        (Some(distro), _) => format!(
                                            "{time} · {}·{distro}",
                                            t("sessionList", "wslBadge")
                                        ),
                                        // 远程会话标识:显示来源连接名
                                        (None, Some(name)) => format!("{time} · {name}"),
                                        (None, None) => time,
                                    }),
                            ),
                    ),
            );
        }

        if has_more {
            let remaining = total - self.display_count;
            list = list.child(
                div()
                    .id("session-load-more")
                    .py(px(6.0))
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .cursor_pointer()
                    .hover(|el| el.text_color(ui::accent()))
                    .on_click(cx.listener(|this: &mut Self, _, _window, cx| {
                        this.display_count += PAGE_SIZE;
                        cx.notify();
                    }))
                    .child(tr!("sessionList", "loadMore", n = remaining)),
            );
        }

        container
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(10.0))
                    .py(px(6.0))
                    .border_b_1()
                    .border_color(ui::border_subtle())
                    .child(
                        div()
                            .flex()
                            .gap(px(6.0))
                            .items_center()
                            .child(
                                div()
                                    .text_size(ui::font_px(11.0))
                                    .text_color(ui::text_muted())
                                    .child(t("panels", "sessions")),
                            )
                            // WSL 加载中的转圈。`wsl_loading` 落回 false 时整个
                            // 元素从树上消失,保底泵随之自停
                            .when(wsl_loading, |el| {
                                el.child(
                                    div()
                                        .id("session-wsl-spinner")
                                        .flex()
                                        .items_center()
                                        .tooltip(|window, cx| {
                                            Tooltip::new(t("sessionList", "wslLoading"))
                                                .build(window, cx)
                                        })
                                        .child(ui::spinner(px(12.0), ui::text_muted())),
                                )
                            })
                            // 远程来源加载中的转圈(原版 `loading && sshConnectionId`)
                            .when(loading && remote, |el| {
                                el.child(
                                    div()
                                        .id("session-remote-spinner")
                                        .flex()
                                        .items_center()
                                        .tooltip(|window, cx| {
                                            Tooltip::new(t("sessionList", "remoteLoading"))
                                                .build(window, cx)
                                        })
                                        .child(ui::spinner(px(12.0), ui::text_muted())),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            // 平铺 | 树 切换。仅在有活动项目时渲染(原版 `:295`)。
                            // 显示的字符与 tooltip **相反** —— 文案是「切到 X 视图」。
                            .when(has_project, |el| {
                                el.child(
                                    div()
                                        .id("session-view-toggle")
                                        .px(px(4.0))
                                        .font_family("monospace")
                                        .text_size(ui::font_px(12.0))
                                        .text_color(ui::text_muted())
                                        .cursor_pointer()
                                        .hover(|el| el.text_color(ui::text_primary()))
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(if tree {
                                                t("sessionList", "viewFlat")
                                            } else {
                                                t("sessionList", "viewTree")
                                            })
                                            .build(window, cx)
                                        })
                                        .on_click(cx.listener(|this: &mut Self, _, _window, cx| {
                                            this.toggle_view(cx);
                                        }))
                                        .child(if tree { "≡" } else { "⑂" }),
                                )
                            })
                            .child(
                                div()
                                    .id("session-refresh")
                                    .px(px(4.0))
                                    .text_size(ui::font_px(12.0))
                                    .text_color(ui::text_muted())
                                    .cursor_pointer()
                                    .hover(|el| el.text_color(ui::accent()))
                                    .tooltip(|window, cx| {
                                        Tooltip::new(t("sessionList", "refresh")).build(window, cx)
                                    })
                                    .on_click(cx.listener(|this: &mut Self, _, _window, cx| {
                                        this.refresh(true, cx);
                                    }))
                                    .child("↻"),
                            ),
                    ),
            )
            .child(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// resume 命令按 agent 分派,id 过白名单。
    #[test]
    fn resume_命令按_agent_分派() {
        assert_eq!(
            build_resume_command("claude", "abc-123").as_deref(),
            Some("claude --resume abc-123")
        );
        assert_eq!(
            build_resume_command("codex", "rollout_9").as_deref(),
            Some("codex resume rollout_9")
        );
        assert_eq!(
            build_resume_command("grok", "0199-x").as_deref(),
            Some("grok --resume 0199-x")
        );
        // 未知 agent 按 claude 兜底(与旧版一致)
        assert!(build_resume_command("whatever", "id1").is_some());
    }

    /// shell 元字符一律拦下 —— 这条命令是要原样写进 PTY 的。
    #[test]
    fn 非法会话_id_拒绝拼命令() {
        for bad in [
            "a b",
            "a;rm -rf /",
            "a|b",
            "a\nb",
            "a$(x)",
            "a`x`",
            "a\"b",
            "a'b",
            "../../etc",
            "",
        ] {
            assert!(
                build_resume_command("claude", bad).is_none(),
                "应拒绝: {bad:?}"
            );
        }
        assert!(build_resume_command("claude", &"a".repeat(129)).is_none());
        assert!(build_resume_command("claude", &"a".repeat(128)).is_some());
    }

    #[test]
    fn wsl_来源判定() {
        assert!(has_wsl_source("\\\\wsl$\\Ubuntu\\home\\u", None));
        assert!(has_wsl_source("\\\\wsl.localhost\\Debian\\srv", None));
        assert!(has_wsl_source("D:\\Git\\x", Some("Ubuntu")));
        assert!(!has_wsl_source("D:\\Git\\x", None));
        assert!(!has_wsl_source("D:\\Git\\x", Some("")));
    }

    #[test]
    fn 时间戳解析不出来时不显示() {
        assert_eq!(format_time("不是时间"), "");
        assert_eq!(format_time(""), "");
    }

    /// 消息头的时间是绝对时刻(本地时区),解析不出来就整个不显示。
    #[test]
    fn 消息时间按本地时钟成串() {
        // 固定偏移写死时区,断言与运行机器的时区无关:UTC+8 的 00:30
        let got = format_message_time("2026-08-24T00:30:05+08:00").expect("应能解析");
        let expect = chrono::DateTime::parse_from_rfc3339("2026-08-24T00:30:05+08:00")
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M:%S")
            .to_string();
        assert_eq!(got, expect);
        // 带毫秒与 Z 的形态(三家 agent 的记录里都出现过)照样认
        assert!(format_message_time("2026-08-24T00:30:05.123Z").is_some());
        assert!(format_message_time("").is_none());
        assert!(format_message_time("不是时间").is_none());
    }

    /// 「复制全文」拼的是 `角色 · 时间` + 正文,时间戳缺失时只留角色。
    #[test]
    fn 复制全文按角色与时间平铺() {
        let msg = |role: &str, content: &str, ts: &str| AiSessionMessage {
            role: role.to_string(),
            content: content.to_string(),
            timestamp: ts.to_string(),
        };
        let preview = Preview {
            session_id: "s1".into(),
            title: "t".into(),
            loading: false,
            error: None,
            messages: vec![
                msg("user", "问题", "2026-08-24T00:30:05+08:00"),
                msg("assistant", "回答", ""),
            ],
            rendered_messages: vec!["问题".into(), "回答".into()],
            shown: PREVIEW_PAGE_SIZE,
            command: None,
        };
        let text = preview.all_text();
        assert!(text.contains("User · "), "{text}");
        assert!(text.contains("\n问题"));
        // 时间戳缺失的那条不留悬空的分隔符
        assert!(text.contains("Assistant\n回答"), "{text}");
        // 两条之间空一行
        assert!(text.contains("问题\n\nAssistant"));
    }

    /// 分页只影响铺出去多少条,「复制全文」始终是整份。
    #[test]
    fn 复制全文不受分页影响() {
        let messages: Vec<AiSessionMessage> = (0..PREVIEW_PAGE_SIZE + 5)
            .map(|i| AiSessionMessage {
                role: "user".into(),
                content: format!("第{i}条"),
                timestamp: String::new(),
            })
            .collect();
        let preview = Preview {
            session_id: "s1".into(),
            title: "t".into(),
            loading: false,
            error: None,
            rendered_messages: messages
                .iter()
                .map(|message| message.content.clone())
                .collect(),
            messages,
            shown: PREVIEW_PAGE_SIZE,
            command: None,
        };
        let text = preview.all_text();
        assert!(text.contains(&format!("第{}条", PREVIEW_PAGE_SIZE + 4)));
    }
}
