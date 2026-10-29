//! 历史区底部的「提交详情」(IDEA Log 右侧那块):完整消息、作者 / 时间 / 哈希 /
//! 父提交 / 引用,以及改动文件列表。
//!
//! 单击提交行 = 选中 → 这里拉 [`mt_project::git::get_commit_detail`](后台);
//! 快速按 ↑/↓ 连换时用请求令牌丢掉迟到的响应。切换期间先留着上一份内容(本地仓库
//! 通常十几毫秒就回来),不清空成「加载中」,免得详情区一闪一闪。
//!
//! 正文放在**只读**的多行输入框里:能选中、能复制,长段落自动换行。

use std::path::PathBuf;

use gpui::{
    AnyElement, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, FontWeight,
    InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Textarea, TextareaState};
use mt_project::git::CommitDetail;
use mt_ui::tooltip::TooltipExt as _;

use super::{GitHistoryContent, format_absolute_time, format_relative_time, grow};
use crate::git_ui::{
    self, ICON_COPY, SectionHeader, commit_file_row, error_text, icon_button, mono, ref_pill,
};
use crate::i18n::{t, tr};
use crate::ui;

/// 详情里的文件列表最多直接画多少行(不虚拟化;再多去提交 diff 里看)。
const MAX_FILE_ROWS: usize = 300;

pub(super) struct DetailState {
    /// 展开 / 收起(收起只剩一条标题栏)。
    pub(super) open: bool,
    loading: bool,
    error: Option<String>,
    data: Option<CommitDetail>,
    /// 请求令牌:连着换选中时,迟到的详情丢弃。
    request: u64,
    /// 只读正文(可选中复制)。
    body: Entity<TextareaState>,
}

impl DetailState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<GitHistoryContent>) -> Self {
        Self {
            open: true,
            loading: false,
            error: None,
            data: None,
            request: 0,
            body: cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 12)),
        }
    }

    /// 换仓库:上一份详情作废,在途的也作废。
    pub(super) fn clear(&mut self) {
        self.request += 1;
        self.loading = false;
        self.error = None;
        self.data = None;
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
    pub(super) fn load_detail(
        &mut self,
        hash: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.repo_path.is_empty() {
            return;
        }
        self.detail.request += 1;
        let req = self.detail.request;
        self.detail.loading = true;
        let repo = PathBuf::from(&self.repo_path);
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { mt_project::git::get_commit_detail(&repo, &hash) })
                .await;
            let _ = this.update_in(cx, |this: &mut Self, window, cx| {
                if this.detail.request != req {
                    return;
                }
                this.detail.loading = false;
                match result {
                    Ok(detail) => {
                        let (_, body) = split_message(&detail.message);
                        this.detail
                            .body
                            .update(cx, |s, cx| s.set_value(body, window, cx));
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

    /// 点父提交:已加载就选中并滚过去。
    fn jump_to(&mut self, hash: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.commits.iter().position(|c| c.hash == hash) {
            self.select_index(index, window, cx);
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
pub(super) fn render(
    this: &GitHistoryContent,
    open: bool,
    cx: &mut Context<GitHistoryContent>,
) -> AnyElement {
    let data = this.detail.data.as_ref();
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
    let header = header
        .build()
        .border_t_1()
        .border_b_1()
        .border_color(ui::border_subtle())
        .tip(if open {
            t("gitLog", "detail.collapse")
        } else {
            t("gitLog", "detail.expand")
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
        .overflow_y_scroll()
        .px(px(12.0))
        .py(px(10.0))
        .flex()
        .flex_col()
        .gap(px(10.0));

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
            .child(title),
    );
    if !text.is_empty() {
        body = body.child(
            div().text_size(ui::font_px(13.0)).child(
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
        mono(div()).child(format!(
            "{}  ({})",
            format_absolute_time(d.author_time),
            format_relative_time(d.author_time, now)
        )),
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
            parents = parents.child(
                link(
                    SharedString::from(format!("git-log-detail-parent-{i}")),
                    short,
                    loaded,
                )
                .tip(if loaded {
                    t("gitLog", "detail.jumpParent")
                } else {
                    t("gitLog", "detail.parentNotLoaded")
                })
                .when(loaded, |el| {
                    el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.jump_to(&target, window, cx);
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
    body = body.child(
        mono(div())
            .text_size(ui::font_px(12.0))
            .text_color(ui::text_muted())
            .child(if d.files.is_empty() {
                t("gitLog", "detail.noFiles").to_string()
            } else {
                tr!("gitLog", "detail.files", count = d.files.len().to_string())
            }),
    );
    let mut files = div().flex().flex_col();
    for (i, file) in d.files.iter().take(MAX_FILE_ROWS).enumerate() {
        let (hash, title, path) = (
            d.hash.clone(),
            split_message(&d.message).0,
            file.path.clone(),
        );
        files = files.child(
            commit_file_row(
                SharedString::from(format!("git-log-detail-file-{i}")),
                &file.path,
                &file.status,
                false,
            )
            .tip(t("gitLog", "detail.openFile"))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_commit_diff(hash.clone(), title.clone(), Some(path.clone()), window, cx);
            })),
        );
    }
    if d.files.len() > MAX_FILE_ROWS {
        let (hash, title) = (d.hash.clone(), split_message(&d.message).0);
        files = files.child(
            div()
                .id("git-log-detail-more-files")
                .px(px(8.0))
                .py(px(6.0))
                .cursor_pointer()
                .text_size(ui::font_px(12.0))
                .text_color(ui::accent())
                .hover(|el| el.underline())
                .child(tr!(
                    "gitLog",
                    "detail.moreFiles",
                    count = (d.files.len() - MAX_FILE_ROWS).to_string()
                ))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.open_commit_diff(hash.clone(), title.clone(), None, window, cx);
                })),
        );
    }
    body.child(files)
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
