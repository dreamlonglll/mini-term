//! 「提交详情」栏(IDEA Log 右侧那块):完整消息、作者 / 提交者 / 时间 / 哈希 / 父提交 /
//! 引用 / 所在分支,以及带增删行数的改动文件。
//!
//! # 单独一栏,挂在 Git 抽屉左边
//!
//! 单击提交行后,详情在 Git 抽屉**左侧**单独展开一栏,盖在终端之上:随抽屉滑进滑出、
//! 随抽屉拖宽左移,自己的宽度也可拖、落 layout.db(外壳、动画与拖拽在 `main.rs` 的
//! `render_commit_detail_layer`)。此前它挤在历史区底部,抽屉那么窄、只分到四成高度,
//! 正文与文件列表都得滚着看。
//!
//! 栏里画什么仍由历史区的选择推出来,状态([`DetailState`])也仍是 [`GitHistoryContent`]
//! 的字段 —— 渲染要读的提交列表、选择、引用,点父提交跳转、点文件开 diff 都在那边。
//! [`CommitDetailPane`] 只是把它画进另一棵子树的视图壳:渲染时借历史区实体来画,历史区
//! 一 notify 它就跟着重画。
//!
//! 开合:单击提交行就打开(关着也重新打开);标题条的 × 或焦点在历史列表时按 Esc 关上,
//! 关上之后 ↑/↓ 换选中不再自己弹出来,直到再单击一行。没有选中(换了仓库)、「提交历史」
//! 区收着、面板在空态、抽屉换到别的面板时都不画(见 `GitPanel::detail_pane`)。
//!
//! # 取数
//!
//! 选中 → 后台拉 [`mt_project::git::get_commit_detail`];快速按 ↑/↓ 连换时用请求令牌丢掉
//! 迟到的响应。切换期间先留着上一份内容(本地仓库通常十几毫秒就回来),不清空成「加载中」,
//! 免得详情一闪一闪。
//!
//! 两项补充信息与详情**并行**单独取(为什么单独取见 `mt_project::git::commit_extras`):
//! 逐文件增删行数([`mt_project::git::get_commit_line_stats`])与包含它的分支
//! ([`mt_project::git::get_branches_containing`])。回来的先后不定,各自记着是哪个提交的,
//! 画的时候与详情对上哈希才用;行数没回来时文件行先不带数字。分支列表变了(提交、推送、
//! 拉取之后)单独重查「所在分支」([`GitHistoryContent::refresh_containing`])。
//!
//! 正文放在**只读**的多行输入框里:能选中、能复制,长段落自动换行。取数在后台、回包时
//! 手上没有窗口,正文先记下来,下一帧渲染时再写进框里([`DetailState::flush_body`])。
//!
//! # 多选
//!
//! 选中两个及以上提交时换成多选态(issue #84,对标 IDEA Log):「已选 N 个提交」+ 选中
//! 提交的小列表 + 这些提交**合并后**的改动文件([`mt_project::git::get_combined_commit_files`],
//! 合并口径见那边的模块注释)。同样后台取、同一个请求令牌防迟到;最多合并
//! [`MULTI_LIMIT`] 个提交(取列表里最新的那些),超出时写明。点文件打开合并 diff。
//!
//! # 改动文件
//!
//! 走共用的目录树(`git_ui::build_change_tree`:单链目录压缩、目录行带文件数),与提交
//! diff 左栏共用一个「列表 / 树形」开关(配置 `gitCommitFilesViewMode`,缺省树形)。
//! 最多直接画 [`MAX_FILE_ROWS`] 行(按拍平后的行数算),其余进提交 diff 里看。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, point,
    prelude::FluentBuilder as _, px,
};
use gpui_component::Size;
use gpui_component::input::{Textarea, TextareaState};
use mt_project::git::{
    CombinedFileChange, CommitDetail, CommitLineStats, ContainingBranch, FileLineStat,
};
use mt_ui::tooltip::TooltipExt as _;

use super::{
    GitHistoryContent, MULTI_LIMIT, format_detail_time, format_relative_time, row_tooltip,
};
use crate::git_ui::{
    self, ChangeTreeRow, ICON_CLOSE, ICON_COMMIT, ICON_COPY, ICON_DIFF, RefKind, avatar,
    error_text, icon, icon_button, mono, more_pill, ref_pill,
};
use crate::i18n::{t, tr};
use crate::ui;

/// 详情里的文件最多直接画多少行(拍平后的行数,目录行也算;不虚拟化,再多去提交 diff
/// 里看)。
const MAX_FILE_ROWS: usize = 300;
/// 多选态的提交小列表最多列几条,其余写「还有 N 个提交」。
const MULTI_LIST_ROWS: usize = 12;
/// 正文只读框最多长到几行(再长框内滚动)。栏本身能滚,给得宽一些。
const BODY_MAX_ROWS: usize = 40;
/// 「所在分支」直接画几个胶囊,其余收进「+N」(悬停列出全部)。
const BRANCH_PILLS: usize = 12;
/// 元信息左列标签的宽(四个汉字 / 「Committed」都放得下)。
const META_LABEL_W: f32 = 72.0;
/// 标题条高度:与抽屉的标题条同高,两栏并排时下边框接成一条线。
const HEADER_H: f32 = 36.0;

