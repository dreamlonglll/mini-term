//! Git 面板的「提交历史」区。对应 `src/components/GitHistoryContent.tsx`(399 行),
//! 之后按 JetBrains IDEA 的 Log 工具窗补了一圈:选中 + 提交详情、键盘导航、过滤栏、
//! 标签胶囊、右键改写操作(见 [`actions`])。
//!
//! # 分页与请求作废(`GitHistoryContent.tsx:234-290`)
//!
//! - 每页 30 条;**首页带 `branch`,续页一律 `branch: None`** ——
//!   续页的游标由已加载列表还原([`GitLogCursor::from_loaded`]:已加载集合 +
//!   它们尚未加载的父),不需要 branch。原版只拿「上一页末尾 commit 的 parent」
//!   续走,合并历史里页边界上另一侧分支的提交会丢、已加载的会被带回来;
//! - 合并新页走 [`mt_project::git::merge_log_page`]:按 hash 去重(防线,游标续页
//!   本身不回已加载的),并在时钟偏移让子提交迟到时做一次稳定的拓扑修正 ——
//!   拓扑图要求父永远在子之后。`has_more = 本页 >= 30 && 真的新增了`;
//! - **请求令牌**:换仓库 / 换分支时令牌 +1,迟到响应直接丢弃。
//!
//! # 过滤
//!
//! 有过滤条件时换走 [`mt_project::git::get_git_log_filtered`]:后端遍历时跳过不命中的
//! 提交,续页游标由后端原样交回(从已加载列表还原不出「走过但没命中」的那些)。
//! 后端每次最多扫 `SCAN_BUDGET` 个提交就先交回,这里见「没凑够、还有下文」就接着要,
//! 凑满 [`FilterState::want`] 为止 —— 换条件时令牌一换,在扫的那批作废,不会被一次
//! 全量遍历拖住。过滤后拓扑线没有意义,**图列隐藏**。
//!
//! # 刷新不闪
//!
//! 手动刷新 / 提交后 / 改写操作后的重载([`GitHistoryContent::reload`])**保留旧列表**,
//! 首页回来时整体替换,期间只在过滤栏右侧标一个「刷新中…」。换仓库 / 换分支 / 换过滤
//! 条件([`GitHistoryContent::restart`])内容本就不同,才当场清空。
//!
//! # 拓扑图为什么自绘
//!
//! gpui 的 `svg()` 是单色 alpha 掩膜(丢色),Image 的 SVG 分支有 BGRA 交换 bug ——
//! 两条都不能用(K 批记档)。这里走 [`gpui::PathBuilder`] 自绘:贝塞尔用
//! `cubic_bezier_to`,渐变用**分段近似**(见 [`GraphCell`] 的注释)。
//!
//! ⚠️ 行高必须恒为 [`GRAPH_ROW_HEIGHT`](crate::git_graph::GRAPH_ROW_HEIGHT),
//! 否则连线跨行接不上(过滤时图列隐藏,行高照旧,uniform_list 才量得准)。

mod actions;
mod detail;

pub use actions::GitLogRewordConfirm;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, AppContext as _, Bounds, ClickEvent, ClipboardItem, Context, Element, Entity,
    EventEmitter, FocusHandle, Focusable as _, GlobalElementId, Hsla, InspectorElementId,
    InteractiveElement, IntoElement, LayoutId, MouseButton, MouseDownEvent, ParentElement,
    PathBuilder, Pixels, Point, Render, ScrollStrategy, SharedString, StatefulInteractiveElement,
    Style, Styled, Subscription, Task, UniformListScrollHandle, Window, canvas, div, point,
    prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_component::input::{Input, InputEvent, InputState};
use mt_project::git::{BranchInfo, GitCommitInfo, GitLogCursor, LogFilter, LogPage, TagInfo};
use mt_ui::tooltip::TooltipExt as _;

use crate::git_graph::{
    self, GRAPH_ROW_HEIGHT, GraphLayout, GraphRow, SegPath, palette_color, segment_path,
};
use crate::git_ui::{
    self, ButtonKind, ICON_CLOSE, ICON_COMMIT, ICON_SEARCH, ICON_USER, RefKind, Tone, avatar,
    empty_state, icon, icon_button, mono, more_pill, ref_pill,
};
use crate::i18n::{t, tr};
use crate::store::AppStore;
use crate::ui;
use crate::{git_diff, git_watch};

gpui::actions!(
    mini_term,
    [
        /// 历史列表选中上一条(↑,焦点在历史区)
        GitLogSelectPrev,
        /// 历史列表选中下一条(↓)
        GitLogSelectNext,
        /// 打开选中提交的 diff(Enter;焦点在过滤框时 = 立即应用过滤)
        GitLogOpenSelected,
        /// 复制选中提交的完整哈希(Ctrl/Cmd+C)
        GitLogCopyHash,
        /// 聚焦过滤框(Ctrl/Cmd+F,历史区上下文内)
        GitLogFocusFilter,
        /// 清空过滤框并把焦点还给列表(过滤框里按 Esc)
        GitLogClearFilter,
    ]
);

/// 每页条数(`GitHistoryContent.tsx:244`)。
const PAGE_SIZE: usize = 30;
/// 过滤框输入去抖。
const FILTER_DEBOUNCE_MS: u64 = 300;
/// 键盘往下走时,离已加载末尾还剩几行就预取下一页。
const PREFETCH_ROWS: usize = 5;
/// 行里直接显示几个引用胶囊,其余收进「+N」。
const MAX_PILLS: usize = 2;
/// 历史区窄于这个宽度时,行里只留作者头像、不写作者名。
const NARROW_WIDTH: f32 = 380.0;

/// 往上冒的事件。
pub enum GitHistoryEvent {
    /// pty-output 嗅探命中 → 容器要重新发现仓库(原版的 `refreshRepos()`)。
    RefreshRepos,
    /// 右键菜单里的改写操作动了仓库(HEAD / 分支 / 标签 / 暂存区 / 工作区):容器要
    /// 重载仓库与分支,并让「更改」区重取。历史区自己已经重载过了。
    RepoChanged,
    /// 撤销了最近一次提交:它的完整消息交给提交框回填(IDEA 的 Undo Commit 口径),
    /// 改完直接再提交即可。
    CommitUndone { message: String },
}

/// 过滤栏的状态。
struct FilterState {
    input: Entity<InputState>,
    /// 已生效(去抖后)的关键词。
    text: String,
    /// 「仅我的」开关。
    only_mine: bool,
    /// 「仅我的」对应的邮箱(开关打开 / 换仓库时查一次 `user.email`)。
    email: Option<String>,
    /// 「仅我的」开着但仓库没配 `user.email`。
    no_email: bool,
    /// 续页游标(后端交回的那个,见模块注释)。`None` = 到底 / 还没开始。
    cursor: Option<GitLogCursor>,
    /// 这一轮至今扫过的提交数(「搜索中…已扫描 N 个」)。
    scanned: usize,
    /// 这一轮想要凑满的条数:首屏一页,每触底一次加一页。
    want: usize,
    debounce: Option<Task<()>>,
}

/// 过滤条件有没有实际生效。「仅我的」查不到邮箱时不算。
fn filter_active(text: &str, only_mine: bool, email: Option<&str>) -> bool {
    !text.is_empty() || (only_mine && email.is_some())
}

impl FilterState {
    fn active(&self) -> bool {
        filter_active(&self.text, self.only_mine, self.email.as_deref())
    }

    fn as_filter(&self) -> LogFilter {
        LogFilter {
            text: self.text.clone(),
            author_email: if self.only_mine {
                self.email.clone()
            } else {
                None
            },
        }
    }
}

