use super::*;

// ─── 提交 diff(某次 commit / 一组提交合并后的多文件,原版 CommitDiffModal) ────

// 左栏文件列表的状态字母表(`CommitDiffModal.tsx:21-26`)收在
// `git_ui::commit_file_status`:提交详情与单文件历史用的是同一张表。

/// 提交 diff 页签左栏的一个文件。单提交与合并变更两种来源在这里拍成同一种形状,
/// 页签正文只有一份(见 [`pages`] 的 `FilesPage`)。
#[derive(Clone)]
pub(super) struct ModalFile {
    pub(super) path: String,
    pub(super) status: String,
    /// 重命名的旧路径(tooltip 写 `old → new`;单提交取 diff 时要用)。
    pub(super) old_path: Option<String>,
    pub(super) source: ModalFileSource,
}

/// 右栏的 diff 从哪儿取。
#[derive(Clone)]
pub(super) enum ModalFileSource {
    /// 单提交:该提交相对第一父([`mt_project::git::get_commit_file_diff`])。
    Commit(String),
    /// 合并变更:两个版本之间([`mt_project::git::get_revision_file_diff`])。
    Revisions {
        base: Option<FileRevision>,
        head: Option<FileRevision>,
    },
}

impl ModalFile {
    pub(super) fn from_commit(hash: &str, file: CommitFileInfo) -> Self {
        Self {
            path: file.path,
            status: file.status,
            old_path: file.old_path,
            source: ModalFileSource::Commit(hash.to_string()),
        }
    }

    pub(super) fn from_combined(file: CombinedFileChange) -> Self {
        Self {
            path: file.path,
            status: file.status,
            old_path: file.old_path,
            source: ModalFileSource::Revisions {
                base: file.base,
                head: file.head,
            },
        }
    }
}

/// 左栏头部:标题 + 一行等宽的副标题(单提交是短哈希,合并变更是「最新 … 最旧」)。
pub(super) struct ModalHeading {
    pub(super) title: String,
    pub(super) subtitle: String,
    /// 副标题的 tooltip(合并变更写合并口径)。
    pub(super) subtitle_tip: Option<&'static str>,
    /// 一个文件都没有时右栏的说明。
    pub(super) empty: &'static str,
}

/// 左栏选中项与视图状态。放进实体:行回调、↑ / ↓ 拿着它的句柄改选中项。
pub(super) struct CommitPick {
    /// 选中第几个文件。按下标认而不是按路径:合并变更在极端的非连续选择下可能出现
    /// 同一路径的两条(见 `mt_project::git::combined` 的模块注释)。
    pub(super) selected: usize,
    /// 树形 / 平铺(打开时取配置 `gitCommitFilesViewMode`,点切换按钮时一并写回)。
    pub(super) tree: bool,
    /// 树形里收起的目录(完整路径)。
    pub(super) collapsed: HashSet<String>,
    /// 左栏拍平后的行(见 [`Self::rows`])。切视图、收起 / 展开目录时作废。
    pub(super) rows: Option<Rc<Vec<ChangeTreeRow>>>,
    /// 目录行的折叠箭头:只让刚点的那一行转(见 `fold_motion`)。
    pub(super) chevrons: crate::fold_motion::FlipTweens,
}

impl CommitPick {
    /// 左栏的行。文件列表在页签的一生里不变,行只随视图与收起的目录变,所以缓存着:
    /// 页签每帧都重画(右栏滚动、拖选),大提交几千个文件、合并查看两百个提交时不能每帧
    /// 重新建一遍树。选中项不在行里(画的时候才比),换选中不必重算。
    pub(super) fn rows(&mut self, files: &[ModalFile]) -> Rc<Vec<ChangeTreeRow>> {
        self.rows
            .get_or_insert_with(|| Rc::new(modal_rows(files, self.tree, &self.collapsed)))
            .clone()
    }

    pub(super) fn set_tree(&mut self, tree: bool) {
        self.tree = tree;
        self.rows = None;
    }

    /// 收起 / 展开一个目录。
    pub(super) fn toggle_dir(&mut self, full_path: &str) {
        let open = self.collapsed.remove(full_path);
        if !open {
            self.collapsed.insert(full_path.to_string());
        }
        self.chevrons
            .flip_open(full_path, open, crate::fold_motion::OPEN_TURNS);
        self.rows = None;
    }
}