/// 详情区要展示的是什么(由历史区的选择推出来,见 `GitHistoryContent::sync_detail`)。
#[derive(Debug, Clone, PartialEq)]
pub(super) enum DetailTarget {
    Single(String),
    /// 参与合并的提交,列表顺序,最多 [`MULTI_LIMIT`] 个。
    Multi(Vec<String>),
}

/// 多选态合并好的改动文件。
struct MultiDetail {
    /// 这一份是哪组提交合并出来的(列表顺序)。
    hashes: Vec<String>,
    result: Result<Vec<CombinedFileChange>, String>,
}

pub(super) struct DetailState {
    /// 栏开着(单击提交行打开,× / Esc 关上;见模块注释)。
    pub(super) open: bool,
    loading: bool,
    error: Option<String>,
    data: Option<CommitDetail>,
    multi: Option<MultiDetail>,
    /// 逐文件增删行数,记着是哪个提交的(文件太多不算、取失败时都不更新)。
    stats: Option<(String, CommitLineStats)>,
    /// 所在分支,记着是哪个提交的。
    containing: Option<(String, Result<Vec<ContainingBranch>, String>)>,
    /// 「所在分支」自己的请求令牌:分支列表变了会单独重查,不跟详情共用。
    containing_request: u64,
    /// 最近一次要的是什么(同一个就不重拉,见 [`Self::retarget`])。
    target: Option<DetailTarget>,
    /// 眼下画出来的是哪一份(换了一份才回顶、展开全部目录)。
    shown: Option<DetailTarget>,
    /// 请求令牌:连着换选中时,迟到的详情丢弃。单选 / 多选共用。
    request: u64,
    /// 只读正文(可选中复制)。
    body: Entity<TextareaState>,
    /// 取回来、还没写进正文框的正文(见模块注释)。
    pending_body: Option<String>,
    /// 改动文件树里收起的目录(完整路径)。换了一份详情就全部展开。
    collapsed: HashSet<String>,
    /// 详情区的滚动位置。换了一个提交就回到顶上(标题与元信息在最上面),
    /// 不然上一个提交往下翻过,新提交一进来先看到的是文件列表的半截。
    scroll: ScrollHandle,
}

impl DetailState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<GitHistoryContent>) -> Self {
        Self {
            open: true,
            loading: false,
            error: None,
            data: None,
            multi: None,
            stats: None,
            containing: None,
            containing_request: 0,
            target: None,
            shown: None,
            request: 0,
            body: cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, BODY_MAX_ROWS)),
            pending_body: None,
            collapsed: HashSet::new(),
            scroll: ScrollHandle::new(),
        }
    }

    /// 换仓库 / 选择清空:上一份详情作废,在途的也作废。
    pub(super) fn clear(&mut self) {
        self.request += 1;
        self.containing_request += 1;
        self.loading = false;
        self.error = None;
        self.data = None;
        self.multi = None;
        self.stats = None;
        self.containing = None;
        self.target = None;
        self.shown = None;
        self.pending_body = None;
        self.collapsed.clear();
    }

    /// 取回来的正文写进只读框(渲染时调:写框要窗口)。
    fn flush_body(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(body) = self.pending_body.take() {
            self.body.update(cx, |s, cx| s.set_value(body, window, cx));
        }
    }

    /// 选择变了,要换成 `target`。与上一次要的相同返回 `false`(不必重拉)。
    ///
    /// 同一形态里换选中照旧留着上一份内容(不闪);换了单选 / 多选形态时,另一形态的旧
    /// 内容不留 —— 否则多选态先露出上一回合并的文件、单选态先露出很久以前那个提交。
    pub(super) fn retarget(&mut self, target: Option<DetailTarget>) -> bool {
        if self.target == target {
            return false;
        }
        let was_multi = matches!(self.target, Some(DetailTarget::Multi(_)));
        let was_single = matches!(self.target, Some(DetailTarget::Single(_)));
        match &target {
            Some(DetailTarget::Multi(_)) if !was_multi => self.multi = None,
            Some(DetailTarget::Single(_)) if !was_single => {
                self.data = None;
                self.error = None;
            }
            _ => {}
        }
        self.target = target;
        true
    }

    /// 换上一份新内容:与眼下画的不是同一份就回顶、展开全部目录;同一份重拉(刷新)
    /// 不打断正在看的位置。
    fn show(&mut self, target: DetailTarget) {
        if self.shown.as_ref() != Some(&target) {
            self.scroll.set_offset(point(px(0.0), px(0.0)));
            self.collapsed.clear();
        }
        self.shown = Some(target);
    }

    /// 多选态已经合并好的这组提交的文件(合并 diff 直接用,不必再合并一次)。
    pub(super) fn combined_files(&self, hashes: &[String]) -> Option<Vec<CombinedFileChange>> {
        let multi = self.multi.as_ref().filter(|m| m.hashes == hashes)?;
        multi.result.as_ref().ok().cloned()
    }

    /// 这个提交的增删行数(还没回来 / 没算时 `None`)。
    fn stats_of(&self, hash: &str) -> Option<&CommitLineStats> {
        self.stats
            .as_ref()
            .filter(|(of, _)| of == hash)
            .map(|(_, stats)| stats)
    }

    /// 这个提交的所在分支(还没回来时 `None`)。
    fn containing_of(&self, hash: &str) -> Option<&Result<Vec<ContainingBranch>, String>> {
        self.containing
            .as_ref()
            .filter(|(of, _)| of == hash)
            .map(|(_, result)| result)
    }
}

