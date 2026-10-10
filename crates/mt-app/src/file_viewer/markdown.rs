//! Markdown 预览的纯逻辑:顶层分块、图片落点解析、本地图片 URL 改写、链接处置
//! 与锚点、自绘表格的排版参数。渲染在 [`super::preview`],不可信输入的清洗在
//! [`super::sanitize`]。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::http_client::Url;
use markdown::{ParseOptions, mdast::Node as MarkdownNode};

// ─── markdown 分段(表格与图片自绘,见 render_markdown) ─────────────
//
// gpui-component 0.5.1 的 TextView 表格是**写死的单行截断**:列宽按字符数
// 原样占比(`node.rs:1070` 的 `relative(len)`)、格子 `.truncate()` ——
// 「文件名列 vs 大段职责列」直接把短列压没、长文本裁掉,与原版
// `.md-preview table`(自动换行 + 浏览器 auto 布局)差一个档次,且
// `TextViewStyle` 没留任何表格钩子。这里把 GFM 表格从文档里拆出来自绘,
// 其余段落照走 TextView;格子内容仍按 markdown 渲染,行内 code/加粗不丢。
//
// **图片同理,而且更硬**:TextView 把图片 URL 一律当网络 URI
// (`node.rs:609` 的 `img(image.url)` 收的是 `SharedUri` → `Resource::Uri`
// → 走 http client),于是 md 里的相对路径图片(README 的截图)在预览里
// 什么都不出;原版靠 `convertFileSrc(fileDir + '/' + src)` 转 asset 协议
// (`FileViewerModal.tsx:145-150`)。这里把「整行只有图片」的行拆出来自绘,
// 相对路径按当前文件所在目录解析成 `Resource::Path`,见
// [`split_top_level_image_paragraph`]
// 与 [`FileViewer::render_md_images`]。
//
// **```mermaid 围栏是第三种自绘块**(issue #80):TextView 只会把它当代码块
// 画出源文本。这里把顶层的 mermaid 围栏拆出来,交给 [`MermaidAsset`] 在后台
// 线程渲染成位图(Mermaid 文本 → SVG → gpui 的 `SvgRenderer`),画法与本地图片同一套框;
// 渲染失败时退回代码块 + 一行原因,见 [`FileViewer::render_md_mermaid`]。

/// GFM 表格的列对齐(分隔行的 `:---:` 语法)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MdAlign {
    Left,
    Center,
    Right,
}

/// 一张解析好的 GFM 表格。格子存**原文**,渲染时逐格走 markdown。
#[derive(Debug, PartialEq)]
pub(super) struct MdTable {
    pub(super) header: Vec<String>,
    pub(super) aligns: Vec<MdAlign>,
    pub(super) rows: Vec<Vec<String>>,
}

/// markdown 里的一张图片。纯图片段落由 AST 确认后拆出来自绘。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct MdImage {
    /// 原文里的目标,**未解码也未解析** —— 落地在 [`resolve_image_src`]
    pub(super) url: String,
    pub(super) alt: String,
    /// `![alt](url "title")` 的 title,悬停显示
    pub(super) title: Option<String>,
    /// `[![alt](img)](link)` 外层链接:点图开外链(徽章行的写法)
    pub(super) link: Option<String>,
}

#[derive(Debug, PartialEq)]
pub(super) enum MdSegment {
    Text(String),
    Table(MdTable),
    /// 一整行的图片(徽章行可能并排多张)
    Images(Vec<MdImage>),
    /// 顶层 ```mermaid 围栏。`code` 是围栏里的图表文本,`raw` 是整个围栏的
    /// 原文(含反引号行)—— 渲染失败时按代码块退回 TextView 用它
    Mermaid {
        code: String,
        raw: String,
    },
}

/// 预处理好的一块正文:`Text` 里的图片目标已改写成绝对 `file://`
/// ([`rewrite_md_image_urls`]),块顶间距([`block_top_margin`])也已算出。
///
/// 与 [`MdSegment`] 分家是因为它要**跨帧活着** —— 见
/// [`super::FileViewer::md_cache`]。
pub(super) enum MdBlock {
    Text(gpui::SharedString),
    Table(MdTable),
    Images(Vec<MdImage>),
    /// 图表文本用 `Arc<str>` 是因为它就是 [`super::mermaid::MermaidKey`] 的主体,
    /// 每帧构 key 只加引用计数;`fallback` 是退回代码块时喂 TextView 的原文
    Mermaid {
        code: Arc<str>,
        fallback: gpui::SharedString,
    },
}

