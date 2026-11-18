//! 关终端 / 关整组的**唯一入口**(带 AI 感知确认框),外加「分支会话到新分屏」。
//!
//! 对应 `src/utils/paneActions.ts` 的 `closePane` / `closeLeaf` / `forkPaneSession`。
//!
//! # 为什么必须收成一个入口
//!
//! 关一个终端有四条路:tab 上的 ×、tab 右键菜单的「关闭此终端」、
//! 分屏控制条的 ×、Ctrl+Shift+W。原版四条都过同一对函数,所以确认框的口径
//! 天然一致;GPUI 侧此前是各调各的 `AppStore::close_*`(**完全不确认**),
//! 正在跑的 AI 会话点一下就没了。
//!
//! # 盘点口径
//!
//! 「活着的 AI 会话」= pane 状态是 `ai-working` 或 `ai-idle`
//! (逐字照抄原版 `p.status === 'ai-working' || p.status === 'ai-idle'`)。
//! 注意**不看** `ai_session` 身份:退出后的 pane 仍留着会话身份备查(供续接),
//! 那不算「关掉会终止的东西」;反过来输入检测认出、还没拿到 hook 身份的 AI
//! 照样是 ai-working,必须算进去。
//!
//! 单个 tab 的文案里带 pane 名(`closeTabAiMessage`),整组的文案里带**个数**
//! (`closeGroupAiMessage`)—— 与原版一字不差;个数之外**再列一串名字**放在
//! 灰色补充行里(原版没有这一段,但「哪几个终端会被杀」是关整组时最想知道的)。
//!
//! # 确认偏好(issue #90)
//!
//! 用户可以选择关终端不再确认(确认框的「不再询问」/ 设置页开关,都写
//! `AppConfig::confirm_terminal_close`)。偏好只决定**弹不弹框**:判据与关闭动作都
//! 收在 [`close_after_prompt`] 一处,弹框与不弹框跑的是同一个关闭闭包,
//! 回收 PTY / 撤出 AI 轮询 / 布局落盘全在 `AppStore::close_*` 里,一步不少。
//! 上面的盘点口径([`is_ai_alive`])不受偏好影响,它还有别的消费方。
//!
//! 例外是 **AI 手上有事**([`is_ai_busy`]:正在工作或亮黄灯等你处理):这时关闭会打断
//! 它手上的事,偏好关着也要问一次,判据见 [`close_needs_prompt`]。这个框不出「不再询问」
//! —— 它本来就关不掉。
//!
//! 反方向的例外是 **PTY 已经死了**([`is_dead`]:退出 / 起不来):关掉它不会终止任何东西,
//! 偏好开着也直接关。关整组时组内**全部**已死才算 —— 混着一个活的就照常问。免确认之后
//! 连点闸要跟着管这一下,见 [`swallows_close_click`]。
//!
//! 关整组 / 关面板在入口处问一次、之后一次关完,不存在「问了一半」:
//! `AppStore::close_*` 这一层本来就不弹框。
//!
//! 关窗([`crate::title_bar::allow_close`])、移除项目、删 worktree 各有各的确认框,
//! **不归这个偏好管**:它们一次毁掉的东西更多,语义也不同。
//!
//! # 已关闭栈
//!
//! 这里的每个关闭动作在真正关之前都把要关的 pane 记进项目的已关闭栈
//! (`AppStore::remember_closed_panes`,「重新打开已关闭的终端」的数据源,见
//! `store::reopen`)。记在 `do_close` 里而不是入口处:确认框被取消就不该入栈,
//! 而且要按确认那一刻的最新布局记(组里这期间可能又多了 tab)。别处顺手关掉的终端
//! (删 worktree、移除项目)不经这里,也就不入栈。
//!
//! # 关掉之后的键盘焦点
//!
//! 关掉的正是拿着键盘焦点的终端时,同一个入口把焦点交给接手的终端
//! ([`hand_focus_after_close`];接手者由 `AppStore::close_pane` 定)。其余落空的情形 ——
//! 点 × / 中键时焦点在页签上、确认框把焦点还给已经关掉的终端、项目里一个终端都不剩 ——
//! 由 Workspace 的「焦点落空」兜底接住(`TerminalArea::restore_lost_focus`)。

use std::time::{Duration, Instant};

use gpui::{App, Context, Entity, Window};
use mt_config::{AiLauncher, AppConfig, ShellConfig};

use crate::i18n::{t, tr};
use crate::menu;
use crate::prompt::Confirm;
use crate::session_branch::{BranchMenuSegment, branch_menu_segment};
use crate::store::{AppStore, resolve_fork_cwd};
use crate::tree::{PaneState, PaneStatus, SplitDirection, SplitNode, StatusLight};

/// 这个状态算「AI 会话还活着」吗。
pub fn is_ai_alive(status: PaneStatus) -> bool {
    matches!(status, PaneStatus::AiWorking | PaneStatus::AiIdle)
}

/// 这个 pane 的 AI 手上有事吗:正在工作,或亮黄灯等你处理(授权 / 表单 / 回合因 API
/// 错误结束)。跟状态灯走:Claude 等授权是 ai-idle + attention,只看 `PaneStatus` 会漏。
fn is_ai_busy(pane: &PaneState) -> bool {
    matches!(
        pane.light(),
        StatusLight::AiWorking | StatusLight::Attention
    )
}

/// 这个 pane 的 PTY 已经死了吗(退出 / 起不来,`status == error`)。error 是这条 PTY 的终态,
/// 只有重连换一条新的才离开,所以关掉它不会终止任何进程 —— 免确认的判据。
///
/// 看 `status` 而不是显示层:干净退出(灰灯)与异常退出(红灯)同样是死的。
fn is_dead(pane: &PaneState) -> bool {
    pane.status == PaneStatus::Error
}

/// 一组 pane 是不是**全部**已死。空组不算(没有东西可关,也就谈不上免确认)。
fn all_dead(panes: &[PaneState]) -> bool {
    !panes.is_empty() && panes.iter().all(is_dead)
}

/// 关这个 tab 会不会因为 PTY 已死而免确认 —— 连点闸据此决定要不要管这一下
/// (见 [`swallows_close_click`])。pane 不在了按「否」算。
pub fn pane_is_dead(store: &AppStore, project_id: &str, pane_id: &str) -> bool {
    store
        .project_state(project_id)
        .and_then(|s| s.pane(pane_id))
        .is_some_and(is_dead)
}

/// [`pane_is_dead`] 的整组版:叶子里全部 pane 都已死。
pub fn leaf_is_dead(store: &AppStore, project_id: &str, leaf_id: &str) -> bool {
    all_dead(&leaf_panes(store, project_id, leaf_id))
}

