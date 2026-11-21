use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Area {
    Conflicts,
    Staged,
    Unstaged,
    Untracked,
}

impl Area {
    /// 渲染 / 键盘导航的分组顺序:冲突置顶。
    pub(super) const ORDER: [Area; 4] = [
        Area::Conflicts,
        Area::Staged,
        Area::Unstaged,
        Area::Untracked,
    ];

    pub(super) fn key(self) -> &'static str {
        match self {
            Area::Conflicts => "conflicts",
            Area::Staged => "staged",
            Area::Unstaged => "unstaged",
            Area::Untracked => "untracked",
        }
    }

    /// 在 [`Self::ORDER`] 里的位置(派生模型按它摆四组的数组)。
    pub(super) fn slot(self) -> usize {
        match self {
            Area::Conflicts => 0,
            Area::Staged => 1,
            Area::Unstaged => 2,
            Area::Untracked => 3,
        }
    }
}

pub(super) fn is_conflicted(file: &ChangeFileStatus) -> bool {
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
pub(super) fn status_text(status: Option<&GitStatus>) -> &'static str {
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
pub(super) fn file_row_tip(
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
pub(super) fn dir_key(area: Area, full_path: &str) -> String {
    format!("{}:{}", area.key(), full_path)
}

/// 某个分组的树形行(界面行序;收起的目录不往下展开)。
pub(super) fn area_tree_rows(
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
pub(super) enum ListRow {
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
    pub(super) fn height(&self) -> f32 {
        match self {
            ListRow::Header { .. } => GROUP_HEADER_GAP + GROUP_HEADER_H,
            ListRow::Dir { .. } | ListRow::File { .. } => ROW_HEIGHT,
        }
    }
}

/// 「更改」列表的派生模型(见模块注释「列表只建看得见的行」):按 `changes` + 两种折叠态
/// + 视图模式算一份,存在 [`GitChanges::model`] 的缓存里。
#[derive(Debug)]
pub(super) struct ListModel {
    /// 按哪种视图算的(与 store 里的视图模式对不上就作废重算)
    pub(super) tree_mode: bool,
    /// 四个分组各自的文件下标(`changes` 里的原顺序),按 [`Area::slot`] 摆
    pub(super) members: [Vec<usize>; 4],
    /// 四个分组的文件**显示顺序**:列表视图同 `members`;树形视图按树走,收起的目录里
    /// 的文件不算。**不看分组折叠** —— 折起来的分组照样能 Shift 连选、diff 页签照样能
    /// 前后换文件(与此前现算的 `area_order` 同一口径)
    pub(super) order: [Vec<usize>; 4],
    /// 界面上的行序:每个非空分组一个标题,展开的分组接着它的文件 / 目录行
    pub(super) rows: Vec<ListRow>,
    /// 每行的尺寸(虚拟列表的 `item_sizes`,只用高)。随模型缓存,同一份数据跨帧是同一个
    /// `Rc`,虚拟列表比一下就知道不用重算前缀和
    pub(super) sizes: Rc<Vec<Size<Pixels>>>,
    /// 列表摊开要多高:上下内边距 + 全部行。没有变更(加载中 / 失败 / 空态整块占位,
    /// 高度不定)为 `None`
    pub(super) height: Option<f32>,
}

impl ListModel {
    pub(super) fn build(
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
    pub(super) fn members(&self, area: Area) -> &[usize] {
        &self.members[area.slot()]
    }

    /// 某个分组里文件的显示顺序(见字段注释)。
    pub(super) fn order(&self, area: Area) -> &[usize] {
        &self.order[area.slot()]
    }

    /// 键盘导航走的全部可见文件行(行序里的文件行:折起来的分组、收起的目录跳过)。
    pub(super) fn nav(&self) -> impl Iterator<Item = (Area, usize)> + '_ {
        self.rows.iter().filter_map(|row| match *row {
            ListRow::File { area, index, .. } => Some((area, index)),
            _ => None,
        })
    }

    /// 某个文件行在行序里是第几行。
    pub(super) fn file_row(&self, area: Area, index: usize) -> Option<usize> {
        self.rows.iter().position(|row| {
            matches!(*row, ListRow::File { area: a, index: i, .. } if a == area && i == index)
        })
    }

    /// 第 `row` 行在列表内容里的上下沿(从列表顶边量,含上内边距)。
    pub(super) fn row_span(&self, row: usize) -> Option<(f32, f32)> {
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
pub(super) fn reveal_offset(offset_y: f32, top: f32, bottom: f32, viewport: f32) -> f32 {
    if bottom - top > viewport || top + offset_y < 0.0 {
        -top
    } else if bottom + offset_y > viewport {
        viewport - bottom
    } else {
        offset_y
    }
}
pub(super) fn same_changes(a: &[ChangeFileStatus], b: &[ChangeFileStatus]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.path == y.path
                && x.old_path == y.old_path
                && x.staged_status == y.staged_status
                && x.unstaged_status == y.unstaged_status
                && x.status_label == y.status_label
        })
}
