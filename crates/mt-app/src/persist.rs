//! 布局树 ↔ 磁盘格式(`SavedProjectLayout`)。
//!
//! 对照 `src/store.ts` 的 `serializeLayout` 与 `src/utils/layoutRestore.ts`。
//! **磁盘格式一个字都不改** —— 这份形状先后经历过两个信封:最早是
//! `config.json` 里的 `savedLayout` 字段,现在是 `layout.db` 的
//! `project_layout.layout_json`(见 `mt-layout`)。换信封时本模块一行没动,
//! 存量数据也是逐字节搬过去的。
//!
//! `tabs` 恒为 0 或 1 个元素:项目级 tab 层早已删除,数组只是历史兼容。读到多元素
//! 时把后续 tab 的 pane 平铺进保留那棵树最左侧叶子的 tab 栏(与 TS 侧同一口径),
//! 不静默吃掉用户的终端。

use mt_config::{
    AppConfig, SavedAiSession, SavedPane, SavedProjectLayout, SavedSplitNode, SavedTab,
};

use crate::tree::{AiSessionRef, PaneState, SplitDirection, SplitNode};

/// 运行时布局 → 磁盘格式。
pub fn serialize_layout(layout: Option<&SplitNode>) -> SavedProjectLayout {
    match layout {
        None => SavedProjectLayout {
            tabs: Vec::new(),
            active_tab_index: 0,
        },
        Some(node) => SavedProjectLayout {
            tabs: vec![SavedTab {
                custom_title: None,
                split_layout: serialize_node(node),
            }],
            active_tab_index: 0,
        },
    }
}

fn serialize_node(node: &SplitNode) -> SavedSplitNode {
    match node {
        SplitNode::Leaf { panes, .. } => SavedSplitNode::Leaf {
            pane: None,
            panes: panes
                .iter()
                .map(|p| SavedPane {
                    shell_name: p.shell_name.clone(),
                    cwd: p.cwd.clone(),
                    ai_session: p.ai_session.as_ref().map(|s| SavedAiSession {
                        agent: s.agent.clone(),
                        cwd: s.cwd.clone(),
                        session_id: s.session_id.clone(),
                    }),
                })
                .collect(),
        },
        // 节点 id 是运行时的(见 tree.rs 的模块注释),不落盘
        SplitNode::Split {
            direction,
            children,
            sizes,
            ..
        } => SavedSplitNode::Split {
            direction: direction.as_str().to_string(),
            children: children.iter().map(serialize_node).collect(),
            sizes: sizes.clone(),
        },
    }
}

/// 磁盘格式 → 运行时布局。shell 名对不上(用户删了某个 shell)时按
/// `defaultShell` → 列表首项回落;一个都没有则这个 pane 丢弃。
pub fn restore_layout(saved: &SavedProjectLayout, config: &AppConfig) -> Option<SplitNode> {
    let trees: Vec<SplitNode> = saved
        .tabs
        .iter()
        .filter_map(|tab| restore_node(&tab.split_layout, config))
        .collect();
    if trees.is_empty() {
        return None;
    }

    let keep = if saved.active_tab_index < trees.len() {
        saved.active_tab_index
    } else {
        0
    };
    let mut extras: Vec<PaneState> = Vec::new();
    let mut kept: Option<SplitNode> = None;
    for (i, tree) in trees.into_iter().enumerate() {
        if i == keep {
            kept = Some(tree);
        } else {
            extras.extend(tree.panes().into_iter().cloned());
        }
    }
    let mut layout = kept?;
    for pane in extras {
        // 追加到最左侧叶子的 tab 栏末尾,不动 activePaneId(与 TS 版一致)
        let active_before = active_pane_of_first_leaf(&layout);
        layout.append_pane(None, pane);
        if let Some(active) = active_before {
            restore_active(&mut layout, &active);
        }
    }
    Some(layout)
}

fn active_pane_of_first_leaf(node: &SplitNode) -> Option<String> {
    match node {
        SplitNode::Leaf { active_pane_id, .. } => Some(active_pane_id.clone()),
        SplitNode::Split { children, .. } => children.first().and_then(active_pane_of_first_leaf),
    }
}

fn restore_active(node: &mut SplitNode, pane_id: &str) {
    if let SplitNode::Leaf { active_pane_id, .. } = node {
        *active_pane_id = pane_id.to_string();
        return;
    }
    if let SplitNode::Split { children, .. } = node
        && let Some(first) = children.first_mut()
    {
        restore_active(first, pane_id);
    }
}