/// 完整消息 → (标题, 正文)。正文去掉与标题之间的空行。
fn split_message(message: &str) -> (String, String) {
    let mut lines = message.lines();
    let title = lines.next().unwrap_or("").trim().to_string();
    let body = lines
        .skip_while(|l| l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (title, body.trim_end().to_string())
}

impl GitHistoryContent {
    /// 详情栏眼下该不该画:有仓库、有选中、没被关上。其余条件(「提交历史」区收着、
    /// 面板在空态)归容器判,见 `GitPanel::detail_pane`。
    pub fn detail_shown(&self) -> bool {
        !self.repo_path.is_empty() && !self.selection.is_empty() && self.detail.open
    }

    /// 关上详情栏(选择照留,再单击一行才重新打开)。
    pub(super) fn close_detail(&mut self, cx: &mut Context<Self>) {
        if self.detail.open {
            self.detail.open = false;
            cx.notify();
        }
    }

    pub(super) fn load_detail(&mut self, hash: String, cx: &mut Context<Self>) {
        if self.repo_path.is_empty() {
            return;
        }
        self.detail.request += 1;
        let req = self.detail.request;
        self.detail.loading = true;
        self.load_containing(hash.clone(), cx);
        let repo = PathBuf::from(&self.repo_path);
        cx.spawn(async move |this, cx| {
            // 详情与行数一起开跑,详情先回来先画
            let (detail_repo, detail_hash) = (repo.clone(), hash.clone());
            let detail_task = cx.background_executor().spawn(async move {
                mt_project::git::get_commit_detail(&detail_repo, &detail_hash)
            });
            let stats_hash = hash.clone();
            let stats_task = cx
                .background_executor()
                .spawn(async move { mt_project::git::get_commit_line_stats(&repo, &stats_hash) });

            let result = detail_task.await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.detail.request != req {
                    return;
                }
                this.detail.loading = false;
                this.detail.show(DetailTarget::Single(hash.clone()));
                match result {
                    Ok(detail) => {
                        let (_, body) = split_message(&detail.message);
                        this.detail.pending_body = Some(body);
                        this.detail.data = Some(detail);
                        this.detail.error = None;
                    }
                    Err(err) => {
                        this.detail.error = Some(error_text(&err));
                        this.detail.data = None;
                    }
                }
                cx.notify();
            });

            let stats = stats_task.await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.detail.request != req {
                    return;
                }
                match stats {
                    Ok(Some(stats)) => this.detail.stats = Some((hash, stats)),
                    // 文件太多,不算(见 `LINE_STATS_MAX_FILES`)
                    Ok(None) => {}
                    Err(err) => eprintln!("[git] 统计提交 {hash} 的增删行数失败: {err:#}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 后台查包含这个提交的分支。
    fn load_containing(&mut self, hash: String, cx: &mut Context<Self>) {
        if self.repo_path.is_empty() {
            return;
        }
        self.detail.containing_request += 1;
        let req = self.detail.containing_request;
        let repo = PathBuf::from(&self.repo_path);
        cx.spawn(async move |this, cx| {
            let task_hash = hash.clone();
            let result = cx
                .background_executor()
                .spawn(async move { mt_project::git::get_branches_containing(&repo, &task_hash) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.detail.containing_request != req {
                    return;
                }
                if let Err(err) = &result {
                    eprintln!("[git] 查包含提交 {hash} 的分支失败: {err:#}");
                }
                this.detail.containing = Some((hash, result.map_err(|err| error_text(&err))));
                cx.notify();
            });
        })
        .detach();
    }

    /// 分支列表变了:单提交详情的「所在分支」重查一遍(旧结果留到新的回来,不闪)。
    pub(super) fn refresh_containing(&mut self, cx: &mut Context<Self>) {
        if let Some(DetailTarget::Single(hash)) = self.detail.target.clone() {
            self.load_containing(hash, cx);
        }
    }

    /// 多选态:后台合并这组提交的改动文件。
    pub(super) fn load_multi_detail(&mut self, hashes: Vec<String>, cx: &mut Context<Self>) {
        if self.repo_path.is_empty() {
            return;
        }
        self.detail.request += 1;
        let req = self.detail.request;
        self.detail.loading = true;
        let repo = PathBuf::from(&self.repo_path);
        cx.spawn(async move |this, cx| {
            let task_hashes = hashes.clone();
            let result = cx
                .background_executor()
                .spawn(
                    async move { mt_project::git::get_combined_commit_files(&repo, &task_hashes) },
                )
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.detail.request != req {
                    return;
                }
                this.detail.loading = false;
                this.detail.show(DetailTarget::Multi(hashes.clone()));
                if let Err(err) = &result {
                    eprintln!("[git] 合并 {} 个提交的改动失败: {err:#}", hashes.len());
                }
                this.detail.multi = Some(MultiDetail {
                    hashes,
                    result: result.map_err(|err| error_text(&err)),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// 点父提交:已加载就选中并滚过去。
    fn jump_to(&mut self, hash: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.commits.iter().position(|c| c.hash == hash) {
            self.select_index(index, cx);
        }
    }
}

/// 详情栏的视图壳(见模块注释「单独一栏」):借历史区实体来画,历史区一 notify 就重画。
pub struct CommitDetailPane {
    history: Entity<GitHistoryContent>,
    _observe: Subscription,
}

impl CommitDetailPane {
    pub fn new(history: Entity<GitHistoryContent>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&history, |_, _, cx| cx.notify());
        Self {
            history,
            _observe: observe,
        }
    }
}

impl Render for CommitDetailPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.history
            .update(cx, |history, cx| render_pane(history, window, cx))
    }
}

/// 元信息的一行:左列标签(muted、定宽),右列值。
fn meta_row(label: &'static str, value: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap(px(12.0))
        .child(
            div()
                .flex_none()
                .w(px(META_LABEL_W))
                .pt(px(2.0))
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_primary())
                .child(value),
        )
}

/// 可点的等宽「链接」文本(哈希 / 父提交):accent 色,hover 下划线。
fn link(
    id: impl Into<gpui::ElementId>,
    text: impl Into<SharedString>,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    let el = mono(div()).id(id).flex_none().text_size(ui::font_px(13.0));
    let el = if enabled {
        el.text_color(ui::accent())
            .cursor_pointer()
            .hover(|el| el.underline())
    } else {
        el.text_color(ui::text_muted())
    };
    el.child(text.into())
}

/// 增删行数:`+12 -3`(新增绿、删除红,为 0 的一侧不写)。两侧都是 0 时为 `None`
/// (纯重命名)。
fn line_counts(additions: usize, deletions: usize) -> Option<gpui::Div> {
    if additions == 0 && deletions == 0 {
        return None;
    }
    Some(
        mono(div())
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_size(ui::font_px(12.0))
            .when(additions > 0, |el| {
                el.child(
                    div()
                        .text_color(git_ui::text_fg(ui::color_success()))
                        .child(format!("+{additions}")),
                )
            })
            .when(deletions > 0, |el| {
                el.child(
                    div()
                        .text_color(git_ui::text_fg(ui::color_error()))
                        .child(format!("-{deletions}")),
                )
            }),
    )
}

/// 文件行右端的行数附注:二进制写「二进制」,纯重命名不写。
fn file_stat_label(stat: &FileLineStat) -> Option<AnyElement> {
    if stat.binary {
        return Some(
            div()
                .flex_none()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(t("gitLog", "detail.binary"))
                .into_any_element(),
        );
    }
    line_counts(stat.additions, stat.deletions).map(IntoElement::into_any_element)
}

/// 详情栏:标题条 + 可滚动的内容。由 [`CommitDetailPane`] 借历史区实体调。
fn render_pane(
    this: &mut GitHistoryContent,
    window: &mut Window,
    cx: &mut Context<GitHistoryContent>,
) -> AnyElement {
    // 后台取回来的正文这时写进只读框(写框要窗口)
    this.detail.flush_body(window, cx);
    let this = &*this;
    let multi = this.selection.len() >= 2;

    let mut body = div()
        .id("git-log-detail-body")
        .flex_1()
        .min_h(px(0.0))
        .track_scroll(&this.detail.scroll)
        .overflow_y_scroll()
        .px(px(16.0))
        .py(px(14.0))
        .flex()
        .flex_col()
        .gap(px(14.0));

    if multi {
        body = render_multi(this, body, cx);
    } else {
        match (this.detail.data.as_ref(), &this.detail.error) {
            (_, Some(err)) => {
                body = body.child(git_ui::notice_line(
                    tr!("gitLog", "detail.failed", detail = err.clone()),
                    ui::color_error(),
                ));
            }
            (None, None) => {
                body = body.child(git_ui::loading_state(t("gitLog", "detail.loading")));
            }
            (Some(d), None) => {
                body = render_content(this, d, body, cx);
            }
        }
    }

    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(ui::bg_surface())
        .child(render_header(this, multi, cx))
        .child(body)
        .into_any_element()
}

/// 标题条:「提交详情」+ 短哈希 + 加载转圈;右边复制消息 / 查看变更 / 关闭。
/// 多选态不写短哈希、不挂「复制消息」(复制哈希走 Ctrl/Cmd+C 与右键菜单),「查看变更」
/// 打开合并 diff。
fn render_header(
    this: &GitHistoryContent,
    multi: bool,
    cx: &mut Context<GitHistoryContent>,
) -> gpui::Div {
    let data = this.detail.data.as_ref().filter(|_| !multi);
    let mut actions = div().flex().flex_none().items_center().gap(px(2.0));
    if let Some(d) = data {
        let message = d.message.clone();
        actions = actions.child(
            icon_button(
                "git-log-detail-copy",
                ICON_COPY,
                t("gitLog", "detail.copyMessage"),
                true,
            )
            .on_click(move |_: &ClickEvent, _window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(message.clone()));
            }),
        );
        let (hash, title) = (d.hash.clone(), split_message(&d.message).0);
        actions = actions.child(
            icon_button(
                "git-log-detail-open-diff",
                ICON_DIFF,
                t("gitLog", "detail.openFile"),
                true,
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_commit_diff(hash.clone(), title.clone(), None, window, cx);
            })),
        );
    }
    if multi {
        actions = actions.child(
            icon_button(
                "git-log-detail-open-combined",
                ICON_DIFF,
                t("gitLog", "multi.openFile"),
                true,
            )
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.view_selected_combined(None, window, cx);
            })),
        );
    }
    actions = actions.child(
        icon_button(
            "git-log-detail-close",
            ICON_CLOSE,
            t("gitLog", "detail.close"),
            true,
        )
        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
            this.close_detail(cx);
        })),
    );

    let short = data.map(|d| d.hash.chars().take(7).collect::<String>());
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.0))
        .h(px(HEADER_H))
        .pl(px(16.0))
        .pr(px(6.0))
        .border_b_1()
        .border_color(ui::border_subtle())
        .child(icon(ICON_COMMIT, git_ui::ICON_SIZE, ui::text_muted()))
        .child(
            div()
                .flex_none()
                .text_size(ui::font_px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(ui::text_primary())
                .child(t("gitLog", "detail.title")),
        )
        .when_some(short, |el, short| {
            el.child(
                mono(div())
                    .flex_none()
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(short),
            )
        })
        .when(this.detail.loading, |el| {
            el.child(ui::spinner(px(git_ui::ICON_SIZE_SM), ui::accent()))
        })
        .child(div().flex_1().min_w_0())
        .child(actions)
}

