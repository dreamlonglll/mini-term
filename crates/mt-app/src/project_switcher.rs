//! 项目快速切换器(Ctrl+Shift+P)。对应 `src/components/ProjectSwitcher.tsx`。
//!
//! 之前切项目只能用鼠标点侧栏,侧栏还能被折叠起来 —— 折叠状态下键盘完全够不到项目。
//! 匹配同时吃项目名和**分组路径**,「前端/webapp」这类同名项目靠分组区分。
//!
//! # 已开终端
//!
//! 查询非空时,项目行之后跟已开终端行([`TerminalRow`]),选中直接回到原 pane,不新建
//! 终端。查询为空时只有项目行,与只能切项目那会儿一样。
//!
//! 候选只从内存里的布局现算([`terminal_rows`]):不扫盘、不连远程、不起 PTY。起 PTY
//! 只发生在真的选中之后,那是切到项目 / 面板本来就有的懒创建。
//!
//! 匹配口径([`filter_terminals`]):页签主段、Agent、项目名、分组走子串 + 模糊;**OSC
//! 动态标题只走子串** —— 那是一整条目录路径之类的长串,子序列匹配随便几个字母都能中。
//! 用的是页签上**看得见**的那段(`pane_title_parts` 的副段,AI 在场时整个收起),
//! 搜到的就是看到的。排序不看状态灯:灯一变行就跳,游标底下的东西会被换掉。
//!
//! # 关窗与焦点
//!
//! `Root::close_dialog` 同步把焦点还给**打开弹窗前**的那个句柄。切到别的项目时还有
//! `WorkbenchArea` 换页那一拍把焦点要回来,跳到同项目的另一个 pane 就没有这一拍了,
//! 焦点会留在旧 pane 上 —— 所以跳 pane 必须**先关窗再激活**([`ProjectSwitcher::jump`])。
//!
//! # 键盘导航为什么要绕一圈
//!
//! `↑` / `↓` 在 gpui-component 的 `Input` 里是**绑成 action 的**
//! (`KeyBinding::new("up", MoveUp, Some("Input"))`),而 gpui 的按键派发是
//! 「先匹配 action、后跑 key 监听」,且单行输入框的 `MoveUp` 处理器直接 return
//! **不** `cx.propagate()` —— 于是挂在外层容器上的 `on_key_down` 一辈子收不到方向键。
//!
//! 破法是让自己的绑定与 `Input` 的绑定**同深度**:上下文谓词写成
//! `"ProjectSwitcher > Input"`(见 `main.rs` 的键位表)。`depth_of` 对
//! `Descendant` 返回的是最深那一层的深度,与裸 `"Input"` 打平;打平之后
//! `bindings_for_input` 按**注册顺序倒序**决胜负,而壳的 `cx.bind_keys` 跑在
//! `gpui_component::init` 之后 —— 所以是我们先派发,且不 propagate,`MoveUp`
//! 根本轮不到。这条链子有单测钉住(见本模块 `tests::方向键绑定压过输入框自带的`)。
//!
//! `Enter` 不走这条路:单行输入框的 `Enter` 处理器**会** `cx.propagate()` 并且
//! 无条件 `cx.emit(InputEvent::PressEnter)`,订阅它更直白。
//!
//! # Esc 与遮罩
//!
//! 走 `gpui_component::dialog::Dialog` 白拿:Esc(`Dialog` 的 `Cancel` action)、
//! 点遮罩、焦点还原都是它的既有行为,`open_guarded` 再补上防叠开与覆盖物栈登记。

use std::collections::HashMap;

use gpui::{
    AnyElement, App, AppContext, Context, Div, Entity, FontWeight, HighlightStyle, Hsla,
    InteractiveElement, IntoElement, ParentElement, Render, ScrollHandle, SharedString, Stateful,
    StatefulInteractiveElement, Styled, StyledText, Subscription, Window, actions, div,
    prelude::FluentBuilder, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use mt_config::{AppConfig, ProjectTreeItem};
use mt_project::search::find_substring_char_ranges;
use mt_ui::icons::{AiVendor, BrandIcon, VectorIcon};

use crate::i18n::t;
use crate::overlay::kind;
use crate::prompt::{autofocus, close_guarded, open_guarded};
use crate::store::AppStore;
use crate::tree::StatusLight;
use crate::ui;

actions!(
    mini_term,
    [
        /// 上一个候选(↑)。绑定在 `"ProjectSwitcher > Input"` 上,见模块注释。
        SwitcherPrev,
        /// 下一个候选(↓)。
        SwitcherNext,
    ]
);

/// 面板高度估算用的几个常数(与 `render` 里那几个 `px()` 一一对应)。
/// 顶部输入行 `py(10) ×2 + 输入框 ≈ 26`;底部提示条 `py(8) ×2 + 10px 文字 ≈ 17`;
/// 每行 `py(8) ×2 + 12px 主行 + 10px 副行`(行高按 1.5 算)。
const HEADER_H: f32 = 46.0;
const FOOTER_H: f32 = 33.0;
const LIST_PAD: f32 = 8.0;
const ROW_H: f32 = 49.0;
const EMPTY_H: f32 = 66.0;

/// 一个候选项目(已算好分组路径与命中位置)。
struct Row {
    id: String,
    name: String,
    path: String,
    group_path: Vec<String>,
    /// 项目聚合的状态灯档位(含第五档 attention,与项目列表同口径)。
    light: StatusLight,
    needs_attention: bool,
    /// 名字上的命中字符下标(**char** 口径);分组路径命中时为空 —— 与原版一致
    /// (`fuzzyMatch(path)` 命中时不高亮名字)。
    hits: Vec<usize>,
}

/// 一个已开终端候选。标题与灯都是页签上那份口径,模块注释有来龙去脉。
struct TerminalRow {
    project_id: String,
    pane_id: String,
    project_name: String,
    group_path: Vec<String>,
    /// 所在面板的名字。项目只有一个面板时为 `None`,不值得多占一段。
    panel: Option<String>,
    /// 页签主段:自定义名 > 远程连接名 > shell 名。
    title: String,
    /// 页签副段:OSC 动态标题,页签上不显示时为 `None`。
    osc_title: Option<String>,
    /// 页签上挂品牌图标的那家 Agent。
    agent: Option<String>,
    light: StatusLight,
    /// 就是当前键盘焦点所在的 pane。
    is_current: bool,
    /// 行上信息完全相同的终端里排第几(屏幕顺序,1 起)。`None` = 没有撞车的。
    duplicate: Option<usize>,
    hits: TerminalHits,
}

/// 终端行的命中区间(**char** 口径),只填决定了这一行排位的那一段。
#[derive(Debug, Default, PartialEq)]
struct TerminalHits {
    title: Vec<(usize, usize)>,
    osc_title: Vec<(usize, usize)>,
    agent: Vec<(usize, usize)>,
    project: Vec<(usize, usize)>,
}

/// 列表里的一行。
enum Candidate {
    Project(Row),
    Terminal(TerminalRow),
}

impl Candidate {
    fn target(&self) -> Target {
        match self {
            Self::Project(row) => Target::Project(row.id.clone()),
            Self::Terminal(row) => Target::Pane {
                project_id: row.project_id.clone(),
                pane_id: row.pane_id.clone(),
            },
        }
    }
}

/// 选中之后去哪。行上的点击闭包记身份而不是下标:闭包是上一帧建的,点下去时 pane
/// 可能已经关了、列表整体挪了位。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    Project(String),
    Pane { project_id: String, pane_id: String },
}

