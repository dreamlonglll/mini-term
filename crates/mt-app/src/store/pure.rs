//! `store` 里那批**不碰 `self`** 的纯函数与它们的类型,连同全部单测。
//!
//! 从 `store.rs` 文件末尾原样搬来(AI 项目聚合 / 标题栏状态灯 / 移动端中转的
//! 纯逻辑 / 终端渲染参数 / 会话分支自记账 / 树操作),段注释随代码走,
//! 逻辑一行未改。`pub` 项由 `store/mod.rs` 原样再导出,对外路径不变。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use mt_config::ProjectConfig;
use mt_ui::TerminalStyle;
use unicode_segmentation::UnicodeSegmentation;

use crate::notify::PaneRef;
use crate::tree::{AiSessionRef, PaneState, PaneStatus, SplitNode};

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
    ///
    /// ⚠️ 2026-09-04 标题栏那颗灯按用户要求撤掉(见 [`crate::title_bar`] 模块注释),
    /// 于是这条**暂时没有生产调用方** —— 档位类型、归并算法与这张文案表都留着:
    /// 它们仍由 [`compute_title_bar_light`] 那几条单测把着,哪天要把灯挂回来
    /// (或换个位置画)不必从头再推一遍优先级。
    #[allow(dead_code)]
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
/// - agent 不支持续接(opencode / pi / 认不出)或 id 非法:命令与白名单都在
///   [`mt_ai::agent::resume_command`](与 AI 历史面板共用一套)。
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
    mt_ai::agent::resume_command(session.agent.as_deref(), &session.session_id)
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
    resolve_resume_cwd_with(session, mt_ai::sessions::lookup_ai_session_cwd)
}

/// [`resolve_resume_cwd`] 的可注入版本:`lookup` 是「按会话 id 翻 jsonl」那一步。
/// 单测换成计数桩,证明非 claude 系根本不去翻盘。
fn resolve_resume_cwd_with(
    session: &AiSessionRef,
    lookup: impl FnOnce(String) -> Option<String>,
) -> Option<String> {
    if let Some(cwd) = session.cwd.as_deref() {
        return Path::new(cwd).is_dir().then(|| cwd.to_string());
    }
    // 只有 claude 系(含缺省 agent)能按 id 反查:codex 不按目录分桶;omp / grok
    // 按目录分桶但没有反查手段 —— 都不去 `~/.claude/projects` 里翻(那里只会有
    // claude 的桶)。口径见 `mt_ai::CwdBucket`
    if !mt_ai::AgentKind::from_session_agent(session.agent.as_deref())
        .is_some_and(|k| k.looks_up_session_cwd())
    {
        return None;
    }
    lookup(session.session_id.clone())
}

/// 启动恢复时,续接 pane 的 PTY 该在哪个目录起 + 反查到的会话 cwd。
///
/// 返回 `(启动目录, 会话 cwd)`。取值链与挪后台之前一字不差(`hydrate_project`
/// 原地那两行):**pane 自己的 cwd 优先**(用户显式给这个 pane 定的目录,worktree
/// 终端靠它),会话 cwd 只在 pane 没指定时兜底;两个都没有 → `None`,调用方落项目根。
/// 会话 cwd 即便没被用来起 PTY 也照样返回 —— 它要随身份写回,下次重启免查。
///
/// `resolve` 即 [`resolve_resume_cwd`](带 `is_dir` 预检、只有 claude 系反查),
/// 可注入只为单测。**同步磁盘 IO**:只许在后台线程上调(见 `start_pty_with`
/// 交给 pane 的启动预案)。
pub(super) fn decide_resume_cwd(
    pane_cwd: Option<String>,
    session: &AiSessionRef,
    resolve: impl FnOnce(&AiSessionRef) -> Option<String>,
) -> (Option<String>, Option<String>) {
    let session_cwd = resolve(session);
    let start = pane_cwd.or_else(|| session_cwd.clone());
    (start, session_cwd)
}