impl Render for CommitPick {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 单提交的左栏头部:标题是提交信息,副标题是短哈希。
pub(super) fn commit_heading(hash: &str, message: String) -> ModalHeading {
    ModalHeading {
        title: message,
        subtitle: hash.chars().take(7).collect(),
        subtitle_tip: None,
        empty: t("commitDiff", "noChanges"),
    }
}

/// 合并变更的左栏头部:标题写明「N 个提交的合并变更」,副标题是最新 … 最旧两个短哈希
/// (`hashes` 按历史列表的顺序,新 → 旧)。
pub(super) fn combined_heading(hashes: &[String]) -> ModalHeading {
    let short = |hash: Option<&String>| -> String {
        hash.map(|h| h.chars().take(7).collect())
            .unwrap_or_default()
    };
    ModalHeading {
        title: tr!(
            "commitDiff",
            "combinedTitle",
            count = hashes.len().to_string()
        ),
        subtitle: format!("{} … {}", short(hashes.first()), short(hashes.last())),
        subtitle_tip: Some(t("gitLog", "multi.rangeTip")),
        empty: t("gitLog", "multi.noFiles"),
    }
}

/// 左栏的行:树形按目录分组(共用的 [`git_ui::build_change_tree`]),平铺一个文件一行。
pub(super) fn modal_rows(
    files: &[ModalFile],
    tree: bool,
    collapsed: &HashSet<String>,
) -> Vec<ChangeTreeRow> {
    let paths: Vec<(usize, &str)> = files
        .iter()
        .enumerate()
        .map(|(i, f)| (i, f.path.as_str()))
        .collect();
    git_ui::change_rows(&paths, tree, &|p| collapsed.contains(p))
}

/// 第 `ix` 个文件在左栏的第几行(目录全展开时;树形里行序与文件序不同)。
pub(super) fn file_row_of(files: &[ModalFile], tree: bool, ix: usize) -> Option<usize> {
    modal_rows(files, tree, &HashSet::new())
        .iter()
        .position(|r| matches!(r, ChangeTreeRow::File { index, .. } if *index == ix))
}

/// 左栏 ↑ / ↓ 换到哪个文件:眼下**看得见**的行里,选中那一行的上一个 / 下一个文件行(目录
/// 行跳过,收起的目录里的不算;到头停住、不绕回)。选中的文件藏在收起的目录里时 ↓ 落到
/// 第一个、↑ 落到最后一个(与「更改」列表的 ↑↓ 同一口径)。返回 `(文件下标, 行号)`;
/// 没得换(到头了、一个文件行都没有)是 `None`。
pub(super) fn step_file(
    rows: &[ChangeTreeRow],
    selected: usize,
    delta: isize,
) -> Option<(usize, usize)> {
    let files: Vec<(usize, usize)> = rows
        .iter()
        .enumerate()
        .filter_map(|(row, r)| match r {
            ChangeTreeRow::File { index, .. } => Some((*index, row)),
            ChangeTreeRow::Dir { .. } => None,
        })
        .collect();
    let last = files.len().checked_sub(1)?;
    let current = files.iter().position(|&(index, _)| index == selected);
    let next = match current {
        None if delta > 0 => 0,
        None => last,
        Some(pos) => (pos as isize + delta).clamp(0, last as isize) as usize,
    };
    (Some(next) != current).then(|| files[next])
}

/// 左栏选中第 `index` 个文件,右栏换成它的 diff(单击行与 ↑ / ↓ 共用)。已经是它就什么都不做。
pub(super) fn choose_modal_file(
    pick: &Entity<CommitPick>,
    state: &Entity<DiffState>,
    files: &[ModalFile],
    repo_path: &GitContext,
    index: usize,
    cx: &mut App,
) {
    if pick.read(cx).selected == index {
        return;
    }
    pick.update(cx, |p, cx| {
        p.selected = index;
        cx.notify();
    });
    if let Some(file) = files.get(index) {
        load_modal_file(state, repo_path, file, Fetch::Fresh, cx);
    }
}

/// ↑ / ↓:左栏换到上一个 / 下一个文件(见 [`step_file`]),选中行滚到看得见。
pub(super) fn step_modal_file(
    pick: &Entity<CommitPick>,
    state: &Entity<DiffState>,
    files: &[ModalFile],
    repo_path: &GitContext,
    scroll: &UniformListScrollHandle,
    delta: isize,
    cx: &mut App,
) {
    let (rows, selected) = pick.update(cx, |p, _| (p.rows(files), p.selected));
    let Some((index, row)) = step_file(&rows, selected, delta) else {
        return;
    };
    scroll.scroll_to_item(row, ScrollStrategy::Nearest);
    choose_modal_file(pick, state, files, repo_path, index, cx);
}

/// 左栏文件行的 tooltip:完整路径,重命名写 `旧 → 新`。
pub(super) fn modal_file_tip(file: &ModalFile) -> String {
    match &file.old_path {
        Some(old) => format!("{old} → {}", file.path),
        None => file.path.clone(),
    }
}

/// 左栏的一行:目录行点击收起 / 展开,文件行点击换右栏的 diff。只给可视区里的行建
/// (左栏是 uniform_list,行等高 [`git_ui::ROW_H`])。`chevrons` 是目录行折叠箭头的
/// 补间表(在 [`CommitPick`] 上,见 `fold_motion`)。
#[allow(clippy::too_many_arguments)]
pub(super) fn modal_row(
    row: &ChangeTreeRow,
    files: &Rc<Vec<ModalFile>>,
    pick: &Entity<CommitPick>,
    state: &Entity<DiffState>,
    repo_path: &GitContext,
    selected: usize,
    tree: bool,
    chevrons: &crate::fold_motion::FlipTweens,
) -> Option<AnyElement> {
    match row {
        ChangeTreeRow::Dir {
            name,
            full_path,
            depth,
            collapsed,
            file_count,
        } => {
            let pick = pick.clone();
            let id = SharedString::from(format!("git-commit-dir-{full_path}"));
            let turns = chevrons.value_open(full_path, !*collapsed, crate::fold_motion::OPEN_TURNS);
            let full_path = full_path.clone();
            let toggle = move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                pick.update(cx, |p, cx| {
                    p.toggle_dir(&full_path);
                    cx.notify();
                });
            };
            Some(
                git_ui::tree_dir_row(id, name.clone(), *file_count, *depth, *collapsed, turns)
                    // 列表项按根布局排,不像块容器那样自动撑满:不补 w_full 悬停底只有字那么宽
                    .w_full()
                    .on_click(toggle)
                    .into_any_element(),
            )
        }
        &ChangeTreeRow::File { index, depth } => {
            let file = files.get(index)?;
            // 提示文字在气泡弹出时才拼:每帧给可视区每一行都先拼好没必要
            let tip = {
                let files = files.clone();
                move |window: &mut Window, cx: &mut App| {
                    let text = files.get(index).map(modal_file_tip).unwrap_or_default();
                    Tooltip::new(text).build(window, cx)
                }
            };
            let (pick, state, files) = (pick.clone(), state.clone(), files.clone());
            let repo = repo_path.clone();
            let choose = move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                choose_modal_file(&pick, &state, &files, &repo, index, cx);
            };
            Some(
                git_ui::commit_file_row_at(
                    SharedString::from(format!("git-commit-file-{index}")),
                    &file.path,
                    &file.status,
                    index == selected,
                    tree.then_some(depth),
                )
                .tip_with(tip)
                .on_click(choose)
                .into_any_element(),
            )
        }
    }
}