pub struct GitHistoryContent {
    store: Entity<AppStore>,
    /// 空串 = 无仓库。
    repo_path: String,
    /// 只用来给提交行标注分支胶囊。
    branches: Vec<BranchInfo>,
    /// HEAD 所在的分支名;detached 时是 `"(1a2b3c4)"` 那种短 hash,查不到分支就当没有。
    /// 只用来找「跟随 HEAD 时正在展示的是哪条本地分支」,进而挂它的上游胶囊。
    head_branch: Option<String>,
    /// HEAD 指向的提交(由分支列表 / detached 短 hash 推出来;画 HEAD 圆环、HEAD 胶囊用)。
    head_hash: Option<String>,
    /// 正在查看(未 checkout)的分支;`None` = 跟随 HEAD。
    view_branch: Option<String>,
    tags: Vec<TagInfo>,
    /// 提交 → 指向它的标签名(行渲染逐行查表,不在每帧里扫全部标签)。
    tags_by_commit: HashMap<String, Vec<String>>,
    commits: Vec<GitCommitInfo>,
    graph: GraphLayout,
    loading: bool,
    has_more: bool,
    /// 保留旧列表的重载在途(过滤栏右侧标「刷新中…」)。
    refreshing: bool,
    load_error: Option<String>,
    /// 请求令牌:换仓库 / 换分支后的迟到响应丢弃。
    request: u64,
    /// pty-output 嗅探的 500ms 去抖终点(与「更改」区各算各的)。
    debounce_until: Option<Instant>,
    filter: FilterState,
    /// 选中的提交(按 hash 记:分页 / 刷新后照样认得出)。
    selected: Option<String>,
    detail: detail::DetailState,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    /// 进行中的改写操作(防重复触发),值是 `gitLog.action.*` 的 key。
    busy: Option<&'static str>,
    /// 历史区当前宽度(canvas 量的),决定行里写不写作者名。
    width: f32,
    _subs: Vec<Subscription>,
}

impl EventEmitter<GitHistoryEvent> for GitHistoryContent {}

/// 任意 flex-grow(gpui 的 `flex_grow()` 只会设成 1)。
fn grow<E: Styled>(mut el: E, value: f32) -> E {
    el.style().flex_grow = Some(value);
    el
}

