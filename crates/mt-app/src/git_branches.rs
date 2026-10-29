//! Git 分支弹窗(仓库栏分支徽章点开)+ 分支动作 + 新建 / 重命名的名称弹窗。
//! 对标 JetBrains IDEA 的 Branches 弹窗。
//!
//! ```text
//! ┌ 🔍 搜索分支…                    × ┐   打开即聚焦;即输即滤(忽略大小写),命中子串高亮
//! │ [+ 新建分支…] [☁ Fetch]           │
//! │ 当前分支                           │
//! │ ✓ main                    ↑2 ↓1  │   置顶,accent 勾
//! │ 最近                               │   HEAD reflog 里最近检出过的 5 个
//! │ ⑂ feature/login                   │
//! │ 本地                               │
//! │ 远程 · origin                      │   远程按远程名分组,行上去掉远程前缀
//! │ ☁ feature/login                   │
//! └ ↑↓ 选择  Enter 检出  → 操作  Esc 关闭 ┘
//! ```
//!
//! # 键盘
//!
//! ↑↓ / → 是绑在 `"GitBranchPopup > Input"` 上的动作(与项目切换器同一套理由:谓词与
//! `Input` 同深度才压得过它自带的方向键,见 `hotkeys.rs`)。→ 只在光标位于搜索词末尾
//! 时才弹菜单,否则 `propagate()` —— gpui 会接着派发下一条匹配的绑定,也就是输入框
//! 自己的 MoveRight,光标照常右移。Enter 订阅输入框的 `PressEnter`(它无条件发),
//! Esc 是 Dialog 自带的取消。
//!
//! # 动作
//!
//! 行上单击 / 右键 / → 弹出该分支的动作菜单;选了动作先收弹窗、再交给 Git 面板的
//! [`GitPanel::run_write_op`](忙碌态、结果摘要 toast、失败详情框、整面板刷新都在那)。
//! 改写历史与删除类动作先确认。检出本地分支前先在后台预检会不会被本地改动挡住,
//! 挡住就转「贮藏后检出」确认框(见 [`checkout`])。

use std::cell::Cell;
use std::path::Path;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, Focusable as _, FontWeight,
    HighlightStyle, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement, Pixels, Point, Render, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, StyledText, Subscription, Window, actions, div, point, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Input, InputEvent, InputState, SelectAll};
use mt_project::git::{
    BranchInfo, BranchNameIssue, DeleteOutcome, MergeOutcome, PushOptions, PushOutcome,
    RebaseOutcome, RemoteCheckout, split_remote_branch, validate_branch_name,
};
use mt_ui::icons::Shape;
use mt_ui::tooltip::TooltipExt as _;

use crate::git_panel::{BranchSnapshot, GitBusy, GitPanel};
use crate::git_ui::{self, Tone};
use crate::i18n::{t, tr};
use crate::menu::{self, MenuEntry, MenuItem};
use crate::prompt::{self, Confirm, autofocus, close_guarded, kind, open_guarded};
use crate::ui;

actions!(
    mini_term,
    [
        /// 分支弹窗:上一行(↑)。绑在 `"GitBranchPopup > Input"` 上,见模块注释。
        BranchPopupPrev,
        /// 分支弹窗:下一行(↓)。
        BranchPopupNext,
        /// 分支弹窗:弹出高亮行的动作菜单(→,光标在搜索词末尾时)。
        BranchPopupMenu,
    ]
);

/// 弹窗宽度。
const POPUP_WIDTH: f32 = 348.0;
/// 行高 / 分组标题高(与 `git_ui::list_row` / `git_ui::list_caption` 一一对应,估高度用)。
const ROW_H: f32 = git_ui::ROW_H;
const GROUP_H: f32 = 26.0;
/// 搜索框 + 动作行 + 底部提示条的固定高度。
const CHROME_H: f32 = 52.0 + 40.0 + 36.0;

// ─── 纯逻辑(可测) ────────────────────────────────────────────

/// 行属于哪个分组。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Group {
    Current,
    Recent,
    Local,
    Remote(String),
}

/// 弹窗里的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub group: Group,
    /// 完整分支名(本地 `feature/x`,远程 `origin/feature/x`)。
    pub name: String,
    /// 行上显示的名字(远程行去掉远程前缀)。
    pub label: String,
    pub is_remote: bool,
    pub is_head: bool,
    /// 命中区间(按 char,落在 `label` 上);空查询或命中在远程前缀上时为 `None`。
    pub hit: Option<(usize, usize)>,
    pub ahead: usize,
    pub behind: usize,
    pub upstream: Option<String>,
}

/// 大小写不敏感的子串匹配。`None` = 不中;`Some(None)` = 中但无可高亮(空查询);
/// `Some(Some(区间))` = 命中区间(char 下标)。
///
/// 逐字符折小写(取 `to_lowercase` 的第一个码点)保证折叠前后一一对应,
/// 高亮区间才落得回原文。
pub(crate) fn find_ci(text: &str, query: &str) -> Option<Option<(usize, usize)>> {
    if query.is_empty() {
        return Some(None);
    }
    let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
    let hay: Vec<char> = text.chars().map(fold).collect();
    let needle: Vec<char> = query.chars().map(fold).collect();
    if needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len())
        .find(|&i| hay[i..i + needle.len()] == needle[..])
        .map(|i| Some((i, i + needle.len())))
}

/// 按快照与搜索词排出全部行:当前分支 → 最近 → 本地 → 远程(按远程名分组,
/// `origin` 打头),组内按名字(忽略大小写)排序;最近组保持 reflog 的新旧顺序。
pub(crate) fn build_rows(snap: &BranchSnapshot, query: &str) -> Vec<Row> {
    let query = query.trim();
    let mut locals: Vec<&BranchInfo> = snap.branches.iter().filter(|b| !b.is_remote).collect();
    locals.sort_by_key(|b| b.name.to_lowercase());
    let head = snap.head.as_deref();

    let local_row = |group: Group, branch: &BranchInfo| -> Option<Row> {
        let hit = find_ci(&branch.name, query)?;
        let tracking = snap.tracking.iter().find(|t| t.branch == branch.name);
        Some(Row {
            group,
            name: branch.name.clone(),
            label: branch.name.clone(),
            is_remote: false,
            is_head: head == Some(branch.name.as_str()),
            hit,
            ahead: tracking.map_or(0, |t| t.ahead),
            behind: tracking.map_or(0, |t| t.behind),
            upstream: tracking
                .map(|t| t.upstream.clone())
                .or_else(|| branch.upstream.clone()),
        })
    };

    let mut rows = Vec::new();
    if let Some(current) = locals.iter().find(|b| Some(b.name.as_str()) == head) {
        rows.extend(local_row(Group::Current, current));
    }
    for name in &snap.recent {
        if Some(name.as_str()) == head {
            continue;
        }
        if let Some(branch) = locals.iter().find(|b| &b.name == name) {
            rows.extend(local_row(Group::Recent, branch));
        }
    }
    for branch in &locals {
        if Some(branch.name.as_str()) != head {
            rows.extend(local_row(Group::Local, branch));
        }
    }

    let mut remote_rows: Vec<Row> = Vec::new();
    for branch in snap.branches.iter().filter(|b| b.is_remote) {
        let (remote, short) = split_remote_branch(&snap.remotes, &branch.name)
            .or_else(|| {
                branch
                    .name
                    .split_once('/')
                    .map(|(r, s)| (r.to_string(), s.to_string()))
            })
            .unwrap_or_else(|| (String::new(), branch.name.clone()));
        // 先在显示名上找(能高亮);找不到再看完整名(`origin/fe` 这种带远程名的查询)
        let hit = match find_ci(&short, query) {
            Some(hit) => hit,
            None if find_ci(&branch.name, query).is_some() => None,
            None => continue,
        };
        remote_rows.push(Row {
            group: Group::Remote(remote),
            name: branch.name.clone(),
            label: short,
            is_remote: true,
            is_head: false,
            hit,
            ahead: 0,
            behind: 0,
            upstream: None,
        });
    }
    remote_rows.sort_by(|a, b| {
        let key = |row: &Row| match &row.group {
            Group::Remote(remote) => (remote != "origin", remote.to_lowercase()),
            _ => (true, String::new()),
        };
        key(a)
            .cmp(&key(b))
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
    });
    rows.extend(remote_rows);
    rows
}