pub struct ProjectSwitcher {
    store: Entity<AppStore>,
    query: Entity<InputState>,
    /// 高亮到第几项。
    cursor: usize,
    /// 列表的滚动位置。行数一多(终端行)键盘移动要把游标带进可视区,行与列表的
    /// 子元素一一对应,所以游标就是子元素下标。
    list_scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

/// 打开切换器。已经开着时是空操作。
pub fn open(store: Entity<AppStore>, window: &mut Window, cx: &mut App) {
    // 守卫要在**建视图之前**判:`open_guarded` 拦下来时输入框已经建好、
    // `window.defer` 也排上了聚焦,而它永远不会被画出来 —— 焦点被送进虚空,
    // 终端从此收不到键。与 `prompt::show_prompt` 同一个坑。
    if crate::overlay::contains(crate::overlay::key(kind::PROJECT_SWITCHER)) {
        return;
    }
    let view = cx.new(|cx| ProjectSwitcher::new(store, window, cx));
    let input = view.read(cx).query.clone();

    open_guarded(kind::PROJECT_SWITCHER, window, cx, {
        let view = view.clone();
        move |dialog, window, cx| {
            // 原版是 `max-h-[60vh]`(内容少就矮一点)。`Dialog` 只吃固定高度,
            // 于是按候选条数估一个自然高度再夹到 60vh —— 常数是本文件下面那几个
            // 内边距/字号加出来的,估短了最后一行进滚动区,估长了下面留白,
            // 两种都不影响可用性。
            let count = view.read(cx).rows(cx).len();
            let natural = if count == 0 {
                HEADER_H + FOOTER_H + EMPTY_H
            } else {
                HEADER_H + FOOTER_H + LIST_PAD + count as f32 * ROW_H
            };
            let height = px(natural).min(window.viewport_size().height * 0.6);
            dialog
                // 头/尾都是自己画的分隔线,交给 Dialog 加边距会把线切断
                .p_0()
                // 原版这个浮层没有 ✕,所以关掉。(2026-09-19 补记:入口已挂
                // `gpui_kit_assets::Assets`,原先记的「`IconName::Close` 没有
                // svg 资产、渲染成空白」已作废 —— 它现在画得出来,更要关掉:
                // `p_0()` 之下上游 ✕ 会贴到 8,8,压在自绘的头部上)
                .close_button(false)
                // 原版 `panelClassName="w-[460px] max-h-[60vh]"` + `align="top"`
                // (`items-start pt-[10vh]`,正好是 Dialog 的默认 margin_top)
                .w(px(460.0))
                .child(div().h(height).child(view.clone()))
        }
    });

    // Dialog 打开时会把焦点抢到自己的面板上,所以聚焦输入框必须**排到它后面**
    // (判据全文见 `prompt::autofocus`)
    autofocus(&input, window, cx);
}

impl ProjectSwitcher {
    fn new(store: Entity<AppStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t("projectSwitcher", "placeholder"))
        });
        // `Enter` 不走 action:单行输入框的 `Enter` 处理器无条件
        // `cx.emit(InputEvent::PressEnter)`,订阅它比抢绑定直白。
        let sub = cx.subscribe_in(
            &query,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                // 关键词一变就把游标拉回第一项(原版 onChange 里那句 setCursor(0))
                InputEvent::Change => {
                    this.cursor = 0;
                    this.list_scroll.scroll_to_item(0);
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.commit(window, cx),
                _ => {}
            },
        );
        // 开着的时候 pane 可能被关掉、灯可能在变:候选是渲染时现算的,这里只负责重画
        let store_sub = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            query,
            cursor: 0,
            list_scroll: ScrollHandle::new(),
            _subs: vec![sub, store_sub],
        }
    }

    /// 当前候选集。每帧现算 —— 项目状态(AI 灯/完成标)随时在变,缓存反而要
    /// 额外的失效通道。项目数是几十条量级,重算成本可以忽略。
    fn rows(&self, cx: &App) -> Vec<Candidate> {
        let store = self.store.read(cx);
        let config = store.config();
        let query = self.query.read(cx).value().to_string();
        let flat = projects_with_group_path(config);

        let projects: Vec<Row> = flat
            .iter()
            .filter_map(|(id, group_path)| {
                let project = config.projects.iter().find(|p| p.id == *id)?;
                let state = store.project_state(id);
                Some(Row {
                    id: project.id.clone(),
                    name: project.name.clone(),
                    path: project.path.clone(),
                    group_path: group_path.clone(),
                    light: state.map(|s| s.highest_light()).unwrap_or_default(),
                    needs_attention: state.is_some_and(|s| s.needs_attention),
                    hits: Vec::new(),
                })
            })
            .collect();
        // 空查询连 pane 都不必走一遍:结果里本来就没有终端行
        let terminals = if query.trim().is_empty() {
            Vec::new()
        } else {
            terminal_rows(store, &flat)
        };

        build_candidates(projects, terminals, &query)
    }

    fn move_cursor(&mut self, delta: i32, cx: &mut Context<Self>) {
        let len = self.rows(cx).len();
        self.cursor = next_cursor(self.cursor, len, delta);
        self.list_scroll.scroll_to_item(self.cursor);
        cx.notify();
    }

    /// 去高亮的那一行(回车)。
    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        let cursor = self.cursor.min(rows.len().saturating_sub(1));
        let Some(target) = rows.get(cursor).map(Candidate::target) else {
            return;
        };
        self.jump(target, window, cx);
    }

    /// 切到目标并关掉浮层。
    fn jump(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        match target {
            Target::Project(id) => {
                self.store
                    .update(cx, |store, cx| store.set_active_project(&id, cx));
                // 侧栏里那一行可能藏在折叠组里 / 可视区外,一并揭示
                crate::project_list::reveal(&id, cx);
                // 关闭排到本轮 effect 之后:这会儿本实体正被 Dialog 的 builder 持有着,
                // 当场 `close_dialog` 等于在自己脚下抽地毯
                window.defer(cx, |window, cx| {
                    close_guarded(kind::PROJECT_SWITCHER, window, cx);
                });
            }
            Target::Pane {
                project_id,
                pane_id,
            } => {
                // 点的是上一帧画的行,这一下之前 pane 可能已经关了:弹窗留着,列表刷新
                let alive = self
                    .store
                    .read(cx)
                    .project_state(&project_id)
                    .is_some_and(|state| state.pane(&pane_id).is_some());
                if !alive {
                    cx.notify();
                    return;
                }
                let store = self.store.clone();
                // 先关窗再激活,理由见模块注释「关窗与焦点」
                window.defer(cx, move |window, cx| {
                    close_guarded(kind::PROJECT_SWITCHER, window, cx);
                    store.update(cx, |store, cx| {
                        store.jump_to_pane(&project_id, &pane_id, window, cx);
                    });
                    // 当前可能停在文档页,不切页的话焦点进了看不见的终端
                    crate::workbench_area::activate_terminal_page(window, cx);
                    // 那个项目在「项目」块里也得看得见(折叠组里 / 可视区外)
                    crate::project_list::reveal(&project_id, cx);
                });
            }
        }
    }
}