fn restore_node(saved: &SavedSplitNode, config: &AppConfig) -> Option<SplitNode> {
    match saved {
        SavedSplitNode::Leaf { pane, panes } => {
            // 旧格式(单 pane)兼容:`panes` 为空时看 `pane`
            let saved_panes: Vec<&SavedPane> = if panes.is_empty() {
                pane.iter().collect()
            } else {
                panes.iter().collect()
            };
            let mut restored: Vec<PaneState> = Vec::new();
            for sp in saved_panes {
                let Some(shell_name) = resolve_shell_name(&sp.shell_name, config) else {
                    continue;
                };
                let mut p = PaneState::new(shell_name);
                p.cwd = sp.cwd.clone();
                p.ai_session = sp.ai_session.as_ref().map(|s| AiSessionRef {
                    agent: s.agent.clone(),
                    session_id: s.session_id.clone(),
                    cwd: s.cwd.clone(),
                });
                // 上次退出时的 AI 会话身份 → 待续接标记(运行时派生,磁盘上没有这个
                // 字段)。`hydrate_project` 起 PTY 后据此写 resume 命令;写完只清标记、
                // **保留身份**(codex resume 不重报 SessionStart,清了第二次重启就断代)。
                //
                // 置位不看 `aiAutoResume`:标记是「这个 pane 还没续过」,开关在写命令
                // 那一刻才判(`src/utils/layoutRestore.ts` 同一口径)。
                p.resume_pending = p.ai_session.is_some();
                restored.push(p);
            }
            if restored.is_empty() {
                return None;
            }
            let active = restored[0].id.clone();
            Some(SplitNode::Leaf {
                id: crate::tree::gen_id("leaf"),
                panes: restored,
                active_pane_id: active,
            })
        }
        SavedSplitNode::Split {
            direction,
            children,
            sizes,
        } => {
            let children: Vec<SplitNode> = children
                .iter()
                .filter_map(|c| restore_node(c, config))
                .collect();
            match children.len() {
                0 => None,
                1 => children.into_iter().next(),
                n => Some(SplitNode::Split {
                    id: crate::tree::gen_id("split"),
                    direction: SplitDirection::from_str(direction),
                    sizes: if sizes.len() == n {
                        sizes.clone()
                    } else {
                        vec![100.0 / n as f64; n]
                    },
                    children,
                }),
            }
        }
    }
}

