//! 「重新打开已关闭的终端」:每个项目一个已关闭栈(运行时、不落盘、上限
//! [`CLOSED_PANES_LIMIT`])。
//!
//! # 恢复的是什么
//!
//! **进程恢复不了** —— 关闭即回收 PTY(`AppStore::close_pane` → `dispose_terminal`),
//! shell 里的作业、回滚缓冲、正在跑的 AI 进程都随之没了。栈里记的是「怎么再开一个像它的」:
//! shell 名、cwd 覆盖、自定义名、AI 会话身份、原所在分组与组内位置。所以入口叫「重新打开」
//! 而不是「撤销」,菜单项下面那行补充说明也把这一点写明。
//!
//! # 入栈
//!
//! 只记**用户亲手关的**:单个关闭、关闭其他 / 右侧、关整组、关面板 —— 它们都经
//! `pane_actions` 的关闭入口,在确认之后、真正关闭之前调 [`AppStore::remember_closed_panes`]。
//! 删 worktree 时顺手关掉的终端、移除项目时回收的终端不入栈:前者的目录正被删掉,
//! 后者连项目都没了(`project_states` 里那一项连同这个栈一起丢掉,不必另清)。
//!
//! 批量关闭按**原顺序**入栈,栈顶是原先最左的那个 —— 逐个重开时依次插回原位
//! ([`push_closed`] 的注释有推演)。
//!
//! # 重开
//!
//! 原分组还在就插回原分组的原下标,否则落到当前活动分组末尾,活动面板也没有了就新建
//! 面板([`reopen_slot`])。shell / cwd / 自定义名照搬;AI 会话身份**只在真要续接时**
//! 带过去(「会话自动续接」开着、本地项目、agent 支持续接,判据整份复用
//! [`resolve_auto_resume_command`]),这时 pane 以「待续接」形态落进树里,由启动恢复同一条
//! [`AppStore::hydrate_project`] 起 PTY、定启动目录、写续接命令 —— 白名单校验、会话 cwd
//! 反查与「pane cwd 优先」的取值链一处不改。续接不了的(远程 / 开关关着 / agent 不支持)
//! 只恢复 shell 与目录,不留身份:留着的话 tab 上挂着品牌图标、重启后还可能被续接,
//! 而这个终端里其实没有 AI。
//!
//! cwd 覆盖指向的目录已经没了(worktree 删了)时回落项目根。查目录要碰盘(WSL 的
//! `\\wsl.localhost\…` 可能冷启动整台 VM),丢后台查完再回主线程落树;远程项目的 cwd
//! 是远端路径,本机查不了也不必查(远端 `cd … 2>/dev/null` 失败就停在家目录)。

use std::path::Path;

use gpui::{Context, Window};

use crate::tree::{AiSessionRef, PaneState, ProjectPanel, SplitNode};

use super::AppStore;
use super::pure::resolve_auto_resume_command;

/// 每个项目最多记多少个已关闭的终端。
pub const CLOSED_PANES_LIMIT: usize = 10;

/// 已关闭栈里的一项:再开一个「像它的」终端所需的全部信息。
#[derive(Clone, Debug, PartialEq)]
pub struct ClosedPane {
    /// 页签主段(关闭那一刻的显示名),只给菜单说明用。
    pub label: String,
    pub shell_name: String,
    /// 工作目录覆盖(`None` = 项目根),与 [`PaneState::cwd`] 同口径。
    pub cwd: Option<String>,
    pub custom_title: Option<String>,
    pub ai_session: Option<AiSessionRef>,
    /// 原所在分组(叶子 id)与组内下标。叶子 id 跨分屏 / 塌陷都稳定(见 `tree.rs` 的
    /// `insert_split_at` 注释),组还在就能插回原位。
    pub leaf_id: String,
    pub index: usize,
}