/// 从内存布局收齐全部已开终端,顺序同屏幕:项目按侧栏,面板按竖条,pane 按分屏树。
///
/// 恢复出来还没起 PTY 的 pane 也算:它们就是用户的页签,选中时才由切项目 / 切面板
/// 那条懒创建补起来。
fn terminal_rows(store: &AppStore, flat: &[(String, Vec<String>)]) -> Vec<TerminalRow> {
    let config = store.config();
    let auto_resume = config.ai_auto_resume.unwrap_or(true);
    let focused = store.focused_pane_id.as_deref();
    let mut rows = Vec::new();
    for (project_id, group_path) in flat {
        let Some(project) = config.projects.iter().find(|p| p.id == *project_id) else {
            continue;
        };
        let Some(state) = store.project_state(project_id) else {
            continue;
        };
        for panel in &state.panels {
            let panel_name = (state.panels.len() > 1)
                .then(|| crate::terminals_panel::panel_title(state, &panel.id));
            for pane in panel.layout.panes() {
                let (title, osc_title) = store.pane_title_parts(project_id, pane);
                // 与页签的品牌图标同一个判据:AI 退出后留着备查的身份不算
                let agent = pane
                    .shows_ai_session(auto_resume)
                    .then(|| pane.ai_agent())
                    .flatten()
                    .map(str::to_string);
                rows.push(TerminalRow {
                    project_id: project_id.clone(),
                    pane_id: pane.id.clone(),
                    project_name: project.name.clone(),
                    group_path: group_path.clone(),
                    panel: panel_name.clone(),
                    title,
                    osc_title,
                    agent,
                    light: pane.light(),
                    is_current: focused == Some(pane.id.as_str()),
                    duplicate: None,
                    hits: TerminalHits::default(),
                });
            }
        }
    }
    number_identical(&mut rows);
    rows
}

// ─── 纯逻辑(可测) ────────────────────────────────────────────

/// 给行上信息完全相同的终端编号,否则同一面板里两个没改过名的 `pwsh` 无从分辨。
/// 在过滤之前按屏幕顺序编:查询变了,同一个 pane 的序号不跟着变。
fn number_identical(rows: &mut [TerminalRow]) {
    fn identity(row: &TerminalRow) -> [Option<&str>; 5] {
        [
            Some(row.project_id.as_str()),
            row.panel.as_deref(),
            Some(row.title.as_str()),
            row.osc_title.as_deref(),
            row.agent.as_deref(),
        ]
    }
    let mut totals: HashMap<_, usize> = HashMap::new();
    for row in rows.iter() {
        *totals.entry(identity(row)).or_default() += 1;
    }
    let mut seen: HashMap<_, usize> = HashMap::new();
    let numbers: Vec<Option<usize>> = rows
        .iter()
        .map(|row| {
            let key = identity(row);
            (totals[&key] > 1).then(|| {
                let nth = seen.entry(key).or_default();
                *nth += 1;
                *nth
            })
        })
        .collect();
    for (row, number) in rows.iter_mut().zip(numbers) {
        row.duplicate = number;
    }
}

/// 子序列模糊匹配(`mt` 命中 `mini-term`),返回命中位置用于高亮。
///
/// 逐条照抄原版 `fuzzyMatch`:大小写不敏感、按顺序贪心取**最早**的那个位置、
/// 空查询返回「全中但不高亮」(空 `hits`)。
///
/// 下标是 **char** 口径(原版是 JS 的 UTF-16 码元口径),中文项目名下才不会
/// 把高亮切在半个字上。
pub fn fuzzy_match(text: &str, query: &str) -> Option<Vec<usize>> {
    if query.is_empty() {
        return Some(Vec::new());
    }
    let haystack: Vec<char> = text.to_lowercase().chars().collect();
    let mut hits = Vec::new();
    let mut from = 0usize;
    for ch in query.to_lowercase().chars() {
        let found = haystack[from.min(haystack.len())..]
            .iter()
            .position(|c| *c == ch)?
            + from.min(haystack.len());
        hits.push(found);
        from = found + 1;
    }
    Some(hits)
}

/// 大小写不敏感的子串命中(`query_lower` 已小写化),全部区间,**char** 口径。
fn substring_ranges(text: &str, query_lower: &str) -> Option<Vec<(usize, usize)>> {
    let ranges = find_substring_char_ranges(text, query_lower);
    (!ranges.is_empty()).then_some(ranges)
}

/// [`fuzzy_match`] 的区间形态,与 [`substring_ranges`] 同一个返回口径。
fn fuzzy_ranges(text: &str, query: &str) -> Option<Vec<(usize, usize)>> {
    fuzzy_match(text, query).map(|hits| hits.into_iter().map(|i| (i, i + 1)).collect())
}

/// 按查询过滤 + 标注命中位置 + 排序。空查询 = 全都要、都不高亮、保持侧栏顺序。
///
/// 排序只分档,同档保持侧栏顺序:名字前缀 > 名字子串 > 名字模糊 > 分组路径。
/// `api` 既能模糊中 `a-p-i-demo` 也能子串中 `my-api`,连着写的那个才是想要的。
fn filter_rows(all: Vec<Row>, query: &str) -> Vec<Row> {
    let query = query.trim();
    if query.is_empty() {
        return all;
    }
    let query_lower = query.to_lowercase();
    let mut ranked: Vec<(u8, Row)> = all
        .into_iter()
        .filter_map(|mut row| {
            if let Some(ranges) = substring_ranges(&row.name, &query_lower) {
                let rank = u8::from(ranges[0].0 != 0);
                row.hits = ranges
                    .into_iter()
                    .flat_map(|(start, end)| start..end)
                    .collect();
                return Some((rank, row));
            }
            if let Some(hits) = fuzzy_match(&row.name, query) {
                row.hits = hits;
                return Some((2, row));
            }
            // 名字没中就试分组路径,命中时不高亮名字(与原版一致)
            let path = row.group_path.join("/");
            fuzzy_match(&path, query).map(|_| (3, row))
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, row)| row).collect()
}

