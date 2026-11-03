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
//! # 列表
//!
//! 排好的行按 (分支数据代号, HEAD, 搜索词) 缓存成 [`RowsModel`],走变高虚拟列表(分组标题
//! 26、行 28)只给视口里的项建元素。弹窗开着时每帧重画、Dialog 的 builder 每帧还要估一次
//! 高度 —— 远程分支上千条时,不能每帧把它们过滤、排序、建元素一遍。分支快照本身也是共享的
//! (见 `git_panel::BranchData`),每帧取快照不再深拷贝整份分支列表。
//!
//! # 动作
//!
//! 行上单击 / 右键 / → 弹出该分支的动作菜单;选了动作先收弹窗、再交给 Git 面板的
//! [`GitPanel::run_write_op`](忙碌态、写操作闸、结果摘要 toast、失败详情框、整面板刷新
//! 都在那)。改写历史与删除类动作先确认。检出本地分支前先在后台预检会不会被本地改动
//! 挡住,挡住就转「贮藏后检出」确认框(见 [`checkout`])。「推送」打开推送对话框并预选
//! 这一行的分支,先看清待推送的提交再推。面板在跑写操作、或别的区占着仓库的写操作闸时,
//! 写类动作一律禁用。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, Focusable as _, FontWeight,
    HighlightStyle, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement, Pixels, Point, Render, ScrollStrategy, SharedString, Size,
    StatefulInteractiveElement, Styled, StyledText, Subscription, Window, actions, div, point,
    prelude::FluentBuilder as _, px, size,
};
use gpui_component::input::{Input, InputEvent, InputState, SelectAll};
use gpui_component::{VirtualListScrollHandle, v_virtual_list};
use mt_project::git::{
    BranchInfo, BranchNameIssue, BranchTracking, DeleteOutcome, MergeOutcome, RebaseOutcome,
    RemoteCheckout, split_remote_branch, validate_branch_name,
};
use mt_ui::icons::Shape;
use mt_ui::tooltip::TooltipExt as _;

use crate::git_panel::{BranchSnapshot, GitBusy, GitPanel};
use crate::git_ui::{self, FormOp, Tone};
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

/// 分支弹窗的键位上下文。`hotkeys.rs` 的谓词是 `"<它> > Input"` —— 为什么非这么写
/// 见模块注释「键盘」。
pub(crate) const BRANCH_POPUP_CONTEXT: &str = "GitBranchPopup";

/// 弹窗宽度。
const POPUP_WIDTH: f32 = 348.0;
/// 行高 / 分组标题高(与 `git_ui::list_row` / `git_ui::list_caption` 一一对应:估高度、
/// 虚拟列表的项高都按它)。
const ROW_H: f32 = git_ui::ROW_H;
const GROUP_H: f32 = 26.0;
/// 列表的内边距(四周)。
const LIST_PAD: f32 = 4.0;
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

/// 大小写不敏感的子串匹配(一次性的搜索词;建行走 [`find_folded`],搜索词只折一次)。
#[cfg(test)]
pub(crate) fn find_ci(text: &str, query: &str) -> Option<Option<(usize, usize)>> {
    let needle: Vec<char> = query.chars().map(fold_char).collect();
    find_folded(text, &needle)
}