/// 大纲(目录)里的一条:一个**顶层**标题(`#` 与 `===` 两种写法都算;列表 /
/// 引用里的不算,与表格、图片只认顶层同一口径)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MdHeading {
    /// 1~6
    pub(super) level: u8,
    /// 剥掉行内标记后的纯文本(强调、code span、链接只留文字)
    pub(super) text: String,
    /// 标题所在块在 [`MdDocument::segs`] 里的下标。标题总是另起一块、且是该块
    /// 的第一个节点,所以滚到块顶就是滚到标题
    pub(super) block: usize,
}

/// 一篇 markdown 的分块结果与大纲。
#[derive(Debug, Default)]
pub(super) struct MdDocument {
    pub(super) segs: Vec<MdSegment>,
    /// 整篇只能整块交回 TextView 时(引用 / 脚注要共享作用域、AST 拿不到)
    /// 为空:所有标题都落在第 0 块,点哪条都只能滚到文首,不如不给
    pub(super) outline: Vec<MdHeading>,
}

impl MdDocument {
    fn text_only(source: &str) -> Self {
        Self {
            segs: markdown_text_only(source),
            outline: Vec::new(),
        }
    }
}

/// 把 markdown 源切成**顶层 AST 块**:确认是顶层 GFM 表格或纯图片段落时才
/// 自绘，其余节点按源码范围交回 TextView。列表/引用/raw HTML/代码块整块保留，
/// 不能先按“看起来像图片的一行”拆开，否则会把容器里的代码误变成真实资源请求。
/// 顶层标题总是另起一块,顺带收进大纲([`MdDocument::outline`])。
///
/// 逐块喂 TextView 而不是整篇 —— 除了表格要自绘,还有一条硬理由:
/// gpui-component 0.5.1 的非虚拟化路径把 `is_last: true` 原样传给 Root 的
/// **每个**子块(`node.rs:1150-1156`,ListState 路径才逐块算),而
/// `is_last → paragraph_gap = 0`,整篇喂进去相邻段落会贴死(用户对照原版
/// 实测)。块间距改由 [`block_top_margin`] 自己控,顺带复刻原版「标题前
/// 间距更大」的非对称节奏(`.md-preview h* { margin-top: 1.4em }`)。
pub(super) fn split_md_document(source: &str) -> MdDocument {
    let Ok(ast) = markdown::to_mdast(source, &ParseOptions::gfm()) else {
        return MdDocument::text_only(source);
    };
    // 引用、脚注与嵌套定义可能跨分段消费；遇到这些就整篇交回 TextView。
    // 仅有未被引用的顶层普通定义时可以安全分块，它本身不产生可见内容。
    if markdown_requires_shared_definition_scope(&ast) {
        return MdDocument::text_only(source);
    }
    let Some(children) = ast.children() else {
        return MdDocument::text_only(source);
    };

    let mut nodes = Vec::with_capacity(children.len());
    let mut previous_end = 0usize;
    for node in children {
        let Some(position) = node.position() else {
            return MdDocument::text_only(source);
        };
        let (start, end) = (position.start.offset, position.end.offset);
        if start > end || start < previous_end || source.get(start..end).is_none() {
            return MdDocument::text_only(source);
        }
        nodes.push((node, start, end));
        previous_end = end;
    }

    let mut segs = Vec::new();
    let mut outline = Vec::new();
    let mut pending_text: Option<(usize, usize)> = None;
    for (node, start, end) in nodes {
        // 未被引用的顶层定义不产生可见内容。既然上面的共享作用域检查已经确认
        // 没有引用消费者，就直接跳过，避免为它建立一个空 TextView 和块间距。
        if matches!(node, MarkdownNode::Definition(_)) {
            continue;
        }
        // 标题另起一块(哪怕与上文之间没有空行):大纲点过去滚到块顶,落点就是
        // 标题本身,而不是它所在那一大块的开头
        if let MarkdownNode::Heading(heading) = node {
            if let Some((text_start, text_end)) = pending_text.take() {
                push_markdown_text(source, text_start, text_end, &mut segs);
            }
            let mut text = String::new();
            collect_plain_text(node, &mut text);
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if !text.is_empty() {
                // 标题原文非空,这一块下一次推进 segs 时一定落在这个下标上
                outline.push(MdHeading {
                    level: heading.depth,
                    text,
                    block: segs.len(),
                });
            }
            pending_text = Some((start, end));
            continue;
        }
        let raw = &source[start..end];
        let custom = match node {
            MarkdownNode::Table(_) => {
                parse_table_block(raw).map(|table| vec![MdSegment::Table(table)])
            }
            MarkdownNode::Paragraph(_) => split_top_level_image_paragraph(node),
            // 只认顶层围栏:列表 / 引用里的 mermaid 围栏随容器整块交给 TextView,
            // 与表格、图片同一口径(容器拆开会把外层结构拆散)
            MarkdownNode::Code(code) if is_mermaid_fence(code.lang.as_deref()) => {
                Some(vec![MdSegment::Mermaid {
                    code: code.value.clone(),
                    raw: raw.to_string(),
                }])
            }
            _ => None,
        };
        if let Some(custom) = custom {
            if let Some((text_start, text_end)) = pending_text.take() {
                push_markdown_text(source, text_start, text_end, &mut segs);
            }
            segs.extend(custom);
            continue;
        }

        pending_text = match pending_text.take() {
            Some((text_start, text_end))
                if source[text_end..start]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count()
                    < 2 =>
            {
                Some((text_start, end))
            }
            Some((text_start, text_end)) => {
                push_markdown_text(source, text_start, text_end, &mut segs);
                Some((start, end))
            }
            None => Some((start, end)),
        };
    }
    if let Some((text_start, text_end)) = pending_text {
        push_markdown_text(source, text_start, text_end, &mut segs);
    }
    MdDocument { segs, outline }
}