/// 把一批关掉的终端压进栈。`batch` 按 tab 顺序给出(单个关闭就是一项)。
///
/// **倒着压**,栈顶是这批里最左的那个:重开是逐个弹栈、各自插回原下标,
/// 从左往右插才不会互相挤位 —— 组里 `[a, x, b, c]` 关掉 a、b、c(下标 0 / 2 / 3)后
/// 剩 `[x]`,依次插 a@0 → `[a, x]`、b@2 → `[a, x, b]`、c@3 → `[a, x, b, c]`,原样还原;
/// 原组整个没了时依次追加到别的组末尾,相对顺序同样不乱。
///
/// 超过 [`CLOSED_PANES_LIMIT`] 时丢最早关的(栈底)。
pub fn push_closed(stack: &mut Vec<ClosedPane>, batch: Vec<ClosedPane>) {
    stack.extend(batch.into_iter().rev());
    let overflow = stack.len().saturating_sub(CLOSED_PANES_LIMIT);
    stack.drain(..overflow);
}

/// 重开落在哪。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReopenSlot {
    /// 插进这个叶子;`index = None` 追加到末尾(回落的分组用不上原下标)。
    Leaf {
        leaf_id: String,
        index: Option<usize>,
    },
    /// 项目一个终端都没有了:新建面板。
    NewPanel,
}

/// 重开的落点:原分组(任一面板里)还在 → 原分组原下标;否则当前活动分组(焦点 pane
/// 所在的那一组,焦点不在活动面板上就取活动面板第一个激活 pane 那组)末尾;
/// 一个面板都没有了 → 新建面板。
pub fn reopen_slot(
    panels: &[ProjectPanel],
    active_panel_id: Option<&str>,
    focused_pane_id: Option<&str>,
    entry: &ClosedPane,
) -> ReopenSlot {
    let origin_alive = panels.iter().any(|panel| {
        matches!(
            panel.layout.node(&entry.leaf_id),
            Some(SplitNode::Leaf { .. })
        )
    });
    if origin_alive {
        return ReopenSlot::Leaf {
            leaf_id: entry.leaf_id.clone(),
            index: Some(entry.index),
        };
    }
    let active = active_panel_id
        .and_then(|id| panels.iter().find(|p| p.id == id))
        .or_else(|| panels.first());
    let Some(layout) = active.map(|p| &p.layout) else {
        return ReopenSlot::NewPanel;
    };
    let leaf = focused_pane_id
        .and_then(|id| layout.leaf_of_pane(id))
        .map(|leaf| leaf.id().to_string())
        .or_else(|| layout.first_leaf_id());
    match leaf {
        Some(leaf_id) => ReopenSlot::Leaf {
            leaf_id,
            index: None,
        },
        None => ReopenSlot::NewPanel,
    }
}

/// 把 pane 插进叶子 `leaf` 的第 `index` 位(越界钳到末尾,`None` 追加)并激活。
/// `leaf` 不是叶子时什么都不做,返回 `false`。
pub fn insert_into_leaf(leaf: &mut SplitNode, pane: PaneState, index: Option<usize>) -> bool {
    let SplitNode::Leaf {
        panes,
        active_pane_id,
        ..
    } = leaf
    else {
        return false;
    };
    let at = index.map_or(panes.len(), |i| i.min(panes.len()));
    *active_pane_id = pane.id.clone();
    panes.insert(at, pane);
    true
}

/// 这一项重开时要不要续接 AI 会话。判据整份是启动恢复那一条
/// ([`resolve_auto_resume_command`]:开关、远程、agent 能力位与 id 白名单),
/// 「待续接」标记按重开的语义视为置位。
pub fn reopen_resumes(entry: &ClosedPane, auto_resume: bool, remote: bool) -> bool {
    resolve_auto_resume_command(auto_resume, true, entry.ai_session.as_ref(), remote).is_some()
}

/// 重开出来的 pane(还没有 PTY,由 `hydrate_project` 起)。新 id、新状态;
/// 续接时带上会话身份并置「待续接」,否则身份一律不带。
pub fn reopened_pane(entry: &ClosedPane, resume: bool) -> PaneState {
    let mut pane = PaneState::new(entry.shell_name.clone());
    pane.cwd = entry.cwd.clone();
    pane.custom_title = entry.custom_title.clone();
    if resume {
        pane.ai_session = entry.ai_session.clone();
        pane.resume_pending = true;
    }
    pane
}