/// shell 名解析:精确匹配 → `defaultShell` → 列表首项。都没有则 `None`。
fn resolve_shell_name(name: &str, config: &AppConfig) -> Option<String> {
    config
        .available_shells
        .iter()
        .find(|s| s.name == name)
        .or_else(|| {
            config
                .available_shells
                .iter()
                .find(|s| s.name == config.default_shell)
        })
        .or_else(|| config.available_shells.first())
        .map(|s| s.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mt_config::ShellConfig;

    fn config() -> AppConfig {
        let mut c = AppConfig::default();
        c.available_shells = vec![
            ShellConfig {
                name: "PowerShell".into(),
                command: "powershell.exe".into(),
                args: None,
            },
            ShellConfig {
                name: "cmd".into(),
                command: "cmd.exe".into(),
                args: None,
            },
        ];
        c.default_shell = "PowerShell".into();
        c
    }

    fn leaf(shell: &str) -> SplitNode {
        SplitNode::leaf(PaneState::new(shell))
    }

    #[test]
    fn 单叶子往返() {
        let tree = leaf("cmd");
        let saved = serialize_layout(Some(&tree));
        assert_eq!(saved.tabs.len(), 1);
        let back = restore_layout(&saved, &config()).unwrap();
        assert_eq!(back.panes().len(), 1);
        assert_eq!(back.panes()[0].shell_name, "cmd");
    }

    #[test]
    fn 分屏树往返保留方向与尺寸() {
        let mut tree = leaf("cmd");
        let a = tree.panes()[0].id.clone();
        tree.insert_split(&a, SplitDirection::Vertical, leaf("PowerShell"));
        if let SplitNode::Split { sizes, .. } = &mut tree {
            *sizes = vec![30.0, 70.0];
        }

        let saved = serialize_layout(Some(&tree));
        let back = restore_layout(&saved, &config()).unwrap();
        let SplitNode::Split {
            direction, sizes, ..
        } = &back
        else {
            panic!("应还原成 split")
        };
        assert_eq!(*direction, SplitDirection::Vertical);
        assert_eq!(sizes, &vec![30.0, 70.0]);
        assert_eq!(back.panes().len(), 2);
    }

    #[test]
    fn 未知_shell_回落默认() {
        let saved = SavedProjectLayout {
            tabs: vec![SavedTab {
                custom_title: None,
                split_layout: SavedSplitNode::Leaf {
                    pane: None,
                    panes: vec![SavedPane {
                        shell_name: "nushell(已删)".into(),
                        cwd: None,
                        ai_session: None,
                    }],
                },
            }],
            active_tab_index: 0,
        };
        let back = restore_layout(&saved, &config()).unwrap();
        assert_eq!(back.panes()[0].shell_name, "PowerShell");
    }

    /// 旧配置的多 tab:保留 activeTabIndex 指的那棵,其余 pane 平铺进它最左叶子。
    #[test]
    fn 多_tab_旧配置合并进一棵树() {
        let mk = |name: &str| SavedTab {
            custom_title: None,
            split_layout: SavedSplitNode::Leaf {
                pane: None,
                panes: vec![SavedPane {
                    shell_name: name.into(),
                    cwd: None,
                    ai_session: None,
                }],
            },
        };
        let saved = SavedProjectLayout {
            tabs: vec![mk("cmd"), mk("PowerShell"), mk("cmd")],
            active_tab_index: 1,
        };
        let back = restore_layout(&saved, &config()).unwrap();
        assert_eq!(back.panes().len(), 3, "一个终端都不能丢");
        assert_eq!(back.panes()[0].shell_name, "PowerShell", "留的是第 1 棵");
        match &back {
            SplitNode::Leaf {
                panes,
                active_pane_id,
                ..
            } => assert_eq!(active_pane_id, &panes[0].id, "激活项不该被追加的 pane 抢走"),
            _ => panic!(),
        }
    }

    /// 旧格式的 `pane`(单数)字段仍读得进来。
    #[test]
    fn 旧格式单_pane_字段兼容() {
        let saved = SavedProjectLayout {
            tabs: vec![SavedTab {
                custom_title: None,
                split_layout: SavedSplitNode::Leaf {
                    pane: Some(SavedPane {
                        shell_name: "cmd".into(),
                        cwd: Some("D:/x".into()),
                        ai_session: None,
                    }),
                    panes: vec![],
                },
            }],
            active_tab_index: 0,
        };
        let back = restore_layout(&saved, &config()).unwrap();
        assert_eq!(back.panes()[0].shell_name, "cmd");
        assert_eq!(back.panes()[0].cwd.as_deref(), Some("D:/x"));
    }

    /// AI 会话身份随布局落盘 —— 重启后据此续接。
    #[test]
    fn 会话身份随布局往返() {
        let mut tree = leaf("cmd");
        let id = tree.panes()[0].id.clone();
        tree.pane_mut(&id).unwrap().ai_session = Some(AiSessionRef {
            agent: Some("claude".into()),
            session_id: "sess-1".into(),
            cwd: Some("D:/proj".into()),
        });
        let saved = serialize_layout(Some(&tree));
        let back = restore_layout(&saved, &config()).unwrap();
        let s = back.panes()[0].ai_session.as_ref().unwrap();
        assert_eq!(s.session_id, "sess-1");
        assert_eq!(s.agent.as_deref(), Some("claude"));
        assert_eq!(s.cwd.as_deref(), Some("D:/proj"));
    }

    /// 恢复布局时按「落盘过 ai_session」置起待续接标记;没有身份的 pane 不置位。
    ///
    /// 置位**不看** `aiAutoResume` 开关 —— 标记的语义是「这个 pane 还没续过」,
    /// 开关在写 resume 命令那一刻才判(`src/utils/layoutRestore.ts` 同一口径)。
    #[test]
    fn 恢复布局按会话身份置起待续接标记() {
        let mut tree = leaf("cmd");
        let with_session = tree.panes()[0].id.clone();
        tree.pane_mut(&with_session).unwrap().ai_session = Some(AiSessionRef {
            agent: Some("claude".into()),
            session_id: "sess-1".into(),
            cwd: Some("D:/proj".into()),
        });
        tree.append_pane(None, PaneState::new("cmd")); // 没有会话身份的那个

        let saved = serialize_layout(Some(&tree));
        // 关掉自动续接也照样置位
        let mut cfg = config();
        cfg.ai_auto_resume = Some(false);
        let back = restore_layout(&saved, &cfg).unwrap();

        let panes = back.panes();
        assert_eq!(panes.len(), 2);
        assert!(panes[0].resume_pending, "落盘过 ai_session 的 pane 要置位");
        assert!(!panes[1].resume_pending, "没有会话身份的不置位");

        // 开着开关时同样置位(置位与开关无关)
        let back = restore_layout(&saved, &config()).unwrap();
        assert!(back.panes()[0].resume_pending);
    }

    /// 待续接标记是运行时派生的,**磁盘格式一个字都不多** —— 序列化只写
    /// shellName/cwd/aiSession 三项。
    #[test]
    fn 待续接标记不进磁盘格式() {
        let mut tree = leaf("cmd");
        let id = tree.panes()[0].id.clone();
        tree.pane_mut(&id).unwrap().resume_pending = true;

        let saved = serialize_layout(Some(&tree));
        let json = serde_json::to_string(&saved).unwrap();
        assert!(!json.contains("resume"), "磁盘格式里不许出现这个字段: {json}");

        // 没有 ai_session 的 pane 转一圈回来标记必须是 false(不是被"记住"了)
        let back = restore_layout(&saved, &config()).unwrap();
        assert!(!back.panes()[0].resume_pending);
    }

    #[test]
    fn 空布局序列化为空_tabs() {
        let saved = serialize_layout(None);
        assert!(saved.tabs.is_empty());
        assert!(restore_layout(&saved, &config()).is_none());
    }
}