impl GitHistoryContent {
    pub fn new(store: Entity<AppStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t("gitLog", "filter.placeholder")));
        let sub = cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, _, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => this.schedule_filter(cx),
                // 回车不等去抖,立刻生效
                InputEvent::PressEnter { .. } => this.apply_filter(cx),
                _ => {}
            },
        );
        Self {
            detail: detail::DetailState::new(window, cx),
            store,
            repo_path: String::new(),
            branches: Vec::new(),
            head_branch: None,
            head_hash: None,
            view_branch: None,
            tags: Vec::new(),
            tags_by_commit: HashMap::new(),
            commits: Vec::new(),
            graph: git_graph::compute(&[]),
            loading: false,
            has_more: false,
            refreshing: false,
            load_error: None,
            request: 0,
            debounce_until: None,
            filter: FilterState {
                input,
                text: String::new(),
                only_mine: false,
                email: None,
                no_email: false,
                cursor: None,
                scanned: 0,
                want: PAGE_SIZE,
                debounce: None,
            },
            selected: None,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            busy: None,
            width: 0.0,
            _subs: vec![sub],
        }
    }

    /// 容器把仓库 / 分支列表 / HEAD 分支 / 查看分支一起透下来。换仓库、换查看分支
    /// 重头拉第一页(原版靠 `key={historyRefreshKey}` 与 effect 依赖达到同样效果);
    /// 分支列表变了只重画胶囊。
    ///
    /// ⚠️ 分支列表要**整条**比,不能只比名字:提交 / 推送后分支名一个没变、
    /// 只有 `commit_hash` 前移,只比名字会把新列表丢掉,胶囊就钉死在旧 commit 上。
    pub fn sync(
        &mut self,
        repo_path: &str,
        branches: &[BranchInfo],
        head_branch: Option<&str>,
        view_branch: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let tags_changed = self.branches != branches || self.head_branch.as_deref() != head_branch;
        let repo_changed = self.repo_path != repo_path;
        let reload = repo_changed || self.view_branch.as_deref() != view_branch;
        self.repo_path = repo_path.to_string();
        self.view_branch = view_branch.map(str::to_string);
        if tags_changed {
            self.branches = branches.to_vec();
            self.head_branch = head_branch.map(str::to_string);
            self.refresh_head_hash();
            cx.notify();
        }
        if repo_changed {
            // 换仓库:选中、详情、标签都是上一个仓库的,一并作废;「仅我的」的邮箱重查
            self.selected = None;
            self.detail.clear();
            self.tags.clear();
            self.tags_by_commit.clear();
            if self.filter.only_mine {
                self.resolve_my_email(cx);
            }
        }
        if reload {
            self.restart(cx);
        }
    }

    /// 刷新:**保留旧列表**,首页回来再整体替换(提交成功 / 手动刷新 / 改写操作后)。
    ///
    /// 原版是给 `GitHistoryContent` 换 `key` 整体重建,列表会白一下;这里只在过滤栏
    /// 右侧标「刷新中…」,内容不闪。
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.refreshing = !self.commits.is_empty();
        self.begin_load(cx);
    }

    /// 整体重来并**当场清空**(换仓库 / 换分支 / 换过滤条件:内容本来就不同)。
    fn restart(&mut self, cx: &mut Context<Self>) {
        self.commits.clear();
        self.graph = git_graph::compute(&[]);
        self.refreshing = false;
        self.begin_load(cx);
    }

    fn begin_load(&mut self, cx: &mut Context<Self>) {
        self.has_more = false;
        self.load_error = None;
        self.filter.cursor = None;
        self.filter.scanned = 0;
        self.filter.want = PAGE_SIZE;
        self.request += 1;
        // ⚠️ loading 必须复位:令牌已 +1,在途响应注定被丢弃,而丢弃分支
        // 不会走到 `loading = false` —— 不复位的话这次 load_page 被 loading
        // 闸挡掉,历史区就永远停在旧仓库的内容上再也不刷
        self.loading = false;
        self.load_page(true, cx);
        cx.notify();
    }

    /// 容器的 pty-output 嗅探命中了。
    pub fn note_pty_hit(&mut self) {
        self.debounce_until = Some(Instant::now() + Duration::from_millis(git_watch::DEBOUNCE_MS));
    }

    /// 容器的节拍。
    pub fn tick(&mut self, cx: &mut Context<Self>) {
        if self.debounce_until.is_some_and(|at| Instant::now() >= at) {
            self.debounce_until = None;
            // 原版去抖回调里做两件事:refreshRepos() + load()
            cx.emit(GitHistoryEvent::RefreshRepos);
            self.reload(cx);
        }
    }

    fn load_page(&mut self, first: bool, cx: &mut Context<Self>) {
        if self.repo_path.is_empty() || self.loading {
            return;
        }
        let req = self.request;
        let repo = PathBuf::from(&self.repo_path);
        // 首页带 branch;续页的游标由已加载列表还原(见模块注释),不需要 branch
        let branch = if first {
            self.view_branch.clone()
        } else {
            None
        };

        if self.filter.active() {
            let filter = self.filter.as_filter();
            let after = if first {
                None
            } else {
                // 续页却没有游标 = 已经走完
                match self.filter.cursor.take() {
                    Some(cursor) => Some(cursor),
                    None => return,
                }
            };
            self.loading = true;
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let page = mt_project::git::get_git_log_filtered(
                            &repo,
                            after,
                            PAGE_SIZE,
                            branch.as_deref(),
                            &filter,
                        );
                        let tags =
                            first.then(|| mt_project::git::list_tags(&repo).unwrap_or_default());
                        page.map(|page| (page, tags))
                    })
                    .await;
                let _ = this.update(cx, |this: &mut Self, cx| {
                    if this.request != req {
                        return;
                    }
                    this.loading = false;
                    match result {
                        Ok((page, tags)) => this.merge_filtered(first, page, tags, cx),
                        Err(err) => this.load_failed(err),
                    }
                    cx.notify();
                });
            })
            .detach();
        } else {
            self.loading = true;
            let after = (!first).then(|| GitLogCursor::from_loaded(&self.commits));
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let page = mt_project::git::get_git_log(
                            &repo,
                            after.as_ref(),
                            Some(PAGE_SIZE),
                            branch.as_deref(),
                        );
                        let tags =
                            first.then(|| mt_project::git::list_tags(&repo).unwrap_or_default());
                        page.map(|page| (page, tags))
                    })
                    .await;
                let _ = this.update(cx, |this: &mut Self, cx| {
                    if this.request != req {
                        return;
                    }
                    this.loading = false;
                    match result {
                        Ok((page, tags)) => this.merge_page(first, page, tags),
                        Err(err) => this.load_failed(err),
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn load_failed(&mut self, err: anyhow::Error) {
        eprintln!("[git] 取提交历史失败: {err:#}");
        self.refreshing = false;
        self.load_error = Some(format!("{err:#}"));
    }

    fn set_tags(&mut self, tags: Vec<TagInfo>) {
        self.tags_by_commit.clear();
        for tag in &tags {
            self.tags_by_commit
                .entry(tag.target_hash.clone())
                .or_default()
                .push(tag.name.clone());
        }
        self.tags = tags;
    }

    /// 合并一页。去重 + 拓扑修正 + `has_more` 判定,见模块注释。
    fn merge_page(&mut self, first: bool, page: Vec<GitCommitInfo>, tags: Option<Vec<TagInfo>>) {
        if first {
            // 刷新保留的旧列表到这里才换掉
            self.commits.clear();
            self.refreshing = false;
        }
        if let Some(tags) = tags {
            self.set_tags(tags);
        }
        self.has_more = merge_into(&mut self.commits, page);
        self.graph = git_graph::compute(&self.commits);
        self.refresh_head_hash();
    }

    /// 合并一页过滤结果;没凑够 [`FilterState::want`] 且还有下文就接着扫。
    fn merge_filtered(
        &mut self,
        first: bool,
        page: LogPage,
        tags: Option<Vec<TagInfo>>,
        cx: &mut Context<Self>,
    ) {
        if first {
            self.commits.clear();
            self.refreshing = false;
        }
        if let Some(tags) = tags {
            self.set_tags(tags);
        }
        self.filter.scanned += page.scanned;
        mt_project::git::merge_log_page(&mut self.commits, page.commits);
        self.filter.cursor = page.next;
        self.has_more = self.filter.cursor.is_some();
        self.refresh_head_hash();
        if self.has_more && self.commits.len() < self.filter.want {
            self.load_page(false, cx);
        }
    }

    /// 触底:再要一页。
    fn load_more(&mut self, cx: &mut Context<Self>) {
        if self.has_more && !self.loading {
            if self.filter.active() {
                self.filter.want = self.commits.len() + PAGE_SIZE;
            }
            self.load_page(false, cx);
        }
    }

    /// HEAD 指向的提交:跟随分支时查分支列表,detached 时按括号里的短 hash 在已加载里找。
    fn refresh_head_hash(&mut self) {
        let Some(head) = self.head_branch.as_deref() else {
            self.head_hash = None;
            return;
        };
        self.head_hash = match head.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
            Some(short) => self
                .commits
                .iter()
                .find(|c| c.hash.starts_with(short))
                .map(|c| c.hash.clone()),
            None => self
                .branches
                .iter()
                .find(|b| !b.is_remote && b.name == head)
                .map(|b| b.commit_hash.clone()),
        };
    }

    /// HEAD 是不是分离头指针。
    fn head_detached(&self) -> bool {
        self.head_branch
            .as_deref()
            .is_some_and(|h| h.starts_with('('))
    }

    // ── 过滤 ──────────────────────────────────────────────

    fn schedule_filter(&mut self, cx: &mut Context<Self>) {
        self.filter.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(FILTER_DEBOUNCE_MS))
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| this.apply_filter(cx));
        }));
    }

    /// 把输入框里的关键词生效。与当前生效的相同就什么都不做(幂等:清空按钮与
    /// 去抖可能先后各来一次)。
    fn apply_filter(&mut self, cx: &mut Context<Self>) {
        self.filter.debounce = None;
        let text = self.filter.input.read(cx).value().trim().to_string();
        if text == self.filter.text {
            return;
        }
        self.filter.text = text;
        self.restart(cx);
    }

    fn clear_filter_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter
            .input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.apply_filter(cx);
    }

    /// 关键词 + 「仅我的」一起清掉。
    fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let had_mine = self.filter.only_mine;
        self.filter.only_mine = false;
        self.filter.no_email = false;
        self.filter
            .input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.filter.debounce = None;
        if !self.filter.text.is_empty() || had_mine {
            self.filter.text.clear();
            self.restart(cx);
        }
        cx.notify();
    }

    fn toggle_only_mine(&mut self, cx: &mut Context<Self>) {
        self.filter.only_mine = !self.filter.only_mine;
        if self.filter.only_mine {
            self.resolve_my_email(cx);
        } else {
            self.filter.no_email = false;
            if self.filter.email.is_some() {
                self.restart(cx);
            }
        }
        cx.notify();
    }

    /// 查当前仓库的 `user.email`,查完按新条件重来。
    fn resolve_my_email(&mut self, cx: &mut Context<Self>) {
        if self.repo_path.is_empty() {
            return;
        }
        let repo = PathBuf::from(&self.repo_path);
        let repo_key = self.repo_path.clone();
        cx.spawn(async move |this, cx| {
            let email = cx
                .background_executor()
                .spawn(async move { mt_project::git::config_user_email(&repo) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.repo_path != repo_key || !this.filter.only_mine {
                    return;
                }
                this.filter.no_email = email.is_none();
                this.filter.email = email;
                this.restart(cx);
            });
        })
        .detach();
    }

    // ── 选中 / 键盘 ───────────────────────────────────────

    fn selected_index(&self) -> Option<usize> {
        let hash = self.selected.as_deref()?;
        self.commits.iter().position(|c| c.hash == hash)
    }

    /// 选中某个提交并拉它的详情。
    fn select_hash(&mut self, hash: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.as_deref() != Some(hash.as_str()) {
            self.selected = Some(hash.clone());
            self.load_detail(hash, window, cx);
        }
        cx.notify();
    }

    /// 选中第 `index` 行,滚到看得见,接近末尾时预取下一页。
    fn select_index(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hash) = self.commits.get(index).map(|c| c.hash.clone()) else {
            return;
        };
        self.select_hash(hash, window, cx);
        self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        if index + PREFETCH_ROWS >= self.commits.len() {
            self.load_more(cx);
        }
    }

    fn move_selection(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.commits.is_empty() {
            return;
        }
        let last = self.commits.len() as isize - 1;
        let next = match self.selected_index() {
            None => 0,
            Some(cur) => (cur as isize + delta).clamp(0, last),
        };
        self.select_index(next as usize, window, cx);
    }

    fn on_select_prev(
        &mut self,
        _: &GitLogSelectPrev,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_selection(-1, window, cx);
    }

    fn on_select_next(
        &mut self,
        _: &GitLogSelectNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_selection(1, window, cx);
    }

    fn filter_focused(&self, window: &Window, cx: &App) -> bool {
        self.filter
            .input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
    }

    fn on_open_selected(
        &mut self,
        _: &GitLogOpenSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.filter_focused(window, cx) {
            self.apply_filter(cx);
            return;
        }
        // 焦点在详情区的只读文本里时,回车归它自己
        if !self.focus.is_focused(window) {
            cx.propagate();
            return;
        }
        if let Some(index) = self.selected_index() {
            self.view_commit(index, window, cx);
        }
    }

    fn on_copy_hash(&mut self, _: &GitLogCopyHash, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window) {
            cx.propagate();
            return;
        }
        if let Some(hash) = self.selected.clone() {
            cx.write_to_clipboard(ClipboardItem::new_string(hash));
        }
    }

    fn on_focus_filter(
        &mut self,
        _: &GitLogFocusFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.filter.input.update(cx, |s, cx| s.focus(window, cx));
    }

    fn on_clear_filter(
        &mut self,
        _: &GitLogClearFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 谓词 `GitHistory > Input` 也罩着详情区的只读正文:那里的 Esc 不归过滤框
        if !self.filter_focused(window, cx) {
            cx.propagate();
            return;
        }
        self.clear_filter_text(window, cx);
        window.focus(&self.focus, cx);
    }

    // ── 打开 diff ─────────────────────────────────────────

    /// 双击行 / Enter / 右键「查看变更」:先取文件列表,再开 CommitDiffModal。
    fn view_commit(&self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(commit) = self.commits.get(index) else {
            return;
        };
        self.open_commit_diff(
            commit.hash.clone(),
            commit.message.clone(),
            None,
            window,
            cx,
        );
    }

    /// 取文件列表 → 打开提交 diff(`initial` = 先选中哪个文件)。
    fn open_commit_diff(
        &self,
        hash: String,
        message: String,
        initial: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let repo = self.repo_path.clone();
        let store = self.store.clone();
        let repo_for_task = PathBuf::from(&repo);
        let hash_for_task = hash.clone();
        cx.spawn_in(window, async move |this, cx| {
            let files =
                cx.background_executor()
                    .spawn(async move {
                        mt_project::git::get_commit_files(&repo_for_task, &hash_for_task)
                    })
                    .await;
            let files = match files {
                Ok(files) => files,
                Err(err) => {
                    let _ = this.update(cx, |this: &mut Self, cx| {
                        this.toast_failure("action.viewChanges", &err, cx);
                    });
                    return;
                }
            };
            let _ = cx.update(|window, cx| match initial {
                None => git_diff::open_commit_diff(store, repo, hash, message, files, window, cx),
                Some(_) => git_diff::open_commit_diff_at(
                    store, repo, hash, message, files, initial, window, cx,
                ),
            });
        })
        .detach();
    }

    // ── 引用 ──────────────────────────────────────────────

    /// 该行要标注的分支胶囊。
    ///
    /// ⚠️ **只标注本工作区的分支**(`GitHistoryContent.tsx:166-169` 原注释):
    /// worktree 与主仓库共享 refs,标出全部分支会把其他工作区/远程的分支全挂到
    /// commit 上,看起来像本工作区持有它们。
    ///
    /// 唯一的例外是**正在展示的本地分支的上游**(`origin/main`):它与本地分支
    /// 胶囊同行就是「已推送」,落在下面几行就是「本地领先、还没推」,远端领先则
    /// 压根不在这条线的历史里。用户一眼就能看出最新代码有没有到远程。
    fn shown_branches(&self, hash: &str) -> Vec<&BranchInfo> {
        pick_shown_branches(
            &self.branches,
            self.head_branch.as_deref(),
            self.view_branch.as_deref(),
            hash,
        )
    }

    fn branch_kind(&self, branch: &BranchInfo) -> RefKind {
        if branch.is_remote {
            RefKind::Remote
        } else if Some(branch.name.as_str()) == self.head_branch.as_deref() {
            RefKind::Head
        } else {
            RefKind::Local
        }
    }

    /// 行上的引用胶囊:detached 的 HEAD → 分支(展示口径见 [`Self::shown_branches`])→ 标签。
    fn row_refs(&self, hash: &str) -> Vec<(String, RefKind)> {
        let mut refs = Vec::new();
        if self.head_detached() && self.head_hash.as_deref() == Some(hash) {
            refs.push((t("gitLog", "ref.head").to_string(), RefKind::Head));
        }
        for branch in self.shown_branches(hash) {
            refs.push((branch.name.clone(), self.branch_kind(branch)));
        }
        if let Some(tags) = self.tags_by_commit.get(hash) {
            refs.extend(tags.iter().map(|t| (t.clone(), RefKind::Tag)));
        }
        refs
    }

    /// 详情区的引用:指向它的**全部**分支(本地在前)+ 标签 —— 详情是查证用的,不做展示裁剪。
    fn all_refs(&self, hash: &str) -> Vec<(String, RefKind)> {
        let mut refs = Vec::new();
        if self.head_detached() && self.head_hash.as_deref() == Some(hash) {
            refs.push((t("gitLog", "ref.head").to_string(), RefKind::Head));
        }
        for branch in self.branches.iter().filter(|b| b.commit_hash == hash) {
            refs.push((branch.name.clone(), self.branch_kind(branch)));
        }
        refs.sort_by_key(|(_, kind)| match kind {
            RefKind::Head => 0,
            RefKind::Local => 1,
            RefKind::Remote => 2,
            RefKind::Tag => 3,
        });
        if let Some(tags) = self.tags_by_commit.get(hash) {
            refs.extend(tags.iter().map(|t| (t.clone(), RefKind::Tag)));
        }
        refs
    }

    // ── toast(走 Git 面板共用的结果 toast) ──────────────

    /// 成功摘要(信息档:`i` 图标,点击只关闭)。
    fn toast_info(&self, message: String, cx: &mut App) {
        git_ui::toast(&self.store, message, Tone::Info, cx);
    }

    /// 失败(需要留意那一档,正文带 git 的原文)。`action` 是 `gitLog.action.*` 的 key。
    fn toast_failure(&self, action: &'static str, err: &anyhow::Error, cx: &mut App) {
        eprintln!("[git] {action} 失败: {err:#}");
        let detail = error_text(err);
        git_ui::toast(
            &self.store,
            tr!(
                "gitLog",
                "toast.failed",
                action = t("gitLog", action),
                detail = detail
            ),
            Tone::Attention,
            cx,
        );
    }
}

