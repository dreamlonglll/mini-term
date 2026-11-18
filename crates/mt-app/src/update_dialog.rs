//! 「发现新版本」弹窗:边条那颗更新按钮点开的(此前是直接跳 GitHub 发布页)。
//!
//! 标题带新版本号,正文是发布时间 + 发布说明(release 的 `body`),底部「忽略此版本 / 稍后 /
//! 前往下载」。发布很频繁(一周数版),每次启动都亮一颗按钮、却看不到改了什么,用户只能每次
//! 都去浏览器里翻 —— 这里把说明直接摆出来,并给一个让这一版不再提醒的出口(判据见
//! [`crate::update_check::should_announce`],恢复入口在设置 →「关于」)。
//!
//! # 发布说明怎么画
//!
//! 与 AI 会话正文同一套:`TextView::markdown`(可选中复制),先过
//! [`crate::file_viewer::sanitize_session_markdown`]。正文来自网络,图片一律退成 alt 文字、
//! 裸 HTML 变成可见的惰性文本 —— 打开弹窗不会发出任何后台请求(会话正文同一条红线);
//! 链接保留,点了用系统浏览器开(`TextView` 的默认行为)。超高时在正文区里滚。

use gpui::{
    App, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Window, div, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::button::ButtonVariants as _;
use gpui_component::dialog::DialogFooter;
use gpui_component::text::{TextView, TextViewStyle};

use crate::i18n::{t, tr};
use crate::prompt::{close_guarded, footer_button, is_open, kind, open_guarded};
use crate::store::AppStore;
use crate::ui;
use crate::update_check::{ReleaseInfo, format_published_at};

/// 弹窗宽度(与 Git 的「内容多的弹窗」同档,见 `git_ui::DIALOG_W_LG`)。
const DIALOG_W: f32 = 560.0;
/// 整块弹窗的舒适高度上限;正文区按视口钳制(见 [`ui::clamp_dialog_body_height`])。
const DIALOG_MAX_H: f32 = 620.0;
/// 正文之外的固定占用:标题行 + 发布时间 + 底部提示 + 按钮行 + 内边距。
const DIALOG_CHROME: f32 = 190.0;

/// 打开「发现新版本」弹窗。同种类只开一个(连点按钮不叠开)。
pub fn open(release: &ReleaseInfo, window: &mut Window, cx: &mut App) {
    // 已经开着就不再清洗一遍正文(守卫本身在 `open_guarded` 里)
    if is_open(kind::UPDATE_NOTES) {
        return;
    }
    let notes: SharedString =
        crate::file_viewer::sanitize_session_markdown(release.body.trim()).into();
    let title = tr!(
        "app",
        "update.dialogTitle",
        version = release.version.as_str()
    );
    let published = tr!(
        "settings",
        "about.publishedAt",
        date = format_published_at(&release.published_at)
    );
    // 正文的 element id 带版本号:换了版本不复用上一份的解析缓存
    let notes_id = SharedString::from(format!("update-notes-{}", release.version));
    let (release_url, release_version) = (release.url.clone(), release.version.clone());
    let store = AppStore::global(cx);

    open_guarded(kind::UPDATE_NOTES, window, cx, move |dialog, window, cx| {
        let viewport = window.viewport_size();
        let body_h =
            ui::clamp_dialog_body_height(px(DIALOG_MAX_H), viewport, 0.8, px(DIALOG_CHROME));
        let (url, version, store) = (release_url.clone(), release_version.clone(), store.clone());
        dialog
            .title(title.clone())
            .w(ui::clamp_dialog_width(px(DIALOG_W), viewport))
            .overlay_closable(true)
            .footer(
                DialogFooter::new()
                    .child(
                        footer_button("update-ignore", t("app", "update.ignore")).on_click(
                            move |_, window, cx| {
                                let version = version.clone();
                                store.update(cx, |store, cx| {
                                    store.set_ignored_update_version(Some(version), cx)
                                });
                                close_guarded(kind::UPDATE_NOTES, window, cx);
                            },
                        ),
                    )
                    .child(
                        footer_button("update-later", t("app", "update.later")).on_click(
                            |_, window, cx| {
                                close_guarded(kind::UPDATE_NOTES, window, cx);
                            },
                        ),
                    )
                    .child(
                        footer_button("update-download", t("app", "update.download"))
                            .primary()
                            .on_click(move |_, window, cx| {
                                cx.open_url(&url);
                                close_guarded(kind::UPDATE_NOTES, window, cx);
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(ui::hint(published.clone()))
                    .child(
                        div()
                            .id("update-notes-scroll")
                            .max_h(body_h)
                            .overflow_y_scroll()
                            .px(px(12.0))
                            .py(px(10.0))
                            .rounded(px(6.0))
                            .bg(ui::bg_base())
                            .border_1()
                            .border_color(ui::border_subtle())
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::text_secondary())
                            .child(if notes.is_empty() {
                                div()
                                    .text_color(ui::text_muted())
                                    .child(t("app", "update.noNotes"))
                                    .into_any_element()
                            } else {
                                TextView::markdown(notes_id.clone(), notes.clone())
                                    .style(notes_style(cx))
                                    .selectable(true)
                                    .into_any_element()
                            }),
                    )
                    .child(ui::hint(t("app", "update.ignoreHint"))),
            )
    });
}

/// 发布说明的排版:按 13px 正文定基准,标题只比正文略大(发布说明里常有一串 `##` 小节,
/// 按文档视图的比例放大会显得很吵)。设置 →「关于」页查到新版时那段说明也用它。
pub(crate) fn notes_style(cx: &App) -> TextViewStyle {
    let mut code_block = gpui::StyleRefinement::default();
    {
        let text = &mut code_block.text;
        text.font_size = Some(ui::font_px(12.0).into());
        text.line_height = Some(gpui::relative(1.5));
    }
    TextViewStyle {
        highlight_theme: cx.theme().highlight_theme.clone(),
        is_dark: cx.theme().mode.is_dark(),
        heading_base_font_size: ui::font_px(13.0),
        paragraph_gap: gpui::rems(0.6),
        code_block,
        // 行内 code 与会话正文 / md 预览同一口径(accent 字 + elevated 底)
        inline_code: gpui::HighlightStyle {
            color: Some(ui::accent()),
            background_color: Some(ui::bg_elevated()),
            ..Default::default()
        },
        ..Default::default()
    }
    .heading_font_size(|level, base| match level {
        1 => base * 1.3,
        2 => base * 1.15,
        _ => base,
    })
}