/// 标题的纯文本:文字与 code span 照收,强调 / 删除线 / 链接只留里面的文字,
/// 图片取 alt,硬换行算一个空格,内联 HTML 丢掉。
fn collect_plain_text(node: &MarkdownNode, out: &mut String) {
    match node {
        MarkdownNode::Text(text) => out.push_str(&text.value),
        MarkdownNode::InlineCode(code) => out.push_str(&code.value),
        MarkdownNode::Image(image) => out.push_str(&image.alt),
        MarkdownNode::Break(_) => out.push(' '),
        MarkdownNode::Html(_) => {}
        _ => {
            for child in node.children().into_iter().flatten() {
                collect_plain_text(child, out);
            }
        }
    }
}

/// 围栏的 info string 是不是 mermaid。GitHub 只认 `mermaid` 一种拼法,这里放宽
/// 大小写(`Mermaid` 也偶有);`mermaid-js` 之类带后缀的不算 —— 那是别的渲染器
/// 的方言,画错不如不画。
fn is_mermaid_fence(lang: Option<&str>) -> bool {
    lang.is_some_and(|lang| lang.trim().eq_ignore_ascii_case("mermaid"))
}

fn markdown_requires_shared_definition_scope(node: &MarkdownNode) -> bool {
    let Some(children) = node.children() else {
        return false;
    };
    children.iter().any(|child| match child {
        MarkdownNode::Definition(_) => false,
        _ => markdown_contains_reference_or_definition(child),
    })
}

fn markdown_contains_reference_or_definition(node: &MarkdownNode) -> bool {
    matches!(
        node,
        MarkdownNode::Definition(_)
            | MarkdownNode::FootnoteDefinition(_)
            | MarkdownNode::ImageReference(_)
            | MarkdownNode::LinkReference(_)
    ) || node.children().is_some_and(|children| {
        children
            .iter()
            .any(markdown_contains_reference_or_definition)
    })
}

fn markdown_text_only(source: &str) -> Vec<MdSegment> {
    let mut segs = Vec::new();
    push_markdown_text(source, 0, source.len(), &mut segs);
    segs
}

fn push_markdown_text(source: &str, start: usize, end: usize, segs: &mut Vec<MdSegment>) {
    if let Some(text) = source.get(start..end)
        && !text.trim().is_empty()
    {
        segs.push(MdSegment::Text(text.to_string()));
    }
}