/// 第 `cursor` 行在列表容器里是第几个子元素(分组标题也占一个)。
/// 键盘移动后滚到可见、→ 弹菜单时定位都靠它。
pub(crate) fn child_index_of(rows: &[Row], cursor: usize) -> Option<usize> {
    let mut index = 0;
    let mut last: Option<&Group> = None;
    for (i, row) in rows.iter().enumerate() {
        if last != Some(&row.group) {
            index += 1;
            last = Some(&row.group);
        }
        if i == cursor {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// 游标环形移动。列表为空时钉在 0。
fn next_cursor(cursor: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    (cursor as i64 + delta as i64).rem_euclid(len as i64) as usize
}

/// 名称输入框的就地校验:返回提示文案的 key(`None` = 可以提交)。
fn name_issue(mode: &NameMode, value: &str, existing: &[String]) -> Option<&'static str> {
    if let Err(issue) = validate_branch_name(value) {
        return Some(match issue {
            BranchNameIssue::Empty => "nameEmpty",
            BranchNameIssue::Whitespace => "nameWhitespace",
            BranchNameIssue::IllegalChar => "nameIllegalChar",
            BranchNameIssue::LeadingDash => "nameLeadingDash",
            BranchNameIssue::Reserved => "nameReserved",
            BranchNameIssue::BadFormat => "nameBadFormat",
        });
    }
    if let NameMode::Rename { old } = mode
        && value == old
    {
        return Some("nameUnchanged");
    }
    if existing.iter().any(|name| name == value) {
        return Some("nameExists");
    }
    None
}

// ─── 弹窗 ─────────────────────────────────────────────────────

pub struct BranchPopup {
    panel: Entity<GitPanel>,
    query: Entity<InputState>,
    /// 高亮到第几行(跨分组连续计数)。
    cursor: usize,
    scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

/// 打开分支弹窗。已经开着时是空操作。
pub(crate) fn open(panel: Entity<GitPanel>, window: &mut Window, cx: &mut App) {
    // 守卫在建视图之前判:拦下来时输入框已建好、聚焦也排上了,焦点会被送进虚空
    // (与 `prompt::show_prompt` 同一个坑)
    if prompt::is_open(kind::GIT_BRANCHES) {
        return;
    }
    let view = cx.new(|cx| BranchPopup::new(panel, window, cx));
    let input = view.read(cx).query.clone();
    open_guarded(kind::GIT_BRANCHES, window, cx, {
        let view = view.clone();
        move |dialog, window, cx| {
            // 按行数估一个自然高度再夹到 70vh:分支少就矮一点
            let natural = view.read(cx).natural_height(cx);
            let height = px(natural)
                .min(window.viewport_size().height * 0.7)
                .max(px(240.0));
            dialog
                .p_0()
                .w(px(POPUP_WIDTH))
                .overlay_closable(true)
                .child(div().h(height).child(view.clone()))
        }
    });
    autofocus(&input, window, cx);
}

impl BranchPopup {
    fn new(panel: Entity<GitPanel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx
            .new(|cx| InputState::new(window, cx).placeholder(t("gitBranch", "searchPlaceholder")));
        let mut subs = vec![cx.subscribe_in(
            &query,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                // 搜索词一变,高亮回到第一条匹配
                InputEvent::Change => {
                    this.cursor = 0;
                    this.scroll.scroll_to_item(0);
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.checkout_cursor(window, cx),
                _ => {}
            },
        )];
        // 分支列表 / 忙碌态在面板上变了(Fetch 回来、后台刷新)跟着重画
        subs.push(cx.observe(&panel, |_, _, cx| cx.notify()));
        Self {
            panel,
            query,
            cursor: 0,
            scroll: ScrollHandle::new(),
            _subs: subs,
        }
    }

    fn rows(&self, cx: &App) -> Vec<Row> {
        let snap = self.panel.read(cx).branch_snapshot();
        let query = self.query.read(cx).value().to_string();
        build_rows(&snap, &query)
    }

    fn natural_height(&self, cx: &App) -> f32 {
        let rows = self.rows(cx);
        if rows.is_empty() {
            return CHROME_H + 150.0;
        }
        let mut groups = 0;
        let mut last: Option<&Group> = None;
        for row in &rows {
            if last != Some(&row.group) {
                groups += 1;
                last = Some(&row.group);
            }
        }
        CHROME_H + 8.0 + groups as f32 * GROUP_H + rows.len() as f32 * ROW_H
    }

    fn move_cursor(&mut self, delta: i32, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        self.cursor = next_cursor(self.cursor.min(rows.len()), rows.len(), delta);
        if let Some(child) = child_index_of(&rows, self.cursor) {
            self.scroll.scroll_to_item(child);
        }
        cx.notify();
    }

    /// Enter:检出高亮的分支(当前分支则什么都不做)。
    fn checkout_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        let Some(row) = rows
            .get(self.cursor.min(rows.len().saturating_sub(1)))
            .cloned()
        else {
            return;
        };
        if row.is_head || self.panel.read(cx).branch_snapshot().busy.is_some() {
            return;
        }
        let panel = self.panel.clone();
        // 关窗排到本轮 effect 之后:这会儿本实体正被 Dialog 的 builder 持有着
        window.defer(cx, move |window, cx| {
            close_guarded(kind::GIT_BRANCHES, window, cx);
            checkout(&panel, &row, window, cx);
        });
    }

    /// 给第 `index` 行弹动作菜单。`at` = 鼠标位置;键盘触发时按行的位置算。
    fn open_menu(
        &mut self,
        index: usize,
        at: Option<Point<Pixels>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.rows(cx);
        let Some(row) = rows.get(index).cloned() else {
            return;
        };
        let position = at
            .or_else(|| {
                child_index_of(&rows, index)
                    .and_then(|child| self.scroll.bounds_for_item(child))
                    .map(|b| point(b.left() + px(36.0), b.bottom() - px(2.0)))
            })
            .unwrap_or_else(|| window.mouse_position());
        let snap = self.panel.read(cx).branch_snapshot();
        let entries = row_menu(&self.panel, &row, &snap);
        menu::show(position, entries, window, cx);
    }

    fn on_prev(&mut self, _: &BranchPopupPrev, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(-1, cx);
    }

    fn on_next(&mut self, _: &BranchPopupNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(1, cx);
    }

    fn on_menu_key(&mut self, _: &BranchPopupMenu, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.query.read(cx);
        let at_end = input.cursor() >= input.value().len() && input.selected_range().is_empty();
        if !at_end {
            // 让给输入框自己的 MoveRight(下一条匹配的绑定)
            cx.propagate();
            return;
        }
        self.open_menu(self.cursor, None, window, cx);
    }

    fn render_row(
        &self,
        index: usize,
        row: &Row,
        active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (shapes, icon_color): (&'static [Shape], _) = if row.is_head {
            (git_ui::ICON_CHECK, ui::accent())
        } else if row.is_remote {
            (git_ui::ICON_REMOTE, ui::text_muted())
        } else {
            (git_ui::ICON_BRANCH, ui::text_muted())
        };
        // 名字 + 命中高亮:**一段**文字带高亮区间,而不是逐段发元素 —— 逐段的话
        // 长名字只能被 overflow 硬裁掉,截断处没有省略号
        let label = SharedString::from(row.label.clone());
        let highlights = row
            .hit
            .and_then(|(start, end)| char_range_to_bytes(&row.label, start, end))
            .map(|range| {
                (
                    range,
                    HighlightStyle {
                        color: Some(ui::accent()),
                        font_weight: Some(FontWeight::BOLD),
                        background_color: Some(ui::with_alpha(ui::accent(), 0.12)),
                        ..Default::default()
                    },
                )
            });
        let name = div()
            .min_w_0()
            .truncate()
            .text_size(ui::font_px(13.0))
            .text_color(ui::text_primary())
            .when(row.is_head, |el| el.font_weight(FontWeight::SEMIBOLD))
            .child(StyledText::new(label).with_highlights(highlights));
        let tip = row.upstream.as_ref().map(|upstream| {
            SharedString::from(tr!(
                "gitBranch",
                "rowTip",
                upstream = upstream.as_str(),
                ahead = row.ahead,
                behind = row.behind
            ))
        });

        let row_id = SharedString::from(format!("git-branch-row-{index}"));
        git_ui::list_row(row_id, active)
            .child(git_ui::icon(shapes, git_ui::ICON_SIZE, icon_color))
            .child(name)
            .child(div().flex_1())
            .when(row.ahead > 0, |el| el.child(git_ui::ahead_pill(row.ahead)))
            .when(row.behind > 0, |el| {
                el.child(git_ui::behind_pill(row.behind))
            })
            .when_some(tip, |el, tip| el.tip(tip))
            // 鼠标移到哪行就高亮哪行(键盘高亮与悬停同一视觉)。挂在 mouse_move 而不是
            // on_hover 上:键盘 ↑↓ 把列表滚到静止的指针底下时 on_hover 也会触发,
            // 高亮会被指针「抢」回去;只有指针真的动了才跟
            .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _window, cx| {
                if this.cursor != index {
                    this.cursor = index;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.cursor = index;
                this.open_menu(index, Some(event.position()), window, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.cursor = index;
                    this.open_menu(index, Some(event.position), window, cx);
                }),
            )
            .into_any_element()
    }
}

/// 命中区间(char 下标,`find_ci` 的口径)→ 字节区间(`StyledText` 的高亮口径)。
/// 越界 / 逆序返回 `None`(一条坏区间不该把整行名字吞掉)。
fn char_range_to_bytes(text: &str, start: usize, end: usize) -> Option<std::ops::Range<usize>> {
    if start >= end {
        return None;
    }
    let byte_at = |n: usize| {
        text.char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(text.len()))
            .nth(n)
    };
    Some(byte_at(start)?..byte_at(end)?)
}

fn group_label(group: &Group) -> String {
    match group {
        Group::Current => t("gitBranch", "groupCurrent").to_string(),
        Group::Recent => t("gitBranch", "groupRecent").to_string(),
        Group::Local => t("gitBranch", "groupLocal").to_string(),
        Group::Remote(remote) => tr!("gitBranch", "groupRemote", remote = remote.as_str()),
    }
}

impl Render for BranchPopup {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snap = self.panel.read(cx).branch_snapshot();
        let query = self.query.read(cx).value().trim().to_string();
        let rows = build_rows(&snap, &query);
        // 候选变少之后游标可能越界
        let cursor = self.cursor.min(rows.len().saturating_sub(1));
        let busy = snap.busy;
        let fetching = busy == Some(GitBusy::Fetch);

        let header = div().px(px(12.0)).pt(px(12.0)).pb(px(8.0)).child(
            Input::new(&self.query)
                .prefix(div().pr(px(4.0)).child(git_ui::icon(
                    git_ui::ICON_SEARCH,
                    git_ui::ICON_SIZE,
                    ui::text_muted(),
                )))
                .cleanable(true),
        );

        let panel_for_new = self.panel.clone();
        let panel_for_fetch = self.panel.clone();
        let actions = div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(8.0))
            .pb(px(8.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(
                git_ui::chip_button(
                    "git-branches-new",
                    git_ui::ICON_PLUS,
                    t("gitBranch", "newBranch"),
                    false,
                    busy.is_some() || snap.repo.is_empty(),
                )
                .when(busy.is_none() && !snap.repo.is_empty(), |el| {
                    el.on_click(move |_: &ClickEvent, window, cx| {
                        close_guarded(kind::GIT_BRANCHES, window, cx);
                        open_name_dialog(
                            panel_for_new.clone(),
                            NameMode::Create { start: None },
                            window,
                            cx,
                        );
                    })
                }),
            )
            .child(
                git_ui::chip_button(
                    "git-branches-fetch",
                    git_ui::ICON_FETCH,
                    if fetching {
                        tr!("gitBranch", "busyTip", op = GitBusy::Fetch.label())
                    } else {
                        t("gitBranch", "fetch").to_string()
                    },
                    fetching,
                    busy.is_some() && !fetching,
                )
                .tip(t("gitBranch", "fetchTip"))
                .when(busy.is_none(), |el| {
                    // Fetch 不收弹窗:回来之后列表就地更新
                    el.on_click(move |_: &ClickEvent, window, cx| {
                        panel_for_fetch.update(cx, |p, cx| p.fetch(window, cx));
                    })
                }),
            );

        let mut list = div()
            .id("git-branches-list")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px(px(4.0))
            .py(px(4.0));

        if rows.is_empty() {
            let this = cx.entity();
            let query_input = self.query.clone();
            list = list.child(if snap.loading && snap.branches.is_empty() {
                git_ui::loading_state(t("gitBranch", "loading"))
            } else if !query.is_empty() {
                git_ui::empty_state(
                    git_ui::ICON_SEARCH,
                    tr!("gitBranch", "noMatch", query = query.as_str()),
                    Some(
                        git_ui::action_button(
                            "git-branches-clear",
                            t("gitBranch", "clearSearch"),
                            git_ui::ButtonKind::Ghost,
                            true,
                        )
                        .on_click(move |_: &ClickEvent, window, cx| {
                            query_input.update(cx, |state, cx| state.set_value("", window, cx));
                            this.update(cx, |this, cx| {
                                this.cursor = 0;
                                cx.notify();
                            });
                        })
                        .into_any_element(),
                    ),
                )
            } else {
                git_ui::empty_state(git_ui::ICON_BRANCH, t("gitBranch", "noBranches"), None)
            });
        }

        let mut last: Option<&Group> = None;
        for (index, row) in rows.iter().enumerate() {
            if last != Some(&row.group) {
                list = list.child(git_ui::list_caption(group_label(&row.group)));
                last = Some(&row.group);
            }
            list = list.child(self.render_row(index, row, index == cursor, cx));
        }

        // detached HEAD 时置顶一句说明(它不是分支,列表里没有它)
        let detached_note = snap.detached.as_ref().map(|label| {
            div()
                .mx(px(12.0))
                .mb(px(8.0))
                .px(px(10.0))
                .py(px(6.0))
                .rounded(px(4.0))
                .bg(ui::with_alpha(ui::color_warning(), 0.12))
                .child(
                    git_ui::notice_line(
                        tr!("gitBranch", "detachedNote", commit = label.as_str()),
                        ui::color_warning(),
                    )
                    .text_color(ui::text_secondary()),
                )
        });

        let hint = |keys: &'static str, label: &'static str| {
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(ui::kbd(keys))
                .child(
                    div()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(label),
                )
        };
        let footer = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.0))
            .h(px(36.0))
            .px(px(12.0))
            .border_t_1()
            .border_color(ui::border_subtle())
            .child(hint("↑↓", t("gitBranch", "hintMove")))
            .child(hint("Enter", t("gitBranch", "hintCheckout")))
            .child(hint("→", t("gitBranch", "hintActions")))
            .child(hint("Esc", t("gitBranch", "hintClose")));

        div()
            // 方向键的上下文锚点。键位表在 `hotkeys.rs`,谓词是
            // `"GitBranchPopup > Input"` —— 为什么非这么写见模块注释
            .key_context("GitBranchPopup")
            .on_action(cx.listener(Self::on_prev))
            .on_action(cx.listener(Self::on_next))
            .on_action(cx.listener(Self::on_menu_key))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(header)
            .children(detached_note)
            .child(actions)
            .child(list)
            .child(footer)
    }
}

