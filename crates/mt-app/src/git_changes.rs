//! Git 面板的「更改」区。对应 `src/components/GitChanges.tsx`(442 行),
//! 之后按 IDEA 的 Commit 工具窗补齐了多选、键盘、冲突处理与提交区增强。
//!
//! # 四个分组的口径
//!
//! ```text
//! conflicts = 任一侧是 conflicted(置顶,有冲突才出现)
//! staged    = 有 stagedStatus,且不是冲突
//! unstaged  = 有 unstagedStatus 且不是 untracked,且不是冲突
//! untracked = unstagedStatus === 'untracked'
//! ```
//!
//! ⚠️ **同一个文件可以同时出现在 staged 与 unstaged 两组**(部分暂存),这是正确
//! 行为 —— 所以行的 `ElementId` 必须带区名前缀(原版 key 是 `${area}-${path}`,
//! 规格 §11 第 29 条),否则 gpui 会撞 id。冲突文件在 git2 的状态里两侧都是
//! conflicted,只进冲突组,不再在已暂存 / 未暂存里各出现一次。
//!
//! # 选择与键盘
//!
//! 单击选中(不再直接开 diff —— 否则没法 Shift 连选),**双击 / Enter 开 diff**;
//! Ctrl/Cmd+单击切换、Shift+单击连选,都只在同一分组内。选中多项时右键菜单与行内
//! 按钮对整组生效。列表获得焦点后 ↑/↓ 移动、Space 暂存 / 取消暂存、Delete 丢弃
//! (键位绑在 `hotkeys.rs` 的表外段,上下文 [`LIST_CONTEXT`],终端拿不到焦点时才生效)。
//!
//! # 丢弃 = 只丢未暂存的部分
//!
//! 原先的「丢弃修改」先把文件从暂存区撤回再从 HEAD 检出,部分暂存的文件连已暂存的
//! 那部分一起没了。现在与 `git restore` / IDEA 的回滚一致:从**暂存区**检出
//! (`mt_project::git::discard_unstaged`),已暂存的部分保留;未跟踪文件照旧删除。
//!
//! # 失败要让用户看得见
//!
//! 原版每个 `invoke` 的 catch 都只 `console.error`(不弹 toast、不显红)。这里
//! 不照抄 —— 只打日志的结果是「点了没反应」,而提交被 hook 拒掉恰恰是用户最需要
//! 知道原因的时候。口径按失败出在哪儿分三种(日志照打):
//!
//! - **提交 / 推送失败**(hook 拒绝 / GPG 出错 / 60s 超时 / 推送被拒):错误原文**就地**
//!   挂在提交按钮下方,错误色、可滚动、带复制按钮 —— 提交的错误常是 hook 的多行输出,
//!   tooltip 装不下。下一次提交、提交成功或换仓库时清掉(hook 输出得留给人读);
//!   「提交成功、推送失败」明确写成「已提交,但推送失败」;
//! - **暂存 / 取消暂存 / 丢弃 / 冲突处理失败**:推 toast。抽屉不是弹窗,toast 层画在
//!   抽屉之上看得见;这几个动作的结果本来就体现在列表上,就地再挂一行反倒挤占列表;
//! - **取变更失败**:列表区的空态换成错误原文 —— 否则读失败会被误认成
//!   「工作区是干净的」。它随 pty 输出每 500ms 去抖重取,推 toast 会刷屏,所以就地。
//!
//! # 阻塞调用
//!
//! 全部 git 调用(git2 的同步 IO 与带超时的 git CLI)一律丢 `cx.background_executor()`
//! (范式照 `file_tree.rs:138-156`);异步回包用请求令牌防迟到。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, EventEmitter,
    FocusHandle, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    Pixels, Point, Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{InputEvent, Textarea, TextareaState};
use mt_project::git::{
    AmendInfo, ChangeFileStatus, CommitMessageEntry, CommitTarget, ConflictInfo, ConflictSide,
    GitStatus,
};
use mt_ui::icons::FileIcon;
use mt_ui::tooltip::TooltipExt as _;

use crate::git_ui::{
    self, ButtonKind, ICON_ALERT, ICON_CHEVRON_DOWN, ICON_CHEVRON_RIGHT, ICON_CLEAN, ICON_COPY,
    ICON_HISTORY, ICON_MINUS, ICON_PLUS, ICON_UNDO, SectionHeader, Tone, danger_icon_button,
    icon_button, mono, status_color, status_label_for,
};
use crate::i18n::{t, tr};
use crate::menu::{self, MenuItem};
use crate::prompt::Confirm;
use crate::store::AppStore;
use crate::ui;
use crate::{git_diff, git_watch};

gpui::actions!(
    mini_term,
    [
        /// 提交(Ctrl+Enter / Cmd+Enter,`GitChanges.tsx:411-415`)
        GitCommitMessage,
        /// 提交并推送(Ctrl+Alt+Enter / Cmd+Alt+Enter)
        GitCommitAndPush,
        /// 更改列表:选中上一行(↑)
        GitChangesSelectPrev,
        /// 更改列表:选中下一行(↓)
        GitChangesSelectNext,
        /// 更改列表:暂存 / 取消暂存所选(Space)
        GitChangesToggleStage,
        /// 更改列表:查看所选的 diff(Enter)
        GitChangesOpenDiff,
        /// 更改列表:丢弃所选的未暂存修改(Delete)
        GitChangesDiscard,
    ]
);

/// 更改列表的 key context(列表获得焦点时生效,键位见 `hotkeys.rs` 表外段)。
pub const LIST_CONTEXT: &str = "GitChangesList";

/// 提交标题的软上限(超出只提示,不拦截)。
const TITLE_SOFT_LIMIT: usize = 72;
/// 标题字数到这里开始显示计数。
const TITLE_COUNT_FROM: usize = 60;
/// 提交消息历史取多少条。
const HISTORY_LIMIT: usize = 20;

/// 「更改」区往上冒的事件。原版是 `onCommitSuccess` 这个 prop。
pub enum GitChangesEvent {
    /// 提交(或提交后的推送)成功 —— 容器要刷新历史与分支(分支头已前移)。
    Committed,
    /// ▾ 菜单「贮藏当前改动…」贮藏成功 —— 工作区与贮藏数都变了,容器整面板刷新。
    Stashed,
}

/// 四个分组。`area` 既决定取哪一个 status,也进 `ElementId` 前缀。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Area {
    Conflicts,
    Staged,
    Unstaged,
    Untracked,
}

impl Area {
    /// 渲染 / 键盘导航的分组顺序:冲突置顶。
    const ORDER: [Area; 4] = [
        Area::Conflicts,
        Area::Staged,
        Area::Unstaged,
        Area::Untracked,
    ];

    fn key(self) -> &'static str {
        match self {
            Area::Conflicts => "conflicts",
            Area::Staged => "staged",
            Area::Unstaged => "unstaged",
            Area::Untracked => "untracked",
        }
    }
}

fn is_conflicted(file: &ChangeFileStatus) -> bool {
    file.staged_status == Some(GitStatus::Conflicted)
        || file.unstaged_status == Some(GitStatus::Conflicted)
}

/// 该文件属不属于该分组(口径见模块注释)。
pub fn in_area(file: &ChangeFileStatus, area: Area) -> bool {
    let conflicted = is_conflicted(file);
    match area {
        Area::Conflicts => conflicted,
        Area::Staged => !conflicted && file.staged_status.is_some(),
        Area::Unstaged => {
            !conflicted && matches!(file.unstaged_status, Some(ref s) if *s != GitStatus::Untracked)
        }
        Area::Untracked => file.unstaged_status == Some(GitStatus::Untracked),
    }
}

/// 该文件在该区里生效的状态(staged 区取 `stagedStatus`,其余取 `unstagedStatus`)。
pub fn status_in_area(file: &ChangeFileStatus, area: Area) -> Option<&GitStatus> {
    match area {
        Area::Staged => file.staged_status.as_ref(),
        _ => file.unstaged_status.as_ref(),
    }
}

// 状态 → 字母 / 颜色的映射(`status_label_for` / `status_color`)收在 `git_ui`:
// 提交详情、提交 diff、单文件历史与这里共用一份。

/// 状态 → 一句话(行 tooltip 用)。
fn status_text(status: Option<&GitStatus>) -> &'static str {
    match status {
        Some(GitStatus::Modified) => t("gitCommit", "statusModified"),
        Some(GitStatus::Added) => t("gitCommit", "statusAdded"),
        Some(GitStatus::Deleted) => t("gitCommit", "statusDeleted"),
        Some(GitStatus::Renamed) => t("gitCommit", "statusRenamed"),
        Some(GitStatus::Untracked) => t("gitCommit", "statusUntracked"),
        Some(GitStatus::Conflicted) => t("gitCommit", "statusConflicted"),
        None => "",
    }
}

/// 提交消息的轻提示(不拦截)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MessageHints {
    /// 标题行字数(按字符,不是字节)
    pub title_chars: usize,
    /// 标题超过 72 字符
    pub title_too_long: bool,
    /// 标题与正文之间没有空行
    pub missing_blank_line: bool,
}

/// 算提交消息的轻提示。开头的空行 git 会剥掉,不算标题。
pub fn message_hints(message: &str) -> MessageHints {
    let body = message.trim_start_matches(['\n', '\r']);
    let mut lines = body.lines();
    let title_chars = lines.next().map_or(0, |l| l.trim_end().chars().count());
    let missing_blank_line = lines.next().is_some_and(|l| !l.trim().is_empty());
    MessageHints {
        title_chars,
        title_too_long: title_chars > TITLE_SOFT_LIMIT,
        missing_blank_line,
    }
}

// ─── 树形视图 ─────────────────────────────────────────────────

/// `buildFileTree` 建出来的节点(`GitChanges.tsx:36-56`)。
///
/// `file` 是**下标**而不是拷贝 —— 建树只是换个排列方式,内容仍在原数组里。
#[derive(Debug, PartialEq)]
pub struct FileTreeNode {
    pub name: String,
    pub full_path: String,
    pub file: Option<usize>,
    pub children: Vec<FileTreeNode>,
}

