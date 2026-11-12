//! diff 页签:单文件 diff / 提交 diff / 合并变更 / 单文件历史 / 磁盘对比五种视图在工作区
//! 页签区里的宿主(原先是五个弹窗)。页签条、关闭、右键批量关闭、切项目与移除项目时的回收
//! 都在 `workbench_area`,这里管「打开哪个 / 复用哪个 / 怎么画 / 什么时候重取」。
//!
//! # 身份与复用
//!
//! 同一项目里同一目标只开一个页签,再次打开就切过去([`DiffTarget`]):
//!
//! | 视图 | 身份 | 已开着时再打开 |
//! |---|---|---|
//! | 单文件 diff | 文件(仓库 + 路径拼成的绝对路径)+ 侧别(已暂存 / 未暂存 / 工作区) | 换上新的仓库、文件序列与回调,原地静默重取(仓库或文件变了就从头取) |
//! | 提交 diff | 仓库 + 提交号 | 带着初始文件的,在左栏选中它 |
//! | 合并变更 | 仓库 + 那组提交(按历史列表的顺序) | 同上 |
//! | 单文件历史 | 文件(项目根 + 相对路径拼成的绝对路径) | 只是切过去 |
//! | 磁盘对比 | 那个文档(文件页签的身份) | 换上新取的两侧内容与发起的文件页签 |
//!
//! 本地路径按文件页签口径规范化;SSH 路径始终按 POSIX 大小写与反斜杠语义比较。
//! 身份包含捕获的后端、授权根与项目,同名路径不跨本地/SSH连接或授权源复用。
//! 页签归属在入口同步捕获;提交/合并变更取数期间切项目,结果只加入原项目、不抢焦点。
//!
//! 单文件 diff 里 Alt+← / Alt+→ 换文件(或块操作后当前文件离开了分组、换到下一个)时**在
//! 当前页签里换**,身份与标题跟着变([`DiffTabEvent::Retarget`]);换到的目标恰好已有另一个
//! 页签时只留一个:撞上的那个正是活动页就留它、关掉换身份的这个(后台页签),否则换身份的
//! 这个接管目标、旧的那个关掉(取舍见 `workbench_area::retarget_plan`)。
//!
//! # 打开一律延到这一轮更新之后
//!
//! 入口散在更改列表、文件树菜单、文件页(gutter / 注解 / 外部改动提示条)、提交历史、推送
//! 对话框里,不少是在别的实体的更新里调的。打开要回读工作区与页签实体,当场做容易撞租约,
//! 所以一律 `window.defer`;提交 / 合并变更先在后台取文件列表(已开着就不取),回来再开,
//! 取失败照旧弹可选中复制的详情框。从弹窗里打开的(推送对话框的待推送提交),调用方先把弹窗
//! 收起来 —— 页签开在弹窗底下是看不见的。
//!
//! # 焦点
//!
//! 打开 / 切到 diff 页签时焦点交给页签([`DiffTab::on_activated`],与文件页签同一套判据:
//! 是活动页、没有弹窗、覆盖物栈不挡)。F7、Alt+←/→、↑/↓、Ctrl/Cmd+C/A 的 key context 都挂在
//! 页签的根上。从更改列表双击 / Enter 打开时焦点也过来 —— 与 IDEA 一样:打开之后在 diff 里
//! 用 Alt+←/→ 逐个文件往下看、F7 跳改动,列表的选中项跟着走(`on_switch`);要回列表点一下
//! 即可。
//!
//! # 防陈旧
//!
//! 页签活得比弹窗久:
//!
//! - 工具栏最右端是「刷新」(弹窗的 ✕ 让位给它,关页签走页签条);
//! - 单文件 diff / 磁盘对比**切回来时静默重取一次**:拿回来的与屏幕上一样就什么都不动,变了
//!   按内容认回行勾选、按行号认回展开过的上下文,滚动位置不动(见 `Fetch::Refresh`);
//! - 单文件历史切回来时比一次 HEAD,变了才整张列表重拉;
//! - 提交 / 合并变更的内容不会变,只在点刷新时重取;
//! - 眼前这一页还接更改列表的取数落地([`notify_repo_changed`]):在 Git 抽屉里暂存 / 丢弃 /
//!   提交、终端里跑 git 命令之后,挤在旁边的那页 diff 当场跟上;不在眼前的页签等切回来再取。

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, InteractiveElement as _,
    IntoElement, KeyDownEvent, Render, Styled as _, Subscription, Task, WeakEntity, Window,
};
use gpui_component::WindowExt as _;
use mt_remote::GitContext;
use mt_ui::icons::Shape;

use super::pages::{FilePage, FilesPage, TextPage};
use super::{
    DiffFile, DiffSide, FileDiffHooks, ModalFile, OnChanged, combined_heading, commit_heading,
    refresh_button, side_label,
};
use crate::file_viewer::{DocumentSource, FileViewer};
use crate::git_file_history::HistoryPage;
use crate::git_ui::{self, ICON_COMMIT, ICON_DIFF, ICON_HISTORY, ICON_MERGE};
use crate::i18n::{t, tr};
use crate::store::AppStore;
use crate::workbench_area::{self, DocumentKey};
use crate::{overlay, ui};

// ─── 身份 ─────────────────────────────────────────────────────

/// 单文件 diff 比的是哪两侧(身份的一部分,同一文件的已暂存 / 未暂存是两个页签)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Side {
    Staged,
    Unstaged,
    Working,
}

impl From<DiffSide> for Side {
    fn from(side: DiffSide) -> Self {
        match side {
            DiffSide::Staged => Side::Staged,
            DiffSide::Unstaged => Side::Unstaged,
            DiffSide::Working => Side::Working,
        }
    }
}

/// diff 页签看的是什么(身份里除项目以外的部分,口径见模块注释)。路径都是规范化过的
/// ([`norm_path`] / [`join_path`]),只拿来比,不拿来取数。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DiffTarget {
    File { file: String, side: Side },
    Commit { repo: String, hash: String },
    Combined { repo: String, hashes: Vec<String> },
    History { file: String },
    Disk(DocumentKey),
}