/// [`leaf_is_dead`] 的「手上只有 pane id」版(Ctrl+Shift+W 关的是焦点 pane 所在的整组)。
pub fn leaf_of_pane_is_dead(store: &AppStore, project_id: &str, pane_id: &str) -> bool {
    store
        .project_state(project_id)
        .and_then(|s| s.leaf_of_pane(pane_id))
        .is_some_and(|node| leaf_is_dead(store, project_id, node.id()))
}

// === 新建终端菜单 ===

/// 这个项目能不能在「新建终端」菜单里出 AI 启动器段。
///
/// 远程项目**一律不出**:SSH 项目的 PTY 是 ssh 启动器,启动初期可能停在口令或
/// host key 确认交互上,**预写的命令会被当口令消费** —— 命令丢失之外,登录本身
/// 还可能因此失败一次;存了密码的连接则会与 PTY 的密码 autofill 状态机抢同一次
/// 输入。判据与 `AppStore::hydrate_project` 的自动续接守卫同源(`store/panes.rs`,
/// 那里对远程项目跳过 resume 预写),也与移动端 `mt_relay::can_start_session`
/// 把远程项目挡在发起会话之外的口径一致。
///
/// WSL 项目**不挡**:本地 PTY 直接起 `wsl.exe`,没有口令交互那一段。移动端连
/// WSL 根项目一起挡是对话镜像盲发的问题,与桌面端这条路径无关。
pub fn project_allows_launchers(ssh_connection_id: Option<&str>) -> bool {
    ssh_connection_id.is_none()
}

/// 「新建终端」菜单的数据源:shell 列表 + 该项目**可用**的启动器。
///
/// 与 [`new_terminal_menu_entries`] 一样收成一处 —— 三处入口各判一次远程守卫
/// 迟早漏掉一处,而漏掉的那处正好是会把用户 ssh 口令吃掉的那条路。
/// 项目不存在时按「不给启动器」处置(保守侧)。
pub fn new_terminal_menu_data(
    store: &AppStore,
    project_id: &str,
) -> (Vec<ShellConfig>, Vec<AiLauncher>) {
    let shells = store.config().available_shells.clone();
    let allows = store
        .project(project_id)
        .is_some_and(|p| project_allows_launchers(p.ssh_connection_id.as_deref()));
    let launchers = if allows {
        store.mobile_relay().launchers
    } else {
        Vec::new()
    };
    (shells, launchers)
}

/// 「新建终端」菜单该不该弹出来。
///
/// 原判据是 `shells.len() <= 1` —— 只有一个 shell 时直接开,别让单 shell 用户
/// 每次多点一下(见 `terminal_area::render_leaf_tab_bar` 那处注释)。接入 AI
/// 启动器后可选项变成两段,判据必须**连启动器一起算**,否则单 shell 用户永远
/// 看不到启动器段。
///
/// ⚠️ **影响面比字面大**:启动器的读口径(`AppStore::mobile_relay`)在配置整块
/// 缺失时回落 `Default`,含预置的 Claude / Codex 两条,`launcher_count` 因此
/// 恒 ≥ 2 —— 除非用户把启动器删光。也就是说**所有**单 shell 用户点「+」从此
/// 都会弹菜单,而不只是自己精简过 shell 列表的那批。这是有意为之:本次的目的
/// 就是把启动器这个入口曝光出来,多的那一次点击换来的是「一键开 AI 会话」。
///
/// 例外是远程项目 —— [`new_terminal_menu_data`] 对它返回空启动器,
/// 于是单 shell + 远程项目仍然是点一下直接开。
pub fn should_show_new_terminal_menu(shell_count: usize, launcher_count: usize) -> bool {
    shell_count + launcher_count > 1
}

/// 「新建终端」菜单的条目:shell 段 +(有启动器时)AI 启动器段 +「管理启动器…」。
///
/// # 为什么收成一个入口
///
/// 新建终端有三条路:tab 栏的 `+`、空态的「+ 新建终端」按钮、终端面板的 `+`。
/// 三处此前是同一份代码复制三遍,菜单内容一致纯靠人工对齐;加了启动器段之后
/// 三份各改一遍必然漂移(与本模块开头「关终端必须收成一个入口」同一个理由)。
///
/// 落点差异由两个回调表达:前两处开 tab(带/不带锚点),第三处开面板。
pub fn new_terminal_menu_entries(
    shells: Vec<ShellConfig>,
    launchers: Vec<AiLauncher>,
    on_shell: impl Fn(ShellConfig, &mut Window, &mut App) + Clone + 'static,
    on_launcher: impl Fn(AiLauncher, &mut Window, &mut App) + Clone + 'static,
) -> Vec<menu::MenuEntry> {
    let mut entries: Vec<menu::MenuEntry> = shells
        .into_iter()
        .map(|shell| {
            let on_shell = on_shell.clone();
            let name = shell.name.clone();
            menu::item(name, move |window, cx| on_shell(shell.clone(), window, cx))
        })
        .collect();

    // 启动器一条都没有时不出这一段(含标题与分隔线)——空标题比没有更难看
    if !launchers.is_empty() {
        entries.push(menu::separator());
        entries.push(menu::MenuEntry::Header(
            t("terminalArea", "aiLaunchers").into(),
        ));
        for launcher in launchers {
            let on_launcher = on_launcher.clone();
            let name = launcher.name.clone();
            entries.push(menu::item(name, move |window, cx| {
                on_launcher(launcher.clone(), window, cx)
            }));
        }
    }

    // 启动器配置住在「移动端」面板里(它同时是移动端发起会话的名单),
    // 桌面端用户找不到那个入口 —— 这一条是唯一的指路牌,故**总是**出现。
    entries.push(menu::separator());
    entries.push(menu::item(
        t("terminalArea", "manageLaunchers"),
        crate::mobile_panel::open,
    ));
    entries
}

/// 盘点一组 pane 里活着的 AI 会话,返回它们的显示名(顺序同 tab 顺序)。
pub fn ai_session_labels(panes: &[PaneState]) -> Vec<String> {
    panes
        .iter()
        .filter(|p| is_ai_alive(p.status))
        .map(|p| p.label().to_string())
        .collect()
}

/// 这个 pane 要计入**关窗**确认吗(`App.tsx:57-60` 的 `collectLiveAiPanes`)。
///
/// 比关 tab / 关整组多一条 `ptyId !== undefined`:布局是从 `config.json` 恢复的,
/// 落盘时带着 `ai-idle` 的 pane 在 PTY 起来之前**什么都不会被杀掉**,
/// 拿它去拦关窗纯属噪音。状态判据本身与关 tab 完全相同。
pub fn counts_for_window_close(pane: &PaneState) -> bool {
    pane.pty_id.is_some() && is_ai_alive(pane.status)
}

/// 关窗确认正文里的一行:`· {项目名} / {标签}`;项目名为空时退成 `· {标签}`
/// (`App.tsx:62-63` 一字不差)。
pub fn window_close_line(project_name: &str, pane: &PaneState) -> String {
    if project_name.is_empty() {
        format!("· {}", pane.label())
    } else {
        format!("· {project_name} / {}", pane.label())
    }
}