fn parse_table_block(source: &str) -> Option<MdTable> {
    let mut lines = source.lines();
    let header = split_cells(lines.next()?);
    let aligns = parse_separator(lines.next()?)?;
    if header.is_empty() || header.len() != aligns.len() {
        return None;
    }
    let mut rows = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            break;
        }
        let mut cells = split_cells(line);
        cells.resize(header.len(), String::new());
        rows.push(cells);
    }
    Some(MdTable {
        header,
        aligns,
        rows,
    })
}

fn markdown_image_from_node(node: &MarkdownNode) -> Option<MdImage> {
    match node {
        MarkdownNode::Image(image) if !image.url.trim().is_empty() => Some(MdImage {
            url: image.url.clone(),
            alt: image.alt.clone(),
            title: image.title.clone(),
            link: None,
        }),
        MarkdownNode::Link(link) if link.children.len() == 1 => {
            let MarkdownNode::Image(image) = &link.children[0] else {
                return None;
            };
            if image.url.trim().is_empty() {
                return None;
            }
            Some(MdImage {
                url: image.url.clone(),
                alt: image.alt.clone(),
                title: image.title.clone(),
                link: (!link.url.is_empty()).then(|| link.url.clone()),
            })
        }
        _ => None,
    }
}

fn split_top_level_image_paragraph(node: &MarkdownNode) -> Option<Vec<MdSegment>> {
    let MarkdownNode::Paragraph(paragraph) = node else {
        return None;
    };
    let mut segs = Vec::new();
    let mut images = Vec::new();
    for child in &paragraph.children {
        if let Some(image) = markdown_image_from_node(child) {
            images.push(image);
            continue;
        }
        let MarkdownNode::Text(text) = child else {
            return None;
        };
        if !text.value.chars().all(char::is_whitespace) {
            return None;
        }
        if text
            .value
            .bytes()
            .any(|byte| byte == b'\n' || byte == b'\r')
            && !images.is_empty()
        {
            segs.push(MdSegment::Images(std::mem::take(&mut images)));
        }
    }
    if !images.is_empty() {
        segs.push(MdSegment::Images(images));
    }
    (!segs.is_empty()).then_some(segs)
}

/// 图片目标的落点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MdImageSrc {
    /// 本地文件(相对路径已按当前文件所在目录解析)
    Local(PathBuf),
    /// 远程图片:字节由 [`super::PreviewHttpClient`] 拉回来(见
    /// [`super::FileViewer::render_md_remote_image`])
    Remote(String),
    /// `data:` / 认不出的 scheme
    Unsupported,
}

/// 图片 URL → 落点。相对路径按**当前 md 文件所在目录**解析,与原版
/// `resolveImgSrc`(`FileViewerModal.tsx:145-150` 的
/// `convertFileSrc(fileDir + '/' + src)`)同一口径。
pub(super) fn resolve_image_src(url: &str, base_dir: &Path) -> MdImageSrc {
    let raw = url.trim();
    if raw.is_empty() {
        return MdImageSrc::Unsupported;
    }
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return MdImageSrc::Remote(raw.to_string());
    }
    if lower.starts_with("file://") {
        let rest = percent_decode(&raw["file://".len()..]);
        // `file:///D:/a.png` → `D:/a.png`;UNC(`file://host/share`)原样留着
        let rest = match rest.strip_prefix('/') {
            Some(tail) if looks_like_drive(tail) => tail.to_string(),
            _ => rest,
        };
        return MdImageSrc::Local(PathBuf::from(rest));
    }
    // 其它 scheme(`data:` / `blob:` / `mailto:` …)一律不认。**两个字母起**才算
    // scheme —— 单字母加冒号是 Windows 盘符(`D:\shots\a.png`)
    if scheme_len(raw).is_some_and(|len| len >= 2) {
        return MdImageSrc::Unsupported;
    }
    let decoded = percent_decode(raw);
    let path = Path::new(&decoded);
    if path.is_absolute() {
        MdImageSrc::Local(path.to_path_buf())
    } else {
        MdImageSrc::Local(base_dir.join(path))
    }
}

/// `D:/…` / `d:\…` 这种盘符开头。
fn looks_like_drive(s: &str) -> bool {
    let mut chars = s.chars();
    matches!((chars.next(), chars.next()), (Some(c), Some(':')) if c.is_ascii_alphabetic())
}

