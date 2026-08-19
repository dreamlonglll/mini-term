//! 关终端 / 关整组的**唯一入口**(带 AI 感知确认框)。
//!
//! 对应 `src/utils/paneActions.ts` 的 `closePane` / `closeLeaf`。
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

use crate::i18n::{t, tr};
use crate::prompt::Confirm;
use crate::store::AppStore;
use crate::tree::{PaneState, PaneStatus, SplitNode};

/// 这个状态算「AI 会话还活着」吗。
pub fn is_ai_alive(status: PaneStatus) -> bool {
    matches!(status, PaneStatus::AiWorking | PaneStatus::AiIdle)
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
        let Some(layout) = store
            .project_state(&project.id)
            .and_then(|s| s.layout.as_ref())
        else {
            continue;
        };
        for pane in layout.panes() {
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
        .and_then(|s| s.layout.as_ref())
        .and_then(|l| l.node(leaf_id))
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
        .and_then(|s| s.layout.as_ref())
        .and_then(|l| l.pane(&pane_id))
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
        .and_then(|s| s.layout.as_ref())
        .and_then(|l| l.leaf_of_pane(&pane_id))
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