// ─── 分支动作 ─────────────────────────────────────────────────

/// 一行的动作菜单。HEAD 行去掉不适用的(检出 / 合并 / 变基到自己 / 删除);
/// 远程行没有推送 / 重命名;有写操作在跑时写类动作一律禁用。
fn row_menu(panel: &Entity<GitPanel>, row: &Row, snap: &BranchSnapshot) -> Vec<MenuEntry> {
    let busy = snap.busy.is_some();
    let current = snap.head.clone();
    // 每个动作:先收弹窗,再交给面板
    let action = |label: String, f: fn(&Entity<GitPanel>, &Row, &mut Window, &mut App)| {
        let (panel, row) = (panel.clone(), row.clone());
        MenuItem::new(label)
            .disabled(busy)
            .on_click(move |window, cx| {
                close_guarded(kind::GIT_BRANCHES, window, cx);
                f(&panel, &row, window, cx);
            })
    };

    let mut entries: Vec<MenuEntry> = vec![MenuEntry::Header(row.name.clone().into())];
    if !row.is_head {
        entries.push(
            action(t("gitBranch", "menuCheckout").to_string(), checkout)
                .shortcut("Enter")
                .into(),
        );
    }
    entries.push(
        action(
            tr!("gitBranch", "menuNewFrom", name = row.label.as_str()),
            |panel, row, window, cx| {
                open_name_dialog(
                    panel.clone(),
                    NameMode::Create {
                        start: Some(row.name.clone()),
                    },
                    window,
                    cx,
                )
            },
        )
        .into(),
    );
    if let Some(current) = current.as_deref().filter(|_| !row.is_head) {
        entries.push(menu::separator());
        entries.push(action(tr!("gitBranch", "menuMerge", current = current), merge).into());
        entries.push(
            action(
                tr!("gitBranch", "menuRebase", current = current),
                confirm_rebase,
            )
            .into(),
        );
    }
    entries.push(menu::separator());
    {
        let (panel, name) = (panel.clone(), row.name.clone());
        entries.push(
            MenuItem::new(t("gitBranch", "menuViewHistory"))
                .on_click(move |window, cx| {
                    close_guarded(kind::GIT_BRANCHES, window, cx);
                    let name = name.clone();
                    panel.update(cx, |p, cx| p.set_view_branch(Some(name), cx));
                })
                .into(),
        );
    }
    if !row.is_remote {
        entries.push(action(t("gitBranch", "menuPush").to_string(), push_branch).into());
        entries.push(
            action(
                t("gitBranch", "menuRename").to_string(),
                |panel, row, window, cx| {
                    open_name_dialog(
                        panel.clone(),
                        NameMode::Rename {
                            old: row.name.clone(),
                        },
                        window,
                        cx,
                    )
                },
            )
            .into(),
        );
    }
    if !row.is_head {
        entries.push(menu::separator());
        entries.push(
            action(
                t(
                    "gitBranch",
                    if row.is_remote {
                        "menuDeleteRemote"
                    } else {
                        "menuDelete"
                    },
                )
                .to_string(),
                confirm_delete,
            )
            .danger()
            .into(),
        );
    }
    entries
}