impl AppStore {
    /// 关闭之前把这几个 pane 记进项目的已关闭栈。`pane_ids` 按 tab 顺序给;
    /// 已经不在布局里的 id 跳过。**只由 `pane_actions` 的关闭入口调用**(见模块注释)。
    pub fn remember_closed_panes(&mut self, project_id: &str, pane_ids: &[String]) {
        let Some(state) = self.project_states.get(project_id) else {
            return;
        };
        let batch: Vec<ClosedPane> = pane_ids
            .iter()
            .filter_map(|id| {
                let SplitNode::Leaf {
                    id: leaf_id, panes, ..
                } = state.leaf_of_pane(id)?
                else {
                    return None;
                };
                let index = panes.iter().position(|p| &p.id == id)?;
                let pane = &panes[index];
                Some(ClosedPane {
                    label: self.pane_title_parts(project_id, pane).0,
                    shell_name: pane.shell_name.clone(),
                    cwd: pane.cwd.clone(),
                    custom_title: pane.custom_title.clone(),
                    ai_session: pane.ai_session.clone(),
                    leaf_id: leaf_id.clone(),
                    index,
                })
            })
            .collect();
        if batch.is_empty() {
            return;
        }
        if let Some(state) = self.project_states.get_mut(project_id) {
            push_closed(&mut state.closed_panes, batch);
        }
    }