fn render_content(
    this: &GitHistoryContent,
    d: &CommitDetail,
    mut body: gpui::Stateful<gpui::Div>,
    cx: &mut Context<GitHistoryContent>,
) -> gpui::Stateful<gpui::Div> {
    let (title, text) = split_message(&d.message);
    let now = chrono::Utc::now().timestamp();

    // ① 标题(大一档、加粗、能折行)+ 正文(只读、可选中)
    body = body.child(
        div()
            .text_size(ui::font_px(15.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(ui::text_primary())
            .child(title.clone()),
    );
    if !text.is_empty() {
        // 多行输入框自带内边距(组件库 Medium 档:左右 10、上下 8,`appearance(false)`
        // 也照样留着),正文因此比标题右缩一截。外面用等量负外边距抵掉,
        // 正文字与标题、元信息同一条左缘。
        let (pad_x, pad_y) = (Size::Medium.input_px(), Size::Medium.input_py());
        body = body.child(
            div()
                .mx(-pad_x)
                .my(-pad_y)
                .text_size(ui::font_px(13.0))
                .child(
                    Textarea::new(&this.detail.body)
                        .readonly(true)
                        .appearance(false),
                ),
        );
    }

    // ② 元信息
    body = body.child(div().h(px(1.0)).bg(ui::border_subtle()));
    let mut meta = div().flex().flex_col().gap(px(10.0));
    meta = meta.child(meta_row(
        t("gitLog", "detail.author"),
        person(&d.author_name, &d.author_email),
    ));
    meta = meta.child(meta_row(
        t("gitLog", "detail.date"),
        mono(div()).child(format_detail_time(d.author_time, now)),
    ));
    // 提交者 / 提交时间与作者不同才写(拣选、变基、改消息、替别人提交之后)
    if d.committer_name != d.author_name || d.committer_email != d.author_email {
        meta = meta.child(meta_row(
            t("gitLog", "detail.committer"),
            person(&d.committer_name, &d.committer_email),
        ));
    }
    if d.commit_time != d.author_time {
        meta = meta.child(meta_row(
            t("gitLog", "detail.commitDate"),
            mono(div()).child(format_detail_time(d.commit_time, now)),
        ));
    }
    {
        let hash = d.hash.clone();
        let short: String = hash.chars().take(7).collect();
        meta = meta.child(meta_row(
            t("gitLog", "detail.hash"),
            link("git-log-detail-hash", hash.clone(), true)
                .tip(t("gitLog", "detail.copyHashTip"))
                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(hash.clone()));
                    this.toast_info(tr!("gitLog", "toast.copied", hash = short), cx);
                })),
        ));
    }
    meta = meta.child(meta_row(
        t("gitLog", "detail.parents"),
        render_parents(this, d, cx),
    ));
    let refs = this.all_refs(&d.hash);
    if !refs.is_empty() {
        let mut pills = div().flex().flex_wrap().gap(px(6.0));
        for (i, (name, kind)) in refs.into_iter().enumerate() {
            pills = pills.child(
                ref_pill(
                    SharedString::from(format!("git-log-detail-ref-{i}")),
                    name.clone(),
                    kind,
                )
                .max_w(px(240.0))
                .tip(name),
            );
        }
        meta = meta.child(meta_row(t("gitLog", "detail.refs"), pills));
    }
    meta = meta.child(meta_row(
        t("gitLog", "detail.branches"),
        render_containing(this, &d.hash),
    ));
    body = body.child(meta);

    // ③ 改动文件(行数回来了就带上)
    body = body.child(div().h(px(1.0)).bg(ui::border_subtle()));
    let stats = this.detail.stats_of(&d.hash);
    let stat_by_path: HashMap<&str, &FileLineStat> = stats
        .map(|s| s.files.iter().map(|f| (f.path.as_str(), f)).collect())
        .unwrap_or_default();
    let files: Vec<FileEntry<'_>> = d
        .files
        .iter()
        .map(|f| FileEntry {
            path: &f.path,
            status: &f.status,
            old_path: f.old_path.as_deref(),
            stat: stat_by_path.get(f.path.as_str()).copied(),
        })
        .collect();
    let caption = if files.is_empty() {
        t("gitLog", "detail.noFiles").to_string()
    } else {
        tr!("gitLog", "detail.files", count = files.len().to_string())
    };
    let hash = d.hash.clone();
    let open: OpenFile = Rc::new(move |this, initial, window, cx| {
        this.open_commit_diff(hash.clone(), title.clone(), initial, window, cx);
    });
    body.child(render_files(
        this,
        &files,
        FileTexts {
            caption,
            caption_tip: None,
            totals: stats.map(|s| (s.additions, s.deletions)),
            open_tip: t("gitLog", "detail.openFile"),
            more_key: "detail.moreFiles",
        },
        open,
        cx,
    ))
}

