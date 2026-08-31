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

use gpui::{App, Entity, Window};
use mt_config::{AiLauncher, ShellConfig};

use crate::i18n::{t, tr};
use crate::menu;
use crate::prompt::Confirm;
use crate::session_branch::{BranchMenuSegment, branch_menu_segment};
use crate::store::{AppStore, resolve_fork_cwd};
use crate::tree::{PaneState, PaneStatus, SplitDirection, SplitNode};

/// 这个状态算「AI 会话还活着」吗。
pub fn is_ai_alive(status: PaneStatus) -> bool {
    matches!(status, PaneStatus::AiWorking | PaneStatus::AiIdle)
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

/// 关闭一个终端 tab。**总是**先确认(与原版一致:没有 AI 也要问一句)。
pub fn close_pane(
    store: Entity<AppStore>,
    project_id: String,
    pane_id: String,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(pane) = store
        .read(cx)
        .project_state(&project_id)
        .and_then(|s| s.pane(&pane_id))
        .cloned()
    else {
        return;
    };
    let label = pane.label().to_string();
    let has_ai = is_ai_alive(pane.status);
    let (title, message) = if has_ai {
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

    Confirm::new(title, message).open(
        move |_window, cx| {
            // 按 id 从**最新**布局关(不是拿确认前那份快照)—— 确认框开着的这段
            // 时间里 pane 可能刚拿到 pty_id,用旧快照会漏掉回收(原版同一条注释)
            store.update(cx, |store, cx| {
                store.close_pane(&project_id, &pane_id, cx);
            });
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
    let panes = leaf_panes(store.read(cx), &project_id, &leaf_id);
    confirm_close_group(
        panes,
        move |_window, cx| {
            // 确认之后**按 pane id 重新定位叶子**:这段时间里可能又分了一次屏,
            // 叶子 id 会变(insert_split 把原叶子换成 split 的一个子节点)
            store.update(cx, |store, cx| {
                store.close_leaf_of_pane(&project_id, &pane_id, cx);
            });
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
    let panes = leaf_panes(store.read(cx), &project_id, &leaf_id);
    confirm_close_group(
        panes,
        move |_window, cx| {
            store.update(cx, |store, cx| {
                store.close_leaf(&project_id, &leaf_id, cx);
            });
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
    confirm_close_group(
        panes,
        move |_window, cx| {
            store.update(cx, |store, cx| {
                store.close_panel(&project_id, &panel_id, cx);
            });
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

/// 「关整组」的确认框(两条入口共用)。组是空的就什么都不做。
fn confirm_close_group(
    panes: Vec<PaneState>,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    if panes.is_empty() {
        return;
    }
    let ai_labels = ai_session_labels(&panes);
    let (title, message) = if ai_labels.is_empty() {
        (
            t("paneGroup", "closeTerminalTitle"),
            t("paneGroup", "closeGroupMessage").to_string(),
        )
    } else {
        (
            t("paneGroup", "closeAiTitle"),
            tr!("paneGroup", "closeGroupAiMessage", count = ai_labels.len()),
        )
    };
    // 名字列在灰色补充行里 —— 正文的口径(个数)与原版一字不差,
    // 「哪几个会被杀」是关整组时最想知道的,不改文案也能给出来
    Confirm::new(title, message)
        .detail(ai_labels)
        .open(on_ok, window, cx);
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