/// 失败的可展示原因:git 的 stderr 原文收掉首尾空白;空的(git 没吭声就非零退出)给个兜底。
fn error_text(err: &anyhow::Error) -> String {
    let text = format!("{err:#}");
    match text.trim() {
        "" => "git failed".to_string(),
        trimmed => trimmed.to_string(),
    }
}

/// [`GitHistoryContent::merge_page`] 的纯函数体,抽出来好测。返回 `has_more`:
/// 本页满页**且**真的新增了提交 —— 整页重复时停下,免得拿同一个游标死循环请求。
fn merge_into(commits: &mut Vec<GitCommitInfo>, page: Vec<GitCommitInfo>) -> bool {
    let full_page = page.len() >= PAGE_SIZE;
    let added = mt_project::git::merge_log_page(commits, page);
    full_page && added > 0
}

/// [`GitHistoryContent::shown_branches`] 的纯函数体,抽出来好测。
fn pick_shown_branches<'a>(
    branches: &'a [BranchInfo],
    head_branch: Option<&str>,
    view_branch: Option<&str>,
    hash: &str,
) -> Vec<&'a BranchInfo> {
    let upstream = tracked_upstream(branches, head_branch, view_branch);
    branches
        .iter()
        .filter(|b| {
            b.is_head || Some(b.name.as_str()) == view_branch || Some(b.name.as_str()) == upstream
        })
        .filter(|b| b.commit_hash == hash)
        .collect()
}

