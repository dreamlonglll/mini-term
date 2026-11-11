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
//! - **提交失败**(hook 拒绝 / GPG 出错 / 60s 超时):错误原文**就地**挂在提交按钮下方,
//!   错误色、可滚动、带复制按钮 —— 提交的错误常是 hook 的多行输出,tooltip 装不下。
//!   下一次提交、提交成功或换仓库时清掉(hook 输出得留给人读);
//! - **暂存 / 取消暂存 / 丢弃 / 冲突处理失败**:弹可选中复制原文的详情框
//!   (`git_ui::show_git_error`,与面板上的 Fetch / 检出同一口径)—— toast 5s 就收,
//!   装不下「哪些文件被 hook / 过滤器挡住」这类多行原文;
//! - **取变更失败**:列表区的空态换成错误原文 —— 否则读失败会被误认成
//!   「工作区是干净的」。它随 pty 输出每 500ms 去抖重取,弹框会刷屏,所以就地。
//!
//! # 提交并推送
//!
//! 与 IDEA 的 Commit and Push 一致:提交成功后**打开推送对话框**(预选当前分支),在那里
//! 看清待推送的提交与推送目标再确认;提交失败就不打开。这里不直接推。
//!
//! # 防重入
//!
//! 暂存 / 丢弃 / 提交都写暂存区,经仓库的写操作闸(`git_ui::gate`)执行:面板在拉取、
//! 历史区在拣选时,行内按钮、组操作与提交按钮一并禁用并写明原因。
//!
//! # 阻塞调用
//!
//! 全部 git 调用(git2 的同步 IO 与带超时的 git CLI)一律丢 `cx.background_executor()`
//! (范式照 `file_tree.rs:138-156`);异步回包用请求令牌防迟到。
//!
//! # 取数调度
//!
//! 列表取数要的人多(工作区监听每 2s 一批、`.git` 监听、pty 嗅探、自己的写操作、diff 弹窗
//! 按块操作),大仓库一次 status 又要两三秒。[`GitChanges::load`] 因此是**单飞**的
//! ([`git_ui::flight`]):同一时刻只跑一份,期间再要只记脏位,跑完当场补一趟;在途那份照样
//! 落地,但「操作后落回的位置」与在等的 diff 弹窗留给补跑那份(在途那份可能还不含那次操作)。
//!
//! 勾着「修改上次提交」时的「已推送」判定另走一趟([`GitChanges::refresh_amend`]),不挡
//! 列表:它要逐个远程分支做祖先判定,大仓库可达十几秒。按引用签名缓存 —— HEAD 与远程跟踪
//! 分支都没动就不重取。
//!
//! 暂存 / 丢弃 / 冲突处理写完,在同一趟后台里**先**读一份签名([`RepoObservation`])再重取
//! 列表,经 [`GitChangesEvent::IndexWritten`] 交给面板记作「已知」:紧接着那次 `.git` 检查
//! 比出来没变,不再把列表、文件树重取一遍。
//!
//! # 列表只建看得见的行
//!
//! 根视图每帧都 render,没套 view 缓存的子视图跟着整个重跑 render + layout + paint ——
//! 任一终端输出的一拍、spinner 的一拍、一次悬停都算。此前整张列表每帧连同目录树一起
//! 重建,几千个改动时每一帧都是 0.1s 级。现在:
//!
//! - 列表走 `v_virtual_list`(变高:分组标题连外边距 32、文件 / 目录行 28),只给视口里
//!   那几行建元素;加载中 / 失败 / 空态这类整块占位仍是普通滚动容器;
//! - 分组 + 树形拍平后的行([`ListRow`])、每行高度、摊开总高、四组的显示顺序收在派生
//!   模型 [`ListModel`] 里,**只在数据落地、折叠切换、切视图模式之后**算一次;render、
//!   面板分高度、键盘导航、Shift 连选、diff 弹窗的文件序列都读这一份;
//! - 取数落地([`GitChanges::apply_snapshot`])先比一次,与眼下的数据一模一样就既不
//!   重算也不重画 —— 监听与 pty 嗅探触发的重取,绝大多数回包都没变。

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, AppContext as _, Bounds, ClickEvent, ClipboardItem, Context, Entity,
    EventEmitter, FocusHandle, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Pixels, Point, Render, SharedString, Size, StatefulInteractiveElement, Styled,
    Subscription, Window, canvas, div, point, prelude::FluentBuilder as _, px, size,
};
use gpui_component::input::{InputEvent, Position, Textarea, TextareaState};
use gpui_component::{VirtualListScrollHandle, v_virtual_list};
use mt_project::git::{
    AmendInfo, ChangeFileStatus, CommitMessageEntry, CommitTarget, ConflictInfo, ConflictSide,
    GitStatus, RepoOpKind,
};
use mt_remote::GitContext;
use mt_ui::tooltip::{Tooltip, TooltipExt as _};

use crate::git_dir_watch::RepoObservation;
use crate::git_ui::flight::{Flight, Landing};
use crate::git_ui::{
    self, ButtonKind, ChangeTreeRow, ICON_ALERT, ICON_CHEVRON_DOWN, ICON_CLEAN, ICON_COPY,
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

/// 「更改」区整体的 key context(提交框里的 Ctrl/Cmd+Enter 等,键位见 `hotkeys.rs`)。
pub(crate) const CHANGES_CONTEXT: &str = "GitChanges";

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
    /// 提交成功 —— 容器要刷新历史与分支(分支头已前移)。`open_push` = 这是「提交并推送」,
    /// 接着打开推送对话框(预选当前分支,见模块注释「提交并推送」)。
    Committed { open_push: bool },
    /// ▾ 菜单「贮藏当前改动…」贮藏成功 —— 工作区与贮藏数都变了,容器整面板刷新。
    Stashed,
    /// 暂存 / 取消暂存 / 丢弃 / 冲突处理写完了(成败都算):写完当场、重取列表**之前**读的
    /// 那份签名与冲突数。容器拿它当「已知」,紧接着那次 `.git` 检查就不再重刷一遍
    /// (见模块注释「取数调度」)。
    IndexWritten(RepoObservation),
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

    /// 在 [`Self::ORDER`] 里的位置(派生模型按它摆四组的数组)。
    fn slot(self) -> usize {
        match self {
            Area::Conflicts => 0,
            Area::Staged => 1,
            Area::Unstaged => 2,
            Area::Untracked => 3,
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

/// 文件行的 tooltip:完整路径(重命名写「旧路径 → 新路径」)+ 状态说明(`partial` =
/// 部分暂存,另注一句)。
fn file_row_tip(
    path: &str,
    renamed_from: Option<&str>,
    status: Option<&GitStatus>,
    partial: bool,
) -> String {
    let shown_path = match renamed_from {
        Some(old) => format!("{old} → {path}"),
        None => path.to_string(),
    };
    let mut describe = status_text(status).to_string();
    if partial {
        describe.push_str(" · ");
        describe.push_str(t("gitCommit", "partiallyStaged"));
    }
    format!("{shown_path}\n{describe}")
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

// 建树 / 拍平是共用的(`git_ui::build_change_tree`:单链目录压缩、目录文件数、目录在前
// 按名称排),提交详情与提交 diff 左栏同一套。这里只管折叠 key 带区名。

/// 目录的折叠 key:带区名 —— 同一路径在四个区各自独立折叠(`GitChanges.tsx:285`、`:292`)。
fn dir_key(area: Area, full_path: &str) -> String {
    format!("{}:{}", area.key(), full_path)
}

/// 某个分组的树形行(界面行序;收起的目录不往下展开)。
fn area_tree_rows(
    paths: &[(usize, &str)],
    area: Area,
    collapsed: &HashSet<String>,
) -> Vec<ChangeTreeRow> {
    let tree = git_ui::build_change_tree(paths);
    if collapsed.is_empty() {
        return git_ui::flatten_change_tree(&tree, &|_| false);
    }
    // 折叠 key 拼在一块复用的缓冲里([`dir_key`] 的同一口径):每个目录行 `format!` 一次
    // 是白分配,目录多的时候一次拍平就是几千次
    let key = RefCell::new(format!("{}:", area.key()));
    let prefix = key.borrow().len();
    git_ui::flatten_change_tree(&tree, &|full_path| {
        let mut key = key.borrow_mut();
        key.truncate(prefix);
        key.push_str(full_path);
        collapsed.contains(key.as_str())
    })
}

// ─── 行模型 ───────────────────────────────────────────────────

/// 「更改」列表里的一行,也就是虚拟列表的一项。
#[derive(Debug, Clone, PartialEq)]
enum ListRow {
    /// 分组标题。`collapsed` = 这一组折起来了(只剩标题)
    Header {
        area: Area,
        count: usize,
        collapsed: bool,
    },
    /// 树形视图的目录行(单链压缩后的名称、递归文件数)
    Dir {
        area: Area,
        name: String,
        full_path: String,
        depth: usize,
        collapsed: bool,
        file_count: usize,
    },
    /// 文件行。`index` 是 `changes` 里的下标,`depth` 是树形视图的层数(列表视图恒 0)
    File {
        area: Area,
        index: usize,
        depth: usize,
    },
}

impl ListRow {
    /// 这一行占多高:分组标题连上外边距,文件 / 目录行一个行高。虚拟列表按它定位,
    /// 每种行画出来的高度必须与它一致(见各 `render_*`)。
    fn height(&self) -> f32 {
        match self {
            ListRow::Header { .. } => GROUP_HEADER_GAP + GROUP_HEADER_H,
            ListRow::Dir { .. } | ListRow::File { .. } => ROW_HEIGHT,
        }
    }
}

/// 「更改」列表的派生模型(见模块注释「列表只建看得见的行」):按 `changes` + 两种折叠态
/// + 视图模式算一份,存在 [`GitChanges::model`] 的缓存里。
#[derive(Debug)]
struct ListModel {
    /// 按哪种视图算的(与 store 里的视图模式对不上就作废重算)
    tree_mode: bool,
    /// 四个分组各自的文件下标(`changes` 里的原顺序),按 [`Area::slot`] 摆
    members: [Vec<usize>; 4],
    /// 四个分组的文件**显示顺序**:列表视图同 `members`;树形视图按树走,收起的目录里
    /// 的文件不算。**不看分组折叠** —— 折起来的分组照样能 Shift 连选、diff 弹窗照样能
    /// 前后换文件(与此前现算的 `area_order` 同一口径)
    order: [Vec<usize>; 4],
    /// 界面上的行序:每个非空分组一个标题,展开的分组接着它的文件 / 目录行
    rows: Vec<ListRow>,
    /// 每行的尺寸(虚拟列表的 `item_sizes`,只用高)。随模型缓存,同一份数据跨帧是同一个
    /// `Rc`,虚拟列表比一下就知道不用重算前缀和
    sizes: Rc<Vec<Size<Pixels>>>,
    /// 列表摊开要多高:上下内边距 + 全部行。没有变更(加载中 / 失败 / 空态整块占位,
    /// 高度不定)为 `None`
    height: Option<f32>,
}

impl ListModel {
    fn build(
        changes: &[ChangeFileStatus],
        collapsed_groups: &HashSet<Area>,
        collapsed_dirs: &HashSet<String>,
        tree_mode: bool,
    ) -> Self {
        let mut members: [Vec<usize>; 4] = Default::default();
        for (index, file) in changes.iter().enumerate() {
            // 部分暂存的文件两组都进(见模块注释「四个分组的口径」)
            for area in Area::ORDER {
                if in_area(file, area) {
                    members[area.slot()].push(index);
                }
            }
        }
        let mut order: [Vec<usize>; 4] = Default::default();
        let mut rows = Vec::new();
        for area in Area::ORDER {
            let indices = &members[area.slot()];
            if indices.is_empty() {
                continue;
            }
            let group_rows = if tree_mode {
                let paths: Vec<(usize, &str)> = indices
                    .iter()
                    .map(|&i| (i, changes[i].path.as_str()))
                    .collect();
                area_tree_rows(&paths, area, collapsed_dirs)
            } else {
                indices
                    .iter()
                    .map(|&index| ChangeTreeRow::File { index, depth: 0 })
                    .collect()
            };
            order[area.slot()] = git_ui::row_file_order(&group_rows);
            let collapsed = collapsed_groups.contains(&area);
            rows.push(ListRow::Header {
                area,
                count: indices.len(),
                collapsed,
            });
            if collapsed {
                continue;
            }
            rows.extend(group_rows.into_iter().map(|row| match row {
                ChangeTreeRow::Dir {
                    name,
                    full_path,
                    depth,
                    collapsed,
                    file_count,
                } => ListRow::Dir {
                    area,
                    name,
                    full_path,
                    depth,
                    collapsed,
                    file_count,
                },
                ChangeTreeRow::File { index, depth } => ListRow::File { area, index, depth },
            }));
        }
        let sizes = rows
            .iter()
            .map(|row| size(px(0.0), px(row.height())))
            .collect();
        let height = (!changes.is_empty())
            .then(|| LIST_PAD_Y + rows.iter().map(ListRow::height).sum::<f32>());
        Self {
            tree_mode,
            members,
            order,
            rows,
            sizes: Rc::new(sizes),
            height,
        }
    }

    /// 某个分组的全部文件(`changes` 里的原顺序)。
    fn members(&self, area: Area) -> &[usize] {
        &self.members[area.slot()]
    }

    /// 某个分组里文件的显示顺序(见字段注释)。
    fn order(&self, area: Area) -> &[usize] {
        &self.order[area.slot()]
    }

    /// 键盘导航走的全部可见文件行(行序里的文件行:折起来的分组、收起的目录跳过)。
    fn nav(&self) -> impl Iterator<Item = (Area, usize)> + '_ {
        self.rows.iter().filter_map(|row| match *row {
            ListRow::File { area, index, .. } => Some((area, index)),
            _ => None,
        })
    }

    /// 某个文件行在行序里是第几行。
    fn file_row(&self, area: Area, index: usize) -> Option<usize> {
        self.rows.iter().position(|row| {
            matches!(*row, ListRow::File { area: a, index: i, .. } if a == area && i == index)
        })
    }

    /// 第 `row` 行在列表内容里的上下沿(从列表顶边量,含上内边距)。
    fn row_span(&self, row: usize) -> Option<(f32, f32)> {
        let height = self.rows.get(row)?.height();
        let top = LIST_PAD_TOP + self.rows[..row].iter().map(ListRow::height).sum::<f32>();
        Some((top, top + height))
    }
}

/// 把上下沿是 `[top, bottom)` 的一行滚进高 `viewport` 的列表视口,返回新的纵向偏移
/// (`offset_y` ≤ 0,与 gpui 的滚动偏移同号)。口径照 gpui `ScrollHandle::scroll_to_item`
/// 的默认策略(`div.rs` 的 `scroll_to_active_item`):比视口高的行顶对齐;否则只滚最少的
/// 量,已经整行可见就不动。上下沿按**整个列表框**比、不扣内边距 —— 换成虚拟列表之前就是
/// 这么滚的,键盘走到最后一行时底下那 8px 内边距照旧不露出来。
fn reveal_offset(offset_y: f32, top: f32, bottom: f32, viewport: f32) -> f32 {
    if bottom - top > viewport || top + offset_y < 0.0 {
        -top
    } else if bottom + offset_y > viewport {
        viewport - bottom
    } else {
        offset_y
    }
}

// ─── 组件 ─────────────────────────────────────────────────────

/// 一次后台取数的结果(变更 + 冲突细节 + 提交区要的 HEAD 信息)。
struct Snapshot {
    changes: Vec<ChangeFileStatus>,
    conflicts: Vec<ConflictInfo>,
    /// 进行中的合并 / 变基…(冲突菜单「采用 ours / theirs」的文案随它翻转)
    op_kind: Option<RepoOpKind>,
    target: Option<CommitTarget>,
    has_head: bool,
}

/// 取数落地会改到的那几样,落地前拿来比「这份快照收下之后界面变不变」
/// (见 [`GitChanges::apply_snapshot`])。只借不拷。
struct Landed<'a> {
    changes: &'a [ChangeFileStatus],
    conflicts: &'a [ConflictInfo],
    op_kind: Option<RepoOpKind>,
    target: Option<&'a CommitTarget>,
    has_head: bool,
    amend: bool,
    load_error: Option<&'a str>,
}

impl Landed<'_> {
    /// 收下 `snap` 之后有没有东西变(落地的赋值口径见 `apply_snapshot`):冲突细节与
    /// 进行中的操作只进菜单,也照样算 —— 宁可多画一帧,不能让菜单拿着旧数据。
    /// 「已推送」判定不在快照里(另走一趟,见 [`GitChanges::refresh_amend`],变了自己重画)。
    fn differs(&self, snap: &Snapshot) -> bool {
        // 没有 HEAD 了 amend 收起
        let amend = self.amend && snap.has_head;
        !same_changes(self.changes, &snap.changes)
            || self.conflicts != snap.conflicts.as_slice()
            || self.op_kind != snap.op_kind
            || self.target != snap.target.as_ref()
            || self.has_head != snap.has_head
            || self.amend != amend
            || self.load_error.is_some()
    }

    /// 取数失败(列表清空、原因顶替空态)之后有没有东西变:同一条错误反复落地不算。
    fn differs_on_error(&self, error: &str) -> bool {
        !self.changes.is_empty()
            || !self.conflicts.is_empty()
            || self.op_kind.is_some()
            || self.load_error != Some(error)
    }
}

/// 两份变更列表一模一样(逐项逐字段比 —— `ChangeFileStatus` 没有派生 `PartialEq`)。
fn same_changes(a: &[ChangeFileStatus], b: &[ChangeFileStatus]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.path == y.path
                && x.old_path == y.old_path
                && x.staged_status == y.staged_status
                && x.unstaged_status == y.unstaged_status
                && x.status_label == y.status_label
        })
}