/// 终端行的命中档位(越小越靠前),顺手把决定排位的那一段的区间填进 `row.hits`。
/// `None` = 不匹配。
///
/// 档位:自己的名字 > Agent > 动态标题 > 所在项目;同一段里子串压过模糊。OSC 标题只有
/// 子串这一档,见模块注释。
fn rank_terminal(row: &mut TerminalRow, query: &str, query_lower: &str) -> Option<u8> {
    if let Some(ranges) = substring_ranges(&row.title, query_lower) {
        let rank = u8::from(ranges[0].0 != 0);
        row.hits.title = ranges;
        return Some(rank);
    }
    if let Some(ranges) = row
        .agent
        .as_deref()
        .and_then(|agent| substring_ranges(agent, query_lower))
    {
        row.hits.agent = ranges;
        return Some(2);
    }
    if let Some(ranges) = row
        .osc_title
        .as_deref()
        .and_then(|title| substring_ranges(title, query_lower))
    {
        row.hits.osc_title = ranges;
        return Some(3);
    }
    if let Some(ranges) = substring_ranges(&row.project_name, query_lower) {
        row.hits.project = ranges;
        return Some(4);
    }
    if let Some(ranges) = fuzzy_ranges(&row.title, query) {
        row.hits.title = ranges;
        return Some(5);
    }
    if let Some(ranges) = row
        .agent
        .as_deref()
        .and_then(|agent| fuzzy_ranges(agent, query))
    {
        row.hits.agent = ranges;
        return Some(6);
    }
    if let Some(ranges) = fuzzy_ranges(&row.project_name, query) {
        row.hits.project = ranges;
        return Some(7);
    }
    // 分组路径的命中不高亮,与项目行同一口径
    fuzzy_match(&row.group_path.join("/"), query).map(|_| 8)
}

/// 过滤并排序已开终端。**空查询一行都没有**;同档保持屏幕顺序。
fn filter_terminals(all: Vec<TerminalRow>, query: &str) -> Vec<TerminalRow> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let query_lower = query.to_lowercase();
    let mut ranked: Vec<(u8, TerminalRow)> = all
        .into_iter()
        .filter_map(|mut row| {
            let rank = rank_terminal(&mut row, query, &query_lower)?;
            Some((rank, row))
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, row)| row).collect()
}

/// 最终的行表:项目行在前,终端行在后。
fn build_candidates(
    projects: Vec<Row>,
    terminals: Vec<TerminalRow>,
    query: &str,
) -> Vec<Candidate> {
    filter_rows(projects, query)
        .into_iter()
        .map(Candidate::Project)
        .chain(
            filter_terminals(terminals, query)
                .into_iter()
                .map(Candidate::Terminal),
        )
        .collect()
}

/// 游标环形移动。列表为空时钉在 0。
pub fn next_cursor(cursor: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    let len_i = len as i64;
    let next = (cursor as i64 + delta as i64).rem_euclid(len_i);
    next as usize
}

/// 展平 `projectTree`,给每个项目算出它的分组路径。对应
/// `src/utils/projectTree.ts::getProjectsWithGroupPath`。
///
/// 不在树里的项目(异常配置 / 还没写过 projectTree 的旧配置)追加到末尾、分组路径为空,
/// 与原版 `getOrderedTree` 的兜底口径一致。
pub fn projects_with_group_path(config: &AppConfig) -> Vec<(String, Vec<String>)> {
    fn walk(
        items: &[ProjectTreeItem],
        group_path: &[String],
        out: &mut Vec<(String, Vec<String>)>,
        seen: &mut Vec<String>,
    ) {
        for item in items {
            match item {
                ProjectTreeItem::Group(group) => {
                    let mut deeper = group_path.to_vec();
                    deeper.push(group.name.clone());
                    walk(&group.children, &deeper, out, seen);
                }
                ProjectTreeItem::ProjectId(id) => {
                    if seen.iter().any(|s| s == id) {
                        continue;
                    }
                    seen.push(id.clone());
                    out.push((id.clone(), group_path.to_vec()));
                }
            }
        }
    }

    let mut out = Vec::new();
    let mut seen = Vec::new();
    if let Some(tree) = config.project_tree.as_ref() {
        walk(tree, &[], &mut out, &mut seen);
    }
    // 树里没有的(以及树里有、projects 里没有的那些 id 会在上层 find 时被丢掉)
    for project in &config.projects {
        if !seen.iter().any(|s| *s == project.id) {
            out.push((project.id.clone(), Vec::new()));
        }
    }
    out
}

/// 第二行那句「分组 / 分组 · 路径」。分组为空时不带前缀,与原版三元一致。
pub fn subtitle(group_path: &[String], path: &str) -> String {
    if group_path.is_empty() {
        path.to_string()
    } else {
        format!("{} · {path}", group_path.join(" / "))
    }
}

/// 终端行第二行那句「分组 / 分组 · 项目 · 面板」,同名终端靠它区分。另外返回项目名在
/// 这句里的起始 char 下标:命中区间是按项目名算的,高亮要平移过去。
fn terminal_context(
    group_path: &[String],
    project_name: &str,
    panel: Option<&str>,
) -> (String, usize) {
    let mut text = String::new();
    if !group_path.is_empty() {
        text.push_str(&group_path.join(" / "));
        text.push_str(" · ");
    }
    let project_start = text.chars().count();
    text.push_str(project_name);
    if let Some(panel) = panel {
        text.push_str(" · ");
        text.push_str(panel);
    }
    (text, project_start)
}

// ─── 渲染 ─────────────────────────────────────────────────────

