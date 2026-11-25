use super::*;

// ─── tab 右键菜单 ─────────────────────────────────────────────

/// tab 右键菜单的**项序**。`None` = 分隔线。
///
/// 底子对照 `PaneGroup.tsx:336-383`(含分支那一段的条件出现);原版之后补的四项
/// (复制此终端 / 最大化 / 关闭其他 / 关闭右侧)按常见终端的习惯插进去:
/// 改名 → 复制 → 分屏 → 分支 → 最大化 → 关闭一族。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TabMenuAction {
    Rename,
    /// 「复制此终端」:同组新页签,继承 shell 与 cwd(`AppStore::duplicate_pane`)。
    Duplicate,
    SplitRight,
    SplitDown,
    /// 「分支会话到新分屏」。有会话身份 + 该 agent 有 fork 能力位时才出。
    ForkSession,
    /// 「查看会话分支」—— 悬停展开家族面板,与上一项同进同出。
    ViewSessionBranches,
    /// 「分支会话(未获会话身份…)」置灰提示。与上面两项**互斥**:
    /// 前者要有身份,它要的恰恰是没身份。
    ForkNeedsIdentity,
    /// 「最大化 / 还原」,与控制条那颗按钮同一个动作(`toggle_maximized_leaf`)。
    ToggleMaximize,
    CloseTab,
    /// 「关闭其他终端」(同组)。
    CloseOthers,
    /// 「关闭右侧终端」(同组)。
    CloseToRight,
    ClosePane,
    /// 「重新打开已关闭的终端」(项目的已关闭栈,见 `store::reopen`)。栈空时置灰。
    ReopenClosed,
}

/// 项序按分支段的两种形态展开。两个入参互斥(有身份就不会缺身份),
/// 判据在 [`branch_menu_segment`] 一处。
pub(super) fn tab_menu_actions(
    can_fork: bool,
    identity_missing: bool,
) -> Vec<Option<TabMenuAction>> {
    use TabMenuAction::*;
    let mut actions = vec![
        Some(Rename),
        Some(Duplicate),
        None,
        Some(SplitRight),
        Some(SplitDown),
    ];
    if can_fork {
        actions.extend([None, Some(ForkSession), Some(ViewSessionBranches)]);
    }
    if identity_missing {
        actions.extend([None, Some(ForkNeedsIdentity)]);
    }
    actions.extend([
        None,
        Some(ToggleMaximize),
        None,
        Some(CloseTab),
        Some(CloseOthers),
        Some(CloseToRight),
        Some(ClosePane),
        None,
        Some(ReopenClosed),
    ]);
    actions
}

/// 「重新打开已关闭的终端」菜单项:页签右键与页签栏「+」的菜单(连同空态页那颗按钮的)
/// 共用这一份。栈空时置灰而不是抽掉(菜单条目固定,肌肉记忆才稳);有可重开的时补一行
/// 说明:要重开的是哪个,以及**原进程回不来** —— 它是新开一个同 shell、同目录的终端,
/// 不是撤销关闭。
pub(crate) fn reopen_closed_menu_item(
    store: &Entity<AppStore>,
    project_id: &str,
    cx: &App,
) -> MenuEntry {
    let next = store.read(cx).next_closed_label(project_id);
    let store = store.clone();
    let pid = project_id.to_string();
    let item = MenuItem::new(t("paneGroup", "reopenClosed"))
        .shortcut(crate::hotkeys::hotkey_label("reopenClosed"))
        .disabled(next.is_none())
        .on_click(move |window, cx| {
            store.update(cx, |store, cx| {
                store.reopen_closed_pane(&pid, window, cx);
            });
        });
    match next {
        Some(label) => item
            .detail(tr!("paneGroup", "reopenClosedDetail", label = label))
            .into(),
        None => item.into(),
    }
}