/// 关窗前跨**全部项目**盘点活着的 AI 会话,返回正文用的名字列表。
///
/// 与 TS 的一处偏差(与 `collect_ai_projects` 同源):那边遍历 `projectStates`
/// (插入序),Rust 侧那是 `HashMap`、遍历序不定,于是改按**配置里的项目次序**走
/// —— 既确定,又与项目列表的上下顺序一致。
pub fn collect_live_ai_panes(store: &AppStore) -> Vec<String> {
    let mut names = Vec::new();
    for project in store.projects() {
        let Some(state) = store.project_state(&project.id) else {
            continue;
        };
        for pane in state.all_panes() {
            if counts_for_window_close(pane) {
                names.push(window_close_line(&project.name, pane));
            }
        }
    }
    names
}

/// 取一个叶子里的全部 pane(拷贝一份,免得确认框开着的时候借用还挂在 store 上)。
fn leaf_panes(store: &AppStore, project_id: &str, leaf_id: &str) -> Vec<PaneState> {
    store
        .project_state(project_id)
        .and_then(|s| s.node(leaf_id))
        .map(|node| match node {
            SplitNode::Leaf { panes, .. } => panes.clone(),
            _ => Vec::new(),
        })
        .unwrap_or_default()
}

/// 用户的确认偏好开没开。连点 / 连发闸只看它:偏好开着时第二下落在遮罩上或被快捷键
/// 让路挡住,偏好关着才需要闸。要不要弹框看 [`close_needs_prompt`]。
pub fn should_confirm_close(config: &AppConfig) -> bool {
    config.confirm_terminal_close
}

/// 这次关闭要不要弹确认框。偏好开着总是问;选了不再询问后(issue #90)普通终端与
/// 闲着的 AI(ai-idle 且没亮黄灯)都直接关,只有 AI 手上有事([`is_ai_busy`])时还要问一次。
///
/// 要关的终端 PTY 全都已死(`dead`)时一律不问:没有进程会被终止,问了只是多点一下。
/// 死掉的 pane 不会是 AI 手上有事(error 不亮黄灯、不转圈),两条例外不打架。
fn close_needs_prompt(config: &AppConfig, ai_busy: bool, dead: bool) -> bool {
    !dead && (should_confirm_close(config) || ai_busy)
}

/// 两次关闭快捷键的间隔短于它,后一次当成按住不放的连发。取值要盖过系统的首次连发
/// 延迟(Windows 最长一档 1s),否则第一次连发能漏过去多关一组。
const CLOSE_HOTKEY_REPEAT_WINDOW: Duration = Duration::from_millis(1100);

/// 这次关闭快捷键该不该吞掉。
///
/// 确认框开着时全局快捷键一律让路,按住不放的连发天然被挡住;不弹框之后这道闸没了,
/// 而键盘焦点不在被关的终端上时(文件树、另一个页签……)绑定每次连发都还能命中,
/// 按久一点就把项目里的终端一组一组全关掉。action 拿不到 `is_held`,只能按间隔判;
/// `last` 每次连发都刷新,所以按多久都只算一次。
///
/// `target_dead`:这一下要关的那组终端全都已退出 —— 偏好开着时它们也免确认(见
/// [`close_needs_prompt`]),于是同一道闸要管:否则按住不放会把一串已退出的组连同
/// 它们的回滚一起关掉,直到撞上一个活的才被确认框拦住。
pub fn swallows_close_hotkey(
    config: &AppConfig,
    target_dead: bool,
    last: Option<Instant>,
    now: Instant,
) -> bool {
    swallows_repeat(config, target_dead, last, now, CLOSE_HOTKEY_REPEAT_WINDOW)
}

/// 两次点 × 的间隔短于它,后一次当成连点。要盖住手快的双击(150~300ms),
/// 又不能长到挨个关 tab 时觉得点不动。
const CLOSE_CLICK_REPEAT_WINDOW: Duration = Duration::from_millis(400);

/// 这次点 × 该不该吞掉。
///
/// 关掉一个 tab 后右边的页签立刻补位,下一个 × 正好滑到鼠标底下;关掉一组后旁边的组
/// 铺过来,它的 × 也可能落到同一处。连点的第二下就会把相邻终端连同里面的进程关掉。
/// 确认框开着时第二下落在遮罩上,没有这个问题;不弹框之后才要这道闸。
/// `last` 每次点击都刷新(吞掉的也算),所以连点多少下都只关一个。
///
/// `target_dead`:这一下点的终端(整组 × 则是组内全部)已经退出。已退出的终端偏好开着
/// 也免确认,连点的第二下要是落在补位过来的另一个已退出终端上,就会无声地把它连同回滚
/// 一起关掉 —— 那正是用户想回看的东西。所以这时偏好开着也照样吞;第二下落在**活的**
/// 终端上则放行,由确认框把关(与偏好开着时一贯的口径一致)。
pub fn swallows_close_click(
    config: &AppConfig,
    target_dead: bool,
    last: Option<Instant>,
    now: Instant,
) -> bool {
    swallows_repeat(config, target_dead, last, now, CLOSE_CLICK_REPEAT_WINDOW)
}

/// 闸只管「这一下会不弹框直接关」的情形:偏好关着,或目标已退出。
fn swallows_repeat(
    config: &AppConfig,
    target_dead: bool,
    last: Option<Instant>,
    now: Instant,
    window: Duration,
) -> bool {
    (!should_confirm_close(config) || target_dead)
        && last.is_some_and(|last| now.duration_since(last) < window)
}

/// 确认框的文案。不弹框时用不上,但算一份没有代价,不值得为此把入口写成两段。
#[derive(Debug, PartialEq, Eq)]
struct ClosePrompt {
    title: &'static str,
    message: String,
    /// 正文下面的灰色补充行(关整组时列出会被杀的 AI 终端)。
    detail: Vec<String>,
    /// 要关的终端里有 AI 手上有事,见 [`close_needs_prompt`]。
    ai_busy: bool,
    /// 要关的终端 PTY 全都已死(关整组时须组内全部),见 [`close_needs_prompt`]。
    dead: bool,
}

/// 关一个 tab 的文案:带 pane 名,有活着的 AI 时换成「关闭 AI 对话」那一档。
fn pane_close_prompt(pane: &PaneState) -> ClosePrompt {
    let label = pane.label().to_string();
    let (title, message) = if is_ai_alive(pane.status) {
        (
            t("paneGroup", "closeAiTitle"),
            tr!("paneGroup", "closeTabAiMessage", label = label),
        )
    } else {
        (
            t("paneGroup", "closeTerminalTitle"),
            tr!("paneGroup", "closeTabMessage", label = label),
        )
    };
    ClosePrompt {
        title,
        message,
        detail: Vec::new(),
        ai_busy: is_ai_busy(pane),
        dead: is_dead(pane),
    }
}