/// URL scheme 的字母数(`https://` → 5);不是 scheme 返回 `None`。
fn scheme_len(s: &str) -> Option<usize> {
    let cut = s.find(':')?;
    let head = &s[..cut];
    (!head.is_empty()
        && head.starts_with(|c: char| c.is_ascii_alphabetic())
        && head
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
    .then_some(cut)
}

/// 本地路径 → `file:///…` URL(百分号编码交给 url crate)。相对路径转不了,
/// 那时返回 `None`、调用方保留原文。
pub(super) fn to_file_url(path: &Path) -> Option<String> {
    Url::from_file_path(path).ok().map(|url| url.to_string())
}

/// 把 md 源里图片的**本地**目标改写成 `file:///…` 绝对 URL。
///
/// 整行只有图片的那些行由 [`super::FileViewer::render_md_images`] 自绘、不经过这里;
/// 这条是给**内联**图片兜底的(列表项 `- ![a](b)`、引用块、表格格子里的图片)——
/// 它们要走 TextView,而那条路只认网络 URI,配上 [`super::PreviewHttpClient`]
/// 才画得出来。
///
/// 只有 AST 已确认的 Image 节点才改写；代码、无效 CommonMark 和普通文本原样保留。
fn collect_local_markdown_image_replacements(
    node: &MarkdownNode,
    base_dir: &Path,
    replacements: &mut Vec<MarkdownReplacement>,
) {
    if let MarkdownNode::Image(image) = node {
        if let MdImageSrc::Local(path) = resolve_image_src(&image.url, base_dir)
            && let Some(url) = to_file_url(&path)
            && let Some(replacement) = markdown_replacement(
                node,
                markdown_image_markup(&image.alt, &url, image.title.as_deref()),
            )
        {
            replacements.push(replacement);
        }
        return;
    }

    if let Some(children) = node.children() {
        for child in children {
            collect_local_markdown_image_replacements(child, base_dir, replacements);
        }
    }
}

fn markdown_image_markup(alt: &str, url: &str, title: Option<&str>) -> String {
    let mut markup = String::with_capacity(alt.len() + url.len() + 8);
    markup.push_str("![");
    for ch in alt.chars() {
        match ch {
            '\n' | '\r' => markup.push(' '),
            _ if ch.is_ascii_punctuation() => {
                markup.push('\\');
                markup.push(ch);
            }
            _ => markup.push(ch),
        }
    }
    markup.push_str("](");
    markup.push_str(url);
    if let Some(title) = title {
        markup.push_str(" \"");
        for ch in title.chars() {
            match ch {
                '\n' | '\r' => markup.push(' '),
                '\\' | '"' => {
                    markup.push('\\');
                    markup.push(ch);
                }
                _ => markup.push(ch),
            }
        }
        markup.push('"');
    }
    markup.push(')');
    markup
}

pub(super) fn rewrite_md_image_urls(source: &str, base_dir: &Path) -> String {
    let Ok(ast) = markdown::to_mdast(source, &ParseOptions::gfm()) else {
        return source.to_string();
    };
    let mut replacements = Vec::new();
    collect_local_markdown_image_replacements(&ast, base_dir, &mut replacements);
    replacements.sort_unstable_by_key(|replacement| std::cmp::Reverse(replacement.start));

    let mut rewritten = source.to_string();
    let mut next_start = source.len();
    for replacement in replacements {
        if replacement.end > next_start
            || replacement.start > replacement.end
            || source.get(replacement.start..replacement.end).is_none()
        {
            continue;
        }
        rewritten.replace_range(replacement.start..replacement.end, &replacement.value);
        next_start = replacement.start;
    }
    rewritten
}

#[derive(Debug)]
pub(super) struct MarkdownReplacement {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) value: String,
}

pub(super) fn markdown_replacement(
    node: &MarkdownNode,
    value: String,
) -> Option<MarkdownReplacement> {
    let position = node.position()?;
    Some(MarkdownReplacement {
        start: position.start.offset,
        end: position.end.offset,
        value,
    })
}

/// `%20` 之类还原成字符(md 里带空格的路径常这么写);非法转义原样留着。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ─── 链接处置(纯逻辑) ────────────────────────────────────────