/// 正在展示的本地分支关联的远程分支名。展示的是远程分支、本地分支没配上游、
/// detached HEAD(`head_branch` 是短 hash,分支列表里查不到)时都是 `None`。
fn tracked_upstream<'a>(
    branches: &'a [BranchInfo],
    head_branch: Option<&str>,
    view_branch: Option<&str>,
) -> Option<&'a str> {
    let shown = view_branch.or(head_branch)?;
    branches
        .iter()
        .find(|b| !b.is_remote && b.name == shown)?
        .upstream
        .as_deref()
}

/// 相对时间(`src/utils/timeFormat.ts:3-18`)。
///
/// ⚠️ 命名空间是 **`time`**,不是 `session_panel` 用的 `sessionList.time.*` ——
/// 两套 key 并存,别串。30 天以上是**纯数字日期**,没有 `time.monthDay` 这种 key。
pub fn format_relative_time(timestamp: i64, now: i64) -> String {
    let diff = now - timestamp;
    if diff < 60 {
        return t("time", "justNow").to_string();
    }
    if diff < 3600 {
        return tr!("time", "minutesAgo", n = (diff / 60).to_string());
    }
    if diff < 86400 {
        return tr!("time", "hoursAgo", n = (diff / 3600).to_string());
    }
    if diff < 2_592_000 {
        return tr!("time", "daysAgo", n = (diff / 86400).to_string());
    }
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(timestamp, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        // 时间戳坏到本地时区都换算不出来时退回原值,别 panic
        None => timestamp.to_string(),
    }
}

/// 绝对时间(本地时区,精确到分):行 tooltip、详情区用。
pub(crate) fn format_absolute_time(timestamp: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(timestamp, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => timestamp.to_string(),
    }
}

/// 行 tooltip:完整标题 + 作者 · 绝对时间 · 短哈希(标题被截断时尤其需要)。
fn row_tooltip(commit: &GitCommitInfo) -> String {
    format!(
        "{}\n{} · {} · {}",
        commit.message,
        commit.author,
        format_absolute_time(commit.timestamp),
        commit.short_hash
    )
}