/// 按 `/` 切 path 建目录树。**不做单链目录压缩**(与 FileTree 的
/// `compactDirChains` 不同,这里没有)。
pub fn build_file_tree(paths: &[(usize, &str)]) -> Vec<FileTreeNode> {
    let mut root: Vec<FileTreeNode> = Vec::new();
    for (index, path) in paths {
        let parts: Vec<&str> = path.split('/').collect();
        let mut current = &mut root;
        let mut path_so_far = String::new();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                path_so_far.push('/');
            }
            path_so_far.push_str(part);
            if i == parts.len() - 1 {
                current.push(FileTreeNode {
                    name: (*part).to_string(),
                    full_path: path_so_far.clone(),
                    file: Some(*index),
                    children: Vec::new(),
                });
            } else {
                let pos = current
                    .iter()
                    .position(|n| n.name == *part && n.file.is_none());
                let pos = match pos {
                    Some(pos) => pos,
                    None => {
                        current.push(FileTreeNode {
                            name: (*part).to_string(),
                            full_path: path_so_far.clone(),
                            file: None,
                            children: Vec::new(),
                        });
                        current.len() - 1
                    }
                };
                current = &mut current[pos].children;
            }
        }
    }
    root
}

/// 拍平后的一行。
enum TreeRow {
    Dir {
        name: String,
        full_path: String,
        depth: usize,
        collapsed: bool,
    },
    File {
        index: usize,
        depth: usize,
    },
}

fn flatten_tree(
    nodes: &[FileTreeNode],
    area: Area,
    collapsed: &HashSet<String>,
    depth: usize,
    out: &mut Vec<TreeRow>,
) {
    for node in nodes {
        match node.file {
            Some(index) => out.push(TreeRow::File { index, depth }),
            None => {
                // 折叠集合 key 带区名 —— 同一路径在四个区各自独立折叠
                // (`GitChanges.tsx:285`、`:292`)
                let key = format!("{}:{}", area.key(), node.full_path);
                let is_collapsed = collapsed.contains(&key);
                out.push(TreeRow::Dir {
                    name: node.name.clone(),
                    full_path: node.full_path.clone(),
                    depth,
                    collapsed: is_collapsed,
                });
                if !is_collapsed {
                    flatten_tree(&node.children, area, collapsed, depth + 1, out);
                }
            }
        }
    }
}

// ─── 组件 ─────────────────────────────────────────────────────

/// 一次后台取数的结果(变更 + 冲突细节 + 提交区要的 HEAD 信息)。
struct Snapshot {
    changes: Vec<ChangeFileStatus>,
    conflicts: Vec<ConflictInfo>,
    target: Option<CommitTarget>,
    has_head: bool,
    /// 勾着 amend 时顺带刷新「已推送」判定;没勾为 `None`
    amend: Option<Option<AmendInfo>>,
}

/// 列表里渲染出来的一行是什么(键盘导航要把选中行滚进视野,得知道它是第几个子元素)。
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowKey {
    Other,
    File(Area, usize),
}

pub struct GitChanges {
    store: Entity<AppStore>,
    /// 空串 = 无仓库。此时 `load` 直接 return(既不 loading 也不报错,
    /// 停在空态 ——`GitChanges.tsx:115`)。
    repo_path: String,
    changes: Vec<ChangeFileStatus>,
    /// 冲突文件的三方条目在不在(删除 / 修改冲突时有一侧缺席),随列表一起取
    conflicts: Vec<ConflictInfo>,
    loading: bool,
    /// 上一次取变更失败的原文;`Some` 时列表区显示它而不是空态。
    load_error: Option<String>,
    /// 组件态,**不落盘**(抽屉一关就没,规格 §11 第 25 条)。
    collapsed_dirs: HashSet<String>,
    collapsed_groups: HashSet<Area>,
    /// 迟到响应丢弃。
    request: u64,
    /// pty-output 嗅探的 500ms 去抖终点。
    debounce_until: Option<Instant>,
    /// 进行中的整文件操作数(暂存 / 丢弃 / 冲突处理)。`> 0` 时行内与组操作按钮禁用
    busy_ops: usize,

    // ── 选择 ──
    /// 选中项所在的分组。多选只在同一分组内
    sel_area: Option<Area>,
    selected: HashSet<String>,
    /// Shift 连选的锚点
    anchor: Option<String>,
    /// 键盘游标(↑/↓ 从这里走)
    cursor: Option<(Area, String)>,
    /// 操作完重取列表后要落回的位置:同一分组的第 n 行(Space 暂存后停在下一行)
    reselect: Option<(Area, usize)>,
    list_focus: FocusHandle,
    list_scroll: ScrollHandle,
    /// 上一帧列表的子元素都是什么(与子元素一一对应)
    row_keys: Vec<RowKey>,