/// 把后台反查所得的会话 cwd 写回 pane 的会话身份。返回「真改了」(调用方据此落盘)。
///
/// 只认**同一个会话**:从反查到回填之间 hook 可能已经报上来一个新身份(用户手快
/// 起了别的会话),那时写回会把别人的目录安到新会话头上。值没变就不动(与原先
/// `sess.cwd != Some(cwd)` 才写的口径相同)。
pub(super) fn apply_resolved_session_cwd(
    session: &mut AiSessionRef,
    session_id: &str,
    cwd: &str,
) -> bool {
    if session.session_id != session_id || session.cwd.as_deref() == Some(cwd) {
        return false;
    }
    session.cwd = Some(cwd.to_string());
    true
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
/// 归一化口径与 [`crate::session_branch::branch_menu_segment`] 登记时同:两边都先小写。
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

// ─── 页签标题跟随 shell(OSC 0/2)的纯函数(可测) ──────────────

/// OSC 标题的字符数上限。tab 栏是一行横排,副段过长会把同组其它 tab 挤出可视区;
/// 比移动端改名那条(64)更紧 —— 那是用户手打的名字,这里是 shell 自动灌的
/// 一整条目录路径,默认就很长。截断而不是丢弃。
pub const MAX_OSC_TITLE_GRAPHEMES: usize = 48;

/// 收敛 shell 报上来的窗口标题:砍控制字符 → 去首尾空白 → 按**字素**限长。
///
/// 三处细节:
/// - 控制字符要在 trim **之前**砍:`\x1b` 之类不算 `char::is_whitespace`,
///   先 trim 的话它们会把真正的空白挡在外面;
/// - 按字素而不是 `char` 截断:emoji 与带修饰符的字(👩‍💻 / é 的组合形式)
///   拆到一半会在 tab 上画出半个字形;
/// - 收敛后为空(纯空白 / 纯控制字符)返回 `None`,与 `ResetTitle` 同归一档。
pub fn sanitize_osc_title(raw: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (i, g) in UnicodeSegmentation::graphemes(trimmed, true).enumerate() {
        if i >= MAX_OSC_TITLE_GRAPHEMES {
            break;
        }
        out.push_str(g);
    }
    Some(out)
}

/// 「这条标题就是 shell 自己的默认标题」——等于什么信息都没给,副段不该显示它。
///
/// 判据全部**大小写不敏感**,并且先把 `Administrator: ` 前缀剥掉再比一次
/// (提权的 Windows 控制台会给标题加这个前缀)。逐条:
///
/// | 形态 | 例 |
/// |------|-----|
/// | 等于 pane 的 shell 名 | 页签本来就叫这个 |
/// | `pwsh` | PowerShell 7 未配主题时的默认 |
/// | 以 `PowerShell` 开头 | `PowerShell 7.5.3` |
/// | `Windows PowerShell` | 内置 5.1 |
/// | `Command Prompt` | 英文版 cmd |
/// | 等于 / 以 `\cmd.exe` 结尾 | `C:\WINDOWS\system32\cmd.exe` |
/// | `bash` / `zsh` / `fish` / `nu` | 类 Unix shell 的裸名 |
pub fn is_default_shell_title(title: &str, shell_name: &str) -> bool {
    let title = title.trim();
    if title.is_empty() {
        return true;
    }
    if default_shell_title_core(title, shell_name) {
        return true;
    }
    // 提权控制台给标题加的 `Administrator: ` 前缀:剥掉再比一次。
    // 前缀是纯 ASCII,所以按字节比 `eq_ignore_ascii_case` 就够,**不走
    // `to_lowercase()` 再切片** —— 那个会改变字节长度(`İ` 变两个码点),
    // 拿小写串的偏移去切原串会切到字符中间直接 panic。
    const ADMIN_PREFIX: &str = "Administrator: ";
    if title.len() > ADMIN_PREFIX.len()
        && title.is_char_boundary(ADMIN_PREFIX.len())
        && title[..ADMIN_PREFIX.len()].eq_ignore_ascii_case(ADMIN_PREFIX)
    {
        let rest = title[ADMIN_PREFIX.len()..].trim();
        return rest.is_empty() || default_shell_title_core(rest, shell_name);
    }
    false
}

/// [`is_default_shell_title`] 的判据本体(不含 `Administrator: ` 剥壳那一层)。
fn default_shell_title_core(title: &str, shell_name: &str) -> bool {
    let lower = title.to_lowercase();
    if !shell_name.trim().is_empty() && lower == shell_name.trim().to_lowercase() {
        return true;
    }
    if lower.starts_with("powershell") {
        return true;
    }
    if matches!(
        lower.as_str(),
        "pwsh" | "windows powershell" | "command prompt" | "bash" | "zsh" | "fish" | "nu"
    ) {
        return true;
    }
    // cmd 把自己的标题设成完整路径:`C:\WINDOWS\system32\cmd.exe`
    lower == "cmd.exe" || lower.ends_with("\\cmd.exe")
}

/// 页签副段的口径。返回 `None` = 这个 pane 不显示副段。
///
/// 四道闸(任一不过就没有副段):
/// 1. 开关关着 / 这个 pane 正显示 AI 会话身份(两者合在 `enabled` 里,见
///    [`subtitle_enabled`]);
/// 2. pane 有自定义名 —— 用户亲手命名了就尊重,不在后面缀一条自动来的;
/// 3. 没收到过标题 / 收到的是空标题;
/// 4. 标题是 shell 自己的默认标题([`is_default_shell_title`])。
///
/// 过了闸再剥一层壳:oh-my-posh 默认标题模板是 `{{ .Shell }} in {{ .Folder }}`,
/// 报上来的是 `pwsh in mini-term`,而主段本来就写着 `pwsh`,原样缀上去就是
/// 「pwsh · pwsh in mini-term」。前缀与 shell 名相同时剥掉,只留目录。
pub fn osc_subtitle(pane: &PaneState, enabled: bool) -> Option<String> {
    subtitle_for_title(pane, pane.osc_title.as_deref(), enabled)
}

/// [`osc_subtitle`] 的判定本体:拿「假如这个 pane 的标题是 `title`」来算副段 ——
/// [`visible_subtitle_changes`] 要在写入之前比新旧两份,不必为此克隆整个 pane。
fn subtitle_for_title(pane: &PaneState, title: Option<&str>, enabled: bool) -> Option<String> {
    if !enabled {
        return None;
    }
    if pane.custom_title.as_deref().is_some_and(|t| !t.is_empty()) {
        return None;
    }
    let title = title?.trim();
    if title.is_empty() || is_default_shell_title(title, &pane.shell_name) {
        return None;
    }
    Some(strip_shell_in_prefix(title, &pane.shell_name).to_string())
}

/// OSC 标题从 `pane.osc_title` 换成 `next` 之后,页签上**看得见**的副段变没变。
///
/// 没变就不必叫醒任何视图(见 `AppStore::set_pane_osc_title`):AI 在场时副段整个
/// 收起(`enabled` 为假),Claude Code 写进标题的 spinner 帧一帧也显示不出来;有自定义
/// 名、新旧都是 shell 默认标题、剥壳后同一个目录,同理。
pub fn visible_subtitle_changes(pane: &PaneState, next: Option<&str>, enabled: bool) -> bool {
    subtitle_for_title(pane, pane.osc_title.as_deref(), enabled)
        != subtitle_for_title(pane, next, enabled)
}

/// 这个 pane 现在该不该显示副段:开关开着,**且 tab 上没在显示 AI 会话身份**。
///
/// AI CLI 会把自己的名字写进窗口标题(Claude Code 是 `✳ Claude Code`),而 tab 上
/// 已经挂着品牌图标了,再缀一段「· ✳ Claude Co…」只是把同一件事说第二遍,还把
/// 胶囊撑到截断。会话身份的判据与品牌图标同一条([`PaneState::shows_ai_session`]),
/// 图标一出现副段就收起,图标一撤副段再回来。
pub fn subtitle_enabled(config: &mt_config::AppConfig, pane: &PaneState) -> bool {
    // 两个开关都是缺省开启
    config.tab_title_follows_shell.unwrap_or(true)
        && !pane.shows_ai_session(config.ai_auto_resume.unwrap_or(true))
}

/// 剥掉 oh-my-posh 默认模板的 `<shell> in ` 前缀(大小写不敏感,按字节比,
/// 理由同 [`is_default_shell_title`] 里的 `Administrator: `)。剥完为空就不剥。
fn strip_shell_in_prefix<'a>(title: &'a str, shell_name: &str) -> &'a str {
    let shell = shell_name.trim();
    if shell.is_empty() {
        return title;
    }
    let prefix = format!("{shell} in ");
    if title.len() > prefix.len()
        && title.is_char_boundary(prefix.len())
        && title[..prefix.len()].eq_ignore_ascii_case(&prefix)
    {
        let rest = title[prefix.len()..].trim();
        if !rest.is_empty() {
            return rest;
        }
    }
    title
}