/// 折一个字符(见 [`find_folded`]):ASCII 直接折,其余取 `to_lowercase` 的第一个码点。
fn fold_char(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// 大小写不敏感的子串匹配。`None` = 不中;`Some(None)` = 中但无可高亮(空查询);
/// `Some(Some(区间))` = 命中区间(char 下标)。`needle` 是用 [`fold_char`] 折好的搜索词。
///
/// 逐字符折小写(取 `to_lowercase` 的第一个码点)保证折叠前后一一对应,
/// 高亮区间才落得回原文。
///
/// 建行时一个搜索词只折一次,逐个分支边走边折,不再每个分支分配两份 `Vec<char>`(远程
/// 分支上千条时那是每次建行几千次分配)。命中的是第一个折叠后完全相同的位置,与整串
/// 折好再逐位比对同一结果(单测对拍)。
fn find_folded(text: &str, needle: &[char]) -> Option<Option<(usize, usize)>> {
    if needle.is_empty() {
        return Some(None);
    }
    let mut start = text.chars();
    let mut at = 0;
    loop {
        let mut hay = start.clone();
        if needle.iter().all(|&n| hay.next().map(fold_char) == Some(n)) {
            return Some(Some((at, at + needle.len())));
        }
        start.next()?;
        at += 1;
    }
}

/// 按快照与搜索词排出全部行:当前分支 → 最近 → 本地 → 远程(按远程名分组,
/// `origin` 打头),组内按名字(忽略大小写)排序;最近组保持 reflog 的新旧顺序。
///
/// 每个排序键只算一次(`sort_by_cached_key`,稳定排序,与逐次比较时现转小写同一结果),
/// 领先落后按分支名建表查(重名取第一条,与逐个 `find` 同一口径)。
pub(crate) fn build_rows(snap: &BranchSnapshot, query: &str) -> Vec<Row> {
    let query = query.trim();
    let needle: Vec<char> = query.chars().map(fold_char).collect();
    let data = &snap.data;
    let mut locals: Vec<&BranchInfo> = data.branches.iter().filter(|b| !b.is_remote).collect();
    locals.sort_by_cached_key(|b| b.name.to_lowercase());
    let head = snap.head.as_deref();
    let mut tracking_of: HashMap<&str, &BranchTracking> = HashMap::new();
    for tracking in &data.tracking {
        tracking_of
            .entry(tracking.branch.as_str())
            .or_insert(tracking);
    }

    let local_row = |group: Group, branch: &BranchInfo| -> Option<Row> {
        let hit = find_folded(&branch.name, &needle)?;
        let tracking = tracking_of.get(branch.name.as_str()).copied();
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
    for name in &data.recent {
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
    for branch in data.branches.iter().filter(|b| b.is_remote) {
        let (remote, short) = split_remote_branch(&data.remotes, &branch.name)
            .or_else(|| {
                branch
                    .name
                    .split_once('/')
                    .map(|(r, s)| (r.to_string(), s.to_string()))
            })
            .unwrap_or_else(|| (String::new(), branch.name.clone()));
        // 先在显示名上找(能高亮);找不到再看完整名(`origin/fe` 这种带远程名的查询)
        let hit = match find_folded(&short, &needle) {
            Some(hit) => hit,
            None if find_folded(&branch.name, &needle).is_some() => None,
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
    remote_rows.sort_by_cached_key(|row| {
        let (not_origin, remote) = match &row.group {
            Group::Remote(remote) => (remote != "origin", remote.to_lowercase()),
            _ => (true, String::new()),
        };
        (not_origin, remote, row.label.to_lowercase())
    });
    rows.extend(remote_rows);
    rows
}

/// 虚拟列表的一项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    /// 分组标题(值是该组第一行的下标)。
    Caption(usize),
    Row(usize),
}

/// 分支列表的派生模型:排好的行 + 虚拟列表的项(分组标题 / 行)与尺寸 + 摊开的自然高度。
///
/// 按 (分支数据代号, HEAD, 搜索词) 缓存([`BranchPopup::model`]):弹窗开着时每帧重画、
/// Dialog 的 builder 每帧还要估一次自然高度,不能每帧把上千条远程分支重新过滤、排序
/// 两遍。数据只在分支快照落地(代号 +1)、搜索词变了时重算。
pub(crate) struct RowsModel {
    generation: u64,
    head: Option<String>,
    query: String,
    pub rows: Vec<Row>,
    items: Vec<Item>,
    /// `rows[i]` 是 `items` 里的第几项(= [`child_index_of`] 的口径)。
    item_of_row: Vec<usize>,
    sizes: Rc<Vec<Size<Pixels>>>,
    /// 摊开的自然高度(Dialog 按它定高,再夹到 70vh)。
    height: f32,
}

impl RowsModel {
    fn build(snap: &BranchSnapshot, query: &str) -> Self {
        let rows = build_rows(snap, query);
        let mut items = Vec::with_capacity(rows.len() + 8);
        let mut item_of_row = Vec::with_capacity(rows.len());
        let mut last: Option<&Group> = None;
        for (i, row) in rows.iter().enumerate() {
            if last != Some(&row.group) {
                items.push(Item::Caption(i));
                last = Some(&row.group);
            }
            item_of_row.push(items.len());
            items.push(Item::Row(i));
        }
        let sizes = items
            .iter()
            .map(|item| match item {
                Item::Caption(_) => size(px(0.0), px(GROUP_H)),
                Item::Row(_) => size(px(0.0), px(ROW_H)),
            })
            .collect();
        let groups = items.len() - rows.len();
        let height = if rows.is_empty() {
            CHROME_H + 150.0
        } else {
            CHROME_H + 8.0 + groups as f32 * GROUP_H + rows.len() as f32 * ROW_H
        };
        Self {
            generation: snap.data.generation,
            head: snap.head.clone(),
            query: query.to_string(),
            rows,
            items,
            item_of_row,
            sizes: Rc::new(sizes),
            height,
        }
    }

    /// 是不是按这份快照、这个搜索词算的。
    fn matches(&self, snap: &BranchSnapshot, query: &str) -> bool {
        self.generation == snap.data.generation && self.head == snap.head && self.query == query
    }

    /// 第 `index` 项在列表内容里的上下沿(相对内容顶,不含列表内边距)。
    fn item_span(&self, index: usize) -> Option<(f32, f32)> {
        let size = self.sizes.get(index)?;
        let top: f32 = self.sizes[..index]
            .iter()
            .map(|s| f32::from(s.height))
            .sum();
        Some((top, top + f32::from(size.height)))
    }
}

/// 第 `cursor` 行在列表里是第几项(分组标题也占一项)。逐行数的写法,
/// [`RowsModel::item_of_row`] 一次建好同一张表(键盘移动后滚到可见、→ 弹菜单定位用),
/// 这里留着对拍。
#[cfg(test)]
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
    scroll: VirtualListScrollHandle,
    /// 排好的行(见 [`RowsModel`]),键不对就重算。
    model: RefCell<Option<Rc<RowsModel>>>,
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
                    this.scroll.scroll_to_item(0, ScrollStrategy::Top);
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
            scroll: VirtualListScrollHandle::new(),
            model: RefCell::new(None),
            _subs: subs,
        }
    }

    /// 眼下这份快照、这个搜索词排出来的行(缓存着,见 [`RowsModel`])。
    fn model(&self, cx: &App) -> Rc<RowsModel> {
        let snap = self.panel.read(cx).branch_snapshot();
        self.model_for(&snap, cx)
    }

    fn model_for(&self, snap: &BranchSnapshot, cx: &App) -> Rc<RowsModel> {
        let value = self.query.read(cx).value();
        let query = value.trim();
        if let Some(model) = self
            .model
            .borrow()
            .as_ref()
            .filter(|model| model.matches(snap, query))
        {
            return model.clone();
        }
        let model = Rc::new(RowsModel::build(snap, query));
        *self.model.borrow_mut() = Some(model.clone());
        model
    }

    fn natural_height(&self, cx: &App) -> f32 {
        self.model(cx).height
    }

    fn move_cursor(&mut self, delta: i32, cx: &mut Context<Self>) {
        let model = self.model(cx);
        let len = model.rows.len();
        self.cursor = next_cursor(self.cursor.min(len), len, delta);
        if let Some(&item) = model.item_of_row.get(self.cursor) {
            self.scroll.scroll_to_item(item, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    /// Enter:检出高亮的分支(当前分支则什么都不做)。
    fn checkout_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model(cx);
        let rows = &model.rows;
        let Some(row) = rows
            .get(self.cursor.min(rows.len().saturating_sub(1)))
            .cloned()
        else {
            return;
        };
        if row.is_head || !self.panel.read(cx).branch_snapshot().writable() {
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
        let model = self.model(cx);
        let Some(row) = model.rows.get(index).cloned() else {
            return;
        };
        // 键盘触发:按这一行在列表里的位置算(虚拟列表不登记子元素的 bounds,按项高
        // 与滚动偏移推:行左缘 + 36、行底 − 2,与此前取子元素 bounds 同一个点)
        let position = at
            .or_else(|| {
                let item = *model.item_of_row.get(index)?;
                let (_, bottom) = model.item_span(item)?;
                let list = self.scroll.bounds();
                let offset = self.scroll.offset();
                Some(point(
                    list.left() + px(LIST_PAD + 36.0),
                    list.top() + offset.y + px(LIST_PAD + bottom - 2.0),
                ))
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

/// 命中区间(char 下标,`find_folded` 的口径)→ 字节区间(`StyledText` 的高亮口径)。
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
        let model = self.model_for(&snap, cx);
        let rows = &model.rows;
        // 候选变少之后游标可能越界
        let cursor = self.cursor.min(rows.len().saturating_sub(1));
        let busy = snap.busy;
        let writable = snap.writable();
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
                    !writable || snap.repo.is_empty(),
                )
                // 置灰时说明原因(推送在后台跑着之类);忙闲会变,提示现读
                .tip_live({
                    let panel = self.panel.downgrade();
                    move |cx| {
                        let snap = panel.upgrade().map(|p| p.read(cx).branch_snapshot());
                        if snap.is_some_and(|s| !s.writable()) {
                            git_ui::gate_busy_text().into()
                        } else {
                            t("gitBranch", "newBranchTip").into()
                        }
                    }
                })
                .when(writable && !snap.repo.is_empty(), |el| {
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
                    !writable && !fetching,
                )
                // 忙闲随写操作变(面板自己的,或别的区占着闸):提示现读(见
                // `TooltipExt::tip_live`)。Fetch 自己在跑时标签已写「Fetch 中…」
                .tip_live({
                    let panel = self.panel.downgrade();
                    move |cx| {
                        let snap = panel.upgrade().map(|p| p.read(cx).branch_snapshot());
                        let waiting =
                            snap.is_some_and(|s| !s.writable() && s.busy != Some(GitBusy::Fetch));
                        if waiting {
                            git_ui::gate_busy_text().into()
                        } else {
                            t("gitBranch", "fetchTip").into()
                        }
                    }
                })
                .when(writable, |el| {
                    // Fetch 不收弹窗:回来之后列表就地更新
                    el.on_click(move |_: &ClickEvent, window, cx| {
                        panel_for_fetch.update(cx, |p, cx| p.fetch(window, cx));
                    })
                }),
            );

        // 有行时走变高虚拟列表(分组标题 26、行 28),只给视口里的那几项建元素;空态 /
        // 加载中这类整块占位仍是普通容器
        let mut list = div().id("git-branches-list").flex_1().min_h(px(0.0));

        if rows.is_empty() {
            let this = cx.entity();
            let query_input = self.query.clone();
            list = list.overflow_y_scroll().px(px(LIST_PAD)).py(px(LIST_PAD));
            list = list.child(if snap.loading && snap.data.branches.is_empty() {
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
        } else {
            let items = model.clone();
            list = list.child(
                v_virtual_list(
                    cx.entity(),
                    "git-branches-rows",
                    model.sizes.clone(),
                    move |this, range, _window, cx| {
                        range
                            .filter_map(|ix| items.items.get(ix).copied())
                            .map(|item| match item {
                                Item::Caption(first) => {
                                    git_ui::list_caption(group_label(&items.rows[first].group))
                                        .into_any_element()
                                }
                                Item::Row(index) => {
                                    this.render_row(index, &items.rows[index], index == cursor, cx)
                                }
                            })
                            .collect::<Vec<_>>()
                    },
                )
                .track_scroll(&self.scroll)
                // 组件库给 base 挂的是双轴 `overflow_scroll`,关掉横轴,与换掉的
                // `overflow_y_scroll()` 一致(同「更改」列表的处理)
                .overflow_x_hidden()
                .px(px(LIST_PAD))
                .py(px(LIST_PAD)),
            );
        }

        // detached HEAD 时置顶一句说明(它不是分支,列表里没有它)。显示名本身带括号
        // (`(1a2b3c4)`),文案里另有一层,只取哈希。前后半句分两段排、后半句整段不断:
        // 放不下时整段换到第二行,不会只剩末字单独成行
        let detached_note = snap.detached.as_ref().map(|label| {
            let commit = crate::git_panel::detached_hash(label);
            div()
                .mx(px(12.0))
                .mb(px(8.0))
                .px(px(10.0))
                .py(px(6.0))
                .rounded(px(4.0))
                .bg(ui::with_alpha(ui::color_warning(), 0.12))
                .child(
                    git_ui::notice_row(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_x(px(4.0))
                            .child(tr!("gitBranch", "detachedNote", commit = commit))
                            .child(
                                div()
                                    .flex_none()
                                    .whitespace_nowrap()
                                    .child(t("gitBranch", "detachedNoteTail")),
                            ),
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
            .key_context(BRANCH_POPUP_CONTEXT)
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
/// 远程行没有推送 / 重命名;有写操作在跑(面板自己的,或别的区占着写操作闸)时写类动作
/// 一律禁用,并在 detail 行写明原因(与仓库栏按钮、提交按钮同一句)。
fn row_menu(panel: &Entity<GitPanel>, row: &Row, snap: &BranchSnapshot) -> Vec<MenuEntry> {
    let busy = !snap.writable();
    let current = snap.head.clone();
    // 每个动作:先收弹窗,再交给面板
    let action = |label: String, f: fn(&Entity<GitPanel>, &Row, &mut Window, &mut App)| {
        let (panel, row) = (panel.clone(), row.clone());
        let item = MenuItem::new(label)
            .disabled(busy)
            .on_click(move |window, cx| {
                close_guarded(kind::GIT_BRANCHES, window, cx);
                f(&panel, &row, window, cx);
            });
        if busy {
            item.detail(git_ui::gate_busy_text())
        } else {
            item
        }
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
enum CheckoutStep {
    /// 没被挡住,已照常检出。本地行是 `None`;远程行是建了跟踪分支还是检出了本地已有的。
    Done(Option<RemoteCheckout>),
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
/// 防重入沿用 [`GitPanel::run_write_op`]。本地没有同名分支的远程行不预检,直接
/// `checkout_remote_branch`(它新建跟踪分支;被本地改动挡住时报 git 原文)。
fn checkout(panel: &Entity<GitPanel>, row: &Row, window: &mut Window, cx: &mut App) {
    let name = row.name.clone();
    let remote = row.is_remote;
    // 要预检的本地分支:本地行就是它自己;远程行在本地已有同名分支时,检出的其实是那个
    let local = if remote {
        let snap = panel.read(cx).branch_snapshot();
        split_remote_branch(&snap.data.remotes, &name)
            .map(|(_, local)| local)
            .filter(|local| {
                snap.data
                    .branches
                    .iter()
                    .any(|b| !b.is_remote && &b.name == local)
            })
    } else {
        Some(name.clone())
    };
    panel.update(cx, |p, cx| {
        let shown = name.clone();
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
                if remote {
                    mt_project::git::checkout_remote_branch(repo, &name)
                        .map(|outcome| CheckoutStep::Done(Some(outcome)))
                } else {
                    mt_project::git::checkout_branch(repo, &name).map(|()| CheckoutStep::Done(None))
                }
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
                // 本地行:检出的分支名;远程行:那条远程分支的全名
                let shown = shown.as_str();
                let (message, tone) = match outcome {
                    None => (tr!("gitBranch", "checkedOut", name = shown), Tone::Info),
                    Some(RemoteCheckout::Created { local }) => (
                        tr!(
                            "gitBranch",
                            "checkedOutTracking",
                            name = local,
                            remote = shown
                        ),
                        Tone::Info,
                    ),
                    Some(RemoteCheckout::Existing { local }) => (
                        tr!(
                            "gitBranch",
                            "checkedOutExisting",
                            name = local,
                            remote = shown
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

/// 推送本地分支:打开推送对话框并预选这一行的分支(当前分支即「当前」),先看清待推送的
/// 提交、推送目标(可勾强推)再确认 —— 与仓库栏推送按钮同一个对话框。
fn push_branch(panel: &Entity<GitPanel>, row: &Row, window: &mut Window, cx: &mut App) {
    let branch = (!row.is_head).then(|| row.name.clone());
    panel.update(cx, |p, cx| p.open_push_dialog(branch, window, cx));
}

/// 删除:本地先 `-d`,未合并时二次确认 `-D`;远程删除是危险操作,确认文案说清后果。
fn confirm_delete(panel: &Entity<GitPanel>, row: &Row, window: &mut Window, cx: &mut App) {
    let (panel, name) = (panel.clone(), row.name.clone());
    if row.is_remote {
        let snap = panel.read(cx).branch_snapshot();
        let (remote, branch) = split_remote_branch(&snap.data.remotes, &name)
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
        .danger()
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
    .danger()
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
                    .danger()
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
    /// 执行中 / 上一次提交失败的 git 原文(就地显示,下次修改时清掉)。
    op: FormOp,
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
        .data
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
                form.op.error = None;
                cx.notify();
            }
        });
        NameForm {
            panel,
            mode,
            input: input.clone(),
            checkout: true,
            op: FormOp::default(),
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
            let (show_issue, valid) =
                name_feedback(name_issue(&f.mode, &value, &f.existing), f.touched);
            let (title, ok_label) = match &f.mode {
                NameMode::Create { .. } => (
                    t("gitBranch", "newBranchTitle"),
                    if f.op.busy {
                        t("gitBranch", "creating")
                    } else {
                        t("gitBranch", "create")
                    },
                ),
                NameMode::Rename { .. } => (
                    t("gitBranch", "renameTitle"),
                    if f.op.busy {
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
            if let Some(err) = &f.op.error {
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
                    f.op.busy || !valid,
                ))
                .child(body)
                .on_ok(move |_: &ClickEvent, window, cx| submit_name(&form_for_ok, window, cx))
        },
    );
    autofocus(&input_for_focus, window, cx);
}

/// 名称弹窗的校验反馈:(就地显示的问题, 名字合不合法)。
///
/// 问题要等用户**动过输入框**才显示:一打开就红一行不友好,而重命名预填的正是原名,
/// 「与原名相同」恰恰一打开就成立(E2E 截到过)。确认键则不等 —— 名字不合法就禁用,
/// 回车照样能提交,那时 `submit_name` 置上 `touched`,问题随之显示出来。
fn name_feedback(issue: Option<&'static str>, touched: bool) -> (Option<&'static str>, bool) {
    (issue.filter(|_| touched), issue.is_none())
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
            f.op.busy,
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
        f.op.begin();
        cx.notify();
    });
    let (name, mode_for_task) = (value.clone(), mode.clone());
    let done_form = form.clone();
    let started = git_ui::spawn_gated(
        &repo,
        move |repo| match &mode_for_task {
            NameMode::Create { start } => {
                mt_project::git::create_branch(repo, &name, start.as_deref(), checkout)
            }
            NameMode::Rename { old } => mt_project::git::rename_branch(repo, old, &name),
        },
        move |result, window, cx| match result {
            Ok(()) => {
                done_form.update(cx, |f, _| f.op.finish());
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
                    p.refresh_after_write(cx);
                });
            }
            Err(err) => {
                let message = git_ui::error_text(&err);
                done_form.update(cx, |f, cx| {
                    f.op.fail(message.clone());
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
        },
        window,
        cx,
    );
    if !started {
        // 别的区正在写这个仓库:就地说明,框留着
        form.update(cx, |f, cx| {
            f.op.fail(git_ui::gate_busy_text().to_string());
            cx.notify();
        });
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_panel::BranchData;

    fn branch(name: &str, remote: bool) -> BranchInfo {
        BranchInfo {
            name: name.into(),
            is_head: false,
            is_remote: remote,
            commit_hash: "abc".into(),
            upstream: None,
        }
    }

    fn snapshot_of(
        branches: Vec<BranchInfo>,
        tracking: Vec<BranchTracking>,
        recent: Vec<String>,
        remotes: Vec<String>,
        head: Option<&str>,
    ) -> BranchSnapshot {
        BranchSnapshot {
            repo: "/r".into(),
            data: Rc::new(BranchData {
                generation: 0,
                branches,
                tracking,
                recent,
                remotes,
            }),
            head: head.map(str::to_string),
            detached: None,
            loading: false,
            busy: None,
            gated: false,
        }
    }

    fn snapshot() -> BranchSnapshot {
        snapshot_of(
            vec![
                branch("main", false),
                branch("feature/Login", false),
                branch("dev", false),
                branch("bugfix", false),
                branch("origin/main", true),
                branch("origin/feature/login", true),
                branch("upstream/main", true),
                branch("team/fork/x", true),
            ],
            vec![BranchTracking {
                branch: "main".into(),
                upstream: "origin/main".into(),
                ahead: 2,
                behind: 1,
            }],
            vec!["dev".into(), "main".into(), "gone".into()],
            vec!["origin".into(), "upstream".into(), "team/fork".into()],
            Some("main"),
        )
    }

    /// 改造前的 `find_ci`(整串折好、逐位比对),对拍用。
    fn find_ci_reference(text: &str, query: &str) -> Option<Option<(usize, usize)>> {
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

    /// 改造前的 `build_rows`(逐分支现转小写比较、线性找领先落后、每次匹配现折两份
    /// `Vec<char>`),对拍用。
    fn build_rows_reference(snap: &BranchSnapshot, query: &str) -> Vec<Row> {
        let query = query.trim();
        let data = &snap.data;
        let mut locals: Vec<&BranchInfo> = data.branches.iter().filter(|b| !b.is_remote).collect();
        locals.sort_by_key(|b| b.name.to_lowercase());
        let head = snap.head.as_deref();
        let local_row = |group: Group, branch: &BranchInfo| -> Option<Row> {
            let hit = find_ci_reference(&branch.name, query)?;
            let tracking = data.tracking.iter().find(|t| t.branch == branch.name);
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
        for name in &data.recent {
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
        for branch in data.branches.iter().filter(|b| b.is_remote) {
            let (remote, short) = split_remote_branch(&data.remotes, &branch.name)
                .or_else(|| {
                    branch
                        .name
                        .split_once('/')
                        .map(|(r, s)| (r.to_string(), s.to_string()))
                })
                .unwrap_or_else(|| (String::new(), branch.name.clone()));
            let hit = match find_ci_reference(&short, query) {
                Some(hit) => hit,
                None if find_ci_reference(&branch.name, query).is_some() => None,
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

    /// 一大份混了大小写、中文、带斜杠远程名、重名领先落后、同名不同大小写的分支:
    /// 新旧两版建出来的行一条不差(各种搜索词,含非 ASCII、大小写、空白、命中远程前缀)。
    #[test]
    fn 建行与旧写法逐条一致() {
        let mut branches = Vec::new();
        let names = [
            "main",
            "Main2",
            "dev",
            "DEV-x",
            "feature/Login",
            "feature/login",
            "修复/登录页",
            "修复/LOGIN",
            "release/1.0",
            "Ärger",
            "ärger-2",
            "zeta",
            "Alpha",
            "alpha",
        ];
        for name in names {
            branches.push(branch(name, false));
        }
        for remote in ["origin", "upstream", "team/fork", "Origin"] {
            for name in names.iter().step_by(2) {
                branches.push(branch(&format!("{remote}/{name}"), true));
            }
        }
        branches.push(branch("stray", true));
        let tracking = vec![
            BranchTracking {
                branch: "dev".into(),
                upstream: "origin/dev".into(),
                ahead: 3,
                behind: 0,
            },
            BranchTracking {
                branch: "dev".into(),
                upstream: "upstream/dev".into(),
                ahead: 9,
                behind: 9,
            },
            BranchTracking {
                branch: "修复/登录页".into(),
                upstream: "origin/修复/登录页".into(),
                ahead: 0,
                behind: 4,
            },
        ];
        let snap = snapshot_of(
            branches,
            tracking,
            vec!["zeta".into(), "dev".into(), "main".into(), "nope".into()],
            vec!["origin".into(), "upstream".into(), "team/fork".into()],
            Some("dev"),
        );
        for query in [
            "", "  ", "LOGIN", "login", "修复", "登录", "ärg", "ÄRG", "origin/", "fork/m", "a",
            "zz", " Dev ", "/",
        ] {
            assert_eq!(
                build_rows(&snap, query),
                build_rows_reference(&snap, query),
                "搜索词 {query:?}"
            );
        }
    }

    /// 新的子串匹配与旧写法逐个一致(大小写、非 ASCII、子串在末尾、搜索词比原文长)。
    #[test]
    fn 子串匹配与旧写法一致() {
        let texts = [
            "Feature/X",
            "修复/登录页",
            "ÄrgerÄrger",
            "aaab",
            "",
            "ab",
            "İstanbul",
        ];
        let queries = [
            "", "x", "X", "登录", "äR", "aab", "ab", "abc", "b", "i̇", "STAN",
        ];
        for text in texts {
            for query in queries {
                assert_eq!(
                    find_ci(text, query),
                    find_ci_reference(text, query),
                    "{text:?} / {query:?}"
                );
            }
        }
    }

    /// 派生模型:项序 = 每组一个标题再跟组里的行,行到项的下标与逐行数的一致,尺寸按
    /// 标题 26 / 行 28,自然高度与旧估法一致;同一份快照、同一个搜索词才算命中缓存。
    #[test]
    fn 派生模型与逐行数一致() {
        let snap = snapshot();
        for query in ["", "main", "zzz"] {
            let model = RowsModel::build(&snap, query);
            assert_eq!(model.rows, build_rows(&snap, query));
            for i in 0..model.rows.len() {
                assert_eq!(Some(model.item_of_row[i]), child_index_of(&model.rows, i));
                assert_eq!(model.items[model.item_of_row[i]], Item::Row(i));
            }
            let groups = model
                .items
                .iter()
                .filter(|item| matches!(item, Item::Caption(_)))
                .count();
            for (item, size) in model.items.iter().zip(model.sizes.iter()) {
                let want = match item {
                    Item::Caption(_) => GROUP_H,
                    Item::Row(_) => ROW_H,
                };
                assert_eq!(f32::from(size.height), want);
            }
            let want_height = if model.rows.is_empty() {
                CHROME_H + 150.0
            } else {
                CHROME_H + 8.0 + groups as f32 * GROUP_H + model.rows.len() as f32 * ROW_H
            };
            assert_eq!(model.height, want_height, "搜索词 {query:?}");
            assert!(model.matches(&snap, query));
            assert!(!model.matches(&snap, "other"));
        }
        let model = RowsModel::build(&snap, "");
        // 第 2 项([标题 当前][main][标题 最近][dev]…)的上下沿
        assert_eq!(
            model.item_span(2),
            Some((GROUP_H + ROW_H, 2.0 * GROUP_H + ROW_H))
        );
        let mut moved = snap.clone();
        moved.head = Some("dev".into());
        assert!(!model.matches(&moved, ""), "HEAD 换了要重排");
        let mut newer = snap.clone();
        newer.data = Rc::new(BranchData {
            generation: 1,
            branches: snap.data.branches.clone(),
            tracking: snap.data.tracking.clone(),
            recent: snap.data.recent.clone(),
            remotes: snap.data.remotes.clone(),
        });
        assert!(!model.matches(&newer, ""), "分支数据换了一代要重排");
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

    /// 重命名弹窗一打开(预填原名、用户还没动)不红字,只把确认键禁用;
    /// 动过之后问题才就地显示;合法名字确认键可点。新建弹窗同一口径。
    #[test]
    fn 名称问题等用户动过才显示() {
        let existing = vec!["main".to_string(), "dev".to_string()];
        let rename = NameMode::Rename { old: "dev".into() };
        let opened = name_feedback(name_issue(&rename, "dev", &existing), false);
        assert_eq!(opened, (None, false), "一打开不红字,确认键禁用");
        let edited = name_feedback(name_issue(&rename, "dev", &existing), true);
        assert_eq!(edited, (Some("nameUnchanged"), false));
        let ok = name_feedback(name_issue(&rename, "dev2", &existing), true);
        assert_eq!(ok, (None, true));
        let create = NameMode::Create { start: None };
        let fresh = name_feedback(name_issue(&create, "", &existing), false);
        assert_eq!(fresh, (None, false));
    }
}