/// 关整组的文案;组是空的返回 `None`(什么都不用做)。
///
/// 名字列在灰色补充行里 —— 正文的口径(个数)与原版一字不差,
/// 「哪几个会被杀」是关整组时最想知道的,不改文案也能给出来。
fn group_close_prompt(panes: &[PaneState]) -> Option<ClosePrompt> {
    if panes.is_empty() {
        return None;
    }
    let detail = ai_session_labels(panes);
    let (title, message) = if detail.is_empty() {
        (
            t("paneGroup", "closeTerminalTitle"),
            t("paneGroup", "closeGroupMessage").to_string(),
        )
    } else {
        (
            t("paneGroup", "closeAiTitle"),
            tr!("paneGroup", "closeGroupAiMessage", count = detail.len()),
        )
    };
    Some(ClosePrompt {
        title,
        message,
        detail,
        ai_busy: panes.iter().any(is_ai_busy),
        dead: all_dead(panes),
    })
}

/// 关同一组里的**几个** tab(「关闭其他 / 关闭右侧」)的文案;没有要关的返回 `None`。
///
/// 盘点、标题、补充行、两条免确认判据都与关整组同一份([`group_close_prompt`]),
/// 只有正文换掉:「该区域内所有终端」不对 —— 右键的那个留着。AI 那一档同时给出总数与
/// AI 个数,一眼看得出「关掉的大半是普通 shell」还是「几乎全是 AI」。
fn batch_close_prompt(panes: &[PaneState]) -> Option<ClosePrompt> {
    let mut prompt = group_close_prompt(panes)?;
    prompt.message = if prompt.detail.is_empty() {
        tr!("paneGroup", "closeSomeMessage", count = panes.len())
    } else {
        tr!(
            "paneGroup",
            "closeSomeAiMessage",
            total = panes.len(),
            count = prompt.detail.len()
        )
    };
    Some(prompt)
}

/// 页签右键「关闭其他终端 / 关闭右侧终端」的范围。作用域都是右键那个 tab 所在的**组**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabCloseScope {
    /// 同组里除它以外的全部。
    Others,
    /// 同组里排在它右边的全部。
    ToRight,
}

/// 一组 tab(按 tab 顺序)里,`anchor` 之外该关的那些。`anchor` 不在这组里就一个都不关。
fn scope_targets(panes: &[PaneState], anchor: &str, scope: TabCloseScope) -> Vec<PaneState> {
    let Some(at) = panes.iter().position(|p| p.id == anchor) else {
        return Vec::new();
    };
    match scope {
        TabCloseScope::Others => panes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != at)
            .map(|(_, p)| p.clone())
            .collect(),
        TabCloseScope::ToRight => panes[at + 1..].to_vec(),
    }
}

/// `pane_id` 所在那一组里,这个范围要关哪些 tab(菜单项置灰与关闭动作共用这一份)。
pub fn tab_close_targets(
    store: &AppStore,
    project_id: &str,
    pane_id: &str,
    scope: TabCloseScope,
) -> Vec<PaneState> {
    match store
        .project_state(project_id)
        .and_then(|s| s.leaf_of_pane(pane_id))
    {
        Some(SplitNode::Leaf { panes, .. }) => scope_targets(panes, pane_id, scope),
        _ => Vec::new(),
    }
}

/// 「关闭其他终端 / 关闭右侧终端」:走与关整组同一道闸([`close_after_prompt`]),
/// 要问就**只问一次**,把要关的几个一起列出来;不逐个弹框。
///
/// 关的是**确认那一刻列出来的那几个**(按 id 从最新布局关):确认框开着的时候又新开了
/// tab 不会被捎带关掉,期间已经被别处关掉的 id 是空操作。右键的那个留着;原先激活的
/// tab 被关掉时,组内激活位按树的规则落到剩下的最后一个上(关右侧时就是它自己)。
pub fn close_tabs(
    store: Entity<AppStore>,
    project_id: String,
    pane_id: String,
    scope: TabCloseScope,
    window: &mut Window,
    cx: &mut App,
) {
    let targets = tab_close_targets(store.read(cx), &project_id, &pane_id, scope);
    let Some(prompt) = batch_close_prompt(&targets) else {
        return;
    };
    let ids: Vec<String> = targets.into_iter().map(|p| p.id).collect();
    close_after_prompt(
        store,
        prompt,
        move |store, cx| {
            store.remember_closed_panes(&project_id, &ids);
            for id in &ids {
                store.close_pane(&project_id, id, cx);
            }
        },
        window,
        cx,
    );
}

/// 关掉之后要不要把键盘焦点交给接手的终端:关之前焦点在某个终端里(`focused_before`),
/// 而那个终端这一关就没了(`still_alive == false`)。
///
/// 焦点原本在别处(文件树、项目列表、另一个没被关的终端)一律不动 —— 关的是别的组,
/// 用户手上的事不该被打断。
fn focus_follows_close(focused_before: Option<u32>, still_alive: bool) -> bool {
    focused_before.is_some() && !still_alive
}

/// 关掉之后把键盘焦点交给接手的终端(见 [`focus_follows_close`])。
///
/// 接手的是活动项目里逻辑焦点所在的 pane —— `AppStore::close_pane` 刚把逻辑焦点交到它
/// 手上(同组眼前那个 tab,整组没了就是相邻那一组)。从前这里什么都不做:被关的终端
/// 连同它的焦点句柄一起没了,键盘焦点落空,打字直接丢、Workspace 的快捷键全不响应,
/// 要拿鼠标点一下才回来。项目里一个终端都不剩时这里交不出去,由 Workspace 的「焦点落空」
/// 兜底接住(落到终端区的兜底焦点上,见 `TerminalArea::restore_lost_focus`)。
fn hand_focus_after_close(
    store: &Entity<AppStore>,
    focused_before: Option<u32>,
    window: &mut Window,
    cx: &mut App,
) {
    let alive = focused_before.is_some_and(|pty_id| store.read(cx).terminal(pty_id).is_some());
    if !focus_follows_close(focused_before, alive) {
        return;
    }
    let target = {
        let store = store.read(cx);
        store
            .active_project_id
            .clone()
            .and_then(|project_id| Some((store.active_pane_id(&project_id)?, project_id)))
    };
    if let Some((pane_id, project_id)) = target {
        store.update(cx, |store, cx| {
            store.focus_pane(&project_id, &pane_id, window, cx)
        });
    }
}