/// 预览里点到的链接该怎么处置(`FileViewerModal.tsx:188-214` 的 `handleLinkClick`)。
///
/// 原版是一个 modal 里换文件、带 `←` 历史栈;GPUI 版文件本来就是页签,
/// 「本地文件」一律作为页签打开(已开的就切过去),历史栈由页签条承担。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LinkAction {
    /// http(s) 外链:弹确认后交给系统浏览器
    External(String),
    /// 文档内锚点(`#标题`):滚到对应标题所在的块
    Anchor(String),
    /// mailto: / tel: 这类其它协议:直接交给系统
    Scheme(String),
    /// 本地文件:已按当前文件所在目录解析成绝对路径(正斜杠)
    Local(String),
    /// 空 href / 解析不出目标
    Ignore,
}

pub(super) fn classify_link(current_file: &str, href: &str) -> LinkAction {
    let href = href.trim();
    if href.is_empty() {
        return LinkAction::Ignore;
    }
    let lower = href.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return LinkAction::External(href.to_string());
    }
    if let Some(id) = href.strip_prefix('#') {
        return LinkAction::Anchor(percent_decode(id));
    }
    if has_url_scheme(href) {
        return LinkAction::Scheme(href.to_string());
    }
    match resolve_local_href(current_file, href) {
        Some(path) => LinkAction::Local(path),
        None => LinkAction::Ignore,
    }
}

/// `^[a-zA-Z][a-zA-Z0-9+.-]*:` 且不是 Windows 盘符 `X:\` / `X:/` 形式。
fn has_url_scheme(href: &str) -> bool {
    let bytes = href.as_bytes();
    if !bytes.first().is_some_and(|b| b.is_ascii_alphabetic()) {
        return false;
    }
    let Some(colon) = href.find(':') else {
        return false;
    };
    if !href[1..colon]
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'))
    {
        return false;
    }
    !(colon == 1 && matches!(bytes.get(2), Some(b'\\') | Some(b'/')))
}

/// 把相对/绝对本地链接解析成规范化的绝对路径(正斜杠、去掉 `./` 与 `..`),
/// `FileViewerModal.tsx:40-61` 的 `resolveLocalHref`。`#锚点` 与 `?查询` 先剥掉。
fn resolve_local_href(current_file: &str, href: &str) -> Option<String> {
    let raw = href.split(['#', '?']).next().unwrap_or("").trim();
    if raw.is_empty() {
        return None;
    }
    let raw = percent_decode(raw).replace('\\', "/");
    let curr = current_file.replace('\\', "/");
    let dir = curr.rfind('/').map(|i| &curr[..i]).unwrap_or("");
    let is_win_abs =
        raw.len() >= 3 && raw.as_bytes()[0].is_ascii_alphabetic() && raw[1..].starts_with(":/");
    let is_posix_abs = raw.starts_with('/');
    let base = if is_win_abs || is_posix_abs {
        raw
    } else {
        format!("{dir}/{raw}")
    };
    // 前导 `/` 看拼完的路径而不是 href 本身:远程(POSIX)文件里的相对链接也得
    // 保住根(原版 `isPosixAbs` 只看 href,这一步会丢掉 `/`,是它的 bug)
    let is_posix_abs = base.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in base.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            seg => out.push(seg),
        }
    }
    let first_is_drive = out
        .first()
        .is_some_and(|s| s.len() == 2 && s.as_bytes()[0].is_ascii_alphabetic() && s.ends_with(':'));
    let joined = out.join("/");
    Some(if is_posix_abs && !first_is_drive {
        format!("/{joined}")
    } else {
        joined
    })
}

/// GitHub 风格 slug(`FileViewerModal.tsx:72-77` 的 `slugify`):小写、只留
/// 字母数字下划线连字符与中文、空白折成一个 `-`。
fn heading_slug(text: &str) -> String {
    let lowered = text.trim().to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut pending_dash = false;
    for c in lowered.chars() {
        if c.is_whitespace() {
            pending_dash = true;
            continue;
        }
        let keep = c.is_ascii_alphanumeric()
            || matches!(c, '_' | '-')
            || ('\u{4e00}'..='\u{9fa5}').contains(&c);
        if !keep {
            continue;
        }
        if pending_dash {
            out.push('-');
            pending_dash = false;
        }
        out.push(c);
    }
    out
}