impl DiffTarget {
    fn file_on(repo: &GitContext, path: &str, side: DiffSide) -> Self {
        DiffTarget::File {
            file: join_path_on(repo, path),
            side: side.into(),
        }
    }

    fn commit_on(repo: &GitContext, hash: &str) -> Self {
        DiffTarget::Commit {
            repo: norm_path_on(repo, repo.path()),
            hash: hash.to_string(),
        }
    }

    fn combined_on(repo: &GitContext, hashes: &[String]) -> Self {
        DiffTarget::Combined {
            repo: norm_path_on(repo, repo.path()),
            hashes: hashes.to_vec(),
        }
    }

    fn history_on(repo: &GitContext, path: &str) -> Self {
        DiffTarget::History {
            file: join_path_on(repo, path),
        }
    }

    #[cfg(test)]
    fn file(repo: &str, path: &str, side: DiffSide) -> Self {
        Self::file_on(&GitContext::local(repo), path, side)
    }

    #[cfg(test)]
    fn commit(repo: &str, hash: &str) -> Self {
        Self::commit_on(&GitContext::local(repo), hash)
    }

    #[cfg(test)]
    fn combined(repo: &str, hashes: &[String]) -> Self {
        Self::combined_on(&GitContext::local(repo), hashes)
    }

    #[cfg(test)]
    fn history(project_root: &str, rel: &str) -> Self {
        Self::history_on(&GitContext::local(project_root), rel)
    }

    /// 这个目标落不落在仓库 `repo`(规范化过的)里:仓库变了时据此挑要跟上的页签。
    /// 提交 / 合并变更的内容不随工作区变,磁盘对比与仓库无关,都不算。
    fn in_repo(&self, repo: &str) -> bool {
        match self {
            DiffTarget::File { file, .. } | DiffTarget::History { file } => file
                .strip_prefix(repo)
                .is_some_and(|rest| rest.starts_with('/') || repo == "/"),
            DiffTarget::Commit { .. } | DiffTarget::Combined { .. } | DiffTarget::Disk(_) => false,
        }
    }
}

/// diff 页签的身份:项目 + 目标。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DiffKey {
    project_id: String,
    backend: String,
    target: DiffTarget,
}

impl DiffKey {
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    fn follows_repo(&self, repo: &GitContext) -> bool {
        self.backend == backend_key(repo)
            && repo.project_id().is_none_or(|id| id == self.project_id)
            && self.target.in_repo(&norm_path_on(repo, repo.path()))
    }
    /// 页签条上的图标形状(与文件页签按类型上色的文件图标分得开)。
    pub fn icon(&self) -> &'static [Shape] {
        match self.target {
            DiffTarget::File { .. } | DiffTarget::Disk(_) => ICON_DIFF,
            DiffTarget::Commit { .. } => ICON_COMMIT,
            DiffTarget::Combined { .. } => ICON_MERGE,
            DiffTarget::History { .. } => ICON_HISTORY,
        }
    }
}

fn backend_key(context: &GitContext) -> String {
    context.with_path("").key()
}

fn norm_path_on(context: &GitContext, path: &str) -> String {
    if context.is_remote() {
        mt_core::path_key::posix_trim_trailing(path).to_string()
    } else {
        norm_path(path)
    }
}

fn join_path_on(context: &GitContext, rel: &str) -> String {
    let root = norm_path_on(context, context.path());
    let rel = norm_path_on(context, rel);
    let rel = rel
        .strip_prefix("./")
        .unwrap_or(&rel)
        .trim_start_matches('/');
    if rel.is_empty() {
        root
    } else {
        format!("{}/{rel}", root.trim_end_matches('/'))
    }
}

/// 路径比对的口径:分隔符统一成 `/`、去掉末尾的 `/`(根目录留着);Windows 上不分大小写
/// (与文件页签的 `normalize_local_document_path` 同口径)。
fn norm_path(path: &str) -> String {
    let unified = path.replace('\\', "/");
    let trimmed = unified.trim_end_matches('/');
    let trimmed = if trimmed.is_empty() {
        unified.as_str()
    } else {
        trimmed
    };
    if cfg!(windows) {
        trimmed.to_lowercase()
    } else {
        trimmed.to_string()
    }
}

/// 根目录 + 相对路径 → 规范化的绝对路径(相对路径开头的 `./`、`/` 去掉)。
#[cfg(test)]
fn join_path(root: &str, rel: &str) -> String {
    let root = norm_path(root);
    let rel = norm_path(rel);
    let rel = rel.trim_start_matches("./").trim_start_matches('/');
    match (root.is_empty(), rel.is_empty()) {
        (_, true) => root,
        (true, false) => rel.to_string(),
        (false, false) => format!("{}/{rel}", root.trim_end_matches('/')),
    }
}

/// 悬停提示里给人看的完整路径(原样大小写,分隔符按平台习惯)。
fn display_path(root: &str, rel: &str) -> String {
    let root = root.trim_end_matches(['/', '\\']);
    let joined = if rel.is_empty() {
        root.to_string()
    } else {
        format!("{root}/{rel}")
    };
    if cfg!(windows) {
        joined.replace('/', "\\")
    } else {
        joined
    }
}

// ─── 标题与提示 ───────────────────────────────────────────────

/// 路径的最后一段。
fn file_name_of(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// 提交信息的第一行(标题里只放得下它)。
fn subject_of(message: &str) -> &str {
    message.lines().next().unwrap_or("").trim()
}

fn short_hash(hash: &str) -> String {
    hash.chars().take(7).collect()
}

/// 单文件 diff 标题里的侧别(短说法;完整的「HEAD → 暂存区」放悬停提示里)。
fn side_tab_label(side: DiffSide) -> &'static str {
    match side {
        DiffSide::Staged => t("diffModal", "tabStaged"),
        DiffSide::Unstaged => t("diffModal", "tabUnstaged"),
        DiffSide::Working => t("diffModal", "tabWorking"),
    }
}

/// 单文件 diff:「a.rs（已暂存）」。
fn file_tab_title(path: &str, side: DiffSide) -> String {
    tr!(
        "diffModal",
        "tabTitle",
        name = file_name_of(path),
        side = side_tab_label(side)
    )
}

