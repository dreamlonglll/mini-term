//! Markdown / HTML 预览的渲染:[`FileViewer`] 的另一半 `impl`(分块缓存、虚拟化
//! 列表、自绘图片 / 表格 / mermaid、链接处置),外加几件无状态的渲染辅助。
//! 纯逻辑在 [`super::markdown`];不可信输入先过 [`super::sanitize`]。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext, ClickEvent, Context, InteractiveElement, IntoElement, ParentElement, Resource,
    StatefulInteractiveElement, Styled, StyledImage as _, Window, div, img, list,
    prelude::FluentBuilder as _, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::scroll::Scrollbar;
use gpui_component::text::{TextView, TextViewStyle};
use mt_ui::tooltip::TooltipExt as _;

use super::file_kind::file_name_of;
use super::html_urls::rewrite_html_urls;
use super::images::{
    local_image_resource, md_image_resources, release_viewer_images, remote_image_resource,
};
use super::markdown::{
    LinkAction, MdAlign, MdBlock, MdImage, MdImageSrc, MdSegment, MdTable, block_has_anchor,
    block_top_margin, classify_link, column_weights, is_plain_cell, resolve_image_src,
    rewrite_md_image_urls, split_md_blocks,
};
use super::mermaid::{MermaidAsset, MermaidError, MermaidKey, release_mermaid_assets};
use super::sanitize::sanitize_remote_markdown;
use super::{DocumentSource, FileViewer};
use crate::i18n::t;
use crate::image_lightbox::{ImageLightbox, ImageLightboxEvent};
use crate::prompt::{Confirm, show_alert};
use crate::ui;

/// markdown 预览的分块缓存。key 是「源码 + 所在目录」,两者都没变就复用。
///
/// 有它是因为**滚动一次就是整个视图重 render 一遍**(`gpui::list` 的滚轮处理
/// 改完位置就 notify 当前 view),而 [`split_md_blocks`] 与
/// [`rewrite_md_image_urls`] 都是全文逐字符扫描 —— 一份 40 KB 的文档每帧
/// 重切一次纯属白烧。缓存的是**分块结果**,不是元素:视口附近的块每帧照建
/// (gpui 的 retained 边界在 Element 那一层,不在这里)。
pub(super) struct MdCache {
    source: String,
    base_dir: PathBuf,
    local_resources: bool,
    /// 分块代次:每重切一次 +1。[`FileViewer::sync_md_list`] 靠它知道「块的
    /// 内容变了、量过的高度全作废」—— 只比块数不够,改一段话块数不变
    generation: u64,
    /// `(块顶间距, 块)`。`Rc` 让 [`FileViewer::render_md_item`] 拿完就撒手,
    /// 不必攥着 `RefCell` 的借用穿过整段渲染
    blocks: Rc<Vec<(f32, MdBlock)>>,
}

/// 与组件库无回调时的默认口径一致:左/中键、键盘、非长按触摸才算「点开」。
fn is_primary_link_click(event: &ClickEvent) -> bool {
    match event {
        ClickEvent::Mouse(click) => {
            matches!(
                click.up.button,
                gpui::MouseButton::Left | gpui::MouseButton::Middle
            )
        }
        ClickEvent::Keyboard(_) => true,
        ClickEvent::Touch(click) => !click.long_press,
    }
}

/// 一个格子的内容元素:纯文字走快路,其余仍逐格按 markdown 渲染。
fn render_md_cell(
    seg_ix: usize,
    row_ix: usize,
    col_ix: usize,
    cell: &str,
    style: &TextViewStyle,
    _window: &mut Window,
    _cx: &mut App,
) -> gpui::AnyElement {
    if is_plain_cell(cell) {
        // 外层刻意与 TextView 那条路同形(它的最外层也是 `div().size_full()`,
        // 见 `text/text_view.rs` 的 `request_layout`)—— 两类格子混在同一张表里,
        // 盒模型差一点就是一行高矮不齐
        return div()
            .size_full()
            .child(gpui::SharedString::from(cell.to_string()))
            .into_any_element();
    }
    TextView::markdown(
        gpui::SharedString::from(format!("md-tbl-{seg_ix}-{row_ix}-{col_ix}")),
        cell.to_string(),
    )
    .style(style.clone())
    .into_any_element()
}