/// 取左栏一个文件的 diff(按它的来源:单提交 / 两个版本之间)。`fetch` 是 [`Fetch::Fresh`]
/// (换文件)或 [`Fetch::Refresh`](刷新按钮:原地重取,内容没变就不动)。
///
/// ⚠️ **单提交里重命名的文件必须带旧路径**(`CommitDiffModal.tsx:57`),否则父树里查
/// 不到旧内容,diff 会显示成「整文件新增」。
pub(super) fn load_modal_file(
    state: &Entity<DiffState>,
    repo_path: &GitContext,
    file: &ModalFile,
    fetch: Fetch,
    cx: &mut App,
) {
    match &file.source {
        ModalFileSource::Commit(hash) => {
            let old_path = file.old_path.clone();
            load_commit_file_at(state, repo_path, hash, &file.path, old_path, fetch, cx);
        }
        ModalFileSource::Revisions { base, head } => {
            let (repo, base, head) = (repo_path.clone(), base.clone(), head.clone());
            load_diff_with(state, Some(repo_path), fetch, cx, move || {
                repo.get_revision_file_diff(base.as_ref(), head.as_ref())
            });
        }
    }
}

/// 某个提交里一个文件的 diff:旧路径由调用方给(单文件历史没有整份文件列表)。
pub(super) fn load_commit_file_at(
    state: &Entity<DiffState>,
    repo_path: &GitContext,
    commit_hash: &str,
    path: &str,
    old_path: Option<String>,
    fetch: Fetch,
    cx: &mut App,
) {
    let (repo, hash, path) = (repo_path.clone(), commit_hash.to_string(), path.to_string());
    load_diff_with(state, Some(repo_path), fetch, cx, move || {
        repo.get_commit_file_diff(&hash, &path, old_path.as_deref())
    });
}