/// 检出前的后台预检结果(见 [`checkout`])。
enum CheckoutStep<T> {
    /// 没被挡住,已照常检出。
    Done(T),
    /// 本地分支 `branch` 的检出会被这些本地改动挡住:转给「贮藏后检出」。
    Blocked {
        branch: String,
        conflicts: Vec<String>,
    },
}

/// 检出本地分支 `branch` 会被哪些本地改动挡住(后台跑)。预检只是提示性的:读失败就当
/// 没挡住,照常交给 git —— 真挡住时它自己会拒绝并给出原文。
fn blocking_changes(repo: &Path, branch: &str) -> Vec<String> {
    mt_project::git::local_changes_conflict_with(repo, branch).unwrap_or_else(|err| {
        eprintln!("[git] 检出前预检失败(照常检出): {err:#}");
        Vec::new()
    })
}

/// 检出(本地直接检出;远程:本地无同名则建跟踪分支,有则检出本地那个并提示)。
///
/// 检出**本地**分支(含远程行在本地已有同名分支的情况)之前,先在后台用
/// [`blocking_changes`] 预检:会被本地改动挡住就不去撞 git 的拒绝,转开「贮藏后检出」
/// 确认框(`git_stash::open_smart_checkout`)。预检与检出在同一个后台任务里,忙碌态 /
/// 防重入沿用 [`GitPanel::run_write_op`]。本地没有同名分支的远程行照旧直接
/// `checkout_remote_branch`(它新建跟踪分支;被本地改动挡住时报 git 原文)。
fn checkout(panel: &Entity<GitPanel>, row: &Row, window: &mut Window, cx: &mut App) {
    let name = row.name.clone();
    if row.is_remote {
        // 本地已有同名分支时,检出的其实是它:预检也对它做
        let snap = panel.read(cx).branch_snapshot();
        let local = split_remote_branch(&snap.remotes, &name)
            .map(|(_, local)| local)
            .filter(|local| {
                snap.branches
                    .iter()
                    .any(|b| !b.is_remote && &b.name == local)
            });
        panel.update(cx, |p, cx| {
            let remote_branch = name.clone();
            p.run_write_op(
                GitBusy::Checkout,
                move |repo| {
                    if let Some(local) = local {
                        let conflicts = blocking_changes(repo, &local);
                        if !conflicts.is_empty() {
                            return Ok(CheckoutStep::Blocked {
                                branch: local,
                                conflicts,
                            });
                        }
                    }
                    mt_project::git::checkout_remote_branch(repo, &name).map(CheckoutStep::Done)
                },
                move |p, step, window, cx| {
                    let outcome = match step {
                        CheckoutStep::Done(outcome) => outcome,
                        CheckoutStep::Blocked { branch, conflicts } => {
                            p.open_smart_checkout(branch, conflicts, window, cx);
                            return;
                        }
                    };
                    p.set_view_branch(None, cx);
                    let (message, tone) = match outcome {
                        RemoteCheckout::Created { local } => (
                            tr!(
                                "gitBranch",
                                "checkedOutTracking",
                                name = local,
                                remote = remote_branch.as_str()
                            ),
                            Tone::Info,
                        ),
                        RemoteCheckout::Existing { local } => (
                            tr!(
                                "gitBranch",
                                "checkedOutExisting",
                                name = local,
                                remote = remote_branch.as_str()
                            ),
                            Tone::Attention,
                        ),
                    };
                    p.notify_result(message, tone, cx);
                },
                window,
                cx,
            );
        });
        return;
    }
    panel.update(cx, |p, cx| {
        let shown = name.clone();
        p.run_write_op(
            GitBusy::Checkout,
            move |repo| {
                let conflicts = blocking_changes(repo, &name);
                if !conflicts.is_empty() {
                    return Ok(CheckoutStep::Blocked {
                        branch: name,
                        conflicts,
                    });
                }
                mt_project::git::checkout_branch(repo, &name).map(CheckoutStep::Done)
            },
            move |p, step, window, cx| match step {
                CheckoutStep::Done(()) => {
                    p.set_view_branch(None, cx);
                    p.notify_result(tr!("gitBranch", "checkedOut", name = shown), Tone::Info, cx);
                }
                CheckoutStep::Blocked { branch, conflicts } => {
                    p.open_smart_checkout(branch, conflicts, window, cx)
                }
            },
            window,
            cx,
        );
    });
}