    // ── 提交区 ──
    commit_input: Entity<TextareaState>,
    /// 占位符里写的分支(随列表一起取)
    target: Option<CommitTarget>,
    /// 当前占位符的文本 —— 只在变化时改,render 里改别的实体要克制
    placeholder: Option<String>,
    has_head: bool,
    amend: bool,
    amend_info: Option<AmendInfo>,
    /// amend 勾上时预填进去的那段消息(取消勾选时消息没动过就清掉)
    amend_prefill: Option<String>,
    amend_request: u64,
    committing: bool,
    pushing: bool,
    history_loading: bool,
    /// 上一次提交 / 推送失败的说明(已格式化好的整句),挂在提交按钮下方。
    commit_error: Option<String>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<GitChangesEvent> for GitChanges {}

impl GitChanges {
    pub fn new(store: Entity<AppStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let commit_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 8)
                .placeholder(t("panels", "commitPlaceholder"))
        });
        // 消息一改就重画:标题字数 / 软提示 / 提交按钮的可用态都跟着消息走
        let subs = vec![cx.subscribe(&commit_input, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })];
        Self {
            store,
            repo_path: String::new(),
            changes: Vec::new(),
            conflicts: Vec::new(),
            loading: false,
            load_error: None,
            collapsed_dirs: HashSet::new(),
            collapsed_groups: HashSet::new(),
            request: 0,
            debounce_until: None,
            busy_ops: 0,
            sel_area: None,
            selected: HashSet::new(),
            anchor: None,
            cursor: None,
            reselect: None,
            list_focus: cx.focus_handle(),
            list_scroll: ScrollHandle::new(),
            row_keys: Vec::new(),
            commit_input,
            target: None,
            placeholder: None,
            has_head: false,
            amend: false,
            amend_info: None,
            amend_prefill: None,
            amend_request: 0,
            committing: false,
            pushing: false,
            history_loading: false,
            commit_error: None,
            _subs: subs,
        }
    }

    /// 换仓库(容器调)。空串 = 无仓库。
    pub fn set_repo(&mut self, repo_path: &str, cx: &mut Context<Self>) {
        if self.repo_path == repo_path {
            return;
        }
        self.repo_path = repo_path.to_string();
        self.changes.clear();
        self.conflicts.clear();
        self.collapsed_dirs.clear();
        self.clear_selection();
        self.target = None;
        self.has_head = false;
        // amend 属于上一个仓库的 HEAD
        self.amend = false;
        self.amend_info = None;
        self.amend_prefill = None;
        // 错误都属于上一个仓库
        self.load_error = None;
        self.commit_error = None;
        self.load(cx);
        cx.notify();
    }

    /// 容器的 pty-output 嗅探命中了 —— 起(或推后)自己那个 500ms 去抖窗口。
    ///
    /// 原版是两个面板**各有一个**定时器(规格 §11 第 27 条),所以这里的窗口
    /// 与提交历史区各算各的。
    pub fn note_pty_hit(&mut self) {
        self.debounce_until = Some(Instant::now() + Duration::from_millis(git_watch::DEBOUNCE_MS));
    }

    /// 容器的节拍:去抖窗口到点了就重取。
    pub fn tick(&mut self, cx: &mut Context<Self>) {
        if self.debounce_until.is_some_and(|at| Instant::now() >= at) {
            self.debounce_until = None;
            self.load(cx);
        }
    }

    pub fn load(&mut self, cx: &mut Context<Self>) {
        if self.repo_path.is_empty() {
            return;
        }
        self.loading = true;
        self.request += 1;
        let req = self.request;
        let repo = PathBuf::from(&self.repo_path);
        let want_amend = self.amend;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { load_snapshot(&repo, want_amend) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.request != req {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(snap) => {
                        this.changes = snap.changes;
                        this.conflicts = snap.conflicts;
                        this.target = snap.target;
                        this.has_head = snap.has_head;
                        if !this.has_head {
                            this.amend = false;
                        }
                        if let Some(info) = snap.amend.filter(|_| this.amend) {
                            this.amend_info = info;
                        }
                        this.load_error = None;
                    }
                    Err(err) => {
                        // 原版 `.catch(() => setChanges([]))` —— 失败即清空;
                        // 这里另把原因留给列表区显示,别让人当成「没有变更」
                        eprintln!("[git] 取变更失败: {err:#}");
                        this.changes.clear();
                        this.conflicts.clear();
                        this.load_error = Some(format!("{err:#}"));
                    }
                }
                this.after_reload(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn tree_mode(&self, cx: &App) -> bool {
        self.store.read(cx).git_changes_view_mode() == "tree"
    }

    fn group_indices(&self, area: Area) -> Vec<usize> {
        self.changes
            .iter()
            .enumerate()
            .filter(|(_, c)| in_area(c, area))
            .map(|(i, _)| i)
            .collect()
    }

    /// 某个分组里文件的**显示顺序**(树形视图按树走,折起来的目录里的文件不算)。
    fn area_order(&self, area: Area, tree_mode: bool) -> Vec<usize> {
        let indices = self.group_indices(area);
        if !tree_mode {
            return indices;
        }
        let paths: Vec<(usize, &str)> = indices
            .iter()
            .map(|&i| (i, self.changes[i].path.as_str()))
            .collect();
        let mut rows = Vec::new();
        flatten_tree(
            &build_file_tree(&paths),
            area,
            &self.collapsed_dirs,
            0,
            &mut rows,
        );
        rows.into_iter()
            .filter_map(|row| match row {
                TreeRow::File { index, .. } => Some(index),
                TreeRow::Dir { .. } => None,
            })
            .collect()
    }

    /// 键盘导航走的全部可见文件行(分组顺序 + 组内显示顺序,折起来的分组跳过)。
    fn nav_rows(&self, tree_mode: bool) -> Vec<(Area, usize)> {
        Area::ORDER
            .into_iter()
            .filter(|area| !self.collapsed_groups.contains(area))
            .flat_map(|area| {
                self.area_order(area, tree_mode)
                    .into_iter()
                    .map(move |i| (area, i))
            })
            .collect()
    }

    // ── 选择 ──

    fn clear_selection(&mut self) {
        self.sel_area = None;
        self.selected.clear();
        self.anchor = None;
        self.cursor = None;
    }

    fn is_selected(&self, area: Area, path: &str) -> bool {
        self.sel_area == Some(area) && self.selected.contains(path)
    }

    fn select_single(&mut self, area: Area, index: usize) {
        let Some(path) = self.changes.get(index).map(|c| c.path.clone()) else {
            return;
        };
        self.sel_area = Some(area);
        self.selected.clear();
        self.selected.insert(path.clone());
        self.anchor = Some(path.clone());
        self.cursor = Some((area, path));
    }

    /// 单击行:修饰键决定是单选、切换还是连选;双击开 diff。
    fn click_row(
        &mut self,
        area: Area,
        index: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.list_focus, cx);
        let Some(path) = self.changes.get(index).map(|c| c.path.clone()) else {
            return;
        };
        let modifiers = event.modifiers();
        let same_area = self.sel_area == Some(area);
        if modifiers.shift && same_area && self.anchor.is_some() {
            let order = self.area_order(area, self.tree_mode(cx));
            let anchor = self.anchor.clone().unwrap_or_default();
            let pos = |p: &str| order.iter().position(|&i| self.changes[i].path == p);
            if let (Some(a), Some(b)) = (pos(&anchor), pos(&path)) {
                let (lo, hi) = (a.min(b), a.max(b));
                self.selected = order[lo..=hi]
                    .iter()
                    .map(|&i| self.changes[i].path.clone())
                    .collect();
                self.cursor = Some((area, path));
            } else {
                self.select_single(area, index);
            }
        } else if modifiers.secondary() {
            if !same_area {
                self.select_single(area, index);
            } else {
                if !self.selected.remove(&path) {
                    self.selected.insert(path.clone());
                }
                self.anchor = Some(path.clone());
                self.cursor = Some((area, path));
                if self.selected.is_empty() {
                    self.sel_area = None;
                }
            }
        } else {
            self.select_single(area, index);
            if event.click_count() >= 2 {
                self.view_diff(index, area, window, cx);
            }
        }
        cx.notify();
    }

    /// 某一行上的动作要作用于哪些文件:行在选中集里 → 整个选中集(按显示顺序),
    /// 否则只有它自己。
    fn targets(&self, area: Area, index: usize, cx: &App) -> Vec<String> {
        let Some(path) = self.changes.get(index).map(|c| c.path.as_str()) else {
            return Vec::new();
        };
        if !self.is_selected(area, path) || self.selected.len() <= 1 {
            return vec![path.to_string()];
        }
        self.area_order(area, self.tree_mode(cx))
            .into_iter()
            .map(|i| self.changes[i].path.clone())
            .filter(|p| self.selected.contains(p))
            .collect()
    }

    /// 当前选中集(键盘动作用),按显示顺序。
    fn selection_targets(&self, cx: &App) -> Option<(Area, Vec<String>)> {
        let area = self.sel_area?;
        let paths: Vec<String> = self
            .area_order(area, self.tree_mode(cx))
            .into_iter()
            .map(|i| self.changes[i].path.clone())
            .filter(|p| self.selected.contains(p))
            .collect();
        (!paths.is_empty()).then_some((area, paths))
    }

    /// 选中集里第一项在本组显示顺序里的位置(操作完落回这里 = 停在「下一行」)。
    fn selection_anchor_pos(&self, area: Area, cx: &App) -> Option<usize> {
        self.area_order(area, self.tree_mode(cx))
            .iter()
            .position(|&i| self.selected.contains(&self.changes[i].path))
    }

    /// 重取列表之后:清掉已不在原分组里的选中项;有待落回的位置就落回去。
    fn after_reload(&mut self, cx: &mut Context<Self>) {
        let tree_mode = self.tree_mode(cx);
        if let Some((area, pos)) = self.reselect.take() {
            let order = self.area_order(area, tree_mode);
            if let Some(&index) = order.get(pos).or(order.last()) {
                self.select_single(area, index);
                self.scroll_to_row(RowKey::File(area, index));
                return;
            }
            self.clear_selection();
            return;
        }
        if let Some(area) = self.sel_area {
            let present: HashSet<&str> = self
                .group_indices(area)
                .into_iter()
                .map(|i| self.changes[i].path.as_str())
                .collect();
            self.selected.retain(|p| present.contains(p.as_str()));
            if self.selected.is_empty() {
                self.clear_selection();
            } else if self
                .cursor
                .as_ref()
                .is_some_and(|(_, p)| !present.contains(p.as_str()))
            {
                self.cursor = None;
            }
        }
    }

    fn scroll_to_row(&self, key: RowKey) {
        if let Some(child) = self.row_keys.iter().position(|k| *k == key) {
            self.list_scroll.scroll_to_item(child);
        }
    }

    fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.nav_rows(self.tree_mode(cx));
        if rows.is_empty() {
            return;
        }
        let current = self.cursor.as_ref().and_then(|(area, path)| {
            rows.iter()
                .position(|&(a, i)| a == *area && self.changes[i].path == *path)
        });
        let next = match current {
            None if delta > 0 => 0,
            None => rows.len() - 1,
            Some(pos) => (pos as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
        };
        let (area, index) = rows[next];
        self.select_single(area, index);
        self.scroll_to_row(RowKey::File(area, index));
        cx.notify();
    }

    fn on_select_prev(&mut self, _: &GitChangesSelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(-1, cx);
    }

    fn on_select_next(&mut self, _: &GitChangesSelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(1, cx);
    }

    /// Space:未暂存 / 未跟踪 → 暂存,已暂存 → 取消暂存。完成后停在原位置的下一行。
    fn on_toggle_stage(
        &mut self,
        _: &GitChangesToggleStage,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 上一次操作还在跑:连按 Space 不叠第二次(与按钮在 busy 时禁用同一口径)
        if self.busy_ops > 0 {
            return;
        }
        let Some((area, paths)) = self.selection_targets(cx) else {
            return;
        };
        let pos = self.selection_anchor_pos(area, cx).unwrap_or(0);
        match area {
            Area::Staged => self.unstage(paths, Some((area, pos)), cx),
            Area::Unstaged | Area::Untracked => self.stage(paths, Some((area, pos)), cx),
            Area::Conflicts => {}
        }
    }

    fn on_open_diff(
        &mut self,
        _: &GitChangesOpenDiff,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((area, path)) = self.cursor.clone() else {
            return;
        };
        if let Some(index) = self.changes.iter().position(|c| c.path == path) {
            self.view_diff(index, area, window, cx);
        }
    }

    fn on_discard_key(
        &mut self,
        _: &GitChangesDiscard,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy_ops > 0 {
            return;
        }
        if let Some((area @ (Area::Unstaged | Area::Untracked), paths)) = self.selection_targets(cx)
        {
            self.discard(area, paths, window, cx);
        }
    }

    // ── 整文件操作 ──

    /// 跑一件阻塞的 git 动作(暂存 / 取消暂存 / 丢弃 / 冲突处理),完成后重取列表。
    /// 失败推 toast(口径见模块注释)。`reselect` 是完成后要落回的位置。
    fn run_op(
        &mut self,
        op: impl FnOnce(&Path) -> anyhow::Result<()> + Send + 'static,
        reselect: Option<(Area, usize)>,
        cx: &mut Context<Self>,
    ) {
        if self.repo_path.is_empty() {
            return;
        }
        self.busy_ops += 1;
        cx.notify();
        let repo = PathBuf::from(&self.repo_path);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { op(&repo) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.busy_ops = this.busy_ops.saturating_sub(1);
                match result {
                    Ok(()) => this.reselect = reselect,
                    Err(err) => {
                        eprintln!("[git] 变更操作失败: {err:#}");
                        this.notify_op_failure(format!("{err:#}"), cx);
                    }
                }
                this.load(cx);
            });
        })
        .detach();
    }

    /// 暂存 / 丢弃这类动作失败的 toast(需要留意那一档:`!` 图标、点击只关闭),
    /// 正文带错误原文。走 Git 面板共用的结果 toast。
    fn notify_op_failure(&self, detail: String, cx: &mut Context<Self>) {
        git_ui::toast(
            &self.store,
            tr!("gitChanges", "operationFailed", detail = detail),
            Tone::Attention,
            cx,
        );
    }

    /// 成功类的提示(已提交 / 已推送)。信息档 toast,点击只关闭。
    fn notify_info(&self, message: String, cx: &mut Context<Self>) {
        git_ui::toast(&self.store, message, Tone::Info, cx);
    }

    fn stage(
        &mut self,
        paths: Vec<String>,
        reselect: Option<(Area, usize)>,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        self.run_op(
            move |repo| mt_project::git::git_stage(repo, &paths),
            reselect,
            cx,
        );
    }

    fn unstage(
        &mut self,
        paths: Vec<String>,
        reselect: Option<(Area, usize)>,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        self.run_op(
            move |repo| mt_project::git::git_unstage(repo, &paths),
            reselect,
            cx,
        );
    }

    /// 某个分组的全部路径。
    fn area_paths(&self, area: Area) -> Vec<String> {
        self.group_indices(area)
            .into_iter()
            .map(|i| self.changes[i].path.clone())
            .collect()
    }

    /// 分组标题上的「全部暂存」:**只暂存这一组**。原先未跟踪组的按钮也调
    /// `git_stage_all`(`add_all("*")`),会把已跟踪的改动一起暂存。
    fn stage_group(&mut self, area: Area, cx: &mut Context<Self>) {
        let paths = self.area_paths(area);
        self.stage(paths, None, cx);
    }

    fn unstage_all(&mut self, cx: &mut Context<Self>) {
        self.run_op(mt_project::git::git_unstage_all, None, cx);
    }

    /// 丢弃(先确认)。未暂存组只丢未暂存的部分(已暂存的保留),未跟踪组删除文件。
    fn discard(
        &mut self,
        area: Area,
        paths: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() || !matches!(area, Area::Unstaged | Area::Untracked) {
            return;
        }
        let count = paths.len().to_string();
        let message = if area == Area::Untracked {
            tr!("gitCommit", "discardUntrackedConfirm", count = count)
        } else {
            tr!("gitCommit", "discardUnstagedConfirm", count = count)
        };
        let this = cx.entity();
        Confirm::new(t("gitChanges", "discardTitle"), message)
            .detail(preview_paths(&paths))
            .ok_text(t("gitChanges", "discardOk"))
            .cancel_text(t("gitChanges", "discardCancel"))
            .open(
                move |_window, cx| {
                    let paths = paths.clone();
                    this.update(cx, |this, cx| {
                        let reselect = this.selection_anchor_pos(area, cx).map(|pos| (area, pos));
                        this.run_op(
                            move |repo| mt_project::git::discard_unstaged(repo, &paths),
                            reselect,
                            cx,
                        );
                    });
                },
                window,
                cx,
            );
    }

    /// 冲突文件采用一侧。
    fn take_side(&mut self, paths: Vec<String>, side: ConflictSide, cx: &mut Context<Self>) {
        self.run_op(
            move |repo| {
                for path in &paths {
                    mt_project::git::resolve_conflict_with(repo, path, side)?;
                }
                Ok(())
            },
            None,
            cx,
        );
    }

    /// 标记为已解决:先在后台看一眼工作区里还有没有 `<<<<<<<`,有就先确认。
    fn mark_resolved(&mut self, paths: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        if paths.is_empty() || self.repo_path.is_empty() {
            return;
        }
        let repo = PathBuf::from(&self.repo_path);
        cx.spawn_in(window, async move |this, cx| {
            let checked = paths.clone();
            let marked: Vec<String> = cx
                .background_executor()
                .spawn(async move {
                    checked
                        .into_iter()
                        .filter(|p| {
                            mt_project::git::has_conflict_markers(&repo, p).unwrap_or(false)
                        })
                        .collect()
                })
                .await;
            let _ = this.update_in(cx, |this: &mut Self, window, cx| {
                let resolve = move |this: &mut Self, cx: &mut Context<Self>| {
                    this.run_op(
                        move |repo| {
                            for path in &paths {
                                mt_project::git::mark_conflict_resolved(repo, path)?;
                            }
                            Ok(())
                        },
                        None,
                        cx,
                    );
                };
                if marked.is_empty() {
                    resolve(this, cx);
                    return;
                }
                let entity = cx.entity();
                let resolve = Rc::new(std::cell::RefCell::new(Some(resolve)));
                Confirm::new(
                    t("gitCommit", "markResolvedTitle"),
                    tr!(
                        "gitCommit",
                        "markResolvedConfirm",
                        count = marked.len().to_string()
                    ),
                )
                .detail(preview_paths(&marked))
                .ok_text(t("gitCommit", "markResolvedOk"))
                .open(
                    move |_window, cx| {
                        if let Some(resolve) = resolve.borrow_mut().take() {
                            entity.update(cx, resolve);
                        }
                    },
                    window,
                    cx,
                );
            });
        })
        .detach();
    }

    /// 打开某一行的 diff,文件序列 = 它所在分组的显示顺序(弹窗里 Alt+←/→ 前后换)。
    fn view_diff(&self, index: usize, area: Area, window: &mut Window, cx: &mut Context<Self>) {
        let order = self.area_order(area, self.tree_mode(cx));
        let side = if area == Area::Staged {
            git_diff::DiffSide::Staged
        } else {
            git_diff::DiffSide::Unstaged
        };
        let files: Vec<git_diff::DiffFile> = order
            .iter()
            .map(|&i| {
                let file = &self.changes[i];
                let status = status_in_area(file, area);
                // 只有「两侧都是普通文本、能写回」的状态才给块操作:未跟踪 / 冲突 /
                // 重命名 / 删除都只支持整文件操作
                let hunk_actions = match area {
                    Area::Staged => {
                        matches!(status, Some(GitStatus::Modified | GitStatus::Added))
                    }
                    Area::Unstaged => matches!(status, Some(GitStatus::Modified)),
                    Area::Untracked | Area::Conflicts => false,
                };
                git_diff::DiffFile {
                    path: file.path.clone(),
                    side,
                    status_label: status_label_for(status).to_string(),
                    hunk_actions,
                }
            })
            .collect();
        let current = order.iter().position(|&i| i == index).unwrap_or(0);
        let weak = cx.entity().downgrade();
        let on_changed: git_diff::OnChanged = Rc::new(move |cx: &mut App| {
            let _ = weak.update(cx, |this, cx| this.load(cx));
        });
        git_diff::open_file_diff(
            self.store.clone(),
            self.repo_path.clone(),
            files,
            current,
            Some(on_changed),
            window,
            cx,
        );
    }

    // ── 提交 ──

    fn staged_count(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| in_area(c, Area::Staged))
            .count()
    }

    fn conflict_count(&self) -> usize {
        self.changes.iter().filter(|c| is_conflicted(c)).count()
    }

    /// 提交按钮为什么点不了(`None` = 能点)。`stage_first` = 「全部暂存并提交」。
    fn commit_blocker(&self, stage_first: bool, cx: &App) -> Option<String> {
        if self.repo_path.is_empty() {
            return Some(t("gitCommit", "blockNoRepo").to_string());
        }
        if self.committing || self.pushing {
            return Some(t("gitCommit", "blockBusy").to_string());
        }
        let conflicts = self.conflict_count();
        if conflicts > 0 {
            return Some(tr!(
                "gitCommit",
                "blockConflicts",
                count = conflicts.to_string()
            ));
        }
        if self.commit_input.read(cx).value().trim().is_empty() {
            return Some(t("gitCommit", "blockNoMessage").to_string());
        }
        if self.amend {
            return (!self.has_head).then(|| t("gitCommit", "amendNoHead").to_string());
        }
        if stage_first {
            return self
                .group_indices(Area::Unstaged)
                .is_empty()
                .then(|| t("gitCommit", "blockNothingToStage").to_string());
        }
        if self.staged_count() == 0 {
            return Some(if self.group_indices(Area::Unstaged).is_empty() {
                t("gitCommit", "blockNothingStaged").to_string()
            } else {
                t("gitCommit", "blockNothingStagedHint").to_string()
            });
        }
        None
    }

    /// 提交。`push` = 提交成功后接着推送;`stage_first` = 先暂存全部已跟踪改动
    /// (未暂存组,不含未跟踪文件)。
    fn commit(
        &mut self,
        push: bool,
        stage_first: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.commit_blocker(stage_first, cx).is_some() {
            return;
        }
        let message = self.commit_input.read(cx).value().trim().to_string();
        let amend = self.amend;
        let stage_paths = if stage_first {
            self.area_paths(Area::Unstaged)
        } else {
            Vec::new()
        };
        self.committing = true;
        self.commit_error = None;
        cx.notify();
        let repo = PathBuf::from(&self.repo_path);
        let input = self.commit_input.clone();
        cx.spawn_in(window, async move |this, cx| {
            // git_commit 走 git CLI(60s 超时兜底 GPG/pre-commit hook 挂起)——
            // 即便如此也不上主线程跑,hook 慢的仓库会把 UI 卡满整个超时窗口
            let result = cx
                .background_executor()
                .spawn(async move {
                    if !stage_paths.is_empty() {
                        mt_project::git::git_stage(&repo, &stage_paths)?;
                    }
                    mt_project::git::git_commit_with(&repo, &message, amend)
                })
                .await;
            let _ = this.update_in(cx, |this: &mut Self, window, cx| {
                this.committing = false;
                match result {
                    Ok(outcome) => {
                        input.update(cx, |state, cx| state.set_value("", window, cx));
                        this.amend = false;
                        this.amend_info = None;
                        this.amend_prefill = None;
                        this.notify_info(
                            tr!(
                                "gitCommit",
                                "committedToast",
                                hash = outcome.short_hash.clone(),
                                title = outcome.summary.clone()
                            ),
                            cx,
                        );
                        this.load(cx);
                        cx.emit(GitChangesEvent::Committed);
                        if push {
                            this.push_after_commit(cx);
                        }
                    }
                    Err(err) => {
                        // 提交消息留在框里不清,改完(或修好 hook)直接再点
                        eprintln!("[git] 提交失败: {err:#}");
                        this.commit_error = Some(tr!(
                            "gitChanges",
                            "commitFailed",
                            detail = format!("{err:#}")
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 提交成功后的推送。失败时提交已经落地,要明确说「已提交,但推送失败」。
    ///
    /// 走与推送对话框同一条 `git_push_with`:新分支没有上游时顺带 `-u`(裸 `git push`
    /// 在这种情况下直接失败),被拒时给出「先拉取」的说明。
    fn push_after_commit(&mut self, cx: &mut Context<Self>) {
        use mt_project::git::{PushOptions, PushOutcome};

        self.pushing = true;
        let repo = PathBuf::from(&self.repo_path);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    mt_project::git::git_push_with(&repo, &PushOptions::default())
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.pushing = false;
                let failure = match result {
                    Ok(PushOutcome::Pushed(target)) => {
                        let target = format!("{}/{}", target.remote, target.remote_branch);
                        this.notify_info(tr!("gitCommit", "pushedToast", target = target), cx);
                        // 远程跟踪分支前移了:分支列表 / 历史的远程标记要跟上
                        cx.emit(GitChangesEvent::Committed);
                        None
                    }
                    Ok(PushOutcome::Rejected { detail }) => Some(format!(
                        "{}\n{}",
                        t("gitBranch", "rejectedHint"),
                        detail.trim()
                    )),
                    Err(err) => Some(format!("{err:#}")),
                };
                if let Some(detail) = failure {
                    eprintln!("[git] 提交后推送失败: {detail}");
                    this.commit_error =
                        Some(tr!("gitCommit", "pushFailedAfterCommit", detail = detail));
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn on_commit_action(
        &mut self,
        _: &GitCommitMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit(false, false, window, cx);
    }

    fn on_commit_push_action(
        &mut self,
        _: &GitCommitAndPush,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit(true, false, window, cx);
    }

    /// 勾 / 取消「修改上次提交」。勾上时取 HEAD 的消息(输入框为空就预填)与「已推送」判定。
    /// 历史区「撤销提交」后回填它的消息(IDEA 的 Undo Commit 口径)。用户已经在框里
    /// 写了东西就不覆盖;Amend 勾着时先收起 —— 被撤销的那条已经不是 HEAD 了。
    pub fn restore_message(&mut self, message: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.amend {
            self.toggle_amend(window, cx);
        }
        if !self.commit_input.read(cx).value().trim().is_empty() {
            return;
        }
        let message = message.trim_end().to_string();
        self.commit_input
            .update(cx, |state, cx| state.set_value(message, window, cx));
        cx.notify();
    }

    fn toggle_amend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_head || self.committing {
            return;
        }
        self.amend = !self.amend;
        self.amend_request += 1;
        if !self.amend {
            // 预填的消息一个字没动过 —— 那是 HEAD 的旧消息,不是用户写的,收起时带走
            let current = self.commit_input.read(cx).value().to_string();
            if self.amend_prefill.take().is_some_and(|p| p == current) {
                self.commit_input
                    .update(cx, |state, cx| state.set_value("", window, cx));
            }
            self.amend_info = None;
            cx.notify();
            return;
        }
        let req = self.amend_request;
        let repo = PathBuf::from(&self.repo_path);
        let input = self.commit_input.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { mt_project::git::amend_info(&repo) })
                .await;
            let _ = this.update_in(cx, |this: &mut Self, window, cx| {
                if this.amend_request != req || !this.amend {
                    return;
                }
                match result {
                    Ok(Some(info)) => {
                        if input.read(cx).value().trim().is_empty() {
                            let message = info.message.trim_end().to_string();
                            input.update(cx, |state, cx| {
                                state.set_value(message.clone(), window, cx)
                            });
                            this.amend_prefill = Some(message);
                        }
                        this.amend_info = Some(info);
                    }
                    // 空仓库:没有上次提交可改
                    Ok(None) => {
                        this.amend = false;
                        this.has_head = false;
                    }
                    Err(err) => {
                        eprintln!("[git] 读取上次提交失败: {err:#}");
                        this.amend = false;
                        this.commit_error = Some(tr!(
                            "gitCommit",
                            "amendInfoFailed",
                            detail = format!("{err:#}")
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// 提交消息历史:后台取最近 20 条,取回来在按钮处弹菜单,选中把整段消息填进输入框。
    fn show_history(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.history_loading || self.repo_path.is_empty() {
            return;
        }
        self.history_loading = true;
        cx.notify();
        let repo = PathBuf::from(&self.repo_path);
        let input = self.commit_input.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { mt_project::git::recent_commit_messages(&repo, HISTORY_LIMIT) })
                .await;
            let _ = this.update_in(cx, |this: &mut Self, window, cx| {
                this.history_loading = false;
                cx.notify();
                let list = match result {
                    Ok(list) => list,
                    Err(err) => {
                        eprintln!("[git] 读取提交消息历史失败: {err:#}");
                        this.notify_op_failure(format!("{err:#}"), cx);
                        return;
                    }
                };
                let entries = history_entries(list, &input, cx.entity());
                menu::show(position, entries, window, cx);
            });
        })
        .detach();
    }

    /// 占位符跟着目标分支走(「提交到 main(Ctrl+Enter)」)。只在文本变了时才改 ——
    /// 切语言 / 换分支都会让文本变,每帧比一次字符串即可。
    fn sync_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shortcut = menu::hotkey_label(true, false, false, "Enter");
        let text = match &self.target {
            Some(CommitTarget::Branch(name)) => tr!(
                "gitCommit",
                "placeholderBranch",
                branch = name.clone(),
                shortcut = shortcut
            ),
            Some(CommitTarget::Detached(hash)) => tr!(
                "gitCommit",
                "placeholderDetached",
                hash = hash.clone(),
                shortcut = shortcut
            ),
            None => t("panels", "commitPlaceholder").to_string(),
        };
        if self.placeholder.as_deref() == Some(text.as_str()) {
            return;
        }
        self.placeholder = Some(text.clone());
        self.commit_input
            .update(cx, |state, cx| state.set_placeholder(text, window, cx));
    }
}

/// 一次取数:变更列表 + 冲突细节 + 提交区要的 HEAD 信息。全部是只读的 git2 查询。
fn load_snapshot(repo: &Path, want_amend: bool) -> anyhow::Result<Snapshot> {
    let changes = mt_project::git::get_changes_status(repo)?;
    let conflicts = if changes.iter().any(is_conflicted) {
        mt_project::git::list_conflicts(repo).unwrap_or_default()
    } else {
        Vec::new()
    };
    Ok(Snapshot {
        changes,
        conflicts,
        target: mt_project::git::commit_target(repo).ok(),
        has_head: mt_project::git::head_exists(repo),
        amend: want_amend.then(|| mt_project::git::amend_info(repo).ok().flatten()),
    })
}

/// 确认框里列出的文件(最多 5 个,多出的写「…等 N 个」)。
fn preview_paths(paths: &[String]) -> Vec<String> {
    const LIMIT: usize = 5;
    let mut lines: Vec<String> = paths.iter().take(LIMIT).map(|p| format!("• {p}")).collect();
    if paths.len() > LIMIT {
        lines.push(tr!(
            "gitCommit",
            "andMore",
            count = (paths.len() - LIMIT).to_string()
        ));
    }
    lines
}

/// 提交消息历史的菜单项。标题截到 60 个字符,detail 放日期。
fn history_entries(
    list: Vec<CommitMessageEntry>,
    input: &Entity<TextareaState>,
    owner: Entity<GitChanges>,
) -> Vec<menu::MenuEntry> {
    if list.is_empty() {
        return vec![
            MenuItem::new(t("gitCommit", "historyEmpty"))
                .disabled(true)
                .into(),
        ];
    }
    list.into_iter()
        .map(|entry| {
            let label: String = if entry.summary.chars().count() > 60 {
                entry.summary.chars().take(60).chain(['…']).collect()
            } else {
                entry.summary.clone()
            };
            let (input, owner) = (input.clone(), owner.clone());
            let message = entry.message;
            MenuItem::new(label)
                .detail(format_date(entry.timestamp))
                .on_click(move |window, cx| {
                    let message = message.clone();
                    input.update(cx, |state, cx| {
                        state.set_value(message, window, cx);
                        state.focus(window, cx);
                    });
                    // set_value 不发 Change 事件,得自己叫一声重画(字数 / 提示 / 按钮)
                    owner.update(cx, |_, cx| cx.notify());
                })
                .into()
        })
        .collect()
}

fn format_date(timestamp: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(timestamp, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => timestamp.to_string(),
    }
}

// ─── 渲染 ─────────────────────────────────────────────────────

/// 文件行 / 目录行的行高(与分支弹窗、提交详情的文件列表同一口径)。
const ROW_HEIGHT: f32 = git_ui::ROW_H;
/// 行右端「状态字母 / hover 时的操作按钮」那一格的宽:两颗图标按钮 + 间隙。
/// 每行都留同样宽,状态字母竖着对齐。
const ROW_ACTIONS_W: f32 = git_ui::ICON_BUTTON * 2.0 + 2.0;
/// 文件行的组名(行内按钮 hover 才出现)。
const ROW_GROUP: &str = "git-file-row";

impl Render for GitChanges {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_placeholder(window, cx);
        let tree_mode = self.tree_mode(cx);
        let list = self.render_list(tree_mode, cx);
        let commit_area = self.render_commit_area(cx);

        div()
            .size_full()
            .flex()
            .flex_col()
            // Ctrl/Cmd+Enter 提交、Ctrl/Cmd+Alt+Enter 提交并推送(键位绑在 hotkeys.rs,
            // 谓词 `"GitChanges > Input"` —— 与项目切换器的方向键同一套路)
            .key_context("GitChanges")
            .on_action(cx.listener(Self::on_commit_action))
            .on_action(cx.listener(Self::on_commit_push_action))
            .child(list)
            .child(commit_area)
    }
}

impl GitChanges {
    fn render_list(&mut self, tree_mode: bool, cx: &mut Context<Self>) -> AnyElement {
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut keys: Vec<RowKey> = Vec::new();
        let empty = self.changes.is_empty();

        if self.loading && empty && self.load_error.is_none() {
            rows.push(git_ui::loading_state(t("gitChanges", "loading")).into_any_element());
            keys.push(RowKey::Other);
        } else if let Some(err) = self.load_error.as_deref().filter(|_| empty) {
            // 取变更失败顶替空态:否则读失败会被误认成「工作区是干净的」
            rows.push(
                git_ui::error_state(tr!("gitChanges", "loadFailed", detail = err))
                    .into_any_element(),
            );
            keys.push(RowKey::Other);
        } else if empty {
            rows.push(self.render_clean_state(cx));
            keys.push(RowKey::Other);
        } else {
            for area in Area::ORDER {
                let indices = self.group_indices(area);
                if indices.is_empty() {
                    continue;
                }
                rows.push(self.render_group_header(area, indices.len(), cx));
                keys.push(RowKey::Other);
                if self.collapsed_groups.contains(&area) {
                    continue;
                }
                if tree_mode {
                    let paths: Vec<(usize, &str)> = indices
                        .iter()
                        .map(|&i| (i, self.changes[i].path.as_str()))
                        .collect();
                    let mut tree_rows = Vec::new();
                    flatten_tree(
                        &build_file_tree(&paths),
                        area,
                        &self.collapsed_dirs,
                        0,
                        &mut tree_rows,
                    );
                    for row in tree_rows {
                        match row {
                            TreeRow::Dir {
                                name,
                                full_path,
                                depth,
                                collapsed,
                            } => {
                                rows.push(
                                    self.render_dir_row(
                                        area, name, full_path, depth, collapsed, cx,
                                    ),
                                );
                                keys.push(RowKey::Other);
                            }
                            TreeRow::File { index, depth } => {
                                rows.push(self.render_file_row(index, area, depth, true, cx));
                                keys.push(RowKey::File(area, index));
                            }
                        }
                    }
                } else {
                    for index in indices {
                        rows.push(self.render_file_row(index, area, 0, false, cx));
                        keys.push(RowKey::File(area, index));
                    }
                }
            }
        }
        self.row_keys = keys;

        div()
            .id("git-changes-list")
            .track_focus(&self.list_focus)
            .key_context(LIST_CONTEXT)
            .on_action(cx.listener(Self::on_select_prev))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_toggle_stage))
            .on_action(cx.listener(Self::on_open_diff))
            .on_action(cx.listener(Self::on_discard_key))
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .px(px(4.0))
            .pt(px(4.0))
            .pb(px(8.0))
            .children(rows)
            .into_any_element()
    }

    /// 工作区干净:32px 勾形图标 + 一句说明 + 「刷新」。
    fn render_clean_state(&self, cx: &mut Context<Self>) -> AnyElement {
        git_ui::empty_state(
            ICON_CLEAN,
            t("gitChanges", "empty"),
            Some(
                git_ui::action_button(
                    "git-changes-refresh",
                    t("gitChanges", "refresh"),
                    ButtonKind::Ghost,
                    true,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.load(cx)))
                .into_any_element(),
            ),
        )
        .py(px(32.0))
        .into_any_element()
    }

    /// 分组标题:折叠箭头 + 标题 + 数量胶囊 + 右对齐的组操作(常显,保证可发现)。
    fn render_group_header(&self, area: Area, count: usize, cx: &mut Context<Self>) -> AnyElement {
        let collapsed = self.collapsed_groups.contains(&area);
        let (title, tone) = match area {
            Area::Conflicts => (t("gitCommit", "conflictsGroup"), ui::color_attention()),
            Area::Staged => (t("panels", "stagedChanges"), ui::text_muted()),
            Area::Unstaged => (t("panels", "unstagedChanges"), ui::text_muted()),
            Area::Untracked => (t("panels", "untrackedFiles"), ui::text_muted()),
        };
        let enabled = self.busy_ops == 0;
        let key = area.key();
        let mut header = SectionHeader::new(SharedString::from(format!("git-group-{key}")), title)
            .group()
            .collapsible(!collapsed)
            .count(count);
        if area == Area::Conflicts {
            header = header.tone(tone).icon(ICON_ALERT);
        }
        // 组操作常显(保证可发现);共用图标按钮自己拦住冒泡,点它们不会顺带折叠
        match area {
            Area::Conflicts => {}
            Area::Staged => {
                header = header.trailing(
                    icon_button(
                        SharedString::from(format!("git-group-{key}-unstage")),
                        ICON_MINUS,
                        t("gitCommit", "unstageAll"),
                        enabled,
                    )
                    .when(enabled, |el| {
                        el.on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.unstage_all(cx);
                        }))
                    }),
                );
            }
            Area::Unstaged | Area::Untracked => {
                if area == Area::Unstaged {
                    header = header.trailing(
                        danger_icon_button(
                            SharedString::from(format!("git-group-{key}-discard")),
                            ICON_UNDO,
                            t("gitCommit", "discardAll"),
                            enabled,
                        )
                        .when(enabled, |el| {
                            el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                let paths = this.area_paths(area);
                                this.discard(area, paths, window, cx);
                            }))
                        }),
                    );
                }
                header = header.trailing(
                    icon_button(
                        SharedString::from(format!("git-group-{key}-stage")),
                        ICON_PLUS,
                        t("gitCommit", "stageAll"),
                        enabled,
                    )
                    .when(enabled, |el| {
                        el.on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.stage_group(area, cx);
                        }))
                    }),
                );
            }
        }

        header
            .build()
            .mt(px(4.0))
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                if !this.collapsed_groups.remove(&area) {
                    this.collapsed_groups.insert(area);
                }
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_dir_row(
        &self,
        area: Area,
        name: String,
        full_path: String,
        depth: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = format!("{}:{}", area.key(), full_path);
        div()
            .id(SharedString::from(format!("git-dir-{key}")))
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(ROW_HEIGHT))
            .pr(px(8.0))
            .pl(px(depth as f32 * 16.0 + 8.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .text_size(ui::font_px(13.0))
            .text_color(ui::text_secondary())
            .hover(|el| el.bg(git_ui::hover_bg()))
            .child(
                div()
                    .flex_none()
                    .w(px(git_ui::ICON_SIZE))
                    .flex()
                    .justify_center()
                    .child(git_ui::icon(
                        if collapsed {
                            ICON_CHEVRON_RIGHT
                        } else {
                            ICON_CHEVRON_DOWN
                        },
                        git_ui::ICON_SIZE_SM,
                        ui::text_muted(),
                    )),
            )
            .child(FileIcon::folder(!collapsed).size(px(git_ui::ICON_SIZE)))
            .child(div().min_w_0().truncate().child(name))
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                if !this.collapsed_dirs.remove(&key) {
                    this.collapsed_dirs.insert(key.clone());
                }
                cx.notify();
            }))
            .into_any_element()
    }

    /// 文件行:文件图标 + 文件名(状态色,删除加删除线)+ 弱化目录 … 右端状态字母;
    /// hover 时字母让位给操作按钮。选中 = accent 淡底 + 左侧 2px accent 竖条
    /// (与 hover 底区分开)。
    fn render_file_row(
        &self,
        index: usize,
        area: Area,
        depth: usize,
        tree_mode: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(file) = self.changes.get(index) else {
            return div().into_any_element();
        };
        let status = status_in_area(file, area);
        let label = status_label_for(status);
        let color = status_color(status);
        let path = file.path.clone();
        let deleted = matches!(status, Some(GitStatus::Deleted));
        let selected = self.is_selected(area, &path);
        let enabled = self.busy_ops == 0;

        // tooltip:完整路径(重命名带旧路径)+ 状态说明(部分暂存另注一句)
        let shown_path = match &file.old_path {
            Some(old) if matches!(status, Some(GitStatus::Renamed)) => format!("{old} → {path}"),
            _ => path.clone(),
        };
        let mut describe = status_text(status).to_string();
        if file.staged_status.is_some() && file.unstaged_status.is_some() && !is_conflicted(file) {
            describe.push_str(" · ");
            describe.push_str(t("gitCommit", "partiallyStaged"));
        }
        let tip = format!("{shown_path}\n{describe}");

        let row_id = SharedString::from(format!("git-file-{}-{}", area.key(), path));
        let id_of =
            |what: &str| SharedString::from(format!("git-file-{what}-{}-{path}", area.key()));

        // 行内按钮(作用于选中集,见 `targets`)
        let mut actions = div()
            .absolute()
            .top_0()
            .right_0()
            .h_full()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(2.0))
            .invisible()
            .group_hover(ROW_GROUP, |s| s.visible());
        if matches!(area, Area::Unstaged | Area::Untracked) {
            actions = actions.child(
                danger_icon_button(
                    id_of("discard"),
                    ICON_UNDO,
                    if area == Area::Untracked {
                        t("gitCommit", "deleteUntracked")
                    } else {
                        t("gitChanges", "contextDiscard")
                    },
                    enabled,
                )
                .when(enabled, |el| {
                    el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        let paths = this.targets(area, index, cx);
                        this.discard(area, paths, window, cx);
                    }))
                }),
            );
        }
        match area {
            Area::Staged => {
                actions = actions.child(
                    icon_button(
                        id_of("unstage"),
                        ICON_MINUS,
                        t("panels", "unstage"),
                        enabled,
                    )
                    .when(enabled, |el| {
                        el.on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            let paths = this.targets(area, index, cx);
                            this.unstage(paths, None, cx);
                        }))
                    }),
                );
            }
            Area::Unstaged | Area::Untracked => {
                let stage = icon_button(id_of("stage"), ICON_PLUS, t("panels", "stage"), enabled);
                actions = actions.child(stage.when(enabled, |el| {
                    el.on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        let paths = this.targets(area, index, cx);
                        this.stage(paths, None, cx);
                    }))
                }));
            }
            Area::Conflicts => {}
        }

        git_ui::list_row(row_id, selected)
            .group(ROW_GROUP)
            .pl(px(depth as f32 * 16.0 + 8.0))
            .child(git_ui::file_label(&path, color, deleted, !tree_mode))
            .child(
                div()
                    .relative()
                    .flex_none()
                    .w(px(ROW_ACTIONS_W))
                    .h(px(git_ui::ICON_BUTTON))
                    .child(
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_end()
                            .pr(px(4.0))
                            .group_hover(ROW_GROUP, |s| s.invisible())
                            .child(git_ui::status_letter(label, color)),
                    )
                    .child(actions),
            )
            .tip(tip)
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.click_row(area, index, event, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    // 右键没落在选中集上:先把选择换成这一行(与资源管理器同手感)
                    let path = this
                        .changes
                        .get(index)
                        .map(|c| c.path.clone())
                        .unwrap_or_default();
                    if !this.is_selected(area, &path) {
                        this.select_single(area, index);
                        cx.notify();
                    }
                    window.focus(&this.list_focus, cx);
                    let entries = this.file_menu(index, area, cx);
                    menu::show(event.position, entries, window, cx);
                }),
            )
            .into_any_element()
    }

    /// 文件行右键菜单(`GitChanges.tsx:242-256`),多选时作用于整组。
    fn file_menu(&self, index: usize, area: Area, cx: &mut Context<Self>) -> Vec<menu::MenuEntry> {
        let this = cx.entity();
        let paths = self.targets(area, index, cx);
        let count = paths.len();
        let many = count > 1;
        let enabled = self.busy_ops == 0;
        let counted = |single: &'static str, multi: &'static str| -> String {
            if many {
                tr!("gitCommit", multi, count = count.to_string())
            } else {
                single.to_string()
            }
        };
        let view_diff = {
            let this = this.clone();
            menu::item(t("gitChanges", "contextViewDiff"), move |window, cx| {
                this.update(cx, |this, cx| this.view_diff(index, area, window, cx));
            })
        };

        let mut entries = Vec::new();
        match area {
            Area::Conflicts => {
                let info = (!many)
                    .then(|| self.changes.get(index))
                    .flatten()
                    .and_then(|file| self.conflicts.iter().find(|c| c.path == file.path));
                for side in [ConflictSide::Ours, ConflictSide::Theirs] {
                    let deleted = info.is_some_and(|c| match side {
                        ConflictSide::Ours => !c.ours,
                        ConflictSide::Theirs => !c.theirs,
                    });
                    let label = match (side, deleted) {
                        (ConflictSide::Ours, false) => t("gitCommit", "takeOurs"),
                        (ConflictSide::Ours, true) => t("gitCommit", "takeOursDeleted"),
                        (ConflictSide::Theirs, false) => t("gitCommit", "takeTheirs"),
                        (ConflictSide::Theirs, true) => t("gitCommit", "takeTheirsDeleted"),
                    };
                    let (this, paths) = (this.clone(), paths.clone());
                    let mut item =
                        MenuItem::new(label)
                            .disabled(!enabled)
                            .on_click(move |_window, cx| {
                                let paths = paths.clone();
                                this.update(cx, |this, cx| this.take_side(paths, side, cx));
                            });
                    if deleted {
                        item = item.danger();
                    }
                    entries.push(item.into());
                }
                let (this2, paths2) = (this.clone(), paths.clone());
                entries.push(
                    MenuItem::new(t("gitCommit", "markResolved"))
                        .disabled(!enabled)
                        .on_click(move |window, cx| {
                            let paths = paths2.clone();
                            this2.update(cx, |this, cx| this.mark_resolved(paths, window, cx));
                        })
                        .into(),
                );
                entries.push(menu::separator());
                entries.push(view_diff);
            }
            Area::Staged => {
                entries.push(view_diff);
                entries.push(menu::separator());
                let (this, paths) = (this.clone(), paths.clone());
                entries.push(
                    MenuItem::new(counted(t("panels", "unstage"), "unstageSelected"))
                        .disabled(!enabled)
                        .on_click(move |_window, cx| {
                            let paths = paths.clone();
                            this.update(cx, |this, cx| this.unstage(paths, None, cx));
                        })
                        .into(),
                );
            }
            Area::Unstaged | Area::Untracked => {
                entries.push(view_diff);
                entries.push(menu::separator());
                {
                    let (this, paths) = (this.clone(), paths.clone());
                    entries.push(
                        MenuItem::new(counted(t("panels", "stage"), "stageSelected"))
                            .disabled(!enabled)
                            .on_click(move |_window, cx| {
                                let paths = paths.clone();
                                this.update(cx, |this, cx| this.stage(paths, None, cx));
                            })
                            .into(),
                    );
                }
                entries.push(menu::separator());
                let discard_label = if area == Area::Untracked {
                    counted(t("gitCommit", "deleteUntracked"), "deleteSelected")
                } else {
                    counted(t("gitChanges", "contextDiscard"), "discardSelected")
                };
                entries.push(
                    MenuItem::new(discard_label)
                        .danger()
                        .disabled(!enabled)
                        .shortcut("Delete")
                        .on_click(move |window, cx| {
                            let paths = paths.clone();
                            this.update(cx, |this, cx| this.discard(area, paths, window, cx));
                        })
                        .into(),
                );
            }
        }
        // 单文件历史(IDEA Show History)。未跟踪文件还没有历史,多选时没有「那一个文件」
        if !many
            && area != Area::Untracked
            && let Some(file) = self.changes.get(index)
        {
            let (store, repo, rel) = (
                self.store.clone(),
                PathBuf::from(&self.repo_path),
                file.path.clone(),
            );
            entries.push(menu::separator());
            entries.push(menu::item(
                t("gitLog", "fileHistory.menu"),
                move |window, cx| {
                    crate::git_file_history::open(
                        store.clone(),
                        repo.clone(),
                        rel.clone(),
                        window,
                        cx,
                    );
                },
            ));
        }
        entries
    }

    /// 提交区:输入框 + 工具条(Amend / 消息历史 / 标题字数)+ 软提示 + 拆分式提交按钮
    /// + 就地错误。
    fn render_commit_area(&self, cx: &mut Context<Self>) -> AnyElement {
        let message = self.commit_input.read(cx).value().to_string();
        let hints = message_hints(&message);
        let blocker = self.commit_blocker(false, cx);
        let can_commit = blocker.is_none();
        let stage_first_ok = self.commit_blocker(true, cx).is_none();
        let staged_count = self.staged_count();

        let commit_label = if self.committing {
            t("gitChanges", "committing").to_string()
        } else if self.pushing {
            t("gitCommit", "pushing").to_string()
        } else if self.amend {
            t("gitCommit", "amendCommit").to_string()
        } else {
            tr!("panels", "commit", count = staged_count.to_string())
        };

        // ── 工具条 ──
        let amend_enabled = self.has_head && !self.committing;
        let amend_toggle = git_ui::check_row(
            "git-commit-amend",
            self.amend,
            t("gitCommit", "amend"),
            false,
        )
        .when(amend_enabled, |el| {
            el.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.toggle_amend(window, cx);
            }))
        })
        .when(!amend_enabled, |el| el.opacity(0.5).cursor_default())
        .tip(if self.has_head {
            t("gitCommit", "amendTip")
        } else {
            t("gitCommit", "amendNoHead")
        });
        let history_enabled = !self.history_loading && !self.repo_path.is_empty();
        let history = icon_button(
            "git-commit-history",
            ICON_HISTORY,
            t("gitCommit", "messageHistory"),
            history_enabled,
        )
        .when(history_enabled, |el| {
            el.on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                this.show_history(event.position(), window, cx);
            }))
        });
        let counter = (hints.title_chars >= TITLE_COUNT_FROM).then(|| {
            mono(div())
                .text_size(ui::font_px(11.0))
                .text_color(if hints.title_too_long {
                    ui::color_error()
                } else {
                    ui::color_warning()
                })
                .child(format!("{}/{TITLE_SOFT_LIMIT}", hints.title_chars))
        });
        let toolbar = div()
            .mt(px(8.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(amend_toggle)
            .child(history)
            .child(div().flex_1())
            .children(counter);

        // ── 警告与软提示 ──
        let pushed_warning = (self.amend && self.amend_info.as_ref().is_some_and(|i| i.pushed))
            .then(|| {
                git_ui::notice_line(t("gitCommit", "amendPushedWarning"), ui::color_warning())
                    .mt(px(6.0))
            });
        let mut soft_hints: Vec<String> = Vec::new();
        if hints.title_too_long {
            soft_hints.push(tr!(
                "gitCommit",
                "hintTitleTooLong",
                count = hints.title_chars.to_string()
            ));
        }
        if hints.missing_blank_line {
            soft_hints.push(t("gitCommit", "hintBlankLine").to_string());
        }

        // ── 提交按钮(主按钮 + ▾) ──
        let main_button = div()
            .id("git-commit-button")
            .flex_1()
            .h(px(30.0))
            .flex()
            .items_center()
            .justify_center()
            .text_size(ui::font_px(13.0))
            .when(can_commit, |el| {
                el.bg(ui::accent())
                    .text_color(ui::bg_base())
                    .cursor_pointer()
                    .hover(|el| el.opacity(0.9))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.commit(false, false, window, cx)
                    }))
            })
            .when(!can_commit, |el| {
                el.bg(ui::bg_elevated()).text_color(ui::text_muted())
            })
            .when_some(blocker.clone(), |el, reason| el.tip(reason))
            .child(commit_label);
        let menu_enabled = !self.committing && !self.pushing;
        let split = div()
            .id("git-commit-more")
            .flex_none()
            .w(px(28.0))
            .h(px(30.0))
            .flex()
            .items_center()
            .justify_center()
            .border_l_1()
            .border_color(ui::with_alpha(ui::bg_base(), 0.35))
            .when(can_commit, |el| el.bg(ui::accent()))
            .when(!can_commit, |el| el.bg(ui::bg_elevated()))
            .child(git_ui::icon(
                ICON_CHEVRON_DOWN,
                git_ui::ICON_SIZE,
                if can_commit {
                    ui::bg_base()
                } else {
                    ui::text_muted()
                },
            ))
            .tip(t("gitCommit", "moreCommitActions"))
            .when(menu_enabled, |el| {
                el.cursor_pointer()
                    .hover(|el| el.opacity(0.9))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        let entries = this.commit_menu(can_commit, stage_first_ok, cx);
                        menu::show(event.position(), entries, window, cx);
                    }))
            });
        let buttons = div()
            .mt(px(8.0))
            .flex()
            .rounded(px(4.0))
            .overflow_hidden()
            .child(main_button)
            .child(split);

        // 错误原文:与弹窗里的 git 失败原文同一套红框淡底,右侧一颗「复制」
        let error = self.commit_error.clone().map(|err| {
            let copy = err.clone();
            div()
                .mt(px(8.0))
                .flex()
                .items_start()
                .gap(px(4.0))
                .pl(px(10.0))
                .pr(px(2.0))
                .py(px(2.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(ui::with_alpha(ui::color_error(), 0.35))
                .bg(ui::with_alpha(ui::color_error(), 0.06))
                .child(
                    div()
                        .id("git-commit-error")
                        .flex_1()
                        .min_w_0()
                        .max_h(px(160.0))
                        .py(px(4.0))
                        .overflow_y_scroll()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::color_error())
                        .child(err),
                )
                .child(
                    icon_button(
                        "git-commit-error-copy",
                        ICON_COPY,
                        t("gitCommit", "copyError"),
                        true,
                    )
                    .on_click(move |_: &ClickEvent, _window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                    }),
                )
        });

        div()
            .flex_none()
            .border_t_1()
            .border_color(ui::border_subtle())
            .p(px(8.0))
            .child(Textarea::new(&self.commit_input))
            .child(toolbar)
            .children(pushed_warning)
            .children(
                soft_hints
                    .into_iter()
                    .map(|hint| git_ui::hint_line(hint).mt(px(4.0))),
            )
            .child(buttons)
            .children(error)
            .into_any_element()
    }

    /// ▾ 菜单:提交并推送 / 全部暂存并提交 /(分隔)贮藏当前改动…。
    fn commit_menu(
        &self,
        can_commit: bool,
        stage_first_ok: bool,
        cx: &mut Context<Self>,
    ) -> Vec<menu::MenuEntry> {
        let this = cx.entity();
        let push = {
            let this = this.clone();
            MenuItem::new(t("gitCommit", "commitAndPush"))
                .shortcut(menu::hotkey_label(true, false, true, "Enter"))
                .disabled(!can_commit)
                .on_click(move |window, cx| {
                    this.update(cx, |this, cx| this.commit(true, false, window, cx));
                })
        };
        let stage_all = {
            let this = this.clone();
            MenuItem::new(t("gitCommit", "stageAllAndCommit"))
                .detail(t("gitCommit", "stageAllAndCommitDetail"))
                .disabled(!stage_first_ok)
                .on_click(move |window, cx| {
                    this.update(cx, |this, cx| this.commit(false, true, window, cx));
                })
        };
        // 贮藏:有改动(含未跟踪)才可用;没有时禁用并写明原因
        let has_changes = !self.changes.is_empty();
        let mut stash = MenuItem::new(t("gitStash", "stashChanges"))
            .disabled(!has_changes)
            .on_click(move |window, cx| {
                this.update(cx, |this, cx| this.open_stash_save(window, cx));
            });
        if !has_changes {
            stash = stash.detail(t("gitStash", "save.nothing"));
        }
        vec![
            push.into(),
            stage_all.into(),
            menu::separator(),
            stash.into(),
        ]
    }

    /// 「贮藏当前改动…」:开新建贮藏弹窗,成功后让容器整面板刷新(见 [`GitChangesEvent::Stashed`])。
    fn open_stash_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.repo_path.is_empty() {
            return;
        }
        let this = cx.entity().downgrade();
        let on_changed: Rc<dyn Fn(&mut App)> = Rc::new(move |cx: &mut App| {
            let _ = this.update(cx, |_, cx| cx.emit(GitChangesEvent::Stashed));
        });
        crate::git_stash::open_save(
            self.store.clone(),
            self.repo_path.clone(),
            on_changed,
            window,
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(
        path: &str,
        staged: Option<GitStatus>,
        unstaged: Option<GitStatus>,
    ) -> ChangeFileStatus {
        ChangeFileStatus {
            path: path.to_string(),
            old_path: None,
            staged_status: staged,
            unstaged_status: unstaged,
            status_label: String::new(),
        }
    }

    fn group(changes: &[ChangeFileStatus], area: Area) -> Vec<&str> {
        changes
            .iter()
            .filter(|c| in_area(c, area))
            .map(|c| c.path.as_str())
            .collect()
    }

    /// 四分组口径:未跟踪不落进「未暂存」,而**同一个文件可以同时进 staged 与
    /// unstaged**(部分暂存);冲突文件只进冲突组。
    #[test]
    fn 分组口径含部分暂存与冲突() {
        let changes = [
            change("a.rs", Some(GitStatus::Added), None),
            // 部分暂存:两组都要出现
            change("b.rs", Some(GitStatus::Modified), Some(GitStatus::Modified)),
            change("c.rs", None, Some(GitStatus::Modified)),
            change("d.rs", None, Some(GitStatus::Untracked)),
            // git2 里冲突文件两侧都是 conflicted
            change(
                "e.rs",
                Some(GitStatus::Conflicted),
                Some(GitStatus::Conflicted),
            ),
        ];
        assert_eq!(group(&changes, Area::Conflicts), vec!["e.rs"]);
        assert_eq!(group(&changes, Area::Staged), vec!["a.rs", "b.rs"]);
        assert_eq!(group(&changes, Area::Unstaged), vec!["b.rs", "c.rs"]);
        assert_eq!(group(&changes, Area::Untracked), vec!["d.rs"]);
    }

    /// 取哪一个 status 由所在区决定 —— 部分暂存的文件在两组里显示的字母不同。
    #[test]
    fn 状态按区取值() {
        let file = change("b.rs", Some(GitStatus::Added), Some(GitStatus::Deleted));
        assert_eq!(status_label_for(status_in_area(&file, Area::Staged)), "A");
        assert_eq!(status_label_for(status_in_area(&file, Area::Unstaged)), "D");
        assert_eq!(
            status_label_for(status_in_area(&file, Area::Untracked)),
            "D",
            "untracked 区同样取 unstagedStatus"
        );
    }

    /// 提交消息的轻提示:标题按字符数(中文不按字节),开头空行不算标题,
    /// 正文前缺空行才提示。
    #[test]
    fn 提交消息轻提示() {
        assert_eq!(message_hints(""), MessageHints::default());
        let ok = message_hints("feat: 标题\n\n正文");
        assert!(!ok.title_too_long && !ok.missing_blank_line);
        assert_eq!(ok.title_chars, 8);

        let crowded = message_hints("标题\n紧跟正文");
        assert!(crowded.missing_blank_line);

        let long = "字".repeat(73);
        let hints = message_hints(&long);
        assert_eq!(hints.title_chars, 73);
        assert!(hints.title_too_long);
        assert!(
            !message_hints(&"字".repeat(72)).title_too_long,
            "72 正好不算超"
        );

        let leading = message_hints("\n\n标题\n");
        assert_eq!(leading.title_chars, 2);
        assert!(!leading.missing_blank_line, "只有标题没有正文");
        assert!(
            !message_hints("标题\n   \n").missing_blank_line,
            "空白行算空行"
        );
    }

    #[test]
    fn 确认框文件预览最多五个() {
        let paths: Vec<String> = (1..=7).map(|i| format!("f{i}")).collect();
        let lines = preview_paths(&paths);
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0], "• f1");
        assert!(lines[5].contains('2'), "{lines:?}");
        assert_eq!(preview_paths(&paths[..2]).len(), 2);
    }

    /// 建树:嵌套路径共用目录节点,**不做单链压缩**。
    #[test]
    fn 建树共用目录节点() {
        let paths = [(0, "src/a.rs"), (1, "src/b.rs"), (2, "README.md")];
        let tree = build_file_tree(&paths);
        assert_eq!(tree.len(), 2, "src 目录 + README.md");
        assert_eq!(tree[0].name, "src");
        assert_eq!(tree[0].full_path, "src");
        assert!(tree[0].file.is_none());
        assert_eq!(tree[0].children.len(), 2);
        assert_eq!(tree[0].children[0].full_path, "src/a.rs");
        assert_eq!(tree[0].children[0].file, Some(0));
        assert_eq!(tree[1].name, "README.md");
        assert_eq!(tree[1].file, Some(2));

        // 深嵌套不压缩:a/b/c/d.rs 是三层目录 + 一个文件
        let deep = build_file_tree(&[(0, "a/b/c/d.rs")]);
        assert_eq!(deep[0].name, "a");
        assert_eq!(deep[0].children[0].name, "b");
        assert_eq!(deep[0].children[0].children[0].name, "c");
        assert_eq!(deep[0].children[0].children[0].children[0].name, "d.rs");
    }

    /// 同名的目录与文件互不合并(`find` 带 `&& !n.file` 那道判定)。
    #[test]
    fn 同名目录与文件不合并() {
        let tree = build_file_tree(&[(0, "build"), (1, "build/out.js")]);
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].file, Some(0), "先来的是文件");
        assert!(tree[1].file.is_none(), "后来的目录另起一个节点");
        assert_eq!(tree[1].children[0].full_path, "build/out.js");
    }

    /// 折叠 key 带区名:同一路径在各个区各自独立折叠。
    #[test]
    fn 折叠_key_按区隔离() {
        let tree = build_file_tree(&[(0, "src/a.rs")]);
        let mut collapsed = HashSet::new();
        collapsed.insert("staged:src".to_string());

        let mut rows = Vec::new();
        flatten_tree(&tree, Area::Staged, &collapsed, 0, &mut rows);
        assert_eq!(rows.len(), 1, "staged 区的 src 折起来了,只剩目录行");

        let mut rows = Vec::new();
        flatten_tree(&tree, Area::Unstaged, &collapsed, 0, &mut rows);
        assert_eq!(rows.len(), 2, "unstaged 区不受影响");
        assert!(matches!(rows[1], TreeRow::File { depth: 1, .. }));
    }
}