/// 单文件 diff 的悬停提示:完整路径 + 比的是哪两侧。
fn file_tab_tip(repo: &str, path: &str, side: DiffSide) -> String {
    format!("{}\n{}", display_path(repo, path), side_label(side))
}

fn file_tab_tip_on(repo: &GitContext, path: &str, side: DiffSide) -> String {
    if repo.is_remote() {
        format!(
            "{}/{}\n{}",
            repo.path().trim_end_matches('/'),
            path,
            side_label(side)
        )
    } else {
        file_tab_tip(repo.path(), path, side)
    }
}

fn file_tab_title_on(repo: &GitContext, path: &str, side: DiffSide) -> String {
    if repo.is_remote() {
        tr!(
            "diffModal",
            "tabTitle",
            name = path.rsplit('/').next().unwrap_or(path),
            side = side_tab_label(side)
        )
    } else {
        file_tab_title(path, side)
    }
}

/// 提交 diff:「提交 abc1234 修复登录页」。
fn commit_tab_title(hash: &str, message: &str) -> String {
    tr!(
        "commitDiff",
        "tabTitle",
        hash = short_hash(hash),
        subject = subject_of(message)
    )
}

/// 提交 diff 的悬停提示:完整的提交信息 + 完整哈希。
fn commit_tab_tip(hash: &str, message: &str) -> String {
    format!("{}\n{hash}", message.trim_end())
}

/// 合并变更:「合并 3 个提交」。
fn combined_tab_title(hashes: &[String]) -> String {
    tr!(
        "commitDiff",
        "combinedTabTitle",
        count = hashes.len().to_string()
    )
}

/// 合并变更的悬停提示:与左栏头部同一套(N 个提交的合并变更 + 最新 … 最旧)。
fn combined_tab_tip(hashes: &[String]) -> String {
    let heading = combined_heading(hashes);
    format!("{}\n{}", heading.title, heading.subtitle)
}

// ─── 页签实体 ─────────────────────────────────────────────────

/// 页签往外报的事。
pub enum DiffTabEvent {
    /// 身份 / 标题变了(单文件 diff 换了文件),页签区跟着改。
    Retarget,
}

/// 页签里装的是哪一种正文。
enum Page {
    File(FilePage),
    Files(FilesPage),
    Text(TextPage),
    History(HistoryPage),
}

/// 工作区页签区里的一个 diff 页签(见模块注释)。
pub struct DiffTab {
    key: DiffKey,
    title: String,
    tip: String,
    page: Page,
    /// 页签正文的焦点:F7 / Alt+←/→ / ↑↓ / Ctrl+C/A 的 key context 挂在它的容器上
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DiffTabEvent> for DiffTab {}

impl DiffTab {
    fn new(
        key: DiffKey,
        title: String,
        tip: String,
        page: Page,
        focus: FocusHandle,
        cx: &mut Context<Self>,
    ) -> Self {
        // 单文件 diff 的控制面一变(换了文件)就核一遍身份与标题
        let subscriptions = match &page {
            Page::File(page) => vec![cx.observe(&page.ctl, |tab, _, cx| tab.sync_target(cx))],
            _ => Vec::new(),
        };
        Self {
            key,
            title,
            tip,
            page,
            focus,
            _subscriptions: subscriptions,
        }
    }

    pub fn key(&self) -> &DiffKey {
        &self.key
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn tip(&self) -> &str {
        &self.tip
    }

    /// 切到这一页之后:交焦点(与文件页签同一套判据,见 [`Self::can_take_focus`]),按需
    /// 静默重取(口径见模块注释「防陈旧」)。
    pub fn on_activated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.can_take_focus(window, cx) {
            self.focus.focus(window, cx);
        }
        match &self.page {
            Page::File(page) => page.refresh(cx),
            Page::Text(page) => page.refresh(cx),
            Page::History(page) => page.check_head(cx),
            // 提交的内容不会变
            Page::Files(_) => {}
        }
    }

    /// 后台页签 / 别的项目的页签不抢焦点;弹窗、菜单这类覆盖物压着时也不抢(它们关掉时
    /// 自己会还焦点)。
    fn can_take_focus(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        workbench_area::is_diff_tab_active(&self.key, cx)
            && !window.has_active_dialog(cx)
            && overlay::allows(overlay::Yield::ToOverlay)
    }

    /// 工具栏的「刷新」。
    fn refresh(&mut self, cx: &mut Context<Self>) {
        match &self.page {
            Page::File(page) => page.refresh(cx),
            Page::Files(page) => page.refresh(cx),
            Page::Text(page) => page.refresh(cx),
            Page::History(page) => page.reload(cx),
        }
    }

    /// 仓库 `repo`(规范化过的)里有东西变了:眼前这一页在那个仓库里就当场跟上,其余等切
    /// 回来时再取(切回来本来就会静默重取)。
    fn on_repo_changed(&mut self, repo: &GitContext, cx: &mut Context<Self>) {
        if !self.key.follows_repo(repo) || !workbench_area::is_diff_tab_active(&self.key, cx) {
            return;
        }
        match &self.page {
            Page::File(page) => page.refresh(cx),
            Page::History(page) => page.check_head(cx),
            Page::Files(_) | Page::Text(_) => {}
        }
    }

    /// 单文件 diff 的控制面变了:换了文件就改身份与标题,告诉页签区。
    fn sync_target(&mut self, cx: &mut Context<Self>) {
        let Page::File(page) = &self.page else {
            return;
        };
        let (repo, file) = page.current(cx);
        let target = DiffTarget::file_on(&repo, &file.path, file.side);
        let title = file_tab_title_on(&repo, &file.path, file.side);
        // 再次打开时可能只换了仓库的写法(文件树按项目根、gutter 按仓库根,见
        // `FilePage::adopt`):身份与标题不变,提示里的完整路径也跟上
        let tip = file_tab_tip_on(&repo, &file.path, file.side);
        if target == self.key.target && title == self.title && tip == self.tip {
            return;
        }
        self.key.target = target;
        self.title = title;
        self.tip = tip;
        cx.emit(DiffTabEvent::Retarget);
        cx.notify();
    }