/// 把 `row` 合并到当前分支。
fn merge(panel: &Entity<GitPanel>, row: &Row, window: &mut Window, cx: &mut App) {
    let name = row.name.clone();
    let current = panel.read(cx).branch_snapshot().head.unwrap_or_default();
    panel.update(cx, |p, cx| {
        let shown = name.clone();
        p.run_write_op(
            GitBusy::Merge,
            move |repo| mt_project::git::merge_branch(repo, &name),
            move |p, outcome, _window, cx| {
                let name = shown.as_str();
                let (message, tone) = match outcome {
                    MergeOutcome::UpToDate => {
                        (tr!("gitBranch", "mergeUpToDate", name = name), Tone::Info)
                    }
                    MergeOutcome::FastForward => (
                        tr!(
                            "gitBranch",
                            "mergeFastForward",
                            name = name,
                            current = current.as_str()
                        ),
                        Tone::Info,
                    ),
                    MergeOutcome::Merged => (
                        tr!(
                            "gitBranch",
                            "mergeMerged",
                            name = name,
                            current = current.as_str()
                        ),
                        Tone::Info,
                    ),
                    MergeOutcome::Conflicts { files } => (
                        tr!("gitBranch", "mergeConflicts", name = name, count = files),
                        Tone::Attention,
                    ),
                };
                p.notify_result(message, tone, cx);
            },
            window,
            cx,
        );
    });
}

/// 把当前分支变基到 `row`:改写当前分支上独有的提交,先确认。
fn confirm_rebase(panel: &Entity<GitPanel>, row: &Row, window: &mut Window, cx: &mut App) {
    let current = panel.read(cx).branch_snapshot().head.unwrap_or_default();
    let (panel, name) = (panel.clone(), row.name.clone());
    Confirm::new(
        t("gitBranch", "rebaseConfirmTitle"),
        tr!(
            "gitBranch",
            "rebaseConfirmMessage",
            current = current.as_str(),
            name = name.as_str()
        ),
    )
    .ok_text(t("gitBranch", "rebaseOk"))
    .open(
        move |window, cx| {
            let (name, current) = (name.clone(), current.clone());
            panel.update(cx, |p, cx| {
                let shown = name.clone();
                p.run_write_op(
                    GitBusy::Rebase,
                    move |repo| mt_project::git::rebase_onto(repo, &name),
                    move |p, outcome, _window, cx| {
                        let name = shown.as_str();
                        let (message, tone) = match outcome {
                            RebaseOutcome::UpToDate => (
                                tr!(
                                    "gitBranch",
                                    "rebaseUpToDate",
                                    current = current.as_str(),
                                    name = name
                                ),
                                Tone::Info,
                            ),
                            RebaseOutcome::Rebased => (
                                tr!(
                                    "gitBranch",
                                    "rebased",
                                    current = current.as_str(),
                                    name = name
                                ),
                                Tone::Info,
                            ),
                            RebaseOutcome::Conflicts { files } => (
                                tr!("gitBranch", "rebaseConflicts", name = name, count = files),
                                Tone::Attention,
                            ),
                        };
                        p.notify_result(message, tone, cx);
                    },
                    window,
                    cx,
                );
            });
        },
        window,
        cx,
    );
}