/// 作者 / 提交者:头像 + 名字 + 弱化的邮箱(窄了邮箱折到下一行)。
fn person(name: &str, email: &str) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(6.0))
        .gap_y(px(2.0))
        .child(avatar(name))
        .child(div().child(name.to_string()))
        .when(!email.is_empty(), |el| {
            el.child(
                div()
                    .text_color(ui::text_muted())
                    .child(format!("<{email}>")),
            )
        })
}

/// 父提交:每个一行,短哈希(已加载的可点、跳过去)+ 它的标题(已加载时才知道)。
fn render_parents(
    this: &GitHistoryContent,
    d: &CommitDetail,
    cx: &mut Context<GitHistoryContent>,
) -> gpui::Div {
    let mut parents = div().flex().flex_col().gap(px(4.0));
    if d.parent_hashes.is_empty() {
        return parents.child(
            div()
                .text_color(ui::text_muted())
                .child(t("gitLog", "detail.noParents")),
        );
    }
    for (i, parent) in d.parent_hashes.iter().enumerate() {
        let loaded = this.commits.iter().find(|c| &c.hash == parent);
        let short: String = parent.chars().take(7).collect();
        let target = parent.clone();
        // 「已加载 / 未加载」会随触底续页变:提示现读,不抓移入那一刻的
        let (view, probe) = (cx.entity().downgrade(), parent.clone());
        let hash_link = link(
            SharedString::from(format!("git-log-detail-parent-{i}")),
            short,
            loaded.is_some(),
        )
        .tip_live(move |cx| {
            let loaded = view
                .upgrade()
                .is_some_and(|v| v.read(cx).commits.iter().any(|c| c.hash == probe));
            if loaded {
                t("gitLog", "detail.jumpParent").into()
            } else {
                t("gitLog", "detail.parentNotLoaded").into()
            }
        })
        .when(loaded.is_some(), |el| {
            el.on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.jump_to(&target, cx);
            }))
        });
        parents = parents.child(
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .min_w_0()
                .child(hash_link)
                .when_some(loaded, |el, commit| {
                    el.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(ui::text_secondary())
                            .child(commit.message.clone()),
                    )
                }),
        );
    }
    parents
}

