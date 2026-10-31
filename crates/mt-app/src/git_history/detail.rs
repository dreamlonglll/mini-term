//! 历史区底部的「提交详情」(IDEA Log 右侧那块):完整消息、作者 / 时间 / 哈希 /
//! 父提交 / 引用,以及改动文件。
//!
//! 单击提交行 = 选中 → 这里拉 [`mt_project::git::get_commit_detail`](后台);
//! 快速按 ↑/↓ 连换时用请求令牌丢掉迟到的响应。切换期间先留着上一份内容(本地仓库
//! 通常十几毫秒就回来),不清空成「加载中」,免得详情区一闪一闪。
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

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, FontWeight,
    InteractiveElement, IntoElement, ParentElement, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, Window, div, point, prelude::FluentBuilder as _, px,
};
use gpui_component::Size;
use gpui_component::input::{Textarea, TextareaState};
use mt_project::git::{CombinedFileChange, CommitDetail};
use mt_ui::tooltip::TooltipExt as _;

use super::{GitHistoryContent, MULTI_LIMIT, format_detail_time, grow, row_tooltip};
use crate::git_ui::{
    self, ChangeTreeRow, ICON_COPY, SectionHeader, error_text, icon_button, mono, ref_pill,
};
use crate::i18n::{t, tr};
use crate::ui;