    /// Ctrl/Cmd+W 关掉这个页签(与文件页签同一个键;挂在容器的按键监听上,理由同
    /// `file_viewer` 的 Ctrl+W)。关页签要回读页签区,延到这一轮更新之后。
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let mods = &keystroke.modifiers;
        if keystroke.key == "w" && mods.secondary() && !mods.shift && !mods.alt {
            cx.stop_propagation();
            let key = self.key.clone();
            window.defer(cx, move |window, cx| {
                workbench_area::close_diff_tab(key, window, cx)
            });
        }
    }
}

impl Render for DiffTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let refresh = refresh_button("diff-tab-refresh", move |cx| {
            let _ = this.update(cx, |tab, cx| tab.refresh(cx));
        });
        let page = match &self.page {
            Page::File(page) => page.render(&self.focus, refresh, cx),
            Page::Files(page) => page.render(&self.focus, refresh, cx),
            Page::Text(page) => page.render(&self.focus, refresh, cx),
            Page::History(page) => page.render(&self.focus, refresh, cx),
        };
        page.size_full()
            .bg(ui::bg_document())
            .on_key_down(cx.listener(Self::on_key_down))
    }
}

// ─── 打开 ─────────────────────────────────────────────────────

/// 页签的来源在入口同步捕获,迟到结果不重新解释活动项目。
enum Home {
    Missing,
    Project(String),
}

impl Home {
    fn capture(store: &Entity<AppStore>, context: &GitContext, cx: &App) -> Self {
        match git_ui::repo_project_id(store, context, cx) {
            Some(id) => Self::Project(id),
            None => Self::Missing,
        }
    }

    fn project_id(&self) -> Option<&str> {
        match self {
            Self::Project(id) => Some(id),
            Self::Missing => None,
        }
    }
}

/// 打开(或切到)一个 diff 页签:延到这一轮更新之后,按 `home` 定下的项目 + `target` 找
/// 已开着的。`open` 拿到找到的那个(`Some`:在它身上补做点事,返回 `None`)或 `None`(建一个
/// 新的返回)。没有活动项目、或记下的项目已经被移除时什么都不做。
///
/// 记下的项目此刻已经不是活动项目(后台取数期间切走了):页签只放进那个项目的页签区,
/// 不切项目、不切页、不抢焦点,回到那个项目时它在页签条上。
fn open_tab_in(
    home: Home,
    backend: String,
    target: DiffTarget,
    window: &mut Window,
    cx: &mut App,
    open: impl FnOnce(
        Option<&Entity<DiffTab>>,
        DiffKey,
        &mut Window,
        &mut App,
    ) -> Option<Entity<DiffTab>>
    + 'static,
) {
    window.defer(cx, move |window, cx| {
        let (project_id, foreground) = {
            let store = AppStore::global(cx);
            let store = store.read(cx);
            let active = store.active_project_id.as_deref();
            match home {
                Home::Missing => return,
                Home::Project(id) => {
                    if store.project(&id).is_none() {
                        return;
                    }
                    let foreground = active == Some(id.as_str());
                    (id, foreground)
                }
            }
        };
        let key = DiffKey {
            project_id,
            backend,
            target,
        };
        let tab = match workbench_area::find_diff_tab(&key, cx) {
            Some(tab) => {
                open(Some(&tab), key, window, cx);
                tab
            }
            None => match open(None, key, window, cx) {
                Some(tab) => tab,
                None => return,
            },
        };
        if foreground {
            workbench_area::show_diff_tab(tab, window, cx);
        } else {
            workbench_area::add_diff_tab(tab, cx);
        }
    });
}

/// 打开单文件 diff。
///
/// `files` 是打开它的那个分组的文件序列(更改列表传整组,文件树传单元素),`current`
/// 是点开的那一个;Alt+← / Alt+→ 在序列里前后换。`on_changed` 在按块操作成功后调用
/// (更改列表传自己的重载,文件树传 `None`)。
pub fn open_file_diff(
    store: Entity<AppStore>,
    repo_path: GitContext,
    files: Vec<DiffFile>,
    current: usize,
    on_changed: Option<OnChanged>,
    window: &mut Window,
    cx: &mut App,
) {
    let hooks = FileDiffHooks {
        on_changed,
        ..FileDiffHooks::default()
    };
    open_file_diff_with(store, repo_path, files, current, hooks, window, cx);
}

/// 同 [`open_file_diff`],回调多两个:`on_switch`(页签里换了文件时调,更改列表让选中项
/// 跟着走)与 `reload_files`(块操作后按重载过的更改列表换文件序列,见
/// [`ReloadFiles`](super::ReloadFiles))。回调里拿更改列表一律用弱引用,列表没了静默忽略。
pub fn open_file_diff_with(
    store: Entity<AppStore>,
    repo_path: GitContext,
    files: Vec<DiffFile>,
    current: usize,
    hooks: FileDiffHooks,
    window: &mut Window,
    cx: &mut App,
) {
    if repo_path.path().is_empty() || files.is_empty() {
        return;
    }
    let current = current.min(files.len() - 1);
    let target = DiffTarget::file_on(&repo_path, &files[current].path, files[current].side);
    let home = Home::capture(&store, &repo_path, cx);
    open_tab_in(
        home,
        backend_key(&repo_path),
        target,
        window,
        cx,
        move |existing, key, _window, cx| {
            if let Some(tab) = existing {
                tab.update(cx, |tab, cx| {
                    if let Page::File(page) = &tab.page {
                        page.adopt(repo_path, files, current, hooks, cx);
                    }
                });
                return None;
            }
            let file = files[current].clone();
            let title = file_tab_title_on(&repo_path, &file.path, file.side);
            let tip = file_tab_tip_on(&repo_path, &file.path, file.side);
            let page = FilePage::new(&store, repo_path, files, current, hooks, cx);
            Some(new_tab(key, title, tip, Page::File(page), cx))
        },
    );
}