/// 底部提示条里那颗键帽(`styles.css` 的 `.kbd`)。
fn kbd(label: &'static str) -> impl IntoElement {
    div()
        .px(px(5.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .bg(ui::bg_elevated())
        .border_1()
        .border_color(ui::border_default())
        .text_size(ui::font_px(10.0))
        .text_color(ui::text_secondary())
        .child(label)
}

fn hint(keys: &[&'static str], label: &'static str) -> impl IntoElement {
    let mut row = div().flex().items_center().gap(px(2.0));
    for key in keys {
        row = row.child(kbd(key));
    }
    row.child(
        div()
            .ml(px(4.0))
            .text_size(ui::font_px(10.0))
            .text_color(ui::text_muted())
            .child(label),
    )
}

/// 往一行文字里追加带命中高亮的片段。逐段发元素(与原版 `Highlight` 逐字发 span
/// 同法,只是把相邻段合并了,见 `ui::highlight_runs`)。
fn push_highlighted(
    mut line: Div,
    text: &str,
    ranges: &[(usize, usize)],
    size: f32,
    base: Hsla,
) -> Div {
    for (run, hit) in ui::highlight_runs(text, ranges) {
        line = line.child(
            div()
                .flex_none()
                .text_size(ui::font_px(size))
                .when(hit, |el| {
                    el.text_color(ui::accent())
                        .font_weight(FontWeight::SEMIBOLD)
                })
                .when(!hit, |el| el.text_color(base))
                .child(SharedString::from(run)),
        );
    }
    line
}

/// 放不下时出省略号的带高亮文字,给动态标题这种长度没谱的串。[`push_highlighted`] 逐段
/// 发元素,尾巴只能被硬裁。
fn truncated_highlighted(text: &str, ranges: &[(usize, usize)], size: f32, base: Hsla) -> Div {
    let style = HighlightStyle {
        color: Some(ui::accent()),
        font_weight: Some(FontWeight::SEMIBOLD),
        ..Default::default()
    };
    let highlights: Vec<_> = ranges
        .iter()
        .filter_map(|&(start, end)| crate::git_branches::char_range_to_bytes(text, start, end))
        .map(|range| (range, style))
        .collect();
    div()
        .min_w_0()
        .truncate()
        .text_size(ui::font_px(size))
        .text_color(base)
        .child(StyledText::new(text.to_string()).with_highlights(highlights))
}

/// 名字后面那个「(当前)」。
fn current_tag() -> impl IntoElement {
    div()
        .ml(px(6.0))
        .flex_none()
        .text_size(ui::font_px(10.0))
        .text_color(ui::text_muted())
        .child(t("projectSwitcher", "current"))
}

impl ProjectSwitcher {
    /// 行的外壳:项目行与终端行的版面、高亮、悬停、点击都是这一份。
    fn row_shell(
        &self,
        id: impl Into<SharedString>,
        idx: usize,
        is_cursor: bool,
        target: Target,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(id.into())
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .py(px(8.0))
            .mx(px(4.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .when(is_cursor, |el| el.bg(ui::accent_subtle()))
            .when(!is_cursor, |el| el.hover(|el| el.bg(ui::border_subtle())))
            // 原版是 onMouseEnter 把游标挪过来
            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                if *hovered && this.cursor != idx {
                    this.cursor = idx;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _event, window, cx| {
                this.cursor = idx;
                this.jump(target.clone(), window, cx);
            }))
    }

    fn render_project_row(
        &self,
        row: &Row,
        idx: usize,
        is_cursor: bool,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // 名字 + 命中高亮
        let ranges: Vec<(usize, usize)> = row.hits.iter().map(|i| (*i, *i + 1)).collect();
        let base = if is_cursor {
            ui::accent()
        } else {
            ui::text_primary()
        };
        let mut name_line = push_highlighted(
            div().flex().items_center().overflow_hidden(),
            &row.name,
            &ranges,
            12.0,
            base,
        );
        if is_active {
            name_line = name_line.child(current_tag());
        }

        self.row_shell(
            format!("switcher-{}", row.id),
            idx,
            is_cursor,
            Target::Project(row.id.clone()),
            cx,
        )
        .child(
            div().flex_1().overflow_hidden().child(name_line).child(
                div()
                    .truncate()
                    .text_size(ui::font_px(10.0))
                    .text_color(ui::text_muted())
                    .child(subtitle(&row.group_path, &row.path)),
            ),
        )
        .when(row.needs_attention, |el| {
            el.child(
                div()
                    .flex_none()
                    .text_size(ui::font_px(10.0))
                    .text_color(ui::color_success())
                    .child(t("panels", "done")),
            )
        })
        .when(row.light != StatusLight::Idle, |el| {
            el.child(div().flex_none().child(ui::status_dot(row.light)))
        })
        .into_any_element()
    }

    /// 终端行:版面与项目行同一份,多一颗打头的图标(品牌 / 终端字形),一眼分得清
    /// 哪些是终端。灯总是画:idle 的空心圈也是一种状态,项目行不画是因为它是聚合。
    fn render_terminal_row(
        &self,
        row: &TerminalRow,
        idx: usize,
        is_cursor: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let base = if is_cursor {
            ui::accent()
        } else {
            ui::text_primary()
        };
        let mut title_line = push_highlighted(
            div().flex().items_center().overflow_hidden(),
            &row.title,
            &row.hits.title,
            12.0,
            base,
        );
        if let Some(number) = row.duplicate {
            title_line = title_line.child(
                div()
                    .ml(px(6.0))
                    .flex_none()
                    .text_size(ui::font_px(10.0))
                    .text_color(ui::text_muted())
                    .child(SharedString::from(format!("#{number}"))),
            );
        }
        if let Some(agent) = row.agent.as_deref() {
            title_line = title_line.child(push_highlighted(
                div().ml(px(6.0)).flex().flex_none().items_center(),
                agent,
                &row.hits.agent,
                10.0,
                ui::text_muted(),
            ));
        }
        if row.is_current {
            title_line = title_line.child(current_tag());
        }
        if let Some(osc_title) = row.osc_title.as_deref() {
            title_line = title_line.child(
                div()
                    .mx(px(6.0))
                    .flex_none()
                    .text_size(ui::font_px(10.0))
                    .text_color(ui::text_muted())
                    .child("·"),
            );
            title_line = title_line.child(truncated_highlighted(
                osc_title,
                &row.hits.osc_title,
                10.0,
                ui::text_muted(),
            ));
        }

        let (context, project_start) =
            terminal_context(&row.group_path, &row.project_name, row.panel.as_deref());
        let context_line = if row.hits.project.is_empty() {
            div()
                .truncate()
                .text_size(ui::font_px(10.0))
                .text_color(ui::text_muted())
                .child(context)
                .into_any_element()
        } else {
            let ranges: Vec<(usize, usize)> = row
                .hits
                .project
                .iter()
                .map(|(start, end)| (start + project_start, end + project_start))
                .collect();
            truncated_highlighted(&context, &ranges, 10.0, ui::text_muted()).into_any_element()
        };

        let icon_ink = ui::text_muted();
        let vendor = row.agent.as_deref().and_then(|agent| {
            AiVendor::from_session_type(agent).or_else(|| AiVendor::infer(Some(agent), None))
        });
        self.row_shell(
            format!("switcher-pane-{}-{}", row.project_id, row.pane_id),
            idx,
            is_cursor,
            Target::Pane {
                project_id: row.project_id.clone(),
                pane_id: row.pane_id.clone(),
            },
            cx,
        )
        .child(
            div().flex_none().child(match vendor {
                Some(vendor) => BrandIcon::new(Some(vendor))
                    .size(px(16.0))
                    .color(icon_ink)
                    .into_any_element(),
                None => VectorIcon::new(crate::terminals_panel::PANEL_ICON, px(16.0))
                    .ink(icon_ink)
                    .into_any_element(),
            }),
        )
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .child(title_line)
                .child(context_line),
        )
        .child(div().flex_none().child(ui::status_dot(row.light)))
        .into_any_element()
    }
}

impl Render for ProjectSwitcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows(cx);
        let active_id = self.store.read(cx).active_project_id.clone();
        // 候选变少之后游标可能越界(原版是一个 useEffect 夹一次)
        let cursor = self.cursor.min(rows.len().saturating_sub(1));

        let mut list = div()
            .id("project-switcher-list")
            .track_scroll(&self.list_scroll)
            .flex_1()
            .overflow_y_scroll()
            .py(px(4.0));

        if rows.is_empty() {
            list = list.child(
                div()
                    .px(px(16.0))
                    .py(px(24.0))
                    .text_center()
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(t("projectSwitcher", "noMatch")),
            );
        }

        for (idx, row) in rows.iter().enumerate() {
            let is_cursor = idx == cursor;
            list = list.child(match row {
                Candidate::Project(row) => {
                    let is_active = Some(&row.id) == active_id.as_ref();
                    self.render_project_row(row, idx, is_cursor, is_active, cx)
                }
                Candidate::Terminal(row) => self.render_terminal_row(row, idx, is_cursor, cx),
            });
        }

        div()
            // 方向键的上下文锚点。键位表在 `main.rs`,谓词是
            // `"ProjectSwitcher > Input"` —— 为什么非这么写见模块注释。
            .key_context("ProjectSwitcher")
            .on_action(cx.listener(|this, _: &SwitcherPrev, _window, cx| {
                this.move_cursor(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitcherNext, _window, cx| {
                this.move_cursor(1, cx);
            }))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .border_b_1()
                    .border_color(ui::border_subtle())
                    .child(Input::new(&self.query).cleanable(false)),
            )
            .child(list)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .flex_none()
                    .px(px(12.0))
                    .py(px(8.0))
                    .border_t_1()
                    .border_color(ui::border_subtle())
                    .child(hint(&["↑", "↓"], t("projectSwitcher", "hintMove")))
                    .child(hint(&["Enter"], t("projectSwitcher", "hintOpen")))
                    .child(hint(&["Esc"], t("projectSwitcher", "hintClose"))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{KeyBinding, KeyContext, Keymap, Keystroke};

    /// 子序列匹配:命中位置是**最早**的那一串,大小写不敏感。
    #[test]
    fn 模糊匹配取最早的子序列() {
        assert_eq!(fuzzy_match("mini-term", "mt"), Some(vec![0, 5]));
        assert_eq!(fuzzy_match("mini-term", "MT"), Some(vec![0, 5]));
        assert_eq!(fuzzy_match("mini-term", "mini"), Some(vec![0, 1, 2, 3]));
        assert_eq!(fuzzy_match("mini-term", "xyz"), None);
        // 顺序不对不算命中
        assert_eq!(fuzzy_match("abc", "cb"), None);
        // 空查询 = 全中且不高亮
        assert_eq!(fuzzy_match("abc", ""), Some(Vec::new()));
    }

    /// 下标按 char 计:中文名下高亮不会切在半个字上。
    #[test]
    fn 模糊匹配下标按字符计() {
        assert_eq!(fuzzy_match("前端项目", "项目"), Some(vec![2, 3]));
    }

    /// 名字没中就试分组路径,且此时**不高亮名字**。
    #[test]
    fn 分组路径也参与匹配() {
        let make = |name: &str, group: &[&str]| Row {
            id: name.to_string(),
            name: name.to_string(),
            path: "/tmp".into(),
            group_path: group.iter().map(|s| s.to_string()).collect(),
            light: StatusLight::Idle,
            needs_attention: false,
            hits: Vec::new(),
        };
        let all = vec![make("webapp", &["前端"]), make("server", &["后端"])];
        let hit = filter_rows(all, "前端");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].name, "webapp");
        assert!(hit[0].hits.is_empty(), "分组命中时不高亮名字");

        let all = vec![make("webapp", &["前端"])];
        let hit = filter_rows(all, "web");
        assert_eq!(hit[0].hits, vec![0, 1, 2]);

        // 空查询 = 全都要、保持顺序
        let all = vec![make("a", &[]), make("b", &[])];
        assert_eq!(filter_rows(all, "  ").len(), 2);
    }

    fn project_row(name: &str, group: &[&str]) -> Row {
        Row {
            id: name.to_string(),
            name: name.to_string(),
            path: "/tmp".into(),
            group_path: group.iter().map(|s| s.to_string()).collect(),
            light: StatusLight::Idle,
            needs_attention: false,
            hits: Vec::new(),
        }
    }

    fn terminal_row(project: &str, group: &[&str], title: &str) -> TerminalRow {
        TerminalRow {
            project_id: project.to_string(),
            pane_id: format!("{project}/{title}"),
            project_name: project.to_string(),
            group_path: group.iter().map(|s| s.to_string()).collect(),
            panel: None,
            title: title.to_string(),
            osc_title: None,
            agent: None,
            light: StatusLight::Idle,
            is_current: false,
            duplicate: None,
            hits: TerminalHits::default(),
        }
    }

    fn terminal_titles(rows: &[TerminalRow]) -> Vec<&str> {
        rows.iter().map(|r| r.title.as_str()).collect()
    }

    /// 只有行上什么都一样的才编号,按屏幕顺序;换了项目、面板或动态标题就分得开了。
    #[test]
    fn 信息完全相同的终端才编号() {
        let mut other_panel = terminal_row("p", &[], "pwsh");
        other_panel.panel = Some("面板 2".into());
        let mut with_title = terminal_row("p", &[], "pwsh");
        with_title.osc_title = Some("D:/work".into());
        let mut rows = vec![
            terminal_row("p", &[], "pwsh"),
            other_panel,
            with_title,
            terminal_row("q", &[], "pwsh"),
            terminal_row("p", &[], "pwsh"),
        ];

        number_identical(&mut rows);

        let numbers: Vec<_> = rows.iter().map(|row| row.duplicate).collect();
        assert_eq!(numbers, [Some(1), None, None, None, Some(2)]);
    }

    /// 同档按子串压过模糊、前缀压过子串,都不中才看分组;同档保持侧栏顺序。
    #[test]
    fn 项目行按命中质量排序且同档保序() {
        let all = vec![
            project_row("a-p-i-demo", &[]),
            project_row("my-api", &[]),
            project_row("other", &["api组"]),
            project_row("api-gateway", &[]),
            project_row("your-api", &[]),
        ];
        let names: Vec<String> = filter_rows(all, "api")
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(
            names,
            ["api-gateway", "my-api", "your-api", "a-p-i-demo", "other"]
        );

        // 子串命中高亮连着的那段,不是模糊匹配最早的那串
        let hit = filter_rows(vec![project_row("a-p-i-api", &[])], "api");
        assert_eq!(hit[0].hits, vec![6, 7, 8]);
    }

    /// 空查询:只有项目行、保持侧栏顺序、谁都不高亮 —— 与只能切项目那会儿一样。
    #[test]
    fn 空查询没有终端行() {
        for query in ["", "   "] {
            let rows = build_candidates(
                vec![project_row("b", &[]), project_row("a", &[])],
                vec![terminal_row("b", &[], "pwsh")],
                query,
            );
            assert_eq!(rows.len(), 2, "{query:?}");
            assert!(
                matches!(&rows[0], Candidate::Project(r) if r.name == "b" && r.hits.is_empty())
            );
            assert!(matches!(&rows[1], Candidate::Project(r) if r.name == "a"));
        }
    }

    /// 终端行再贴切也排在项目行后面:项目行的位置不能被它挤动。
    #[test]
    fn 项目行在前终端行在后() {
        let rows = build_candidates(
            vec![project_row("a-pwsh-b", &[])],
            vec![
                terminal_row("other", &[], "pwsh"),
                terminal_row("a-pwsh-b", &[], "cmd"),
            ],
            "pwsh",
        );
        assert_eq!(rows.len(), 3);
        assert!(matches!(&rows[0], Candidate::Project(_)));
        assert!(matches!(&rows[1], Candidate::Terminal(r) if r.title == "pwsh"));
        assert!(matches!(&rows[2], Candidate::Terminal(r) if r.title == "cmd"));
    }

    /// 同名终端:第二行带分组与项目(多面板时带面板),一眼分得开;去向也各是各的。
    #[test]
    fn 同名终端靠上下文区分() {
        let mut in_panel = terminal_row("webapp", &["后端"], "pwsh");
        in_panel.panel = Some("面板 2".into());
        let rows = filter_terminals(
            vec![
                terminal_row("webapp", &["前端", "实验"], "pwsh"),
                in_panel,
                terminal_row("server", &[], "pwsh"),
            ],
            "pwsh",
        );
        assert_eq!(rows.len(), 3);
        let contexts: Vec<String> = rows
            .iter()
            .map(|r| terminal_context(&r.group_path, &r.project_name, r.panel.as_deref()).0)
            .collect();
        assert_eq!(
            contexts,
            ["前端 / 实验 · webapp", "后端 · webapp · 面板 2", "server"]
        );
        let targets: Vec<Target> = rows
            .into_iter()
            .map(|r| Candidate::Terminal(r).target())
            .collect();
        assert_eq!(targets.len(), 3);
        assert_ne!(targets[0], targets[2]);
        assert!(matches!(&targets[0], Target::Pane { project_id, .. } if project_id == "webapp"));
    }

    /// 项目名的命中要平移到第二行那句里:前面还有分组前缀,而且按 char 算。
    #[test]
    fn 项目名高亮平移到上下文行() {
        let (text, start) = terminal_context(&["前端".into()], "webapp", Some("面板 2"));
        assert_eq!(text, "前端 · webapp · 面板 2");
        assert_eq!(start, 5);
        let rows = filter_terminals(vec![terminal_row("webapp", &["前端"], "pwsh")], "web");
        assert_eq!(rows[0].hits.project, vec![(0, 3)]);
        let (_, no_group) = terminal_context(&[], "webapp", None);
        assert_eq!(no_group, 0);
    }

    /// 长 OSC 标题:子序列随便都能中,所以只认连着的子串。
    #[test]
    fn 长_osc_标题不走模糊() {
        let mut row = terminal_row("zzz", &[], "cmd");
        row.osc_title = Some("D:/Git/mini-term/crates/mt-app/src/project_switcher.rs".into());
        let filter = |row: TerminalRow, query: &str| filter_terminals(vec![row], query);

        // 这几个都是标题的子序列,走模糊就全中了
        for query in ["dgit", "mtsw", "gtcas"] {
            let probe = TerminalRow {
                osc_title: row.osc_title.clone(),
                ..terminal_row("zzz", &[], "cmd")
            };
            assert!(
                fuzzy_match(probe.osc_title.as_deref().unwrap(), query).is_some(),
                "{query} 得是子序列,这条测试才有意义"
            );
            assert!(filter(probe, query).is_empty(), "{query} 不该中");
        }

        // 连着的子串中,大小写不敏感,区间落在 OSC 那一段
        let hit = filter(row, "Project_Switcher");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].hits.osc_title, vec![(35, 51)]);
        assert!(hit[0].hits.title.is_empty());
    }

    /// 固定的短名字照样走模糊:标题、Agent、项目名。
    #[test]
    fn 固定标题与_agent_与项目名走模糊() {
        let mut row = terminal_row("mini-term", &[], "我的构建窗口");
        row.agent = Some("claude".into());
        let probe = |query: &str| {
            let mut copy = terminal_row("mini-term", &[], "我的构建窗口");
            copy.agent = row.agent.clone();
            filter_terminals(vec![copy], query)
        };
        assert_eq!(probe("构窗")[0].hits.title, vec![(2, 3), (4, 5)]);
        assert_eq!(probe("cld")[0].hits.agent, vec![(0, 1), (1, 2), (4, 5)]);
        assert_eq!(probe("mtm")[0].hits.project, vec![(0, 1), (5, 6), (8, 9)]);
        assert!(probe("xyz").is_empty());
    }

    /// 分组也能找到终端,但不高亮(与项目行同一口径)。
    #[test]
    fn 分组路径命中终端不高亮() {
        let rows = filter_terminals(vec![terminal_row("web", &["前端"], "pwsh")], "前端");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hits, TerminalHits::default());
    }

    /// 排位:自己的名字 > Agent > 动态标题 > 所在项目,子串压过模糊,同档保持屏幕顺序。
    #[test]
    fn 终端行按命中段排序() {
        let mut by_agent = terminal_row("zz", &[], "agent 命中");
        by_agent.agent = Some("api-agent".into());
        let mut by_osc = terminal_row("zz", &[], "osc 命中");
        by_osc.osc_title = Some("run api server".into());
        let rows = filter_terminals(
            vec![
                terminal_row("zz", &[], "a-p-i 模糊"),
                terminal_row("my-api", &[], "项目命中"),
                by_osc,
                by_agent,
                terminal_row("zz", &[], "my api"),
                terminal_row("zz", &[], "api 一号"),
                terminal_row("zz", &[], "api 二号"),
            ],
            "api",
        );
        assert_eq!(
            terminal_titles(&rows),
            [
                "api 一号",
                "api 二号",
                "my api",
                "agent 命中",
                "osc 命中",
                "项目命中",
                "a-p-i 模糊",
            ]
        );
    }

    /// 排序不看状态灯:灯一变行就跳,游标底下的东西会被换掉。
    #[test]
    fn 状态灯不影响排序() {
        let make = |light| {
            let mut first = terminal_row("p", &[], "pwsh 1");
            let mut second = terminal_row("p", &[], "pwsh 2");
            first.light = StatusLight::Idle;
            second.light = light;
            filter_terminals(vec![first, second], "pwsh")
        };
        assert_eq!(
            terminal_titles(&make(StatusLight::Idle)),
            ["pwsh 1", "pwsh 2"]
        );
        assert_eq!(
            terminal_titles(&make(StatusLight::Attention)),
            ["pwsh 1", "pwsh 2"]
        );
    }

    /// 选中去哪:项目行切项目,终端行回到原 pane。
    #[test]
    fn 候选的去向按身份记() {
        assert_eq!(
            Candidate::Project(project_row("a", &[])).target(),
            Target::Project("a".into())
        );
        let row = terminal_row("a", &[], "pwsh");
        let pane_id = row.pane_id.clone();
        assert_eq!(
            Candidate::Terminal(row).target(),
            Target::Pane {
                project_id: "a".into(),
                pane_id,
            }
        );
    }

    /// 游标环形移动,空列表钉在 0。
    #[test]
    fn 游标环形移动() {
        assert_eq!(next_cursor(0, 3, 1), 1);
        assert_eq!(next_cursor(2, 3, 1), 0, "到底折回第一项");
        assert_eq!(next_cursor(0, 3, -1), 2, "到顶折到最后一项");
        assert_eq!(next_cursor(0, 0, 1), 0);
        assert_eq!(next_cursor(5, 3, 1), 0, "越界的游标也要落回合法区间");
    }

    /// 分组路径展平:嵌套组拼成路径,树外项目追加到末尾且路径为空。
    #[test]
    fn 分组路径展平() {
        use mt_config::{ProjectGroup, ProjectTreeItem};
        let mut config = AppConfig::default();
        config.project_tree = Some(vec![
            ProjectTreeItem::ProjectId("top".into()),
            ProjectTreeItem::Group(ProjectGroup {
                id: "g1".into(),
                name: "前端".into(),
                collapsed: false,
                children: vec![
                    ProjectTreeItem::ProjectId("web".into()),
                    ProjectTreeItem::Group(ProjectGroup {
                        id: "g2".into(),
                        name: "实验".into(),
                        collapsed: false,
                        children: vec![ProjectTreeItem::ProjectId("lab".into())],
                    }),
                ],
            }),
            // 重复 id:只认第一次出现的位置
            ProjectTreeItem::ProjectId("web".into()),
        ]);
        config.projects = vec![project("top"), project("web"), project("lab"), project("orphan")];

        let flat = projects_with_group_path(&config);
        assert_eq!(
            flat,
            vec![
                ("top".to_string(), vec![]),
                ("web".to_string(), vec!["前端".to_string()]),
                ("lab".to_string(), vec!["前端".to_string(), "实验".to_string()]),
                ("orphan".to_string(), vec![]),
            ]
        );
    }

    /// 没有 projectTree 的旧配置:全部按 projects 的顺序、分组路径为空。
    #[test]
    fn 无分组树时按项目顺序() {
        let mut config = AppConfig::default();
        config.projects = vec![project("a"), project("b")];
        assert_eq!(
            projects_with_group_path(&config),
            vec![("a".to_string(), vec![]), ("b".to_string(), vec![])]
        );
    }

    #[test]
    fn 第二行文案() {
        assert_eq!(subtitle(&[], "D:/x"), "D:/x");
        assert_eq!(
            subtitle(&["前端".into(), "实验".into()], "D:/x"),
            "前端 / 实验 · D:/x"
        );
    }

    /// **机制回归测试**:`"ProjectSwitcher > Input"` 的绑定必须压过 `Input` 自带的
    /// `up`/`down`,否则焦点在输入框里时方向键只会移动光标,列表一动不动。
    ///
    /// 判据有两条,缺一不可:① 谓词深度打平(`Descendant` 取最深那层);
    /// ② 打平后按注册顺序**倒序**决胜负 —— 壳的 `cx.bind_keys` 跑在
    /// `gpui_component::init` 之后,所以壳的绑定后注册、先派发。
    #[test]
    fn 方向键绑定压过输入框自带的() {
        gpui::actions!(test_only, [InputMoveUp, SwitcherPrev]);

        let keymap = Keymap::new(vec![
            // 组件库先注册(gpui_component::init)
            KeyBinding::new("up", InputMoveUp, Some("Input")),
            // 壳后注册(main.rs 的 cx.bind_keys)
            KeyBinding::new("up", SwitcherPrev, Some("ProjectSwitcher > Input")),
        ]);
        let stack = context_stack(&["Workspace", "Dialog", "ProjectSwitcher", "Input"]);
        let (bindings, _) =
            keymap.bindings_for_input(&[Keystroke::parse("up").unwrap()], &stack);

        assert!(!bindings.is_empty(), "up 至少要匹配到一条绑定");
        assert!(
            bindings[0].action().partial_eq(&SwitcherPrev),
            "第一个派发的必须是切换器的动作,实际是 {:?}",
            bindings[0].action()
        );

        // 反证:不在切换器里(普通输入框)时,方向键仍归输入框
        let stack = context_stack(&["Workspace", "Dialog", "Input"]);
        let (bindings, _) =
            keymap.bindings_for_input(&[Keystroke::parse("up").unwrap()], &stack);
        assert!(
            bindings[0].action().partial_eq(&InputMoveUp),
            "切换器之外不许抢输入框的方向键"
        );
    }

    fn context_stack(names: &[&str]) -> Vec<KeyContext> {
        names
            .iter()
            .map(|n| KeyContext::parse(n).expect("上下文名必须能解析"))
            .collect()
    }

    fn project(id: &str) -> mt_config::ProjectConfig {
        mt_config::ProjectConfig::new(id, id, format!("/tmp/{id}"))
    }
}