impl Render for GitHistoryContent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        let mut root = div()
            .id("git-history")
            .key_context("GitHistory")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(ui::bg_surface())
            .on_action(cx.listener(Self::on_select_prev))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_open_selected))
            .on_action(cx.listener(Self::on_copy_hash))
            .on_action(cx.listener(Self::on_focus_filter))
            .on_action(cx.listener(Self::on_clear_filter))
            // 量宽度:窄的时候行里只留作者头像
            .child({
                let this = this.clone();
                canvas(
                    move |bounds: Bounds<Pixels>, _window, cx| {
                        this.update(cx, |h: &mut GitHistoryContent, _cx| {
                            h.width = f32::from(bounds.size.width);
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            });

        if self.repo_path.is_empty() {
            return root.child(
                empty_state(ICON_COMMIT, t("gitHistoryContent", "noRepos"), None).size_full(),
            );
        }

        root = root.child(self.render_filter_bar(cx));

        let list = self.render_list(window, cx);
        let has_detail = self.selected.is_some();
        let detail_open = has_detail && self.detail.open;
        root = root.child(grow(
            div()
                .flex_basis(px(0.0))
                .min_h(px(0.0))
                .overflow_hidden()
                .px(px(4.0))
                .py(px(4.0))
                .child(list),
            if detail_open { 0.58 } else { 1.0 },
        ));
        if has_detail {
            root = root.child(detail::render(self, detail_open, cx));
        }
        root
    }
}

impl GitHistoryContent {
    /// 过滤栏:输入框(左搜索图标、右清空)+「仅我的」胶囊 + 计数 / 进行中状态 + 清除。
    fn render_filter_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let has_text = !self.filter.input.read(cx).value().is_empty();
        let active = self.filter.active() || self.filter.only_mine;
        let clear_text = has_text.then(|| {
            icon_button(
                "git-log-filter-clear-text",
                ICON_CLOSE,
                t("gitLog", "filter.clearText"),
                true,
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.clear_filter_text(window, cx);
            }))
        });
        let input = div().flex_1().min_w_0().child(
            Input::new(&self.filter.input)
                .prefix(div().pl(px(2.0)).child(icon(
                    ICON_SEARCH,
                    git_ui::ICON_SIZE,
                    ui::text_muted(),
                )))
                .when_some(clear_text, |el, btn| el.suffix(btn)),
        );

        let mine = self.filter.only_mine;
        let mine_color = if self.filter.no_email {
            ui::color_warning()
        } else if mine {
            ui::accent()
        } else {
            ui::text_secondary()
        };
        let mine_tip: SharedString = if self.filter.no_email {
            t("gitLog", "filter.noEmail").into()
        } else if let Some(email) = self.filter.email.as_deref().filter(|_| mine) {
            tr!("gitLog", "filter.onlyMineTip", email = email).into()
        } else {
            t("gitLog", "filter.onlyMine").into()
        };
        let mine_pill = div()
            .id("git-log-only-mine")
            .flex_none()
            .h(px(26.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .rounded_full()
            .border_1()
            .cursor_pointer()
            .text_size(ui::font_px(12.0))
            .text_color(mine_color)
            .map(|el| {
                if mine {
                    el.bg(git_ui::soft_bg(mine_color))
                        .text_color(git_ui::soft_fg(mine_color))
                        .border_color(ui::with_alpha(mine_color, 0.5))
                } else {
                    el.border_color(ui::border_default())
                        .hover(|el| el.bg(git_ui::button_hover_bg()))
                }
            })
            .child(icon(ICON_USER, git_ui::ICON_SIZE_SM, mine_color))
            .child(t("gitLog", "filter.onlyMine"))
            .tip(mine_tip)
            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                this.toggle_only_mine(cx);
            }));

        // 右侧状态位:进行中的改写操作 > 刷新失败(旧列表还留着时) > 过滤计数 > 刷新中
        let refresh_error = self
            .load_error
            .as_deref()
            .filter(|_| !self.commits.is_empty());
        let status: Option<String> = if let Some(action) = self.busy {
            Some(tr!("gitLog", "busy", action = t("gitLog", action)))
        } else if let Some(err) = refresh_error {
            Some(tr!("gitLog", "loadFailed", detail = err))
        } else if self.filter.active() {
            // 首批还没回来时不写「匹配 0 条」(列表区已经在说「搜索中…」)
            let count = self.commits.len().to_string();
            (!self.commits.is_empty() || !(self.loading || self.has_more)).then(|| {
                if self.has_more {
                    tr!("gitLog", "filter.countLoaded", count = count)
                } else {
                    tr!("gitLog", "filter.count", count = count)
                }
            })
        } else if self.refreshing {
            Some(t("gitLog", "refreshing").to_string())
        } else {
            None
        };

        div()
            .flex_none()
            .h(px(44.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(input)
            .child(mine_pill)
            .when_some(status, |el, status| {
                let color = if self.busy.is_some() {
                    ui::accent()
                } else if refresh_error.is_some() {
                    ui::color_error()
                } else {
                    ui::text_muted()
                };
                el.child(
                    mono(div())
                        .id("git-log-status")
                        .flex_none()
                        .max_w(px(160.0))
                        .truncate()
                        .text_size(ui::font_px(12.0))
                        .text_color(color)
                        // 截断时全文挂 tooltip(刷新失败的 git 原文可能很长)
                        .tip(status.clone())
                        .child(status),
                )
            })
            .when(active, |el| {
                let clear = git_ui::action_button(
                    "git-log-filter-clear",
                    t("gitLog", "filter.clear"),
                    ButtonKind::Ghost,
                    true,
                );
                el.child(
                    clear.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.clear_filter(window, cx);
                    })),
                )
            })
            .into_any_element()
    }

    /// 列表区:空态 / uniform_list(末尾可能挂一行「加载中 / 搜索中」)。
    fn render_list(&self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.commits.is_empty() {
            return self.render_empty(cx);
        }
        // 刷新(保留旧列表)期间不挂这一行:那一趟取的是首页,状态已标在过滤栏右侧
        let footer: Option<String> = (self.loading && !self.refreshing).then(|| {
            if self.filter.active() {
                tr!(
                    "gitLog",
                    "filter.searching",
                    count = self.filter.scanned.to_string()
                )
            } else {
                t("gitLog", "loadingMore").to_string()
            }
        });
        let commits = self.commits.len();
        let count = commits + usize::from(footer.is_some());
        let this = cx.entity();
        uniform_list(
            "git-commit-list",
            count,
            move |range, _window, cx: &mut App| {
                // 触底再要一页。**延到下一拍**执行:这里还在窗口的 prepaint 里,
                // 直接 update 自己会与正在进行的渲染打架。
                if range.end >= commits {
                    let this = this.clone();
                    cx.spawn(async move |cx| {
                        this.update(cx, |this: &mut GitHistoryContent, cx| this.load_more(cx));
                    })
                    .detach();
                }
                let this = this.clone();
                let footer = footer.clone();
                range
                    .map(|i| {
                        if i >= commits {
                            return footer_row(footer.clone().unwrap_or_default());
                        }
                        this.update(cx, |this, cx| this.render_commit(i, cx))
                    })
                    .collect::<Vec<_>>()
            },
        )
        .track_scroll(&self.scroll)
        .size_full()
        .into_any_element()
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(err) = &self.load_error {
            return git_ui::error_state(tr!("gitLog", "loadFailed", detail = err.clone()))
                .into_any_element();
        }
        if self.filter.active() {
            if self.loading || self.has_more {
                return git_ui::loading_state(tr!(
                    "gitLog",
                    "filter.searching",
                    count = self.filter.scanned.to_string()
                ))
                .size_full()
                .into_any_element();
            }
            let clear = git_ui::action_button(
                "git-log-empty-clear",
                t("gitLog", "filter.clearFilter"),
                ButtonKind::Primary,
                true,
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.clear_filter(window, cx);
            }))
            .into_any_element();
            return empty_state(ICON_SEARCH, t("gitLog", "filter.noMatch"), Some(clear))
                .size_full()
                .into_any_element();
        }
        if self.loading {
            return git_ui::loading_state(t("gitHistoryContent", "loading"))
                .size_full()
                .into_any_element();
        }
        empty_state(ICON_COMMIT, t("gitHistoryContent", "noCommits"), None)
            .size_full()
            .into_any_element()
    }

    fn render_commit(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(commit) = self.commits.get(index) else {
            return div().into_any_element();
        };
        let show_graph = !self.filter.active();
        let graph_row = if show_graph {
            self.graph.rows.get(index).cloned()
        } else {
            None
        };
        let selected = self.selected.as_deref() == Some(commit.hash.as_str());
        let is_head = self.head_hash.as_deref() == Some(commit.hash.as_str());
        let is_merge = commit.parent_hashes.len() >= 2;
        let now = chrono::Utc::now().timestamp();
        let relative = format_relative_time(commit.timestamp, now);
        let narrow = self.width > 0.0 && self.width < NARROW_WIDTH;
        let hash_full = commit.hash.clone();
        let short_hash = commit.short_hash.clone();
        let refs = self.row_refs(&commit.hash);

        // 共用列表行(选中 = accent 淡底 + 左侧竖条、悬停薄纱底)。
        // ⚠️ `w_full` 不能省(`list_row` 已带):行是 uniform_list 的 item,单独
        // `layout_as_root`,而 taffy 对没有已知主轴尺寸的 flex 根节点按**内容**宽度算、
        // 不吃 available space。没有它整行只有内容那么宽,`flex_1` 的标题是 0。
        let mut row = git_ui::list_row(
            SharedString::from(format!("git-commit-{hash_full}")),
            selected,
        )
        .h(px(GRAPH_ROW_HEIGHT))
        .pl(px(if graph_row.is_some() { 4.0 } else { 10.0 }));
        if let Some(graph_row) = graph_row {
            row = row.child(GraphCell {
                row: graph_row,
                width: self.graph.width,
                is_head,
                selected,
            });
        }
        row = row.child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(13.0))
                // 合并提交降低存在感
                .text_color(if is_merge {
                    ui::text_muted()
                } else {
                    ui::text_primary()
                })
                .child(commit.message.clone()),
        );

        // 引用胶囊:最多 MAX_PILLS 个(窄时 1 个),其余收进「+N」,全名挂 tooltip
        let max_pills = if narrow { 1 } else { MAX_PILLS };
        for (i, (name, kind)) in refs.iter().take(max_pills).enumerate() {
            row = row.child(
                ref_pill(
                    SharedString::from(format!("git-ref-{short_hash}-{i}")),
                    name.clone(),
                    *kind,
                )
                .max_w(px(160.0))
                .tip(name.clone()),
            );
        }
        if refs.len() > max_pills {
            let all = refs
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            row = row.child(
                more_pill(
                    SharedString::from(format!("git-ref-more-{short_hash}")),
                    refs.len() - max_pills,
                )
                .tip(all),
            );
        }

        row = row
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(avatar(&commit.author))
                    .when(!narrow, |el| {
                        el.child(
                            div()
                                .w(px(72.0))
                                .min_w_0()
                                .truncate()
                                .text_size(ui::font_px(12.0))
                                .text_color(ui::text_muted())
                                .child(commit.author.clone()),
                        )
                    }),
            )
            .child(
                mono(div())
                    .flex_none()
                    .w(px(76.0))
                    .text_right()
                    .truncate()
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(relative),
            )
            .tip(row_tooltip(commit));

        let hash_for_click = hash_full.clone();
        row.on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
            window.focus(&this.focus, cx);
            // ⚠️ 双击打开 diff(`GitHistoryContent.tsx:105`);单击 = 选中
            if event.click_count() >= 2 {
                this.view_commit(index, window, cx);
            } else {
                this.select_hash(hash_for_click.clone(), window, cx);
            }
        }))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                window.focus(&this.focus, cx);
                this.select_hash(hash_full.clone(), window, cx);
                actions::open_commit_menu(this, index, event.position, window, cx);
            }),
        )
        .into_any_element()
    }
}

/// 列表末尾那一行「加载更多… / 搜索中…」(共用的加载提示行)。高度与提交行一致
/// (uniform_list 按行等高排)。
fn footer_row(text: String) -> AnyElement {
    div()
        .w_full()
        .h(px(GRAPH_ROW_HEIGHT))
        .flex()
        .items_center()
        .justify_center()
        .child(git_ui::busy_line(text))
        .into_any_element()
}

// ─── 拓扑图单元格(自绘) ─────────────────────────────────────