/// 后台取某个提交的改动文件,回来再打开它的多文件 diff 页签(`initial` = 先选中哪个文件)。
/// 这个提交已经开着就直接切过去(带着 `initial` 的在左栏选中它),不再取一遍。
///
/// 历史区(双击 / 详情区点文件)、推送对话框的待推送提交、单文件历史「在完整提交 diff
/// 中查看」、文件页注解列点提交,四处同一条路。取文件失败弹可选中复制的详情框
/// ([`git_ui::show_git_error`]:多行原文,toast 装不下)。
///
/// 返回的任务在页签开出来(或失败框弹出)之后结束,要「打开中」防连点的调用方可以等它,
/// 其余直接 `detach`。
pub fn open_commit_diff_loading(
    store: Entity<AppStore>,
    repo_path: GitContext,
    commit_hash: String,
    commit_message: String,
    initial: Option<String>,
    window: &mut Window,
    cx: &mut App,
) -> Task<()> {
    let target = DiffTarget::commit_on(&repo_path, &commit_hash);
    let home = Home::capture(&store, &repo_path, cx);
    let backend = backend_key(&repo_path);
    window.spawn(cx, async move |cx| {
        let started = cx.update(|window, cx| {
            begin_files_tab(&home, &backend, &target, initial.as_deref(), window, cx)
        });
        let Ok(Some(project_id)) = started else {
            return;
        };
        let (task_repo, task_hash) = (repo_path.clone(), commit_hash.clone());
        let files =
            mt_remote::run_git_on(&repo_path, move || task_repo.get_commit_files(&task_hash)).await;
        let _ = cx.update(|window, cx| match files {
            Ok(files) => {
                let title = commit_tab_title(&commit_hash, &commit_message);
                let tip = commit_tab_tip(&commit_hash, &commit_message);
                let heading = commit_heading(&commit_hash, commit_message);
                let files = files
                    .into_iter()
                    .map(|file| ModalFile::from_commit(&commit_hash, file))
                    .collect();
                open_files_tab(
                    Home::Project(project_id),
                    backend,
                    target,
                    FilesSpec {
                        store,
                        repo_path,
                        heading,
                        files,
                        initial,
                        title,
                        tip,
                    },
                    window,
                    cx,
                );
            }
            Err(err) => {
                eprintln!("[git] 取提交 {commit_hash} 的文件失败: {err:#}");
                git_ui::show_git_error(
                    t("gitBranch", "viewCommitFailed"),
                    git_ui::repo_text(&store, &repo_path, git_ui::error_text(&err), cx),
                    window,
                    cx,
                );
            }
        });
    })
}

/// 历史区多选的「查看变更」:后台合并这组提交的改动文件(口径见
/// `mt_project::git::combined`),回来打开合并变更页签。`hashes` 按历史列表的顺序(新 → 旧)。
/// 已开着就直接切过去;失败口径同 [`open_commit_diff_loading`]。
pub fn open_combined_diff_loading(
    store: Entity<AppStore>,
    repo_path: GitContext,
    hashes: Vec<String>,
    initial: Option<String>,
    window: &mut Window,
    cx: &mut App,
) -> Task<()> {
    let target = DiffTarget::combined_on(&repo_path, &hashes);
    let home = Home::capture(&store, &repo_path, cx);
    let backend = backend_key(&repo_path);
    window.spawn(cx, async move |cx| {
        let started = cx.update(|window, cx| {
            begin_files_tab(&home, &backend, &target, initial.as_deref(), window, cx)
        });
        let Ok(Some(project_id)) = started else {
            return;
        };
        let (task_repo, task_hashes) = (repo_path.clone(), hashes.clone());
        let files = mt_remote::run_git_on(&repo_path, move || {
            task_repo.get_combined_commit_files(&task_hashes)
        })
        .await;
        let _ = cx.update(|window, cx| match files {
            Ok(files) => {
                let spec = combined_spec(store, repo_path, &hashes, files, initial);
                open_files_tab(Home::Project(project_id), backend, target, spec, window, cx);
            }
            Err(err) => {
                eprintln!("[git] 合并 {} 个提交的改动失败: {err:#}", hashes.len());
                git_ui::show_git_error(
                    t("gitLog", "multi.viewFailed"),
                    git_ui::repo_text(&store, &repo_path, git_ui::error_text(&err), cx),
                    window,
                    cx,
                );
            }
        });
    })
}

/// 拿着已合并好的文件打开合并变更页签(历史区详情已经合并过同一组提交时直接用)。
pub fn open_combined_diff(
    store: Entity<AppStore>,
    repo_path: GitContext,
    hashes: Vec<String>,
    files: Vec<mt_project::git::CombinedFileChange>,
    initial: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let target = DiffTarget::combined_on(&repo_path, &hashes);
    let home = Home::capture(&store, &repo_path, cx);
    let backend = backend_key(&repo_path);
    let spec = combined_spec(store, repo_path, &hashes, files, initial);
    open_files_tab(home, backend, target, spec, window, cx);
}

/// 合并变更页签要的东西。
fn combined_spec(
    store: Entity<AppStore>,
    repo_path: GitContext,
    hashes: &[String],
    files: Vec<mt_project::git::CombinedFileChange>,
    initial: Option<String>,
) -> FilesSpec {
    FilesSpec {
        store,
        repo_path,
        heading: combined_heading(hashes),
        files: files.into_iter().map(ModalFile::from_combined).collect(),
        initial,
        title: combined_tab_title(hashes),
        tip: combined_tab_tip(hashes),
    }
}

/// 提交 / 合并变更页签要的全部东西(文件列表已经取好)。
struct FilesSpec {
    store: Entity<AppStore>,
    repo_path: GitContext,
    heading: super::ModalHeading,
    files: Vec<ModalFile>,
    initial: Option<String>,
    title: String,
    tip: String,
}

fn open_files_tab(
    home: Home,
    backend: String,
    target: DiffTarget,
    spec: FilesSpec,
    window: &mut Window,
    cx: &mut App,
) {
    open_tab_in(
        home,
        backend,
        target,
        window,
        cx,
        move |existing, key, _, cx| {
            if let Some(tab) = existing {
                // 后台取文件列表的这段时间里同一目标已经开好了(连点两下):在它身上选中初始文件
                if let Some(path) = spec.initial.as_deref() {
                    tab.update(cx, |tab, cx| tab.select_path(path, cx));
                }
                return None;
            }
            let FilesSpec {
                store,
                repo_path,
                heading,
                files,
                initial,
                title,
                tip,
            } = spec;
            let page = FilesPage::new(store, repo_path, heading, files, initial, cx);
            Some(new_tab(key, title, tip, Page::Files(page), cx))
        },
    );
}