/// 「所在分支」:当前分支排第一,本地在前、远程在后;太多的收进「+N」,悬停列出。
fn render_containing(this: &GitHistoryContent, hash: &str) -> AnyElement {
    let muted = |text: String| {
        div()
            .text_color(ui::text_muted())
            .child(text)
            .into_any_element()
    };
    let list = match this.detail.containing_of(hash) {
        None => return muted(t("gitLog", "detail.branchesLoading").to_string()),
        Some(Err(err)) => {
            return div()
                .text_color(ui::color_error())
                .child(tr!("gitLog", "detail.branchesFailed", detail = err.clone()))
                .into_any_element();
        }
        Some(Ok(list)) if list.is_empty() => {
            return muted(t("gitLog", "detail.branchesNone").to_string());
        }
        Some(Ok(list)) => list,
    };
    let head = this.head_branch.as_deref();
    let is_head = |b: &ContainingBranch| !b.is_remote && Some(b.name.as_str()) == head;
    let mut ordered: Vec<&ContainingBranch> = list.iter().collect();
    ordered.sort_by_key(|b| !is_head(b));

    let mut pills = div().flex().flex_wrap().gap(px(6.0));
    for (i, branch) in ordered.iter().take(BRANCH_PILLS).enumerate() {
        let kind = if is_head(branch) {
            RefKind::Head
        } else if branch.is_remote {
            RefKind::Remote
        } else {
            RefKind::Local
        };
        pills = pills.child(
            ref_pill(
                SharedString::from(format!("git-log-detail-branch-{i}")),
                branch.name.clone(),
                kind,
            )
            .max_w(px(240.0))
            .tip(branch.name.clone()),
        );
    }
    if ordered.len() > BRANCH_PILLS {
        let rest = ordered[BRANCH_PILLS..]
            .iter()
            .map(|b| b.name.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        pills = pills
            .child(more_pill("git-log-detail-branch-more", ordered.len() - BRANCH_PILLS).tip(rest));
    }
    pills.into_any_element()
}

/// 多选态的正文:「已选 N 个提交」+ 选中提交的小列表 +(超上限的说明)+ 合并后的改动文件。
fn render_multi(
    this: &GitHistoryContent,
    mut body: gpui::Stateful<gpui::Div>,
    cx: &mut Context<GitHistoryContent>,
) -> gpui::Stateful<gpui::Div> {
    let total = this.selection.len();
    let now = chrono::Utc::now().timestamp();
    body = body.child(
        div()
            .text_size(ui::font_px(15.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(ui::text_primary())
            .child(tr!("gitLog", "multi.title", count = total.to_string())),
    );

    // 选中提交的小列表:短哈希 + 标题 + 作者 + 相对时间,完整信息挂 tooltip
    let picked = this.selection.ordered(&this.commits);
    let mut list = div().flex().flex_col().gap(px(2.0));
    for commit in picked.iter().take(MULTI_LIST_ROWS) {
        list = list.child(
            div()
                .id(SharedString::from(format!("git-log-multi-{}", commit.hash)))
                .flex()
                .items_center()
                .gap(px(8.0))
                .h(px(26.0))
                .child(
                    mono(div())
                        .flex_none()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_muted())
                        .child(commit.short_hash.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(ui::font_px(13.0))
                        .text_color(ui::text_primary())
                        .child(commit.message.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .max_w(px(120.0))
                        .truncate()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_muted())
                        .child(commit.author.clone()),
                )
                .child(
                    mono(div())
                        .flex_none()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_muted())
                        .child(format_relative_time(commit.timestamp, now)),
                )
                .tip(row_tooltip(commit)),
        );
    }
    if picked.len() > MULTI_LIST_ROWS {
        list = list.child(
            div()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(tr!(
                    "gitLog",
                    "multi.more",
                    count = (picked.len() - MULTI_LIST_ROWS).to_string()
                )),
        );
    }
    body = body.child(list);
    if total > MULTI_LIMIT {
        body = body.child(git_ui::notice_line(
            tr!("gitLog", "multi.limit", limit = MULTI_LIMIT.to_string()),
            ui::color_warning(),
        ));
    }

    body = body.child(div().h(px(1.0)).bg(ui::border_subtle()));
    match this.detail.multi.as_ref().map(|m| &m.result) {
        None => body.child(git_ui::loading_state(t("gitLog", "multi.loading"))),
        Some(Err(err)) => body.child(git_ui::notice_line(
            tr!("gitLog", "multi.failed", detail = err.clone()),
            ui::color_error(),
        )),
        Some(Ok(files)) => {
            let entries: Vec<FileEntry<'_>> = files
                .iter()
                .map(|f| FileEntry {
                    path: &f.path,
                    status: &f.status,
                    old_path: f.old_path.as_deref(),
                    stat: None,
                })
                .collect();
            let caption = if entries.is_empty() {
                t("gitLog", "multi.noFiles").to_string()
            } else {
                tr!("gitLog", "detail.files", count = entries.len().to_string())
            };
            // 点文件 = 打开合并 diff(用的是这一份已经合并好的文件)
            let open: OpenFile = Rc::new(|this, initial, window, cx| {
                this.view_selected_combined(initial, window, cx);
            });
            body.child(render_files(
                this,
                &entries,
                FileTexts {
                    caption,
                    caption_tip: Some(t("gitLog", "multi.rangeTip")),
                    totals: None,
                    open_tip: t("gitLog", "multi.openFile"),
                    more_key: "multi.moreFiles",
                },
                open,
                cx,
            ))
        }
    }
}

/// 文件区里的一个文件(单提交 / 多选合并共用一套画法)。
struct FileEntry<'a> {
    path: &'a str,
    status: &'a str,
    /// 重命名的起点路径(悬停写成 `旧 → 新`,与提交 diff 左栏同一口径)。
    old_path: Option<&'a str>,
    /// 增删行数(单提交且已算好时才有)。
    stat: Option<&'a FileLineStat>,
}

/// 点文件 / 「还有 N 个文件」:`Some(路径)` = 打开并选中它,`None` = 打开、选第一个。
type OpenFile = Rc<
    dyn Fn(&mut GitHistoryContent, Option<String>, &mut Window, &mut Context<GitHistoryContent>),
>;

/// 文件区的几段文案。
struct FileTexts {
    /// 标题行(「N 个文件」/ 没有文件的说明)。
    caption: String,
    /// 标题行的 tooltip(多选态写合并口径)。
    caption_tip: Option<&'static str>,
    /// 全部文件合计的增删行数(单提交且已算好时才有)。
    totals: Option<(usize, usize)>,
    /// 文件行的 tooltip。
    open_tip: &'static str,
    /// 「还有 N 个文件」的 `gitLog.*` key。
    more_key: &'static str,
}

/// 文件区:标题行(文件数 + 合计行数 + 列表 / 树形切换)+ 文件行(最多 [`MAX_FILE_ROWS`]
/// 行)+「还有 N 个文件」。
fn render_files(
    this: &GitHistoryContent,
    files: &[FileEntry<'_>],
    texts: FileTexts,
    open: OpenFile,
    cx: &mut Context<GitHistoryContent>,
) -> gpui::Div {
    let tree = this.store.read(cx).git_commit_files_tree();
    let caption = mono(div())
        .id("git-log-detail-files-caption")
        .min_w_0()
        .truncate()
        .text_size(ui::font_px(12.0))
        .text_color(ui::text_muted())
        .when_some(texts.caption_tip, |el, tip| el.tip(tip))
        .child(texts.caption);
    let totals = texts.totals.and_then(|(added, removed)| {
        let counts = line_counts(added, removed)?;
        Some(
            div()
                .id("git-log-detail-files-totals")
                .flex_none()
                .child(counts)
                .tip(tr!(
                    "gitLog",
                    "detail.statsTip",
                    added = added.to_string(),
                    removed = removed.to_string()
                )),
        )
    });
    let caption_row = div()
        .flex()
        .items_center()
        .gap(px(10.0))
        .child(caption)
        .children(totals)
        .child(div().flex_1())
        .when(!files.is_empty(), |el| {
            el.child(
                git_ui::view_mode_toggle("git-log-detail-view-mode", tree).on_click(cx.listener(
                    |this, _: &ClickEvent, _window, cx| {
                        this.store.update(cx, |store, cx| {
                            let next = !store.git_commit_files_tree();
                            store.set_git_commit_files_tree(next, cx);
                        });
                        cx.notify();
                    },
                )),
            )
        });

    let paths: Vec<(usize, &str)> = files.iter().enumerate().map(|(i, f)| (i, f.path)).collect();
    let collapsed = &this.detail.collapsed;
    let rows = git_ui::change_rows(&paths, tree, &|p| collapsed.contains(p));
    let hidden = git_ui::hidden_file_count(&rows, MAX_FILE_ROWS);
    let mut list = div().flex().flex_col();
    for row in rows.into_iter().take(MAX_FILE_ROWS) {
        match row {
            ChangeTreeRow::Dir {
                name,
                full_path,
                depth,
                collapsed,
                file_count,
            } => {
                let id = SharedString::from(format!("git-log-detail-dir-{full_path}"));
                list = list.child(
                    git_ui::tree_dir_row(id, name, file_count, depth, collapsed).on_click(
                        cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            if !this.detail.collapsed.remove(&full_path) {
                                this.detail.collapsed.insert(full_path.clone());
                            }
                            cx.notify();
                        }),
                    ),
                );
            }
            ChangeTreeRow::File { index, depth } => {
                let Some(file) = files.get(index) else {
                    continue;
                };
                let (open, path) = (open.clone(), file.path.to_string());
                let on_click = cx.listener(move |this, _: &ClickEvent, window, cx| {
                    open(this, Some(path.clone()), window, cx);
                });
                let tip = match file.old_path {
                    Some(old) => format!("{old} → {}\n{}", file.path, texts.open_tip),
                    None => texts.open_tip.to_string(),
                };
                list = list.child(
                    git_ui::commit_file_row_with(
                        SharedString::from(format!("git-log-detail-file-{index}")),
                        file.path,
                        file.status,
                        false,
                        tree.then_some(depth),
                        file.stat.and_then(file_stat_label),
                    )
                    .tip(tip)
                    .on_click(on_click),
                );
            }
        }
    }
    if hidden > 0 {
        list = list.child(
            div()
                .id("git-log-detail-more-files")
                .px(px(8.0))
                .py(px(6.0))
                .cursor_pointer()
                .text_size(ui::font_px(12.0))
                .text_color(ui::accent())
                .hover(|el| el.underline())
                .child(tr!("gitLog", texts.more_key, count = hidden.to_string()))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    open(this, None, window, cx);
                })),
        );
    }
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(caption_row)
        .child(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 消息拆成标题与正文() {
        assert_eq!(
            split_message("feat: x\n\nbody line 1\nbody line 2\n"),
            (
                "feat: x".to_string(),
                "body line 1\nbody line 2".to_string()
            )
        );
        assert_eq!(
            split_message("only title"),
            ("only title".to_string(), String::new())
        );
        assert_eq!(split_message(""), (String::new(), String::new()));
        // 标题后直接跟正文(没有空行)也认
        assert_eq!(split_message("t\nb"), ("t".to_string(), "b".to_string()));
    }
}