/// pane 显示名**主段**的判定本体:自定义名 > 远程连接名 > shell 名。
///
/// 远程那一档要查连接表,所以两份数据由 store 传进来 —— 判定本身不碰 `self`,
/// 于是三档都能直接单测(`AppStore` 要 gpui 的 `Context` 才造得出来)。
/// `project` 为 `None`(项目已被删 / 调用方没有项目上下文)时退到 shell 名。
pub fn pane_primary_label_of(
    project: Option<&ProjectConfig>,
    connections: &[mt_config::SshConnection],
    pane: &PaneState,
) -> String {
    if let Some(title) = pane.custom_title.as_deref().filter(|t| !t.is_empty()) {
        return title.to_string();
    }
    if let Some(project) = project.filter(|p| crate::ssh_conn::is_remote_project(p)) {
        return crate::ssh_conn::remote_pane_label(project, connections);
    }
    pane.shell_name.clone()
}

/// [`AppStore::pane_title_parts`](crate::store::AppStore::pane_title_parts) 的
/// 判定本体:主段见 [`pane_primary_label_of`],副段见 [`osc_subtitle`]。
pub fn pane_title_parts_of(
    project: Option<&ProjectConfig>,
    connections: &[mt_config::SshConnection],
    pane: &PaneState,
    enabled: bool,
) -> (String, Option<String>) {
    (
        pane_primary_label_of(project, connections, pane),
        osc_subtitle(pane, enabled),
    )
}