/// 关终端的**唯一闸**:按 [`close_needs_prompt`] 弹确认框或直接关,两条路跑同一个 `do_close`,
/// 关完同一处交接键盘焦点([`hand_focus_after_close`])。
///
/// 不弹框时也要 `window.defer`:调用方多半是 `cx.listener` 里(tab 的 ×、控制条、
/// Ctrl+Shift+W),`TerminalArea` / `Workspace` 正被 update 着。确认框那条路的
/// `do_close` 是在弹窗的 action 里跑的,没有这层租约;这里排到本轮 effect 之后,
/// 两条路才是真的同一种调用环境,不必去推敲 `AppStore::close_*` 会不会读回它们。
///
/// 「焦点在哪个终端里」**在入口处就记下**:弹了确认框之后焦点在框上,等到确认那一刻再看
/// 就分不清了。确认框取消不走 `do_close`,焦点由弹窗还回原处,这里一下都不碰。
fn close_after_prompt(
    store: Entity<AppStore>,
    prompt: ClosePrompt,
    do_close: impl Fn(&mut AppStore, &mut Context<AppStore>) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let focused_before = store.read(cx).focused_terminal(window, cx);
    if !close_needs_prompt(store.read(cx).config(), prompt.ai_busy, prompt.dead) {
        window.defer(cx, move |window, cx| {
            store.update(cx, |store, cx| do_close(store, cx));
            hand_focus_after_close(&store, focused_before, window, cx);
        });
        return;
    }
    let mut confirm = Confirm::new(prompt.title, prompt.message).detail(prompt.detail);
    // AI 手上有事时偏好关着也会弹这个框,勾「不再询问」并不能让它不再出现
    if !prompt.ai_busy {
        let store_for_opt_out = store.clone();
        confirm = confirm.dont_ask_again(
            t("paneGroup", "closeDontAskAgain"),
            t("paneGroup", "closeDontAskHint"),
            move |cx| {
                store_for_opt_out.update(cx, |store, cx| {
                    store.patch_config(|c| c.confirm_terminal_close = false, cx);
                });
            },
        );
    }
    confirm.open(
        move |window, cx| {
            store.update(cx, |store, cx| do_close(store, cx));
            // 弹窗随后(关闭动画走完)会把焦点「还」给打开它之前的那个句柄 —— 正是刚关掉
            // 的终端,还不回去就是一次焦点落空,由 Workspace 的兜底再交回接手的终端。
            // 这里先交一次,确认之后立刻就能打字
            hand_focus_after_close(&store, focused_before, window, cx);
        },
        window,
        cx,
    );
}

/// 关闭一个终端 tab。偏好开着时**总是**先确认(与原版一致:没有 AI 也要问一句);
/// 偏好关着时只有 AI 手上有事才问。PTY 已经退出的不问(见 [`close_needs_prompt`])。
pub fn close_pane(
    store: Entity<AppStore>,
    project_id: String,
    pane_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(prompt) = store
        .read(cx)
        .project_state(&project_id)
        .and_then(|s| s.pane(&pane_id))
        .map(pane_close_prompt)
    else {
        return;
    };
    close_after_prompt(
        store,
        prompt,
        move |store, cx| {
            // 按 id 从**最新**布局关(不是拿确认前那份快照)—— 确认框开着的这段
            // 时间里 pane 可能刚拿到 pty_id,用旧快照会漏掉回收(原版同一条注释)
            store.remember_closed_panes(&project_id, std::slice::from_ref(&pane_id));
            store.close_pane(&project_id, &pane_id, cx);
        },
        window,
        cx,
    );
}

/// 关闭某个 pane **所在的整组**(Ctrl+Shift+W / 右键「关闭整个区域」的落点)。
pub fn close_leaf_of_pane(
    store: Entity<AppStore>,
    project_id: String,
    pane_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(leaf_id) = store
        .read(cx)
        .project_state(&project_id)
        .and_then(|s| s.leaf_of_pane(&pane_id))
        .map(|node| node.id().to_string())
    else {
        return;
    };
    let Some(prompt) = group_close_prompt(&leaf_panes(store.read(cx), &project_id, &leaf_id))
    else {
        return;
    };
    close_after_prompt(
        store,
        prompt,
        move |store, cx| {
            // 确认之后**按 pane id 重新定位叶子**:这段时间里可能又分了一次屏,
            // 叶子 id 会变(insert_split 把原叶子换成 split 的一个子节点)
            let ids = store.leaf_pane_ids(&project_id, &pane_id);
            store.remember_closed_panes(&project_id, &ids);
            store.close_leaf_of_pane(&project_id, &pane_id, cx);
        },
        window,
        cx,
    );
}

/// 关闭一整个分屏格(它的全部 tab)—— 调用方手上已经有 leaf id 的那一路。
pub fn close_leaf(
    store: Entity<AppStore>,
    project_id: String,
    leaf_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(prompt) = group_close_prompt(&leaf_panes(store.read(cx), &project_id, &leaf_id))
    else {
        return;
    };
    close_after_prompt(
        store,
        prompt,
        move |store, cx| {
            let ids: Vec<String> = leaf_panes(store, &project_id, &leaf_id)
                .into_iter()
                .map(|p| p.id)
                .collect();
            store.remember_closed_panes(&project_id, &ids);
            store.close_leaf(&project_id, &leaf_id, cx);
        },
        window,
        cx,
    );
}

/// 关闭一整个项目级面板(它的全部 pane,含所有分屏格)。
/// 确认口径与关整组一致 —— 盘点面板里活着的 AI 会话。
pub fn close_panel(
    store: Entity<AppStore>,
    project_id: String,
    panel_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let panes: Vec<PaneState> = store
        .read(cx)
        .project_state(&project_id)
        .and_then(|s| s.panels.iter().find(|p| p.id == panel_id))
        .map(|p| p.layout.panes().into_iter().cloned().collect())
        .unwrap_or_default();
    let Some(prompt) = group_close_prompt(&panes) else {
        return;
    };
    close_after_prompt(
        store,
        prompt,
        move |store, cx| {
            let ids: Vec<String> = store
                .project_state(&project_id)
                .and_then(|s| s.panels.iter().find(|p| p.id == panel_id))
                .map(|p| p.layout.panes().into_iter().map(|x| x.id.clone()).collect())
                .unwrap_or_default();
            store.remember_closed_panes(&project_id, &ids);
            store.close_panel(&project_id, &panel_id, cx);
        },
        window,
        cx,
    );
}