/// 取一份 diff:按 [`Fetch`] 复位、令牌 +1,`load` 丢后台跑,回来时令牌对得上才落地
/// (见 [`DiffState::begin_fetch`] / [`DiffState::land_fetch`])。
pub(super) fn load_diff_with(
    state: &Entity<DiffState>,
    context: Option<&GitContext>,
    fetch: Fetch,
    cx: &mut App,
    load: impl FnOnce() -> anyhow::Result<GitDiffResult> + Send + 'static,
) {
    let req = state.update(cx, |s, cx| {
        if !s.begin_fetch(fetch) {
            return None;
        }
        cx.notify();
        Some(s.request)
    });
    let Some(req) = req else {
        return;
    };

    let state = state.clone();
    let context = context.cloned();
    cx.spawn(async move |cx| {
        let result = match context {
            Some(context) => mt_remote::run_git_on(&context, load).await,
            None => cx.background_executor().spawn(async move { load() }).await,
        };
        state.update(cx, |s: &mut DiffState, cx| {
            if s.land_fetch(req, fetch, result) {
                cx.notify();
            }
        });
    })
    .detach();
}

// ─── 嵌入式单文件提交 diff(单文件历史页签 / 贮藏弹窗用) ──────

/// 「某个提交里的某一个文件」的 diff 面板,给别的视图嵌进去用(单文件历史页签的右栏、
/// 贮藏弹窗的右栏)。
///
/// 只是把提交 diff 右栏那一套(状态 + 取数 + 正文 + 右上角工具组)包了一层,行为与那边
/// 一致;文案沿用 `commitDiff` 命名空间。
#[derive(Clone)]
pub(crate) struct CommitFileDiffPane {
    pub(super) state: Entity<DiffState>,
    /// 当前这份是不是改了名的文件(各克隆共用):纯重命名没有改动块,正文要说一句
    pub(super) renamed: Rc<std::cell::Cell<bool>>,
}

impl CommitFileDiffPane {
    pub(crate) fn new(store: &Entity<AppStore>, cx: &mut App) -> Self {
        let font_size = store.read(cx).config().terminal_font_size as f32;
        Self {
            state: cx.new(|_| DiffState::new(font_size)),
            renamed: Rc::default(),
        }
    }

    /// 换成另一个(提交, 文件)。重命名过的文件要给 `old_path`,否则父树里查不到旧内容。
    pub(crate) fn load(
        &self,
        repo_path: &GitContext,
        commit_hash: &str,
        path: &str,
        old_path: Option<String>,
        cx: &mut App,
    ) {
        self.renamed
            .set(old_path.as_deref().is_some_and(|old| old != path));
        load_commit_file_at(
            &self.state,
            repo_path,
            commit_hash,
            path,
            old_path,
            Fetch::Fresh,
            cx,
        );
    }

    /// 正文(加载中 / 出错 / 二进制 / 过大 / 正常 五选一)。纯重命名与提交 diff 页签
    /// 同一句说明,别留一片空白。
    pub(crate) fn body(&self, cx: &mut App) -> AnyElement {
        let rename_only = self.renamed.get().then(|| t("gitCommit", "renameOnly"));
        render_body(
            &self.state,
            t("commitDiff", "loading"),
            t("commitDiff", "binaryFile"),
            t("commitDiff", "tooLarge"),
            rename_only,
            None,
            cx,
        )
    }

    /// 给宿主的焦点容器挂上正文的 Ctrl/Cmd+C、Ctrl/Cmd+A(键位按宿主的 key context
    /// 绑在 `hotkeys.rs`)。
    pub(crate) fn text_actions(&self, el: gpui::Div) -> gpui::Div {
        selection::text_actions(el, &self.state)
    }

    /// 右上角:改动跳转 + 并排 / 行内切换 + 最右端那一颗(弹窗里是关掉它的 ✕,页签里是刷新,
    /// 见 [`ToolbarTail`])。
    pub(crate) fn toolbar(
        &self,
        id_prefix: &'static str,
        tail: ToolbarTail,
        cx: &mut App,
    ) -> AnyElement {
        view_toggle(
            &self.state,
            id_prefix,
            ToolbarLabels {
                side: t("commitDiff", "sideBySide"),
                inline: t("commitDiff", "inline"),
                prev: t("commitDiff", "prevChange"),
                next: t("commitDiff", "nextChange"),
            },
            tail,
            cx,
        )
    }
}