/// 标题行的纯文本:原版取的是渲染后的 `textContent`,这里把常见的行内标记
/// (强调、code span、链接的 `[文字](url)`)剥掉后再做 slug。
fn strip_inline_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        rest = &rest[open + 1..];
        // `[文字](url)`:只留文字;不成对就把 `[` 当普通字符
        if let Some(close) = rest.find("](")
            && let Some(end) = rest[close..].find(')')
        {
            out.push_str(&rest[..close]);
            rest = &rest[close + end + 1..];
        } else {
            out.push('[');
        }
    }
    out.push_str(rest);
    out.chars()
        .filter(|c| !matches!(c, '*' | '`' | '~'))
        .collect()
}

/// 一块正文里有没有这个锚点:原始 HTML 的 `id="…"`,或某一行是 ATX 标题且
/// slug 相同(围栏代码块里的 `# 注释` 不算)。目标也过一遍 slug,
/// `#My Heading` / `#my-heading` 都能命中。
pub(super) fn block_has_anchor(text: &str, raw_id: &str) -> bool {
    if text.contains(&format!("id=\"{raw_id}\"")) || text.contains(&format!("id='{raw_id}'")) {
        return true;
    }
    let want = heading_slug(raw_id);
    if want.is_empty() {
        return false;
    }
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if !(1..=6).contains(&hashes) || !trimmed[hashes..].starts_with(' ') {
            continue;
        }
        let title = trimmed[hashes..].trim().trim_end_matches('#').trim();
        if heading_slug(&strip_inline_markup(title)) == want {
            return true;
        }
    }
    false
}

/// 块顶间距:对照 `.md-preview` 的纵向节奏 —— 段落间 `p { margin: 0.8em }`
/// (相邻外边距在 CSS 里折叠,取 0.8em ≈ 11px);标题前 `margin-top: 1.4em`
/// (≈20px,原版按标题自身字号算,这里取 h2/h3 档的近似);表格 `margin: 1em`
/// (≈13px)。首块为 0,标题后的间距由**下一块**的 11px 承担(原版 0.6em≈10px)。
pub(super) fn block_top_margin(ix: usize, seg: &MdSegment) -> f32 {
    if ix == 0 {
        return 0.0;
    }
    match seg {
        // 图片与表格同档:原版 `.md-preview img` 吃 p 的 0.8em,块级化之后
        // 按「独立块」给 1em(≈13px),与表格一致;mermaid 图表就是一张图
        MdSegment::Table(_) | MdSegment::Images(_) | MdSegment::Mermaid { .. } => 13.0,
        MdSegment::Text(text) => {
            let first = text.trim_start();
            // `#`~`######` + 空格才是标题(# 后无空格在 CommonMark 里不算)
            let hashes = first.chars().take_while(|c| *c == '#').count();
            if (1..=6).contains(&hashes) && first[hashes..].starts_with(' ') {
                20.0
            } else {
                11.0
            }
        }
    }
}