/// 一行的拓扑图。宽度 = [`GraphLayout::width`],高度恒为 [`GRAPH_ROW_HEIGHT`]。
///
/// # 渐变的取舍
///
/// 原版每条异色线段挂一个 `<linearGradient>`(三个 stop:0% color / 70% color /
/// 100% endColor)。gpui 的 `paint_path` 只吃单色,这里改成**分段近似**:
/// 把路径按参数 t 切成若干小段,`t < 0.7` 用起点色,之后线性插值到终点色。
/// 渐变是纯装饰(分支线并入主线时的过渡),分段够密时肉眼无差。
struct GraphCell {
    row: GraphRow,
    width: f32,
    /// HEAD 所在的提交:节点外加一圈 accent 环。
    is_head: bool,
    /// 行被选中(合并节点的空心底要跟着行底换色)。
    selected: bool,
}

/// 渐变分段数。段与段之间留一点重叠(见 `paint`),8 段在行高上已看不出台阶。
const GRADIENT_STEPS: usize = 8;
/// 连线线宽。
const GRAPH_STROKE: f32 = 1.75;
/// 节点半径。
const NODE_RADIUS: f32 = 4.5;

impl IntoElement for GraphCell {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

fn lerp_color(a: Hsla, b: Hsla, t: f32) -> Hsla {
    // 在 RGB 上插值:HSL 上插值会绕色相环(蓝→绿会经过青,原版 SVG 是 RGB 插值)
    let (a, b) = (gpui::Rgba::from(a), gpui::Rgba::from(b));
    gpui::Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
    .into()
}

/// 贝塞尔 / 直线上按参数 t 取点。
fn sample(path: &SegPath, t: f32) -> (f32, f32) {
    match path {
        SegPath::Line { x, y0, y1 } => (*x, y0 + (y1 - y0) * t),
        SegPath::Cubic { p0, c1, c2, p1 } => {
            let u = 1.0 - t;
            let (b0, b1, b2, b3) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            (
                b0 * p0.0 + b1 * c1.0 + b2 * c2.0 + b3 * p1.0,
                b0 * p0.1 + b1 * c1.1 + b2 * c2.1 + b3 * p1.1,
            )
        }
    }
}

impl Element for GraphCell {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = px(self.width).into();
        style.size.height = px(GRAPH_ROW_HEIGHT).into();
        style.flex_shrink = 0.0;
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _layout: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) {
        let origin = bounds.origin;
        let map =
            |(x, y): (f32, f32)| -> Point<Pixels> { point(origin.x + px(x), origin.y + px(y)) };

        for seg in &self.row.segments {
            let Some(path) = segment_path(seg, self.row.lane) else {
                continue;
            };
            let color = palette_color(seg.color);
            if !git_graph::needs_gradient(seg) {
                let mut builder = PathBuilder::stroke(px(GRAPH_STROKE));
                match path {
                    SegPath::Line { x, y0, y1 } => {
                        builder.move_to(map((x, y0)));
                        builder.line_to(map((x, y1)));
                    }
                    SegPath::Cubic { p0, c1, c2, p1 } => {
                        builder.move_to(map(p0));
                        builder.cubic_bezier_to(map(p1), map(c1), map(c2));
                    }
                }
                if let Ok(built) = builder.build() {
                    window.paint_path(built, color);
                }
                continue;
            }

            // 异色:按 t 分段,0..0.7 用起点色,之后插值到终点色
            let end = palette_color(seg.end_color.unwrap_or(seg.color));
            for step in 0..GRADIENT_STEPS {
                let t0 = step as f32 / GRADIENT_STEPS as f32;
                // 段尾多伸一点点,免得相邻段之间露出缝
                let t1 = ((step + 1) as f32 / GRADIENT_STEPS as f32 + 0.02).min(1.0);
                let mid = (t0 + t1) * 0.5;
                let color = if mid <= 0.7 {
                    color
                } else {
                    lerp_color(color, end, (mid - 0.7) / 0.3)
                };
                let mut builder = PathBuilder::stroke(px(GRAPH_STROKE));
                builder.move_to(map(sample(&path, t0)));
                // 分段本身用折线近似:一段只占整条曲线的 1/8,直线误差 < 0.5px
                builder.line_to(map(sample(&path, mid)));
                builder.line_to(map(sample(&path, t1)));
                if let Ok(built) = builder.build() {
                    window.paint_path(built, color);
                }
            }
        }

        // 节点圆(`GitHistoryContent.tsx:68-75`)
        let cx_pos = git_graph::lane_x(self.row.lane as i32);
        let cy = GRAPH_ROW_HEIGHT / 2.0;
        let color = palette_color(self.row.color);
        if self.is_head {
            // HEAD:节点外一圈 accent 环(外沿 7.25px,不压到相邻 lane 的线)
            paint_circle(
                window,
                map,
                (cx_pos, cy),
                NODE_RADIUS + 2.0,
                None,
                ui::accent(),
            );
        }
        if self.row.is_merge {
            // 合并:空心圈。先垫一层与行底同色的实心,免得连线从圈里穿过;
            // 选中行的底是面板底 + accent_subtle 叠加,这里照样叠两层
            for ground in
                std::iter::once(ui::bg_surface()).chain(self.selected.then(ui::accent_subtle))
            {
                paint_circle(window, map, (cx_pos, cy), NODE_RADIUS, Some(()), ground);
            }
            paint_circle(window, map, (cx_pos, cy), NODE_RADIUS - 0.75, None, color);
        } else {
            paint_circle(window, map, (cx_pos, cy), NODE_RADIUS, Some(()), color);
        }
    }
}