/// 提交 / 合并变更起步:记下页签归属的项目(发起这一刻的活动项目,见 [`Home`])。这个目标
/// 在那个项目里已经开着就切过去(带着 `initial` 的在左栏选中它)、返回 `None`;没开着返回
/// 记下的项目,调用方接着去后台取文件列表。没有活动项目也就没有页签区,返回 `None`,别去取了。
fn begin_files_tab(
    home: &Home,
    backend: &str,
    target: &DiffTarget,
    initial: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) -> Option<String> {
    let project_id = home.project_id()?.to_string();
    AppStore::global(cx).read(cx).project(&project_id)?;
    let key = DiffKey {
        project_id,
        backend: backend.to_string(),
        target: target.clone(),
    };
    let Some(tab) = workbench_area::find_diff_tab(&key, cx) else {
        return Some(key.project_id);
    };
    if let Some(path) = initial {
        tab.update(cx, |tab, cx| tab.select_path(path, cx));
    }
    if AppStore::global(cx).read(cx).active_project_id.as_deref() == Some(key.project_id()) {
        workbench_area::show_diff_tab(tab, window, cx);
    }
    None
}

/// 打开某个文件的单文件历史页签。`project_root` 可以是仓库里的子目录,`file_rel` 是
/// 相对它的路径(SSH 使用 POSIX 语义);所属仓库由后端就近发现。已开着就切过去。
pub(crate) fn open_file_history(
    store: Entity<AppStore>,
    project_root: GitContext,
    file_rel: String,
    window: &mut Window,
    cx: &mut App,
) {
    let rel = if project_root.is_remote() {
        file_rel
    } else {
        file_rel.replace('\\', "/")
    };
    let root = project_root.path().to_string();
    let target = DiffTarget::history_on(&project_root, &rel);
    let home = Home::capture(&store, &project_root, cx);
    open_tab_in(
        home,
        backend_key(&project_root),
        target,
        window,
        cx,
        move |existing, key, _window, cx| {
            if existing.is_some() {
                return None;
            }
            let name = if project_root.is_remote() {
                rel.rsplit('/').next().unwrap_or(&rel)
            } else {
                file_name_of(&rel)
            };
            let title = tr!("gitLog", "fileHistory.tabTitle", name = name);
            let tip = if project_root.is_remote() {
                format!("{}/{}", root.trim_end_matches('/'), rel)
            } else {
                display_path(&root, &rel)
            };
            // 历史页的行点击要把焦点要回来(之后 ↑/↓ 才有人接),与页签用同一个焦点句柄
            let focus = cx.focus_handle();
            let page = HistoryPage::new(store, project_root, rel, focus.clone(), cx);
            Some(cx.new(|cx| DiffTab::new(key, title, tip, Page::History(page), focus, cx)))
        },
    );
}

/// 磁盘对比页签要的东西(文件页外部改动提示条的「对比」)。
pub struct TextDiffSpec {
    /// 文件名(抬头与标题用)
    pub name: String,
    /// 那个文件页签的来源(身份按它认:同一个文档只开一个对比页签)
    pub source: DocumentSource,
    /// 那个文件页签:刷新时现取它编辑器里的内容
    pub viewer: WeakEntity<FileViewer>,
    /// 左:磁盘上的版本
    pub disk: String,
    /// 右:编辑器里未保存的修改
    pub mine: String,
}

/// 两段文本的只读 diff 页签:左 = 磁盘上的版本,右 = 编辑器里未保存的修改。复用单文件 diff
/// 的正文渲染(并排 / 行内、F7 跳改动、选中复制),没有块操作、不换文件。同一个文档已经开着
/// 对比页签就换上这次的两侧内容、切过去。
pub fn open_text_diff(spec: TextDiffSpec, window: &mut Window, cx: &mut App) {
    let target = DiffTarget::Disk(workbench_area::document_key(&spec.source));
    let home = Home::Project(spec.source.project_id().to_string());
    open_tab_in(
        home,
        String::new(),
        target,
        window,
        cx,
        move |existing, key, _window, cx| {
            let TextDiffSpec {
                name,
                source,
                viewer,
                disk,
                mine,
            } = spec;
            if let Some(tab) = existing {
                tab.update(cx, |tab, cx| {
                    if let Page::Text(page) = &mut tab.page {
                        page.set_texts(viewer, disk, mine, cx);
                    }
                });
                return None;
            }
            let title = tr!("fileViewer", "compareTabTitle", name = name.as_str());
            let tip = format!(
                "{}\n{} → {}",
                source.path().display(),
                t("fileViewer", "compareSideDisk"),
                t("fileViewer", "compareSideUnsaved")
            );
            let store = AppStore::global(cx);
            let page = TextPage::new(&store, &name, viewer, disk, mine, cx);
            Some(new_tab(key, title, tip, Page::Text(page), cx))
        },
    );
}

fn new_tab(key: DiffKey, title: String, tip: String, page: Page, cx: &mut App) -> Entity<DiffTab> {
    cx.new(|cx| {
        let focus = cx.focus_handle();
        DiffTab::new(key, title, tip, page, focus, cx)
    })
}

impl DiffTab {
    /// 提交 / 合并变更页签被再次打开、带着初始文件:在左栏选中它。
    fn select_path(&mut self, path: &str, cx: &mut Context<Self>) {
        if let Page::Files(page) = &self.page {
            page.select_path(path, cx);
        }
    }
}