/// 自绘一张表。样式逐条对照 `.md-preview table`(styles.css:889-910):
/// 100% 宽、0.92em、collapse 边框(--border-default)、格子 8×12 padding、
/// 表头 --bg-elevated + 600、偶数数据行 --bg-surface 斑马纹;格子**自动换行**
/// (min_w_0,不 truncate),列宽按内容长度加权 —— 浏览器 auto 布局的近似。
fn render_md_table(
    seg_ix: usize,
    table: &MdTable,
    style: &TextViewStyle,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyElement {
    let weights = column_weights(table);
    let row_count = table.rows.len() + 1;
    let mut rows_el = Vec::with_capacity(row_count);
    for (row_ix, cells) in std::iter::once(&table.header)
        .chain(table.rows.iter())
        .enumerate()
    {
        let is_header = row_ix == 0;
        let mut cell_els = Vec::with_capacity(cells.len());
        for (col_ix, cell) in cells.iter().enumerate() {
            let weight = weights.get(col_ix).copied().unwrap_or(0.2);
            let align = table.aligns.get(col_ix).copied().unwrap_or(MdAlign::Left);
            cell_els.push(
                div()
                    .w(gpui::relative(weight))
                    .min_w(px(0.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .when(col_ix + 1 != cells.len(), |el| {
                        el.border_r_1().border_color(ui::border_default())
                    })
                    .when(align == MdAlign::Center, |el| el.flex().justify_center())
                    .when(align == MdAlign::Right, |el| el.flex().justify_end())
                    // 带标记的格子仍按 markdown 渲染(行内 code 胶囊/加粗/链接不丢),
                    // 纯文字的走快路 —— 理由见 [`is_plain_cell`]
                    .child(render_md_cell(
                        seg_ix, row_ix, col_ix, cell, style, window, cx,
                    ))
                    .into_any_element(),
            );
        }
        rows_el.push(
            div()
                .flex()
                .flex_row()
                .w_full()
                .when(is_header, |el| {
                    el.bg(ui::bg_elevated())
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                })
                // 原版 `tr:nth-child(even)`:数据行在 tbody 里从 1 数,偶数行上色
                .when(!is_header && row_ix % 2 == 0, |el| el.bg(ui::bg_surface()))
                .when(row_ix + 1 != row_count, |el| {
                    el.border_b_1().border_color(ui::border_default())
                })
                .children(cell_els)
                .into_any_element(),
        );
    }
    div()
        .w_full()
        // 上下外边距不在这里:块间距统一由 render_markdown 的 block_top_margin 给
        .text_size(ui::font_px(12.9))
        .border_1()
        .border_color(ui::border_default())
        .children(rows_el)
        .into_any_element()
}

/// 目标是不是 svg。远程 URL 只看路径末尾 —— 查询串(`?style=flat`)不算扩展名,
/// 徽章那类 URL 常带。
fn is_svg_target(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    file_name_of(path)
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("svg"))
}

const MARKDOWN_CONTENT_MAX_WIDTH: f32 = 860.0;

/// 图片该占多宽(逻辑像素):原尺寸与可用宽取小 —— 小图保持原大(原版
/// `max-width:100%` 也不放大),大图压到可用宽。
///
/// `size()` 给的是**设备像素**。svg 那条路(本地/远程 svg 与 mermaid 图表都算)
/// 由 `SvgRenderer` 按 [`gpui::SMOOTH_SVG_SCALE_FACTOR`] 放大后光栅化,换算回逻辑
/// 像素要除回去。位图自己记着的倍率(`RenderImage::scale_factor`)是 `pub(crate)`
/// 的,外面读不到,只能读同样由 gpui 公开的那个常量 —— 至少上游调它的时候这里
/// 跟着变,不再是写死的 2.0。
fn image_display_width(data: &gpui::RenderImage, is_svg: bool, avail_w: f32) -> f32 {
    image_natural_size(data, is_svg)
        .width
        .clamp(1.0, avail_w.max(1.0))
}

/// 图片的原始**逻辑**尺寸(svg 那条路已把栅格倍率除回去,口径同上)。放大浮层
/// 按它算「适应窗口」与 100%。
fn image_natural_size(data: &gpui::RenderImage, is_svg: bool) -> gpui::Size<f32> {
    let scale = if is_svg {
        gpui::SMOOTH_SVG_SCALE_FACTOR
    } else {
        1.0
    };
    let size = data.size(0);
    gpui::size(
        (size.width.0.max(1) as f32 / scale).max(1.0),
        (size.height.0.max(1) as f32 / scale).max(1.0),
    )
}

fn image_aspect_ratio(data: &gpui::RenderImage) -> f32 {
    let size = data.size(0);
    let width = size.width.0.max(1) as f32;
    let height = size.height.0.max(1) as f32;
    width / height
}

fn markdown_image_can_load(is_remote_document: bool, approved: bool) -> bool {
    !is_remote_document || approved
}

/// 图片画不出来时的占位:一枚描边小卡片,写 alt(没有就写文件名)。
///
/// 三种情况共用 —— 还在取(读盘 / 拉网)、取不到(文件不在、格式解不了、403、
/// 离线)、`data:` 之类不支持的目标。`hint` 给悬停提示(远程 URL / 解析后的
/// 本地路径),`open` 有值时可点,点了用系统浏览器打开原图。
fn md_image_placeholder(
    id: gpui::SharedString,
    label: gpui::SharedString,
    hint: Option<String>,
    open: Option<String>,
) -> gpui::AnyElement {
    div()
        .id(id)
        .max_w_full()
        .min_w_0()
        .flex()
        .items_center()
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(ui::border_default())
        .bg(ui::bg_elevated())
        .text_size(ui::font_px(12.0))
        .text_color(ui::text_muted())
        .child(div().min_w_0().truncate().child(label))
        .when_some(hint, |el, hint| el.tip(hint))
        .when_some(open, |el, url| {
            el.cursor_pointer()
                .hover(|el| el.text_color(ui::text_primary()))
                .on_click(move |_: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                    cx.open_url(&url);
                })
        })
        .into_any_element()
}

impl FileViewer {
    /// 自绘一行 md 图片(纯图片段落由 [`super::markdown`] 的
    /// `split_top_level_image_paragraph` 拆出)。
    ///
    /// TextView 那条路把图片目标一律当**网络 URI**(见 [`super::markdown`] 开头
    /// 「markdown 分段」一节),于是 README 里 `![主界面](docs/screenshots/main.png)`
    /// 这种相对路径在预览里什么都不出 —— 原版是 `convertFileSrc(fileDir + '/' + src)`。
    /// 这里按当前文件所在目录解析成 `Resource::Path` 自己画。
    ///
    /// 远程图片先画不触网的占位；用户明确点击后才把 `Resource::Uri` 交给
    /// [`super::PreviewHttpClient`]。本地 Markdown 维持原来的自动加载行为。
    ///
    /// 860px 只用于算设计宽；真实布局由带原图宽高比的外层框负责。父栏变窄时
    /// `max_w_full` 会压缩框宽，`aspect_ratio` 同步重算高度，不再依赖整窗 viewport。
    fn render_md_images(
        &self,
        seg_ix: usize,
        images: &[MdImage],
        avail_w: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let base_dir = self.preview_base_dir();
        // 并排多张(徽章行)时按张数分宽;单张吃满
        let each_w = (avail_w / images.len().max(1) as f32 - 8.0).max(24.0);
        let mut els = Vec::with_capacity(images.len());
        for (ix, image) in images.iter().enumerate() {
            let id = gpui::SharedString::from(format!("file-viewer-md-img-{seg_ix}-{ix}"));
            let label_text = if image.alt.is_empty() {
                file_name_of(&image.url).to_string()
            } else {
                image.alt.clone()
            };
            let label = gpui::SharedString::from(label_text.clone());
            let source = resolve_image_src(&image.url, &base_dir);
            let el = if self.source.is_remote() {
                match source {
                    MdImageSrc::Remote(url)
                        if markdown_image_can_load(
                            true,
                            self.approved_remote_images.contains(&url),
                        ) =>
                    {
                        self.render_md_remote_image(id, label, &url, each_w, window, cx)
                    }
                    MdImageSrc::Remote(url) => {
                        let consent_id = gpui::SharedString::from(format!(
                            "file-viewer-md-img-consent-{seg_ix}-{ix}"
                        ));
                        let approved_url = url.clone();
                        let prompt = t("fileViewer", "remoteImageClickToLoad");
                        let placeholder_label =
                            gpui::SharedString::from(format!("{label_text} · {prompt}"));
                        div()
                            .id(consent_id)
                            .max_w_full()
                            .min_w_0()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.approved_remote_images.insert(approved_url.clone());
                                cx.notify();
                            }))
                            .child(md_image_placeholder(
                                id,
                                placeholder_label,
                                Some(format!("{prompt}\n{url}")),
                                None,
                            ))
                            .into_any_element()
                    }
                    MdImageSrc::Local(_) | MdImageSrc::Unsupported => md_image_placeholder(
                        id,
                        label,
                        Some(t("fileViewer", "remoteRelativeImage").to_string()),
                        None,
                    ),
                }
            } else {
                match source {
                    MdImageSrc::Local(path) => {
                        self.render_md_local_image(id, label, &path, each_w, window, cx)
                    }
                    MdImageSrc::Remote(url) => {
                        self.render_md_remote_image(id, label, &url, each_w, window, cx)
                    }
                    MdImageSrc::Unsupported => {
                        md_image_placeholder(id, label, Some(image.url.clone()), None)
                    }
                }
            };
            // 外层链接(`[![alt](img)](link)`):点图开外链。只认 http(s) ——
            // 本地目标要走「页内跳转」,而那条路整条不做(见 `file_viewer` 模块注释偏差 1)
            let el = match image.link.as_deref().map(str::trim) {
                Some(link) if link.starts_with("http://") || link.starts_with("https://") => {
                    let url = link.to_string();
                    let tip = link.to_string();
                    div()
                        .id(gpui::SharedString::from(format!(
                            "file-viewer-md-img-link-{seg_ix}-{ix}"
                        )))
                        .max_w_full()
                        .min_w_0()
                        .cursor_pointer()
                        .tip(tip)
                        .on_click(move |_: &ClickEvent, _window, cx| cx.open_url(&url))
                        .child(el)
                        .into_any_element()
                }
                _ => el,
            };
            els.push(el);
        }
        div()
            .w_full()
            .max_w_full()
            .min_w_0()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.0))
            .children(els)
            .into_any_element()
    }

    /// 一张本地图片:读得出来画图,读不出来 / 还在读画占位。
    fn render_md_local_image(
        &self,
        id: gpui::SharedString,
        label: gpui::SharedString,
        path: &Path,
        avail_w: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let resource = local_image_resource(path);
        let hint = path.to_string_lossy().to_string();
        match self.use_viewer_image(&resource, window, cx) {
            // 还在读 / 读不出来(文件不在、格式解不了)都给占位,不留白
            None | Some(Err(_)) => md_image_placeholder(id, label, Some(hint), None),
            Some(Ok(data)) => {
                // ImgState 会按 element id 跨帧保存 GIF/WebP 的 frame_index。资源换代
                // 后必须换 id，否则旧动画帧下标可能越过新图片的 frame_count。
                let image_id = gpui::SharedString::from(format!("{id}-{}", data.id.0));
                let mut frame = div();
                frame.style().aspect_ratio = Some(image_aspect_ratio(&data));
                frame
                    .w(px(image_display_width(
                        &data,
                        is_svg_target(&hint),
                        avail_w,
                    )))
                    .max_w_full()
                    .min_w_0()
                    .child(
                        img(data.clone())
                            .id(image_id)
                            .size_full()
                            .object_fit(gpui::ObjectFit::Contain),
                    )
                    .into_any_element()
            }
        }
    }

    /// 一张网络图片(徽章、外链截图)。与本地那支同一套尺寸规则,差别只在资源
    /// 是 URI —— 字节由 [`super::PreviewHttpClient`] 拉回来。
    ///
    /// 拉不动(离线 / 403 / 超时)时占位**可点**,用系统浏览器打开原图:总比
    /// 一个死框强。还在拉的时候也是占位,拿到字节后自然换成图。
    fn render_md_remote_image(
        &self,
        id: gpui::SharedString,
        label: gpui::SharedString,
        url: &str,
        avail_w: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let resource = remote_image_resource(url);
        match self.use_viewer_image(&resource, window, cx) {
            None | Some(Err(_)) => {
                md_image_placeholder(id, label, Some(url.to_string()), Some(url.to_string()))
            }
            Some(Ok(data)) => {
                // 同一槽位换成另一份 RenderImage 时重置动画状态；同一资源跨帧的
                // ImageId 保持稳定，因此 GIF/WebP 仍能连续播放。
                let image_id = gpui::SharedString::from(format!("{id}-{}", data.id.0));
                let mut frame = div();
                frame.style().aspect_ratio = Some(image_aspect_ratio(&data));
                frame
                    .w(px(image_display_width(&data, is_svg_target(url), avail_w)))
                    .max_w_full()
                    .min_w_0()
                    .child(
                        img(data.clone())
                            .id(image_id)
                            .size_full()
                            .object_fit(gpui::ObjectFit::Contain),
                    )
                    .into_any_element()
            }
        }
    }

    /// 一块 ```mermaid 围栏(issue #80)。图表由 [`MermaidAsset`] 在后台渲染成
    /// 位图,这里三态:还没好 → 占位卡片;好了 → 与本地图片同一套框(原尺寸与
    /// 列宽取小、等比缩);失败 → **退回代码块**照 TextView 画,底下补一行原因
    /// —— 画不出来的图表至少要让人看得见原文,与 GitHub 对错误围栏的处置一致。
    ///
    /// 与图片不同,这条路**不分本地 / 远程文档**:渲染是纯文本计算,不碰盘不触网。
    #[allow(clippy::too_many_arguments)]
    fn render_md_mermaid(
        &mut self,
        seg_ix: usize,
        code: &Arc<str>,
        fallback: &gpui::SharedString,
        style: &TextViewStyle,
        avail_w: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let id = gpui::SharedString::from(format!("file-viewer-md-mermaid-{seg_ix}"));
        let key = MermaidKey::new(code, cx);
        // 先记后要:use_asset 一旦调用,资源系统里就有了这个 key 的任务
        self.mermaid_requested.get_mut().insert(key.clone());
        let failure = |id: gpui::SharedString, err: &MermaidError| {
            let reason = format!("{}: {err}", t("fileViewer", "mermaidRenderFailed"));
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    TextView::markdown(id, fallback.clone())
                        .style(style.clone())
                        .selectable(true),
                )
                .child(
                    div()
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_muted())
                        .child(reason),
                )
                .into_any_element()
        };
        match window.use_asset::<MermaidAsset>(&key, cx) {
            // 主题切换后同一张图要按新配色重画,期间若换成一张矮矮的占位卡片,
            // 列表总高一缩、滚动位置就被夹回上面(在文末切主题会被顶回开头,
            // 真机复现)。旧配色的那份还在缓存里(重切分块前不放):画成功过的
            // 按它的尺寸把占位撑到同高;失败过的直接照旧画退回的代码块(排版与
            // 配色无关,新一轮结论必然相同)。首次渲染没有旧图,照旧一张卡片。
            None => {
                let placeholder = md_image_placeholder(
                    id.clone(),
                    t("fileViewer", "mermaidRendering").into(),
                    None,
                    None,
                );
                match self.mermaid_sibling_result(&key, cx) {
                    Some(Ok(data)) => {
                        let width = image_display_width(&data, true, avail_w);
                        div()
                            .w_full()
                            .h(px(width / image_aspect_ratio(&data)))
                            .flex()
                            .flex_col()
                            .justify_center()
                            .child(placeholder)
                            .into_any_element()
                    }
                    Some(Err(err)) => failure(id, &err),
                    None => placeholder,
                }
            }
            Some(Ok(data)) => {
                // 点开在整窗浮层里看(滚轮缩放 / 拖动平移):大一点的架构图缩进
                // 860px 的列里字全糊在一起,原地放大又会与文档滚动抢滚轮,
                // 所以是弹窗(用户拍板)。浮层拿的是同一份位图,不重新渲染。
                let natural = image_natural_size(&data, true);
                let image = data.clone();
                let lightbox_open = self.lightbox.is_some();
                let mut frame = div().id(id);
                frame.style().aspect_ratio = Some(image_aspect_ratio(&data));
                frame
                    .w(px(image_display_width(&data, true, avail_w)))
                    .max_w_full()
                    .min_w_0()
                    .cursor_pointer()
                    // 浮层开着时把提示摘掉:gpui 的延时显示任务按元素**绝对边界**判
                    // 悬停、不认遮挡(div.rs `handle_tooltip_mouse_move` 上方的 TODO),
                    // 移上来就点的话提示会在浮层打开后才冒出来、盖在浮层上;元素这一帧
                    // 没有 tooltip builder 时上游会把挂起的任务连同提示一起丢掉。
                    .when(!lightbox_open, |el| el.tip(t("fileViewer", "lightboxOpen")))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        this.open_lightbox(image.clone(), natural, window, cx);
                    }))
                    .child(
                        img(data.clone())
                            .size_full()
                            .object_fit(gpui::ObjectFit::Contain),
                    )
                    .into_any_element()
            }
            Some(Err(err)) => failure(id, &err),
        }
    }

    /// 打开图片放大浮层。先清后建:换浮层时旧实体必须先 drop 掉,否则 overlay 栈
    /// 会错乱(理由见 `ImageLightbox::new` 的注释)。
    fn open_lightbox(
        &mut self,
        image: Arc<gpui::RenderImage>,
        natural: gpui::Size<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.lightbox = None;
        self._lightbox_sub = None;
        let lightbox = cx.new(|cx| ImageLightbox::new(image, natural, window, cx));
        self._lightbox_sub = Some(cx.subscribe_in(
            &lightbox,
            window,
            |this: &mut Self, _, event, _window, cx| {
                let ImageLightboxEvent::Dismissed = event;
                // 只丢实体、不动 `_lightbox_sub` —— 那是正在跑的这条订阅自己,
                // 实体一没它也不会再触发,真正的清理在下一次 open_lightbox 开头
                this.lightbox = None;
                cx.notify();
            },
        ));
        self.lightbox = Some(lightbox);
        cx.notify();
    }

    /// 同一张图表另一套配色的渲染结果(还在缓存里的话)。只翻
    /// [`Self::mermaid_requested`] 里要过的 key —— `fetch_asset` 对没见过的 key
    /// 会发起渲染,这里只准取现成的。
    fn mermaid_sibling_result(
        &self,
        key: &MermaidKey,
        cx: &mut App,
    ) -> Option<<MermaidAsset as gpui::Asset>::Output> {
        let sibling = self
            .mermaid_requested
            .borrow()
            .iter()
            .find(|other| other.code == key.code && *other != key)
            .cloned()?;
        cx.fetch_asset::<MermaidAsset>(&sibling)
    }

    /// 预览里链接的点击回调(挂在每个 `TextView` 上)。组件要 `Send + Sync`,
    /// 只捕获 `WeakEntity`(它是 `PhantomData<fn(T) -> T>`,与 `T` 无关);
    /// 真正的处置在 [`Self::follow_link`]。
    fn preview_link_handler(
        &self,
        cx: &Context<Self>,
    ) -> impl Fn(&gpui::SharedString, &ClickEvent, &mut Window, &mut App) + Send + Sync + 'static
    {
        let this = cx.weak_entity();
        move |url, event, window, cx| {
            if !is_primary_link_click(event) {
                return;
            }
            let url = url.to_string();
            let _ = this.update(cx, |viewer, cx| viewer.follow_link(&url, window, cx));
        }
    }

    /// 原版 `handleLinkClick` 的四条处置(分类在 [`classify_link`]):
    /// 外链弹确认再开浏览器;锚点滚到标题所在的块;其它协议直接交给系统;
    /// 本地文件作为页签打开(本地来源先验文件在不在,远程交给页签自己报错)。
    pub(super) fn follow_link(&mut self, href: &str, window: &mut Window, cx: &mut Context<Self>) {
        match classify_link(&self.current_path.to_string_lossy(), href) {
            LinkAction::External(url) => {
                let open = url.clone();
                Confirm::new(t("externalLink", "openConfirm"), url).open(
                    move |_window, cx| cx.open_url(&open),
                    window,
                    cx,
                );
            }
            LinkAction::Anchor(id) => self.scroll_to_anchor(&id, cx),
            LinkAction::Scheme(url) => cx.open_url(&url),
            LinkAction::Local(target) => {
                // 解析结果是正斜杠的;本地来源按组件重拼成平台分隔符,页头显示的
                // 路径才与从文件树打开的一致(页签去重本身不看分隔符)
                let path: PathBuf = PathBuf::from(&target).components().collect();
                let source = match &self.source {
                    DocumentSource::Local {
                        project_id,
                        project_root,
                        ..
                    } => {
                        if !path.is_file() {
                            show_alert(t("fileViewer", "linkTargetMissing"), target, window, cx);
                            return;
                        }
                        DocumentSource::Local {
                            project_id: project_id.clone(),
                            project_root: project_root.clone(),
                            path,
                        }
                    }
                    DocumentSource::Remote {
                        project_id,
                        connection,
                        project_root,
                        ..
                    } => DocumentSource::Remote {
                        project_id: project_id.clone(),
                        connection: connection.clone(),
                        project_root: project_root.clone(),
                        // 远程一律 POSIX,保持解析出来的正斜杠形态
                        path: PathBuf::from(&target),
                    },
                };
                // 回调在事件派发里跑,开页签要动 WorkbenchArea 与(可能是本页签的)
                // 文档实体,推到下一轮 effect 再做
                window.defer(cx, move |window, cx| {
                    crate::workbench_area::open_document_source(source, window, cx);
                });
            }
            LinkAction::Ignore => {}
        }
    }

    /// 文档内锚点:找到含该标题的块,滚到块顶(原版 `scrollIntoView({block: "start"})`)。
    /// 分块缓存一定在(能点到链接就说明预览已经渲染过)。
    fn scroll_to_anchor(&mut self, id: &str, cx: &mut Context<Self>) {
        let blocks = self.md_cache.borrow().as_ref().map(|c| c.blocks.clone());
        let Some(blocks) = blocks else {
            return;
        };
        let hit = blocks.iter().position(|(_, block)| match block {
            MdBlock::Text(text) => block_has_anchor(text, id),
            _ => false,
        });
        if let Some(item_ix) = hit {
            self.md_list.scroll_to(gpui::ListOffset {
                item_ix,
                offset_in_item: px(0.0),
            });
            cx.notify();
        }
    }

    /// 预览态的正文当前目录:相对路径的图片 / 资源按它解析。
    fn preview_base_dir(&self) -> PathBuf {
        self.current_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    /// 预览态要渲染的源码:切到预览那一刻的草稿快照,没有草稿就用磁盘现内容。
    ///
    /// 借出去而不是 clone —— 这条每帧都走(滚动即重画),而正文动辄几十 KB。
    pub(super) fn preview_source(&self) -> &str {
        self.preview_draft.as_deref().unwrap_or(self.doc.disk())
    }

    /// 正文分块(带缓存,见 [`MdCache`])。源码或所在目录变了才重切。
    /// 返回 `(分块, 代次)`,代次每重切一次 +1(见 [`MdCache::generation`])。
    ///
    /// 重切时顺手把文档里已经没有的 mermaid 图表从资源系统里放掉
    /// ([`Self::release_stale_mermaid`]),`window` / `cx` 只为这一件事。
    fn md_blocks(
        &self,
        source: &str,
        base_dir: &Path,
        local_resources: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> (Rc<Vec<(f32, MdBlock)>>, u64) {
        // 先把命中与否算完再撒手,别让 borrow 活到 borrow_mut 那一行
        let (hit, last_generation) = {
            let cache = self.md_cache.borrow();
            let hit = cache.as_ref().and_then(|c| {
                (c.source == source
                    && c.base_dir == base_dir
                    && c.local_resources == local_resources)
                    .then(|| (c.blocks.clone(), c.generation))
            });
            (hit, cache.as_ref().map_or(0, |c| c.generation))
        };
        if let Some(hit) = hit {
            return hit;
        }
        let generation = last_generation + 1;

        let blocks: Vec<(f32, MdBlock)> = split_md_blocks(source)
            .into_iter()
            .enumerate()
            .map(|(ix, seg)| {
                let mt = block_top_margin(ix, &seg);
                let block = match seg {
                    // 交给 TextView 的段里还可能有**内联**图片(列表项 / 引用块 /
                    // 表格格子),它们的本地路径得先转成 file:// 才画得出来
                    // (见 rewrite_md_image_urls);块级图片行不走这里,
                    // 拿的是拆好的原始 url
                    MdSegment::Text(text) => MdBlock::Text(if local_resources {
                        rewrite_md_image_urls(&text, base_dir).into()
                    } else {
                        sanitize_remote_markdown(&text).into()
                    }),
                    MdSegment::Table(mut table) => {
                        for cell in table
                            .header
                            .iter_mut()
                            .chain(table.rows.iter_mut().flatten())
                        {
                            *cell = if local_resources {
                                rewrite_md_image_urls(cell, base_dir)
                            } else {
                                sanitize_remote_markdown(cell)
                            };
                        }
                        MdBlock::Table(table)
                    }
                    MdSegment::Images(images) => MdBlock::Images(images),
                    // 围栏原文按与 Text 段相同的口径过一遍(代码块里没有活动构造,
                    // 两个函数对它都是恒等,过一遍只是不让它成为口径上的例外)
                    MdSegment::Mermaid { code, raw } => MdBlock::Mermaid {
                        code: code.into(),
                        fallback: if local_resources {
                            rewrite_md_image_urls(&raw, base_dir).into()
                        } else {
                            sanitize_remote_markdown(&raw).into()
                        },
                    },
                };
                (mt, block)
            })
            .collect();

        self.release_stale_mermaid(&blocks, window, cx);
        self.release_stale_images(&blocks, base_dir, window, cx);

        let blocks = Rc::new(blocks);
        *self.md_cache.borrow_mut() = Some(MdCache {
            source: source.to_string(),
            base_dir: base_dir.to_path_buf(),
            local_resources,
            generation,
            blocks: blocks.clone(),
        });
        (blocks, generation)
    }

    /// 重切分块后,把要过但新分块里已经没有的 mermaid 图表放掉(理由见
    /// [`Self::mermaid_requested`])。按图表文本比,不按 key 比:主题切换后旧配色
    /// 的那份也一并放掉 —— 切回来重画一次是 ms 级的事,留着是几 MB 的显存。
    fn release_stale_mermaid(&self, blocks: &[(f32, MdBlock)], window: &mut Window, cx: &mut App) {
        let mut requested = self.mermaid_requested.borrow_mut();
        if requested.is_empty() {
            return;
        }
        let live: HashSet<&str> = blocks
            .iter()
            .filter_map(|(_, block)| match block {
                MdBlock::Mermaid { code, .. } => Some(&**code),
                _ => None,
            })
            .collect();
        let stale: Vec<MermaidKey> = requested
            .iter()
            .filter(|key| !live.contains(&*key.code))
            .cloned()
            .collect();
        for key in &stale {
            requested.remove(key);
        }
        release_mermaid_assets(&stale, cx, Some(window));
    }

    /// 重切分块后,撤掉本页签对新分块里已经没有的图片的持有(改了图片链接、
    /// 删了图片行、所在目录变了)。别的页签还在用的由账本留着,见 [`super::images::HoldTable`]。
    ///
    /// 在渲染途中放是安全的:所有 render / prepaint 都先于本帧任何 paint
    /// (`window.rs` 的 `draw_roots`),这些图本帧不会再被画;上一帧的场景在
    /// 本帧画完后就被替换,不会再呈现。
    fn release_stale_images(
        &self,
        blocks: &[(f32, MdBlock)],
        base_dir: &Path,
        window: &mut Window,
        cx: &mut App,
    ) {
        let stale: Vec<Resource> = {
            let mut requested = self.images_requested.borrow_mut();
            if requested.is_empty() {
                return;
            }
            let live = md_image_resources(blocks, base_dir);
            requested.extract_if(|key| !live.contains(key)).collect()
        };
        release_viewer_images(stale, cx, Some(window));
    }

    /// 让 [`Self::md_list`] 与当前分块对齐:块数变了、分块代次变了(源码或所在
    /// 目录变)、视口宽变了(拖分栏 / 缩放窗口,块高全部作废)三种情况都重置列表
    /// 让它重新量高,但**保住逻辑滚动位置**(块序号 + 块内偏移)—— `reset` 会把
    /// 位置清零,这里先存后还;块数变少时 `scroll_to` 自己夹到末尾。
    ///
    /// 宽度看的是列表**上一帧**的布局边界:`gpui::list` 自己在 prepaint 里发现
    /// 变宽会把块高作废,但只重量视口内的块,视口外的按 0 计,滚动条就会随着
    /// 滚动一路乱跳;这里晚一帧补一次全量测量(`measure_all` 经 `reset` 重新
    /// 武装),代价与旧路径的一帧相同,只在真的变宽时付。首帧的宽是 0,不算变。
    fn sync_md_list(&self, block_count: usize, generation: u64) {
        let width = self.md_list.viewport_bounds().size.width;
        let (last_generation, last_width) = self.md_list_sync.get();
        let width_changed = last_width != px(0.0) && width != last_width;
        if self.md_list.item_count() != block_count
            || generation != last_generation
            || width_changed
        {
            let keep = self.md_list.logical_scroll_top();
            self.md_list.reset(block_count);
            self.md_list.scroll_to(keep);
        }
        self.md_list_sync.set((generation, width));
    }

    /// 富文本排版。markdown 与 html 两支预览共用一份 —— 两边走的是
    /// gpui-component 的同一个渲染器,样式没有理由分家。
    ///
    /// 对齐原版 `.md-preview`(styles.css:814-887):基准 1.08rem ≈ 14px
    /// (root=uiFontSize,走 ui::font_px 保持随设置缩放)、行高 1.7、标题
    /// 1.8/1.4/1.15/1em、段距 0.8em、代码块 0.85em —— TextView 默认基准吃
    /// gpui 的 16px、标题倍率 2/1.5/1.25,整体明显偏大(用户实测)。
    fn preview_text_style(&self, cx: &mut Context<Self>) -> TextViewStyle {
        let mut code_block = gpui::StyleRefinement::default();
        {
            // `refine_style` 排在组件自己的 `.text_size(mono_font_size)` 之后,
            // 这里的字号能赢(node.rs:384-386)
            let text = &mut code_block.text;
            text.font_size = Some(ui::font_px(11.9).into());
            text.line_height = Some(gpui::relative(1.6));
        }
        TextViewStyle {
            highlight_theme: cx.theme().highlight_theme.clone(),
            is_dark: cx.theme().mode.is_dark(),
            heading_base_font_size: ui::font_px(14.0),
            // 段间距曾按原版 p margin 0.8em 压到 0.7rem,用户体感偏密 ——
            // 回到组件默认 1rem(16px,也接近原版 ul 的浏览器默认 margin 档)
            paragraph_gap: gpui::rems(1.0),
            code_block,
            // 行内 code 对齐原版 `.md-preview code`:字 --accent 橙、底 --bg-elevated。
            // 0.6.2 起 TextViewStyle 有了 inline_code 钩子(设了的字段赢,None 沿用
            // 主题默认),不再借全局 `Theme.colors.accent` 换底
            inline_code: gpui::HighlightStyle {
                color: Some(ui::accent()),
                background_color: Some(ui::bg_elevated()),
                ..Default::default()
            },
            ..Default::default()
        }
        .heading_font_size(|level, base| match level {
            1 => base * 1.8,
            2 => base * 1.4,
            3 => base * 1.15,
            _ => base,
        })
    }

    /// 预览滚动壳:内容器挂上 [`Self::preview_scroll`](进度跨卸载存活),
    /// 再叠一层滚动条(见 [`Self::scrollbar_overlay`])。html 预览用;markdown
    /// 预览的滚动由 `gpui::list` 自己管,见 [`Self::render_markdown`]。
    fn preview_scroll_shell(
        &self,
        bar_id: &'static str,
        content: gpui::Stateful<gpui::Div>,
    ) -> gpui::AnyElement {
        div()
            .size_full()
            .relative()
            .child(content.track_scroll(&self.preview_scroll))
            .child(self.scrollbar_overlay(bar_id, &self.preview_scroll))
            .into_any_element()
    }

    /// 叠在滚动内容之上的滚动条层(显隐跟主题的 `scrollbar_show`,默认滚动时
    /// 现身、闲置淡出,与终端滚动条同口径)。
    ///
    /// 滚动条要自己套一层 `absolute` + 四边贴 0 的壳,理由与 `menu.rs` 那处相同:
    /// `Scrollbar` 元素自身是 absolute 却不带 inset,直接塞进流式布局会被 taffy
    /// 按静态位置摆到内容下方去。壳上没有监听器,不挡内容的点击与选择。
    fn scrollbar_overlay<H: gpui_component::scroll::ScrollbarHandle + Clone>(
        &self,
        bar_id: &'static str,
        handle: &H,
    ) -> gpui::Div {
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .child(Scrollbar::vertical(handle).id(bar_id))
    }

    /// Markdown 预览。样式对照 `src/styles.css:813-943` 的 `.md-preview`:
    /// 容器 `p-6 max-w-[860px] mx-auto`、段间距 1 rem、正文 1.08rem/1.7。
    ///
    /// 代码块高亮是**改善**(原版 `.md-preview pre code` 只设颜色不做高亮),
    /// 且与编辑器同一份 `highlight_theme`,两处颜色一致。
    ///
    /// # 为什么是 `gpui::list` 而不是 `overflow_y_scroll`
    ///
    /// GPUI 没有跨帧的布局缓存:一次 notify = 整棵元素树重建 + taffy 全量布局。
    /// 非虚拟化容器下滚一格就把整篇文档重排一遍,视口外的也不例外。2026-09-09
    /// 对一份 65 KB / 17 张表 / 226 个列表项的文档采样(debug 带符号):主线程
    /// 90% 在 `Window::draw`,其中 **taffy 布局 81.5%**、元素构建 6.6%、文本整形
    /// 6.4%(DirectWrite 本身 0.18%,行排版有缓存)、绘制 <2%;装机版实测每滚
    /// 一格 130ms(≈8fps),同文件源码视图 16ms。成本 ∝ 元素数,不 ∝ 字节数
    /// (每个表格格子 ≈0.04ms,列表项比同文字段落贵 1.7 倍),release 只比 debug
    /// 快 1.3 倍 —— 编译优化救不了,只能不排视口外的东西。
    ///
    /// 于是这里一块([`MdBlock`])一项交给 `gpui::list`:只有视口 ± overdraw 里的
    /// 块会被 [`Self::render_md_item`] 建出来、参与布局;块高量过就记在
    /// [`Self::md_list`] 里,拖分栏 / 改源码时由 [`Self::sync_md_list`] 作废重量。
    /// 只留第一屏内容的对照文档实测 28ms/帧,这就是虚拟化后的预期档位。
    ///
    /// 已知取舍:① 滚出视口的块的 TextView 状态(含选区)随 element state 一起
    /// 回收,滚回来重新解析那一块(几百微秒);② 滚轮步长由 list 写死为每行 20px,
    /// 比 `overflow_y_scroll` 按行高算的略慢;③ 水平内边距放在每一项上而不是
    /// 列表上 —— list 的 padding 只影响纵向,横向不缩项宽。
    pub(super) fn render_markdown(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let base_dir = self.preview_base_dir();
        // 表格、图片与 mermaid 图表拆出来自绘(组件表格单行截断、图片只认网络
        // URI、mermaid 只会画成代码,见 split_md_blocks 一节的说明),其余段落照走
        // TextView。分块结果跨帧缓存(见 MdCache)——「滚一格重 render 一遍」
        // 这条路上,每帧重切 40 KB 正文是白烧。
        let (blocks, generation) = self.md_blocks(
            self.preview_source(),
            &base_dir,
            !self.source.is_remote(),
            window,
            cx,
        );
        self.sync_md_list(blocks.len(), generation);
        let content = div()
            .size_full()
            .text_size(ui::font_px(14.0))
            // 原版 .md-preview 是 1.7;数值对齐后用户仍觉得密(体感口径),
            // 放宽到 1.85 —— 表格格子行高同源跟随
            .line_height(gpui::relative(1.85))
            .child(
                list(self.md_list.clone(), cx.processor(Self::render_md_item))
                    .size_full()
                    // 原版容器 p-6 的纵向那一半;横向的在每一项上(见方法注释)
                    .pt(px(24.0))
                    .pb(px(24.0)),
            );
        div()
            .size_full()
            .relative()
            .child(content)
            .child(self.scrollbar_overlay("file-viewer-md-scrollbar", &self.md_list))
            .into_any_element()
    }

    /// 虚拟化列表的一项 = 一块正文([`MdBlock`]),只有视口附近的块会被调到
    /// (见 [`Self::render_markdown`])。行根必须撑满列表宽(`w_full`,否则里面
    /// `relative()` 的宽度没有参照),再按原版 `.md-preview` 收成 860px 居中列。
    ///
    /// 段落 id 按块序编,文档不变即稳定,TextView 的解析结果与选区靠它跨帧复用。
    fn render_md_item(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // 缓存一定已由本帧的 render_markdown 填好;拿 Rc 就撒手,别让 borrow
        // 活到下面 render_md_images 的 &self 调用
        let blocks = self.md_cache.borrow().as_ref().map(|c| c.blocks.clone());
        let Some((mt, block)) = blocks.as_ref().and_then(|blocks| blocks.get(ix)) else {
            return div().into_any_element();
        };
        let style = self.preview_text_style(cx);
        // 块间距按原版纵向节奏由这里统一给(em 基准,随 uiFontSize 缩放),
        // TextView 内部的 paragraph_gap 在非虚拟化路径上是坏的(见 split_md_blocks
        // 注释)
        let content = match block {
            MdBlock::Text(text) => TextView::markdown(
                gpui::SharedString::from(format!("file-viewer-md-body-{ix}")),
                text.clone(),
            )
            .style(style.clone())
            .selectable(true)
            .on_link_click(self.preview_link_handler(cx))
            .into_any_element(),
            MdBlock::Table(table) => render_md_table(ix, table, &style, window, cx),
            MdBlock::Images(images) => {
                self.render_md_images(ix, images, MARKDOWN_CONTENT_MAX_WIDTH, window, cx)
            }
            MdBlock::Mermaid { code, fallback } => self.render_md_mermaid(
                ix,
                code,
                fallback,
                &style,
                MARKDOWN_CONTENT_MAX_WIDTH,
                window,
                cx,
            ),
        };
        div()
            .w_full()
            .px(px(24.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(MARKDOWN_CONTENT_MAX_WIDTH))
                    .min_w_0()
                    .mx_auto()
                    .when(*mt > 0.0, |el| el.mt(ui::font_px(*mt)))
                    .child(content),
            )
            .into_any_element()
    }

    /// Trusted local HTML preview。Windows / macOS 先走系统 WebView(见 [`super::webview`]),
    /// 效果与浏览器一致;建不起来或在 Linux 上时回落到下面的**富文本简版渲染** ——
    /// `TextView::html` 与 markdown 那支是同一个渲染器:标题 / 段落 / 列表 /
    /// 表格 / 图片 / 链接认得,CSS 与脚本一概不跑,带样式的页面会走样。
    ///
    /// 简版配套两条:顶上一句说明写清楚它是简版,工具栏常驻「用浏览器打开」给真效果
    /// 的出口。图片与其它本地资源靠 [`rewrite_html_urls`] 转 `file://`(原版是
    /// `convertFileSrc`),由 [`super::PreviewHttpClient`] 读盘。
    pub(super) fn render_html(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        debug_assert!(!self.source.is_remote());
        #[cfg(any(windows, target_os = "macos"))]
        if let Some(webview) = self.render_html_webview(window, cx) {
            return webview;
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = window;
        let source = rewrite_html_urls(self.preview_source(), &self.preview_base_dir());
        let style = self.preview_text_style(cx);
        let content = div()
            .id("file-viewer-html")
            .size_full()
            .overflow_y_scroll()
            .p(px(24.0))
            .text_size(ui::font_px(14.0))
            .line_height(gpui::relative(1.85))
            .child(
                div()
                    .w_full()
                    .max_w(px(MARKDOWN_CONTENT_MAX_WIDTH))
                    .min_w_0()
                    .mx_auto()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    // 说明条:别让人对着走样的排版猜是不是文件坏了
                    .child(
                        div()
                            .px(px(10.0))
                            .py(px(6.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(ui::border_subtle())
                            .bg(ui::bg_elevated())
                            .text_size(ui::font_px(12.0))
                            .text_color(ui::text_muted())
                            .child(t("fileViewer", "htmlPreviewNote")),
                    )
                    .child(
                        TextView::html("file-viewer-html-body", source)
                            .style(style)
                            .selectable(true)
                            .on_link_click(self.preview_link_handler(cx)),
                    ),
            );
        self.preview_scroll_shell("file-viewer-html-scrollbar", content)
    }
}

#[cfg(test)]
#[path = "preview_tests.rs"]
mod tests;