/// 把 [`AppStore::pane_title_parts`](crate::store::AppStore::pane_title_parts)
/// 的两段拼成一行 —— `pane_display_label` 与移动端快照共用这一个拼法。
pub fn join_title_parts(primary: &str, subtitle: Option<&str>) -> String {
    match subtitle {
        Some(sub) if !sub.is_empty() => format!("{primary} · {sub}"),
        _ => primary.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // 只有测试用得到的类型(`two_projects` 造布局用),不进模块顶部的
    // `use`,免得非测试构建多一条无人使用的导入。
    use crate::tree::ProjectPanel;

    fn project(id: &str, name: &str) -> ProjectConfig {
        ProjectConfig::new(id, name, format!("/tmp/{id}"))
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

    /// 命令按 agent 分派;缺省 agent 按 claude(会话身份的约定),hook 上报的
    /// `claude-code` 同样按 claude。
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
        let s = session(Some("omp"), "1f9d2a6b9c0d1234");
        assert_eq!(
            resolve_auto_resume_command(true, true, Some(&s), false).as_deref(),
            Some("omp --resume 1f9d2a6b9c0d1234")
        );
        for agent in [None, Some(""), Some("claude-code")] {
            let s = session(agent, "abc-123");
            assert_eq!(
                resolve_auto_resume_command(true, true, Some(&s), false).as_deref(),
                Some("claude --resume abc-123"),
                "{agent:?}"
            );
        }
    }

    /// 认不出的 agent 不再兜底成 claude:旧口径会在启动时往 pane 里敲一条
    /// `claude --resume <别家的 id>`。
    #[test]
    fn 自动续接认不出的_agent_不续() {
        for agent in ["opencode", "pi", "gemini"] {
            let s = session(Some(agent), "abc-123");
            assert!(
                resolve_auto_resume_command(true, true, Some(&s), false).is_none(),
                "{agent}"
            );
        }
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

    /// 续接启动目录的取值链(挪后台前后一字不差):pane cwd 优先、会话 cwd 兜底、
    /// 都没有落 `None`(调用方用项目根);会话 cwd 即便没被用上也要交回去写回。
    #[test]
    fn 续接启动目录_pane_优先会话兜底() {
        let s = session(Some("claude"), "abc-123");
        let found = || Some("D:/proj/sub".to_string());

        // pane 自己有目录:用它起,反查结果照样交回(写回用)
        assert_eq!(
            decide_resume_cwd(Some("D:/wt".into()), &s, |_| found()),
            (Some("D:/wt".into()), Some("D:/proj/sub".into()))
        );
        // pane 没指定:会话 cwd 兜底
        assert_eq!(
            decide_resume_cwd(None, &s, |_| found()),
            (Some("D:/proj/sub".into()), Some("D:/proj/sub".into()))
        );
        // 都没有:None(调用方落项目根)
        assert_eq!(decide_resume_cwd(None, &s, |_| None), (None, None));
        assert_eq!(
            decide_resume_cwd(Some("D:/wt".into()), &s, |_| None),
            (Some("D:/wt".into()), None)
        );
    }

    /// 反查只在 claude 系且会话记录里没有 cwd 时才发生:codex / omp / grok 根本
    /// 不碰盘,记录里带着(且在盘上)的 cwd 直接用。
    #[test]
    fn 续接启动目录_codex_omp_不反查() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let lookup = |_: String| {
            calls.set(calls.get() + 1);
            Some("D:/from-jsonl".to_string())
        };
        for agent in ["codex", "omp", "grok", "opencode", "gemini"] {
            let s = session(Some(agent), "rollout_9");
            assert_eq!(
                decide_resume_cwd(None, &s, |s| resolve_resume_cwd_with(s, lookup)),
                (None, None),
                "{agent} 不该反查"
            );
        }
        assert_eq!(calls.get(), 0, "非 claude 系一次都不许翻盘");

        // claude(含缺省 agent 与 hook 上报的 claude-code)没有 cwd → 反查一次
        for agent in [Some("claude"), None, Some("claude-code")] {
            let s = session(agent, "abc-123");
            assert_eq!(
                decide_resume_cwd(None, &s, |s| resolve_resume_cwd_with(s, lookup)).0,
                Some("D:/from-jsonl".into())
            );
        }
        assert_eq!(calls.get(), 3);

        // 记录里带着在盘上的 cwd:直接用,不反查
        let tmp = std::env::temp_dir().to_string_lossy().to_string();
        let mut s = session(Some("claude"), "abc-123");
        s.cwd = Some(tmp.clone());
        assert_eq!(
            decide_resume_cwd(None, &s, |s| resolve_resume_cwd_with(s, lookup)),
            (Some(tmp.clone()), Some(tmp))
        );
        assert_eq!(calls.get(), 3);
    }

    /// 写回只认同一个会话,值没变不算改动。
    #[test]
    fn 反查所得目录只写回同一个会话() {
        let mut s = session(Some("claude"), "abc-123");
        assert!(apply_resolved_session_cwd(&mut s, "abc-123", "D:/p"));
        assert_eq!(s.cwd.as_deref(), Some("D:/p"));
        assert!(
            !apply_resolved_session_cwd(&mut s, "abc-123", "D:/p"),
            "没变不落盘"
        );
        // hook 已经报上来别的身份:不许把旧会话的目录安到新会话头上
        let mut other = session(Some("claude"), "new-456");
        assert!(!apply_resolved_session_cwd(&mut other, "abc-123", "D:/p"));
        assert_eq!(other.cwd, None);
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

    // ─── 页签标题跟随 shell(OSC 0/2) ───────────────────────────

    /// 清洗:控制字符要在 trim 之前砍掉,收敛后为空归一成 `None`。
    #[test]
    fn osc_标题清洗去控制字符并归一空串() {
        assert_eq!(
            sanitize_osc_title("  ~/repo/mini-term  ").as_deref(),
            Some("~/repo/mini-term")
        );
        assert_eq!(
            sanitize_osc_title("\u{1b}\u{7} ~/repo \u{7}").as_deref(),
            Some("~/repo"),
            "控制字符先砍再 trim —— 反过来的话 ESC/BEL 会把真空白挡在外面"
        );
        assert_eq!(sanitize_osc_title("").as_deref(), None);
        assert_eq!(sanitize_osc_title("   \t  ").as_deref(), None);
        assert_eq!(
            sanitize_osc_title("\u{1b}\u{7}\u{0}").as_deref(),
            None,
            "纯控制字符与 ResetTitle 同归一档"
        );
        // 换行/制表符也是控制字符,不该在一行横排的 tab 上出现
        assert_eq!(sanitize_osc_title("a\nb\tc").as_deref(), Some("abc"));
    }

    /// 截断按**字素**走:多字节字与 emoji 簇不许被切成半个字形。
    #[test]
    fn osc_标题按字素截断() {
        let ascii = "x".repeat(100);
        assert_eq!(
            sanitize_osc_title(&ascii).unwrap().chars().count(),
            MAX_OSC_TITLE_GRAPHEMES
        );

        // 多字节(每个汉字 3 字节):按字素算仍是 48 个
        let cjk = "目".repeat(100);
        let cut = sanitize_osc_title(&cjk).unwrap();
        assert_eq!(cut.chars().count(), MAX_OSC_TITLE_GRAPHEMES);
        assert_eq!(cut.len(), MAX_OSC_TITLE_GRAPHEMES * 3);

        // 字素簇:ZWJ 家庭 emoji 与组合变音符各算**一个**,不许拆开
        let cluster = "\u{1f469}\u{200d}\u{1f4bb}".repeat(60); // 👩‍💻
        let cut = sanitize_osc_title(&cluster).unwrap();
        assert_eq!(
            UnicodeSegmentation::graphemes(cut.as_str(), true).count(),
            MAX_OSC_TITLE_GRAPHEMES
        );
        assert!(
            cut.ends_with('\u{1f4bb}'),
            "末尾必须是完整的一簇,不能停在 ZWJ 上:{cut:?}"
        );

        let combining = "e\u{301}".repeat(60); // é 的组合形式
        let cut = sanitize_osc_title(&combining).unwrap();
        assert_eq!(
            UnicodeSegmentation::graphemes(cut.as_str(), true).count(),
            MAX_OSC_TITLE_GRAPHEMES
        );
        assert!(cut.ends_with('\u{301}'), "组合符不能与基字被切开:{cut:?}");

        // 刚好 48 个字素:一个都不动
        let exact = "y".repeat(MAX_OSC_TITLE_GRAPHEMES);
        assert_eq!(sanitize_osc_title(&exact).as_deref(), Some(exact.as_str()));
    }

    /// 默认标题抑制表逐条。全部大小写不敏感,`Administrator: ` 前缀剥掉再比一次。
    #[test]
    fn 默认标题抑制表逐条() {
        // 等于 shell 名
        assert!(is_default_shell_title("pwsh", "pwsh"));
        assert!(is_default_shell_title("PWSH", "pwsh"));
        assert!(is_default_shell_title("Git Bash", "git bash"));
        // 裸 pwsh:哪怕 pane 的 shell 名是别的
        assert!(is_default_shell_title("pwsh", "Git Bash"));
        // PowerShell 前缀
        assert!(is_default_shell_title("PowerShell 7.5.3", "pwsh"));
        assert!(is_default_shell_title("powershell 7.5.3", "pwsh"));
        assert!(is_default_shell_title("PowerShell", "pwsh"));
        assert!(is_default_shell_title("Windows PowerShell", "pwsh"));
        // cmd
        assert!(is_default_shell_title("Command Prompt", "cmd"));
        assert!(is_default_shell_title("command prompt", "cmd"));
        assert!(is_default_shell_title("cmd.exe", "cmd"));
        assert!(is_default_shell_title(
            "C:\\WINDOWS\\system32\\cmd.exe",
            "cmd"
        ));
        assert!(is_default_shell_title(
            "c:\\windows\\system32\\CMD.EXE",
            "cmd"
        ));
        // 类 Unix shell 裸名
        for s in ["bash", "zsh", "fish", "nu", "BASH", "Zsh"] {
            assert!(is_default_shell_title(s, "pwsh"), "{s} 该被抑制");
        }
        // Administrator: 前缀剥掉再比
        assert!(is_default_shell_title("Administrator: pwsh", "pwsh"));
        assert!(is_default_shell_title(
            "administrator: C:\\WINDOWS\\system32\\cmd.exe",
            "cmd"
        ));
        assert!(is_default_shell_title(
            "Administrator: Windows PowerShell",
            "pwsh"
        ));
        // 空 / 纯空白
        assert!(is_default_shell_title("", "pwsh"));
        assert!(is_default_shell_title("   ", "pwsh"));

        // 真正有信息的标题:一条都不许被抑制
        for s in ["~/repo/mini-term", "D:\\Git\\mini-term", "npm run dev"] {
            assert!(!is_default_shell_title(s, "pwsh"), "{s} 不该被抑制");
        }
        assert!(!is_default_shell_title("Administrator: ~/repo", "pwsh"));
        assert!(
            !is_default_shell_title("nushell", "pwsh"),
            "只抑制裸 `nu`,`nushell` 是别的东西"
        );
        assert!(
            is_default_shell_title("PowerShellery", "pwsh"),
            "`以 PowerShell 开头` 是写死的口径,宁可多抑制一条也不让版本号糊上 tab"
        );
    }

    fn titled(shell: &str, osc: Option<&str>, custom: Option<&str>) -> PaneState {
        let mut pane = PaneState::new(shell);
        pane.osc_title = osc.map(str::to_string);
        pane.custom_title = custom.map(str::to_string);
        pane
    }

    /// 副段四道闸:开关 / 自定义名 / 空标题 / 默认标题。
    #[test]
    fn 副段的四道闸() {
        let p = titled("pwsh", Some("~/repo/mini-term"), None);
        assert_eq!(osc_subtitle(&p, true).as_deref(), Some("~/repo/mini-term"));
        assert_eq!(osc_subtitle(&p, false), None, "开关关着就没有副段");

        assert_eq!(
            osc_subtitle(&titled("pwsh", Some("~/repo"), Some("构建")), true),
            None,
            "用户命名了就尊重,不缀自动来的"
        );
        assert_eq!(
            osc_subtitle(&titled("pwsh", Some("~/repo"), Some("")), true).as_deref(),
            Some("~/repo"),
            "空的自定义名等于没命名"
        );
        assert_eq!(osc_subtitle(&titled("pwsh", None, None), true), None);
        assert_eq!(osc_subtitle(&titled("pwsh", Some("   "), None), true), None);
        assert_eq!(
            osc_subtitle(&titled("pwsh", Some("PowerShell 7.5.3"), None), true),
            None,
            "默认标题 = 没给信息"
        );
        assert_eq!(
            osc_subtitle(&titled("pwsh", Some("pwsh"), None), true),
            None
        );
    }

    /// tab 上挂着 AI 品牌图标时,标题里的「Claude Code」只是把同一件事说第二遍。
    #[test]
    fn ai_会话在场时不显示副段() {
        use crate::tree::PaneStatus;
        let config = mt_config::AppConfig::default();
        let mut pane = titled("pwsh", Some("✳ Claude Code"), None);
        assert!(subtitle_enabled(&config, &pane), "纯 shell 时照常显示");
        pane.status = PaneStatus::AiWorking;
        assert!(
            !subtitle_enabled(&config, &pane),
            "品牌图标已经说明跑的是谁"
        );
        pane.status = PaneStatus::AiIdle;
        assert!(!subtitle_enabled(&config, &pane));
        pane.status = PaneStatus::Idle;
        assert!(subtitle_enabled(&config, &pane), "AI 退出后副段回来");

        let off = mt_config::AppConfig {
            tab_title_follows_shell: Some(false),
            ..Default::default()
        };
        assert!(!subtitle_enabled(&off, &pane), "开关关着一律没有");
    }

    /// oh-my-posh 默认模板 `{{ .Shell }} in {{ .Folder }}`:前缀等于 shell 名时剥掉。
    #[test]
    fn 副段剥掉_shell_in_前缀() {
        assert_eq!(
            osc_subtitle(&titled("pwsh", Some("pwsh in mini-term"), None), true).as_deref(),
            Some("mini-term")
        );
        assert_eq!(
            osc_subtitle(&titled("pwsh", Some("PWSH in ~/repo"), None), true).as_deref(),
            Some("~/repo"),
            "大小写不敏感"
        );
        assert_eq!(
            osc_subtitle(&titled("cmd", Some("pwsh in mini-term"), None), true).as_deref(),
            Some("pwsh in mini-term"),
            "前缀不是自己的 shell 名就不剥"
        );
        assert_eq!(
            osc_subtitle(&titled("pwsh", Some("pwsh in "), None), true).as_deref(),
            Some("pwsh in"),
            "剥完为空就不剥(标题本身 trim 后是「pwsh in」)"
        );
        assert_eq!(
            osc_subtitle(&titled("pwsh", Some("pwsh in 终端"), None), true).as_deref(),
            Some("终端"),
            "剩余部分是多字节也不会切到字符中间"
        );
    }

    /// 只有页签上看得见的副段变了才算变化 —— 其余的只存不报(不叫醒整窗观察者)。
    #[test]
    fn 看不见的标题变化不算副段变化() {
        let pane = titled("pwsh", Some("~/repo/a"), None);
        // 纯 shell 换目录:看得见
        assert!(visible_subtitle_changes(&pane, Some("~/repo/b"), true));
        // ResetTitle:副段消失,也是看得见的变化
        assert!(visible_subtitle_changes(&pane, None, true));

        // AI 在场(副段收起):Claude Code 的 spinner 帧一帧也显示不出来
        let ai = titled("pwsh", Some("✳ Claude Code"), None);
        assert!(!visible_subtitle_changes(&ai, Some("✶ Claude Code"), false));
        assert!(!visible_subtitle_changes(&ai, None, false));

        // 有自定义名:副段恒不显示
        let named = titled("pwsh", Some("~/repo/a"), Some("构建"));
        assert!(!visible_subtitle_changes(&named, Some("~/repo/b"), true));

        // 默认标题换默认标题:前后都没有副段
        let default = titled("pwsh", Some("pwsh"), None);
        assert!(!visible_subtitle_changes(
            &default,
            Some("PowerShell 7.5.3"),
            true
        ));
        // 默认标题 → 有信息的标题:副段出现
        assert!(visible_subtitle_changes(&default, Some("~/repo"), true));

        // 剥壳之后是同一个目录:显示的字一个没变
        let posh = titled("pwsh", Some("pwsh in mini-term"), None);
        assert!(!visible_subtitle_changes(
            &posh,
            Some("PWSH in mini-term"),
            true
        ));
    }

    #[test]
    fn 两段拼接() {
        assert_eq!(join_title_parts("pwsh", Some("~/repo")), "pwsh · ~/repo");
        assert_eq!(join_title_parts("pwsh", None), "pwsh");
        assert_eq!(join_title_parts("pwsh", Some("")), "pwsh");
    }

    fn remote_project(conn_id: &str) -> ProjectConfig {
        let mut p = project("r1", "远程项目");
        p.ssh_connection_id = Some(conn_id.to_string());
        p
    }

    fn connection(id: &str, name: &str) -> mt_config::SshConnection {
        mt_config::SshConnection {
            id: id.to_string(),
            name: name.to_string(),
            host: "example.com".into(),
            port: 22,
            user: "u".into(),
            password: None,
            identity_file: None,
            group: None,
            extra: Default::default(),
        }
    }

    /// 主段三档 × 副段:自定义名 > 远程连接名 > shell 名,副段只缀在后两档上。
    #[test]
    fn 标题两段各档() {
        let local = project("p1", "本地项目");
        let remote = remote_project("c1");
        let conns = vec![connection("c1", "生产机")];

        // 本地:主段 = shell 名,副段 = OSC 标题
        let pane = titled("pwsh", Some("~/repo/mini-term"), None);
        assert_eq!(
            pane_title_parts_of(Some(&local), &conns, &pane, true),
            ("pwsh".to_string(), Some("~/repo/mini-term".to_string()))
        );
        // 开关关掉:主段一字不变,副段没了
        assert_eq!(
            pane_title_parts_of(Some(&local), &conns, &pane, false),
            ("pwsh".to_string(), None)
        );

        // 远程:主段 = 连接名(不是 shell 名),副段照常
        assert_eq!(
            pane_title_parts_of(Some(&remote), &conns, &pane, true),
            ("生产机".to_string(), Some("~/repo/mini-term".to_string()))
        );
        // 断链(连接被删):主段回落 "ssh",不许退回 shell 名
        assert_eq!(
            pane_title_parts_of(Some(&remote), &[], &pane, true).0,
            "ssh"
        );

        // 自定义名:主段是它,副段**一律** None —— 用户命名了就尊重
        let named = titled("pwsh", Some("~/repo/mini-term"), Some("构建"));
        assert_eq!(
            pane_title_parts_of(Some(&local), &conns, &named, true),
            ("构建".to_string(), None)
        );
        assert_eq!(
            pane_title_parts_of(Some(&remote), &conns, &named, true),
            ("构建".to_string(), None),
            "自定义名也盖过远程连接名"
        );

        // 没有项目上下文:退到 shell 名
        assert_eq!(pane_title_parts_of(None, &conns, &pane, true).0, "pwsh");

        // 没报过标题 / 报的是默认标题:只有主段
        assert_eq!(
            pane_title_parts_of(Some(&local), &conns, &titled("pwsh", None, None), true),
            ("pwsh".to_string(), None)
        );
        assert_eq!(
            pane_title_parts_of(
                Some(&local),
                &conns,
                &titled("pwsh", Some("PowerShell 7.5.3"), None),
                true
            ),
            ("pwsh".to_string(), None)
        );

        // 拼成一行(`pane_display_label` 的口径)
        let (primary, sub) = pane_title_parts_of(Some(&local), &conns, &pane, true);
        assert_eq!(
            join_title_parts(&primary, sub.as_deref()),
            "pwsh · ~/repo/mini-term"
        );
    }

    /// 启动提示框的正文:没降级不弹;哪一路坏了就带上哪一路的详情,
    /// 两路都坏两段都在(配置那段在前)。
    #[test]
    fn 只读状态的提示正文按降级路数拼() {
        use crate::store::ReadOnlyState;

        let healthy = ReadOnlyState::default();
        assert!(!healthy.is_degraded());
        assert!(healthy.dialog_message().is_none(), "没降级不该弹框");

        let config_only = ReadOnlyState {
            config_error: Some("cfg-detail".into()),
            layout_error: None,
        };
        assert!(config_only.is_degraded());
        let msg = config_only.dialog_message().unwrap();
        assert!(msg.contains("cfg-detail"), "{msg}");

        let layout_only = ReadOnlyState {
            config_error: None,
            layout_error: Some("layout-detail".into()),
        };
        let msg = layout_only.dialog_message().unwrap();
        assert!(
            msg.contains("layout-detail") && !msg.contains("cfg-detail"),
            "{msg}"
        );

        let both = ReadOnlyState {
            config_error: Some("cfg-detail".into()),
            layout_error: Some("layout-detail".into()),
        };
        let msg = both.dialog_message().unwrap();
        let (cfg_at, layout_at) = (msg.find("cfg-detail"), msg.find("layout-detail"));
        assert!(cfg_at.is_some() && layout_at.is_some(), "{msg}");
        assert!(cfg_at < layout_at, "配置那段在前: {msg}");
    }

    /// 配置加载失败(手上是空默认配置)时布局库**只读不删**:拿空项目表去对账,
    /// 会把全部项目行当无主行删光,配置恢复后分屏树全丢。正常加载时照旧对账。
    #[test]
    fn 配置加载失败时不清布局库的项目行() {
        use mt_config::{SavedPane, SavedProjectLayout, SavedSplitNode, SavedTab};

        let dir = std::env::temp_dir().join(format!(
            "mt-app-layout-readonly-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let layouts = mt_layout::LayoutStore::open_at(&dir).unwrap();
        let saved = SavedProjectLayout {
            tabs: vec![SavedTab {
                custom_title: None,
                split_layout: SavedSplitNode::Leaf {
                    pane: None,
                    panes: vec![SavedPane {
                        shell_name: "cmd".into(),
                        cwd: None,
                        ai_session: None,
                    }],
                },
            }],
            active_tab_index: 0,
        };
        layouts.save_project_layout("p-real", &saved, 0).unwrap();

        let mut read_only = mt_config::AppConfig::default();
        assert!(read_only.projects.is_empty(), "前提:默认配置一个项目都没有");
        crate::store::apply_layout_db(&layouts, &mut read_only, false);
        assert!(
            layouts.load_project_layouts().contains_key("p-real"),
            "只读模式下不能删用户的布局"
        );

        let mut loaded = mt_config::AppConfig::default();
        crate::store::apply_layout_db(&layouts, &mut loaded, true);
        assert!(
            !layouts.load_project_layouts().contains_key("p-real"),
            "正常加载时不在项目表里的行照旧当无主行清掉"
        );

        drop(layouts);
        std::fs::remove_dir_all(&dir).ok();
    }
}