/// 拆一行表格的格子:剥外侧竖线;反引号 code span 里的 `|` 不拆
/// (`process_monitor.rs` 这类格子里常有内联 code),`\|` 是字面竖线。
fn split_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut in_code = false;
    for ch in t.chars() {
        match ch {
            '`' => {
                in_code = !in_code;
                cur.push(ch);
            }
            '|' if !in_code => {
                if cur.ends_with('\\') {
                    cur.pop();
                    cur.push('|');
                } else {
                    cells.push(cur.trim().to_string());
                    cur.clear();
                }
            }
            _ => cur.push(ch),
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

/// 分隔行(`| --- | :---: |`)→ 每列对齐;不是分隔行返回 `None`。
fn parse_separator(line: &str) -> Option<Vec<MdAlign>> {
    if !line.contains('-') {
        return None;
    }
    let cells = split_cells(line);
    let mut aligns = Vec::with_capacity(cells.len());
    for cell in &cells {
        let c = cell.trim();
        let dashes = c.trim_matches(':');
        if dashes.is_empty() || !dashes.chars().all(|ch| ch == '-') {
            return None;
        }
        aligns.push(match (c.starts_with(':'), c.ends_with(':')) {
            (true, true) => MdAlign::Center,
            (false, true) => MdAlign::Right,
            _ => MdAlign::Left,
        });
    }
    Some(aligns)
}

/// 列宽权重:各列取最长格子的显示宽(CJK 记 2),clamp 后归一化。
/// 不 clamp 的话短列会被大段长文列压到读不出字(组件那版第一列
/// `process_mon…` 被截断的直接原因);上限则挡住「一格超长把别列全挤扁」。
pub(super) fn column_weights(table: &MdTable) -> Vec<f32> {
    let n = table.header.len().max(1);
    let mut lens = vec![1usize; n];
    for (ix, cell) in table.header.iter().enumerate() {
        lens[ix] = lens[ix].max(display_width(cell));
    }
    for row in &table.rows {
        for (ix, cell) in row.iter().enumerate() {
            if ix < n {
                lens[ix] = lens[ix].max(display_width(cell));
            }
        }
    }
    let capped: Vec<f32> = lens.iter().map(|l| (*l).clamp(6, 60) as f32).collect();
    let total: f32 = capped.iter().sum();
    capped.iter().map(|l| l / total).collect()
}

/// 近似显示宽:ASCII 记 1、其余(CJK/全角为主)记 2。行内标记(`` ` ``/`**`)
/// 会略微虚高,权重口径下无关紧要。
fn display_width(s: &str) -> usize {
    s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

/// 格子内容能不能不起 `TextView`、直接当纯文本画。
///
/// **表格自绘的代价全压在这一个判定上。** 每个格子一个 [`gpui_component::text::TextView::markdown`],
/// 每个 TextView 又是「focus handle + key context + 隐藏滚动条层 + 自己的
/// 元素树」一整套。此前 [`super::FileViewer::render_markdown`] 的滚动容器还是非虚拟化
/// 的普通 div,滚一格 = 整篇重建一遍、视口外的表格也不例外:实测一份 26 张表的
/// 需求文档是每帧 1425 个 TextView,滚动直接卡死。现在容器已换成 `gpui::list`
/// 按块虚拟化,只有视口附近的块付钱,但一张 20 行的表仍可能整张在视口里 ——
/// 2026-09-09 采样(见 render_markdown 注释)一个格子的 TextView 约 0.04ms,
/// 快路仍然值得留着。
///
/// 判据**保守到底**:只要出现任何可能被 markdown 当标记的字符就判否,宁可多起
/// 一个 TextView,也不能把行内 code / 加粗 / 链接画成源码。放行的格子渲染结果
/// 与走 TextView **逐像素一致** —— 组件的普通文本 run 直接吃
/// `window.text_style()`(`text/inline.rs:247-259`),字号颜色行高全靠继承,
/// 与这里的纯文本元素同源;连「多个空格折叠成一个」那点差别也靠下面那条挡掉。
pub(super) fn is_plain_cell(s: &str) -> bool {
    // markdown 折叠空白,纯文本不折 —— 有连续空白就交回 TextView,免得两类格子
    // 排版有肉眼可见的差
    if s.contains('\t') || s.contains("  ") {
        return false;
    }
    // 行内标记:出现在任何位置都可能起作用
    if s.bytes().any(|b| {
        matches!(
            b,
            b'`' | b'*' | b'_' | b'[' | b']' | b'<' | b'>' | b'&' | b'~' | b'\\' | b'!' | b'|'
        )
    }) {
        return false;
    }
    // GFM 的 autolink literal:裸 URL / www. / 邮箱会自动变链接
    // (解析走 `ParseOptions::gfm()`,见 gpui-component `text/format/markdown.rs`)
    if s.contains("://") || s.contains("www.") || s.contains('@') {
        return false;
    }
    // 块级标记只在行首起作用,而格子内容没有换行、且在 [`split_cells`] 里已 trim,
    // 只看开头一处。`-`/`+`/`#` 不管后面跟不跟空格一律判否 —— 差一个字符的判定
    // 不值得赌(`---` 是分隔线,`- 项` 是列表)。`=` 反倒安全:setext 标题要有上一行,
    // 单行 `===` 只会是段落,于是 `a=b` 这类格子照走快路
    let Some(first) = s.as_bytes().first().copied() else {
        return true;
    };
    if matches!(first, b'#' | b'-' | b'+') {
        return false;
    }
    // `1. 项` / `1) 项` 有序列表。点号后必须是空白(或到头)才算 ——
    // 否则 `1.5 倍` 这种会被误判
    if first.is_ascii_digit() {
        let rest = s.trim_start_matches(|c: char| c.is_ascii_digit());
        if let Some(after) = rest.strip_prefix(['.', ')'])
            && (after.is_empty() || after.starts_with(char::is_whitespace))
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
#[path = "markdown_tests.rs"]
mod tests;