pub struct GitChanges {
    store: Entity<AppStore>,
    /// 捕获的仓库后端;`None` 时停在空态,不发起任何 IO。
    context: Option<GitContext>,
    changes: Vec<ChangeFileStatus>,
    /// 冲突文件的三方条目在不在(删除 / 修改冲突时有一侧缺席),随列表一起取
    conflicts: Vec<ConflictInfo>,
    /// 进行中的合并 / 变基 / 拣选 / 还原,随列表一起取。变基时 git 的 ours / theirs
    /// 与直觉相反,冲突菜单据此换成写清方向的文案
    op_kind: Option<RepoOpKind>,
    loading: bool,
    loaded_once: bool,
    /// 上一次取变更失败的原文;`Some` 时列表区显示它而不是空态。
    load_error: Option<String>,
    /// 组件态,**不落盘**(抽屉一关就没,规格 §11 第 25 条)。
    collapsed_dirs: HashSet<String>,
    collapsed_groups: HashSet<Area>,
    /// 列表取数的单飞调度(见模块注释「取数调度」);换仓库时作废在途的那份。
    flight: Flight,
    /// 派生模型的缓存(见 [`Self::model`])。改了 `changes` / 两种折叠态的地方都要
    /// [`Self::invalidate_model`];视图模式不归这里管,`model` 自己比
    list_model: RefCell<Option<Rc<ListModel>>>,
    /// diff 弹窗按块操作后在等的回执:最新一次重取落地后,按那时的列表把该分组的新文件
    /// 序列交回去(见 `git_diff::ReloadFiles`)。换仓库时以 `None` 交回。
    diff_waiters: Vec<(Area, git_diff::FilesDone)>,
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
    /// 列表的滚动位置。虚拟列表与加载中 / 空态那几种普通滚动容器共用(后者走它底下那个
    /// `ScrollHandle`);必须存在视图上,每帧新建等于每帧把滚动清零
    list_scroll: VirtualListScrollHandle,
    /// 下一帧把键盘游标所在的行滚进视野(键盘走动、操作后落回、diff 弹窗里换了文件)。
    /// 到 render 时才按当帧的行序找行,prepaint 时按当帧的视口算偏移
    reveal_cursor: bool,

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
    /// 勾着 amend 时随列表重取「已推送」判定的那一趟(单飞,见 [`Self::refresh_amend`])
    amend_flight: Flight,
    /// 眼下 `amend_info` 是在哪份引用签名下取的:没变就不重取
    amend_key: Option<u64>,
    committing: bool,
    history_loading: bool,
    /// 上一次提交失败的说明(已格式化好的整句),挂在提交按钮下方。
    commit_error: Option<String>,
    /// 提交区整块的高度(canvas 量的):面板按内容分高度时要([`Self::natural_height`])。
    commit_area_h: Rc<Cell<f32>>,
    /// 提交按钮那一行(主按钮 + ▾)的窗口坐标:▾ 菜单贴着它的下沿 / 上沿弹,不压按钮。
    commit_buttons: Rc<Cell<Option<Bounds<Pixels>>>>,
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
            context: None,
            changes: Vec::new(),
            conflicts: Vec::new(),
            op_kind: None,
            loading: false,
            loaded_once: false,
            load_error: None,
            collapsed_dirs: HashSet::new(),
            collapsed_groups: HashSet::new(),
            flight: Flight::default(),
            list_model: RefCell::new(None),
            diff_waiters: Vec::new(),
            debounce_until: None,
            busy_ops: 0,
            sel_area: None,
            selected: HashSet::new(),
            anchor: None,
            cursor: None,
            reselect: None,
            list_focus: cx.focus_handle(),
            list_scroll: VirtualListScrollHandle::new(),
            reveal_cursor: false,
            commit_input,
            target: None,
            placeholder: None,
            has_head: false,
            amend: false,
            amend_info: None,
            amend_prefill: None,
            amend_request: 0,
            amend_flight: Flight::default(),
            amend_key: None,
            committing: false,
            history_loading: false,
            commit_error: None,
            commit_area_h: Rc::new(Cell::new(0.0)),
            commit_buttons: Rc::new(Cell::new(None)),
            _subs: subs,
        }
    }

    /// 「更改」区整块摊开(列表不滚动)要多高:列表内容 + 上下留白 + 提交区。面板分配
    /// 「更改 / 提交历史」两块高度时用(`git_panel::section_split`)。提交区是上一帧量的;
    /// 列表有分组时读派生模型的摊开总高([`ListModel::height`]),否则也取上一帧量的。
    /// 还没画过时为 `None`。
    ///
    /// 列表高度按**眼下的数据**、不按上一帧量:外部改动多出一组(未暂存)的那一帧,
    /// 「更改」区还是按旧内容分的高度,排在最后的分组(未跟踪)被挤出可见区、下一帧才
    /// 回来(E2E r4 A12 的过渡帧)。模型是缓存的,面板一帧问几次都只是读一个数。
    pub fn natural_height(&self, cx: &App) -> Option<f32> {
        let commit = self.commit_area_h.get();
        if commit <= 0.0 {
            return None;
        }
        let list = match self.model(cx).height {
            Some(list) => list,
            None => {
                let last = self.list_scroll.children_count().checked_sub(1)?;
                let first = self.list_scroll.bounds_for_item(0)?;
                let last = self.list_scroll.bounds_for_item(last)?;
                // 列表的上下内边距(4 + 8)+ 首个分组标题的上外边距 4
                f32::from(last.bottom() - first.top()) + LIST_PAD_Y + GROUP_HEADER_GAP
            }
        };
        Some(list + commit)
    }

    /// 「更改」区最矮要多高:提交区整块 + 列表至少露出一行(列表本身更矮时按列表)。
    /// 中缝拖到下限时面板按它兜底,提交按钮整颗露着、点得到(E2E r4 B11)。`natural` 是
    /// 同一帧的 [`Self::natural_height`](面板已经问过一次,不再算第二遍)。还没画过时
    /// 为 `None`。
    pub fn min_height(&self, natural: Option<f32>) -> Option<f32> {
        let commit = self.commit_area_h.get();
        if commit <= 0.0 {
            return None;
        }
        let floor = commit + LIST_PAD_Y + GROUP_HEADER_GAP + ROW_HEIGHT;
        Some(natural.map_or(floor, |natural| natural.min(floor)))
    }

    /// 换仓库(容器调)。`None` = 无仓库;后端身份随上下文一起比较。
    pub fn set_repo(&mut self, repo: Option<GitContext>, cx: &mut Context<Self>) {
        if self.context == repo {
            return;
        }
        self.context = repo;
        self.loading = false;
        self.loaded_once = false;
        self.busy_ops = 0;
        self.committing = false;
        self.history_loading = false;
        self.amend_request += 1;
        self.changes.clear();
        self.conflicts.clear();
        self.op_kind = None;
        self.collapsed_dirs.clear();
        self.invalidate_model();
        self.clear_selection();
        self.target = None;
        self.has_head = false;
        // amend 属于上一个仓库的 HEAD
        self.amend = false;
        self.amend_info = None;
        self.amend_prefill = None;
        self.amend_key = None;
        self.amend_flight.reset();
        // 错误都属于上一个仓库
        self.load_error = None;
        self.commit_error = None;
        // 在等序列的 diff 弹窗属于上一个仓库:沿用它们的旧序列
        self.flush_diff_waiters(false, cx);
        // 上一个仓库在途的那趟作废:回来一律丢,新仓库这趟不必等它
        self.flight.reset();
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

    /// 重取列表(单飞,见模块注释「取数调度」)。勾着 amend 时顺带另起一趟刷新「已推送」判定。
    pub fn load(&mut self, cx: &mut Context<Self>) {
        if self.context.is_none() {
            self.flush_diff_waiters(false, cx);
            return;
        }
        self.refresh_amend(cx);
        self.start_load(cx);
    }

    /// 在 `at` 那一刻之前的工作区 / 索引变化,列表这边是不是已经有一趟取数会反映出来
    /// (在那之后才开跑的,或者排着补跑)。面板拿它判断监听交来的一批要不要再要一次。
    pub fn load_covers(&self, at: Instant) -> bool {
        self.flight.covers(at)
    }

    fn start_load(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.context.clone() else {
            return;
        };
        // 在途:只记脏位,那趟落地后当场补跑
        let Some(token) = self.flight.begin(Instant::now()) else {
            return;
        };
        self.loading = !self.loaded_once;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { load_snapshot(&repo) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                let rerun = match this.flight.finish(token) {
                    Landing::Stale => return,
                    Landing::Done { rerun } => rerun,
                };
                this.apply_snapshot(result, rerun, cx);
                if rerun {
                    this.start_load(cx);
                }
            });
        })
        .detach();
        // 只有首次取数显示加载态;空的稳定快照也不能在轮询时反复闪烁。
        if self.loading {
            cx.notify();
        }
    }

    /// 勾着「修改上次提交」时重取「已推送」判定(见模块注释「取数调度」):单独一趟、不挡
    /// 列表;引用签名没变就只算一次签名、不重取。没勾 / 无仓库时什么都不做。
    fn refresh_amend(&mut self, cx: &mut Context<Self>) {
        if !self.amend || self.context.is_none() {
            return;
        }
        let Some(token) = self.amend_flight.begin(Instant::now()) else {
            return;
        };
        let req = self.amend_request;
        let Some(repo) = self.context.clone() else {
            return;
        };
        let known = self.amend_key;
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_executor()
                .spawn(async move { fetch_amend(&repo, known) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                let rerun = match this.amend_flight.finish(token) {
                    Landing::Stale => return,
                    Landing::Done { rerun } => rerun,
                };
                // 期间取消过勾选(令牌前移)就作废;换仓库时 amend_flight 已作废,走不到这里
                if this.amend
                    && this.amend_request == req
                    && let Some((key, info)) = fetched
                {
                    this.amend_key = key;
                    if this.amend_info != info {
                        this.amend_info = info;
                        cx.notify();
                    }
                }
                if rerun {
                    this.refresh_amend(cx);
                }
            });
        })
        .detach();
    }

    /// 一次取数落地(迟到的回包单飞调度已经丢掉了)。收下之后界面有东西变(数据、选择、
    /// 加载态)才重算派生模型、才重画;与眼下一模一样的回包什么都不碰 —— 此前每次落地
    /// 都无条件 notify,监听 / pty 嗅探触发的每一趟都是一次整窗帧。
    ///
    /// `rerun` = 在途期间又有人要过、落地后当场补跑:这份照样落(比画着的新),但它可能
    /// 还不含触发补跑的那次操作 ——「操作后落回的位置」与在等的 diff 弹窗留给补跑那份。
    /// 不补跑时 diff 弹窗的回执照交(它要的是「这一次重取落地了」,不管变没变)。
    fn apply_snapshot(
        &mut self,
        result: anyhow::Result<Snapshot>,
        rerun: bool,
        cx: &mut Context<Self>,
    ) {
        let loading_shown = self.shows_loading();
        self.loading = false;
        self.loaded_once = true;
        let reselect = if rerun { self.reselect.take() } else { None };
        let loaded = result.is_ok();
        let changed = match result {
            Ok(snap) => {
                let changed = self.landed().differs(&snap);
                self.changes = snap.changes;
                self.conflicts = snap.conflicts;
                self.op_kind = snap.op_kind;
                self.target = snap.target;
                self.has_head = snap.has_head;
                if !self.has_head {
                    self.amend = false;
                }
                self.load_error = None;
                changed
            }
            Err(err) => {
                // 原版 `.catch(() => setChanges([]))` —— 失败即清空;
                // 这里另把原因留给列表区显示,别让人当成「没有变更」
                eprintln!("[git] 取变更失败: {err:#}");
                let error = format!("{err:#}");
                let changed = self.landed().differs_on_error(&error);
                self.changes.clear();
                self.conflicts.clear();
                self.op_kind = None;
                self.load_error = Some(error);
                changed
            }
        };
        if changed {
            self.invalidate_model();
        }
        let reselected = self.after_reload(cx);
        if rerun {
            self.reselect = reselect;
        } else {
            self.flush_diff_waiters(loaded, cx);
        }
        if changed || reselected || loading_shown != self.shows_loading() {
            cx.notify();
        }
    }

    /// 落地前的比对视图(见 [`Landed`])。
    fn landed(&self) -> Landed<'_> {
        Landed {
            changes: &self.changes,
            conflicts: &self.conflicts,
            op_kind: self.op_kind,
            target: self.target.as_ref(),
            has_head: self.has_head,
            amend: self.amend,
            load_error: self.load_error.as_deref(),
        }
    }

    /// 列表区此刻画的是不是「加载中」(只有没数据、也没报错时才画它)。
    fn shows_loading(&self) -> bool {
        self.loading && self.changes.is_empty() && self.load_error.is_none()
    }

    /// 把在等序列的 diff 弹窗回执交出去:`loaded` 时按眼下的列表给该分组的新序列,否则
    /// `None`(弹窗沿用旧序列)。回执改的是弹窗的实体、还可能回调本列表(换了文件时让
    /// 选中项跟过去),延到这一轮更新之后再调,免得本实体正被借着时重入。
    fn flush_diff_waiters(&mut self, loaded: bool, cx: &mut Context<Self>) {
        if self.diff_waiters.is_empty() {
            return;
        }
        let model = self.model(cx);
        for (area, done) in std::mem::take(&mut self.diff_waiters) {
            let files = loaded.then(|| self.diff_files(model.order(area), area));
            cx.defer(move |cx| done(files, cx));
        }
    }

    fn tree_mode(&self, cx: &App) -> bool {
        self.store.read(cx).git_changes_view_mode() == "tree"
    }

    /// 派生模型(见 [`ListModel`])。缓存着、且是按眼下的视图模式算的就直接给,否则按
    /// 眼下的数据算一份存起来 —— 视图模式住在 store 里(面板标题栏的切换钮、设置同步都
    /// 能改它),这里每次比一下,不靠谁记得来作废。
    fn model(&self, cx: &App) -> Rc<ListModel> {
        let tree_mode = self.tree_mode(cx);
        if let Some(model) = self
            .list_model
            .borrow()
            .as_ref()
            .filter(|model| model.tree_mode == tree_mode)
        {
            return model.clone();
        }
        let model = Rc::new(ListModel::build(
            &self.changes,
            &self.collapsed_groups,
            &self.collapsed_dirs,
            tree_mode,
        ));
        *self.list_model.borrow_mut() = Some(model.clone());
        model
    }

    /// 派生模型作废(`changes` / 分组折叠 / 目录折叠变了),下次用到时重算。
    fn invalidate_model(&self) {
        self.list_model.borrow_mut().take();
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
            let model = self.model(cx);
            let order = model.order(area);
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
        self.model(cx)
            .order(area)
            .iter()
            .map(|&i| self.changes[i].path.clone())
            .filter(|p| self.selected.contains(p))
            .collect()
    }

    /// 当前选中集(键盘动作用),按显示顺序。
    fn selection_targets(&self, cx: &App) -> Option<(Area, Vec<String>)> {
        let area = self.sel_area?;
        let paths: Vec<String> = self
            .model(cx)
            .order(area)
            .iter()
            .map(|&i| self.changes[i].path.clone())
            .filter(|p| self.selected.contains(p))
            .collect();
        (!paths.is_empty()).then_some((area, paths))
    }

    /// 选中集里第一项在本组显示顺序里的位置(操作完落回这里 = 停在「下一行」)。
    fn selection_anchor_pos(&self, area: Area, cx: &App) -> Option<usize> {
        self.model(cx)
            .order(area)
            .iter()
            .position(|&i| self.selected.contains(&self.changes[i].path))
    }

    /// 重取列表之后:清掉已不在原分组里的选中项;有待落回的位置就落回去。选择因此
    /// 变了就返回 `true`(数据没变时落地也得为它重画)。
    fn after_reload(&mut self, cx: &mut Context<Self>) -> bool {
        let model = self.model(cx);
        if let Some((area, pos)) = self.reselect.take() {
            let order = model.order(area);
            if let Some(&index) = order.get(pos).or(order.last()) {
                self.select_single(area, index);
                self.reveal_cursor = true;
                return true;
            }
            self.clear_selection();
            return true;
        }
        let Some(area) = self.sel_area else {
            return false;
        };
        let present: HashSet<&str> = model
            .members(area)
            .iter()
            .map(|&i| self.changes[i].path.as_str())
            .collect();
        let before = self.selected.len();
        self.selected.retain(|p| present.contains(p.as_str()));
        if self.selected.is_empty() {
            self.clear_selection();
            true
        } else if self
            .cursor
            .as_ref()
            .is_some_and(|(_, p)| !present.contains(p.as_str()))
        {
            self.cursor = None;
            true
        } else {
            self.selected.len() != before
        }
    }

    fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows: Vec<(Area, usize)> = self.model(cx).nav().collect();
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
        self.reveal_cursor = true;
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
        window: &mut Window,
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
            Area::Staged => self.unstage(paths, Some((area, pos)), window, cx),
            Area::Unstaged | Area::Untracked => self.stage(paths, Some((area, pos)), window, cx),
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

    /// 仓库的写操作闸被别的区占着(面板在拉取、历史区在拣选…):行内 / 组操作与提交
    /// 按钮按它禁用。自己在跑的那一件由 `busy_ops` / `committing` 管。
    fn gated(&self) -> bool {
        self.context
            .as_ref()
            .is_some_and(|repo| git_ui::gate::is_busy_key(&repo.key()))
    }

    /// 整文件操作能不能点:自己没在跑、闸也没被别人占。render 里只问一次、传给每一行
    /// (问闸要拼一次比较键,不该每行一次)。
    fn ops_enabled(&self) -> bool {
        self.ops_enabled_with(self.gated())
    }

    /// [`Self::ops_enabled`],闸的状态由调用方给(同一帧已经问过了)。
    fn ops_enabled_with(&self, gated: bool) -> bool {
        self.busy_ops == 0 && !gated
    }

    /// 跑一件阻塞的 git 动作(暂存 / 取消暂存 / 丢弃 / 冲突处理),占闸、后台执行,完成后
    /// 重取列表。失败弹详情框(口径见模块注释);闸被占时推 toast、不执行。`reselect` 是
    /// 完成后要落回的位置。
    fn run_op(
        &mut self,
        op: impl FnOnce(&GitContext) -> anyhow::Result<()> + Send + 'static,
        reselect: Option<(Area, usize)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.context.clone() else {
            return;
        };
        self.run_op_on(&repo, op, reselect, window, cx);
    }

    fn run_op_on(
        &mut self,
        repo: &GitContext,
        op: impl FnOnce(&GitContext) -> anyhow::Result<()> + Send + 'static,
        reselect: Option<(Area, usize)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected = self.context.as_ref() == Some(repo);
        let context = repo.clone();
        let this = cx.entity().downgrade();
        let started = git_ui::spawn_gated(
            repo,
            move |repo| {
                let result = op(repo);
                // 写完当场读一份签名与冲突数(成败都读:失败也可能改了一半),**先于**下面那次
                // 重取列表 —— 面板拿它当「已知」,不能比列表更新(见 `IndexWritten`)
                let observed = RepoObservation {
                    at: Instant::now(),
                    signature: repo.repo_signature().ok(),
                    op: repo.repo_operation_state().ok(),
                };
                Ok((result, observed))
            },
            move |outcome, window, cx| {
                // 外层只是包装,实际恒为 `Ok`
                let (result, observed) = match outcome {
                    Ok((result, observed)) => (result, Some(observed)),
                    Err(err) => (Err(err), None),
                };
                let _ = this.update(cx, |this: &mut Self, cx| {
                    if this.context.as_ref() != Some(&context) {
                        return;
                    }
                    this.busy_ops = this.busy_ops.saturating_sub(1);
                    match result {
                        Ok(()) => this.reselect = reselect,
                        Err(err) => {
                            eprintln!("[git] 变更操作失败: {err:#}");
                            git_ui::show_git_error(
                                t("gitChanges", "operationFailedTitle"),
                                git_ui::error_text(&err),
                                window,
                                cx,
                            );
                        }
                    }
                    this.load(cx);
                    // 丢弃改了工作区、解决冲突改了状态:文件树的 git 标记跟着重算
                    git_watch::request_refresh(git_watch::Subscriber::FileTree);
                    if let Some(observed) = observed {
                        cx.emit(GitChangesEvent::IndexWritten(observed));
                    }
                });
            },
            window,
            cx,
        );
        if !started {
            self.notify_gate_busy(cx);
            return;
        }
        if selected {
            self.busy_ops += 1;
            cx.notify();
        }
    }

    /// 写操作闸被占:推一条 toast(列表与按钮已画成禁用,走到这里多半是键盘快捷键)。
    fn notify_gate_busy(&self, cx: &mut Context<Self>) {
        git_ui::toast(
            &self.store,
            git_ui::gate_busy_text().to_string(),
            Tone::Attention,
            cx,
        );
    }

    /// 成功类的提示(已提交)。信息档 toast,点击只关闭。
    fn notify_info(&self, message: String, cx: &mut Context<Self>) {
        git_ui::toast(&self.store, message, Tone::Info, cx);
    }

    fn stage(
        &mut self,
        paths: Vec<String>,
        reselect: Option<(Area, usize)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        self.run_op(move |repo| repo.git_stage(&paths), reselect, window, cx);
    }

    fn unstage(
        &mut self,
        paths: Vec<String>,
        reselect: Option<(Area, usize)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        self.run_op(move |repo| repo.git_unstage(&paths), reselect, window, cx);
    }

    /// 某个分组的全部路径。
    fn area_paths(&self, area: Area, cx: &App) -> Vec<String> {
        self.model(cx)
            .members(area)
            .iter()
            .map(|&i| self.changes[i].path.clone())
            .collect()
    }

    /// 分组标题上的「全部暂存」:**只暂存这一组**。原先未跟踪组的按钮也调
    /// `git_stage_all`(`add_all("*")`),会把已跟踪的改动一起暂存。
    fn stage_group(&mut self, area: Area, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.area_paths(area, cx);
        self.stage(paths, None, window, cx);
    }

    fn unstage_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_op(|repo| repo.git_unstage_all(), None, window, cx);
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
        let Some(repo) = self.context.clone() else {
            return;
        };
        Confirm::new(t("gitChanges", "discardTitle"), message)
            .detail(preview_paths(&paths))
            .ok_text(t("gitChanges", "discardOk"))
            .cancel_text(t("gitChanges", "discardCancel"))
            .danger()
            .open(
                move |window, cx| {
                    let paths = paths.clone();
                    this.update(cx, |this, cx| {
                        let reselect = this.selection_anchor_pos(area, cx).map(|pos| (area, pos));
                        this.run_op_on(
                            &repo,
                            move |repo| repo.discard_unstaged(&paths),
                            reselect,
                            window,
                            cx,
                        );
                    });
                },
                window,
                cx,
            );
    }

    /// 冲突文件采用一侧。
    fn take_side(
        &mut self,
        paths: Vec<String>,
        side: ConflictSide,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_op(
            move |repo| {
                for path in &paths {
                    repo.resolve_conflict_with(path, side)?;
                }
                Ok(())
            },
            None,
            window,
            cx,
        );
    }

    /// 标记为已解决:先在后台看一眼工作区里还有没有 `<<<<<<<`,有就先确认。
    fn mark_resolved(&mut self, paths: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        let Some(repo) = self.context.clone() else {
            return;
        };
        let context = repo.clone();
        cx.spawn_in(window, async move |this, cx| {
            let checked = paths.clone();
            let marked: Vec<String> = cx
                .background_executor()
                .spawn(async move {
                    checked
                        .into_iter()
                        .filter(|p| repo.has_conflict_markers(p).unwrap_or(false))
                        .collect()
                })
                .await;
            let _ = this.update_in(cx, |this: &mut Self, window, cx| {
                let resolve =
                    move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                        this.run_op_on(
                            &context,
                            move |repo| {
                                for path in &paths {
                                    repo.mark_conflict_resolved(path)?;
                                }
                                Ok(())
                            },
                            None,
                            window,
                            cx,
                        );
                    };
                if marked.is_empty() {
                    resolve(this, window, cx);
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
                    move |window, cx| {
                        if let Some(resolve) = resolve.borrow_mut().take() {
                            entity.update(cx, |this, cx| resolve(this, window, cx));
                        }
                    },
                    window,
                    cx,
                );
            });
        })
        .detach();
    }

    /// diff 弹窗里能前后换的文件序列:某个分组的显示顺序(`order`,取自派生模型)。
    fn diff_files(&self, order: &[usize], area: Area) -> Vec<git_diff::DiffFile> {
        order.iter().map(|&i| self.diff_file(i, area)).collect()
    }

    fn diff_file(&self, index: usize, area: Area) -> git_diff::DiffFile {
        let file = &self.changes[index];
        let status = status_in_area(file, area);
        // 只有「两侧都是普通文本、能写回」的状态才给块操作:未跟踪 / 冲突 /
        // 重命名 / 删除都只支持整文件操作
        let hunk_actions = match area {
            Area::Staged => matches!(status, Some(GitStatus::Modified | GitStatus::Added)),
            Area::Unstaged => matches!(status, Some(GitStatus::Modified)),
            Area::Untracked | Area::Conflicts => false,
        };
        git_diff::DiffFile {
            path: file.path.clone(),
            // 已暂存的重命名:HEAD 那一侧按旧路径取(暂存侧才比 HEAD)
            old_path: file
                .old_path
                .clone()
                .filter(|_| matches!(status, Some(GitStatus::Renamed))),
            side: if area == Area::Staged {
                git_diff::DiffSide::Staged
            } else {
                git_diff::DiffSide::Unstaged
            },
            status_label: status_label_for(status).to_string(),
            hunk_actions,
        }
    }

    /// 打开某一行的 diff,文件序列 = 它所在分组的显示顺序(弹窗里 Alt+←/→ 前后换)。
    /// 按块操作之后序列跟着重载过的列表走(取消暂存最后一块,文件就离开了「已暂存」)。
    fn view_diff(&self, index: usize, area: Area, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.context.clone() else {
            return;
        };
        let context = repo.clone();
        let model = self.model(cx);
        let order = model.order(area);
        let files = self.diff_files(order, area);
        let current = order.iter().position(|&i| i == index).unwrap_or(0);
        let weak = cx.entity().downgrade();
        let reload_files: git_diff::ReloadFiles = Rc::new(
            move |done: git_diff::FilesDone, cx: &mut App| match weak.upgrade() {
                Some(this) => this.update(cx, |this, cx| {
                    if this.context.as_ref() != Some(&context) {
                        done(None, cx);
                        return;
                    }
                    this.diff_waiters.push((area, done));
                    this.load(cx);
                }),
                None => done(None, cx),
            },
        );
        let weak = cx.entity().downgrade();
        let context = repo.clone();
        let on_switch: git_diff::OnSwitch =
            Rc::new(move |file: &git_diff::DiffFile, cx: &mut App| {
                let path = file.path.clone();
                let _ = weak.update(cx, |this, cx| {
                    if this.context.as_ref() == Some(&context) {
                        this.follow_diff(area, &path, cx);
                    }
                });
            });
        git_diff::open_file_diff_with(
            self.store.clone(),
            repo,
            files,
            current,
            git_diff::FileDiffHooks {
                on_switch: Some(on_switch),
                reload_files: Some(reload_files),
                ..git_diff::FileDiffHooks::default()
            },
            window,
            cx,
        );
    }

    /// diff 弹窗里换了文件:列表的选中项跟过去(并滚进视野),关掉弹窗时停在看到的那个。
    fn follow_diff(&mut self, area: Area, path: &str, cx: &mut Context<Self>) {
        let Some(index) = self
            .changes
            .iter()
            .position(|c| c.path == path && in_area(c, area))
        else {
            return;
        };
        self.select_single(area, index);
        self.reveal_cursor = true;
        cx.notify();
    }

    // ── 提交 ──

    /// 提交按钮为什么点不了(`None` = 能点)。`stage_first` = 「全部暂存并提交」。
    fn commit_blocker(&self, stage_first: bool, cx: &App) -> Option<String> {
        self.commit_blocker_with(stage_first, self.gated(), &self.model(cx), cx)
    }

    /// [`Self::commit_blocker`],闸的状态与派生模型由调用方给(render 里同一帧已经
    /// 取过;已暂存 / 冲突 / 未暂存的数目都从模型读,不再每帧扫一遍 `changes`)。
    fn commit_blocker_with(
        &self,
        stage_first: bool,
        gated: bool,
        model: &ListModel,
        cx: &App,
    ) -> Option<String> {
        if self.context.is_none() {
            return Some(t("gitCommit", "blockNoRepo").to_string());
        }
        if self.committing {
            return Some(t("gitCommit", "blockBusy").to_string());
        }
        if gated {
            return Some(git_ui::gate_busy_text().to_string());
        }
        let conflicts = model.members(Area::Conflicts).len();
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
            return model
                .members(Area::Unstaged)
                .is_empty()
                .then(|| t("gitCommit", "blockNothingToStage").to_string());
        }
        if model.members(Area::Staged).is_empty() {
            return Some(if model.members(Area::Unstaged).is_empty() {
                t("gitCommit", "blockNothingStaged").to_string()
            } else {
                t("gitCommit", "blockNothingStagedHint").to_string()
            });
        }
        None
    }

    /// 提交。`open_push` = 「提交并推送」:提交成功后让容器打开推送对话框(见模块注释);
    /// `stage_first` = 先暂存全部已跟踪改动(未暂存组,不含未跟踪文件)。
    fn commit(
        &mut self,
        open_push: bool,
        stage_first: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.commit_blocker(stage_first, cx).is_some() {
            return;
        }
        let Some(repo) = self.context.clone() else {
            return;
        };
        let context = repo.clone();
        let message = self.commit_input.read(cx).value().trim().to_string();
        let amend = self.amend;
        let stage_paths = if stage_first {
            self.area_paths(Area::Unstaged, cx)
        } else {
            Vec::new()
        };
        let input = self.commit_input.clone();
        let this = cx.entity().downgrade();
        // git_commit 走 git CLI(60s 超时兜底 GPG/pre-commit hook 挂起)——
        // 即便如此也不上主线程跑,hook 慢的仓库会把 UI 卡满整个超时窗口
        let started = git_ui::spawn_gated(
            &repo,
            move |repo| {
                if !stage_paths.is_empty() {
                    repo.git_stage(&stage_paths)?;
                }
                repo.git_commit_with(&message, amend)
            },
            move |result, window, cx| {
                let _ = this.update(cx, |this: &mut Self, cx| {
                    if this.context.as_ref() != Some(&context) {
                        return;
                    }
                    this.committing = false;
                    match result {
                        Ok(outcome) => {
                            input.update(cx, |state, cx| state.set_value("", window, cx));
                            this.amend = false;
                            this.amend_info = None;
                            this.amend_prefill = None;
                            this.amend_key = None;
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
                            git_watch::request_refresh(git_watch::Subscriber::FileTree);
                            cx.emit(GitChangesEvent::Committed { open_push });
                        }
                        Err(err) => {
                            // 提交消息留在框里不清,改完(或修好 hook)直接再点;提交失败
                            // 就不打开推送对话框
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
            },
            window,
            cx,
        );
        if !started {
            self.notify_gate_busy(cx);
            return;
        }
        self.committing = true;
        self.commit_error = None;
        cx.notify();
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
            self.amend_key = None;
            cx.notify();
            return;
        }
        let req = self.amend_request;
        let Some(repo) = self.context.clone() else {
            return;
        };
        let input = self.commit_input.clone();
        cx.spawn_in(window, async move |this, cx| {
            let (key, result) = cx
                .background_executor()
                .spawn(async move {
                    // 顺带记下这份是在哪份引用签名下取的:之后随列表重取时没变就不再取
                    (amend_key_of(&repo), repo.amend_info())
                })
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
                        this.amend_key = key;
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
        if self.history_loading || self.context.is_none() {
            return;
        }
        self.history_loading = true;
        cx.notify();
        let Some(repo) = self.context.clone() else {
            return;
        };
        let context = repo.clone();
        let input = self.commit_input.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { repo.recent_commit_messages(HISTORY_LIMIT) })
                .await;
            let _ = this.update_in(cx, |this: &mut Self, window, cx| {
                if this.context.as_ref() != Some(&context) {
                    return;
                }
                this.history_loading = false;
                cx.notify();
                let list = match result {
                    Ok(list) => list,
                    Err(err) => {
                        eprintln!("[git] 读取提交消息历史失败: {err:#}");
                        git_ui::show_git_error(
                            t("gitChanges", "operationFailedTitle"),
                            git_ui::error_text(&err),
                            window,
                            cx,
                        );
                        return;
                    }
                };
                let entries = history_entries(list, &input, cx.entity(), context);
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

/// 「已推送」判定的缓存键:引用那一半签名(HEAD + 全部引用)。只看 HEAD 不够 —— 推送 /
/// fetch 挪了远程跟踪分支,同一个 HEAD 的判定也会变。读不到为 `None`(当作变了)。
fn amend_key_of(repo: &GitContext) -> Option<u64> {
    repo.repo_signature().ok().map(|sig| sig.refs)
}

/// 手上那份 amend 信息还作不作数:键读不到、或与取它时的键不同,都要重取。
fn amend_stale(known: Option<u64>, now: Option<u64>) -> bool {
    now.is_none() || now != known
}

/// [`GitChanges::refresh_amend`] 的后台那一趟:键没变返回 `None`(不重取),否则
/// `Some((新键, 信息))` —— 信息读失败 / 空仓库为 `None`,与原先随列表取时同一口径。
fn fetch_amend(repo: &GitContext, known: Option<u64>) -> Option<(Option<u64>, Option<AmendInfo>)> {
    let key = amend_key_of(repo);
    amend_stale(known, key).then(|| (key, repo.amend_info().ok().flatten()))
}

/// 一次取数:变更列表 + 冲突细节 + 提交区要的 HEAD 信息。全部是只读的 git2 查询。
/// amend 的「已推送」判定不在这里(见 [`GitChanges::refresh_amend`])。
fn load_snapshot(repo: &GitContext) -> anyhow::Result<Snapshot> {
    let changes = repo.get_changes_status()?;
    let conflicts = if changes.iter().any(is_conflicted) {
        repo.list_conflicts()?
    } else {
        Vec::new()
    };
    let op_kind = repo.repo_operation_state()?.kind;
    Ok(Snapshot {
        changes,
        conflicts,
        op_kind,
        target: Some(repo.commit_target()?),
        has_head: repo.head_exists(),
    })
}

/// 冲突菜单「采用一侧」的文案 key(`gitCommit` 命名空间)。`deleted` = 那一侧是删除。
///
/// 变基时 git 的 ours / theirs 与直觉**相反**:ours 是被变基到的那一侧(上游),
/// theirs 才是正在重放的、用户自己的提交 —— 照「我方 / 对方」写,用户点「采用我方」
/// 恰好丢掉自己的改动。合并 / 拣选 / 还原时 ours 就是当前分支,维持原文案。
fn take_side_key(side: ConflictSide, deleted: bool, op: Option<RepoOpKind>) -> &'static str {
    let rebasing = op == Some(RepoOpKind::Rebase);
    match (side, deleted, rebasing) {
        (ConflictSide::Ours, false, false) => "takeOurs",
        (ConflictSide::Ours, true, false) => "takeOursDeleted",
        (ConflictSide::Theirs, false, false) => "takeTheirs",
        (ConflictSide::Theirs, true, false) => "takeTheirsDeleted",
        (ConflictSide::Ours, false, true) => "takeOursRebase",
        (ConflictSide::Ours, true, true) => "takeOursRebaseDeleted",
        (ConflictSide::Theirs, false, true) => "takeTheirsRebase",
        (ConflictSide::Theirs, true, true) => "takeTheirsRebaseDeleted",
    }
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
    context: GitContext,
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
            let context = context.clone();
            let message = entry.message;
            MenuItem::new(label)
                .detail(format_date(entry.timestamp))
                .on_click(move |window, cx| {
                    if owner.read(cx).context.as_ref() != Some(&context) {
                        return;
                    }
                    let message = message.clone();
                    let end = text_end(&message);
                    input.update(cx, |state, cx| {
                        state.set_value(message, window, cx);
                        // 多行输入框 set_value 后光标在开头;接着往下写是常态,挪到末尾
                        // (set_cursor_position 顺带聚焦)
                        state.set_cursor_position(end, window, cx);
                    });
                    // set_value 不发 Change 事件,得自己叫一声重画(字数 / 提示 / 按钮)
                    owner.update(cx, |_, cx| cx.notify());
                })
                .into()
        })
        .collect()
}

/// 一段文本末尾的光标位置(行、列都从 0 数,列按字符)。
fn text_end(text: &str) -> Position {
    let line = text.matches('\n').count();
    let last = text.rsplit('\n').next().unwrap_or_default();
    Position::new(line as u32, last.chars().count() as u32)
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
/// 列表的上 / 下内边距。
const LIST_PAD_TOP: f32 = 4.0;
const LIST_PAD_BOTTOM: f32 = 8.0;
const LIST_PAD_Y: f32 = LIST_PAD_TOP + LIST_PAD_BOTTOM;
/// 分组标题的高(`git_ui::SectionHeader` 的分组档)与上外边距。
const GROUP_HEADER_H: f32 = 28.0;
const GROUP_HEADER_GAP: f32 = 4.0;
/// ▾ 菜单的高度估计(三项、两三项带副标题、一条分隔线;按界面字号缩放)。只用来判断
/// 按钮下方放不放得下、放不下就往上弹,估大了只是离按钮远一点。
const COMMIT_MENU_EST_H: f32 = 160.0;
/// 行右端「状态字母 / hover 时的操作按钮」那一格的宽:两颗图标按钮 + 间隙。
/// 每行都留同样宽,状态字母竖着对齐。
const ROW_ACTIONS_W: f32 = git_ui::ICON_BUTTON * 2.0 + 2.0;
/// 文件行的组名(行内按钮 hover 才出现)。
const ROW_GROUP: &str = "git-file-row";

impl Render for GitChanges {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_placeholder(window, cx);
        // 写操作闸每帧只问一次:行内 / 组操作按钮与提交按钮都按它画
        let gated = self.gated();
        let model = self.model(cx);
        let list = self.render_list(&model, self.ops_enabled_with(gated), cx);
        let commit_area = self.render_commit_area(&model, gated, cx);

        div()
            .size_full()
            .flex()
            .flex_col()
            // Ctrl/Cmd+Enter 提交、Ctrl/Cmd+Alt+Enter 提交并推送(键位绑在 hotkeys.rs,
            // 谓词 `"GitChanges > Input"` —— 与项目切换器的方向键同一套路)
            .key_context(CHANGES_CONTEXT)
            .on_action(cx.listener(Self::on_commit_action))
            .on_action(cx.listener(Self::on_commit_push_action))
            .child(list)
            .child(commit_area)
    }
}

impl GitChanges {
    /// 列表区。有变更时是虚拟列表(只建视口里的行,见模块注释「列表只建看得见的行」);
    /// 加载中 / 取数失败 / 工作区干净三种整块占位仍是普通滚动容器。焦点、键位与滚动
    /// 位置两种形态共用。`enabled` = 整文件操作能不能点(同一帧只问一次闸)。
    fn render_list(
        &mut self,
        model: &Rc<ListModel>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let empty = self.changes.is_empty();
        let placeholder = if self.loading && empty && self.load_error.is_none() {
            Some(git_ui::loading_state(t("gitChanges", "loading")).into_any_element())
        } else if let Some(err) = self.load_error.as_deref().filter(|_| empty) {
            // 取变更失败顶替空态:否则读失败会被误认成「工作区是干净的」
            Some(
                git_ui::error_state(tr!("gitChanges", "loadFailed", detail = err))
                    .into_any_element(),
            )
        } else if empty {
            Some(self.render_clean_state(cx))
        } else {
            None
        };

        let list = div()
            .id("git-changes-list")
            .track_focus(&self.list_focus)
            .key_context(LIST_CONTEXT)
            .on_action(cx.listener(Self::on_select_prev))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_toggle_stage))
            .on_action(cx.listener(Self::on_open_diff))
            .on_action(cx.listener(Self::on_discard_key))
            .flex_1()
            .min_h(px(0.0));
        if let Some(placeholder) = placeholder {
            self.reveal_cursor = false;
            return list
                .overflow_y_scroll()
                .track_scroll(&self.list_scroll)
                .px(px(4.0))
                .pt(px(LIST_PAD_TOP))
                .pb(px(LIST_PAD_BOTTOM))
                .child(placeholder)
                .into_any_element();
        }

        // 游标行要滚进视野:行序按这一帧的模型找,偏移等 prepaint 量到这一帧的视口再算
        // (下面那块 canvas 排在虚拟列表**之前**,先于它 prepaint,改的偏移当帧就生效)
        let reveal = std::mem::take(&mut self.reveal_cursor)
            .then(|| self.cursor_row(model))
            .flatten()
            .and_then(|row| model.row_span(row));
        let scroll = self.list_scroll.clone();
        let rows = model.clone();
        let tree_mode = model.tree_mode;
        list.relative()
            .child(
                canvas(
                    move |bounds: Bounds<Pixels>, _window, _cx| {
                        let Some((top, bottom)) = reveal else {
                            return;
                        };
                        let mut offset = scroll.offset();
                        let viewport = f32::from(bounds.size.height);
                        offset.y = px(reveal_offset(f32::from(offset.y), top, bottom, viewport));
                        scroll.set_offset(offset);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                v_virtual_list(
                    cx.entity(),
                    "git-changes-rows",
                    model.sizes.clone(),
                    move |this, range, _window, cx| {
                        range
                            .filter_map(|ix| rows.rows.get(ix))
                            .map(|row| this.render_row(row, enabled, tree_mode, cx))
                            .collect::<Vec<_>>()
                    },
                )
                .track_scroll(&self.list_scroll)
                // 组件库给 base 挂的是双轴 `overflow_scroll`,关掉横轴,与换掉的
                // `overflow_y_scroll()` 一致(横向内容宽是拿首行量的,首帧量的是 min-content,
                // 会凭空多出一段横向可滚范围 —— 搜索弹窗同一处理)
                .overflow_x_hidden()
                .px(px(4.0))
                .pt(px(LIST_PAD_TOP))
                .pb(px(LIST_PAD_BOTTOM)),
            )
            .into_any_element()
    }

    /// 键盘游标所在的文件行在行序里是第几行(游标所在分组折起来了 / 目录收起了就没有)。
    fn cursor_row(&self, model: &ListModel) -> Option<usize> {
        let (area, path) = self.cursor.as_ref()?;
        let index = model
            .members(*area)
            .iter()
            .copied()
            .find(|&i| self.changes[i].path == *path)?;
        model.file_row(*area, index)
    }

    /// 虚拟列表的一项。每种行画出来的高度与 [`ListRow::height`] 一致,宽度撑满(虚拟列表
    /// 按根布局排每一项,不像普通块容器那样自动撑满)。
    fn render_row(
        &self,
        row: &ListRow,
        enabled: bool,
        tree_mode: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match row {
            ListRow::Header {
                area,
                count,
                collapsed,
            } => self.render_group_header(*area, *count, *collapsed, enabled, cx),
            ListRow::Dir {
                area,
                name,
                full_path,
                depth,
                collapsed,
                file_count,
            } => self.render_dir_row(
                *area,
                name.clone(),
                full_path.clone(),
                *file_count,
                *depth,
                *collapsed,
                cx,
            ),
            ListRow::File { area, index, depth } => {
                self.render_file_row(*index, *area, *depth, tree_mode, enabled, cx)
            }
        }
    }

    /// 工作区干净:32px 勾形图标 + 一句说明 + 「刷新」。
    ///
    /// 上下留白只用 `empty_state` 自带的那一份(24)。此前另叠了 `py(32)`,整块比列表区高,
    /// 「刷新」被挤到可见区外、只露一道顶边(E2E b/s75)。现在面板也会按内容给「更改」区
    /// 让高度(`git_panel::section_split`),这一块整个看得见。
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
        .into_any_element()
    }

    /// 分组标题:折叠箭头 + 标题 + 数量胶囊 + 右对齐的组操作(常显,保证可发现)。
    ///
    /// 上外边距 4 改成外面包一层的上内边距:虚拟列表把每一项当布局根排,根节点的外边距
    /// 靠不住;包一层不可交互的壳,点那 4px 照旧什么都不触发,看上去一模一样。
    fn render_group_header(
        &self,
        area: Area,
        count: usize,
        collapsed: bool,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (title, tone) = match area {
            Area::Conflicts => (t("gitCommit", "conflictsGroup"), ui::color_attention()),
            Area::Staged => (t("panels", "stagedChanges"), ui::text_muted()),
            Area::Unstaged => (t("panels", "unstagedChanges"), ui::text_muted()),
            Area::Untracked => (t("panels", "untrackedFiles"), ui::text_muted()),
        };
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
                        el.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.unstage_all(window, cx);
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
                                let paths = this.area_paths(area, cx);
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
                        el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.stage_group(area, window, cx);
                        }))
                    }),
                );
            }
        }

        let toggle = cx.listener(move |this, _: &ClickEvent, _window, cx| {
            if !this.collapsed_groups.remove(&area) {
                this.collapsed_groups.insert(area);
            }
            this.invalidate_model();
            cx.notify();
        });
        div()
            .w_full()
            .pt(px(GROUP_HEADER_GAP))
            .child(header.build().on_click(toggle))
            .into_any_element()
    }

    /// 目录行(共用的 [`git_ui::tree_dir_row`]:单链压缩后的名称 + 弱化文件数)。
    #[allow(clippy::too_many_arguments)]
    fn render_dir_row(
        &self,
        area: Area,
        name: String,
        full_path: String,
        file_count: usize,
        depth: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = dir_key(area, &full_path);
        git_ui::tree_dir_row(
            SharedString::from(format!("git-dir-{key}")),
            name,
            file_count,
            depth,
            collapsed,
        )
        .w_full()
        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
            if !this.collapsed_dirs.remove(&key) {
                this.collapsed_dirs.insert(key.clone());
            }
            this.invalidate_model();
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
        enabled: bool,
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

        // tooltip:完整路径(重命名带旧路径)+ 状态说明(部分暂存另注一句)。文字等气泡
        // 真要弹出时才拼(上游等满停留时长才调构造闭包),这里只带拼它要的几样
        let tip_old = file
            .old_path
            .clone()
            .filter(|_| matches!(status, Some(GitStatus::Renamed)));
        let tip_path = path.clone();
        let tip_status = status.cloned();
        let tip_partial =
            file.staged_status.is_some() && file.unstaged_status.is_some() && !is_conflicted(file);

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
                        el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            let paths = this.targets(area, index, cx);
                            this.unstage(paths, None, window, cx);
                        }))
                    }),
                );
            }
            Area::Unstaged | Area::Untracked => {
                let stage = icon_button(id_of("stage"), ICON_PLUS, t("panels", "stage"), enabled);
                actions = actions.child(stage.when(enabled, |el| {
                    el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        let paths = this.targets(area, index, cx);
                        this.stage(paths, None, window, cx);
                    }))
                }));
            }
            Area::Conflicts => {}
        }

        git_ui::list_row(row_id, selected)
            .group(ROW_GROUP)
            .pl(px(git_ui::tree_row_pad(depth)))
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
            .tip_with(move |window, cx| {
                let tip = file_row_tip(
                    &tip_path,
                    tip_old.as_deref(),
                    tip_status.as_ref(),
                    tip_partial,
                );
                Tooltip::new(tip).build(window, cx)
            })
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
        let context = self.context.clone();
        let paths = self.targets(area, index, cx);
        let count = paths.len();
        let many = count > 1;
        let enabled = self.ops_enabled();
        let counted = |single: &'static str, multi: &'static str| -> String {
            if many {
                tr!("gitCommit", multi, count = count.to_string())
            } else {
                single.to_string()
            }
        };
        let view_diff = {
            let this = this.clone();
            let context = context.clone();
            menu::item(t("gitChanges", "contextViewDiff"), move |window, cx| {
                this.update(cx, |this, cx| {
                    if this.context == context {
                        this.view_diff(index, area, window, cx);
                    }
                });
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
                    let label = t("gitCommit", take_side_key(side, deleted, self.op_kind));
                    let (this, paths) = (this.clone(), paths.clone());
                    let context = context.clone();
                    let mut item =
                        MenuItem::new(label)
                            .disabled(!enabled)
                            .on_click(move |window, cx| {
                                let paths = paths.clone();
                                this.update(cx, |this, cx| {
                                    if this.context == context {
                                        this.take_side(paths, side, window, cx);
                                    }
                                });
                            });
                    if deleted {
                        item = item.danger();
                    }
                    entries.push(item.into());
                }
                let (this2, paths2) = (this.clone(), paths.clone());
                let context = context.clone();
                entries.push(
                    MenuItem::new(t("gitCommit", "markResolved"))
                        .disabled(!enabled)
                        .on_click(move |window, cx| {
                            let paths = paths2.clone();
                            this2.update(cx, |this, cx| {
                                if this.context == context {
                                    this.mark_resolved(paths, window, cx);
                                }
                            });
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
                let context = context.clone();
                entries.push(
                    MenuItem::new(counted(t("panels", "unstage"), "unstageSelected"))
                        .disabled(!enabled)
                        .on_click(move |window, cx| {
                            let paths = paths.clone();
                            this.update(cx, |this, cx| {
                                if this.context == context {
                                    this.unstage(paths, None, window, cx);
                                }
                            });
                        })
                        .into(),
                );
            }
            Area::Unstaged | Area::Untracked => {
                entries.push(view_diff);
                entries.push(menu::separator());
                {
                    let (this, paths) = (this.clone(), paths.clone());
                    let context = context.clone();
                    entries.push(
                        MenuItem::new(counted(t("panels", "stage"), "stageSelected"))
                            .disabled(!enabled)
                            .on_click(move |window, cx| {
                                let paths = paths.clone();
                                this.update(cx, |this, cx| {
                                    if this.context == context {
                                        this.stage(paths, None, window, cx);
                                    }
                                });
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
                            this.update(cx, |this, cx| {
                                if this.context == context {
                                    this.discard(area, paths, window, cx);
                                }
                            });
                        })
                        .into(),
                );
            }
        }
        // 单文件历史(IDEA Show History)。未跟踪文件还没有历史,多选时没有「那一个文件」
        if !many
            && area != Area::Untracked
            && let Some(file) = self.changes.get(index)
            && let Some(repo) = self.context.clone()
        {
            let (store, repo, rel) = (self.store.clone(), repo, file.path.clone());
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
    fn render_commit_area(
        &self,
        model: &ListModel,
        gated: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let message = self.commit_input.read(cx).value().to_string();
        let hints = message_hints(&message);
        let blocker = self.commit_blocker_with(false, gated, model, cx);
        let can_commit = blocker.is_none();
        let stage_first_ok = self.commit_blocker_with(true, gated, model, cx).is_none();
        let staged_count = model.members(Area::Staged).len();

        let commit_label = if self.committing {
            t("gitChanges", "committing").to_string()
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
        let history_enabled = !self.history_loading && self.context.is_some();
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
        let menu_enabled = !self.committing;
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
                        let position = this.commit_menu_anchor(event.position(), window);
                        menu::show(position, entries, window, cx);
                    }))
            });
        let buttons_bounds = self.commit_buttons.clone();
        let buttons = div()
            .relative()
            .mt(px(8.0))
            .flex()
            .rounded(px(4.0))
            .overflow_hidden()
            .child(main_button)
            .child(split)
            // 记下这一行在窗口里的位置(▾ 菜单的锚点,见 `commit_menu_anchor`)
            .child(
                canvas(
                    move |bounds: Bounds<Pixels>, _window, _cx| buttons_bounds.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            );

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

        let area_h = self.commit_area_h.clone();
        div()
            .relative()
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
            // 量提交区整块的高度(面板按内容分高度时要,见 `natural_height`)
            .child(
                canvas(
                    move |bounds: Bounds<Pixels>, _window, _cx| {
                        area_h.set(f32::from(bounds.size.height));
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .into_any_element()
    }

    /// ▾ 菜单的左上角:贴着提交按钮那一行的下沿弹出(与整行左对齐);下面放不下就整块
    /// 弹到上沿之上。菜单基件只会把越出视口的面板**平移**回来 —— 在下沿弹、被推回来时
    /// 正好压住按钮下半截(E2E b/s60),所以朝上弹要在这里先算好。还没画过按钮(量不到)
    /// 时退回鼠标点。
    fn commit_menu_anchor(&self, fallback: Point<Pixels>, window: &Window) -> Point<Pixels> {
        let Some(row) = self.commit_buttons.get() else {
            return fallback;
        };
        let gap = px(4.0);
        let estimate = ui::font_px(COMMIT_MENU_EST_H);
        if row.bottom() + gap + estimate <= window.viewport_size().height {
            point(row.left(), row.bottom() + gap)
        } else {
            point(
                row.left(),
                px(f32::from(row.top() - gap - estimate).max(0.0)),
            )
        }
    }

    /// ▾ 菜单:提交并推送…(提交后打开推送对话框)/ 全部暂存并提交 /(分隔)贮藏当前改动…。
    fn commit_menu(
        &self,
        can_commit: bool,
        stage_first_ok: bool,
        cx: &mut Context<Self>,
    ) -> Vec<menu::MenuEntry> {
        let this = cx.entity();
        let context = self.context.clone();
        let push = {
            let this = this.clone();
            let context = context.clone();
            MenuItem::new(t("gitCommit", "commitAndPush"))
                .detail(t("gitCommit", "commitAndPushDetail"))
                .shortcut(menu::hotkey_label(true, false, true, "Enter"))
                .disabled(!can_commit)
                .on_click(move |window, cx| {
                    this.update(cx, |this, cx| {
                        if this.context == context {
                            this.commit(true, false, window, cx);
                        }
                    });
                })
        };
        let stage_all = {
            let this = this.clone();
            let context = context.clone();
            MenuItem::new(t("gitCommit", "stageAllAndCommit"))
                .detail(t("gitCommit", "stageAllAndCommitDetail"))
                .disabled(!stage_first_ok)
                .on_click(move |window, cx| {
                    this.update(cx, |this, cx| {
                        if this.context == context {
                            this.commit(false, true, window, cx);
                        }
                    });
                })
        };
        // 贮藏:有改动(含未跟踪)才可用;没有时禁用并写明原因
        let has_changes = !self.changes.is_empty();
        let mut stash = MenuItem::new(t("gitStash", "stashChanges"))
            .disabled(!has_changes)
            .on_click(move |window, cx| {
                this.update(cx, |this, cx| {
                    if this.context == context {
                        this.open_stash_save(window, cx);
                    }
                });
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
        let Some(repo) = self.context.clone() else {
            return;
        };
        let this = cx.entity().downgrade();
        let context = repo.clone();
        let on_changed: Rc<dyn Fn(&mut App)> = Rc::new(move |cx: &mut App| {
            let _ = this.update(cx, |this, cx| {
                if this.context.as_ref() == Some(&context) {
                    cx.emit(GitChangesEvent::Stashed);
                }
            });
        });
        crate::git_stash::open_save(self.store.clone(), repo, on_changed, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 变基中「采用 ours / theirs」换成写清方向的文案(ours = 变基目标,theirs = 你的
    /// 改动);合并 / 拣选 / 还原 / 没有进行中的操作时维持「我方 / 对方」。
    #[test]
    fn 变基时采用一侧的文案写清方向() {
        use ConflictSide::{Ours, Theirs};
        for op in [
            None,
            Some(RepoOpKind::Merge),
            Some(RepoOpKind::CherryPick),
            Some(RepoOpKind::Revert),
        ] {
            assert_eq!(take_side_key(Ours, false, op), "takeOurs");
            assert_eq!(take_side_key(Theirs, true, op), "takeTheirsDeleted");
        }
        let rebase = Some(RepoOpKind::Rebase);
        assert_eq!(take_side_key(Ours, false, rebase), "takeOursRebase");
        assert_eq!(take_side_key(Ours, true, rebase), "takeOursRebaseDeleted");
        assert_eq!(take_side_key(Theirs, false, rebase), "takeTheirsRebase");
        assert_eq!(
            take_side_key(Theirs, true, rebase),
            "takeTheirsRebaseDeleted"
        );
        // 文案里真写着方向:theirs 那条点明是「你的改动」
        let theirs = mt_i18n::t_in(mt_i18n::Locale::Zh, "gitCommit", "takeTheirsRebase");
        assert!(theirs.contains("你的改动"), "{theirs}");
    }

    /// amend 的「已推送」判定按引用签名缓存:键没变才沿用,读不到当作变了。
    #[test]
    fn amend_信息按引用签名缓存() {
        assert!(!amend_stale(Some(7), Some(7)), "HEAD 与远程跟踪分支都没动");
        assert!(amend_stale(Some(7), Some(8)), "提交 / 推送 / fetch 之后");
        assert!(amend_stale(None, Some(7)), "还没取过");
        assert!(amend_stale(Some(7), None), "签名读不到时宁可重取");
        assert!(amend_stale(None, None));
    }

    /// 真仓库上走一遍:第一次取到信息,键不变时不重取,提交之后重取。
    #[test]
    fn amend_信息键不变不重取() {
        let root = std::env::temp_dir().join(format!(
            "mini-term-amend-key-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@e"])
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git 起不来");
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "first"]);

        let context = GitContext::local(root.to_string_lossy().into_owned());
        let (key, info) = fetch_amend(&context, None).expect("还没取过:要取");
        assert!(key.is_some());
        assert_eq!(
            info.map(|i| i.message.trim().to_string()).as_deref(),
            Some("first")
        );
        assert!(fetch_amend(&context, key).is_none(), "键没变:不重取");

        std::fs::write(root.join("a.txt"), "b\n").unwrap();
        git(&["commit", "-q", "-am", "second"]);
        let (new_key, info) = fetch_amend(&context, key).expect("HEAD 动了:重取");
        assert_ne!(new_key, key);
        assert_eq!(
            info.map(|i| i.message.trim().to_string()).as_deref(),
            Some("second")
        );
        std::fs::remove_dir_all(&root).ok();
    }

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

    /// 列表高度按眼下的数据算(E2E r4 A12:外部改动多出「未暂存」组的那一帧,按上一帧量的
    /// 高度分区,「未跟踪」组被挤出可见区)。分组标题 28 + 上边距 4、行 28、内边距 12;
    /// 折起来的分组只算标题;树形视图把目录行也算上;没有变更时说不准。
    #[test]
    fn 列表高度按数据算() {
        let none_dirs = HashSet::new();
        let none_groups = HashSet::new();
        let height = |changes: &[ChangeFileStatus], groups: &HashSet<Area>, tree: bool| {
            ListModel::build(changes, groups, &none_dirs, tree).height
        };
        let untracked = vec![change("src/new.ts", None, Some(GitStatus::Untracked))];
        assert_eq!(
            height(&untracked, &none_groups, false),
            Some(12.0 + 32.0 + 28.0)
        );
        // 多出一组未暂存:两组标题 + 两行
        let mut both = untracked.clone();
        both.insert(0, change("src/api.ts", None, Some(GitStatus::Modified)));
        assert_eq!(
            height(&both, &none_groups, false),
            Some(12.0 + 2.0 * 32.0 + 2.0 * 28.0)
        );
        // 折起来的分组只算标题
        let folded: HashSet<Area> = [Area::Untracked].into_iter().collect();
        assert_eq!(
            height(&both, &folded, false),
            Some(12.0 + 2.0 * 32.0 + 28.0)
        );
        // 树形:每组多一行目录 src/
        assert_eq!(
            height(&both, &none_groups, true),
            Some(12.0 + 2.0 * 32.0 + 4.0 * 28.0)
        );
        assert_eq!(height(&[], &none_groups, false), None);
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

    /// 折叠 key 带区名:同一路径在各个区各自独立折叠。
    #[test]
    fn 折叠_key_按区隔离() {
        let paths = [(0, "src/a.rs"), (1, "src/b.rs")];
        let mut collapsed = HashSet::new();
        collapsed.insert("staged:src".to_string());

        let rows = area_tree_rows(&paths, Area::Staged, &collapsed);
        assert_eq!(rows.len(), 1, "staged 区的 src 折起来了,只剩目录行");

        let rows = area_tree_rows(&paths, Area::Unstaged, &collapsed);
        assert_eq!(rows.len(), 3, "unstaged 区不受影响");
        assert!(matches!(rows[1], ChangeTreeRow::File { depth: 1, .. }));
    }

    /// 树形视图的行序(渲染、键盘、Shift 连选、diff 弹窗前后换文件都按它):目录在前、
    /// 单链目录压缩成一行;键盘走的文件顺序与画出来的行序一致。
    #[test]
    fn 树形行序与键盘顺序一致() {
        let paths = [
            (0, "z.md"),
            (1, "src/ui/panel/a.rs"),
            (2, "src/ui/panel/b.rs"),
            (3, "docs/x.md"),
        ];
        let rows = area_tree_rows(&paths, Area::Unstaged, &HashSet::new());
        let dirs: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                ChangeTreeRow::Dir { name, .. } => Some(name.as_str()),
                ChangeTreeRow::File { .. } => None,
            })
            .collect();
        assert_eq!(dirs, ["docs", "src/ui/panel"], "src/ui/panel 压缩成一行");
        assert_eq!(git_ui::row_file_order(&rows), [3, 1, 2, 0]);
    }

    // ── 派生模型与换掉的逐帧现算逐行对账 ──
    //
    // 下面几个 `old_*` 是换成派生模型之前 render / `list_height` / `area_order` /
    // `nav_rows` 的原样实现,留在测试里当对照:模型的行序、高度、显示顺序、键盘顺序
    // 必须与它们逐项一致(界面零变化的判据)。

    fn old_tree_rows(
        paths: &[(usize, &str)],
        area: Area,
        collapsed: &HashSet<String>,
    ) -> Vec<ChangeTreeRow> {
        git_ui::flatten_change_tree(&git_ui::build_change_tree(paths), &|full_path| {
            collapsed.contains(&dir_key(area, full_path))
        })
    }

    fn old_group_indices(changes: &[ChangeFileStatus], area: Area) -> Vec<usize> {
        changes
            .iter()
            .enumerate()
            .filter(|(_, c)| in_area(c, area))
            .map(|(i, _)| i)
            .collect()
    }

    fn old_group_paths(changes: &[ChangeFileStatus], area: Area) -> Vec<(usize, &str)> {
        old_group_indices(changes, area)
            .into_iter()
            .map(|i| (i, changes[i].path.as_str()))
            .collect()
    }

    fn old_list_height(
        changes: &[ChangeFileStatus],
        collapsed_groups: &HashSet<Area>,
        collapsed_dirs: &HashSet<String>,
        tree_mode: bool,
    ) -> Option<f32> {
        if changes.is_empty() {
            return None;
        }
        let mut height = LIST_PAD_Y;
        for area in Area::ORDER {
            let paths = old_group_paths(changes, area);
            if paths.is_empty() {
                continue;
            }
            height += GROUP_HEADER_GAP + GROUP_HEADER_H;
            if collapsed_groups.contains(&area) {
                continue;
            }
            let rows = if tree_mode {
                old_tree_rows(&paths, area, collapsed_dirs).len()
            } else {
                paths.len()
            };
            height += rows as f32 * ROW_HEIGHT;
        }
        Some(height)
    }

    fn old_area_order(
        changes: &[ChangeFileStatus],
        area: Area,
        collapsed_dirs: &HashSet<String>,
        tree_mode: bool,
    ) -> Vec<usize> {
        if !tree_mode {
            return old_group_indices(changes, area);
        }
        let paths = old_group_paths(changes, area);
        git_ui::row_file_order(&old_tree_rows(&paths, area, collapsed_dirs))
    }

    fn old_nav_rows(
        changes: &[ChangeFileStatus],
        collapsed_groups: &HashSet<Area>,
        collapsed_dirs: &HashSet<String>,
        tree_mode: bool,
    ) -> Vec<(Area, usize)> {
        Area::ORDER
            .into_iter()
            .filter(|area| !collapsed_groups.contains(area))
            .flat_map(|area| {
                old_area_order(changes, area, collapsed_dirs, tree_mode)
                    .into_iter()
                    .map(move |i| (area, i))
            })
            .collect()
    }

    /// 旧 `render_list` 往滚动容器里 push 的子元素序列(换成本模块的行类型)。
    fn old_render_rows(
        changes: &[ChangeFileStatus],
        collapsed_groups: &HashSet<Area>,
        collapsed_dirs: &HashSet<String>,
        tree_mode: bool,
    ) -> Vec<ListRow> {
        let mut rows = Vec::new();
        for area in Area::ORDER {
            let indices = old_group_indices(changes, area);
            if indices.is_empty() {
                continue;
            }
            let collapsed = collapsed_groups.contains(&area);
            rows.push(ListRow::Header {
                area,
                count: indices.len(),
                collapsed,
            });
            if collapsed {
                continue;
            }
            if tree_mode {
                let paths = old_group_paths(changes, area);
                for row in old_tree_rows(&paths, area, collapsed_dirs) {
                    rows.push(match row {
                        ChangeTreeRow::Dir {
                            name,
                            full_path,
                            depth,
                            collapsed,
                            file_count,
                        } => ListRow::Dir {
                            area,
                            name,
                            full_path,
                            depth,
                            collapsed,
                            file_count,
                        },
                        ChangeTreeRow::File { index, depth } => {
                            ListRow::File { area, index, depth }
                        }
                    });
                }
            } else {
                rows.extend(indices.into_iter().map(|index| ListRow::File {
                    area,
                    index,
                    depth: 0,
                }));
            }
        }
        rows
    }

    /// 测试用的确定性伪随机数(不引依赖)。
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }

        fn pick(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// 一份随机的变更列表:路径不重复(git 给的就不重复),目录名混大小写、文件与目录
    /// 同名,状态覆盖部分暂存 / 冲突 / 未跟踪 / 重命名。
    fn random_changes(rng: &mut Lcg, count: usize) -> Vec<ChangeFileStatus> {
        const DIRS: [&str; 7] = ["src", "Src", "ui", "UI", "a", "docs", "x"];
        const FILES: [&str; 7] = ["a.rs", "A.rs", "b.md", "x", "ui", "src", "Z.txt"];
        const STATUS: [(Option<GitStatus>, Option<GitStatus>); 8] = [
            (Some(GitStatus::Added), None),
            (Some(GitStatus::Modified), Some(GitStatus::Modified)),
            (None, Some(GitStatus::Modified)),
            (None, Some(GitStatus::Untracked)),
            (Some(GitStatus::Conflicted), Some(GitStatus::Conflicted)),
            (Some(GitStatus::Deleted), None),
            (Some(GitStatus::Renamed), Some(GitStatus::Deleted)),
            (None, Some(GitStatus::Deleted)),
        ];
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        while out.len() < count {
            let depth = rng.pick(4);
            let mut path: Vec<&str> = (0..depth).map(|_| DIRS[rng.pick(DIRS.len())]).collect();
            path.push(FILES[rng.pick(FILES.len())]);
            let path = path.join("/");
            if !seen.insert(path.clone()) {
                // 组合空间不大,撞了就换一个(数量给得小,不会死循环)
                continue;
            }
            let (staged, unstaged) = STATUS[rng.pick(STATUS.len())].clone();
            out.push(change(&path, staged, unstaged));
        }
        out
    }

    /// 某份变更列表里出现过的目录(全部前缀),随机挑一部分按区名拼成折叠 key。
    fn random_collapsed_dirs(rng: &mut Lcg, changes: &[ChangeFileStatus]) -> HashSet<String> {
        let mut dirs: Vec<String> = Vec::new();
        for file in changes {
            let parts: Vec<&str> = file.path.split('/').collect();
            for end in 1..parts.len() {
                dirs.push(parts[..end].join("/"));
            }
        }
        let mut out = HashSet::new();
        if dirs.is_empty() {
            return out;
        }
        for _ in 0..rng.pick(4) {
            let area = Area::ORDER[rng.pick(4)];
            out.insert(dir_key(area, &dirs[rng.pick(dirs.len())]));
        }
        out
    }

    /// 派生模型与旧的逐帧现算逐项一致:行序(连标题 / 目录行的每个字段)、摊开总高、
    /// 四组成员与显示顺序、键盘顺序;尺寸表与行一一对应。随机数据覆盖两种视图、分组
    /// 折叠、目录折叠(压缩节点折叠 key 是最深那一级的完整路径)。
    #[test]
    fn 派生模型与逐帧现算逐行一致() {
        let mut rng = Lcg(0x5eed);
        for round in 0..300 {
            let count = rng.pick(40);
            let changes = random_changes(&mut rng, count);
            let groups: HashSet<Area> = Area::ORDER
                .into_iter()
                .filter(|_| rng.pick(4) == 0)
                .collect();
            let dirs = random_collapsed_dirs(&mut rng, &changes);
            for tree in [false, true] {
                let model = ListModel::build(&changes, &groups, &dirs, tree);
                let ctx = format!("round {round} tree {tree} groups {groups:?} dirs {dirs:?}");
                assert_eq!(
                    model.rows,
                    old_render_rows(&changes, &groups, &dirs, tree),
                    "{ctx}"
                );
                assert_eq!(
                    model.height,
                    old_list_height(&changes, &groups, &dirs, tree),
                    "{ctx}"
                );
                for area in Area::ORDER {
                    assert_eq!(
                        model.members(area),
                        old_group_indices(&changes, area),
                        "{ctx}"
                    );
                    assert_eq!(
                        model.order(area),
                        old_area_order(&changes, area, &dirs, tree),
                        "{ctx} {area:?}"
                    );
                }
                assert_eq!(
                    model.nav().collect::<Vec<_>>(),
                    old_nav_rows(&changes, &groups, &dirs, tree),
                    "{ctx}"
                );
                assert_eq!(model.sizes.len(), model.rows.len());
                for (size, row) in model.sizes.iter().zip(&model.rows) {
                    assert_eq!(f32::from(size.height), row.height());
                }
            }
        }
    }

    /// 键盘导航的下标映射:文件行 → 行号,行号 → 上下沿(从列表顶边量,含上内边距);
    /// 折起来的分组里的文件没有行。
    #[test]
    fn 文件行的行号与上下沿() {
        let changes = vec![
            change("src/a.rs", Some(GitStatus::Added), None),
            change("src/b.rs", None, Some(GitStatus::Modified)),
            change("new.ts", None, Some(GitStatus::Untracked)),
        ];
        let (no_groups, no_dirs) = (HashSet::new(), HashSet::new());
        let model = ListModel::build(&changes, &no_groups, &no_dirs, false);
        // 已暂存标题 / a / 未暂存标题 / b / 未跟踪标题 / new
        assert_eq!(model.file_row(Area::Staged, 0), Some(1));
        assert_eq!(model.file_row(Area::Unstaged, 1), Some(3));
        assert_eq!(model.file_row(Area::Untracked, 2), Some(5));
        assert_eq!(model.file_row(Area::Staged, 1), None, "b 不在已暂存组");
        assert_eq!(model.row_span(0), Some((4.0, 36.0)), "标题连上外边距 32");
        assert_eq!(model.row_span(1), Some((36.0, 64.0)));
        let last_top = 4.0 + 3.0 * 32.0 + 2.0 * 28.0;
        assert_eq!(model.row_span(5), Some((last_top, 184.0)));
        assert_eq!(model.row_span(6), None);
        // 摊开总高 = 上内边距 + 全部行 + 下内边距,最后一行的下沿再加 8
        assert_eq!(model.height, Some(184.0 + 8.0));

        let folded: HashSet<Area> = [Area::Unstaged].into_iter().collect();
        let model = ListModel::build(&changes, &folded, &no_dirs, false);
        assert_eq!(
            model.file_row(Area::Unstaged, 1),
            None,
            "折起来的分组没有文件行"
        );
        assert_eq!(model.file_row(Area::Untracked, 2), Some(4));
        assert_eq!(model.order(Area::Unstaged), [1], "显示顺序不看分组折叠");
        assert_eq!(
            model.nav().collect::<Vec<_>>(),
            [(Area::Staged, 0), (Area::Untracked, 2)],
            "键盘跳过折起来的分组"
        );
    }

    /// 滚进视野的偏移与 gpui `scroll_to_item` 的默认策略一致:整行可见不动、上面露不全
    /// 顶对齐、下面露不全底对齐、比视口高的行顶对齐;上下沿按整个列表框比。
    #[test]
    fn 滚进视野只滚最少的量() {
        // 视口 100,行在内容里 [40, 68)
        let reveal = |offset: f32, top: f32, viewport: f32| {
            reveal_offset(offset, top, top + ROW_HEIGHT, viewport)
        };
        assert_eq!(reveal(0.0, 40.0, 100.0), 0.0, "整行可见");
        assert_eq!(reveal(-50.0, 40.0, 100.0), -40.0, "在上面:顶对齐");
        assert_eq!(reveal(0.0, 120.0, 100.0), -48.0, "在下面:底对齐");
        assert_eq!(reveal(-10.0, 4.0, 100.0), -4.0, "露了半截也算");
        assert_eq!(reveal(-30.0, 40.0, 20.0), -40.0, "比视口高:顶对齐");
    }

    fn snapshot(changes: Vec<ChangeFileStatus>) -> Snapshot {
        Snapshot {
            changes,
            conflicts: Vec::new(),
            op_kind: None,
            target: Some(CommitTarget::Branch("main".into())),
            has_head: true,
        }
    }

    fn landed_from(snap: &Snapshot) -> Landed<'_> {
        Landed {
            changes: &snap.changes,
            conflicts: &snap.conflicts,
            op_kind: snap.op_kind,
            target: snap.target.as_ref(),
            has_head: snap.has_head,
            amend: false,
            load_error: None,
        }
    }

    /// 落地去重:一模一样的回包不算变;任何一个会进界面(或菜单)的字段变了都算。
    #[test]
    fn 取数落地没变就不重画() {
        let files = vec![
            change("a.rs", Some(GitStatus::Modified), None),
            change("b.rs", None, Some(GitStatus::Untracked)),
        ];
        let base = snapshot(files.clone());
        let landed = landed_from(&base);
        assert!(!landed.differs(&snapshot(files.clone())), "同一份数据");

        let mut moved = files.clone();
        moved[0].unstaged_status = Some(GitStatus::Modified);
        assert!(landed.differs(&snapshot(moved)), "状态变了");
        let mut renamed = files.clone();
        renamed[1].old_path = Some("old.rs".into());
        assert!(landed.differs(&snapshot(renamed)), "旧路径变了");
        assert!(landed.differs(&snapshot(files[..1].to_vec())), "少了一个");

        let mut next = snapshot(files.clone());
        next.conflicts = vec![ConflictInfo {
            path: "a.rs".into(),
            ours: true,
            theirs: false,
        }];
        assert!(landed.differs(&next), "冲突细节变了(菜单文案跟着它)");
        let mut next = snapshot(files.clone());
        next.op_kind = Some(RepoOpKind::Rebase);
        assert!(landed.differs(&next), "进行中的操作变了");
        let mut next = snapshot(files.clone());
        next.target = Some(CommitTarget::Branch("dev".into()));
        assert!(landed.differs(&next), "提交目标(占位符)变了");
        let mut next = snapshot(files.clone());
        next.has_head = false;
        assert!(landed.differs(&next), "HEAD 没了");

        let errored = Landed {
            load_error: Some("boom"),
            ..landed_from(&base)
        };
        assert!(errored.differs(&snapshot(files)), "上次失败、这次成功");
    }

    /// 勾着 amend 时回包没变就不算变(「已推送」判定另走一趟);HEAD 没了 amend 收起算变。
    #[test]
    fn 取数落地的_amend_口径() {
        let base = snapshot(Vec::new());
        let amending = Landed {
            amend: true,
            ..landed_from(&base)
        };
        let mut next = snapshot(Vec::new());
        assert!(!amending.differs(&next), "数据没变");
        next.has_head = false;
        assert!(amending.differs(&next), "HEAD 没了:amend 收起");
        assert!(
            !landed_from(&base).differs(&snapshot(Vec::new())),
            "没勾 amend"
        );
    }

    /// 取数失败:同一条错误反复落地不算变;换了原因、或此前还有数据都算。
    #[test]
    fn 取数失败的落地去重() {
        let empty = snapshot(Vec::new());
        let failing = Landed {
            load_error: Some("boom"),
            target: None,
            ..landed_from(&empty)
        };
        assert!(!failing.differs_on_error("boom"));
        assert!(failing.differs_on_error("bang"));
        let full = snapshot(vec![change("a.rs", None, Some(GitStatus::Modified))]);
        assert!(landed_from(&full).differs_on_error("boom"), "列表要清空");
    }

    /// 行 tooltip 现拼的文字与此前每帧预拼的一致:重命名带旧路径,部分暂存另注一句。
    #[test]
    fn 文件行提示文字() {
        let text = |status: Option<&GitStatus>| status_text(status).to_string();
        assert_eq!(
            file_row_tip("src/a.rs", None, Some(&GitStatus::Modified), false),
            format!("src/a.rs\n{}", text(Some(&GitStatus::Modified)))
        );
        assert_eq!(
            file_row_tip("b.rs", Some("a.rs"), Some(&GitStatus::Renamed), false),
            format!("a.rs → b.rs\n{}", text(Some(&GitStatus::Renamed)))
        );
        assert_eq!(
            file_row_tip("c.rs", None, Some(&GitStatus::Modified), true),
            format!(
                "c.rs\n{} · {}",
                text(Some(&GitStatus::Modified)),
                t("gitCommit", "partiallyStaged")
            )
        );
    }
}
