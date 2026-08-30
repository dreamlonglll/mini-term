//! `store` 里那批**不碰 `self`** 的纯函数与它们的类型,连同全部单测。
//!
//! 从 `store.rs` 文件末尾原样搬来(AI 项目聚合 / 标题栏状态灯 / 移动端中转的
//! 纯逻辑 / 终端渲染参数 / 会话分支自记账 / 树操作),段注释随代码走,
//! 逻辑一行未改。`pub` 项由 `store/mod.rs` 原样再导出,对外路径不变。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use mt_config::ProjectConfig;
use mt_ui::TerminalStyle;

use crate::notify::PaneRef;
use crate::session_panel::build_resume_command;
use crate::tree::{AiSessionRef, PaneStatus, SplitNode};

use super::ProjectState;

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
// 拆分前是模块私有;现在调用点(`store::panes`)是兄弟模块,升到 `pub(super)`。
pub(super) fn next_maximized(current: Option<&str>, requested: Option<&str>) -> Option<String> {
    match requested {
        Some(id) if current != Some(id) => Some(id.to_string()),
        _ => None,
    }
}

// 拆分前是模块私有;现在调用点(`store::prefs`)是兄弟模块,升到 `pub(super)`。
pub(super) fn rename_pane_in_states(
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
        let Some(pane) = state.pane_mut(pane_id) else {
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
// 拆分前是模块私有;现在调用点(`store::prefs`)是兄弟模块,升到 `pub(super)`。
pub(super) fn find_pane_of_pty(
    states: &HashMap<String, ProjectState>,
    pty_id: u32,
) -> Option<(String, String)> {
    states.iter().find_map(|(project_id, state)| {
        state
            .layouts()
            .find_map(|layout| layout.pane_by_pty(pty_id))
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
///
/// 与 [`CJK_FALLBACK_FONTS`] 同样**三家并列**:回退表里点不到的名字会被跳过,
/// 列全比按平台切省事,也让同一串字族配置在三个平台上表现一致。此前只有
/// `Segoe UI Emoji` 一个,macOS / Linux 上必然落空,终端里的 emoji 无字体可用。
const EMOJI_FALLBACK_FONTS: [&str; 3] = ["Segoe UI Emoji", "Apple Color Emoji", "Noto Color Emoji"];

/// `config.terminalFontSize` + `terminalFontFamily` → [`TerminalStyle`]。
///
/// 字族那一串是 CSS `font-family` 语法(原版直接喂 xterm),而
/// [`TerminalStyle`] 是「主字体 + 回退列表」两段式:取首项当主字体,其余进回退,
/// 再自动补 CJK 与 emoji —— 与原版 `resolveTerminalFontFamily` 同语义
/// (它是往用户串后面拼 `CJK_FALLBACK_FONTS`)。
///
/// 字族为空 / 只写了通用族名时整段回落 [`TerminalStyle::default`]。
pub fn terminal_style_from(size: f64, family: Option<&str>, ligatures: bool) -> TerminalStyle {
    let mut style = TerminalStyle {
        font_size: gpui::px(size as f32),
        ligatures,
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
    for extra in CJK_FALLBACK_FONTS.iter().chain(EMOJI_FALLBACK_FONTS.iter()) {
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
// 拆分前是模块私有;现在调用点(`store::panes::hydrate_project`)是兄弟模块,
// 升到 `pub(super)`。
pub(super) fn resolve_resume_cwd(session: &AiSessionRef) -> Option<String> {
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

// 拆分前是模块私有;现在调用点(`store::layout`)是兄弟模块,升到 `pub(super)`。
pub(super) fn collect_node_ids(node: &SplitNode, out: &mut HashSet<String>) {
    out.insert(node.id().to_string());
    if let SplitNode::Split { children, .. } = node {
        for c in children {
            collect_node_ids(c, out);
        }
    }
}

/// 从 projectTree 里摘掉一个项目(递归进分组)。
// 拆分前是模块私有;现在调用点(`store::projects`)是兄弟模块,升到 `pub(super)`。
pub(super) fn remove_from_tree(tree: &mut Vec<mt_config::ProjectTreeItem>, project_id: &str) {
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
    // 只有测试用得到的两个类型(`two_projects` 造布局用),不进模块顶部的
    // `use`,免得非测试构建多两条无人使用的导入。
    use crate::tree::{PaneState, ProjectPanel};

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

    /// [`AppStore::title_bar_snapshot`] 把状态灯与胶囊下拉合成了**一次**全 pane
    /// 遍历。这条闸看住的就是那次合并没改结果:同一份 pane 快照喂给两个聚合器,
    /// 必须与「各自扫一遍」逐字相同。
    ///
    /// (`PaneRef` 为此加了 `Copy` —— 加完之后编译器不会再拦「谁把 Vec 吃掉了」,
    /// 所以得有一条用例替它站岗。)
    #[test]
    fn 一份_pane_快照喂两个聚合器与各扫一遍等价() {
        let projects = vec![project("a", "A"), project("b", "B")];
        let panes = vec![
            pane("a", "w", PaneStatus::AiWorking, false),
            pane("a", "d", PaneStatus::Idle, false),
            pane("b", "x", PaneStatus::AiIdle, true),
        ];
        let done = |id: &str| id == "d";

        // 合成一次遍历(`title_bar_snapshot` 的内层写法)
        let light_merged = compute_title_bar_light(panes.iter().copied(), done);
        let projects_merged = collect_ai_projects(panes.iter().copied(), &projects, done);
        // 各扫一遍(合并之前那两次 `pane_refs(None)`)
        let light_split = compute_title_bar_light(panes.clone(), done);
        let projects_split = collect_ai_projects(panes.clone(), &projects, done);

        assert_eq!(light_merged, light_split);
        assert_eq!(projects_merged, projects_split);

        // 顺带钉死这组数据的期望值 —— 两边一起错的话上面三条是测不出来的
        assert_eq!(light_merged, TitleBarLight::Attention, "b 有待确认,压过 a 的处理中");
        assert_eq!(projects_merged.attention, 1);
        assert_eq!(projects_merged.working, 1);
        assert_eq!(projects_merged.done, 1);
        assert_eq!(projects_merged.entries.len(), 2);
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
        sa.panels.push(ProjectPanel::new(SplitNode::leaf(a)));
        states.insert("p-a".to_string(), sa);
        let mut sb = ProjectState::new();
        sb.panels.push(ProjectPanel::new(SplitNode::leaf(b)));
        states.insert("p-b".to_string(), sb);
        // 布局还没建出来的项目也要能安全跳过
        states.insert("p-empty".to_string(), ProjectState::new());
        (states, a_id, b_id)
    }

    fn title_of(states: &HashMap<String, ProjectState>, pane_id: &str) -> Option<String> {
        states
            .values()
            .find_map(|s| s.pane(pane_id))
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
            false,
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
        // 三家的 emoji 字体都要在:同一串配置换个平台照样画得出 emoji
        for emoji in EMOJI_FALLBACK_FONTS {
            assert!(
                fallbacks.iter().any(|f| f == emoji),
                "缺 emoji 回退 {emoji}"
            );
        }
    }

    /// 字族为空 / 只有通用族名时整段回落默认样式(只改字号)。
    #[test]
    fn 终端字族为空时回落默认() {
        let default = TerminalStyle::default();
        for family in [None, Some(""), Some("   "), Some("monospace, serif")] {
            let style = terminal_style_from(14.0, family, false);
            assert_eq!(style.font_family, default.font_family, "{family:?}");
            assert_eq!(style.font_fallbacks, default.font_fallbacks, "{family:?}");
        }
    }

    /// 重复声明 CJK 字体时不该在回退串里出现两次。
    #[test]
    fn 终端字族回退不重复() {
        let style = terminal_style_from(14.0, Some("Consolas, 'Microsoft YaHei'"), false);
        let yahei = style
            .font_fallbacks
            .iter()
            .filter(|f| f.as_ref() == "Microsoft YaHei")
            .count();
        assert_eq!(yahei, 1);
    }

    /// 连字开关一路穿到 [`TerminalStyle`],**不被字族解析那一段吃掉** ——
    /// 早退分支(字族为空 / 只剩通用族名)也得带着它走。
    #[test]
    fn 连字开关穿到样式并存活于早退分支() {
        let on = terminal_style_from(14.0, Some("Fira Code"), true);
        assert!(on.ligatures);
        assert!(!terminal_style_from(14.0, Some("Fira Code"), false).ligatures);
        // 这两条走的是 `terminal_style_from` 里的两个 `return style` 早退
        assert!(terminal_style_from(14.0, None, true).ligatures);
        assert!(terminal_style_from(14.0, Some("monospace"), true).ligatures);
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