/// 仓库 `repo` 里有东西变了(更改列表的取数落地了:Git 抽屉里暂存 / 丢弃 / 提交、监听到
/// 工作区或 `.git` 变化、终端里跑了 git 命令)。眼前那一页 diff 在这个仓库里就当场静默重取,
/// 其余等切回来再取。调用方要保证此刻没借着任何 diff 页签(更改列表是延到更新之后调的)。
pub fn notify_repo_changed(repo: &GitContext, cx: &mut App) {
    if repo.path().is_empty() {
        return;
    }
    for tab in workbench_area::diff_tabs(cx) {
        tab.update(cx, |tab, cx| tab.on_repo_changed(repo, cx));
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[gpui::test]
    fn deferred_open_uses_source_project_not_current_project(cx: &mut gpui::TestAppContext) {
        cx.skip_drawing();
        cx.update(gpui_component::init);
        let config = mt_config::AppConfig {
            projects: vec![
                mt_config::ProjectConfig::new("a", "a", "/source-a"),
                mt_config::ProjectConfig::new("b", "b", "/source-b"),
            ],
            last_active_project_id: Some("a".into()),
            ..Default::default()
        };
        let store = cx.new(|cx| AppStore::git_test_store(config, cx));
        cx.update(|cx| AppStore::set_global(store.clone(), cx));
        let window = cx.add_window(|window, cx| {
            let empty = cx.new(|_| gpui::Empty);
            gpui_component::Root::new(empty, window, cx)
        });
        let landed = std::rc::Rc::new(std::cell::RefCell::new(None));
        cx.update_window(window.into(), |_, window, cx| {
            let repo = GitContext::local("/source-a");
            let home = Home::capture(&store, &repo, cx);
            let done = landed.clone();
            open_tab_in(
                home,
                backend_key(&repo),
                DiffTarget::file_on(&repo, "file", DiffSide::Unstaged),
                window,
                cx,
                move |_, key, _, _| {
                    *done.borrow_mut() = Some(key);
                    None
                },
            );
            store.update(cx, |store, _| store.active_project_id = Some("b".into()));
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(landed.borrow().as_ref().unwrap().project_id(), "a");
        assert_eq!(
            cx.update(|cx| store.read(cx).active_project_id.clone())
                .as_deref(),
            Some("b")
        );
    }

    fn remote(host: &str) -> GitContext {
        GitContext::remote(
            "p".into(),
            mt_config::SshConnection {
                id: "diff-consumer".into(),
                name: "test".into(),
                host: host.into(),
                port: 22,
                user: "test".into(),
                password: None,
                identity_file: None,
                group: None,
                extra: Default::default(),
            },
            "/Repo".into(),
        )
    }

    #[test]
    fn remote_tabs_and_refresh_keep_backend_and_posix_identity() {
        let a = remote("one.invalid");
        let b = remote("two.invalid");
        let key = |repo: &GitContext, path: &str| DiffKey {
            project_id: "p".into(),
            backend: backend_key(repo),
            target: DiffTarget::file_on(repo, path, DiffSide::Unstaged),
        };
        let ka = key(&a, "src/A\\B.rs");
        assert_ne!(ka, key(&b, "src/A\\B.rs"));
        assert_ne!(ka, key(&GitContext::local("/Repo"), "src/A\\B.rs"));
        assert_ne!(ka, key(&a, "src/a\\B.rs"));
        assert_ne!(ka, key(&a, "src/A/B.rs"));
        assert_eq!(ka, key(&a.with_path("/Repo/"), "./src/A\\B.rs"));
        assert!(ka.follows_repo(&a));
        assert!(!ka.follows_repo(&b));
        assert!(!ka.follows_repo(&a.with_path("/Repo2")));
        let other_project = GitContext::remote(
            "other".into(),
            a.connection().unwrap().clone(),
            "/Repo".into(),
        );
        assert!(!ka.follows_repo(&other_project));
        assert_eq!(backend_key(&a), backend_key(&a.with_path("/Repo/sub")));
        let history = DiffKey {
            target: DiffTarget::history_on(&a.with_path("/Repo/sub"), "A\\B.rs"),
            ..ka.clone()
        };
        assert!(history.follows_repo(&a));
        assert!(!history.follows_repo(&b));
        let tip = file_tab_tip_on(&a, "A\\B.rs", DiffSide::Unstaged);
        assert!(tip.starts_with("/Repo/A\\B.rs\n"), "{tip}");
        assert!(file_tab_title_on(&a, "A\\B.rs", DiffSide::Unstaged).contains("A\\B.rs"));
    }

    /// 同一个文件,文件树按项目根(子目录)传、更改列表 / gutter 按仓库根传,拼出来是同一个
    /// 身份;分隔符、末尾的 `/`、相对路径开头的 `./` 都不影响。
    #[test]
    fn 同一文件不同写法认成同一个目标() {
        let from_list = DiffTarget::file("D:/repo", "sub/a.rs", DiffSide::Working);
        let from_tree = DiffTarget::file(r"D:\repo\sub\", "a.rs", DiffSide::Working);
        let dotted = DiffTarget::file("D:/repo/", "./sub/a.rs", DiffSide::Working);
        assert_eq!(from_list, from_tree);
        assert_eq!(from_list, dotted);
        if cfg!(windows) {
            // Windows 上不分大小写(与文件页签同口径)
            assert_eq!(
                from_list,
                DiffTarget::file("d:/REPO", "Sub/A.rs", DiffSide::Working)
            );
        }
        // POSIX 的根目录不会被裁成空串
        assert_eq!(norm_path("/"), "/");
        assert_eq!(join_path("/", "a.rs"), "/a.rs");
    }

    /// 单文件 diff 的身份带着侧别:同一文件的已暂存 / 未暂存 / 工作区是三个页签;别的文件
    /// 是另一个页签。
    #[test]
    fn 侧别与文件都参与身份() {
        let staged = DiffTarget::file("/r", "a.rs", DiffSide::Staged);
        let unstaged = DiffTarget::file("/r", "a.rs", DiffSide::Unstaged);
        let working = DiffTarget::file("/r", "a.rs", DiffSide::Working);
        assert_ne!(staged, unstaged);
        assert_ne!(unstaged, working);
        assert_ne!(staged, DiffTarget::file("/r", "b.rs", DiffSide::Staged));
        // 同一目标、不同项目是两个页签
        assert_ne!(
            DiffKey {
                project_id: "p1".into(),
                backend: "local".into(),
                target: staged.clone(),
            },
            DiffKey {
                project_id: "p2".into(),
                backend: "local".into(),
                target: staged,
            }
        );
    }

    /// 提交按(仓库, 提交号)认,仓库路径的写法不影响;合并变更按那组提交认 —— 同一组、
    /// 同一顺序才是同一个页签。单文件历史按文件认。五种目标互不相等。
    #[test]
    fn 提交与合并变更与历史的身份() {
        assert_eq!(
            DiffTarget::commit("/r/", "abc123"),
            DiffTarget::commit("/r", "abc123")
        );
        assert_ne!(
            DiffTarget::commit("/r", "abc123"),
            DiffTarget::commit("/r", "def456")
        );
        assert_ne!(
            DiffTarget::commit("/r", "abc123"),
            DiffTarget::commit("/other", "abc123")
        );
        let pair = ["n".to_string(), "o".to_string()];
        let reversed = ["o".to_string(), "n".to_string()];
        assert_eq!(
            DiffTarget::combined("/r", &pair),
            DiffTarget::combined("/r/", &pair)
        );
        assert_ne!(
            DiffTarget::combined("/r", &pair),
            DiffTarget::combined("/r", &reversed)
        );
        assert_eq!(
            DiffTarget::history("/r/sub", "a.rs"),
            DiffTarget::history("/r", "sub/a.rs")
        );
        assert_ne!(
            DiffTarget::history("/r", "a.rs"),
            DiffTarget::file("/r", "a.rs", DiffSide::Working)
        );
    }

    /// 磁盘对比按文档认:同一个文件页签只开一个对比页签。
    #[test]
    fn 磁盘对比按文档认() {
        let source = |path: &str| DocumentSource::Local {
            project_id: "p".into(),
            project_root: PathBuf::from("/r"),
            path: PathBuf::from(path),
        };
        let a = DiffTarget::Disk(workbench_area::document_key(&source("/r/a.rs")));
        assert_eq!(
            a,
            DiffTarget::Disk(workbench_area::document_key(&source("/r/a.rs")))
        );
        assert_ne!(
            a,
            DiffTarget::Disk(workbench_area::document_key(&source("/r/b.rs")))
        );
        assert!(!a.in_repo("/r"), "磁盘对比与仓库无关,仓库变了不跟");
    }

    /// 仓库变了时挑页签:落在仓库里的单文件 diff / 单文件历史才算(前缀要落在路径分隔处);
    /// 提交 / 合并变更的内容不随工作区变,不算。
    #[test]
    fn 仓库变了挑落在仓库里的页签() {
        let repo = norm_path("/work/repo");
        assert!(DiffTarget::file("/work/repo", "src/a.rs", DiffSide::Unstaged).in_repo(&repo));
        assert!(DiffTarget::history("/work/repo/src", "a.rs").in_repo(&repo));
        assert!(
            !DiffTarget::file("/work/repo2", "a.rs", DiffSide::Unstaged).in_repo(&repo),
            "repo2 只是前缀相同"
        );
        assert!(!DiffTarget::commit("/work/repo", "abc").in_repo(&repo));
        assert!(!DiffTarget::combined("/work/repo", &["abc".to_string()]).in_repo(&repo));
    }

    /// 页签图标按目标分四种形状,与文件页签的文件图标分得开。
    #[test]
    fn 图标按目标分() {
        let key = |target| DiffKey {
            project_id: "p".into(),
            backend: "local".into(),
            target,
        };
        // const 切片每次用到都可能落在不同地址,不能比指针;Shape 没有 PartialEq,按 Debug 比内容。
        let same = |a: &[mt_ui::icons::Shape], b: &[mt_ui::icons::Shape]| {
            format!("{a:?}") == format!("{b:?}")
        };
        assert!(same(
            key(DiffTarget::file("/r", "a", DiffSide::Staged)).icon(),
            ICON_DIFF
        ));
        assert!(same(key(DiffTarget::commit("/r", "a")).icon(), ICON_COMMIT));
        assert!(same(
            key(DiffTarget::combined("/r", &["a".to_string()])).icon(),
            ICON_MERGE
        ));
        assert!(same(
            key(DiffTarget::history("/r", "a")).icon(),
            ICON_HISTORY
        ));
        assert!(
            !same(ICON_DIFF, ICON_COMMIT),
            "四个图标得互不相同,上面的断言才有意义"
        );
    }

    /// 标题:单文件 diff 是「文件名 + 短侧别」,提交是「短哈希 + 提交信息第一行」,合并变更
    /// 写明几个提交;悬停提示给完整路径 + 两侧说明 / 完整提交信息 + 完整哈希。
    #[test]
    fn 页签标题与提示() {
        let title = file_tab_title("src/ui/a.rs", DiffSide::Staged);
        assert!(
            title.contains("a.rs") && !title.contains("src/ui"),
            "{title}"
        );
        assert!(title.contains(t("diffModal", "tabStaged")), "{title}");
        assert_ne!(title, file_tab_title("src/ui/a.rs", DiffSide::Unstaged));

        let tip = file_tab_tip("/repo", "src/ui/a.rs", DiffSide::Unstaged);
        assert!(
            tip.contains("a.rs") && tip.contains(side_label(DiffSide::Unstaged)),
            "{tip}"
        );

        let hash = "0123456789abcdef";
        let title = commit_tab_title(hash, "修复登录页\n\n详细说明");
        assert!(
            title.contains("0123456") && !title.contains("0123456789"),
            "{title}"
        );
        assert!(
            title.contains("修复登录页") && !title.contains("详细说明"),
            "{title}"
        );
        let tip = commit_tab_tip(hash, "修复登录页\n\n详细说明\n");
        assert!(tip.contains("详细说明") && tip.ends_with(hash), "{tip}");

        let hashes = [
            "aaaaaaaaaa".to_string(),
            "bbbbbbbbbb".to_string(),
            "cccccccccc".to_string(),
        ];
        assert!(combined_tab_title(&hashes).contains('3'));
        let tip = combined_tab_tip(&hashes);
        assert!(tip.contains("aaaaaaa") && tip.contains("ccccccc"), "{tip}");
    }

    /// 提示里的完整路径按平台习惯写分隔符,原样大小写。
    #[test]
    fn 提示路径原样拼接() {
        let shown = display_path("D:/Work/Repo/", "src/A.rs");
        if cfg!(windows) {
            assert_eq!(shown, r"D:\Work\Repo\src\A.rs");
        } else {
            assert_eq!(shown, "D:/Work/Repo/src/A.rs");
        }
        assert_eq!(file_name_of(r"a/b\c.rs"), "c.rs");
        assert_eq!(subject_of("  标题  \n正文"), "标题");
        assert_eq!(subject_of(""), "");
    }
}