/// 推送本地分支。当前分支走推送对话框(能看到待推送的提交、可选强推);
/// 别的分支直接推(没上游时顺带 `-u`),摘要里带推了几个提交。
fn push_branch(panel: &Entity<GitPanel>, row: &Row, window: &mut Window, cx: &mut App) {
    if row.is_head {
        panel.update(cx, |p, cx| p.open_push_dialog(window, cx));
        return;
    }
    let name = row.name.clone();
    panel.update(cx, |p, cx| {
        p.run_write_op(
            GitBusy::Push,
            move |repo: &Path| {
                let count = mt_project::git::outgoing_commits(repo, Some(&name), 1000)
                    .map(|o| o.commits.len())
                    .unwrap_or(0);
                let options = PushOptions {
                    branch: Some(name.clone()),
                    force_with_lease: false,
                };
                Ok((count, mt_project::git::git_push_with(repo, &options)?))
            },
            |p, (count, outcome), window, cx| match outcome {
                PushOutcome::Pushed(target) => {
                    let message = crate::git_push::pushed_summary(&target, count, false, false);
                    p.notify_result(message, Tone::Info, cx);
                }
                PushOutcome::Rejected { detail } => {
                    git_ui::show_git_error(
                        tr!("gitBranch", "opFailed", op = GitBusy::Push.label()),
                        format!("{}\n\n{detail}", t("gitBranch", "rejectedHint")),
                        window,
                        cx,
                    );
                }
            },
            window,
            cx,
        );
    });
}

/// 删除:本地先 `-d`,未合并时二次确认 `-D`;远程删除是危险操作,确认文案说清后果。
fn confirm_delete(panel: &Entity<GitPanel>, row: &Row, window: &mut Window, cx: &mut App) {
    let (panel, name) = (panel.clone(), row.name.clone());
    if row.is_remote {
        let snap = panel.read(cx).branch_snapshot();
        let (remote, branch) = split_remote_branch(&snap.remotes, &name)
            .unwrap_or_else(|| (String::new(), name.clone()));
        Confirm::new(
            t("gitBranch", "deleteRemoteTitle"),
            tr!(
                "gitBranch",
                "deleteRemoteMessage",
                remote = remote.as_str(),
                branch = branch.as_str()
            ),
        )
        .ok_text(t("gitBranch", "deleteRemoteOk"))
        .open(
            move |window, cx| {
                let name = name.clone();
                panel.update(cx, |p, cx| {
                    let shown = name.clone();
                    p.run_write_op(
                        GitBusy::Delete,
                        move |repo| mt_project::git::delete_remote_branch(repo, &name),
                        move |p, (), _window, cx| {
                            p.notify_result(
                                tr!("gitBranch", "deletedRemote", name = shown),
                                Tone::Info,
                                cx,
                            );
                        },
                        window,
                        cx,
                    );
                });
            },
            window,
            cx,
        );
        return;
    }
    Confirm::new(
        t("gitBranch", "deleteTitle"),
        tr!("gitBranch", "deleteMessage", name = name.as_str()),
    )
    .ok_text(t("gitBranch", "deleteOk"))
    .open(
        move |window, cx| delete_local(&panel, name.clone(), false, window, cx),
        window,
        cx,
    );
}

fn delete_local(
    panel: &Entity<GitPanel>,
    name: String,
    force: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let panel_for_retry = panel.clone();
    panel.update(cx, |p, cx| {
        let shown = name.clone();
        p.run_write_op(
            GitBusy::Delete,
            move |repo| mt_project::git::delete_branch(repo, &name, force),
            move |p, outcome, window, cx| match outcome {
                DeleteOutcome::Deleted => {
                    p.notify_result(
                        tr!("gitBranch", "deleted", name = shown.as_str()),
                        Tone::Info,
                        cx,
                    );
                }
                // 未完全合并:二次确认强制删除(-D)
                DeleteOutcome::NotMerged => {
                    let panel = panel_for_retry.clone();
                    let name = shown.clone();
                    Confirm::new(
                        t("gitBranch", "forceDeleteTitle"),
                        tr!("gitBranch", "forceDeleteMessage", name = shown.as_str()),
                    )
                    .ok_text(t("gitBranch", "forceDeleteOk"))
                    .open(
                        move |window, cx| delete_local(&panel, name.clone(), true, window, cx),
                        window,
                        cx,
                    );
                }
            },
            window,
            cx,
        );
    });
}

// ─── 新建 / 重命名分支的名称弹窗 ─────────────────────────────

/// 名称弹窗的两种用途。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NameMode {
    /// 新建;`start = None` 表示从 HEAD 起。
    Create {
        start: Option<String>,
    },
    Rename {
        old: String,
    },
}

struct NameForm {
    panel: Entity<GitPanel>,
    mode: NameMode,
    input: Entity<InputState>,
    /// 「创建后检出」,默认勾上。
    checkout: bool,
    busy: bool,
    /// 上一次提交失败的 git 原文(就地显示,下次修改时清掉)。
    error: Option<String>,
    /// 用户动过输入框:空名字的提示在这之后才显示(一打开就红一行不友好)。
    touched: bool,
    /// 已有的本地分支名(「已存在」就地校验)。
    existing: Vec<String>,
    _sub: Subscription,
}