    /// 某个 pane 所在那一组的全部 pane id(tab 顺序)。关整组入栈前用。
    pub fn leaf_pane_ids(&self, project_id: &str, pane_id: &str) -> Vec<String> {
        match self
            .project_states
            .get(project_id)
            .and_then(|s| s.leaf_of_pane(pane_id))
        {
            Some(SplitNode::Leaf { panes, .. }) => panes.iter().map(|p| p.id.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// 下一个会被重开的终端的显示名(栈顶);栈空返回 `None`(入口据此置灰)。
    pub fn next_closed_label(&self, project_id: &str) -> Option<String> {
        self.project_states
            .get(project_id)
            .and_then(|s| s.closed_panes.last())
            .map(|entry| entry.label.clone())
    }

    /// 重开栈顶那一个(Ctrl+Shift+Z / 菜单)。栈空返回 `false`。
    ///
    /// cwd 覆盖要查盘的那一路先丢后台,落树挪到回调里(见模块注释);弹栈在这里同步做,
    /// 连按几下各弹各的。
    pub fn reopen_closed_pane(
        &mut self,
        project_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(remote) = self
            .project(project_id)
            .map(|p| p.ssh_connection_id.is_some())
        else {
            return false;
        };
        let Some(entry) = self
            .project_states
            .get_mut(project_id)
            .and_then(|s| s.closed_panes.pop())
        else {
            return false;
        };
        match entry.cwd.clone() {
            Some(cwd) if !remote => {
                let project_id = project_id.to_string();
                cx.spawn_in(window, async move |this, cx| {
                    let exists = cx
                        .background_executor()
                        .spawn(async move { Path::new(&cwd).is_dir() })
                        .await;
                    let _ = this.update_in(cx, |store, window, cx| {
                        store.place_reopened(&project_id, entry, exists, window, cx);
                    });
                })
                .detach();
            }
            _ => self.place_reopened(project_id, entry, true, window, cx),
        }
        true
    }

    /// 把弹出来的那一项落进树里并起 PTY。`cwd_ok = false`:cwd 覆盖指向的目录没了,
    /// 回落项目根(连同续接时的启动目录也改由会话 cwd / 项目根决定)。
    fn place_reopened(
        &mut self,
        project_id: &str,
        mut entry: ClosedPane,
        cwd_ok: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 查目录那段时间里项目可能被移除了
        let Some(remote) = self
            .project(project_id)
            .map(|p| p.ssh_connection_id.is_some())
        else {
            return;
        };
        if !cwd_ok {
            entry.cwd = None;
        }
        let auto_resume = self.config.ai_auto_resume.unwrap_or(true);
        let pane = reopened_pane(&entry, reopen_resumes(&entry, auto_resume, remote));
        let pane_id = pane.id.clone();
        // 「当前活动分组」与快捷键取目标同一条回落链(切项目回来焦点指着别处也认得出)
        let focused = self.active_pane_id(project_id);
        let Some(state) = self.project_states.get_mut(project_id) else {
            return;
        };
        let slot = reopen_slot(
            &state.panels,
            state.active_panel_id.as_deref(),
            focused.as_deref(),
            &entry,
        );
        let placed = match slot {
            ReopenSlot::Leaf { leaf_id, index } => state
                .node_mut(&leaf_id)
                .is_some_and(|leaf| insert_into_leaf(leaf, pane, index)),
            ReopenSlot::NewPanel => {
                let panel = ProjectPanel::new(SplitNode::leaf(pane));
                state.active_panel_id = Some(panel.id.clone());
                state.panels.push(panel);
                true
            }
        };
        if !placed {
            return;
        }
        // 原分组可能在后台面板上:先切过去(切面板会顺手补 PTY),再补一次活动面板 ——
        // 已经在活动面板上时 `set_active_panel` 是空操作,PTY 靠这一句起
        let panel_id = self
            .project_states
            .get(project_id)
            .and_then(|s| s.panel_id_of_pane(&pane_id))
            .map(str::to_string);
        if let Some(panel_id) = panel_id {
            self.set_active_panel(project_id, &panel_id, cx);
        }
        self.hydrate_project(project_id, cx);
        self.after_layout_change(project_id, cx);
        // 落点可能缩在最大化的折叠条里:切项目 / 面板、接管铺满、激活并给焦点
        self.jump_to_pane(project_id, &pane_id, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{SplitDirection, gen_id};

    fn entry(label: &str, leaf_id: &str, index: usize) -> ClosedPane {
        ClosedPane {
            label: label.to_string(),
            shell_name: "pwsh".to_string(),
            cwd: None,
            custom_title: None,
            ai_session: None,
            leaf_id: leaf_id.to_string(),
            index,
        }
    }

    fn labels(stack: &[ClosedPane]) -> Vec<&str> {
        stack.iter().map(|e| e.label.as_str()).collect()
    }

    /// 单个关闭依次压栈、后关的先重开;批量的倒着压,弹栈顺序 = 原 tab 顺序。
    #[test]
    fn 批量关闭按原顺序出栈() {
        let mut stack = Vec::new();
        push_closed(&mut stack, vec![entry("solo", "leaf-1", 0)]);
        push_closed(
            &mut stack,
            vec![
                entry("a", "leaf-1", 0),
                entry("b", "leaf-1", 2),
                entry("c", "leaf-1", 3),
            ],
        );
        let mut popped = Vec::new();
        while let Some(e) = stack.pop() {
            popped.push(e.label);
        }
        assert_eq!(popped, vec!["a", "b", "c", "solo"]);
    }

    /// 上限 10:超出时丢最早关的,最近关的那些都还在。
    #[test]
    fn 已关闭栈超限丢最早的() {
        let mut stack = Vec::new();
        for i in 0..8 {
            push_closed(&mut stack, vec![entry(&format!("t{i}"), "leaf", 0)]);
        }
        // 一次关掉一整组 5 个:总数 13,丢掉最早的 3 个(t0..t2)
        let batch: Vec<ClosedPane> = (0..5).map(|i| entry(&format!("g{i}"), "leaf", i)).collect();
        push_closed(&mut stack, batch);
        assert_eq!(stack.len(), CLOSED_PANES_LIMIT);
        assert_eq!(labels(&stack)[..2], ["t3", "t4"]);
        assert_eq!(stack.last().unwrap().label, "g0", "栈顶是这批最左的那个");
    }

    /// 依次插回原下标能还原组内顺序(推演见 `push_closed` 注释)。
    #[test]
    fn 逐个重开插回原位() {
        let mut leaf = SplitNode::leaf(PaneState::new("x"));
        let mut stack = Vec::new();
        push_closed(
            &mut stack,
            vec![entry("a", "", 0), entry("b", "", 2), entry("c", "", 3)],
        );
        while let Some(e) = stack.pop() {
            assert!(insert_into_leaf(
                &mut leaf,
                PaneState::new(e.label),
                Some(e.index)
            ));
        }
        let names: Vec<String> = leaf.panes().iter().map(|p| p.shell_name.clone()).collect();
        assert_eq!(names, vec!["a", "x", "b", "c"]);
        // 插进去的那个成了激活 tab;下标越界钳到末尾,None 追加
        let SplitNode::Leaf {
            panes,
            active_pane_id,
            ..
        } = &leaf
        else {
            unreachable!()
        };
        assert_eq!(active_pane_id, &panes[3].id);
        assert!(insert_into_leaf(
            &mut leaf,
            PaneState::new("tail"),
            Some(99)
        ));
        assert!(insert_into_leaf(&mut leaf, PaneState::new("end"), None));
        let names: Vec<String> = leaf.panes().iter().map(|p| p.shell_name.clone()).collect();
        assert_eq!(names.last().map(String::as_str), Some("end"));
        assert_eq!(names[names.len() - 2], "tail");
    }

    fn panel(layout: SplitNode) -> ProjectPanel {
        ProjectPanel::new(layout)
    }

    /// 落点回落链:原分组(任一面板里)→ 焦点所在分组 → 活动面板第一组 → 新建面板。
    #[test]
    fn 重开落点的回落选择() {
        let left = SplitNode::leaf(PaneState::new("l"));
        let right = SplitNode::leaf(PaneState::new("r"));
        let (left_id, right_id) = (left.id().to_string(), right.id().to_string());
        let right_pane = right.panes()[0].id.clone();
        let split = SplitNode::Split {
            id: gen_id("split"),
            direction: SplitDirection::Horizontal,
            sizes: vec![50.0, 50.0],
            children: vec![left, right],
        };
        let other = SplitNode::leaf(PaneState::new("o"));
        let other_id = other.id().to_string();
        let panels = vec![panel(split), panel(other)];
        let active = panels[0].id.clone();

        // 原分组还在(哪怕在后台面板上):插回原下标
        assert_eq!(
            reopen_slot(&panels, Some(&active), None, &entry("x", &other_id, 3)),
            ReopenSlot::Leaf {
                leaf_id: other_id,
                index: Some(3)
            }
        );
        // 原分组没了:焦点所在那组的末尾
        let gone = entry("x", "leaf-已关", 1);
        assert_eq!(
            reopen_slot(&panels, Some(&active), Some(&right_pane), &gone),
            ReopenSlot::Leaf {
                leaf_id: right_id,
                index: None
            }
        );
        // 焦点不在活动面板上:活动面板的第一组
        assert_eq!(
            reopen_slot(&panels, Some(&active), Some("pane-别处"), &gone),
            ReopenSlot::Leaf {
                leaf_id: left_id,
                index: None
            }
        );
        // 一个面板都没有了
        assert_eq!(reopen_slot(&[], None, None, &gone), ReopenSlot::NewPanel);
    }

    /// 续接与否整份复用启动恢复的判据:开关、远程、agent 能力位都要过;续接不了时
    /// 只恢复 shell / 目录 / 名字,不带会话身份。
    #[test]
    fn 只有能续接时才带会话身份() {
        let mut e = entry("claude", "leaf", 0);
        e.cwd = Some("D:/proj/sub".into());
        e.custom_title = Some("修 bug".into());
        e.ai_session = Some(AiSessionRef {
            agent: Some("claude".into()),
            session_id: "0199a1b2-c3d4-7e8f-9012-3456789abcde".into(),
            cwd: None,
        });

        assert!(reopen_resumes(&e, true, false));
        assert!(!reopen_resumes(&e, false, false), "自动续接关着");
        assert!(!reopen_resumes(&e, true, true), "远程项目");
        let mut no_session = e.clone();
        no_session.ai_session = None;
        assert!(!reopen_resumes(&no_session, true, false));
        let mut bad_id = e.clone();
        bad_id.ai_session.as_mut().unwrap().session_id = "x; rm -rf /".into();
        assert!(!reopen_resumes(&bad_id, true, false), "id 过不了白名单");

        let resumed = reopened_pane(&e, true);
        assert_eq!(resumed.ai_session, e.ai_session);
        assert!(resumed.resume_pending);
        assert_eq!(resumed.cwd.as_deref(), Some("D:/proj/sub"));
        assert_eq!(resumed.custom_title.as_deref(), Some("修 bug"));
        assert_eq!(resumed.pty_id, None, "PTY 由 hydrate_project 起");

        let plain = reopened_pane(&e, false);
        assert_eq!(plain.ai_session, None);
        assert!(!plain.resume_pending);
        assert_eq!(plain.shell_name, "pwsh");
        assert_eq!(plain.cwd.as_deref(), Some("D:/proj/sub"));
    }
}