/// 画一个圆。`fill` 为 `Some` 时实心,否则 1.5px 描边。
fn paint_circle(
    window: &mut Window,
    map: impl Fn((f32, f32)) -> Point<Pixels>,
    center: (f32, f32),
    r: f32,
    fill: Option<()>,
    color: Hsla,
) {
    const SEGMENTS: usize = 24;
    let mut builder = match fill {
        Some(()) => PathBuilder::fill(),
        None => PathBuilder::stroke(px(1.5)),
    };
    for i in 0..SEGMENTS {
        let theta = i as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
        let p = map((center.0 + r * theta.cos(), center.1 + r * theta.sin()));
        if i == 0 {
            builder.move_to(p);
        } else {
            builder.line_to(p);
        }
    }
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_graph::test_commit;

    /// 去重(防线):重复 hash 必须丢掉,否则拓扑图连线会算错;整页重复时
    /// 停止分页,免得拿同一个游标死循环。
    #[test]
    fn 分页去重与_has_more() {
        let mut commits = Vec::new();
        // 满页 30 条 → 还有更多
        let page: Vec<_> = (0..PAGE_SIZE)
            .map(|i| test_commit(&format!("c{i}"), &[]))
            .collect();
        assert!(merge_into(&mut commits, page.clone()));
        assert_eq!(commits.len(), PAGE_SIZE);

        // 整页都是重复的 → 合并后长度没涨 → 停止分页
        assert!(!merge_into(&mut commits, page.clone()));
        assert_eq!(commits.len(), PAGE_SIZE, "重复 hash 必须丢掉");

        // 不满页 → 到底了
        assert!(!merge_into(&mut commits, vec![test_commit("tail", &[])]));
        assert_eq!(commits.len(), PAGE_SIZE + 1);
    }

    /// 迟到的子提交(时钟偏移):它的父已经在上一页展示了,合并时要把它提到父
    /// 前面 —— 拓扑图按「父永远在子之后」自上而下派 lane,倒挂的话父那条线
    /// 永远等不到人。
    #[test]
    fn 迟到的子提交合并后排到父前面() {
        let mut commits = vec![
            test_commit("m", &["a", "late"]),
            test_commit("a", &["base"]),
            test_commit("base", &[]),
        ];
        merge_into(&mut commits, vec![test_commit("late", &["base"])]);
        let order: Vec<_> = commits.iter().map(|c| c.hash.as_str()).collect();
        assert_eq!(order, ["m", "a", "late", "base"]);
        // 末行(根提交 base)不再往下派线。倒挂时末行是 late,它派给 base 的那条线
        // 往下空等一个永远不会再出现的父,图上就是一根断在底边的线
        let layout = git_graph::compute(&commits);
        let last = layout.rows.last().unwrap();
        assert!(last.segments.iter().all(|s| s.from != -1), "{last:?}");
    }

    /// `origin/` 开头的当远程分支,与 `get_repo_branches` 的产出形态一致。
    fn branch(name: &str, hash: &str, is_head: bool, upstream: Option<&str>) -> BranchInfo {
        BranchInfo {
            name: name.into(),
            is_head,
            is_remote: name.starts_with("origin/"),
            commit_hash: hash.into(),
            upstream: upstream.map(str::to_string),
        }
    }

    fn names(picked: &[&BranchInfo]) -> Vec<String> {
        picked.iter().map(|b| b.name.clone()).collect()
    }

    /// 跟随 HEAD 时,HEAD 分支的上游胶囊也要挂上:本地领先一格时 `origin/main`
    /// 落在下一行(一眼看出没推),推完两者同行。其它远程分支照旧不挂。
    #[test]
    fn 上游胶囊跟着展示中的本地分支() {
        let branches = vec![
            branch("main", "c2", true, Some("origin/main")),
            branch("dev", "c0", false, Some("origin/dev")),
            branch("origin/main", "c1", false, None),
            branch("origin/dev", "c0", false, None),
        ];
        assert_eq!(
            names(&pick_shown_branches(&branches, Some("main"), None, "c2")),
            vec!["main"]
        );
        assert_eq!(
            names(&pick_shown_branches(&branches, Some("main"), None, "c1")),
            vec!["origin/main"],
            "本地领先:上游落在旧 commit 上"
        );
        // c0 上有 dev / origin/dev,但展示的是 main,一个都不挂
        assert!(pick_shown_branches(&branches, Some("main"), None, "c0").is_empty());

        // 推完:上游前移到 c2,与 main 同行,顺序本地在前远程在后
        let pushed = vec![
            branch("main", "c2", true, Some("origin/main")),
            branch("origin/main", "c2", false, None),
        ];
        assert_eq!(
            names(&pick_shown_branches(&pushed, Some("main"), None, "c2")),
            vec!["main", "origin/main"]
        );
    }

    /// 查看别的本地分支:挂它自己 + 它的上游,HEAD 胶囊照旧保留;
    /// 查看的是远程分支 / 分支没配上游 / detached HEAD 都不再多挂。
    #[test]
    fn 上游只认本地分支() {
        let branches = vec![
            branch("main", "c2", true, Some("origin/main")),
            branch("dev", "c0", false, Some("origin/dev")),
            branch("lonely", "c3", false, None),
            branch("origin/main", "c1", false, None),
            branch("origin/dev", "c0", false, None),
        ];
        // 查看 dev:dev 与 origin/dev 同行,origin/main 不再挂
        assert_eq!(
            names(&pick_shown_branches(
                &branches,
                Some("main"),
                Some("dev"),
                "c0"
            )),
            vec!["dev", "origin/dev"]
        );
        assert!(pick_shown_branches(&branches, Some("main"), Some("dev"), "c1").is_empty());
        // 查看远程分支 origin/dev:只挂它自己(与 HEAD)
        assert_eq!(
            names(&pick_shown_branches(
                &branches,
                Some("main"),
                Some("origin/dev"),
                "c0"
            )),
            vec!["origin/dev"]
        );
        // 没配上游的本地分支:只有它自己
        assert_eq!(
            tracked_upstream(&branches, Some("main"), Some("lonely")),
            None
        );
        // detached HEAD:head_branch 是短 hash,查不到 → 没有上游
        assert_eq!(tracked_upstream(&branches, Some("(1a2b3c4)"), None), None);
        assert_eq!(tracked_upstream(&branches, None, None), None);
    }

    /// 相对时间四个档位的边界(59/60/3599/3600/86399/86400/2591999/2592000)。
    #[test]
    fn 相对时间四档边界() {
        let now = 1_700_000_000i64;
        // 「刚刚」这一档没有占位符,直接比字面量
        assert_eq!(format_relative_time(now, now), t("time", "justNow"));
        assert_eq!(format_relative_time(now - 59, now), t("time", "justNow"));

        // 分 / 时 / 天三档各自与 `tr!` 的展开一致
        assert_eq!(
            format_relative_time(now - 60, now),
            tr!("time", "minutesAgo", n = "1".to_string())
        );
        assert_eq!(
            format_relative_time(now - 3599, now),
            tr!("time", "minutesAgo", n = "59".to_string())
        );
        assert_eq!(
            format_relative_time(now - 3600, now),
            tr!("time", "hoursAgo", n = "1".to_string())
        );
        assert_eq!(
            format_relative_time(now - 86_399, now),
            tr!("time", "hoursAgo", n = "23".to_string())
        );
        assert_eq!(
            format_relative_time(now - 86_400, now),
            tr!("time", "daysAgo", n = "1".to_string())
        );
        assert_eq!(
            format_relative_time(now - 2_591_999, now),
            tr!("time", "daysAgo", n = "29".to_string())
        );

        // 满 30 天换成纯数字日期(YYYY-MM-DD),没有任何本地化词条
        let date = format_relative_time(now - 2_592_000, now);
        assert_eq!(date.len(), 10, "{date}");
        assert_eq!(date.matches('-').count(), 2, "{date}");
    }

    /// 绝对时间精确到分;行 tooltip 三段齐全。
    #[test]
    fn 绝对时间与行提示() {
        let abs = format_absolute_time(1_700_000_000);
        assert_eq!(abs.len(), 16, "{abs}");
        let mut c = test_commit("abcdef1234", &[]);
        c.message = "fix: something".into();
        let tip = row_tooltip(&c);
        let lines: Vec<_> = tip.lines().collect();
        assert_eq!(lines[0], "fix: something");
        assert!(lines[1].starts_with("tester · ") && lines[1].ends_with("abcdef1"));
    }

    /// 过滤条件:「仅我的」查不到邮箱时不算生效;两项都给时一起带下去。
    #[test]
    fn 过滤条件是否生效() {
        assert!(!filter_active("", false, None));
        assert!(!filter_active("", true, None), "没有邮箱不算");
        assert!(filter_active("", true, Some("me@x.y")));
        assert!(
            !filter_active("", false, Some("me@x.y")),
            "开关关着邮箱不算"
        );
        assert!(filter_active("fix", false, None));
    }

    /// 贝塞尔采样:两端点必须落在路径端点上(渐变分段接得住)。
    #[test]
    fn 贝塞尔采样端点对齐() {
        let cubic = SegPath::Cubic {
            p0: (7.0, 0.0),
            c1: (7.0, 12.0),
            c2: (21.0, 12.0),
            p1: (21.0, 24.0),
        };
        assert_eq!(sample(&cubic, 0.0), (7.0, 0.0));
        let end = sample(&cubic, 1.0);
        assert!((end.0 - 21.0).abs() < 0.001 && (end.1 - 24.0).abs() < 0.001);
        // 中点落在两端之间
        let mid = sample(&cubic, 0.5);
        assert!(mid.0 > 7.0 && mid.0 < 21.0);

        let line = SegPath::Line {
            x: 7.0,
            y0: 24.0,
            y1: 48.0,
        };
        assert_eq!(sample(&line, 0.0), (7.0, 24.0));
        assert_eq!(sample(&line, 1.0), (7.0, 48.0));
        assert_eq!(sample(&line, 0.5), (7.0, 36.0));
    }
}