impl Render for NameForm {
    /// 状态盒子。真正的画面由 Dialog 的 builder 每帧重建。
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 打开新建 / 重命名分支的名称弹窗:打开即聚焦输入框(重命名时全选旧名),
/// 名称即时校验就地显示,Enter 提交、Esc 取消。
pub(crate) fn open_name_dialog(
    panel: Entity<GitPanel>,
    mode: NameMode,
    window: &mut Window,
    cx: &mut App,
) {
    if prompt::is_open(kind::GIT_BRANCH_NAME) {
        return;
    }
    let snap = panel.read(cx).branch_snapshot();
    if snap.repo.is_empty() {
        return;
    }
    let existing: Vec<String> = snap
        .branches
        .iter()
        .filter(|b| !b.is_remote)
        .map(|b| b.name.clone())
        .collect();
    let default_value = match &mode {
        NameMode::Rename { old } => old.clone(),
        NameMode::Create { .. } => String::new(),
    };
    let rename = matches!(mode, NameMode::Rename { .. });
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(t("gitBranch", "namePlaceholder"))
            .default_value(default_value)
    });
    let form = cx.new(|cx| {
        let sub = cx.subscribe(&input, |form: &mut NameForm, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                form.touched = true;
                form.error = None;
                cx.notify();
            }
        });
        NameForm {
            panel,
            mode,
            input: input.clone(),
            checkout: true,
            busy: false,
            error: None,
            touched: false,
            existing,
            _sub: sub,
        }
    });
    let head_label = snap
        .head
        .clone()
        .or(snap.detached.clone())
        .unwrap_or_default();
    // 重命名:第一帧把旧名全选(多半是整个换掉),手法同 `prompt::show_prompt`
    let select_all = Cell::new(rename);
    let input_for_focus = input.clone();

    open_guarded(
        kind::GIT_BRANCH_NAME,
        window,
        cx,
        move |dialog, window, cx| {
            if select_all.take() {
                let focus = input.read(cx).focus_handle(cx);
                window.on_next_frame(move |window, cx| {
                    focus.dispatch_action(&SelectAll, window, cx);
                });
            }
            let f = form.read(cx);
            let value = f.input.read(cx).value().trim().to_string();
            let issue = name_issue(&f.mode, &value, &f.existing);
            let show_issue = issue.filter(|_| f.touched || !value.is_empty());
            let (title, ok_label) = match &f.mode {
                NameMode::Create { .. } => (
                    t("gitBranch", "newBranchTitle"),
                    if f.busy {
                        t("gitBranch", "creating")
                    } else {
                        t("gitBranch", "create")
                    },
                ),
                NameMode::Rename { .. } => (
                    t("gitBranch", "renameTitle"),
                    if f.busy {
                        t("gitBranch", "renaming")
                    } else {
                        t("gitBranch", "rename")
                    },
                ),
            };

            // 第一行:起点(新建)/ 原名(重命名),引用胶囊按语义表上色
            // (HEAD = accent、其它本地分支 = success、远程 = info)
            let (lead, lead_name) = match &f.mode {
                NameMode::Create { start } => (
                    t("gitBranch", "startPoint"),
                    start.clone().unwrap_or_else(|| head_label.clone()),
                ),
                NameMode::Rename { old } => (t("gitBranch", "currentName"), old.clone()),
            };
            let lead_kind = if lead_name == head_label {
                git_ui::RefKind::Head
            } else if snap_is_local(&f.existing, &lead_name) {
                git_ui::RefKind::Local
            } else {
                git_ui::RefKind::Remote
            };
            let mut body = git_ui::form_body()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .min_w_0()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_muted())
                        .child(git_ui::icon(
                            git_ui::ICON_BRANCH,
                            git_ui::ICON_SIZE,
                            ui::text_muted(),
                        ))
                        .child(div().flex_none().child(lead))
                        .child(
                            git_ui::ref_pill("git-branch-name-lead", lead_name.clone(), lead_kind)
                                .tip(lead_name),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(Input::new(&f.input))
                        .child(match show_issue {
                            Some(key) => {
                                git_ui::notice_line(t("gitBranch", key), ui::color_error())
                            }
                            None => git_ui::hint_line(t("gitBranch", "nameHint")),
                        }),
                );
            if matches!(f.mode, NameMode::Create { .. }) {
                let form_for_toggle = form.clone();
                body = body.child(
                    git_ui::check_row(
                        "git-new-branch-checkout",
                        f.checkout,
                        t("gitBranch", "checkoutAfterCreate"),
                        false,
                    )
                    .on_click(move |_: &ClickEvent, _window, cx| {
                        form_for_toggle.update(cx, |f, cx| {
                            f.checkout = !f.checkout;
                            cx.notify();
                        });
                    }),
                );
            }
            if let Some(err) = &f.error {
                body = body.child(git_ui::error_block(err.clone()));
            }

            let form_for_ok = form.clone();
            dialog
                .title(title)
                .w(px(git_ui::DIALOG_W_MD))
                .overlay_closable(true)
                .footer(git_ui::dialog_footer(
                    ok_label,
                    Some(t("gitBranch", "cancel").into()),
                    false,
                    f.busy,
                ))
                .child(body)
                .on_ok(move |_: &ClickEvent, window, cx| submit_name(&form_for_ok, window, cx))
        },
    );
    autofocus(&input_for_focus, window, cx);
}

/// 起点是不是本地分支(决定胶囊配色:本地 success / 远程 info)。
fn snap_is_local(existing: &[String], name: &str) -> bool {
    existing.iter().any(|n| n == name)
}