/// 详情里的文件最多直接画多少行(拍平后的行数,目录行也算;不虚拟化,再多去提交 diff
/// 里看)。
const MAX_FILE_ROWS: usize = 300;
/// 多选态的提交小列表最多列几条,其余写「还有 N 个提交」。
const MULTI_LIST_ROWS: usize = 8;

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
    /// 展开 / 收起(收起只剩一条标题栏)。
    pub(super) open: bool,
    loading: bool,
    error: Option<String>,
    data: Option<CommitDetail>,
    multi: Option<MultiDetail>,
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
            target: None,
            shown: None,
            request: 0,
            body: cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 12)),
            pending_body: None,
            collapsed: HashSet::new(),
            scroll: ScrollHandle::new(),
        }
    }

    /// 换仓库 / 选择清空:上一份详情作废,在途的也作废。
    pub(super) fn clear(&mut self) {
        self.request += 1;
        self.loading = false;
        self.error = None;
        self.data = None;
        self.multi = None;
        self.target = None;
        self.shown = None;
        self.pending_body = None;
        self.collapsed.clear();
    }

    /// 取回来的正文写进只读框(渲染时调:写框要窗口)。
    pub(super) fn flush_body(&mut self, window: &mut Window, cx: &mut App) {
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
    pub(super) fn load_detail(&mut self, hash: String, cx: &mut Context<Self>) {
        if self.repo_path.is_empty() {
            return;
        }
        self.detail.request += 1;
        let req = self.detail.request;
        self.detail.loading = true;
        let repo = PathBuf::from(&self.repo_path);
        cx.spawn(async move |this, cx| {
            let task_hash = hash.clone();
            let result = cx
                .background_executor()
                .spawn(async move { mt_project::git::get_commit_detail(&repo, &task_hash) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.detail.request != req {
                    return;
                }
                this.detail.loading = false;
                this.detail.show(DetailTarget::Single(hash));
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
        })
        .detach();
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

/// 元信息的一行:左列标签(muted、定宽),右列值。
fn meta_row(label: &'static str, value: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap(px(12.0))
        .child(
            div()
                .flex_none()
                .w(px(56.0))
                .pt(px(1.0))
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(ui::font_px(12.0))
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
    let el = mono(div()).id(id).flex_none().text_size(ui::font_px(12.0));
    let el = if enabled {
        el.text_color(ui::accent())
            .cursor_pointer()
            .hover(|el| el.underline())
    } else {
        el.text_color(ui::text_muted())
    };
    el.child(text.into())
}

/// 详情区:标题栏(收起 / 展开、复制消息)+ 内容。`open = false` 时只有标题栏。
///
/// 标题栏走共用的区块标题:左侧折叠箭头 + 「提交详情」,整行可点收起 / 展开
/// (与上面「更改」「提交历史」两块同一手感);右侧短哈希、加载转圈与「复制消息」。
/// 多选态不写短哈希、不挂「复制消息」(复制哈希走 Ctrl/Cmd+C 与右键菜单)。
pub(super) fn render(
    this: &GitHistoryContent,
    open: bool,
    cx: &mut Context<GitHistoryContent>,
) -> AnyElement {
    let multi = this.selection.len() >= 2;
    let data = this.detail.data.as_ref().filter(|_| !multi);
    let short = data.map(|d| d.hash.chars().take(7).collect::<String>());
    let message_for_copy = data.map(|d| d.message.clone());

    let mut header =
        SectionHeader::new("git-log-detail-header", t("gitLog", "detail.title")).collapsible(open);
    if this.detail.loading {
        header = header.trailing(ui::spinner(px(git_ui::ICON_SIZE_SM), ui::accent()));
    }
    if let Some(short) = short {
        header = header.trailing(
            mono(div())
                .px(px(4.0))
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(short),
        );
    }
    if let Some(message) = message_for_copy {
        header = header.trailing(
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
    }
    // 点一下就切展开 / 收起:提示文案在气泡渲染时现读(见 `TooltipExt::tip_live`)
    let view = cx.entity().downgrade();
    let header = header
        .build()
        .border_t_1()
        .border_b_1()
        .border_color(ui::border_subtle())
        .tip_live(move |cx| {
            let open = view.upgrade().is_some_and(|v| v.read(cx).detail.open);
            if open {
                t("gitLog", "detail.collapse").into()
            } else {
                t("gitLog", "detail.expand").into()
            }
        })
        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
            this.detail.open = !this.detail.open;
            cx.notify();
        }));

    if !open {
        return div()
            .flex_none()
            .bg(ui::bg_surface())
            .child(header)
            .into_any_element();
    }

    let mut body = div()
        .id("git-log-detail-body")
        .flex_1()
        .min_h(px(0.0))
        .track_scroll(&this.detail.scroll)
        .overflow_y_scroll()
        .px(px(12.0))
        .py(px(10.0))
        .flex()
        .flex_col()
        .gap(px(10.0));

    if multi {
        body = render_multi(this, body, cx);
    } else {
        match (data, &this.detail.error) {
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

    grow(
        div()
            .flex_basis(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .bg(ui::bg_surface())
            .child(header)
            .child(body),
        0.42,
    )
    .into_any_element()
}

fn render_content(
    this: &GitHistoryContent,
    d: &CommitDetail,
    mut body: gpui::Stateful<gpui::Div>,
    cx: &mut Context<GitHistoryContent>,
) -> gpui::Stateful<gpui::Div> {
    let (title, text) = split_message(&d.message);
    let now = chrono::Utc::now().timestamp();

    // ① 标题(大一档、加粗)+ 正文(只读、可选中)
    body = body.child(
        div()
            .text_size(ui::font_px(14.0))
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
    let mut meta = div().flex().flex_col().gap(px(6.0));
    meta = meta.child(meta_row(
        t("gitLog", "detail.author"),
        format!("{} <{}>", d.author_name, d.author_email),
    ));
    if d.committer_name != d.author_name || d.committer_email != d.author_email {
        meta = meta.child(meta_row(
            t("gitLog", "detail.committer"),
            format!("{} <{}>", d.committer_name, d.committer_email),
        ));
    }
    meta = meta.child(meta_row(
        t("gitLog", "detail.date"),
        mono(div()).child(format_detail_time(d.author_time, now)),
    ));
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
    {
        let mut parents = div().flex().flex_wrap().gap(px(10.0));
        if d.parent_hashes.is_empty() {
            parents = parents.child(
                div()
                    .text_color(ui::text_muted())
                    .child(t("gitLog", "detail.noParents")),
            );
        }
        for (i, parent) in d.parent_hashes.iter().enumerate() {
            let loaded = this.commits.iter().any(|c| &c.hash == parent);
            let short: String = parent.chars().take(7).collect();
            let target = parent.clone();
            // 「已加载 / 未加载」会随触底续页变:提示现读,不抓移入那一刻的
            let (view, probe) = (cx.entity().downgrade(), parent.clone());
            parents = parents.child(
                link(
                    SharedString::from(format!("git-log-detail-parent-{i}")),
                    short,
                    loaded,
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
                .when(loaded, |el| {
                    el.on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.jump_to(&target, cx);
                    }))
                }),
            );
        }
        meta = meta.child(meta_row(t("gitLog", "detail.parents"), parents));
    }
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
    body = body.child(meta);

    // ③ 改动文件
    body = body.child(div().h(px(1.0)).bg(ui::border_subtle()));
    let files: Vec<FileEntry<'_>> = d
        .files
        .iter()
        .map(|f| FileEntry {
            path: &f.path,
            status: &f.status,
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
            open_tip: t("gitLog", "detail.openFile"),
            more_key: "detail.moreFiles",
        },
        open,
        cx,
    ))
}

/// 多选态的正文:「已选 N 个提交」+ 选中提交的小列表 +(超上限的说明)+ 合并后的改动文件。
fn render_multi(
    this: &GitHistoryContent,
    mut body: gpui::Stateful<gpui::Div>,
    cx: &mut Context<GitHistoryContent>,
) -> gpui::Stateful<gpui::Div> {
    let total = this.selection.len();
    body = body.child(
        div()
            .text_size(ui::font_px(14.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(ui::text_primary())
            .child(tr!("gitLog", "multi.title", count = total.to_string())),
    );

    // 选中提交的小列表:短哈希 + 标题,完整信息挂 tooltip
    let picked = this.selection.ordered(&this.commits);
    let mut list = div().flex().flex_col().gap(px(2.0));
    for commit in picked.iter().take(MULTI_LIST_ROWS) {
        list = list.child(
            div()
                .id(SharedString::from(format!("git-log-multi-{}", commit.hash)))
                .flex()
                .items_center()
                .gap(px(8.0))
                .h(px(22.0))
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
    /// 文件行的 tooltip。
    open_tip: &'static str,
    /// 「还有 N 个文件」的 `gitLog.*` key。
    more_key: &'static str,
}

/// 文件区:标题行(文件数 + 列表 / 树形切换)+ 文件行(最多 [`MAX_FILE_ROWS`] 行)+
/// 「还有 N 个文件」。
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
    let caption_row = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(caption)
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
                list = list.child(
                    git_ui::commit_file_row_at(
                        SharedString::from(format!("git-log-detail-file-{index}")),
                        file.path,
                        file.status,
                        false,
                        tree.then_some(depth),
                    )
                    .tip(texts.open_tip)
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