/// 把 pane 里跑着的 AI 会话**分支到新分屏**(`paneActions.ts::forkPaneSession`)。
///
/// ```text
/// pane 的 ai_session ──→ 能力位查命令(claude --resume … --fork-session / codex fork …)
///                        └─ 拼不出(无身份 / grok / 坏 id)→ 什么都不做
/// 后台线程:resolve_fork_cwd(会话 cwd 预检 → claude 系反查 → None)
///     ↓ 回主线程
/// split_pane_with_cwd(横向) → register_pending_fork(新 ptyId) → 写 fork 命令
/// ```
///
/// 原 pane 的原会话继续跑;右侧分出来的新 pane 起一个新进程跑 fork 出来的会话。
/// PTY 内核缓冲 stdin,shell 就绪前写入不丢(与重启自动续接同一时序)。
/// **新进程 = 新权限上下文**:原会话里「本会话允许」的授权不迁移(CLI 官方行为)。
///
/// 登记必须在写命令**之前** —— hook 可能在命令刚回车就把新会话身份报上来,
/// 晚一步这条 child→parent 边就永远记不上了(Claude 的 CLI fork 不写磁盘指针,
/// 自记账是唯一来源)。
pub fn fork_pane_session(
    store: Entity<AppStore>,
    project_id: String,
    pane_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(session) = store
        .read(cx)
        .project_state(&project_id)
        .and_then(|s| s.pane(&pane_id))
        .and_then(|p| p.ai_session.clone())
    else {
        return;
    };
    // 命令与归一化 agent 都由菜单那份判据产出 —— 菜单出得来的项,动作就跑得通
    let BranchMenuSegment::Fork { command, agent, .. } = branch_menu_segment(Some(&session), None)
    else {
        return;
    };
    let parent_session_id = session.session_id.clone();

    window
        .spawn(cx, async move |cx| {
            // 反查是**同步磁盘遍历**(`~/.claude/projects` 逐桶找文件),
            // 落在 GPUI 主线程上就是整个窗口卡住
            let cwd = cx
                .background_executor()
                .spawn(async move { resolve_fork_cwd(&session) })
                .await;
            let _ = cx.update(|window, cx| {
                store.update(cx, |store, cx| {
                    let Some(new_pane) = store.split_pane_with_cwd(
                        &project_id,
                        &pane_id,
                        SplitDirection::Horizontal,
                        cwd,
                        window,
                        cx,
                    ) else {
                        // 源 pane 在这期间被关掉了 —— 分不出屏,什么都不做
                        // (原版那条「带 cwd 失败就不带 cwd 重试」在这里是死路:
                        // GPUI 侧 spawn_pane 从不因目录失败而返回 None,
                        // 起不来的 PTY 会以错误文本留在 pane 里,见 `start_pty`)
                        return;
                    };
                    let pty_id = store
                        .project_state(&project_id)
                        .and_then(|s| s.pane(&new_pane))
                        .and_then(|p| p.pty_id);
                    if let Some(pty_id) = pty_id {
                        store.register_pending_fork(pty_id, &agent, &parent_session_id);
                    }
                    store.write_to_pane(&project_id, &new_pane, &format!("{command}\r"), cx);
                });
            });
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 「只有一个可选项就别弹菜单」那道闸:接入 AI 启动器后必须**连启动器一起算**。
    /// 只按 shell 数判的话,单 shell 用户永远看不到启动器段 —— 而那正是这次要给
    /// 他们的入口。因为启动器读口径缺省含预置两条,实际效果是所有单 shell 用户
    /// 点「+」都会弹菜单(远程项目除外,那边启动器为空)。
    #[test]
    fn single_option_gate_counts_launchers_too() {
        // 一个 shell、零启动器:只有一条路,直接开(原行为;也是远程项目的情形)
        assert!(!should_show_new_terminal_menu(1, 0));
        // 一个 shell、一条启动器:有得选了,必须弹
        assert!(should_show_new_terminal_menu(1, 1));
        // 预置两条启动器是缺省状态 —— 单 shell 用户实际落在这一档
        assert!(should_show_new_terminal_menu(1, 2));
        // 多 shell 照旧弹
        assert!(should_show_new_terminal_menu(2, 0));
        // 一条 shell 都没有(配置损坏)也不弹:调用方那条分支会回落默认 shell
        assert!(!should_show_new_terminal_menu(0, 0));
    }

    /// 远程项目不出启动器段:ssh 启动初期停在口令/host key 交互上时,
    /// 预写的 `{命令}\r` 会被当口令消费 —— 判据与 `hydrate_project` 的自动续接
    /// 守卫同源。WSL 不挡(本地直接起 wsl.exe,没有那段交互)。
    #[test]
    fn launcher_section_hidden_for_remote_projects_only() {
        assert!(project_allows_launchers(None));
        assert!(!project_allows_launchers(Some("conn-1")));
    }

    fn pane(label: &str, status: PaneStatus) -> PaneState {
        let mut p = PaneState::new(label);
        p.status = status;
        p
    }

    fn opted_out_config() -> AppConfig {
        AppConfig {
            confirm_terminal_close: false,
            ..Default::default()
        }
    }

    /// 没选过的人照旧确认;选了不再询问后只有 AI 手上有事还要问。
    #[test]
    fn 不再询问后只有_ai_手上有事还要确认() {
        let confirming = AppConfig::default();
        assert!(close_needs_prompt(&confirming, false, false));
        assert!(close_needs_prompt(&confirming, true, false));

        let opted_out = opted_out_config();
        assert!(!close_needs_prompt(&opted_out, false, false));
        assert!(close_needs_prompt(&opted_out, true, false));
    }

    /// PTY 已退出 / 起不来的终端关掉不终止任何东西:偏好开着也直接关。
    /// 干净退出(灰灯)与异常退出(红灯)一样算死 —— 判据看状态,不看显示层。
    #[test]
    fn 已退出的终端关闭免确认() {
        let confirming = AppConfig::default();
        assert!(!close_needs_prompt(&confirming, false, true));

        let crashed = pane("pwsh", PaneStatus::Error);
        let prompt = pane_close_prompt(&crashed);
        assert!(prompt.dead);
        assert!(!close_needs_prompt(
            &confirming,
            prompt.ai_busy,
            prompt.dead
        ));

        let mut clean = pane("pwsh", PaneStatus::Error);
        clean.clean_exit = true;
        assert!(pane_close_prompt(&clean).dead, "灰灯也是死的");

        // 活着的照旧问
        let alive = pane_close_prompt(&pane("pwsh", PaneStatus::Idle));
        assert!(!alive.dead);
        assert!(close_needs_prompt(&confirming, alive.ai_busy, alive.dead));
    }

    /// 关整组:组内**全部**已退出才免确认,混着一个活的就照常问。
    #[test]
    fn 整组全部已退出才免确认() {
        let all = group_close_prompt(&[
            pane("bash", PaneStatus::Error),
            pane("cmd", PaneStatus::Error),
        ])
        .unwrap();
        assert!(all.dead);
        assert!(!close_needs_prompt(
            &AppConfig::default(),
            all.ai_busy,
            all.dead
        ));

        let mixed = group_close_prompt(&[
            pane("bash", PaneStatus::Error),
            pane("pwsh", PaneStatus::Idle),
        ])
        .unwrap();
        assert!(!mixed.dead);
        assert!(close_needs_prompt(
            &AppConfig::default(),
            mixed.ai_busy,
            mixed.dead
        ));

        assert!(!all_dead(&[]), "空组不算全部已退出");
    }

    fn attention_pane(label: &str, status: PaneStatus) -> PaneState {
        let mut p = pane(label, status);
        p.attention = true;
        p
    }

    /// 正在工作或亮黄灯才算手上有事:Codex 等授权是 ai-working + attention,
    /// Claude 等授权是 ai-idle + attention。闲着的 ai-idle 偏好关着时照样直接关;
    /// 死掉的终端残留 attention 也不算,它不亮黄灯。
    #[test]
    fn 单个_tab_正在工作或亮黄灯才算手上有事() {
        for (status, attention, expect) in [
            (PaneStatus::AiWorking, false, true),
            (PaneStatus::AiWorking, true, true),
            (PaneStatus::AiIdle, true, true),
            (PaneStatus::AiIdle, false, false),
            (PaneStatus::Idle, false, false),
            (PaneStatus::Idle, true, false),
            (PaneStatus::Error, false, false),
            (PaneStatus::Error, true, false),
        ] {
            let mut p = pane("pi", status);
            p.attention = attention;
            assert_eq!(
                pane_close_prompt(&p).ai_busy,
                expect,
                "{status:?} attention={attention}"
            );
        }
    }

    /// 关整组 / 关面板:组里只要有一个 AI 手上有事就算,否则它会被一并杀掉而没人问。
    #[test]
    fn 整组里有一个_ai_手上有事就算() {
        let working = group_close_prompt(&[
            pane("bash", PaneStatus::Idle),
            pane("pi", PaneStatus::AiWorking),
        ])
        .unwrap();
        assert!(working.ai_busy);

        let waiting = group_close_prompt(&[
            pane("bash", PaneStatus::Idle),
            attention_pane("claude", PaneStatus::AiIdle),
        ])
        .unwrap();
        assert!(waiting.ai_busy);

        let idle = group_close_prompt(&[
            pane("bash", PaneStatus::Idle),
            pane("codex", PaneStatus::AiIdle),
        ])
        .unwrap();
        assert!(!idle.ai_busy);
    }

    /// 单个 tab 的文案分档跟着 AI 判据走,偏好没有掺和进去。
    #[test]
    fn 单个_tab_的文案按_ai_分档() {
        let ai = pane_close_prompt(&pane("pwsh", PaneStatus::AiWorking));
        assert_eq!(ai.title, t("paneGroup", "closeAiTitle"));
        assert!(ai.message.contains("pwsh"));
        assert!(ai.detail.is_empty());

        let plain = pane_close_prompt(&pane("pwsh", PaneStatus::Idle));
        assert_eq!(plain.title, t("paneGroup", "closeTerminalTitle"));
        assert!(plain.message.contains("pwsh"));

        // 输入检测认出的 AI 同样是 ai-idle,不要求 hook 身份
        let idle_ai = pane_close_prompt(&pane("bash", PaneStatus::AiIdle));
        assert_eq!(idle_ai.title, t("paneGroup", "closeAiTitle"));
    }

    /// 整组汇总成一个确认框:AI 终端的个数进正文、名字进补充行;空组什么都不做。
    #[test]
    fn 整组汇总成一个确认框() {
        assert_eq!(group_close_prompt(&[]), None);

        let plain = group_close_prompt(&[pane("bash", PaneStatus::Idle)]).unwrap();
        assert_eq!(plain.title, t("paneGroup", "closeTerminalTitle"));
        assert!(plain.detail.is_empty());

        let mixed = group_close_prompt(&[
            pane("bash", PaneStatus::Idle),
            pane("pi", PaneStatus::AiWorking),
            pane("codex", PaneStatus::AiIdle),
        ])
        .unwrap();
        assert_eq!(mixed.title, t("paneGroup", "closeAiTitle"));
        assert!(mixed.message.contains('2'));
        assert_eq!(mixed.detail, vec!["pi".to_string(), "codex".to_string()]);
    }

    fn ids(panes: &[PaneState]) -> Vec<&str> {
        panes.iter().map(|p| p.id.as_str()).collect()
    }

    /// 关闭其他 = 组里除它以外全部;关闭右侧 = 排在它右边的全部。顺序保持 tab 顺序;
    /// 不在这组里、或已经是最右 / 唯一一个时没有可关的(菜单据此置灰)。
    #[test]
    fn 关闭其他与关闭右侧的范围() {
        let panes: Vec<PaneState> = ["a", "b", "c", "d"]
            .into_iter()
            .map(|name| pane(name, PaneStatus::Idle))
            .collect();
        let all = ids(&panes);
        let (a, b, d) = (all[0], all[1], all[3]);

        let others = scope_targets(&panes, b, TabCloseScope::Others);
        assert_eq!(ids(&others), [all[0], all[2], all[3]]);
        let right = scope_targets(&panes, b, TabCloseScope::ToRight);
        assert_eq!(ids(&right), [all[2], all[3]]);
        assert_eq!(
            ids(&scope_targets(&panes, a, TabCloseScope::ToRight)),
            [all[1], all[2], all[3]],
            "最左那个关右侧 = 其余全部"
        );

        assert!(
            scope_targets(&panes, d, TabCloseScope::ToRight).is_empty(),
            "最右的没有右侧"
        );
        assert!(
            scope_targets(&panes[..1], a, TabCloseScope::Others).is_empty(),
            "只剩它自己"
        );
        assert!(scope_targets(&panes, "不在这组", TabCloseScope::Others).is_empty());
    }

    /// 批量关闭只弹一个框:标题、AI 名单、两条免确认判据与关整组同一份,正文说的是「这几个」。
    #[test]
    fn 批量关闭汇总成一个确认框() {
        assert_eq!(batch_close_prompt(&[]), None);

        let plain = batch_close_prompt(&[
            pane("bash", PaneStatus::Idle),
            pane("cmd", PaneStatus::Idle),
        ])
        .unwrap();
        assert_eq!(plain.title, t("paneGroup", "closeTerminalTitle"));
        assert!(plain.message.contains('2'));
        assert_ne!(
            plain.message,
            t("paneGroup", "closeGroupMessage"),
            "右键那个留着,不是全部"
        );
        assert!(plain.detail.is_empty());

        let mixed = batch_close_prompt(&[
            pane("bash", PaneStatus::Idle),
            pane("pi", PaneStatus::AiWorking),
            pane("cmd", PaneStatus::Idle),
        ])
        .unwrap();
        assert_eq!(mixed.title, t("paneGroup", "closeAiTitle"));
        assert!(mixed.message.contains('3') && mixed.message.contains('1'));
        assert_eq!(mixed.detail, vec!["pi".to_string()]);
        assert!(mixed.ai_busy, "AI 正在干活:偏好关着也要问");

        let dead = batch_close_prompt(&[
            pane("bash", PaneStatus::Error),
            pane("cmd", PaneStatus::Error),
        ])
        .unwrap();
        assert!(dead.dead, "全部已退出:免确认");
        assert!(!close_needs_prompt(
            &AppConfig::default(),
            dead.ai_busy,
            dead.dead
        ));
    }

    /// 关掉的正是拿着键盘焦点的终端才交接;焦点在别处(文件树、没被关的终端)不动。
    #[test]
    fn 只有关掉拿着焦点的终端才交接键盘焦点() {
        assert!(
            focus_follows_close(Some(3), false),
            "焦点所在的终端被关了:交给接手的"
        );
        assert!(
            !focus_follows_close(Some(3), true),
            "焦点所在的终端还在(关的是别的组 / 别的 tab):不动"
        );
        assert!(
            !focus_follows_close(None, false),
            "焦点原本不在终端里(文件树、项目列表):不抢"
        );
    }

    /// 按住 Ctrl+Shift+W 不放:首次连发隔 500ms~1s 才来、之后几十毫秒一次,一律吞掉;
    /// 松手隔够了能再关。确认框开着的人走不到这条(弹框本身就挡住了连发)。
    #[test]
    fn 按住关闭快捷键只算一次() {
        let config = opted_out_config();
        let start = Instant::now();
        assert!(
            !swallows_close_hotkey(&config, false, None, start),
            "第一次按下不吞"
        );
        for ms in [33, 500, 1000] {
            let now = start + Duration::from_millis(ms);
            assert!(
                swallows_close_hotkey(&config, false, Some(start), now),
                "{ms}ms"
            );
        }
        let later = start + CLOSE_HOTKEY_REPEAT_WINDOW;
        assert!(!swallows_close_hotkey(&config, false, Some(start), later));

        let confirming = AppConfig::default();
        let soon = start + Duration::from_millis(33);
        assert!(
            !swallows_close_hotkey(&confirming, false, Some(start), soon),
            "确认开着时不归这道闸管"
        );
        // 目标组全部已退出:偏好开着也免确认,于是按住不放同样只关一组
        assert!(swallows_close_hotkey(&confirming, true, Some(start), soon));
    }

    /// 连点 × 的第二下落在补位过来的相邻 tab 上,要吞掉;隔够了是在挨个关,照常放行。
    /// 确认开着时第二下落在遮罩上,不归这道闸管。
    #[test]
    fn 连点关闭按钮只算一次() {
        let config = opted_out_config();
        let start = Instant::now();
        assert!(
            !swallows_close_click(&config, false, None, start),
            "第一次点击不吞"
        );
        for ms in [150, 200, 399] {
            let now = start + Duration::from_millis(ms);
            assert!(
                swallows_close_click(&config, false, Some(start), now),
                "{ms}ms"
            );
        }
        let later = start + CLOSE_CLICK_REPEAT_WINDOW;
        assert!(!swallows_close_click(&config, false, Some(start), later));

        let confirming = AppConfig::default();
        let soon = start + Duration::from_millis(150);
        assert!(
            !swallows_close_click(&confirming, false, Some(start), soon),
            "确认开着时不归这道闸管"
        );
    }

    /// 已退出的终端偏好开着也免确认:连点的第二下落在补位过来的另一个已退出终端上要吞掉
    /// (否则它的回滚无声无息就没了);落在活的终端上放行,由确认框把关。
    #[test]
    fn 已退出终端免确认时连点同样只算一次() {
        let confirming = AppConfig::default();
        let start = Instant::now();
        let soon = start + Duration::from_millis(150);
        assert!(swallows_close_click(&confirming, true, Some(start), soon));
        assert!(
            !swallows_close_click(&confirming, true, None, start),
            "第一次点击不吞"
        );
        assert!(!swallows_close_click(
            &confirming,
            true,
            Some(start),
            start + CLOSE_CLICK_REPEAT_WINDOW
        ));
        // 偏好关着时本来就吞,目标死活不影响
        assert!(swallows_close_click(
            &opted_out_config(),
            true,
            Some(start),
            soon
        ));
    }

    /// 「活着的 AI」只认两个状态 —— idle 与 error 都不算。
    #[test]
    fn ai_存活判据只认两态() {
        assert!(is_ai_alive(PaneStatus::AiWorking));
        assert!(is_ai_alive(PaneStatus::AiIdle));
        assert!(!is_ai_alive(PaneStatus::Idle));
        // shell 退出(error)不该让关闭确认框变成「有 AI 在跑」
        assert!(!is_ai_alive(PaneStatus::Error));
    }

    /// 盘点取显示名、保持 tab 顺序、非 AI 的不进表。
    #[test]
    fn 盘点按_tab_顺序列出活着的会话() {
        let mut renamed = pane("pwsh", PaneStatus::AiWorking);
        renamed.custom_title = Some("codex 跑测试".into());
        let panes = vec![
            pane("bash", PaneStatus::Idle),
            renamed,
            pane("cmd", PaneStatus::Error),
            pane("pwsh", PaneStatus::AiIdle),
        ];
        assert_eq!(
            ai_session_labels(&panes),
            vec!["codex 跑测试".to_string(), "pwsh".to_string()]
        );
    }

    /// 一个 AI 都没有时盘点为空 —— 调用方据此走「不带名字」的那套文案。
    #[test]
    fn 没有_ai_时盘点为空() {
        let panes = vec![pane("bash", PaneStatus::Idle), pane("cmd", PaneStatus::Error)];
        assert!(ai_session_labels(&panes).is_empty());
    }

    /// 关窗口径**比关 tab 多一条 pty_id**:恢复出来还没起进程的 pane 关掉不损失
    /// 任何东西,拿它拦关窗是纯噪音。
    #[test]
    fn 关窗盘点要求_pty_已起() {
        let mut restored = pane("pwsh", PaneStatus::AiIdle);
        restored.pty_id = None;
        assert!(!counts_for_window_close(&restored), "没起过 PTY 的不算");

        let mut live = pane("pwsh", PaneStatus::AiIdle);
        live.pty_id = Some(7);
        assert!(counts_for_window_close(&live));

        // 关 tab 那条口径**不看** pty_id —— 两者有意不同,别互相同化
        assert!(is_ai_alive(restored.status));
    }

    /// 状态判据与关 tab 完全一致:只有两个 AI 态算,idle / error 都不算。
    #[test]
    fn 关窗盘点的状态判据与关_tab_同() {
        for (status, expect) in [
            (PaneStatus::AiWorking, true),
            (PaneStatus::AiIdle, true),
            (PaneStatus::Idle, false),
            (PaneStatus::Error, false),
        ] {
            let mut p = pane("pwsh", status);
            p.pty_id = Some(1);
            assert_eq!(counts_for_window_close(&p), expect, "{status:?}");
        }
    }

    /// 正文一行的拼串:`· 项目名 / 标签`,项目名为空时退成 `· 标签`;
    /// 标签取 `customTitle || shellName`。
    #[test]
    fn 关窗清单每行的拼串() {
        let mut p = pane("pwsh", PaneStatus::AiWorking);
        p.pty_id = Some(1);
        assert_eq!(window_close_line("mini-term", &p), "· mini-term / pwsh");
        assert_eq!(window_close_line("", &p), "· pwsh");

        p.custom_title = Some("codex 跑测试".into());
        assert_eq!(
            window_close_line("mini-term", &p),
            "· mini-term / codex 跑测试"
        );
    }
}