/// 名称弹窗的「创建 / 重命名」。返回 `false` = 先不关窗(结果回来再说)。
fn submit_name(form: &Entity<NameForm>, window: &mut Window, cx: &mut App) -> bool {
    let (busy, invalid, value, panel, mode, checkout) = {
        let f = form.read(cx);
        let value = f.input.read(cx).value().trim().to_string();
        (
            f.busy,
            name_issue(&f.mode, &value, &f.existing).is_some(),
            value,
            f.panel.clone(),
            f.mode.clone(),
            f.checkout,
        )
    };
    if busy {
        return false;
    }
    if invalid {
        form.update(cx, |f, cx| {
            f.touched = true;
            cx.notify();
        });
        return false;
    }
    let repo = panel.read(cx).repo_path().to_string();
    if repo.is_empty() {
        return true;
    }
    form.update(cx, |f, cx| {
        f.busy = true;
        f.error = None;
        cx.notify();
    });
    let form = form.clone();
    window
        .spawn(cx, async move |cx| {
            let (name, mode_for_task) = (value.clone(), mode.clone());
            let result = cx
                .background_executor()
                .spawn(async move {
                    let repo = Path::new(&repo);
                    match &mode_for_task {
                        NameMode::Create { start } => {
                            mt_project::git::create_branch(repo, &name, start.as_deref(), checkout)
                        }
                        NameMode::Rename { old } => {
                            mt_project::git::rename_branch(repo, old, &name)
                        }
                    }
                })
                .await;
            let _ = cx.update(|window, cx| match result {
                Ok(()) => {
                    close_guarded(kind::GIT_BRANCH_NAME, window, cx);
                    let message = match &mode {
                        NameMode::Create { .. } if checkout => {
                            tr!("gitBranch", "createdAndCheckedOut", name = value.as_str())
                        }
                        NameMode::Create { .. } => {
                            tr!("gitBranch", "created", name = value.as_str())
                        }
                        NameMode::Rename { old } => tr!(
                            "gitBranch",
                            "renamed",
                            old = old.as_str(),
                            new = value.as_str()
                        ),
                    };
                    panel.update(cx, |p, cx| {
                        if checkout || matches!(mode, NameMode::Rename { .. }) {
                            p.set_view_branch(None, cx);
                        }
                        p.notify_result(message, Tone::Info, cx);
                        p.refresh_all(cx);
                    });
                }
                Err(err) => {
                    let message = format!("{err:#}");
                    form.update(cx, |f, cx| {
                        f.busy = false;
                        f.error = Some(message.clone());
                        cx.notify();
                    });
                    // 等结果期间弹窗被 Esc 关了:原因不能丢,退到详情框
                    if !prompt::is_open(kind::GIT_BRANCH_NAME) {
                        let op = match mode {
                            NameMode::Create { .. } => GitBusy::Create,
                            NameMode::Rename { .. } => GitBusy::Rename,
                        };
                        git_ui::show_git_error(
                            tr!("gitBranch", "opFailed", op = op.label()),
                            message,
                            window,
                            cx,
                        );
                    }
                }
            });
        })
        .detach();
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use mt_project::git::BranchTracking;

    fn branch(name: &str, remote: bool) -> BranchInfo {
        BranchInfo {
            name: name.into(),
            is_head: false,
            is_remote: remote,
            commit_hash: "abc".into(),
            upstream: None,
        }
    }

    fn snapshot() -> BranchSnapshot {
        BranchSnapshot {
            repo: "/r".into(),
            branches: vec![
                branch("main", false),
                branch("feature/Login", false),
                branch("dev", false),
                branch("bugfix", false),
                branch("origin/main", true),
                branch("origin/feature/login", true),
                branch("upstream/main", true),
                branch("team/fork/x", true),
            ],
            tracking: vec![BranchTracking {
                branch: "main".into(),
                upstream: "origin/main".into(),
                ahead: 2,
                behind: 1,
            }],
            recent: vec!["dev".into(), "main".into(), "gone".into()],
            remotes: vec!["origin".into(), "upstream".into(), "team/fork".into()],
            head: Some("main".into()),
            detached: None,
            loading: false,
            busy: None,
        }
    }

    fn names(rows: &[Row]) -> Vec<(&Group, &str)> {
        rows.iter().map(|r| (&r.group, r.label.as_str())).collect()
    }

    /// 分组顺序:当前 → 最近(reflog 序,排除当前与已删)→ 本地(名字序,不含当前)
    /// → 远程(origin 打头,远程名带 `/` 也拆得对,行上去掉远程前缀)。
    #[test]
    fn 分组与排序() {
        let rows = build_rows(&snapshot(), "");
        let origin = Group::Remote("origin".into());
        let upstream = Group::Remote("upstream".into());
        let fork = Group::Remote("team/fork".into());
        assert_eq!(
            names(&rows),
            vec![
                (&Group::Current, "main"),
                (&Group::Recent, "dev"),
                (&Group::Local, "bugfix"),
                (&Group::Local, "dev"),
                (&Group::Local, "feature/Login"),
                (&origin, "feature/login"),
                (&origin, "main"),
                (&fork, "x"),
                (&upstream, "main"),
            ]
        );
        let head = &rows[0];
        assert!(head.is_head);
        assert_eq!((head.ahead, head.behind), (2, 1));
        assert_eq!(head.upstream.as_deref(), Some("origin/main"));
        assert_eq!(rows[5].name, "origin/feature/login", "远程行保留完整名");
    }

    /// 搜索:忽略大小写的子串,命中区间按 char 落在显示名上;带远程名的查询也能
    /// 命中远程行(此时不高亮)。
    #[test]
    fn 搜索过滤与高亮() {
        let rows = build_rows(&snapshot(), "LOGIN");
        assert_eq!(
            names(&rows),
            vec![
                (&Group::Local, "feature/Login"),
                (&Group::Remote("origin".into()), "feature/login"),
            ]
        );
        assert_eq!(rows[0].hit, Some((8, 13)));

        let rows = build_rows(&snapshot(), "origin/ma");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "origin/main");
        assert_eq!(rows[0].hit, None, "命中在远程前缀上,显示名里没有可高亮的");

        assert!(build_rows(&snapshot(), "zzz").is_empty());
        // 首尾空白不算
        assert_eq!(build_rows(&snapshot(), "  dev ").len(), 2);
    }

    /// 命中区间(char)换成 `StyledText` 要的字节区间:中文名不能切在半个字上。
    #[test]
    fn 命中区间换成字节区间() {
        assert_eq!(char_range_to_bytes("feature/Login", 8, 13), Some(8..13));
        assert_eq!(char_range_to_bytes("修复/登录页", 3, 5), Some(7..13));
        // 命中到末尾
        assert_eq!(char_range_to_bytes("修复", 1, 2), Some(3..6));
        // 坏区间不 panic
        assert_eq!(char_range_to_bytes("abc", 2, 9), None);
        assert_eq!(char_range_to_bytes("abc", 2, 2), None);
    }

    #[test]
    fn 子串匹配按字符() {
        assert_eq!(find_ci("Feature/X", ""), Some(None));
        assert_eq!(find_ci("Feature/X", "x"), Some(Some((8, 9))));
        assert_eq!(find_ci("修复/登录页", "登录"), Some(Some((3, 5))));
        assert_eq!(find_ci("ab", "abc"), None);
    }

    /// 子元素下标:每遇到新分组多一个标题。
    #[test]
    fn 行在列表里的位置() {
        let rows = build_rows(&snapshot(), "");
        // [标题 当前][main][标题 最近][dev][标题 本地][bugfix]...
        assert_eq!(child_index_of(&rows, 0), Some(1));
        assert_eq!(child_index_of(&rows, 1), Some(3));
        assert_eq!(child_index_of(&rows, 2), Some(5));
        assert_eq!(child_index_of(&rows, 3), Some(6));
        assert_eq!(child_index_of(&rows, 99), None);
    }

    #[test]
    fn 游标环形移动() {
        assert_eq!(next_cursor(0, 3, 1), 1);
        assert_eq!(next_cursor(2, 3, 1), 0);
        assert_eq!(next_cursor(0, 3, -1), 2);
        assert_eq!(next_cursor(0, 0, 1), 0);
    }

    /// 名称就地校验:非法 / 已存在 / 与原名相同各有提示,合法放行。
    #[test]
    fn 名称就地校验() {
        let existing = vec!["main".to_string(), "dev".to_string()];
        let create = NameMode::Create { start: None };
        assert_eq!(name_issue(&create, "", &existing), Some("nameEmpty"));
        assert_eq!(
            name_issue(&create, "a b", &existing),
            Some("nameWhitespace")
        );
        assert_eq!(
            name_issue(&create, "-x", &existing),
            Some("nameLeadingDash")
        );
        assert_eq!(
            name_issue(&create, "a..b", &existing),
            Some("nameBadFormat")
        );
        assert_eq!(name_issue(&create, "dev", &existing), Some("nameExists"));
        assert_eq!(name_issue(&create, "feature/new", &existing), None);
        let rename = NameMode::Rename { old: "dev".into() };
        assert_eq!(name_issue(&rename, "dev", &existing), Some("nameUnchanged"));
        assert_eq!(name_issue(&rename, "main", &existing), Some("nameExists"));
        assert_eq!(name_issue(&rename, "dev2", &existing), None);
        // 每个 key 都有文案
        for key in [
            "nameEmpty",
            "nameWhitespace",
            "nameIllegalChar",
            "nameLeadingDash",
            "nameReserved",
            "nameBadFormat",
            "nameExists",
            "nameUnchanged",
        ] {
            assert!(
                mt_i18n::lookup(mt_i18n::Locale::Zh, "gitBranch", key).is_some(),
                "{key}"
            );
            assert!(
                mt_i18n::lookup(mt_i18n::Locale::En, "gitBranch", key).is_some(),
                "{key}"
            );
        }
    }
}