/// 「最大化 / 还原」这一项现在是什么样:`None` = 用不上(置灰,标「最大化」);
/// `Some(true)` = 它那一组正铺满(标「还原」);`Some(false)` = 可以铺满。
///
/// 判据与控制条那颗按钮一致:真分了屏才有意义(`maximized_pane_id` 自带这道闸),
/// 「正铺满」看**组**不看 pane(铺满后组内切过 tab,记着的还是切换前那个 pane),
/// 与 [`AppStore::toggle_maximized_leaf`] 的口径同一份。再多一条:pane 得在活动面板的
/// 布局里 —— 终端列表竖条里右键的可能是别的面板上的 pane,铺满只作用于活动面板。
pub(super) fn maximize_menu_state(
    layout: Option<&SplitNode>,
    maximized_pane_id: Option<&str>,
    pane_id: &str,
) -> Option<bool> {
    let layout = layout?;
    if !matches!(layout, SplitNode::Split { .. }) {
        return None;
    }
    let leaf = layout.leaf_of_pane(pane_id)?.id();
    let maximized_leaf = maximized_pane_id
        .and_then(|id| layout.leaf_of_pane(id))
        .map(|node| node.id());
    Some(maximized_leaf == Some(leaf))
}

/// 组装一个 tab 的右键菜单。`label` 是它当前的显示名(重命名的默认值)。
/// `pub(crate)`:终端列表竖条([`crate::terminals_panel`])的行右键与 tab 右键
/// **必须同一份菜单** —— 同一个 pane 在两处的操作面不该有差异。
///
/// `is_active` = 它是所在分组当前激活的 tab。重命名只给当前 tab(与双击改名同一口径,
/// 见 [`super::SwitchClickGuard`]):右键别的 tab 时这一项置灰而不是抽掉 —— 菜单条目
/// 固定,肌肉记忆才稳。其余各项照旧作用于右键的那个 tab。
pub(crate) fn tab_menu(
    store: &Entity<AppStore>,
    project_id: &str,
    pane_id: &str,
    label: &str,
    is_active: bool,
    cx: &App,
) -> Vec<MenuEntry> {
    let pane_state = store
        .read(cx)
        .project_state(project_id)
        .and_then(|s| s.pane(pane_id));
    let segment = pane_state
        .map(|p| branch_menu_segment(p.ai_session.as_ref(), p.detected_agent.as_deref()))
        .unwrap_or(BranchMenuSegment::None);
    let project_path = store
        .read(cx)
        .project(project_id)
        .map(|p| p.path.clone())
        .unwrap_or_default();
    let fork_session_id = match &segment {
        BranchMenuSegment::Fork { session_id, .. } => session_id.clone(),
        _ => String::new(),
    };
    let maximize = {
        let s = store.read(cx);
        maximize_menu_state(
            s.project_state(project_id)
                .and_then(|st| st.active_layout()),
            s.maximized_pane_id(project_id),
            pane_id,
        )
    };
    // 没得可关就置灰(而不是抽掉那一项):菜单条目固定,肌肉记忆才稳(文件页签同一口径)
    let nothing_to_close = |scope| {
        pane_actions::tab_close_targets(store.read(cx), project_id, pane_id, scope).is_empty()
    };
    let no_others = nothing_to_close(pane_actions::TabCloseScope::Others);
    let no_right = nothing_to_close(pane_actions::TabCloseScope::ToRight);

    let mut entries = Vec::new();
    for action in tab_menu_actions(
        matches!(segment, BranchMenuSegment::Fork { .. }),
        segment == BranchMenuSegment::NeedsIdentity,
    ) {
        let Some(action) = action else {
            entries.push(menu::separator());
            continue;
        };
        let store = store.clone();
        let pid = project_id.to_string();
        let pane = pane_id.to_string();
        entries.push(match action {
            TabMenuAction::Rename => {
                let label = label.to_string();
                MenuItem::new(t("paneGroup", "rename"))
                    .shortcut(crate::hotkeys::hotkey_label("renamePane"))
                    .disabled(!is_active)
                    .on_click(move |window, cx| {
                        // 复用既有的重命名对话框(双击 tab 走的也是它)
                        modal::open_rename_pane(
                            store.clone(),
                            pid.clone(),
                            pane.clone(),
                            label.clone(),
                            window,
                            cx,
                        );
                    })
                    .into()
            }
            TabMenuAction::Duplicate => {
                menu::item(t("paneGroup", "duplicate"), move |window, cx| {
                    store.update(cx, |store, cx| {
                        store.duplicate_pane(&pid, &pane, window, cx);
                    });
                })
            }
            TabMenuAction::SplitRight => MenuItem::new(t("paneGroup", "splitRight"))
                .shortcut(crate::hotkeys::hotkey_label("splitRight"))
                .on_click(move |window, cx| {
                    store.update(cx, |store, cx| {
                        store.split_pane(&pid, &pane, SplitDirection::Horizontal, window, cx);
                    });
                })
                .into(),
            TabMenuAction::SplitDown => MenuItem::new(t("paneGroup", "splitDown"))
                .shortcut(crate::hotkeys::hotkey_label("splitDown"))
                .on_click(move |window, cx| {
                    store.update(cx, |store, cx| {
                        store.split_pane(&pid, &pane, SplitDirection::Vertical, window, cx);
                    });
                })
                .into(),
            // 分支三项:出不出由 `segment` 决定(项序表已经据此排好),
            // 内容与终端本体右键**同一份实现**(`branch_family` 里那三个构造器)
            TabMenuAction::ForkSession => branch_family::fork_menu_item(&store, pid, pane),
            TabMenuAction::ViewSessionBranches => branch_family::view_branches_menu_item(
                &store,
                project_path.clone(),
                fork_session_id.clone(),
            ),
            TabMenuAction::ForkNeedsIdentity => branch_family::needs_identity_menu_item(),
            TabMenuAction::ToggleMaximize => MenuItem::new(t(
                "paneGroup",
                if maximize == Some(true) {
                    "restore"
                } else {
                    "maximize"
                },
            ))
            .shortcut(crate::hotkeys::hotkey_label("toggleMaximize"))
            .disabled(maximize.is_none())
            .on_click(move |_window, cx| {
                store.update(cx, |store, cx| store.toggle_maximized_leaf(&pid, &pane, cx));
            })
            .into(),
            // 关闭一族全走 pane_actions —— 与 tab 上的 ×、Ctrl+Shift+W 同一个
            // AI 感知确认入口;批量的两项只弹一个汇总确认框
            TabMenuAction::CloseTab => MenuItem::new(t("paneGroup", "closeTab"))
                .shortcut(crate::hotkeys::hotkey_label("closeTab"))
                .on_click(move |window, cx| {
                    pane_actions::close_pane(store.clone(), pid.clone(), pane.clone(), window, cx);
                })
                .into(),
            TabMenuAction::CloseOthers => MenuItem::new(t("paneGroup", "closeOthers"))
                .disabled(no_others)
                .on_click(move |window, cx| {
                    let scope = pane_actions::TabCloseScope::Others;
                    pane_actions::close_tabs(
                        store.clone(),
                        pid.clone(),
                        pane.clone(),
                        scope,
                        window,
                        cx,
                    );
                })
                .into(),
            TabMenuAction::CloseToRight => MenuItem::new(t("paneGroup", "closeToRight"))
                .disabled(no_right)
                .on_click(move |window, cx| {
                    let scope = pane_actions::TabCloseScope::ToRight;
                    pane_actions::close_tabs(
                        store.clone(),
                        pid.clone(),
                        pane.clone(),
                        scope,
                        window,
                        cx,
                    );
                })
                .into(),
            TabMenuAction::ClosePane => MenuItem::new(t("paneGroup", "closePane"))
                .danger()
                .shortcut(crate::hotkeys::hotkey_label("closePane"))
                .on_click(move |window, cx| {
                    pane_actions::close_leaf_of_pane(
                        store.clone(),
                        pid.clone(),
                        pane.clone(),
                        window,
                        cx,
                    );
                })
                .into(),
            TabMenuAction::ReopenClosed => reopen_closed_menu_item(&store, &pid, cx),
        });
    }
    entries
}
